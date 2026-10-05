//! The loop filter (RFC 6386 section 15): the normal filter on luma and
//! chroma or the simple filter on luma alone, across macroblock edges and,
//! for macroblocks with coefficients or subblock prediction, the edges
//! between subblocks.
//!
//! A frame is filtered macroblock by macroblock in raster order, each one
//! left edge, inner vertical edges, top edge, inner horizontal edges, one
//! macroblock row behind the decoding (intra prediction reads unfiltered
//! samples, and the row being decoded is never touched by filtering the
//! row above it).

use crate::picture::Picture;

/// How one macroblock is filtered.
#[derive(Clone, Copy, Default, Debug)]
pub struct MbFilter {
    /// 0 leaves the macroblock alone.
    pub level: u8,
    pub interior_limit: u8,
    pub hev_threshold: u8,
    /// Filter the edges between subblocks too.
    pub inner: bool,
}

impl MbFilter {
    /// 15.2 and 15.4: the limits of a level, given the frame's sharpness.
    pub fn new(level: i32, sharpness: i32, key_frame: bool, inner: bool) -> MbFilter {
        let level = level.clamp(0, 63);
        let mut interior = level;
        if sharpness > 0 {
            interior >>= (sharpness + 3) >> 2;
            interior = interior.min(9 - sharpness);
        }
        let interior = interior.max(1);
        let hev = match (key_frame, level) {
            (_, 40..) => 2 + !key_frame as u8,
            (false, 20..) => 2,
            (_, 15..) => 1,
            _ => 0,
        };
        MbFilter { level: level as u8, interior_limit: interior as u8, hev_threshold: hev, inner }
    }
}

/// The filter across one edge, with its limits (the edge limit is at most
/// 193, the interior limit 63).
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// 15.3: the normal filter across a macroblock edge (up to three
    /// samples each side change).
    Mb { edge_limit: i32, interior: i32, hev: i32 },
    /// 15.3: the normal filter across a subblock edge (up to two).
    Inner { edge_limit: i32, interior: i32, hev: i32 },
    /// 15.2: the simple filter (one).
    Simple { edge_limit: i32 },
}

#[inline(always)]
fn clamp_s8(v: i32) -> i32 {
    v.clamp(-128, 127)
}

#[inline(always)]
fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// One position of an edge: `p[i]` is the i-th sample before it (0
/// nearest), `q[i]` the i-th after, `step` apart in `buf` from `at` (the
/// first sample after the edge). Written as ffmpeg writes it, on unsigned
/// samples; the differences are those of the RFC's signed samples.
#[cfg_attr(feature = "simd", allow(dead_code))]
fn filter_position(buf: &mut [u8], at: usize, step: usize, kind: Kind) {
    let s = |k: isize| buf[(at as isize + k * step as isize) as usize] as i32;
    let p = [s(-1), s(-2), s(-3), s(-4)];
    let q = [s(0), s(1), s(2), s(3)];
    let simple_limit = |limit: i32| 2 * (p[0] - q[0]).abs() + ((p[1] - q[1]).abs() >> 1) <= limit;
    let interior_ok = |i: i32| (p[3] - p[2]).abs() <= i && (p[2] - p[1]).abs() <= i && (p[1] - p[0]).abs() <= i && (q[3] - q[2]).abs() <= i && (q[2] - q[1]).abs() <= i && (q[1] - q[0]).abs() <= i;
    let high_variance = |t: i32| (p[1] - p[0]).abs() > t || (q[1] - q[0]).abs() > t;
    // move p0 and q0 towards each other (estimating the step with the outer
    // taps when `outer`), and when not `outer` p1 and q1 by half as much;
    // (a + 4) >> 3 and (a + 3) >> 3 round the halves apart
    let adjust = |buf: &mut [u8], outer: bool| {
        let a = clamp_s8(if outer { clamp_s8(p[1] - q[1]) } else { 0 } + 3 * (q[0] - p[0]));
        let f1 = (a + 4).min(127) >> 3;
        let f2 = (a + 3).min(127) >> 3;
        buf[at - step] = clamp_u8(p[0] + f2);
        buf[at] = clamp_u8(q[0] - f1);
        if !outer {
            let a = (f1 + 1) >> 1;
            buf[at - 2 * step] = clamp_u8(p[1] + a);
            buf[at + step] = clamp_u8(q[1] - a);
        }
    };
    match kind {
        Kind::Simple { edge_limit } => {
            if simple_limit(edge_limit) {
                adjust(buf, true);
            }
        }
        Kind::Inner { edge_limit, interior, hev } => {
            if simple_limit(edge_limit) && interior_ok(interior) {
                adjust(buf, high_variance(hev));
            }
        }
        Kind::Mb { edge_limit, interior, hev } => {
            if !simple_limit(edge_limit) || !interior_ok(interior) {
                return;
            }
            if high_variance(hev) {
                adjust(buf, true);
                return;
            }
            // 27/128, 18/128 and 9/128 of the step: about 3/7, 2/7, 1/7
            let w = clamp_s8(clamp_s8(p[1] - q[1]) + 3 * (q[0] - p[0]));
            let a = [(27 * w + 63) >> 7, (18 * w + 63) >> 7, (9 * w + 63) >> 7];
            for (i, &a) in a.iter().enumerate() {
                buf[at - (i + 1) * step] = clamp_u8(p[i] + a);
                buf[at + i * step] = clamp_u8(q[i] - a);
            }
        }
    }
}

/// Sixteen positions of an edge at once in byte lanes (wasm simd128, SSE2
/// or NEON through `wide`): sixteen of luma, or eight of U beside the same
/// eight of V, whose limits are the same. It computes exactly what
/// `filter_position` does, as libvpx's SIMD filters do: the differences
/// across the edge on unsigned bytes with saturation, the adjustments on
/// signed bytes (samples less 128), where saturating is clamping, and the
/// adjustments of positions left alone masked to zero.
#[cfg(feature = "simd")]
mod simd {
    use super::Kind;
    use bytemuck::cast;
    use wide::{i16x8, i8x16, u16x8, u8x16};

    #[inline(always)]
    fn splat(v: i32) -> u8x16 {
        u8x16::splat(v as u8)
    }

    /// |a - b| of unsigned bytes.
    #[inline(always)]
    fn absdiff(a: u8x16, b: u8x16) -> u8x16 {
        a.saturating_sub(b) | b.saturating_sub(a)
    }

    /// Unsigned bytes shifted right by `N`: through 16-bit lanes, dropping
    /// the bits that cross from one byte into the next.
    #[inline(always)]
    fn shr_u<const N: i32>(v: u8x16) -> u8x16 {
        cast::<u16x8, u8x16>(cast::<u8x16, u16x8>(v) >> N) & splat(0xff >> N)
    }

    /// Signed bytes shifted right by `N`: the logical shift with its sign
    /// bit (bit 7 - `N`) extended.
    #[inline(always)]
    fn shr_s<const N: i32>(v: i8x16) -> i8x16 {
        let sign = 0x80 >> N;
        cast::<u8x16, i8x16>(shr_u::<N>(cast(v)) ^ splat(sign)) - i8x16::splat(sign as i8)
    }

    /// Samples as signed bytes (less 128), and back.
    #[inline(always)]
    fn signed(v: u8x16) -> i8x16 {
        cast(v ^ splat(0x80))
    }

    #[inline(always)]
    fn unsigned(v: i8x16) -> u8x16 {
        cast::<i8x16, u8x16>(v) ^ splat(0x80)
    }

    /// (k w + 63) >> 7 for k = 27, 18 and 9, the macroblock filter's
    /// 3/7, 2/7 and 1/7 of the step, through 16-bit lanes.
    #[inline(always)]
    fn weights(w: i8x16) -> [i8x16; 3] {
        // a byte beside itself is 257 w, whose high byte is w
        let w: u8x16 = cast(w);
        let lo9 = (cast::<u8x16, i16x8>(u8x16::unpack_low(w, w)) >> 8) * 9i16;
        let hi9 = (cast::<u8x16, i16x8>(u8x16::unpack_high(w, w)) >> 8) * 9i16;
        let round = |lo: i16x8, hi: i16x8| i8x16::from_i16x16_saturate(cast([(lo + 63i16) >> 7, (hi + 63i16) >> 7]));
        [round(lo9 * 3i16, hi9 * 3i16), round(lo9 + lo9, hi9 + hi9), round(lo9, hi9)]
    }

    /// Filter sixteen positions: `t` holds p3, p2, p1, p0, q0, q1, q2, q3.
    /// Returns whether any position is filtered.
    #[inline(always)]
    fn filter(t: &mut [u8x16; 8], kind: Kind) -> bool {
        let [p3, p2, p1, p0, q0, q1, q2, q3] = *t;
        let (edge_limit, interior, hev) = match kind {
            Kind::Simple { edge_limit } => (edge_limit, 0, 0),
            Kind::Inner { edge_limit, interior, hev } | Kind::Mb { edge_limit, interior, hev } => (edge_limit, interior, hev),
        };
        let simple = matches!(kind, Kind::Simple { .. });
        // non-zero where the position is left alone: 2 |p0 - q0| +
        // |p1 - q1| / 2 over the edge limit (saturating at 255, over every
        // limit), or a step within either side over the interior limit
        let d = absdiff(p0, q0);
        let mut over = d.saturating_add(d).saturating_add(shr_u::<1>(absdiff(p1, q1))).saturating_sub(splat(edge_limit));
        let (dp, dq) = (absdiff(p1, p0), absdiff(q1, q0));
        if !simple {
            let steps = absdiff(p3, p2).max(absdiff(p2, p1)).max(dp).max(dq).max(absdiff(q2, q1)).max(absdiff(q3, q2));
            over |= steps.saturating_sub(splat(interior));
        }
        let mask = over.cmp_eq(u8x16::ZERO);
        if mask.none() {
            return false;
        }
        let mask: i8x16 = cast(mask);
        // high edge variance, always for the simple filter
        let high: i8x16 = if simple { i8x16::splat(-1) } else { cast(!dp.max(dq).saturating_sub(splat(hev)).cmp_eq(u8x16::ZERO)) };
        let (ps1, ps0, qs0, qs1) = (signed(p1), signed(p0), signed(q0), signed(q1));
        let step = qs0.saturating_sub(ps0);
        // v + 3 (q0 - p0), clamped: saturating each addition clamps alike,
        // as the three go the same way
        let add_steps = |v: i8x16| v.saturating_add(step).saturating_add(step).saturating_add(step);
        let outer = ps1.saturating_sub(qs1);
        // how far q0 and p0 move: a / 8, the halves rounded apart
        let moves = |a: i8x16| (shr_s::<3>(a.saturating_add(i8x16::splat(4))), shr_s::<3>(a.saturating_add(i8x16::splat(3))));
        match kind {
            Kind::Simple { .. } => {
                let (f1, f2) = moves(add_steps(outer) & mask);
                t[3] = unsigned(ps0.saturating_add(f2));
                t[4] = unsigned(qs0.saturating_sub(f1));
            }
            Kind::Inner { .. } => {
                let (f1, f2) = moves(add_steps(outer & high) & mask);
                t[3] = unsigned(ps0.saturating_add(f2));
                t[4] = unsigned(qs0.saturating_sub(f1));
                // without high variance p1 and q1 move half as far
                let a = shr_s::<1>(f1 + i8x16::splat(1)) & !high;
                t[2] = unsigned(ps1.saturating_add(a));
                t[5] = unsigned(qs1.saturating_sub(a));
            }
            Kind::Mb { .. } => {
                // with high variance p0 and q0 alone move, as above;
                // without, the macroblock filter proper
                let w = add_steps(outer) & mask;
                let (f1, f2) = moves(w & high);
                let [a0, a1, a2] = weights(w & !high);
                t[1] = unsigned(signed(p2).saturating_add(a2));
                t[2] = unsigned(ps1.saturating_add(a1));
                t[3] = unsigned(ps0.saturating_add(f2).saturating_add(a0));
                t[4] = unsigned(qs0.saturating_sub(f1).saturating_sub(a0));
                t[5] = unsigned(qs1.saturating_sub(a1));
                t[6] = unsigned(signed(q2).saturating_sub(a2));
            }
        }
        true
    }

    /// The rows of p3 ..= q3 a filter can change.
    fn changed(kind: Kind) -> std::ops::Range<usize> {
        match kind {
            Kind::Mb { .. } => 1..7,
            Kind::Inner { .. } => 2..6,
            Kind::Simple { .. } => 3..5,
        }
    }

    /// Eight samples of `a` then eight of `b`.
    #[inline(always)]
    fn load8x2(a: &[u8], b: &[u8]) -> u8x16 {
        let mut v = [0u8; 16];
        v[..8].copy_from_slice(&a[..8]);
        v[8..].copy_from_slice(&b[..8]);
        u8x16::from(v)
    }

    /// Eight samples, in the low half.
    #[inline(always)]
    fn load8(a: &[u8]) -> u8x16 {
        load8x2(a, &[0; 8])
    }

    /// The first `n` samples of `rows`, which lose them: the rows of a
    /// plane taken one after another, each checked against the plane once.
    #[inline(always)]
    fn take<'a>(rows: &mut &'a [u8], n: usize) -> &'a [u8] {
        let (first, rest) = rows.split_at(n);
        *rows = rest;
        first
    }

    #[inline(always)]
    fn take_mut<'a>(rows: &mut &'a mut [u8], n: usize) -> &'a mut [u8] {
        let (first, rest) = std::mem::take(rows).split_at_mut(n);
        *rows = rest;
        first
    }

    /// Two rows of eight samples from a vector, into the two rows of the
    /// plane `lines` begins with, at column `x`.
    #[inline(always)]
    fn store_pair(pair: u8x16, lines: &mut [u8], stride: usize, x: usize) {
        let s = pair.as_array_ref();
        lines[x..x + 8].copy_from_slice(&s[..8]);
        lines[stride + x..stride + x + 8].copy_from_slice(&s[8..]);
    }

    /// One round of byte interleaving: outputs 2j and 2j + 1 are the low
    /// and the high halves of inputs j and j + 4 interleaved.
    #[inline(always)]
    fn interleave(v: [u8x16; 8]) -> [u8x16; 8] {
        let [a0, a1, a2, a3, b0, b1, b2, b3] = v;
        let (lo, hi) = (u8x16::unpack_low, u8x16::unpack_high);
        [lo(a0, b0), hi(a0, b0), lo(a1, b1), hi(a1, b1), lo(a2, b2), hi(a2, b2), lo(a3, b3), hi(a3, b3)]
    }

    /// Filter across a vertical edge: sixteen rows of eight samples (four
    /// each side, in their low halves) become eight columns of sixteen,
    /// each round of interleaving moving one bit of the row number into the
    /// lane number and one of the column number into the vector's, and
    /// three rounds more take the columns back to rows, two to a vector
    /// (none if nothing changed).
    #[inline(always)]
    fn vertical(rows: &[u8x16; 16], kind: Kind) -> Option<[u8x16; 8]> {
        let mut t = [u8x16::ZERO; 8];
        for (k, v) in t.iter_mut().enumerate() {
            *v = u8x16::unpack_low(rows[k], rows[k + 8]);
        }
        let mut t = interleave(interleave(interleave(t)));
        filter(&mut t, kind).then(|| interleave(interleave(interleave(t))))
    }

    /// A luma edge along a row: the eight `rows` hold p3 ..= q3, the
    /// positions are columns `x` .. `x + 16`.
    #[inline(always)]
    pub fn horizontal_luma(rows: &mut [u8], stride: usize, x: usize, kind: Kind) {
        let mut t = [u8x16::ZERO; 8];
        let mut lines: &[u8] = rows;
        for t in t.iter_mut() {
            *t = u8x16::from(<[u8; 16]>::try_from(&take(&mut lines, stride)[x..x + 16]).unwrap());
        }
        if filter(&mut t, kind) {
            let changed = changed(kind);
            let mut lines = &mut rows[changed.start * stride..];
            for t in &t[changed] {
                take_mut(&mut lines, stride)[x..x + 16].copy_from_slice(t.as_array_ref());
            }
        }
    }

    /// Chroma edges along a row: columns `x` .. `x + 8` of the eight rows
    /// of each plane.
    #[inline(always)]
    pub fn horizontal_chroma(u: &mut [u8], v: &mut [u8], stride: usize, x: usize, kind: Kind) {
        let mut t = [u8x16::ZERO; 8];
        let (mut lu, mut lv): (&[u8], &[u8]) = (u, v);
        for t in t.iter_mut() {
            *t = load8x2(&take(&mut lu, stride)[x..], &take(&mut lv, stride)[x..]);
        }
        if filter(&mut t, kind) {
            let changed = changed(kind);
            let (mut lu, mut lv) = (&mut u[changed.start * stride..], &mut v[changed.start * stride..]);
            for t in &t[changed] {
                let s = t.as_array_ref();
                take_mut(&mut lu, stride)[x..x + 8].copy_from_slice(&s[..8]);
                take_mut(&mut lv, stride)[x..x + 8].copy_from_slice(&s[8..]);
            }
        }
    }

    /// A luma edge down the sixteen `rows`, before column `x`.
    #[inline(always)]
    pub fn vertical_luma(rows: &mut [u8], stride: usize, x: usize, kind: Kind) {
        let mut t = [u8x16::ZERO; 16];
        let mut lines: &[u8] = rows;
        for t in t.iter_mut() {
            *t = load8(&take(&mut lines, stride)[x - 4..]);
        }
        if let Some(pairs) = vertical(&t, kind) {
            let mut lines = rows;
            for pair in pairs {
                store_pair(pair, take_mut(&mut lines, 2 * stride), stride, x - 4);
            }
        }
    }

    /// Chroma edges down the eight rows of each plane, before column `x`.
    #[inline(always)]
    pub fn vertical_chroma(u: &mut [u8], v: &mut [u8], stride: usize, x: usize, kind: Kind) {
        let mut t = [u8x16::ZERO; 16];
        let (tu, tv) = t.split_at_mut(8);
        let (mut lu, mut lv): (&[u8], &[u8]) = (u, v);
        for (tu, tv) in tu.iter_mut().zip(tv) {
            *tu = load8(&take(&mut lu, stride)[x - 4..]);
            *tv = load8(&take(&mut lv, stride)[x - 4..]);
        }
        if let Some(pairs) = vertical(&t, kind) {
            let (pu, pv) = pairs.split_at(4);
            for (pairs, mut lines) in [(pu, u), (pv, v)] {
                for &pair in pairs {
                    store_pair(pair, take_mut(&mut lines, 2 * stride), stride, x - 4);
                }
            }
        }
    }
}

/// Filter the sixteen positions of a luma edge: down the sixteen `rows`
/// (whole rows of the plane) before column `x` when `vertical`, otherwise
/// between the fourth and fifth of the eight `rows` at columns `x` ..
/// `x + 16`.
#[inline]
fn luma_edge(rows: &mut [u8], stride: usize, x: usize, vertical: bool, kind: Kind) {
    #[cfg(feature = "simd")]
    if vertical {
        simd::vertical_luma(rows, stride, x, kind);
    } else {
        simd::horizontal_luma(rows, stride, x, kind);
    }
    #[cfg(not(feature = "simd"))]
    for i in 0..16 {
        if vertical {
            filter_position(rows, i * stride + x, 1, kind);
        } else {
            filter_position(rows, 4 * stride + x + i, stride, kind);
        }
    }
}

/// Filter the eight positions of an edge in each chroma plane: down the
/// eight rows before column `x`, or between the fourth and fifth of eight
/// rows at columns `x` .. `x + 8`.
#[inline]
fn chroma_edge(u: &mut [u8], v: &mut [u8], stride: usize, x: usize, vertical: bool, kind: Kind) {
    #[cfg(feature = "simd")]
    if vertical {
        simd::vertical_chroma(u, v, stride, x, kind);
    } else {
        simd::horizontal_chroma(u, v, stride, x, kind);
    }
    #[cfg(not(feature = "simd"))]
    for plane in [u, v] {
        for i in 0..8 {
            if vertical {
                filter_position(plane, i * stride + x, 1, kind);
            } else {
                filter_position(plane, 4 * stride + x + i, stride, kind);
            }
        }
    }
}

/// Filter macroblock row `mb_y` of `pic` with the normal (`simple` false)
/// or simple filter; `row` holds the row's macroblock parameters. The
/// simple filter leaves chroma alone.
pub fn filter_row(pic: &mut Picture, mb_y: usize, row: &[MbFilter], simple: bool) {
    let stride = pic.width;
    let uv_stride = pic.width / 2;
    // the rows the edges touch: those of the macroblock row for vertical
    // edges, and for a horizontal edge `dy` rows down it the four either
    // side (reaching into the row above for the macroblock edge)
    let (top, uv_top) = (16 * mb_y, 8 * mb_y);
    let luma_rows = top * stride..(top + 16) * stride;
    let chroma_rows = uv_top * uv_stride..(uv_top + 8) * uv_stride;
    let luma_around = |dy: usize| (top + dy - 4) * stride..(top + dy + 4) * stride;
    let chroma_around = |dy: usize| (uv_top + dy - 4) * uv_stride..(uv_top + dy + 4) * uv_stride;
    for (mb_x, f) in row.iter().enumerate() {
        if f.level == 0 {
            continue;
        }
        let level = f.level as i32;
        let interior = f.interior_limit as i32;
        let hev = f.hev_threshold as i32;
        let mb_limit = (level + 2) * 2 + interior;
        let sub_limit = level * 2 + interior;
        let (x, uv_x) = (16 * mb_x, 8 * mb_x);
        let (mb_kind, inner_kind) = if simple {
            (Kind::Simple { edge_limit: mb_limit }, Kind::Simple { edge_limit: sub_limit })
        } else {
            (Kind::Mb { edge_limit: mb_limit, interior, hev }, Kind::Inner { edge_limit: sub_limit, interior, hev })
        };
        if mb_x > 0 {
            luma_edge(&mut pic.y[luma_rows.clone()], stride, x, true, mb_kind);
            if !simple {
                chroma_edge(&mut pic.u[chroma_rows.clone()], &mut pic.v[chroma_rows.clone()], uv_stride, uv_x, true, mb_kind);
            }
        }
        if f.inner {
            for dx in [4, 8, 12] {
                luma_edge(&mut pic.y[luma_rows.clone()], stride, x + dx, true, inner_kind);
            }
            if !simple {
                chroma_edge(&mut pic.u[chroma_rows.clone()], &mut pic.v[chroma_rows.clone()], uv_stride, uv_x + 4, true, inner_kind);
            }
        }
        if mb_y > 0 {
            luma_edge(&mut pic.y[luma_around(0)], stride, x, false, mb_kind);
            if !simple {
                chroma_edge(&mut pic.u[chroma_around(0)], &mut pic.v[chroma_around(0)], uv_stride, uv_x, false, mb_kind);
            }
        }
        if f.inner {
            for dy in [4, 8, 12] {
                luma_edge(&mut pic.y[luma_around(dy)], stride, x, false, inner_kind);
            }
            if !simple {
                chroma_edge(&mut pic.u[chroma_around(4)], &mut pic.v[chroma_around(4)], uv_stride, uv_x, false, inner_kind);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_follow_sharpness_and_level() {
        let f = MbFilter::new(32, 0, true, true);
        assert_eq!((f.level, f.interior_limit, f.hev_threshold), (32, 32, 1));
        let f = MbFilter::new(32, 4, false, true);
        assert_eq!((f.interior_limit, f.hev_threshold), (5, 2));
        let f = MbFilter::new(63, 7, false, true);
        assert_eq!((f.interior_limit, f.hev_threshold), (2, 3));
        let f = MbFilter::new(70, 0, true, false);
        assert_eq!((f.level, f.hev_threshold), (63, 2));
        let f = MbFilter::new(-3, 0, true, false);
        assert_eq!((f.level, f.interior_limit), (0, 1));
    }

    #[test]
    fn a_small_step_is_smoothed() {
        // a vertical edge between columns 3 and 4 of a flat 8x16 block
        let mut buf = vec![0u8; 8 * 16];
        for row in buf.chunks_mut(8) {
            row[..4].fill(100);
            row[4..].fill(106);
        }
        let kind = Kind::Mb { edge_limit: 30, interior: 10, hev: 3 };
        luma_edge(&mut buf, 8, 4, true, kind);
        let row = &buf[..8];
        assert!(row.windows(2).all(|w| w[0] <= w[1]), "{row:?}");
        assert!(row[3] > 100 && row[4] < 106, "{row:?}");
        // an edge beyond the limit is left alone
        let mut hard = vec![0u8; 8 * 16];
        for row in hard.chunks_mut(8) {
            row[4..].fill(200);
        }
        let before = hard.clone();
        luma_edge(&mut hard, 8, 4, true, kind);
        assert_eq!(hard, before);
    }

    /// Every way of filtering an edge gives what `filter_position` gives,
    /// on edges of every character (noise, gentle and sharp steps, each
    /// plane its own).
    #[test]
    fn edges_match_the_per_position_filter() {
        let mut seed = 9u32;
        let mut rand = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed >> 8
        };
        let stride = 24;
        for round in 0..3000 {
            let mut plane = || {
                let spread = [4, 12, 40, 255][rand() as usize % 4];
                let base = rand() % 256;
                let jump = rand() % 60;
                (0..stride * 24)
                    .map(|i| {
                        let side = if (round / 4) % 2 == 0 { (i % stride >= 12) as u32 } else { (i / stride >= 12) as u32 };
                        (base + side * jump + rand() % spread).min(255) as u8
                    })
                    .collect::<Vec<u8>>()
            };
            let (y, u, v) = (plane(), plane(), plane());
            let level = (rand() % 64) as i32;
            let interior = 1 + (rand() % 63) as i32;
            let hev = (rand() % 4) as i32;
            let kinds = [Kind::Mb { edge_limit: (level + 2) * 2 + interior, interior, hev }, Kind::Inner { edge_limit: level * 2 + interior, interior, hev }, Kind::Simple { edge_limit: level * 2 + interior }];
            for kind in kinds {
                // a vertical edge before column 12 down rows 4 .. 20 (8 ..
                // 16 in chroma), a horizontal one before row 12 along
                // columns 4 .. 20 (8 .. 16)
                for vertical in [true, false] {
                    let (rows, x, step, along) = if vertical { (4..20, 12, 1, stride) } else { (8..16, 4, stride, 1) };
                    let at = |rows: std::ops::Range<usize>, x: usize| if vertical { rows.start * stride + x } else { 12 * stride + x };
                    let mut a = y.clone();
                    let mut b = y.clone();
                    luma_edge(&mut a[rows.start * stride..rows.end * stride], stride, x, vertical, kind);
                    for i in 0..16 {
                        filter_position(&mut b, at(rows.clone(), x) + i * along, step, kind);
                    }
                    assert_eq!(a, b, "luma {kind:?} vertical {vertical}");
                    let (rows, x) = (8..16, if vertical { 12 } else { 8 });
                    let (mut au, mut av) = (u.clone(), v.clone());
                    let (mut bu, mut bv) = (u.clone(), v.clone());
                    chroma_edge(&mut au[rows.start * stride..rows.end * stride], &mut av[rows.start * stride..rows.end * stride], stride, x, vertical, kind);
                    for i in 0..8 {
                        filter_position(&mut bu, at(rows.clone(), x) + i * along, step, kind);
                        filter_position(&mut bv, at(rows.clone(), x) + i * along, step, kind);
                    }
                    assert_eq!((au, av), (bu, bv), "chroma {kind:?} vertical {vertical}");
                }
            }
        }
    }
}
