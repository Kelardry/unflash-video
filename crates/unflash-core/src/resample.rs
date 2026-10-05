//! Area-average downsampling of 8-bit pictures to the analysis size
//! ([`Shrink`]): the boxes the GPU ingest pass averages (fractional overlap
//! weights in code space, rounded back to 8 bits), summed exactly, for
//! every picture made small off the GPU; and the blur that softens stripes.

/// One axis of an area-average: for each output sample, the first source
/// sample its box covers and the integer weights of the samples it covers
/// (overlaps in units of `gcd(src, out) / out` source samples, so every box
/// weighs `src / gcd` in all).
#[derive(Clone, Debug)]
struct Axis {
    boxes: Vec<(usize, Vec<u32>)>,
    total: u32,
}

impl Axis {
    fn new(src: usize, out: usize) -> Axis {
        fn gcd(a: usize, b: usize) -> usize {
            if b == 0 { a } else { gcd(b, a % b) }
        }
        let g = gcd(src, out).max(1);
        // box k is [k·src, (k+1)·src) and sample i is [i·out, (i+1)·out), in 1/out units
        let boxes = (0..out)
            .map(|k| {
                let (lo, hi) = (k * src, (k + 1) * src);
                let first = lo / out;
                let last = (hi - 1) / out;
                let w = (first..=last).map(|i| ((hi.min((i + 1) * out) - lo.max(i * out)) / g) as u32).collect();
                (first, w)
            })
            .collect();
        Axis { boxes, total: (src / g) as u32 }
    }
}

/// Area-average downsampling to the analysis size in exact integer
/// arithmetic, for pictures made small before they reach the detector (in
/// the browser's decode workers, so the page never handles the full-size
/// picture, and in the built-in decoders) and for the CPU detector's own:
/// the boxes of the GPU ingest pass, whose float sums can land either side
/// of an exact half, so a code can differ by one there; here the exact
/// average is rounded half to even, as WGSL rounds. Rows are summed first
/// (a straight run over each row, which the compiler vectorises), then
/// columns, on one output row's sums at a time.
///
/// A 4:2:0 picture is converted a row at a time into three planes (R, G, B)
/// with each chroma sample's terms worked out once per chroma row, so that
/// the conversion is one straight loop over the row that the compiler
/// vectorises, and summed plane by plane. Averages are divided by a
/// multiply and a shift ([`Divider`]) rather than a division per value.
#[derive(Clone, Debug)]
pub struct Shrink {
    sw: usize,
    sh: usize,
    aw: usize,
    ah: usize,
    cols: Axis,
    rows: Axis,
    acc: Vec<u32>,
    acc16: Vec<u16>,
    /// R, G and B of the row converted last, one plane after another.
    planes: Vec<u8>,
    /// The chroma terms (R, G, B) of the chroma row used last, one per luma column.
    terms: Vec<i32>,
}

impl Shrink {
    /// For pictures of `sw`×`sh` to `aw`×`ah`. A picture of no width or
    /// height has nothing to average (its boxes would be endless), so
    /// callers turn one away before it gets here.
    pub fn new(sw: u32, sh: u32, aw: u32, ah: u32) -> Shrink {
        assert!(sw > 0 && sh > 0, "a {sw}×{sh} picture has nothing to shrink");
        let (sw, sh, aw, ah) = (sw as usize, sh as usize, aw.max(1) as usize, ah.max(1) as usize);
        Shrink { sw, sh, aw, ah, cols: Axis::new(sw, aw), rows: Axis::new(sh, ah), acc: Vec::new(), acc16: Vec::new(), planes: Vec::new(), terms: Vec::new() }
    }

    /// The size of the pictures it takes.
    pub fn source_size(&self) -> (usize, usize) {
        (self.sw, self.sh)
    }

    /// The same sizes as this one was made for.
    pub fn fits(&self, sw: u32, sh: u32, aw: u32, ah: u32) -> bool {
        (self.sw, self.sh, self.aw, self.ah) == (sw as usize, sh as usize, aw as usize, ah as usize)
    }

    /// The shrink in `slot` if it was made for these sizes, else a new one
    /// put there (a source's pictures change size seldom, if ever).
    pub fn reuse(slot: &mut Option<Shrink>, sw: u32, sh: u32, aw: u32, ah: u32) -> &mut Shrink {
        if !slot.as_ref().is_some_and(|k| k.fits(sw, sh, aw, ah)) {
            *slot = Some(Shrink::new(sw, sh, aw, ah));
        }
        slot.as_mut().unwrap()
    }

    /// A picture of four bytes a pixel (R, G, B and one ignored, or B, G, R
    /// and one ignored with `bgr`), its first row at `offset` and each
    /// `stride` bytes after the one before, to RGBA8 at the analysis size.
    pub fn packed(&mut self, src: &[u8], offset: usize, stride: usize, bgr: bool, out: &mut Vec<u8>) {
        let len = self.sw * 4;
        assert!(stride >= len && src.len() >= offset + (self.sh - 1) * stride + len, "picture data too short for its size");
        self.shrink_rows(&mut Packed { src, offset, stride, len }, bgr, out);
    }

    /// A 4:2:0 picture: each row converted to RGB in 16.16 fixed point (the
    /// GPU converts with the same coefficients in f32; the two agree but for
    /// rounding at exact halves), then summed.
    pub fn yuv420(&mut self, data: &[u8], l: &crate::yuv::YuvLayout, out: &mut Vec<u8>) {
        assert!(l.fits(data.len(), self.sw, self.sh), "picture data too short for its layout");
        let planes = Planes { y: &data[l.y_off..], y_stride: l.y_stride, u: &data[l.u_off..], u_stride: l.u_stride, v: &data[l.v_off.min(data.len())..], v_stride: l.v_stride, nv12: l.nv12 };
        self.shrink_planes(planes, l.coefficients(), out);
    }

    /// A 4:2:0 picture held as three planes (a decoder's own picture, from
    /// the first visible sample of each plane), made small the same way.
    #[allow(clippy::too_many_arguments)]
    pub fn yuv420_planes(&mut self, y: &[u8], y_stride: usize, u: &[u8], u_stride: usize, v: &[u8], v_stride: usize, bt709: bool, full_range: bool, out: &mut Vec<u8>) {
        let (w, h, cw, ch) = (self.sw, self.sh, self.sw.div_ceil(2), self.sh.div_ceil(2));
        assert!(y_stride >= w && y.len() >= (h - 1) * y_stride + w, "luma plane too short");
        assert!(u_stride >= cw && u.len() >= (ch - 1) * u_stride + cw && v_stride >= cw && v.len() >= (ch - 1) * v_stride + cw, "chroma planes too short");
        let coef = crate::yuv::YuvLayout::packed_i420(w, h, bt709, full_range).coefficients();
        self.shrink_planes(Planes { y, y_stride, u, u_stride, v, v_stride, nv12: false }, coef, out);
    }

    fn shrink_planes(&mut self, p: Planes, coef: [i32; 6], out: &mut Vec<u8>) {
        let (sw, aw, ah) = (self.sw, self.aw, self.ah);
        out.clear();
        out.resize(aw * ah * 4, 255);
        let [ky, kr, kgu, kgv, kb, yoff] = coef;
        let cw = sw.div_ceil(2);
        let mut rgb = std::mem::take(&mut self.planes);
        rgb.resize(3 * sw, 0);
        let mut terms = std::mem::take(&mut self.terms);
        terms.resize(3 * cw, 0);
        let (mut last_row, mut last_crow) = (usize::MAX, usize::MAX);
        // row `y` as R, G, B planes, each pixel converted as `convert_row` says
        let mut convert = |y: usize, rgb: &mut [u8], terms: &mut [i32]| {
            if y == last_row {
                return;
            }
            last_row = y;
            let cy = y / 2;
            if cy != last_crow {
                // the chroma terms, once per chroma sample for the two rows that share them
                last_crow = cy;
                let (tr, rest) = terms.split_at_mut(cw);
                let (tg, tb) = rest.split_at_mut(cw);
                if p.nv12 {
                    let crow = &p.u[cy * p.u_stride..][..2 * cw];
                    for (cx, uv) in crow.chunks_exact(2).enumerate() {
                        let (u, v) = (uv[0] as i32 - 128, uv[1] as i32 - 128);
                        tr[cx] = kr * v + 32768;
                        tg[cx] = -kgu * u - kgv * v + 32768;
                        tb[cx] = kb * u + 32768;
                    }
                } else {
                    let (urow, vrow) = (&p.u[cy * p.u_stride..][..cw], &p.v[cy * p.v_stride..][..cw]);
                    for cx in 0..cw {
                        let (u, v) = (urow[cx] as i32 - 128, vrow[cx] as i32 - 128);
                        tr[cx] = kr * v + 32768;
                        tg[cx] = -kgu * u - kgv * v + 32768;
                        tb[cx] = kb * u + 32768;
                    }
                }
            }
            let (r, rest) = rgb.split_at_mut(sw);
            let (g, b) = rest.split_at_mut(sw);
            convert_row(&p.y[y * p.y_stride..][..sw], terms, cw, ky, yoff, [r, g, b]);
        };
        let total = self.cols.total as u64 * self.rows.total as u64;
        let div = Divider::new(total, 255 * total);
        let narrow = self.rows.total as usize * 255 <= u16::MAX as usize;
        macro_rules! shrink_with {
            ($acc:expr, $t:ty) => {{
                let acc = &mut $acc[..3 * sw];
                for (y, (first, wy)) in self.rows.boxes.iter().enumerate() {
                    acc.fill(0);
                    for (k, &w) in wy.iter().enumerate() {
                        convert(first + k, &mut rgb, &mut terms);
                        let w = w as $t;
                        for (a, &v) in acc.iter_mut().zip(rgb.iter()) {
                            *a += w * v as $t;
                        }
                    }
                    let orow = &mut out[y * aw * 4..(y + 1) * aw * 4];
                    for (c, plane) in acc.chunks_exact(sw).enumerate() {
                        for (x, (first, wx)) in self.cols.boxes.iter().enumerate() {
                            let sum: u64 = wx.iter().zip(&plane[*first..]).map(|(&w, &a)| w as u64 * a as u64).sum();
                            orow[x * 4 + c] = div.round(sum);
                        }
                    }
                }
            }};
        }
        if narrow {
            let mut acc = std::mem::take(&mut self.acc16);
            acc.resize(3 * sw, 0);
            shrink_with!(acc, u16);
            self.acc16 = acc;
        } else {
            let mut acc = std::mem::take(&mut self.acc);
            acc.resize(3 * sw, 0);
            shrink_with!(acc, u32);
            self.acc = acc;
        }
        self.planes = rgb;
        self.terms = terms;
    }

    fn shrink_rows<R: Rows>(&mut self, rows: &mut R, bgr: bool, out: &mut Vec<u8>) {
        let (sw, aw, ah) = (self.sw, self.aw, self.ah);
        out.clear();
        out.resize(aw * ah * 4, 255);
        let total = self.cols.total as u64 * self.rows.total as u64;
        let div = Divider::new(total, 255 * total);
        let (ri, bi) = if bgr { (2, 0) } else { (0, 2) };
        // a row box's sums in 16 bits when they fit (twice the lanes)
        let narrow = self.rows.total as usize * 255 <= u16::MAX as usize;
        // a pixel's whole sum in 32 bits unless the boxes are huge
        let wide = total * 255 > u32::MAX as u64;
        macro_rules! shrink_with {
            ($acc:expr, $t:ty) => {{
                let acc = &mut $acc[..sw * 4];
                for (y, (first, wy)) in self.rows.boxes.iter().enumerate() {
                    acc.fill(0);
                    for (k, &w) in wy.iter().enumerate() {
                        let row = rows.row(first + k);
                        let w = w as $t;
                        for (a, &p) in acc.iter_mut().zip(row) {
                            *a += w * p as $t;
                        }
                    }
                    let orow = &mut out[y * aw * 4..(y + 1) * aw * 4];
                    for (x, (first, wx)) in self.cols.boxes.iter().enumerate() {
                        let o = &mut orow[x * 4..x * 4 + 3];
                        if wide {
                            let mut sum = [0u64; 3];
                            for (k, &w) in wx.iter().enumerate() {
                                let p = &acc[(first + k) * 4..][..3];
                                sum[0] += w as u64 * p[0] as u64;
                                sum[1] += w as u64 * p[1] as u64;
                                sum[2] += w as u64 * p[2] as u64;
                            }
                            o[0] = div.round(sum[ri]);
                            o[1] = div.round(sum[1]);
                            o[2] = div.round(sum[bi]);
                        } else {
                            let mut sum = [0u32; 3];
                            for (k, &w) in wx.iter().enumerate() {
                                let p = &acc[(first + k) * 4..][..3];
                                sum[0] += w * p[0] as u32;
                                sum[1] += w * p[1] as u32;
                                sum[2] += w * p[2] as u32;
                            }
                            o[0] = div.round(sum[ri] as u64);
                            o[1] = div.round(sum[1] as u64);
                            o[2] = div.round(sum[bi] as u64);
                        }
                    }
                }
            }};
        }
        if narrow {
            let mut acc = std::mem::take(&mut self.acc16);
            acc.resize(sw * 4, 0);
            shrink_with!(acc, u16);
            self.acc16 = acc;
        } else {
            let mut acc = std::mem::take(&mut self.acc);
            acc.resize(sw * 4, 0);
            shrink_with!(acc, u32);
            self.acc = acc;
        }
    }
}

/// Where [`Shrink`] reads a picture's rows, four bytes a pixel.
trait Rows {
    fn row(&mut self, y: usize) -> &[u8];
}

struct Packed<'a> {
    src: &'a [u8],
    offset: usize,
    stride: usize,
    len: usize,
}

impl Rows for Packed<'_> {
    fn row(&mut self, y: usize) -> &[u8] {
        &self.src[self.offset + y * self.stride..][..self.len]
    }
}

/// The planes of a 4:2:0 picture, each from its first visible sample (with
/// `nv12`, `u` holds the interleaved chroma and `v` is unused).
struct Planes<'a> {
    y: &'a [u8],
    y_stride: usize,
    u: &'a [u8],
    u_stride: usize,
    v: &'a [u8],
    v_stride: usize,
    nv12: bool,
}

/// A row of luma samples and its chroma terms (`t`: R, G and B, `cw`
/// apart, one per chroma sample, each with the half that rounds the sum to
/// nearest) converted to R, G and B planes, each sample `(yy + term) >> 16`
/// clamped to 0..=255.
fn convert_row(yrow: &[u8], t: &[i32], cw: usize, ky: i32, yoff: i32, out: [&mut [u8]; 3]) {
    let w = yrow.len();
    assert!(t.len() >= 3 * cw && cw >= w.div_ceil(2) && out.iter().all(|o| o.len() >= w));
    #[allow(unused_mut)]
    let mut out = out;
    // SAFETY: the lengths were checked above; simd128 is enabled for the build
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    let x0 = unsafe { convert_row_simd128(yrow, t, cw, ky, yoff, &mut out) };
    #[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
    let x0 = 0;
    let [r, g, b] = out;
    for x in x0..w {
        let yy = (yrow[x] as i32 - yoff) * ky;
        let c = x >> 1;
        r[x] = ((yy + t[c]) >> 16).clamp(0, 255) as u8;
        g[x] = ((yy + t[cw + c]) >> 16).clamp(0, 255) as u8;
        b[x] = ((yy + t[2 * cw + c]) >> 16).clamp(0, 255) as u8;
    }
}

/// `convert_row` sixteen samples at a time: each chroma term is loaded once
/// and spread over its two samples by a shuffle, and the clamp is the
/// saturation of the two narrowing steps (i32 to i16 to u8), which gives
/// exactly 0..=255. Returns how many samples it converted (a multiple of 16).
#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
#[target_feature(enable = "simd128")]
unsafe fn convert_row_simd128(yrow: &[u8], t: &[i32], cw: usize, ky: i32, yoff: i32, out: &mut [&mut [u8]; 3]) -> usize {
    use core::arch::wasm32::*;
    let n = yrow.len() / 16 * 16;
    let (kyv, yoffv) = (i32x4_splat(ky), i32x4_splat(yoff));
    let mut x = 0;
    while x < n {
        let yv = v128_load(yrow.as_ptr().add(x) as *const v128);
        let (lo, hi) = (u16x8_extend_low_u8x16(yv), u16x8_extend_high_u8x16(yv));
        let yy = [u32x4_extend_low_u16x8(lo), u32x4_extend_high_u16x8(lo), u32x4_extend_low_u16x8(hi), u32x4_extend_high_u16x8(hi)].map(|v| i32x4_mul(i32x4_sub(v, yoffv), kyv));
        // chroma samples x / 2 .. x / 2 + 8 (within the first cw: x + 16 <= n <= w)
        let c = x / 2;
        for (k, plane) in out.iter_mut().enumerate() {
            let tp = t.as_ptr().add(k * cw + c);
            let (t0, t1) = (v128_load(tp as *const v128), v128_load(tp.add(4) as *const v128));
            let v = [i32x4_shuffle::<0, 0, 1, 1>(t0, t0), i32x4_shuffle::<2, 2, 3, 3>(t0, t0), i32x4_shuffle::<0, 0, 1, 1>(t1, t1), i32x4_shuffle::<2, 2, 3, 3>(t1, t1)];
            let s = [0, 1, 2, 3].map(|i| i32x4_shr(i32x4_add(yy[i], v[i]), 16));
            let px = u8x16_narrow_i16x8(i16x8_narrow_i32x4(s[0], s[1]), i16x8_narrow_i32x4(s[2], s[3]));
            v128_store(plane.as_mut_ptr().add(x) as *mut v128, px);
        }
        x += 16;
    }
    n
}

/// `n / d` rounded to the nearest integer, halves to even, capped at 255.
fn round_div(n: u64, d: u64) -> u8 {
    let (q, r) = (n / d, n % d);
    let q = if 2 * r > d || (2 * r == d && q % 2 == 1) { q + 1 } else { q };
    q.min(255) as u8
}

/// Division by a fixed `d` of numerators up to `max`, by a multiply and a
/// shift (Granlund and Montgomery): with `s` = ⌈log₂ d⌉ and numerators
/// below 2^`bits`, `m` = ⌊2^(bits+s) / d⌋ + 1 gives ⌊n / d⌋ = (n·m) >>
/// (bits+s) exactly. A division costs tens of cycles, and a shrink divides
/// each of its 100 000 or so output values; where the product would not
/// fit in 64 bits it divides after all.
#[derive(Clone, Copy, Debug)]
struct Divider {
    d: u64,
    m: u64,
    shift: u32,
    exact: bool,
}

impl Divider {
    fn new(d: u64, max: u64) -> Divider {
        let d = d.max(1);
        let bits = 64 - max.max(1).leading_zeros();
        let s = 64 - (d - 1).leading_zeros();
        let m = ((1u128 << (bits + s)) / d as u128 + 1) as u64;
        // n·m must fit: bits + bits(m) <= 64
        let exact = bits + s <= 63 && bits + (64 - m.leading_zeros()) <= 64;
        Divider { d, m, shift: bits + s, exact }
    }

    /// `n / d`, rounded as [`round_div`] rounds.
    #[inline(always)]
    fn round(&self, n: u64) -> u8 {
        if !self.exact {
            return round_div(n, self.d);
        }
        let q = (n * self.m) >> self.shift;
        let r = n - q * self.d;
        let q = if 2 * r > self.d || (2 * r == self.d && q % 2 == 1) { q + 1 } else { q };
        q.min(255) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The float area average the GPU ingest pass computes, the reference
    /// [`Shrink`] is held to: an RGBA8 (or RGB8 with `bpp = 3`) picture of
    /// `sw`×`sh` to `aw`×`ah` RGBA8, alpha 255.
    fn area_downsample(src: &[u8], bpp: usize, sw: u32, sh: u32, aw: u32, ah: u32) -> Vec<u8> {
        let (sw, sh, aw, ah) = (sw as usize, sh as usize, aw as usize, ah as usize);
        assert!(src.len() >= sw * sh * bpp, "source too short");
        let mut out = vec![255u8; aw * ah * 4];
        if sw == aw && sh == ah {
            for i in 0..aw * ah {
                out[i * 4..i * 4 + 3].copy_from_slice(&src[i * bpp..i * bpp + 3]);
            }
            return out;
        }
        let fx = sw as f32 / aw as f32;
        let fy = sh as f32 / ah as f32;
        for y in 0..ah {
            let fy0 = y as f32 * fy;
            let fy1 = (y + 1) as f32 * fy;
            let iy0 = fy0.floor() as usize;
            let iy1 = (fy1.ceil() as usize).min(sh);
            for x in 0..aw {
                let fx0 = x as f32 * fx;
                let fx1 = (x + 1) as f32 * fx;
                let ix0 = fx0.floor() as usize;
                let ix1 = (fx1.ceil() as usize).min(sw);
                let mut acc = [0f32; 3];
                let mut wsum = 0f32;
                for sy in iy0..iy1 {
                    let wy = fy1.min((sy + 1) as f32) - fy0.max(sy as f32);
                    for sx in ix0..ix1 {
                        let wx = fx1.min((sx + 1) as f32) - fx0.max(sx as f32);
                        let w = wx * wy;
                        let p = &src[(sy * sw + sx) * bpp..];
                        acc[0] += p[0] as f32 / 255.0 * w;
                        acc[1] += p[1] as f32 / 255.0 * w;
                        acc[2] += p[2] as f32 / 255.0 * w;
                        wsum += w;
                    }
                }
                let o = (y * aw + x) * 4;
                let inv = 1.0 / wsum.max(1e-9);
                for c in 0..3 {
                    out[o + c] = (acc[c] * inv * 255.0).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        out
    }

    #[test]
    fn identity_and_halving() {
        let src: Vec<u8> = (0..4 * 4 * 3).map(|i| (i * 7 % 256) as u8).collect();
        let same = area_downsample(&src, 3, 4, 4, 4, 4);
        for i in 0..16 {
            assert_eq!(&same[i * 4..i * 4 + 3], &src[i * 3..i * 3 + 3]);
            assert_eq!(same[i * 4 + 3], 255);
        }
        // 2x2 blocks of 0 and 200 average to 100
        let mut src = vec![0u8; 4 * 4 * 4];
        for y in 0..4 {
            for x in 0..4 {
                if (x + y) % 2 == 0 {
                    let i = (y * 4 + x) * 4;
                    src[i] = 200;
                    src[i + 1] = 200;
                    src[i + 2] = 200;
                }
            }
        }
        let half = area_downsample(&src, 4, 4, 4, 2, 2);
        assert_eq!(half.len(), 16);
        assert!(half.iter().step_by(4).all(|&v| v == 100));
        // fractional boxes: 3 -> 2 columns, weights 1.5 each
        let src: Vec<u8> = vec![0, 0, 0, 90, 90, 90, 180, 180, 180];
        let out = area_downsample(&src, 3, 3, 1, 2, 1);
        // left box covers px0 (w1) + half of px1 (w0.5): (0*1 + 90*0.5)/1.5 = 30
        assert_eq!(out[0], 30);
        assert_eq!(out[4], 150);
    }

    /// A picture of pseudo-random pixels (smooth enough to look like video
    /// in places, noisy in others).
    fn picture(w: usize, h: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2654435761) | 1;
        (0..w * h * 4)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let smooth = ((i / 4 % w) * 255 / w.max(1)) as u32;
                (if (i / 4 / w) % 3 == 0 { smooth } else { x % 256 }) as u8
            })
            .collect()
    }

    #[test]
    fn shrink_matches_the_float_area_average() {
        for &(sw, sh, aw, ah) in &[(1920u32, 960u32, 256u32, 128u32), (1920, 1080, 256, 144), (1280, 720, 256, 144), (3840, 2160, 256, 144), (1918, 1078, 256, 143), (640, 360, 256, 144), (256, 144, 256, 144), (97, 61, 13, 7)] {
            let src = picture(sw as usize, sh as usize, sw ^ sh);
            let want = area_downsample(&src, 4, sw, sh, aw, ah);
            let mut sh_ = Shrink::new(sw, sh, aw, ah);
            let mut got = Vec::new();
            sh_.packed(&src, 0, sw as usize * 4, false, &mut got);
            assert_eq!(got.len(), want.len());
            let diff: Vec<i32> = got.iter().zip(&want).map(|(&a, &b)| a as i32 - b as i32).collect();
            let off = diff.iter().filter(|&&d| d != 0).count();
            assert!(diff.iter().all(|d| d.abs() <= 1), "{sw}x{sh} -> {aw}x{ah}: a code off by more than one");
            // only exact halves can round differently
            assert!(off * 1000 <= diff.len(), "{sw}x{sh} -> {aw}x{ah}: {off} of {} codes differ", diff.len());
            if (sw, sh) == (aw, ah) {
                assert_eq!(off, 0, "the same size copies through");
            }
        }
    }

    #[test]
    fn shrink_reads_bgr_strided_and_yuv_pictures() {
        let (sw, sh, aw, ah) = (60usize, 40usize, 16u32, 10u32);
        let src = picture(sw, sh, 7);
        let mut plain = Vec::new();
        let mut s = Shrink::new(sw as u32, sh as u32, aw, ah);
        s.packed(&src, 0, sw * 4, false, &mut plain);
        // the same pixels as B, G, R, X, rows padded to 256 bytes, 32 bytes in
        let stride = 256;
        let mut bgr = vec![0u8; 32 + sh * stride];
        for y in 0..sh {
            for x in 0..sw {
                let p = &src[(y * sw + x) * 4..];
                bgr[32 + y * stride + x * 4..][..4].copy_from_slice(&[p[2], p[1], p[0], 9]);
            }
        }
        let mut got = Vec::new();
        s.packed(&bgr, 32, stride, true, &mut got);
        assert_eq!(got, plain);
        // 4:2:0: as its RGB conversion shrunk
        let l = crate::yuv::YuvLayout::packed_i420(sw, sh, true, false);
        let yuv: Vec<u8> = (0..sw * sh * 3 / 2).map(|i| (i * 37 % 220 + 16) as u8).collect();
        let mut rgba = Vec::new();
        crate::yuv::to_rgba(&yuv, sw, sh, &l, &mut rgba);
        let mut want = Vec::new();
        s.packed(&rgba, 0, sw * 4, false, &mut want);
        s.yuv420(&yuv, &l, &mut got);
        assert_eq!(got, want);
        assert!(s.fits(sw as u32, sh as u32, aw, ah) && !s.fits(sw as u32, sh as u32, aw, ah + 1));
        // the same picture as a decoder holds it: planes padded, read from a crop offset
        let (pad, cx, cy) = (72usize, 4usize, 2usize);
        let mut yp = vec![0u8; pad * (sh + 8)];
        let mut up = vec![0u8; pad / 2 * (sh / 2 + 4)];
        let mut vp = vec![0u8; pad / 2 * (sh / 2 + 4)];
        for r in 0..sh {
            yp[(cy + r) * pad + cx..][..sw].copy_from_slice(&yuv[r * sw..][..sw]);
        }
        for r in 0..sh / 2 {
            up[(cy / 2 + r) * pad / 2 + cx / 2..][..sw / 2].copy_from_slice(&yuv[sw * sh + r * sw / 2..][..sw / 2]);
            vp[(cy / 2 + r) * pad / 2 + cx / 2..][..sw / 2].copy_from_slice(&yuv[sw * sh * 5 / 4 + r * sw / 2..][..sw / 2]);
        }
        let mut planes = Vec::new();
        s.yuv420_planes(&yp[cy * pad + cx..], pad, &up[cy / 2 * pad / 2 + cx / 2..], pad / 2, &vp[cy / 2 * pad / 2 + cx / 2..], pad / 2, true, false, &mut planes);
        assert_eq!(planes, want);
        // NV12 of an odd size, rows padded
        let (w, h) = (37usize, 23usize);
        let cw = w.div_ceil(2);
        let l = crate::yuv::YuvLayout { nv12: true, y_off: 0, y_stride: 40, u_off: 40 * h, u_stride: 2 * cw + 2, v_off: 0, v_stride: 0, bt709: false, full_range: true };
        let data: Vec<u8> = (0..40 * h + (2 * cw + 2) * h.div_ceil(2)).map(|i| (i * 53 % 251) as u8).collect();
        let mut rgba = Vec::new();
        crate::yuv::to_rgba(&data, w, h, &l, &mut rgba);
        let mut s = Shrink::new(w as u32, h as u32, 9, 5);
        s.packed(&rgba, 0, w * 4, false, &mut want);
        s.yuv420(&data, &l, &mut got);
        assert_eq!(got, want);
    }

    #[test]
    fn divider_matches_division() {
        for d in [1u64, 2, 3, 7, 15, 225, 255, 1000, 4096, 50625, 65793, 1 << 20] {
            let max = 255 * d;
            let div = Divider::new(d, max);
            let step = (max / 200_000).max(1);
            let mut n = 0;
            while n <= max {
                assert_eq!(div.round(n), round_div(n, d), "{n} / {d}");
                n += if n < 4 * d { 1 } else { step };
            }
            assert_eq!(div.round(max), round_div(max, d));
        }
    }

    #[test]
    fn halves_round_to_even() {
        assert_eq!(round_div(5, 2), 2);
        assert_eq!(round_div(7, 2), 4);
        assert_eq!(round_div(8, 3), 3);
        assert_eq!(round_div(700, 1), 255);
    }

    /// A picture of no width or height is turned away with a message. (A
    /// size-0 axis put each box's last sample at 0 - 1, which wraps in a
    /// release build, and making the shrink then asked for more memory than
    /// there is.)
    #[test]
    #[should_panic(expected = "has nothing to shrink")]
    fn zero_sizes_are_turned_away() {
        Shrink::new(0, 360, 256, 144);
    }

    #[test]
    fn reuse_keeps_a_shrink_that_fits() {
        let mut slot = None;
        let (src, mut out) = (picture(64, 36, 3), Vec::new());
        Shrink::reuse(&mut slot, 64, 36, 16, 9).packed(&src, 0, 64 * 4, false, &mut out);
        // the same sizes: the same shrink, its row sums' room still there
        assert!(!Shrink::reuse(&mut slot, 64, 36, 16, 9).acc16.is_empty());
        // other sizes: a new one
        let k = Shrink::reuse(&mut slot, 64, 36, 16, 8);
        assert!(k.fits(64, 36, 16, 8) && k.acc16.is_empty());
    }
}

/// Three-pass box blur of an RGB8 picture (close to a Gaussian of
/// σ ≈ 0.9·radius, edges replicated), for softening a regular pattern.
/// `radius` 0 copies. The output is written to `dst`.
pub fn blur_rgb(src: &[u8], w: u32, h: u32, radius: u32, dst: &mut Vec<u8>) {
    blur_px::<3>(src, w, h, radius, dst)
}

/// The blur over `C` interleaved 8-bit channels.
fn blur_px<const C: usize>(src: &[u8], w: u32, h: u32, radius: u32, dst: &mut Vec<u8>) {
    let (w, h) = (w as usize, h as usize);
    let n = w * h * C;
    dst.clear();
    dst.extend_from_slice(&src[..n]);
    if radius == 0 || w == 0 || h == 0 {
        return;
    }
    let r = radius as usize;
    let mut tmp = vec![0u8; n];
    let mut line: Vec<[u32; C]> = Vec::with_capacity(w.max(h));
    for _ in 0..3 {
        box_pass::<C>(dst, &mut tmp, w, h, r, true, &mut line);
        box_pass::<C>(&tmp, dst, w, h, r, false, &mut line);
    }
}

/// One box pass along rows (`horizontal`) or columns.
fn box_pass<const C: usize>(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize, horizontal: bool, line: &mut Vec<[u32; C]>) {
    let (lines, len) = if horizontal { (h, w) } else { (w, h) };
    let win = (2 * r + 1) as u32;
    let half = win / 2;
    for l in 0..lines {
        let at = |i: usize| -> usize {
            if horizontal {
                (l * w + i) * C
            } else {
                (i * w + l) * C
            }
        };
        line.clear();
        for i in 0..len {
            let k = at(i);
            let mut px = [0u32; C];
            for c in 0..C {
                px[c] = src[k + c] as u32;
            }
            line.push(px);
        }
        let clamp = |i: isize| -> [u32; C] { line[i.clamp(0, len as isize - 1) as usize] };
        let mut sum = [0u32; C];
        for k in -(r as isize)..=(r as isize) {
            let v = clamp(k);
            for c in 0..C {
                sum[c] += v[c];
            }
        }
        for i in 0..len {
            let k = at(i);
            for c in 0..C {
                dst[k + c] = ((sum[c] + half) / win) as u8;
            }
            let add = clamp(i as isize + r as isize + 1);
            let sub = clamp(i as isize - r as isize);
            for c in 0..C {
                sum[c] = sum[c] + add[c] - sub[c];
            }
        }
    }
}

#[cfg(test)]
mod blur_tests {
    use super::*;

    #[test]
    fn blur_keeps_flat_pictures_and_flattens_stripes() {
        let (w, h) = (32u32, 8u32);
        let flat = vec![100u8; (w * h * 4) as usize];
        let mut out = Vec::new();
        blur_px::<4>(&flat, w, h, 3, &mut out);
        assert_eq!(out, flat);
        let mut stripes = vec![255u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let v = if (x / 3) % 2 == 0 { 20 } else { 200 };
                let k = ((y * w + x) * 4) as usize;
                stripes[k] = v;
                stripes[k + 1] = v;
                stripes[k + 2] = v;
            }
        }
        blur_px::<4>(&stripes, w, h, 3, &mut out);
        let row: Vec<u8> = (0..w).map(|x| out[(x * 4) as usize]).collect();
        let (lo, hi) = (row[4..28].iter().min().unwrap(), row[4..28].iter().max().unwrap());
        assert!(hi - lo < 20, "stripes should flatten: {row:?}");
        assert!(out.iter().skip(3).step_by(4).all(|&a| a == 255), "alpha is untouched");
        blur_px::<4>(&stripes, w, h, 0, &mut out);
        assert_eq!(out, stripes);
    }
}
