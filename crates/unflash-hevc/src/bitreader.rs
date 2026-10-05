//! Reading the raw byte sequence payload of a NAL unit: fixed-length fields
//! and Exp-Golomb codes (9.2).

use crate::{Error, Result};

/// The RBSP of a NAL unit (emulation-prevention bytes removed), read MSB
/// first.
pub struct BitReader<'a> {
    data: &'a [u8],
    /// bit position
    pos: usize,
}

/// Remove the `emulation_prevention_three_byte`s of a NAL unit payload.
pub fn unescape(nal: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(nal.len());
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
    fn peek32(&self) -> u32 {
        let byte = self.pos >> 3;
        let mut v: u64 = 0;
        for i in 0..5 {
            v = (v << 8) | *self.data.get(byte + i).unwrap_or(&0) as u64;
        }
        ((v << (self.pos & 7)) >> 8) as u32
    }

    /// u(n), n <= 32.
    #[inline]
    pub fn u(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        if self.pos + n as usize > self.data.len() * 8 {
            return Err(Error::Bitstream("read past the end of the NAL unit"));
        }
        let v = self.peek32() >> (32 - n);
        self.pos += n as usize;
        Ok(v)
    }

    #[inline]
    pub fn flag(&mut self) -> Result<bool> {
        Ok(self.u(1)? != 0)
    }

    /// The next `n` bits (n <= 32) without consuming them, if there are
    /// that many.
    pub fn peek(&self, n: u32) -> Option<u32> {
        if n == 0 || self.pos + n as usize > self.data.len() * 8 {
            return None;
        }
        Some(self.peek32() >> (32 - n))
    }

    /// Return to an earlier bit position (to retry a parse).
    pub fn seek(&mut self, pos: usize) {
        self.pos = pos.min(self.data.len() * 8);
    }

    /// Skip `n` bits (which must be there).
    pub fn skip(&mut self, n: usize) -> Result<()> {
        if self.pos + n > self.data.len() * 8 {
            return Err(Error::Bitstream("read past the end of the NAL unit"));
        }
        self.pos += n;
        Ok(())
    }

    /// ue(v): unsigned Exp-Golomb, up to 2^32 - 2.
    pub fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0u32;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::Bitstream("Exp-Golomb code too long"));
            }
        }
        let suffix = self.u(zeros)?;
        Ok(((1u64 << zeros) - 1 + suffix as u64) as u32)
    }

    /// ue(v) that must not exceed `max`.
    pub fn ue_max(&mut self, max: u32, what: &'static str) -> Result<u32> {
        let v = self.ue()?;
        if v > max {
            return Err(Error::Bitstream(what));
        }
        Ok(v)
    }

    /// se(v): signed Exp-Golomb.
    pub fn se(&mut self) -> Result<i32> {
        let k = self.ue()? as i64;
        let v = if k & 1 == 1 { (k + 1) >> 1 } else { -(k >> 1) };
        Ok(v as i32)
    }

    /// se(v) that must lie in `min..=max`.
    pub fn se_range(&mut self, min: i32, max: i32, what: &'static str) -> Result<i32> {
        let v = self.se()?;
        if v < min || v > max {
            return Err(Error::Bitstream(what));
        }
        Ok(v)
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
        // codes for 0,1,2,3,4: 1, 010, 011, 00100, 00101
        let bits = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut r = BitReader::new(&bits);
        for want in 0..5 {
            assert_eq!(r.ue().unwrap(), want);
        }
        let bits = [0b0100_1100, 0b1000_0000]; // 010 011 00100
        let mut r = BitReader::new(&bits);
        assert_eq!(r.se().unwrap(), 1);
        assert_eq!(r.se().unwrap(), -1);
        assert_eq!(r.se().unwrap(), 2);
        // 65535: 16 zeros, a one, 16 zero bits
        let bits = [0x00, 0x00, 0x80, 0x00, 0x00];
        let mut r = BitReader::new(&bits);
        assert_eq!(r.ue().unwrap(), 65535);
        assert_eq!(r.bit_pos(), 33);
        // a code running off the end is an error, not a panic
        let mut r = BitReader::new(&[0, 0]);
        assert!(r.ue().is_err());
    }

    #[test]
    fn unescaping() {
        let mut out = Vec::new();
        unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3], &mut out);
        assert_eq!(out, vec![0, 0, 1, 0, 0, 0, 0]);
        unescape(&[0, 0, 4], &mut out);
        assert_eq!(out, vec![0, 0, 4]);
    }
}
