//! Inter prediction samples: fractional sample interpolation (8.5.3.3.3)
//! into 14-bit intermediates, and the default and explicit weighted
//! sample prediction (8.5.3.3.4) that turns them into samples.

use crate::picture::{Plane, Sample};

/// The 8-tap luma filters for the quarter positions (Table 8-8 taps).
const LUMA: [[i32; 8]; 4] = [[0, 0, 0, 64, 0, 0, 0, 0], [-1, 4, -10, 58, 17, -5, 1, 0], [-1, 4, -11, 40, 40, -11, 4, -1], [0, 1, -5, 17, 58, -10, 4, -1]];
/// The 4-tap chroma filters for the eighth positions.
const CHROMA: [[i32; 4]; 8] = [[0, 64, 0, 0], [-2, 58, 10, -2], [-4, 54, 16, -2], [-6, 46, 28, -4], [-4, 36, 36, -4], [-4, 28, 46, -6], [-2, 16, 54, -4], [-2, 10, 58, -2]];

/// Largest prediction block side.
pub const MAX_PB: usize = 64;
/// The source window of the largest block with the 8-tap margins.
const WIN: usize = MAX_PB + 7;

/// Reusable buffers for the interpolation: an edge-replicated source
/// window and the first filter pass of two-dimensional positions.
pub struct Scratch<P> {
    win: Vec<P>,
    tmp: Vec<i16>,
}

impl<P: Sample> Scratch<P> {
    pub fn new() -> Scratch<P> {
        Scratch { win: vec![P::default(); WIN * WIN], tmp: vec![0; WIN * MAX_PB] }
    }
}

impl<P: Sample> Default for Scratch<P> {
    fn default() -> Self {
        Self::new()
    }
}

/// The filter taps of fractional position `f`.
fn taps<const TAPS: usize>(f: usize) -> [i32; TAPS] {
    std::array::from_fn(|k| if TAPS == 8 { LUMA[f][k] } else { CHROMA[f][k] })
}

/// Eight-lane versions of the row kernels (wasm simd128, SSE2 or NEON
/// through `wide`), for the leading multiple of eight samples of a row;
/// each returns how many it did. They compute exactly what the scalar
/// loops compute, in 16-bit lanes throughout.
#[cfg(feature = "simd")]
mod simd {
    use wide::i16x8;

    use crate::picture::Sample;

    /// Eight outputs of a filter pass, Σ `c[k]` · `v(k)` >> `shift`. The
    /// sums of a first pass over samples deeper than 8 bits and of a second
    /// pass over 16-bit intermediates need more than 16 bits, so each input
    /// is split into its part above the shift and its low bits: the two
    /// sums fit 16 bits (the first up to the wrap-around the 16-bit
    /// intermediates have anyway), and their combination is the exact
    /// shifted sum.
    #[inline(always)]
    fn taps8<const TAPS: usize>(v: impl Fn(usize) -> i16x8, c: &[i32; TAPS], shift: u32) -> i16x8 {
        let c: [i16x8; TAPS] = std::array::from_fn(|k| i16x8::splat(c[k] as i16));
        if shift == 0 {
            let mut sum = i16x8::ZERO;
            for (k, &ck) in c.iter().enumerate() {
                sum += v(k) * ck;
            }
            sum
        } else {
            let mask = i16x8::splat((1 << shift) - 1);
            let (mut high, mut low) = (i16x8::ZERO, i16x8::ZERO);
            for (k, &ck) in c.iter().enumerate() {
                let x = v(k);
                high += (x >> shift) * ck;
                low += (x & mask) * ck;
            }
            high + (low >> shift)
        }
    }

    #[inline(always)]
    pub fn filter_row<P: Sample, const TAPS: usize>(r: &[P], c: &[i32; TAPS], shift: u32, out: &mut [i16]) -> usize {
        let mut i = 0;
        while i + 8 <= out.len() {
            // the window of these eight outputs, so that the loads need no checks
            let win = &r[i..i + 7 + TAPS];
            let v = taps8(|k| P::load8(&win[k..k + 8]), c, shift);
            out[i..i + 8].copy_from_slice(v.as_array_ref());
            i += 8;
        }
        i
    }

    #[inline(always)]
    pub fn filter_column<P: Sample, const TAPS: usize>(rows: &[&[P]; TAPS], c: &[i32; TAPS], shift: u32, out: &mut [i16]) -> usize {
        let mut i = 0;
        while i + 8 <= out.len() {
            let v = taps8(|k| P::load8(&rows[k][i..i + 8]), c, shift);
            out[i..i + 8].copy_from_slice(v.as_array_ref());
            i += 8;
        }
        i
    }

    #[inline(always)]
    pub fn filter_column_i16<const TAPS: usize>(rows: &[&[i16]; TAPS], c: &[i32; TAPS], out: &mut [i16]) -> usize {
        let mut i = 0;
        while i + 8 <= out.len() {
            let v = taps8(|k| i16x8::from_slice_unaligned(&rows[k][i..i + 8]), c, 6);
            out[i..i + 8].copy_from_slice(v.as_array_ref());
            i += 8;
        }
        i
    }

    #[inline(always)]
    pub fn full_sample_row<P: Sample>(r: &[P], shift: u32, out: &mut [i16]) -> usize {
        let mut i = 0;
        while i + 8 <= out.len() {
            out[i..i + 8].copy_from_slice((P::load8(&r[i..i + 8]) << shift).as_array_ref());
            i += 8;
        }
        i
    }

    /// (s + offset) >> shift into `d`, clipped. Saturating the addition is
    /// exact: whatever saturates is clipped to the maximum anyway.
    #[inline(always)]
    pub fn uni_row<P: Sample>(s: &[i16], offset: i32, shift: u32, max: i32, d: &mut [P]) -> usize {
        let mut i = 0;
        while i + 8 <= d.len() {
            let v = i16x8::from_slice_unaligned(&s[i..i + 8]).saturating_add(i16x8::splat(offset as i16)) >> shift;
            P::store8(v, max as i16, &mut d[i..i + 8]);
            i += 8;
        }
        i
    }

    /// (a + b + offset) >> shift into `d`, clipped (saturating as above).
    #[inline(always)]
    pub fn bi_row<P: Sample>(a: &[i16], b: &[i16], offset: i32, shift: u32, max: i32, d: &mut [P]) -> usize {
        let mut i = 0;
        while i + 8 <= d.len() {
            let (p, q) = (i16x8::from_slice_unaligned(&a[i..i + 8]), i16x8::from_slice_unaligned(&b[i..i + 8]));
            let v = p.saturating_add(q).saturating_add(i16x8::splat(offset as i16)) >> shift;
            P::store8(v, max as i16, &mut d[i..i + 8]);
            i += 8;
        }
        i
    }

    /// Explicit weighting, ((s · w + 2^(sh − 1)) >> sh) + o clipped, from
    /// the 16-bit halves of the products: the high half scaled up plus the
    /// low half's rounded share (((lo >> (sh − 1)) + 1) >> 1 is the rounded
    /// lo >> sh). The high half is first held to where the result is out
    /// of range anyway, which keeps everything in 16 bits and the
    /// saturating additions exact. `sh` is at least 2 (log2WD includes
    /// 14 − bitDepth).
    #[inline(always)]
    pub fn weighted_row<P: Sample>(s: &[i16], w: i32, o: i32, sh: u32, max: i32, d: &mut [P]) -> usize {
        let wv = i16x8::splat(w as i16);
        let mask = i16x8::splat(((1 << (17 - sh)) - 1) as i16);
        let limit = 1i16 << (sh - 1);
        let (low, high) = (i16x8::splat(-limit), i16x8::splat(limit - 1));
        let (offset, one) = (i16x8::splat(o as i16), i16x8::splat(1));
        let mut i = 0;
        while i + 8 <= d.len() {
            let p = i16x8::from_slice_unaligned(&s[i..i + 8]);
            let hi = i16x8::mul_keep_high(p, wv).max(low).min(high);
            let x = ((p * wv) >> (sh - 1)) & mask;
            let v = (hi << (16 - sh)).saturating_add((x >> 1) + (x & one)).saturating_add(offset);
            P::store8(v, max as i16, &mut d[i..i + 8]);
            i += 8;
        }
        i
    }
}

/// One row of a horizontal filter pass over samples: `out[i]` =
/// Σ `c[k]` · `r[i + k]` >> `shift`.
#[inline(always)]
fn filter_row<P: Sample, const TAPS: usize>(r: &[P], c: &[i32; TAPS], shift: u32, out: &mut [i16]) {
    let r = &r[..out.len() + TAPS - 1];
    #[cfg(feature = "simd")]
    let done = simd::filter_row(r, c, shift, out);
    #[cfg(not(feature = "simd"))]
    let done = 0;
    for (i, o) in out.iter_mut().enumerate().skip(done) {
        let s: i32 = c.iter().zip(&r[i..i + TAPS]).map(|(&ck, v)| ck * v.get()).sum();
        *o = (s >> shift) as i16;
    }
}

/// One row of a vertical filter pass over samples, from the `TAPS` rows
/// around it.
#[inline(always)]
fn filter_column<P: Sample, const TAPS: usize>(rows: [&[P]; TAPS], c: &[i32; TAPS], shift: u32, out: &mut [i16]) {
    let rows = rows.map(|r| &r[..out.len()]);
    #[cfg(feature = "simd")]
    let done = simd::filter_column(&rows, c, shift, out);
    #[cfg(not(feature = "simd"))]
    let done = 0;
    for (i, o) in out.iter_mut().enumerate().skip(done) {
        let s: i32 = c.iter().zip(&rows).map(|(&ck, row)| ck * row[i].get()).sum();
        *o = (s >> shift) as i16;
    }
}

/// One row of the vertical second pass over the first pass's 16-bit
/// intermediates (8-230, 8-243): shifted by 6, wrapping to 16 bits.
#[inline(always)]
fn filter_column_i16<const TAPS: usize>(rows: [&[i16]; TAPS], c: &[i32; TAPS], out: &mut [i16]) {
    let rows = rows.map(|r| &r[..out.len()]);
    #[cfg(feature = "simd")]
    let done = simd::filter_column_i16(&rows, c, out);
    #[cfg(not(feature = "simd"))]
    let done = 0;
    for (i, o) in out.iter_mut().enumerate().skip(done) {
        let s: i32 = c.iter().zip(&rows).map(|(&ck, row)| ck * row[i] as i32).sum();
        *o = (s >> 6) as i16;
    }
}

/// Interpolate a `w`×`h` block of `plane` whose top-left full-sample
/// position is (`xi`, `yi`) at fractional offset (`xf`, `yf`) (in
/// 1/4 samples for luma, `TAPS` = 8, or 1/8 for chroma, `TAPS` = 4), into
/// `dst` (stride `w`) at 14-bit precision. Samples outside the plane
/// repeat the nearest edge sample (8-222, 8-239).
#[allow(clippy::too_many_arguments)]
fn interpolate<P: Sample, const TAPS: usize>(plane: &Plane<P>, xi: i32, yi: i32, xf: usize, yf: usize, w: usize, h: usize, bit_depth: u32, dst: &mut [i16], scratch: &mut Scratch<P>) {
    let before = TAPS / 2 - 1;
    let (pw, ph) = (plane.width as i32, plane.height as i32);
    let (x0, y0) = (xi - before as i32, yi - before as i32);
    let (ww, wh) = (w + TAPS - 1, h + TAPS - 1);
    // the source rows: straight from the plane when the window is inside,
    // else an edge-replicated copy
    let Scratch { win, tmp } = scratch;
    let (src, base, ss): (&[P], usize, usize) = if x0 >= 0 && y0 >= 0 && x0 + ww as i32 <= pw && y0 + wh as i32 <= ph {
        (&plane.data, y0 as usize * plane.stride + x0 as usize, plane.stride)
    } else {
        // the window columns left of, inside and right of the picture
        let inside_from = (-x0).clamp(0, ww as i32) as usize;
        let inside_to = (pw - x0).clamp(inside_from as i32, ww as i32) as usize;
        for (j, out) in win.chunks_exact_mut(ww).take(wh).enumerate() {
            let sy = (y0 + j as i32).clamp(0, ph - 1) as usize;
            let row = &plane.data[sy * plane.stride..][..plane.width];
            out[..inside_from].fill(row[0]);
            if inside_to > inside_from {
                let first = (x0 + inside_from as i32) as usize;
                out[inside_from..inside_to].copy_from_slice(&row[first..first + inside_to - inside_from]);
            }
            out[inside_to..].fill(row[plane.width - 1]);
        }
        (&win[..], 0, ww)
    };
    let row = |j: usize| &src[base + j * ss..][..ww];
    let dst = &mut dst[..w * h];
    let shift1 = bit_depth - 8;
    match (xf, yf) {
        (0, 0) => {
            let shift3 = 14 - bit_depth;
            for (j, out) in dst.chunks_exact_mut(w).enumerate() {
                let r = &row(j + before)[before..];
                #[cfg(feature = "simd")]
                let done = simd::full_sample_row(r, shift3, out);
                #[cfg(not(feature = "simd"))]
                let done = 0;
                for (o, s) in out[done..].iter_mut().zip(&r[done..]) {
                    *o = (s.get() << shift3) as i16;
                }
            }
        }
        (_, 0) => {
            let c = taps::<TAPS>(xf);
            for (j, out) in dst.chunks_exact_mut(w).enumerate() {
                filter_row(row(j + before), &c, shift1, out);
            }
        }
        (0, _) => {
            let c = taps::<TAPS>(yf);
            for (j, out) in dst.chunks_exact_mut(w).enumerate() {
                filter_column(std::array::from_fn(|k| &row(j + k)[before..]), &c, shift1, out);
            }
        }
        _ => {
            let (ch, cv) = (taps::<TAPS>(xf), taps::<TAPS>(yf));
            let tmp = &mut tmp[..w * wh];
            for (j, out) in tmp.chunks_exact_mut(w).enumerate() {
                filter_row(row(j), &ch, shift1, out);
            }
            let tmp = &*tmp;
            for (j, out) in dst.chunks_exact_mut(w).enumerate() {
                filter_column_i16(std::array::from_fn(|k| &tmp[(j + k) * w..][..w]), &cv, out);
            }
        }
    }
}

/// 8.5.3.3.3.2: the luma prediction of a `w`×`h` block at (`x`, `y`)
/// displaced by `mv` (quarter samples).
#[allow(clippy::too_many_arguments)]
pub fn luma<P: Sample>(plane: &Plane<P>, x: i32, y: i32, mv: [i16; 2], w: usize, h: usize, bit_depth: u32, dst: &mut [i16], scratch: &mut Scratch<P>) {
    let (mx, my) = (mv[0] as i32, mv[1] as i32);
    interpolate::<P, 8>(plane, x + (mx >> 2), y + (my >> 2), (mx & 3) as usize, (my & 3) as usize, w, h, bit_depth, dst, scratch);
}

/// 8.5.3.3.3.3: the chroma prediction (4:2:0) of a `w`×`h` block at chroma
/// position (`x`, `y`) for the luma vector `mv` (eighth chroma samples).
#[allow(clippy::too_many_arguments)]
pub fn chroma<P: Sample>(plane: &Plane<P>, x: i32, y: i32, mv: [i16; 2], w: usize, h: usize, bit_depth: u32, dst: &mut [i16], scratch: &mut Scratch<P>) {
    let (mx, my) = (mv[0] as i32, mv[1] as i32);
    interpolate::<P, 4>(plane, x + (mx >> 3), y + (my >> 3), (mx & 7) as usize, (my & 7) as usize, w, h, bit_depth, dst, scratch);
}

/// Explicit weighting parameters of one component: log2WD, and the
/// weight and offset per list.
#[derive(Clone, Copy, Debug)]
pub struct Weights {
    pub log2wd: u32,
    pub w: [i32; 2],
    pub o: [i32; 2],
}

/// 8.5.3.3.4.2: the default single-list prediction into `dst`.
pub fn put_uni<P: Sample>(src: &[i16], w: usize, h: usize, bit_depth: u32, dst: &mut [P], ds: usize) {
    let shift = 14 - bit_depth;
    let offset = 1 << (shift - 1);
    let max = (1 << bit_depth) - 1;
    for (j, s) in src[..w * h].chunks_exact(w).enumerate() {
        let d = &mut dst[j * ds..j * ds + w];
        #[cfg(feature = "simd")]
        let done = simd::uni_row(s, offset, shift, max, d);
        #[cfg(not(feature = "simd"))]
        let done = 0;
        for (d, &s) in d[done..].iter_mut().zip(&s[done..]) {
            *d = P::new(((s as i32 + offset) >> shift).clamp(0, max));
        }
    }
}

/// 8.5.3.3.4.2: the default bi-prediction average.
pub fn put_bi<P: Sample>(a: &[i16], b: &[i16], w: usize, h: usize, bit_depth: u32, dst: &mut [P], ds: usize) {
    let shift = 15 - bit_depth;
    let offset = 1 << (shift - 1);
    let max = (1 << bit_depth) - 1;
    for (j, (ra, rb)) in a[..w * h].chunks_exact(w).zip(b[..w * h].chunks_exact(w)).enumerate() {
        let d = &mut dst[j * ds..j * ds + w];
        #[cfg(feature = "simd")]
        let done = simd::bi_row(ra, rb, offset, shift, max, d);
        #[cfg(not(feature = "simd"))]
        let done = 0;
        for ((d, &p), &q) in d[done..].iter_mut().zip(&ra[done..]).zip(&rb[done..]) {
            *d = P::new(((p as i32 + q as i32 + offset) >> shift).clamp(0, max));
        }
    }
}

/// 8.5.3.3.4.3: explicit weighting of a single list `l`.
#[allow(clippy::too_many_arguments)]
pub fn put_weighted_uni<P: Sample>(src: &[i16], w: usize, h: usize, bit_depth: u32, wt: &Weights, l: usize, dst: &mut [P], ds: usize) {
    let max = (1 << bit_depth) - 1;
    let (w0, o0, sh) = (wt.w[l], wt.o[l], wt.log2wd);
    let round = if sh >= 1 { 1 << (sh - 1) } else { 0 };
    for (j, s) in src[..w * h].chunks_exact(w).enumerate() {
        let d = &mut dst[j * ds..j * ds + w];
        #[cfg(feature = "simd")]
        let done = simd::weighted_row(s, w0, o0, sh, max, d);
        #[cfg(not(feature = "simd"))]
        let done = 0;
        for (d, &s) in d[done..].iter_mut().zip(&s[done..]) {
            *d = P::new((((s as i32 * w0 + round) >> sh) + o0).clamp(0, max));
        }
    }
}

/// 8.5.3.3.4.3: explicit weighted bi-prediction.
#[allow(clippy::too_many_arguments)]
pub fn put_weighted_bi<P: Sample>(a: &[i16], b: &[i16], w: usize, h: usize, bit_depth: u32, wt: &Weights, dst: &mut [P], ds: usize) {
    let max = (1 << bit_depth) - 1;
    let sh = wt.log2wd;
    let add = (wt.o[0] + wt.o[1] + 1) << sh;
    for j in 0..h {
        let (ra, rb) = (&a[j * w..j * w + w], &b[j * w..j * w + w]);
        for ((d, &p), &q) in dst[j * ds..j * ds + w].iter_mut().zip(ra).zip(rb) {
            *d = P::new(((p as i32 * wt.w[0] + q as i32 * wt.w[1] + add) >> (sh + 1)).clamp(0, max));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: &mut u32) -> Vec<u8> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (*seed >> 24) as u8
            })
            .collect()
    }

    /// The spec's per-sample formulation of the luma interpolation.
    fn reference(p: &Plane<u8>, x: i32, y: i32, mv: [i16; 2]) -> i32 {
        let at = |xx: i32, yy: i32| p.data[yy.clamp(0, p.height as i32 - 1) as usize * p.stride + xx.clamp(0, p.width as i32 - 1) as usize] as i32;
        let (xi, yi) = (x + (mv[0] as i32 >> 2), y + (mv[1] as i32 >> 2));
        let (xf, yf) = ((mv[0] & 3) as usize, (mv[1] & 3) as usize);
        let h = |yy: i32| -> i32 { (0..8).map(|k| LUMA[xf][k] * at(xi + k as i32 - 3, yy)).sum::<i32>() };
        match (xf, yf) {
            (0, 0) => at(xi, yi) << 6,
            (_, 0) => h(yi),
            (0, _) => (0..8).map(|k| LUMA[yf][k] * at(xi, yi + k as i32 - 3)).sum::<i32>(),
            _ => (0..8).map(|k| LUMA[yf][k] * h(yi + k as i32 - 3)).sum::<i32>() >> 6,
        }
    }

    #[test]
    fn luma_interpolation_matches_the_spec() {
        let mut seed = 5;
        let plane = Plane { data: noise(40 * 30, &mut seed), stride: 40, width: 40, height: 30 };
        let mut dst = vec![0i16; 16 * 16];
        let mut scratch = Scratch::new();
        for &(x, y) in &[(8, 8), (0, 0), (-5, 3), (36, 26), (20, -9), (-100, 5), (140, -60), (38, 29)] {
            for mvx in -9..10 {
                for mvy in -9..10 {
                    luma(&plane, x, y, [mvx, mvy], 8, 4, 8, &mut dst, &mut scratch);
                    for j in 0..4 {
                        for i in 0..8 {
                            assert_eq!(dst[j * 8 + i] as i32, reference(&plane, x + i as i32, y + j as i32, [mvx, mvy]), "({x},{y}) mv ({mvx},{mvy})");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn explicit_weighting_matches_the_formula() {
        let mut seed = 3u32;
        let mut rand = |n: i32| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as i32 % n
        };
        for bit_depth in [8, 10, 12] {
            let max = (1 << bit_depth) - 1;
            for denom in 0..8 {
                let log2wd = denom + 14 - bit_depth;
                for _ in 0..50 {
                    let w0 = (1 << denom) + rand(256) - 128;
                    let o0 = (rand(256) - 128) << (bit_depth - 8);
                    // intermediates across the whole 16-bit range, extremes included
                    let src: Vec<i16> = (0..16).map(|k| if k < 2 { [i16::MIN, i16::MAX][k] } else { (rand(65536) - 32768) as i16 }).collect();
                    let wt = Weights { log2wd, w: [w0, w0], o: [o0, o0] };
                    let mut d = [0u16; 16];
                    put_weighted_uni(&src, 16, 1, bit_depth, &wt, 0, &mut d, 16);
                    let mut d8 = [0u8; 16];
                    if bit_depth == 8 {
                        put_weighted_uni(&src, 16, 1, bit_depth, &wt, 0, &mut d8, 16);
                    }
                    for (k, (&s, &v)) in src.iter().zip(&d).enumerate() {
                        let want = (((s as i32 * w0 + (1 << (log2wd - 1))) >> log2wd) + o0).clamp(0, max);
                        assert_eq!(v as i32, want, "bit depth {bit_depth} denom {denom} w {w0} o {o0} sample {k} ({s})");
                        if bit_depth == 8 {
                            assert_eq!(d8[k] as i32, want, "8-bit samples, denom {denom} w {w0} o {o0} sample {k} ({s})");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn default_weighting() {
        let a = [64i16 * 100; 4];
        let b = [64i16 * 50; 4];
        let mut d = [0u8; 4];
        put_uni(&a, 4, 1, 8, &mut d, 4);
        assert_eq!(d, [100; 4]);
        put_bi(&a, &b, 4, 1, 8, &mut d, 4);
        assert_eq!(d, [75; 4]);
    }
}
