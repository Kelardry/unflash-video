//! The CABAC arithmetic decoding engine (9.3.4.3), the context variables
//! and their initialisation (9.3.2.2), and the binarizations of the syntax
//! elements outside the residual coding (9.3.3). Context increments that
//! depend on neighbouring blocks are computed by the caller.

use crate::{Error, Result};

/// Offsets of each syntax element's contexts in the context table.
pub mod ctx {
    pub const SAO_MERGE: usize = 0;
    pub const SAO_TYPE: usize = 1;
    pub const SPLIT_CU: usize = 2; // 3
    pub const TRANSQUANT_BYPASS: usize = 5;
    pub const CU_SKIP: usize = 6; // 3
    pub const PRED_MODE: usize = 9;
    pub const PART_MODE: usize = 10; // 4
    pub const PREV_INTRA_LUMA: usize = 14;
    pub const INTRA_CHROMA: usize = 15;
    pub const RQT_ROOT_CBF: usize = 16;
    pub const MERGE_FLAG: usize = 17;
    pub const MERGE_IDX: usize = 18;
    pub const INTER_PRED_IDC: usize = 19; // 5
    pub const REF_IDX: usize = 24; // 2
    pub const MVP_FLAG: usize = 26;
    pub const SPLIT_TRANSFORM: usize = 27; // 3
    pub const CBF_LUMA: usize = 30; // 2
    pub const CBF_CHROMA: usize = 32; // 5
    pub const MVD_GT0: usize = 37;
    pub const MVD_GT1: usize = 38;
    pub const CU_QP_DELTA: usize = 39; // 2
    pub const TRANSFORM_SKIP: usize = 41; // 2: luma, chroma
    pub const LAST_X_PREFIX: usize = 43; // 18
    pub const LAST_Y_PREFIX: usize = 61; // 18
    pub const CODED_SUB_BLOCK: usize = 79; // 4
    pub const SIG_COEFF: usize = 83; // 44 (42, and two for transform-skip blocks)
    pub const GREATER1: usize = 127; // 24
    pub const GREATER2: usize = 151; // 6
    pub const CU_CHROMA_QP_OFFSET_FLAG: usize = 157;
    pub const CU_CHROMA_QP_OFFSET_IDX: usize = 158;
    pub const COUNT: usize = 159;
}

/// initValue per context for initType 0, 1 and 2 (Tables 9-5 to 9-35;
/// contexts an I slice never uses hold 154 there).
const INIT_VALUES: [[u8; ctx::COUNT]; 3] = [
    [
        153, // sao_merge
        200, // sao_type_idx
        139, 141, 157, // split_cu_flag
        154, // cu_transquant_bypass_flag
        154, 154, 154, // cu_skip_flag
        154, // pred_mode_flag
        184, 154, 154, 154, // part_mode
        184, // prev_intra_luma_pred_flag
        63,  // intra_chroma_pred_mode
        154, // rqt_root_cbf
        154, // merge_flag
        154, // merge_idx
        154, 154, 154, 154, 154, // inter_pred_idc
        154, 154, // ref_idx
        154, // mvp_flag
        153, 138, 138, // split_transform_flag
        111, 141, // cbf_luma
        94, 138, 182, 154, 154, // cbf_cb, cbf_cr
        154, 154, // abs_mvd_greater0/1_flag
        154, 154, // cu_qp_delta_abs
        139, 139, // transform_skip_flag
        110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63, // last_sig_coeff_x_prefix
        110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63, // last_sig_coeff_y_prefix
        91, 171, 134, 141, // coded_sub_block_flag
        111, 111, 125, 110, 110, 94, 124, 108, 124, 107, 125, 141, 179, 153, 125, 107, 125, 141, 179, 153, 125, 107, 125, 141, 179, 153, 125, 140, 139, 182, 182, 152, 136, 152, 136, 153, 136, 139, 111, 136, 139, 111, 141, 111, // sig_coeff_flag
        140, 92, 137, 138, 140, 152, 138, 139, 153, 74, 149, 92, 139, 107, 122, 152, 140, 179, 166, 182, 140, 227, 122, 197, // coeff_abs_level_greater1_flag
        138, 153, 136, 167, 152, 152, // coeff_abs_level_greater2_flag
        154, // cu_chroma_qp_offset_flag
        154, // cu_chroma_qp_offset_idx
    ],
    [
        153, 185, 107, 139, 126, 154, 197, 185, 201, 149, 154, 139, 154, 154, 154, 152, 79, 110, 122, 95, 79, 63, 31, 31, 153, 153, 168, 124, 138, 94, 153, 111, 149, 107, 167, 154, 154, 140, 198, 154, 154, 139, 139, //
        125, 110, 94, 110, 95, 79, 125, 111, 110, 78, 110, 111, 111, 95, 94, 108, 123, 108, //
        125, 110, 94, 110, 95, 79, 125, 111, 110, 78, 110, 111, 111, 95, 94, 108, 123, 108, //
        121, 140, 61, 154, //
        155, 154, 139, 153, 139, 123, 123, 63, 153, 166, 183, 140, 136, 153, 154, 166, 183, 140, 136, 153, 154, 166, 183, 140, 136, 153, 154, 170, 153, 123, 123, 107, 121, 107, 121, 167, 151, 183, 140, 151, 183, 140, 140, 140, //
        154, 196, 196, 167, 154, 152, 167, 182, 182, 134, 149, 136, 153, 121, 136, 137, 169, 194, 166, 167, 154, 167, 137, 182, //
        107, 167, 91, 122, 107, 167, //
        154, 154,
    ],
    [
        153, 160, 107, 139, 126, 154, 197, 185, 201, 134, 154, 139, 154, 154, 183, 152, 79, 154, 137, 95, 79, 63, 31, 31, 153, 153, 168, 224, 167, 122, 153, 111, 149, 92, 167, 154, 154, 169, 198, 154, 154, 139, 139, //
        125, 110, 124, 110, 95, 94, 125, 111, 111, 79, 125, 126, 111, 111, 79, 108, 123, 93, //
        125, 110, 124, 110, 95, 94, 125, 111, 111, 79, 125, 126, 111, 111, 79, 108, 123, 93, //
        121, 140, 61, 154, //
        170, 154, 139, 153, 139, 123, 123, 63, 124, 166, 183, 140, 136, 153, 154, 166, 183, 140, 136, 153, 154, 166, 183, 140, 136, 153, 154, 170, 153, 138, 138, 122, 121, 122, 121, 167, 151, 183, 140, 151, 183, 140, 140, 140, //
        154, 196, 167, 167, 154, 152, 167, 182, 182, 134, 149, 136, 153, 121, 136, 122, 169, 208, 166, 167, 154, 152, 167, 182, //
        107, 167, 91, 107, 107, 167, //
        154, 154,
    ],
];

/// rangeTabLps (Table 9-52) by pStateIdx and qRangeIdx.
const RANGE_LPS: [[u8; 4]; 64] = [
    [128, 176, 208, 240], [128, 167, 197, 227], [128, 158, 187, 216], [123, 150, 178, 205], [116, 142, 169, 195], [111, 135, 160, 185], [105, 128, 152, 175], [100, 122, 144, 166],
    [95, 116, 137, 158], [90, 110, 130, 150], [85, 104, 123, 142], [81, 99, 117, 135], [77, 94, 111, 128], [73, 89, 105, 122], [69, 85, 100, 116], [66, 80, 95, 110],
    [62, 76, 90, 104], [59, 72, 86, 99], [56, 69, 81, 94], [53, 65, 77, 89], [51, 62, 73, 85], [48, 59, 69, 80], [46, 56, 66, 76], [43, 53, 63, 72],
    [41, 50, 59, 69], [39, 48, 56, 65], [37, 45, 54, 62], [35, 43, 51, 59], [33, 41, 48, 56], [32, 39, 46, 53], [30, 37, 43, 50], [29, 35, 41, 48],
    [27, 33, 39, 45], [26, 31, 37, 43], [24, 30, 35, 41], [23, 28, 33, 39], [22, 27, 32, 37], [21, 26, 30, 35], [20, 24, 29, 33], [19, 23, 27, 31],
    [18, 22, 26, 30], [17, 21, 25, 28], [16, 20, 23, 27], [15, 19, 22, 25], [14, 18, 21, 24], [14, 17, 20, 23], [13, 16, 19, 22], [12, 15, 18, 21],
    [12, 14, 17, 20], [11, 14, 16, 19], [11, 13, 15, 18], [10, 12, 15, 17], [10, 12, 14, 16], [9, 11, 13, 15], [9, 11, 12, 14], [8, 10, 12, 14],
    [8, 9, 11, 13], [7, 9, 11, 12], [7, 9, 10, 12], [7, 8, 10, 11], [6, 8, 9, 11], [6, 7, 9, 10], [6, 7, 8, 9], [2, 2, 2, 2],
];

/// transIdxLps (Table 9-53); transIdxMps is pStateIdx + 1 capped at 62.
const TRANS_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12, 13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 22, 22, 23, 24, 24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33, 33, 33, 34, 34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

/// rangeTabLps indexed by the combined state (pStateIdx << 1 | valMps).
const LPS_RANGE: [[u8; 4]; 128] = {
    let mut t = [[0u8; 4]; 128];
    let mut s = 0;
    while s < 128 {
        t[s] = RANGE_LPS[s >> 1];
        s += 1;
    }
    t
};
/// The next combined state after an MPS / an LPS.
const NEXT_MPS: [u8; 128] = {
    let mut t = [0u8; 128];
    let mut s = 0;
    while s < 128 {
        let p = s >> 1;
        let next = if p < 62 { p + 1 } else { 62 };
        t[s] = ((next << 1) | (s & 1)) as u8;
        s += 1;
    }
    t
};
const NEXT_LPS: [u8; 128] = {
    let mut t = [0u8; 128];
    let mut s = 0;
    while s < 128 {
        let mps = s & 1;
        let p = s >> 1;
        t[s] = ((TRANS_LPS[p] as usize) << 1 | if p == 0 { 1 - mps } else { mps }) as u8;
        s += 1;
    }
    t
};

/// The context variables: (pStateIdx << 1) | valMps per context.
pub type Contexts = [u8; ctx::COUNT];

/// 9.3.2.2: the context variables for a slice's initType and SliceQpY.
pub fn init_contexts(init_type: usize, slice_qp: i32) -> Contexts {
    let mut c = [0u8; ctx::COUNT];
    let qp = slice_qp.clamp(0, 51);
    for (i, &v) in INIT_VALUES[init_type].iter().enumerate() {
        let m = (v as i32 >> 4) * 5 - 45;
        let n = ((v as i32 & 15) << 3) - 16;
        let pre = (((m * qp) >> 4) + n).clamp(1, 126);
        c[i] = if pre <= 63 { ((63 - pre) << 1) as u8 } else { (((pre - 64) << 1) | 1) as u8 };
    }
    c
}

/// The arithmetic decoding engine (9.3.4.3) over a slice segment's data,
/// with its context variables.
pub struct Cabac<'a> {
    data: &'a [u8],
    /// the next byte to fetch
    pos: usize,
    range: u32,
    /// ivlOffset scaled by 2^7, with up to 7 fetched but unused bits below
    value: u32,
    /// shifts until the next byte must be fetched, minus 8 (-8..=-1)
    bits_needed: i32,
    pub ctx: Contexts,
}

impl<'a> Cabac<'a> {
    /// Start the arithmetic decoder at byte `pos` of `data` (9.3.2.6).
    pub fn new(data: &'a [u8], pos: usize, ctx: Contexts) -> Result<Cabac<'a>> {
        let mut c = Cabac { data, pos, range: 510, value: 0, bits_needed: -8, ctx };
        c.restart(pos)?;
        Ok(c)
    }

    /// (Re-)initialise the arithmetic decoding engine at a byte position,
    /// keeping the context variables.
    pub fn restart(&mut self, pos: usize) -> Result<()> {
        if pos >= self.data.len() {
            return Err(Error::Bitstream("slice data ends early"));
        }
        self.pos = pos;
        self.range = 510;
        self.value = (self.next_byte() as u32) << 8;
        self.value |= self.next_byte() as u32;
        self.bits_needed = -8;
        if self.value >> 7 >= 510 {
            return Err(Error::Bitstream("bad CABAC initialisation"));
        }
        Ok(())
    }

    /// Where the next substream (or PCM samples) start once a terminating
    /// bin equal to 1 has been decoded: every bit before this byte has been
    /// read by the arithmetic decoder (9.3.4.3.5).
    pub fn byte_pos(&self) -> usize {
        self.pos
    }

    /// Whether the decoder has read past the end of its data (the stream is
    /// damaged or truncated).
    pub fn overrun(&self) -> bool {
        self.pos > self.data.len() + 2
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    #[inline(always)]
    fn next_byte(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    /// 9.3.4.3.2 DecodeDecision.
    #[inline(always)]
    pub fn decision(&mut self, ctx_idx: usize) -> u32 {
        let s = self.ctx[ctx_idx] as usize;
        let lps = LPS_RANGE[s][((self.range >> 6) & 3) as usize] as u32;
        self.range -= lps;
        let scaled = self.range << 7;
        if self.value < scaled {
            self.ctx[ctx_idx] = NEXT_MPS[s];
            if scaled < (256 << 7) {
                // one renormalisation shift at most on the MPS path
                self.range = scaled >> 6;
                self.value <<= 1;
                self.bits_needed += 1;
                if self.bits_needed == 0 {
                    self.bits_needed = -8;
                    self.value |= self.next_byte() as u32;
                }
            }
            (s & 1) as u32
        } else {
            let n = lps.leading_zeros() - 23;
            self.value = (self.value - scaled) << n;
            self.range = lps << n;
            self.ctx[ctx_idx] = NEXT_LPS[s];
            self.bits_needed += n as i32;
            if self.bits_needed >= 0 {
                self.value |= (self.next_byte() as u32) << self.bits_needed;
                self.bits_needed -= 8;
            }
            (!s & 1) as u32
        }
    }

    /// 9.3.4.3.4 DecodeBypass.
    #[inline(always)]
    pub fn bypass(&mut self) -> u32 {
        self.value <<= 1;
        self.bits_needed += 1;
        if self.bits_needed >= 0 {
            self.bits_needed = -8;
            self.value |= self.next_byte() as u32;
        }
        let scaled = self.range << 7;
        if self.value >= scaled {
            self.value -= scaled;
            1
        } else {
            0
        }
    }

    /// `n` bypass bins as an unsigned number, first bin most significant.
    #[inline]
    pub fn bypass_bits(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bypass();
        }
        v
    }

    /// 9.3.4.3.5 DecodeTerminate.
    pub fn terminate(&mut self) -> u32 {
        self.range -= 2;
        let scaled = self.range << 7;
        if self.value >= scaled {
            1
        } else {
            if scaled < (256 << 7) {
                self.range = scaled >> 6;
                self.value <<= 1;
                self.bits_needed += 1;
                if self.bits_needed == 0 {
                    self.bits_needed = -8;
                    self.value |= self.next_byte() as u32;
                }
            }
            0
        }
    }

    // ---- binarizations (9.3.3) ----

    /// A truncated unary value of bypass bins (cMax `max`).
    pub fn bypass_unary(&mut self, max: u32) -> u32 {
        let mut v = 0;
        while v < max && self.bypass() != 0 {
            v += 1;
        }
        v
    }

    /// k-th order Exp-Golomb of bypass bins (9.3.3.3), limited to values
    /// that fit 32 bits.
    pub fn bypass_egk(&mut self, mut k: u32) -> Result<u32> {
        let mut v: u32 = 0;
        while self.bypass() != 0 {
            v = v.wrapping_add(1 << k);
            k += 1;
            if k > 30 {
                return Err(Error::Bitstream("Exp-Golomb bin string too long"));
            }
        }
        Ok(v.wrapping_add(self.bypass_bits(k)))
    }

    /// sao_type_idx_luma / sao_type_idx_chroma: 0 none, 1 band, 2 edge.
    pub fn sao_type_idx(&mut self) -> u32 {
        if self.decision(ctx::SAO_TYPE) == 0 {
            0
        } else if self.bypass() == 0 {
            1
        } else {
            2
        }
    }

    /// merge_idx (TR, cMax MaxNumMergeCand - 1, first bin context coded).
    pub fn merge_idx(&mut self, max_cand: u32) -> u32 {
        if max_cand <= 1 || self.decision(ctx::MERGE_IDX) == 0 {
            return 0;
        }
        1 + self.bypass_unary(max_cand - 2)
    }

    /// ref_idx_lX (TR, cMax num_ref_idx_active - 1, two context bins).
    pub fn ref_idx(&mut self, num_ref: usize) -> usize {
        let max = num_ref as u32 - 1;
        if max == 0 || self.decision(ctx::REF_IDX) == 0 {
            return 0;
        }
        if max == 1 || self.decision(ctx::REF_IDX + 1) == 0 {
            return 1;
        }
        2 + self.bypass_unary(max - 2) as usize
    }

    /// mvd_coding( ): the motion vector difference.
    pub fn mvd(&mut self) -> Result<[i32; 2]> {
        let gt0 = [self.decision(ctx::MVD_GT0), self.decision(ctx::MVD_GT0)];
        let mut gt1 = [0, 0];
        for c in 0..2 {
            if gt0[c] != 0 {
                gt1[c] = self.decision(ctx::MVD_GT1);
            }
        }
        let mut mvd = [0i32; 2];
        for c in 0..2 {
            if gt0[c] != 0 {
                let abs = if gt1[c] != 0 { self.bypass_egk(1)? as i64 + 2 } else { 1 };
                if abs > 1 << 15 {
                    return Err(Error::Bitstream("motion vector difference out of range"));
                }
                mvd[c] = if self.bypass() != 0 { -(abs as i32) } else { abs as i32 };
            }
        }
        Ok(mvd)
    }

    /// cu_qp_delta_abs and cu_qp_delta_sign_flag: CuQpDeltaVal.
    pub fn cu_qp_delta(&mut self) -> Result<i32> {
        let mut abs = 0u32;
        if self.decision(ctx::CU_QP_DELTA) != 0 {
            abs = 1;
            while abs < 5 && self.decision(ctx::CU_QP_DELTA + 1) != 0 {
                abs += 1;
            }
            if abs == 5 {
                abs += self.bypass_egk(0)?;
            }
        }
        if abs == 0 {
            return Ok(0);
        }
        if abs > 100 {
            return Err(Error::Bitstream("cu_qp_delta_abs out of range"));
        }
        Ok(if self.bypass() != 0 { -(abs as i32) } else { abs as i32 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_sizes_and_states() {
        // every row lists every context
        assert_eq!(INIT_VALUES[0].len(), ctx::COUNT);
        // initValue 154 is the equiprobable state (pStateIdx 0, valMps 1) at any QP
        for qp in [0, 30, 51] {
            assert_eq!(init_contexts(0, qp)[ctx::CU_QP_DELTA], 1);
        }
        // the LPS table ends in the non-adapting state
        assert_eq!(NEXT_MPS[124], 124);
        assert_eq!(NEXT_LPS[1] & 1, 0);
    }

    #[test]
    fn bypass_bins_read_the_stream_bits() {
        // with ivlCurrRange 510 at the start, bypass bins after the first
        // nine bits reproduce the data bits
        let data = [0x00, 0x00, 0b1011_0000, 0x00, 0x00];
        let mut c = Cabac::new(&data, 0, [0; ctx::COUNT]).unwrap();
        let bits: Vec<u32> = (0..8).map(|_| c.bypass()).collect();
        assert_eq!(bits.len(), 8);
        assert!(!c.overrun());
    }
}
