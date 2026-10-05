//! Corrupt input must never panic (a panic aborts the whole WebAssembly
//! instance): the test streams are decoded with random bit flips, byte
//! changes, truncated and reordered samples, and every result — frames,
//! damaged frames or errors — is acceptable except a panic.
//!
//! VP9_FUZZ_ROUNDS=n runs n rounds per stream instead of a few (in a debug
//! build, arithmetic overflow is caught too).

use std::path::PathBuf;

use unflash_vp9::header::{parse_uncompressed_header, StreamState};
use unflash_vp9::Decoder;

fn media(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/media/vp9").join(name)
}

fn ivf_samples(data: &[u8]) -> Vec<Vec<u8>> {
    unflash_mp4::ivf::frames(data).expect("an IVF file").into_iter().map(|(_, f)| f.to_vec()).collect()
}

/// xorshift64*: deterministic, so a failure can be replayed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn mutate(rng: &mut Rng, samples: &mut Vec<Vec<u8>>, other: &[Vec<u8>]) {
    for _ in 0..1 + rng.below(3) {
        let i = rng.below(samples.len());
        let s = &mut samples[i];
        match rng.below(8) {
            // flip a few bits, often in the headers
            0 | 1 => {
                for _ in 0..1 + rng.below(4) {
                    if !s.is_empty() {
                        let limit = if rng.below(2) == 0 { s.len().min(24) } else { s.len() };
                        let k = rng.below(limit);
                        s[k] ^= 1 << rng.below(8);
                    }
                }
            }
            // overwrite a run of bytes
            2 => {
                if !s.is_empty() {
                    let k = rng.below(s.len());
                    for j in k..(k + 1 + rng.below(16)).min(s.len()) {
                        s[j] = rng.next() as u8;
                    }
                }
            }
            // truncate
            3 => {
                let n = rng.below(s.len() + 1);
                s.truncate(n);
            }
            // drop or repeat a sample
            4 => {
                samples.remove(i);
                if samples.is_empty() {
                    samples.push(Vec::new());
                }
            }
            5 => {
                let s = samples[i].clone();
                samples.insert(rng.below(samples.len()), s);
            }
            // a sample of another stream
            6 => {
                samples[i] = other[rng.below(other.len())].clone();
            }
            // random bytes appended (a bogus superframe index, say)
            _ => {
                for _ in 0..1 + rng.below(12) {
                    s.push(rng.next() as u8);
                }
            }
        }
    }
}

#[test]
fn corrupt_streams_do_not_panic() {
    let rounds: usize = std::env::var("VP9_FUZZ_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(12);
    let names = ["altref.ivf", "p2_10bit.ivf", "tiles.ivf", "aq_cyclic.ivf", "lossless.ivf", "odd_small.ivf", "resize_odd.ivf", "errres.ivf"];
    let streams: Vec<Vec<Vec<u8>>> = names.iter().map(|n| ivf_samples(&std::fs::read(media(n)).expect("run tests/media/vp9/gen.sh"))).collect();
    let all: Vec<Vec<u8>> = streams.iter().flatten().cloned().collect();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for (name, samples) in names.iter().zip(&streams) {
        for round in 0..rounds {
            let mut s = samples.clone();
            mutate(&mut rng, &mut s, &all);
            let mut dec = Decoder::new(&[]).unwrap();
            let mut frames = 0;
            for sample in &s {
                if let Ok(f) = dec.decode(sample, 0.0) {
                    frames += f.len();
                }
            }
            assert!(frames <= s.len() * 8, "{name} round {round}: too many frames");
        }
    }
}

/// A frame of another size that fails in its compressed header leaves the
/// decoder at the old size, and the next frame's segment map prediction
/// must still find a map of that size: here aq_cyclic's 176x144 frames
/// (which predict their maps temporally) around odd_small's 33x17 key
/// frame, whose compressed header no longer starts with its zero marker
/// (the map was left at the key frame's size, and read past its end).
#[test]
fn failed_size_change_keeps_the_segment_map_whole() {
    let cyclic = ivf_samples(&std::fs::read(media("aq_cyclic.ivf")).expect("run tests/media/vp9/gen.sh"));
    let mut key = ivf_samples(&std::fs::read(media("odd_small.ivf")).expect("run tests/media/vp9/gen.sh")).swap_remove(0);
    let fh = parse_uncompressed_header(&key, &mut StreamState::default(), &[None; 8]).unwrap();
    key[fh.uncompressed_size] = 0xff;
    let mut dec = Decoder::new(&[]).unwrap();
    for s in &cyclic[..6] {
        dec.decode(s, 0.0).unwrap();
    }
    assert!(dec.decode(&key, 0.0).is_err());
    assert!(dec.decode(&cyclic[6], 0.0).is_ok());
}

/// Every byte of a sample can be a frame that shows a reference frame
/// again (show_existing_frame), each shown frame a full copy: a sample
/// shows at most eight, as many as a superframe can hold.
#[test]
fn shown_frames_per_sample_are_bounded() {
    let key = ivf_samples(&std::fs::read(media("odd_small.ivf")).expect("run tests/media/vp9/gen.sh")).swap_remove(0);
    let mut dec = Decoder::new(&[]).unwrap();
    assert_eq!(dec.decode(&key, 0.0).unwrap().len(), 1);
    // (0x88: show_existing_frame of slot 0, in one byte)
    let shown = dec.decode(&[0x88; 300], 0.0).unwrap().len();
    assert!((1..=8).contains(&shown), "{shown} frames from one sample");
}
