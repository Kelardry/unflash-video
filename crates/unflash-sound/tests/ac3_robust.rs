//! Damaged and unusual input, and the API's promises: damaged frames are
//! silence of their length and are counted, the decoder carries on, and
//! nothing in any input makes it panic (a panic aborts the WebAssembly
//! instance). Also: substreams it does not play are skipped, a layout
//! change resizes the output, `reset` forgets the overlap, the reduced
//! sample rates, and the Matroska files the app's tests use. (What DTS's
//! tests check alike is written once, in `common`.)

mod common;

use std::path::PathBuf;

use common::ac3::{fix_crcs, frames};
use common::{assert_whole, rms, Lcg};
use unflash_sound::ac3::testing::frame_bytes;
use unflash_sound::ac3::{Decoded, Decoder, Error, Output};

fn media(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/media").join(name)
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(media(&format!("ac3/{name}"))).unwrap_or_else(|e| panic!("{name}: {e} (run tests/media/ac3/gen.sh)"))
}

fn decode(data: &[u8], noise: bool) -> (Result<Decoded, Error>, Vec<Vec<f32>>) {
    common::decode_all::<Decoder>(data, noise)
}

const STREAMS: [&str; 8] = [
    "ac3_stereo_44k.ac3",
    "ac3_3f2r_lfe_48k.ac3",
    "ac3_1f_lfe_32k.ac3",
    "ac3_2f1r_lfe_44k.ac3",
    "ac3_3f2r_annex_d_48k.ac3",
    "eac3_stereo_44k_48kbps.eac3",
    "eac3_3f2r_lfe_32k_mix.eac3",
    "eac3_mono_48k.eac3",
];

#[test]
fn a_frame_with_a_flipped_bit_is_silence_of_its_length() {
    for name in STREAMS {
        let data = read(name);
        common::damaged_frame_is_silence::<Decoder>(
            name,
            &data,
            &frames(&data),
            // a bit in the middle of the frame (its audio, not its sync word)
            |bad, f, _| bad[(f.start + f.end) / 2] ^= 0x10,
            // the next frame starts from silence (no overlap from the
            // damaged frame), its second block on is as before
            |_| Some(256),
        );
    }
}

#[test]
fn a_stream_cut_mid_frame_ends_with_a_damaged_frame() {
    for name in STREAMS {
        let data = read(name);
        // (frame 3 cut after its first 7 bytes too, frame 2 within its
        // first 3)
        common::cut_mid_frame::<Decoder>(name, &data, &frames(&data), 7, 3);
    }
}

#[test]
fn nothing_to_decode() {
    assert_eq!(decode(&[], true).0, Err(Error::NoSync));
    assert_eq!(decode(&[0x0b, 0x77], true).0, Err(Error::NoSync));
    common::random_bytes::<Decoder>(&mut Lcg(7), Error::NoSync);
    // a frame of a substream this decoder does not play is not enough
    let data = read("eac3_mono_48k.eac3");
    let mut other = data[frames(&data)[0].clone()].to_vec();
    other[2] = (other[2] & 0xc7) | (1 << 3); // substreamid 1
    fix_crcs(&mut other);
    assert_eq!(decode(&other, true).0, Err(Error::NoSync));
}

/// Mutations of frames (`common::mutated_piece`) whose CRCs are then made
/// to check again, so that the parser sees the damage: no panics, and a
/// frame either decodes or is silence of its length. The frame after the
/// mutated one is decoded too: what a damaged frame leaves behind must not
/// break the next one.
#[test]
fn mutated_frames_do_not_panic() {
    let mut rng = Lcg(1);
    let mut rounds = 0;
    for name in STREAMS {
        let data = read(name);
        let fr = frames(&data);
        for _ in 0..500 {
            // (the header's bytes: bsid, acmod, sample rate, stream type, ...)
            let piece = common::mutated_piece(&mut rng, &data, &fr, 2..10, |piece, f0| {
                // keep the frame's length as its header now says
                if let Some(size) = frame_bytes(&piece[f0..]) {
                    if f0 + size <= piece.len() && size >= 8 {
                        piece.truncate(f0 + size);
                        fix_crcs(&mut piece[f0..]);
                    }
                }
            });
            for noise in [true, false] {
                for output in [Output::Native, Output::Stereo] {
                    let mut dec = Decoder::new(output);
                    dec.set_noise(noise);
                    let mut out = Vec::new();
                    let d = dec.decode(&piece, &mut out).ok();
                    assert_whole(name, d, &out);
                }
            }
            rounds += 1;
        }
    }
    assert_eq!(rounds, 4000);
}

/// Truncations, insertions and deletions anywhere in a stream, fed in
/// random pieces (`common::damaged_stream`).
#[test]
fn damaged_streams_do_not_panic() {
    let mut rng = Lcg(3);
    for name in STREAMS {
        let data = read(name);
        for _ in 0..60 {
            common::damaged_stream::<Decoder>(&mut rng, name, &data);
        }
    }
}

/// Frames of dependent substreams, and of independent substreams other
/// than 0, are skipped: the output is that of substream 0 alone.
#[test]
fn other_substreams_are_skipped() {
    let main = read("eac3_stereo_48k.eac3");
    let other = read("eac3_3f2r_lfe_48k.eac3");
    let (want, want_out) = decode(&main, false);
    let mf = frames(&main);
    let of = frames(&other);
    let mut mixed = Vec::new();
    let mut extra = 0;
    for (i, m) in mf.iter().enumerate() {
        mixed.extend_from_slice(&main[m.clone()]);
        let o = &of[i % of.len()];
        // an independent substream 1 (same syntax), and a dependent one
        // (strmtyp 1: its syntax differs, but it is only skipped)
        for header in [0b00_001u8, 0b01_000] {
            let mut f = other[o.clone()].to_vec();
            f[2] = (f[2] & 0x07) | header << 3;
            fix_crcs(&mut f);
            mixed.extend_from_slice(&f);
            extra += 1;
        }
    }
    let mut dec = Decoder::new(Output::Native);
    dec.set_noise(false);
    let mut out = Vec::new();
    let got = dec.decode(&mixed, &mut out).unwrap();
    assert_eq!(got, want.unwrap());
    assert_eq!(out, want_out);
    assert_eq!(dec.features().1, extra);
}

/// A stream whose layout changes (stereo, 5.1, then mono in a later call):
/// `out` follows the latest frame's layout (`common::layout_change`).
#[test]
fn a_layout_change_resizes_the_output() {
    common::layout_change::<Decoder>(&read("eac3_stereo_48k.eac3"), &read("ac3_3f2r_lfe_48k.ac3"), &read("ac3_mono_48k.ac3"), 48000);
}

/// Decoding frame by frame gives what decoding all at once gives.
#[test]
fn frame_by_frame() {
    for name in STREAMS {
        let data = read(name);
        common::frame_by_frame::<Decoder>(name, &data, &frames(&data));
    }
}

/// E-AC-3's reduced sample rates (fscod 3 with fscod2: 24, 22.05, 16
/// kHz) are the full rates' syntax at half the rate. Rewriting the rate
/// codes of streams at 48, 44.1 and 32 kHz gives streams of the reduced
/// rates that decode to the same samples, at half the rate. (No reference
/// decodes these: ffmpeg 6.1 says "Reduced sampling rate is not
/// implemented". That the bit allocation's hearing threshold is fscod2's
/// table is this decoder's reading; the standard does not say.)
#[test]
fn reduced_sample_rates() {
    for (name, fscod2, rate) in [("eac3_stereo_48k.eac3", 0u8, 24000), ("eac3_stereo_44k_48kbps.eac3", 1, 22050), ("eac3_3f2r_lfe_32k_mix.eac3", 2, 16000)] {
        let data = read(name);
        let mut half = data.clone();
        for f in frames(&data) {
            let frame = &mut half[f];
            assert_eq!(frame[4] >> 4 & 3, 3, "six blocks per frame");
            // fscod 3, then fscod2 where numblkscod was
            frame[4] = (frame[4] & 0x0f) | 0xc0 | fscod2 << 4;
            fix_crcs(frame);
        }
        let (full, full_out) = decode(&data, false);
        let mut dec = Decoder::new(Output::Native);
        dec.set_noise(false);
        let mut out = Vec::new();
        let r = dec.decode(&half, &mut out).unwrap();
        assert_eq!(r, full.unwrap(), "{name}");
        assert_eq!(dec.info().unwrap().sample_rate, rate);
        assert_eq!(out, full_out, "{name}");
    }
}

/// The app's Matroska test files: blocks as the demuxer hands them over
/// (with the bytes header stripping left out put back), against ffmpeg's
/// decode of the file.
#[test]
fn matroska_blocks() {
    for (file, eac3) in [("mkv/h264_ac3.mkv", false), ("mkv/h264_eac3.mkv", true)] {
        let Ok(data) = std::fs::read(media(file)) else {
            eprintln!("SKIPPED matroska_blocks for {file}: not there (tests/media/gen.sh makes it)");
            continue;
        };
        let movie = unflash_mp4::demux::parse_bytes(&data).unwrap();
        let track = movie.audio().expect("an audio track");
        let mut dec = Decoder::new(Output::Native);
        dec.set_noise(false);
        dec.set_trace(true);
        let mut quiet = Vec::new();
        let mut noisy_dec = Decoder::new(Output::Native);
        noisy_dec.set_trace(true);
        let mut noisy = Vec::new();
        for s in &track.samples {
            let mut block = track.prefix.clone();
            block.extend_from_slice(&data[s.offset as usize..s.offset as usize + s.size as usize]);
            let d = dec.decode(&block, &mut quiet).unwrap();
            assert_eq!(d.damaged, 0, "{file}");
            noisy_dec.decode(&block, &mut noisy).unwrap();
        }
        let info = dec.info().unwrap();
        assert_eq!((info.eac3, info.sample_rate, info.channels), (eac3, track.sample_rate, track.channels as usize), "{file}");
        if !common::ffmpeg_available() {
            eprintln!("SKIPPED the comparison with ffmpeg for {file}: ffmpeg is not installed");
            continue;
        }
        let reference = common::ffmpeg_decode(&media(file), &[], &["-map", "0:a"]).expect("ffmpeg decodes the file");
        assert_eq!(reference[0].len(), quiet[0].len(), "{file}: length");
        let trace = noisy_dec.take_trace();
        let map: Vec<Option<usize>> = (0..reference.len()).map(Some).collect();
        for (c, s) in common::ac3::compare(&quiet, &noisy, &trace, &reference, &map).iter().enumerate() {
            eprintln!("{file} channel {c}: {}", common::ac3::describe(s));
            assert!(s.bins > 1000 && s.over == 0, "{file} channel {c}");
        }
    }
}

/// 1+1 dual mono, which ffmpeg's encoder does not make: frames built
/// here, with dither in Ch1 only and a dynamic range word for Ch2.
#[test]
fn dual_mono() {
    let data = common::ac3::dual_mono_frames(20, 10, 0x40);
    let (r, out) = decode(&data, true);
    let r = r.unwrap();
    assert_eq!((r.samples, r.damaged), (20 * 1536, 0));
    assert_eq!(out.len(), 2);
    assert!(out[1].iter().all(|&v| v == 0.0), "Ch2 is not dithered");
    let level = rms(&out[0]);
    assert!(level > 1e-5, "Ch1 is dithered: {level}");
    // Stereo output: Ch1 left, Ch2 right
    let mut dec = Decoder::new(Output::Stereo);
    let mut stereo = Vec::new();
    dec.decode(&data, &mut stereo).unwrap();
    assert_eq!(stereo, out);
    assert_eq!(dec.info(), Some(unflash_sound::ac3::StreamInfo { sample_rate: 48000, channels: 2, eac3: false, acmod: 0, lfe: false }));
    let mut dec = Decoder::new(Output::Native);
    dec.set_noise(false);
    let mut quiet = Vec::new();
    dec.decode(&data, &mut quiet).unwrap();
    assert!(quiet.iter().flatten().all(|&v| v == 0.0));
    if common::ffmpeg_available() {
        let reference = common::ffmpeg_decode_bytes(&data, "dual.ac3", &[], &[]).expect("ffmpeg decodes dual mono");
        assert_eq!(reference.len(), 2);
        assert_eq!(reference[0].len(), out[0].len());
        assert!(reference[1].iter().all(|&v| v == 0.0), "ffmpeg's Ch2");
        let ratio = rms(&reference[0]) / level;
        eprintln!("dual mono Ch1 dither: ffmpeg's is {ratio:.3} times ours ({level:.2e})");
        assert!((0.9..=1.1).contains(&ratio), "{ratio}");
    }
}

/// E-AC-3 frames of one, two and three blocks, which ffmpeg's encoder does
/// not make: 256 samples a block, and noise as loud as ffmpeg's.
#[test]
fn eac3_frames_of_one_two_and_three_blocks() {
    for blocks in [1, 2, 3, 6] {
        let data = common::ac3::eac3_frames(40, blocks, 9);
        let (r, out) = decode(&data, true);
        let r = r.unwrap();
        assert_eq!((r.samples, r.damaged), (40 * 256 * blocks, 0), "{blocks} blocks");
        let level = rms(&out[0]);
        assert!(level > 1e-5, "{blocks} blocks: dithered");
        if common::ffmpeg_available() {
            let reference = common::ffmpeg_decode_bytes(&data, &format!("{blocks}-blocks.eac3"), &[], &[]).expect("ffmpeg decodes it");
            assert_eq!(reference[0].len(), out[0].len(), "{blocks} blocks");
            for c in 0..2 {
                let ratio = rms(&reference[c]) / rms(&out[c]);
                eprintln!("{blocks} blocks, channel {c}: ffmpeg's dither is {ratio:.3} times ours");
                assert!((0.9..=1.1).contains(&ratio), "{blocks} blocks: {ratio}");
            }
        }
    }
}
