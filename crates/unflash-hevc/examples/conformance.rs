//! Decode the JCT-VC HEVC conformance streams (Annex B byte streams, as
//! mirrored by ffmpeg's FATE suite) and compare every picture with
//! ffmpeg's per-frame MD5s, `<stream>.framemd5`. Missing ones are made
//! with ffmpeg (which must then be on the path) as
//!
//!     ffmpeg -flags unaligned -i <stream> -pix_fmt <format> -f framemd5 <stream>.framemd5
//!
//! where the format is the decoder's native one at the stream's bit depth
//! (yuv420p, yuv420p10le, ...; `-flags unaligned` makes ffmpeg apply a
//! left crop exactly). Output order is not compared, only the multiset of
//! pictures. The decoder also checks every picture against the stream's
//! decoded picture hash SEI messages (most conformance streams carry
//! them), which covers pictures that are never output and streams ffmpeg
//! cannot decode.
//!
//!     cargo run --release -p unflash-hevc --example conformance -- <dir> [name-filter]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use unflash_hevc::{Decoder, Error};

#[path = "../tests/common/mod.rs"]
mod common;
use common::{framemd5, md5};

/// Streams in which the standard's output process (C.5.2.2) discards
/// pictures still waiting for output at an IRAP picture, as ffmpeg does.
/// This decoder hands out every picture as soon as it is decoded (the
/// caller orders them by timestamp), so it returns those pictures too;
/// they are checked against the stream's picture hashes instead.
const OUTPUT_DIFFERENCES: &[(&str, &str)] = &[
    ("BUMPING_A_ericsson_1.bit", "BLA pictures with no_output_of_prior_pics_flag"),
    ("NoOutPrior_A_Qualcomm_1.bit", "a CRA picture after an end of sequence"),
    ("NoOutPrior_B_Qualcomm_1.bit", "an IDR picture with no_output_of_prior_pics_flag"),
    ("RAP_B_Bossen_1.bit", "a CRA picture after an end of sequence"),
];

fn streams(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("directory of conformance streams")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            ["bit", "bin", "hevc", "265"].contains(&ext.as_str())
        })
        .collect();
    v.sort();
    v
}

/// ffmpeg's per-frame MD5s of a stream, made first if there are none yet.
fn expected(path: &Path, bit_depth: u8) -> Option<Vec<String>> {
    let file = format!("{}.framemd5", path.display());
    if !Path::new(&file).exists() {
        let format = if bit_depth > 8 { format!("yuv420p{bit_depth}le") } else { "yuv420p".to_string() };
        let made = Command::new("ffmpeg").args(["-nostdin", "-v", "error", "-flags", "unaligned", "-i"]).arg(path).args(["-pix_fmt", &format, "-f", "framemd5", &file]).status();
        if !made.is_ok_and(|s| s.success()) {
            return None;
        }
    }
    let md5s = framemd5(&std::fs::read_to_string(&file).ok()?);
    (!md5s.is_empty()).then_some(md5s)
}

/// How many suffix SEI NAL units open with a decoded picture hash message.
fn picture_hashes(data: &[u8]) -> usize {
    data.windows(6).filter(|w| w[..3] == [0, 0, 1] && (w[3] >> 1) & 0x3f == 40 && w[5] == 132).count()
}

/// Decode a whole stream; the MD5 of every output picture, in output
/// order, how many were damaged (or differ from their hash), and the bit
/// depth.
fn decode(data: &[u8]) -> Result<(Vec<String>, usize, u8), Error> {
    let mut dec = Decoder::new(&[])?;
    dec.set_check_hashes(true);
    // (ffmpeg's MD5s of a deeper stream are of its 16-bit samples)
    dec.set_keep_deep(true);
    let mut frames = dec.decode_annexb(data, 0.0)?;
    frames.extend(dec.flush()?);
    let damaged = frames.iter().filter(|f| f.damaged).count();
    let bit_depth = frames.first().map_or(8, |f| f.bit_depth);
    Ok((frames.iter().map(md5).collect(), damaged, bit_depth))
}

/// How many of `want` are in `got`, as multisets.
fn matching(got: &[String], want: &[String]) -> usize {
    let (mut a, mut b) = (got.to_vec(), want.to_vec());
    a.sort();
    b.sort();
    let (mut i, mut j, mut n) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                n += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    n
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("directory of conformance streams"));
    let filter = std::env::args().nth(2).unwrap_or_default().to_ascii_lowercase();
    let (mut ok, mut bad, mut unsupported, mut errors, mut missing) = (0, 0, 0, 0, 0);
    let mut total_time = 0.0;
    for path in streams(&dir) {
        let name = path.strip_prefix(&dir).unwrap_or(&path).display().to_string();
        if !filter.is_empty() && !name.to_ascii_lowercase().contains(&filter) {
            continue;
        }
        // the name first, so that a crash shows which stream caused it
        print!("{name:40} ");
        std::io::stdout().flush().ok();
        let data = std::fs::read(&path).expect("read");
        let hashes = picture_hashes(&data);
        let t0 = std::time::Instant::now();
        let result = decode(&data);
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        total_time += ms;
        let (got, damaged, bit_depth) = match result {
            Ok(r) => r,
            Err(Error::Unsupported(s)) => {
                unsupported += 1;
                println!("unsupported: {s}");
                continue;
            }
            Err(Error::Bitstream(s)) => {
                errors += 1;
                println!("ERROR: {s}");
                continue;
            }
        };
        let Some(want) = expected(&path, bit_depth) else {
            if damaged == 0 && hashes >= got.len() {
                ok += 1;
                println!("ok   {} pictures, no ffmpeg reference but all match the stream's hashes ({ms:.0} ms)", got.len());
            } else {
                missing += 1;
                println!("no ffmpeg reference ({} pictures, {damaged} damaged or unlike their hash, {hashes} hashes)", got.len());
            }
            continue;
        };
        let n = matching(&got, &want);
        let note = OUTPUT_DIFFERENCES.iter().find(|(n, _)| *n == name).map(|(_, why)| *why);
        if n == want.len() && got.len() == want.len() && damaged == 0 {
            ok += 1;
            println!("ok   {} pictures{} ({ms:.0} ms)", got.len(), if got == want { "" } else { ", output order differs" });
        } else if let (Some(why), true) = (note, n == want.len() && damaged == 0) {
            ok += 1;
            println!("ok   {} pictures, and {} the output process discards after {why}, checked by hash ({ms:.0} ms)", n, got.len() - n);
        } else {
            bad += 1;
            let first_bad = got.iter().position(|g| !want.contains(g));
            println!("MISMATCH {n} of {} pictures match (ours {}, damaged or unlike their hash {damaged}){}", want.len(), got.len(), first_bad.map_or(String::new(), |i| format!(", first wrong picture at output {i}")));
        }
    }
    println!("\n{ok} bit-exact, {bad} mismatching, {unsupported} unsupported, {errors} errors, {missing} without reference ({:.1} s)", total_time / 1e3);
    if bad > 0 || errors > 0 {
        std::process::exit(1);
    }
}
