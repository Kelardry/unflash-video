//! Every stream in tests/media/ac3 (made by its gen.sh with ffmpeg's
//! encoders) against ffmpeg's float decoder, run at test time: every
//! transform coefficient without noise must agree to float precision, and
//! the noise must be as loud as ffmpeg's (see `common/ac3.rs`). Without
//! ffmpeg the comparisons are skipped, with a message.

mod common;

use std::path::PathBuf;

use common::ac3::{compare, describe};
use common::{ffmpeg_decode, skip};
use unflash_sound::ac3::{Decoder, Output, StreamInfo};

fn media(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/media/ac3").join(name)
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(media(name)).unwrap_or_else(|e| panic!("{name}: {e} (run tests/media/ac3/gen.sh)"))
}

/// Decode a stream and compare it with ffmpeg's decode.
fn check(name: &str) {
    if skip(name) {
        return;
    }
    let data = read(name);
    let noisy = common::ac3::decode(&data, Output::Native, true);
    let quiet = common::ac3::decode(&data, Output::Native, false);
    let reference = ffmpeg_decode(&media(name), &[], &[]).expect("ffmpeg decodes the stream");
    assert_eq!(noisy.out.len(), reference.len(), "{name}: channel count");
    assert_eq!(noisy.out[0].len(), reference[0].len(), "{name}: sample count");
    assert_eq!(noisy.decoded.damaged, 0, "{name}: damaged frames");
    let map: Vec<Option<usize>> = (0..reference.len()).map(Some).collect();
    for (c, s) in compare(&quiet.out, &noisy.out, &noisy.trace, &reference, &map).iter().enumerate() {
        eprintln!("{name} channel {c}: {}", describe(s));
        assert!(s.bins > 1000, "{name} channel {c}: too little to compare");
        assert_eq!(s.over, 0, "{name} channel {c}: coefficients differ from ffmpeg's by up to {:e}", s.max_diff);
        assert_eq!(s.skipped_blocks, 0, "{name} channel {c}");
        for (what, n) in [("dither", s.dither), ("AHT dither", s.aht_dither), ("SPX noise", s.spx)] {
            // below 1e-7 the noise is lost in the float rounding of the output
            if n.bins >= 300 && n.rms_ours() > 1e-7 {
                assert!((0.7..=1.4).contains(&n.ratio()), "{name} channel {c}: ffmpeg's {what} is {:.2} times ours", n.ratio());
            }
        }
        if s.time_noise_rms > 1e-6 {
            assert!((0.7..=1.4).contains(&s.time_ratio), "{name} channel {c}: ffmpeg's noise is {:.2} times ours in the time domain", s.time_ratio);
        }
        // ffmpeg's float transform itself strays by up to 1.3e-5 at full
        // scale (ours stays within 2e-7 of a double precision one)
        assert!(s.quiet_max_diff < 5e-5, "{name} channel {c}: noise-free blocks differ by {:e}", s.quiet_max_diff);
    }
}

#[test]
fn ac3_mono() {
    check("ac3_mono_48k.ac3");
}
#[test]
fn ac3_stereo_44k() {
    check("ac3_stereo_44k.ac3");
}
#[test]
fn ac3_stereo_32k_low_rate() {
    check("ac3_stereo_32k_48kbps.ac3");
}
#[test]
fn ac3_2f1r() {
    check("ac3_2f1r_48k.ac3");
}
#[test]
fn ac3_3f() {
    check("ac3_3f_44k.ac3");
}
#[test]
fn ac3_3f1r() {
    check("ac3_3f1r_32k.ac3");
}
#[test]
fn ac3_2f2r() {
    check("ac3_2f2r_48k.ac3");
}
#[test]
fn ac3_3f2r_lfe() {
    check("ac3_3f2r_lfe_48k.ac3");
}
#[test]
fn ac3_3f2r_lfe_44k_low_rate() {
    check("ac3_3f2r_lfe_44k_192k.ac3");
}
#[test]
fn ac3_1f_lfe() {
    check("ac3_1f_lfe_32k.ac3");
}
#[test]
fn ac3_2f_lfe() {
    check("ac3_2f_lfe_48k.ac3");
}
#[test]
fn ac3_2f1r_lfe() {
    check("ac3_2f1r_lfe_44k.ac3");
}
#[test]
fn ac3_3f1r_lfe() {
    check("ac3_3f1r_lfe_48k.ac3");
}
#[test]
fn ac3_annex_d() {
    check("ac3_3f2r_annex_d_48k.ac3");
}
#[test]
fn ac3_silence() {
    check("ac3_silence_48k.ac3");
}
#[test]
fn eac3_mono() {
    check("eac3_mono_48k.eac3");
}
#[test]
fn eac3_stereo() {
    check("eac3_stereo_48k.eac3");
}
#[test]
fn eac3_stereo_low_rate() {
    check("eac3_stereo_44k_48kbps.eac3");
}
#[test]
fn eac3_3f2r_lfe() {
    check("eac3_3f2r_lfe_48k.eac3");
}
#[test]
fn eac3_mixing_metadata() {
    check("eac3_3f2r_lfe_32k_mix.eac3");
}

const ALL: [&str; 20] = [
    "ac3_mono_48k.ac3",
    "ac3_stereo_44k.ac3",
    "ac3_stereo_32k_48kbps.ac3",
    "ac3_2f1r_48k.ac3",
    "ac3_3f_44k.ac3",
    "ac3_3f1r_32k.ac3",
    "ac3_2f2r_48k.ac3",
    "ac3_3f2r_lfe_48k.ac3",
    "ac3_3f2r_lfe_44k_192k.ac3",
    "ac3_1f_lfe_32k.ac3",
    "ac3_2f_lfe_48k.ac3",
    "ac3_2f1r_lfe_44k.ac3",
    "ac3_3f1r_lfe_48k.ac3",
    "ac3_3f2r_annex_d_48k.ac3",
    "ac3_silence_48k.ac3",
    "eac3_mono_48k.eac3",
    "eac3_stereo_48k.eac3",
    "eac3_stereo_44k_48kbps.eac3",
    "eac3_3f2r_lfe_48k.eac3",
    "eac3_3f2r_lfe_32k_mix.eac3",
];

/// The streams between them use what ffmpeg's encoders can make.
#[test]
fn streams_cover_the_encoders_tools() {
    let mut f = unflash_sound::ac3::Features::default();
    for name in ALL {
        let g = common::ac3::decode(&read(name), Output::Native, true).features;
        f.coupling |= g.coupling;
        f.rematrixing |= g.rematrixing;
        f.dither |= g.dither;
        f.lfe |= g.lfe;
    }
    assert!(f.coupling && f.rematrixing && f.dither && f.lfe, "{f:?}");
}

/// What `info` says of each stream.
#[test]
fn stream_info() {
    let expect = |name: &str, sample_rate: u32, channels: usize, eac3: bool, acmod: u8, lfe: bool| {
        let data = read(name);
        let want = StreamInfo { sample_rate, channels, eac3, acmod, lfe };
        let mut dec = Decoder::new(Output::Native);
        let mut out = Vec::new();
        let d = dec.decode(&data, &mut out).unwrap();
        assert_eq!(dec.info(), Some(want), "{name}");
        assert_eq!(out.len(), channels, "{name}");
        assert!(out.iter().all(|c| c.len() == d.samples));
        assert_eq!(d.samples % 1536, 0);
    };
    expect("ac3_mono_48k.ac3", 48000, 1, false, 1, false);
    expect("ac3_stereo_44k.ac3", 44100, 2, false, 2, false);
    expect("ac3_2f1r_48k.ac3", 48000, 3, false, 4, false);
    expect("ac3_3f_44k.ac3", 44100, 3, false, 3, false);
    expect("ac3_3f1r_32k.ac3", 32000, 4, false, 5, false);
    expect("ac3_2f2r_48k.ac3", 48000, 4, false, 6, false);
    expect("ac3_3f2r_lfe_48k.ac3", 48000, 6, false, 7, true);
    expect("ac3_1f_lfe_32k.ac3", 32000, 2, false, 1, true);
    expect("eac3_stereo_48k.eac3", 48000, 2, true, 2, false);
    expect("eac3_3f2r_lfe_48k.eac3", 48000, 6, true, 7, true);
}

/// Each output's (input channel, gain) pairs.
type Gains = (Vec<(usize, f32)>, Vec<(usize, f32)>);

/// The Lo/Ro gains of §7.8.2 for a coding mode, written out again here
/// (the decoder has its own copy): Lo = L + clev C + slev Ls (a single
/// surround at -3 dB more), scaled so the gains sum to at most 1.
fn lo_ro(acmod: u8, lfe: bool, clev: f32, slev: f32) -> [Vec<(usize, f32)>; 2] {
    // our Native order: L R [C] [LFE] [S | Ls Rs]
    let s3 = std::f32::consts::FRAC_1_SQRT_2;
    let (mut l, mut r): Gains = match acmod {
        1 => (vec![(0, 1.0)], vec![(0, 1.0)]),
        0 | 2 => (vec![(0, 1.0)], vec![(1, 1.0)]),
        _ => (vec![(0, 1.0)], vec![(1, 1.0)]),
    };
    let has_c = acmod & 1 == 1 && acmod > 1;
    if has_c {
        l.push((2, clev));
        r.push((2, clev));
    }
    let s0 = 2 + has_c as usize + lfe as usize;
    match acmod {
        4 | 5 => {
            l.push((s0, slev * s3));
            r.push((s0, slev * s3));
        }
        6 | 7 => {
            l.push((s0, slev));
            r.push((s0 + 1, slev));
        }
        _ => {}
    }
    let sum: f32 = l.iter().map(|g| g.1).sum();
    if sum > 1.0 {
        for g in l.iter_mut().chain(r.iter_mut()) {
            g.1 /= sum;
        }
    }
    [l, r]
}

/// `Output::Stereo` is the Lo/Ro formula applied to `Output::Native`
/// (with the stream's levels; the LFE channel left out).
#[test]
fn stereo_output_is_the_standard_downmix() {
    // (stream, clev, slev): AC-3's cmixlev and surmixlev as ffmpeg writes
    // them by default (-4.5 dB, -6 dB), Annex D's Lo/Ro levels, E-AC-3's
    // default and its mixing metadata
    let cases = [
        ("ac3_mono_48k.ac3", 0.0, 0.0),
        ("ac3_stereo_44k.ac3", 0.0, 0.0),
        ("ac3_2f1r_48k.ac3", 0.0, 0.5),
        ("ac3_3f_44k.ac3", 0.5946, 0.0),
        ("ac3_3f1r_32k.ac3", 0.5946, 0.5),
        ("ac3_2f2r_48k.ac3", 0.0, 0.5),
        ("ac3_3f2r_annex_d_48k.ac3", 1.0, 0.5),
        ("eac3_3f2r_lfe_32k_mix.eac3", 0.5, std::f32::consts::FRAC_1_SQRT_2),
    ];
    for (name, clev, slev) in cases {
        let data = read(name);
        let native = common::ac3::decode(&data, Output::Native, true);
        let stereo = common::ac3::decode(&data, Output::Stereo, true);
        let info = native.info.unwrap();
        assert_eq!(stereo.out.len(), 2);
        let gains = lo_ro(info.acmod, info.lfe, clev, slev);
        for (side, g) in gains.iter().enumerate() {
            for i in 0..native.out[0].len() {
                let want: f32 = g.iter().map(|&(c, k)| k * native.out[c][i]).sum();
                assert!((stereo.out[side][i] - want).abs() < 2e-6, "{name} side {side} sample {i}: {} vs {want}", stereo.out[side][i]);
            }
        }
    }
    // 5.1 with LFE: the LFE (Native channel 3) stays out
    let data = read("ac3_3f2r_lfe_48k.ac3");
    let native = common::ac3::decode(&data, Output::Native, true);
    let stereo = common::ac3::decode(&data, Output::Stereo, true);
    let (c, s) = (0.5946f32, 0.5f32);
    let sum = 1.0 + c + s;
    for i in 0..native.out[0].len() {
        let n = |k: usize| native.out[k][i];
        let lo = (n(0) + c * n(2) + s * n(4)) / sum;
        let ro = (n(1) + c * n(2) + s * n(5)) / sum;
        assert!((stereo.out[0][i] - lo).abs() < 2e-6 && (stereo.out[1][i] - ro).abs() < 2e-6, "sample {i}");
    }
}

/// ffmpeg's own AC-3 downmix (`-downmix stereo`) uses the same levels and
/// scaling; its `-ac 2` (libswresample's matrix: 1, 0.707, 0.707, not
/// scaled, the stream's levels ignored) does not, which is reported only.
#[test]
fn stereo_output_is_close_to_ffmpegs_downmix() {
    if skip("stereo_output_is_close_to_ffmpegs_downmix") {
        return;
    }
    for name in ["ac3_3f1r_32k.ac3", "ac3_2f1r_48k.ac3", "ac3_3f2r_lfe_48k.ac3", "ac3_3f2r_annex_d_48k.ac3", "eac3_3f2r_lfe_48k.eac3", "eac3_3f2r_lfe_32k_mix.eac3"] {
        let data = read(name);
        let noisy = common::ac3::decode(&data, Output::Stereo, true);
        let quiet = common::ac3::decode(&data, Output::Stereo, false);
        let rms = |a: &[f32], b: &[f32]| (a.iter().zip(b).map(|(x, y)| ((x - y) as f64).powi(2)).sum::<f64>() / a.len() as f64).sqrt();
        let path = media(name);
        let own = ffmpeg_decode(&path, &["-downmix", "stereo"], &[]).unwrap();
        let swr = ffmpeg_decode(&path, &[], &["-ac", "2"]).unwrap();
        for (side, swr) in swr.iter().enumerate().take(2) {
            let theirs = &own[side];
            assert_eq!(theirs.len(), quiet.out[side].len(), "{name}");
            // ffmpeg's downmix less ours without noise is ffmpeg's noise,
            // which must be as loud as ours
            let (d_own, noise) = (rms(theirs, &quiet.out[side]), rms(&noisy.out[side], &quiet.out[side]));
            let d_swr = rms(swr, &noisy.out[side]);
            let level = rms(&quiet.out[side], &vec![0.0; theirs.len()]);
            eprintln!("{name} side {side}: level {level:.4}; ffmpeg -downmix stereo less ours without noise: {d_own:.2e} (our noise {noise:.2e}); ffmpeg -ac 2 less ours: {d_swr:.2e}");
            // In coupled bands ffmpeg's dither is correlated between
            // channels (A/52 §7.3.4 dithers each channel after decoupling
            // so that it is not; ours is not), so its noise adds up
            // coherently in a downmix: up to 1.7 times louder here.
            if noise > 1e-6 {
                assert!((0.7..=1.7).contains(&(d_own / noise)), "{name} side {side}: {d_own} from ffmpeg's downmix, noise {noise}");
            } else {
                assert!(d_own < 5e-5, "{name} side {side}: {d_own} from ffmpeg's downmix");
            }
        }
    }
}
