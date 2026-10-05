//! The frame header: the uncompressed chunk every frame starts with (RFC
//! 6386 section 9.1) and the fields at the start of the first partition
//! (9.2 to 9.11, 19.2), with the state they update from frame to frame.

use crate::bool_decoder::BoolDecoder;
use crate::tables::{AC_Q, COEFF_UPDATE_PROBS, DC_Q, DEFAULT_COEFF_PROBS, DEFAULT_MV_PROBS, DEFAULT_UV_MODE_PROBS, DEFAULT_YMODE_PROBS, MV_UPDATE_PROBS};
use crate::{Error, Result};

/// The probabilities that carry over from frame to frame. A key frame
/// resets them; a frame with `refresh_entropy_probs` clear updates them for
/// itself only (the decoder restores the saved copy afterwards).
#[derive(Clone)]
pub struct EntropyContext {
    /// [block type][band][context][token tree node]
    pub coeff: [[[[u8; 11]; 3]; 8]; 4],
    /// [row, column][component probability]
    pub mv: [[u8; 19]; 2],
    pub ymode: [u8; 4],
    pub uv_mode: [u8; 3],
}

impl Default for EntropyContext {
    fn default() -> Self {
        EntropyContext { coeff: DEFAULT_COEFF_PROBS, mv: DEFAULT_MV_PROBS, ymode: DEFAULT_YMODE_PROBS, uv_mode: DEFAULT_UV_MODE_PROBS }
    }
}

/// 9.3: segmentation. The quantiser and filter level adjustments persist
/// until updated (fields left out of an update read as 0); a key frame
/// clears everything.
#[derive(Clone, Default)]
pub struct Segmentation {
    pub enabled: bool,
    pub update_map: bool,
    /// The values below replace the base values rather than adjust them.
    pub absolute: bool,
    pub quant: [i32; 4],
    pub filter_level: [i32; 4],
    pub tree_probs: [u8; 3],
}

/// 9.6: loop filter level adjustments by reference frame and prediction
/// mode. They persist until updated; a key frame clears them.
#[derive(Clone, Default)]
pub struct FilterDeltas {
    pub enabled: bool,
    /// Intra, last, golden, alt-ref.
    pub ref_frame: [i32; 4],
    /// `B_PRED`, `ZEROMV`, the other whole-macroblock vectors, `SPLITMV`.
    pub mode: [i32; 4],
}

/// Everything the frame header updates for the frames after it.
#[derive(Clone, Default)]
pub struct State {
    pub probs: EntropyContext,
    pub segmentation: Segmentation,
    pub deltas: FilterDeltas,
}

/// 9.1: the three bytes every frame starts with.
#[derive(Clone, Copy, Debug)]
pub struct FrameTag {
    pub key_frame: bool,
    /// 0: six-tap filter; 1, 2: bilinear; 3: bilinear with whole-sample
    /// chroma vectors. The loop filter type comes from the header, not from
    /// here (as in libvpx and ffmpeg).
    pub version: u8,
    pub show_frame: bool,
    pub first_part_size: usize,
}

/// A frame's header fields (the persistent ones are in [`State`]).
pub struct FrameHeader {
    pub tag: FrameTag,
    /// Key frames: the picture size, and the scaling a player may apply
    /// (informational: the decoded picture is not scaled).
    pub width: u32,
    pub height: u32,
    pub scale: (u8, u8),
    /// Key frames: `color_space` (0 is YCbCr as BT.601; 1 is reserved) and
    /// `clamping_type` (1 promises reconstruction never needs clamping, so
    /// clamping anyway changes nothing).
    pub color_space: bool,
    pub clamping_type: bool,
    pub simple_filter: bool,
    pub filter_level: i32,
    pub sharpness: i32,
    pub q_index: i32,
    /// Quantiser index deltas: Y1 DC, Y2 DC, Y2 AC, chroma DC, chroma AC.
    pub q_deltas: [i32; 5],
    pub refresh_golden: bool,
    pub refresh_altref: bool,
    /// 0: none, 1: from the last frame, 2: from the alt-ref (golden) or
    /// golden (alt-ref) frame.
    pub copy_to_golden: u8,
    pub copy_to_altref: u8,
    pub sign_bias_golden: bool,
    pub sign_bias_altref: bool,
    pub refresh_entropy: bool,
    pub refresh_last: bool,
    /// The probability of `mb_skip_coeff` = 0, when the flag is coded.
    pub skip_prob: Option<u8>,
    pub prob_intra: u8,
    pub prob_last: u8,
    pub prob_golden: u8,
}

/// Dequantisation factors of one segment: [DC, AC] for luma, the second
/// order luma DC block and chroma.
#[derive(Clone, Copy, Default, Debug)]
pub struct Dequant {
    pub y1: [i32; 2],
    pub y2: [i32; 2],
    pub uv: [i32; 2],
}

/// The most token partitions a frame can have.
pub const MAX_PARTITIONS: usize = 8;

/// A parsed frame: the header, the first partition positioned at the
/// macroblock records, and the token partitions.
pub struct Parsed<'a> {
    pub header: FrameHeader,
    pub first: BoolDecoder<'a>,
    pub partitions: [&'a [u8]; MAX_PARTITIONS],
    pub num_partitions: usize,
    /// The probabilities to restore after the frame (`refresh_entropy_probs`
    /// clear), as they were before this frame's updates.
    pub saved_probs: Option<EntropyContext>,
}

/// 9.1: the frame tag.
pub fn parse_tag(data: &[u8]) -> Result<FrameTag> {
    if data.len() < 3 {
        return Err(Error::Bitstream("frame shorter than its tag"));
    }
    let raw = data[0] as u32 | (data[1] as u32) << 8 | (data[2] as u32) << 16;
    Ok(FrameTag { key_frame: raw & 1 == 0, version: ((raw >> 1) & 7) as u8, show_frame: (raw >> 4) & 1 == 1, first_part_size: (raw >> 5) as usize })
}

/// Parse a frame's headers, updating `st` (the caller keeps it only when
/// this succeeds).
pub fn parse_frame<'a>(data: &'a [u8], st: &mut State) -> Result<Parsed<'a>> {
    let tag = parse_tag(data)?;
    let mut pos = 3;
    let (mut width, mut height, mut scale) = (0, 0, (0, 0));
    if tag.key_frame {
        if data.len() < 10 {
            return Err(Error::Bitstream("key frame shorter than its header"));
        }
        if data[3..6] != [0x9d, 0x01, 0x2a] {
            return Err(Error::Bitstream("key frame start code missing"));
        }
        let w = u16::from_le_bytes([data[6], data[7]]);
        let h = u16::from_le_bytes([data[8], data[9]]);
        width = (w & 0x3fff) as u32;
        height = (h & 0x3fff) as u32;
        scale = ((w >> 14) as u8, (h >> 14) as u8);
        if width == 0 || height == 0 {
            return Err(Error::Bitstream("key frame of zero size"));
        }
        pos = 10;
    }
    if tag.first_part_size > data.len() - pos {
        return Err(Error::Bitstream("first partition runs past the frame"));
    }
    let mut bd = BoolDecoder::new(&data[pos..pos + tag.first_part_size]);
    let rest = &data[pos + tag.first_part_size..];

    let (mut color_space, mut clamping_type) = (false, false);
    if tag.key_frame {
        *st = State::default();
        color_space = bd.read_flag();
        clamping_type = bd.read_flag();
    }

    let seg = &mut st.segmentation;
    seg.enabled = bd.read_flag();
    if seg.enabled {
        seg.update_map = bd.read_flag();
        let update_data = bd.read_flag();
        if update_data {
            seg.absolute = bd.read_flag();
            for q in seg.quant.iter_mut() {
                *q = bd.read_optional_signed(7);
            }
            for l in seg.filter_level.iter_mut() {
                *l = bd.read_optional_signed(6);
            }
        }
        if seg.update_map {
            for p in seg.tree_probs.iter_mut() {
                *p = if bd.read_flag() { bd.read_literal(8) as u8 } else { 255 };
            }
        }
    } else {
        seg.update_map = false;
    }

    let simple_filter = bd.read_flag();
    let filter_level = bd.read_literal(6) as i32;
    let sharpness = bd.read_literal(3) as i32;
    let deltas = &mut st.deltas;
    deltas.enabled = bd.read_flag();
    if deltas.enabled && bd.read_flag() {
        // deltas not sent keep their values (the reference decoder's
        // zeroing is not what libvpx and ffmpeg do)
        for d in deltas.ref_frame.iter_mut().chain(deltas.mode.iter_mut()) {
            if bd.read_flag() {
                let v = bd.read_literal(6) as i32;
                *d = if bd.read_flag() { -v } else { v };
            }
        }
    }

    // 9.5: the token partitions, all but the last with a 3-byte size
    let num_partitions = 1usize << bd.read_literal(2);
    let table = 3 * (num_partitions - 1);
    if rest.len() < table {
        return Err(Error::Bitstream("token partition sizes run past the frame"));
    }
    let mut partitions: [&[u8]; MAX_PARTITIONS] = [&[]; MAX_PARTITIONS];
    let mut p = table;
    for (i, part) in partitions.iter_mut().enumerate().take(num_partitions) {
        let size = if i + 1 < num_partitions { rest[3 * i] as usize | (rest[3 * i + 1] as usize) << 8 | (rest[3 * i + 2] as usize) << 16 } else { rest.len() - p };
        if size > rest.len() - p {
            return Err(Error::Bitstream("token partition runs past the frame"));
        }
        *part = &rest[p..p + size];
        p += size;
    }

    let q_index = bd.read_literal(7) as i32;
    let mut q_deltas = [0; 5];
    for d in q_deltas.iter_mut() {
        *d = bd.read_optional_signed(4);
    }

    let (mut refresh_golden, mut refresh_altref, mut copy_to_golden, mut copy_to_altref) = (true, true, 0, 0);
    let (mut sign_bias_golden, mut sign_bias_altref) = (false, false);
    if !tag.key_frame {
        refresh_golden = bd.read_flag();
        refresh_altref = bd.read_flag();
        if !refresh_golden {
            copy_to_golden = bd.read_literal(2) as u8;
        }
        if !refresh_altref {
            copy_to_altref = bd.read_literal(2) as u8;
        }
        sign_bias_golden = bd.read_flag();
        sign_bias_altref = bd.read_flag();
    }
    let refresh_entropy = bd.read_flag();
    let refresh_last = tag.key_frame || bd.read_flag();
    let saved_probs = if refresh_entropy { None } else { Some(st.probs.clone()) };

    // 13.4: coefficient probability updates
    for (i, types) in st.probs.coeff.iter_mut().enumerate() {
        for (j, bands) in types.iter_mut().enumerate() {
            for (k, ctx) in bands.iter_mut().enumerate() {
                for (l, prob) in ctx.iter_mut().enumerate() {
                    if bd.read(COEFF_UPDATE_PROBS[i][j][k][l]) {
                        *prob = bd.read_literal(8) as u8;
                    }
                }
            }
        }
    }

    let skip_prob = if bd.read_flag() { Some(bd.read_literal(8) as u8) } else { None };
    let (mut prob_intra, mut prob_last, mut prob_golden) = (0, 0, 0);
    if !tag.key_frame {
        prob_intra = bd.read_literal(8) as u8;
        prob_last = bd.read_literal(8) as u8;
        prob_golden = bd.read_literal(8) as u8;
        if bd.read_flag() {
            for p in st.probs.ymode.iter_mut() {
                *p = bd.read_literal(8) as u8;
            }
        }
        if bd.read_flag() {
            for p in st.probs.uv_mode.iter_mut() {
                *p = bd.read_literal(8) as u8;
            }
        }
        // 17.2: motion vector probability updates, 7 bits scaled to 8
        for (i, comp) in st.probs.mv.iter_mut().enumerate() {
            for (j, prob) in comp.iter_mut().enumerate() {
                if bd.read(MV_UPDATE_PROBS[i][j]) {
                    let v = bd.read_literal(7) as u8;
                    *prob = if v == 0 { 1 } else { v << 1 };
                }
            }
        }
    }

    let header = FrameHeader {
        tag,
        width,
        height,
        scale,
        color_space,
        clamping_type,
        simple_filter,
        filter_level,
        sharpness,
        q_index,
        q_deltas,
        refresh_golden,
        refresh_altref,
        copy_to_golden,
        copy_to_altref,
        sign_bias_golden,
        sign_bias_altref,
        refresh_entropy,
        refresh_last,
        skip_prob,
        prob_intra,
        prob_last,
        prob_golden,
    };
    Ok(Parsed { header, first: bd, partitions, num_partitions, saved_probs })
}

/// 9.6 and 14.1: the dequantisation factors of the four segments (all
/// alike without segmentation). As ffmpeg does, the segment's index and
/// each delta are added before clamping to 0..=127 (libvpx clamps the
/// segment's index on its own first; streams never tell them apart).
pub fn dequant_factors(h: &FrameHeader, seg: &Segmentation) -> [Dequant; 4] {
    let mut out = [Dequant::default(); 4];
    for (s, dq) in out.iter_mut().enumerate() {
        let base = match (seg.enabled, seg.absolute) {
            (false, _) => h.q_index,
            (true, true) => seg.quant[s],
            (true, false) => h.q_index + seg.quant[s],
        };
        let q = |delta: i32| (base + delta).clamp(0, 127) as usize;
        let [y1_dc, y2_dc, y2_ac, uv_dc, uv_ac] = h.q_deltas;
        dq.y1 = [DC_Q[q(y1_dc)], AC_Q[q(0)]];
        dq.y2 = [DC_Q[q(y2_dc)] * 2, (AC_Q[q(y2_ac)] * 155 / 100).max(8)];
        dq.uv = [DC_Q[q(uv_dc)].min(132), AC_Q[q(uv_ac)]];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_fields() {
        // key frame, version 0, shown, first partition 0x123 bytes
        let raw: u32 = (0x123 << 5) | (1 << 4);
        let t = parse_tag(&[raw as u8, (raw >> 8) as u8, (raw >> 16) as u8]).unwrap();
        assert!(t.key_frame && t.show_frame);
        assert_eq!((t.version, t.first_part_size), (0, 0x123));
        let raw: u32 = 1 | (3 << 1);
        let t = parse_tag(&[raw as u8, 0, 0]).unwrap();
        assert!(!t.key_frame && !t.show_frame);
        assert_eq!(t.version, 3);
        assert!(parse_tag(&[0, 0]).is_err());
    }

    #[test]
    fn truncated_frames_are_errors() {
        let mut st = State::default();
        // key frame claiming a 100-byte first partition in a 12-byte frame
        let raw: u32 = (100 << 5) | (1 << 4);
        let frame = [raw as u8, (raw >> 8) as u8, (raw >> 16) as u8, 0x9d, 0x01, 0x2a, 16, 0, 16, 0, 0, 0];
        assert!(parse_frame(&frame, &mut st).is_err());
        let mut bad_code = frame;
        bad_code[4] = 0;
        assert!(parse_frame(&bad_code, &mut st).is_err());
        assert!(parse_frame(&frame[..9], &mut st).is_err());
    }

    #[test]
    fn quantiser_limits() {
        let raw: u32 = 1 << 4;
        let frame = [raw as u8, 0, 0, 0x9d, 0x01, 0x2a, 16, 0, 16, 0];
        let mut st = State::default();
        let mut h = parse_frame(&frame, &mut st).unwrap().header;
        h.q_index = 127;
        h.q_deltas = [15, 15, 15, 15, 15];
        let dq = dequant_factors(&h, &st.segmentation)[0];
        assert_eq!(dq.y1, [157, 284]);
        assert_eq!(dq.y2, [314, 284 * 155 / 100]);
        assert_eq!(dq.uv, [132, 284]);
        h.q_index = 0;
        h.q_deltas = [-15, -15, -15, -15, -15];
        let dq = dequant_factors(&h, &st.segmentation)[0];
        assert_eq!((dq.y2[1], dq.uv[0]), (8, 4));
    }
}
