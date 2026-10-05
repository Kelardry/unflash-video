//! Sequence and picture parameter sets (7.3.2.1, 7.3.2.2) and the scaling
//! tables derived from them.

use crate::bitreader::BitReader;
use crate::tables::{DEFAULT_SCALING4, DEFAULT_SCALING8, DEQUANT4_INIT, DEQUANT8_INIT, ZIGZAG4X4, ZIGZAG8X8};
use crate::{Error, Result};

/// The part of the VUI the decoder cares about (colour conversion).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Vui {
    pub video_full_range: bool,
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sps {
    pub id: u32,
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub log2_max_frame_num: u32,
    pub poc_type: u32,
    pub log2_max_poc_lsb: u32,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_allowed: bool,
    pub width_mbs: u32,
    /// Frame height in macroblocks (twice the map units of an interlaced sequence).
    pub height_mbs: u32,
    pub frame_mbs_only: bool,
    /// mb_adaptive_frame_field_flag: frames may mix frame and field macroblock pairs.
    pub mbaff: bool,
    pub direct_8x8_inference: bool,
    /// left, right, top, bottom, in luma samples
    pub crop: (u32, u32, u32, u32),
    /// seq_scaling_matrix_present_flag (decides the PPS fall-back rule).
    pub scaling_present: bool,
    /// Resolved scaling lists in transmission (zigzag) order:
    /// 0..3 intra Y/Cb/Cr, 3..6 inter Y/Cb/Cr; 8x8: intra Y, inter Y.
    pub scaling4: [[u8; 16]; 6],
    pub scaling8: [[u8; 64]; 2],
    pub vui: Option<Vui>,
}

impl Sps {
    pub fn width(&self) -> u32 {
        self.width_mbs * 16
    }
    pub fn height(&self) -> u32 {
        self.height_mbs * 16
    }
    /// The displayed picture size after cropping.
    pub fn cropped_size(&self) -> (u32, u32) {
        let (l, r, t, b) = self.crop;
        (self.width().saturating_sub(l + r).max(1), self.height().saturating_sub(t + b).max(1))
    }
    pub fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub entropy_coding_mode: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_default_active: [u32; 2],
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp: i32,
    pub chroma_qp_index_offset: [i32; 2],
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    /// Scaling lists as sent, None where one is not (the fall-back rule
    /// applies when the PPS is paired with its SPS, see
    /// [`ScalingTables::new`]).
    pub scaling_present: bool,
    pub sent4: [Option<ScalingList<16>>; 6],
    pub sent8: [Option<ScalingList<64>>; 2],
}

/// A scaling list as a parameter set sends it (7.3.2.1.1.1): its own
/// values, or the default list (useDefaultScalingMatrixFlag).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalingList<const N: usize> {
    Default,
    List([u8; N]),
}

/// LevelScale4x4 / LevelScale8x8 (8.5.9) for every qP % 6 and list.
#[derive(Clone, Debug)]
pub struct ScalingTables {
    /// [qp % 6][list 0..6][raster position]
    pub level4: [[[i32; 16]; 6]; 6],
    /// [qp % 6][list 0..2][raster position]
    pub level8: [[[i32; 64]; 2]; 6],
}

/// 7.3.2.1.1.1: one scaling list.
fn parse_scaling_list<const N: usize>(r: &mut BitReader) -> Result<ScalingList<N>> {
    let mut list = [0u8; N];
    let mut last = 8i32;
    let mut next = 8i32;
    for (j, slot) in list.iter_mut().enumerate() {
        if next != 0 {
            let delta = r.se()?;
            next = (last + delta + 256).rem_euclid(256);
            if j == 0 && next == 0 {
                // useDefaultScalingMatrixFlag (no more deltas follow)
                return Ok(ScalingList::Default);
            }
        }
        *slot = if next == 0 { last as u8 } else { next as u8 };
        last = *slot as i32;
    }
    Ok(ScalingList::List(list))
}

/// The six 4x4 and two 8x8 scaling lists a parameter set sends (None where
/// it does not).
type SentLists = ([Option<ScalingList<16>>; 6], [Option<ScalingList<64>>; 2]);

/// The first `n` of the scaling lists, each with its present flag.
fn parse_scaling_lists(r: &mut BitReader, n: usize) -> Result<SentLists> {
    let (mut sent4, mut sent8) = ([None; 6], [None; 2]);
    for i in 0..n {
        if !r.flag()? {
            continue;
        }
        if i < 6 {
            sent4[i] = Some(parse_scaling_list(r)?);
        } else {
            sent8[i - 6] = Some(parse_scaling_list(r)?);
        }
    }
    Ok((sent4, sent8))
}

/// Table 7-2: the lists in use from those a parameter set sends. A Y list
/// not sent falls back to `fall4` (intra, inter) or `fall8`, a Cb or Cr
/// list to the list before it.
fn resolve(sent4: &[Option<ScalingList<16>>; 6], sent8: &[Option<ScalingList<64>>; 2], fall4: [[u8; 16]; 2], fall8: [[u8; 64]; 2]) -> ([[u8; 16]; 6], [[u8; 64]; 2]) {
    let mut lists4 = [[0u8; 16]; 6];
    for i in 0..6 {
        lists4[i] = match sent4[i] {
            Some(ScalingList::List(l)) => l,
            Some(ScalingList::Default) => DEFAULT_SCALING4[i / 3],
            None if i % 3 == 0 => fall4[i / 3],
            None => lists4[i - 1],
        };
    }
    let lists8 = [0, 1].map(|k| match sent8[k] {
        Some(ScalingList::List(l)) => l,
        Some(ScalingList::Default) => DEFAULT_SCALING8[k],
        None => fall8[k],
    });
    (lists4, lists8)
}

const HIGH_PROFILES: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

fn parse_hrd(r: &mut BitReader) -> Result<()> {
    let cpb_cnt = r.ue_max(31, "cpb_cnt_minus1")? + 1;
    r.u(4)?;
    r.u(4)?;
    for _ in 0..cpb_cnt {
        r.ue()?;
        r.ue()?;
        r.flag()?;
    }
    r.u(5)?;
    r.u(5)?;
    r.u(5)?;
    r.u(5)?;
    Ok(())
}

fn parse_vui(r: &mut BitReader) -> Result<Vui> {
    let mut v = Vui { colour_primaries: 2, transfer_characteristics: 2, matrix_coefficients: 2, ..Default::default() };
    if r.flag()? {
        // aspect_ratio_info_present_flag
        let idc = r.u(8)?;
        if idc == 255 {
            r.u(16)?;
            r.u(16)?;
        }
    }
    if r.flag()? {
        // overscan_info_present_flag
        r.flag()?;
    }
    if r.flag()? {
        // video_signal_type_present_flag
        r.u(3)?;
        v.video_full_range = r.flag()?;
        if r.flag()? {
            v.colour_primaries = r.u(8)? as u8;
            v.transfer_characteristics = r.u(8)? as u8;
            v.matrix_coefficients = r.u(8)? as u8;
        }
    }
    if r.flag()? {
        // chroma_loc_info_present_flag
        r.ue()?;
        r.ue()?;
    }
    if r.flag()? {
        // timing_info_present_flag
        r.u(32)?;
        r.u(32)?;
        r.flag()?;
    }
    let nal_hrd = r.flag()?;
    if nal_hrd {
        parse_hrd(r)?;
    }
    let vcl_hrd = r.flag()?;
    if vcl_hrd {
        parse_hrd(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.flag()?; // low_delay_hrd_flag
    }
    r.flag()?; // pic_struct_present_flag
    if r.flag()? {
        // bitstream_restriction_flag: its fields are read only to check
        // that they are all there (a VUI cut short is left out)
        r.flag()?;
        for _ in 0..6 {
            r.ue()?;
        }
    }
    Ok(v)
}

/// Parse a `seq_parameter_set_rbsp` (the NAL payload without its header,
/// emulation prevention removed).
pub fn parse_sps(rbsp: &[u8]) -> Result<Sps> {
    let mut r = BitReader::new(rbsp);
    let profile_idc = r.u(8)? as u8;
    let constraint_flags = r.u(8)? as u8;
    let level_idc = r.u(8)? as u8;
    let id = r.ue_max(31, "seq_parameter_set_id")?;
    // Flat_4x4_16 and Flat_8x8_16 unless the sequence sends lists
    let mut scaling4 = [[16; 16]; 6];
    let mut scaling8 = [[16; 64]; 2];
    let mut scaling_present = false;
    if HIGH_PROFILES.contains(&profile_idc) {
        let chroma_format_idc = r.ue_max(3, "chroma_format_idc")?;
        if chroma_format_idc == 3 {
            r.flag()?; // separate_colour_plane_flag
        }
        if chroma_format_idc != 1 {
            return Err(Error::Unsupported("only 4:2:0 chroma is supported"));
        }
        let bd_luma = r.ue_max(6, "bit_depth_luma_minus8")?;
        let bd_chroma = r.ue_max(6, "bit_depth_chroma_minus8")?;
        if bd_luma != 0 || bd_chroma != 0 {
            return Err(Error::Unsupported("only 8-bit video is supported"));
        }
        if r.flag()? {
            return Err(Error::Unsupported("lossless (transform bypass) coding"));
        }
        scaling_present = r.flag()?;
        if scaling_present {
            // seq_scaling_matrix_present_flag: fall-back rule A
            let (sent4, sent8) = parse_scaling_lists(&mut r, 8)?;
            (scaling4, scaling8) = resolve(&sent4, &sent8, DEFAULT_SCALING4, DEFAULT_SCALING8);
        }
    }
    let log2_max_frame_num = r.ue_max(12, "log2_max_frame_num_minus4")? + 4;
    let poc_type = r.ue_max(2, "pic_order_cnt_type")?;
    let mut log2_max_poc_lsb = 0;
    let mut delta_pic_order_always_zero = false;
    let mut offset_for_non_ref_pic = 0;
    let mut offset_for_top_to_bottom_field = 0;
    let mut offset_for_ref_frame = Vec::new();
    if poc_type == 0 {
        log2_max_poc_lsb = r.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? + 4;
    } else if poc_type == 1 {
        delta_pic_order_always_zero = r.flag()?;
        offset_for_non_ref_pic = r.se()?;
        offset_for_top_to_bottom_field = r.se()?;
        let n = r.ue_max(255, "num_ref_frames_in_pic_order_cnt_cycle")?;
        for _ in 0..n {
            offset_for_ref_frame.push(r.se()?);
        }
    }
    let max_num_ref_frames = r.ue_max(16, "max_num_ref_frames")?;
    let gaps_in_frame_num_allowed = r.flag()?;
    let width_mbs = r.ue_max(1023, "pic_width_in_mbs_minus1")? + 1;
    let height_map_units = r.ue_max(1023, "pic_height_in_map_units_minus1")? + 1;
    let frame_mbs_only = r.flag()?;
    let height_mbs = height_map_units * if frame_mbs_only { 1 } else { 2 };
    // the largest pictures any level allows (Table A-1, level 6.2: MaxFS
    // macroblocks, at most sqrt(8 * MaxFS) wide or high), which also bounds
    // what a damaged parameter set can make the decoder allocate
    if width_mbs > 1055 || height_mbs > 1055 || width_mbs * height_mbs > 139_264 {
        return Err(Error::Unsupported("picture size"));
    }
    let mbaff = if frame_mbs_only { false } else { r.flag()? };
    let direct_8x8_inference = r.flag()?;
    let mut crop = (0, 0, 0, 0);
    if r.flag()? {
        // frame_cropping_flag; CropUnitX = 2, CropUnitY = 2 * (2 - frame_mbs_only_flag) for 4:2:0
        // (the offsets bounded as HEVC bounds its conformance window, so
        // that they in samples and their sums below cannot wrap)
        let cy = if frame_mbs_only { 2 } else { 4 };
        let mut offset = |unit: u32| r.ue_max(8192, "frame_crop_offset").map(|v| v * unit);
        crop = (offset(2)?, offset(2)?, offset(cy)?, offset(cy)?);
    }
    let vui = if r.flag()? { parse_vui(&mut r).ok() } else { None };
    let sps = Sps {
        id,
        profile_idc,
        constraint_flags,
        level_idc,
        log2_max_frame_num,
        poc_type,
        log2_max_poc_lsb,
        delta_pic_order_always_zero,
        offset_for_non_ref_pic,
        offset_for_top_to_bottom_field,
        offset_for_ref_frame,
        max_num_ref_frames,
        gaps_in_frame_num_allowed,
        width_mbs,
        height_mbs,
        frame_mbs_only,
        mbaff,
        direct_8x8_inference,
        crop,
        scaling_present,
        scaling4,
        scaling8,
        vui,
    };
    if sps.crop.0 + sps.crop.1 >= sps.width() || sps.crop.2 + sps.crop.3 >= sps.height() {
        return Err(Error::Bitstream("frame cropping larger than the picture"));
    }
    Ok(sps)
}

/// Parse a `pic_parameter_set_rbsp`.
pub fn parse_pps(rbsp: &[u8]) -> Result<Pps> {
    let mut r = BitReader::new(rbsp);
    let id = r.ue_max(255, "pic_parameter_set_id")?;
    let sps_id = r.ue_max(31, "seq_parameter_set_id")?;
    let entropy_coding_mode = r.flag()?;
    let bottom_field_pic_order_in_frame_present = r.flag()?;
    let num_slice_groups = r.ue_max(7, "num_slice_groups_minus1")? + 1;
    if num_slice_groups > 1 {
        return Err(Error::Unsupported("slice groups (flexible macroblock ordering)"));
    }
    let l0 = r.ue_max(31, "num_ref_idx_l0_default_active_minus1")? + 1;
    let l1 = r.ue_max(31, "num_ref_idx_l1_default_active_minus1")? + 1;
    let weighted_pred = r.flag()?;
    let weighted_bipred_idc = r.u(2)?;
    let pic_init_qp = 26 + r.se()?;
    let _pic_init_qs = 26 + r.se()?;
    let cqo = r.se()?;
    if !(-12..=12).contains(&cqo) || !(0..=51).contains(&pic_init_qp) {
        return Err(Error::Bitstream("PPS values out of range"));
    }
    let deblocking_filter_control_present = r.flag()?;
    let constrained_intra_pred = r.flag()?;
    let redundant_pic_cnt_present = r.flag()?;
    let mut transform_8x8_mode = false;
    let mut scaling_present = false;
    let (mut sent4, mut sent8) = ([None; 6], [None; 2]);
    let mut second_cqo = cqo;
    if r.more_rbsp_data() {
        transform_8x8_mode = r.flag()?;
        scaling_present = r.flag()?;
        if scaling_present {
            (sent4, sent8) = parse_scaling_lists(&mut r, 6 + if transform_8x8_mode { 2 } else { 0 })?;
        }
        second_cqo = r.se()?;
        if !(-12..=12).contains(&second_cqo) {
            return Err(Error::Bitstream("second_chroma_qp_index_offset out of range"));
        }
    }
    Ok(Pps {
        id,
        sps_id,
        entropy_coding_mode,
        bottom_field_pic_order_in_frame_present,
        num_ref_idx_default_active: [l0, l1],
        weighted_pred,
        weighted_bipred_idc,
        pic_init_qp,
        chroma_qp_index_offset: [cqo, second_cqo],
        deblocking_filter_control_present,
        constrained_intra_pred,
        redundant_pic_cnt_present,
        transform_8x8_mode,
        scaling_present,
        sent4,
        sent8,
    })
}

impl ScalingTables {
    /// Resolve the picture's scaling lists and build the LevelScale tables.
    /// Lists the PPS leaves out follow fall-back rule A (the defaults) when
    /// the SPS carries no matrix and rule B (the SPS lists) when it does
    /// (7.4.2.2, Table 7-2).
    pub fn new(sps: &Sps, pps: &Pps) -> ScalingTables {
        let (lists4, lists8) = if !pps.scaling_present {
            (sps.scaling4, sps.scaling8)
        } else if sps.scaling_present {
            resolve(&pps.sent4, &pps.sent8, [sps.scaling4[0], sps.scaling4[3]], sps.scaling8)
        } else {
            resolve(&pps.sent4, &pps.sent8, DEFAULT_SCALING4, DEFAULT_SCALING8)
        };
        let mut level4 = [[[0i32; 16]; 6]; 6];
        let mut level8 = [[[0i32; 64]; 2]; 6];
        for m in 0..6 {
            let v = DEQUANT4_INIT[m];
            for (list, table) in lists4.iter().enumerate() {
                for k in 0..16 {
                    let pos = ZIGZAG4X4[k] as usize;
                    let (i, j) = (pos / 4, pos % 4);
                    // the reference's table holds (v0, v2, v1) of Table 8-14
                    let norm = if i % 2 == 0 && j % 2 == 0 {
                        v[0]
                    } else if i % 2 == 1 && j % 2 == 1 {
                        v[2]
                    } else {
                        v[1]
                    };
                    level4[m][list][pos] = table[k] as i32 * norm as i32;
                }
            }
            let v = DEQUANT8_INIT[m];
            for (list, table) in lists8.iter().enumerate() {
                for k in 0..64 {
                    let pos = ZIGZAG8X8[k] as usize;
                    let (i, j) = (pos / 8, pos % 8);
                    let norm = if i % 4 == 0 && j % 4 == 0 {
                        v[0]
                    } else if i % 2 == 1 && j % 2 == 1 {
                        v[1]
                    } else if i % 4 == 2 && j % 4 == 2 {
                        v[2]
                    } else if (i % 4 == 0 && j % 2 == 1) || (i % 2 == 1 && j % 4 == 0) {
                        v[3]
                    } else if (i % 4 == 0 && j % 4 == 2) || (i % 4 == 2 && j % 4 == 0) {
                        v[4]
                    } else {
                        v[5]
                    };
                    level8[m][list][pos] = table[k] as i32 * norm as i32;
                }
            }
        }
        ScalingTables { level4, level8 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_level_scale_matches_the_normadjust_tables() {
        let sps = Sps {
            id: 0,
            profile_idc: 66,
            constraint_flags: 0,
            level_idc: 30,
            log2_max_frame_num: 4,
            poc_type: 2,
            log2_max_poc_lsb: 0,
            delta_pic_order_always_zero: false,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            offset_for_ref_frame: vec![],
            max_num_ref_frames: 1,
            gaps_in_frame_num_allowed: false,
            width_mbs: 4,
            height_mbs: 3,
            frame_mbs_only: true,
            mbaff: false,
            direct_8x8_inference: true,
            crop: (0, 0, 0, 0),
            scaling_present: false,
            scaling4: [[16; 16]; 6],
            scaling8: [[16; 64]; 2],
            vui: None,
        };
        let pps = Pps {
            id: 0,
            sps_id: 0,
            entropy_coding_mode: false,
            bottom_field_pic_order_in_frame_present: false,
            num_ref_idx_default_active: [1, 1],
            weighted_pred: false,
            weighted_bipred_idc: 0,
            pic_init_qp: 26,
            chroma_qp_index_offset: [0, 0],
            deblocking_filter_control_present: false,
            constrained_intra_pred: false,
            redundant_pic_cnt_present: false,
            transform_8x8_mode: false,
            scaling_present: false,
            sent4: [None; 6],
            sent8: [None; 2],
        };
        let t = ScalingTables::new(&sps, &pps);
        // qp%6 = 0: v = 10, 16, 13 -> ×16
        assert_eq!(t.level4[0][0][0], 160);
        assert_eq!(t.level4[0][0][5], 256);
        assert_eq!(t.level4[0][0][1], 208);
        assert_eq!(t.level8[0][0][0], 320);
        assert_eq!(t.level8[0][0][9], 288);
        assert_eq!(t.level8[0][0][18], 512);
    }
}
