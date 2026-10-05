//! Scaling and inverse transforms (8.5.9 – 8.5.13).

#[inline(always)]
fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// 8.5.12.1: scale the AC / 4x4 coefficient levels of one block in place
/// (raster order); `dc_done` leaves position 0 alone because it came out of
/// a DC transform.
pub fn dequant4x4(c: &mut [i32; 16], level_scale: &[i32; 16], qp: i32, dc_done: bool) {
    let qp6 = qp / 6;
    let start = if dc_done { 1 } else { 0 };
    if qp >= 24 {
        let sh = qp6 - 4;
        for i in start..16 {
            c[i] = (c[i] * level_scale[i]) << sh;
        }
    } else {
        let sh = 4 - qp6;
        let add = 1 << (3 - qp6);
        for i in start..16 {
            c[i] = (c[i] * level_scale[i] + add) >> sh;
        }
    }
}

/// 8.5.13.1: scale an 8x8 block in place (raster order).
pub fn dequant8x8(c: &mut [i32; 64], level_scale: &[i32; 64], qp: i32) {
    let qp6 = qp / 6;
    if qp >= 36 {
        let sh = qp6 - 6;
        for i in 0..64 {
            c[i] = (c[i] * level_scale[i]) << sh;
        }
    } else {
        let sh = 6 - qp6;
        let add = 1 << (5 - qp6);
        for i in 0..64 {
            c[i] = (c[i] * level_scale[i] + add) >> sh;
        }
    }
}

/// 8.5.10: the Intra16x16 luma DC transform and scaling. `c` holds the 16
/// DC levels in raster order of the 4x4 blocks; returns dcY per block.
pub fn luma_dc(c: &[i32; 16], level_scale00: i32, qp: i32) -> [i32; 16] {
    let mut f = [0i32; 16];
    // rows: f = H * c
    let mut t = [0i32; 16];
    for j in 0..4 {
        let (c0, c1, c2, c3) = (c[j], c[4 + j], c[8 + j], c[12 + j]);
        t[j] = c0 + c1 + c2 + c3;
        t[4 + j] = c0 + c1 - c2 - c3;
        t[8 + j] = c0 - c1 - c2 + c3;
        t[12 + j] = c0 - c1 + c2 - c3;
    }
    for i in 0..4 {
        let (t0, t1, t2, t3) = (t[4 * i], t[4 * i + 1], t[4 * i + 2], t[4 * i + 3]);
        f[4 * i] = t0 + t1 + t2 + t3;
        f[4 * i + 1] = t0 + t1 - t2 - t3;
        f[4 * i + 2] = t0 - t1 - t2 + t3;
        f[4 * i + 3] = t0 - t1 + t2 - t3;
    }
    let qp6 = qp / 6;
    let mut out = [0i32; 16];
    for i in 0..16 {
        out[i] = if qp >= 36 { (f[i] * level_scale00) << (qp6 - 6) } else { (f[i] * level_scale00 + (1 << (5 - qp6))) >> (6 - qp6) };
    }
    out
}

/// 8.5.11: the 2x2 chroma DC transform and scaling (4:2:0). `c` in raster
/// order of the four 4x4 chroma blocks.
pub fn chroma_dc(c: &[i32; 4], level_scale00: i32, qpc: i32) -> [i32; 4] {
    let f0 = c[0] + c[1] + c[2] + c[3];
    let f1 = c[0] - c[1] + c[2] - c[3];
    let f2 = c[0] + c[1] - c[2] - c[3];
    let f3 = c[0] - c[1] - c[2] + c[3];
    let f = [f0, f1, f2, f3];
    let mut out = [0i32; 4];
    for i in 0..4 {
        out[i] = ((f[i] * level_scale00) << (qpc / 6)) >> 5;
    }
    out
}

/// 8.5.12.2: inverse 4x4 transform of scaled coefficients (raster order),
/// added to the prediction already in `dst`.
pub fn idct4x4_add(d: &[i32; 16], dst: &mut [u8], stride: usize) {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let (d0, d1, d2, d3) = (d[4 * i], d[4 * i + 1], d[4 * i + 2], d[4 * i + 3]);
        let e0 = d0 + d2;
        let e1 = d0 - d2;
        let e2 = (d1 >> 1) - d3;
        let e3 = d1 + (d3 >> 1);
        t[4 * i] = e0 + e3;
        t[4 * i + 1] = e1 + e2;
        t[4 * i + 2] = e1 - e2;
        t[4 * i + 3] = e0 - e3;
    }
    for j in 0..4 {
        let (f0, f1, f2, f3) = (t[j], t[4 + j], t[8 + j], t[12 + j]);
        let g0 = f0 + f2;
        let g1 = f0 - f2;
        let g2 = (f1 >> 1) - f3;
        let g3 = f1 + (f3 >> 1);
        let h = [g0 + g3, g1 + g2, g1 - g2, g0 - g3];
        for i in 0..4 {
            let p = &mut dst[i * stride + j];
            *p = clip(*p as i32 + ((h[i] + 32) >> 6));
        }
    }
}

/// A block whose only coefficient is the DC: the transform is a constant.
pub fn idct4x4_dc_add(dc: i32, dst: &mut [u8], stride: usize) {
    let v = (dc + 32) >> 6;
    for i in 0..4 {
        for j in 0..4 {
            let p = &mut dst[i * stride + j];
            *p = clip(*p as i32 + v);
        }
    }
}

/// 8.5.13.2: inverse 8x8 transform added to the prediction in `dst`.
pub fn idct8x8_add(d: &[i32; 64], dst: &mut [u8], stride: usize) {
    let mut t = [0i32; 64];
    for i in 0..8 {
        let r = &d[8 * i..8 * i + 8];
        let a0 = r[0] + r[4];
        let a4 = r[0] - r[4];
        let a2 = (r[2] >> 1) - r[6];
        let a6 = r[2] + (r[6] >> 1);
        let b0 = a0 + a6;
        let b2 = a4 + a2;
        let b4 = a4 - a2;
        let b6 = a0 - a6;
        let a1 = -r[3] + r[5] - r[7] - (r[7] >> 1);
        let a3 = r[1] + r[7] - r[3] - (r[3] >> 1);
        let a5 = -r[1] + r[7] + r[5] + (r[5] >> 1);
        let a7 = r[3] + r[5] + r[1] + (r[1] >> 1);
        let b1 = a1 + (a7 >> 2);
        let b7 = a7 - (a1 >> 2);
        let b3 = a3 + (a5 >> 2);
        let b5 = (a3 >> 2) - a5;
        let o = &mut t[8 * i..8 * i + 8];
        o[0] = b0 + b7;
        o[1] = b2 + b5;
        o[2] = b4 + b3;
        o[3] = b6 + b1;
        o[4] = b6 - b1;
        o[5] = b4 - b3;
        o[6] = b2 - b5;
        o[7] = b0 - b7;
    }
    for j in 0..8 {
        let c = |k: usize| t[8 * k + j];
        let a0 = c(0) + c(4);
        let a4 = c(0) - c(4);
        let a2 = (c(2) >> 1) - c(6);
        let a6 = c(2) + (c(6) >> 1);
        let b0 = a0 + a6;
        let b2 = a4 + a2;
        let b4 = a4 - a2;
        let b6 = a0 - a6;
        let a1 = -c(3) + c(5) - c(7) - (c(7) >> 1);
        let a3 = c(1) + c(7) - c(3) - (c(3) >> 1);
        let a5 = -c(1) + c(7) + c(5) + (c(5) >> 1);
        let a7 = c(3) + c(5) + c(1) + (c(1) >> 1);
        let b1 = a1 + (a7 >> 2);
        let b7 = a7 - (a1 >> 2);
        let b3 = a3 + (a5 >> 2);
        let b5 = (a3 >> 2) - a5;
        let h = [b0 + b7, b2 + b5, b4 + b3, b6 + b1, b6 - b1, b4 - b3, b2 - b5, b0 - b7];
        for i in 0..8 {
            let p = &mut dst[i * stride + j];
            *p = clip(*p as i32 + ((h[i] + 32) >> 6));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_only_blocks_are_flat() {
        let mut dst = [100u8; 16];
        let mut d = [0i32; 16];
        d[0] = 64 * 3;
        idct4x4_add(&d, &mut dst, 4);
        assert!(dst.iter().all(|&v| v == 103));
        let mut dst2 = [100u8; 16];
        idct4x4_dc_add(64 * 3, &mut dst2, 4);
        assert_eq!(dst, dst2);
        let mut dst = [10u8; 64];
        let mut d = [0i32; 64];
        d[0] = 64 * 5;
        idct8x8_add(&d, &mut dst, 8);
        assert!(dst.iter().all(|&v| v == 15));
    }

    #[test]
    fn hadamard_of_a_single_dc() {
        let mut c = [0i32; 16];
        c[0] = 10;
        // qp 30 -> qp/6 = 5, LevelScale(0,0,0) = 160 for flat lists at qp%6 = 0
        let out = luma_dc(&c, 160, 30);
        // f = 10 everywhere; (10 * 160 + 1) >> 1 = 800
        assert!(out.iter().all(|&v| v == 800));
        let out = chroma_dc(&[8, 0, 0, 0], 160, 30);
        assert!(out.iter().all(|&v| v == ((8 * 160) << 5) >> 5));
    }
}
