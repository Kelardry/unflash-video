//! Decoding speed: decode a file (IVF or WebM / MP4) a few times and report
//! frames per second.
//!
//!     cargo run --release -p unflash-vp9 --example bench -- file.webm [runs]

use unflash_vp9::Decoder;

fn samples(data: &[u8]) -> Vec<&[u8]> {
    if let Some(frames) = unflash_mp4::ivf::frames(data) {
        return frames.into_iter().map(|(_, f)| f).collect();
    }
    let movie = unflash_mp4::demux::parse_bytes(data).expect("demux");
    let track = movie.video().expect("video track");
    track.samples.iter().map(|s| &data[s.offset as usize..(s.offset + s.size as u64) as usize]).collect()
}

fn main() {
    let path = std::env::args().nth(1).expect("file");
    let runs: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    let data = std::fs::read(&path).unwrap();
    let samples = samples(&data);
    let mut best = f64::MAX;
    let mut frames = 0;
    let mut size = (0, 0);
    for _ in 0..runs {
        let mut dec = Decoder::new(&[]).unwrap();
        let t0 = std::time::Instant::now();
        frames = 0;
        for s in &samples {
            for f in dec.decode(s, 0.0).expect("decode") {
                size = (f.width, f.height);
                frames += 1;
            }
        }
        best = best.min(t0.elapsed().as_secs_f64());
    }
    println!("{}x{} {} frames: {:.1} fps", size.0, size.1, frames, frames as f64 / best);
}
