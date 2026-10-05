//! Intra prediction (RFC 6386 section 12).
//!
//! The predictors take their edges as small arrays the caller gathers from
//! the picture. Beyond the picture the row above reads 127 (its above-left
//! corner too) and the column to the left reads 129, which is how libvpx
//! lays out its frame borders; only the 16x16 and chroma DC modes look at
//! which edges exist instead.

use crate::tables::*;

#[inline(always)]
fn avg2(a: u8, b: u8) -> u8 {
    ((a as u32 + b as u32 + 1) >> 1) as u8
}

#[inline(always)]
fn avg3(a: u8, b: u8, c: u8) -> u8 {
    ((a as u32 + 2 * b as u32 + c as u32 + 2) >> 2) as u8
}

/// 12.2 and 12.3: predict an `N`x`N` block (16 for luma, 8 for chroma).
/// `above[0]` is the above-left sample and `above[1..=N]` the row above;
/// `left` is the column to the left. The DC mode averages only the edges
/// inside the picture (`have_above`, `have_left`).
pub fn predict_block<const N: usize>(mode: u8, above: &[u8], left: &[u8], have_above: bool, have_left: bool, dst: &mut [u8], stride: usize) {
    let top = &above[1..=N];
    match mode {
        V_PRED => {
            for row in dst.chunks_mut(stride).take(N) {
                row[..N].copy_from_slice(top);
            }
        }
        H_PRED => {
            for (row, &l) in dst.chunks_mut(stride).take(N).zip(left) {
                row[..N].fill(l);
            }
        }
        TM_PRED => {
            let p = above[0] as i32;
            for (row, &l) in dst.chunks_mut(stride).take(N).zip(left) {
                let d = l as i32 - p;
                for (o, &a) in row[..N].iter_mut().zip(top) {
                    *o = (a as i32 + d).clamp(0, 255) as u8;
                }
            }
        }
        _ => {
            let shift = N.trailing_zeros();
            let sum = |e: &[u8]| e[..N].iter().map(|&v| v as u32).sum::<u32>();
            let dc = match (have_above, have_left) {
                (true, true) => (sum(top) + sum(left) + N as u32) >> (shift + 1),
                (true, false) => (sum(top) + (N as u32 >> 1)) >> shift,
                (false, true) => (sum(left) + (N as u32 >> 1)) >> shift,
                (false, false) => 128,
            };
            for row in dst.chunks_mut(stride).take(N) {
                row[..N].fill(dc as u8);
            }
        }
    }
}

/// 12.3: predict a 4x4 luma subblock. `above[0]` is the above-left
/// sample, `above[1..=4]` the row above and `above[5..=8]` the four
/// samples to its right; `left` is the column to the left.
pub fn predict_subblock(mode: u8, above: &[u8; 9], left: &[u8; 4], dst: &mut [u8], stride: usize) {
    let p = above[0];
    let a = &above[1..9];
    let l = left;
    // the edge from the bottom-left corner round to the top right, as the
    // RFC's E: L[3], L[2], L[1], L[0], P, A[0] ..= A[3]
    let e = [l[3], l[2], l[1], l[0], p, a[0], a[1], a[2], a[3]];
    let mut b = [[0u8; 4]; 4];
    match mode {
        B_DC_PRED => {
            let s: u32 = a[..4].iter().chain(l.iter()).map(|&v| v as u32).sum();
            b = [[((s + 4) >> 3) as u8; 4]; 4];
        }
        B_TM_PRED => {
            for (r, row) in b.iter_mut().enumerate() {
                for (c, v) in row.iter_mut().enumerate() {
                    *v = (l[r] as i32 + a[c] as i32 - p as i32).clamp(0, 255) as u8;
                }
            }
        }
        B_VE_PRED => {
            let row = [avg3(p, a[0], a[1]), avg3(a[0], a[1], a[2]), avg3(a[1], a[2], a[3]), avg3(a[2], a[3], a[4])];
            b = [row; 4];
        }
        B_HE_PRED => {
            let col = [avg3(p, l[0], l[1]), avg3(l[0], l[1], l[2]), avg3(l[1], l[2], l[3]), avg3(l[2], l[3], l[3])];
            for (row, v) in b.iter_mut().zip(col) {
                *row = [v; 4];
            }
        }
        B_LD_PRED => {
            for (r, row) in b.iter_mut().enumerate() {
                for (c, v) in row.iter_mut().enumerate() {
                    let k = r + c;
                    *v = if k < 6 { avg3(a[k], a[k + 1], a[k + 2]) } else { avg3(a[6], a[7], a[7]) };
                }
            }
        }
        B_RD_PRED => {
            for (r, row) in b.iter_mut().enumerate() {
                for (c, v) in row.iter_mut().enumerate() {
                    let k = 4 + c - r;
                    *v = avg3(e[k - 1], e[k], e[k + 1]);
                }
            }
        }
        B_VR_PRED => {
            b[3][0] = avg3(e[1], e[2], e[3]);
            b[2][0] = avg3(e[2], e[3], e[4]);
            b[3][1] = avg3(e[3], e[4], e[5]);
            b[1][0] = b[3][1];
            b[2][1] = avg2(e[4], e[5]);
            b[0][0] = b[2][1];
            b[3][2] = avg3(e[4], e[5], e[6]);
            b[1][1] = b[3][2];
            b[2][2] = avg2(e[5], e[6]);
            b[0][1] = b[2][2];
            b[3][3] = avg3(e[5], e[6], e[7]);
            b[1][2] = b[3][3];
            b[2][3] = avg2(e[6], e[7]);
            b[0][2] = b[2][3];
            b[1][3] = avg3(e[6], e[7], e[8]);
            b[0][3] = avg2(e[7], e[8]);
        }
        B_VL_PRED => {
            b[0][0] = avg2(a[0], a[1]);
            b[1][0] = avg3(a[0], a[1], a[2]);
            b[2][0] = avg2(a[1], a[2]);
            b[0][1] = b[2][0];
            b[1][1] = avg3(a[1], a[2], a[3]);
            b[3][0] = b[1][1];
            b[2][1] = avg2(a[2], a[3]);
            b[0][2] = b[2][1];
            b[3][1] = avg3(a[2], a[3], a[4]);
            b[1][2] = b[3][1];
            b[2][2] = avg2(a[3], a[4]);
            b[0][3] = b[2][2];
            b[3][2] = avg3(a[3], a[4], a[5]);
            b[1][3] = b[3][2];
            // the last two break the pattern
            b[2][3] = avg3(a[4], a[5], a[6]);
            b[3][3] = avg3(a[5], a[6], a[7]);
        }
        B_HD_PRED => {
            b[3][0] = avg2(e[0], e[1]);
            b[3][1] = avg3(e[0], e[1], e[2]);
            b[2][0] = avg2(e[1], e[2]);
            b[3][2] = b[2][0];
            b[2][1] = avg3(e[1], e[2], e[3]);
            b[3][3] = b[2][1];
            b[2][2] = avg2(e[2], e[3]);
            b[1][0] = b[2][2];
            b[2][3] = avg3(e[2], e[3], e[4]);
            b[1][1] = b[2][3];
            b[1][2] = avg2(e[3], e[4]);
            b[0][0] = b[1][2];
            b[1][3] = avg3(e[3], e[4], e[5]);
            b[0][1] = b[1][3];
            b[0][2] = avg3(e[4], e[5], e[6]);
            b[0][3] = avg3(e[5], e[6], e[7]);
        }
        _ => {
            // B_HU_PRED
            b[0][0] = avg2(l[0], l[1]);
            b[0][1] = avg3(l[0], l[1], l[2]);
            b[0][2] = avg2(l[1], l[2]);
            b[1][0] = b[0][2];
            b[0][3] = avg3(l[1], l[2], l[3]);
            b[1][1] = b[0][3];
            b[1][2] = avg2(l[2], l[3]);
            b[2][0] = b[1][2];
            b[1][3] = avg3(l[2], l[3], l[3]);
            b[2][1] = b[1][3];
            b[2][2] = l[3];
            b[2][3] = l[3];
            b[3] = [l[3]; 4];
        }
    }
    for (r, row) in b.iter().enumerate() {
        dst[r * stride..r * stride + 4].copy_from_slice(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_uses_the_edges_it_has() {
        let above: Vec<u8> = (0..17).map(|i| i as u8 * 2).collect();
        let left: Vec<u8> = (0..16).map(|i| 100 + i as u8).collect();
        let mut dst = vec![0u8; 16 * 16];
        predict_block::<16>(DC_PRED, &above, &left, true, true, &mut dst, 16);
        let want = (above[1..].iter().map(|&v| v as u32).sum::<u32>() + left.iter().map(|&v| v as u32).sum::<u32>() + 16) >> 5;
        assert!(dst.iter().all(|&v| v as u32 == want));
        predict_block::<16>(DC_PRED, &above, &left, false, true, &mut dst, 16);
        assert!(dst.iter().all(|&v| v as u32 == (left.iter().map(|&v| v as u32).sum::<u32>() + 8) >> 4));
        predict_block::<8>(DC_PRED, &above, &left, false, false, &mut dst, 8);
        assert!(dst[..64].iter().all(|&v| v == 128));
    }

    #[test]
    fn true_motion_clamps() {
        let above = [200u8, 250, 10, 128, 60, 0, 0, 0, 0];
        let left = [255u8, 0, 128, 90];
        let mut dst = [0u8; 16];
        predict_subblock(B_TM_PRED, &above, &left, &mut dst, 4);
        assert_eq!(&dst[..4], &[255, 65, 183, 115]);
        assert_eq!(&dst[4..8], &[50, 0, 0, 0]);
    }
}
