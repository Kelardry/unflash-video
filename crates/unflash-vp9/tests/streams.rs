//! Bit-exactness against ffmpeg: every stream in tests/media/vp9 decodes to
//! the per-frame MD5s ffmpeg's decoder produced (`gen.sh`), over the 8-bit
//! planes or, for deeper streams, the 16-bit ones.

use std::path::PathBuf;

use unflash_vp9::Decoder;

mod common;
use common::frame_md5;

fn media(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/media/vp9").join(name)
}

/// The samples of an IVF file, or of a WebM / MP4 file through the
/// demuxer.
fn samples(data: &[u8]) -> Vec<(&[u8], f64)> {
    if let Some(frames) = unflash_mp4::ivf::frames(data) {
        return frames.into_iter().map(|(ts, f)| (f, ts as f64)).collect();
    }
    let movie = unflash_mp4::demux::parse_bytes(data).unwrap();
    let track = movie.video().unwrap();
    track.samples.iter().map(|s| (&data[s.offset as usize..(s.offset + s.size as u64) as usize], s.pts as f64)).collect()
}

fn check(file: &str) {
    let data = std::fs::read(media(file)).expect("run tests/media/vp9/gen.sh");
    let mut dec = Decoder::new(&[]).unwrap();
    // (ffmpeg's MD5s of a deeper stream are of its 16-bit samples)
    dec.set_keep_deep(true);
    let mut got = Vec::new();
    for (sample, pts) in samples(&data) {
        let frames = dec.decode(sample, pts).unwrap_or_else(|e| panic!("{file}: {e} at pts {pts}"));
        for f in frames {
            assert!(!f.damaged, "{file}: damaged frame at pts {pts}");
            assert_eq!(f.y.len(), (f.width * f.height) as usize);
            assert_eq!(f.u.len(), (f.width.div_ceil(2) * f.height.div_ceil(2)) as usize);
            got.push(frame_md5(&f));
        }
    }
    let stem = file.rsplit_once('.').unwrap().0;
    let text = std::fs::read_to_string(media(&format!("{stem}.framemd5"))).unwrap();
    let want: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).map(|l| l.rsplit(',').next().unwrap().trim()).collect();
    assert_eq!(got.len(), want.len(), "{file}: frame count");
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, w, "{file}: frame {i} differs from ffmpeg");
    }
}

#[test]
fn altref_superframes_compound() {
    check("altref.ivf");
    check("altref_mb.ivf");
}
#[test]
fn webm_through_the_demuxer() {
    check("altref.webm");
}
#[test]
fn realtime_modes() {
    check("realtime.ivf");
}
#[test]
fn profile2_10_bit() {
    check("p2_10bit.ivf");
}
#[test]
fn profile2_12_bit() {
    check("p2_12bit.ivf");
}
#[test]
fn error_resilient() {
    check("errres.ivf");
}
#[test]
fn frame_parallel_no_adaptation() {
    check("parallel.ivf");
}
#[test]
fn tile_columns_and_rows() {
    check("tiles.ivf");
}
#[test]
fn lossless_walsh_hadamard() {
    check("lossless.ivf");
    check("lossless_10.ivf");
}
#[test]
fn segmentation_adaptive_quantisation() {
    check("aq_variance.ivf");
    check("aq_cyclic.ivf");
}
#[test]
fn loop_filter_sharpness() {
    check("sharpness.ivf");
}
#[test]
fn quantiser_extremes() {
    check("q_fine.ivf");
    check("q_coarse.ivf");
}
#[test]
fn odd_sizes() {
    check("odd.ivf");
    check("odd_small.ivf");
}
#[test]
fn scaled_references() {
    check("resize.ivf");
    check("resize_odd.ivf");
}

/// The full-precision planes come only when asked for (the app reads the
/// 8-bit ones alone); the 8-bit planes are the same either way, and are the
/// full-precision ones rounded.
#[test]
fn deep_frames_carry_rounded_eight_bit_planes() {
    for file in ["p2_10bit.ivf", "p2_12bit.ivf"] {
        let data = std::fs::read(media(file)).expect("run tests/media/vp9/gen.sh");
        let (mut plain, mut deep) = (Decoder::new(&[]).unwrap(), Decoder::new(&[]).unwrap());
        deep.set_keep_deep(true);
        for (sample, pts) in samples(&data) {
            let (a, b) = (plain.decode(sample, pts).unwrap(), deep.decode(sample, pts).unwrap());
            assert_eq!(a.len(), b.len(), "{file}");
            for (p, f) in a.iter().zip(&b) {
                assert!(p.y16.is_none() && p.u16.is_none() && p.v16.is_none(), "{file}: 16-bit planes nobody asked for");
                assert!(p.y == f.y && p.u == f.u && p.v == f.v, "{file}: the 8-bit planes differ");
                let shift = f.bit_depth as u32 - 8;
                for (full, rounded) in [(&f.y16, &f.y), (&f.u16, &f.u), (&f.v16, &f.v)] {
                    let full = full.as_ref().unwrap();
                    assert_eq!(full.len(), rounded.len());
                    assert!(full.iter().zip(rounded).all(|(&h, &l)| ((h as u32 + (1 << (shift - 1))) >> shift).min(255) as u8 == l), "{file}: not rounded");
                }
            }
        }
    }
}
