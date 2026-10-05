//! Residual coding (7.3.8.11) with its context selection (9.3.4.2.3 –
//! 9.3.4.2.7), and the scaling of the coefficients (8.6.3) as they are
//! decoded.

use crate::cabac::ctx;
use crate::ctu::SliceDecoder;
use crate::picture::Sample;
use crate::tables::{Scan, DIAG2, DIAG4, DIAG8, HOR2, SCAN4X4, SCAN4X4_POS, VER2};
use crate::transform::LEVEL_SCALE;
use crate::{Error, Result};

/// ctxIdxMap (Table 9-50) for sig_coeff_flag in 4x4 blocks.
const CTX_IDX_MAP: [u8; 16] = [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8];

/// The sig_coeff_flag context increment of a position (y * 4 + x) in a
/// sub-block of a larger block by prevCsbf (9.3.4.2.5): 2, 1 or 0 by how
/// close the position is to the coded neighbouring sub-blocks.
const SIG_PATTERN: [[u8; 16]; 4] = {
    let mut t = [[0u8; 16]; 4];
    let mut i = 0;
    while i < 16 {
        let (x, y) = (i % 4, i / 4);
        t[0][i] = if x + y == 0 { 2 } else if x + y <= 2 { 1 } else { 0 };
        t[1][i] = if y == 0 { 2 } else if y == 1 { 1 } else { 0 };
        t[2][i] = if x == 0 { 2 } else if x == 1 { 1 } else { 0 };
        t[3][i] = 2;
        i += 1;
    }
    t
};

/// What residual coding found in a block.
pub struct Coded {
    pub transform_skip: bool,
    /// The largest column and row holding a non-zero coefficient.
    pub max_x: usize,
    pub max_y: usize,
}

/// The sub-block scan of a block with `1 << log2_sb` sub-blocks per side.
fn sub_block_scan(log2_sb: usize, scan_idx: usize) -> &'static [(u8, u8)] {
    const ONE: Scan<1> = [(0, 0)];
    match (log2_sb, scan_idx) {
        (0, _) => &ONE,
        (1, 1) => &HOR2,
        (1, 2) => &VER2,
        (1, _) => &DIAG2,
        (2, _) => &DIAG4,
        _ => &DIAG8,
    }
}

impl<'a, P: Sample> SliceDecoder<'a, P> {
    /// Parse the residual of a `1 << log2` block of component `c` into
    /// `self.coeffs` (row-major), scaled with `qp` unless the coding unit
    /// bypasses transform and quantisation.
    pub(crate) fn residual_coding(&mut self, log2: usize, c: usize, scan_idx: usize, qp: i32, bit_depth: u32) -> Result<Coded> {
        let n = 1usize << log2;
        self.coeffs[..n * n].fill(0);
        let bypass = self.cu_bypass();
        let transform_skip = self.pps.transform_skip_enabled && !bypass && log2 <= self.pps.log2_max_transform_skip_size as usize && self.cabac.decision(ctx::TRANSFORM_SKIP + (c > 0) as usize) != 0;
        // last significant coefficient (9.3.4.2.3)
        let (off, shift) = if c == 0 { (3 * (log2 - 2) + ((log2 - 1) >> 2), (log2 + 1) >> 2) } else { (15, log2 - 2) };
        let max_prefix = (log2 << 1) - 1;
        let mut prefix = [0usize; 2];
        for (k, base) in [ctx::LAST_X_PREFIX, ctx::LAST_Y_PREFIX].into_iter().enumerate() {
            while prefix[k] < max_prefix && self.cabac.decision(base + off + (prefix[k] >> shift)) != 0 {
                prefix[k] += 1;
            }
        }
        let mut last = [0usize; 2];
        for k in 0..2 {
            last[k] = if prefix[k] > 3 {
                let bits = (prefix[k] >> 1) - 1;
                (1 << bits) * (2 + (prefix[k] & 1)) + self.cabac.bypass_bits(bits as u32) as usize
            } else {
                prefix[k]
            };
        }
        if scan_idx == 2 {
            last.swap(0, 1);
        }
        let (last_x, last_y) = (last[0], last[1]);
        if last_x >= n || last_y >= n {
            return Err(Error::Bitstream("last significant coefficient outside the block"));
        }
        // scaling (8.6.3): m * levelScale << (qP / 6), rounded by bdShift
        let flat = self.layout.scaling.is_none() || (transform_skip && n > 4);
        let matrix = if self.cu_intra() { c } else { 3 + c };
        let factors: &[u8] = match &self.layout.scaling {
            Some(f) if !flat => f.get(log2, matrix),
            _ => &[],
        };
        let level_scale = (LEVEL_SCALE[(qp % 6) as usize] as i64) << (qp / 6);
        let bd_shift = bit_depth as i64 + log2 as i64 - 5;
        let round = 1i64 << (bd_shift - 1);

        let log2_sb = log2 - 2;
        let nsb = 1usize << log2_sb;
        let sb_scan = sub_block_scan(log2_sb, scan_idx);
        let pos_scan = &SCAN4X4[scan_idx];
        let last_sb = sb_scan.iter().position(|&(x, y)| x as usize == last_x >> 2 && y as usize == last_y >> 2).unwrap_or(0);
        let last_pos = SCAN4X4_POS[scan_idx][(last_y & 3) * 4 + (last_x & 3)] as usize;
        let sign_hiding = self.pps.sign_data_hiding && !bypass;
        let chroma_off = if c > 0 { 27 } else { 0 };
        // coded_sub_block_flag per sub-block, bit ys * 8 + xs
        let mut csbf: u64 = 0;
        let mut greater1_ctx = 1u32;
        let (mut max_x, mut max_y) = (0usize, 0usize);
        for i in (0..=last_sb).rev() {
            let (xs, ys) = (sb_scan[i].0 as usize, sb_scan[i].1 as usize);
            let right = xs + 1 < nsb && csbf >> (ys * 8 + xs + 1) & 1 != 0;
            let below = ys + 1 < nsb && csbf >> ((ys + 1) * 8 + xs) & 1 != 0;
            let mut infer_dc = false;
            let coded = if i < last_sb && i > 0 {
                let inc = ((right || below) as usize) + if c > 0 { 2 } else { 0 };
                infer_dc = true;
                self.cabac.decision(ctx::CODED_SUB_BLOCK + inc) != 0
            } else {
                true
            };
            if !coded {
                continue;
            }
            csbf |= 1 << (ys * 8 + xs);
            // significant coefficients, as scan positions in decoding order
            let mut sig = [0u8; 16];
            let mut nsig = 0;
            let first = if i == last_sb {
                sig[0] = last_pos as u8;
                nsig = 1;
                last_pos as isize - 1
            } else {
                15
            };
            let pattern = &SIG_PATTERN[right as usize | (below as usize) << 1];
            // the sub-block's offset into the sig_coeff_flag contexts
            let sig_offset = match (c, log2, scan_idx) {
                (0, 3, 0) => 9,
                (0, 3, _) => 15,
                (0, _, _) => 21,
                (_, 3, _) => 9,
                _ => 12,
            } + if c == 0 && xs + ys > 0 { 3 } else { 0 };
            let mut pos = first;
            while pos >= 0 {
                let np = pos as usize;
                let (xp, yp) = (pos_scan[np].0 as usize, pos_scan[np].1 as usize);
                if np > 0 || !infer_dc {
                    let sig_ctx = if log2 == 2 {
                        CTX_IDX_MAP[(yp << 2) + xp] as usize
                    } else if xs + ys + xp + yp == 0 {
                        0
                    } else {
                        pattern[(yp << 2) + xp] as usize + sig_offset
                    };
                    if self.cabac.decision(ctx::SIG_COEFF + chroma_off + sig_ctx) != 0 {
                        sig[nsig] = np as u8;
                        nsig += 1;
                        infer_dc = false;
                    }
                } else {
                    // the DC of a coded sub-block whose other flags were all 0
                    sig[nsig] = 0;
                    nsig += 1;
                }
                pos -= 1;
            }
            if nsig == 0 {
                continue;
            }
            // coeff_abs_level_greater1_flag (9.3.4.2.6)
            let mut ctx_set = if i == 0 || c > 0 { 0 } else { 2 };
            if greater1_ctx == 0 {
                ctx_set += 1;
            }
            greater1_ctx = 1;
            let mut gt1 = [false; 16];
            let mut first_gt1 = 16;
            let g1_base = ctx::GREATER1 + if c > 0 { 16 } else { 0 };
            for (k, g) in gt1.iter_mut().enumerate().take(nsig.min(8)) {
                let f = self.cabac.decision(g1_base + ctx_set * 4 + greater1_ctx.min(3) as usize) != 0;
                *g = f;
                if greater1_ctx > 0 {
                    greater1_ctx = if f { 0 } else { greater1_ctx + 1 };
                }
                if f && first_gt1 == 16 {
                    first_gt1 = k;
                }
            }
            let gt2 = first_gt1 < 16 && self.cabac.decision(ctx::GREATER2 + ctx_set + if c > 0 { 4 } else { 0 }) != 0;
            // coeff_sign_flag, first in scan order first
            let hidden = sign_hiding && sig[0] as isize - sig[nsig - 1] as isize > 3;
            let nsigns = if hidden { nsig - 1 } else { nsig };
            let signs = self.cabac.bypass_bits(nsigns as u32) << (32 - nsigns);
            // coeff_abs_level_remaining and the levels
            let mut rice = 0u32;
            let mut sum_abs: i64 = 0;
            for k in 0..nsig {
                let base = 1 + gt1[k] as i64 + (k == first_gt1 && gt2) as i64;
                let threshold = if k < 8 {
                    if k == first_gt1 {
                        3
                    } else {
                        2
                    }
                } else {
                    1
                };
                let mut abs = base;
                if base == threshold {
                    abs += self.coeff_abs_level_remaining(rice)? as i64;
                    if abs > 3 * (1 << rice) {
                        rice = (rice + 1).min(4);
                    }
                }
                let mut level = if k < nsigns && (signs << k) & 0x8000_0000 != 0 { -abs } else { abs };
                if hidden {
                    sum_abs += abs;
                    if k == nsig - 1 && sum_abs & 1 == 1 {
                        level = -level;
                    }
                }
                let np = sig[k] as usize;
                let (xc, yc) = ((xs << 2) + pos_scan[np].0 as usize, (ys << 2) + pos_scan[np].1 as usize);
                let level = level.clamp(-(1 << 24), 1 << 24);
                let value = if bypass {
                    level as i32
                } else {
                    let m = if flat { 16 } else { factors[yc * n + xc] as i64 };
                    ((level * m * level_scale + round) >> bd_shift).clamp(-32768, 32767) as i32
                };
                self.coeffs[yc * n + xc] = value;
                max_x = max_x.max(xc);
                max_y = max_y.max(yc);
            }
        }
        Ok(Coded { transform_skip, max_x, max_y })
    }

    /// coeff_abs_level_remaining (9.3.3.11): a Rice prefix of up to four
    /// ones, then an Exp-Golomb escape of order `rice` + 1.
    fn coeff_abs_level_remaining(&mut self, rice: u32) -> Result<u32> {
        let mut prefix = 0;
        while self.cabac.bypass() != 0 {
            prefix += 1;
            if prefix > 28 {
                return Err(Error::Bitstream("coeff_abs_level_remaining too long"));
            }
        }
        if prefix < 4 {
            Ok((prefix << rice) + self.cabac.bypass_bits(rice))
        } else {
            let bits = prefix - 3 + rice;
            Ok((((1 << (prefix - 3)) + 2) << rice) + self.cabac.bypass_bits(bits))
        }
    }
}
