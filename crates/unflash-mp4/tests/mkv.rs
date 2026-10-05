//! Demux the Matroska / WebM files `tests/media/gen.sh` made and compare
//! every packet with ffprobe's view of it: its bytes (an adler32), size,
//! presentation time and keyframe flag; then copy video and audio into an
//! MP4 under the sample entries the demuxer built and have ffmpeg decode
//! the result.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use unflash_mp4::demux::{parse_bytes, Movie, TrackKind};
use unflash_mp4::mux::{Muxer, TrackDesc};
use unflash_mp4::Track;

#[derive(Deserialize)]
struct Packets {
    packets: Vec<Packet>,
}

#[derive(Deserialize)]
struct Packet {
    pts_time: Option<String>,
    flags: String,
    size: String,
    data_hash: String,
}

fn media_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/media/mkv")
}

fn files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(media_dir()) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().map(|x| x == "mkv" || x == "webm").unwrap_or(false)).collect(),
        Err(_) => vec![],
    };
    v.sort();
    v
}

fn adler32(b: &[u8]) -> u32 {
    let (mut a, mut s) = (1u32, 0u32);
    for &x in b {
        a = (a + x as u32) % 65521;
        s = (s + a) % 65521;
    }
    (s << 16) | a
}

fn parse(data: &[u8], chunk: Option<u64>) -> Movie {
    let mut d = match chunk {
        // the sniffing front end, as the app uses it
        None => return parse_bytes(data).unwrap(),
        Some(c) => unflash_mp4::mkv::MkvDemuxer::with_chunk(data.len() as u64, c),
    };
    while let Some((off, len)) = d.need() {
        let end = (off + len).min(data.len() as u64);
        d.feed(off, &data[off as usize..end as usize]).unwrap();
    }
    d.into_movie().expect("a movie")
}

fn sample_bytes(data: &[u8], t: &Track, i: usize) -> Vec<u8> {
    let s = &t.samples[i];
    [&t.prefix[..], &data[s.offset as usize..(s.offset + s.size as u64) as usize]].concat()
}

fn check(name: &str, data: &[u8], t: &Track, listing: &Path) {
    let pk: Packets = serde_json::from_str(&std::fs::read_to_string(listing).unwrap()).unwrap();
    assert_eq!(t.samples.len(), pk.packets.len(), "{name} {:?}: packet count", t.kind);
    for (i, (s, p)) in t.samples.iter().zip(&pk.packets).enumerate() {
        let b = sample_bytes(data, t, i);
        assert_eq!(b.len() as u64, p.size.parse::<u64>().unwrap(), "{name} {:?} packet {i}: size", t.kind);
        assert_eq!(format!("adler32:{:08x}", adler32(&b)), p.data_hash, "{name} {:?} packet {i}: bytes", t.kind);
        let pts: f64 = p.pts_time.as_deref().unwrap_or("0").parse().unwrap();
        let ours = t.to_secs(s.pts);
        // Matroska keeps milliseconds; audio is put on its sample clock
        assert!((ours - pts).abs() < 1.5e-3, "{name} {:?} packet {i}: pts {ours} vs {pts}", t.kind);
        if t.kind == TrackKind::Video {
            assert_eq!(s.sync, p.flags.contains('K'), "{name} packet {i}: keyframe flag {}", p.flags);
        }
        assert!(s.dts <= s.pts, "{name} packet {i}: decodes after it is shown");
    }
}

fn has(cmd: &str) -> bool {
    Command::new(cmd).arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

#[test]
fn matroska_matches_ffmpeg() {
    let files = files();
    if files.is_empty() {
        eprintln!("no Matroska media; run tests/media/gen.sh");
        return;
    }
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let stem = f.file_stem().unwrap().to_string_lossy().to_string();
        let data = std::fs::read(&f).unwrap();
        let movie = parse(&data, None);
        assert_eq!(movie.format, if name.ends_with(".webm") { "webm" } else { "matroska" }, "{name}");
        let v = movie.video().unwrap_or_else(|| panic!("{name}: no video"));
        eprintln!(
            "{name}: {} {}x{} {} samples{}",
            v.codec,
            v.width,
            v.height,
            v.samples.len(),
            movie.audio().map(|a| format!(" + {} {} Hz {} ch, {} packets, entry {}", a.codec, a.sample_rate, a.channels, a.samples.len(), String::from_utf8_lossy(a.sample_entry.get(4..8).unwrap_or(b"none")))).unwrap_or_default()
        );
        assert_eq!((v.width, v.height), (64, 48), "{name}");
        let dir = media_dir();
        check(&name, &data, v, &dir.join(format!("{stem}.video.json")));
        let listing = dir.join(format!("{stem}.audio.json"));
        if listing.exists() {
            let a = movie.audio().unwrap_or_else(|| panic!("{name}: no audio"));
            check(&name, &data, a, &listing);
        }
        let expect = [
            ("h264", "avc1."),
            ("hevc", "hvc1."),
            ("vp9", "vp09.00."),
            ("vp8", "vp8"),
            ("av1", "av01."),
            ("live", "avc1."),
        ];
        let prefix = expect.iter().find(|(k, _)| stem.starts_with(k)).map(|e| e.1).unwrap();
        assert!(v.codec.starts_with(prefix), "{name}: codec {}", v.codec);
        // the same parse in small reads
        let again = parse(&data, Some(333));
        assert_eq!(again.tracks.len(), movie.tracks.len());
        for (x, y) in again.tracks.iter().zip(&movie.tracks) {
            assert_eq!(x.samples, y.samples, "{name}: small reads");
        }
        remux(&name, &data, &movie);
    }
}

/// Video and audio copied into an MP4 under the demuxer's sample entries
/// must decode without complaint, every frame there.
fn remux(name: &str, data: &[u8], movie: &Movie) {
    let v = movie.video().unwrap();
    let mut mx = Muxer::new();
    let vt = mx.add_track(TrackDesc::Copy { kind: TrackKind::Video, sample_entry: v.sample_entry.clone(), timescale: v.timescale, width: v.width, height: v.height });
    let audio = movie.audio().filter(|a| !a.sample_entry.is_empty());
    let at = audio.map(|a| mx.add_track(TrackDesc::Copy { kind: TrackKind::Audio, sample_entry: a.sample_entry.clone(), timescale: a.timescale, width: 0, height: 0 }));
    let mut out = mx.start();
    for (i, s) in v.samples.iter().enumerate() {
        let b = sample_bytes(data, v, i);
        out.extend_from_slice(&b);
        mx.add_sample(vt, s.dts, s.pts, s.duration, s.sync, b.len() as u32).unwrap();
    }
    if let (Some(a), Some(at)) = (audio, at) {
        for (i, s) in a.samples.iter().enumerate() {
            let b = sample_bytes(data, a, i);
            out.extend_from_slice(&b);
            mx.add_sample(at, s.dts, s.pts, s.duration, true, b.len() as u32).unwrap();
        }
    }
    let (moov, (patch_at, patch)) = mx.finish().unwrap();
    out[patch_at as usize..patch_at as usize + 8].copy_from_slice(&patch);
    out.extend_from_slice(&moov);
    let back = parse_bytes(&out).unwrap_or_else(|e| panic!("{name}: our MP4 does not parse: {e}"));
    assert_eq!(back.video().unwrap().codec, v.codec, "{name}: video codec after the copy");
    if let Some(a) = audio {
        assert_eq!(back.audio().unwrap().codec, a.codec.replace("mp4a.6B", "mp3"), "{name}: audio codec after the copy");
    }
    if !(has("ffmpeg") && has("ffprobe")) {
        eprintln!("ffmpeg not available; skipped the decode check");
        return;
    }
    let tmp = std::env::temp_dir().join(format!("unflash_mkv_{name}.mp4"));
    std::fs::write(&tmp, &out).unwrap();
    let dec = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&tmp).args(["-f", "null", "-"]).output().unwrap();
    let err = String::from_utf8_lossy(&dec.stderr);
    assert!(dec.status.success() && err.trim().is_empty(), "{name}: ffmpeg complained about the copy: {err}");
    let probe = Command::new("ffprobe").args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"]).arg(&tmp).output().unwrap();
    let frames: usize = String::from_utf8_lossy(&probe.stdout).trim().parse().unwrap();
    assert_eq!(frames, v.samples.len(), "{name}: frames decoded from the copy");
    if let Some(a) = audio {
        let probe = Command::new("ffprobe").args(["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=codec_name,sample_rate,duration", "-of", "csv=p=0"]).arg(&tmp).output().unwrap();
        let s = String::from_utf8_lossy(&probe.stdout).trim().to_string();
        eprintln!("  copied audio: {s} (source {} s)", a.duration_secs());
        let dur: f64 = s.split(',').nth(2).and_then(|d| d.parse().ok()).unwrap_or(0.0);
        assert!((dur - a.duration_secs()).abs() < 0.05, "{name}: audio duration {dur} vs {}", a.duration_secs());
    }
}
