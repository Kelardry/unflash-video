//! Slice segment data (7.3.8): the coding tree units of a slice segment
//! with their entropy coding state (tiles, wavefronts, dependent slice
//! segments), and the reconstruction of each coding unit as it is parsed
//! (8.4, 8.6). Residual coding and motion vector prediction continue this
//! type in `residual.rs` and `mv.rs`.

use std::rc::Rc;

use crate::bitreader::BitReader;
use crate::cabac::{ctx, init_contexts, Cabac, Contexts};
use crate::inter::{self, Scratch, Weights};
use crate::intra::{self, MAX_LINE};
use crate::meta::{self, Meta, Motion, SaoParams, BYPASS, CODED, INTRA, PCM, PU_LEFT, PU_TOP, SKIP, TU_LEFT, TU_TOP};
use crate::picture::{Picture, Sample};
use crate::ps::{Layout, Pps, Sps};
use crate::slice::{SliceHeader, SliceType};
use crate::tables::qpc;
use crate::transform;
use crate::{Error, Result};

/// A reference picture of the current slice.
pub struct RefPic<P> {
    pub pic: Rc<Picture<P>>,
    pub poc: i32,
    pub long_term: bool,
}

/// Entropy coding state carried from one slice segment of a picture to
/// the next: the contexts stored for wavefront synchronisation and at the
/// end of a slice segment (for a dependent one), and QpY of the last
/// coding unit.
#[derive(Default)]
pub struct Carry {
    pub wpp: Option<Contexts>,
    pub segment_end: Option<Contexts>,
    pub last_qp: i32,
}

/// PartMode (Table 7-10): how a coding unit splits into prediction blocks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PartMode {
    P2Nx2N,
    P2NxN,
    PNx2N,
    PNxN,
    P2NxnU,
    P2NxnD,
    PnLx2N,
    PnRx2N,
}

/// The coding unit being decoded.
#[derive(Clone, Copy)]
struct Cu {
    intra: bool,
    bypass: bool,
    part: PartMode,
    /// IntraPredModeC.
    chroma_mode: u32,
    max_trafo_depth: usize,
}

/// initType (9.3.2.2): which set of initialisation values the slice's
/// contexts start from.
fn init_type(hdr: &SliceHeader) -> usize {
    match hdr.slice_type {
        SliceType::I => 0,
        SliceType::P => 1 + hdr.cabac_init as usize,
        SliceType::B => 2 - hdr.cabac_init as usize,
    }
}

/// Decodes the coding tree units of one slice segment into the picture,
/// recording what the loop filters and later pictures need in `meta`.
pub struct SliceDecoder<'a, P: Sample> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub layout: &'a Layout,
    pub hdr: &'a SliceHeader,
    pub slice_idx: u16,
    pub cabac: Cabac<'a>,
    pub pic: &'a mut Picture<P>,
    pub meta: &'a mut Meta,
    pub refs: &'a [Vec<RefPic<P>>; 2],
    pub carry: &'a mut Carry,
    /// PicOrderCntVal of the current picture.
    pub poc: i32,
    /// The collocated picture for temporal motion vector prediction.
    pub col: Option<&'a Picture<P>>,
    /// NoBackwardPredFlag (8.5.3.2.9).
    pub no_backward_pred: bool,
    /// Explicit weights per list, reference index and component.
    pub weights: Option<Box<[[[Weights; 3]; 16]; 2]>>,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) bit_depth: u32,
    pub(crate) chroma: bool,
    log2_qg: usize,
    log2_chroma_qg: usize,
    // quantisation state
    qp_y: i32,
    qp_pred: i32,
    qg_pending: Option<(usize, usize)>,
    cu_qp_delta_coded: bool,
    cu_qp_delta: i32,
    chroma_qp_offset_coded: bool,
    cu_qp_offset: [i32; 2],
    cu: Cu,
    // buffers
    pub(crate) coeffs: Vec<i32>,
    pub(crate) res: Vec<i32>,
    pub(crate) pred: [Vec<i16>; 2],
    pub(crate) scratch: Scratch<P>,
}

/// Interleave the bits of `x` (even positions) and `y` (odd positions):
/// the z-scan order of blocks inside a coding tree block.
#[inline]
fn morton(x: usize, y: usize) -> usize {
    let spread = |mut v: usize| {
        v &= 0xff;
        v = (v | (v << 4)) & 0x0f0f;
        v = (v | (v << 2)) & 0x3333;
        (v | (v << 1)) & 0x5555
    };
    spread(x) | (spread(y) << 1)
}

impl<'a, P: Sample> SliceDecoder<'a, P> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(sps: &'a Sps, pps: &'a Pps, layout: &'a Layout, hdr: &'a SliceHeader, slice_idx: u16, rbsp: &'a [u8], pic: &'a mut Picture<P>, meta: &'a mut Meta, refs: &'a [Vec<RefPic<P>>; 2], carry: &'a mut Carry, poc: i32) -> Result<SliceDecoder<'a, P>> {
        let cabac = Cabac::new(rbsp, hdr.data_offset, init_contexts(init_type(hdr), hdr.qp))?;
        let col = if hdr.temporal_mvp && !hdr.is_intra() {
            let l = if hdr.slice_type == SliceType::B && !hdr.collocated_from_l0 { 1 } else { 0 };
            refs[l].get(hdr.collocated_ref_idx).map(|r| &*r.pic).filter(|p| !p.generated)
        } else {
            None
        };
        let no_backward_pred = refs.iter().all(|l| l.iter().all(|r| r.poc <= poc));
        let weights = hdr.weights.as_ref().map(|pw| {
            let shift1 = 14 - sps.bit_depth;
            let mut t = Box::new([[[Weights { log2wd: 0, w: [0; 2], o: [0; 2] }; 3]; 16]; 2]);
            for l in 0..2 {
                for i in 0..16 {
                    for c in 0..3 {
                        let (w, o) = pw.w[l][i][c];
                        let denom = if c == 0 { pw.luma_log2_denom } else { pw.chroma_log2_denom };
                        t[l][i][c] = Weights { log2wd: denom + shift1, w: [w, w], o: [o, o] };
                    }
                }
            }
            t
        });
        let log2_qg = (sps.log2_ctb - pps.diff_cu_qp_delta_depth) as usize;
        let log2_chroma_qg = (sps.log2_ctb - pps.diff_cu_chroma_qp_offset_depth) as usize;
        Ok(SliceDecoder {
            sps,
            pps,
            layout,
            hdr,
            slice_idx,
            cabac,
            pic,
            meta,
            refs,
            carry,
            poc,
            col,
            no_backward_pred,
            weights,
            width: sps.width as i32,
            height: sps.height as i32,
            bit_depth: sps.bit_depth,
            chroma: sps.chroma_format_idc != 0,
            log2_qg,
            log2_chroma_qg,
            qp_y: hdr.qp,
            qp_pred: hdr.qp,
            qg_pending: None,
            cu_qp_delta_coded: false,
            cu_qp_delta: 0,
            chroma_qp_offset_coded: false,
            cu_qp_offset: [0, 0],
            cu: Cu { intra: false, bypass: false, part: PartMode::P2Nx2N, chroma_mode: 0, max_trafo_depth: 0 },
            coeffs: vec![0; 32 * 32],
            res: vec![0; 32 * 32],
            pred: [vec![0; 64 * 64], vec![0; 64 * 64]],
            scratch: Scratch::new(),
        })
    }

    pub(crate) fn cu_part(&self) -> PartMode {
        self.cu.part
    }
    pub(crate) fn cu_intra(&self) -> bool {
        self.cu.intra
    }
    pub(crate) fn cu_bypass(&self) -> bool {
        self.cu.bypass
    }

    fn tile_of(&self, rs: usize) -> u16 {
        self.layout.tile_id[rs]
    }

    /// The first CTB of a tile (in tile scan).
    fn tile_start(&self, ts: usize) -> bool {
        ts == 0 || self.tile_of(self.layout.ts_to_rs[ts] as usize) != self.tile_of(self.layout.ts_to_rs[ts - 1] as usize)
    }

    /// The first CTB of a CTB row within a tile.
    fn row_start(&self, rs: usize) -> bool {
        let w = self.layout.width_ctbs as usize;
        rs.is_multiple_of(w) || self.tile_of(rs) != self.tile_of(rs - 1)
    }

    /// 9.3.2.1: the context variables at the start of the CTU at `ts`
    /// (the first of the slice segment, or of a tile or wavefront row).
    fn start_contexts(&mut self, ts: usize, rs: usize, segment_start: bool) {
        let fresh = || init_contexts(init_type(self.hdr), self.hdr.qp);
        let ctx = if self.tile_start(ts) {
            fresh()
        } else if self.pps.entropy_coding_sync && self.row_start(rs) {
            let size = 1i32 << self.sps.log2_ctb;
            let (x0, y0) = self.ctb_origin(rs);
            match self.carry.wpp {
                Some(c) if self.z_available_from(rs, x0 as i32 + size, y0 as i32 - size) => c,
                _ => fresh(),
            }
        } else if segment_start && self.hdr.dependent {
            self.carry.segment_end.unwrap_or_else(fresh)
        } else {
            fresh()
        };
        self.cabac.ctx = ctx;
    }

    fn ctb_origin(&self, rs: usize) -> (usize, usize) {
        let w = self.layout.width_ctbs as usize;
        ((rs % w) << self.sps.log2_ctb, (rs / w) << self.sps.log2_ctb)
    }

    /// Decode the slice segment.
    pub fn decode(&mut self) -> Result<()> {
        let total = self.layout.ts_to_rs.len();
        let mut ts = self.layout.rs_to_ts[self.hdr.segment_address as usize] as usize;
        let mut rs = self.hdr.segment_address as usize;
        if !self.hdr.dependent {
            self.carry.last_qp = self.hdr.qp;
        }
        self.start_contexts(ts, rs, true);
        if self.tile_start(ts) || (self.pps.entropy_coding_sync && self.row_start(rs)) {
            self.carry.last_qp = self.hdr.qp;
        }
        loop {
            self.decode_ctu(rs)?;
            if self.cabac.overrun() {
                return Err(Error::Bitstream("slice data ends early"));
            }
            let end = self.cabac.terminate() != 0;
            if self.pps.entropy_coding_sync {
                let w = self.layout.width_ctbs as usize;
                if rs % w == 1 || (rs > 1 && self.tile_of(rs) != self.tile_of(rs - 2)) {
                    self.carry.wpp = Some(self.cabac.ctx);
                }
            }
            ts += 1;
            if end {
                if self.pps.dependent_slice_segments_enabled {
                    self.carry.segment_end = Some(self.cabac.ctx);
                }
                return Ok(());
            }
            if ts >= total {
                return Err(Error::Bitstream("slice runs past the end of the picture"));
            }
            rs = self.layout.ts_to_rs[ts] as usize;
            let new_tile = self.pps.tiles_enabled && self.tile_start(ts);
            let new_row = self.pps.entropy_coding_sync && self.row_start(rs);
            if new_tile || new_row {
                // end_of_subset_one_bit, byte alignment, and the next substream
                if self.cabac.terminate() == 0 {
                    return Err(Error::Bitstream("missing end_of_subset_one_bit"));
                }
                let pos = self.cabac.byte_pos();
                self.cabac.restart(pos)?;
                self.start_contexts(ts, rs, false);
                self.carry.last_qp = self.hdr.qp;
            }
        }
    }

    // ---- availability ----

    /// 6.4.1: whether the block at (`xn`, `yn`) is available to the one at
    /// (`xc`, `yc`) (decoded before it, in the same slice and tile).
    #[inline]
    pub(crate) fn z_available(&self, xc: i32, yc: i32, xn: i32, yn: i32) -> bool {
        if xn < 0 || yn < 0 || xn >= self.width || yn >= self.height {
            return false;
        }
        let l = self.sps.log2_ctb;
        let (cx, cy, nx, ny) = (xc >> l, yc >> l, xn >> l, yn >> l);
        if cx == nx && cy == ny {
            let m = self.sps.log2_min_tb;
            let mask = (1 << l) - 1;
            return morton(((xn & mask) >> m) as usize, ((yn & mask) >> m) as usize) <= morton(((xc & mask) >> m) as usize, ((yc & mask) >> m) as usize);
        }
        let w = self.layout.width_ctbs as usize;
        let (cur, nb) = (cy as usize * w + cx as usize, ny as usize * w + nx as usize);
        self.ctb_available(cur, nb)
    }

    /// Whether CTB `nb` precedes CTB `cur` in the same slice and tile.
    #[inline]
    fn ctb_available(&self, cur: usize, nb: usize) -> bool {
        let s = self.meta.ctb_slice[nb];
        s != meta::NO_SLICE && self.layout.rs_to_ts[nb] < self.layout.rs_to_ts[cur] && self.meta.slices[s as usize].addr == self.hdr.slice_address && self.layout.tile_id[nb] == self.layout.tile_id[cur]
    }

    /// 6.4.1 for a location outside the current CTB `rs` (the wavefront
    /// neighbour).
    fn z_available_from(&self, rs: usize, xn: i32, yn: i32) -> bool {
        if xn < 0 || yn < 0 || xn >= self.width || yn >= self.height {
            return false;
        }
        let l = self.sps.log2_ctb;
        let w = self.layout.width_ctbs as usize;
        self.ctb_available(rs, (yn >> l) as usize * w + (xn >> l) as usize)
    }

    // ---- coding tree unit ----

    fn decode_ctu(&mut self, rs: usize) -> Result<()> {
        let (x0, y0) = self.ctb_origin(rs);
        self.meta.ctb_slice[rs] = self.slice_idx;
        if self.hdr.sao_luma || self.hdr.sao_chroma {
            self.parse_sao(rs)?;
        }
        self.coding_quadtree(x0, y0, self.sps.log2_ctb as usize, 0)
    }

    /// 7.3.8.3: `sao( rx, ry )`.
    fn parse_sao(&mut self, rs: usize) -> Result<()> {
        let w = self.layout.width_ctbs as usize;
        let (rx, ry) = (rs % w, rs / w);
        let addr = self.hdr.slice_address as usize;
        let mut merge_left = false;
        if rx > 0 && rs > addr && self.tile_of(rs) == self.tile_of(rs - 1) {
            merge_left = self.cabac.decision(ctx::SAO_MERGE) != 0;
        }
        let mut merge_up = false;
        if ry > 0 && !merge_left && rs - w >= addr && self.tile_of(rs) == self.tile_of(rs - w) {
            merge_up = self.cabac.decision(ctx::SAO_MERGE) != 0;
        }
        if merge_left || merge_up {
            let from = if merge_left { rs - 1 } else { rs - w };
            self.meta.sao[rs] = self.meta.sao[from];
            return Ok(());
        }
        let mut p = SaoParams::default();
        let comps = if self.chroma { 3 } else { 1 };
        let max_offset = (1 << (self.bit_depth.min(10) - 5)) - 1;
        for c in 0..comps {
            if (c == 0 && !self.hdr.sao_luma) || (c > 0 && !self.hdr.sao_chroma) {
                continue;
            }
            p.kind[c] = match c {
                0 | 1 => self.cabac.sao_type_idx() as u8,
                _ => p.kind[1],
            };
            if p.kind[c] == 0 {
                continue;
            }
            let mut abs = [0i32; 4];
            for a in abs.iter_mut() {
                *a = self.cabac.bypass_unary(max_offset) as i32;
            }
            let scale = if c == 0 { self.pps.log2_sao_offset_scale_luma } else { self.pps.log2_sao_offset_scale_chroma };
            if p.kind[c] == 1 {
                for a in abs.iter_mut() {
                    if *a != 0 && self.cabac.bypass() != 0 {
                        *a = -*a;
                    }
                }
                p.class[c] = self.cabac.bypass_bits(5) as u8;
            } else {
                abs[2] = -abs[2];
                abs[3] = -abs[3];
                p.class[c] = match c {
                    0 | 1 => self.cabac.bypass_bits(2) as u8,
                    _ => p.class[1],
                };
            }
            for (o, a) in p.offsets[c].iter_mut().zip(abs) {
                *o = (a << scale) as i16;
            }
        }
        self.meta.sao[rs] = p;
        Ok(())
    }

    /// 7.3.8.4: `coding_quadtree( )`.
    fn coding_quadtree(&mut self, x0: usize, y0: usize, log2: usize, depth: usize) -> Result<()> {
        let size = 1usize << log2;
        let min_cb = self.sps.log2_min_cb as usize;
        let split = if x0 + size <= self.width as usize && y0 + size <= self.height as usize && log2 > min_cb {
            let (xi, yi) = (x0 as i32, y0 as i32);
            let mut inc = 0;
            if self.z_available(xi, yi, xi - 1, yi) && self.meta.depth[self.meta.at(x0 - 1, y0)] as usize > depth {
                inc += 1;
            }
            if self.z_available(xi, yi, xi, yi - 1) && self.meta.depth[self.meta.at(x0, y0 - 1)] as usize > depth {
                inc += 1;
            }
            self.cabac.decision(ctx::SPLIT_CU + inc) != 0
        } else {
            log2 > min_cb
        };
        if self.pps.cu_qp_delta_enabled && log2 >= self.log2_qg {
            self.cu_qp_delta_coded = false;
            self.cu_qp_delta = 0;
            self.qg_pending = Some((x0, y0));
        }
        if self.hdr.cu_chroma_qp_offset_enabled && log2 >= self.log2_chroma_qg {
            self.chroma_qp_offset_coded = false;
        }
        if split {
            let half = size >> 1;
            let (x1, y1) = (x0 + half, y0 + half);
            self.coding_quadtree(x0, y0, log2 - 1, depth + 1)?;
            if x1 < self.width as usize {
                self.coding_quadtree(x1, y0, log2 - 1, depth + 1)?;
            }
            if y1 < self.height as usize {
                self.coding_quadtree(x0, y1, log2 - 1, depth + 1)?;
            }
            if x1 < self.width as usize && y1 < self.height as usize {
                self.coding_quadtree(x1, y1, log2 - 1, depth + 1)?;
            }
            Ok(())
        } else {
            self.coding_unit(x0, y0, log2, depth)
        }
    }

    // ---- quantisation parameters (8.6.1) ----

    /// qPY_PRED at the first coding unit of a quantisation group.
    fn start_quant_group(&mut self, xq: usize, yq: usize) {
        let prev = self.carry.last_qp;
        let mask = (1usize << self.sps.log2_ctb) - 1;
        // the left / above neighbours count only inside the current CTB
        let a = if xq & mask != 0 { self.meta.qp[self.meta.at(xq - 1, yq)] as i32 } else { prev };
        let b = if yq & mask != 0 { self.meta.qp[self.meta.at(xq, yq - 1)] as i32 } else { prev };
        self.qp_pred = (a + b + 1) >> 1;
    }

    /// QpY from qPY_PRED and CuQpDeltaVal (8-283).
    fn update_qp(&mut self) {
        let off = self.sps.qp_bd_offset();
        self.qp_y = ((self.qp_pred + self.cu_qp_delta + 52 + 2 * off).rem_euclid(52 + off)) - off;
    }

    /// Qp′Cb / Qp′Cr (8-285 .. 8-290).
    fn chroma_qp(&self, c: usize) -> i32 {
        let off = self.sps.qp_bd_offset();
        let o = if c == 1 { self.pps.cb_qp_offset + self.hdr.cb_qp_offset } else { self.pps.cr_qp_offset + self.hdr.cr_qp_offset } + self.cu_qp_offset[c - 1];
        qpc((self.qp_y + o).clamp(-off, 57)) + off
    }

    // ---- coding unit ----

    /// 7.3.8.5: `coding_unit( )`.
    fn coding_unit(&mut self, x0: usize, y0: usize, log2: usize, depth: usize) -> Result<()> {
        let n = 1usize << log2;
        if let Some((xq, yq)) = self.qg_pending.take() {
            self.start_quant_group(xq, yq);
        } else if !self.pps.cu_qp_delta_enabled {
            self.qp_pred = self.hdr.qp;
        }
        self.update_qp();
        let w4 = self.meta.w4;
        // the coding unit's own block records start empty
        Meta::fill(&mut self.meta.edges, w4, x0, y0, n, n, 0);
        Meta::fill(&mut self.meta.depth, w4, x0, y0, n, n, depth as u8);
        let bypass = self.pps.transquant_bypass_enabled && self.cabac.decision(ctx::TRANSQUANT_BYPASS) != 0;
        let mut skip = false;
        if !self.hdr.is_intra() {
            let (xi, yi) = (x0 as i32, y0 as i32);
            let mut inc = 0;
            if self.z_available(xi, yi, xi - 1, yi) && self.meta.flags[self.meta.at(x0 - 1, y0)] & SKIP != 0 {
                inc += 1;
            }
            if self.z_available(xi, yi, xi, yi - 1) && self.meta.flags[self.meta.at(x0, y0 - 1)] & SKIP != 0 {
                inc += 1;
            }
            skip = self.cabac.decision(ctx::CU_SKIP + inc) != 0;
        }
        // CU and TU boundaries: the coding unit's own edges
        self.mark_edges(x0, y0, n, n, TU_LEFT | PU_LEFT, TU_TOP | PU_TOP);
        let bflag = if bypass { BYPASS } else { 0 };
        self.cu = Cu { intra: false, bypass, part: PartMode::P2Nx2N, chroma_mode: 0, max_trafo_depth: 0 };
        if skip {
            Meta::fill(&mut self.meta.flags, w4, x0, y0, n, n, SKIP | bflag);
            self.prediction_unit(x0, y0, n, x0, y0, n, n, 0, true)?;
            self.finish_cu(x0, y0, n);
            return Ok(());
        }
        let intra = self.hdr.is_intra() || self.cabac.decision(ctx::PRED_MODE) != 0;
        let min_cb = self.sps.log2_min_cb as usize;
        let part = if !intra || log2 == min_cb { self.part_mode(intra, log2)? } else { PartMode::P2Nx2N };
        self.cu.intra = intra;
        self.cu.part = part;
        Meta::fill(&mut self.meta.flags, w4, x0, y0, n, n, if intra { INTRA } else { 0 } | bflag);
        if intra {
            let pcm = part == PartMode::P2Nx2N && self.sps.pcm_enabled && log2 >= self.sps.log2_min_pcm_cb as usize && log2 <= self.sps.log2_max_pcm_cb as usize && self.cabac.terminate() != 0;
            if pcm {
                Meta::fill(&mut self.meta.flags, w4, x0, y0, n, n, INTRA | PCM | bflag);
                self.pcm_sample(x0, y0, log2)?;
                self.finish_cu(x0, y0, n);
                return Ok(());
            }
            self.intra_modes(x0, y0, log2, part == PartMode::PNxN);
            let split = (part == PartMode::PNxN) as usize;
            self.cu.max_trafo_depth = self.sps.max_transform_hierarchy_depth_intra as usize + split;
            self.transform_tree(x0, y0, x0, y0, log2, 0, 0, [true, true])?;
        } else {
            let first_merged = self.inter_prediction_units(x0, y0, n, part)?;
            let rqt_root_cbf = (part == PartMode::P2Nx2N && first_merged) || self.cabac.decision(ctx::RQT_ROOT_CBF) != 0;
            if rqt_root_cbf {
                self.cu.max_trafo_depth = self.sps.max_transform_hierarchy_depth_inter as usize;
                self.transform_tree(x0, y0, x0, y0, log2, 0, 0, [true, true])?;
            }
        }
        self.finish_cu(x0, y0, n);
        Ok(())
    }

    /// Record the coding unit's QpY (known once its transform tree has
    /// been parsed) and carry it to the next quantisation group.
    fn finish_cu(&mut self, x0: usize, y0: usize, n: usize) {
        let w4 = self.meta.w4;
        Meta::fill(&mut self.meta.qp, w4, x0, y0, n, n, self.qp_y as i8);
        self.carry.last_qp = self.qp_y;
    }

    /// Mark the left and top edges of a `w`×`h` block at (`x`, `y`).
    fn mark_edges(&mut self, x: usize, y: usize, w: usize, h: usize, left: u8, top: u8) {
        let w4 = self.meta.w4;
        let (x4, y4) = (x >> 2, y >> 2);
        for r in 0..h.div_ceil(4) {
            self.meta.edges[(y4 + r) * w4 + x4] |= left;
        }
        for c in 0..w.div_ceil(4) {
            self.meta.edges[y4 * w4 + x4 + c] |= top;
        }
    }

    /// part_mode (Table 9-45).
    fn part_mode(&mut self, intra: bool, log2: usize) -> Result<PartMode> {
        if self.cabac.decision(ctx::PART_MODE) != 0 {
            return Ok(PartMode::P2Nx2N);
        }
        if intra {
            return Ok(PartMode::PNxN);
        }
        let min_cb = self.sps.log2_min_cb as usize;
        if log2 == min_cb {
            if self.cabac.decision(ctx::PART_MODE + 1) != 0 {
                return Ok(PartMode::P2NxN);
            }
            if log2 == 3 || self.cabac.decision(ctx::PART_MODE + 2) != 0 {
                return Ok(PartMode::PNx2N);
            }
            return Ok(PartMode::PNxN);
        }
        let horizontal = self.cabac.decision(ctx::PART_MODE + 1) != 0;
        if !self.sps.amp_enabled {
            return Ok(if horizontal { PartMode::P2NxN } else { PartMode::PNx2N });
        }
        let no_amp = self.cabac.decision(ctx::PART_MODE + 3) != 0;
        Ok(match (horizontal, no_amp) {
            (true, true) => PartMode::P2NxN,
            (false, true) => PartMode::PNx2N,
            (true, false) => {
                if self.cabac.bypass() != 0 {
                    PartMode::P2NxnD
                } else {
                    PartMode::P2NxnU
                }
            }
            (false, false) => {
                if self.cabac.bypass() != 0 {
                    PartMode::PnRx2N
                } else {
                    PartMode::PnLx2N
                }
            }
        })
    }

    /// 7.3.8.7: PCM samples, read from the bitstream after pcm_flag.
    fn pcm_sample(&mut self, x0: usize, y0: usize, log2: usize) -> Result<()> {
        let data = self.cabac.data();
        let start = self.cabac.byte_pos();
        if start > data.len() {
            return Err(Error::Bitstream("PCM samples past the end of the slice"));
        }
        let mut r = BitReader::new(&data[start..]);
        let n = 1usize << log2;
        let (bd, pbd) = (self.bit_depth, self.sps.pcm_bit_depth);
        {
            let plane = &mut self.pic.planes[0];
            for j in 0..n {
                for i in 0..n {
                    let v = r.u(pbd)? << (bd - pbd);
                    plane.data[(y0 + j) * plane.stride + x0 + i] = P::new(v as i32);
                }
            }
        }
        if self.chroma {
            let pbd = self.sps.pcm_bit_depth_chroma;
            let nc = n / 2;
            for c in 1..3 {
                let plane = &mut self.pic.planes[c];
                for j in 0..nc {
                    for i in 0..nc {
                        let v = r.u(pbd)? << (self.sps.bit_depth_chroma - pbd);
                        plane.data[(y0 / 2 + j) * plane.stride + x0 / 2 + i] = P::new(v as i32);
                    }
                }
            }
        }
        let end = start + r.byte_pos();
        self.cabac.restart(end)
    }

    /// 7.3.8.5 / 8.4.2 / 8.4.3: the luma intra prediction modes of the
    /// prediction blocks and the chroma mode.
    fn intra_modes(&mut self, x0: usize, y0: usize, log2: usize, split: bool) {
        let parts = if split { 4 } else { 1 };
        let pb = if split { 1usize << (log2 - 1) } else { 1 << log2 };
        let mut prev_flag = [false; 4];
        for f in prev_flag.iter_mut().take(parts) {
            *f = self.cabac.decision(ctx::PREV_INTRA_LUMA) != 0;
        }
        let w4 = self.meta.w4;
        let mut first_mode = 0;
        for (i, &flag) in prev_flag.iter().enumerate().take(parts) {
            let (xp, yp) = (x0 + (i & 1) * pb, y0 + (i >> 1) * pb);
            let (mpm_idx, rem) = if flag { (self.cabac.bypass_unary(2), 0) } else { (0, self.cabac.bypass_bits(5)) };
            let cands = self.mpm_candidates(xp, yp);
            let mode = if flag {
                cands[mpm_idx as usize]
            } else {
                let mut sorted = cands;
                sorted.sort_unstable();
                let mut m = rem;
                for &c in &sorted {
                    if m >= c {
                        m += 1;
                    }
                }
                m
            };
            Meta::fill(&mut self.meta.ipm, w4, xp, yp, pb, pb, mode as u8);
            if i == 0 {
                first_mode = mode;
            }
        }
        if self.chroma {
            let icpm = if self.cabac.decision(ctx::INTRA_CHROMA) == 0 { 4 } else { self.cabac.bypass_bits(2) };
            self.cu.chroma_mode = match icpm {
                4 => first_mode,
                _ => {
                    let m = [intra::PLANAR, 26, 10, intra::DC][icpm as usize];
                    if m == first_mode {
                        34
                    } else {
                        m
                    }
                }
            };
        }
    }

    /// 8.4.2: candModeList of the prediction block at (`xp`, `yp`).
    fn mpm_candidates(&self, xp: usize, yp: usize) -> [u32; 3] {
        let (xi, yi) = (xp as i32, yp as i32);
        let cand = |xn: i32, yn: i32, above: bool| -> u32 {
            if !self.z_available(xi, yi, xn, yn) {
                return intra::DC;
            }
            let i = self.meta.at(xn as usize, yn as usize);
            let f = self.meta.flags[i];
            // an above neighbour outside the current CTB counts as DC
            if f & INTRA == 0 || f & PCM != 0 || (above && yn < ((yi >> self.sps.log2_ctb) << self.sps.log2_ctb)) {
                return intra::DC;
            }
            self.meta.ipm[i] as u32
        };
        let a = cand(xi - 1, yi, false);
        let b = cand(xi, yi - 1, true);
        if a == b {
            if a < 2 {
                [intra::PLANAR, intra::DC, 26]
            } else {
                [a, 2 + ((a + 29) % 32), 2 + ((a - 2 + 1) % 32)]
            }
        } else {
            let c = if a != intra::PLANAR && b != intra::PLANAR {
                intra::PLANAR
            } else if a != intra::DC && b != intra::DC {
                intra::DC
            } else {
                26
            };
            [a, b, c]
        }
    }

    // ---- transform tree ----

    /// 7.3.8.8: `transform_tree( )`; `parent_cbf` holds cbf_cb and cbf_cr
    /// of the parent node (true at the root).
    #[allow(clippy::too_many_arguments)]
    fn transform_tree(&mut self, x0: usize, y0: usize, xb: usize, yb: usize, log2: usize, depth: usize, blk: usize, parent_cbf: [bool; 2]) -> Result<()> {
        let max_tb = self.sps.log2_max_tb as usize;
        let min_tb = self.sps.log2_min_tb as usize;
        let intra_split = self.cu.intra && self.cu.part == PartMode::PNxN;
        let split = if log2 <= max_tb && log2 > min_tb && depth < self.cu.max_trafo_depth && !(intra_split && depth == 0) {
            self.cabac.decision(ctx::SPLIT_TRANSFORM + 5 - log2) != 0
        } else {
            let inter_split = self.sps.max_transform_hierarchy_depth_inter == 0 && !self.cu.intra && self.cu.part != PartMode::P2Nx2N && depth == 0;
            log2 > max_tb || (intra_split && depth == 0) || inter_split
        };
        let mut cbf = [false, false];
        if log2 > 2 && self.chroma {
            for c in 0..2 {
                if depth == 0 || parent_cbf[c] {
                    cbf[c] = self.cabac.decision(ctx::CBF_CHROMA + depth) != 0;
                }
            }
        } else if log2 == 2 {
            // 4x4 luma blocks: their chroma is coded with the parent's flags
            cbf = parent_cbf;
        }
        if split {
            let half = 1 << (log2 - 1);
            self.transform_tree(x0, y0, x0, y0, log2 - 1, depth + 1, 0, cbf)?;
            self.transform_tree(x0 + half, y0, x0, y0, log2 - 1, depth + 1, 1, cbf)?;
            self.transform_tree(x0, y0 + half, x0, y0, log2 - 1, depth + 1, 2, cbf)?;
            self.transform_tree(x0 + half, y0 + half, x0, y0, log2 - 1, depth + 1, 3, cbf)?;
            return Ok(());
        }
        let cbf_luma = if self.cu.intra || depth != 0 || cbf[0] || cbf[1] { self.cabac.decision(ctx::CBF_LUMA + (depth == 0) as usize) != 0 } else { true };
        self.transform_unit(x0, y0, xb, yb, log2, blk, cbf_luma, cbf)
    }

    /// 7.3.8.10: `transform_unit( )` with the reconstruction of its blocks.
    #[allow(clippy::too_many_arguments)]
    fn transform_unit(&mut self, x0: usize, y0: usize, xb: usize, yb: usize, log2: usize, blk: usize, cbf_luma: bool, cbf_chroma: [bool; 2]) -> Result<()> {
        let n = 1usize << log2;
        self.mark_edges(x0, y0, n, n, TU_LEFT, TU_TOP);
        let any_chroma = self.chroma && (cbf_chroma[0] || cbf_chroma[1]);
        if cbf_luma || any_chroma {
            if self.pps.cu_qp_delta_enabled && !self.cu_qp_delta_coded {
                self.cu_qp_delta = self.cabac.cu_qp_delta()?;
                self.cu_qp_delta_coded = true;
                let off = self.sps.qp_bd_offset();
                if self.cu_qp_delta < -(26 + off / 2) || self.cu_qp_delta > 25 + off / 2 {
                    return Err(Error::Bitstream("CuQpDeltaVal out of range"));
                }
                self.update_qp();
            }
            if self.hdr.cu_chroma_qp_offset_enabled && any_chroma && !self.cu.bypass && !self.chroma_qp_offset_coded {
                let list = &self.pps.chroma_qp_offset_list;
                if self.cabac.decision(ctx::CU_CHROMA_QP_OFFSET_FLAG) != 0 {
                    let mut idx = 0;
                    while idx + 1 < list.len() && self.cabac.decision(ctx::CU_CHROMA_QP_OFFSET_IDX) != 0 {
                        idx += 1;
                    }
                    self.cu_qp_offset = [list[idx].0, list[idx].1];
                } else {
                    self.cu_qp_offset = [0, 0];
                }
                self.chroma_qp_offset_coded = true;
            }
        }
        let intra = self.cu.intra;
        if intra {
            let mode = self.meta.ipm[self.meta.at(x0, y0)] as u32;
            self.intra_predict(0, x0, y0, n, mode);
        }
        if cbf_luma {
            let w4 = self.meta.w4;
            for r in (y0 >> 2)..((y0 + n) >> 2) {
                for f in &mut self.meta.flags[r * w4 + (x0 >> 2)..r * w4 + ((x0 + n) >> 2)] {
                    *f |= CODED;
                }
            }
            self.residual_block(0, x0, y0, log2)?;
        }
        if !self.chroma {
            return Ok(());
        }
        // 4:2:0 chroma: with the luma block when it is larger than 4x4, else
        // once for the four 4x4 luma blocks, with the last of them
        let (xc, yc, log2c) = if log2 > 2 {
            (x0 / 2, y0 / 2, log2 - 1)
        } else if blk == 3 {
            (xb / 2, yb / 2, 2)
        } else {
            return Ok(());
        };
        for c in 1..3 {
            if intra {
                self.intra_predict(c, xc, yc, 1 << log2c, self.cu.chroma_mode);
            }
            if cbf_chroma[c - 1] {
                self.residual_block(c, xc, yc, log2c)?;
            }
        }
        Ok(())
    }

    /// Parse one residual block of component `c` at component location
    /// (`x`, `y`) and add it to the prediction.
    fn residual_block(&mut self, c: usize, x: usize, y: usize, log2: usize) -> Result<()> {
        let n = 1usize << log2;
        let intra = self.cu.intra;
        let scan_idx = if intra && (log2 == 2 || (log2 == 3 && c == 0)) {
            let mode = if c == 0 { self.meta.ipm[self.meta.at(x, y)] as u32 } else { self.cu.chroma_mode };
            if (6..=14).contains(&mode) {
                2
            } else if (22..=30).contains(&mode) {
                1
            } else {
                0
            }
        } else {
            0
        };
        let qp = if c == 0 { self.qp_y + self.sps.qp_bd_offset() } else { self.chroma_qp(c) };
        let bd = if c == 0 { self.bit_depth } else { self.sps.bit_depth_chroma };
        let coded = self.residual_coding(log2, c, scan_idx, qp, bd)?;
        let bd_shift = 20 - bd;
        let nn = n * n;
        if self.cu.bypass {
            self.res[..nn].copy_from_slice(&self.coeffs[..nn]);
        } else if coded.transform_skip {
            let ts_shift = 5 + log2 as u32;
            let round = 1 << (bd_shift - 1);
            for i in 0..nn {
                self.res[i] = ((self.coeffs[i] << ts_shift) + round) >> bd_shift;
            }
        } else {
            let dst = intra && c == 0 && n == 4;
            transform::inverse_transform(&mut self.coeffs, n, dst, bd_shift, coded.max_x, coded.max_y, &mut self.res);
        }
        let plane = &mut self.pic.planes[c];
        let stride = plane.stride;
        transform::add_residual(&mut plane.data[y * stride + x..], stride, &self.res, n, bd);
        Ok(())
    }

    // ---- intra prediction ----

    /// 8.4.4.2: predict the `n`×`n` block of component `c` at (`xt`, `yt`)
    /// (component samples) with `mode`.
    fn intra_predict(&mut self, c: usize, xt: usize, yt: usize, n: usize, mode: u32) {
        let sub = (c > 0) as usize;
        // the block's luma location, and the component samples per 4x4 luma block
        let (xl, yl) = ((xt << sub) as i32, (yt << sub) as i32);
        let unit = 4 >> sub;
        let cip = self.pps.constrained_intra_pred;
        // The neighbours directly left of and above the block make up the
        // aligned blocks of its size there, which precede it in z-scan
        // order: one availability check covers each of the two runs.
        let near_left = self.z_available(xl, yl, xl - 1, yl);
        let near_top = self.z_available(xl, yl, xl, yl - 1);
        let usable = |s: &Self, near: Option<bool>, xn: i32, yn: i32| -> bool { near.unwrap_or_else(|| s.z_available(xl, yl, xn, yn)) && (!cip || s.meta.flags[s.meta.at(xn as usize, yn as usize)] & INTRA != 0) };
        let mut line = [0i32; MAX_LINE];
        let mut avail = [false; MAX_LINE];
        let len = 4 * n + 1;
        let corner = 2 * n;
        let plane = &self.pic.planes[c];
        let stride = plane.stride;
        let (pw, ph) = (plane.width, plane.height);
        // left column, bottom up to the corner: p[-1][y] is line[2n - 1 - y]
        if xt > 0 {
            let mut k = 0;
            while k < 2 * n {
                let yn = yt + k;
                if yn < ph && usable(self, (k < n).then_some(near_left), xl - 1, (yn << sub) as i32) {
                    for j in k..(k + unit).min(2 * n) {
                        if yt + j < ph {
                            line[corner - 1 - j] = plane.data[(yt + j) * stride + xt - 1].get();
                            avail[corner - 1 - j] = true;
                        }
                    }
                }
                k += unit;
            }
        }
        if xt > 0 && yt > 0 && usable(self, None, xl - 1, yl - 1) {
            line[corner] = plane.data[(yt - 1) * stride + xt - 1].get();
            avail[corner] = true;
        }
        // top row: p[x][-1] is line[2n + 1 + x]
        if yt > 0 {
            let mut k = 0;
            while k < 2 * n {
                let xn = xt + k;
                if xn < pw && usable(self, (k < n).then_some(near_top), (xn << sub) as i32, yl - 1) {
                    for i in k..(k + unit).min(2 * n) {
                        if xt + i < pw {
                            line[corner + 1 + i] = plane.data[(yt - 1) * stride + xt + i].get();
                            avail[corner + 1 + i] = true;
                        }
                    }
                }
                k += unit;
            }
        }
        let bd = if c == 0 { self.bit_depth } else { self.sps.bit_depth_chroma };
        intra::substitute(&mut line[..len], &avail[..len], bd);
        if c == 0 {
            intra::filter(&mut line[..len], n, mode, self.sps.strong_intra_smoothing, bd);
        }
        let plane = &mut self.pic.planes[c];
        intra::predict(&line[..len], n, mode, c == 0 && n < 32, bd, &mut plane.data[yt * stride + xt..], stride);
    }

    // ---- inter prediction ----

    /// The prediction units of an inter coding unit (7.3.8.5); returns
    /// whether the first one is merged.
    fn inter_prediction_units(&mut self, x0: usize, y0: usize, n: usize, part: PartMode) -> Result<bool> {
        let (h, q) = (n / 2, n / 4);
        let parts: &[(usize, usize, usize, usize)] = match part {
            PartMode::P2Nx2N => &[(0, 0, n, n)],
            PartMode::P2NxN => &[(0, 0, n, h), (0, h, n, h)],
            PartMode::PNx2N => &[(0, 0, h, n), (h, 0, h, n)],
            PartMode::P2NxnU => &[(0, 0, n, q), (0, q, n, n - q)],
            PartMode::P2NxnD => &[(0, 0, n, n - q), (0, n - q, n, q)],
            PartMode::PnLx2N => &[(0, 0, q, n), (q, 0, n - q, n)],
            PartMode::PnRx2N => &[(0, 0, n - q, n), (n - q, 0, q, n)],
            PartMode::PNxN => &[(0, 0, h, h), (h, 0, h, h), (0, h, h, h), (h, h, h, h)],
        };
        let mut first_merged = false;
        for (i, &(dx, dy, w, hh)) in parts.iter().enumerate() {
            let merged = self.prediction_unit(x0, y0, n, x0 + dx, y0 + dy, w, hh, i, false)?;
            if i == 0 {
                first_merged = merged;
            }
        }
        Ok(first_merged)
    }

    /// 7.3.8.6: `prediction_unit( )`, its motion, and its prediction
    /// samples; returns merge_flag.
    #[allow(clippy::too_many_arguments)]
    fn prediction_unit(&mut self, xc: usize, yc: usize, nc: usize, xp: usize, yp: usize, w: usize, h: usize, part_idx: usize, skip: bool) -> Result<bool> {
        let merge = skip || self.cabac.decision(ctx::MERGE_FLAG) != 0;
        let motion = if merge {
            let idx = self.cabac.merge_idx(self.hdr.max_num_merge_cand);
            self.merge_motion(xc, yc, nc, xp, yp, w, h, part_idx, idx as usize)
        } else {
            let mut pred = [true, false];
            if self.hdr.slice_type == SliceType::B {
                let depth = self.meta.depth[self.meta.at(xc, yc)] as usize;
                pred = if w + h != 12 && self.cabac.decision(ctx::INTER_PRED_IDC + depth) != 0 {
                    [true, true]
                } else if self.cabac.decision(ctx::INTER_PRED_IDC + 4) != 0 {
                    [false, true]
                } else {
                    [true, false]
                };
            }
            let mut ref_idx = [-1i8; 2];
            let mut mvd = [[0i32; 2]; 2];
            let mut mvp_flag = [0u32; 2];
            for l in 0..2 {
                if !pred[l] {
                    continue;
                }
                let num = self.hdr.num_ref_idx[l];
                ref_idx[l] = if num > 1 { self.cabac.ref_idx(num) as i8 } else { 0 };
                if l == 1 && self.hdr.mvd_l1_zero && pred[0] {
                    mvd[1] = [0, 0];
                } else {
                    mvd[l] = self.cabac.mvd()?;
                }
                mvp_flag[l] = self.cabac.decision(ctx::MVP_FLAG);
            }
            let mut m = Motion::NONE;
            for l in 0..2 {
                if !pred[l] {
                    continue;
                }
                if ref_idx[l] as usize >= self.refs[l].len() {
                    return Err(Error::Bitstream("reference index outside the list"));
                }
                let mvp = self.amvp(xc, yc, nc, xp, yp, w, h, part_idx, l, ref_idx[l] as usize, mvp_flag[l] as usize);
                m.ref_idx[l] = ref_idx[l];
                for k in 0..2 {
                    // (8-94 .. 8-97): the sum wraps to 16 bits
                    m.mv[l][k] = (mvp[k] as i32 + mvd[l][k]) as i16;
                }
            }
            m
        };
        for l in 0..2 {
            if motion.uses(l) && motion.ref_idx[l] as usize >= self.refs[l].len() {
                return Err(Error::Bitstream("reference index outside the list"));
            }
        }
        let w4 = self.meta.w4;
        Meta::fill(&mut self.meta.motion, w4, xp, yp, w, h, motion);
        self.mark_edges(xp, yp, w, h, PU_LEFT, PU_TOP);
        self.predict_inter(xp, yp, w, h, &motion);
        Ok(merge)
    }

    /// 8.5.3.3: the prediction samples of a prediction block.
    fn predict_inter(&mut self, xp: usize, yp: usize, w: usize, h: usize, m: &Motion) {
        let bi = m.uses(0) && m.uses(1);
        let comps = if self.chroma { 3 } else { 1 };
        for c in 0..comps {
            let (x, y, cw, ch) = if c == 0 { (xp, yp, w, h) } else { (xp / 2, yp / 2, w / 2, h / 2) };
            let bd = if c == 0 { self.bit_depth } else { self.sps.bit_depth_chroma };
            for l in 0..2 {
                if !m.uses(l) {
                    continue;
                }
                let r = &self.refs[l][m.ref_idx[l] as usize];
                let plane = &r.pic.planes[c];
                let dst = &mut self.pred[l];
                if c == 0 {
                    inter::luma(plane, x as i32, y as i32, m.mv[l], cw, ch, bd, dst, &mut self.scratch);
                } else {
                    inter::chroma(plane, x as i32, y as i32, m.mv[l], cw, ch, bd, dst, &mut self.scratch);
                }
            }
            let plane = &mut self.pic.planes[c];
            let stride = plane.stride;
            let dst = &mut plane.data[y * stride + x..];
            match &self.weights {
                Some(wt) => {
                    if bi {
                        let (a, b) = (&wt[0][m.ref_idx[0] as usize][c], &wt[1][m.ref_idx[1] as usize][c]);
                        let both = Weights { log2wd: a.log2wd, w: [a.w[0], b.w[1]], o: [a.o[0], b.o[1]] };
                        inter::put_weighted_bi(&self.pred[0], &self.pred[1], cw, ch, bd, &both, dst, stride);
                    } else {
                        let l = if m.uses(0) { 0 } else { 1 };
                        inter::put_weighted_uni(&self.pred[l], cw, ch, bd, &wt[l][m.ref_idx[l] as usize][c], l, dst, stride);
                    }
                }
                None => {
                    if bi {
                        inter::put_bi(&self.pred[0], &self.pred[1], cw, ch, bd, dst, stride);
                    } else {
                        let l = if m.uses(0) { 0 } else { 1 };
                        inter::put_uni(&self.pred[l], cw, ch, bd, dst, stride);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn z_order() {
        assert_eq!(morton(0, 0), 0);
        assert_eq!(morton(1, 0), 1);
        assert_eq!(morton(0, 1), 2);
        assert_eq!(morton(1, 1), 3);
        assert_eq!(morton(2, 0), 4);
        assert_eq!(morton(3, 3), 15);
        assert_eq!(morton(15, 15), 255);
    }
}
