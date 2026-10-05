//! The constant tables of A/52 (the body for AC-3, Annex E for E-AC-3).
//!
//! The larger ones were extracted from the standard's text and checked
//! against ETSI TS 102 366 (the same standard in its ETSI edition, whose
//! tables were extracted separately) and, where a formula exists, against
//! the formula; the unit tests below repeat those checks.

use std::f32::consts::{FRAC_1_SQRT_2, SQRT_2};

/// Nominal bit rates in kbit/s for each `frmsizecod / 2` (Table 5.18).
pub const BITRATES: [u16; 19] = [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640];

/// An AC-3 sync frame's length in 16-bit words (Table 5.18), for `fscod`
/// 0 to 2 and `frmsizecod` 0 to 37.
pub fn ac3_frame_words(fscod: u8, frmsizecod: u8) -> Option<usize> {
    let kbps = *BITRATES.get(frmsizecod as usize / 2)? as usize;
    match fscod {
        0 => Some(kbps * 2),
        // 1536 samples at 44.1 kHz: kbps * 1536 / 44.1 / 16 words, rounded
        // down, and one word more for the odd codes
        1 => Some(kbps * 15360 / 7056 + (frmsizecod & 1) as usize),
        2 => Some(kbps * 3),
        _ => None,
    }
}

/// Sample rates for `fscod` (Table 5.6) and, for E-AC-3's `fscod` 3,
/// `fscod2` (Table E2.3).
pub const SAMPLE_RATES: [u32; 3] = [48000, 44100, 32000];
pub const REDUCED_SAMPLE_RATES: [u32; 3] = [24000, 22050, 16000];

/// Audio blocks per E-AC-3 sync frame for `numblkscod` (Table E2.4).
pub const BLOCKS_PER_FRAME: [usize; 4] = [1, 2, 3, 6];

/// Full bandwidth channels for `acmod` (Table 5.8).
pub const NFCHANS: [usize; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

// Bit allocation parameters (Tables 7.6 to 7.11).
pub const SLOWDEC: [i32; 4] = [0x0f, 0x11, 0x13, 0x15];
pub const FASTDEC: [i32; 4] = [0x3f, 0x53, 0x67, 0x7b];
pub const SLOWGAIN: [i32; 4] = [0x540, 0x4d8, 0x478, 0x410];
pub const DBPBTAB: [i32; 4] = [0x000, 0x700, 0x900, 0xb00];
/// The last entry, 0xf800, is a 16-bit negative number.
pub const FLOORTAB: [i32; 8] = [0x2f0, 0x2b0, 0x270, 0x230, 0x1f0, 0x170, 0x0f0, -0x800];
pub const FASTGAIN: [i32; 8] = [0x080, 0x100, 0x180, 0x200, 0x280, 0x300, 0x380, 0x400];

/// First bin of each of the 50 bit allocation bands (Table 7.12).
pub const BNDTAB: [u16; 50] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 31, 34, 37, 40, 43, 46, 49, 55, 61, 67, 73, 79, 85,
    97, 109, 121, 133, 157, 181, 205, 229,
];
/// Width of each band in bins (Table 7.12).
pub const BNDSZ: [u16; 50] = [
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 3, 3, 3, 3, 3, 3, 3, 6, 6, 6, 6, 6, 6, 12, 12, 12, 12, 24, 24, 24, 24, 24,
];

/// The band of each bin (Table 7.13), which is what `BNDTAB` and `BNDSZ`
/// say; bins 253 to 255 are in no band and read 0.
pub const MASKTAB: [u8; 256] = masktab();

const fn masktab() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut band = 0;
    while band < 50 {
        let mut i = 0;
        while i < BNDSZ[band] {
            t[(BNDTAB[band] + i) as usize] = band as u8;
            i += 1;
        }
        band += 1;
    }
    t
}

/// The log-addition table (Table 7.14).
pub const LATAB: [u16; 256] = [
    0x0040, 0x003f, 0x003e, 0x003d, 0x003c, 0x003b, 0x003a, 0x0039, 0x0038, 0x0037,
    0x0036, 0x0035, 0x0034, 0x0034, 0x0033, 0x0032, 0x0031, 0x0030, 0x002f, 0x002f,
    0x002e, 0x002d, 0x002c, 0x002c, 0x002b, 0x002a, 0x0029, 0x0029, 0x0028, 0x0027,
    0x0026, 0x0026, 0x0025, 0x0024, 0x0024, 0x0023, 0x0023, 0x0022, 0x0021, 0x0021,
    0x0020, 0x0020, 0x001f, 0x001e, 0x001e, 0x001d, 0x001d, 0x001c, 0x001c, 0x001b,
    0x001b, 0x001a, 0x001a, 0x0019, 0x0019, 0x0018, 0x0018, 0x0017, 0x0017, 0x0016,
    0x0016, 0x0015, 0x0015, 0x0015, 0x0014, 0x0014, 0x0013, 0x0013, 0x0013, 0x0012,
    0x0012, 0x0012, 0x0011, 0x0011, 0x0011, 0x0010, 0x0010, 0x0010, 0x000f, 0x000f,
    0x000f, 0x000e, 0x000e, 0x000e, 0x000d, 0x000d, 0x000d, 0x000d, 0x000c, 0x000c,
    0x000c, 0x000c, 0x000b, 0x000b, 0x000b, 0x000b, 0x000a, 0x000a, 0x000a, 0x000a,
    0x000a, 0x0009, 0x0009, 0x0009, 0x0009, 0x0009, 0x0008, 0x0008, 0x0008, 0x0008,
    0x0008, 0x0008, 0x0007, 0x0007, 0x0007, 0x0007, 0x0007, 0x0007, 0x0006, 0x0006,
    0x0006, 0x0006, 0x0006, 0x0006, 0x0006, 0x0006, 0x0005, 0x0005, 0x0005, 0x0005,
    0x0005, 0x0005, 0x0005, 0x0005, 0x0004, 0x0004, 0x0004, 0x0004, 0x0004, 0x0004,
    0x0004, 0x0004, 0x0004, 0x0004, 0x0004, 0x0003, 0x0003, 0x0003, 0x0003, 0x0003,
    0x0003, 0x0003, 0x0003, 0x0003, 0x0003, 0x0003, 0x0003, 0x0003, 0x0003, 0x0002,
    0x0002, 0x0002, 0x0002, 0x0002, 0x0002, 0x0002, 0x0002, 0x0002, 0x0002, 0x0002,
    0x0002, 0x0002, 0x0002, 0x0002, 0x0002, 0x0002, 0x0002, 0x0002, 0x0001, 0x0001,
    0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001,
    0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001,
    0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
];

/// The hearing threshold for `fscod` 0 to 2 by band (Table 7.15).
pub const HTH: [[u16; 50]; 3] = [
    [
        0x04d0, 0x04d0, 0x0440, 0x0400, 0x03e0, 0x03c0, 0x03b0, 0x03b0, 0x03a0, 0x03a0,
        0x03a0, 0x03a0, 0x03a0, 0x0390, 0x0390, 0x0390, 0x0380, 0x0380, 0x0370, 0x0370,
        0x0360, 0x0360, 0x0350, 0x0350, 0x0340, 0x0340, 0x0330, 0x0320, 0x0310, 0x0300,
        0x02f0, 0x02f0, 0x02f0, 0x02f0, 0x0300, 0x0310, 0x0340, 0x0390, 0x03e0, 0x0420,
        0x0460, 0x0490, 0x04a0, 0x0460, 0x0440, 0x0440, 0x0520, 0x0800, 0x0840, 0x0840,
    ],
    [
        0x04f0, 0x04f0, 0x0460, 0x0410, 0x03e0, 0x03d0, 0x03c0, 0x03b0, 0x03b0, 0x03a0,
        0x03a0, 0x03a0, 0x03a0, 0x03a0, 0x0390, 0x0390, 0x0390, 0x0380, 0x0380, 0x0380,
        0x0370, 0x0370, 0x0360, 0x0360, 0x0350, 0x0350, 0x0340, 0x0340, 0x0320, 0x0310,
        0x0300, 0x02f0, 0x02f0, 0x02f0, 0x02f0, 0x0300, 0x0320, 0x0350, 0x0390, 0x03e0,
        0x0420, 0x0450, 0x04a0, 0x0490, 0x0460, 0x0440, 0x0480, 0x0630, 0x0840, 0x0840,
    ],
    [
        0x0580, 0x0580, 0x04b0, 0x0450, 0x0420, 0x03f0, 0x03e0, 0x03d0, 0x03c0, 0x03b0,
        0x03b0, 0x03b0, 0x03a0, 0x03a0, 0x03a0, 0x03a0, 0x03a0, 0x03a0, 0x03a0, 0x03a0,
        0x0390, 0x0390, 0x0390, 0x0390, 0x0380, 0x0380, 0x0380, 0x0370, 0x0360, 0x0350,
        0x0340, 0x0330, 0x0320, 0x0310, 0x0300, 0x02f0, 0x02f0, 0x02f0, 0x0300, 0x0310,
        0x0330, 0x0350, 0x03c0, 0x0410, 0x0470, 0x04a0, 0x0460, 0x0440, 0x0450, 0x04e0,
    ],
];

/// Bit allocation pointers by address (Table 7.16).
pub const BAPTAB: [u8; 64] = [
    0, 1, 1, 1, 1, 1, 2, 2, 3, 3, 3, 4, 4, 5, 5, 6,
    6, 6, 6, 7, 7, 7, 7, 8, 8, 8, 8, 9, 9, 9, 9, 10,
    10, 10, 10, 11, 11, 11, 11, 12, 12, 12, 12, 13, 13, 13, 13, 14,
    14, 14, 14, 14, 14, 14, 14, 15, 15, 15, 15, 15, 15, 15, 15, 15,
];

/// High efficiency bit allocation pointers of the AHT (Table E3.1).
pub const HEBAPTAB: [u8; 64] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 8, 8, 8, 9, 9, 9, 10,
    10, 10, 10, 11, 11, 11, 11, 12, 12, 12, 12, 13, 13, 13, 13, 14,
    14, 14, 14, 15, 15, 15, 15, 16, 16, 16, 16, 17, 17, 17, 17, 18,
    18, 18, 18, 18, 18, 18, 18, 19, 19, 19, 19, 19, 19, 19, 19, 19,
];

/// Mantissa bits for `bap` 6 to 15 (Table 7.18; 0 to 5 are grouped or
/// symmetric and read elsewhere).
pub const QNTZTAB: [u32; 16] = [0, 0, 0, 0, 0, 0, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16];

/// Index bits of the vector quantizers, `hebap` 1 to 7, and mantissa bits
/// `m` of the scalar and gain-adaptive quantizers, `hebap` 8 to 19 (Table
/// E3.2).
pub const HEBAP_BITS: [u32; 20] = [0, 2, 3, 4, 5, 7, 8, 9, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16];

/// The large mantissa remapping constants (a, b) of the gain-adaptive
/// quantizer (Table E3.6), 16-bit two's complement fractions, for `hebap`
/// 8 to 19; for each, the gain 1, 2 and 4 quantizers, each with the b for
/// x >= 0 and the b for x < 0: `[a, b(x>=0), b(x<0)]`. Gains 2 and 4 do not
/// apply above hebap 16 (zeros there).
pub const GAQ_REMAP: [[[u16; 3]; 3]; 12] = [
    [[0x1249, 0x0000, 0x0000], [0xd555, 0x4000, 0xeaab], [0xedb7, 0x2000, 0xfb6e]],
    [[0x0889, 0x0000, 0x0000], [0xc925, 0x4000, 0xd249], [0xe666, 0x2000, 0xeccd]],
    [[0x0421, 0x0000, 0x0000], [0xc444, 0x4000, 0xc889], [0xe319, 0x2000, 0xe632]],
    [[0x0208, 0x0000, 0x0000], [0xc211, 0x4000, 0xc421], [0xe186, 0x2000, 0xe30c]],
    [[0x0102, 0x0000, 0x0000], [0xc104, 0x4000, 0xc208], [0xe0c2, 0x2000, 0xe183]],
    [[0x0081, 0x0000, 0x0000], [0xc081, 0x4000, 0xc102], [0xe060, 0x2000, 0xe0c1]],
    [[0x0040, 0x0000, 0x0000], [0xc040, 0x4000, 0xc081], [0xe030, 0x2000, 0xe060]],
    [[0x0020, 0x0000, 0x0000], [0xc020, 0x4000, 0xc040], [0xe018, 0x2000, 0xe030]],
    [[0x0010, 0x0000, 0x0000], [0xc010, 0x4000, 0xc020], [0xe00c, 0x2000, 0xe018]],
    [[0x0008, 0x0000, 0x0000], [0, 0, 0], [0, 0, 0]],
    [[0x0002, 0x0000, 0x0000], [0, 0, 0], [0, 0, 0]],
    [[0x0000, 0x0000, 0x0000], [0, 0, 0], [0, 0, 0]],
];

/// The exponent strategies (0 reuse, 1 D15, 2 D25, 3 D45) of the six
/// blocks for each frame exponent strategy code (Table E2.10).
pub const FRMEXPSTR: [[u8; 6]; 32] = [
    [1, 0, 0, 0, 0, 0],
    [1, 0, 0, 0, 0, 3],
    [1, 0, 0, 0, 2, 0],
    [1, 0, 0, 0, 3, 3],
    [2, 0, 0, 2, 0, 0],
    [2, 0, 0, 2, 0, 3],
    [2, 0, 0, 3, 2, 0],
    [2, 0, 0, 3, 3, 3],
    [2, 0, 1, 0, 0, 0],
    [2, 0, 2, 0, 0, 3],
    [2, 0, 2, 0, 2, 0],
    [2, 0, 2, 0, 3, 3],
    [2, 0, 3, 2, 0, 0],
    [2, 0, 3, 2, 0, 3],
    [2, 0, 3, 3, 2, 0],
    [2, 0, 3, 3, 3, 3],
    [3, 1, 0, 0, 0, 0],
    [3, 1, 0, 0, 0, 3],
    [3, 2, 0, 0, 2, 0],
    [3, 2, 0, 0, 3, 3],
    [3, 2, 0, 2, 0, 0],
    [3, 2, 0, 2, 0, 3],
    [3, 2, 0, 3, 2, 0],
    [3, 2, 0, 3, 3, 3],
    [3, 3, 1, 0, 0, 0],
    [3, 3, 2, 0, 0, 3],
    [3, 3, 2, 0, 2, 0],
    [3, 3, 2, 0, 3, 3],
    [3, 3, 3, 2, 0, 0],
    [3, 3, 3, 2, 0, 3],
    [3, 3, 3, 3, 2, 0],
    [3, 3, 3, 3, 3, 3],
];

/// The default spectral extension band structure, by sub-band (Table
/// E2.11).
pub const DEFAULT_SPX_BNDSTRC: [bool; 17] = [false, false, false, false, false, false, false, false, true, false, true, false, true, false, true, false, true];

/// The default coupling band structure, by coupling sub-band (Table
/// E2.12; sub-band 0 always starts a band).
pub const DEFAULT_CPL_BNDSTRC: [bool; 18] = [false, false, false, false, false, false, false, false, true, false, true, true, false, true, true, true, true, true];

/// First bin of each spectral extension sub-band (Table E3.13).
pub const fn spx_band_start(subband: usize) -> usize {
    25 + 12 * subband
}

/// Spectral extension notch filter attenuations for `spxattencod` 0 to 31
/// and bins 0 to 2 (Table E3.14): 2^-(j+1)(code+1)/15.
pub fn spx_atten(code: u8, j: usize) -> f32 {
    (2f64.powf(-((j as f64 + 1.0) * (code as f64 + 1.0)) / 15.0)) as f32
}

/// Table E3.14 as printed, for the unit test of `spx_atten`.
#[cfg(test)]
const SPX_ATTEN_TABLE: [[f64; 3]; 32] = [
    [0.954841604, 0.911722489, 0.870550563],
    [0.911722489, 0.831237896, 0.757858283],
    [0.870550563, 0.757858283, 0.659753955],
    [0.831237896, 0.690956440, 0.574349177],
    [0.793700526, 0.629960525, 0.500000000],
    [0.757858283, 0.574349177, 0.435275282],
    [0.723634619, 0.523647061, 0.378929142],
    [0.690956440, 0.477420802, 0.329876978],
    [0.659753955, 0.435275282, 0.287174589],
    [0.629960525, 0.396850263, 0.250000000],
    [0.601512518, 0.361817309, 0.217637641],
    [0.574349177, 0.329876978, 0.189464571],
    [0.548412490, 0.300756259, 0.164938489],
    [0.523647061, 0.274206245, 0.143587294],
    [0.500000000, 0.250000000, 0.125000000],
    [0.477420802, 0.227930622, 0.108818820],
    [0.455861244, 0.207809474, 0.094732285],
    [0.435275282, 0.189464571, 0.082469244],
    [0.415618948, 0.172739110, 0.071793647],
    [0.396850263, 0.157490131, 0.062500000],
    [0.378929142, 0.143587294, 0.054409410],
    [0.361817309, 0.130911765, 0.047366143],
    [0.345478220, 0.119355200, 0.041234622],
    [0.329876978, 0.108818820, 0.035896824],
    [0.314980262, 0.099212566, 0.031250000],
    [0.300756259, 0.090454327, 0.027204705],
    [0.287174589, 0.082469244, 0.023683071],
    [0.274206245, 0.075189065, 0.020617311],
    [0.261823531, 0.068551561, 0.017948412],
    [0.250000000, 0.062500000, 0.015625000],
    [0.238710401, 0.056982656, 0.013602353],
    [0.227930622, 0.051952369, 0.011841536],
];

/// The transform window (Table 7.33) as printed, to five decimals. The
/// decoder computes it to full precision instead (`window()`); the unit
/// test checks that the two agree.
#[cfg(test)]
#[allow(clippy::approx_constant)] // 0.78530 is the table's, not pi / 4
pub const WINDOW_TABLE: [f32; 256] = [
    0.00014, 0.00024, 0.00037, 0.00051, 0.00067, 0.00086, 0.00107, 0.00130, 0.00157, 0.00187,
    0.00220, 0.00256, 0.00297, 0.00341, 0.00390, 0.00443, 0.00501, 0.00564, 0.00632, 0.00706,
    0.00785, 0.00871, 0.00962, 0.01061, 0.01166, 0.01279, 0.01399, 0.01526, 0.01662, 0.01806,
    0.01959, 0.02121, 0.02292, 0.02472, 0.02662, 0.02863, 0.03073, 0.03294, 0.03527, 0.03770,
    0.04025, 0.04292, 0.04571, 0.04862, 0.05165, 0.05481, 0.05810, 0.06153, 0.06508, 0.06878,
    0.07261, 0.07658, 0.08069, 0.08495, 0.08935, 0.09389, 0.09859, 0.10343, 0.10842, 0.11356,
    0.11885, 0.12429, 0.12988, 0.13563, 0.14152, 0.14757, 0.15376, 0.16011, 0.16661, 0.17325,
    0.18005, 0.18699, 0.19407, 0.20130, 0.20867, 0.21618, 0.22382, 0.23161, 0.23952, 0.24757,
    0.25574, 0.26404, 0.27246, 0.28100, 0.28965, 0.29841, 0.30729, 0.31626, 0.32533, 0.33450,
    0.34376, 0.35311, 0.36253, 0.37204, 0.38161, 0.39126, 0.40096, 0.41072, 0.42054, 0.43040,
    0.44030, 0.45023, 0.46020, 0.47019, 0.48020, 0.49022, 0.50025, 0.51028, 0.52031, 0.53033,
    0.54033, 0.55031, 0.56026, 0.57019, 0.58007, 0.58991, 0.59970, 0.60944, 0.61912, 0.62873,
    0.63827, 0.64774, 0.65713, 0.66643, 0.67564, 0.68476, 0.69377, 0.70269, 0.71150, 0.72019,
    0.72877, 0.73723, 0.74557, 0.75378, 0.76186, 0.76981, 0.77762, 0.78530, 0.79283, 0.80022,
    0.80747, 0.81457, 0.82151, 0.82831, 0.83496, 0.84145, 0.84779, 0.85398, 0.86001, 0.86588,
    0.87160, 0.87716, 0.88257, 0.88782, 0.89291, 0.89785, 0.90264, 0.90728, 0.91176, 0.91610,
    0.92028, 0.92432, 0.92822, 0.93197, 0.93558, 0.93906, 0.94240, 0.94560, 0.94867, 0.95162,
    0.95444, 0.95713, 0.95971, 0.96217, 0.96451, 0.96674, 0.96887, 0.97089, 0.97281, 0.97463,
    0.97635, 0.97799, 0.97953, 0.98099, 0.98236, 0.98366, 0.98488, 0.98602, 0.98710, 0.98811,
    0.98905, 0.98994, 0.99076, 0.99153, 0.99225, 0.99291, 0.99353, 0.99411, 0.99464, 0.99513,
    0.99558, 0.99600, 0.99639, 0.99674, 0.99706, 0.99736, 0.99763, 0.99788, 0.99811, 0.99831,
    0.99850, 0.99867, 0.99882, 0.99895, 0.99908, 0.99919, 0.99929, 0.99938, 0.99946, 0.99953,
    0.99959, 0.99965, 0.99969, 0.99974, 0.99978, 0.99981, 0.99984, 0.99986, 0.99988, 0.99990,
    0.99992, 0.99993, 0.99994, 0.99995, 0.99996, 0.99997, 0.99998, 0.99998, 0.99998, 0.99999,
    0.99999, 0.99999, 0.99999, 1.00000, 1.00000, 1.00000, 1.00000, 1.00000, 1.00000, 1.00000,
    1.00000, 1.00000, 1.00000, 1.00000, 1.00000, 1.00000,
];

/// The transform window w[0..256] (Table 7.33): the first half of the
/// symmetric 512-point window, a Kaiser-Bessel-derived window with alpha
/// 5, which is what the printed table is to five decimals.
pub fn window() -> [f32; 256] {
    // zeroth order modified Bessel function of the first kind
    fn i0(x: f64) -> f64 {
        let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
        loop {
            term *= (x / (2.0 * k)) * (x / (2.0 * k));
            sum += term;
            k += 1.0;
            if term < 1e-20 * sum {
                return sum;
            }
        }
    }
    let alpha = 5.0;
    let kaiser: Vec<f64> = (0..=256)
        .map(|n| {
            let r = 2.0 * n as f64 / 256.0 - 1.0;
            i0(std::f64::consts::PI * alpha * (1.0 - r * r).max(0.0).sqrt())
        })
        .collect();
    let total: f64 = kaiser.iter().sum();
    let mut w = [0f32; 256];
    let mut acc = 0.0;
    for n in 0..256 {
        acc += kaiser[n];
        w[n] = (acc / total).sqrt() as f32;
    }
    w
}

/// The gain of a `dynrng` word (§7.7.1.2): 2^(X+1) * 0.1YYYYY in binary,
/// X the signed top three bits, from -24.08 dB to +23.95 dB; 0 is unity.
pub fn dynrng_gain(code: u8) -> f32 {
    let x = (code as i8 >> 5) as i32;
    let y = (code & 0x1f) as f32 + 32.0;
    y / 64.0 * 2f32.powi(x + 1)
}

/// Center mix levels for `cmixlev` (Table 5.9; the reserved code as -4.5
/// dB, as §5.4.2.4 allows).
pub const CMIXLEV: [f32; 4] = [FRAC_1_SQRT_2, 0.594_603_55, 0.5, 0.594_603_55];
/// Surround mix levels for `surmixlev` (Table 5.10; the reserved code as
/// -6 dB, as §5.4.2.5 allows).
pub const SURMIXLEV: [f32; 4] = [FRAC_1_SQRT_2, 0.5, 0.0, 0.5];
/// Lo/Ro center mix levels for `lorocmixlev` (Table D2.5; also E-AC-3).
pub const LOROCMIXLEV: [f32; 8] = [SQRT_2, 1.189_207_1, 1.0, 0.840_896_4, FRAC_1_SQRT_2, 0.594_603_55, 0.5, 0.0];
/// Lo/Ro surround mix levels for `lorosurmixlev` (Table D2.6; the reserved
/// codes 0 to 2 as 0.841, as the table's text says).
pub const LOROSURMIXLEV: [f32; 8] = [0.840_896_4, 0.840_896_4, 0.840_896_4, 0.840_896_4, FRAC_1_SQRT_2, 0.594_603_55, 0.5, 0.0];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_sizes_match_table_5_18() {
        // a few rows as printed
        assert_eq!(ac3_frame_words(0, 0), Some(64));
        assert_eq!(ac3_frame_words(1, 0), Some(69));
        assert_eq!(ac3_frame_words(1, 1), Some(70));
        assert_eq!(ac3_frame_words(2, 0), Some(96));
        assert_eq!(ac3_frame_words(1, 30), Some(975));
        assert_eq!(ac3_frame_words(1, 31), Some(976));
        assert_eq!(ac3_frame_words(1, 36), Some(1393));
        assert_eq!(ac3_frame_words(1, 37), Some(1394));
        assert_eq!(ac3_frame_words(0, 37), Some(1280));
        assert_eq!(ac3_frame_words(2, 37), Some(1920));
        assert_eq!(ac3_frame_words(1, 17), Some(279));
        assert_eq!(ac3_frame_words(1, 24), Some(557));
        assert_eq!(ac3_frame_words(1, 26), Some(696));
        assert_eq!(ac3_frame_words(0, 38), None);
        assert_eq!(ac3_frame_words(3, 0), None);
        // every 44.1 kHz frame lasts 1536 samples at its bit rate
        for code in 0..38u8 {
            let words = ac3_frame_words(1, code).unwrap() - (code & 1) as usize;
            let exact = BITRATES[code as usize / 2] as f64 * 1000.0 * 1536.0 / 44100.0 / 16.0;
            assert!(words as f64 <= exact && exact < words as f64 + 1.0, "{code}");
        }
    }

    #[test]
    fn bands_cover_the_bins_in_order() {
        for b in 0..49 {
            assert_eq!(BNDTAB[b] + BNDSZ[b], BNDTAB[b + 1]);
        }
        assert_eq!(BNDTAB[49] + BNDSZ[49], 253);
        // as printed in Table 7.13
        assert_eq!(&MASKTAB[25..40], &[25, 26, 27, 28, 28, 28, 29, 29, 29, 30, 30, 30, 31, 31, 31]);
        assert_eq!(MASKTAB[252], 49);
        assert_eq!(MASKTAB[229], 49);
        assert_eq!(MASKTAB[228], 48);
        assert_eq!(&MASKTAB[253..], &[0, 0, 0]);
    }

    /// Each latab entry is 10 log10(1 + 10^(-d/10)) dB for a level
    /// difference d of 2 * index psd steps (128 steps per 6.02 dB),
    /// within one step, and never increases.
    #[test]
    fn latab_is_the_log_addition_curve() {
        for (i, &v) in LATAB.iter().enumerate() {
            let step_db = 20.0 * 2f64.log10() / 128.0;
            let d = 2.0 * i as f64 * step_db;
            let exact = 10.0 * (1.0 + 10f64.powf(-d / 10.0)).log10() / step_db;
            assert!((v as f64 - exact).abs() <= 1.0, "latab[{i}]");
            if i > 0 {
                assert!(v <= LATAB[i - 1]);
            }
        }
    }

    #[test]
    fn bap_tables_are_monotonic() {
        for i in 1..64 {
            assert!(BAPTAB[i] >= BAPTAB[i - 1] && BAPTAB[i] <= BAPTAB[i - 1] + 1);
            assert!(HEBAPTAB[i] >= HEBAPTAB[i - 1] && HEBAPTAB[i] <= HEBAPTAB[i - 1] + 1);
        }
        assert_eq!((BAPTAB[63], HEBAPTAB[63]), (15, 19));
        // how many addresses map to each pointer, as printed
        let count = |t: &[u8; 64], v: u8| t.iter().filter(|&&x| x == v).count();
        let bap: Vec<usize> = (0..16).map(|v| count(&BAPTAB, v)).collect();
        assert_eq!(bap, [1, 5, 2, 3, 2, 2, 4, 4, 4, 4, 4, 4, 4, 4, 8, 9]);
        let hebap: Vec<usize> = (0..20).map(|v| count(&HEBAPTAB, v)).collect();
        assert_eq!(hebap, [1, 1, 1, 1, 1, 1, 1, 1, 4, 3, 4, 4, 4, 4, 4, 4, 4, 4, 8, 9]);
    }

    /// The remapping constants follow from the quantizers of Table E3.5:
    /// with m mantissa bits, gain 1 scales by (1 + a) = 2^m / (2^m - 1);
    /// the gain 2 large quantizer has 2^(m-1) levels from +-1/2 in steps
    /// of 1/(2^(m-1) - 1); the gain 4 one 2^m levels from +-1/4 in steps
    /// of 3/(2^(m+1) - 2).
    #[test]
    fn gaq_remap_constants_follow_the_quantizers() {
        let frac = |v: u16| v as i16 as f64 / 32768.0;
        for (i, row) in GAQ_REMAP.iter().enumerate() {
            let hebap = i + 8;
            let m = HEBAP_BITS[hebap] as i32;
            let q = 1.0 / 32768.0 * 1.5;
            let a1 = 2f64.powi(m) / (2f64.powi(m) - 1.0) - 1.0;
            assert!((frac(row[0][0]) - a1).abs() < q, "hebap {hebap} gain 1");
            assert_eq!((row[0][1], row[0][2]), (0, 0));
            if hebap > 16 {
                continue;
            }
            let step2 = 1.0 / (2f64.powi(m - 1) - 1.0);
            let a2 = step2 * 2f64.powi(m - 2) - 1.0;
            assert!((frac(row[1][0]) - a2).abs() < q, "hebap {hebap} gain 2 a");
            assert!((frac(row[1][1]) - 0.5).abs() < q);
            assert!((frac(row[1][2]) - (step2 - 0.5)).abs() < q, "hebap {hebap} gain 2 b");
            let step4 = 3.0 / (2f64.powi(m + 1) - 2.0);
            let a4 = step4 * 2f64.powi(m - 1) - 1.0;
            assert!((frac(row[2][0]) - a4).abs() < q, "hebap {hebap} gain 4 a");
            assert!((frac(row[2][1]) - 0.25).abs() < q);
            assert!((frac(row[2][2]) - (step4 - 0.25)).abs() < q, "hebap {hebap} gain 4 b");
        }
    }

    #[test]
    fn frame_exponent_strategies_start_with_new_exponents() {
        for (i, row) in FRMEXPSTR.iter().enumerate() {
            assert_ne!(row[0], 0, "{i}");
            assert!(row.iter().all(|&s| s <= 3));
        }
        // all 32 rows differ
        for (i, row) in FRMEXPSTR.iter().enumerate() {
            assert!(FRMEXPSTR[..i].iter().all(|other| other != row), "{i}");
        }
    }

    #[test]
    fn spx_attenuation_matches_table_e3_14() {
        for (code, row) in SPX_ATTEN_TABLE.iter().enumerate() {
            for (j, &v) in row.iter().enumerate() {
                assert!((spx_atten(code as u8, j) as f64 - v).abs() < 1e-7, "{code} {j}");
            }
        }
    }

    #[test]
    fn computed_window_matches_table_7_33() {
        let w = window();
        for n in 0..256 {
            assert!((w[n] - WINDOW_TABLE[n]).abs() <= 0.5e-5 + 1e-7, "w[{n}] {} vs {}", w[n], WINDOW_TABLE[n]);
        }
        // the window is power complementary: w[n]^2 + w[511-n]^2 = 1
        for n in 0..256 {
            let (a, b) = (w[n] as f64, w[255 - n] as f64);
            assert!((a * a + b * b - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn dynrng_gains() {
        assert_eq!(dynrng_gain(0), 1.0);
        assert!((20.0 * dynrng_gain(0x7f).log10() - 23.95).abs() < 0.01);
        assert!((20.0 * dynrng_gain(0x80).log10() + 24.08).abs() < 0.01);
        // X = -1 is 0 dB, Y = 0.100000 is -6.02 dB
        assert!((20.0 * dynrng_gain(0xe0).log10() + 6.02).abs() < 0.01);
        assert!((20.0 * dynrng_gain(0xff).log10() + 0.14).abs() < 0.01);
    }
}
