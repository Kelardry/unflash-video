//! Samples in 16-bit SIMD lanes, for the SIMD kernels: the loads, stores
//! and interpolation arithmetic that differ between 8-bit samples and
//! deeper ones.

use wide::{i16x8, u8x16};

use crate::frame::Pixel;

pub trait Lanes: Pixel {
    /// Eight samples. Loads may read up to 16 bytes: slices must extend
    /// that far (see `PAD`).
    fn load(s: &[Self]) -> i16x8;
    /// Sixteen samples, as two vectors.
    fn load2(s: &[Self]) -> [i16x8; 2];
    /// Store the first `N` lanes, which are in range.
    fn store<const N: usize>(v: i16x8, d: &mut [Self]);
    /// Round2(the 8-tap sum, 7) of samples of depth `bd`, before clipping.
    fn taps(s: &[i16x8; 8], f: &[i16x8; 8], bd: u32) -> i16x8;
}

/// 16 bytes (a whole-vector load is one instruction everywhere, where
/// assembling a vector from 8 bytes takes many in WebAssembly).
#[inline(always)]
fn bytes(s: &[u8]) -> u8x16 {
    u8x16::from(<[u8; 16]>::try_from(&s[..16]).unwrap())
}

impl Lanes for u8 {
    #[inline(always)]
    fn load(s: &[u8]) -> i16x8 {
        i16x8::from_u8x16_low(bytes(s))
    }
    #[inline(always)]
    fn load2(s: &[u8]) -> [i16x8; 2] {
        let b = bytes(s);
        [i16x8::from_u8x16_low(b), i16x8::from_u8x16_high(b)]
    }
    #[inline(always)]
    fn store<const N: usize>(v: i16x8, d: &mut [u8]) {
        d[..N].copy_from_slice(&u8x16::narrow_i16x8(v, v).as_array_ref()[..N]);
    }
    /// Over all the filters, an 8-tap sum of 8-bit samples lies in
    /// -13770..=46410: too wide for signed 16-bit lanes, but offset by
    /// 108 * 128 (and the rounding 64) it is exact in wrapping 16-bit
    /// arithmetic read as unsigned, and its logical shift right by 7 is the
    /// rounded value plus 108.
    #[inline(always)]
    fn taps(s: &[i16x8; 8], f: &[i16x8; 8], _bd: u32) -> i16x8 {
        let mut acc = i16x8::splat(108 * 128 + 64);
        for t in 0..8 {
            acc += s[t] * f[t];
        }
        ((acc >> 7_i32) & i16x8::splat(511)) - i16x8::splat(108)
    }
}

impl Lanes for u16 {
    #[inline(always)]
    fn load(s: &[u16]) -> i16x8 {
        i16x8::new(<[u16; 8]>::try_from(&s[..8]).unwrap().map(|v| v as i16))
    }
    #[inline(always)]
    fn load2(s: &[u16]) -> [i16x8; 2] {
        [Self::load(s), Self::load(&s[8..])]
    }
    #[inline(always)]
    fn store<const N: usize>(v: i16x8, d: &mut [u16]) {
        for (d, v) in d[..N].iter_mut().zip(v.to_array()) {
            *d = v as u16;
        }
    }
    /// A sum of 10 or 12-bit samples is too wide for 16 bits, so each
    /// sample is split into its high and low halves of k = bd / 2 bits, the
    /// two sums (at most 63 * 182) taken apart, and recombined:
    /// Round2(2^k A + B, 7) = (A + ((B + 64) >> k)) >> (7 - k).
    #[inline(always)]
    fn taps(s: &[i16x8; 8], f: &[i16x8; 8], bd: u32) -> i16x8 {
        let k = bd / 2;
        let low = i16x8::splat((1 << k) - 1);
        let mut a = i16x8::ZERO;
        let mut b = i16x8::splat(64);
        for t in 0..8 {
            a += (s[t] >> k) * f[t];
            b += (s[t] & low) * f[t];
        }
        (a + (b >> k)) >> (7 - k)
    }
}
