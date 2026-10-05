//! The deblocking filter (8.7.2), run over a whole decoded picture: all
//! vertical edges first, then all horizontal ones.
//!
//! Edges lie on the 8x8 grid. An edge belongs to the coding unit on its
//! right (below): that unit's slice decides whether it is filtered at all
//! and with which offsets, and whether it is filtered across a slice
//! boundary.

use crate::meta::{Meta, Motion, RefKey, SliceInfo, BYPASS, CODED, INTRA, NO_SLICE, PCM, PU_LEFT, PU_TOP, TU_LEFT, TU_TOP};
use crate::picture::{Picture, Sample};
use crate::ps::{Layout, Pps, Sps};
use crate::tables::qpc;

/// β′ (Table 8-12) by Q.
const BETA: [u8; 52] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 20, 22, 24, 26, 28, 30, 32, 34, 36, 38, 40, 42, 44, 46, 48, 50, 52, 54, 56, 58, 60, 62, 64];
/// tC′ (Table 8-12) by Q.
const TC: [u8; 54] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 5, 5, 6, 6, 7, 8, 9, 10, 11, 13, 14, 16, 18, 20, 22, 24];
/// Clip3 (5-4), for bounds that are ordered by construction.
#[inline(always)]
fn clip3(lo: i32, hi: i32, v: i32) -> i32 {
    v.max(lo).min(hi)
}

/// What the filter needs to know about the picture's blocks.
struct Ctx<'a> {
    meta: &'a Meta,
    layout: &'a Layout,
    log2_ctb: u32,
    across_tiles: bool,
    pcm_unfiltered: bool,
}

impl<'a> Ctx<'a> {
    fn ctb(&self, x4: usize, y4: usize) -> usize {
        let l = self.log2_ctb - 2;
        (y4 >> l) * self.layout.width_ctbs as usize + (x4 >> l)
    }

    /// The slice segment of the 4x4 block (`x4`, `y4`).
    fn slice(&self, x4: usize, y4: usize) -> Option<&'a SliceInfo> {
        let s = self.meta.ctb_slice[self.ctb(x4, y4)];
        (s != NO_SLICE).then(|| &self.meta.slices[s as usize])
    }

    /// Whether the samples of block `b` are left as they are.
    fn unfiltered(&self, b: usize) -> bool {
        let f = self.meta.flags[b];
        f & BYPASS != 0 || (self.pcm_unfiltered && f & PCM != 0)
    }

    /// 8.7.2.4: the boundary strength of the edge between the 4x4 block
    /// `p` and the block `q` to its right or below; `tu` / `pu` are q's
    /// edge flags for this direction.
    fn strength(&self, (px, py): (usize, usize), (qx, qy): (usize, usize), tu: u8, pu: u8) -> u8 {
        let w4 = self.meta.w4;
        let (p, q) = (py * w4 + px, qy * w4 + qx);
        let e = self.meta.edges[q];
        if e & (tu | pu) == 0 {
            return 0;
        }
        let (cp, cq) = (self.ctb(px, py), self.ctb(qx, qy));
        let (np, nq) = (self.meta.ctb_slice[cp], self.meta.ctb_slice[cq]);
        if np == NO_SLICE || nq == NO_SLICE {
            return 0;
        }
        let (sp, sq) = (&self.meta.slices[np as usize], &self.meta.slices[nq as usize]);
        if sq.deblocking_disabled {
            return 0;
        }
        // a coding tree block lies in one slice segment and one tile
        if cp != cq && ((sq.addr != sp.addr && !sq.loop_filter_across_slices) || (!self.across_tiles && self.layout.tile_id[cp] != self.layout.tile_id[cq])) {
            return 0;
        }
        let (fp, fq) = (self.meta.flags[p], self.meta.flags[q]);
        if (fp | fq) & INTRA != 0 {
            return 2;
        }
        if e & tu != 0 && (fp | fq) & CODED != 0 {
            return 1;
        }
        if e & pu == 0 {
            // inside one prediction block: the same motion either side
            return 0;
        }
        motion_strength(&self.meta.motion[p], &sp.refs, &self.meta.motion[q], &sq.refs)
    }
}

/// The motion part of 8.7.2.4: whether the two blocks predict from
/// different pictures, a different number of vectors, or vectors 4 or
/// more quarter samples apart.
fn motion_strength(mp: &Motion, rp: &[Vec<RefKey>; 2], mq: &Motion, rq: &[Vec<RefKey>; 2]) -> u8 {
    let id = |m: &Motion, r: &[Vec<RefKey>; 2], l: usize| -> Option<u32> { m.uses(l).then(|| r[l].get(m.ref_idx[l] as usize).map_or(u32::MAX, |k| k.id)) };
    let far = |a: [i16; 2], b: [i16; 2]| (a[0] as i32 - b[0] as i32).abs() >= 4 || (a[1] as i32 - b[1] as i32).abs() >= 4;
    let (p0, p1, q0, q1) = (id(mp, rp, 0), id(mp, rp, 1), id(mq, rq, 0), id(mq, rq, 1));
    let np = p0.is_some() as u8 + p1.is_some() as u8;
    let nq = q0.is_some() as u8 + q1.is_some() as u8;
    if np != nq {
        return 1;
    }
    if np == 1 {
        let (pa, mva) = if let Some(a) = p0 { (a, mp.mv[0]) } else { (p1.unwrap_or(0), mp.mv[1]) };
        let (qa, mvb) = if let Some(a) = q0 { (a, mq.mv[0]) } else { (q1.unwrap_or(0), mq.mv[1]) };
        return (pa != qa || far(mva, mvb)) as u8;
    }
    let (p0, p1, q0, q1) = (p0.unwrap_or(0), p1.unwrap_or(0), q0.unwrap_or(0), q1.unwrap_or(0));
    if !((p0 == q0 && p1 == q1) || (p0 == q1 && p1 == q0)) {
        return 1;
    }
    if p0 != p1 {
        // two different pictures: compare the vectors to the same picture
        if p0 == q0 {
            (far(mp.mv[0], mq.mv[0]) || far(mp.mv[1], mq.mv[1])) as u8
        } else {
            (far(mp.mv[0], mq.mv[1]) || far(mp.mv[1], mq.mv[0])) as u8
        }
    } else {
        // the same picture twice: either pairing may match
        ((far(mp.mv[0], mq.mv[0]) || far(mp.mv[1], mq.mv[1])) && (far(mp.mv[0], mq.mv[1]) || far(mp.mv[1], mq.mv[0]))) as u8
    }
}

/// The samples across an edge segment, gathered: `lines[k][i]` is sample
/// `i` of line `k`, p3 p2 p1 p0 q0 q1 q2 q3 for luma (`N` = 8) and
/// p1 p0 q0 q1 for chroma (`N` = 4). The edge at (`x`, `y`) is vertical
/// (the lines are rows) or horizontal (the lines are columns).
struct Segment<const N: usize> {
    lines: [[i32; N]; 4],
}

impl<const N: usize> Segment<N> {
    fn load<P: Sample>(d: &[P], stride: usize, x: usize, y: usize, vertical: bool, count: usize) -> Self {
        let mut lines = [[0i32; N]; 4];
        if vertical {
            for (k, line) in lines.iter_mut().enumerate().take(count) {
                let row = &d[(y + k) * stride + x - N / 2..][..N];
                for (v, s) in line.iter_mut().zip(row) {
                    *v = s.get();
                }
            }
        } else {
            for i in 0..N {
                let row = &d[(y + i - N / 2) * stride + x..][..count];
                for (line, s) in lines.iter_mut().zip(row) {
                    line[i] = s.get();
                }
            }
        }
        Segment { lines }
    }

    /// Write back the samples the filters may change (all but the outermost).
    fn store<P: Sample>(&self, d: &mut [P], stride: usize, x: usize, y: usize, vertical: bool, count: usize) {
        if vertical {
            for (k, line) in self.lines.iter().enumerate().take(count) {
                let row = &mut d[(y + k) * stride + x - N / 2..][..N];
                for (s, &v) in row[1..N - 1].iter_mut().zip(&line[1..N - 1]) {
                    *s = P::new(v);
                }
            }
        } else {
            for i in 1..N - 1 {
                let row = &mut d[(y + i - N / 2) * stride + x..][..count];
                for (s, line) in row.iter_mut().zip(&self.lines) {
                    *s = P::new(line[i]);
                }
            }
        }
    }
}

/// Filter one 4-line luma edge segment (8.7.2.5.3, 8.7.2.5.4, 8.7.2.5.6,
/// 8.7.2.5.7); returns whether any sample may have changed.
fn luma_segment(seg: &mut Segment<8>, beta: i32, tc: i32, no_p: bool, no_q: bool, max: i32) -> bool {
    // p_i is at 3 - i, q_i at 4 + i
    let l = &seg.lines;
    let dp = |k: usize| (l[k][1] - 2 * l[k][2] + l[k][3]).abs();
    let dq = |k: usize| (l[k][6] - 2 * l[k][5] + l[k][4]).abs();
    let (dp0, dp3, dq0, dq3) = (dp(0), dp(3), dq(0), dq(3));
    let (dpq0, dpq3) = (dp0 + dq0, dp3 + dq3);
    if dpq0 + dpq3 >= beta {
        return false;
    }
    let sam = |k: usize, dpq: i32| -> bool {
        let (p0, p3, q0, q3) = (l[k][3], l[k][0], l[k][4], l[k][7]);
        dpq < (beta >> 2) && (p3 - p0).abs() + (q0 - q3).abs() < (beta >> 3) && (p0 - q0).abs() < ((5 * tc + 1) >> 1)
    };
    let strong = sam(0, 2 * dpq0) && sam(3, 2 * dpq3);
    let side = (beta + (beta >> 1)) >> 3;
    let (dep, deq) = (dp0 + dp3 < side, dq0 + dq3 < side);
    for line in seg.lines.iter_mut() {
        let [p3, p2, p1, p0, q0, q1, q2, q3] = *line;
        if strong {
            let tc2 = 2 * tc;
            if !no_p {
                line[3] = clip3(p0 - tc2, p0 + tc2, (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3);
                line[2] = clip3(p1 - tc2, p1 + tc2, (p2 + p1 + p0 + q0 + 2) >> 2);
                line[1] = clip3(p2 - tc2, p2 + tc2, (2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3);
            }
            if !no_q {
                line[4] = clip3(q0 - tc2, q0 + tc2, (p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3);
                line[5] = clip3(q1 - tc2, q1 + tc2, (p0 + q0 + q1 + q2 + 2) >> 2);
                line[6] = clip3(q2 - tc2, q2 + tc2, (p0 + q0 + q1 + 3 * q2 + 2 * q3 + 4) >> 3);
            }
        } else {
            let delta = (9 * (q0 - p0) - 3 * (q1 - p1) + 8) >> 4;
            if delta.abs() >= tc * 10 {
                continue;
            }
            let delta = clip3(-tc, tc, delta);
            if !no_p {
                line[3] = clip3(0, max, p0 + delta);
                if dep {
                    let dp = clip3(-(tc >> 1), tc >> 1, (((p2 + p0 + 1) >> 1) - p1 + delta) >> 1);
                    line[2] = clip3(0, max, p1 + dp);
                }
            }
            if !no_q {
                line[4] = clip3(0, max, q0 - delta);
                if deq {
                    let dq = clip3(-(tc >> 1), tc >> 1, (((q2 + q0 + 1) >> 1) - q1 - delta) >> 1);
                    line[5] = clip3(0, max, q1 + dq);
                }
            }
        }
    }
    true
}

/// Filter one chroma sample line across an edge (8.7.2.5.8).
fn chroma_line(line: &mut [i32; 4], tc: i32, no_p: bool, no_q: bool, max: i32) {
    let [p1, p0, q0, q1] = *line;
    let delta = clip3(-tc, tc, (((q0 - p0) << 2) + p1 - q1 + 4) >> 3);
    if !no_p {
        line[1] = clip3(0, max, p0 + delta);
    }
    if !no_q {
        line[2] = clip3(0, max, q0 - delta);
    }
}

/// Run the deblocking filter over a decoded picture.
pub fn deblock<P: Sample>(pic: &mut Picture<P>, meta: &Meta, sps: &Sps, pps: &Pps, layout: &Layout) {
    let cx = Ctx { meta, layout, log2_ctb: sps.log2_ctb, across_tiles: pps.loop_filter_across_tiles, pcm_unfiltered: sps.pcm_loop_filter_disabled };
    let (w4, h4) = (meta.w4, meta.h4);
    let bd = sps.bit_depth;
    let bdc = sps.bit_depth_chroma;
    // the edges to filter in one direction: q block and boundary strength
    let mut edges: Vec<(usize, usize, u8)> = Vec::new();
    for vertical in [true, false] {
        // the edges lie on the 8x8 grid: every other column (row) of 4x4 blocks
        let (tu, pu) = if vertical { (TU_LEFT, PU_LEFT) } else { (TU_TOP, PU_TOP) };
        let before = |x4: usize, y4: usize| if vertical { (x4 - 1, y4) } else { (x4, y4 - 1) };
        let grid = |n: usize| (1..n.div_ceil(2)).map(|k| 2 * k);
        edges.clear();
        let mut add = |x4: usize, y4: usize| {
            let s = cx.strength(before(x4, y4), (x4, y4), tu, pu);
            if s > 0 {
                edges.push((x4, y4, s));
            }
        };
        if vertical {
            for y4 in 0..h4 {
                grid(w4).for_each(|x4| add(x4, y4));
            }
        } else {
            for y4 in grid(h4) {
                (0..w4).for_each(|x4| add(x4, y4));
            }
        }
        // luma
        let plane = &mut pic.planes[0];
        let stride = plane.stride;
        let max = (1 << bd) - 1;
        for &(x4, y4, s) in &edges {
            let (px, py) = before(x4, y4);
            let (p, q) = (py * w4 + px, y4 * w4 + x4);
            let Some(sq) = cx.slice(x4, y4) else { continue };
            let qpl = (meta.qp[p] as i32 + meta.qp[q] as i32 + 1) >> 1;
            let beta = BETA[(qpl + (sq.beta_offset_div2 << 1)).clamp(0, 51) as usize] as i32 * (1 << (bd - 8));
            let tc = TC[(qpl + 2 * (s as i32 - 1) + (sq.tc_offset_div2 << 1)).clamp(0, 53) as usize] as i32 * (1 << (bd - 8));
            let (x, y) = (x4 * 4, y4 * 4);
            let mut seg = Segment::<8>::load(&plane.data, stride, x, y, vertical, 4);
            if luma_segment(&mut seg, beta, tc, cx.unfiltered(p), cx.unfiltered(q), max) {
                seg.store(&mut plane.data, stride, x, y, vertical, 4);
            }
        }
        if sps.chroma_format_idc == 0 {
            continue;
        }
        // chroma: the edges of strength 2 on the 8x8 chroma grid (every
        // other luma edge); a 4-line chroma segment spans two luma blocks
        // and takes the first one's strength
        let on_chroma_grid = |x4: usize, y4: usize| if vertical { x4.is_multiple_of(4) && y4.is_multiple_of(2) } else { y4.is_multiple_of(4) && x4.is_multiple_of(2) };
        let max = (1 << bdc) - 1;
        for c in 1..3 {
            let offset = if c == 1 { pps.cb_qp_offset } else { pps.cr_qp_offset };
            let plane = &mut pic.planes[c];
            let stride = plane.stride;
            for &(x4, y4, _) in edges.iter().filter(|&&(x4, y4, s)| s == 2 && on_chroma_grid(x4, y4)) {
                let (px, py) = before(x4, y4);
                let (p, q) = (py * w4 + px, y4 * w4 + x4);
                let Some(sq) = cx.slice(x4, y4) else { continue };
                let qpi = ((meta.qp[p] as i32 + meta.qp[q] as i32 + 1) >> 1) + offset;
                let tc = TC[(qpc(qpi) + 2 + (sq.tc_offset_div2 << 1)).clamp(0, 53) as usize] as i32 * (1 << (bdc - 8));
                let (xc, yc) = (x4 * 2, y4 * 2);
                // the chroma lines of the segment inside the plane
                let count = 4.min(if vertical { plane.height.saturating_sub(yc) } else { plane.width.saturating_sub(xc) });
                let mut seg = Segment::<4>::load(&plane.data, stride, xc, yc, vertical, count);
                for (k, line) in seg.lines.iter_mut().enumerate().take(count) {
                    // the luma blocks either side of this line
                    let (qb, pb) = if vertical {
                        let row = ((yc + k) / 2) * w4;
                        (row + x4, row + x4 - 1)
                    } else {
                        let col = (xc + k) / 2;
                        (y4 * w4 + col, (y4 - 1) * w4 + col)
                    };
                    chroma_line(line, tc, cx.unfiltered(pb), cx.unfiltered(qb), max);
                }
                seg.store(&mut plane.data, stride, xc, yc, vertical, count);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_strengths() {
        let refs = [vec![RefKey { poc: 0, long_term: false, id: 1 }, RefKey { poc: 8, long_term: false, id: 2 }], vec![RefKey { poc: 8, long_term: false, id: 2 }, RefKey { poc: 0, long_term: false, id: 1 }]];
        let a = Motion { mv: [[4, 0], [0, 0]], ref_idx: [0, -1] };
        // the same picture through the other list, vectors 3 apart: no edge
        let b = Motion { mv: [[0, 0], [7, 0]], ref_idx: [-1, 1] };
        assert_eq!(motion_strength(&a, &refs, &b, &refs), 0);
        // 4 apart: an edge
        let c = Motion { mv: [[0, 0], [8, 0]], ref_idx: [-1, 1] };
        assert_eq!(motion_strength(&a, &refs, &c, &refs), 1);
        // bi-prediction from the same two pictures in swapped lists
        let d = Motion { mv: [[1, 1], [2, 2]], ref_idx: [0, 0] };
        let e = Motion { mv: [[2, 2], [1, 1]], ref_idx: [1, 1] };
        assert_eq!(motion_strength(&d, &refs, &e, &refs), 0);
        assert_eq!(motion_strength(&a, &refs, &d, &refs), 1);
    }
}
