//! Reading the raw byte sequence payload of a NAL unit: fixed-length fields,
//! Exp-Golomb codes and the CAVLC bit patterns.

use crate::{Error, Result};

/// The RBSP of a NAL unit (emulation-prevention bytes removed), read MSB
/// first.
pub struct BitReader<'a> {
    data: &'a [u8],
    /// bit position
    pos: usize,
}

/// Remove the `emulation_prevention_three_byte`s of a NAL unit payload.
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    #[inline]
    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn bits_left(&self) -> isize {
        self.data.len() as isize * 8 - self.pos as isize
    }

    /// The next 32 bits (zero-padded past the end), without consuming.
    #[inline]
    pub fn peek32(&self) -> u32 {
        let byte = self.pos >> 3;
        let mut v: u64 = 0;
        for i in 0..5 {
            v = (v << 8) | *self.data.get(byte + i).unwrap_or(&0) as u64;
        }
        ((v << (self.pos & 7)) >> 8) as u32
    }

    #[inline]
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        self.peek32() >> (32 - n)
    }

    #[inline]
    pub fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    /// u(n), n <= 32.
    #[inline]
    pub fn u(&mut self, n: u32) -> Result<u32> {
        if (self.pos + n as usize) > self.data.len() * 8 {
            return Err(Error::Bitstream("read past the end of the NAL unit"));
        }
        let v = self.peek(n);
        self.pos += n as usize;
        Ok(v)
    }

    #[inline]
    pub fn flag(&mut self) -> Result<bool> {
        Ok(self.u(1)? != 0)
    }

    /// ue(v): unsigned Exp-Golomb.
    #[inline]
    pub fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0u32;
        loop {
            let chunk = self.peek(16);
            if chunk != 0 {
                // the zeros of this chunk and the 1 that ends them
                let z = chunk.leading_zeros() - 16;
                zeros += z;
                self.pos += z as usize + 1;
                break;
            }
            zeros += 16;
            self.pos += 16;
            if zeros > 32 || self.bits_left() <= 0 {
                return Err(Error::Bitstream("bad Exp-Golomb code"));
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        if zeros > 31 {
            return Err(Error::Bitstream("Exp-Golomb code too long"));
        }
        let suffix = self.u(zeros)?;
        Ok((1u32 << zeros) - 1 + suffix)
    }

    /// ue(v) that must fit `max`.
    pub fn ue_max(&mut self, max: u32, what: &'static str) -> Result<u32> {
        let v = self.ue()?;
        if v > max {
            return Err(Error::Bitstream(what));
        }
        Ok(v)
    }

    /// se(v): signed Exp-Golomb.
    #[inline]
    pub fn se(&mut self) -> Result<i32> {
        let k = self.ue()?;
        if k & 1 == 1 {
            Ok(((k + 1) >> 1) as i32)
        } else {
            Ok(-((k >> 1) as i32))
        }
    }

    /// te(v) with the given range.
    #[inline]
    pub fn te(&mut self, range: u32) -> Result<u32> {
        if range > 1 {
            self.ue()
        } else {
            Ok(1 - self.u(1)?)
        }
    }

    /// Whether syntax follows before the `rbsp_trailing_bits`.
    pub fn more_rbsp_data(&self) -> bool {
        let total = self.data.len() * 8;
        if self.pos >= total {
            return false;
        }
        // find the last 1 bit of the payload (the stop bit)
        let mut last = self.data.len();
        while last > 0 && self.data[last - 1] == 0 {
            last -= 1;
        }
        if last == 0 {
            return false;
        }
        let b = self.data[last - 1];
        let stop_bit_pos = (last - 1) * 8 + (7 - b.trailing_zeros() as usize);
        self.pos < stop_bit_pos
    }

    /// Skip to the next byte boundary.
    pub fn byte_align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    #[inline]
    pub fn byte_pos(&self) -> usize {
        self.pos >> 3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        // codes for 0,1,2,3,4: 1, 010, 011, 00100, 00101 -> bits 1 010 011 00100 00101
        let bits = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut r = BitReader::new(&bits);
        assert_eq!(r.ue().unwrap(), 0);
        assert_eq!(r.ue().unwrap(), 1);
        assert_eq!(r.ue().unwrap(), 2);
        assert_eq!(r.ue().unwrap(), 3);
        assert_eq!(r.ue().unwrap(), 4);
        let bits = [0b0100_1100, 0b1000_0000]; // 010 011 00100
        let mut r = BitReader::new(&bits);
        assert_eq!(r.se().unwrap(), 1); // ue 1
        assert_eq!(r.se().unwrap(), -1); // ue 2
        assert_eq!(r.se().unwrap(), 2); // ue 3
        // codes of 16 leading zeros and more: 65535 = 16 zeros, then 1 0000000000000000; 65536 next
        let bits = [0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x40, 0x00, 0x40];
        let mut r = BitReader::new(&bits);
        assert_eq!(r.ue().unwrap(), 65535);
        assert_eq!(r.bit_pos(), 33);
        assert_eq!(r.ue().unwrap(), 65536);
        assert_eq!(r.bit_pos(), 66);
    }

    #[test]
    fn unescaping() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]), vec![0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(unescape(&[0, 0, 4]), vec![0, 0, 4]);
    }

    #[test]
    fn trailing_bits() {
        let bits = [0b1100_0000]; // 1 bit of payload then the stop bit
        let r = BitReader::new(&bits);
        assert!(r.more_rbsp_data());
        let mut r = BitReader::new(&bits);
        r.skip(1);
        assert!(!r.more_rbsp_data());
    }
}
