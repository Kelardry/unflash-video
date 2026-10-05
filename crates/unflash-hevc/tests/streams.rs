//! Bit-exactness against ffmpeg: every stream in tests/media/hevc decodes
//! to the per-frame MD5s ffmpeg's decoder produced (`gen.sh`), and the
//! decoder survives damaged copies of them.

use std::path::PathBuf;

use unflash_hevc::{Decoder, Error, Frame};
use unflash_mp4::demux::parse_bytes;

mod common;
use common::{framemd5, md5};

fn media(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/media/hevc").join(name)
}

/// The MP4 file's parameter sets and samples.
fn samples(name: &str) -> (Vec<u8>, Vec<(Vec<u8>, f64)>) {
    let data = std::fs::read(media(&format!("{name}.mp4"))).expect("run tests/media/hevc/gen.sh");
    let movie = parse_bytes(&data).unwrap();
    let track = movie.video().unwrap();
    let samples = track.samples.iter().map(|s| (data[s.offset as usize..(s.offset + s.size as u64) as usize].to_vec(), s.pts as f64)).collect();
    (track.description.clone().unwrap(), samples)
}

/// Decode a file; every picture in presentation order (deeper ones at full
/// precision too with `keep_deep`).
fn decode(name: &str, keep_deep: bool) -> Result<Vec<Frame>, Error> {
    let (config, samples) = samples(name);
    let mut dec = Decoder::new(&config)?;
    dec.set_keep_deep(keep_deep);
    let mut frames = Vec::new();
    for (bytes, pts) in &samples {
        let got = dec.decode(bytes, *pts)?;
        assert!(got.iter().all(|f| f.pts == *pts), "{name}: a picture without its sample's timestamp");
        frames.extend(got);
    }
    frames.extend(dec.flush()?);
    frames.sort_by(|a, b| a.pts.total_cmp(&b.pts));
    Ok(frames)
}

fn expected(name: &str) -> Vec<String> {
    framemd5(&std::fs::read_to_string(media(&format!("{name}.framemd5"))).unwrap())
}

fn check(name: &str) {
    // (ffmpeg's MD5s of a deeper stream are of its 16-bit samples)
    let got = decode(name, true).unwrap_or_else(|e| panic!("{name}: {e}"));
    let want = expected(name);
    assert_eq!(got.len(), want.len(), "{name}: frame count");
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert!(!g.damaged, "{name}: damaged frame {i}");
        assert_eq!(&md5(g), w, "{name}: frame {i} differs from ffmpeg");
    }
}

#[test]
fn intra_no_loop_filters() {
    check("intra");
}
#[test]
fn intra_deblocking_sao_strong_smoothing() {
    check("intra_filters");
}
#[test]
fn p_pictures_deep_transform_trees() {
    check("p_frames");
}
#[test]
fn b_pyramid_merge_candidates() {
    check("b_pyramid");
}
#[test]
fn explicit_weighted_prediction() {
    check("weighted");
}
#[test]
fn asymmetric_and_rectangular_partitions() {
    check("amp_rect");
}
#[test]
fn transform_skip_scaling_lists() {
    check("tskip_scaling");
}
#[test]
fn several_slices_deblocking_offsets() {
    check("slices");
}
#[test]
fn wavefronts() {
    check("wpp");
}
#[test]
fn lossless() {
    check("lossless");
}
#[test]
fn lossless_coding_units_qp_deltas() {
    check("cu_lossless");
}
#[test]
fn constrained_intra_prediction() {
    check("cip");
}
#[test]
fn open_gops() {
    check("open_gop");
}
#[test]
fn conformance_window_chroma_qp_offsets() {
    check("odd_size");
}
#[test]
fn main10() {
    check("main10");
}
#[test]
fn main10_weighted_lossless() {
    check("main10_wp_lossless");
}

#[test]
fn monochrome() {
    let got = decode("mono", false).unwrap();
    let want = expected("mono");
    assert_eq!(got.len(), want.len(), "mono: frame count");
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert!(!g.damaged, "mono: damaged frame {i}");
        assert!(g.u.iter().chain(&g.v).all(|&c| c == 128), "mono: frame {i} is not grey");
        // ffmpeg's gray frames have the luma plane only
        assert_eq!(&format!("{:x}", md5::compute(&g.y)), w, "mono: frame {i} differs from ffmpeg");
    }
}

/// The full-precision planes come only when asked for (the app reads the
/// 8-bit ones alone), and the 8-bit planes are the same either way.
#[test]
fn ten_bit_frames_carry_rounded_eight_bit_planes() {
    let plain = decode("main10", false).unwrap();
    let deep = decode("main10", true).unwrap();
    assert_eq!(plain.len(), deep.len());
    for (p, f) in plain.iter().zip(&deep) {
        assert_eq!((p.bit_depth, f.bit_depth), (10, 10));
        assert!(p.y16.is_none() && p.u16.is_none() && p.v16.is_none());
        assert!(p.y == f.y && p.u == f.u && p.v == f.v);
        let y16 = f.y16.as_ref().unwrap();
        assert_eq!(y16.len(), f.y.len());
        assert!(y16.iter().zip(&f.y).all(|(&h, &l)| ((h + 2) >> 2).min(255) as u8 == l));
    }
}

/// A small deterministic generator for the damage the tests below do.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// Damaged input must never panic (a panic aborts the WebAssembly
/// instance): truncated samples, flipped bits, overwritten bytes, dropped
/// and repeated samples; errors and damaged pictures are fine.
#[test]
fn damaged_streams_do_not_panic() {
    let names = ["intra_filters", "b_pyramid", "weighted", "amp_rect", "tskip_scaling", "slices", "wpp", "cu_lossless", "open_gop", "odd_size", "main10_wp_lossless", "mono"];
    let mut rng = Lcg(1);
    for name in names {
        let (config, samples) = samples(name);
        for _ in 0..24 {
            let mut dec = Decoder::new(&config).unwrap();
            for (i, (bytes, pts)) in samples.iter().enumerate() {
                let mut b = bytes.clone();
                match rng.next() % 8 {
                    0 if !b.is_empty() => b.truncate(rng.next() as usize % b.len()),
                    1 | 2 => {
                        for _ in 0..1 + rng.next() % 4 {
                            let k = rng.next() as usize % b.len().max(1);
                            if let Some(x) = b.get_mut(k) {
                                *x ^= 1 << (rng.next() % 8);
                            }
                        }
                    }
                    3 => {
                        let k = rng.next() as usize % b.len().max(1);
                        for x in b.iter_mut().skip(k).take(1 + rng.next() as usize % 16) {
                            *x = rng.next() as u8;
                        }
                    }
                    4 if i % 3 == 0 => continue,
                    _ => {}
                }
                let _ = dec.decode(&b, *pts);
                if rng.next().is_multiple_of(16) {
                    let _ = dec.decode(&b, *pts);
                }
            }
            let _ = dec.flush();
        }
    }
}

/// The parameter sets an hvcC box carries.
fn config_nal_units(config: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 23;
    for _ in 0..config[22] {
        let n = u16::from_be_bytes([config[i + 1], config[i + 2]]);
        i += 3;
        for _ in 0..n {
            let len = u16::from_be_bytes([config[i], config[i + 1]]) as usize;
            out.push(config[i + 2..i + 2 + len].to_vec());
            i += 2 + len;
        }
    }
    out
}

#[test]
fn damaged_configuration_and_annexb_do_not_panic() {
    let (config, samples) = samples("b_pyramid");
    let mut rng = Lcg(7);
    for _ in 0..200 {
        let mut c = config.clone();
        let k = rng.next() as usize % c.len();
        c[k] ^= 1 << (rng.next() % 8);
        c.truncate(c.len() - rng.next() as usize % 8);
        if let Ok(mut dec) = Decoder::new(&c) {
            for (bytes, pts) in samples.iter().take(6) {
                let _ = dec.decode(bytes, *pts);
            }
            let _ = dec.flush();
        }
    }
    // the same stream as Annex B, parameter sets included, cut and corrupted
    let mut annexb = Vec::new();
    for nal in config_nal_units(&config) {
        annexb.extend_from_slice(&[0, 0, 0, 1]);
        annexb.extend_from_slice(&nal);
    }
    for (bytes, _) in &samples {
        let mut i = 0;
        while i + 4 <= bytes.len() {
            let n = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
            let end = (i + 4 + n).min(bytes.len());
            annexb.extend_from_slice(&[0, 0, 0, 1]);
            annexb.extend_from_slice(&bytes[i + 4..end]);
            i = end;
        }
    }
    let mut dec = Decoder::new(&[]).unwrap();
    let clean = dec.decode_annexb(&annexb, 0.0).unwrap().len() + dec.flush().unwrap().len();
    assert_eq!(clean, expected("b_pyramid").len());
    for round in 0..60 {
        let mut a = annexb.clone();
        for _ in 0..1 + round % 8 {
            let k = rng.next() as usize % a.len();
            a[k] = rng.next() as u8;
        }
        let cut = rng.next() as usize % a.len();
        let mut dec = Decoder::new(&[]).unwrap();
        let _ = dec.decode_annexb(&a[..cut], 0.0);
        let _ = dec.decode_annexb(&a[cut..], 1.0);
        let _ = dec.flush();
    }
}

/// The NAL units of a sample (these files have four-byte lengths).
fn nal_units(sample: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= sample.len() {
        let n = u32::from_be_bytes([sample[i], sample[i + 1], sample[i + 2], sample[i + 3]]) as usize;
        out.push(&sample[i + 4..i + 4 + n]);
        i += 4 + n;
    }
    out
}

/// A sample of the given NAL units.
fn sample_of(nal_units: &[&[u8]]) -> Vec<u8> {
    nal_units.iter().flat_map(|n| (n.len() as u32).to_be_bytes().into_iter().chain(n.iter().copied())).collect()
}

/// A NAL unit length near 2^32 is an error. (On 32-bit targets, wasm32
/// among them, the bounds check wrapped around and the slicing panicked;
/// on 64-bit ones the sum cannot overflow, so only a 32-bit target can
/// fail this test.)
#[test]
fn nal_length_near_four_gigabytes() {
    let mut dec = Decoder::new(&[]).unwrap();
    assert!(dec.decode(&[0xff; 8], 0.0).is_err());
}

/// A slice that refers to other parameter sets than its picture's is
/// refused rather than decoded with the picture's: here slices.mp4's
/// parameter sets and the second slice of its first picture (160x96, at
/// CTB 3) follow the first slice of cip's first picture (96x64, two CTBs),
/// with whose parameter sets it was decoded, from past the picture's end
/// (a panic).
#[test]
fn slice_with_other_parameter_sets_is_refused() {
    let (config, cip) = samples("cip");
    let (other_config, other) = samples("slices");
    let other_sets = config_nal_units(&other_config);
    let mut nals = nal_units(&cip[0].0);
    nals.extend(other_sets.iter().map(|n| &n[..]));
    nals.push(nal_units(&other[0].0).into_iter().filter(|n| (n[0] >> 1) & 0x3f < 32).nth(1).unwrap());
    let mut dec = Decoder::new(&config).unwrap();
    assert!(dec.decode(&sample_of(&nals), 0.0).is_err());
}

/// A new sequence parameter set at a picture that is not a random access
/// point (a bad splice) starts without the old sequence's pictures: here
/// b_pyramid's parameter sets and its tenth picture, which predicts from
/// order counts 8, 7, 5 and 3, follow mono's last picture (with mono's
/// 4:0:0 pictures of order counts 5 to 9 as references, which 4:2:0
/// chroma prediction panicked on).
#[test]
fn new_sequence_without_a_random_access_point() {
    let (mono_config, mono) = samples("mono");
    let (config, b_pyramid) = samples("b_pyramid");
    let mut dec = Decoder::new(&mono_config).unwrap();
    for (bytes, pts) in &mono {
        dec.decode(bytes, *pts).unwrap();
    }
    let sets = config_nal_units(&config);
    let mut nals: Vec<&[u8]> = sets.iter().map(|n| &n[..]).collect();
    nals.extend(nal_units(&b_pyramid[9].0));
    let frames = dec.decode(&sample_of(&nals), 0.0).unwrap();
    assert!(frames.len() == 1 && frames[0].damaged && (frames[0].width, frames[0].height) == (128, 96));
}
