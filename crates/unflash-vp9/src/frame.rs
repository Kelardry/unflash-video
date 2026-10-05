//! Decoded frames: sample planes of 8-bit (`u8`) or 10/12-bit (`u16`)
//! samples, and the pixel type the prediction and filter code is generic
//! over.

use crate::idct::scalar_transform_add;
use crate::inter::{predict_block, Mc};
use crate::loopfilter::{filter_run_lines, Run};

/// A sample type: `u8` for 8-bit streams, `u16` for 10 and 12-bit ones.
/// The hot kernels are methods, so that each type can have SIMD ones.
pub trait Pixel: Copy + Default + PartialEq + Send + Sync + 'static {
    fn get(self) -> i32;
    /// A value known to be in range.
    fn new(v: i32) -> Self;
    /// Clip1: clamp to 0..(1 << bd) - 1.
    fn clip(v: i32, bd: u32) -> Self;

    /// Loop filter one run of up to 8 lines across an edge.
    fn filter_run(d: &mut [Self], stride: usize, run: &Run, bd: u32) {
        filter_run_lines(d, stride, run, bd);
    }

    /// Interpolate an unscaled inter block from its filter footprint.
    fn predict(src: &[Self], ss: usize, dst: &mut [Self], ds: usize, mc: &Mc, tmp: &mut [i32]) {
        predict_block(src, ss, dst, ds, mc, tmp);
    }

    /// Add the inverse transform of a block (not DC-only) to the prediction.
    fn transform_add(coef: &mut [i32], tx_size: usize, tx_type: u8, rows: usize, dst: &mut [Self], stride: usize, bd: u32) {
        if bd == 8 {
            scalar_transform_add::<i32, Self>(coef, tx_size, tx_type, rows, dst, stride, bd);
        } else {
            scalar_transform_add::<i64, Self>(coef, tx_size, tx_type, rows, dst, stride, bd);
        }
    }
}

impl Pixel for u8 {
    #[inline(always)]
    fn get(self) -> i32 {
        self as i32
    }
    #[inline(always)]
    fn new(v: i32) -> Self {
        v as u8
    }
    #[inline(always)]
    fn clip(v: i32, _bd: u32) -> Self {
        v.clamp(0, 255) as u8
    }

    #[cfg(feature = "simd")]
    fn filter_run(d: &mut [u8], stride: usize, run: &Run, bd: u32) {
        crate::loopfilter::simd::filter_run(d, stride, run, bd);
    }

    #[cfg(feature = "simd")]
    fn predict(src: &[u8], ss: usize, dst: &mut [u8], ds: usize, mc: &Mc, _tmp: &mut [i32]) {
        crate::inter::simd::predict(src, ss, dst, ds, mc);
    }

    #[cfg(feature = "simd")]
    fn transform_add(coef: &mut [i32], tx_size: usize, tx_type: u8, rows: usize, dst: &mut [u8], stride: usize, bd: u32) {
        if crate::idct::simd::NATIVE_MUL {
            crate::idct::simd::transform_add(coef, tx_size, tx_type, rows, dst, stride);
        } else {
            scalar_transform_add::<i32, u8>(coef, tx_size, tx_type, rows, dst, stride, bd);
        }
    }
}

impl Pixel for u16 {
    #[inline(always)]
    fn get(self) -> i32 {
        self as i32
    }
    #[inline(always)]
    fn new(v: i32) -> Self {
        v as u16
    }
    #[inline(always)]
    fn clip(v: i32, bd: u32) -> Self {
        v.clamp(0, (1 << bd) - 1) as u16
    }

    #[cfg(feature = "simd")]
    fn filter_run(d: &mut [u16], stride: usize, run: &Run, bd: u32) {
        crate::loopfilter::simd::filter_run(d, stride, run, bd);
    }

    #[cfg(feature = "simd")]
    fn predict(src: &[u16], ss: usize, dst: &mut [u16], ds: usize, mc: &Mc, _tmp: &mut [i32]) {
        crate::inter::simd::predict(src, ss, dst, ds, mc);
    }
}

/// One plane. The buffer covers whole 64x64 superblocks (blocks at the
/// right and bottom edges are decoded in full); `width` and `height` are
/// the plane's visible size, which is also where references stop:
/// prediction reads past them as the nearest visible sample.
#[derive(Clone)]
pub struct Plane<T> {
    pub data: Vec<T>,
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

/// A decoded frame, 4:2:0.
#[derive(Clone)]
pub struct FrameBuf<T> {
    pub planes: [Plane<T>; 3],
    /// FrameWidth and FrameHeight.
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub color_space: u8,
    pub color_range: bool,
    /// Some of it could not be decoded, or it was predicted from a damaged
    /// frame.
    pub damaged: bool,
}

/// Samples after each plane (and the inter prediction's edge buffer), so
/// that the SIMD kernels can load a whole 16-byte vector at any sample:
/// one instruction everywhere, where assembling a vector from 8 bytes takes
/// many in WebAssembly.
pub const PAD: usize = 16;

impl<T: Pixel> FrameBuf<T> {
    pub fn new(width: u32, height: u32, bit_depth: u8) -> Self {
        let (w, h) = (width as usize, height as usize);
        let aw = (w + 63) & !63;
        let ah = (h + 63) & !63;
        let luma = Plane { data: vec![T::default(); aw * ah + PAD], stride: aw, width: w, height: h };
        let chroma = || Plane { data: vec![T::default(); (aw / 2) * (ah / 2) + PAD], stride: aw / 2, width: w.div_ceil(2), height: h.div_ceil(2) };
        FrameBuf { planes: [luma, chroma(), chroma()], width, height, bit_depth, color_space: 0, color_range: false, damaged: false }
    }

    /// Whether this buffer can hold a frame of this size and depth.
    pub fn fits(&self, width: u32, height: u32, bit_depth: u8) -> bool {
        self.width == width && self.height == height && self.bit_depth == bit_depth
    }
}
