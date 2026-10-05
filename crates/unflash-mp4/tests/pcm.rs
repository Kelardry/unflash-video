//! Uncompressed sound in the MOV and MP4 files `tests/media/gen.sh` made:
//! the demuxer's packets must hold the bytes ffmpeg reads out of each file
//! (`.pcm`), in order, and last as long, under the codec string its form
//! goes by.

use std::path::PathBuf;

use unflash_mp4::demux::{parse_bytes, TrackKind};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/media/pcm")
}

#[test]
fn pcm_packets_hold_the_sound() {
    let Ok(rd) = std::fs::read_dir(dir()) else {
        eprintln!("no PCM media; run tests/media/gen.sh");
        return;
    };
    let mut files: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| matches!(p.extension().and_then(|x| x.to_str()), Some("mov" | "mp4"))).collect();
    files.sort();
    assert!(files.len() >= 16, "the PCM media: {files:?}");
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let file = std::fs::read(&path).unwrap();
        let movie = parse_bytes(&file).unwrap_or_else(|e| panic!("{name}: {e}"));
        let t = movie.tracks.iter().find(|t| t.kind == TrackKind::Audio).unwrap_or_else(|| panic!("{name}: no sound"));
        // the codec string for the form: pcm_s16le -> pcm-s16, pcm_s16be -> pcm-s16be, mulaw -> ulaw
        let form = name.split('.').next().unwrap().split('_').nth(1).unwrap();
        let want = match form {
            "mulaw" => "ulaw".to_string(),
            "alaw" => "alaw".to_string(),
            f => format!("pcm-{}", f.trim_end_matches("le")),
        };
        assert_eq!(t.codec, want, "{name}");
        assert!(!t.copyable(), "{name}: PCM is re-encoded, not copied");
        let channels = if name.starts_with("pcm6") { 6 } else { 1 };
        let rate = if name.starts_with("pcm96") { 96000 } else { 48000 };
        assert_eq!((t.channels, t.sample_rate), (channels, rate), "{name}");
        let mut bytes = Vec::new();
        let mut ticks = 0u64;
        for (i, s) in t.samples.iter().enumerate() {
            bytes.extend_from_slice(&file[s.offset as usize..s.offset as usize + s.size as usize]);
            assert_eq!(s.dts, ticks as i64, "{name}: packet {i} starts where the one before ended");
            ticks += s.duration as u64;
        }
        let want = std::fs::read(path.with_extension(format!("{}.pcm", path.extension().unwrap().to_string_lossy()))).unwrap();
        assert_eq!(bytes.len(), want.len(), "{name}: bytes of sound");
        assert!(bytes == want, "{name}: the packets hold the sound's bytes in order");
        assert_eq!(ticks, rate as u64, "{name}: a second of sound");
        assert!(t.samples.len() < 100, "{name}: {} packets, not a sample per frame", t.samples.len());
    }
}
