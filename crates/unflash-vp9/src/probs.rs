//! Probability tables: the frame context (the probabilities a frame decodes
//! with, 10.5), the counts of decoded symbols (8.3) and the backward
//! adaptation that merges the two at the end of a frame (8.4).

use crate::tables::*;

/// One `T` per transform size, plane type (luma, chroma), reference (intra,
/// inter), band and context.
pub type PerCoefContext<T> = [[[[[T; 6]; 6]; 2]; 2]; 4];

/// Coefficient probabilities: `[tx size][plane > 0][inter][band][context][node]`.
pub type CoefProbs = PerCoefContext<[u8; 3]>;

/// The probabilities a frame decodes with; four of these are kept between
/// frames and a frame picks one with `frame_context_idx`.
#[derive(Clone)]
pub struct FrameContext {
    pub tx8: [[u8; 1]; 2],
    pub tx16: [[u8; 2]; 2],
    pub tx32: [[u8; 3]; 2],
    pub coef: CoefProbs,
    pub skip: [u8; 3],
    pub inter_mode: [[u8; 3]; 7],
    pub interp_filter: [[u8; 2]; 4],
    pub is_inter: [u8; 4],
    pub comp_mode: [u8; 5],
    pub single_ref: [[u8; 2]; 5],
    pub comp_ref: [u8; 5],
    pub y_mode: [[u8; 9]; 4],
    pub uv_mode: [[u8; 9]; 10],
    pub partition: [[u8; 3]; 16],
    pub mv_joint: [u8; 3],
    pub mv_sign: [u8; 2],
    pub mv_class: [[u8; 10]; 2],
    pub mv_class0_bit: [u8; 2],
    pub mv_bits: [[u8; 10]; 2],
    pub mv_class0_fr: [[[u8; 3]; 2]; 2],
    pub mv_fr: [[u8; 3]; 2],
    pub mv_class0_hp: [u8; 2],
    pub mv_hp: [u8; 2],
}

impl Default for FrameContext {
    fn default() -> Self {
        FrameContext {
            tx8: [[100], [66]],
            tx16: [[20, 152], [15, 101]],
            tx32: [[3, 136, 37], [5, 52, 13]],
            coef: DEFAULT_COEF_PROBS,
            skip: DEFAULT_SKIP_PROB,
            inter_mode: DEFAULT_INTER_MODE_PROBS,
            interp_filter: DEFAULT_INTERP_FILTER_PROBS,
            is_inter: DEFAULT_IS_INTER_PROB,
            comp_mode: DEFAULT_COMP_MODE_PROB,
            single_ref: DEFAULT_SINGLE_REF_PROB,
            comp_ref: DEFAULT_COMP_REF_PROB,
            y_mode: DEFAULT_Y_MODE_PROBS,
            uv_mode: DEFAULT_UV_MODE_PROBS,
            partition: DEFAULT_PARTITION_PROBS,
            mv_joint: DEFAULT_MV_JOINT_PROBS,
            mv_sign: DEFAULT_MV_SIGN_PROB,
            mv_class: DEFAULT_MV_CLASS_PROBS,
            mv_class0_bit: DEFAULT_MV_CLASS0_BIT_PROB,
            mv_bits: DEFAULT_MV_BITS_PROB,
            mv_class0_fr: DEFAULT_MV_CLASS0_FR_PROBS,
            mv_fr: DEFAULT_MV_FR_PROBS,
            mv_class0_hp: DEFAULT_MV_CLASS0_HP_PROB,
            mv_hp: DEFAULT_MV_HP_PROB,
        }
    }
}

/// How many times each symbol was decoded in each context during a frame.
/// Coefficient tokens are counted as zero, one, two-or-more and "no more
/// coefficients" (index 3), with the number of end-of-block checks kept
/// apart; together they give the spec's token and more_coefs counts.
#[derive(Clone, Default)]
pub struct Counts {
    pub y_mode: [[u32; 10]; 4],
    pub uv_mode: [[u32; 10]; 10],
    pub partition: [[u32; 4]; 16],
    pub interp_filter: [[u32; 3]; 4],
    pub inter_mode: [[u32; 4]; 7],
    pub tx8: [[u32; 2]; 2],
    pub tx16: [[u32; 3]; 2],
    pub tx32: [[u32; 4]; 2],
    pub is_inter: [[u32; 2]; 4],
    pub comp_mode: [[u32; 2]; 5],
    pub single_ref: [[[u32; 2]; 2]; 5],
    pub comp_ref: [[u32; 2]; 5],
    pub skip: [[u32; 2]; 3],
    pub mv_joint: [u32; 4],
    pub mv_sign: [[u32; 2]; 2],
    pub mv_class: [[u32; 11]; 2],
    pub mv_class0_bit: [[u32; 2]; 2],
    pub mv_bits: [[[u32; 2]; 10]; 2],
    pub mv_class0_fr: [[[u32; 4]; 2]; 2],
    pub mv_fr: [[u32; 4]; 2],
    pub mv_class0_hp: [[u32; 2]; 2],
    pub mv_hp: [[u32; 2]; 2],
    pub coef: PerCoefContext<[u32; 4]>,
    pub eob_branch: PerCoefContext<u32>,
}

impl Counts {
    pub fn clear(&mut self) {
        *self = Counts::default();
    }
}

// The decoding trees (9.3.1): pairs of children, a child <= 0 being the leaf
// with value -child.
pub const PARTITION_TREE: [i8; 6] = [0, 2, -1, 4, -2, -3];
pub const INTRA_MODE_TREE: [i8; 18] = [0, 2, -9, 4, -1, 6, 8, 12, -2, 10, -4, -5, -3, 14, -8, 16, -6, -7];
pub const SEGMENT_TREE: [i8; 14] = [2, 4, 6, 8, 10, 12, 0, -1, -2, -3, -4, -5, -6, -7];
pub const TX_SIZE_32_TREE: [i8; 6] = [0, 2, -1, 4, -2, -3];
pub const TX_SIZE_16_TREE: [i8; 4] = [0, 2, -1, -2];
pub const TX_SIZE_8_TREE: [i8; 2] = [0, -1];
/// Inter modes relative to NEARESTMV: 0 NEARESTMV, 1 NEARMV, 2 ZEROMV, 3 NEWMV.
pub const INTER_MODE_TREE: [i8; 6] = [-2, 2, 0, 4, -1, -3];
pub const INTERP_FILTER_TREE: [i8; 4] = [0, 2, -1, -2];
pub const MV_JOINT_TREE: [i8; 6] = [0, 2, -1, 4, -2, -3];
pub const MV_CLASS_TREE: [i8; 20] = [0, 2, -1, 4, 6, 8, -2, -3, 10, 12, -4, -5, -6, 14, 16, 18, -7, -8, -9, -10];
pub const MV_FR_TREE: [i8; 6] = [0, 2, -1, 4, -2, -3];
const BINARY_TREE: [i8; 2] = [0, -1];
/// The part of the token tree the model probabilities cover (8.4.3): ZERO,
/// ONE, and TWO standing for every larger token.
const SMALL_TOKEN_TREE: [i8; 6] = [0, 0, 0, 4, -1, -2];

const COUNT_SAT: u32 = 20;
const MAX_UPDATE_FACTOR: u32 = 128;
const COEF_COUNT_SAT: u32 = 24;

/// merge_prob (8.4.1).
fn merge_prob(pre: u8, ct0: u32, ct1: u32, count_sat: u32, max_update_factor: u32) -> u8 {
    let den = ct0 as u64 + ct1 as u64;
    let prob = if den == 0 { 128 } else { ((ct0 as u64 * 256 + (den >> 1)) / den).clamp(1, 255) };
    let count = den.min(count_sat as u64);
    let factor = max_update_factor as u64 * count / count_sat as u64;
    ((pre as u64 * (256 - factor) + prob * factor + 128) >> 8) as u8
}

/// merge_probs (8.4.2): adapt the probabilities of a tree from the counts of
/// its leaves; returns how often node `i` was passed.
fn merge_probs(tree: &[i8], i: usize, probs: &mut [u8], counts: &[u32], count_sat: u32, max_update_factor: u32) -> u32 {
    let side = |probs: &mut [u8], s: i8| if s <= 0 { counts[(-s) as usize] } else { merge_probs(tree, s as usize, probs, counts, count_sat, max_update_factor) };
    let left = side(probs, tree[i]);
    let right = side(probs, tree[i + 1]);
    probs[i >> 1] = merge_prob(probs[i >> 1], left, right, count_sat, max_update_factor);
    left + right
}

fn adapt_probs(tree: &[i8], probs: &mut [u8], counts: &[u32]) {
    merge_probs(tree, 0, probs, counts, COUNT_SAT, MAX_UPDATE_FACTOR);
}

fn adapt_prob(prob: &mut u8, counts: &[u32; 2]) {
    *prob = merge_prob(*prob, counts[0], counts[1], COUNT_SAT, MAX_UPDATE_FACTOR);
}

/// The coefficient adaptation (8.4.3): `fc.coef` becomes `pre.coef` merged
/// with the frame's counts. `update_factor` is 128 for the first inter frame
/// after a key frame, 112 otherwise.
pub fn adapt_coef_probs(fc: &mut FrameContext, pre: &FrameContext, counts: &Counts, update_factor: u32) {
    for t in 0..4 {
        for i in 0..2 {
            for j in 0..2 {
                for k in 0..6 {
                    let contexts = if k == 0 { 3 } else { 6 };
                    for l in 0..contexts {
                        let c = &counts.coef[t][i][j][k][l];
                        let more = [c[3], counts.eob_branch[t][i][j][k][l].saturating_sub(c[3])];
                        let mut p = pre.coef[t][i][j][k][l];
                        merge_probs(&SMALL_TOKEN_TREE, 2, &mut p, &c[..3], COEF_COUNT_SAT, update_factor);
                        merge_probs(&BINARY_TREE, 0, &mut p, &more, COEF_COUNT_SAT, update_factor);
                        fc.coef[t][i][j][k][l] = p;
                    }
                }
            }
        }
    }
}

/// The adaptation of every other probability (8.4.4), for inter frames:
/// they start again from `pre` (the probabilities the frame loaded, before
/// its forward updates) and those of tools the frame used are merged with
/// its counts. `fc.coef` is kept (adapted by `adapt_coef_probs`).
pub fn adapt_noncoef_probs(fc: &mut FrameContext, pre: &FrameContext, counts: &Counts, switchable: bool, tx_select: bool, allow_hp: bool) {
    let mut p = pre.clone();
    for i in 0..4 {
        adapt_prob(&mut p.is_inter[i], &counts.is_inter[i]);
    }
    for i in 0..5 {
        adapt_prob(&mut p.comp_mode[i], &counts.comp_mode[i]);
        adapt_prob(&mut p.comp_ref[i], &counts.comp_ref[i]);
        for j in 0..2 {
            adapt_prob(&mut p.single_ref[i][j], &counts.single_ref[i][j]);
        }
    }
    for i in 0..7 {
        adapt_probs(&INTER_MODE_TREE, &mut p.inter_mode[i], &counts.inter_mode[i]);
    }
    for i in 0..4 {
        adapt_probs(&INTRA_MODE_TREE, &mut p.y_mode[i], &counts.y_mode[i]);
    }
    for i in 0..10 {
        adapt_probs(&INTRA_MODE_TREE, &mut p.uv_mode[i], &counts.uv_mode[i]);
    }
    for i in 0..16 {
        adapt_probs(&PARTITION_TREE, &mut p.partition[i], &counts.partition[i]);
    }
    for i in 0..3 {
        adapt_prob(&mut p.skip[i], &counts.skip[i]);
    }
    if switchable {
        for i in 0..4 {
            adapt_probs(&INTERP_FILTER_TREE, &mut p.interp_filter[i], &counts.interp_filter[i]);
        }
    }
    if tx_select {
        for i in 0..2 {
            adapt_probs(&TX_SIZE_8_TREE, &mut p.tx8[i], &counts.tx8[i]);
            adapt_probs(&TX_SIZE_16_TREE, &mut p.tx16[i], &counts.tx16[i]);
            adapt_probs(&TX_SIZE_32_TREE, &mut p.tx32[i], &counts.tx32[i]);
        }
    }
    adapt_probs(&MV_JOINT_TREE, &mut p.mv_joint, &counts.mv_joint);
    for i in 0..2 {
        adapt_prob(&mut p.mv_sign[i], &counts.mv_sign[i]);
        adapt_probs(&MV_CLASS_TREE, &mut p.mv_class[i], &counts.mv_class[i]);
        adapt_prob(&mut p.mv_class0_bit[i], &counts.mv_class0_bit[i]);
        for j in 0..10 {
            adapt_prob(&mut p.mv_bits[i][j], &counts.mv_bits[i][j]);
        }
        for j in 0..2 {
            adapt_probs(&MV_FR_TREE, &mut p.mv_class0_fr[i][j], &counts.mv_class0_fr[i][j]);
        }
        adapt_probs(&MV_FR_TREE, &mut p.mv_fr[i], &counts.mv_fr[i]);
        if allow_hp {
            adapt_prob(&mut p.mv_class0_hp[i], &counts.mv_class0_hp[i]);
            adapt_prob(&mut p.mv_hp[i], &counts.mv_hp[i]);
        }
    }
    p.coef = fc.coef;
    *fc = p;
}
