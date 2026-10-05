//! Scaling (8.6.3) and inverse transformation (8.6.4) of residual blocks,
//! and adding residuals to predicted samples.

use crate::picture::Sample;

/// levelScale[ ] (8.6.3).
pub const LEVEL_SCALE: [i32; 6] = [40, 45, 51, 57, 64, 72];

/// The magnitudes of the 32-point transform: T[j] for the angle jπ/64
/// (T[0] is the DC basis).
const T: [i8; 33] = [64, 90, 90, 90, 89, 88, 87, 85, 83, 82, 80, 78, 75, 73, 70, 67, 64, 61, 57, 54, 50, 46, 43, 38, 36, 31, 25, 22, 18, 13, 9, 4, 0];

/// transMatrix (8-318 .. 8-321) as `M[frequency][sample]`: the basis
/// function k at sample n is T at the angle k(2n + 1) folded into the
/// first quadrant with the cosine's sign.
pub const DCT: [[i8; 32]; 32] = {
    let mut m = [[0i8; 32]; 32];
    let mut k = 0;
    while k < 32 {
        let mut n = 0;
        while n < 32 {
            let j = (k * (2 * n + 1)) % 128;
            m[k][n] = if k == 0 {
                64
            } else if j <= 32 {
                T[j]
            } else if j <= 64 {
                -T[64 - j]
            } else if j <= 96 {
                -T[j - 64]
            } else {
                T[128 - j]
            };
            n += 1;
        }
        k += 1;
    }
    m
};

/// The 4x4 DST of intra luma blocks (8-316), `[frequency][sample]`.
const DST: [[i32; 4]; 4] = [[29, 55, 74, 84], [74, 74, 0, -74], [84, -29, -74, 55], [55, -84, 74, -29]];

#[cfg(any(not(feature = "simd"), test))]
/// The `n`-point inverse DCT (`n` = 4, 8, 16 or 32) of `x`, of which only
/// the first `count` values may be non-zero, into `out`. The even
/// coefficients make the half-size transform (the matrix's even rows are
/// its rows), and the odd ones a part that the matrix's symmetry adds to
/// the first half and subtracts from the mirrored second half, which
/// takes a quarter of the multiplications of the plain matrix product.
fn idct(x: &[i32], n: usize, count: usize, out: &mut [i32]) {
    if n == 4 {
        let (e0, e1) = (64 * (x[0] + x[2]), 64 * (x[0] - x[2]));
        let (o0, o1) = (83 * x[1] + 36 * x[3], 36 * x[1] - 83 * x[3]);
        out[..4].copy_from_slice(&[e0 + o0, e1 + o1, e1 - o1, e0 - o0]);
        return;
    }
    let (half, step) = (n / 2, 32 / n);
    let mut even_x = [0i32; 16];
    for (e, &v) in even_x.iter_mut().zip(x.iter().step_by(2)).take(count.div_ceil(2)) {
        *e = v;
    }
    let mut even = [0i32; 16];
    idct(&even_x[..half], half, count.div_ceil(2), &mut even[..half]);
    let mut odd = [0i32; 16];
    let mut k = 1;
    while k < count {
        let (v, row) = (x[k], &DCT[k * step][..half]);
        for (o, &m) in odd.iter_mut().zip(row) {
            *o += m as i32 * v;
        }
        k += 2;
    }
    for (m, (&e, &o)) in even[..half].iter().zip(&odd).enumerate() {
        out[m] = e + o;
        out[n - 1 - m] = e - o;
    }
}

/// 8.6.4: the inverse transform of the `n`×`n` scaled coefficients in `c`
/// (row-major: `c[y * n + x]` is `d[x][y]`), whose non-zero values all lie in
/// columns 0..=`max_x` and rows 0..=`max_y`, into residuals (row-major,
/// after the final `bd_shift`, 8-299) in `res`. `c` is used as scratch.
pub fn inverse_transform(c: &mut [i32], n: usize, dst: bool, bd_shift: u32, max_x: usize, max_y: usize, res: &mut [i32]) {
    if max_x == 0 && max_y == 0 && !dst {
        // a DC coefficient alone: the transform is a constant
        let g = ((64 * c[0] + 64) >> 7).clamp(-32768, 32767);
        let v = (64 * g + (1 << (bd_shift - 1))) >> bd_shift;
        res[..n * n].fill(v);
        return;
    }
    let c = &mut c[..n * n];
    let res = &mut res[..n * n];
    // columns: g[x][y] = Clip3(coeffMin, coeffMax, (Σ_j M[j][y] d[x][j] + 64) >> 7),
    // written over the column's coefficients; then the rows: r[x][y] =
    // Σ_j M[j][x] g[j][y] with the bdShift rounding (the columns beyond
    // max_x are still zero coefficients)
    #[cfg(feature = "simd")]
    {
        simd::columns(c, n, dst, max_x, max_y);
        simd::rows(c, n, dst, max_x, bd_shift, res);
    }
    #[cfg(not(feature = "simd"))]
    {
        let transform = |x: &[i32], count: usize, out: &mut [i32]| {
            if dst {
                for (y, o) in out[..4].iter_mut().enumerate() {
                    *o = (0..count).map(|j| DST[j][y] * x[j]).sum();
                }
            } else {
                idct(x, n, count, out);
            }
        };
        let mut col = [0i32; 32];
        let mut out = [0i32; 32];
        for x in 0..=max_x {
            for (j, v) in col[..=max_y].iter_mut().enumerate() {
                *v = c[j * n + x];
            }
            transform(&col[..n], max_y + 1, &mut out[..n]);
            for (y, &s) in out[..n].iter().enumerate() {
                c[y * n + x] = ((s + 64) >> 7).clamp(-32768, 32767);
            }
        }
        let round = 1 << (bd_shift - 1);
        for (row, r) in c.chunks_exact(n).zip(res.chunks_exact_mut(n)) {
            transform(row, max_x + 1, &mut out[..n]);
            for (r, &s) in r.iter_mut().zip(&out[..n]) {
                *r = (s + round) >> bd_shift;
            }
        }
    }
}

/// The transforms as matrix products eight lanes at a time: each pair of
/// coefficients, repeated across the lanes, is multiplied with the pair's
/// two matrix rows at four sample positions and summed into 32 bits by one
/// dot product instruction (pmaddwd on SSE2, i32x4.dot_i16x8_s in wasm
/// simd128). The inputs fit 16 bits (the coefficients and the first
/// stage's output are clipped to them) and the sums 32, so this is exact.
#[cfg(feature = "simd")]
mod simd {
    use wide::{i16x8, i32x4};

    use super::{DCT, DST};

    /// transMatrix of the `n`-point transform, at frequency `k` and sample `s`.
    const fn m(n: usize, dst: bool, k: usize, s: usize) -> i16 {
        if dst {
            DST[k][s] as i16
        } else {
            DCT[k * (32 / n)][s] as i16
        }
    }

    /// The matrix in pairs of frequencies and quads of samples: entry
    /// `[p * n / 4 + q]` is m(2p, 4q), m(2p + 1, 4q), m(2p, 4q + 1), ...
    const fn pairs(n: usize, dst: bool) -> [[i16; 8]; 128] {
        let mut t = [[0i16; 8]; 128];
        let quads = if n < 4 { 1 } else { n / 4 };
        let mut p = 0;
        while p < n / 2 {
            let mut q = 0;
            while q < quads {
                let mut i = 0;
                while i < 4 {
                    t[p * quads + q][2 * i] = m(n, dst, 2 * p, 4 * q + i);
                    t[p * quads + q][2 * i + 1] = m(n, dst, 2 * p + 1, 4 * q + i);
                    i += 1;
                }
                q += 1;
            }
            p += 1;
        }
        t
    }

    static TABLES: [[[i16; 8]; 128]; 5] = [pairs(4, true), pairs(4, false), pairs(8, false), pairs(16, false), pairs(32, false)];

    fn table(n: usize, dst: bool) -> &'static [[i16; 8]; 128] {
        &TABLES[match (n, dst) {
            (_, true) => 0,
            (4, _) => 1,
            (8, _) => 2,
            (16, _) => 3,
            _ => 4,
        }]
    }

    /// Σ_{k < count} m(k, s) · x(k) for the `n` samples s, four per lane group.
    #[inline(always)]
    fn product(x: impl Fn(usize) -> i32, t: &[[i16; 8]; 128], n: usize, count: usize, acc: &mut [i32x4; 8]) {
        let quads = n / 4;
        acc[..quads].fill(i32x4::ZERO);
        for p in 0..count.div_ceil(2) {
            let (a, b) = (x(2 * p) as i16, x(2 * p + 1) as i16);
            let xv = i16x8::new([a, b, a, b, a, b, a, b]);
            for (q, sum) in acc[..quads].iter_mut().enumerate() {
                *sum += xv.dot(i16x8::new(t[p * quads + q]));
            }
        }
    }

    pub fn columns(c: &mut [i32], n: usize, dst: bool, max_x: usize, max_y: usize) {
        let t = table(n, dst);
        let mut acc = [i32x4::ZERO; 8];
        let (lo, hi) = (i32x4::splat(-32768), i32x4::splat(32767));
        for x in 0..=max_x {
            product(|j| c[j * n + x], t, n, max_y + 1, &mut acc);
            for (q, &sum) in acc[..n / 4].iter().enumerate() {
                let g = ((sum + 64i32) >> 7i32).max(lo).min(hi);
                for (i, &v) in g.to_array().iter().enumerate() {
                    c[(4 * q + i) * n + x] = v;
                }
            }
        }
    }

    pub fn rows(c: &[i32], n: usize, dst: bool, max_x: usize, bd_shift: u32, res: &mut [i32]) {
        let t = table(n, dst);
        let mut acc = [i32x4::ZERO; 8];
        let round = 1 << (bd_shift - 1);
        for (row, r) in c.chunks_exact(n).zip(res.chunks_exact_mut(n)) {
            product(|k| row[k], t, n, max_x + 1, &mut acc);
            for (out, &sum) in r.chunks_exact_mut(4).zip(&acc[..n / 4]) {
                out.copy_from_slice(&((sum + round) >> bd_shift).to_array());
            }
        }
    }
}

/// Add residuals to the prediction in `dst` (stride `ds`), clipping to
/// the bit depth.
pub fn add_residual<P: Sample>(dst: &mut [P], ds: usize, res: &[i32], n: usize, bit_depth: u32) {
    let max = (1 << bit_depth) - 1;
    for y in 0..n {
        let d = &mut dst[y * ds..y * ds + n];
        let r = &res[y * n..y * n + n];
        for (p, &v) in d.iter_mut().zip(r) {
            *p = P::new((p.get() + v).clamp(0, max));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_matches_the_spec_rows() {
        assert_eq!(DCT[1][..16], [90, 90, 88, 85, 82, 78, 73, 67, 61, 54, 46, 38, 31, 22, 13, 4]);
        assert_eq!(DCT[5][..16], [88, 67, 31, -13, -54, -82, -90, -78, -46, -4, 38, 73, 90, 85, 61, 22]);
        assert_eq!(DCT[13][..16], [73, -31, -90, -22, 78, 67, -38, -90, -13, 82, 61, -46, -88, -4, 85, 54]);
        assert_eq!(DCT[31][..16], [4, -13, 22, -31, 38, -46, 54, -61, 67, -73, 78, -82, 85, -88, 90, -90]);
        assert_eq!(DCT[1][16..], [-4, -13, -22, -31, -38, -46, -54, -61, -67, -73, -78, -82, -85, -88, -90, -90]);
        assert_eq!(DCT[27][16..], [88, -67, 31, 13, -54, 82, -90, 78, -46, 4, 38, -73, 90, -85, 61, -22]);
        assert_eq!(DCT[28][16..], [18, -50, 75, -89, 89, -75, 50, -18, -18, 50, -75, 89, -89, 75, -50, 18]);
        assert_eq!(DCT[29][16..], [-90, 82, -67, 46, -22, -4, 31, -54, 73, -85, 90, -88, 78, -61, 38, -13]);
        assert_eq!(DCT[16][..8], [64, -64, -64, 64, 64, -64, -64, 64]);
    }

    #[test]
    fn butterflies_match_the_matrix_product() {
        let mut seed = 7u32;
        for n in [4, 8, 16, 32] {
            for count in 1..=n {
                let x: Vec<i32> = (0..n)
                    .map(|k| {
                        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                        if k < count {
                            (seed >> 16) as i32 % 65536 - 32768
                        } else {
                            0
                        }
                    })
                    .collect();
                let mut out = vec![0; n];
                idct(&x, n, count, &mut out);
                for (y, &o) in out.iter().enumerate() {
                    let direct: i32 = (0..n).map(|k| DCT[k * (32 / n)][y] as i32 * x[k]).sum();
                    assert_eq!(o, direct, "n {n} count {count} y {y}");
                }
            }
        }
    }

    /// 8.6.4 as written: the matrix products with the intermediate clip.
    fn reference(c: &[i32], n: usize, dst: bool, bd_shift: u32) -> Vec<i32> {
        let m = |k: usize, s: usize| if dst { DST[k][s] } else { DCT[k * (32 / n)][s] as i32 };
        let mut g = vec![0i64; n * n];
        for x in 0..n {
            for y in 0..n {
                let s: i64 = (0..n).map(|j| m(j, y) as i64 * c[j * n + x] as i64).sum();
                g[y * n + x] = ((s + 64) >> 7).clamp(-32768, 32767);
            }
        }
        let mut r = vec![0i32; n * n];
        for y in 0..n {
            for x in 0..n {
                let s: i64 = (0..n).map(|j| m(j, x) as i64 * g[y * n + j]).sum();
                r[y * n + x] = ((s + (1 << (bd_shift - 1))) >> bd_shift) as i32;
            }
        }
        r
    }

    #[test]
    fn transforms_match_the_spec() {
        let mut seed = 11u32;
        let mut rand = |range: i32| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as i32 % (2 * range + 1) - range
        };
        for (n, dst) in [(4, true), (4, false), (8, false), (16, false), (32, false)] {
            for round in 0..40 {
                let (max_x, max_y) = ((rand(n as i32).unsigned_abs() as usize) % n, (rand(n as i32).unsigned_abs() as usize) % n);
                // small levels, and full-range ones that saturate the intermediate clip
                let range = if round % 2 == 0 { 300 } else { 32767 };
                let mut c = vec![0i32; n * n];
                for y in 0..=max_y {
                    for x in 0..=max_x {
                        c[y * n + x] = rand(range);
                    }
                }
                let want = reference(&c, n, dst, 12);
                let mut res = vec![0i32; n * n];
                inverse_transform(&mut c, n, dst, 12, max_x, max_y, &mut res);
                assert_eq!(res, want, "n {n} dst {dst} last ({max_x}, {max_y})");
            }
        }
    }

    #[test]
    fn dc_blocks_are_flat() {
        let mut c = vec![0i32; 64];
        c[0] = 100;
        let mut res = vec![0i32; 64];
        inverse_transform(&mut c, 8, false, 12, 0, 0, &mut res);
        let mut c2 = vec![0i32; 64];
        c2[0] = 100;
        c2[1] = 0;
        let mut res2 = vec![0i32; 64];
        // the general path with a (zero) second coefficient gives the same
        inverse_transform(&mut c2, 8, false, 12, 1, 0, &mut res2);
        assert_eq!(res, res2);
        assert!(res.iter().all(|&v| v == res[0]));
    }
}
