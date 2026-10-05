//! Intra prediction (8.5.1): one transform block at a time, from the
//! reconstructed (not yet loop filtered) samples above and to the left.

use crate::frame::Pixel;
use crate::tile::TileDecoder;

pub const V_PRED: u8 = 1;
pub const H_PRED: u8 = 2;
pub const D45_PRED: u8 = 3;
pub const D135_PRED: u8 = 4;
pub const D117_PRED: u8 = 5;
pub const D153_PRED: u8 = 6;
pub const D207_PRED: u8 = 7;
pub const D63_PRED: u8 = 8;
pub const TM_PRED: u8 = 9;

#[inline(always)]
fn avg2(a: i32, b: i32) -> i32 {
    (a + b + 1) >> 1
}

#[inline(always)]
fn avg3(a: i32, b: i32, c: i32) -> i32 {
    (a + 2 * b + c + 2) >> 2
}

impl<'a, 'd, T: Pixel> TileDecoder<'a, 'd, T> {
    /// Predict the transform block of `4 << tx_size` samples at (`x`, `y`)
    /// of `plane`. `not_on_right`: the block is not in the rightmost column
    /// of transform blocks of its block, so for 4x4 blocks the samples above
    /// and to the right are already decoded.
    #[allow(clippy::too_many_arguments)]
    pub fn predict_intra(&mut self, plane: usize, x: usize, y: usize, have_left: bool, have_above: bool, not_on_right: bool, tx_size: usize, mode: u8) {
        let size = 4usize << tx_size;
        let bd = self.bit_depth;
        let sub = (plane > 0) as usize;
        let max_x = ((self.mi_cols * 8) >> sub) - 1;
        let max_y = ((self.mi_rows * 8) >> sub) - 1;
        let p = &mut self.cur.planes[plane];
        let stride = p.stride;
        let data = &mut p.data;
        let base = 1i32 << (bd - 1);

        // edge[0] is aboveRow[-1], edge[1 + i] is aboveRow[i]
        let mut edge = [0i32; 65];
        let mut left = [0i32; 32];
        if have_above {
            let row = (y - 1) * stride;
            for i in 0..size {
                edge[1 + i] = data[row + (x + i).min(max_x)].get();
            }
            if not_on_right && tx_size == 0 {
                for i in size..2 * size {
                    edge[1 + i] = data[row + (x + i).min(max_x)].get();
                }
            } else {
                let last = edge[size];
                edge[1 + size..1 + 2 * size].fill(last);
            }
            edge[0] = if have_left { data[row + x - 1].get() } else { base + 1 };
        } else {
            edge[..1 + 2 * size].fill(base - 1);
        }
        if have_left {
            for (i, l) in left[..size].iter_mut().enumerate() {
                *l = data[(y + i).min(max_y) * stride + x - 1].get();
            }
        } else {
            left[..size].fill(base + 1);
        }
        let above = &edge[1..];
        let corner = edge[0];

        let dst = &mut data[y * stride..];
        let mut put = |i: usize, j: usize, v: i32| dst[i * stride + x + j] = T::new(v);
        match mode {
            V_PRED => {
                for i in 0..size {
                    for (j, &a) in above[..size].iter().enumerate() {
                        put(i, j, a);
                    }
                }
            }
            H_PRED => {
                for (i, &l) in left[..size].iter().enumerate() {
                    for j in 0..size {
                        put(i, j, l);
                    }
                }
            }
            D207_PRED => {
                let mut pred = [[0i32; 32]; 32];
                pred[size - 1][..size].fill(left[size - 1]);
                for i in 0..size - 1 {
                    pred[i][0] = avg2(left[i], left[i + 1]);
                }
                for i in 0..size.saturating_sub(2) {
                    pred[i][1] = avg3(left[i], left[i + 1], left[i + 2]);
                }
                pred[size - 2][1] = (left[size - 2] + 3 * left[size - 1] + 2) >> 2;
                for j in 2..size {
                    for i in (0..size - 1).rev() {
                        pred[i][j] = pred[i + 1][j - 2];
                    }
                }
                for (i, row) in pred.iter().enumerate().take(size) {
                    for (j, &v) in row.iter().enumerate().take(size) {
                        put(i, j, v);
                    }
                }
            }
            D45_PRED => {
                for i in 0..size {
                    for j in 0..size {
                        let k = i + j;
                        put(i, j, if k + 2 < 2 * size { avg3(above[k], above[k + 1], above[k + 2]) } else { above[2 * size - 1] });
                    }
                }
            }
            D63_PRED => {
                for i in 0..size {
                    for j in 0..size {
                        let k = i / 2 + j;
                        put(i, j, if i & 1 != 0 { avg3(above[k], above[k + 1], above[k + 2]) } else { avg2(above[k], above[k + 1]) });
                    }
                }
            }
            D117_PRED => {
                let mut pred = [[0i32; 32]; 32];
                pred[0][0] = avg2(corner, above[0]);
                for j in 1..size {
                    pred[0][j] = avg2(above[j - 1], above[j]);
                }
                pred[1][0] = avg3(left[0], corner, above[0]);
                pred[1][1] = avg3(corner, above[0], above[1]);
                for j in 2..size {
                    pred[1][j] = avg3(above[j - 2], above[j - 1], above[j]);
                }
                pred[2][0] = avg3(corner, left[0], left[1]);
                for i in 3..size {
                    pred[i][0] = avg3(left[i - 3], left[i - 2], left[i - 1]);
                }
                for i in 2..size {
                    for j in 1..size {
                        pred[i][j] = pred[i - 2][j - 1];
                    }
                }
                for (i, row) in pred.iter().enumerate().take(size) {
                    for (j, &v) in row.iter().enumerate().take(size) {
                        put(i, j, v);
                    }
                }
            }
            D135_PRED => {
                let mut pred = [[0i32; 32]; 32];
                pred[0][0] = avg3(left[0], corner, above[0]);
                pred[0][1] = avg3(corner, above[0], above[1]);
                for j in 2..size {
                    pred[0][j] = avg3(above[j - 2], above[j - 1], above[j]);
                }
                pred[1][0] = avg3(corner, left[0], left[1]);
                for i in 2..size {
                    pred[i][0] = avg3(left[i - 2], left[i - 1], left[i]);
                }
                for i in 1..size {
                    for j in 1..size {
                        pred[i][j] = pred[i - 1][j - 1];
                    }
                }
                for (i, row) in pred.iter().enumerate().take(size) {
                    for (j, &v) in row.iter().enumerate().take(size) {
                        put(i, j, v);
                    }
                }
            }
            D153_PRED => {
                let mut pred = [[0i32; 32]; 32];
                pred[0][0] = avg2(left[0], corner);
                for i in 1..size {
                    pred[i][0] = avg2(left[i - 1], left[i]);
                }
                pred[0][1] = avg3(left[0], corner, above[0]);
                pred[1][1] = avg3(corner, left[0], left[1]);
                for i in 2..size {
                    pred[i][1] = avg3(left[i - 2], left[i - 1], left[i]);
                }
                pred[0][2] = avg3(corner, above[0], above[1]);
                for j in 3..size {
                    pred[0][j] = avg3(above[j - 3], above[j - 2], above[j - 1]);
                }
                for i in 1..size {
                    for j in 2..size {
                        pred[i][j] = pred[i - 1][j - 2];
                    }
                }
                for (i, row) in pred.iter().enumerate().take(size) {
                    for (j, &v) in row.iter().enumerate().take(size) {
                        put(i, j, v);
                    }
                }
            }
            TM_PRED => {
                for i in 0..size {
                    for j in 0..size {
                        dst[i * stride + x + j] = T::clip(above[j] + left[i] - corner, bd);
                    }
                }
            }
            _ => {
                // DC_PRED
                let log2 = tx_size + 2;
                let v = match (have_left, have_above) {
                    (true, true) => {
                        let sum: i32 = left[..size].iter().sum::<i32>() + above[..size].iter().sum::<i32>();
                        (sum + size as i32) >> (log2 + 1)
                    }
                    (true, false) => (left[..size].iter().sum::<i32>() + (1 << (log2 - 1))) >> log2,
                    (false, true) => (above[..size].iter().sum::<i32>() + (1 << (log2 - 1))) >> log2,
                    (false, false) => base,
                };
                for i in 0..size {
                    for j in 0..size {
                        put(i, j, v);
                    }
                }
            }
        }
    }
}
