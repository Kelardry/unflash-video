//! The transport streams `tests/media/gen.sh` made, against ffmpeg's MP4 of
//! each (`-c copy`): every sample the demuxer finds, read as the app reads
//! it (`ts::read_sample`), must be the MP4's sample byte for byte, with the
//! same time and key frame flag. Blu-ray LPCM, which an MP4 can't hold, is
//! checked against ffprobe's packets.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use unflash_mp4::demux::{parse_bytes, Movie, Track, TrackKind};
use unflash_mp4::ts::read_sample;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/media/ts")
}

fn streams() -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir()) else { return vec![] };
    let mut v: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| matches!(p.extension().and_then(|x| x.to_str()), Some("ts" | "m2ts"))).collect();
    v.sort();
    v
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", p.display()))
}

fn first(m: &Movie, kind: TrackKind) -> Option<&Track> {
    m.tracks.iter().find(|t| t.kind == kind && !t.samples.is_empty())
}

fn us(t: &Track, ticks: i64) -> f64 {
    ticks as f64 * 1e6 / t.timescale as f64
}

/// A length-prefixed sample with its NAL units' trailing zero bytes left
/// out, as the demuxer leaves them out (ffmpeg's HEVC keeps one: the
/// first byte of the next start code).
fn without_trailing_zeros(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= b.len() {
        let n = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
        let mut nal = &b[i + 4..i + 4 + n];
        while let [rest @ .., 0] = nal {
            nal = rest;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
        i += 4 + n;
    }
    out
}

#[test]
fn transport_streams_match_ffmpegs_mp4() {
    let files = streams();
    if files.is_empty() {
        eprintln!("no transport streams; run tests/media/gen.sh");
        return;
    }
    let mut checked = 0;
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let mp4_path = with_suffix(&path, ".mp4");
        if !mp4_path.exists() {
            continue;
        }
        let ts = std::fs::read(&path).unwrap();
        let mp4 = std::fs::read(&mp4_path).unwrap();
        let ours = parse_bytes(&ts).unwrap_or_else(|e| panic!("{name}: {e}"));
        let theirs = parse_bytes(&mp4).unwrap();
        assert_eq!(ours.format, "mpegts");
        assert_eq!(ours.packet_size, if name.ends_with(".m2ts") { 192 } else { 188 }, "{name}");
        for kind in [TrackKind::Video, TrackKind::Audio] {
            let a = first(&ours, kind).unwrap_or_else(|| panic!("{name}: no {kind:?}"));
            let b = first(&theirs, kind).unwrap();
            let what = format!("{name} {kind:?} ({})", a.codec);
            // (MPEG audio of layer II: ffmpeg's MP4 names it mp3)
            if !(a.codec == "mp4a.6B" && b.codec == "mp3") {
                assert_eq!(a.codec.split_once('.').map(|x| x.1).unwrap_or(&a.codec), b.codec.split_once('.').map(|x| x.1).unwrap_or(&b.codec), "{what}: codec");
            }
            if kind == TrackKind::Video {
                assert_eq!((a.width, a.height), (b.width, b.height), "{what}: size");
            } else {
                assert_eq!((a.sample_rate, a.channels), (b.sample_rate, b.channels), "{what}: rate and channels");
            }
            assert_eq!(a.samples.len(), b.samples.len(), "{what}: samples");
            let video = kind == TrackKind::Video;
            let (a0, b0) = (us(a, a.samples[0].pts), us(b, b.samples[0].pts));
            for (i, (x, y)) in a.samples.iter().zip(&b.samples).enumerate() {
                let got = read_sample(&ts, ours.packet_size as u64, a.id as u16, video, x.offset, x.size).unwrap_or_else(|e| panic!("{what} sample {i}: {e}"));
                let mut want = mp4[y.offset as usize..(y.offset + y.size as u64) as usize].to_vec();
                if video {
                    want = without_trailing_zeros(&want);
                }
                assert_eq!(x.size as usize, want.len(), "{what} sample {i}: size");
                assert!(got == want, "{what} sample {i}: bytes");
                assert_eq!(x.sync, y.sync, "{what} sample {i}: key frame");
                let (dt_a, dt_b) = (us(a, x.pts) - a0, us(b, y.pts) - b0);
                assert!((dt_a - dt_b).abs() < 30.0, "{what} sample {i}: time {dt_a} µs vs {dt_b} µs");
            }
            checked += 1;
        }
    }
    assert!(checked >= 10, "{checked} tracks checked");
}

#[derive(Deserialize)]
struct Packets {
    packets: Vec<Packet>,
}

#[derive(Deserialize)]
struct Packet {
    size: String,
}

#[test]
fn blu_ray_lpcm() {
    let path = dir().join("h264_lpcm.m2ts");
    let Ok(ts) = std::fs::read(&path) else {
        eprintln!("no transport streams; run tests/media/gen.sh");
        return;
    };
    let m = parse_bytes(&ts).unwrap();
    let a = first(&m, TrackKind::Audio).unwrap();
    assert_eq!((a.codec.as_str(), a.sample_rate, a.channels), ("pcm-s16be", 48000, 2));
    let listing: Packets = serde_json::from_str(&std::fs::read_to_string(with_suffix(&path, ".audio.json")).unwrap()).unwrap();
    assert_eq!(a.samples.len(), listing.packets.len());
    for (i, (s, p)) in a.samples.iter().zip(&listing.packets).enumerate() {
        // a PES each: its 4-byte header left out
        assert_eq!(s.size + 4, p.size.parse::<u32>().unwrap(), "sample {i}");
        assert_eq!(s.duration * 4, s.size, "sample {i}: 16-bit stereo");
        let got = read_sample(&ts, 192, a.id as u16, false, s.offset, s.size).unwrap();
        assert_eq!(got.len(), s.size as usize);
    }
    let seconds: f64 = a.samples.iter().map(|s| s.duration as f64).sum::<f64>() / 48000.0;
    assert!((seconds - 2.0).abs() < 0.05, "{seconds} s of sound");
}
