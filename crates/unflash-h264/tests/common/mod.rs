//! What the integration tests share: the media and ffmpeg's MD5s of it,
//! decoding a track to MD5s, and muxing samples into a file.

// (each test file uses some of these)
#![allow(dead_code)]

use std::path::PathBuf;

use unflash_h264::yuv::to_i420;
use unflash_h264::{Decoder, Error};
use unflash_mp4::mux::dts_from_cts;
use unflash_mp4::{Muxer, Sample, Track, TrackDesc};

pub fn media(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/media/h264").join(name)
}

pub fn read(name: &str) -> Vec<u8> {
    std::fs::read(media(&format!("{name}.mp4"))).expect("run tests/media/h264/gen.sh")
}

/// ffmpeg's per-frame MD5s of a stream, presentation order.
pub fn expected(name: &str) -> Vec<String> {
    framemd5(&std::fs::read_to_string(media(&format!("{name}.framemd5"))).unwrap())
}

/// The MD5s in ffmpeg's `framemd5` output.
pub fn framemd5(text: &str) -> Vec<String> {
    text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).map(|l| l.rsplit(',').next().unwrap().trim().to_string()).collect()
}

/// The bytes of a sample of a file.
pub fn sample<'a>(file: &'a [u8], s: &Sample) -> &'a [u8] {
    &file[s.offset as usize..(s.offset + s.size as u64) as usize]
}

/// Decode the samples of `track`, each as `map` makes it: the MD5 of every
/// frame, in presentation order (none of them damaged; `what` names the
/// stream when one is).
pub fn md5s(dec: &mut Decoder, file: &[u8], track: &Track, what: &str, mut map: impl FnMut(&[u8]) -> Vec<u8>) -> Result<Vec<String>, Error> {
    let mut frames: Vec<(i64, String)> = Vec::new();
    let mut buf = Vec::new();
    for s in &track.samples {
        if let Some(f) = dec.decode_sample(&map(sample(file, s)), s.pts as f64)? {
            assert!(!f.damaged, "{what}: damaged frame at pts {}", s.pts);
            to_i420(&f.pic, f.crop, &mut buf);
            frames.push((s.pts, format!("{:x}", md5::compute(&buf))));
        }
    }
    frames.sort_by_key(|f| f.0);
    Ok(frames.into_iter().map(|f| f.1).collect())
}

/// A file of one H.264 track: `record` its `avcC`, `samples` (bytes,
/// presentation time, sync) its samples, timed as `like`'s.
pub fn mux(record: Vec<u8>, like: &Track, samples: &[(Vec<u8>, i64, bool)]) -> Vec<u8> {
    let mut mx = Muxer::new();
    let vt = mx.add_track(TrackDesc::Video { codec: "avc1.640028".into(), width: like.width, height: like.height, timescale: like.timescale, description: record });
    let mut file = mx.start();
    let cts: Vec<i64> = samples.iter().map(|p| p.1).collect();
    for ((bytes, pts, sync), (dts, dur)) in samples.iter().zip(dts_from_cts(&cts, like.samples[0].duration)) {
        file.extend_from_slice(bytes);
        mx.add_sample(vt, dts, *pts, dur, *sync, bytes.len() as u32).unwrap();
    }
    let (moov, (at, patch)) = mx.finish().unwrap();
    file[at as usize..at as usize + 8].copy_from_slice(&patch);
    file.extend_from_slice(&moov);
    file
}
