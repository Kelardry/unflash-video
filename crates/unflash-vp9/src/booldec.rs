//! The boolean (arithmetic) decoder of the compressed header and the tiles
//! (9.2).
//!
//! The coded bits are kept in a 64-bit window, most significant bit first,
//! and refilled eight bytes at a time, so a decision costs a multiply, a
//! compare and a shift. Past the end of the data the window fills with
//! zeros (the decoding stays defined, as the specification's does) and the
//! decoder remembers that it ran out, which marks the frame damaged.

use crate::{Error, Result};

/// Added to the bit count once the data is exhausted, so the window is
/// never refilled again (the missing bits read as zeros).
const LOTS_OF_BITS: i32 = 0x4000;

pub struct BoolDecoder<'a> {
    data: &'a [u8],
    pos: usize,
    /// The window: the top 8 bits are compared with the split, the bits
    /// below are buffered.
    value: u64,
    /// Buffered bits below the top 8 (negative: the window needs a refill).
    count: i32,
    range: u32,
}

impl<'a> BoolDecoder<'a> {
    /// init_bool: start decoding `data`, whose first decision is a marker
    /// that must be 0.
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.is_empty() {
            return Err(Error::Bitstream("empty arithmetic-coded partition"));
        }
        let mut d = BoolDecoder { data, pos: 0, value: 0, count: -8, range: 255 };
        d.fill();
        if d.read(128) {
            return Err(Error::Bitstream("arithmetic-coded partition does not start with a zero marker"));
        }
        Ok(d)
    }

    #[inline(always)]
    fn fill(&mut self) {
        // the next byte's least significant bit goes to bit `shift`
        let shift = 48 - self.count;
        if let Some(bytes) = self.data.get(self.pos..self.pos + 8) {
            let be = u64::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]]);
            let n = (shift >> 3) + 1;
            self.value |= (be >> (64 - 8 * n)) << (shift & 7);
            self.count += 8 * n;
            self.pos += n as usize;
        } else {
            self.fill_slow(shift);
        }
    }

    #[cold]
    fn fill_slow(&mut self, mut shift: i32) {
        while shift >= 0 {
            let Some(&b) = self.data.get(self.pos) else {
                self.count += LOTS_OF_BITS;
                return;
            };
            self.value |= (b as u64) << shift;
            self.pos += 1;
            self.count += 8;
            shift -= 8;
        }
    }

    /// read_bool(p): a decision that is 0 with probability p / 256.
    #[inline(always)]
    pub fn read(&mut self, prob: u8) -> bool {
        let split = (self.range * prob as u32 + (256 - prob as u32)) >> 8;
        if self.count < 0 {
            self.fill();
        }
        let bigsplit = (split as u64) << 56;
        let bit = self.value >= bigsplit;
        if bit {
            self.range -= split;
            self.value -= bigsplit;
        } else {
            self.range = split;
        }
        // renormalise: the range is 1..=255, bring it back to 128..=255
        let shift = self.range.leading_zeros() - 24;
        self.range <<= shift;
        self.value <<= shift;
        self.count -= shift as i32;
        bit
    }

    /// An equiprobable decision.
    #[inline(always)]
    pub fn read_bit(&mut self) -> bool {
        self.read(128)
    }

    /// read_literal(n): n equiprobable bits, most significant first.
    pub fn read_literal(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.read_bit() as u32;
        }
        v
    }

    /// Decode a tree-coded value: `tree` holds pairs of children, a child
    /// `<= 0` being the leaf with value `-child`; node `i` uses `probs[i >> 1]`.
    #[inline(always)]
    pub fn read_tree(&mut self, tree: &[i8], probs: &[u8]) -> usize {
        let mut n = 0usize;
        loop {
            let bit = self.read(probs[n >> 1]) as usize;
            let next = tree[n + bit];
            if next <= 0 {
                return (-next) as usize;
            }
            n = next as usize;
        }
    }

    /// Whether decoding has consumed bits past the end of the data (the
    /// encoder pads its partitions so that a valid one never does).
    pub fn overran(&self) -> bool {
        self.count > 64 && self.count < LOTS_OF_BITS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A straightforward encoder (the inverse of 9.2) to produce test data.
    struct Encoder {
        low: u64,
        range: u32,
        count: i32,
        out: Vec<u8>,
    }

    impl Encoder {
        fn new() -> Self {
            Encoder { low: 0, range: 255, count: -24, out: Vec::new() }
        }
        fn write(&mut self, bit: bool, prob: u8) {
            let split = 1 + (((self.range - 1) * prob as u32) >> 8);
            let (mut low, mut range) = (self.low, if bit { self.range - split } else { split });
            if bit {
                low += split as u64;
            }
            let mut shift = range.leading_zeros() as i32 - 24;
            range <<= shift;
            self.count += shift;
            if self.count >= 0 {
                let offset = shift - self.count;
                if (low << (offset - 1)) & 0x8000_0000 != 0 {
                    let mut x = self.out.len();
                    while x > 0 && self.out[x - 1] == 0xff {
                        self.out[x - 1] = 0;
                        x -= 1;
                    }
                    self.out[x - 1] += 1;
                }
                self.out.push((low >> (24 - offset)) as u8);
                low <<= offset;
                shift = self.count;
                low &= 0xff_ffff;
                self.count -= 8;
            }
            low <<= shift;
            self.low = low;
            self.range = range;
        }
        fn finish(mut self) -> Vec<u8> {
            for _ in 0..32 {
                self.write(false, 128);
            }
            self.out
        }
    }

    #[test]
    fn round_trip() {
        let mut e = Encoder::new();
        e.write(false, 128);
        let mut bits = Vec::new();
        let mut x: u32 = 12345;
        for _ in 0..5000 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            let prob = ((x >> 16) % 255 + 1) as u8;
            let bit = (x >> 8) % 256 >= prob as u32;
            bits.push((bit, prob));
            e.write(bit, prob);
        }
        let data = e.finish();
        let mut d = BoolDecoder::new(&data).unwrap();
        for (i, &(bit, prob)) in bits.iter().enumerate() {
            assert_eq!(d.read(prob), bit, "decision {i}");
        }
        assert!(!d.overran());
        // reading on past the end is defined and flagged
        for _ in 0..10_000 {
            d.read(128);
        }
        assert!(d.overran());
    }

    #[test]
    fn marker_and_empty() {
        assert!(BoolDecoder::new(&[]).is_err());
        assert!(BoolDecoder::new(&[0xff]).is_err());
        let mut d = BoolDecoder::new(&[0x00]).unwrap();
        assert_eq!(d.read_literal(4), 0);
    }
}
