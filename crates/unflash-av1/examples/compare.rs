//! Decode the AV1 track of an MP4 / Matroska / WebM file, compare every
//! picture with ffmpeg's libdav1d decoding (when ffmpeg is installed), then
//! time the decoder on one thread.
//!
//!     cargo run --release -p unflash-av1 --example compare -- file.mkv [max_frames]
//!
//! 10-bit pictures are compared at 10 bits (`Decoder::decode_raw`).

use std::process::Command;
use std::time::Instant;

use unflash_av1::Decoder;
use unflash_mp4::demux::parse_bytes;

fn main() {
    let path = std::env::args().nth(1).expect("file");
    let max: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let data = std::fs::read(&path).unwrap();
    let movie = parse_bytes(&data).unwrap();
    let track = movie.tracks.iter().find(|t| t.codec.starts_with("av01") && !t.samples.is_empty()).expect("an AV1 track");
    let config = track.description.clone().unwrap_or_default();
    let samples: Vec<(Vec<u8>, f64)> = track
        .samples
        .iter()
        .enumerate()
        .take(max)
        .map(|(i, s)| {
            let mut b = track.prefix.clone();
            b.extend_from_slice(&data[s.offset as usize..][..s.size as usize]);
            (b, track.pts_secs(i))
        })
        .collect();
    println!("{path}: {} {}x{}, {} samples", track.codec, track.width, track.height, samples.len());

    // ffmpeg's pictures, at the stream's own bit depth
    let ten_bit = track.codec.ends_with(".10");
    let pix = if ten_bit { "yuv420p10le" } else { "yuv420p" };
    let want: Option<Vec<String>> = Command::new("ffmpeg")
        .args(["-v", "error", "-c:v", "libdav1d", "-i", &path, "-frames:v", &samples.len().to_string(), "-autoscale", "0", "-fps_mode", "passthrough", "-f", "framemd5", "-pix_fmt", pix, "-"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).map(|l| l.rsplit(',').next().unwrap().trim().to_string()).collect());
    if let Some(want) = &want {
        let mut dec = Decoder::new(&config).unwrap();
        let mut got = Vec::new();
        let mut damaged = 0;
        for (bytes, pts) in &samples {
            match dec.decode_raw(bytes, *pts) {
                Ok(frames) => {
                    for f in frames {
                        damaged += f.damaged as usize;
                        let mut c = md5::Context::new();
                        for plane in [&f.y, &f.u, &f.v] {
                            let bytes: Vec<u8> = if f.bit_depth == 8 { plane.iter().map(|&v| v as u8).collect() } else { plane.iter().flat_map(|v| v.to_le_bytes()).collect() };
                            c.consume(&bytes);
                        }
                        got.push((format!("{:x}", c.compute()), f.pts, f.width, f.height));
                    }
                }
                Err(e) => println!("sample at {pts}: {e}"),
            }
        }
        let same = got.iter().zip(want).take_while(|(g, w)| &g.0 == *w).count();
        if same == got.len() && got.len() == want.len() {
            println!("bit-exact with ffmpeg: {} pictures ({damaged} damaged)", got.len());
        } else {
            println!("{} pictures, ffmpeg {}; the first {same} match", got.len(), want.len());
            if let Some(g) = got.get(same) {
                println!("first difference: picture {same}, pts {}, {}x{}", g.1, g.2, g.3);
            }
        }
    } else {
        println!("(ffmpeg with libdav1d not found: no comparison)");
    }

    let mut dec = Decoder::new(&config).unwrap();
    let t0 = Instant::now();
    let mut n = 0;
    for (bytes, pts) in &samples {
        n += dec.decode(bytes, *pts).map(|f| f.len()).unwrap_or(0);
    }
    n += dec.flush().unwrap().len();
    let s = t0.elapsed().as_secs_f64();
    println!("{n} pictures in {s:.2} s, {:.1} fps", n as f64 / s);
}
