//! Time the decoder over an Annex B stream, for profiling:
//!
//!     cargo run --release -p unflash-h264 --example bench -- file.264 [reps] [fast]
//!
//! Prints the frames decoded and the best time of `reps` runs (the stream
//! is read into memory first; pictures are dropped as they come). With
//! `fast`, the deblocking filter is left out.

use std::time::Instant;
use unflash_h264::decoder::annexb_nal_units;
use unflash_h264::Decoder;

fn main() {
    let path = std::env::args().nth(1).expect("an Annex B .264 file");
    let reps: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    let fast = std::env::args().nth(3).is_some_and(|s| s == "fast");
    let data = std::fs::read(&path).unwrap();
    let nals = annexb_nal_units(&data);
    let mut best = f64::MAX;
    let mut frames = 0;
    let mut check = 0u64;
    for _ in 0..reps {
        let mut dec = Decoder::new();
        dec.set_skip_deblock(fast);
        let t0 = Instant::now();
        let mut n = 0;
        check = 0;
        let mut take = |frames: Vec<unflash_h264::DecodedFrame>| {
            for f in frames {
                n += 1;
                // touch the picture so that nothing is optimised away
                check = check.wrapping_add(f.pic.y.iter().step_by(4099).map(|&v| v as u64).sum::<u64>());
            }
        };
        for nal in &nals {
            dec.decode_nal(nal, 0.0).unwrap();
            take(dec.decode_annexb(&[], 0.0).unwrap());
        }
        take(dec.flush().unwrap().into_iter().collect());
        best = best.min(t0.elapsed().as_secs_f64());
        frames = n;
    }
    println!("{frames} frames, best of {reps}: {best:.3} s = {:.1} fps ({:.2} MB/s of stream) [check {check}]", frames as f64 / best, data.len() as f64 / 1e6 / best);
}
