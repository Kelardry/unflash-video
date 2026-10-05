//! Bit-exactness against ffmpeg: every stream in tests/media/h264 decodes
//! to the per-frame MD5s ffmpeg's decoder produced (`gen.sh`).

mod common;

use unflash_h264::Decoder;
use unflash_mp4::demux::parse_bytes;

fn check(name: &str) {
    let data = common::read(name);
    let movie = parse_bytes(&data).unwrap();
    let track = movie.video().unwrap();
    let mut dec = Decoder::new();
    let got = dec.configure_avcc(track.description.as_ref().unwrap()).and_then(|_| common::md5s(&mut dec, &data, track, name, |b| b.to_vec())).unwrap_or_else(|e| panic!("{name}: {e}"));
    let want = common::expected(name);
    assert_eq!(got.len(), want.len(), "{name}: frame count");
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, w, "{name}: frame {i} differs from ffmpeg");
    }
}

#[test]
fn constrained_baseline_cavlc() {
    check("cb_cavlc");
}
#[test]
fn baseline_cropped_two_slices() {
    check("cb_crop_slices");
}
#[test]
fn main_cabac_bframes_temporal_direct() {
    check("main_cabac_b");
}
#[test]
fn main_cavlc_bframes_weighted() {
    check("main_cavlc_wp");
}
#[test]
fn high_8x8_transform_aq_slices() {
    check("high_8x8_aq");
}
#[test]
fn high_scaling_matrices_weighted() {
    check("high_cqm_wp");
}
#[test]
fn high_constrained_intra_no_deblock() {
    check("high_cip_nodeblock");
}
#[test]
fn high_open_gop_many_refs() {
    check("high_opengop_refs");
}
#[test]
fn high_cropped_cabac() {
    check("high_crop_cabac");
}
#[test]
fn pcm_macroblocks_cabac() {
    check("pcm_cabac");
}
#[test]
fn pcm_macroblocks_cavlc() {
    check("pcm_cavlc");
}
#[test]
fn high_interlaced_mbaff() {
    check("high_interlaced");
}
