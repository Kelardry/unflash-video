//! Decoding speed: decode an IVF or WebM file a few times over and report
//! frames per second.
//!
//!     cargo run --release -p unflash-vp8 --example bench -- <file> [runs]

use unflash_vp8::Decoder;

/// The frames of an IVF file, or of the video track of a WebM file.
fn frames(data: &[u8]) -> Vec<&[u8]> {
    if let Some(frames) = unflash_mp4::ivf::frames(data) {
        return frames.into_iter().map(|(_, f)| f).collect();
    }
    let movie = unflash_mp4::demux::parse_bytes(data).expect("an IVF or WebM file");
    let track = movie.video().expect("a video track");
    track.samples.iter().map(|s| &data[s.offset as usize..(s.offset + s.size as u64) as usize]).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.iter().find(|a| a.parse::<u32>().is_err()).expect("a file");
    let runs = args.iter().filter_map(|a| a.parse::<u32>().ok()).next().unwrap_or(3);
    let data = std::fs::read(path).expect("read");
    let samples = frames(&data);
    let mut best = f64::MAX;
    let mut shown = 0;
    let mut size = (0, 0);
    for _ in 0..runs {
        let mut dec = Decoder::new(&[]).unwrap();
        let t0 = std::time::Instant::now();
        shown = 0;
        for (i, s) in samples.iter().enumerate() {
            for f in dec.decode(s, i as f64).expect("decode") {
                shown += 1;
                size = (f.width, f.height);
            }
        }
        best = best.min(t0.elapsed().as_secs_f64());
    }
    println!("{}x{}: {} frames in {:.3} s (best of {runs}): {:.1} frames/s, {:.2} ms/frame", size.0, size.1, shown, best, shown as f64 / best, best * 1e3 / shown as f64);
}
