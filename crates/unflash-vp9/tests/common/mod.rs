//! Shared by the tests and `examples/conformance.rs`: the MD5 of a frame.

use unflash_vp9::Frame;

/// The MD5 of a frame as ffmpeg's framemd5 computes it: the planes packed
/// one after the other, 16-bit samples little-endian.
pub fn frame_md5(f: &Frame) -> String {
    let bytes = match (&f.y16, &f.u16, &f.v16) {
        (Some(y), Some(u), Some(v)) => y.iter().chain(u).chain(v).flat_map(|s| s.to_le_bytes()).collect(),
        _ => [&f.y[..], &f.u[..], &f.v[..]].concat(),
    };
    format!("{:x}", md5::compute(bytes))
}
