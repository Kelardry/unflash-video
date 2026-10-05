//! Decode the VP8 test vectors (`vp80-00-comprehensive-001.ivf` to `-018`,
//! `vp80-01-intra-*` to `vp80-06-smallsize`, listed in libvpx's
//! `test/test_vectors.cc` and served with their `.md5` files from
//! https://storage.googleapis.com/downloads.webmproject.org/test_data/libvpx/)
//! and compare every picture with the MD5s libvpx made (`<vector>.ivf.md5`),
//! or with ffmpeg's where there is no `.md5` (`<vector>.ivf.framemd5`, from
//! `ffmpeg -c:v vp8 -i <vector>.ivf -fps_mode passthrough -autoscale 0 -f
//! framemd5 <vector>.ivf.framemd5`: without `-autoscale 0` ffmpeg scales
//! the pictures after a size change to the first size before hashing them).
//! ffmpeg's decoder gives libvpx's MD5s on every vector.
//!
//!     cargo run --release -p unflash-vp8 --example conformance -- <dir> [name-filter]

use std::path::{Path, PathBuf};

use unflash_vp8::{Decoder, Error};

fn vectors(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).expect("directory of test vectors").flatten().map(|e| e.path()).filter(|p| p.extension().and_then(|e| e.to_str()) == Some("ivf")).collect();
    v.sort();
    v
}

/// The reference MD5s: libvpx's `.md5` ("md5  name" per frame) or ffmpeg's
/// `.framemd5` (the last comma-separated field).
fn expected(path: &Path) -> Option<(Vec<String>, &'static str)> {
    if let Ok(text) = std::fs::read_to_string(format!("{}.md5", path.display())) {
        return Some((text.lines().filter_map(|l| l.split_whitespace().next()).map(str::to_string).collect(), "libvpx"));
    }
    let text = std::fs::read_to_string(format!("{}.framemd5", path.display())).ok()?;
    Some((text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).map(|l| l.rsplit(',').next().unwrap().trim().to_string()).collect(), "ffmpeg"))
}

/// Decode a whole vector: the MD5 of every shown picture, and how many
/// were damaged.
fn decode(path: &Path) -> Result<(Vec<String>, usize), Error> {
    let data = std::fs::read(path).expect("read");
    let mut dec = Decoder::new(&[])?;
    let (mut md5s, mut damaged) = (Vec::new(), 0);
    for (ts, frame) in unflash_mp4::ivf::frames(&data).unwrap_or_default() {
        for f in dec.decode(frame, ts as f64)? {
            let mut ctx = md5::Context::new();
            ctx.consume(&f.y);
            ctx.consume(&f.u);
            ctx.consume(&f.v);
            md5s.push(format!("{:x}", ctx.compute()));
            damaged += f.damaged as usize;
        }
    }
    Ok((md5s, damaged))
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("directory of test vectors"));
    let filter = std::env::args().nth(2).unwrap_or_default().to_ascii_lowercase();
    let (mut ok, mut bad, mut unsupported, mut errors, mut missing) = (0, 0, 0, 0, 0);
    for path in vectors(&dir) {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !filter.is_empty() && !name.to_ascii_lowercase().contains(&filter) {
            continue;
        }
        let Some((want, source)) = expected(&path) else {
            println!("{name:34} no reference MD5s");
            missing += 1;
            continue;
        };
        let t0 = std::time::Instant::now();
        match decode(&path) {
            Ok((got, damaged)) => {
                let first_bad = got.iter().zip(&want).position(|(g, w)| g != w);
                if got.len() == want.len() && first_bad.is_none() && damaged == 0 {
                    ok += 1;
                    println!("{name:34} ok   {} frames, as {source} ({:.0} ms)", got.len(), t0.elapsed().as_secs_f64() * 1e3);
                } else {
                    bad += 1;
                    let matching = got.iter().zip(&want).filter(|(g, w)| g == w).count();
                    println!("{name:34} MISMATCH {matching} of {} frames match {source} (ours {}, damaged {}){}", want.len(), got.len(), damaged, first_bad.map_or(String::new(), |i| format!(", first wrong frame {i}")));
                }
            }
            Err(Error::Unsupported(s)) => {
                unsupported += 1;
                println!("{name:34} unsupported: {s}");
            }
            Err(Error::Bitstream(s)) => {
                errors += 1;
                println!("{name:34} ERROR: {s}");
            }
        }
    }
    println!("\n{ok} bit-exact, {bad} mismatching, {unsupported} unsupported, {errors} errors, {missing} without reference");
    if bad > 0 || errors > 0 {
        std::process::exit(1);
    }
}
