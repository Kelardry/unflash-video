//! Motion vector prediction (8.5.3.2): merge candidates, advanced motion
//! vector prediction, and the temporal candidates from the collocated
//! picture.

use crate::ctu::{PartMode, SliceDecoder};
use crate::meta::{Motion, INTRA};
use crate::picture::{Picture, Sample, COL_L0, COL_L1, COL_LT0, COL_LT1};
use crate::slice::SliceType;

/// l0CandIdx / l1CandIdx of the combined bi-predictive candidates (Table 8-7).
const COMB: [(usize, usize); 12] = [(0, 1), (1, 0), (0, 2), (2, 0), (1, 2), (2, 1), (0, 3), (3, 0), (1, 3), (3, 1), (2, 3), (3, 2)];

/// Scale a motion vector by the ratio of two order count distances
/// (8-179 .. 8-181).
fn scale(mv: [i16; 2], td: i32, tb: i32) -> [i16; 2] {
    let td = td.clamp(-128, 127);
    let tb = tb.clamp(-128, 127);
    if td == 0 {
        return mv;
    }
    let tx = (16384 + (td.abs() >> 1)) / td;
    let f = ((tb * tx + 32) >> 6).clamp(-4096, 4095);
    mv.map(|c| {
        let p = f * c as i32;
        (p.signum() * ((p.abs() + 127) >> 8)).clamp(-32768, 32767) as i16
    })
}

impl<'a, P: Sample> SliceDecoder<'a, P> {
    /// 6.4.2: whether the prediction block covering (`xn`, `yn`) is
    /// available to prediction block `part_idx` (at `xp`, `yp`, `w`×`h`)
    /// of the coding block at (`xc`, `yc`) of size `nc`, and inter coded.
    #[allow(clippy::too_many_arguments)]
    fn pb_available(&self, xc: i32, yc: i32, nc: i32, xp: i32, yp: i32, w: i32, h: i32, part_idx: usize, xn: i32, yn: i32) -> bool {
        let same_cb = xc <= xn && yc <= yn && xn < xc + nc && yn < yc + nc;
        let available = if !same_cb {
            self.z_available(xp, yp, xn, yn)
        } else {
            // the second of four prediction blocks cannot see the third
            !((w << 1) == nc && (h << 1) == nc && part_idx == 1 && yc + h <= yn && xc + w > xn)
        };
        available && self.meta.flags[self.meta.at(xn as usize, yn as usize)] & INTRA == 0
    }

    #[inline]
    fn motion_at(&self, x: i32, y: i32) -> Motion {
        self.meta.motion[self.meta.at(x as usize, y as usize)]
    }

    /// 8.5.3.2.2: the motion of a merged prediction block.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn merge_motion(&self, xc: usize, yc: usize, nc: usize, xp: usize, yp: usize, w: usize, h: usize, part_idx: usize, merge_idx: usize) -> Motion {
        let (orig_w, orig_h) = (w, h);
        let (xc, yc, nc) = (xc as i32, yc as i32, nc as i32);
        let (mut xp, mut yp, mut w, mut h, mut part_idx) = (xp as i32, yp as i32, w as i32, h as i32, part_idx);
        let par = self.pps.log2_parallel_merge_level;
        if par > 2 && nc == 8 {
            // one merge candidate list for all prediction blocks of the coding unit
            (xp, yp, w, h, part_idx) = (xc, yc, nc, nc, 0);
        }
        let part = self.cu_part();
        let same_region = |xn: i32, yn: i32| (xp >> par) == (xn >> par) && (yp >> par) == (yn >> par);
        let avail = |xn: i32, yn: i32| !same_region(xn, yn) && self.pb_available(xc, yc, nc, xp, yp, w, h, part_idx, xn, yn);
        let mut cands = [Motion::NONE; 5];
        let mut count = 0;
        // spatial candidates (8.5.3.2.3)
        let (xa1, ya1) = (xp - 1, yp + h - 1);
        let av_a1 = !(matches!(part, PartMode::PNx2N | PartMode::PnLx2N | PartMode::PnRx2N) && part_idx == 1) && avail(xa1, ya1);
        let a1 = if av_a1 { self.motion_at(xa1, ya1) } else { Motion::NONE };
        if av_a1 {
            cands[count] = a1;
            count += 1;
        }
        let (xb1, yb1) = (xp + w - 1, yp - 1);
        let av_b1 = !(matches!(part, PartMode::P2NxN | PartMode::P2NxnU | PartMode::P2NxnD) && part_idx == 1) && avail(xb1, yb1);
        let b1 = if av_b1 { self.motion_at(xb1, yb1) } else { Motion::NONE };
        if av_b1 && !(av_a1 && a1 == b1) {
            cands[count] = b1;
            count += 1;
        }
        let (xb0, yb0) = (xp + w, yp - 1);
        if avail(xb0, yb0) {
            let b0 = self.motion_at(xb0, yb0);
            if !(av_b1 && b1 == b0) {
                cands[count] = b0;
                count += 1;
            }
        }
        let (xa0, ya0) = (xp - 1, yp + h);
        if avail(xa0, ya0) {
            let a0 = self.motion_at(xa0, ya0);
            if !(av_a1 && a1 == a0) {
                cands[count] = a0;
                count += 1;
            }
        }
        if count != 4 {
            let (xb2, yb2) = (xp - 1, yp - 1);
            if avail(xb2, yb2) {
                let b2 = self.motion_at(xb2, yb2);
                if !(av_a1 && a1 == b2 || av_b1 && b1 == b2) {
                    cands[count] = b2;
                    count += 1;
                }
            }
        }
        let max = self.hdr.max_num_merge_cand as usize;
        let is_b = self.hdr.slice_type == SliceType::B;
        if merge_idx >= count {
            // the temporal candidate (reference index 0 in both lists)
            if self.hdr.temporal_mvp {
                let mv0 = self.temporal_mv(xp, yp, w, h, 0, 0);
                let mv1 = if is_b { self.temporal_mv(xp, yp, w, h, 1, 0) } else { None };
                if mv0.is_some() || mv1.is_some() {
                    cands[count] = Motion { mv: [mv0.unwrap_or([0, 0]), mv1.unwrap_or([0, 0])], ref_idx: [if mv0.is_some() { 0 } else { -1 }, if mv1.is_some() { 0 } else { -1 }] };
                    count += 1;
                }
            }
            // combined bi-predictive candidates (8.5.3.2.4)
            let orig = count;
            if is_b && orig > 1 && orig < max {
                for &(i0, i1) in COMB.iter().take(orig * (orig - 1)) {
                    let (c0, c1) = (cands[i0], cands[i1]);
                    if c0.uses(0) && c1.uses(1) && (self.refs[0][c0.ref_idx[0] as usize].poc != self.refs[1][c1.ref_idx[1] as usize].poc || c0.mv[0] != c1.mv[1]) {
                        cands[count] = Motion { mv: [c0.mv[0], c1.mv[1]], ref_idx: [c0.ref_idx[0], c1.ref_idx[1]] };
                        count += 1;
                        if count == max {
                            break;
                        }
                    }
                }
            }
            // zero candidates (8.5.3.2.5)
            let num_ref = if is_b { self.hdr.num_ref_idx[0].min(self.hdr.num_ref_idx[1]) } else { self.hdr.num_ref_idx[0] };
            let mut zero = 0;
            while count < max {
                let r = if zero < num_ref { zero as i8 } else { 0 };
                cands[count] = Motion { mv: [[0; 2]; 2], ref_idx: [r, if is_b { r } else { -1 }] };
                count += 1;
                zero += 1;
            }
        }
        let mut m = cands[merge_idx.min(count.saturating_sub(1))];
        if m.uses(0) && m.uses(1) && orig_w + orig_h == 12 {
            // 8x4 and 4x8 blocks predict from one list
            m.ref_idx[1] = -1;
            m.mv[1] = [0, 0];
        }
        m
    }

    /// 8.5.3.2.6: the motion vector predictor of list `x` for reference
    /// index `ref_idx`, chosen by mvp_lX_flag.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn amvp(&self, xc: usize, yc: usize, nc: usize, xp: usize, yp: usize, w: usize, h: usize, part_idx: usize, x: usize, ref_idx: usize, mvp_flag: usize) -> [i16; 2] {
        let (xc, yc, nc, xp, yp, w, h) = (xc as i32, yc as i32, nc as i32, xp as i32, yp as i32, w as i32, h as i32);
        let y = 1 - x;
        let refs = self.refs;
        let target = &refs[x][ref_idx];
        let avail = |xn: i32, yn: i32| self.pb_available(xc, yc, nc, xp, yp, w, h, part_idx, xn, yn);
        // a neighbour's vector pointing at the target picture itself
        let same = |m: &Motion| -> Option<[i16; 2]> {
            if m.uses(x) && refs[x][m.ref_idx[x] as usize].poc == target.poc {
                Some(m.mv[x])
            } else if m.uses(y) && refs[y][m.ref_idx[y] as usize].poc == target.poc {
                Some(m.mv[y])
            } else {
                None
            }
        };
        // a neighbour's vector to a picture of the same kind, scaled
        let scaled = |m: &Motion| -> Option<[i16; 2]> {
            let pick = if m.uses(x) && refs[x][m.ref_idx[x] as usize].long_term == target.long_term {
                Some((m.mv[x], &refs[x][m.ref_idx[x] as usize]))
            } else if m.uses(y) && refs[y][m.ref_idx[y] as usize].long_term == target.long_term {
                Some((m.mv[y], &refs[y][m.ref_idx[y] as usize]))
            } else {
                None
            };
            pick.map(|(mv, r)| if r.poc != target.poc && !r.long_term && !target.long_term { scale(mv, self.poc - r.poc, self.poc - target.poc) } else { mv })
        };
        let a_pos = [(xp - 1, yp + h), (xp - 1, yp + h - 1)];
        let av_a = [avail(a_pos[0].0, a_pos[0].1), avail(a_pos[1].0, a_pos[1].1)];
        let is_scaled = av_a[0] || av_a[1];
        let mut a = None;
        for k in 0..2 {
            if av_a[k] && a.is_none() {
                a = same(&self.motion_at(a_pos[k].0, a_pos[k].1));
            }
        }
        for k in 0..2 {
            if av_a[k] && a.is_none() {
                a = scaled(&self.motion_at(a_pos[k].0, a_pos[k].1));
            }
        }
        let b_pos = [(xp + w, yp - 1), (xp + w - 1, yp - 1), (xp - 1, yp - 1)];
        let av_b = [avail(b_pos[0].0, b_pos[0].1), avail(b_pos[1].0, b_pos[1].1), avail(b_pos[2].0, b_pos[2].1)];
        let mut b = None;
        for k in 0..3 {
            if av_b[k] && b.is_none() {
                b = same(&self.motion_at(b_pos[k].0, b_pos[k].1));
            }
        }
        if !is_scaled {
            if b.is_some() {
                a = b;
            }
            b = None;
            for k in 0..3 {
                if av_b[k] && b.is_none() {
                    b = scaled(&self.motion_at(b_pos[k].0, b_pos[k].1));
                }
            }
        }
        let mut list = [[0i16; 2]; 2];
        let mut i = 0;
        if let Some(av) = a {
            list[i] = av;
            i += 1;
            if let Some(bv) = b {
                if bv != av {
                    list[i] = bv;
                    i += 1;
                }
            }
        } else if let Some(bv) = b {
            list[i] = bv;
            i += 1;
        }
        if i < 2 && !(a.is_some() && b.is_some() && a != b) {
            if let Some(cv) = self.temporal_mv(xp, yp, w, h, x, ref_idx) {
                list[i] = cv;
            }
        }
        list[mvp_flag]
    }

    /// 8.5.3.2.8: the temporal motion vector predictor for list `x` and
    /// reference index `ref_idx`, if there is one.
    fn temporal_mv(&self, xp: i32, yp: i32, w: i32, h: i32, x: usize, ref_idx: usize) -> Option<[i16; 2]> {
        let col = self.col?;
        let l = self.sps.log2_ctb;
        let (xbr, ybr) = (xp + w, yp + h);
        if yp >> l == ybr >> l && ybr < self.height && xbr < self.width {
            if let Some(mv) = self.col_mv(col, xbr, ybr, x, ref_idx) {
                return Some(mv);
            }
        }
        self.col_mv(col, xp + (w >> 1), yp + (h >> 1), x, ref_idx)
    }

    /// 8.5.3.2.9: the collocated motion vector at the 16x16 block covering
    /// (`xs`, `ys`) in the collocated picture.
    fn col_mv(&self, col: &Picture<P>, xs: i32, ys: i32, x: usize, ref_idx: usize) -> Option<[i16; 2]> {
        let cw = col.width().div_ceil(16);
        let cm = col.col.get((ys >> 4) as usize * cw + (xs >> 4) as usize)?;
        let list = if cm.flags & COL_L0 == 0 {
            if cm.flags & COL_L1 == 0 {
                return None;
            }
            1
        } else if cm.flags & COL_L1 == 0 {
            0
        } else if self.no_backward_pred {
            x
        } else {
            self.hdr.collocated_from_l0 as usize
        };
        let col_lt = cm.flags & if list == 0 { COL_LT0 } else { COL_LT1 } != 0;
        let target = &self.refs[x][ref_idx];
        if target.long_term != col_lt {
            return None;
        }
        let mv = cm.mv[list];
        let col_diff = col.poc - cm.poc[list];
        let cur_diff = self.poc - target.poc;
        if target.long_term || col_diff == cur_diff {
            Some(mv)
        } else {
            Some(scale(mv, col_diff, cur_diff))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling() {
        // twice the distance: twice the vector
        assert_eq!(scale([16, -8], 1, 2), [32, -16]);
        // the same distance keeps it
        assert_eq!(scale([7, 3], 4, 4), [7, 3]);
        // a quarter of the distance: 2.5 rounds towards zero
        assert_eq!(scale([10, -10], 4, 1), [2, -2]);
    }
}
