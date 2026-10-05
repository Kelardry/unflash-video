//! A most-significant-bit-first reader over one frame (an AC-3 sync frame,
//! a DTS core frame).
//!
//! Reads past the end return zeros and are remembered: the frame parsers
//! check `overrun()` at their checkpoints and give up on the frame, which
//! is how a frame that claims more data than it has is caught (for AC-3,
//! A/52 §7.10.2, condition 21).

pub struct Bits<'a> {
    data: &'a [u8],
    /// Position in bits from the start of `data`.
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Bits<'a> {
        Bits { data, pos: 0 }
    }

    /// Read `n` bits (0 to 32) as an unsigned number.
    #[inline]
    pub fn read(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte = self.pos >> 3;
        let shift = (self.pos & 7) as u32;
        // eight bytes from `byte`, zero past the end of the data
        let word = if byte + 8 <= self.data.len() {
            u64::from_be_bytes(self.data[byte..byte + 8].try_into().unwrap())
        } else {
            let mut b = [0u8; 8];
            if byte < self.data.len() {
                let avail = self.data.len() - byte;
                b[..avail].copy_from_slice(&self.data[byte..]);
            }
            u64::from_be_bytes(b)
        };
        self.pos = self.pos.saturating_add(n as usize);
        ((word << shift) >> (64 - n)) as u32
    }

    /// Read one bit as a flag.
    #[inline]
    pub fn flag(&mut self) -> bool {
        self.read(1) != 0
    }

    /// Read `n` bits (1 to 32) as a two's complement number.
    #[inline]
    pub fn read_signed(&mut self, n: u32) -> i32 {
        let v = self.read(n);
        ((v << (32 - n)) as i32) >> (32 - n)
    }

    pub fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn seek(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// Skip to the next multiple of `n` bits from the start of the data.
    pub fn align(&mut self, n: usize) {
        self.pos = self.pos.div_ceil(n).saturating_mul(n);
    }

    /// Whether a read went past the end of the data.
    pub fn overrun(&self) -> bool {
        self.pos > self.data.len() * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_across_bytes_and_past_the_end() {
        let data = [0b1010_1100, 0b0101_0011, 0xff];
        let mut b = Bits::new(&data);
        assert_eq!(b.read(3), 0b101);
        assert_eq!(b.read(7), 0b0110001);
        assert_eq!(b.read_signed(4), 0b0100);
        assert_eq!(b.read_signed(3), -1);
        assert!(!b.overrun());
        assert_eq!(b.read(7), 0b111_1111);
        assert!(!b.overrun());
        assert_eq!(b.position(), 24);
        assert_eq!(b.read(16), 0);
        assert!(b.overrun());
        let mut b = Bits::new(&[0x80, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(b.read(32), 0x8000_0000);
        b.skip(38);
        assert_eq!(b.read(2), 0b01);
        assert!(!b.overrun());
        b.seek(3);
        b.align(32);
        assert_eq!(b.position(), 32);
        b.align(32);
        assert_eq!(b.position(), 32);
    }
}
