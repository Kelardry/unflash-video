//! Decode a stream with the built-in decoder and compare every picture
//! with ffmpeg's, reporting where the first differences are. MP4 files are
//! compared in presentation order; Annex B streams (`.bit`, `.hevc`,
//! `.265`) picture by picture against ffmpeg's closest picture.
//!
//!     cargo run --release -p unflash-hevc --example compare -- file [max_frames]

use std::io::Read;
use std::process::{Command, Stdio};

use unflash_hevc::{Decoder, Frame};
use unflash_mp4::demux::parse_bytes;

/// The picture as ffmpeg's rawvideo output lays it out: 8-bit planes, or
/// 16-bit little-endian ones above 8 bits.
fn raw(f: &Frame) -> Vec<u8> {
    match (&f.y16, &f.u16, &f.v16) {
        (Some(y), Some(u), Some(v)) => y.iter().chain(u).chain(v).flat_map(|s| s.to_le_bytes()).collect(),
        _ => [&f.y[..], &f.u[..], &f.v[..]].concat(),
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("file");
    let max: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let data = std::fs::read(&path).unwrap();
    let t0 = std::time::Instant::now();
    let mut frames: Vec<Frame> = Vec::new();
    let annexb = !path.ends_with(".mp4") && !path.ends_with(".mkv");
    if annexb {
        let mut dec = Decoder::new(&[]).unwrap();
        dec.set_keep_deep(true);
        match dec.decode_annexb(&data, 0.0) {
            Ok(f) => frames.extend(f),
            Err(e) => println!("error: {e}"),
        }
        frames.extend(dec.flush().unwrap_or_default());
    } else {
        let movie = parse_bytes(&data).unwrap();
        let track = movie.video().unwrap();
        let mut dec = Decoder::new(track.description.as_deref().unwrap_or(&[])).unwrap();
        dec.set_keep_deep(true);
        for (i, s) in track.samples.iter().enumerate() {
            let bytes = &data[s.offset as usize..(s.offset + s.size as u64) as usize];
            match dec.decode(bytes, s.pts as f64) {
                Ok(f) => frames.extend(f),
                Err(e) => println!("sample {i}: error {e}"),
            }
            if frames.len() >= max {
                break;
            }
        }
        frames.extend(dec.flush().unwrap_or_default());
        frames.sort_by(|a, b| a.pts.total_cmp(&b.pts));
    }
    let dt = t0.elapsed();
    let Some(first) = frames.first() else {
        println!("no pictures decoded");
        std::process::exit(1);
    };
    let (w, h, deep) = (first.width as usize, first.height as usize, first.bit_depth > 8);
    println!("decoded {} pictures of {w}x{h} in {:.1} ms ({:.1} fps)", frames.len(), dt.as_secs_f64() * 1e3, frames.len() as f64 / dt.as_secs_f64());
    let fmt = if deep { "yuv420p10le" } else { "yuv420p" };
    // exact cropping: by default ffmpeg keeps a left crop that would break alignment
    let mut child = Command::new("ffmpeg").args(["-v", "error", "-flags", "unaligned", "-i", &path, "-f", "rawvideo", "-pix_fmt", fmt, "-"]).stdout(Stdio::piped()).spawn().expect("ffmpeg");
    let mut reference = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut reference).unwrap();
    child.wait().expect("ffmpeg");
    let bps = if deep { 2 } else { 1 };
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let size = (w * h + 2 * cw * ch) * bps;
    let theirs: Vec<&[u8]> = reference.chunks_exact(size).collect();
    println!("ffmpeg produced {} pictures", theirs.len());
    let mut bad = 0;
    for (k, f) in frames.iter().enumerate() {
        let ours = raw(f);
        if ours.len() != size {
            println!("picture {k}: size {}x{} differs", f.width, f.height);
            bad += 1;
            continue;
        }
        let other = if annexb {
            // the closest of ffmpeg's pictures
            theirs.iter().min_by_key(|t| t.iter().zip(&ours).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum::<u64>()).copied()
        } else {
            theirs.get(k).copied()
        };
        let Some(other) = other else { break };
        if ours == other {
            continue;
        }
        bad += 1;
        if bad > 4 {
            continue;
        }
        let sample = |buf: &[u8], i: usize| if deep { u16::from_le_bytes([buf[2 * i], buf[2 * i + 1]]) as i32 } else { buf[i] as i32 };
        let n = size / bps;
        let diffs: Vec<usize> = (0..n).filter(|&i| sample(&ours, i) != sample(other, i)).collect();
        let locate = |i: usize| -> (&str, usize, usize) {
            if i < w * h {
                ("Y", i % w, i / w)
            } else if i < w * h + cw * ch {
                let c = i - w * h;
                ("U", c % cw, c / cw)
            } else {
                let c = i - w * h - cw * ch;
                ("V", c % cw, c / cw)
            }
        };
        let (plane, x, y) = locate(diffs[0]);
        println!("picture {k} (pts {}{}): {} samples differ; first {plane} ({x}, {y}): ours {} ffmpeg {}", f.pts, if f.damaged { ", damaged" } else { "" }, diffs.len(), sample(&ours, diffs[0]), sample(other, diffs[0]));
        if std::env::var("HEVC_DIFF").is_ok() {
            for &i in diffs.iter().take(40) {
                let (plane, x, y) = locate(i);
                println!("   {plane} ({x}, {y}): ours {} ffmpeg {}", sample(&ours, i), sample(other, i));
            }
        }
    }
    if bad == 0 && frames.len() == theirs.len() {
        println!("all {} pictures identical", frames.len());
    } else {
        println!("{bad} pictures differ");
        std::process::exit(1);
    }
}
