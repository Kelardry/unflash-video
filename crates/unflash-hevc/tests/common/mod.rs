//! Shared by the tests and `examples/conformance.rs`: the MD5 of a picture,
//! and the MD5s of ffmpeg's framemd5 output.

use unflash_hevc::Frame;

/// The MD5 of a picture as ffmpeg's framemd5 hashes it: the 8-bit planes,
/// or the 16-bit little-endian ones above 8 bits.
pub fn md5(f: &Frame) -> String {
    let mut ctx = md5::Context::new();
    match (&f.y16, &f.u16, &f.v16) {
        (Some(y), Some(u), Some(v)) => {
            for p in [y, u, v] {
                let bytes: Vec<u8> = p.iter().flat_map(|s| s.to_le_bytes()).collect();
                ctx.consume(&bytes);
            }
        }
        _ => {
            ctx.consume(&f.y);
            ctx.consume(&f.u);
            ctx.consume(&f.v);
        }
    }
    format!("{:x}", ctx.compute())
}

/// The per-picture MD5s of a framemd5 file (the last field of each line).
pub fn framemd5(text: &str) -> Vec<String> {
    text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).map(|l| l.rsplit(',').next().unwrap_or("").trim().to_string()).collect()
}
