//! DTS's part of `common`, for its tests and examples: the comparison of
//! two decodes, a writer of core frames from a plan of every syntax
//! element (for the frames ffmpeg's encoder does not make), and the
//! containers FATE's samples come in.

use unflash_sound::dts::testing::{Book, AUDIO, BIT_ALLOC, SCALES, TMODE};
use unflash_sound::dts::{Decoded, Decoder, Features, Output, StreamInfo};

use super::BitWriter;

/// How far apart this decoder's and ffmpeg's samples may be, at full
/// scale 1: ffmpeg's core decoder dequantizes and predicts in fixed point
/// (24-bit samples with a few more bits), this one in floating point.
pub const TOLERANCE: f64 = 1e-4;

/// One run of our decoder over a whole stream.
pub struct Ours {
    pub out: Vec<Vec<f32>>,
    pub decoded: Decoded,
    pub features: Features,
    pub skipped_substream_frames: u64,
    pub info: Option<StreamInfo>,
}

/// Decode `data` all at once.
pub fn decode(data: &[u8], output: Output) -> Ours {
    let mut dec = Decoder::new(output);
    let mut out = Vec::new();
    let decoded = dec.decode(data, &mut out).expect("a core frame");
    let (features, skipped) = dec.features();
    Ours { out, decoded, features, skipped_substream_frames: skipped, info: dec.info() }
}

// ---------------------------------------------------------------------
// The comparison.

/// How one output channel compares with ffmpeg's.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChannelStats {
    pub samples: usize,
    /// The largest difference, and where.
    pub max_diff: f64,
    pub at: usize,
    /// rms of the difference, and of ffmpeg's signal.
    pub rms_diff: f64,
    pub rms: f64,
    /// The largest sample of ffmpeg's.
    pub peak: f64,
}

/// Compare `ours[c]` with `reference[map[c]]` sample by sample (`None`
/// leaves a channel out), over their common length.
pub fn compare(ours: &[Vec<f32>], reference: &[Vec<f32>], map: &[Option<usize>]) -> Vec<ChannelStats> {
    let mut stats = vec![ChannelStats::default(); ours.len()];
    for (c, s) in stats.iter_mut().enumerate() {
        let Some(r) = map.get(c).copied().flatten() else { continue };
        let (a, b) = (&ours[c], &reference[r]);
        let n = a.len().min(b.len());
        let (mut sq, mut sq_ref) = (0f64, 0f64);
        for i in 0..n {
            let d = (a[i] as f64 - b[i] as f64).abs();
            if d > s.max_diff {
                s.max_diff = d;
                s.at = i;
            }
            sq += d * d;
            sq_ref += (b[i] as f64).powi(2);
            s.peak = s.peak.max((b[i] as f64).abs());
        }
        s.samples = n;
        s.rms_diff = (sq / n.max(1) as f64).sqrt();
        s.rms = (sq_ref / n.max(1) as f64).sqrt();
    }
    stats
}

/// A short line about a channel's comparison.
pub fn describe(s: &ChannelStats) -> String {
    let mut line =
        format!("{} samples, max diff {:.1e} (at {}), rms diff {:.1e}; ffmpeg's rms {:.3}, peak {:.3}", s.samples, s.max_diff, s.at, s.rms_diff, s.rms, s.peak);
    if s.max_diff > TOLERANCE {
        line += &format!("  OVER {TOLERANCE:.0e}");
    }
    line
}

// ---------------------------------------------------------------------
// Building frames.

impl BitWriter {
    /// Write `level`'s code from a Huffman book.
    pub fn huffman(&mut self, book: Book, level: i32) {
        let &(_, len, code) = book.iter().find(|e| e.0 as i32 == level).unwrap_or_else(|| panic!("level {level} not in a book of {}", book.len()));
        self.put(len as u32, code as u32);
    }
}

/// The primary audio coding header's choices for one channel (Table
/// 5-21).
#[derive(Clone, Debug)]
pub struct Channel {
    /// nSUBS (2 to 32), nVQSUB (1 to 32), JOINX (0, or source + 1).
    pub subs: usize,
    pub vqsub: usize,
    pub joinx: usize,
    pub thuff: usize,
    pub shuff: usize,
    pub bhuff: usize,
    /// SEL and ADJ (the index, 0 to 3) for ABITS 1 to 10.
    pub sel: [usize; 10],
    pub adj: [usize; 10],
}

impl Default for Channel {
    fn default() -> Self {
        Channel { subs: 32, vqsub: 32, joinx: 0, thuff: 0, shuff: 6, bhuff: 6, sel: [0; 10], adj: [0; 10] }
    }
}

/// One subband of one channel in one subframe.
#[derive(Clone, Debug, Default)]
pub struct Band {
    pub pmode: bool,
    pub pvq: u16,
    pub abits: u8,
    pub tmode: u8,
    /// Scale factor table indexes (the second when there is a transient).
    pub scales: [i32; 2],
    /// The high frequency vector (subbands from nVQSUB).
    pub hfvq: u16,
    /// 8 × nSSC quantization indexes.
    pub levels: Vec<i32>,
}

#[derive(Clone, Debug, Default)]
pub struct Subframe {
    pub nssc: usize,
    pub psc: u8,
    /// [channel][subband 0 to 31].
    pub bands: Vec<Vec<Band>>,
    /// JOIN_SHUFF, and each joint subband's code (-64 to 64 for the
    /// Huffman books, the index for the linear ones) per channel.
    pub join_shuff: Vec<usize>,
    pub join_codes: Vec<Vec<i32>>,
    /// V1.2.1's DOWN indexes, written when the reserved bit is set.
    pub down: Vec<[u32; 2]>,
    pub range: u8,
    /// 2 × LFF × nSSC decimated LFE samples and their scale index.
    pub lfe: Vec<i32>,
    pub lfe_scale: u32,
}

/// The auxiliary data (5.8.2).
#[derive(Clone, Debug, Default)]
pub struct Aux {
    pub time_stamp: Option<u64>,
    /// nPrmChDownMixType and the 9-bit coefficient codes.
    pub downmix: Option<(u8, Vec<u32>)>,
    /// Spoil the CRC.
    pub bad_crc: bool,
}

/// A core frame, every syntax element.
#[derive(Clone, Debug)]
pub struct Frame {
    pub normal: bool,
    pub deficit: u8,
    pub cpf: bool,
    /// NBLKS + 1 (None: what the subframes add up to).
    pub blocks: Option<usize>,
    pub amode: u8,
    pub sfreq: u8,
    pub rate: u8,
    pub fixed_bit: bool,
    pub dynf: bool,
    pub time_stamp: Option<u32>,
    pub aux: Option<Aux>,
    pub hdcd: bool,
    pub ext_audio_id: u8,
    /// Core extension data after the audio (EXT_AUDIO), DWORD aligned.
    pub extension: Option<Vec<u8>>,
    pub aspf: bool,
    pub lff: u8,
    pub hflag: bool,
    pub filts: bool,
    pub vernum: u8,
    pub chist: u8,
    pub pcmr: u8,
    pub sumf: bool,
    pub sums: bool,
    pub dialnorm: u8,
    pub channels: Vec<Channel>,
    pub subframes: Vec<Subframe>,
    /// Bytes to pad the frame to (FSIZE + 1); None: as few as it takes.
    pub size: Option<usize>,
}

impl Default for Frame {
    fn default() -> Self {
        Frame {
            normal: true,
            deficit: 31,
            cpf: false,
            blocks: None,
            amode: 2,
            sfreq: 13,
            rate: 22,
            fixed_bit: false,
            dynf: false,
            time_stamp: None,
            aux: None,
            hdcd: false,
            ext_audio_id: 0,
            extension: None,
            aspf: false,
            lff: 0,
            hflag: true,
            filts: false,
            vernum: 7,
            chist: 0,
            pcmr: 6,
            sumf: false,
            sums: false,
            dialnorm: 0,
            channels: Vec::new(),
            subframes: Vec::new(),
            size: None,
        }
    }
}

/// Levels of the quantizers ABITS 1 to 10, and the SEL value that is a
/// block code (1 to 7) or plain (8 to 10) (Table 5-26).
pub const LEVELS: [i32; 10] = [3, 5, 7, 9, 13, 17, 25, 33, 65, 129];
pub fn special_sel(abits: usize) -> usize {
    match abits {
        1 => 1,
        2..=5 => 3,
        _ => 7,
    }
}
const BLOCK_BITS: [u32; 7] = [7, 10, 12, 13, 15, 17, 19];

/// The range of quantization indexes a subband with `abits` and `sel`
/// can hold.
pub fn level_range(abits: usize, sel: usize) -> (i32, i32) {
    match abits {
        0 => (0, 0),
        1..=10 if sel < special_sel(abits) || abits <= 7 => {
            let h = (LEVELS[abits - 1] - 1) / 2;
            (-h, h)
        }
        _ => {
            let bits = abits as u32 - 3;
            (-(1 << (bits - 1)), (1 << (bits - 1)) - 1)
        }
    }
}

/// Write a frame (the 16-bit big endian form).
pub fn write_frame(f: &Frame) -> Vec<u8> {
    let nch = f.channels.len();
    let mut w = BitWriter::default();
    w.put(32, 0x7ffe8001);
    w.put(1, f.normal as u32);
    w.put(5, f.deficit as u32);
    w.put(1, f.cpf as u32);
    let blocks = f.blocks.unwrap_or_else(|| f.subframes.iter().map(|s| 8 * s.nssc).sum());
    w.put(7, blocks as u32 - 1);
    let fsize_at = w.bits();
    w.put(14, 0);
    w.put(6, f.amode as u32);
    w.put(4, f.sfreq as u32);
    w.put(5, f.rate as u32);
    w.put(1, f.fixed_bit as u32);
    w.put(1, f.dynf as u32);
    w.put(1, f.time_stamp.is_some() as u32);
    w.put(1, f.aux.is_some() as u32);
    w.put(1, f.hdcd as u32);
    w.put(3, f.ext_audio_id as u32);
    w.put(1, f.extension.is_some() as u32);
    w.put(1, f.aspf as u32);
    w.put(2, f.lff as u32);
    w.put(1, f.hflag as u32);
    if f.cpf {
        w.put(16, 0xa5c3); // HCRC, not checked
    }
    w.put(1, f.filts as u32);
    w.put(4, f.vernum as u32);
    w.put(2, f.chist as u32);
    w.put(3, f.pcmr as u32);
    w.put(1, f.sumf as u32);
    w.put(1, f.sums as u32);
    w.put(4, f.dialnorm as u32);
    // primary audio coding header
    w.put(4, f.subframes.len() as u32 - 1);
    w.put(3, nch as u32 - 1);
    for c in &f.channels {
        w.put(5, c.subs as u32 - 2);
    }
    for c in &f.channels {
        w.put(5, c.vqsub as u32 - 1);
    }
    for c in &f.channels {
        w.put(3, c.joinx as u32);
    }
    for c in &f.channels {
        w.put(2, c.thuff as u32);
    }
    for c in &f.channels {
        w.put(3, c.shuff as u32);
    }
    for c in &f.channels {
        w.put(3, c.bhuff as u32);
    }
    for n in 0..10 {
        let bits = match n {
            0 => 1,
            1..=4 => 2,
            _ => 3,
        };
        for c in &f.channels {
            w.put(bits, c.sel[n] as u32);
        }
    }
    for n in 0..10 {
        for c in &f.channels {
            if c.sel[n] < special_sel(n + 1) {
                w.put(2, c.adj[n] as u32);
            }
        }
    }
    if f.cpf {
        w.put(16, 0x5a3c); // AHCRC
    }
    for s in &f.subframes {
        write_subframe(&mut w, f, s);
    }
    // optional information
    if let Some(t) = f.time_stamp {
        w.put(32, t);
    }
    if let Some(aux) = &f.aux {
        let bytes = aux_bytes(aux);
        w.put(6, bytes.len() as u32);
        w.align(32);
        for b in bytes {
            w.put(8, b as u32);
        }
    }
    if f.cpf && f.dynf {
        w.put(16, 0x3c5a); // OCRC
    }
    if let Some(ext) = &f.extension {
        w.align(32);
        for &b in ext {
            w.put(8, b as u32);
        }
    }
    w.align(8);
    let mut bytes = w.bytes;
    if let Some(size) = f.size {
        assert!(size >= bytes.len(), "the frame takes {} bytes, more than {size}", bytes.len());
        bytes.resize(size, 0);
    }
    if bytes.len() < 96 {
        bytes.resize(96, 0);
    }
    let fsize = bytes.len() as u32 - 1;
    let mut w = BitWriter { bytes, bits: 0 };
    w.set(fsize_at, 14, fsize);
    w.bytes
}

fn write_subframe(w: &mut BitWriter, f: &Frame, s: &Subframe) {
    let chans = &f.channels;
    w.put(2, s.nssc as u32 - 1);
    w.put(3, s.psc as u32);
    for (c, ch) in chans.iter().enumerate() {
        for n in 0..ch.subs {
            w.put(1, s.bands[c][n].pmode as u32);
        }
    }
    for (c, ch) in chans.iter().enumerate() {
        for n in 0..ch.subs {
            if s.bands[c][n].pmode {
                w.put(12, s.bands[c][n].pvq as u32);
            }
        }
    }
    for (c, ch) in chans.iter().enumerate() {
        for n in 0..ch.vqsub {
            let a = s.bands[c][n].abits as u32;
            match ch.bhuff {
                5 => w.put(4, a),
                6 => w.put(5, a),
                book => w.huffman(BIT_ALLOC[book], a as i32),
            }
        }
    }
    if s.nssc > 1 {
        for (c, ch) in chans.iter().enumerate() {
            for n in 0..ch.vqsub {
                if s.bands[c][n].abits > 0 {
                    w.huffman(TMODE[ch.thuff], s.bands[c][n].tmode as i32);
                }
            }
        }
    }
    for (c, ch) in chans.iter().enumerate() {
        let mut prev = 0;
        let mut scale = |w: &mut BitWriter, index: i32| {
            match ch.shuff {
                5 => w.put(6, index as u32),
                6 => w.put(7, index as u32),
                book => w.huffman(SCALES[book], index - prev),
            }
            prev = index;
        };
        for n in 0..ch.vqsub {
            let b = &s.bands[c][n];
            if b.abits > 0 {
                scale(w, b.scales[0]);
                if b.tmode > 0 {
                    scale(w, b.scales[1]);
                }
            }
        }
        for n in ch.vqsub..ch.subs {
            scale(w, s.bands[c][n].scales[0]);
        }
    }
    for (c, ch) in chans.iter().enumerate() {
        if ch.joinx > 0 {
            w.put(3, s.join_shuff[c] as u32);
        }
    }
    for (c, ch) in chans.iter().enumerate() {
        if ch.joinx > 0 {
            for &v in &s.join_codes[c] {
                match s.join_shuff[c] {
                    5 => w.put(6, v as u32),
                    6 => w.put(7, v as u32),
                    book => w.huffman(SCALES[book], v),
                }
            }
        }
    }
    if f.fixed_bit && chans.len() > 2 {
        for d in &s.down {
            w.put(7, d[0]);
            w.put(7, d[1]);
        }
    }
    if f.dynf {
        w.put(8, s.range as u32);
    }
    if f.cpf {
        w.put(16, 0xc35a); // SICRC
    }
    for (c, ch) in chans.iter().enumerate() {
        for n in ch.vqsub..ch.subs {
            w.put(10, s.bands[c][n].hfvq as u32);
        }
    }
    if f.lff > 0 {
        assert_eq!(s.lfe.len(), 2 * f.lff as usize * s.nssc);
        for &v in &s.lfe {
            w.put(8, v as u32 & 0xff);
        }
        w.put(8, s.lfe_scale);
    }
    for sub in 0..s.nssc {
        for (c, ch) in chans.iter().enumerate() {
            for n in 0..ch.vqsub {
                let b = &s.bands[c][n];
                let abits = b.abits as usize;
                if abits == 0 {
                    continue;
                }
                let levels = &b.levels[8 * sub..8 * sub + 8];
                let sel = if abits <= 10 { ch.sel[abits - 1] } else { 0 };
                if abits <= 10 && sel < special_sel(abits) {
                    for &l in levels {
                        w.huffman(AUDIO[abits - 1][sel], l);
                    }
                } else if abits <= 7 {
                    let l = LEVELS[abits - 1];
                    let offset = (l - 1) / 2;
                    for half in 0..2 {
                        let code = levels[4 * half..4 * half + 4].iter().rev().fold(0u32, |acc, &v| acc * l as u32 + (v + offset) as u32);
                        w.put(BLOCK_BITS[abits - 1], code);
                    }
                } else {
                    let bits = abits as u32 - 3;
                    for &l in levels {
                        w.put(bits, (l as u32) & ((1 << bits) - 1));
                    }
                }
            }
        }
        if sub == s.nssc - 1 || f.aspf {
            w.put(16, 0xffff);
        }
    }
}

/// The bytes of the auxiliary data (5.8.2), sync word to CRC.
pub fn aux_bytes(aux: &Aux) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put(32, 0x9a1105a0);
    w.put(1, aux.time_stamp.is_some() as u32);
    if let Some(t) = aux.time_stamp {
        w.align(4);
        w.put(8, (t >> 28) as u32 & 0xff);
        w.put(4, 0b1011);
        w.put(28, t as u32 & 0x0fff_ffff);
        w.put(4, 0b1011);
    }
    w.put(1, aux.downmix.is_some() as u32);
    if let Some((kind, codes)) = &aux.downmix {
        w.put(3, *kind as u32);
        for &c in codes {
            w.put(9, c);
        }
    }
    w.align(8);
    let mut crc = unflash_sound::dts::testing::crc16(&w.bytes[4..]);
    if aux.bad_crc {
        crc ^= 1;
    }
    w.put(16, crc as u32);
    w.bytes
}

// ---------------------------------------------------------------------
// Containers and streams.

/// The core frames of an elementary stream of 16-bit big endian frames
/// (whole frames only), found by their sizes; DTS-HD substreams after
/// each core frame count as part of it.
pub fn frames(data: &[u8]) -> Vec<std::ops::Range<usize>> {
    let mut v = Vec::new();
    let mut pos = 0;
    while pos + 16 <= data.len() {
        assert_eq!(&data[pos..pos + 4], &[0x7f, 0xfe, 0x80, 0x01], "no core frame at byte {pos}");
        let size = frame_size(&data[pos..]).unwrap();
        let mut end = pos + size;
        // extension substreams (and the zero bytes that align them)
        loop {
            let mut p = end;
            while p < data.len() && p < end + 3 && data[p] == 0 {
                p += 1;
            }
            if data.get(p..p + 4) == Some(&[0x64, 0x58, 0x20, 0x25][..]) {
                end = p + substream_size(&data[p..]);
            } else {
                break;
            }
        }
        if end > data.len() {
            break;
        }
        v.push(pos..end);
        pos = end;
    }
    v
}

/// A core frame's size (FSIZE + 1) from its first bytes.
pub fn frame_size(f: &[u8]) -> Option<usize> {
    if f.len() < 10 || f[..4] != [0x7f, 0xfe, 0x80, 0x01] {
        return None;
    }
    let bits = u64::from_be_bytes(f[4..12].try_into().unwrap());
    Some(((bits >> (64 - 1 - 5 - 1 - 7 - 14)) & 0x3fff) as usize + 1)
}

/// An extension substream frame's size (nuExtSSFsize).
pub fn substream_size(f: &[u8]) -> usize {
    let bits = u64::from_be_bytes(f[4..12].try_into().unwrap());
    let long = (bits >> (64 - 11)) & 1 == 1;
    if long {
        ((bits >> (64 - 11 - 12 - 20)) & 0xfffff) as usize + 1
    } else {
        ((bits >> (64 - 11 - 8 - 16)) & 0xffff) as usize + 1
    }
}

/// The audio of a DTS-HD file (the `.dtshd` of FATE's dcadec suite):
/// the STRMDATA chunk, as ffmpeg's demuxer reads it (the file's other
/// chunks, BLACKOUT among them, hold a frame of their own).
pub fn dtshd_stream(data: &[u8]) -> Option<&[u8]> {
    if !data.starts_with(b"DTSHDHDR") {
        return None;
    }
    let mut pos = 0;
    while pos + 16 <= data.len() {
        let size = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap()) as usize;
        let body = pos + 16..(pos + 16 + size).min(data.len());
        if &data[pos..pos + 8] == b"STRMDATA" {
            return Some(&data[body]);
        }
        pos = body.end;
    }
    None
}

/// The PES payloads of an MPEG transport stream's first audio stream
/// (the first PID whose PES has stream_id 0xBD or 0xC0 to 0xDF), each a
/// whole number of frames, as a demuxer hands them over.
pub fn ts_payloads(data: &[u8]) -> Vec<Vec<u8>> {
    ts_audio(data).1
}

/// `ts_payloads`, and the PID they are on.
pub fn ts_audio(data: &[u8]) -> (u16, Vec<Vec<u8>>) {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut pid_of_audio: Option<u16> = None;
    let mut cur: Option<Vec<u8>> = None;
    for p in data.chunks_exact(188) {
        if p[0] != 0x47 {
            continue;
        }
        let pid = ((p[1] as u16 & 0x1f) << 8) | p[2] as u16;
        let start = p[1] & 0x40 != 0;
        let afc = (p[3] >> 4) & 3;
        let mut i = 4;
        if afc & 2 != 0 {
            i += 1 + p[4] as usize;
        }
        if afc & 1 == 0 || i >= 188 {
            continue;
        }
        let payload = &p[i..];
        if start && payload.len() > 9 && payload[..3] == [0, 0, 1] {
            let sid = payload[3];
            if pid_of_audio.is_none() && (sid == 0xbd || (0xc0..=0xdf).contains(&sid)) {
                pid_of_audio = Some(pid);
            }
            if Some(pid) == pid_of_audio {
                if let Some(c) = cur.take() {
                    out.push(c);
                }
                let header = 9 + payload[8] as usize;
                cur = Some(payload.get(header..).unwrap_or(&[]).to_vec());
            }
        } else if Some(pid) == pid_of_audio {
            if let Some(c) = cur.as_mut() {
                c.extend_from_slice(payload);
            }
        }
    }
    if let Some(c) = cur {
        out.push(c);
    }
    (pid_of_audio.unwrap_or(0), out)
}

/// Where the LFE channel is in the WAVE order of an arrangement: after
/// its front channels.
pub fn lfe_index(amode: u8) -> usize {
    match amode {
        0 => 1,
        5 | 7 | 9 | 11 | 12 => 3,
        _ => 2,
    }
}
