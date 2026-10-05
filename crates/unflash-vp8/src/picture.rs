//! Decoded pictures: three planes at the macroblock-aligned size (the
//! samples beyond the display size are decoded and predicted from like the
//! rest).

use crate::{Error, Result};

#[derive(Default)]
pub struct Picture {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    /// Luma width and height, whole macroblocks; chroma is half of each.
    pub width: usize,
    pub height: usize,
}

/// A zeroed buffer, or an error when memory runs out: a corrupt key frame
/// can ask for a 16383x16383 picture, and failing to allocate must not
/// abort the WebAssembly instance.
pub fn try_alloc<T: Clone + Default>(n: usize) -> Result<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| Error::Unsupported("picture too large for the memory available"))?;
    v.resize(n, T::default());
    Ok(v)
}

impl Picture {
    pub fn new(mb_w: usize, mb_h: usize) -> Result<Picture> {
        let (width, height) = (mb_w * 16, mb_h * 16);
        let luma = width * height;
        Ok(Picture { y: try_alloc(luma)?, u: try_alloc(luma / 4)?, v: try_alloc(luma / 4)?, width, height })
    }
}
