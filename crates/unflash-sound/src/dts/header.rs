//! Finding frames: the core frame's sync word in its four packings, its
//! bit stream header (clause 5.4.2), and the header of the DTS-HD
//! extension substream (clause 7.5.2), which is only stepped over.

use crate::bits::Bits;
use crate::dts::tables;

/// How a core frame is laid out in bytes. The standard's form is 16-bit
/// big endian words; CD and WAV rips also carry byte-swapped words, and
/// words of which only the low 14 bits carry data (the top two repeat
/// bit 13), so that the stream is harmless noise if played as PCM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packing {
    Be16,
    Le16,
    Be14,
    Le14,
}

/// The sync word of an extension substream (clause 7.4.1).
pub const SUBSTREAM_SYNC: [u8; 4] = [0x64, 0x58, 0x20, 0x25];

/// The packing of a core sync word at the start of `data`, if one is
/// there: 0x7FFE8001, or 0x1FFFE800 followed by 0x07Fx in 14-bit words
/// (the sync extension of clause 5.3 in part), or either byte-swapped.
pub fn core_sync(data: &[u8]) -> Option<Packing> {
    if data.len() < 6 {
        return None;
    }
    match data[..4] {
        [0x7f, 0xfe, 0x80, 0x01] => Some(Packing::Be16),
        [0xfe, 0x7f, 0x01, 0x80] => Some(Packing::Le16),
        [0x1f, 0xff, 0xe8, 0x00] if data[4] == 0x07 && data[5] & 0xf0 == 0xf0 => Some(Packing::Be14),
        [0xff, 0x1f, 0x00, 0xe8] if data[5] == 0x07 && data[4] & 0xf0 == 0xf0 => Some(Packing::Le14),
        _ => None,
    }
}

/// Whether a frame of any kind starts at `data[pos..]`.
pub fn any_sync(data: &[u8], pos: usize) -> bool {
    data.get(pos..pos + 4) == Some(&SUBSTREAM_SYNC[..]) || data.get(pos..).is_some_and(|d| core_sync(d).is_some())
}

/// `raw` (a frame in `packing`, from its sync word) as 16-bit big endian
/// bytes, as far as `raw` goes (a 14-bit frame's last word may be part
/// full; a byte-swapped frame of an odd length ends with a whole word).
pub fn to_be16(raw: &[u8], packing: Packing, out: &mut Vec<u8>) {
    out.clear();
    let word = |i: usize| -> u16 {
        let (a, b) = (raw[2 * i], raw[2 * i + 1]);
        match packing {
            Packing::Be16 | Packing::Be14 => u16::from_be_bytes([a, b]),
            Packing::Le16 | Packing::Le14 => u16::from_le_bytes([a, b]),
        }
    };
    let words = raw.len() / 2;
    match packing {
        Packing::Be16 => out.extend_from_slice(raw),
        Packing::Le16 => {
            for i in 0..words {
                out.extend_from_slice(&word(i).to_be_bytes());
            }
        }
        Packing::Be14 | Packing::Le14 => {
            let (mut acc, mut n) = (0u32, 0u32);
            for i in 0..words {
                acc = (acc << 14) | (word(i) & 0x3fff) as u32;
                n += 14;
                while n >= 8 {
                    n -= 8;
                    out.push((acc >> n) as u8);
                }
                acc &= (1 << n) - 1;
            }
        }
    }
}

/// How many bytes of `packing` hold `bytes` bytes of the 16-bit form.
pub fn raw_len(bytes: usize, packing: Packing) -> usize {
    match packing {
        Packing::Be16 => bytes,
        Packing::Le16 => bytes.div_ceil(2) * 2,
        Packing::Be14 | Packing::Le14 => (bytes * 8).div_ceil(14) * 2,
    }
}

/// The bit stream header of a core frame (Table 5-1), every field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    /// FTYPE: a normal frame (1) or a termination frame (0).
    pub normal: bool,
    /// SHORT: the deficit sample count (31 in a normal frame).
    pub deficit: u8,
    /// CPF: the frame carries CRC words (which are not checked).
    pub crc_present: bool,
    /// NBLKS + 1: blocks of 32 samples in the frame.
    pub blocks: usize,
    /// FSIZE + 1: bytes in the frame (16-bit form), extensions included.
    pub bytes: usize,
    pub amode: u8,
    pub sfreq: u8,
    pub sample_rate: u32,
    /// RATE (Table 5-7): the target bit rate; 31 is the lossless one,
    /// which chooses the lossless step sizes.
    pub rate: u8,
    /// The reserved bit after RATE (the embedded downmix flag of V1.2.1).
    pub fixed_bit: bool,
    /// DYNF: a dynamic range coefficient in each subframe.
    pub dynamic_range: bool,
    /// TIMEF: a time stamp after the audio data.
    pub time_stamp: bool,
    /// AUXF: auxiliary data after the audio data.
    pub aux: bool,
    pub hdcd: bool,
    /// EXT_AUDIO_ID and EXT_AUDIO: the core extension after the audio
    /// data (0 XCh, 2 X96, 6 XXCh).
    pub ext_audio_id: u8,
    pub ext_audio: bool,
    /// ASPF: a DSYNC word after each subsubframe, not only each subframe.
    pub dsync_every_subsubframe: bool,
    /// LFF: no LFE (0), LFE interpolated 128 times (1) or 64 times (2).
    pub lff: u8,
    /// HFLAG: predict from the previous frame's history.
    pub predictor_history: bool,
    /// FILTS: the perfect reconstruction filter bank.
    pub perfect: bool,
    pub vernum: u8,
    pub copy_history: u8,
    pub pcm_resolution: u8,
    /// SUMF, SUMS: front and surround pairs sum/difference coded.
    pub sum_front: bool,
    pub sum_surround: bool,
    /// DIALNORM (or UNSPEC): the dialogue normalization gain (Table
    /// 5-20), which this decoder does not apply (nor does ffmpeg's).
    pub dialnorm: u8,
    /// Where the primary audio coding header starts, in bits.
    pub end: usize,
}

impl Header {
    /// Samples per channel in the frame.
    pub fn samples(&self) -> usize {
        32 * self.blocks
    }
}

/// Why a header is not a frame's.
pub type BadHeader = &'static str;

/// Parse the bit stream header of a core frame in 16-bit big endian form
/// (`data` from the sync word; 20 bytes are enough). Fields out of their
/// valid range make it fail, so that it also tells a real frame from a
/// chance sync word.
pub fn parse(data: &[u8]) -> Result<Header, BadHeader> {
    if data.len() < 4 || data[..4] != [0x7f, 0xfe, 0x80, 0x01] {
        return Err("no sync word");
    }
    let mut b = Bits::new(data);
    b.skip(32);
    let mut h = Header { normal: b.flag(), deficit: b.read(5) as u8, crc_present: b.flag(), ..Default::default() };
    h.blocks = b.read(7) as usize + 1;
    h.bytes = b.read(14) as usize + 1;
    h.amode = b.read(6) as u8;
    h.sfreq = b.read(4) as u8;
    h.sample_rate = tables::SAMPLE_RATES[h.sfreq as usize];
    h.rate = b.read(5) as u8;
    h.fixed_bit = b.flag();
    h.dynamic_range = b.flag();
    h.time_stamp = b.flag();
    h.aux = b.flag();
    h.hdcd = b.flag();
    h.ext_audio_id = b.read(3) as u8;
    h.ext_audio = b.flag();
    h.dsync_every_subsubframe = b.flag();
    h.lff = b.read(2) as u8;
    h.predictor_history = b.flag();
    if h.crc_present {
        b.skip(16); // HCRC
    }
    h.perfect = b.flag();
    h.vernum = b.read(4) as u8;
    h.copy_history = b.read(2) as u8;
    h.pcm_resolution = b.read(3) as u8;
    h.sum_front = b.flag();
    h.sum_surround = b.flag();
    h.dialnorm = b.read(4) as u8;
    h.end = b.position();
    if b.overrun() {
        return Err("header cut off");
    }
    if h.normal && h.deficit != 31 {
        return Err("a normal frame with a deficit sample count");
    }
    if h.blocks < 6 {
        return Err("fewer than 6 blocks (NBLKS below 5)");
    }
    if h.bytes < 96 {
        return Err("a frame size below 96 bytes (FSIZE below 95)");
    }
    if h.sample_rate == 0 {
        return Err("invalid sample rate code");
    }
    if h.lff == 3 {
        return Err("invalid LFE flag");
    }
    Ok(h)
}

/// The size of an extension substream frame (clause 7.5.2) at the start
/// of `data`, once its header's CRC checks; `None` if there is none.
pub fn substream_size(data: &[u8]) -> Option<usize> {
    if data.len() < 12 || data[..4] != SUBSTREAM_SYNC {
        return None;
    }
    let mut b = Bits::new(&data[..12]);
    b.skip(32 + 8 + 2); // sync, UserDefinedBits, nExtSSIndex
    let long = b.flag();
    let header = b.read(if long { 12 } else { 8 }) as usize + 1;
    let size = b.read(if long { 20 } else { 16 }) as usize + 1;
    if header < 8 || header > size || header > data.len() {
        return None;
    }
    // CRC-16 from nExtSSIndex (byte 5) to the byte before the CRC word
    let stored = u16::from_be_bytes([data[header - 2], data[header - 1]]);
    (crc16(&data[5..header - 2]) == stored).then_some(size)
}

/// The CRC of Annex B: x^16 + x^12 + x^5 + 1 from 0xFFFF, bits most
/// significant first.
pub fn crc16(data: &[u8]) -> u16 {
    let mut r = 0xffffu16;
    for &byte in data {
        r ^= (byte as u16) << 8;
        for _ in 0..8 {
            r = if r & 0x8000 != 0 { (r << 1) ^ 0x1021 } else { r << 1 };
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_is_ccitt() {
        // the check value of CRC-16/CCITT-FALSE
        assert_eq!(crc16(b"123456789"), 0x29b1);
    }

    #[test]
    fn packings_convert_to_the_standard_form() {
        let be: Vec<u8> = (0..28u8).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
        let mut out = Vec::new();
        // byte-swapped
        let le: Vec<u8> = be.chunks(2).flat_map(|w| [w[1], w[0]]).collect();
        to_be16(&le, Packing::Le16, &mut out);
        assert_eq!(out, be);
        // 14 bits a word: 28 bytes are 16 words of 14 bits
        let mut words = Vec::new();
        let bits: Vec<u8> = be.iter().flat_map(|b| (0..8).rev().map(move |k| (b >> k) & 1)).collect();
        for chunk in bits.chunks(14) {
            let mut w = chunk.iter().fold(0u16, |a, &b| a << 1 | b as u16) << (14 - chunk.len());
            if w & 0x2000 != 0 {
                w |= 0xc000;
            }
            words.push(w);
        }
        assert_eq!(raw_len(be.len(), Packing::Be14), words.len() * 2);
        let raw: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
        to_be16(&raw, Packing::Be14, &mut out);
        assert_eq!(out, be);
        let raw: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        to_be16(&raw, Packing::Le14, &mut out);
        assert_eq!(out, be);
        // the sync words
        assert_eq!(core_sync(&[0x7f, 0xfe, 0x80, 0x01, 0xfc, 0x3c]), Some(Packing::Be16));
        assert_eq!(core_sync(&[0xfe, 0x7f, 0x01, 0x80, 0x3c, 0xfc]), Some(Packing::Le16));
        assert_eq!(core_sync(&[0x1f, 0xff, 0xe8, 0x00, 0x07, 0xf1]), Some(Packing::Be14));
        assert_eq!(core_sync(&[0xff, 0x1f, 0x00, 0xe8, 0xf1, 0x07]), Some(Packing::Le14));
        assert_eq!(core_sync(&[0x1f, 0xff, 0xe8, 0x00, 0x00, 0x00]), None);
    }
}
