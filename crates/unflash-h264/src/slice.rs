//! Slice headers (7.3.3) with their reference picture list modifications,
//! prediction weight tables and reference marking operations.

use crate::bitreader::BitReader;
use crate::ps::{Pps, Sps};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
}

impl SliceType {
    pub fn from_code(v: u32) -> Result<SliceType> {
        match v % 5 {
            0 => Ok(SliceType::P),
            1 => Ok(SliceType::B),
            2 => Ok(SliceType::I),
            _ => Err(Error::Unsupported("SP / SI slices")),
        }
    }
}

/// One `ref_pic_list_modification` operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefListMod {
    /// modification_of_pic_nums_idc 0: abs_diff_pic_num_minus1 + 1
    ShortTermSub(u32),
    /// idc 1
    ShortTermAdd(u32),
    /// idc 2: long_term_pic_num
    LongTerm(u32),
}

/// A memory management control operation (7.3.3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mmco {
    /// 1: difference_of_pic_nums_minus1 + 1
    UnmarkShortTerm(u32),
    /// 2: long_term_pic_num
    UnmarkLongTerm(u32),
    /// 3: (difference_of_pic_nums_minus1 + 1, long_term_frame_idx)
    ShortToLong(u32, u32),
    /// 4: max_long_term_frame_idx_plus1
    MaxLongTermIdx(u32),
    /// 5
    UnmarkAll,
    /// 6: long_term_frame_idx
    CurrentToLong(u32),
}

/// Explicit weights of one reference: (weight, offset), None = default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct WeightEntry {
    pub luma: Option<(i32, i32)>,
    pub chroma: [Option<(i32, i32)>; 2],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredWeightTable {
    pub luma_log2_denom: u32,
    pub chroma_log2_denom: u32,
    pub lists: [Vec<WeightEntry>; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub struct SliceHeader {
    pub nal_unit_type: u8,
    pub nal_ref_idc: u8,
    pub first_mb: u32,
    pub slice_type: SliceType,
    pub pps_id: u32,
    pub frame_num: u32,
    /// field_pic_flag / bottom_field_flag: the slice belongs to a field picture.
    pub field_pic: bool,
    pub bottom_field: bool,
    pub idr_pic_id: u32,
    pub poc_lsb: u32,
    pub delta_poc_bottom: i32,
    pub delta_poc: [i32; 2],
    pub redundant_pic_cnt: u32,
    pub direct_spatial_mv_pred: bool,
    pub num_ref_idx_active: [u32; 2],
    pub ref_list_mods: [Vec<RefListMod>; 2],
    pub pred_weight: Option<PredWeightTable>,
    pub long_term_reference: bool,
    /// None: sliding window marking; Some: adaptive marking operations.
    pub mmco: Option<Vec<Mmco>>,
    pub cabac_init_idc: u32,
    pub slice_qp: i32,
    pub disable_deblocking_filter_idc: u32,
    /// FilterOffsetA / FilterOffsetB (already doubled).
    pub alpha_offset: i32,
    pub beta_offset: i32,
}

impl SliceHeader {
    pub fn is_idr(&self) -> bool {
        self.nal_unit_type == 5
    }
    pub fn is_ref(&self) -> bool {
        self.nal_ref_idc != 0
    }
    /// The picture structure: 1 top field, 2 bottom field, 3 frame.
    pub fn structure(&self) -> u8 {
        if !self.field_pic {
            3
        } else if self.bottom_field {
            2
        } else {
            1
        }
    }
}

fn parse_weights(r: &mut BitReader, n: u32, table: &mut Vec<WeightEntry>) -> Result<()> {
    for _ in 0..n {
        let mut e = WeightEntry::default();
        if r.flag()? {
            let w = r.se()?;
            let o = r.se()?;
            if !(-128..=127).contains(&w) || !(-128..=127).contains(&o) {
                return Err(Error::Bitstream("luma weight out of range"));
            }
            e.luma = Some((w, o));
        }
        if r.flag()? {
            for c in 0..2 {
                let w = r.se()?;
                let o = r.se()?;
                if !(-128..=127).contains(&w) || !(-128..=127).contains(&o) {
                    return Err(Error::Bitstream("chroma weight out of range"));
                }
                e.chroma[c] = Some((w, o));
            }
        }
        table.push(e);
    }
    Ok(())
}

fn parse_list_mods(r: &mut BitReader) -> Result<Vec<RefListMod>> {
    let mut ops = Vec::new();
    if r.flag()? {
        loop {
            let idc = r.ue_max(3, "modification_of_pic_nums_idc")?;
            match idc {
                0 => ops.push(RefListMod::ShortTermSub(r.ue()? + 1)),
                1 => ops.push(RefListMod::ShortTermAdd(r.ue()? + 1)),
                2 => ops.push(RefListMod::LongTerm(r.ue()?)),
                _ => break,
            }
            if ops.len() > 66 {
                return Err(Error::Bitstream("too many reference list modifications"));
            }
        }
    }
    Ok(ops)
}

/// Parse a slice header. `r` is left at the start of the slice data.
pub fn parse_slice_header(r: &mut BitReader, nal_unit_type: u8, nal_ref_idc: u8, spss: &[Option<Sps>], ppss: &[Option<Pps>]) -> Result<SliceHeader> {
    let first_mb = r.ue()?;
    let slice_type = SliceType::from_code(r.ue_max(9, "slice_type")?)?;
    let pps_id = r.ue_max(255, "pic_parameter_set_id")?;
    let pps = ppss.get(pps_id as usize).and_then(|p| p.as_ref()).ok_or(Error::Bitstream("slice refers to a missing PPS"))?;
    let sps = spss.get(pps.sps_id as usize).and_then(|s| s.as_ref()).ok_or(Error::Bitstream("PPS refers to a missing SPS"))?;
    // (a field has half the frame's macroblocks and an MBAFF frame counts
    // pairs: SliceDecoder::decode stops a slice that starts past those)
    if first_mb >= sps.width_mbs * sps.height_mbs {
        return Err(Error::Bitstream("first_mb_in_slice outside the picture"));
    }
    let frame_num = r.u(sps.log2_max_frame_num)?;
    let mut field_pic = false;
    let mut bottom_field = false;
    if !sps.frame_mbs_only {
        field_pic = r.flag()?;
        if field_pic {
            bottom_field = r.flag()?;
        }
    }
    let is_idr = nal_unit_type == 5;
    let idr_pic_id = if is_idr { r.ue_max(65535, "idr_pic_id")? } else { 0 };
    let mut poc_lsb = 0;
    let mut delta_poc_bottom = 0;
    let mut delta_poc = [0i32; 2];
    if sps.poc_type == 0 {
        poc_lsb = r.u(sps.log2_max_poc_lsb)?;
        if pps.bottom_field_pic_order_in_frame_present && !field_pic {
            delta_poc_bottom = r.se()?;
        }
    } else if sps.poc_type == 1 && !sps.delta_pic_order_always_zero {
        delta_poc[0] = r.se()?;
        if pps.bottom_field_pic_order_in_frame_present && !field_pic {
            delta_poc[1] = r.se()?;
        }
    }
    let redundant_pic_cnt = if pps.redundant_pic_cnt_present { r.ue_max(127, "redundant_pic_cnt")? } else { 0 };
    let mut direct_spatial_mv_pred = false;
    if slice_type == SliceType::B {
        direct_spatial_mv_pred = r.flag()?;
    }
    let mut num_ref_idx_active = pps.num_ref_idx_default_active;
    if slice_type != SliceType::I {
        if r.flag()? {
            num_ref_idx_active[0] = r.ue_max(31, "num_ref_idx_l0_active_minus1")? + 1;
            if slice_type == SliceType::B {
                num_ref_idx_active[1] = r.ue_max(31, "num_ref_idx_l1_active_minus1")? + 1;
            }
        }
    }
    if slice_type != SliceType::B {
        num_ref_idx_active[1] = 0;
    }
    if slice_type == SliceType::I {
        num_ref_idx_active[0] = 0;
    }
    let mut ref_list_mods = [Vec::new(), Vec::new()];
    if slice_type != SliceType::I {
        ref_list_mods[0] = parse_list_mods(r)?;
        if slice_type == SliceType::B {
            ref_list_mods[1] = parse_list_mods(r)?;
        }
    }
    let mut pred_weight = None;
    if (pps.weighted_pred && slice_type == SliceType::P) || (pps.weighted_bipred_idc == 1 && slice_type == SliceType::B) {
        let luma_log2_denom = r.ue_max(7, "luma_log2_weight_denom")?;
        let chroma_log2_denom = r.ue_max(7, "chroma_log2_weight_denom")?;
        let mut lists = [Vec::new(), Vec::new()];
        parse_weights(r, num_ref_idx_active[0], &mut lists[0])?;
        if slice_type == SliceType::B {
            parse_weights(r, num_ref_idx_active[1], &mut lists[1])?;
        }
        pred_weight = Some(PredWeightTable { luma_log2_denom, chroma_log2_denom, lists });
    }
    let mut long_term_reference = false;
    let mut mmco = None;
    if nal_ref_idc != 0 {
        if is_idr {
            r.flag()?; // no_output_of_prior_pics_flag
            long_term_reference = r.flag()?;
        } else if r.flag()? {
            let mut ops = Vec::new();
            loop {
                let op = r.ue_max(6, "memory_management_control_operation")?;
                match op {
                    0 => break,
                    1 => ops.push(Mmco::UnmarkShortTerm(r.ue()? + 1)),
                    2 => ops.push(Mmco::UnmarkLongTerm(r.ue()?)),
                    // (a long-term frame index is below max_num_ref_frames, so
                    // at most 15, as ffmpeg also requires; unbounded, a damaged
                    // stream could add long-term frames to the buffer without end)
                    3 => {
                        let d = r.ue()? + 1;
                        ops.push(Mmco::ShortToLong(d, r.ue_max(15, "long_term_frame_idx")?));
                    }
                    4 => ops.push(Mmco::MaxLongTermIdx(r.ue()?)),
                    5 => ops.push(Mmco::UnmarkAll),
                    _ => ops.push(Mmco::CurrentToLong(r.ue_max(15, "long_term_frame_idx")?)),
                }
                if ops.len() > 66 {
                    return Err(Error::Bitstream("too many marking operations"));
                }
            }
            mmco = Some(ops);
        }
    }
    let mut cabac_init_idc = 0;
    if pps.entropy_coding_mode && slice_type != SliceType::I {
        cabac_init_idc = r.ue_max(2, "cabac_init_idc")?;
    }
    let slice_qp = pps.pic_init_qp + r.se()?;
    if !(0..=51).contains(&slice_qp) {
        return Err(Error::Bitstream("slice QP out of range"));
    }
    let mut disable_deblocking_filter_idc = 0;
    let mut alpha_offset = 0;
    let mut beta_offset = 0;
    if pps.deblocking_filter_control_present {
        disable_deblocking_filter_idc = r.ue_max(2, "disable_deblocking_filter_idc")?;
        if disable_deblocking_filter_idc != 1 {
            let a = r.se()?;
            let b = r.se()?;
            if !(-6..=6).contains(&a) || !(-6..=6).contains(&b) {
                return Err(Error::Bitstream("deblocking offsets out of range"));
            }
            alpha_offset = a * 2;
            beta_offset = b * 2;
        }
    }
    Ok(SliceHeader {
        nal_unit_type,
        nal_ref_idc,
        first_mb,
        slice_type,
        pps_id,
        frame_num,
        field_pic,
        bottom_field,
        idr_pic_id,
        poc_lsb,
        delta_poc_bottom,
        delta_poc,
        redundant_pic_cnt,
        direct_spatial_mv_pred,
        num_ref_idx_active,
        ref_list_mods,
        pred_weight,
        long_term_reference,
        mmco,
        cabac_init_idc,
        slice_qp,
        disable_deblocking_filter_idc,
        alpha_offset,
        beta_offset,
    })
}
