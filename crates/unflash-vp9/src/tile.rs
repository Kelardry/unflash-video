//! Decoding a tile (6.4): the partition tree of each superblock and the
//! mode info of each block (intra and inter frames alike), with the
//! contexts their probabilities depend on (9.3.2). The residual and the
//! prediction of a block are in `residual.rs`, the motion vector
//! prediction in `mvpred.rs`.

use crate::booldec::BoolDecoder;
use crate::frame::{FrameBuf, Pixel, PAD};
use crate::header::*;
use crate::probs::*;
use crate::tables::*;

pub const DC_PRED: u8 = 0;
pub const TM_PRED: u8 = 9;
pub const NEARESTMV: u8 = 10;
pub const NEARMV: u8 = 11;
pub const ZEROMV: u8 = 12;
pub const NEWMV: u8 = 13;

const PARTITION_NONE: usize = 0;
const PARTITION_HORZ: usize = 1;
const PARTITION_VERT: usize = 2;
const PARTITION_SPLIT: usize = 3;

/// A motion vector in eighth samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mv {
    pub row: i16,
    pub col: i16,
}

impl Mv {
    pub const ZERO: Mv = Mv { row: 0, col: 0 };
}

/// What later blocks, the loop filter and the next frame need to know
/// about the block covering one 8x8 position.
#[derive(Clone, Copy, Debug, Default)]
pub struct MiInfo {
    /// The motion vectors of the four 4x4 sub-blocks, per reference (index 3
    /// is the block's vector for blocks of 8x8 and more).
    pub mv: [[Mv; 4]; 2],
    /// INTRA_FRAME or LAST..ALTREF; the second is NONE_FRAME unless compound.
    pub ref_frame: [u8; 2],
    pub size: u8,
    pub skip: bool,
    pub tx_size: u8,
    /// The intra mode (of the last sub-block when smaller than 8x8) or the
    /// inter mode (NEARESTMV..NEWMV).
    pub y_mode: u8,
    /// The intra modes of the four sub-blocks.
    pub sub_modes: [u8; 4],
    pub segment_id: u8,
    pub filter: u8,
}

/// The motion of one 8x8 position of the previous frame (PrevMvs,
/// PrevRefFrames).
#[derive(Clone, Copy, Debug, Default)]
pub struct PrevMv {
    pub ref_frame: [u8; 2],
    pub mv: [Mv; 2],
}

/// The block being decoded.
#[derive(Clone, Debug, Default)]
pub struct Block {
    pub row: usize,
    pub col: usize,
    pub size: usize,
    pub avail_u: bool,
    pub avail_l: bool,
    pub segment_id: u8,
    pub skip: bool,
    pub is_inter: bool,
    pub tx_size: usize,
    pub y_mode: u8,
    pub sub_modes: [u8; 4],
    pub uv_mode: u8,
    pub ref_frame: [u8; 2],
    pub filter: u8,
    /// BlockMvs: per reference, per 4x4 sub-block.
    pub mvs: [[Mv; 4]; 2],
    /// Transform blocks with coefficients (EobTotal).
    pub eob_total: usize,
}

/// The per-frame context arrays above the tile rows (cleared once a frame).
#[derive(Default)]
pub struct AboveCtx {
    /// Per plane, per 4 samples: the last transform block there had
    /// coefficients.
    pub nonzero: [Vec<u8>; 3],
    /// Per 8x8 column.
    pub partition: Vec<u8>,
    pub seg_pred: Vec<u8>,
}

impl AboveCtx {
    pub fn reset(&mut self, sb64_cols: usize) {
        let n = sb64_cols * 16;
        for (p, v) in self.nonzero.iter_mut().enumerate() {
            v.clear();
            v.resize(if p == 0 { n } else { n / 2 }, 0);
        }
        self.partition.clear();
        self.partition.resize(sb64_cols * 8, 0);
        self.seg_pred.clear();
        self.seg_pred.resize(sb64_cols * 8, 0);
    }
}

/// The context arrays left of the superblock being decoded (cleared at the
/// start of each superblock row of a tile).
#[derive(Default)]
pub struct LeftCtx {
    pub nonzero: [[u8; 16]; 3],
    pub partition: [u8; 8],
    pub seg_pred: [u8; 8],
}

/// A reference frame as the current frame sees it: the samples, and the
/// scale from the current frame's size to its (8.5.2.3).
pub struct RefFrame<'a, T> {
    pub buf: &'a FrameBuf<T>,
    /// (RefFrameWidth << 14) / FrameWidth, and the same for heights.
    pub x_scale: i32,
    pub y_scale: i32,
    /// Sampling steps in sixteenth samples (16 when not scaled).
    pub x_step: i32,
    pub y_step: i32,
    pub scaled: bool,
    /// The scale is within what VP9 allows (2x down to 16x up).
    pub valid: bool,
}

/// Scratch buffers kept between blocks.
pub struct Scratch<T> {
    /// The coefficients of a transform block (zeroed after use).
    pub coef: Vec<i32>,
    /// The energy class of each decoded token (TokenCache).
    pub token_cache: Vec<u8>,
    /// The horizontally filtered rows of inter prediction: 64 columns of up
    /// to 2 x 64 + 8 rows (a reference twice the size).
    pub mc_tmp: Vec<i32>,
    /// An edge-replicated copy of the reference samples a block needs.
    pub edge: Vec<T>,
}

impl<T: Pixel> Default for Scratch<T> {
    fn default() -> Self {
        Scratch { coef: vec![0; 32 * 32], token_cache: vec![0; 32 * 32], mc_tmp: vec![0; 64 * (2 * 64 + 8)], edge: vec![T::default(); (64 + 7) * (64 + 7) + PAD] }
    }
}

/// Decodes the blocks of one tile into the current frame.
pub struct TileDecoder<'a, 'd, T: Pixel> {
    pub fh: &'a FrameHeader,
    pub fc: &'a FrameContext,
    /// Symbol counts, when the frame adapts its probabilities.
    pub counts: Option<&'a mut Counts>,
    pub bd: BoolDecoder<'d>,
    pub cur: &'a mut FrameBuf<T>,
    /// LAST, GOLDEN and ALTREF (None in intra frames).
    pub refs: [Option<RefFrame<'a, T>>; 3],
    pub mi: &'a mut [MiInfo],
    pub mi_cols: usize,
    pub mi_rows: usize,
    pub prev_mvs: Option<&'a [PrevMv]>,
    pub prev_segment_ids: &'a [u8],
    pub above: &'a mut AboveCtx,
    pub left: LeftCtx,
    pub mi_row_start: usize,
    pub mi_row_end: usize,
    pub mi_col_start: usize,
    pub mi_col_end: usize,
    pub scratch: &'a mut Scratch<T>,
    /// Dequantisation factors: `[segment][plane > 0][dc, ac]`.
    pub dequant: [[[i32; 2]; 2]; 8],
    pub bit_depth: u32,
    /// Some block was decoded from invalid data (the frame is damaged).
    pub damaged: bool,
}

impl<'a, 'd, T: Pixel> TileDecoder<'a, 'd, T> {
    /// decode_tile (6.4.2).
    pub fn decode(&mut self) {
        for r in (self.mi_row_start..self.mi_row_end).step_by(8) {
            self.left = LeftCtx::default();
            for c in (self.mi_col_start..self.mi_col_end).step_by(8) {
                self.decode_partition(r, c, BLOCK_64X64);
            }
        }
        if self.bd.overran() {
            self.damaged = true;
        }
    }

    #[inline]
    pub fn mi_at(&self, r: usize, c: usize) -> &MiInfo {
        &self.mi[r * self.mi_cols + c]
    }

    fn decode_partition(&mut self, r: usize, c: usize, bsize: usize) {
        if r >= self.mi_rows || c >= self.mi_cols {
            return;
        }
        let num8x8 = NUM_8X8_WIDE[bsize] as usize;
        let half = num8x8 >> 1;
        let has_rows = r + half < self.mi_rows;
        let has_cols = c + half < self.mi_cols;
        let partition = self.read_partition(r, c, bsize, num8x8, has_rows, has_cols);
        let subsize = SUBSIZE_LOOKUP[partition][bsize] as usize;
        if subsize < BLOCK_8X8 || partition == PARTITION_NONE {
            self.decode_block(r, c, subsize);
        } else if partition == PARTITION_HORZ {
            self.decode_block(r, c, subsize);
            if has_rows {
                self.decode_block(r + half, c, subsize);
            }
        } else if partition == PARTITION_VERT {
            self.decode_block(r, c, subsize);
            if has_cols {
                self.decode_block(r, c + half, subsize);
            }
        } else {
            self.decode_partition(r, c, subsize);
            self.decode_partition(r, c + half, subsize);
            self.decode_partition(r + half, c, subsize);
            self.decode_partition(r + half, c + half, subsize);
        }
        if bsize == BLOCK_8X8 || partition != PARTITION_SPLIT {
            let above = 15 >> B_WIDTH_LOG2[subsize];
            let left = 15 >> B_HEIGHT_LOG2[subsize];
            for i in 0..num8x8 {
                self.above.partition[c + i] = above;
                self.left.partition[(r + i) & 7] = left;
            }
        }
    }

    fn read_partition(&mut self, r: usize, c: usize, bsize: usize, num8x8: usize, has_rows: bool, has_cols: bool) -> usize {
        let bsl = MI_WIDTH_LOG2[bsize] as usize;
        let boffset = 3 - bsl;
        let mut above = 0;
        let mut left = 0;
        for i in 0..num8x8 {
            above |= self.above.partition[c + i];
            left |= self.left.partition[(r + i) & 7];
        }
        let above = ((above >> boffset) & 1) as usize;
        let left = ((left >> boffset) & 1) as usize;
        let ctx = bsl * 4 + left * 2 + above;
        let probs = if self.fh.is_intra() { &KF_PARTITION_PROBS[ctx] } else { &self.fc.partition[ctx] };
        let p = if has_rows && has_cols {
            self.bd.read_tree(&PARTITION_TREE, probs)
        } else if has_cols {
            if self.bd.read(probs[1]) {
                PARTITION_SPLIT
            } else {
                PARTITION_HORZ
            }
        } else if has_rows {
            if self.bd.read(probs[2]) {
                PARTITION_SPLIT
            } else {
                PARTITION_VERT
            }
        } else {
            PARTITION_SPLIT
        };
        if let Some(c) = self.counts.as_deref_mut() {
            c.partition[ctx][p] += 1;
        }
        p
    }

    fn decode_block(&mut self, r: usize, c: usize, bsize: usize) {
        let mut b = Block { row: r, col: c, size: bsize, avail_u: r > 0, avail_l: c > self.mi_col_start, ..Default::default() };
        if self.fh.is_intra() {
            self.intra_frame_mode_info(&mut b);
        } else {
            self.inter_frame_mode_info(&mut b);
        }
        self.residual(&mut b);
        if b.is_inter && bsize >= BLOCK_8X8 && b.eob_total == 0 {
            b.skip = true;
        }
        let info = MiInfo {
            mv: b.mvs,
            ref_frame: b.ref_frame,
            size: bsize as u8,
            skip: b.skip,
            tx_size: b.tx_size as u8,
            y_mode: b.y_mode,
            sub_modes: b.sub_modes,
            segment_id: b.segment_id,
            filter: b.filter,
        };
        let w = (NUM_8X8_WIDE[bsize] as usize).min(self.mi_cols - c);
        let h = (NUM_8X8_HIGH[bsize] as usize).min(self.mi_rows - r);
        for y in 0..h {
            let row = (r + y) * self.mi_cols + c;
            self.mi[row..row + w].fill(info);
        }
    }

    // ---- intra frames ----------------------------------------------------------

    /// intra_frame_mode_info (6.4.6).
    fn intra_frame_mode_info(&mut self, b: &mut Block) {
        let seg = &self.fh.seg;
        b.segment_id = if seg.enabled && seg.update_map { self.bd.read_tree(&SEGMENT_TREE, &seg.tree_probs) as u8 } else { 0 };
        self.read_skip(b);
        b.tx_size = self.read_tx_size(b, true);
        b.ref_frame = [INTRA_FRAME, NONE_FRAME];
        b.is_inter = false;
        if b.size >= BLOCK_8X8 {
            let above = if b.avail_u { self.mi_at(b.row - 1, b.col).sub_modes[2] } else { DC_PRED };
            let left = if b.avail_l { self.mi_at(b.row, b.col - 1).sub_modes[1] } else { DC_PRED };
            let m = self.bd.read_tree(&INTRA_MODE_TREE, &KF_Y_MODE_PROBS[above as usize][left as usize]) as u8;
            b.y_mode = m;
            b.sub_modes = [m; 4];
        } else {
            let (w4, h4) = (NUM_4X4_WIDE[b.size] as usize, NUM_4X4_HIGH[b.size] as usize);
            let mut m = DC_PRED;
            for idy in (0..2).step_by(h4) {
                for idx in (0..2).step_by(w4) {
                    let above = if idy > 0 {
                        b.sub_modes[idx]
                    } else if b.avail_u {
                        self.mi_at(b.row - 1, b.col).sub_modes[2 + idx]
                    } else {
                        DC_PRED
                    };
                    let left = if idx > 0 {
                        b.sub_modes[idy * 2]
                    } else if b.avail_l {
                        self.mi_at(b.row, b.col - 1).sub_modes[1 + idy * 2]
                    } else {
                        DC_PRED
                    };
                    m = self.bd.read_tree(&INTRA_MODE_TREE, &KF_Y_MODE_PROBS[above as usize][left as usize]) as u8;
                    for y2 in 0..h4 {
                        for x2 in 0..w4 {
                            b.sub_modes[(idy + y2) * 2 + idx + x2] = m;
                        }
                    }
                }
            }
            b.y_mode = m;
        }
        b.uv_mode = self.bd.read_tree(&INTRA_MODE_TREE, &KF_UV_MODE_PROBS[b.y_mode as usize]) as u8;
    }

    fn read_skip(&mut self, b: &mut Block) {
        if self.fh.seg.active(b.segment_id, SEG_LVL_SKIP) {
            b.skip = true;
            return;
        }
        let mut ctx = 0;
        if b.avail_u {
            ctx += self.mi_at(b.row - 1, b.col).skip as usize;
        }
        if b.avail_l {
            ctx += self.mi_at(b.row, b.col - 1).skip as usize;
        }
        b.skip = self.bd.read(self.fc.skip[ctx]);
        if let Some(c) = self.counts.as_deref_mut() {
            c.skip[ctx][b.skip as usize] += 1;
        }
    }

    /// read_tx_size (6.4.10).
    fn read_tx_size(&mut self, b: &Block, allow_select: bool) -> usize {
        let max_tx = MAX_TX_SIZE[b.size] as usize;
        if !(allow_select && self.fh.tx_mode == TX_MODE_SELECT && b.size >= BLOCK_8X8) {
            return max_tx.min(TX_MODE_TO_BIGGEST_TX_SIZE[self.fh.tx_mode as usize] as usize);
        }
        let mut above = max_tx;
        let mut left = max_tx;
        if b.avail_u {
            let m = self.mi_at(b.row - 1, b.col);
            if !m.skip {
                above = m.tx_size as usize;
            }
        }
        if b.avail_l {
            let m = self.mi_at(b.row, b.col - 1);
            if !m.skip {
                left = m.tx_size as usize;
            }
        }
        if !b.avail_l {
            left = above;
        }
        if !b.avail_u {
            above = left;
        }
        let ctx = (above + left > max_tx) as usize;
        let fc = self.fc;
        let tx = match max_tx {
            1 => self.bd.read_tree(&TX_SIZE_8_TREE, &fc.tx8[ctx]),
            2 => self.bd.read_tree(&TX_SIZE_16_TREE, &fc.tx16[ctx]),
            _ => self.bd.read_tree(&TX_SIZE_32_TREE, &fc.tx32[ctx]),
        };
        if let Some(c) = self.counts.as_deref_mut() {
            match max_tx {
                1 => c.tx8[ctx][tx] += 1,
                2 => c.tx16[ctx][tx] += 1,
                _ => c.tx32[ctx][tx] += 1,
            }
        }
        tx
    }

    // ---- inter frames ----------------------------------------------------------

    /// The reference frames of the blocks above and to the left
    /// (AboveRefFrame, LeftRefFrame), as INTRA_FRAME / NONE when unavailable.
    fn neighbour_refs(&self, b: &Block) -> ([u8; 2], [u8; 2]) {
        let above = if b.avail_u { self.mi_at(b.row - 1, b.col).ref_frame } else { [INTRA_FRAME, NONE_FRAME] };
        let left = if b.avail_l { self.mi_at(b.row, b.col - 1).ref_frame } else { [INTRA_FRAME, NONE_FRAME] };
        (above, left)
    }

    /// inter_frame_mode_info (6.4.11).
    fn inter_frame_mode_info(&mut self, b: &mut Block) {
        self.inter_segment_id(b);
        self.read_skip(b);
        self.read_is_inter(b);
        b.tx_size = self.read_tx_size(b, !b.skip || !b.is_inter);
        if b.is_inter {
            self.inter_block_mode_info(b);
        } else {
            self.intra_block_mode_info(b);
        }
    }

    /// inter_segment_id (6.4.12).
    fn inter_segment_id(&mut self, b: &mut Block) {
        let seg = &self.fh.seg;
        if !seg.enabled {
            b.segment_id = 0;
            return;
        }
        if !seg.update_map {
            b.segment_id = self.predicted_segment_id(b);
            return;
        }
        if seg.temporal_update {
            let ctx = (self.left.seg_pred[b.row & 7] + self.above.seg_pred[b.col]) as usize;
            let pred = self.bd.read(seg.pred_probs[ctx]);
            b.segment_id = if pred { self.predicted_segment_id(b) } else { self.bd.read_tree(&SEGMENT_TREE, &seg.tree_probs) as u8 };
            for i in 0..NUM_8X8_WIDE[b.size] as usize {
                self.above.seg_pred[b.col + i] = pred as u8;
            }
            for i in 0..NUM_8X8_HIGH[b.size] as usize {
                self.left.seg_pred[(b.row + i) & 7] = pred as u8;
            }
        } else {
            b.segment_id = self.bd.read_tree(&SEGMENT_TREE, &seg.tree_probs) as u8;
        }
    }

    /// get_segment_id (6.4.14): the smallest segment of the previous map in
    /// the block's visible area.
    fn predicted_segment_id(&self, b: &Block) -> u8 {
        let w = (NUM_8X8_WIDE[b.size] as usize).min(self.mi_cols - b.col);
        let h = (NUM_8X8_HIGH[b.size] as usize).min(self.mi_rows - b.row);
        let mut seg = 7;
        for y in 0..h {
            let row = (b.row + y) * self.mi_cols + b.col;
            for &s in &self.prev_segment_ids[row..row + w] {
                seg = seg.min(s);
            }
        }
        seg
    }

    fn read_is_inter(&mut self, b: &mut Block) {
        let seg = &self.fh.seg;
        if seg.active(b.segment_id, SEG_LVL_REF_FRAME) {
            b.is_inter = seg.data(b.segment_id, SEG_LVL_REF_FRAME) != INTRA_FRAME as i32;
            return;
        }
        let (above, left) = self.neighbour_refs(b);
        let (above_intra, left_intra) = (above[0] == INTRA_FRAME, left[0] == INTRA_FRAME);
        let ctx = if b.avail_u && b.avail_l {
            if left_intra && above_intra {
                3
            } else {
                (left_intra || above_intra) as usize
            }
        } else if b.avail_u || b.avail_l {
            2 * (if b.avail_u { above_intra } else { left_intra }) as usize
        } else {
            0
        };
        b.is_inter = self.bd.read(self.fc.is_inter[ctx]);
        if let Some(c) = self.counts.as_deref_mut() {
            c.is_inter[ctx][b.is_inter as usize] += 1;
        }
    }

    /// intra_block_mode_info (6.4.15): an intra block in an inter frame.
    fn intra_block_mode_info(&mut self, b: &mut Block) {
        b.ref_frame = [INTRA_FRAME, NONE_FRAME];
        let fc = self.fc;
        if b.size >= BLOCK_8X8 {
            let ctx = SIZE_GROUP[b.size] as usize;
            let m = self.bd.read_tree(&INTRA_MODE_TREE, &fc.y_mode[ctx]);
            if let Some(c) = self.counts.as_deref_mut() {
                c.y_mode[ctx][m] += 1;
            }
            b.y_mode = m as u8;
            b.sub_modes = [m as u8; 4];
        } else {
            let (w4, h4) = (NUM_4X4_WIDE[b.size] as usize, NUM_4X4_HIGH[b.size] as usize);
            let mut m = 0;
            for idy in (0..2).step_by(h4) {
                for idx in (0..2).step_by(w4) {
                    m = self.bd.read_tree(&INTRA_MODE_TREE, &fc.y_mode[0]);
                    if let Some(c) = self.counts.as_deref_mut() {
                        c.y_mode[0][m] += 1;
                    }
                    for y2 in 0..h4 {
                        for x2 in 0..w4 {
                            b.sub_modes[(idy + y2) * 2 + idx + x2] = m as u8;
                        }
                    }
                }
            }
            b.y_mode = m as u8;
        }
        let m = self.bd.read_tree(&INTRA_MODE_TREE, &fc.uv_mode[b.y_mode as usize]);
        if let Some(c) = self.counts.as_deref_mut() {
            c.uv_mode[b.y_mode as usize][m] += 1;
        }
        b.uv_mode = m as u8;
    }

    /// inter_block_mode_info (6.4.16).
    fn inter_block_mode_info(&mut self, b: &mut Block) {
        self.read_ref_frames(b);
        let is_compound = b.ref_frame[1] > INTRA_FRAME;
        let mut best = [Mv::ZERO; 2];
        let mut nearest = [Mv::ZERO; 2];
        let mut near = [Mv::ZERO; 2];
        let mut mode_ctx = 0;
        for j in 0..1 + is_compound as usize {
            let (list, ctx) = self.find_mv_refs(b, b.ref_frame[j], -1);
            if j == 0 {
                mode_ctx = ctx;
            }
            let list = self.find_best_ref_mvs(b, list);
            nearest[j] = list[0];
            near[j] = list[1];
            best[j] = list[0];
        }
        let seg_skip = self.fh.seg.active(b.segment_id, SEG_LVL_SKIP);
        if seg_skip {
            b.y_mode = ZEROMV;
        } else if b.size >= BLOCK_8X8 {
            b.y_mode = self.read_inter_mode(mode_ctx);
        }
        b.filter = if self.fh.interp_filter == SWITCHABLE { self.read_interp_filter(b) } else { self.fh.interp_filter };
        if b.size < BLOCK_8X8 {
            let (w4, h4) = (NUM_4X4_WIDE[b.size] as usize, NUM_4X4_HIGH[b.size] as usize);
            for idy in (0..2).step_by(h4) {
                for idx in (0..2).step_by(w4) {
                    let block = idy * 2 + idx;
                    b.y_mode = self.read_inter_mode(mode_ctx);
                    if b.y_mode == NEARESTMV || b.y_mode == NEARMV {
                        for j in 0..1 + is_compound as usize {
                            let (n0, n1) = self.append_sub8x8_mvs(b, block, j);
                            nearest[j] = n0;
                            near[j] = n1;
                        }
                    }
                    let mv = self.assign_mv(b, is_compound, &nearest, &near, &best);
                    for y2 in 0..h4 {
                        for x2 in 0..w4 {
                            let k = (idy + y2) * 2 + idx + x2;
                            b.mvs[0][k] = mv[0];
                            b.mvs[1][k] = mv[1];
                        }
                    }
                }
            }
        } else {
            let mv = self.assign_mv(b, is_compound, &nearest, &near, &best);
            b.mvs = [[mv[0]; 4], [mv[1]; 4]];
        }
    }

    fn read_inter_mode(&mut self, ctx: usize) -> u8 {
        let m = self.bd.read_tree(&INTER_MODE_TREE, &self.fc.inter_mode[ctx]);
        if let Some(c) = self.counts.as_deref_mut() {
            c.inter_mode[ctx][m] += 1;
        }
        NEARESTMV + m as u8
    }

    fn read_interp_filter(&mut self, b: &Block) -> u8 {
        let (above, left) = self.neighbour_refs(b);
        let left_f = if b.avail_l && left[0] > INTRA_FRAME { self.mi_at(b.row, b.col - 1).filter } else { 3 };
        let above_f = if b.avail_u && above[0] > INTRA_FRAME { self.mi_at(b.row - 1, b.col).filter } else { 3 };
        let ctx = if left_f == above_f {
            left_f
        } else if left_f == 3 {
            above_f
        } else if above_f == 3 {
            left_f
        } else {
            3
        } as usize;
        let f = self.bd.read_tree(&INTERP_FILTER_TREE, &self.fc.interp_filter[ctx]);
        if let Some(c) = self.counts.as_deref_mut() {
            c.interp_filter[ctx][f] += 1;
        }
        f as u8
    }

    /// read_ref_frames (6.4.17).
    fn read_ref_frames(&mut self, b: &mut Block) {
        let seg = &self.fh.seg;
        if seg.active(b.segment_id, SEG_LVL_REF_FRAME) {
            b.ref_frame = [seg.data(b.segment_id, SEG_LVL_REF_FRAME) as u8 & 3, NONE_FRAME];
            return;
        }
        let fh = self.fh;
        let fc = self.fc;
        let compound = if fh.reference_mode == REFERENCE_MODE_SELECT {
            let ctx = self.comp_mode_ctx(b);
            let v = self.bd.read(fc.comp_mode[ctx]);
            if let Some(c) = self.counts.as_deref_mut() {
                c.comp_mode[ctx][v as usize] += 1;
            }
            v
        } else {
            fh.reference_mode == COMPOUND_REFERENCE
        };
        if compound {
            let idx = fh.ref_frame_sign_bias[fh.comp_fixed_ref as usize] as usize;
            let ctx = self.comp_ref_ctx(b);
            let v = self.bd.read(fc.comp_ref[ctx]);
            if let Some(c) = self.counts.as_deref_mut() {
                c.comp_ref[ctx][v as usize] += 1;
            }
            b.ref_frame[idx] = fh.comp_fixed_ref;
            b.ref_frame[1 - idx] = fh.comp_var_ref[v as usize];
        } else {
            let ctx = self.single_ref_p1_ctx(b);
            let p1 = self.bd.read(fc.single_ref[ctx][0]);
            if let Some(c) = self.counts.as_deref_mut() {
                c.single_ref[ctx][0][p1 as usize] += 1;
            }
            let rf = if p1 {
                let ctx = self.single_ref_p2_ctx(b);
                let p2 = self.bd.read(fc.single_ref[ctx][1]);
                if let Some(c) = self.counts.as_deref_mut() {
                    c.single_ref[ctx][1][p2 as usize] += 1;
                }
                if p2 {
                    ALTREF_FRAME
                } else {
                    GOLDEN_FRAME
                }
            } else {
                LAST_FRAME
            };
            b.ref_frame = [rf, NONE_FRAME];
        }
    }

    fn comp_mode_ctx(&self, b: &Block) -> usize {
        let (above, left) = self.neighbour_refs(b);
        let fixed = self.fh.comp_fixed_ref;
        let (above_single, left_single) = (above[1] == NONE_FRAME, left[1] == NONE_FRAME);
        let (above_intra, left_intra) = (above[0] == INTRA_FRAME, left[0] == INTRA_FRAME);
        if b.avail_u && b.avail_l {
            if above_single && left_single {
                ((above[0] == fixed) ^ (left[0] == fixed)) as usize
            } else if above_single {
                2 + (above[0] == fixed || above_intra) as usize
            } else if left_single {
                2 + (left[0] == fixed || left_intra) as usize
            } else {
                4
            }
        } else if b.avail_u {
            if above_single {
                (above[0] == fixed) as usize
            } else {
                3
            }
        } else if b.avail_l {
            if left_single {
                (left[0] == fixed) as usize
            } else {
                3
            }
        } else {
            1
        }
    }

    fn comp_ref_ctx(&self, b: &Block) -> usize {
        let (above, left) = self.neighbour_refs(b);
        let fh = self.fh;
        let fix_ref_idx = fh.ref_frame_sign_bias[fh.comp_fixed_ref as usize] as usize;
        let var_ref_idx = 1 - fix_ref_idx;
        let var0 = fh.comp_var_ref[0];
        let var1 = fh.comp_var_ref[1];
        let (above_single, left_single) = (above[1] == NONE_FRAME, left[1] == NONE_FRAME);
        let (above_intra, left_intra) = (above[0] == INTRA_FRAME, left[0] == INTRA_FRAME);
        if b.avail_u && b.avail_l {
            if above_intra && left_intra {
                2
            } else if left_intra {
                if above_single {
                    1 + 2 * (above[0] != var1) as usize
                } else {
                    1 + 2 * (above[var_ref_idx] != var1) as usize
                }
            } else if above_intra {
                if left_single {
                    1 + 2 * (left[0] != var1) as usize
                } else {
                    1 + 2 * (left[var_ref_idx] != var1) as usize
                }
            } else {
                let vrfa = if above_single { above[0] } else { above[var_ref_idx] };
                let vrfl = if left_single { left[0] } else { left[var_ref_idx] };
                if vrfa == vrfl && var1 == vrfa {
                    0
                } else if left_single && above_single {
                    if (vrfa == fh.comp_fixed_ref && vrfl == var0) || (vrfl == fh.comp_fixed_ref && vrfa == var0) {
                        4
                    } else if vrfa == vrfl {
                        3
                    } else {
                        1
                    }
                } else if left_single || above_single {
                    let vrfc = if left_single { vrfa } else { vrfl };
                    let rfs = if above_single { vrfa } else { vrfl };
                    if vrfc == var1 && rfs != var1 {
                        1
                    } else if rfs == var1 && vrfc != var1 {
                        2
                    } else {
                        4
                    }
                } else if vrfa == vrfl {
                    4
                } else {
                    2
                }
            }
        } else if b.avail_u {
            if above_intra {
                2
            } else if above_single {
                3 * (above[0] != var1) as usize
            } else {
                4 * (above[var_ref_idx] != var1) as usize
            }
        } else if b.avail_l {
            if left_intra {
                2
            } else if left_single {
                3 * (left[0] != var1) as usize
            } else {
                4 * (left[var_ref_idx] != var1) as usize
            }
        } else {
            2
        }
    }

    fn single_ref_p1_ctx(&self, b: &Block) -> usize {
        let (above, left) = self.neighbour_refs(b);
        let (above_single, left_single) = (above[1] == NONE_FRAME, left[1] == NONE_FRAME);
        let (above_intra, left_intra) = (above[0] == INTRA_FRAME, left[0] == INTRA_FRAME);
        let last = LAST_FRAME;
        if b.avail_u && b.avail_l {
            if above_intra && left_intra {
                2
            } else if left_intra {
                if above_single {
                    4 * (above[0] == last) as usize
                } else {
                    1 + (above[0] == last || above[1] == last) as usize
                }
            } else if above_intra {
                if left_single {
                    4 * (left[0] == last) as usize
                } else {
                    1 + (left[0] == last || left[1] == last) as usize
                }
            } else if above_single && left_single {
                2 * (above[0] == last) as usize + 2 * (left[0] == last) as usize
            } else if !above_single && !left_single {
                1 + (above[0] == last || above[1] == last || left[0] == last || left[1] == last) as usize
            } else {
                let rfs = if above_single { above[0] } else { left[0] };
                let crf1 = if above_single { left[0] } else { above[0] };
                let crf2 = if above_single { left[1] } else { above[1] };
                if rfs == last {
                    3 + (crf1 == last || crf2 == last) as usize
                } else {
                    (crf1 == last || crf2 == last) as usize
                }
            }
        } else if b.avail_u {
            if above_intra {
                2
            } else if above_single {
                4 * (above[0] == last) as usize
            } else {
                1 + (above[0] == last || above[1] == last) as usize
            }
        } else if b.avail_l {
            if left_intra {
                2
            } else if left_single {
                4 * (left[0] == last) as usize
            } else {
                1 + (left[0] == last || left[1] == last) as usize
            }
        } else {
            2
        }
    }

    fn single_ref_p2_ctx(&self, b: &Block) -> usize {
        let (above, left) = self.neighbour_refs(b);
        let (above_single, left_single) = (above[1] == NONE_FRAME, left[1] == NONE_FRAME);
        let (above_intra, left_intra) = (above[0] == INTRA_FRAME, left[0] == INTRA_FRAME);
        let (last, golden, altref) = (LAST_FRAME, GOLDEN_FRAME, ALTREF_FRAME);
        if b.avail_u && b.avail_l {
            if above_intra && left_intra {
                2
            } else if left_intra {
                if above_single {
                    if above[0] == last {
                        3
                    } else {
                        4 * (above[0] == golden) as usize
                    }
                } else {
                    1 + 2 * (above[0] == golden || above[1] == golden) as usize
                }
            } else if above_intra {
                if left_single {
                    if left[0] == last {
                        3
                    } else {
                        4 * (left[0] == golden) as usize
                    }
                } else {
                    1 + 2 * (left[0] == golden || left[1] == golden) as usize
                }
            } else if above_single && left_single {
                if above[0] == last && left[0] == last {
                    3
                } else if above[0] == last {
                    4 * (left[0] == golden) as usize
                } else if left[0] == last {
                    4 * (above[0] == golden) as usize
                } else {
                    2 * (above[0] == golden) as usize + 2 * (left[0] == golden) as usize
                }
            } else if !above_single && !left_single {
                if above[0] == left[0] && above[1] == left[1] {
                    3 * (above[0] == golden || above[1] == golden) as usize
                } else {
                    2
                }
            } else {
                let rfs = if above_single { above[0] } else { left[0] };
                let crf1 = if above_single { left[0] } else { above[0] };
                let crf2 = if above_single { left[1] } else { above[1] };
                if rfs == golden {
                    3 + (crf1 == golden || crf2 == golden) as usize
                } else if rfs == altref {
                    (crf1 == golden || crf2 == golden) as usize
                } else {
                    1 + 2 * (crf1 == golden || crf2 == golden) as usize
                }
            }
        } else if b.avail_u {
            if above_intra || (above[0] == last && above_single) {
                2
            } else if above_single {
                4 * (above[0] == golden) as usize
            } else {
                3 * (above[0] == golden || above[1] == golden) as usize
            }
        } else if b.avail_l {
            if left_intra || (left[0] == last && left_single) {
                2
            } else if left_single {
                4 * (left[0] == golden) as usize
            } else {
                3 * (left[0] == golden || left[1] == golden) as usize
            }
        } else {
            2
        }
    }

    /// assign_mv (6.4.18): the motion vectors of a block or sub-block.
    fn assign_mv(&mut self, b: &Block, is_compound: bool, nearest: &[Mv; 2], near: &[Mv; 2], best: &[Mv; 2]) -> [Mv; 2] {
        let mut mv = [Mv::ZERO; 2];
        for i in 0..1 + is_compound as usize {
            mv[i] = match b.y_mode {
                NEWMV => self.read_mv(best[i]),
                NEARESTMV => nearest[i],
                NEARMV => near[i],
                _ => Mv::ZERO,
            };
        }
        mv
    }

    /// read_mv (6.4.19): a new vector coded as a difference from `best`.
    fn read_mv(&mut self, best: Mv) -> Mv {
        let use_hp = self.fh.allow_high_precision_mv && use_mv_hp(best);
        let joint = self.bd.read_tree(&MV_JOINT_TREE, &self.fc.mv_joint);
        if let Some(c) = self.counts.as_deref_mut() {
            c.mv_joint[joint] += 1;
        }
        let mut diff = [0i32; 2];
        if joint == 2 || joint == 3 {
            diff[0] = self.read_mv_component(0, use_hp);
        }
        if joint == 1 || joint == 3 {
            diff[1] = self.read_mv_component(1, use_hp);
        }
        let row = best.row as i32 + diff[0];
        let col = best.col as i32 + diff[1];
        // conforming vectors satisfy -(1 << 14) < v < (1 << 14) - 1
        let valid = |v: i32| v > -(1 << 14) && v < (1 << 14) - 1;
        if !valid(row) || !valid(col) {
            self.damaged = true;
        }
        let clamp = |v: i32| v.clamp(1 - (1 << 14), (1 << 14) - 2) as i16;
        Mv { row: clamp(row), col: clamp(col) }
    }

    /// read_mv_component (6.4.20).
    fn read_mv_component(&mut self, comp: usize, use_hp: bool) -> i32 {
        let fc = self.fc;
        let sign = self.bd.read(fc.mv_sign[comp]);
        let class = self.bd.read_tree(&MV_CLASS_TREE, &fc.mv_class[comp]);
        let mag;
        let (fr, hp);
        if class == 0 {
            let bit = self.bd.read(fc.mv_class0_bit[comp]) as usize;
            fr = self.bd.read_tree(&MV_FR_TREE, &fc.mv_class0_fr[comp][bit]);
            hp = if use_hp { self.bd.read(fc.mv_class0_hp[comp]) as usize } else { 1 };
            if let Some(c) = self.counts.as_deref_mut() {
                c.mv_class0_bit[comp][bit] += 1;
                c.mv_class0_fr[comp][bit][fr] += 1;
                c.mv_class0_hp[comp][hp] += 1;
            }
            mag = ((bit << 3) | (fr << 1) | hp) + 1;
        } else {
            let mut d = 0;
            for i in 0..class {
                let bit = self.bd.read(fc.mv_bits[comp][i]) as usize;
                if let Some(c) = self.counts.as_deref_mut() {
                    c.mv_bits[comp][i][bit] += 1;
                }
                d |= bit << i;
            }
            fr = self.bd.read_tree(&MV_FR_TREE, &fc.mv_fr[comp]);
            hp = if use_hp { self.bd.read(fc.mv_hp[comp]) as usize } else { 1 };
            if let Some(c) = self.counts.as_deref_mut() {
                c.mv_fr[comp][fr] += 1;
                c.mv_hp[comp][hp] += 1;
            }
            mag = (2 << (class + 2)) + ((d << 3) | (fr << 1) | hp) + 1;
        }
        if let Some(c) = self.counts.as_deref_mut() {
            c.mv_sign[comp][sign as usize] += 1;
            c.mv_class[comp][class] += 1;
        }
        if sign {
            -(mag as i32)
        } else {
            mag as i32
        }
    }
}

/// use_mv_hp (6.5.13): small vectors may use eighth-sample precision.
#[inline]
pub fn use_mv_hp(mv: Mv) -> bool {
    ((mv.row as i32).abs() >> 3) < 8 && ((mv.col as i32).abs() >> 3) < 8
}
