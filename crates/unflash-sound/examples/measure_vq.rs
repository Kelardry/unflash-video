//! Measure the two vector quantizer code books of Annex D.10, which the
//! standard does not print ("Due to its extensive size, this table is not
//! included here"), from ffmpeg's decoder as a black box, and write them
//! as `src/dts/vq.rs`:
//!
//!     cargo run --release -p unflash-sound --example measure_vq -- src/dts/vq.rs
//!
//! Frames built here put each code vector alone in one subband of one
//! channel (subband 1, the others silent), with the perfect
//! reconstruction filter bank, whose synthesis is orthogonal: ffmpeg's
//! output of that channel, correlated with the filter bank's response to
//! a single subband sample (this decoder's, which agrees with ffmpeg's to
//! float precision), gives back the subband's samples. A high frequency
//! vector (D.10.2) is then the samples over the scale factor, times 16;
//! an ADPCM vector (D.10.1) is the four prediction coefficients that,
//! after an impulse in the subband's residual, predict the next seven
//! samples from the four before, times 2^13 (least squares). Both come
//! out within a small fraction of an integer of an integer; the program
//! checks that, and that the first ADPCM entry is 9928, the example
//! D.10.1 gives.

#[path = "../tests/common/mod.rs"]
mod common;

use common::dts::{write_frame, Band, Channel, Frame, Subframe};
use common::{ffmpeg_available, ffmpeg_decode_bytes};
use unflash_sound::dts::testing::RMS6;
use unflash_sound::dts::{Decoder, Output};

/// Five channels (3/2), subband 1 of each active, the others silent.
fn frame_for(subframes: usize, nssc: usize, vq: bool) -> Frame {
    let mut f = Frame { amode: 9, filts: true, hflag: true, ..Default::default() };
    let ch = Channel { subs: 2, vqsub: if vq { 1 } else { 2 }, shuff: 5, bhuff: 6, ..Default::default() };
    f.channels = vec![ch; 5];
    let band = |len: usize| Band { levels: vec![0; len], ..Default::default() };
    f.subframes = (0..subframes).map(|_| Subframe { nssc, bands: vec![vec![band(8 * nssc), band(8 * nssc)]; 5], ..Default::default() }).collect();
    f
}

/// The step size of ABITS 11 (D.2.1) and the impulse put in the ADPCM
/// residuals: level 127 with scale factor index 44.
const STEP_11: f64 = 146801.0 / 4194304.0;
const IMPULSE_SCALE: usize = 44;
/// The scale factor index of the high frequency vectors.
const VQ_SCALE: usize = 44;

/// This decoder's output for a single sample `amplitude` in subband 1
/// of a channel at slot 0, as a function of time (1024 samples, more
/// than the filter bank's reach).
fn impulse_response() -> (Vec<f64>, f64) {
    let mut f = frame_for(16, 1, false);
    for s in f.subframes.iter_mut() {
        for c in 0..5 {
            s.bands[c][1] = Band { abits: 11, scales: [IMPULSE_SCALE as i32, 0], levels: vec![0; 8], ..Default::default() };
        }
    }
    f.subframes[0].bands[0][1].levels[0] = 127;
    let data = write_frame(&f);
    let mut dec = Decoder::new(Output::Native);
    let mut out = Vec::new();
    dec.decode(&data, &mut out).unwrap();
    // channel C (coded 0) is WAVE channel 2
    let h: Vec<f64> = out[2][..1024].iter().map(|&v| v as f64).collect();
    let amplitude = 127.0 * STEP_11 * RMS6[IMPULSE_SCALE] as f64;
    (h, amplitude)
}

/// Subband 1's samples of one channel of `y`, in units of the impulse
/// response's amplitude.
fn analyse(y: &[f32], h: &[f64], slots: usize) -> Vec<f64> {
    let energy: f64 = h.iter().map(|v| v * v).sum();
    (0..slots)
        .map(|t| {
            let seg = &y[(32 * t).min(y.len())..];
            seg.iter().zip(h).map(|(&a, &b)| a as f64 * b).sum::<f64>() / energy
        })
        .collect()
}

/// Solve the 4 by 4 normal equations by Gaussian elimination.
fn solve4(mut a: [[f64; 5]; 4]) -> [f64; 4] {
    for i in 0..4 {
        let p = (i..4).max_by(|&x, &y| a[x][i].abs().total_cmp(&a[y][i].abs())).unwrap();
        a.swap(i, p);
        for r in 0..4 {
            if r != i {
                let k = a[r][i] / a[i][i];
                let pivot = a[i];
                for (x, p) in a[r][i..].iter_mut().zip(&pivot[i..]) {
                    *x -= k * p;
                }
            }
        }
    }
    [a[0][4] / a[0][0], a[1][4] / a[1][1], a[2][4] / a[2][2], a[3][4] / a[3][3]]
}

fn main() {
    let path = std::env::args().nth(1).expect("the file to write (src/dts/vq.rs)");
    if !ffmpeg_available() {
        eprintln!("ffmpeg and ffprobe must be on the path");
        std::process::exit(2);
    }
    let (h, amplitude) = impulse_response();
    // WAVE order of 3/2: L R C Ls Rs, coded C L R Ls Rs
    let wave_of_coded = [2, 0, 1, 3, 4];

    // D.10.1: 16 subframes of 8 samples a frame, every other one silent
    // (so that the next starts without history), in the others one
    // vector per channel with an impulse at the start
    let per_frame = 8 * 5;
    let frames = 4096usize.div_ceil(per_frame);
    let mut data = Vec::new();
    for fi in 0..=frames {
        let mut f = frame_for(16, 1, false);
        if fi < frames {
            for (s, sub) in f.subframes.iter_mut().enumerate().filter(|(s, _)| s % 2 == 1) {
                for c in 0..5 {
                    let k = (fi * per_frame + s / 2 * 5 + c) % 4096;
                    let mut levels = vec![0; 8];
                    levels[0] = 127;
                    sub.bands[c][1] = Band { pmode: true, pvq: k as u16, abits: 11, scales: [IMPULSE_SCALE as i32, 0], levels, ..Default::default() };
                }
            }
        }
        data.extend(write_frame(&f));
    }
    let y = ffmpeg_decode_bytes(&data, "adpcm.dts", &[], &[]).expect("ffmpeg decodes the frames");
    let slots = (frames + 1) * 128;
    let mut adpcm = vec![[0i16; 4]; 4096];
    let mut worst: f64 = 0.0;
    for c in 0..5 {
        let x = analyse(&y[wave_of_coded[c]], &h, slots);
        for fi in 0..frames {
            for s in (1..16).step_by(2) {
                let k = fi * per_frame + s / 2 * 5 + c;
                if k >= 4096 {
                    continue;
                }
                let at = fi * 128 + s * 8;
                // x[m] = sum_i c_i x[m - 1 - i] for m = 1 to 7, least squares
                let mut a = [[0f64; 5]; 4];
                for m in at + 1..at + 8 {
                    let row = [x[m - 1], x[m - 2], x[m - 3], x[m - 4]];
                    for i in 0..4 {
                        for j in 0..4 {
                            a[i][j] += row[i] * row[j];
                        }
                        a[i][4] += row[i] * x[m];
                    }
                }
                let coeffs = solve4(a);
                for i in 0..4 {
                    let v = coeffs[i] * 8192.0;
                    worst = worst.max((v - v.round()).abs());
                    adpcm[k][i] = v.round() as i16;
                }
            }
        }
    }
    eprintln!("ADPCM vectors: the largest distance from an integer {worst:.3}");
    assert!(worst < 0.25, "not integers: {worst}");
    let worst_adpcm = worst;
    assert_eq!(adpcm[0][0], 9928, "D.10.1's example");

    // D.10.2: 4 subframes of 32 samples a frame, one vector per channel
    // and subframe
    let per_frame = 4 * 5;
    let frames = 1024usize.div_ceil(per_frame);
    let mut data = Vec::new();
    for fi in 0..=frames {
        let mut f = frame_for(4, 4, true);
        if fi < frames {
            for (s, sub) in f.subframes.iter_mut().enumerate() {
                for c in 0..5 {
                    let k = (fi * per_frame + s * 5 + c) % 1024;
                    sub.bands[c][1] = Band { hfvq: k as u16, scales: [VQ_SCALE as i32, 0], ..Default::default() };
                }
            }
        }
        data.extend(write_frame(&f));
    }
    let y = ffmpeg_decode_bytes(&data, "hfvq.dts", &[], &[]).expect("ffmpeg decodes the frames");
    let slots = (frames + 1) * 128;
    let mut hf = vec![[0i8; 32]; 1024];
    let mut worst: f64 = 0.0;
    for c in 0..5 {
        let x = analyse(&y[wave_of_coded[c]], &h, slots);
        for fi in 0..frames {
            for s in 0..4 {
                let k = fi * per_frame + s * 5 + c;
                if k >= 1024 {
                    continue;
                }
                for m in 0..32 {
                    let v = x[fi * 128 + s * 32 + m] * amplitude / RMS6[VQ_SCALE] as f64 * 16.0;
                    worst = worst.max((v - v.round()).abs());
                    assert!((-128.0..=127.0).contains(&v.round()), "vector {k}: {v}");
                    hf[k][m] = v.round() as i8;
                }
            }
        }
    }
    eprintln!("high frequency vectors: the largest distance from an integer {worst:.3}");
    assert!(worst < 0.25, "not integers: {worst}");

    let mut s = String::new();
    s += "//! The vector quantizer code books of Annex D.10, which the standard\n";
    s += "//! does not print (\"Due to its extensive size, this table is not included\n";
    s += "//! here\"). They were measured from ffmpeg's decoder as a black box by\n";
    s += "//! `examples/measure_vq.rs`, which wrote this file: see there how. The\n";
    s += &format!("//! ADPCM entries came out within {worst_adpcm:.2} of an integer, the high frequency\n");
    s += &format!("//! ones within {worst:.3}, and the first ADPCM entry is the 9928 of D.10.1's\n");
    s += "//! example.\n\n";
    s += "/// D.10.1: the ADPCM prediction coefficients of each 12-bit PVQ index,\n";
    s += "/// times 2^13, for the samples 1 to 4 back.\n";
    s += "pub static ADPCM: [[i16; 4]; 4096] = [\n";
    for row in adpcm.chunks(6) {
        let items: Vec<String> = row.iter().map(|v| format!("[{}, {}, {}, {}]", v[0], v[1], v[2], v[3])).collect();
        s += &format!("    {},\n", items.join(", "));
    }
    s += "];\n\n";
    s += "/// D.10.2: the 32 subband samples of each 10-bit high frequency VQ\n";
    s += "/// index, times 2^4.\n";
    s += "pub static HIGH_FREQUENCY: [[i8; 32]; 1024] = [\n";
    for v in &hf {
        let items: Vec<String> = v.iter().map(|x| x.to_string()).collect();
        s += &format!("    [{}],\n", items.join(", "));
    }
    s += "];\n";
    std::fs::write(&path, s).expect("write the file");
    eprintln!("wrote {path}");
}
