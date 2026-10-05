//! The boolean entropy decoder every VP8 partition is coded with (RFC 6386
//! section 7), and the field and tree reads built on it.

/// Reads the bools of one partition, MSB first. Past the end of the data it
/// reads zeros as the reference decoder does, and keeps count so a frame
/// that runs out of data can be recognised (see [`BoolDecoder::exhausted`]).
#[derive(Clone)]
pub struct BoolDecoder<'a> {
    data: &'a [u8],
    /// The next byte of `data` to load; it keeps counting past the end
    /// while zeros are loaded in place of data.
    pos: usize,
    /// The coded bits not yet decoded, from the most significant bit: the
    /// top eight are compared with the split, `count` more follow them.
    value: u64,
    count: i32,
    /// The width of the current interval, 128..=255 between reads.
    range: u32,
}

impl<'a> BoolDecoder<'a> {
    pub fn new(data: &'a [u8]) -> BoolDecoder<'a> {
        let mut d = BoolDecoder { data, pos: 0, value: 0, count: -8, range: 255 };
        d.fill();
        d
    }

    /// Load whole bytes below the valid bits, as many as fit (seven, or
    /// eight on the first fill).
    #[inline(never)]
    fn fill(&mut self) {
        let mut shift = 48 - self.count;
        if self.pos + 8 <= self.data.len() {
            let mut chunk = [0u8; 8];
            chunk.copy_from_slice(&self.data[self.pos..self.pos + 8]);
            let n = (shift / 8 + 1) as usize;
            let v = u64::from_be_bytes(chunk) & (!0u64 << (64 - 8 * n));
            self.value |= v >> (56 - shift);
            self.pos += n;
            self.count += 8 * n as i32;
            return;
        }
        while shift >= 0 {
            let byte = self.data.get(self.pos).copied().unwrap_or(0);
            self.value |= (byte as u64) << shift;
            self.pos += 1;
            self.count += 8;
            shift -= 8;
        }
    }

    /// One bool whose probability of being 0 is `prob` / 256.
    #[inline(always)]
    pub fn read(&mut self, prob: u8) -> bool {
        if self.count < 0 {
            self.fill();
        }
        let split = 1 + (((self.range - 1) * prob as u32) >> 8);
        let big_split = (split as u64) << 56;
        let bit = self.value >= big_split;
        if bit {
            self.range -= split;
            self.value -= big_split;
        } else {
            self.range = split;
        }
        // back to 128..=255: the range is at least 1 here
        let shift = self.range.leading_zeros() - 24;
        self.range <<= shift;
        self.value <<= shift;
        self.count -= shift as i32;
        bit
    }

    /// An even-odds bool: a flag or one bit of a literal.
    #[inline]
    pub fn read_flag(&mut self) -> bool {
        self.read(128)
    }

    /// An `n`-bit unsigned literal, MSB first (`L(n)` in the RFC).
    pub fn read_literal(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.read_flag() as u32;
        }
        v
    }

    /// A flag, then if it is set an `n`-bit magnitude and a sign; 0 when the
    /// flag is clear. The frame header's optional signed fields.
    pub fn read_optional_signed(&mut self, n: u32) -> i32 {
        if !self.read_flag() {
            return 0;
        }
        let v = self.read_literal(n) as i32;
        if self.read_flag() {
            -v
        } else {
            v
        }
    }

    /// A value coded with a tree (section 8.1): `tree[i]` and `tree[i + 1]`
    /// are the two branches of node `i / 2`, read with `probs[i / 2]`; a
    /// branch is a leaf holding `-value` when it is not positive.
    #[inline]
    pub fn read_tree(&mut self, tree: &[i8], probs: &[u8]) -> u8 {
        let mut i = 0usize;
        loop {
            let next = tree[i + self.read(probs[i >> 1]) as usize];
            if next <= 0 {
                return (-next) as u8;
            }
            i = next as usize;
        }
    }

    /// Whether the decoder has read well past the end of its data (two
    /// bytes of slack for encoders that flush tightly): the data was cut
    /// short or is corrupt.
    pub fn exhausted(&self) -> bool {
        let consumed = 8 * self.pos as i64 - 8 - self.count as i64;
        consumed > 8 * (self.data.len() as i64 + 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boolean encoder of RFC 6386 section 7.3, to round-trip through
    /// the decoder.
    struct Encoder {
        out: Vec<u8>,
        range: u32,
        bottom: u32,
        bit_count: i32,
    }

    impl Encoder {
        fn new() -> Self {
            Encoder { out: Vec::new(), range: 255, bottom: 0, bit_count: 24 }
        }

        fn add_one_to_output(&mut self) {
            let mut i = self.out.len();
            while i > 0 {
                i -= 1;
                if self.out[i] == 255 {
                    self.out[i] = 0;
                } else {
                    self.out[i] += 1;
                    break;
                }
            }
        }

        fn write(&mut self, prob: u8, bit: bool) {
            let split = 1 + (((self.range - 1) * prob as u32) >> 8);
            if bit {
                self.bottom = self.bottom.wrapping_add(split);
                self.range -= split;
            } else {
                self.range = split;
            }
            while self.range < 128 {
                self.range <<= 1;
                if self.bottom & (1 << 31) != 0 {
                    self.add_one_to_output();
                }
                self.bottom <<= 1;
                self.bit_count -= 1;
                if self.bit_count == 0 {
                    self.out.push((self.bottom >> 24) as u8);
                    self.bottom &= (1 << 24) - 1;
                    self.bit_count = 8;
                }
            }
        }

        fn finish(mut self) -> Vec<u8> {
            for _ in 0..32 {
                self.write(128, false);
            }
            self.out
        }
    }

    fn noise(n: usize, seed: &mut u32) -> Vec<u32> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *seed >> 8
            })
            .collect()
    }

    #[test]
    fn round_trip() {
        let mut seed = 1;
        let r = noise(20000, &mut seed);
        let probs: Vec<u8> = r.iter().map(|v| (v & 0xff).max(1) as u8).collect();
        let bits: Vec<bool> = r.iter().zip(&probs).map(|(v, &p)| ((v >> 8) & 0xff) >= p as u32).collect();
        let mut e = Encoder::new();
        for (&b, &p) in bits.iter().zip(&probs) {
            e.write(p, b);
        }
        let data = e.finish();
        let mut d = BoolDecoder::new(&data);
        for (i, (&b, &p)) in bits.iter().zip(&probs).enumerate() {
            assert_eq!(d.read(p), b, "bool {i}");
        }
        assert!(!d.exhausted());
    }

    #[test]
    fn fields_and_trees() {
        let mut e = Encoder::new();
        for bit in [true, false, true, true] {
            e.write(128, bit);
        }
        // optional signed: flag, magnitude 5 in 4 bits, sign
        for bit in [true, false, true, false, true, true] {
            e.write(128, bit);
        }
        // the tree 0 -> {leaf 0, node 2}, node 2 -> {leaf 1, leaf 2}: code '11'
        e.write(200, true);
        e.write(100, true);
        let data = e.finish();
        let mut d = BoolDecoder::new(&data);
        assert_eq!(d.read_literal(4), 0b1011);
        assert_eq!(d.read_optional_signed(4), -5);
        assert_eq!(d.read_tree(&[0, 2, -1, -2], &[200, 100]), 2);
    }

    #[test]
    fn runs_out_quietly() {
        let data = [0x12, 0x34];
        let mut d = BoolDecoder::new(&data);
        for _ in 0..200 {
            d.read(128);
        }
        assert!(d.exhausted());
        let mut d = BoolDecoder::new(&[]);
        assert!(!d.read(1));
    }
}
