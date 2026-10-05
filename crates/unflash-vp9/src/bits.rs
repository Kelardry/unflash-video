//! Reading the uncompressed header: fixed-width fields, most significant
//! bit first (9.1).

use crate::{Error, Result};

pub struct BitReader<'a> {
    data: &'a [u8],
    /// bit position
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// f(n), n <= 32.
    pub fn f(&mut self, n: u32) -> Result<u32> {
        if self.pos + n as usize > self.data.len() * 8 {
            return Err(Error::Bitstream("frame header runs past the end of the frame"));
        }
        let mut v = 0u32;
        for _ in 0..n {
            let bit = (self.data[self.pos >> 3] >> (7 - (self.pos & 7))) & 1;
            v = (v << 1) | bit as u32;
            self.pos += 1;
        }
        Ok(v)
    }

    pub fn bit(&mut self) -> Result<bool> {
        Ok(self.f(1)? != 0)
    }

    /// s(n): a magnitude of n bits, then a sign bit.
    pub fn s(&mut self, n: u32) -> Result<i32> {
        let v = self.f(n)? as i32;
        Ok(if self.bit()? { -v } else { v })
    }

    /// The header's length in bytes, its trailing bits included.
    pub fn bytes_read(&self) -> usize {
        self.pos.div_ceil(8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields() {
        let mut r = BitReader::new(&[0b1010_0111, 0b1000_0000]);
        assert_eq!(r.f(2).unwrap(), 2);
        assert_eq!(r.f(3).unwrap(), 0b100);
        assert_eq!(r.s(2).unwrap(), -3);
        assert_eq!(r.bytes_read(), 1);
        assert!(r.bit().unwrap());
        assert_eq!(r.bytes_read(), 2);
        assert!(r.f(8).is_err());
    }
}
