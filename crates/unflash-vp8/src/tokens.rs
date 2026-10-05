//! DCT and WHT coefficient tokens (RFC 6386 section 13): the dequantised
//! coefficients of a macroblock, and the "has coefficients" contexts that
//! link neighbouring blocks.

use crate::bool_decoder::BoolDecoder;
use crate::header::Dequant;
use crate::tables::{COEFF_BANDS, DCT_CAT_PROBS, ZIGZAG};

/// Block types, the first index of the coefficient probabilities.
const Y_AFTER_Y2: usize = 0;
const Y2: usize = 1;
const CHROMA: usize = 2;
const Y_WITH_DC: usize = 3;

/// The index of the second-order luma DC block among a macroblock's 25.
pub const Y2_BLOCK: usize = 24;

/// Per-block "has coefficients" flags along one macroblock edge: four luma
/// columns (or rows), two U, two V and the Y2 block.
pub type NonZero = [u8; 9];

/// A macroblock's coefficients: 16 luma blocks, 4 U, 4 V and the Y2 block,
/// each in raster order and dequantised (to 16 bits, as libvpx and ffmpeg
/// store them). Kept zeroed between macroblocks: whoever uses a block
/// clears it.
#[derive(Default)]
pub struct Coeffs {
    pub blocks: [[i16; 16]; 25],
    /// For each block, the position after its last token: 0 when it has
    /// none, 1 when only the DC can be non-zero.
    pub eob: [u8; 25],
}

/// The value of a token past `DCT_1` (the tree from node 3 on).
#[inline(always)]
fn large_value(bd: &mut BoolDecoder, p: &[u8; 11]) -> i32 {
    if !bd.read(p[3]) {
        if !bd.read(p[4]) {
            2
        } else {
            3 + bd.read(p[5]) as i32
        }
    } else if !bd.read(p[6]) {
        if !bd.read(p[7]) {
            5 + bd.read(159) as i32
        } else {
            7 + 2 * bd.read(165) as i32 + bd.read(145) as i32
        }
    } else {
        let a = bd.read(p[8]) as usize;
        let cat = 2 * a + bd.read(p[9 + a]) as usize;
        let mut extra = 0;
        for &prob in DCT_CAT_PROBS[cat] {
            if prob == 0 {
                break;
            }
            extra = 2 * extra + bd.read(prob) as i32;
        }
        3 + (8 << cat) + extra
    }
}

/// 13.2: the tokens of one block, from position `first` with context `ctx`
/// (how many of the blocks above and to the left have coefficients).
/// Returns the position after the last token read (0: none). After a zero
/// token no end-of-block can follow, so that test is skipped.
#[inline(always)]
fn read_block(bd: &mut BoolDecoder, probs: &[[[u8; 11]; 3]; 8], ctx: usize, first: usize, dq: [i32; 2], out: &mut [i16; 16]) -> usize {
    let mut i = first;
    let mut p = &probs[COEFF_BANDS[i]][ctx];
    if !bd.read(p[0]) {
        return 0;
    }
    loop {
        if !bd.read(p[1]) {
            i += 1;
            if i == 16 {
                return 16;
            }
            p = &probs[COEFF_BANDS[i]][0];
            continue;
        }
        let (v, next_ctx) = if !bd.read(p[2]) { (1, 1) } else { (large_value(bd, p), 2) };
        let v = if bd.read_flag() { -v } else { v };
        out[ZIGZAG[i]] = (v * dq[(i > 0) as usize]) as i16;
        i += 1;
        if i == 16 {
            return 16;
        }
        p = &probs[COEFF_BANDS[i]][next_ctx];
        if !bd.read(p[0]) {
            return i;
        }
    }
}

/// 13: read a macroblock's tokens into `out` (which must be zeroed),
/// updating the contexts above and to the left. `y2` says whether the luma
/// DCs have their own block (every mode but `B_PRED` and `SPLITMV`).
/// Returns whether any block has coefficients.
pub fn read_mb(partition: &mut BoolDecoder, probs: &[[[[u8; 11]; 3]; 8]; 4], y2: bool, dq: &Dequant, above: &mut NonZero, left: &mut NonZero, out: &mut Coeffs) -> bool {
    // a copy of the decoder the compiler can keep in registers, where the
    // stores of the coefficients cannot touch it
    let mut local = partition.clone();
    let any = read_blocks(&mut local, probs, y2, dq, above, left, out);
    *partition = local;
    any
}

#[inline(always)]
fn read_blocks(bd: &mut BoolDecoder, probs: &[[[[u8; 11]; 3]; 8]; 4], y2: bool, dq: &Dequant, above: &mut NonZero, left: &mut NonZero, out: &mut Coeffs) -> bool {
    let mut any = 0;
    let (first, luma) = if y2 {
        let ctx = (above[8] + left[8]) as usize;
        let eob = read_block(bd, &probs[Y2], ctx, 0, dq.y2, &mut out.blocks[Y2_BLOCK]);
        above[8] = (eob > 0) as u8;
        left[8] = above[8];
        out.eob[Y2_BLOCK] = eob as u8;
        any |= eob;
        (1, Y_AFTER_Y2)
    } else {
        (0, Y_WITH_DC)
    };
    for b in 0..16 {
        let (x, y) = (b & 3, b >> 2);
        let ctx = (above[x] + left[y]) as usize;
        let eob = read_block(bd, &probs[luma], ctx, first, dq.y1, &mut out.blocks[b]);
        above[x] = (eob > 0) as u8;
        left[y] = above[x];
        out.eob[b] = eob as u8;
        any |= eob;
    }
    for b in 16..24 {
        // U then V, each 2x2 blocks: contexts 4, 5 for U and 6, 7 for V
        let base = 4 + 2 * ((b - 16) >> 2);
        let (x, y) = (base + (b & 1), base + ((b >> 1) & 1));
        let ctx = (above[x] + left[y]) as usize;
        let eob = read_block(bd, &probs[CHROMA], ctx, 0, dq.uv, &mut out.blocks[b]);
        above[x] = (eob > 0) as u8;
        left[y] = above[x];
        out.eob[b] = eob as u8;
        any |= eob;
    }
    any != 0
}

/// The contexts of a macroblock without coefficients (`mb_skip_coeff`):
/// every block reads as empty, but the Y2 context only changes when the
/// macroblock would have had a Y2 block.
pub fn skip_mb(y2: bool, above: &mut NonZero, left: &mut NonZero) {
    above[..8].fill(0);
    left[..8].fill(0);
    if y2 {
        above[8] = 0;
        left[8] = 0;
    }
}
