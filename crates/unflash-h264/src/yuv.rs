//! Decoded pictures copied out as packed planes.

use crate::picture::Picture;

/// The cropped planes as packed I420 (Y, then Cb, then Cr), as ffmpeg's
/// rawvideo output lays them out.
pub fn to_i420(pic: &Picture, crop: (usize, usize, usize, usize), out: &mut Vec<u8>) {
    let (cx, cy, w, h) = crop;
    out.clear();
    let lw = pic.width;
    for y in 0..h {
        out.extend_from_slice(&pic.y[(cy + y) * lw + cx..(cy + y) * lw + cx + w]);
    }
    let cw = pic.width / 2;
    let (ccx, ccy, cwid, chei) = (cx / 2, cy / 2, w.div_ceil(2), h.div_ceil(2));
    for plane in [&pic.u, &pic.v] {
        for y in 0..chei {
            out.extend_from_slice(&plane[(ccy + y) * cw + ccx..(ccy + y) * cw + ccx + cwid]);
        }
    }
}
