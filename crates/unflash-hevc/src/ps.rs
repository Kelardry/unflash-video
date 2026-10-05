//! Sequence and picture parameter sets (7.3.2.2, 7.3.2.3), with the parts
//! of the profile, VUI and scaling list syntax they carry, the short-term
//! reference picture sets (7.3.7, 7.4.8), and the picture layout derived
//! from an SPS and PPS pair (tiles, 6.5.1).

use crate::bitreader::BitReader;
use crate::tables::{DEFAULT_INTER_8X8, DEFAULT_INTRA_8X8, DIAG4, DIAG8};
use crate::{Error, Result};

/// Most entries a short-term reference picture set can hold (the largest
/// DPB of any level is 16 pictures, the current one included).
pub const MAX_ST_REFS: usize = 16;

/// A short-term reference picture set (7.4.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StRps {
    pub num_negative: usize,
    pub num_positive: usize,
    /// DeltaPocS0 (decreasing, negative) followed by DeltaPocS1
    /// (increasing, positive).
    pub delta_poc: [i32; MAX_ST_REFS],
    /// UsedByCurrPicS0 followed by UsedByCurrPicS1.
    pub used: [bool; MAX_ST_REFS],
}

impl Default for StRps {
    fn default() -> Self {
        StRps { num_negative: 0, num_positive: 0, delta_poc: [0; MAX_ST_REFS], used: [false; MAX_ST_REFS] }
    }
}

impl StRps {
    /// NumDeltaPocs.
    pub fn len(&self) -> usize {
        self.num_negative + self.num_positive
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The pictures of this set the current picture may reference.
    pub fn num_used(&self) -> usize {
        self.used[..self.len()].iter().filter(|&&u| u).count()
    }
}

/// 7.3.7: `st_ref_pic_set( idx )`; `sets` holds the sets parsed before it
/// (the SPS's sets, for a set in a slice header).
pub fn parse_st_rps(r: &mut BitReader, idx: usize, sets: &[StRps], in_slice_header: bool) -> Result<StRps> {
    let mut rps = StRps::default();
    let inter_rps_pred = idx != 0 && r.flag()?;
    if inter_rps_pred {
        let delta_idx = if in_slice_header { r.ue_max(idx as u32 - 1, "delta_idx_minus1")? as usize + 1 } else { 1 };
        let ref_rps = sets.get(idx.wrapping_sub(delta_idx)).ok_or(Error::Bitstream("inter RPS prediction from a missing set"))?;
        let sign = r.flag()?;
        let abs = r.ue_max(1 << 15, "abs_delta_rps_minus1")? as i32 + 1;
        let delta_rps = if sign { -abs } else { abs };
        let n = ref_rps.len();
        let mut used_by_curr = [false; MAX_ST_REFS + 1];
        let mut use_delta = [true; MAX_ST_REFS + 1];
        for j in 0..=n {
            used_by_curr[j] = r.flag()?;
            if !used_by_curr[j] {
                use_delta[j] = r.flag()?;
            }
        }
        // (7-61), (7-62): the sets of the reference shifted by deltaRps
        let neg = ref_rps.num_negative;
        let pos = ref_rps.num_positive;
        let mut out_poc = [0i32; 2 * MAX_ST_REFS + 2];
        let mut out_used = [false; 2 * MAX_ST_REFS + 2];
        let mut i = 0;
        for j in (0..pos).rev() {
            let d = ref_rps.delta_poc[neg + j] + delta_rps;
            if d < 0 && use_delta[neg + j] {
                out_poc[i] = d;
                out_used[i] = used_by_curr[neg + j];
                i += 1;
            }
        }
        if delta_rps < 0 && use_delta[n] {
            out_poc[i] = delta_rps;
            out_used[i] = used_by_curr[n];
            i += 1;
        }
        for j in 0..neg {
            let d = ref_rps.delta_poc[j] + delta_rps;
            if d < 0 && use_delta[j] {
                out_poc[i] = d;
                out_used[i] = used_by_curr[j];
                i += 1;
            }
        }
        let num_negative = i;
        for j in (0..neg).rev() {
            let d = ref_rps.delta_poc[j] + delta_rps;
            if d > 0 && use_delta[j] {
                out_poc[i] = d;
                out_used[i] = used_by_curr[j];
                i += 1;
            }
        }
        if delta_rps > 0 && use_delta[n] {
            out_poc[i] = delta_rps;
            out_used[i] = used_by_curr[n];
            i += 1;
        }
        for j in 0..pos {
            let d = ref_rps.delta_poc[neg + j] + delta_rps;
            if d > 0 && use_delta[neg + j] {
                out_poc[i] = d;
                out_used[i] = used_by_curr[neg + j];
                i += 1;
            }
        }
        if i > MAX_ST_REFS {
            return Err(Error::Bitstream("short-term reference picture set too large"));
        }
        rps.num_negative = num_negative;
        rps.num_positive = i - num_negative;
        rps.delta_poc[..i].copy_from_slice(&out_poc[..i]);
        rps.used[..i].copy_from_slice(&out_used[..i]);
    } else {
        let neg = r.ue_max(MAX_ST_REFS as u32, "num_negative_pics")? as usize;
        let pos = r.ue_max((MAX_ST_REFS - neg) as u32, "num_positive_pics")? as usize;
        let mut poc = 0i32;
        for i in 0..neg {
            poc -= r.ue_max(1 << 15, "delta_poc_s0_minus1")? as i32 + 1;
            rps.delta_poc[i] = poc;
            rps.used[i] = r.flag()?;
        }
        poc = 0;
        for i in neg..neg + pos {
            poc += r.ue_max(1 << 15, "delta_poc_s1_minus1")? as i32 + 1;
            rps.delta_poc[i] = poc;
            rps.used[i] = r.flag()?;
        }
        rps.num_negative = neg;
        rps.num_positive = pos;
    }
    Ok(rps)
}

/// Scaling lists as signalled (7.3.4) or defaulted (Table 7-5 / 7-6):
/// `ScalingList[sizeId][matrixId]` in diagonal scan order, and the DC values
/// of the 16x16 and 32x32 lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScalingList {
    pub lists: [[[u8; 64]; 6]; 4],
    pub dc: [[u8; 6]; 2],
}

impl Default for ScalingList {
    /// The default lists (`scaling_list_enabled_flag` without data).
    fn default() -> Self {
        let mut lists = [[[16u8; 64]; 6]; 4];
        for size in lists.iter_mut().skip(1) {
            for (m, list) in size.iter_mut().enumerate() {
                *list = if m < 3 { DEFAULT_INTRA_8X8 } else { DEFAULT_INTER_8X8 };
            }
        }
        ScalingList { lists, dc: [[16; 6]; 2] }
    }
}

/// 7.3.4: `scaling_list_data( )`.
fn parse_scaling_list(r: &mut BitReader) -> Result<ScalingList> {
    let mut sl = ScalingList::default();
    for size in 0..4 {
        let step = if size == 3 { 3 } else { 1 };
        let mut m = 0;
        while m < 6 {
            let coefs = if size == 0 { 16 } else { 64 };
            if !r.flag()? {
                // scaling_list_pred_matrix_id_delta
                let delta = r.ue_max((m / step) as u32, "scaling_list_pred_matrix_id_delta")? as usize * step;
                if delta != 0 {
                    let from = m - delta;
                    sl.lists[size][m] = sl.lists[size][from];
                    if size > 1 {
                        sl.dc[size - 2][m] = sl.dc[size - 2][from];
                    }
                } else {
                    sl.lists[size][m] = if size == 0 {
                        [16; 64]
                    } else if m < 3 {
                        DEFAULT_INTRA_8X8
                    } else {
                        DEFAULT_INTER_8X8
                    };
                    if size > 1 {
                        sl.dc[size - 2][m] = 16;
                    }
                }
            } else {
                let mut next = 8i32;
                if size > 1 {
                    let dc = r.se_range(-7, 247, "scaling_list_dc_coef_minus8")?;
                    next = dc + 8;
                    sl.dc[size - 2][m] = next as u8;
                }
                for i in 0..coefs {
                    let delta = r.se_range(-128, 127, "scaling_list_delta_coef")?;
                    next = (next + delta + 256).rem_euclid(256);
                    if next == 0 {
                        return Err(Error::Bitstream("scaling list coefficient of zero"));
                    }
                    sl.lists[size][m][i] = next as u8;
                }
            }
            m += step;
        }
    }
    // 32x32 chroma lists (4:4:4 only) come from the 16x16 ones (7-50, 7-51)
    for m in [1, 2, 4, 5] {
        sl.lists[3][m] = sl.lists[2][m];
        sl.dc[1][m] = sl.dc[0][m];
    }
    Ok(sl)
}

/// ScalingFactor (7.4.5) for every transform size and matrixId, in raster
/// order (`y * size + x`).
#[derive(Clone, Debug)]
pub struct ScalingFactors {
    pub m4: [[u8; 16]; 6],
    pub m8: [[u8; 64]; 6],
    pub m16: Vec<[u8; 256]>,
    pub m32: Vec<[u8; 1024]>,
}

impl ScalingFactors {
    pub fn new(sl: &ScalingList) -> ScalingFactors {
        let mut f = ScalingFactors { m4: [[0; 16]; 6], m8: [[0; 64]; 6], m16: vec![[0; 256]; 6], m32: vec![[0; 1024]; 6] };
        for m in 0..6 {
            for (i, &(x, y)) in DIAG4.iter().enumerate() {
                f.m4[m][y as usize * 4 + x as usize] = sl.lists[0][m][i];
            }
            for (i, &(x, y)) in DIAG8.iter().enumerate() {
                let (x, y) = (x as usize, y as usize);
                f.m8[m][y * 8 + x] = sl.lists[1][m][i];
                for j in 0..2 {
                    for k in 0..2 {
                        f.m16[m][(y * 2 + j) * 16 + x * 2 + k] = sl.lists[2][m][i];
                    }
                }
                for j in 0..4 {
                    for k in 0..4 {
                        f.m32[m][(y * 4 + j) * 32 + x * 4 + k] = sl.lists[3][m][i];
                    }
                }
            }
            f.m16[m][0] = sl.dc[0][m];
            f.m32[m][0] = sl.dc[1][m];
        }
        f
    }

    /// The factors of one transform block size (log2 2..=5) and matrixId.
    pub fn get(&self, log2: usize, matrix: usize) -> &[u8] {
        match log2 {
            2 => &self.m4[matrix],
            3 => &self.m8[matrix],
            4 => &self.m16[matrix],
            _ => &self.m32[matrix],
        }
    }
}

/// The part of the VUI the decoder reports (colour conversion).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Vui {
    pub video_full_range: bool,
    /// colour_primaries, transfer_characteristics and matrix_coeffs (2 =
    /// unspecified when not signalled).
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coeffs: u8,
}

/// A sequence parameter set (7.3.2.2), as far as decoding needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sps {
    pub id: u32,
    pub general_profile_idc: u8,
    /// 0 = 4:0:0, 1 = 4:2:0.
    pub chroma_format_idc: u32,
    pub width: u32,
    pub height: u32,
    /// Conformance window in luma samples: left, right, top, bottom.
    pub conf_win: (u32, u32, u32, u32),
    pub bit_depth: u32,
    pub bit_depth_chroma: u32,
    pub log2_max_poc_lsb: u32,
    pub log2_min_cb: u32,
    pub log2_ctb: u32,
    pub log2_min_tb: u32,
    pub log2_max_tb: u32,
    pub max_transform_hierarchy_depth_inter: u32,
    pub max_transform_hierarchy_depth_intra: u32,
    pub scaling_list_enabled: bool,
    /// The SPS's lists (the defaults when none are signalled).
    pub scaling_list: ScalingList,
    pub amp_enabled: bool,
    pub sao_enabled: bool,
    pub pcm_enabled: bool,
    pub pcm_bit_depth: u32,
    pub pcm_bit_depth_chroma: u32,
    pub log2_min_pcm_cb: u32,
    pub log2_max_pcm_cb: u32,
    pub pcm_loop_filter_disabled: bool,
    pub st_rps: Vec<StRps>,
    pub long_term_refs_present: bool,
    /// lt_ref_pic_poc_lsb_sps and used_by_curr_pic_lt_sps_flag.
    pub lt_ref_pics: Vec<(u32, bool)>,
    pub temporal_mvp_enabled: bool,
    pub strong_intra_smoothing: bool,
    pub vui: Option<Vui>,
    /// sps_max_num_reorder_pics of the highest sub-layer: how many pictures
    /// may come before one in decoding order and after it in output order.
    pub max_num_reorder_pics: u32,
}

impl Sps {
    pub fn ctb_size(&self) -> u32 {
        1 << self.log2_ctb
    }
    pub fn width_ctbs(&self) -> u32 {
        self.width.div_ceil(self.ctb_size())
    }
    pub fn height_ctbs(&self) -> u32 {
        self.height.div_ceil(self.ctb_size())
    }
    /// The picture size after the conformance window.
    pub fn cropped_size(&self) -> (u32, u32) {
        let (l, r, t, b) = self.conf_win;
        (self.width - l - r, self.height - t - b)
    }
    /// QpBdOffsetY (the chroma offset is the same: both depths are equal).
    pub fn qp_bd_offset(&self) -> i32 {
        6 * (self.bit_depth as i32 - 8)
    }
}

/// 7.3.3: `profile_tier_level( 1, maxNumSubLayersMinus1 )`; returns
/// general_profile_idc.
fn parse_profile_tier_level(r: &mut BitReader, max_sub_layers_minus1: u32) -> Result<u8> {
    r.u(3)?; // general_profile_space, general_tier_flag
    let profile = r.u(5)? as u8;
    // compatibility, source and constraint flags, general_level_idc
    r.skip(32 + 4 + 43 + 1 + 8)?;
    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        profile_present[i] = r.flag()?;
        level_present[i] = r.flag()?;
    }
    if max_sub_layers_minus1 > 0 {
        r.skip(2 * (8 - max_sub_layers_minus1 as usize))?;
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if profile_present[i] {
            r.skip(88)?;
        }
        if level_present[i] {
            r.skip(8)?;
        }
    }
    Ok(profile)
}

/// E.2.3: `sub_layer_hrd_parameters( )`.
fn skip_sub_layer_hrd(r: &mut BitReader, cpb_cnt: u32, sub_pic: bool) -> Result<()> {
    for _ in 0..cpb_cnt {
        r.ue()?;
        r.ue()?;
        if sub_pic {
            r.ue()?;
            r.ue()?;
        }
        r.flag()?;
    }
    Ok(())
}

/// E.2.2: `hrd_parameters( 1, maxNumSubLayersMinus1 )`, skipped.
fn skip_hrd(r: &mut BitReader, max_sub_layers_minus1: u32) -> Result<()> {
    let nal = r.flag()?;
    let vcl = r.flag()?;
    let mut sub_pic = false;
    if nal || vcl {
        sub_pic = r.flag()?;
        if sub_pic {
            r.skip(8 + 5 + 1 + 5)?;
        }
        r.skip(4 + 4)?;
        if sub_pic {
            r.skip(4)?;
        }
        r.skip(5 + 5 + 5)?;
    }
    for _ in 0..=max_sub_layers_minus1 {
        let fixed_general = r.flag()?;
        let fixed_within_cvs = fixed_general || r.flag()?;
        let mut low_delay = false;
        if fixed_within_cvs {
            r.ue()?;
        } else {
            low_delay = r.flag()?;
        }
        let cpb_cnt = if low_delay { 1 } else { r.ue_max(31, "cpb_cnt_minus1")? + 1 };
        if nal {
            skip_sub_layer_hrd(r, cpb_cnt, sub_pic)?;
        }
        if vcl {
            skip_sub_layer_hrd(r, cpb_cnt, sub_pic)?;
        }
    }
    Ok(())
}

/// The VUI from `timing_info` on; `alt` reads the draft syntax some old
/// encoders wrote, without the default display window (as ffmpeg retries).
fn parse_vui_tail(r: &mut BitReader, max_sub_layers_minus1: u32, alt: bool) -> Result<()> {
    if !alt {
        // default_display_window_flag, unless the next bits look like the
        // draft syntax (as ffmpeg checks)
        let looks_invalid = r.bits_left() >= 68 && r.peek(21) == Some(0x100000);
        if !looks_invalid && r.flag()? {
            for _ in 0..4 {
                r.ue()?;
            }
        }
    }
    if r.flag()? {
        // vui_timing_info_present_flag
        r.skip(64)?;
        if r.flag()? {
            r.ue()?;
        }
        if r.flag()? {
            skip_hrd(r, max_sub_layers_minus1)?;
        }
    }
    if r.flag()? {
        // bitstream_restriction_flag
        r.skip(3)?;
        for _ in 0..5 {
            r.ue()?;
        }
    }
    Ok(())
}

/// E.2.1: `vui_parameters( )`.
fn parse_vui(r: &mut BitReader, max_sub_layers_minus1: u32) -> Result<Vui> {
    let mut v = Vui { colour_primaries: 2, transfer_characteristics: 2, matrix_coeffs: 2, ..Default::default() };
    if r.flag()? {
        // aspect_ratio_info_present_flag
        if r.u(8)? == 255 {
            r.skip(32)?;
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
            v.matrix_coeffs = r.u(8)? as u8;
        }
    }
    if r.flag()? {
        // chroma_loc_info_present_flag
        r.ue()?;
        r.ue()?;
    }
    r.skip(3)?; // neutral_chroma_indication_flag, field_seq_flag, frame_field_info_present_flag
    let pos = r.bit_pos();
    let ok = parse_vui_tail(r, max_sub_layers_minus1, false).is_ok() && r.bits_left() >= 1;
    if !ok {
        r.seek(pos);
        parse_vui_tail(r, max_sub_layers_minus1, true)?;
    }
    Ok(v)
}

/// 7.3.2.2: parse a sequence parameter set RBSP (after the two-byte NAL
/// unit header).
pub fn parse_sps(rbsp: &[u8]) -> Result<Sps> {
    let mut r = BitReader::new(rbsp);
    r.u(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.u(3)?;
    if max_sub_layers_minus1 > 6 {
        return Err(Error::Bitstream("sps_max_sub_layers_minus1"));
    }
    r.flag()?; // sps_temporal_id_nesting_flag
    let general_profile_idc = parse_profile_tier_level(&mut r, max_sub_layers_minus1)?;
    let id = r.ue_max(15, "sps_seq_parameter_set_id")?;
    let chroma_format_idc = r.ue_max(3, "chroma_format_idc")?;
    if chroma_format_idc == 3 && r.flag()? {
        return Err(Error::Unsupported("separate colour planes"));
    }
    if chroma_format_idc >= 2 {
        return Err(Error::Unsupported("4:2:2 or 4:4:4 chroma"));
    }
    let width = r.ue()?;
    let height = r.ue()?;
    // the largest pictures any level allows (6.2: MaxLumaPs, and at most
    // sqrt(8 * MaxLumaPs) wide or high), which also bounds the memory a
    // damaged parameter set can make the decoder allocate
    if width == 0 || height == 0 || width > 16888 || height > 16888 || width as u64 * height as u64 > 35_651_584 {
        return Err(Error::Unsupported("picture size"));
    }
    let (sub_w, sub_h) = if chroma_format_idc == 1 { (2, 2) } else { (1, 1) };
    let mut conf_win = (0, 0, 0, 0);
    if r.flag()? {
        let l = r.ue_max(8192, "conf_win_left_offset")? * sub_w;
        let rt = r.ue_max(8192, "conf_win_right_offset")? * sub_w;
        let t = r.ue_max(8192, "conf_win_top_offset")? * sub_h;
        let b = r.ue_max(8192, "conf_win_bottom_offset")? * sub_h;
        if l + rt >= width || t + b >= height {
            return Err(Error::Bitstream("conformance window larger than the picture"));
        }
        conf_win = (l, rt, t, b);
    }
    let bit_depth = r.ue_max(8, "bit_depth_luma_minus8")? + 8;
    let bit_depth_chroma = r.ue_max(8, "bit_depth_chroma_minus8")? + 8;
    if bit_depth > 12 || (chroma_format_idc != 0 && bit_depth_chroma != bit_depth) {
        return Err(Error::Unsupported("bit depth (more than 12 bits, or chroma differing from luma)"));
    }
    let log2_max_poc_lsb = r.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? + 4;
    let ordering_info_present = r.flag()?;
    let first = if ordering_info_present { 0 } else { max_sub_layers_minus1 };
    // the buffer size and latency (pictures are output as decoded); the
    // reordering of the highest sub-layer is what a caller putting them in
    // presentation order has to allow for
    let mut max_num_reorder_pics = 0;
    for _ in first..=max_sub_layers_minus1 {
        r.ue_max(15, "sps_max_dec_pic_buffering_minus1")?;
        max_num_reorder_pics = r.ue_max(15, "sps_max_num_reorder_pics")?;
        r.ue()?;
    }
    let log2_min_cb = r.ue_max(3, "log2_min_luma_coding_block_size_minus3")? + 3;
    let log2_ctb = log2_min_cb + r.ue_max(3, "log2_diff_max_min_luma_coding_block_size")?;
    if log2_ctb > 6 {
        return Err(Error::Bitstream("coding tree block larger than 64x64"));
    }
    let log2_min_tb = r.ue_max(3, "log2_min_luma_transform_block_size_minus2")? + 2;
    let log2_max_tb = log2_min_tb + r.ue_max(3, "log2_diff_max_min_luma_transform_block_size")?;
    if log2_min_tb >= log2_min_cb || log2_max_tb > log2_ctb.min(5) {
        return Err(Error::Bitstream("transform block sizes"));
    }
    if width % (1 << log2_min_cb) != 0 || height % (1 << log2_min_cb) != 0 {
        return Err(Error::Bitstream("picture size not a multiple of the minimum coding block"));
    }
    let max_transform_hierarchy_depth_inter = r.ue_max(log2_ctb - log2_min_tb, "max_transform_hierarchy_depth_inter")?;
    let max_transform_hierarchy_depth_intra = r.ue_max(log2_ctb - log2_min_tb, "max_transform_hierarchy_depth_intra")?;
    let scaling_list_enabled = r.flag()?;
    let mut scaling_list = ScalingList::default();
    if scaling_list_enabled && r.flag()? {
        scaling_list = parse_scaling_list(&mut r)?;
    }
    let amp_enabled = r.flag()?;
    let sao_enabled = r.flag()?;
    let pcm_enabled = r.flag()?;
    let (mut pcm_bit_depth, mut pcm_bit_depth_chroma, mut log2_min_pcm_cb, mut log2_max_pcm_cb, mut pcm_loop_filter_disabled) = (8, 8, 3, 3, false);
    if pcm_enabled {
        pcm_bit_depth = r.u(4)? + 1;
        pcm_bit_depth_chroma = r.u(4)? + 1;
        log2_min_pcm_cb = r.ue_max(2, "log2_min_pcm_luma_coding_block_size_minus3")? + 3;
        log2_max_pcm_cb = log2_min_pcm_cb + r.ue_max(2, "log2_diff_max_min_pcm_luma_coding_block_size")?;
        pcm_loop_filter_disabled = r.flag()?;
        if pcm_bit_depth > bit_depth || pcm_bit_depth_chroma > bit_depth_chroma || log2_max_pcm_cb > log2_ctb.min(5) {
            return Err(Error::Bitstream("PCM parameters"));
        }
    }
    let num_st_rps = r.ue_max(64, "num_short_term_ref_pic_sets")? as usize;
    let mut st_rps = Vec::with_capacity(num_st_rps);
    for i in 0..num_st_rps {
        let rps = parse_st_rps(&mut r, i, &st_rps, false)?;
        st_rps.push(rps);
    }
    let long_term_refs_present = r.flag()?;
    let mut lt_ref_pics = Vec::new();
    if long_term_refs_present {
        let n = r.ue_max(32, "num_long_term_ref_pics_sps")?;
        for _ in 0..n {
            let lsb = r.u(log2_max_poc_lsb)?;
            let used = r.flag()?;
            lt_ref_pics.push((lsb, used));
        }
    }
    let temporal_mvp_enabled = r.flag()?;
    let strong_intra_smoothing = r.flag()?;
    let vui = if r.flag()? { Some(parse_vui(&mut r, max_sub_layers_minus1)?) } else { None };
    // The extensions came with version 2 of the standard; in a sequence of
    // a version 1 profile (Main, Main 10, Main Still Picture) the tools
    // they enable must be off, and version 1 decoders ignore the extension
    // data, which some such streams fill with arbitrary bits.
    if !(1..=3).contains(&general_profile_idc) && r.flag()? {
        // sps_extension_present_flag
        let range = r.flag()?;
        let multilayer = r.flag()?;
        let ext_3d = r.flag()?;
        let scc = r.flag()?;
        r.u(4)?; // sps_extension_4bits
        if range {
            // transform_skip_rotation, transform_skip_context, implicit and
            // explicit RDPCM, extended precision, intra smoothing disabled,
            // high precision offsets, persistent Rice adaptation, CABAC
            // bypass alignment
            if r.u(9)? != 0 {
                return Err(Error::Unsupported("range extension coding tools"));
            }
        }
        if multilayer {
            r.flag()?; // inter_view_mv_vert_constraint_flag
        }
        // the 3D extension concerns other layers; nothing after it is read
        if scc && !ext_3d {
            return Err(Error::Unsupported("screen content coding"));
        }
    }
    Ok(Sps {
        id,
        general_profile_idc,
        chroma_format_idc,
        width,
        height,
        conf_win,
        bit_depth,
        bit_depth_chroma,
        log2_max_poc_lsb,
        log2_min_cb,
        log2_ctb,
        log2_min_tb,
        log2_max_tb,
        max_transform_hierarchy_depth_inter,
        max_transform_hierarchy_depth_intra,
        scaling_list_enabled,
        scaling_list,
        amp_enabled,
        sao_enabled,
        pcm_enabled,
        pcm_bit_depth,
        pcm_bit_depth_chroma,
        log2_min_pcm_cb,
        log2_max_pcm_cb,
        pcm_loop_filter_disabled,
        st_rps,
        long_term_refs_present,
        lt_ref_pics,
        temporal_mvp_enabled,
        strong_intra_smoothing,
        vui,
        max_num_reorder_pics,
    })
}

/// A picture parameter set (7.3.2.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u32,
    pub sign_data_hiding: bool,
    pub cabac_init_present: bool,
    pub num_ref_idx_default: [u32; 2],
    pub init_qp: i32,
    pub constrained_intra_pred: bool,
    pub transform_skip_enabled: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: u32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub slice_chroma_qp_offsets_present: bool,
    pub weighted_pred: bool,
    pub weighted_bipred: bool,
    pub transquant_bypass_enabled: bool,
    pub tiles_enabled: bool,
    pub entropy_coding_sync: bool,
    pub num_tile_columns: u32,
    pub num_tile_rows: u32,
    pub uniform_spacing: bool,
    /// column_width_minus1 + 1 / row_height_minus1 + 1, all but the last.
    pub column_widths: Vec<u32>,
    pub row_heights: Vec<u32>,
    pub loop_filter_across_tiles: bool,
    pub loop_filter_across_slices: bool,
    pub deblocking_override_enabled: bool,
    pub deblocking_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    /// pps_scaling_list_data_present_flag's lists.
    pub scaling_list: Option<ScalingList>,
    pub lists_modification_present: bool,
    pub log2_parallel_merge_level: u32,
    pub slice_header_extension_present: bool,
    /// Range extension: Log2MaxTransformSkipSize.
    pub log2_max_transform_skip_size: u32,
    /// Range extension: cb_qp_offset_list / cr_qp_offset_list and the
    /// depth of the chroma QP offset groups.
    pub chroma_qp_offset_list: Vec<(i32, i32)>,
    pub diff_cu_chroma_qp_offset_depth: u32,
    pub log2_sao_offset_scale_luma: u32,
    pub log2_sao_offset_scale_chroma: u32,
}

/// 7.3.2.3: parse a picture parameter set RBSP. `spss` are the sequence
/// parameter sets received so far: as ffmpeg does, the extensions are only
/// read for a sequence of a range extension (or later) profile, so the
/// arbitrary extension data version 1 streams may carry is ignored.
pub fn parse_pps(rbsp: &[u8], spss: &[Option<std::rc::Rc<Sps>>]) -> Result<Pps> {
    let mut r = BitReader::new(rbsp);
    let id = r.ue_max(63, "pps_pic_parameter_set_id")?;
    let sps_id = r.ue_max(15, "pps_seq_parameter_set_id")?;
    let dependent_slice_segments_enabled = r.flag()?;
    let output_flag_present = r.flag()?;
    let num_extra_slice_header_bits = r.u(3)?;
    let sign_data_hiding = r.flag()?;
    let cabac_init_present = r.flag()?;
    let num_ref_idx_default = [r.ue_max(14, "num_ref_idx_l0_default_active_minus1")? + 1, r.ue_max(14, "num_ref_idx_l1_default_active_minus1")? + 1];
    let init_qp = 26 + r.se_range(-(26 + 48), 25, "init_qp_minus26")?;
    let constrained_intra_pred = r.flag()?;
    let transform_skip_enabled = r.flag()?;
    let cu_qp_delta_enabled = r.flag()?;
    let diff_cu_qp_delta_depth = if cu_qp_delta_enabled { r.ue_max(3, "diff_cu_qp_delta_depth")? } else { 0 };
    let cb_qp_offset = r.se_range(-12, 12, "pps_cb_qp_offset")?;
    let cr_qp_offset = r.se_range(-12, 12, "pps_cr_qp_offset")?;
    let slice_chroma_qp_offsets_present = r.flag()?;
    let weighted_pred = r.flag()?;
    let weighted_bipred = r.flag()?;
    let transquant_bypass_enabled = r.flag()?;
    let tiles_enabled = r.flag()?;
    let entropy_coding_sync = r.flag()?;
    let (mut num_tile_columns, mut num_tile_rows, mut uniform_spacing) = (1, 1, true);
    let (mut column_widths, mut row_heights) = (Vec::new(), Vec::new());
    let mut loop_filter_across_tiles = true;
    if tiles_enabled {
        num_tile_columns = r.ue_max(1023, "num_tile_columns_minus1")? + 1;
        num_tile_rows = r.ue_max(1023, "num_tile_rows_minus1")? + 1;
        uniform_spacing = r.flag()?;
        if !uniform_spacing {
            for _ in 1..num_tile_columns {
                column_widths.push(r.ue_max(1 << 12, "column_width_minus1")? + 1);
            }
            for _ in 1..num_tile_rows {
                row_heights.push(r.ue_max(1 << 12, "row_height_minus1")? + 1);
            }
        }
        loop_filter_across_tiles = r.flag()?;
    }
    let loop_filter_across_slices = r.flag()?;
    let (mut deblocking_override_enabled, mut deblocking_disabled, mut beta_offset_div2, mut tc_offset_div2) = (false, false, 0, 0);
    if r.flag()? {
        // deblocking_filter_control_present_flag
        deblocking_override_enabled = r.flag()?;
        deblocking_disabled = r.flag()?;
        if !deblocking_disabled {
            beta_offset_div2 = r.se_range(-6, 6, "pps_beta_offset_div2")?;
            tc_offset_div2 = r.se_range(-6, 6, "pps_tc_offset_div2")?;
        }
    }
    let scaling_list = if r.flag()? { Some(parse_scaling_list(&mut r)?) } else { None };
    let lists_modification_present = r.flag()?;
    let log2_parallel_merge_level = r.ue_max(4, "log2_parallel_merge_level_minus2")? + 2;
    let slice_header_extension_present = r.flag()?;
    let mut log2_max_transform_skip_size = 2;
    let mut chroma_qp_offset_list = Vec::new();
    let mut diff_cu_chroma_qp_offset_depth = 0;
    let (mut log2_sao_offset_scale_luma, mut log2_sao_offset_scale_chroma) = (0, 0);
    let extensions = spss.get(sps_id as usize).and_then(|s| s.as_ref()).is_some_and(|s| s.general_profile_idc >= 4);
    if extensions && r.flag()? {
        // pps_extension_present_flag
        let range = r.flag()?;
        let multilayer = r.flag()?;
        let ext_3d = r.flag()?;
        let scc = r.flag()?;
        r.u(4)?; // pps_extension_4bits
        if range {
            if transform_skip_enabled {
                log2_max_transform_skip_size = r.ue_max(3, "log2_max_transform_skip_block_size_minus2")? + 2;
            }
            if r.flag()? {
                return Err(Error::Unsupported("cross-component prediction"));
            }
            if r.flag()? {
                // chroma_qp_offset_list_enabled_flag
                diff_cu_chroma_qp_offset_depth = r.ue_max(3, "diff_cu_chroma_qp_offset_depth")?;
                let n = r.ue_max(5, "chroma_qp_offset_list_len_minus1")? + 1;
                for _ in 0..n {
                    let cb = r.se_range(-12, 12, "cb_qp_offset_list")?;
                    let cr = r.se_range(-12, 12, "cr_qp_offset_list")?;
                    chroma_qp_offset_list.push((cb, cr));
                }
            }
            log2_sao_offset_scale_luma = r.ue_max(6, "log2_sao_offset_scale_luma")?;
            log2_sao_offset_scale_chroma = r.ue_max(6, "log2_sao_offset_scale_chroma")?;
        }
        // the multi-layer and 3D extensions concern other layers, and
        // nothing after them is read
        if scc && !multilayer && !ext_3d {
            return Err(Error::Unsupported("screen content coding"));
        }
    }
    Ok(Pps {
        id,
        sps_id,
        dependent_slice_segments_enabled,
        output_flag_present,
        num_extra_slice_header_bits,
        sign_data_hiding,
        cabac_init_present,
        num_ref_idx_default,
        init_qp,
        constrained_intra_pred,
        transform_skip_enabled,
        cu_qp_delta_enabled,
        diff_cu_qp_delta_depth,
        cb_qp_offset,
        cr_qp_offset,
        slice_chroma_qp_offsets_present,
        weighted_pred,
        weighted_bipred,
        transquant_bypass_enabled,
        tiles_enabled,
        entropy_coding_sync,
        num_tile_columns,
        num_tile_rows,
        uniform_spacing,
        column_widths,
        row_heights,
        loop_filter_across_tiles,
        loop_filter_across_slices,
        deblocking_override_enabled,
        deblocking_disabled,
        beta_offset_div2,
        tc_offset_div2,
        scaling_list,
        lists_modification_present,
        log2_parallel_merge_level,
        slice_header_extension_present,
        log2_max_transform_skip_size,
        chroma_qp_offset_list,
        diff_cu_chroma_qp_offset_depth,
        log2_sao_offset_scale_luma,
        log2_sao_offset_scale_chroma,
    })
}

/// What a picture needs from its SPS and PPS together: the tile layout and
/// scan conversions (6.5.1) and the scaling factors in use, checked for
/// consistency between the two.
#[derive(Debug)]
pub struct Layout {
    pub width_ctbs: u32,
    pub height_ctbs: u32,
    pub rs_to_ts: Vec<u32>,
    pub ts_to_rs: Vec<u32>,
    /// TileId per CTB, in raster order.
    pub tile_id: Vec<u16>,
    /// ScalingFactor of the PPS's lists, else the SPS's; None when scaling
    /// lists are off (flat 16).
    pub scaling: Option<ScalingFactors>,
}

impl Layout {
    pub fn new(sps: &Sps, pps: &Pps) -> Result<Layout> {
        let (w, h) = (sps.width_ctbs(), sps.height_ctbs());
        let min_qp = -(sps.qp_bd_offset());
        if pps.init_qp < min_qp || pps.diff_cu_qp_delta_depth > sps.log2_ctb - sps.log2_min_cb || pps.log2_parallel_merge_level > sps.log2_ctb {
            return Err(Error::Bitstream("picture parameters inconsistent with the sequence"));
        }
        if pps.diff_cu_chroma_qp_offset_depth > sps.log2_ctb - sps.log2_min_cb || pps.log2_max_transform_skip_size > sps.log2_max_tb.max(2) {
            return Err(Error::Bitstream("picture parameters inconsistent with the sequence"));
        }
        if pps.num_tile_columns > w || pps.num_tile_rows > h {
            return Err(Error::Bitstream("more tiles than coding tree blocks"));
        }
        let split = |n: u32, total: u32, explicit: &[u32]| -> Result<Vec<u32>> {
            let mut bd = vec![0u32; n as usize + 1];
            for i in 0..n as usize {
                let size = if pps.uniform_spacing {
                    ((i as u32 + 1) * total) / n - (i as u32 * total) / n
                } else if i + 1 < n as usize {
                    explicit[i]
                } else {
                    total.checked_sub(bd[i]).filter(|&s| s > 0).ok_or(Error::Bitstream("tile sizes exceed the picture"))?
                };
                bd[i + 1] = bd[i] + size;
                if bd[i + 1] > total {
                    return Err(Error::Bitstream("tile sizes exceed the picture"));
                }
            }
            Ok(bd)
        };
        let col_bd = split(pps.num_tile_columns, w, &pps.column_widths)?;
        let row_bd = split(pps.num_tile_rows, h, &pps.row_heights)?;
        let n = (w * h) as usize;
        let mut rs_to_ts = vec![0u32; n];
        let mut tile_id = vec![0u16; n];
        for rs in 0..n as u32 {
            let (tbx, tby) = (rs % w, rs / w);
            let tile_x = (0..pps.num_tile_columns as usize).rev().find(|&i| tbx >= col_bd[i]).unwrap_or(0);
            let tile_y = (0..pps.num_tile_rows as usize).rev().find(|&j| tby >= row_bd[j]).unwrap_or(0);
            let mut ts = 0u32;
            for i in 0..tile_x {
                ts += (row_bd[tile_y + 1] - row_bd[tile_y]) * (col_bd[i + 1] - col_bd[i]);
            }
            for j in 0..tile_y {
                ts += w * (row_bd[j + 1] - row_bd[j]);
            }
            ts += (tby - row_bd[tile_y]) * (col_bd[tile_x + 1] - col_bd[tile_x]) + tbx - col_bd[tile_x];
            rs_to_ts[rs as usize] = ts;
            tile_id[rs as usize] = (tile_y * pps.num_tile_columns as usize + tile_x) as u16;
        }
        let mut ts_to_rs = vec![0u32; n];
        for (rs, &ts) in rs_to_ts.iter().enumerate() {
            ts_to_rs[ts as usize] = rs as u32;
        }
        let scaling = if sps.scaling_list_enabled { Some(ScalingFactors::new(pps.scaling_list.as_ref().unwrap_or(&sps.scaling_list))) } else { None };
        Ok(Layout { width_ctbs: w, height_ctbs: h, rs_to_ts, ts_to_rs, tile_id, scaling })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bit writer for building test RBSPs.
    struct W(Vec<bool>);
    impl W {
        fn u(&mut self, n: u32, v: u32) {
            for i in (0..n).rev() {
                self.0.push((v >> i) & 1 != 0);
            }
        }
        fn ue(&mut self, v: u32) {
            let x = v + 1;
            let len = 32 - x.leading_zeros();
            self.u(len - 1, 0);
            self.u(len, x);
        }
        fn bytes(&self) -> Vec<u8> {
            let mut out = vec![0u8; self.0.len().div_ceil(8) + 1];
            for (i, &b) in self.0.iter().enumerate() {
                if b {
                    out[i / 8] |= 0x80 >> (i % 8);
                }
            }
            out
        }
    }

    #[test]
    fn inter_rps_prediction() {
        // set 0: -1, -3 (both used); set 1 predicted from it with deltaRps = -1
        let mut w = W(Vec::new());
        w.ue(2); // num_negative_pics
        w.ue(0); // num_positive_pics
        w.ue(0); // -1
        w.u(1, 1);
        w.ue(1); // -3
        w.u(1, 1);
        // set 1: inter_ref_pic_set_prediction_flag, sign, abs_delta_rps_minus1
        w.u(1, 1);
        w.u(1, 1);
        w.ue(0);
        for _ in 0..3 {
            w.u(1, 1); // used_by_curr_pic_flag for -1, -3 and deltaRps itself
        }
        let data = w.bytes();
        let mut r = BitReader::new(&data);
        let s0 = parse_st_rps(&mut r, 0, &[], false).unwrap();
        assert_eq!(&s0.delta_poc[..2], &[-1, -3]);
        let s1 = parse_st_rps(&mut r, 1, &[s0], false).unwrap();
        assert_eq!(s1.num_negative, 3);
        assert_eq!(&s1.delta_poc[..3], &[-1, -2, -4]);
    }

    #[test]
    fn uniform_tiles() {
        let bd = |n: u32, total: u32| (0..=n).map(|i| (i * total) / n).collect::<Vec<_>>();
        assert_eq!(bd(3, 10), vec![0, 3, 6, 10]);
    }
}
