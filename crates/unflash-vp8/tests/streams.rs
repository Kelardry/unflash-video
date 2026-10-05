//! Bit-exactness against ffmpeg: every stream in tests/media/vp8 decodes to
//! the per-frame MD5s ffmpeg's own VP8 decoder produced (`gen.sh`); and
//! damaged streams decode without panicking.

use std::path::PathBuf;

use unflash_mp4::demux::parse_bytes;
use unflash_vp8::{Decoder, Error, Frame};

fn media(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/media/vp8").join(name)
}

/// The samples of a file in decoding order with their times: the frames
/// of an IVF file, or the video samples of a WebM file.
fn samples(file: &str) -> Vec<(f64, Vec<u8>)> {
    let data = std::fs::read(media(file)).expect("run tests/media/vp8/gen.sh");
    if let Some(frames) = unflash_mp4::ivf::frames(&data) {
        return frames.into_iter().map(|(ts, f)| (ts as f64, f.to_vec())).collect();
    }
    let movie = parse_bytes(&data).unwrap();
    let track = movie.video().unwrap();
    assert_eq!(track.codec, "vp8");
    track.samples.iter().map(|s| (s.pts as f64, data[s.offset as usize..(s.offset + s.size as u64) as usize].to_vec())).collect()
}

fn md5(f: &Frame) -> String {
    let mut ctx = md5::Context::new();
    ctx.consume(&f.y);
    ctx.consume(&f.u);
    ctx.consume(&f.v);
    format!("{:x}", ctx.compute())
}

/// Decode a file: the MD5 of every shown frame, in output order.
fn decode(file: &str) -> Result<Vec<String>, Error> {
    let mut dec = Decoder::new(&[])?;
    let mut md5s = Vec::new();
    for (pts, s) in samples(file) {
        for f in dec.decode(&s, pts)? {
            assert!(!f.damaged, "{file}: damaged frame at {pts}");
            assert_eq!(f.pts, pts);
            assert_eq!(f.y.len(), (f.width * f.height) as usize);
            assert_eq!(f.u.len(), (f.width.div_ceil(2) * f.height.div_ceil(2)) as usize);
            assert!(!f.bt709 && !f.full_range);
            md5s.push(md5(&f));
        }
    }
    assert!(dec.flush()?.is_empty());
    Ok(md5s)
}

fn expected(file: &str) -> Vec<String> {
    let stem = file.rsplit_once('.').unwrap().0;
    let text = std::fs::read_to_string(media(&format!("{stem}.framemd5"))).unwrap();
    text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).map(|l| l.rsplit(',').next().unwrap().trim().to_string()).collect()
}

fn check(file: &str) {
    let got = decode(file).unwrap_or_else(|e| panic!("{file}: {e}"));
    let want = expected(file);
    assert_eq!(got.len(), want.len(), "{file}: frame count");
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, w, "{file}: frame {i} differs from ffmpeg");
    }
}

#[test]
fn smooth_sixtap_splits_and_references() {
    check("smooth.ivf");
}
#[test]
fn intra_subblock_modes() {
    check("intra.ivf");
}
#[test]
fn altref_invisible_frames_sign_bias_webm() {
    check("altref.webm");
}
#[test]
fn error_resilient_cyclic_segmentation_webm() {
    check("resilient.webm");
}
#[test]
fn segment_map_updated_then_kept() {
    check("roi.ivf");
}
#[test]
fn eight_token_partitions() {
    check("partitions.ivf");
}
#[test]
fn loop_filter_sharpness() {
    check("sharpness.ivf");
}
#[test]
fn low_quantiser_large_coefficients() {
    check("lowq.ivf");
}
#[test]
fn high_quantiser_strong_filtering() {
    check("highq.ivf");
}
#[test]
fn odd_size() {
    check("odd.ivf");
}
#[test]
fn smaller_than_a_macroblock() {
    check("tiny.ivf");
}
#[test]
fn version1_bilinear_simple_filter() {
    check("version1.ivf");
}
#[test]
fn version2_bilinear() {
    check("version2.ivf");
}
#[test]
fn version3_whole_sample_chroma() {
    check("version3.ivf");
}
#[test]
fn temporal_layers() {
    check("layers.ivf");
}
#[test]
fn long_vectors_beyond_the_picture() {
    check("pan.ivf");
}
#[test]
fn size_change_at_a_key_frame() {
    check("resize.ivf");
}

/// Starting at a later key frame gives the same pictures as decoding from
/// the start: nothing leaks across key frames.
#[test]
fn key_frames_start_afresh() {
    let all = decode("intra.ivf").unwrap();
    let s = samples("intra.ivf");
    let mut dec = Decoder::new(&[]).unwrap();
    // an inter frame first is refused
    assert!(matches!(dec.decode(&s[1].1, 0.0), Err(Error::Bitstream(_))));
    let mut got = Vec::new();
    for (pts, data) in &s[5..] {
        got.extend(dec.decode(data, *pts).unwrap().iter().map(md5));
    }
    assert_eq!(got, all[5..]);
}

/// Truncated, corrupted and nonsensical samples give errors or damaged
/// frames, never a panic, and the decoder recovers at the next key frame.
#[test]
fn damaged_streams_do_not_panic() {
    let mut seed = 12345u32;
    let mut rand = move || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        seed >> 8
    };
    for file in ["smooth.ivf", "altref.webm", "partitions.ivf", "resilient.webm", "resize.ivf", "version3.ivf"] {
        let s = samples(file);
        // every frame cut short at a few lengths
        let mut dec = Decoder::new(&[]).unwrap();
        for (pts, data) in &s {
            for cut in [0, 1, 2, 3, 9, 10, 11, data.len() / 3, data.len() / 2, data.len() - 1] {
                let _ = dec.decode(&data[..cut.min(data.len())], *pts);
            }
            let _ = dec.decode(data, *pts);
        }
        // bytes flipped at random
        let mut dec = Decoder::new(&[]).unwrap();
        for (pts, data) in &s {
            let mut bad = data.clone();
            for _ in 0..1 + rand() % 8 {
                let i = rand() as usize % bad.len();
                bad[i] ^= 1 << (rand() % 8);
            }
            let _ = dec.decode(&bad, *pts);
        }
        // the clean stream from the first key frame decodes exactly again
        let mut dec = Decoder::new(&[]).unwrap();
        let _ = dec.decode(&[0x50, 0x42, 0x00], 0.0);
        let mut got = Vec::new();
        for (pts, data) in &s {
            got.extend(dec.decode(data, *pts).unwrap().iter().map(md5));
        }
        assert_eq!(got, expected(file), "{file}");
    }
    // key frames of absurd sizes and garbage
    let mut dec = Decoder::new(&[]).unwrap();
    let mut key = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0xff, 0x3f, 0xff, 0x3f];
    key.extend((0..64).map(|_| rand() as u8));
    let _ = dec.decode(&key, 0.0);
    for len in 0..200 {
        let junk: Vec<u8> = (0..len).map(|_| rand() as u8).collect();
        let _ = dec.decode(&junk, 0.0);
    }
}
