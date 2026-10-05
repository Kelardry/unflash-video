//! What the two comparison examples share.

use std::process::Command;

/// ffmpeg's pictures of the stream at `path` as packed I420, in
/// presentation order (`args` go before its input); ends the example when
/// ffmpeg fails.
pub fn ffmpeg_i420(args: &[&str], path: &str) -> Vec<u8> {
    let out = Command::new("ffmpeg").args(["-v", "error"]).args(args).args(["-i", path, "-f", "rawvideo", "-pix_fmt", "yuv420p", "-"]).output().expect("ffmpeg");
    if !out.status.success() {
        println!("ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr));
        std::process::exit(1);
    }
    out.stdout
}

/// The plane of sample `i` of a packed I420 picture of `w` x `h`, and its
/// position in luma samples.
pub fn locate(i: usize, w: usize, h: usize) -> (&'static str, usize, usize) {
    if i < w * h {
        return ("Y", i % w, i / w);
    }
    let ci = i - w * h;
    let cw = w.div_ceil(2);
    let csz = cw * h.div_ceil(2);
    if ci < csz {
        ("U", (ci % cw) * 2, (ci / cw) * 2)
    } else {
        ("V", ((ci - csz) % cw) * 2, ((ci - csz) / cw) * 2)
    }
}
