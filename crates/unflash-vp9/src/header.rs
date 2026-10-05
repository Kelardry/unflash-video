//! The uncompressed header (6.2, 7.2) and the compressed header (6.3, 7.3)
//! of a frame, and the state that carries from one frame's header to the
//! next: the colour format, the loop filter deltas, the segmentation
//! parameters and the four saved probability contexts.

use crate::bits::BitReader;
use crate::booldec::BoolDecoder;
use crate::probs::FrameContext;
use crate::tables::INV_MAP_TABLE;
use crate::{Error, Result};

pub const INTRA_FRAME: u8 = 0;
pub const LAST_FRAME: u8 = 1;
pub const GOLDEN_FRAME: u8 = 2;
pub const ALTREF_FRAME: u8 = 3;
/// The absent second reference of a single-reference or intra block.
pub const NONE_FRAME: u8 = 0;

/// interpolation_filter values: EIGHTTAP (regular), EIGHTTAP_SMOOTH,
/// EIGHTTAP_SHARP, BILINEAR, and SWITCHABLE (chosen per block).
pub const SWITCHABLE: u8 = 4;

pub const ONLY_4X4: u8 = 0;
pub const ALLOW_32X32: u8 = 3;
pub const TX_MODE_SELECT: u8 = 4;

pub const SINGLE_REFERENCE: u8 = 0;
pub const COMPOUND_REFERENCE: u8 = 1;
pub const REFERENCE_MODE_SELECT: u8 = 2;

pub const SEG_LVL_ALT_Q: usize = 0;
pub const SEG_LVL_ALT_L: usize = 1;
pub const SEG_LVL_REF_FRAME: usize = 2;
pub const SEG_LVL_SKIP: usize = 3;

/// color_space values the output cares about.
pub const CS_UNKNOWN: u8 = 0;
pub const CS_BT_601: u8 = 1;
pub const CS_BT_709: u8 = 2;
pub const CS_SMPTE_240: u8 = 4;
pub const CS_BT_2020: u8 = 5;
pub const CS_RGB: u8 = 7;

/// The largest frame the decoder accepts (in area: either side may take
/// its full 16 bits), so that a corrupt size cannot ask for gigabytes.
const MAX_AREA: u64 = 8192 * 8192;

#[derive(Clone, Copy, Debug, Default)]
pub struct LoopFilterParams {
    pub level: u8,
    pub sharpness: u8,
    pub delta_enabled: bool,
    /// Kept between frames (reset by `setup_past_independence`).
    pub ref_deltas: [i8; 4],
    pub mode_deltas: [i8; 2],
}

#[derive(Clone, Debug, Default)]
pub struct Segmentation {
    pub enabled: bool,
    pub update_map: bool,
    pub temporal_update: bool,
    /// Feature data are values, not deltas from the frame's.
    pub abs_delta: bool,
    pub tree_probs: [u8; 7],
    pub pred_probs: [u8; 3],
    pub feature_enabled: [[bool; 4]; 8],
    pub feature_data: [[i16; 4]; 8],
}

impl Segmentation {
    /// seg_feature_active (6.4.9).
    #[inline]
    pub fn active(&self, segment: u8, feature: usize) -> bool {
        self.enabled && self.feature_enabled[segment as usize & 7][feature]
    }

    #[inline]
    pub fn data(&self, segment: u8, feature: usize) -> i32 {
        self.feature_data[segment as usize & 7][feature] as i32
    }
}

/// What carries from one frame header to the next.
#[derive(Clone)]
pub struct StreamState {
    /// The colour format of the last key or intra-only frame (inter frames
    /// inherit it).
    pub bit_depth: u8,
    pub color_space: u8,
    pub color_range: bool,
    pub lf: LoopFilterParams,
    pub seg: Segmentation,
    /// The four probability contexts (save_probs / load_probs).
    pub contexts: [FrameContext; 4],
    /// The type of the last frame decoded (LastFrameType).
    pub key_frame: bool,
}

impl Default for StreamState {
    fn default() -> Self {
        StreamState {
            bit_depth: 8,
            color_space: CS_UNKNOWN,
            color_range: false,
            lf: LoopFilterParams::default(),
            seg: Segmentation::default(),
            contexts: [FrameContext::default(), FrameContext::default(), FrameContext::default(), FrameContext::default()],
            key_frame: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FrameHeader {
    pub profile: u8,
    pub show_existing_frame: bool,
    pub frame_to_show: usize,
    pub key_frame: bool,
    /// The previous frame was a key frame (the coefficient probabilities
    /// then adapt faster).
    pub after_key_frame: bool,
    pub show_frame: bool,
    pub error_resilient_mode: bool,
    pub intra_only: bool,
    pub reset_frame_context: u8,
    pub refresh_frame_flags: u8,
    /// Slots of LAST, GOLDEN and ALTREF.
    pub ref_frame_idx: [usize; 3],
    /// Indexed by reference frame (INTRA_FRAME..=ALTREF_FRAME).
    pub ref_frame_sign_bias: [bool; 4],
    pub width: u32,
    pub height: u32,
    pub render_width: u32,
    pub render_height: u32,
    pub bit_depth: u8,
    pub color_space: u8,
    pub color_range: bool,
    pub allow_high_precision_mv: bool,
    pub interp_filter: u8,
    pub refresh_frame_context: bool,
    pub frame_parallel_decoding_mode: bool,
    pub frame_context_idx: usize,
    /// setup_past_independence was invoked: the previous segmentation map
    /// is to be cleared.
    pub reset_past: bool,
    pub lf: LoopFilterParams,
    pub base_q_idx: u8,
    pub delta_q_y_dc: i8,
    pub delta_q_uv_dc: i8,
    pub delta_q_uv_ac: i8,
    pub lossless: bool,
    pub seg: Segmentation,
    pub tile_cols_log2: u32,
    pub tile_rows_log2: u32,
    /// Bytes of the uncompressed header and of the compressed header.
    pub uncompressed_size: usize,
    pub compressed_size: usize,
    // from the compressed header
    pub tx_mode: u8,
    pub reference_mode: u8,
    pub comp_fixed_ref: u8,
    pub comp_var_ref: [u8; 2],
}

impl FrameHeader {
    /// FrameIsIntra.
    pub fn is_intra(&self) -> bool {
        self.key_frame || self.intra_only
    }

    pub fn mi_cols(&self) -> usize {
        (self.width as usize + 7) >> 3
    }

    pub fn mi_rows(&self) -> usize {
        (self.height as usize + 7) >> 3
    }

    pub fn sb64_cols(&self) -> usize {
        (self.mi_cols() + 7) >> 3
    }
}

fn frame_sync_code(r: &mut BitReader) -> Result<()> {
    if r.f(8)? != 0x49 || r.f(8)? != 0x83 || r.f(8)? != 0x42 {
        return Err(Error::Bitstream("bad frame sync code"));
    }
    Ok(())
}

/// color_config (6.2.2); profiles 1 and 3 never get here.
fn color_config(r: &mut BitReader, fh: &mut FrameHeader) -> Result<()> {
    fh.bit_depth = if fh.profile >= 2 {
        if r.bit()? {
            12
        } else {
            10
        }
    } else {
        8
    };
    fh.color_space = r.f(3)? as u8;
    if fh.color_space == CS_RGB {
        return Err(Error::Unsupported("RGB (4:4:4) needs profile 1 or 3"));
    }
    fh.color_range = r.bit()?;
    Ok(())
}

fn frame_size(r: &mut BitReader) -> Result<(u32, u32)> {
    let w = r.f(16)? + 1;
    let h = r.f(16)? + 1;
    Ok((w, h))
}

fn check_size(w: u32, h: u32) -> Result<()> {
    if w as u64 * h as u64 > MAX_AREA {
        return Err(Error::Unsupported("frame area larger than 8192x8192"));
    }
    Ok(())
}

fn render_size(r: &mut BitReader, fh: &mut FrameHeader) -> Result<()> {
    if r.bit()? {
        let (w, h) = frame_size(r)?;
        fh.render_width = w;
        fh.render_height = h;
    } else {
        fh.render_width = fh.width;
        fh.render_height = fh.height;
    }
    Ok(())
}

/// setup_past_independence (7.2): the parts that live in the header state.
fn setup_past_independence(state: &mut StreamState, fh: &mut FrameHeader) {
    state.seg.feature_enabled = [[false; 4]; 8];
    state.seg.feature_data = [[0; 4]; 8];
    state.seg.abs_delta = false;
    state.lf.delta_enabled = true;
    state.lf.ref_deltas = [1, 0, -1, -1];
    state.lf.mode_deltas = [0, 0];
    fh.ref_frame_sign_bias = [false; 4];
    fh.reset_past = true;
}

/// Parse the uncompressed header of a frame. `ref_sizes` are the frame
/// sizes of the eight reference slots (None: empty). The state's resets
/// (setup_past_independence, probability context resets) are applied here
/// in the order the specification gives.
pub fn parse_uncompressed_header(data: &[u8], state: &mut StreamState, ref_sizes: &[Option<(u32, u32)>; 8]) -> Result<FrameHeader> {
    let mut r = BitReader::new(data);
    let mut fh = FrameHeader::default();
    if r.f(2)? != 2 {
        return Err(Error::Bitstream("bad frame marker"));
    }
    let low = r.f(1)?;
    let high = r.f(1)?;
    fh.profile = ((high << 1) + low) as u8;
    if fh.profile == 1 || fh.profile == 3 {
        return Err(Error::Unsupported("profiles 1 and 3 (4:4:4, 4:2:2 and 4:4:0 chroma)"));
    }
    fh.show_existing_frame = r.bit()?;
    if fh.show_existing_frame {
        fh.frame_to_show = r.f(3)? as usize;
        fh.show_frame = true;
        fh.uncompressed_size = r.bytes_read();
        return Ok(fh);
    }
    fh.key_frame = !r.bit()?;
    fh.after_key_frame = state.key_frame;
    state.key_frame = fh.key_frame;
    fh.show_frame = r.bit()?;
    fh.error_resilient_mode = r.bit()?;
    fh.bit_depth = state.bit_depth;
    fh.color_space = state.color_space;
    fh.color_range = state.color_range;
    if fh.key_frame {
        frame_sync_code(&mut r)?;
        color_config(&mut r, &mut fh)?;
        let (w, h) = frame_size(&mut r)?;
        fh.width = w;
        fh.height = h;
        render_size(&mut r, &mut fh)?;
        fh.refresh_frame_flags = 0xff;
    } else {
        fh.intra_only = if fh.show_frame { false } else { r.bit()? };
        fh.reset_frame_context = if fh.error_resilient_mode { 0 } else { r.f(2)? as u8 };
        if fh.intra_only {
            frame_sync_code(&mut r)?;
            if fh.profile > 0 {
                color_config(&mut r, &mut fh)?;
            } else {
                fh.bit_depth = 8;
                fh.color_space = CS_BT_601;
                fh.color_range = false;
            }
            fh.refresh_frame_flags = r.f(8)? as u8;
            let (w, h) = frame_size(&mut r)?;
            fh.width = w;
            fh.height = h;
            render_size(&mut r, &mut fh)?;
        } else {
            fh.refresh_frame_flags = r.f(8)? as u8;
            for i in 0..3 {
                fh.ref_frame_idx[i] = r.f(3)? as usize;
                fh.ref_frame_sign_bias[1 + i] = r.bit()?;
            }
            let mut found = None;
            for i in 0..3 {
                if r.bit()? {
                    found = Some(ref_sizes[fh.ref_frame_idx[i]].ok_or(Error::Bitstream("reference frame missing"))?);
                    break;
                }
            }
            let (w, h) = match found {
                Some(s) => s,
                None => frame_size(&mut r)?,
            };
            fh.width = w;
            fh.height = h;
            render_size(&mut r, &mut fh)?;
            fh.allow_high_precision_mv = r.bit()?;
            fh.interp_filter = if r.bit()? { SWITCHABLE } else { [1, 0, 2, 3][r.f(2)? as usize] };
        }
    }
    check_size(fh.width, fh.height)?;
    if !fh.error_resilient_mode {
        fh.refresh_frame_context = r.bit()?;
        fh.frame_parallel_decoding_mode = r.bit()?;
    } else {
        fh.refresh_frame_context = false;
        fh.frame_parallel_decoding_mode = true;
    }
    fh.frame_context_idx = r.f(2)? as usize;
    if fh.is_intra() || fh.error_resilient_mode {
        setup_past_independence(state, &mut fh);
        if fh.key_frame || fh.error_resilient_mode || fh.reset_frame_context == 3 {
            for c in state.contexts.iter_mut() {
                *c = FrameContext::default();
            }
        } else if fh.reset_frame_context == 2 {
            state.contexts[fh.frame_context_idx] = FrameContext::default();
        }
        fh.frame_context_idx = 0;
    }
    loop_filter_params(&mut r, state)?;
    fh.lf = state.lf;
    quantization_params(&mut r, &mut fh)?;
    segmentation_params(&mut r, state)?;
    fh.seg = state.seg.clone();
    tile_info(&mut r, &mut fh)?;
    fh.compressed_size = r.f(16)? as usize;
    if fh.compressed_size == 0 {
        return Err(Error::Bitstream("empty compressed header"));
    }
    fh.uncompressed_size = r.bytes_read();
    // what the next frames inherit
    if fh.is_intra() {
        state.bit_depth = fh.bit_depth;
        state.color_space = fh.color_space;
        state.color_range = fh.color_range;
    }
    Ok(fh)
}

fn loop_filter_params(r: &mut BitReader, state: &mut StreamState) -> Result<()> {
    let lf = &mut state.lf;
    lf.level = r.f(6)? as u8;
    lf.sharpness = r.f(3)? as u8;
    lf.delta_enabled = r.bit()?;
    if lf.delta_enabled && r.bit()? {
        for i in 0..4 {
            if r.bit()? {
                lf.ref_deltas[i] = r.s(6)? as i8;
            }
        }
        for i in 0..2 {
            if r.bit()? {
                lf.mode_deltas[i] = r.s(6)? as i8;
            }
        }
    }
    Ok(())
}

fn read_delta_q(r: &mut BitReader) -> Result<i8> {
    Ok(if r.bit()? { r.s(4)? as i8 } else { 0 })
}

fn quantization_params(r: &mut BitReader, fh: &mut FrameHeader) -> Result<()> {
    fh.base_q_idx = r.f(8)? as u8;
    fh.delta_q_y_dc = read_delta_q(r)?;
    fh.delta_q_uv_dc = read_delta_q(r)?;
    fh.delta_q_uv_ac = read_delta_q(r)?;
    fh.lossless = fh.base_q_idx == 0 && fh.delta_q_y_dc == 0 && fh.delta_q_uv_dc == 0 && fh.delta_q_uv_ac == 0;
    Ok(())
}

fn read_prob(r: &mut BitReader) -> Result<u8> {
    Ok(if r.bit()? { r.f(8)? as u8 } else { 255 })
}

fn segmentation_params(r: &mut BitReader, state: &mut StreamState) -> Result<()> {
    const BITS: [u32; 4] = [8, 6, 2, 0];
    const SIGNED: [bool; 4] = [true, true, false, false];
    let seg = &mut state.seg;
    seg.enabled = r.bit()?;
    seg.update_map = false;
    if !seg.enabled {
        return Ok(());
    }
    seg.update_map = r.bit()?;
    if seg.update_map {
        for i in 0..7 {
            seg.tree_probs[i] = read_prob(r)?;
        }
        seg.temporal_update = r.bit()?;
        for i in 0..3 {
            seg.pred_probs[i] = if seg.temporal_update { read_prob(r)? } else { 255 };
        }
    }
    if r.bit()? {
        seg.abs_delta = r.bit()?;
        for i in 0..8 {
            for j in 0..4 {
                let enabled = r.bit()?;
                let mut value = 0;
                if enabled {
                    value = r.f(BITS[j])? as i16;
                    if SIGNED[j] && r.bit()? {
                        value = -value;
                    }
                }
                seg.feature_enabled[i][j] = enabled;
                seg.feature_data[i][j] = value;
            }
        }
    }
    Ok(())
}

fn tile_info(r: &mut BitReader, fh: &mut FrameHeader) -> Result<()> {
    let sb_cols = fh.sb64_cols();
    let mut min_log2 = 0;
    while (64 << min_log2) < sb_cols {
        min_log2 += 1;
    }
    let mut max_log2 = 1;
    while (sb_cols >> max_log2) >= 4 {
        max_log2 += 1;
    }
    max_log2 -= 1;
    fh.tile_cols_log2 = min_log2;
    while fh.tile_cols_log2 < max_log2 {
        if r.bit()? {
            fh.tile_cols_log2 += 1;
        } else {
            break;
        }
    }
    fh.tile_rows_log2 = r.f(1)?;
    if fh.tile_rows_log2 == 1 {
        fh.tile_rows_log2 += r.f(1)?;
    }
    Ok(())
}

// ---- compressed header -------------------------------------------------------

/// inv_recenter_nonneg (6.3.6).
fn inv_recenter_nonneg(v: u32, m: u32) -> u32 {
    if v > 2 * m {
        v
    } else if v & 1 != 0 {
        m - ((v + 1) >> 1)
    } else {
        m + (v >> 1)
    }
}

/// diff_update_prob (6.3.3): an optional update of one probability.
fn diff_update_prob(bd: &mut BoolDecoder, prob: &mut u8) {
    if !bd.read(252) {
        return;
    }
    // decode_term_subexp (6.3.4)
    let delta = if !bd.read_bit() {
        bd.read_literal(4)
    } else if !bd.read_bit() {
        bd.read_literal(4) + 16
    } else if !bd.read_bit() {
        bd.read_literal(5) + 32
    } else {
        let v = bd.read_literal(7);
        if v < 65 {
            v + 64
        } else {
            (v << 1) - 1 + bd.read_bit() as u32
        }
    };
    // inv_remap_prob (6.3.5); the delta is at most 254
    let v = INV_MAP_TABLE[delta as usize] as u32;
    let m = (*prob as u32).saturating_sub(1);
    *prob = if (m << 1) <= 255 { 1 + inv_recenter_nonneg(v, m) } else { 255 - inv_recenter_nonneg(v, 254 - m) } as u8;
}

fn update_mv_prob(bd: &mut BoolDecoder, prob: &mut u8) {
    if bd.read(252) {
        *prob = ((bd.read_literal(7) << 1) | 1) as u8;
    }
}

/// Parse the compressed header (6.3): the transform mode, the reference
/// mode, and the forward updates of the probabilities in `fc`.
pub fn parse_compressed_header(data: &[u8], fh: &mut FrameHeader, fc: &mut FrameContext) -> Result<()> {
    let mut bd = BoolDecoder::new(data)?;
    // read_tx_mode
    fh.tx_mode = if fh.lossless {
        ONLY_4X4
    } else {
        let mut m = bd.read_literal(2) as u8;
        if m == ALLOW_32X32 {
            m += bd.read_bit() as u8;
        }
        m
    };
    if fh.tx_mode == TX_MODE_SELECT {
        for i in 0..2 {
            diff_update_prob(&mut bd, &mut fc.tx8[i][0]);
        }
        for i in 0..2 {
            for j in 0..2 {
                diff_update_prob(&mut bd, &mut fc.tx16[i][j]);
            }
        }
        for i in 0..2 {
            for j in 0..3 {
                diff_update_prob(&mut bd, &mut fc.tx32[i][j]);
            }
        }
    }
    // read_coef_probs
    let max_tx = [0, 1, 2, 3, 3][fh.tx_mode as usize];
    for t in 0..=max_tx {
        if bd.read_bit() {
            for plane in fc.coef[t].iter_mut() {
                for rf in plane.iter_mut() {
                    for (k, band) in rf.iter_mut().enumerate() {
                        let contexts = if k == 0 { 3 } else { 6 };
                        for ctx in band.iter_mut().take(contexts) {
                            for p in ctx.iter_mut() {
                                diff_update_prob(&mut bd, p);
                            }
                        }
                    }
                }
            }
        }
    }
    for p in fc.skip.iter_mut() {
        diff_update_prob(&mut bd, p);
    }
    if !fh.is_intra() {
        for ctx in fc.inter_mode.iter_mut() {
            for p in ctx.iter_mut() {
                diff_update_prob(&mut bd, p);
            }
        }
        if fh.interp_filter == SWITCHABLE {
            for ctx in fc.interp_filter.iter_mut() {
                for p in ctx.iter_mut() {
                    diff_update_prob(&mut bd, p);
                }
            }
        }
        for p in fc.is_inter.iter_mut() {
            diff_update_prob(&mut bd, p);
        }
        frame_reference_mode(&mut bd, fh);
        if fh.reference_mode == REFERENCE_MODE_SELECT {
            for p in fc.comp_mode.iter_mut() {
                diff_update_prob(&mut bd, p);
            }
        }
        if fh.reference_mode != COMPOUND_REFERENCE {
            for ctx in fc.single_ref.iter_mut() {
                diff_update_prob(&mut bd, &mut ctx[0]);
                diff_update_prob(&mut bd, &mut ctx[1]);
            }
        }
        if fh.reference_mode != SINGLE_REFERENCE {
            for p in fc.comp_ref.iter_mut() {
                diff_update_prob(&mut bd, p);
            }
        }
        for ctx in fc.y_mode.iter_mut() {
            for p in ctx.iter_mut() {
                diff_update_prob(&mut bd, p);
            }
        }
        for ctx in fc.partition.iter_mut() {
            for p in ctx.iter_mut() {
                diff_update_prob(&mut bd, p);
            }
        }
        mv_probs(&mut bd, fh, fc);
    }
    if bd.overran() {
        return Err(Error::Bitstream("compressed header runs past its end"));
    }
    Ok(())
}

/// frame_reference_mode (6.3.12) with setup_compound_reference_mode (6.3.18).
fn frame_reference_mode(bd: &mut BoolDecoder, fh: &mut FrameHeader) {
    let bias = fh.ref_frame_sign_bias;
    let compound_allowed = bias[GOLDEN_FRAME as usize] != bias[LAST_FRAME as usize] || bias[ALTREF_FRAME as usize] != bias[LAST_FRAME as usize];
    fh.reference_mode = SINGLE_REFERENCE;
    if compound_allowed && bd.read_bit() {
        fh.reference_mode = if bd.read_bit() { REFERENCE_MODE_SELECT } else { COMPOUND_REFERENCE };
        if bias[LAST_FRAME as usize] == bias[GOLDEN_FRAME as usize] {
            fh.comp_fixed_ref = ALTREF_FRAME;
            fh.comp_var_ref = [LAST_FRAME, GOLDEN_FRAME];
        } else if bias[LAST_FRAME as usize] == bias[ALTREF_FRAME as usize] {
            fh.comp_fixed_ref = GOLDEN_FRAME;
            fh.comp_var_ref = [LAST_FRAME, ALTREF_FRAME];
        } else {
            fh.comp_fixed_ref = LAST_FRAME;
            fh.comp_var_ref = [GOLDEN_FRAME, ALTREF_FRAME];
        }
    }
}

fn mv_probs(bd: &mut BoolDecoder, fh: &FrameHeader, fc: &mut FrameContext) {
    for p in fc.mv_joint.iter_mut() {
        update_mv_prob(bd, p);
    }
    for i in 0..2 {
        update_mv_prob(bd, &mut fc.mv_sign[i]);
        for p in fc.mv_class[i].iter_mut() {
            update_mv_prob(bd, p);
        }
        update_mv_prob(bd, &mut fc.mv_class0_bit[i]);
        for p in fc.mv_bits[i].iter_mut() {
            update_mv_prob(bd, p);
        }
    }
    for i in 0..2 {
        for j in 0..2 {
            for p in fc.mv_class0_fr[i][j].iter_mut() {
                update_mv_prob(bd, p);
            }
        }
        for p in fc.mv_fr[i].iter_mut() {
            update_mv_prob(bd, p);
        }
    }
    if fh.allow_high_precision_mv {
        for i in 0..2 {
            update_mv_prob(bd, &mut fc.mv_class0_hp[i]);
            update_mv_prob(bd, &mut fc.mv_hp[i]);
        }
    }
}
