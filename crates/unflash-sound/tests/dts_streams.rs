//! Every stream in tests/media/dts (made by its gen.sh with ffmpeg's
//! encoder) against ffmpeg's decoder, run at test time: every sample of
//! every channel must agree to 1e-4 of full scale (they agree to about
//! 1e-6: ffmpeg's decoder works in fixed point before its filter bank).
//! Without ffmpeg the comparisons are skipped, with a message. Also: what
//! `info` says, the stereo downmix, the other packings of the same
//! frames, and a transport stream's packets.

mod common;

use std::path::PathBuf;

use common::dts::{compare, describe, frames, TOLERANCE};
use common::{ffmpeg_decode, ffmpeg_decode_bytes, skip};
use unflash_sound::dts::{Decoder, Output, StreamInfo};

fn media(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/media/dts").join(name)
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(media(name)).unwrap_or_else(|e| panic!("{name}: {e} (run tests/media/dts/gen.sh)"))
}

/// Decode a stream and compare it with ffmpeg's decode.
fn check(name: &str) {
    if skip(name) {
        return;
    }
    let data = read(name);
    let ours = common::dts::decode(&data, Output::Native);
    let reference = ffmpeg_decode(&media(name), &[], &[]).expect("ffmpeg decodes the stream");
    assert_eq!(ours.out.len(), reference.len(), "{name}: channel count");
    assert_eq!(ours.out[0].len(), reference[0].len(), "{name}: sample count");
    assert_eq!(ours.decoded.damaged, 0, "{name}: damaged frames");
    let map: Vec<Option<usize>> = (0..reference.len()).map(Some).collect();
    for (c, s) in compare(&ours.out, &reference, &map).iter().enumerate() {
        eprintln!("{name} channel {c}: {}", describe(s));
        assert!(s.rms > 1e-3 || name.contains("silence"), "{name} channel {c}: too little to compare");
        assert!(s.max_diff <= TOLERANCE, "{name} channel {c}: differs from ffmpeg's by up to {:e} at sample {}", s.max_diff, s.at);
    }
}

#[test]
fn mono_48k_320k() {
    check("dts_mono_48k_320k.dts");
}
#[test]
fn stereo_44k_384k() {
    check("dts_stereo_44k_384k.dts");
}
#[test]
fn stereo_48k_1411k() {
    check("dts_stereo_48k_1411k.dts");
}
#[test]
fn stereo_32k_448k() {
    check("dts_stereo_32k_448k.dts");
}
#[test]
fn mono_22k_320k() {
    check("dts_mono_22k_320k.dts");
}
#[test]
fn quad_48k_640k() {
    check("dts_quad_48k_640k.dts");
}
#[test]
fn five_0_44k_768k() {
    check("dts_5.0_44k_768k.dts");
}
#[test]
fn five_1_48k_768k() {
    check("dts_5.1_48k_768k.dts");
}
#[test]
fn five_1_44k_1536k() {
    check("dts_5.1_44k_1536k.dts");
}
#[test]
fn stereo_48k_adpcm() {
    check("dts_stereo_48k_adpcm.dts");
}
#[test]
fn five_1_48k_adpcm() {
    check("dts_5.1_48k_adpcm.dts");
}
#[test]
fn silence() {
    check("dts_silence_48k.dts");
}

const ALL: [&str; 12] = [
    "dts_mono_48k_320k.dts",
    "dts_stereo_44k_384k.dts",
    "dts_stereo_48k_1411k.dts",
    "dts_stereo_32k_448k.dts",
    "dts_mono_22k_320k.dts",
    "dts_quad_48k_640k.dts",
    "dts_5.0_44k_768k.dts",
    "dts_5.1_48k_768k.dts",
    "dts_5.1_44k_1536k.dts",
    "dts_stereo_48k_adpcm.dts",
    "dts_5.1_48k_adpcm.dts",
    "dts_silence_48k.dts",
];

/// The streams between them use what ffmpeg's encoder can make.
#[test]
fn streams_cover_the_encoders_tools() {
    let mut f = unflash_sound::dts::Features::default();
    for name in ALL {
        let g = common::dts::decode(&read(name), Output::Native).features;
        f.adpcm |= g.adpcm;
        f.lfe_64 |= g.lfe_64;
        f.huffman_bit_allocation |= g.huffman_bit_allocation;
        f.huffman_samples |= g.huffman_samples;
        f.block_codes |= g.block_codes;
        f.plain_samples |= g.plain_samples;
        f.scales_7bit |= g.scales_7bit;
        f.nonperfect_filter |= g.nonperfect_filter;
        f.subsubframes |= g.subsubframes;
    }
    assert!(
        f.adpcm
            && f.lfe_64
            && f.huffman_bit_allocation
            && f.huffman_samples
            && f.block_codes
            && f.plain_samples
            && f.scales_7bit
            && f.nonperfect_filter
            && f.subsubframes,
        "{f:?}"
    );
}

/// What `info` says of each stream.
#[test]
fn stream_info() {
    let expect = |name: &str, sample_rate: u32, channels: usize, amode: u8, lfe: bool| {
        let data = read(name);
        let want = StreamInfo { sample_rate, channels, amode, lfe, frame_samples: 512 };
        let mut dec = Decoder::new(Output::Native);
        let mut out = Vec::new();
        let d = dec.decode(&data, &mut out).unwrap();
        assert_eq!(dec.info(), Some(want), "{name}");
        assert_eq!(out.len(), channels, "{name}");
        assert!(out.iter().all(|c| c.len() == d.samples));
        assert_eq!(d.samples, 512 * frames(&data).len());
    };
    expect("dts_mono_48k_320k.dts", 48000, 1, 0, false);
    expect("dts_stereo_44k_384k.dts", 44100, 2, 2, false);
    expect("dts_stereo_32k_448k.dts", 32000, 2, 2, false);
    expect("dts_mono_22k_320k.dts", 22050, 1, 0, false);
    expect("dts_quad_48k_640k.dts", 48000, 4, 8, false);
    expect("dts_5.0_44k_768k.dts", 44100, 5, 9, false);
    expect("dts_5.1_48k_768k.dts", 48000, 6, 9, true);
}

/// Gains of (channel, gain) into one output.
type Taps = Vec<(usize, f32)>;
/// Each output's (Native channel, gain) pairs.
type Gains = [Taps; 2];

/// The default Lo/Ro, written out again here (the decoder has its own
/// copy): Lo = L + a C + a Ls, Ro = R + a C + a Rs with a = -3 dB (a
/// single surround a² into both), scaled so the gains sum to at most 1;
/// the LFE left out.
fn lo_ro(amode: u8, lfe: bool) -> Gains {
    let a = std::f32::consts::FRAC_1_SQRT_2;
    // our Native order: L R [C] [LFE] [S | Ls Rs]
    let has_c = matches!(amode, 5 | 7 | 9);
    let s0 = 2 + has_c as usize + lfe as usize;
    let (mut l, mut r): (Taps, Taps) = match amode {
        0 => (vec![(0, 1.0)], vec![(0, 1.0)]),
        _ => (vec![(0, 1.0)], vec![(1, 1.0)]),
    };
    if has_c {
        l.push((2, a));
        r.push((2, a));
    }
    match amode {
        6 | 7 => {
            l.push((s0, 0.5));
            r.push((s0, 0.5));
        }
        8 | 9 => {
            l.push((s0, a));
            r.push((s0 + 1, a));
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

/// Without embedded coefficients (ffmpeg's encoder writes none),
/// `Output::Stereo` is the default Lo/Ro applied to `Output::Native`.
#[test]
fn stereo_output_is_the_default_downmix() {
    for name in
        ["dts_mono_48k_320k.dts", "dts_stereo_44k_384k.dts", "dts_quad_48k_640k.dts", "dts_5.0_44k_768k.dts", "dts_5.1_48k_768k.dts", "dts_5.1_48k_adpcm.dts"]
    {
        let data = read(name);
        let native = common::dts::decode(&data, Output::Native);
        let stereo = common::dts::decode(&data, Output::Stereo);
        assert!(!stereo.features.embedded_downmix);
        let info = native.info.unwrap();
        assert_eq!(stereo.out.len(), 2);
        let gains = lo_ro(info.amode, info.lfe);
        for (side, g) in gains.iter().enumerate() {
            for i in 0..native.out[0].len() {
                let want: f32 = g.iter().map(|&(c, k)| k * native.out[c][i]).sum();
                assert!((stereo.out[side][i] - want).abs() < 2e-6, "{name} side {side} sample {i}: {} vs {want}", stereo.out[side][i]);
            }
        }
    }
}

/// `data`'s frames in the other packings: 16-bit words byte-swapped, and
/// 14 bits a word (the top two bits repeating bit 13), either way round.
fn repack(data: &[u8], fourteen: bool, little: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for f in frames(data) {
        let frame = &data[f];
        let words: Vec<u16> = if fourteen {
            let bits: Vec<u8> = frame.iter().flat_map(|b| (0..8).rev().map(move |k| (b >> k) & 1)).collect();
            bits.chunks(14)
                .map(|c| {
                    let w = c.iter().fold(0u16, |a, &b| a << 1 | b as u16) << (14 - c.len());
                    if w & 0x2000 != 0 {
                        w | 0xc000
                    } else {
                        w
                    }
                })
                .collect()
        } else {
            let mut f = frame.to_vec();
            if f.len() % 2 == 1 {
                f.push(0);
            }
            f.chunks(2).map(|w| u16::from_be_bytes([w[0], w[1]])).collect()
        };
        for w in words {
            out.extend_from_slice(&if little { w.to_le_bytes() } else { w.to_be_bytes() });
        }
    }
    out
}

/// The same frames in 16-bit words byte-swapped, and in 14-bit words
/// either way round (as CD and WAV rips carry DTS), decode to the same
/// samples; ffmpeg reads them alike.
#[test]
fn other_packings_decode_alike() {
    for name in ["dts_stereo_44k_384k.dts", "dts_5.1_48k_768k.dts", "dts_mono_22k_320k.dts"] {
        let data = read(name);
        let want = common::dts::decode(&data, Output::Native);
        for (fourteen, little) in [(false, true), (true, false), (true, true)] {
            let packed = repack(&data, fourteen, little);
            let got = common::dts::decode(&packed, Output::Native);
            assert_eq!(got.decoded, want.decoded, "{name} 14-bit {fourteen} little endian {little}");
            assert_eq!(got.out, want.out, "{name} 14-bit {fourteen} little endian {little}");
            assert_eq!(got.info, want.info, "{name} 14-bit {fourteen} little endian {little}");
            if !skip("other_packings_decode_alike against ffmpeg") {
                let reference = ffmpeg_decode_bytes(&packed, &format!("packed-{fourteen}-{little}.dts"), &["-f", "dts"], &[]).expect("ffmpeg decodes it");
                let map: Vec<Option<usize>> = (0..reference.len()).map(Some).collect();
                for (c, s) in compare(&got.out, &reference, &map).iter().enumerate() {
                    assert!(
                        s.samples == got.out[0].len() && s.max_diff <= TOLERANCE,
                        "{name} 14-bit {fourteen} little endian {little} channel {c}: {}",
                        describe(s)
                    );
                }
            }
        }
    }
}

/// A stream muxed into MPEG-TS by ffmpeg, decoded a packet (PES
/// payload) at a time as a demuxer hands them over, is the stream.
#[test]
fn transport_stream_packets() {
    if skip("transport_stream_packets") {
        return;
    }
    for name in ["dts_5.1_48k_768k.dts", "dts_stereo_44k_384k.dts"] {
        let path = std::env::temp_dir().join(format!("unflash-sound-{}-{name}.ts", std::process::id()));
        let ok = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y", "-i"])
            .arg(media(name))
            .args(["-c", "copy", "-f", "mpegts"])
            .arg(&path)
            .status()
            .unwrap()
            .success();
        assert!(ok, "ffmpeg muxes {name}");
        let ts = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let packets = common::dts::ts_payloads(&ts);
        assert!(packets.len() > 1, "{name}");
        let mut dec = Decoder::new(Output::Native);
        let mut out = Vec::new();
        for p in &packets {
            let d = dec.decode(p, &mut out).unwrap();
            assert_eq!(d.damaged, 0, "{name}");
        }
        let want = common::dts::decode(&read(name), Output::Native);
        assert_eq!(out, want.out, "{name}");
    }
}
