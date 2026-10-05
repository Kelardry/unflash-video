//! Motion vector prediction (6.5): the candidate vectors of a block from
//! its neighbours in this frame and from the co-located block of the
//! previous frame, and the inter mode context counted on the way.

use crate::frame::Pixel;
use crate::header::INTRA_FRAME;
use crate::tables::{COUNTER_TO_CONTEXT, IDX_N_COLUMN_TO_SUBBLOCK, MODE_2_COUNTER, MV_REF_BLOCKS, NUM_8X8_HIGH, NUM_8X8_WIDE};
use crate::tile::{use_mv_hp, Block, Mv, TileDecoder};

/// Candidates may point this far (in eighth samples) past the frame edges.
const MV_BORDER: i32 = 16 << 3;
/// The margin of the final nearest / near / best vectors:
/// (BORDERINPIXELS - INTERP_EXTEND) << 3.
const BEST_BORDER: i32 = (160 - 4) << 3;

/// The list being built by find_mv_refs.
struct Candidates {
    list: [Mv; 2],
    count: usize,
}

impl Candidates {
    /// add_mv_ref_list (6.5.6).
    #[inline]
    fn add(&mut self, mv: Mv) {
        if self.count >= 2 || (self.count == 1 && mv == self.list[0]) {
            return;
        }
        self.list[self.count] = mv;
        self.count += 1;
    }
}

impl<'a, 'd, T: Pixel> TileDecoder<'a, 'd, T> {
    /// is_inside (6.5.2): candidates may be in tiles above but not in tiles
    /// to the side.
    #[inline]
    fn is_inside(&self, r: isize, c: isize) -> bool {
        r >= 0 && (r as usize) < self.mi_rows && c >= self.mi_col_start as isize && (c as usize) < self.mi_col_end
    }

    /// find_mv_refs (6.5.1): up to two candidate vectors for `ref_frame`
    /// (clamped to near the frame), and the inter mode context. `block` is
    /// the sub-block of a block smaller than 8x8, or -1.
    pub fn find_mv_refs(&self, b: &Block, ref_frame: u8, block: i32) -> ([Mv; 2], usize) {
        let mut cand = Candidates { list: [Mv::ZERO; 2], count: 0 };
        let mut different_ref_found = false;
        let mut context_counter = 0usize;
        let search = &MV_REF_BLOCKS[b.size];
        let bias = &self.fh.ref_frame_sign_bias;
        let (r, c) = (b.row as isize, b.col as isize);
        for (i, &(dr, dc)) in search.iter().enumerate() {
            let (cr, cc) = (r + dr as isize, c + dc as isize);
            if !self.is_inside(cr, cc) {
                continue;
            }
            let m = self.mi_at(cr as usize, cc as usize);
            different_ref_found = true;
            if i < 2 {
                context_counter += MODE_2_COUNTER[m.y_mode as usize] as usize;
            }
            for j in 0..2 {
                if m.ref_frame[j] == ref_frame {
                    // the two nearest neighbours of a sub-8x8 block offer the
                    // vector of their adjacent sub-block
                    let idx = if i < 2 && block >= 0 { IDX_N_COLUMN_TO_SUBBLOCK[block as usize][(dc == 0) as usize] as usize } else { 3 };
                    cand.add(m.mv[j][idx]);
                    break;
                }
            }
        }
        let prev = self.prev_mvs.map(|p| p[b.row * self.mi_cols + b.col]);
        if let Some(p) = prev {
            for j in 0..2 {
                if p.ref_frame[j] == ref_frame {
                    cand.add(p.mv[j]);
                    break;
                }
            }
        }
        // then vectors of other references, their sign flipped when that
        // reference lies in the other direction
        let add_diff = |cand: &mut Candidates, refs: [u8; 2], mvs: [Mv; 2]| {
            let same = mvs[0] == mvs[1];
            for j in 0..2 {
                if refs[j] > INTRA_FRAME && refs[j] != ref_frame && (j == 0 || !same) {
                    let mut mv = mvs[j];
                    if bias[refs[j] as usize] != bias[ref_frame as usize] {
                        mv = Mv { row: mv.row.wrapping_neg(), col: mv.col.wrapping_neg() };
                    }
                    cand.add(mv);
                }
            }
        };
        if different_ref_found {
            for &(dr, dc) in search.iter() {
                let (cr, cc) = (r + dr as isize, c + dc as isize);
                if self.is_inside(cr, cc) {
                    let m = self.mi_at(cr as usize, cc as usize);
                    add_diff(&mut cand, m.ref_frame, [m.mv[0][3], m.mv[1][3]]);
                }
            }
        }
        if let Some(p) = prev {
            add_diff(&mut cand, p.ref_frame, p.mv);
        }
        let mode_ctx = COUNTER_TO_CONTEXT[context_counter] as usize;
        let mut list = cand.list;
        for mv in list.iter_mut() {
            *mv = self.clamp_mv(b, *mv, MV_BORDER);
        }
        (list, mode_ctx)
    }

    /// clamp_mv_row / clamp_mv_col (6.5.4, 6.5.5): keep a vector within
    /// `border` eighth samples of the frame, measured from the block.
    fn clamp_mv(&self, b: &Block, mv: Mv, border: i32) -> Mv {
        let bh = NUM_8X8_HIGH[b.size] as i32;
        let bw = NUM_8X8_WIDE[b.size] as i32;
        let to_top = -((b.row as i32 * 8) * 8);
        let to_bottom = ((self.mi_rows as i32 - bh - b.row as i32) * 8) * 8;
        let to_left = -((b.col as i32 * 8) * 8);
        let to_right = ((self.mi_cols as i32 - bw - b.col as i32) * 8) * 8;
        Mv { row: (mv.row as i32).clamp(to_top - border, to_bottom + border) as i16, col: (mv.col as i32).clamp(to_left - border, to_right + border) as i16 }
    }

    /// find_best_ref_mvs (6.5.12): drop the eighth-sample bit where it may
    /// not be used and clamp again; the result gives NearestMv, NearMv and
    /// BestMv.
    pub fn find_best_ref_mvs(&self, b: &Block, list: [Mv; 2]) -> [Mv; 2] {
        let allow_hp = self.fh.allow_high_precision_mv;
        list.map(|mv| {
            let mut mv = mv;
            if !allow_hp || !use_mv_hp(mv) {
                let lower = |v: i16| if v & 1 != 0 { v - v.signum() } else { v };
                mv = Mv { row: lower(mv.row), col: lower(mv.col) };
            }
            self.clamp_mv(b, mv, BEST_BORDER)
        })
    }

    /// append_sub8x8_mvs (6.5.14): the nearest and near vectors of sub-block
    /// `block`, preferring the vectors of the sub-blocks already decoded.
    pub fn append_sub8x8_mvs(&self, b: &Block, block: usize, list_idx: usize) -> (Mv, Mv) {
        let (list, _) = self.find_mv_refs(b, b.ref_frame[list_idx], block as i32);
        let mvs = &b.mvs[list_idx];
        let mut out = [Mv::ZERO; 2];
        let mut n = 0;
        let push = |out: &mut [Mv; 2], mv: Mv, n: &mut usize| {
            if *n < 2 && (*n == 0 || mv != out[0]) {
                out[*n] = mv;
                *n += 1;
            }
        };
        match block {
            0 => {
                out = list;
                n = 2;
            }
            1 | 2 => push(&mut out, mvs[0], &mut n),
            _ => {
                push(&mut out, mvs[2], &mut n);
                push(&mut out, mvs[1], &mut n);
                push(&mut out, mvs[0], &mut n);
            }
        }
        for mv in list {
            push(&mut out, mv, &mut n);
        }
        (out[0], out[1])
    }
}
