//! Slice segment headers (7.3.6) and the NAL unit types that carry them.

use crate::bitreader::BitReader;
use crate::ps::{parse_st_rps, Pps, Sps, StRps};
use crate::{Error, Result};

/// NAL unit types (Table 7-1).
pub mod nal {
    pub const TRAIL_N: u8 = 0;
    pub const RADL_N: u8 = 6;
    pub const RADL_R: u8 = 7;
    pub const RASL_N: u8 = 8;
    pub const RASL_R: u8 = 9;
    pub const RSV_VCL_N14: u8 = 14;
    pub const BLA_W_LP: u8 = 16;
    pub const BLA_N_LP: u8 = 18;
    pub const IDR_W_RADL: u8 = 19;
    pub const IDR_N_LP: u8 = 20;
    pub const CRA_NUT: u8 = 21;
    pub const RSV_IRAP_23: u8 = 23;
    pub const SPS: u8 = 33;
    pub const PPS: u8 = 34;
    pub const AUD: u8 = 35;
    pub const EOS: u8 = 36;
    pub const EOB: u8 = 37;
    pub const SEI_SUFFIX: u8 = 40;

    pub fn is_irap(t: u8) -> bool {
        (BLA_W_LP..=RSV_IRAP_23).contains(&t)
    }
    pub fn is_idr(t: u8) -> bool {
        t == IDR_W_RADL || t == IDR_N_LP
    }
    pub fn is_bla(t: u8) -> bool {
        (BLA_W_LP..=BLA_N_LP).contains(&t)
    }
    pub fn is_rasl(t: u8) -> bool {
        t == RASL_N || t == RASL_R
    }
    pub fn is_radl(t: u8) -> bool {
        t == RADL_N || t == RADL_R
    }
    /// A sub-layer non-reference picture (the even types below 16).
    pub fn is_sub_layer_non_ref(t: u8) -> bool {
        t <= RSV_VCL_N14 && t.is_multiple_of(2)
    }
}

/// slice_type (Table 7-7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceType {
    B,
    P,
    I,
}

/// One entry of the long-term part of a slice's reference picture set, as
/// signalled (the order counts are resolved against the picture's POC).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct LongTermEntry {
    /// PocLsbLt.
    pub poc_lsb: u32,
    pub used: bool,
    pub msb_present: bool,
    /// DeltaPocMsbCycleLt.
    pub msb_cycle: u32,
}

/// Explicit weighted prediction parameters (7.3.6.3, 7.4.7.3): weight and
/// offset (already scaled to the bit depth) per list, reference index and
/// component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredWeights {
    pub luma_log2_denom: u32,
    pub chroma_log2_denom: u32,
    /// `[list][ref_idx][component]` -> (weight, offset)
    pub w: [[[(i32, i32); 3]; 16]; 2],
}

/// A slice segment header (7.3.6.1), with what a dependent segment takes
/// from the independent one before it.
#[derive(Clone, Debug)]
pub struct SliceHeader {
    pub nal_type: u8,
    pub first_slice_segment_in_pic: bool,
    pub no_output_of_prior_pics: bool,
    pub pps_id: u32,
    pub dependent: bool,
    /// slice_segment_address (a CTB address in raster scan).
    pub segment_address: u32,
    /// SliceAddrRs: the address of the independent slice segment this one
    /// belongs to.
    pub slice_address: u32,
    pub slice_type: SliceType,
    pub pic_output: bool,
    pub poc_lsb: u32,
    pub st_rps: StRps,
    pub lt: Vec<LongTermEntry>,
    pub temporal_mvp: bool,
    pub sao_luma: bool,
    pub sao_chroma: bool,
    pub num_ref_idx: [usize; 2],
    /// list_entry_lX when ref_pic_list_modification_flag_lX is set.
    pub list_entry: [Option<[u8; 16]>; 2],
    pub mvd_l1_zero: bool,
    pub cabac_init: bool,
    pub collocated_from_l0: bool,
    pub collocated_ref_idx: usize,
    pub weights: Option<Box<PredWeights>>,
    pub max_num_merge_cand: u32,
    pub qp: i32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub cu_chroma_qp_offset_enabled: bool,
    pub deblocking_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub loop_filter_across_slices: bool,
    /// Byte offset of the slice segment data in the RBSP.
    pub data_offset: usize,
}

impl SliceHeader {
    /// NumPicTotalCurr (7-55).
    pub fn num_pic_total_curr(&self) -> usize {
        self.st_rps.num_used() + self.lt.iter().filter(|e| e.used).count()
    }
    pub fn is_intra(&self) -> bool {
        self.slice_type == SliceType::I
    }
}

fn ceil_log2(n: usize) -> u32 {
    if n <= 1 {
        0
    } else {
        usize::BITS - (n - 1).leading_zeros()
    }
}

/// 7.3.6.3: `pred_weight_table( )`.
fn parse_pred_weights(r: &mut BitReader, sps: &Sps, slice_type: SliceType, num_ref_idx: [usize; 2]) -> Result<PredWeights> {
    let luma_log2_denom = r.ue_max(7, "luma_log2_weight_denom")?;
    let chroma = sps.chroma_format_idc != 0;
    let chroma_log2_denom = if chroma {
        let d = luma_log2_denom as i32 + r.se()?;
        if !(0..=7).contains(&d) {
            return Err(Error::Bitstream("delta_chroma_log2_weight_denom"));
        }
        d as u32
    } else {
        0
    };
    let mut pw = PredWeights { luma_log2_denom, chroma_log2_denom, w: [[[(1 << luma_log2_denom, 0), (1 << chroma_log2_denom, 0), (1 << chroma_log2_denom, 0)]; 16]; 2] };
    let lists = if slice_type == SliceType::B { 2 } else { 1 };
    let luma_shift = sps.bit_depth - 8;
    let chroma_shift = sps.bit_depth_chroma - 8;
    for (l, &n) in num_ref_idx.iter().enumerate().take(lists) {
        let mut luma_flags = [false; 16];
        let mut chroma_flags = [false; 16];
        for f in luma_flags.iter_mut().take(n) {
            *f = r.flag()?;
        }
        if chroma {
            for f in chroma_flags.iter_mut().take(n) {
                *f = r.flag()?;
            }
        }
        for i in 0..n {
            if luma_flags[i] {
                let dw = r.se_range(-128, 127, "delta_luma_weight")?;
                let o = r.se_range(-128, 127, "luma_offset")?;
                pw.w[l][i][0] = ((1 << luma_log2_denom) + dw, o << luma_shift);
            }
            if chroma_flags[i] {
                for c in 1..3 {
                    let dw = r.se_range(-128, 127, "delta_chroma_weight")?;
                    let weight = (1 << chroma_log2_denom) + dw;
                    let d_off = r.se_range(-512, 511, "delta_chroma_offset")?;
                    // (7-56): the offset is predicted from the weight
                    let half = 128;
                    let o = (half - ((half * weight) >> chroma_log2_denom) + d_off).clamp(-half, half - 1);
                    pw.w[l][i][c] = (weight, o << chroma_shift);
                }
            }
        }
    }
    Ok(pw)
}

/// 7.3.6.1: parse a slice segment header. `prev` is the header of the
/// previous independent slice segment of the picture (a dependent segment
/// takes its values from it).
pub fn parse_slice_header(r: &mut BitReader, nal_type: u8, spss: &[Option<std::rc::Rc<Sps>>], ppss: &[Option<std::rc::Rc<Pps>>], prev: Option<&SliceHeader>) -> Result<SliceHeader> {
    let first_slice_segment_in_pic = r.flag()?;
    let no_output_of_prior_pics = if nal::is_irap(nal_type) { r.flag()? } else { false };
    let pps_id = r.ue_max(63, "slice_pic_parameter_set_id")?;
    let pps = ppss[pps_id as usize].as_ref().ok_or(Error::Bitstream("slice refers to a missing PPS"))?;
    let sps = spss[pps.sps_id as usize].as_ref().ok_or(Error::Bitstream("PPS refers to a missing SPS"))?;
    let pic_size_ctbs = (sps.width_ctbs() * sps.height_ctbs()) as usize;
    let mut dependent = false;
    let mut segment_address = 0;
    if !first_slice_segment_in_pic {
        if pps.dependent_slice_segments_enabled {
            dependent = r.flag()?;
        }
        segment_address = r.u(ceil_log2(pic_size_ctbs))?;
        if segment_address as usize >= pic_size_ctbs {
            return Err(Error::Bitstream("slice_segment_address outside the picture"));
        }
    }
    let mut h = if dependent {
        let prev = prev.ok_or(Error::Bitstream("dependent slice segment without an independent one"))?;
        if prev.pps_id != pps_id {
            return Err(Error::Bitstream("PPS changes within a slice"));
        }
        let mut h = prev.clone();
        h.first_slice_segment_in_pic = false;
        h.no_output_of_prior_pics = no_output_of_prior_pics;
        h.dependent = true;
        h.segment_address = segment_address;
        h
    } else {
        parse_independent(r, nal_type, sps, pps)?
    };
    h.nal_type = nal_type;
    h.first_slice_segment_in_pic = first_slice_segment_in_pic;
    h.no_output_of_prior_pics = no_output_of_prior_pics;
    h.pps_id = pps_id;
    h.segment_address = segment_address;
    if !dependent {
        h.slice_address = segment_address;
    }
    if pps.tiles_enabled || pps.entropy_coding_sync {
        let n = r.ue_max(pic_size_ctbs as u32, "num_entry_point_offsets")?;
        if n > 0 {
            let len = r.ue_max(31, "offset_len_minus1")? + 1;
            r.skip(n as usize * len as usize)?;
        }
    }
    if pps.slice_header_extension_present {
        let len = r.ue_max(256, "slice_segment_header_extension_length")?;
        r.skip(8 * len as usize)?;
    }
    // byte_alignment( ): a one bit, then zero bits
    if !r.flag()? {
        return Err(Error::Bitstream("slice header alignment bit"));
    }
    r.byte_align();
    if r.bits_left() <= 0 {
        return Err(Error::Bitstream("slice segment without data"));
    }
    h.data_offset = r.byte_pos();
    Ok(h)
}

/// The fields of an independent slice segment header after
/// slice_segment_address.
fn parse_independent(r: &mut BitReader, nal_type: u8, sps: &Sps, pps: &Pps) -> Result<SliceHeader> {
    r.skip(pps.num_extra_slice_header_bits as usize)?;
    let slice_type = match r.ue_max(2, "slice_type")? {
        0 => SliceType::B,
        1 => SliceType::P,
        _ => SliceType::I,
    };
    if nal::is_irap(nal_type) && slice_type != SliceType::I {
        return Err(Error::Bitstream("inter slice in an IRAP picture"));
    }
    let pic_output = if pps.output_flag_present { r.flag()? } else { true };
    let mut poc_lsb = 0;
    let mut st_rps = StRps::default();
    let mut lt = Vec::new();
    let mut temporal_mvp = false;
    if !nal::is_idr(nal_type) {
        poc_lsb = r.u(sps.log2_max_poc_lsb)?;
        let from_sps = r.flag()?;
        if !from_sps {
            st_rps = parse_st_rps(r, sps.st_rps.len(), &sps.st_rps, true)?;
        } else {
            if sps.st_rps.is_empty() {
                return Err(Error::Bitstream("short-term RPS from an SPS without any"));
            }
            let idx = r.u(ceil_log2(sps.st_rps.len()))? as usize;
            st_rps = *sps.st_rps.get(idx).ok_or(Error::Bitstream("short_term_ref_pic_set_idx"))?;
        }
        if sps.long_term_refs_present {
            let num_lt_sps = if !sps.lt_ref_pics.is_empty() { r.ue_max(sps.lt_ref_pics.len() as u32, "num_long_term_sps")? as usize } else { 0 };
            let num_lt_pics = r.ue_max(16, "num_long_term_pics")? as usize;
            if st_rps.len() + num_lt_sps + num_lt_pics > 16 {
                return Err(Error::Bitstream("too many reference pictures"));
            }
            let mut cycle = 0u32;
            for i in 0..num_lt_sps + num_lt_pics {
                let mut e = LongTermEntry::default();
                if i < num_lt_sps {
                    let idx = if sps.lt_ref_pics.len() > 1 { r.u(ceil_log2(sps.lt_ref_pics.len()))? as usize } else { 0 };
                    let &(lsb, used) = sps.lt_ref_pics.get(idx).ok_or(Error::Bitstream("lt_idx_sps"))?;
                    e.poc_lsb = lsb;
                    e.used = used;
                } else {
                    e.poc_lsb = r.u(sps.log2_max_poc_lsb)?;
                    e.used = r.flag()?;
                }
                e.msb_present = r.flag()?;
                let delta = if e.msb_present { r.ue_max(1 << 24, "delta_poc_msb_cycle_lt")? } else { 0 };
                // (7-52): the cycles accumulate within each of the two groups
                cycle = if i == 0 || i == num_lt_sps { delta } else { cycle + delta };
                e.msb_cycle = cycle;
                lt.push(e);
            }
        }
        if sps.temporal_mvp_enabled {
            temporal_mvp = r.flag()?;
        }
    }
    let (mut sao_luma, mut sao_chroma) = (false, false);
    if sps.sao_enabled {
        sao_luma = r.flag()?;
        if sps.chroma_format_idc != 0 {
            sao_chroma = r.flag()?;
        }
    }
    let mut h = SliceHeader {
        nal_type,
        first_slice_segment_in_pic: false,
        no_output_of_prior_pics: false,
        pps_id: pps.id,
        dependent: false,
        segment_address: 0,
        slice_address: 0,
        slice_type,
        pic_output,
        poc_lsb,
        st_rps,
        lt,
        temporal_mvp,
        sao_luma,
        sao_chroma,
        num_ref_idx: [0, 0],
        list_entry: [None, None],
        mvd_l1_zero: false,
        cabac_init: false,
        collocated_from_l0: true,
        collocated_ref_idx: 0,
        weights: None,
        max_num_merge_cand: 5,
        qp: 0,
        cb_qp_offset: 0,
        cr_qp_offset: 0,
        cu_chroma_qp_offset_enabled: false,
        deblocking_disabled: pps.deblocking_disabled,
        beta_offset_div2: pps.beta_offset_div2,
        tc_offset_div2: pps.tc_offset_div2,
        loop_filter_across_slices: pps.loop_filter_across_slices,
        data_offset: 0,
    };
    if slice_type != SliceType::I {
        let mut n = [pps.num_ref_idx_default[0] as usize, pps.num_ref_idx_default[1] as usize];
        if r.flag()? {
            n[0] = r.ue_max(14, "num_ref_idx_l0_active_minus1")? as usize + 1;
            if slice_type == SliceType::B {
                n[1] = r.ue_max(14, "num_ref_idx_l1_active_minus1")? as usize + 1;
            }
        }
        if slice_type != SliceType::B {
            n[1] = 0;
        }
        h.num_ref_idx = n;
        let total = h.num_pic_total_curr();
        if total == 0 {
            return Err(Error::Bitstream("inter slice with no reference pictures"));
        }
        if pps.lists_modification_present && total > 1 {
            let bits = ceil_log2(total);
            let lists = if slice_type == SliceType::B { 2 } else { 1 };
            for (l, &count) in n.iter().enumerate().take(lists) {
                if r.flag()? {
                    let mut entries = [0u8; 16];
                    for e in entries.iter_mut().take(count) {
                        let v = r.u(bits)? as usize;
                        if v >= total {
                            return Err(Error::Bitstream("list_entry outside the reference picture set"));
                        }
                        *e = v as u8;
                    }
                    h.list_entry[l] = Some(entries);
                }
            }
        }
        if slice_type == SliceType::B {
            h.mvd_l1_zero = r.flag()?;
        }
        if pps.cabac_init_present {
            h.cabac_init = r.flag()?;
        }
        if temporal_mvp {
            if slice_type == SliceType::B {
                h.collocated_from_l0 = r.flag()?;
            }
            let l = if h.collocated_from_l0 { 0 } else { 1 };
            if n[l] > 1 {
                h.collocated_ref_idx = r.ue_max(n[l] as u32 - 1, "collocated_ref_idx")? as usize;
            }
        }
        if (pps.weighted_pred && slice_type == SliceType::P) || (pps.weighted_bipred && slice_type == SliceType::B) {
            h.weights = Some(Box::new(parse_pred_weights(r, sps, slice_type, n)?));
        }
        h.max_num_merge_cand = 5 - r.ue_max(4, "five_minus_max_num_merge_cand")?;
    }
    let qp = pps.init_qp + r.se()?;
    if qp < -sps.qp_bd_offset() || qp > 51 {
        return Err(Error::Bitstream("slice_qp_delta"));
    }
    h.qp = qp;
    if pps.slice_chroma_qp_offsets_present {
        h.cb_qp_offset = r.se_range(-12, 12, "slice_cb_qp_offset")?;
        h.cr_qp_offset = r.se_range(-12, 12, "slice_cr_qp_offset")?;
        if !(-12..=12).contains(&(pps.cb_qp_offset + h.cb_qp_offset)) || !(-12..=12).contains(&(pps.cr_qp_offset + h.cr_qp_offset)) {
            return Err(Error::Bitstream("chroma QP offsets out of range"));
        }
    }
    if !pps.chroma_qp_offset_list.is_empty() {
        h.cu_chroma_qp_offset_enabled = r.flag()?;
    }
    let override_flag = pps.deblocking_override_enabled && r.flag()?;
    if override_flag {
        h.deblocking_disabled = r.flag()?;
        if !h.deblocking_disabled {
            h.beta_offset_div2 = r.se_range(-6, 6, "slice_beta_offset_div2")?;
            h.tc_offset_div2 = r.se_range(-6, 6, "slice_tc_offset_div2")?;
        }
    }
    if pps.loop_filter_across_slices && (sao_luma || sao_chroma || !h.deblocking_disabled) {
        h.loop_filter_across_slices = r.flag()?;
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log2_ceilings() {
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(4), 2);
        assert_eq!(ceil_log2(5), 3);
    }

    #[test]
    fn nal_classes() {
        assert!(nal::is_irap(nal::CRA_NUT));
        assert!(!nal::is_irap(nal::RASL_R));
        assert!(nal::is_sub_layer_non_ref(nal::TRAIL_N));
        assert!(!nal::is_sub_layer_non_ref(nal::RADL_R));
        assert!(!nal::is_sub_layer_non_ref(nal::CRA_NUT));
    }
}
