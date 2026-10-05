//! 8-bit 4:2:0 YCbCr pictures in the layouts WebCodecs' `VideoFrame.copyTo`
//! and the built-in decoder deliver (I420: three planes; NV12: a luma plane
//! and an interleaved CbCr plane), and how their codes map to RGB: in 16.16
//! fixed point where [`crate::resample::Shrink`] converts them, in f32 with
//! the same coefficients in the GPU stage's shader (`shaders/yuv.wgsl`).

/// Where the planes of a 4:2:0 picture sit in one buffer, and how its codes
/// map to RGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct YuvLayout {
    /// NV12: one interleaved CbCr plane at `u_off` / `u_stride`; otherwise
    /// I420 with separate Cb and Cr planes.
    pub nv12: bool,
    pub y_off: usize,
    pub y_stride: usize,
    pub u_off: usize,
    pub u_stride: usize,
    pub v_off: usize,
    pub v_stride: usize,
    /// BT.709 matrix (else BT.601).
    pub bt709: bool,
    /// Full-range codes (else limited / video range).
    pub full_range: bool,
}

impl YuvLayout {
    /// From the words the web app passes: [format (0 I420, 1 NV12), y_off,
    /// y_stride, u_off, u_stride, v_off, v_stride, matrix (0 BT.601, 1
    /// BT.709), full_range].
    pub fn from_words(w: &[u32]) -> Option<YuvLayout> {
        if w.len() < 9 {
            return None;
        }
        Some(YuvLayout {
            nv12: w[0] == 1,
            y_off: w[1] as usize,
            y_stride: w[2] as usize,
            u_off: w[3] as usize,
            u_stride: w[4] as usize,
            v_off: w[5] as usize,
            v_stride: w[6] as usize,
            bt709: w[7] == 1,
            full_range: w[8] != 0,
        })
    }

    /// Packed I420 (Y, then Cb, then Cr, each tightly packed), as the
    /// built-in decoder writes it.
    pub fn packed_i420(width: usize, height: usize, bt709: bool, full_range: bool) -> YuvLayout {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        YuvLayout { nv12: false, y_off: 0, y_stride: width, u_off: width * height, u_stride: cw, v_off: width * height + cw * ch, v_stride: cw, bt709, full_range }
    }

    /// Whether `len` bytes hold a width × height picture in this layout.
    pub fn fits(&self, len: usize, width: usize, height: usize) -> bool {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let plane = |off: usize, stride: usize, w: usize, h: usize| h == 0 || (stride >= w && off + (h - 1) * stride + w <= len);
        plane(self.y_off, self.y_stride, width, height)
            && if self.nv12 { plane(self.u_off, self.u_stride, 2 * cw, ch) } else { plane(self.u_off, self.u_stride, cw, ch) && plane(self.v_off, self.v_stride, cw, ch) }
    }

    /// The conversion coefficients (ky, kr, kgu, kgv, kb) in 16.16 fixed
    /// point and the luma offset:
    /// R = ky·(Y − yoff) + kr·(V − 128), G = ky·(Y − yoff) − kgu·(U − 128) −
    /// kgv·(V − 128), B = ky·(Y − yoff) + kb·(U − 128).
    pub fn coefficients(&self) -> [i32; 6] {
        match (self.bt709, self.full_range) {
            (true, false) => [76309, 117489, 13975, 34925, 138438, 16],
            (false, false) => [76309, 104597, 25675, 53279, 132201, 16],
            (true, true) => [65536, 103206, 12276, 30679, 121608, 0],
            (false, true) => [65536, 91881, 22553, 46802, 116129, 0],
        }
    }
}

/// Convert a picture to RGBA8 (alpha 255); each chroma sample covers its
/// 2×2 luma block. `out` is resized to width × height × 4. (The plain
/// statement of the conversion the shrink's faster one is tested against.)
#[cfg(test)]
pub(crate) fn to_rgba(data: &[u8], width: usize, height: usize, l: &YuvLayout, out: &mut Vec<u8>) {
    assert!(l.fits(data.len(), width, height), "picture data too short for its layout");
    let [ky, kr, kgu, kgv, kb, yoff] = l.coefficients();
    out.clear();
    out.resize(width * height * 4, 255);
    let cw = width.div_ceil(2);
    let px = |o: &mut [u8], y: u8, u: i32, v: i32| {
        let yy = (y as i32 - yoff) * ky;
        let r = (yy + kr * v + 32768) >> 16;
        let g = (yy - kgu * u - kgv * v + 32768) >> 16;
        let b = (yy + kb * u + 32768) >> 16;
        o[0] = r.clamp(0, 255) as u8;
        o[1] = g.clamp(0, 255) as u8;
        o[2] = b.clamp(0, 255) as u8;
    };
    for y in 0..height {
        let yrow = &data[l.y_off + y * l.y_stride..][..width];
        let cy = y / 2;
        let orow = &mut out[y * width * 4..(y + 1) * width * 4];
        if l.nv12 {
            let crow = &data[l.u_off + cy * l.u_stride..][..2 * cw];
            for x in 0..width {
                let c = &crow[(x / 2) * 2..];
                px(&mut orow[x * 4..], yrow[x], c[0] as i32 - 128, c[1] as i32 - 128);
            }
        } else {
            let urow = &data[l.u_off + cy * l.u_stride..][..cw];
            let vrow = &data[l.v_off + cy * l.v_stride..][..cw];
            for x in 0..width {
                px(&mut orow[x * 4..], yrow[x], urow[x / 2] as i32 - 128, vrow[x / 2] as i32 - 128);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grey_and_primaries_convert_as_expected() {
        // 2x2 I420: Y 16 / 235 (black / white, limited), grey chroma
        let data = [16u8, 235, 16, 235, 128, 128];
        let l = YuvLayout::packed_i420(2, 2, true, false);
        assert!(l.fits(data.len(), 2, 2));
        let mut out = Vec::new();
        to_rgba(&data, 2, 2, &l, &mut out);
        assert_eq!(&out[0..4], &[0, 0, 0, 255]);
        assert_eq!(&out[4..8], &[255, 255, 255, 255]);
        // full range: codes pass through
        let l = YuvLayout::packed_i420(2, 2, true, true);
        to_rgba(&[100, 100, 100, 100, 128, 128], 2, 2, &l, &mut out);
        assert_eq!(&out[0..3], &[100, 100, 100]);
        // pure red in BT.601 limited: Y 81, Cb 90, Cr 240
        let l = YuvLayout::packed_i420(2, 2, false, false);
        to_rgba(&[81, 81, 81, 81, 90, 240], 2, 2, &l, &mut out);
        assert!(out[0] >= 250 && out[1] <= 5 && out[2] <= 5, "{:?}", &out[0..3]);
    }

    #[test]
    fn nv12_and_strided_layouts() {
        // 2x2, luma stride 4, interleaved chroma at offset 8 with stride 4
        let data = [16u8, 235, 0, 0, 16, 235, 0, 0, 128, 128, 0, 0];
        let l = YuvLayout { nv12: true, y_off: 0, y_stride: 4, u_off: 8, u_stride: 4, v_off: 0, v_stride: 0, bt709: true, full_range: false };
        assert!(l.fits(data.len(), 2, 2));
        assert!(!l.fits(9, 2, 2));
        let mut out = Vec::new();
        to_rgba(&data, 2, 2, &l, &mut out);
        assert_eq!(&out[0..3], &[0, 0, 0]);
        assert_eq!(&out[4..7], &[255, 255, 255]);
        assert_eq!(YuvLayout::from_words(&[1, 0, 4, 8, 4, 0, 0, 1, 0]), Some(l));
        assert_eq!(YuvLayout::from_words(&[1, 0, 4]), None);
    }
}
