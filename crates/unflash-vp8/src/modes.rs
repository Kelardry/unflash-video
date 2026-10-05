//! Macroblock prediction records from the first partition: segment, skip
//! flag, intra modes, and the reference frame and motion vectors of inter
//! macroblocks (RFC 6386 sections 10, 11, 16 and 17).

use crate::bool_decoder::BoolDecoder;
use crate::header::{EntropyContext, Segmentation};
use crate::tables::*;

/// A motion vector in quarter luma samples (eighth chroma samples). Sums
/// wrap at 16 bits as in ffmpeg, which only matters for pictures more than
/// 8192 samples wide.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Mv {
    pub x: i16,
    pub y: i16,
}

impl Mv {
    pub const ZERO: Mv = Mv { x: 0, y: 0 };

    fn is_zero(self) -> bool {
        self == Mv::ZERO
    }
}

/// Reference frames, as stored in [`MbInfo::ref_frame`].
pub const INTRA: u8 = 0;
pub const LAST: u8 = 1;
pub const GOLDEN: u8 = 2;
pub const ALTREF: u8 = 3;

/// One macroblock's prediction record.
#[derive(Clone, Copy, Debug)]
pub struct MbInfo {
    /// A luma intra mode or an inter mode (`tables::DC_PRED` ..).
    pub y_mode: u8,
    pub uv_mode: u8,
    pub ref_frame: u8,
    pub segment: u8,
    /// `mb_skip_coeff`: the macroblock has no coefficients.
    pub skip: bool,
    /// The partitioning of a `SPLITMV` macroblock.
    pub split: u8,
    /// Subblock intra modes; for 16x16 modes the subblock mode each
    /// implies, which the key-frame contexts of later macroblocks use.
    pub bmodes: [u8; 16],
    /// Subblock motion vectors in raster order: all alike unless the
    /// macroblock is split, all zero for intra macroblocks.
    pub mvs: [Mv; 16],
}

impl MbInfo {
    /// What a macroblock outside the picture looks like to its neighbours:
    /// intra, with no motion and `B_DC_PRED` subblocks.
    pub const OUTSIDE: MbInfo = MbInfo { y_mode: DC_PRED, uv_mode: DC_PRED, ref_frame: INTRA, segment: 0, skip: false, split: 0, bmodes: [B_DC_PRED; 16], mvs: [Mv::ZERO; 16] };

    /// The macroblock's motion vector as its neighbours see it (a split
    /// macroblock's last subblock vector).
    #[inline]
    pub fn mv(&self) -> Mv {
        self.mvs[15]
    }
}

impl Default for MbInfo {
    fn default() -> Self {
        MbInfo::OUTSIDE
    }
}

/// The frame-level parameters of the macroblock records.
pub struct ModeParams<'a> {
    pub key_frame: bool,
    pub segmentation: &'a Segmentation,
    pub skip_prob: Option<u8>,
    pub prob_intra: u8,
    pub prob_last: u8,
    pub prob_golden: u8,
    pub probs: &'a EntropyContext,
    /// Sign bias by reference frame (intra, last, golden, alt-ref).
    pub sign_bias: [bool; 4],
    pub mb_w: usize,
    pub mb_h: usize,
}

/// The subblock mode a 16x16 luma mode implies for the key-frame contexts
/// of its neighbours.
fn implied_bmode(y_mode: u8) -> u8 {
    match y_mode {
        V_PRED => B_VE_PRED,
        H_PRED => B_HE_PRED,
        TM_PRED => B_TM_PRED,
        _ => B_DC_PRED,
    }
}

/// Read the record of macroblock (`mb_x`, `mb_y`) given its neighbours
/// above, to the left and above-left (`MbInfo::OUTSIDE` beyond the
/// picture). `segment` is the macroblock's entry of the segment map: read
/// when the map is updated, kept when segmentation is on without an
/// update, and 0 when it is off.
#[allow(clippy::too_many_arguments)]
pub fn read_mb(bd: &mut BoolDecoder, p: &ModeParams, mb_x: usize, mb_y: usize, above: &MbInfo, left: &MbInfo, above_left: &MbInfo, segment: &mut u8) -> MbInfo {
    let seg = p.segmentation;
    if seg.update_map {
        *segment = bd.read_tree(&SEGMENT_TREE, &seg.tree_probs);
    } else if !seg.enabled {
        *segment = 0;
    }
    let mut mb = MbInfo { segment: *segment, ..MbInfo::OUTSIDE };
    mb.skip = match p.skip_prob {
        Some(prob) => bd.read(prob),
        None => false,
    };

    if p.key_frame {
        mb.y_mode = bd.read_tree(&KF_YMODE_TREE, &KF_YMODE_PROBS);
        if mb.y_mode == B_PRED {
            for b in 0..16 {
                let a = if b < 4 { above.bmodes[b + 12] } else { mb.bmodes[b - 4] };
                let l = if b & 3 == 0 { left.bmodes[b + 3] } else { mb.bmodes[b - 1] };
                mb.bmodes[b] = bd.read_tree(&BMODE_TREE, &KF_BMODE_PROBS[a as usize][l as usize]);
            }
        } else {
            mb.bmodes = [implied_bmode(mb.y_mode); 16];
        }
        mb.uv_mode = bd.read_tree(&UV_MODE_TREE, &KF_UV_MODE_PROBS);
        return mb;
    }

    if !bd.read(p.prob_intra) {
        mb.y_mode = bd.read_tree(&YMODE_TREE, &p.probs.ymode);
        if mb.y_mode == B_PRED {
            for m in mb.bmodes.iter_mut() {
                *m = bd.read_tree(&BMODE_TREE, &BMODE_PROBS);
            }
        } else {
            mb.bmodes = [implied_bmode(mb.y_mode); 16];
        }
        mb.uv_mode = bd.read_tree(&UV_MODE_TREE, &p.probs.uv_mode);
        return mb;
    }

    mb.ref_frame = if bd.read(p.prob_last) { GOLDEN + bd.read(p.prob_golden) as u8 } else { LAST };
    read_inter_modes(bd, p, mb_x, mb_y, above, left, above_left, &mut mb);
    mb
}

/// 16.3: the motion vectors of the neighbours (above, left, above-left),
/// with the sign flipped for references of the other sign bias, merged
/// into up to three distinct candidates weighted 2, 2 and 1. Returns the
/// candidates (entry 0 is the zero vector) and their weights.
fn find_near_mvs(p: &ModeParams, ref_frame: u8, above: &MbInfo, left: &MbInfo, above_left: &MbInfo) -> ([Mv; 4], [usize; 4]) {
    let mut near = [Mv::ZERO; 4];
    let mut cnt = [0usize; 4];
    let mut idx = 0;
    for (n, edge) in [above, left, above_left].into_iter().enumerate() {
        if edge.ref_frame == INTRA {
            continue;
        }
        let weight = if n == 2 { 1 } else { 2 };
        let mut mv = edge.mv();
        if mv.is_zero() {
            cnt[0] += weight;
            continue;
        }
        if p.sign_bias[edge.ref_frame as usize] != p.sign_bias[ref_frame as usize] {
            mv = Mv { x: mv.x.wrapping_neg(), y: mv.y.wrapping_neg() };
        }
        if n == 0 || mv != near[idx] {
            idx += 1;
            near[idx] = mv;
        }
        cnt[idx] += weight;
    }
    (near, cnt)
}

/// 18.1: keep a predicted vector within 16 samples (plus the block) of the
/// picture, as the bounds clipped to 16 bits in ffmpeg.
fn clamp_mv(mv: Mv, mb_x: usize, mb_y: usize, p: &ModeParams) -> Mv {
    let bound = |pos: usize, count: usize| {
        let lo = (-64 - 64 * pos as i64).max(i16::MIN as i64);
        let hi = (64 * (count as i64 - 1 - pos as i64) + 64).min(i16::MAX as i64);
        (lo as i16, hi as i16)
    };
    let (x0, x1) = bound(mb_x, p.mb_w);
    let (y0, y1) = bound(mb_y, p.mb_h);
    Mv { x: mv.x.clamp(x0, x1), y: mv.y.clamp(y0, y1) }
}

/// 16.3 and 16.4: the inter mode and motion vectors of `mb`, whose
/// reference frame is known.
#[allow(clippy::too_many_arguments)]
fn read_inter_modes(bd: &mut BoolDecoder, p: &ModeParams, mb_x: usize, mb_y: usize, above: &MbInfo, left: &MbInfo, above_left: &MbInfo, mb: &mut MbInfo) {
    let (mut near, mut cnt) = find_near_mvs(p, mb.ref_frame, above, left, above_left);
    let mv = if !bd.read(MODE_CONTEXTS[cnt[0]][0]) {
        mb.y_mode = ZEROMV;
        Mv::ZERO
    } else {
        // three distinct candidates: the above-left one counts for the
        // nearest when it equals the above one
        if cnt[3] > 0 && near[1] == near[3] {
            cnt[1] += 1;
        }
        if cnt[2] > cnt[1] {
            cnt.swap(1, 2);
            near.swap(1, 2);
        }
        if !bd.read(MODE_CONTEXTS[cnt[1]][1]) {
            mb.y_mode = NEARESTMV;
            clamp_mv(near[1], mb_x, mb_y, p)
        } else if !bd.read(MODE_CONTEXTS[cnt[2]][2]) {
            mb.y_mode = NEARMV;
            clamp_mv(near[2], mb_x, mb_y, p)
        } else {
            let best = clamp_mv(if cnt[1] >= cnt[0] { near[1] } else { Mv::ZERO }, mb_x, mb_y, p);
            let splits = (above.y_mode == SPLITMV) as usize * 2 + (left.y_mode == SPLITMV) as usize * 2 + (above_left.y_mode == SPLITMV) as usize;
            if bd.read(MODE_CONTEXTS[splits][3]) {
                mb.y_mode = SPLITMV;
                read_split_mvs(bd, p, best, above, left, mb);
                return;
            }
            mb.y_mode = NEWMV;
            read_mv(bd, &p.probs.mv, best)
        }
    };
    mb.mvs = [mv; 16];
}

/// 16.4: the partitioning and subblock vectors of a split macroblock.
fn read_split_mvs(bd: &mut BoolDecoder, p: &ModeParams, best: Mv, above: &MbInfo, left: &MbInfo, mb: &mut MbInfo) {
    mb.split = bd.read_tree(&SPLIT_MV_TREE, &SPLIT_MV_PROBS);
    let parts = &MV_PARTITIONS[mb.split as usize];
    for part in 0..MV_PARTITION_COUNT[mb.split as usize] {
        // the first subblock of the partition decides the context
        let k = parts.iter().position(|&q| q as usize == part).unwrap_or(0);
        let l = if k & 3 == 0 { left.mvs[k + 3] } else { mb.mvs[k - 1] };
        let a = if k < 4 { above.mvs[k + 12] } else { mb.mvs[k - 4] };
        let ctx = if l == a {
            if l.is_zero() {
                4
            } else {
                3
            }
        } else if a.is_zero() {
            2
        } else if l.is_zero() {
            1
        } else {
            0
        };
        let mv = match bd.read_tree(&SUBMV_REF_TREE, &SUBMV_REF_PROBS[ctx]) {
            LEFT4X4 => l,
            ABOVE4X4 => a,
            ZERO4X4 => Mv::ZERO,
            _ => read_mv(bd, &p.probs.mv, best),
        };
        for (b, &q) in parts.iter().enumerate() {
            if q as usize == part {
                mb.mvs[b] = mv;
            }
        }
    }
}

/// 17.1: a motion vector (row first) relative to `base`.
fn read_mv(bd: &mut BoolDecoder, probs: &[[u8; 19]; 2], base: Mv) -> Mv {
    let y = read_mv_component(bd, &probs[0]);
    let x = read_mv_component(bd, &probs[1]);
    Mv { x: base.x.wrapping_add(x), y: base.y.wrapping_add(y) }
}

/// 17.1: one motion vector component: a short magnitude coded with a
/// tree, or a long one bit by bit (bit 3 last, implied when no higher bit
/// is set), then the sign.
fn read_mv_component(bd: &mut BoolDecoder, p: &[u8; 19]) -> i16 {
    const IS_SHORT: usize = 0;
    const SIGN: usize = 1;
    const SHORT: usize = 2;
    const BITS: usize = 9;
    let mut x: i16;
    if bd.read(p[IS_SHORT]) {
        x = 0;
        for i in 0..3 {
            x += (bd.read(p[BITS + i]) as i16) << i;
        }
        for i in (4..10).rev() {
            x += (bd.read(p[BITS + i]) as i16) << i;
        }
        if x & !15 == 0 || bd.read(p[BITS + 3]) {
            x += 8;
        }
    } else {
        x = bd.read_tree(&SMALL_MV_TREE, &p[SHORT..BITS]) as i16;
    }
    if x != 0 && bd.read(p[SIGN]) {
        -x
    } else {
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn near_mv_search_merges_and_flips() {
        let seg = Segmentation::default();
        let probs = EntropyContext::default();
        let p = ModeParams { key_frame: false, segmentation: &seg, skip_prob: None, prob_intra: 0, prob_last: 0, prob_golden: 0, probs: &probs, sign_bias: [false, false, true, false], mb_w: 4, mb_h: 4 };
        let inter = |r: u8, x: i16, y: i16| MbInfo { ref_frame: r, mvs: [Mv { x, y }; 16], y_mode: NEWMV, ..MbInfo::OUTSIDE };
        // above and left alike: one candidate of weight 4, above-left another of weight 1
        let (near, cnt) = find_near_mvs(&p, LAST, &inter(LAST, 4, 2), &inter(LAST, 4, 2), &inter(LAST, -8, 0));
        assert_eq!((near[1], near[2], cnt), (Mv { x: 4, y: 2 }, Mv { x: -8, y: 0 }, [0, 4, 1, 0]));
        // the golden frame has the other sign bias: its vector is flipped
        let (near, cnt) = find_near_mvs(&p, LAST, &inter(GOLDEN, 4, 2), &MbInfo::OUTSIDE, &inter(LAST, 0, 0));
        assert_eq!((near[1], cnt), (Mv { x: -4, y: -2 }, [1, 2, 0, 0]));
    }

    #[test]
    fn clamping_bounds() {
        let seg = Segmentation::default();
        let probs = EntropyContext::default();
        let p = ModeParams { key_frame: false, segmentation: &seg, skip_prob: None, prob_intra: 0, prob_last: 0, prob_golden: 0, probs: &probs, sign_bias: [false; 4], mb_w: 3, mb_h: 2 };
        assert_eq!(clamp_mv(Mv { x: -500, y: 500 }, 0, 0, &p), Mv { x: -64, y: 128 });
        assert_eq!(clamp_mv(Mv { x: 500, y: -500 }, 2, 1, &p), Mv { x: 64, y: -128 });
    }
}
