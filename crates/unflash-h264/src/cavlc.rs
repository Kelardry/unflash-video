//! CAVLC parsing of residual blocks (9.2).

use std::sync::OnceLock;

use crate::bitreader::BitReader;
use crate::tables::*;
use crate::{Error, Result};

#[derive(Clone, Copy)]
struct Entry {
    /// symbol, or the sub-table index when `len == SUB`
    value: i16,
    /// code length; 0 = no code; SUB = look up the next 8 bits in a sub-table
    len: u8,
}

const SUB: u8 = 0xff;

/// A variable-length code table for codes of up to 16 bits: 8 bits of
/// direct lookup, and a second 8-bit table for the longer codes.
pub struct Vlc {
    primary: Vec<Entry>,
    secondary: Vec<Entry>,
}

impl Vlc {
    fn build(codes: &[(u8, u16, i16)]) -> Vlc {
        let mut primary = vec![Entry { value: 0, len: 0 }; 256];
        let mut secondary: Vec<Entry> = Vec::new();
        for &(len, code, value) in codes {
            if len == 0 {
                continue;
            }
            assert!(len <= 16, "code too long");
            if len <= 8 {
                let start = (code as usize) << (8 - len);
                for k in 0..(1usize << (8 - len)) {
                    primary[start + k] = Entry { value, len };
                }
            } else {
                let prefix = (code >> (len - 8)) as usize;
                let sub = if primary[prefix].len == SUB {
                    primary[prefix].value as usize
                } else {
                    let idx = secondary.len() / 256;
                    secondary.extend(std::iter::repeat(Entry { value: 0, len: 0 }).take(256));
                    primary[prefix] = Entry { value: idx as i16, len: SUB };
                    idx
                };
                let rest_len = len - 8;
                let rest = (code & ((1 << rest_len) - 1)) as usize;
                let start = rest << (8 - rest_len);
                for k in 0..(1usize << (8 - rest_len)) {
                    secondary[sub * 256 + start + k] = Entry { value, len: rest_len };
                }
            }
        }
        Vlc { primary, secondary }
    }

    #[inline]
    pub fn read(&self, r: &mut BitReader) -> Result<i32> {
        let peek = r.peek(16) as usize;
        let e = self.primary[peek >> 8];
        if e.len == SUB {
            let e2 = self.secondary[e.value as usize * 256 + (peek & 0xff)];
            if e2.len == 0 {
                return Err(Error::Bitstream("invalid CAVLC code"));
            }
            r.skip(8 + e2.len as u32);
            Ok(e2.value as i32)
        } else {
            if e.len == 0 {
                return Err(Error::Bitstream("invalid CAVLC code"));
            }
            r.skip(e.len as u32);
            Ok(e.value as i32)
        }
    }
}

pub struct Tables {
    /// by nC class (0-1, 2-3, 4-7, 8+): value = total_coeff * 4 + trailing_ones
    coeff_token: [Vlc; 4],
    chroma_dc_coeff_token: Vlc,
    /// by total_coeff - 1
    total_zeros: [Vlc; 15],
    chroma_dc_total_zeros: [Vlc; 3],
    /// by min(zeros_left, 7) - 1
    run_before: [Vlc; 7],
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let ct = |i: usize| Vlc::build(&(0..68).map(|k| (COEFF_TOKEN_LEN[i][k], COEFF_TOKEN_BITS[i][k] as u16, k as i16)).collect::<Vec<_>>());
        let tz = |i: usize| Vlc::build(&(0..16).map(|k| (TOTAL_ZEROS_LEN[i][k], TOTAL_ZEROS_BITS[i][k] as u16, k as i16)).collect::<Vec<_>>());
        let ctz = |i: usize| Vlc::build(&(0..4).map(|k| (CHROMA_DC_TOTAL_ZEROS_LEN[i][k], CHROMA_DC_TOTAL_ZEROS_BITS[i][k] as u16, k as i16)).collect::<Vec<_>>());
        let rb = |i: usize| Vlc::build(&(0..16).map(|k| (RUN_LEN[i][k], RUN_BITS[i][k] as u16, k as i16)).collect::<Vec<_>>());
        Tables {
            coeff_token: [ct(0), ct(1), ct(2), ct(3)],
            chroma_dc_coeff_token: Vlc::build(&(0..20).map(|k| (CHROMA_DC_COEFF_TOKEN_LEN[k], CHROMA_DC_COEFF_TOKEN_BITS[k] as u16, k as i16)).collect::<Vec<_>>()),
            total_zeros: std::array::from_fn(tz),
            chroma_dc_total_zeros: std::array::from_fn(ctz),
            run_before: std::array::from_fn(rb),
        }
    })
}

/// Parse one residual block. `nc` is the coefficient-count context
/// (-1 for chroma DC), `start..=end` the coefficient positions the block
/// covers (0..=15, 1..=15 for AC blocks, 0..=3 for chroma DC). Coefficient
/// levels are written to `coeffs[start..=end]` in scan order; returns
/// TotalCoeff.
pub fn residual_block(r: &mut BitReader, nc: i32, start: usize, end: usize, coeffs: &mut [i32; 16]) -> Result<u8> {
    let t = tables();
    let max_num_coeff = end - start + 1;
    let token = if nc < 0 {
        t.chroma_dc_coeff_token.read(r)?
    } else {
        let cls = match nc {
            0 | 1 => 0,
            2 | 3 => 1,
            4..=7 => 2,
            _ => 3,
        };
        t.coeff_token[cls].read(r)?
    };
    let total_coeff = (token >> 2) as usize;
    let trailing_ones = (token & 3) as usize;
    if total_coeff > max_num_coeff {
        return Err(Error::Bitstream("more coefficients than the block holds"));
    }
    if total_coeff == 0 {
        return Ok(0);
    }
    let mut levels = [0i32; 16];
    let mut suffix_length = if total_coeff > 10 && trailing_ones < 3 { 1 } else { 0 };
    for i in 0..total_coeff {
        if i < trailing_ones {
            levels[i] = 1 - 2 * r.u(1)? as i32;
            continue;
        }
        let peek = r.peek32();
        let level_prefix = peek.leading_zeros();
        if level_prefix >= 32 {
            return Err(Error::Bitstream("bad level_prefix"));
        }
        r.skip(level_prefix + 1);
        let mut level_code = (level_prefix.min(15) << suffix_length) as i32;
        if suffix_length > 0 || level_prefix >= 14 {
            let size = if level_prefix == 14 && suffix_length == 0 {
                4
            } else if level_prefix >= 15 {
                level_prefix - 3
            } else {
                suffix_length
            };
            if size > 0 {
                level_code += r.u(size)? as i32;
            }
        }
        if level_prefix >= 15 && suffix_length == 0 {
            level_code += 15;
        }
        if level_prefix >= 16 {
            level_code += (1 << (level_prefix - 3)) - 4096;
        }
        if i == trailing_ones && trailing_ones < 3 {
            level_code += 2;
        }
        levels[i] = if level_code % 2 == 0 { (level_code + 2) >> 1 } else { (-level_code - 1) >> 1 };
        if suffix_length == 0 {
            suffix_length = 1;
        }
        if levels[i].abs() > (3 << (suffix_length - 1)) && suffix_length < 6 {
            suffix_length += 1;
        }
    }
    let mut zeros_left = if total_coeff < max_num_coeff {
        if nc < 0 {
            t.chroma_dc_total_zeros[total_coeff - 1].read(r)? as usize
        } else {
            t.total_zeros[total_coeff - 1].read(r)? as usize
        }
    } else {
        0
    };
    if zeros_left + total_coeff > max_num_coeff {
        return Err(Error::Bitstream("total_zeros too large"));
    }
    let mut runs = [0usize; 16];
    for i in 0..total_coeff - 1 {
        if zeros_left > 0 {
            let run = t.run_before[zeros_left.min(7) - 1].read(r)? as usize;
            if run > zeros_left {
                return Err(Error::Bitstream("run_before too large"));
            }
            runs[i] = run;
            zeros_left -= run;
        }
    }
    runs[total_coeff - 1] = zeros_left;
    let mut pos: isize = -1;
    for i in (0..total_coeff).rev() {
        pos += runs[i] as isize + 1;
        coeffs[start + pos as usize] = levels[i];
    }
    Ok(total_coeff as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_trailing_one() {
        // one coefficient of +1 with nC 0: coeff_token 01 (TotalCoeff 1,
        // TrailingOnes 1), its sign bit 0, then total_zeros 0 (1, Table 9-7)
        let bits = [0b0101_0000u8];
        let mut r = BitReader::new(&bits);
        let mut c = [0i32; 16];
        let n = residual_block(&mut r, 0, 0, 15, &mut c).unwrap();
        assert_eq!(n, 1);
        assert_eq!(c[0], 1);
        assert!(c[1..].iter().all(|&v| v == 0));
        // the same coefficient after two zeros: total_zeros 2 (010)
        let bits = [0b0100_1000u8];
        let mut r = BitReader::new(&bits);
        let mut c = [0i32; 16];
        residual_block(&mut r, 0, 0, 15, &mut c).unwrap();
        assert_eq!(c[2], 1);
        assert_eq!(c[0], 0);
    }
}
