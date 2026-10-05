//! Decode an IVF, WebM or MP4 file with the built-in decoder and compare
//! every frame with ffmpeg's output, reporting the first differing frame
//! and where in it the samples differ.
//!
//!     cargo run --release -p unflash-vp9 --example compare -- file.ivf [max_frames]

use std::io::Read;
use std::process::{Command, Stdio};

use unflash_vp9::{Decoder, Frame};

/// The samples of a file: (data, pts), from IVF or through the MP4 /
/// Matroska demuxer.
fn samples(data: &[u8]) -> Vec<(Vec<u8>, f64)> {
    if let Some(frames) = unflash_mp4::ivf::frames(data) {
        return frames.into_iter().map(|(ts, f)| (f.to_vec(), ts as f64)).collect();
    }
    let movie = unflash_mp4::demux::parse_bytes(data).expect("demux");
    let track = movie.video().expect("video track");
    track.samples.iter().map(|s| (data[s.offset as usize..(s.offset + s.size as u64) as usize].to_vec(), s.pts as f64)).collect()
}

/// The frame as ffmpeg's rawvideo lays it out (16-bit little-endian
/// samples above 8 bits).
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
    let mut dec = Decoder::new(&[]).unwrap();
    dec.set_keep_deep(true);
    let mut frames = Vec::new();
    let t0 = std::time::Instant::now();
    for (i, (s, pts)) in samples(&data).iter().enumerate() {
        if frames.len() >= max {
            break;
        }
        match dec.decode(s, *pts) {
            Ok(fs) => frames.extend(fs),
            Err(e) => {
                println!("sample {i}: {e}");
                break;
            }
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    println!("{} frames in {:.3} s ({:.1} fps)", frames.len(), secs, frames.len() as f64 / secs);
    let Some(first) = frames.first() else { return };
    let pix_fmt = match first.bit_depth {
        8 => "yuv420p",
        10 => "yuv420p10le",
        _ => "yuv420p12le",
    };
    // (-autoscale 0: frames keep their own size when it changes)
    let mut child = Command::new("ffmpeg").args(["-v", "error", "-i", &path, "-autoscale", "0", "-f", "rawvideo", "-pix_fmt", pix_fmt, "-"]).stdout(Stdio::piped()).spawn().expect("ffmpeg");
    let mut reference = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut reference).unwrap();
    child.wait().unwrap();
    let mut offset = 0;
    for (i, f) in frames.iter().enumerate() {
        let ours = raw(f);
        if offset + ours.len() > reference.len() {
            println!("frame {i}: ffmpeg has no more frames");
            return;
        }
        let theirs = &reference[offset..offset + ours.len()];
        offset += ours.len();
        if ours != theirs {
            let bps = if f.bit_depth > 8 { 2 } else { 1 };
            let (w, h) = (f.width as usize, f.height as usize);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let planes = [(0, w, h), (w * h, cw, ch), (w * h + cw * ch, cw, ch)];
            println!("frame {i} (pts {}, {}x{}, damaged {}) differs:", f.pts, w, h, f.damaged);
            for (p, &(start, pw, ph)) in planes.iter().enumerate() {
                let mut count = 0;
                let mut firsts = Vec::new();
                for y in 0..ph {
                    for x in 0..pw {
                        let k = (start + y * pw + x) * bps;
                        if ours[k..k + bps] != theirs[k..k + bps] {
                            count += 1;
                            if firsts.len() < 6 {
                                firsts.push(format!("({x},{y}) ours {} ffmpeg {}", ours[k], theirs[k]));
                            }
                        }
                    }
                }
                if count > 0 {
                    println!("  plane {p}: {count} samples differ, first {}", firsts.join(", "));
                }
            }
            return;
        }
    }
    let total = reference.len();
    println!("all {} frames match ffmpeg{}", frames.len(), if offset < total { format!(" (ffmpeg has {} more bytes)", total - offset) } else { String::new() });
}
