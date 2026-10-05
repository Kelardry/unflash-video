//! The deblocking filter (8.7), run over a whole decoded picture.

use crate::picture::{Picture, BOTTOM, FRAME};
use crate::tables::{ALPHA, BETA, TC0};

/// What the filter needs to know about each macroblock.
#[derive(Clone, Copy, Debug, Default)]
pub struct MbDeblockInfo {
    pub decoded: bool,
    pub intra: bool,
    pub transform8x8: bool,
    /// QPY (0 for I_PCM).
    pub qp: i32,
    /// QPc of Cb and Cr.
    pub qpc: [i32; 2],
    /// Bit per 4x4 luma block (raster order in the macroblock): has
    /// non-zero coefficients (an 8x8 transform block sets all four bits).
    pub nonzero: u16,
    pub slice: u32,
    pub filter_idc: u8,
    pub alpha_offset: i32,
    pub beta_offset: i32,
}

#[inline(always)]
fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// 8.7.2.1: the boundary strength between two inter 4x4 blocks without
/// coefficients (the intra and coefficient cases are decided by the caller).
#[inline]
fn motion_bs(pic: &Picture, pb: usize, qb: usize, mvy_limit: i32) -> u8 {
    let rp = [pic.ref_id[0][pb], pic.ref_id[1][pb]];
    let rq = [pic.ref_id[0][qb], pic.ref_id[1][qb]];
    let mp = [pic.mv[0][pb], pic.mv[1][pb]];
    let mq = [pic.mv[0][qb], pic.mv[1][qb]];
    let np = (rp[0] >= 0) as usize + (rp[1] >= 0) as usize;
    let nq = (rq[0] >= 0) as usize + (rq[1] >= 0) as usize;
    if np != nq {
        return 1;
    }
    let far = |a: [i16; 2], b: [i16; 2]| (a[0] as i32 - b[0] as i32).abs() >= 4 || (a[1] as i32 - b[1] as i32).abs() >= mvy_limit;
    if np == 1 {
        let (ip, vp) = if rp[0] >= 0 { (rp[0], mp[0]) } else { (rp[1], mp[1]) };
        let (iq, vq) = if rq[0] >= 0 { (rq[0], mq[0]) } else { (rq[1], mq[1]) };
        if ip != iq {
            return 1;
        }
        return far(vp, vq) as u8;
    }
    // two motion vectors each: the same two pictures, and the vectors paired by picture
    let same_set = (rp[0] == rq[0] && rp[1] == rq[1]) || (rp[0] == rq[1] && rp[1] == rq[0]);
    if !same_set {
        return 1;
    }
    if rp[0] != rp[1] {
        let (q0, q1) = if rp[0] == rq[0] { (mq[0], mq[1]) } else { (mq[1], mq[0]) };
        return (far(mp[0], q0) || far(mp[1], q1)) as u8;
    }
    // both vectors refer to the same picture: either pairing may pass
    let straight = far(mp[0], mq[0]) || far(mp[1], mq[1]);
    let crossed = far(mp[0], mq[1]) || far(mp[1], mq[0]);
    (straight && crossed) as u8
}

/// The boundary strength of the edge between blocks `pb` (in MB `p`) and
/// `qb` (in MB `q`); `p_bit` / `q_bit` select their coefficient flags.
/// `strong` says an intra macroblock edge gets bS 4 (a frame macroblock
/// edge, or a vertical one in a field); vertical motion differs at
/// `mvy_limit` quarter samples (2 for field macroblocks).
#[inline]
#[allow(clippy::too_many_arguments)]
fn boundary_strength(pic: &Picture, p: &MbDeblockInfo, q: &MbDeblockInfo, pb: usize, qb: usize, p_bit: u16, q_bit: u16, strong: bool, mvy_limit: i32) -> u8 {
    if p.intra || q.intra {
        return if strong { 4 } else { 3 };
    }
    if p.nonzero & p_bit != 0 || q.nonzero & q_bit != 0 {
        return 2;
    }
    motion_bs(pic, pb, qb, mvy_limit)
}

/// The strengths of the internal edges of a macroblock (edges 1..3, both
/// directions; 1 and 3 left alone with the 8x8 transform): 3 in an intra
/// macroblock; none in an inter one without coefficients whose blocks all
/// move as one; else as `inter_bs` finds them.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn internal_bs(pic: &Picture, cur: &MbDeblockInfo, bx0: usize, by0: usize, w4: usize, mvy_limit: i32, bs_v: &mut [[u8; 4]; 4], bs_h: &mut [[u8; 4]; 4]) {
    if cur.intra {
        for e in 1..4 {
            if cur.transform8x8 && e % 2 == 1 {
                continue;
            }
            bs_v[e] = [3; 4];
            bs_h[e] = [3; 4];
        }
    } else if cur.nonzero != 0 || !uniform_motion(pic, bx0, by0, w4) {
        inter_bs(pic, cur, bx0, by0, w4, mvy_limit, bs_v, bs_h);
    }
}

/// The strengths of the internal edges of an inter macroblock: 2 where a
/// block on either side has coefficients (tested for every segment at once
/// on the coefficient bits), else the motion test.
#[allow(clippy::too_many_arguments)]
fn inter_bs(pic: &Picture, cur: &MbDeblockInfo, bx0: usize, by0: usize, w4: usize, mvy_limit: i32, bs_v: &mut [[u8; 4]; 4], bs_h: &mut [[u8; 4]; 4]) {
    let nz = cur.nonzero;
    // bit 4k + e - 1: block (e - 1, k) or (e, k) of vertical edge e has coefficients
    let coef_v = nz | (nz >> 1);
    // bit 4(e - 1) + k: block (k, e - 1) or (k, e) of horizontal edge e
    let coef_h = nz | (nz >> 4);
    for e in 1..4 {
        if cur.transform8x8 && e % 2 == 1 {
            continue;
        }
        for k in 0..4 {
            // vertical edge e (x = 4e), segment k (rows 4k..)
            let pb = (by0 + k) * w4 + bx0 + e - 1;
            bs_v[e][k] = if (coef_v >> (4 * k + e - 1)) & 1 != 0 { 2 } else { motion_bs(pic, pb, pb + 1, mvy_limit) };
            // horizontal edge e (y = 4e), segment k (columns 4k..)
            let pb = (by0 + e - 1) * w4 + bx0 + k;
            bs_h[e][k] = if (coef_h >> (4 * (e - 1) + k)) & 1 != 0 { 2 } else { motion_bs(pic, pb, pb + w4, mvy_limit) };
        }
    }
}

/// Whether all sixteen 4x4 blocks of the macroblock at (`bx0`, `by0`) (in
/// 4x4 units) carry the same motion, so its internal edges need no filtering
/// when it has no coefficients.
fn uniform_motion(pic: &Picture, bx0: usize, by0: usize, w4: usize) -> bool {
    let b0 = by0 * w4 + bx0;
    let key = |b: usize| (pic.ref_id[0][b], pic.ref_id[1][b], pic.mv[0][b], pic.mv[1][b]);
    let k0 = key(b0);
    for y in 0..4 {
        for x in 0..4 {
            if key((by0 + y) * w4 + bx0 + x) != k0 {
                return false;
            }
        }
    }
    true
}

/// Filter one line of luma samples across a vertical edge: `s` holds
/// p3 p2 p1 p0 q0 q1 q2 q3.
#[inline(always)]
fn luma_line(s: &mut [u8; 8], bs: u8, alpha: i32, beta: i32, tc0: i32) {
    let p0 = s[3] as i32;
    let q0 = s[4] as i32;
    if (p0 - q0).abs() >= alpha {
        return;
    }
    let p1 = s[2] as i32;
    let q1 = s[5] as i32;
    if (p1 - p0).abs() >= beta || (q1 - q0).abs() >= beta {
        return;
    }
    let p2 = s[1] as i32;
    let q2 = s[6] as i32;
    let ap = (p2 - p0).abs();
    let aq = (q2 - q0).abs();
    if bs == 4 {
        let strong = (p0 - q0).abs() < ((alpha >> 2) + 2);
        if ap < beta && strong {
            let p3 = s[0] as i32;
            s[3] = ((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3) as u8;
            s[2] = ((p2 + p1 + p0 + q0 + 2) >> 2) as u8;
            s[1] = ((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3) as u8;
        } else {
            s[3] = ((2 * p1 + p0 + q1 + 2) >> 2) as u8;
        }
        if aq < beta && strong {
            let q3 = s[7] as i32;
            s[4] = ((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3) as u8;
            s[5] = ((p0 + q0 + q1 + q2 + 2) >> 2) as u8;
            s[6] = ((2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3) as u8;
        } else {
            s[4] = ((2 * q1 + q0 + p1 + 2) >> 2) as u8;
        }
        return;
    }
    let tc = tc0 + (ap < beta) as i32 + (aq < beta) as i32;
    let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
    s[3] = clip(p0 + delta);
    s[4] = clip(q0 - delta);
    if ap < beta {
        s[2] = (p1 + ((p2 + ((p0 + q0 + 1) >> 1) - (p1 << 1)) >> 1).clamp(-tc0, tc0)) as u8;
    }
    if aq < beta {
        s[5] = (q1 + ((q2 + ((p0 + q0 + 1) >> 1) - (q1 << 1)) >> 1).clamp(-tc0, tc0)) as u8;
    }
}

/// tC0 of each four-line segment of an edge with strengths `bs` (0 where
/// the segment is not filtered or gets bS 4).
#[inline(always)]
fn tc0_of(bs: [u8; 4], tc0s: [u8; 3]) -> [i16; 4] {
    let t = [0, tc0s[0] as i16, tc0s[1] as i16, tc0s[2] as i16, 0, 0, 0, 0];
    bs.map(|b| t[b as usize & 7])
}

/// Whether an edge gets the bS 4 filter (all its segments do, or none).
#[inline(always)]
fn strong(bs: [u8; 4]) -> bool {
    debug_assert!(bs == [4; 4] || !bs.contains(&4), "{bs:?}");
    bs[0] == 4
}

#[cfg(feature = "simd")]
use simd::{chroma_edge_h, chroma_edge_v, luma_edge_h, luma_edge_v};

#[cfg(not(feature = "simd"))]
use scalar::{chroma_edge_h, chroma_edge_v, luma_edge_h, luma_edge_v};

/// The edge filters eight lines at a time (wasm simd128, SSE2 or NEON
/// through `wide`): each of `luma_line`'s and `chroma_line`'s decisions is a
/// lane mask, so no line takes a branch, and a vertical edge's lines are
/// transposed into lanes and back. The sums fit i16; the saturating
/// narrowing to u8 is the final clip.
#[cfg(feature = "simd")]
mod simd {
    use crate::inter::simd::{load8, store8};
    use wide::{i16x8, u8x16, CmpGt, CmpLt};

    /// Lanes 0-3 from `a` and 4-7 from `b` (two four-line luma segments).
    #[inline(always)]
    fn quads(a: i16, b: i16) -> i16x8 {
        i16x8::new([a, a, a, a, b, b, b, b])
    }

    /// Lanes 2k and 2k + 1 from `s[k]` (the four two-line chroma segments).
    #[inline(always)]
    fn pairs(s: [i16; 4]) -> i16x8 {
        i16x8::new([s[0], s[0], s[1], s[1], s[2], s[2], s[3], s[3]])
    }

    /// Lanes set (all ones) where a segment is filtered at all (bS > 0).
    #[inline(always)]
    fn on(v: i16x8) -> i16x8 {
        v.cmp_gt(i16x8::ZERO)
    }

    /// The luma filter on eight lines (one per lane): `p` / `q` hold p0..p3
    /// / q0..q3, `on` masks the lines filtered at all and `tc0` is their
    /// tC0; bS 4 when `strong_edge`. Returns p0..p2 and q0..q2.
    #[inline(always)]
    fn luma8(p: [i16x8; 4], q: [i16x8; 4], on: i16x8, tc0: i16x8, alpha: i16x8, beta: i16x8, strong_edge: bool) -> ([i16x8; 3], [i16x8; 3]) {
        let [p0, p1, p2, p3] = p;
        let [q0, q1, q2, q3] = q;
        let d = (p0 - q0).abs();
        let filt = on & d.cmp_lt(alpha) & (p1 - p0).abs().cmp_lt(beta) & (q1 - q0).abs().cmp_lt(beta);
        let ap = (p2 - p0).abs().cmp_lt(beta);
        let aq = (q2 - q0).abs().cmp_lt(beta);
        if strong_edge {
            let strong = filt & d.cmp_lt((alpha >> 2i32) + 2i16);
            let (sp, sq) = (strong & ap, strong & aq);
            let p0f = sp.blend((p2 + (p1 + p0 + q0) * 2i16 + q1 + 4i16) >> 3i32, (p1 * 2i16 + p0 + q1 + 2i16) >> 2i32);
            let q0f = sq.blend((q2 + (q1 + q0 + p0) * 2i16 + p1 + 4i16) >> 3i32, (q1 * 2i16 + q0 + p1 + 2i16) >> 2i32);
            let p1f = (p2 + p1 + p0 + q0 + 2i16) >> 2i32;
            let q1f = (q2 + q1 + q0 + p0 + 2i16) >> 2i32;
            let p2f = (p3 * 2i16 + p2 * 3i16 + p1 + p0 + q0 + 4i16) >> 3i32;
            let q2f = (q3 * 2i16 + q2 * 3i16 + q1 + q0 + p0 + 4i16) >> 3i32;
            ([filt.blend(p0f, p0), sp.blend(p1f, p1), sp.blend(p2f, p2)], [filt.blend(q0f, q0), sq.blend(q1f, q1), sq.blend(q2f, q2)])
        } else {
            // tC = tC0 + (ap < beta) + (aq < beta), the masks being -1
            let tc = tc0 - ap - aq;
            let delta = ((((q0 - p0) << 2i32) + (p1 - q1) + 4i16) >> 3i32).max(-tc).min(tc);
            let avg = (p0 + q0 + 1i16) >> 1i32;
            let p1f = p1 + ((p2 + avg - (p1 << 1i32)) >> 1i32).max(-tc0).min(tc0);
            let q1f = q1 + ((q2 + avg - (q1 << 1i32)) >> 1i32).max(-tc0).min(tc0);
            ([filt.blend(p0 + delta, p0), (filt & ap).blend(p1f, p1), p2], [filt.blend(q0 - delta, q0), (filt & aq).blend(q1f, q1), q2])
        }
    }

    /// The chroma filter on eight lines: their p0 and q0.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn chroma8(p1: i16x8, p0: i16x8, q0: i16x8, q1: i16x8, on: i16x8, tc0: i16x8, alpha: i16x8, beta: i16x8, strong_edge: bool) -> (i16x8, i16x8) {
        let filt = on & (p0 - q0).abs().cmp_lt(alpha) & (p1 - p0).abs().cmp_lt(beta) & (q1 - q0).abs().cmp_lt(beta);
        let (p0f, q0f) = if strong_edge {
            ((p1 * 2i16 + p0 + q1 + 2i16) >> 2i32, (q1 * 2i16 + q0 + p1 + 2i16) >> 2i32)
        } else {
            let tc = tc0 + 1i16;
            let delta = ((((q0 - p0) << 2i32) + (p1 - q1) + 4i16) >> 3i32).max(-tc).min(tc);
            (p0 + delta, q0 - delta)
        };
        (filt.blend(p0f, p0), filt.blend(q0f, q0))
    }

    /// Luma across a vertical edge: sixteen lines `stride` apart from `at`
    /// (the first line's q0), `bs` and `tc0` per four lines.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    pub fn luma_edge_v(pl: &mut [u8], at: usize, stride: usize, bs: [u8; 4], tc0: [i16; 4], strong_edge: bool, alpha: i32, beta: i32) {
        let (alpha, beta) = (i16x8::splat(alpha as i16), i16x8::splat(beta as i16));
        for h in 0..2 {
            if bs[2 * h] == 0 && bs[2 * h + 1] == 0 {
                continue;
            }
            // lane k of vector j: sample j (p3 .. q3) of line k
            let base = at + 8 * h * stride - 4;
            let t = i16x8::transpose(std::array::from_fn(|r| load8(&pl[base + r * stride..])));
            let (p, q) = luma8([t[3], t[2], t[1], t[0]], [t[4], t[5], t[6], t[7]], on(quads(bs[2 * h] as i16, bs[2 * h + 1] as i16)), quads(tc0[2 * h], tc0[2 * h + 1]), alpha, beta, strong_edge);
            let out = i16x8::transpose([t[0], p[2], p[1], p[0], q[0], q[1], q[2], t[7]]);
            for (r, v) in out.into_iter().enumerate() {
                store8(v, &mut pl[base + r * stride..]);
            }
        }
    }

    /// Luma across a horizontal edge: sixteen columns from `at` (the first
    /// column's q0; rows `stride` apart), `bs` and `tc0` per four columns.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    pub fn luma_edge_h(pl: &mut [u8], at: usize, stride: usize, bs: [u8; 4], tc0: [i16; 4], strong_edge: bool, alpha: i32, beta: i32) {
        let (alpha, beta) = (i16x8::splat(alpha as i16), i16x8::splat(beta as i16));
        for h in 0..2 {
            if bs[2 * h] == 0 && bs[2 * h + 1] == 0 {
                continue;
            }
            let c = at + 8 * h;
            let p = [1, 2, 3, 4].map(|k| load8(&pl[c - k * stride..]));
            let q = [0, 1, 2, 3].map(|k| load8(&pl[c + k * stride..]));
            let (p, q) = luma8(p, q, on(quads(bs[2 * h] as i16, bs[2 * h + 1] as i16)), quads(tc0[2 * h], tc0[2 * h + 1]), alpha, beta, strong_edge);
            for k in 0..if strong_edge { 3 } else { 2 } {
                store8(p[k], &mut pl[c - (k + 1) * stride..]);
                store8(q[k], &mut pl[c + k * stride..]);
            }
        }
    }

    /// Chroma across a vertical edge of both planes: eight lines `stride`
    /// apart from `at` in each, `bs` per two lines; `tc0`, `alpha` and
    /// `beta` per plane.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    pub fn chroma_edge_v(u: &mut [u8], v: &mut [u8], at: usize, stride: usize, bs: [u8; 4], tc0: [[i16; 4]; 2], strong_edge: bool, alpha: [i32; 2], beta: [i32; 2]) {
        // vector r: p1 p0 q0 q1 of line r of Cb, then of Cr; transposed,
        // lane r of vector j is sample j of line r
        let t = i16x8::transpose(std::array::from_fn(|r| {
            let o = at + r * stride - 2;
            let mut s = [0u8; 8];
            s[..4].copy_from_slice(&u[o..o + 4]);
            s[4..].copy_from_slice(&v[o..o + 4]);
            load8(&s)
        }));
        let on = on(pairs(bs.map(|b| b as i16)));
        let (u0, u1) = chroma8(t[0], t[1], t[2], t[3], on, pairs(tc0[0]), i16x8::splat(alpha[0] as i16), i16x8::splat(beta[0] as i16), strong_edge);
        let (v0, v1) = chroma8(t[4], t[5], t[6], t[7], on, pairs(tc0[1]), i16x8::splat(alpha[1] as i16), i16x8::splat(beta[1] as i16), strong_edge);
        let out = i16x8::transpose([t[0], u0, u1, t[3], t[4], v0, v1, t[7]]);
        for (r, x) in out.into_iter().enumerate() {
            let o = at + r * stride - 2;
            let b = u8x16::narrow_i16x8(x, x).to_array();
            u[o..o + 4].copy_from_slice(&b[..4]);
            v[o..o + 4].copy_from_slice(&b[4..8]);
        }
    }

    /// Chroma across a horizontal edge: eight columns from `at`, `bs` and
    /// `tc0` per two columns.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    pub fn chroma_edge_h(pl: &mut [u8], at: usize, stride: usize, bs: [u8; 4], tc0: [i16; 4], strong_edge: bool, alpha: i32, beta: i32) {
        let row = |k: usize| load8(&pl[at + k * stride - 2 * stride..]);
        let (p1, p0, q0, q1) = (row(0), row(1), row(2), row(3));
        let (p0, q0) = chroma8(p1, p0, q0, q1, on(pairs(bs.map(|b| b as i16))), pairs(tc0), i16x8::splat(alpha as i16), i16x8::splat(beta as i16), strong_edge);
        store8(p0, &mut pl[at - stride..]);
        store8(q0, &mut pl[at..]);
    }
}

/// The edge filters line by line (builds without the `simd` feature, and
/// the reference the tests hold the SIMD filters to).
#[cfg(any(test, not(feature = "simd")))]
mod scalar {
    use super::{chroma_line, luma_line};

    /// Luma across a vertical edge: sixteen lines `stride` apart from `at`
    /// (the first line's q0), `bs` and `tc0` per four lines.
    #[allow(clippy::too_many_arguments)]
    pub fn luma_edge_v(pl: &mut [u8], at: usize, stride: usize, bs: [u8; 4], tc0: [i16; 4], _strong_edge: bool, alpha: i32, beta: i32) {
        for r in 0..16 {
            let k = r / 4;
            if bs[k] != 0 {
                let o = at + r * stride - 4;
                luma_line((&mut pl[o..o + 8]).try_into().unwrap(), bs[k], alpha, beta, tc0[k] as i32);
            }
        }
    }

    /// Luma across a horizontal edge: sixteen columns from `at`, `bs` and
    /// `tc0` per four columns.
    #[allow(clippy::too_many_arguments)]
    pub fn luma_edge_h(pl: &mut [u8], at: usize, stride: usize, bs: [u8; 4], tc0: [i16; 4], _strong_edge: bool, alpha: i32, beta: i32) {
        for c in 0..16 {
            let k = c / 4;
            if bs[k] != 0 {
                let o = at + c - 4 * stride;
                let mut s: [u8; 8] = std::array::from_fn(|i| pl[o + i * stride]);
                luma_line(&mut s, bs[k], alpha, beta, tc0[k] as i32);
                for (i, v) in s.into_iter().enumerate() {
                    pl[o + i * stride] = v;
                }
            }
        }
    }

    /// Chroma across a vertical edge of both planes: eight lines from `at`
    /// in each, `bs` per two lines; `tc0`, `alpha` and `beta` per plane.
    #[allow(clippy::too_many_arguments)]
    pub fn chroma_edge_v(u: &mut [u8], v: &mut [u8], at: usize, stride: usize, bs: [u8; 4], tc0: [[i16; 4]; 2], _strong_edge: bool, alpha: [i32; 2], beta: [i32; 2]) {
        for (c, plane) in [u, v].into_iter().enumerate() {
            for r in 0..8 {
                let k = r / 2;
                if bs[k] != 0 {
                    let o = at + r * stride - 2;
                    chroma_line((&mut plane[o..o + 4]).try_into().unwrap(), bs[k], alpha[c], beta[c], tc0[c][k] as i32);
                }
            }
        }
    }

    /// Chroma across a horizontal edge: eight columns from `at`, `bs` and
    /// `tc0` per two columns.
    #[allow(clippy::too_many_arguments)]
    pub fn chroma_edge_h(pl: &mut [u8], at: usize, stride: usize, bs: [u8; 4], tc0: [i16; 4], _strong_edge: bool, alpha: i32, beta: i32) {
        for c in 0..8 {
            let k = c / 2;
            if bs[k] != 0 {
                let o = at + c - 2 * stride;
                let mut s: [u8; 4] = std::array::from_fn(|i| pl[o + i * stride]);
                chroma_line(&mut s, bs[k], alpha, beta, tc0[k] as i32);
                for (i, v) in s.into_iter().enumerate() {
                    pl[o + i * stride] = v;
                }
            }
        }
    }
}

/// Filter one line of chroma samples across a vertical edge: `s` holds
/// p1 p0 q0 q1.
#[inline(always)]
fn chroma_line(s: &mut [u8; 4], bs: u8, alpha: i32, beta: i32, tc0: i32) {
    let p1 = s[0] as i32;
    let p0 = s[1] as i32;
    let q0 = s[2] as i32;
    let q1 = s[3] as i32;
    if (p0 - q0).abs() >= alpha || (p1 - p0).abs() >= beta || (q1 - q0).abs() >= beta {
        return;
    }
    if bs == 4 {
        s[1] = ((2 * p1 + p0 + q1 + 2) >> 2) as u8;
        s[2] = ((2 * q1 + q0 + p1 + 2) >> 2) as u8;
    } else {
        let tc = tc0 + 1;
        let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
        s[1] = clip(p0 + delta);
        s[2] = clip(q0 - delta);
    }
}

/// alpha, beta and the tc0 row for an edge between macroblocks of QP
/// `qp_p` and `qp_q` with the current MB's filter offsets.
#[inline(always)]
fn thresholds(qp_p: i32, qp_q: i32, cur: &MbDeblockInfo) -> (i32, i32, [u8; 3]) {
    let qpav = (qp_p + qp_q + 1) >> 1;
    let index_a = (qpav + cur.alpha_offset).clamp(0, 51) as usize;
    let index_b = (qpav + cur.beta_offset).clamp(0, 51) as usize;
    (ALPHA[index_a] as i32, BETA[index_b] as i32, TC0[index_a])
}

/// Luma across the vertical edges of a macroblock whose first sample is
/// `at` (lines `stride` apart): edge 0 against the left macroblock, of QP
/// `left_qp` (None leaves it alone), then edges 1..3.
#[inline(always)]
fn luma_v(y: &mut [u8], at: usize, stride: usize, cur: &MbDeblockInfo, bs: &[[u8; 4]; 4], left_qp: Option<i32>) {
    for e in 0..4 {
        if (e == 0 && left_qp.is_none()) || (cur.transform8x8 && e % 2 == 1) || bs[e] == [0; 4] {
            continue;
        }
        let qp_p = if e == 0 { left_qp.unwrap_or(cur.qp) } else { cur.qp };
        let (alpha, beta, tc0s) = thresholds(qp_p, cur.qp, cur);
        luma_edge_v(y, at + e * 4, stride, bs[e], tc0_of(bs[e], tc0s), strong(bs[e]), alpha, beta);
    }
}

/// Luma across the horizontal edges: edge 0 against the macroblock above,
/// of QP `above_qp` (None leaves it alone), then edges 1..3.
#[inline(always)]
fn luma_h(y: &mut [u8], at: usize, stride: usize, cur: &MbDeblockInfo, bs: &[[u8; 4]; 4], above_qp: Option<i32>) {
    for e in 0..4 {
        if (e == 0 && above_qp.is_none()) || (cur.transform8x8 && e % 2 == 1) || bs[e] == [0; 4] {
            continue;
        }
        let qp_p = if e == 0 { above_qp.unwrap_or(cur.qp) } else { cur.qp };
        let (alpha, beta, tc0s) = thresholds(qp_p, cur.qp, cur);
        luma_edge_h(y, at + e * 4 * stride, stride, bs[e], tc0_of(bs[e], tc0s), strong(bs[e]), alpha, beta);
    }
}

/// Chroma across the vertical edges 0 and 4 of both components (with the
/// strengths of luma edges 0 and 2): edge 0 against the left macroblock, of
/// chroma QPs `left_qpc` (None leaves it alone).
#[inline(always)]
fn chroma_v(u: &mut [u8], v: &mut [u8], at: usize, stride: usize, cur: &MbDeblockInfo, bs_v: &[[u8; 4]; 4], left_qpc: Option<[i32; 2]>) {
    for (e, luma_e) in [(0usize, 0usize), (4, 2)] {
        if (e == 0 && left_qpc.is_none()) || bs_v[luma_e] == [0; 4] {
            continue;
        }
        let qpc_p = if e == 0 { left_qpc.unwrap_or(cur.qpc) } else { cur.qpc };
        let th = [0, 1].map(|comp| thresholds(qpc_p[comp], cur.qpc[comp], cur));
        let bs = bs_v[luma_e];
        chroma_edge_v(u, v, at + e, stride, bs, th.map(|t| tc0_of(bs, t.2)), strong(bs), th.map(|t| t.0), th.map(|t| t.1));
    }
}

/// Chroma component `comp` across the horizontal edges 0 and 4: edge 0
/// against the macroblock above, of chroma QP `above_qpc` (None leaves it
/// alone).
#[inline(always)]
fn chroma_h(plane: &mut [u8], at: usize, stride: usize, cur: &MbDeblockInfo, comp: usize, bs_h: &[[u8; 4]; 4], above_qpc: Option<i32>) {
    for (e, luma_e) in [(0usize, 0usize), (4, 2)] {
        if (e == 0 && above_qpc.is_none()) || bs_h[luma_e] == [0; 4] {
            continue;
        }
        let qp_p = if e == 0 { above_qpc.unwrap_or(cur.qpc[comp]) } else { cur.qpc[comp] };
        let (alpha, beta, tc0s) = thresholds(qp_p, cur.qpc[comp], cur);
        let bs = bs_h[luma_e];
        chroma_edge_h(plane, at + e * stride, stride, bs, tc0_of(bs, tc0s), strong(bs), alpha, beta);
    }
}

/// Deblock the picture (a frame, or the field `structure` of it) in
/// macroblock order.
pub fn filter_picture(pic: &mut Picture, mbs: &[MbDeblockInfo], width_mbs: usize, height_mbs: usize, structure: u8) {
    if pic.mbaff && structure == FRAME {
        return filter_picture_mbaff(pic, mbs, width_mbs, height_mbs);
    }
    let w4 = pic.width / 4;
    let field = structure != FRAME;
    let parity = (structure == BOTTOM) as usize;
    let mvy_limit = if field { 2 } else { 4 };
    // strides and macroblock rows of the field / frame being filtered
    let lw = if field { 2 * pic.width } else { pic.width };
    let cw = if field { pic.width } else { pic.width / 2 };
    let rows = if field { height_mbs / 2 } else { height_mbs };
    let row_step = if field { 2 } else { 1 };
    for row in 0..rows {
        let my = if field { 2 * row + parity } else { row };
        for mx in 0..width_mbs {
            let cur = mbs[my * width_mbs + mx];
            if !cur.decoded || cur.filter_idc == 1 {
                continue;
            }
            // the neighbours across whose edges the filter works
            let across = |n: &MbDeblockInfo| n.decoded && !(cur.filter_idc == 2 && n.slice != cur.slice);
            let left = if mx > 0 { Some(mbs[my * width_mbs + mx - 1]) } else { None };
            let left = if left.as_ref().is_some_and(across) { left } else { None };
            let above = if row > 0 { Some(mbs[(my - row_step) * width_mbs + mx]) } else { None };
            let above = if above.as_ref().is_some_and(across) { above } else { None };
            // boundary strengths: [edge 0..4][segment 0..4]
            let mut bs_v = [[0u8; 4]; 4];
            let mut bs_h = [[0u8; 4]; 4];
            let bx0 = mx * 4;
            let by0 = my * 4;
            if let Some(l) = left {
                for k in 0..4 {
                    let qb = (by0 + k) * w4 + bx0;
                    bs_v[0][k] = boundary_strength(pic, &l, &cur, qb - 1, qb, 1 << (k * 4 + 3), 1 << (k * 4), true, mvy_limit);
                }
            }
            if let Some(a) = above {
                // the last block row of the macroblock above (of the same field)
                let above_off = (4 * row_step - 3) * w4;
                for k in 0..4 {
                    let qb = by0 * w4 + bx0 + k;
                    bs_h[0][k] = boundary_strength(pic, &a, &cur, qb - above_off, qb, 1 << (12 + k), 1 << k, !field, mvy_limit);
                }
            }
            internal_bs(pic, &cur, bx0, by0, w4, mvy_limit, &mut bs_v, &mut bs_h);
            let x0 = mx * 16;
            // the first luma / chroma line of the macroblock, in lines of the frame
            let y0 = if field { 32 * row + parity } else { 16 * my };
            let ybase = y0 * pic.width + x0;
            let cybase = (if field { 16 * row + parity } else { 8 * my }) * (pic.width / 2) + mx * 8;
            // luma, vertical edges then horizontal edges; chroma, the vertical
            // edges of both components together (each component's vertical
            // edges still come before its horizontal ones)
            luma_v(&mut pic.y, ybase, lw, &cur, &bs_v, left.map(|l| l.qp));
            luma_h(&mut pic.y, ybase, lw, &cur, &bs_h, above.map(|a| a.qp));
            chroma_v(&mut pic.u, &mut pic.v, cybase, cw, &cur, &bs_v, left.map(|l| l.qpc));
            for comp in 0..2 {
                let plane = if comp == 0 { &mut pic.u } else { &mut pic.v };
                chroma_h(plane, cybase, cw, &cur, comp, &bs_h, above.map(|a| a.qpc[comp]));
            }
        }
    }
}

/// Deblock an MBAFF frame (8.7 with MbaffFrameFlag = 1): pair by pair, top
/// then bottom macroblock, each along its own kind of rows. An edge between
/// a frame macroblock and a field pair is "mixed": the left one is filtered
/// row by row against whichever macroblock of the left pair owns the row
/// (eight strengths, two quantiser averages), the top edge of a frame
/// macroblock under a field pair is filtered twice in field mode (once per
/// field macroblock above), and the top edge of a field macroblock over a
/// frame pair in field mode against the frame macroblock's interleaved rows;
/// mixed edges get bS 1 (2 with coefficients) instead of a motion test.
fn filter_picture_mbaff(pic: &mut Picture, mbs: &[MbDeblockInfo], wm: usize, hm: usize) {
    let width = pic.width;
    let cwidth = width / 2;
    let w4 = width / 4;
    for pr in 0..hm / 2 {
        for mx in 0..wm {
            for b in 0..2 {
                let my = 2 * pr + b;
                let addr = my * wm + mx;
                let cur = mbs[addr];
                if !cur.decoded || cur.filter_idc == 1 {
                    continue;
                }
                let field = pic.mb_field[addr];
                let mvy_limit = if field { 2 } else { 4 };
                let (lw, cw) = if field { (2 * width, 2 * cwidth) } else { (width, cwidth) };
                let ybase = if field { (32 * pr + b) * width } else { 16 * my * width } + 16 * mx;
                let cybase = if field { (16 * pr + b) * cwidth } else { 8 * my * cwidth } + 8 * mx;
                let (bx0, by0) = (mx * 4, my * 4);
                let avail = |n: &MbDeblockInfo| n.decoded && !(cur.filter_idc == 2 && n.slice != cur.slice);
                let mut bs_v = [[0u8; 4]; 4];
                let mut bs_h = [[0u8; 4]; 4];
                // ---- the left edge: the left pair's macroblock of this row, or both of them
                let left_top = (2 * pr * wm + mx).wrapping_sub(1);
                let do_left = mx > 0 && avail(&mbs[left_top]);
                let mixed_left = do_left && pic.mb_field[left_top] != field;
                // the left macroblock when it is of this one's kind (a mixed
                // edge is filtered row by row below)
                let left = if do_left && !mixed_left { Some(mbs[addr - 1]) } else { None };
                let mut bs_left8 = [0u8; 8];
                if let Some(l) = left {
                    for k in 0..4 {
                        let qb = (by0 + k) * w4 + bx0;
                        bs_v[0][k] = boundary_strength(pic, &l, &cur, qb - 1, qb, 1 << (k * 4 + 3), 1 << (k * 4), true, mvy_limit);
                    }
                } else if mixed_left {
                    let lt = mbs[left_top];
                    let lb = mbs[left_top + wm];
                    for (i, bs) in bs_left8.iter_mut().enumerate() {
                        // segment i is two rows of this macroblock (block row i / 2); the
                        // left macroblock owning them and its block row there:
                        let (nb, nb_row) = if field {
                            // a field macroblock's rows 0..7 lie in the left pair's top frame
                            // macroblock, 8..15 in its bottom one
                            (if i < 4 { lt } else { lb }, i & 3)
                        } else {
                            // a frame macroblock's even rows belong to the left top field
                            // macroblock, its odd rows to the bottom one
                            (if i & 1 == 0 { lt } else { lb }, 2 * b + (i >> 2))
                        };
                        *bs = if cur.intra || nb.intra {
                            4
                        } else {
                            let cur_nz = cur.nonzero & (1 << ((i >> 1) * 4)) != 0;
                            let nb_nz = nb.nonzero & (1 << (nb_row * 4 + 3)) != 0;
                            1 + (cur_nz || nb_nz) as u8
                        };
                    }
                }
                // ---- the top edge: the macroblock above in this macroblock's kind of rows
                let above_field_pair = pr > 0 && pic.mb_field[(2 * pr - 2) * wm + mx];
                // a frame macroblock under a field pair: filtered once per field
                let double_top = !field && b == 0 && above_field_pair && avail(&mbs[(2 * pr - 1) * wm + mx]);
                let above_addr: Option<usize> = if !field {
                    if my > 0 {
                        Some(addr - wm)
                    } else {
                        None
                    }
                } else if pr == 0 {
                    None
                } else if b == 0 {
                    Some(if above_field_pair { (2 * pr - 2) * wm + mx } else { (2 * pr - 1) * wm + mx })
                } else {
                    Some((2 * pr - 1) * wm + mx)
                };
                let above_addr = above_addr.filter(|&a| avail(&mbs[a]) && !double_top);
                if let Some(a_addr) = above_addr {
                    let a = mbs[a_addr];
                    let a_field = pic.mb_field[a_addr];
                    let mixed = a_field != field;
                    let strong = !field && !a_field;
                    let a_row3 = ((a_addr / wm) * 4 + 3) * w4 + bx0;
                    for k in 0..4 {
                        let qb = by0 * w4 + bx0 + k;
                        bs_h[0][k] = if mixed {
                            if cur.intra || a.intra {
                                3
                            } else {
                                1 + ((cur.nonzero >> k) & 1 != 0 || (a.nonzero >> (12 + k)) & 1 != 0) as u8
                            }
                        } else {
                            boundary_strength(pic, &a, &cur, a_row3 + k, qb, 1 << (12 + k), 1 << k, strong, mvy_limit)
                        };
                    }
                }
                // ---- internal edges
                internal_bs(pic, &cur, bx0, by0, w4, mvy_limit, &mut bs_v, &mut bs_h);
                // ---- luma, vertical edges
                if mixed_left {
                    let lt = mbs[left_top];
                    let lb = mbs[left_top + wm];
                    let th = [thresholds(lt.qp, cur.qp, &cur), thresholds(lb.qp, cur.qp, &cur)];
                    for r in 0..16 {
                        let (bs, half) = if field { (bs_left8[r / 2], (r >= 8) as usize) } else { (bs_left8[2 * (r / 4) + (r & 1)], r & 1) };
                        if bs == 0 {
                            continue;
                        }
                        let (alpha, beta, tc0s) = th[half];
                        let tc0 = if bs < 4 { tc0s[bs as usize - 1] as i32 } else { 0 };
                        let o = ybase + r * lw - 4;
                        let s: &mut [u8; 8] = (&mut pic.y[o..o + 8]).try_into().unwrap();
                        luma_line(s, bs, alpha, beta, tc0);
                    }
                }
                luma_v(&mut pic.y, ybase, lw, &cur, &bs_v, left.map(|l| l.qp));
                // ---- luma, horizontal edges
                let mut double_bs = [[0u8; 4]; 2];
                if double_top {
                    for j in 0..2 {
                        let nb = mbs[(2 * pr - 2 + j) * wm + mx];
                        for i in 0..4 {
                            double_bs[j][i] = if cur.intra || nb.intra { 3 } else { 1 + ((cur.nonzero >> i) & 1 != 0 || (nb.nonzero >> (12 + i)) & 1 != 0) as u8 };
                        }
                        let (alpha, beta, tc0s) = thresholds(nb.qp, cur.qp, &cur);
                        // the field's rows of this frame macroblock (j, j + 2, ...) against
                        // the field macroblock above (rows -2, -4, ... from row j)
                        luma_edge_h(&mut pic.y, ybase + j * width, 2 * width, double_bs[j], tc0_of(double_bs[j], tc0s), false, alpha, beta);
                    }
                }
                let above = above_addr.map(|a| mbs[a]);
                luma_h(&mut pic.y, ybase, lw, &cur, &bs_h, above.map(|a| a.qp));
                // ---- chroma (the vertical edges of both components together:
                // each component's vertical edges still come before its
                // horizontal ones)
                if mixed_left {
                    let lt = mbs[left_top];
                    let lb = mbs[left_top + wm];
                    for comp in 0..2 {
                        let th = [thresholds(lt.qpc[comp], cur.qpc[comp], &cur), thresholds(lb.qpc[comp], cur.qpc[comp], &cur)];
                        let plane = if comp == 0 { &mut pic.u } else { &mut pic.v };
                        for r in 0..8 {
                            let bs = bs_left8[r];
                            let half = if field { (r >= 4) as usize } else { r & 1 };
                            if bs == 0 {
                                continue;
                            }
                            let (alpha, beta, tc0s) = th[half];
                            let tc0 = if bs < 4 { tc0s[bs as usize - 1] as i32 } else { 0 };
                            let o = cybase + r * cw - 2;
                            let s: &mut [u8; 4] = (&mut plane[o..o + 4]).try_into().unwrap();
                            chroma_line(s, bs, alpha, beta, tc0);
                        }
                    }
                }
                chroma_v(&mut pic.u, &mut pic.v, cybase, cw, &cur, &bs_v, left.map(|l| l.qpc));
                for comp in 0..2 {
                    let plane = if comp == 0 { &mut pic.u } else { &mut pic.v };
                    if double_top {
                        for j in 0..2 {
                            let nb = mbs[(2 * pr - 2 + j) * wm + mx];
                            let (alpha, beta, tc0s) = thresholds(nb.qpc[comp], cur.qpc[comp], &cur);
                            chroma_edge_h(plane, cybase + j * cwidth, 2 * cwidth, double_bs[j], tc0_of(double_bs[j], tc0s), false, alpha, beta);
                        }
                    }
                    chroma_h(plane, cybase, cw, &cur, comp, &bs_h, above.map(|a| a.qpc[comp]));
                }
            }
        }
    }
}

#[cfg(all(test, feature = "simd"))]
mod tests {
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u32) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            ((self.0 >> 32) as u32) % n
        }
    }

    const S: usize = 32;

    /// A 32x32 plane of lines that are flat, stepped at the middle, noisy
    /// or random, along rows (`across_x`) or columns, so that each of the
    /// filter's conditions goes both ways.
    fn plane(rng: &mut Rng, across_x: bool) -> Vec<u8> {
        let mut p = vec![0u8; S * S];
        for line in 0..S {
            let base = rng.below(256) as i32;
            let step = rng.below(81) as i32 - 40;
            let noise = [0, 1, 3, 8, 40][rng.below(5) as usize];
            for i in 0..S {
                let v = if noise == 40 && rng.below(4) == 0 {
                    rng.below(256) as i32
                } else {
                    base + if i >= S / 2 { step } else { 0 } + rng.below(2 * noise + 1) as i32 - noise as i32
                };
                let at = if across_x { line * S + i } else { i * S + line };
                p[at] = v.clamp(0, 255) as u8;
            }
        }
        p
    }

    /// A random edge: strengths per segment (all 4 or each 0..3) and the
    /// thresholds of random indices.
    fn edge(rng: &mut Rng) -> ([u8; 4], i32, i32, [u8; 3]) {
        let bs = if rng.below(4) == 0 { [4; 4] } else { [0; 4].map(|_: u8| rng.below(4) as u8) };
        let a = rng.below(52) as usize;
        let b = rng.below(52) as usize;
        (bs, ALPHA[a] as i32, BETA[b] as i32, TC0[a])
    }

    #[test]
    fn simd_edges_filter_as_the_line_filters() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..20000 {
            // luma, vertical edge at x = 16 over rows 8..24
            let (bs, alpha, beta, tc0s) = edge(&mut rng);
            let tc0 = tc0_of(bs, tc0s);
            let mut want = plane(&mut rng, true);
            let mut got = want.clone();
            scalar::luma_edge_v(&mut want, 8 * S + 16, S, bs, tc0, strong(bs), alpha, beta);
            simd::luma_edge_v(&mut got, 8 * S + 16, S, bs, tc0, strong(bs), alpha, beta);
            assert_eq!(want, got, "luma vertical: bs {bs:?} alpha {alpha} beta {beta} tc0 {tc0:?}");

            // luma, horizontal edge at y = 16 over columns 8..24
            let (bs, alpha, beta, tc0s) = edge(&mut rng);
            let tc0 = tc0_of(bs, tc0s);
            let mut want = plane(&mut rng, false);
            let mut got = want.clone();
            scalar::luma_edge_h(&mut want, 16 * S + 8, S, bs, tc0, strong(bs), alpha, beta);
            simd::luma_edge_h(&mut got, 16 * S + 8, S, bs, tc0, strong(bs), alpha, beta);
            assert_eq!(want, got, "luma horizontal: bs {bs:?} alpha {alpha} beta {beta} tc0 {tc0:?}");

            // chroma, vertical edge at x = 16 over rows 8..16 of both planes
            // (thresholds of their own, as with a second chroma QP offset)
            let (bs, alpha, beta, tc0s) = edge(&mut rng);
            let (_, alpha2, beta2, tc0s2) = edge(&mut rng);
            let tc = [tc0_of(bs, tc0s), tc0_of(bs, tc0s2)];
            let mut want = [plane(&mut rng, true), plane(&mut rng, true)];
            let mut got = want.clone();
            let [u, v] = &mut want;
            scalar::chroma_edge_v(u, v, 8 * S + 16, S, bs, tc, strong(bs), [alpha, alpha2], [beta, beta2]);
            let [u, v] = &mut got;
            simd::chroma_edge_v(u, v, 8 * S + 16, S, bs, tc, strong(bs), [alpha, alpha2], [beta, beta2]);
            assert_eq!(want, got, "chroma vertical: bs {bs:?} alpha {alpha}/{alpha2} beta {beta}/{beta2} tc0 {tc:?}");

            // chroma, horizontal edge at y = 16 over columns 8..16
            let (bs, alpha, beta, tc0s) = edge(&mut rng);
            let tc0 = tc0_of(bs, tc0s);
            let mut want = plane(&mut rng, false);
            let mut got = want.clone();
            scalar::chroma_edge_h(&mut want, 16 * S + 8, S, bs, tc0, strong(bs), alpha, beta);
            simd::chroma_edge_h(&mut got, 16 * S + 8, S, bs, tc0, strong(bs), alpha, beta);
            assert_eq!(want, got, "chroma horizontal: bs {bs:?} alpha {alpha} beta {beta} tc0 {tc0:?}");
        }
    }
}
