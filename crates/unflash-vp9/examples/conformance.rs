//! Decode the libvpx VP9 test vectors (vp90-2-*.webm / .ivf with their
//! `.md5` files, from
//! https://storage.googleapis.com/downloads.webmproject.org/test_data/libvpx/)
//! and compare every frame's MD5 with libvpx's. The `invalid-*` files are
//! corrupt on purpose: they only have to decode without a panic.
//!
//!     cargo run --release -p unflash-vp9 --example conformance -- <dir> [name-filter]

use std::path::{Path, PathBuf};

use unflash_vp9::{Decoder, Error};

#[path = "../tests/common/mod.rs"]
mod common;
use common::frame_md5;

fn vectors(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("directory of test vectors")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            (name.starts_with("vp9") || name.starts_with("invalid-vp9")) && (name.ends_with(".webm") || name.ends_with(".ivf"))
        })
        .collect();
    v.sort();
    v
}

/// The samples of an IVF file, or of a WebM file through the demuxer.
fn samples(data: &[u8]) -> Option<Vec<&[u8]>> {
    if let Some(frames) = unflash_mp4::ivf::frames(data) {
        return Some(frames.into_iter().map(|(_, f)| f).collect());
    }
    let movie = unflash_mp4::demux::parse_bytes(data).ok()?;
    let track = movie.video()?;
    Some(track.samples.iter().map(|s| &data[s.offset as usize..(s.offset as usize + s.size as usize).min(data.len())]).collect())
}

struct Decoded {
    /// The MD5s of the frames each sample showed.
    per_sample: Vec<Vec<String>>,
    damaged: usize,
    error: Option<Error>,
}

fn decode(data: &[u8], keep_going: bool) -> Option<Decoded> {
    let mut dec = match Decoder::new(&[]) {
        Ok(d) => d,
        Err(e) => return Some(Decoded { per_sample: Vec::new(), damaged: 0, error: Some(e) }),
    };
    // (libvpx's MD5s of a deeper vector are of its 16-bit samples)
    dec.set_keep_deep(true);
    let mut out = Decoded { per_sample: Vec::new(), damaged: 0, error: None };
    for s in samples(data)? {
        match dec.decode(s, 0.0) {
            Ok(frames) => {
                out.damaged += frames.iter().filter(|f| f.damaged).count();
                out.per_sample.push(frames.iter().map(frame_md5).collect());
            }
            Err(e) => {
                out.per_sample.push(Vec::new());
                if out.error.is_none() {
                    out.error = Some(e);
                }
                if !keep_going {
                    break;
                }
            }
        }
    }
    Some(out)
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("directory of test vectors"));
    let filter = std::env::args().nth(2).unwrap_or_default();
    let (mut ok, mut bad, mut unsupported, mut errors, mut invalid) = (0, 0, 0, 0, 0);
    for path in vectors(&dir) {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.contains(&filter) {
            continue;
        }
        let data = std::fs::read(&path).unwrap();
        let t0 = std::time::Instant::now();
        if name.starts_with("invalid-") {
            // corrupt streams: decoding must not panic; errors are expected
            let r = decode(&data, true);
            invalid += 1;
            match r {
                Some(d) => println!("{name:60} survived ({} frames, {} damaged{})", d.per_sample.iter().map(|s| s.len()).sum::<usize>(), d.damaged, d.error.map_or(String::new(), |e| format!(", first error: {e}"))),
                None => println!("{name:60} survived (the demuxer rejects it)"),
            }
            continue;
        }
        let Ok(text) = std::fs::read_to_string(format!("{}.md5", path.display())) else {
            println!("{name:60} no .md5 file");
            continue;
        };
        let want: Vec<&str> = text.lines().filter_map(|l| l.split_whitespace().next()).collect();
        let Some(d) = decode(&data, false) else {
            println!("{name:60} ERROR: cannot demux");
            errors += 1;
            continue;
        };
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        match d.error {
            Some(Error::Unsupported(s)) => {
                unsupported += 1;
                println!("{name:60} unsupported: {s}");
                continue;
            }
            Some(Error::Bitstream(s)) => {
                errors += 1;
                println!("{name:60} ERROR after {} samples: {s}", d.per_sample.len() - 1);
                continue;
            }
            None => {}
        }
        // libvpx returns one frame per sample (the last one shown); a
        // superframe may show two
        let all: Vec<&String> = d.per_sample.iter().flatten().collect();
        let last: Vec<&String> = d.per_sample.iter().filter_map(|s| s.last()).collect();
        let got = if all.len() == want.len() { &all } else { &last };
        let matching = got.iter().zip(&want).filter(|(g, w)| g.as_str() == **w).count();
        if got.len() == want.len() && matching == want.len() && d.damaged == 0 {
            ok += 1;
            println!("{name:60} ok   {} frames ({ms:.0} ms){}", want.len(), if got.len() != all.len() { ", one per sample" } else { "" });
        } else {
            bad += 1;
            let first = got.iter().zip(&want).position(|(g, w)| g.as_str() != *w);
            println!("{name:60} MISMATCH {matching} of {} frames match (ours {}, damaged {}){}", want.len(), got.len(), d.damaged, first.map_or(String::new(), |i| format!(", first wrong frame {i}")));
        }
    }
    println!("\n{ok} bit-exact, {bad} mismatching, {unsupported} unsupported, {errors} errors; {invalid} invalid streams decoded without a panic");
    if bad > 0 || errors > 0 {
        std::process::exit(1);
    }
}
