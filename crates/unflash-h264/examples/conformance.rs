//! Decode the JVT conformance streams (Annex B byte streams) and compare
//! every picture with ffmpeg's per-frame MD5s (`<stream>.framemd5`, made with
//! `ffmpeg -i <stream> -f framemd5 <stream>.framemd5`). Output order is not
//! compared, only the multiset of pictures.
//!
//!     cargo run --release -p unflash-h264 --example conformance -- <dir> [name-filter]

use std::path::{Path, PathBuf};

use unflash_h264::yuv::to_i420;
use unflash_h264::{Decoder, Error};

fn streams(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for sub in [dir.to_path_buf(), dir.join("FRext")] {
        let Ok(rd) = std::fs::read_dir(&sub) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            if ["264", "jsv", "avc", "26l", "jvt", "h264"].contains(&ext.as_str()) {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

fn expected(path: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(format!("{}.framemd5", path.display())).ok()?;
    Some(text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).map(|l| l.rsplit(',').next().unwrap().trim().to_string()).collect())
}

/// Decode a whole stream; the MD5 of every output picture, in output order.
fn decode(path: &Path) -> Result<(Vec<String>, usize), Error> {
    let data = std::fs::read(path).expect("read");
    let mut dec = Decoder::new();
    let mut md5s = Vec::new();
    let mut damaged = 0;
    let mut buf = Vec::new();
    let mut push = |f: unflash_h264::DecodedFrame| {
        to_i420(&f.pic, f.crop, &mut buf);
        md5s.push(format!("{:x}", md5::compute(&buf)));
        damaged += f.damaged as usize;
    };
    for f in dec.decode_annexb(&data, 0.0)? {
        push(f);
    }
    if let Some(f) = dec.flush()? {
        push(f);
    }
    Ok((md5s, damaged))
}

/// Streams where ffmpeg's output is known to deviate from the standard, so
/// a mismatch is expected: with disable_deblocking_filter_idc 2 in field
/// pictures ffmpeg decides whether to restore the unfiltered top-left
/// sample for intra prediction from the other field's slice table
/// (xchg_mb_border uses a frame-row step), so its intra prediction sees a
/// deblocked corner sample.
const KNOWN_DEVIATIONS: &[&str] = &["slice2_field_aurora4.264"];

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("directory of conformance streams"));
    let filter = std::env::args().nth(2).unwrap_or_default().to_ascii_lowercase();
    let (mut ok, mut bad, mut unsupported, mut errors, mut missing) = (0, 0, 0, 0, 0);
    for path in streams(&dir) {
        let name = path.strip_prefix(&dir).unwrap().display().to_string();
        if !filter.is_empty() && !name.to_ascii_lowercase().contains(&filter) {
            continue;
        }
        let Some(want) = expected(&path) else {
            println!("{name:32} no ffmpeg reference");
            missing += 1;
            continue;
        };
        let t0 = std::time::Instant::now();
        match decode(&path) {
            Ok((got, damaged)) => {
                let mut a = got.clone();
                let mut b = want.clone();
                a.sort();
                b.sort();
                let matching = {
                    let (mut i, mut j, mut m) = (0, 0, 0);
                    while i < a.len() && j < b.len() {
                        match a[i].cmp(&b[j]) {
                            std::cmp::Ordering::Equal => {
                                m += 1;
                                i += 1;
                                j += 1;
                            }
                            std::cmp::Ordering::Less => i += 1,
                            std::cmp::Ordering::Greater => j += 1,
                        }
                    }
                    m
                };
                let same_order = got == want;
                if matching == want.len() && got.len() == want.len() && damaged == 0 {
                    ok += 1;
                    println!("{name:32} ok   {} frames{} ({:.0} ms)", got.len(), if same_order { "" } else { ", output order differs" }, t0.elapsed().as_secs_f64() * 1e3);
                } else if KNOWN_DEVIATIONS.contains(&name.as_str()) && got.len() == want.len() && damaged == 0 {
                    ok += 1;
                    println!("{name:32} ok?  {} frames, {} match ffmpeg (known ffmpeg deviation, not compared)", got.len(), matching);
                } else {
                    bad += 1;
                    let first_bad = got.iter().position(|g| !want.contains(g));
                    println!("{name:32} MISMATCH {} of {} frames match (ours {}, damaged {}){}", matching, want.len(), got.len(), damaged, first_bad.map_or(String::new(), |i| format!(", first wrong picture at output {i}")));
                }
            }
            Err(Error::Unsupported(s)) => {
                unsupported += 1;
                println!("{name:32} unsupported: {s}");
            }
            Err(Error::Bitstream(s)) => {
                errors += 1;
                println!("{name:32} ERROR: {s}");
            }
        }
    }
    println!("\n{ok} ok, {bad} mismatching, {unsupported} unsupported, {errors} errors, {missing} without reference");
    if bad > 0 || errors > 0 {
        std::process::exit(1);
    }
}
