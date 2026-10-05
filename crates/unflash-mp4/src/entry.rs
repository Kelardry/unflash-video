//! MP4 audio sample entries built from scratch: for audio that arrives in
//! another container (Matroska, a transport stream) or from an encoder, so
//! an export can carry it in an MP4 track. Each builder takes what the
//! codec's own setup data says (an AudioSpecificConfig, an OpusHead, a FLAC
//! STREAMINFO, the first AC-3 / E-AC-3 / MPEG audio frame). And what the
//! heads of those frames (and DTS's) say, for the readers.

use crate::reader::{Bits, Writer};
use crate::Error;

/// Sampling rates by AAC sampling frequency index.
pub(crate) const AAC_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// The fixed part of an audio sample entry, then `children`.
fn audio_entry(fourcc: &[u8; 4], channels: u32, rate: u32, children: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    let at = w.begin_box(fourcc);
    w.zeros(6);
    w.u16(1); // data reference index
    w.zeros(8); // version 0 fields
    w.u16(channels.clamp(1, 0xffff) as u16);
    w.u16(16); // sample size
    w.zeros(4);
    // 16.16; a rate that does not fit is left to the codec's own setup data
    w.u32(if rate <= 0xffff { rate << 16 } else { 0 });
    children(&mut w);
    w.end_box(at);
    w.buf
}

fn descriptor(w: &mut Writer, tag: u8, body: &[u8]) {
    w.u8(tag);
    let n = body.len();
    if n < 0x80 {
        w.u8(n as u8);
    } else {
        w.u8(0x80 | ((n >> 21) & 0x7f) as u8);
        w.u8(0x80 | ((n >> 14) & 0x7f) as u8);
        w.u8(0x80 | ((n >> 7) & 0x7f) as u8);
        w.u8((n & 0x7f) as u8);
    }
    w.bytes(body);
}

/// An `esds` box: object type `oti` (0x40 AAC, 0x6B MPEG-1 audio, 0x69
/// MPEG-2 audio), with a DecoderSpecificInfo when there is one.
fn esds(w: &mut Writer, oti: u8, dsi: Option<&[u8]>) {
    let mut dcd = Writer::new();
    dcd.u8(oti);
    dcd.u8(0x15); // audio stream, upstream 0, reserved 1
    dcd.u24(0); // buffer size
    dcd.u32(0); // max bitrate
    dcd.u32(0); // average bitrate
    if let Some(d) = dsi {
        descriptor(&mut dcd, 0x05, d);
    }
    let mut es = Writer::new();
    es.u16(0); // ES_ID
    es.u8(0); // flags
    descriptor(&mut es, 0x04, &dcd.buf);
    descriptor(&mut es, 0x06, &[0x02]);
    let at = w.begin_full_box(b"esds", 0, 0);
    descriptor(w, 0x03, &es.buf);
    w.end_box(at);
}

/// What an AudioSpecificConfig says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Asc {
    /// The core audio object type (2 = AAC LC).
    pub(crate) aot: u8,
    pub(crate) rate: u32,
    /// SBR (HE-AAC) signalled explicitly, and the rate it outputs.
    pub(crate) sbr_rate: Option<u32>,
    /// 960-sample frames instead of 1024.
    pub(crate) short_frames: bool,
}

pub(crate) fn parse_asc(asc: &[u8]) -> Option<Asc> {
    let mut r = Bits::new(asc);
    let aot = |r: &mut Bits| -> Option<u8> {
        let a = r.u(5)?;
        Some(if a == 31 { 32 + r.u(6)? as u8 } else { a as u8 })
    };
    let rate = |r: &mut Bits| -> Option<u32> {
        let i = r.u(4)?;
        if i == 15 {
            r.u(24)
        } else {
            AAC_RATES.get(i as usize).copied()
        }
    };
    let mut a = aot(&mut r)?;
    let core_rate = rate(&mut r)?;
    r.skip(4)?; // the channel configuration
    let mut sbr_rate = None;
    if a == 5 || a == 29 {
        sbr_rate = Some(rate(&mut r)?);
        a = aot(&mut r)?;
    }
    let short_frames = matches!(a, 1..=4 | 6 | 7 | 17 | 19..=23) && r.u(1).unwrap_or(0) == 1;
    Some(Asc { aot: a, rate: core_rate, sbr_rate, short_frames })
}

/// A two-byte AudioSpecificConfig for an object type, rate and channel
/// count (with the SBR sync extension when `sbr_rate` is given).
pub(crate) fn make_asc(aot: u8, rate: u32, channels: u32, sbr_rate: Option<u32>) -> Vec<u8> {
    let idx = |r: u32| AAC_RATES.iter().position(|&x| x == r).unwrap_or(4) as u8;
    let sri = idx(rate);
    // the channel configuration: the count, but 7 for 7.1 (8 is reserved)
    let config = if channels == 8 { 7 } else { channels.min(15) as u8 };
    let mut v = vec![(aot << 3) | (sri >> 1), ((sri & 1) << 7) | (config << 3)];
    if let Some(s) = sbr_rate {
        // sync extension 0x2b7, SBR (object type 5), present, extension rate
        v.extend_from_slice(&[0x56, 0xe5, 0x80 | (idx(s) << 3)]);
    }
    v
}

/// `mp4a` + `esds` for AAC.
pub(crate) fn aac_entry(asc: &[u8], rate: u32, channels: u32) -> Vec<u8> {
    audio_entry(b"mp4a", channels, rate, |w| esds(w, 0x40, Some(asc)))
}

/// MPEG audio (layers I-III) in `mp4a`, object type 0x6B (MPEG-1 rates)
/// or 0x69 (MPEG-2's half rates).
pub(crate) fn mpeg_audio_entry(rate: u32, channels: u32) -> Vec<u8> {
    let oti = if rate >= 32000 { 0x6B } else { 0x69 };
    audio_entry(b"mp4a", channels, rate, |w| esds(w, oti, None))
}

/// What the head of an MPEG audio frame says: (rate, channels, samples per frame).
pub(crate) fn mpeg_audio_frame(h: &[u8]) -> Option<(u32, u32, u32)> {
    if h.len() < 4 || h[0] != 0xff || h[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = (h[1] >> 3) & 3; // 0: 2.5, 2: 2, 3: 1
    let layer = (h[1] >> 1) & 3; // 1: III, 2: II, 3: I
    let ri = (h[2] >> 2) & 3;
    if version == 1 || layer == 0 || ri == 3 {
        return None;
    }
    let base = [44100u32, 48000, 32000][ri as usize];
    let rate = match version {
        3 => base,
        2 => base / 2,
        _ => base / 4,
    };
    let channels = if h[3] >> 6 == 3 { 1 } else { 2 };
    let samples = match (layer, version) {
        (3, _) => 384,
        (2, _) => 1152,
        (_, 3) => 1152,
        _ => 576,
    };
    Some((rate, channels, samples))
}

/// What the head of a DTS core frame (sync 0x7FFE8001, 16-bit big endian;
/// 11 bytes of it) says: (rate, channels, samples per frame, frame bytes).
pub(crate) fn dts_frame(h: &[u8]) -> Option<(u32, u32, u32, u32)> {
    if h.len() < 11 || h[..4] != [0x7f, 0xfe, 0x80, 0x01] {
        return None;
    }
    let mut r = Bits::new(h);
    r.skip(32 + 1 + 5 + 1)?; // the sync word, the frame type, the deficit samples, the CRC flag
    let blocks = r.u(7)? + 1;
    let size = r.u(14)? + 1;
    let amode = r.u(6)? as usize;
    let rate = [0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 0, 0][r.u(4)? as usize];
    r.skip(5 + 5)?; // rate, then the fixed bit and four flags
    r.skip(3 + 1 + 1)?; // the extension's kind, its flag, the audio sync word flag
    let lfe = r.u(2)? != 0;
    let channels = [1u32, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8].get(amode).copied().unwrap_or(2) + lfe as u32;
    (size >= 96 && rate > 0 && blocks >= 6).then_some((rate, channels, blocks * 32, size))
}

/// `Opus` + `dOps`, from an OpusHead (Ogg / Matroska / WebCodecs form).
pub(crate) fn opus_entry(head: &[u8]) -> Result<Vec<u8>, Error> {
    if head.len() < 19 || &head[..8] != b"OpusHead" {
        return Err("Opus without an OpusHead".into());
    }
    let channels = head[9];
    let pre_skip = u16::from_le_bytes([head[10], head[11]]);
    let input_rate = u32::from_le_bytes([head[12], head[13], head[14], head[15]]);
    let gain = i16::from_le_bytes([head[16], head[17]]);
    let family = head[18];
    let mapping = if family != 0 {
        let need = 21 + channels as usize;
        if head.len() < need {
            return Err("OpusHead too short for its channel mapping".into());
        }
        head[19..need].to_vec()
    } else {
        vec![]
    };
    Ok(audio_entry(b"Opus", channels as u32, 48000, |w| {
        let at = w.begin_box(b"dOps");
        w.u8(0);
        w.u8(channels);
        w.u16(pre_skip);
        w.u32(input_rate);
        w.i16(gain);
        w.u8(family);
        w.bytes(&mapping);
        w.end_box(at);
    }))
}

/// An OpusHead for a stream without one (up to two channels).
pub(crate) fn make_opus_head(channels: u32, pre_skip: u16, input_rate: u32) -> Vec<u8> {
    let mut v = b"OpusHead".to_vec();
    v.push(1);
    v.push(channels.clamp(1, 2) as u8);
    v.extend_from_slice(&pre_skip.to_le_bytes());
    v.extend_from_slice(&input_rate.to_le_bytes());
    v.extend_from_slice(&0i16.to_le_bytes());
    v.push(0);
    v
}

/// Samples in an Opus packet, from its TOC byte (and the frame count byte
/// of a code 3 packet), at 48 kHz.
pub(crate) fn opus_packet_samples(p: &[u8]) -> Option<u32> {
    let toc = *p.first()?;
    let config = toc >> 3;
    let per_frame = match config {
        0..=11 => [480, 960, 1920, 2880][(config & 3) as usize],
        12..=15 => [480, 960][(config & 1) as usize],
        _ => [120, 240, 480, 960][(config & 3) as usize],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*p.get(1)? & 0x3f) as u32,
    };
    Some(per_frame * frames)
}

/// FLAC's STREAMINFO block (34 bytes) out of a `fLaC` + metadata blocks
/// record, or out of the blocks alone.
pub(crate) fn flac_streaminfo(private: &[u8]) -> Option<&[u8]> {
    let b = private.strip_prefix(b"fLaC").unwrap_or(private);
    if b.len() < 4 + 34 || b[0] & 0x7f != 0 {
        return None;
    }
    Some(&b[4..4 + 34])
}

/// (rate, channels, fixed block size or 0) from a STREAMINFO.
pub(crate) fn flac_info(si: &[u8]) -> (u32, u32, u32) {
    let min_block = u16::from_be_bytes([si[0], si[1]]) as u32;
    let max_block = u16::from_be_bytes([si[2], si[3]]) as u32;
    let rate = ((si[10] as u32) << 12) | ((si[11] as u32) << 4) | (si[12] as u32 >> 4);
    let channels = ((si[12] >> 1) & 7) as u32 + 1;
    (rate, channels, if min_block == max_block { max_block } else { 0 })
}

/// `fLaC` + `dfLa` holding the STREAMINFO.
pub(crate) fn flac_entry(si: &[u8]) -> Vec<u8> {
    let (rate, channels, _) = flac_info(si);
    audio_entry(b"fLaC", channels, rate, |w| {
        let at = w.begin_full_box(b"dfLa", 0, 0);
        w.u8(0x80); // last metadata block, STREAMINFO
        w.u24(si.len() as u32);
        w.bytes(si);
        w.end_box(at);
    })
}

/// Sampling rates by AC-3 / E-AC-3 fscod.
const AC3_RATES: [u32; 3] = [48000, 44100, 32000];

/// AC-3 frame sizes in 16-bit words, by frmsizecod / 2 and fscod (at 44.1
/// kHz an odd frmsizecod adds a word).
const AC3_SIZES: [[u16; 3]; 19] = [
    [64, 69, 96],
    [80, 87, 120],
    [96, 104, 144],
    [112, 121, 168],
    [128, 139, 192],
    [160, 174, 240],
    [192, 208, 288],
    [224, 243, 336],
    [256, 278, 384],
    [320, 348, 480],
    [384, 417, 576],
    [448, 487, 672],
    [512, 557, 768],
    [640, 696, 960],
    [768, 835, 1152],
    [896, 975, 1344],
    [1024, 1114, 1536],
    [1152, 1253, 1728],
    [1280, 1393, 1920],
];

/// Full-range channels by acmod (the LFE channel is lfeon's).
const DOLBY_CHANNELS: [u32; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

/// What the head of an AC-3 or E-AC-3 frame says (its first 8 bytes).
pub(crate) struct Dolby {
    eac3: bool,
    /// The frame's length in bytes.
    pub(crate) bytes: u32,
    pub(crate) rate: u32,
    /// Samples per channel in the frame.
    pub(crate) samples: u32,
    pub(crate) channels: u32,
    /// A dependent substream (or a further independent one): it goes with
    /// the frame before.
    pub(crate) follows: bool,
    // (for dac3 and dec3)
    fscod: u32,
    frmsizecod: u32,
    bsid: u32,
    bsmod: u32,
    acmod: u32,
    lfeon: u32,
}

/// The head of an AC-3 frame (bsid up to 10) or an E-AC-3 one (11 to 16).
pub(crate) fn dolby_frame(h: &[u8]) -> Option<Dolby> {
    if h.len() < 8 || h[0] != 0x0b || h[1] != 0x77 {
        return None;
    }
    let bsid = (h[5] >> 3) as u32;
    let mut r = Bits::new(&h[2..]);
    if bsid <= 10 {
        // the CRC, fscod, frmsizecod, bsid, bsmod, acmod, the mix levels
        // that acmod has, lfeon
        r.skip(16)?;
        let fscod = r.u(2)?;
        let frmsizecod = r.u(6)?;
        if fscod == 3 || frmsizecod >= 38 {
            return None;
        }
        r.skip(5)?;
        let bsmod = r.u(3)?;
        let acmod = r.u(3)?;
        if acmod & 1 != 0 && acmod != 1 {
            r.skip(2)?;
        }
        if acmod & 4 != 0 {
            r.skip(2)?;
        }
        if acmod == 2 {
            r.skip(2)?;
        }
        let lfeon = r.u(1)?;
        let words = AC3_SIZES[frmsizecod as usize / 2][fscod as usize] as u32 + if fscod == 1 { frmsizecod & 1 } else { 0 };
        let channels = DOLBY_CHANNELS[acmod as usize] + lfeon;
        Some(Dolby { eac3: false, bytes: words * 2, rate: AC3_RATES[fscod as usize], samples: 1536, channels, follows: false, fscod, frmsizecod, bsid, bsmod, acmod, lfeon })
    } else if bsid <= 16 {
        // strmtyp, substreamid, frmsiz, fscod, numblkscod (or for the half
        // rates fscod2, with six blocks), acmod, lfeon
        let strmtyp = r.u(2)?;
        let substream = r.u(3)?;
        let frmsiz = r.u(11)?;
        let fscod = r.u(2)?;
        let code = r.u(2)?;
        let (rate, blocks) = if fscod == 3 { ([24000, 22050, 16000].get(code as usize).copied()?, 6) } else { (AC3_RATES[fscod as usize], [1, 2, 3, 6][code as usize]) };
        let acmod = r.u(3)?;
        let lfeon = r.u(1)?;
        let channels = DOLBY_CHANNELS[acmod as usize] + lfeon;
        let follows = strmtyp == 1 || substream != 0;
        Some(Dolby { eac3: true, bytes: (frmsiz + 1) * 2, rate, samples: 256 * blocks, channels, follows, fscod, frmsizecod: 0, bsid, bsmod: 0, acmod, lfeon })
    } else {
        None
    }
}

/// `ac-3` + `dac3` from the head of an AC-3 frame; with its rate and
/// channel count.
pub(crate) fn ac3_entry(frame: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let d = dolby_frame(frame).filter(|d| !d.eac3)?;
    let dac3 = [((d.fscod << 6) | (d.bsid << 1) | (d.bsmod >> 2)) as u8, (((d.bsmod & 3) << 6) | (d.acmod << 3) | (d.lfeon << 2) | ((d.frmsizecod >> 1) >> 3)) as u8, (((d.frmsizecod >> 1) & 7) << 5) as u8];
    Some((
        audio_entry(b"ac-3", d.channels, d.rate, |w| {
            let at = w.begin_box(b"dac3");
            w.bytes(&dac3);
            w.end_box(at);
        }),
        d.rate,
        d.channels,
    ))
}

/// `ec-3` + `dec3` from the head of an E-AC-3 frame (one independent
/// substream); with its rate, channel count and samples per frame.
pub(crate) fn eac3_entry(frame: &[u8]) -> Option<(Vec<u8>, u32, u32, u32)> {
    let d = dolby_frame(frame).filter(|d| d.eac3)?;
    // kbit/s from the frame size
    let data_rate = (d.bytes as u64 * 8 * d.rate as u64 / d.samples as u64 / 1000) as u32;
    let mut w = Writer::new();
    // data_rate(13) num_ind_sub(3) = 0 (one), then fscod(2) bsid(5) reserved(1) asvc(1) bsmod(3) acmod(3) lfeon(1) reserved(3) num_dep_sub(4) reserved(1)
    w.u16(((data_rate.min(0x1fff)) << 3) as u16);
    w.u24((d.fscod << 22) | (d.bsid << 17) | (d.acmod << 9) | (d.lfeon << 8));
    let dec3 = w.buf;
    Some((
        audio_entry(b"ec-3", d.channels, d.rate, |w| {
            let at = w.begin_box(b"dec3");
            w.bytes(&dec3);
            w.end_box(at);
        }),
        d.rate,
        d.channels,
        d.samples,
    ))
}

/// An MP4 sample entry for audio an encoder made (WebCodecs' codec string
/// and decoderConfig description): AAC, Opus or FLAC.
pub fn encoded_audio_entry(codec: &str, description: &[u8], rate: u32, channels: u32) -> Result<Vec<u8>, Error> {
    if let Some(rest) = codec.strip_prefix("mp4a.40.") {
        let asc = if description.is_empty() { make_asc(rest.parse().unwrap_or(2), rate, channels, None) } else { description.to_vec() };
        return Ok(aac_entry(&asc, rate, channels));
    }
    match codec {
        "opus" => {
            let head = if description.len() >= 19 && description.starts_with(b"OpusHead") { description.to_vec() } else { make_opus_head(channels, 312, rate) };
            opus_entry(&head)
        }
        "flac" => flac_streaminfo(description).map(flac_entry).ok_or_else(|| "FLAC without a STREAMINFO".to_string()),
        other => Err(format!("no MP4 sample entry for {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{from_sample_entry, CodecInfo};

    /// The codec an audio sample entry built here names (version 0: 28
    /// bytes of fixed fields, then the codec's boxes).
    fn codec_of_entry(entry: &[u8]) -> Result<CodecInfo, Error> {
        from_sample_entry(&entry[4..8].try_into().unwrap(), &entry[8 + 28..])
    }

    #[test]
    fn asc_round_trip() {
        let asc = make_asc(2, 48000, 2, None);
        assert_eq!(asc, vec![0x11, 0x90]);
        // 7.1 is channel configuration 7 (8 is reserved)
        assert_eq!(make_asc(2, 48000, 8, None), vec![0x11, 0xb8]);
        let a = parse_asc(&asc).unwrap();
        assert_eq!((a.aot, a.rate, a.sbr_rate, a.short_frames), (2, 48000, None, false));
        let he = make_asc(2, 24000, 2, Some(48000));
        let a = parse_asc(&he).unwrap();
        assert_eq!((a.aot, a.rate), (2, 24000));
        // explicit hierarchical SBR: object type 5 first
        let a = parse_asc(&[0x2b, 0x92, 0x08, 0x00]).unwrap();
        assert_eq!((a.aot, a.rate, a.sbr_rate), (2, 22050, Some(44100)));
    }

    #[test]
    fn entries_parse_back() {
        let e = aac_entry(&make_asc(2, 44100, 2, None), 44100, 2);
        let c = codec_of_entry(&e).unwrap();
        assert_eq!(c.codec, "mp4a.40.2");
        assert_eq!(c.description.as_deref(), Some(&make_asc(2, 44100, 2, None)[..]));
        assert_eq!(codec_of_entry(&mpeg_audio_entry(44100, 2)).unwrap().codec, "mp3");
        let head = make_opus_head(2, 312, 48000);
        assert_eq!(codec_of_entry(&opus_entry(&head).unwrap()).unwrap().codec, "opus");
        // AC-3 at 48 kHz, 5.1 (acmod 7 + LFE), 384 kbit/s
        let frame = [0x0b, 0x77, 0, 0, 0x1c, 0x40, 0xe1, 0xf0];
        let (e, rate, ch) = ac3_entry(&frame).unwrap();
        assert_eq!((rate, ch), (48000, 6));
        assert_eq!(codec_of_entry(&e).unwrap().codec, "ac-3");
        // dac3: fscod 0, bsid 8, bsmod 0, acmod 7, lfeon, bit rate code 14
        assert_eq!(&e[e.len() - 3..], &[0x10, 0x3d, 0xc0]);
        assert_eq!(dolby_frame(&frame).map(|d| (d.bytes, d.samples, d.follows)), Some((1536, 1536, false)));
        // E-AC-3: an independent substream (6 blocks at 48 kHz, 5.1, 256
        // bytes), then a dependent one that goes with it
        let ind = [0x0b, 0x77, 0x00, 0x7f, 0x3f, 0x80, 0, 0];
        let (e, rate, ch, samples) = eac3_entry(&ind).unwrap();
        assert_eq!((rate, ch, samples), (48000, 6, 1536));
        assert_eq!(codec_of_entry(&e).unwrap().codec, "ec-3");
        // dec3: 64 kbit/s, one independent substream; fscod 0, bsid 16, acmod 7, lfeon
        assert_eq!(&e[e.len() - 5..], &[0x02, 0x00, 0x20, 0x0f, 0x00]);
        let dep = dolby_frame(&[0x0b, 0x77, 0x40, 0x3f, 0x3f, 0x80, 0, 0]).unwrap();
        assert_eq!((dep.bytes, dep.follows), (128, true));
        assert!(ac3_entry(&ind).is_none() && eac3_entry(&frame).is_none());
    }

    #[test]
    fn packet_lengths() {
        assert_eq!(opus_packet_samples(&[0xfc]), Some(960)); // CELT 20 ms, one frame
        assert_eq!(opus_packet_samples(&[0x0b]), None); // code 3 without its frame count
        assert_eq!(opus_packet_samples(&[0x0b, 0x02]), Some(1920)); // SILK 20 ms, two frames
        assert_eq!(opus_packet_samples(&[0x7b, 0x03]), Some(2880)); // hybrid 20 ms, three frames
        assert_eq!(opus_packet_samples(&[0x81]), Some(240)); // CELT 2.5 ms, two frames (code 1)
        assert_eq!(mpeg_audio_frame(&[0xff, 0xfb, 0x90, 0x64]), Some((44100, 2, 1152)));
        // DTS: 16 blocks (512 samples), 2013 bytes, stereo (amode 2) at 48 kHz, with LFE
        let mut h = vec![0x7f, 0xfe, 0x80, 0x01];
        let bits: u64 = (1 << 63) | (31 << 58) | (15 << 50) | (2012 << 36) | (2 << 30) | (13 << 26) | (15 << 21) | (1 << 9);
        h.extend_from_slice(&bits.to_be_bytes()[..7]);
        assert_eq!(dts_frame(&h), Some((48000, 3, 512, 2013)));
        assert_eq!(mpeg_audio_frame(&[0xff, 0xf3, 0x84, 0xc4]), Some((24000, 1, 576)));
    }
}
