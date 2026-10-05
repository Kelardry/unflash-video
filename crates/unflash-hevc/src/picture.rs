//! Decoded pictures: sample planes (8-bit streams in `u8`, deeper ones in
//! `u16`) and the motion data a later picture's temporal prediction reads.

#[cfg(feature = "simd")]
use wide::{i16x8, u8x16};

/// A sample type: `u8` for 8-bit streams, `u16` for 9 to 12 bits. The
/// reconstruction is generic over it, so 8-bit pictures take half the
/// memory and bandwidth.
pub trait Sample: Copy + Default + PartialEq + Into<i32> + std::fmt::Debug + 'static {
    /// The largest bit depth the type holds.
    const MAX_BITS: u32;
    fn get(self) -> i32;
    /// From a value already in the range of the bit depth.
    fn new(v: i32) -> Self;
    /// The first eight samples of `s` as 16-bit lanes.
    #[cfg(feature = "simd")]
    fn load8(s: &[Self]) -> i16x8;
    /// Eight lanes into `d[..8]`, clamped to 0..=`max`.
    #[cfg(feature = "simd")]
    fn store8(v: i16x8, max: i16, d: &mut [Self]);
}

impl Sample for u8 {
    const MAX_BITS: u32 = 8;
    #[inline(always)]
    fn get(self) -> i32 {
        self as i32
    }
    #[inline(always)]
    fn new(v: i32) -> Self {
        v as u8
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn load8(s: &[u8]) -> i16x8 {
        let mut a = [0u8; 16];
        a[..8].copy_from_slice(&s[..8]);
        i16x8::from_u8x16_low(u8x16::from(a))
    }
    /// (8-bit pictures: the saturating narrowing is the clamp.)
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn store8(v: i16x8, _max: i16, d: &mut [u8]) {
        let p = u8x16::narrow_i16x8(v, v);
        d[..8].copy_from_slice(&p.as_array_ref()[..8]);
    }
}

impl Sample for u16 {
    const MAX_BITS: u32 = 16;
    #[inline(always)]
    fn get(self) -> i32 {
        self as i32
    }
    #[inline(always)]
    fn new(v: i32) -> Self {
        v as u16
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn load8(s: &[u16]) -> i16x8 {
        let s = &s[..8];
        i16x8::new(std::array::from_fn(|i| s[i] as i16))
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn store8(v: i16x8, max: i16, d: &mut [u16]) {
        let v = v.max(i16x8::ZERO).min(i16x8::splat(max));
        for (o, &x) in d[..8].iter_mut().zip(v.as_array_ref()) {
            *o = x as u16;
        }
    }
}

/// One colour component of a picture.
#[derive(Clone)]
pub struct Plane<P> {
    pub data: Vec<P>,
    /// Samples per row (the plane's width).
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

impl<P: Sample> Plane<P> {
    pub fn new(width: usize, height: usize, fill: P) -> Plane<P> {
        Plane { data: vec![fill; width * height], stride: width, width, height }
    }
}

/// The motion of one 16x16 block of a decoded picture as temporal motion
/// vector prediction sees it (8.5.3.2.8 reads the motion field at a 16x16
/// granularity): per list the vector, the order count of the picture it
/// points at and whether that picture was a long-term reference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColMv {
    pub mv: [[i16; 2]; 2],
    pub poc: [i32; 2],
    /// Bit 0 / 1: list 0 / 1 used; bit 2 / 3: its reference was long-term.
    pub flags: u8,
}

pub const COL_L0: u8 = 1;
pub const COL_L1: u8 = 2;
pub const COL_LT0: u8 = 4;
pub const COL_LT1: u8 = 8;

/// A decoded (or generated) picture.
pub struct Picture<P> {
    /// Unique within a decoder instance.
    pub id: u32,
    pub planes: [Plane<P>; 3],
    pub poc: i32,
    /// Per 16x16 block, in raster order (`width.div_ceil(16)` per row).
    pub col: Vec<ColMv>,
    /// Stands in for a reference picture the stream did not provide.
    pub generated: bool,
}

impl<P: Sample> Picture<P> {
    /// A picture of the given luma size; `chroma` is false for 4:0:0.
    pub fn new(id: u32, width: usize, height: usize, chroma: bool, fill: P) -> Picture<P> {
        let (cw, ch) = if chroma { (width.div_ceil(2), height.div_ceil(2)) } else { (0, 0) };
        Picture {
            id,
            planes: [Plane::new(width, height, fill), Plane::new(cw, ch, fill), Plane::new(cw, ch, fill)],
            poc: 0,
            col: vec![ColMv::default(); width.div_ceil(16) * height.div_ceil(16)],
            generated: false,
        }
    }

    pub fn width(&self) -> usize {
        self.planes[0].width
    }
    pub fn height(&self) -> usize {
        self.planes[0].height
    }

    /// Whether this buffer can hold a picture of the given format.
    pub fn fits(&self, width: usize, height: usize, chroma: bool) -> bool {
        self.width() == width && self.height() == height && (self.planes[1].width != 0) == chroma
    }
}
