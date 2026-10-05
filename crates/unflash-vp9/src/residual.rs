//! The residual of a block (6.4.21 - 6.4.26): its prediction, then for
//! each transform block the coefficient tokens, their dequantisation and
//! the reconstruction.

use crate::frame::Pixel;
use crate::idct::{reconstruct, DCT_DCT};
use crate::tables::*;
use crate::tile::{Block, TileDecoder};

impl<'a, 'd, T: Pixel> TileDecoder<'a, 'd, T> {
    /// residual (6.4.21).
    pub fn residual(&mut self, b: &mut Block) {
        let bsize = b.size.max(BLOCK_8X8);
        for plane in 0..3 {
            let (tx_size, plane_size, sub) = if plane == 0 {
                (b.tx_size, bsize, 0)
            } else {
                let uv = UV_BLOCK_SIZE[bsize] as usize;
                let tx = if b.size < BLOCK_8X8 { 0 } else { b.tx_size.min(MAX_TX_SIZE[uv] as usize) };
                (tx, uv, 1)
            };
            let step = 1usize << tx_size;
            let (w4, h4) = (NUM_4X4_WIDE[plane_size] as usize, NUM_4X4_HIGH[plane_size] as usize);
            let base_x = (b.col * 8) >> sub;
            let base_y = (b.row * 8) >> sub;
            if b.is_inter {
                if b.size < BLOCK_8X8 {
                    for y in 0..h4 {
                        for x in 0..w4 {
                            self.predict_inter(b, plane, base_x + 4 * x, base_y + 4 * y, 4, 4, y * w4 + x);
                        }
                    }
                } else {
                    self.predict_inter(b, plane, base_x, base_y, w4 * 4, h4 * 4, 0);
                }
            }
            let max_x = (self.mi_cols * 8) >> sub;
            let max_y = (self.mi_rows * 8) >> sub;
            let mut block_idx = 0;
            for y in (0..h4).step_by(step) {
                for x in (0..w4).step_by(step) {
                    let start_x = base_x + 4 * x;
                    let start_y = base_y + 4 * y;
                    let mut nonzero = false;
                    if start_x < max_x && start_y < max_y {
                        let mode = if plane > 0 {
                            b.uv_mode
                        } else if b.size >= BLOCK_8X8 {
                            b.y_mode
                        } else {
                            b.sub_modes[block_idx]
                        };
                        if !b.is_inter {
                            let have_left = b.avail_l || x > 0;
                            let have_above = b.avail_u || y > 0;
                            self.predict_intra(plane, start_x, start_y, have_left, have_above, x + step < w4, tx_size, mode);
                        }
                        if !b.skip {
                            let tx_type = if plane > 0 || tx_size == 3 || self.fh.lossless || b.is_inter { DCT_DCT } else { MODE2TXFM[mode as usize] };
                            let (eob, rows) = self.tokens(b, plane, start_x, start_y, tx_size, tx_type);
                            if eob > 0 {
                                nonzero = true;
                                b.eob_total += 1;
                                let p = &mut self.cur.planes[plane];
                                let off = start_y * p.stride + start_x;
                                reconstruct(&mut self.scratch.coef, tx_size, tx_type, self.fh.lossless, eob, rows, &mut p.data[off..], p.stride, self.bit_depth);
                            }
                        }
                    }
                    let ax = start_x >> 2;
                    let ly = (start_y >> 2) & 15;
                    self.above.nonzero[plane][ax..ax + step].fill(nonzero as u8);
                    for i in 0..step {
                        self.left.nonzero[plane][(ly + i) & 15] = nonzero as u8;
                    }
                    block_idx += 1;
                }
            }
        }
    }

    /// tokens (6.4.24): read the coefficients of one transform block into
    /// `scratch.coef` (dequantised, raster order). Returns the number of
    /// scan positions read, and how many leading rows hold coefficients.
    fn tokens(&mut self, b: &Block, plane: usize, start_x: usize, start_y: usize, tx_size: usize, tx_type: u8) -> (usize, usize) {
        let ptype = (plane > 0) as usize;
        let sub = ptype;
        // the context of the first coefficient: whether the transform
        // blocks above and to the left, inside the frame, had any
        let max_x4 = (2 * self.mi_cols) >> sub;
        let max_y4 = (2 * self.mi_rows) >> sub;
        let (x4, y4) = (start_x >> 2, start_y >> 2);
        let n = 1usize << tx_size;
        let mut above = 0;
        let mut left = 0;
        for i in 0..n {
            if x4 + i < max_x4 {
                above |= self.above.nonzero[plane][x4 + i];
            }
            if y4 + i < max_y4 {
                left |= self.left.nonzero[plane][(y4 + i) & 15];
            }
        }
        let mut ctx = (above + left) as usize;

        let seg_eob = 16usize << (tx_size << 1);
        let scan = &SCANS[tx_size][tx_type as usize];
        let bands: &[u8] = if tx_size == 0 { &COEFBAND_4X4 } else { &COEFBAND_8X8PLUS };
        let probs = &self.fc.coef[tx_size][ptype][b.is_inter as usize];
        let dq = self.dequant[b.segment_id as usize & 7][ptype];
        let dq_shift = (tx_size == 3) as u32;
        let log2w = tx_size + 2;
        let bd = &mut self.bd;
        let coef = &mut self.scratch.coef;
        let cache = &mut self.scratch.token_cache;
        let mut counts = self.counts.as_deref_mut().map(|c| (&mut c.coef[tx_size][ptype][b.is_inter as usize], &mut c.eob_branch[tx_size][ptype][b.is_inter as usize]));
        let cat6_skip = match self.bit_depth {
            12 => 0,
            10 => 2,
            _ => 4,
        };
        let mut max_row = 0;
        let mut c = 0;
        let mut check_eob = true;
        while c < seg_eob {
            let pos = scan.scan[c] as usize;
            let band = bands[c] as usize;
            let p = &probs[band][ctx];
            if check_eob {
                if let Some((_, eob)) = counts.as_mut() {
                    eob[band][ctx] += 1;
                }
                if !bd.read(p[0]) {
                    if let Some((cc, _)) = counts.as_mut() {
                        cc[band][ctx][3] += 1;
                    }
                    break;
                }
            }
            if !bd.read(p[1]) {
                if let Some((cc, _)) = counts.as_mut() {
                    cc[band][ctx][0] += 1;
                }
                cache[pos] = 0;
                check_eob = false;
            } else {
                check_eob = true;
                let (val, energy) = if !bd.read(p[2]) {
                    if let Some((cc, _)) = counts.as_mut() {
                        cc[band][ctx][1] += 1;
                    }
                    (1, 1)
                } else {
                    if let Some((cc, _)) = counts.as_mut() {
                        cc[band][ctx][2] += 1;
                    }
                    let pp = &PARETO_FULL[p[2] as usize];
                    if !bd.read(pp[0]) {
                        if !bd.read(pp[1]) {
                            (2, 2)
                        } else if !bd.read(pp[2]) {
                            (3, 3)
                        } else {
                            (4, 3)
                        }
                    } else if !bd.read(pp[3]) {
                        if !bd.read(pp[4]) {
                            (5 + read_extra(bd, &CAT1_PROBS), 4)
                        } else {
                            (7 + read_extra(bd, &CAT2_PROBS), 4)
                        }
                    } else if !bd.read(pp[5]) {
                        if !bd.read(pp[6]) {
                            (11 + read_extra(bd, &CAT3_PROBS), 5)
                        } else {
                            (19 + read_extra(bd, &CAT4_PROBS), 5)
                        }
                    } else if !bd.read(pp[7]) {
                        (35 + read_extra(bd, &CAT5_PROBS), 5)
                    } else {
                        (67 + read_extra(bd, &CAT6_PROBS[cat6_skip..]), 5)
                    }
                };
                cache[pos] = energy;
                let q = if c == 0 { dq[0] } else { dq[1] };
                let v = ((val as i64 * q as i64) >> dq_shift) as i32;
                coef[pos] = if bd.read_bit() { v.wrapping_neg() } else { v };
                max_row = max_row.max(pos >> log2w);
            }
            c += 1;
            if c < seg_eob {
                let nb = scan.neighbours[c];
                ctx = ((1 + cache[nb[0] as usize] + cache[nb[1] as usize]) >> 1) as usize;
            }
        }
        (c, max_row + 1)
    }
}

/// The extra bits of a DCT_VAL_CATEGORY token, most significant first.
#[inline]
fn read_extra(bd: &mut crate::booldec::BoolDecoder, probs: &[u8]) -> i32 {
    let mut v = 0;
    for &p in probs {
        v = (v << 1) | bd.read(p) as i32;
    }
    v
}
