//! The inverse transforms (RFC 6386 section 14): the Walsh-Hadamard
//! transform of the second-order luma DC block and the 4x4 DCT added to
//! the prediction. Intermediate rows are stored in 16 bits as libvpx and
//! ffmpeg store them, which only matters for coefficients no encoder makes.

/// 14.3: invert the Y2 block (raster order); `out[i]` is the DC of luma
/// block `i` (raster order).
pub fn iwht(input: &[i16; 16], out: &mut [i16; 16]) {
    let mut t = [0i16; 16];
    for i in 0..4 {
        let a = input[i] as i32 + input[12 + i] as i32;
        let b = input[4 + i] as i32 + input[8 + i] as i32;
        let c = input[4 + i] as i32 - input[8 + i] as i32;
        let d = input[i] as i32 - input[12 + i] as i32;
        t[i] = (a + b) as i16;
        t[4 + i] = (d + c) as i16;
        t[8 + i] = (a - b) as i16;
        t[12 + i] = (d - c) as i16;
    }
    for i in 0..4 {
        let r = &t[4 * i..4 * i + 4];
        let a = r[0] as i32 + r[3] as i32 + 3;
        let b = r[1] as i32 + r[2] as i32;
        let c = r[1] as i32 - r[2] as i32;
        let d = r[0] as i32 - r[3] as i32 + 3;
        out[4 * i] = ((a + b) >> 3) as i16;
        out[4 * i + 1] = ((d + c) >> 3) as i16;
        out[4 * i + 2] = ((a - b) >> 3) as i16;
        out[4 * i + 3] = ((d - c) >> 3) as i16;
    }
}

/// x * sqrt(2) * cos(pi / 8), in the fixed point the RFC defines.
#[inline(always)]
fn mul_20091(x: i32) -> i32 {
    ((x * 20091) >> 16) + x
}

/// x * sqrt(2) * sin(pi / 8).
#[inline(always)]
fn mul_35468(x: i32) -> i32 {
    (x * 35468) >> 16
}

/// Add a 4x4 residual (raster order) to the prediction in `dst`, clipping
/// to 0..=255. The transform's residual stays under 16000 in size (the
/// second pass's gain on 16-bit rows, over 8), so it and the prediction
/// plus it fit 16 bits; with `simd` two rows go through at once and the
/// saturating narrowing is the clip.
#[inline(always)]
fn add_4x4(r: &[i16; 16], dst: &mut [u8], stride: usize) {
    #[cfg(feature = "simd")]
    {
        use wide::{i16x8, u8x16};
        for pair in 0..2 {
            let (top, bottom) = (2 * pair * stride, (2 * pair + 1) * stride);
            let mut p = [0u8; 16];
            p[..4].copy_from_slice(&dst[top..top + 4]);
            p[4..8].copy_from_slice(&dst[bottom..bottom + 4]);
            let res: [i16; 8] = r[8 * pair..8 * pair + 8].try_into().unwrap();
            let v = i16x8::from_u8x16_low(u8x16::from(p)) + i16x8::from(res);
            let out = u8x16::narrow_i16x8(v, v);
            dst[top..top + 4].copy_from_slice(&out.as_array_ref()[..4]);
            dst[bottom..bottom + 4].copy_from_slice(&out.as_array_ref()[4..8]);
        }
    }
    #[cfg(not(feature = "simd"))]
    for (i, row) in r.chunks(4).enumerate() {
        for (p, &v) in dst[i * stride..i * stride + 4].iter_mut().zip(row) {
            *p = (*p as i32 + v as i32).clamp(0, 255) as u8;
        }
    }
}

/// 14.4: inverse DCT of a block (raster order) added to the prediction in
/// `dst` (stride `stride`).
pub fn idct_add(c: &[i16; 16], dst: &mut [u8], stride: usize) {
    // columns
    let mut t = [0i16; 16];
    for i in 0..4 {
        let (c0, c1, c2, c3) = (c[i] as i32, c[4 + i] as i32, c[8 + i] as i32, c[12 + i] as i32);
        let a = c0 + c2;
        let b = c0 - c2;
        let cc = mul_35468(c1) - mul_20091(c3);
        let d = mul_20091(c1) + mul_35468(c3);
        t[4 * i] = (a + d) as i16;
        t[4 * i + 1] = (b + cc) as i16;
        t[4 * i + 2] = (b - cc) as i16;
        t[4 * i + 3] = (a - d) as i16;
    }
    // rows: t[4 * column + row]
    let mut r = [0i16; 16];
    for i in 0..4 {
        let (t0, t1, t2, t3) = (t[i] as i32, t[4 + i] as i32, t[8 + i] as i32, t[12 + i] as i32);
        let a = t0 + t2;
        let b = t0 - t2;
        let cc = mul_35468(t1) - mul_20091(t3);
        let d = mul_20091(t1) + mul_35468(t3);
        r[4 * i] = ((a + d + 4) >> 3) as i16;
        r[4 * i + 1] = ((b + cc + 4) >> 3) as i16;
        r[4 * i + 2] = ((b - cc + 4) >> 3) as i16;
        r[4 * i + 3] = ((a - d + 4) >> 3) as i16;
    }
    add_4x4(&r, dst, stride);
}

/// A block whose only coefficient is the DC: the transform is a constant
/// (the same as [`idct_add`] gives).
pub fn idct_dc_add(dc: i16, dst: &mut [u8], stride: usize) {
    add_4x4(&[((dc as i32 + 4) >> 3) as i16; 16], dst, stride);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_only_blocks_are_flat() {
        for dc in [-2048i16, -300, -5, 0, 4, 77, 1000, 2047] {
            let mut a = [100u8; 16];
            let mut b = [100u8; 16];
            let mut c = [0i16; 16];
            c[0] = dc;
            idct_add(&c, &mut a, 4);
            idct_dc_add(dc, &mut b, 4);
            assert_eq!(a, b, "dc {dc}");
        }
        let mut dc = [0i16; 16];
        dc[0] = 85;
        let mut out = [0i16; 16];
        iwht(&dc, &mut out);
        assert!(out.iter().all(|&v| v == (85 + 3) >> 3));
    }

    #[test]
    fn wht_inverts_the_forward_transform() {
        // the forward WHT of libvpx's encoder (vp8_short_walsh4x4_c)
        fn fwht(input: &[i32; 16]) -> [i16; 16] {
            let mut t = [0i32; 16];
            for i in 0..4 {
                let ip = &input[4 * i..4 * i + 4];
                let a1 = (ip[0] + ip[2]) << 2;
                let d1 = (ip[1] + ip[3]) << 2;
                let c1 = (ip[1] - ip[3]) << 2;
                let b1 = (ip[0] - ip[2]) << 2;
                t[4 * i] = a1 + d1 + (a1 != 0) as i32;
                t[4 * i + 1] = b1 + c1;
                t[4 * i + 2] = b1 - c1;
                t[4 * i + 3] = a1 - d1;
            }
            let mut out = [0i16; 16];
            for i in 0..4 {
                let a1 = t[i] + t[8 + i];
                let d1 = t[4 + i] + t[12 + i];
                let c1 = t[4 + i] - t[12 + i];
                let b1 = t[i] - t[8 + i];
                let mut v = [a1 + d1, b1 + c1, b1 - c1, a1 - d1];
                for x in v.iter_mut() {
                    *x += (*x < 0) as i32;
                    *x = (*x + 3) >> 3;
                }
                for k in 0..4 {
                    out[4 * k + i] = v[k] as i16;
                }
            }
            out
        }
        let input: [i32; 16] = [40, -8, 16, 0, 3, 3, -60, 12, 7, 0, 0, 90, -1, 25, 4, -33];
        let mut back = [0i16; 16];
        iwht(&fwht(&input), &mut back);
        for i in 0..16 {
            assert!((back[i] as i32 - input[i]).abs() <= 1, "{i}: {} vs {}", back[i], input[i]);
        }
    }
}
