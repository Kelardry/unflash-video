//! Bit-exactness against ffmpeg: every picture of the streams in
//! tests/media/av1 matches the per-frame MD5 of ffmpeg's libdav1d decoding
//! (`gen.sh` there), and so does tests/media/av1.mp4 when ffmpeg is here to
//! decode it. Also: decoding from a key frame in the middle, pts, damaged
//! samples, and the formats turned down.

use std::path::{Path, PathBuf};
use std::process::Command;

use unflash_av1::{to_8bit, Decoder, Error, Frame, RawFrame};
use unflash_mp4::demux::parse_bytes;

fn media(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/media/av1").join(name)
}

/// A file's video track: its codec configuration and its samples (bytes,
/// pts in seconds, sync flag) in decoding order.
struct Stream {
    config: Vec<u8>,
    samples: Vec<(Vec<u8>, f64, bool)>,
}

fn load(path: &Path) -> Stream {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e} (run tests/media/av1/gen.sh)", path.display()));
    let movie = parse_bytes(&data).unwrap();
    let track = movie.video().unwrap();
    assert!(track.codec.starts_with("av01"), "{}: {}", path.display(), track.codec);
    let samples = track
        .samples
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut bytes = track.prefix.clone();
            bytes.extend_from_slice(&data[s.offset as usize..][..s.size as usize]);
            (bytes, track.pts_secs(i), s.sync)
        })
        .collect();
    Stream { config: track.description.clone().unwrap_or_default(), samples }
}

/// Every picture of samples `from..`, flushed at the end.
fn decode_from(s: &Stream, from: usize) -> Vec<Frame> {
    let mut dec = Decoder::new(&s.config).unwrap();
    let mut out = Vec::new();
    for (bytes, pts, _) in &s.samples[from..] {
        out.extend(dec.decode(bytes, *pts).unwrap_or_else(|e| panic!("sample at {pts}: {e}")));
    }
    out.extend(dec.flush().unwrap());
    out
}

fn decode(s: &Stream) -> Vec<Frame> {
    decode_from(s, 0)
}

fn decode_raw(s: &Stream) -> Vec<RawFrame> {
    let mut dec = Decoder::new(&s.config).unwrap();
    let mut out = Vec::new();
    for (bytes, pts, _) in &s.samples {
        out.extend(dec.decode_raw(bytes, *pts).unwrap_or_else(|e| panic!("sample at {pts}: {e}")));
    }
    out
}

fn md5_frame(f: &Frame) -> String {
    let mut c = md5::Context::new();
    c.consume(&f.y);
    c.consume(&f.u);
    c.consume(&f.v);
    format!("{:x}", c.compute())
}

/// The MD5 of a 10-bit picture as ffmpeg's yuv420p10le (little-endian words).
fn md5_raw(f: &RawFrame) -> String {
    let mut c = md5::Context::new();
    for plane in [&f.y, &f.u, &f.v] {
        let bytes: Vec<u8> = plane.iter().flat_map(|v| v.to_le_bytes()).collect();
        c.consume(&bytes);
    }
    format!("{:x}", c.compute())
}

/// ffmpeg's frame MD5s from `framemd5` output: (picture bytes, md5).
fn parse_framemd5(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split(',').map(|x| x.trim()).collect();
            (f[4].parse().unwrap(), f[5].to_string())
        })
        .collect()
}

fn oracle(name: &str) -> Vec<(usize, String)> {
    let path = media(name).with_extension("framemd5");
    parse_framemd5(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

fn check_frames(name: &str, got: &[Frame], want: &[(usize, String)]) {
    assert_eq!(got.len(), want.len(), "{name}: picture count");
    for (i, (f, (size, md5))) in got.iter().zip(want).enumerate() {
        let (w, h) = (f.width as usize, f.height as usize);
        assert_eq!(f.y.len(), w * h, "{name}: picture {i} luma size");
        assert_eq!(f.u.len(), (w + 1) / 2 * ((h + 1) / 2), "{name}: picture {i} chroma size");
        assert_eq!(f.y.len() + f.u.len() + f.v.len(), *size, "{name}: picture {i} is {w}x{h}");
        assert!(!f.damaged, "{name}: picture {i} damaged");
        assert_eq!(&md5_frame(f), md5, "{name}: picture {i} ({w}x{h}, pts {}) differs from ffmpeg's", f.pts);
    }
}

fn check(name: &str) -> Vec<Frame> {
    let s = load(&media(name));
    let frames = decode(&s);
    check_frames(name, &frames, &oracle(name));
    // each sample (temporal unit) makes exactly one picture shown, with its pts
    assert_eq!(frames.len(), s.samples.len(), "{name}: one picture per sample");
    for (f, (_, pts, _)) in frames.iter().zip(&s.samples) {
        assert_eq!(f.pts, *pts, "{name}: pts");
    }
    frames
}

#[test]
fn aom_odd_size_altref_bt709_full_range() {
    let frames = check("aom_odd.mp4");
    assert_eq!((frames[0].width, frames[0].height), (101, 75));
    assert!(frames.iter().all(|f| f.bt709 && f.full_range), "tagged BT.709 full range");
}

#[test]
fn aom_altref_show_existing_tiles() {
    let frames = check("aom_altref_tiles.mp4");
    // untagged and small: BT.601, limited range
    assert!(frames.iter().all(|f| !f.bt709 && !f.full_range));
}

#[test]
fn svt_film_grain() {
    check("svt_grain.mp4");
}

#[test]
fn rav1e_stream() {
    check("rav1e.mp4");
}

#[test]
fn frame_size_changes_with_scaled_references() {
    let frames = check("svt_resize.mp4");
    let mut sizes: Vec<(u32, u32)> = frames.iter().map(|f| (f.width, f.height)).collect();
    sizes.sort();
    sizes.dedup();
    assert!(sizes.len() >= 4, "sizes {sizes:?}");
    assert!(sizes.iter().any(|&(w, h)| w % 2 == 1 || h % 2 == 1), "an odd size among {sizes:?}");
}

#[test]
fn ten_bit_exact_and_rounded_to_8_bits() {
    let name = "svt_10bit.mkv";
    let s = load(&media(name));
    let raw = decode_raw(&s);
    let want = oracle(name);
    assert_eq!(raw.len(), want.len(), "picture count");
    for (i, (r, (size, md5))) in raw.iter().zip(&want).enumerate() {
        assert_eq!(r.bit_depth, 10);
        assert_eq!((r.y.len() + r.u.len() + r.v.len()) * 2, *size, "picture {i} size");
        assert_eq!(&md5_raw(r), md5, "picture {i}: 10-bit samples differ from ffmpeg's");
    }
    let frames = decode(&s);
    assert_eq!(frames.len(), raw.len());
    for (i, (f, r)) in frames.iter().zip(&raw).enumerate() {
        assert_eq!((f.width, f.height, f.pts), (r.width, r.height, r.pts));
        for (got, src) in [(&f.y, &r.y), (&f.u, &r.u), (&f.v, &r.v)] {
            assert_eq!(got.len(), src.len());
            assert!(got.iter().zip(src.iter()).all(|(&g, &s)| g == to_8bit(s)), "picture {i}: 8-bit rounding");
        }
        assert!(f.bt709, "tagged BT.2020: the BT.709 matrix is the nearer");
        assert!(!f.full_range);
    }
}

#[test]
fn monochrome_has_grey_chroma() {
    let name = "aom_gray.mp4";
    let frames = decode(&load(&media(name)));
    let want = oracle(name);
    assert_eq!(frames.len(), want.len());
    for (i, (f, (size, md5))) in frames.iter().zip(&want).enumerate() {
        assert_eq!(f.y.len(), *size);
        assert_eq!(&format!("{:x}", md5::compute(&f.y)), md5, "picture {i}: luma differs from ffmpeg's");
        assert!(f.u.iter().chain(&f.v).all(|&c| c == 128), "picture {i}: grey chroma");
        assert_eq!(f.u.len(), (f.width as usize + 1) / 2 * ((f.height as usize + 1) / 2));
    }
}

#[test]
fn formats_turned_down() {
    for (name, why) in [("aom_444.mp4", "4:4:4 chroma"), ("aom_422.mp4", "4:2:2 chroma"), ("aom_12bit.mp4", "12-bit samples")] {
        let s = load(&media(name));
        assert_eq!(Decoder::new(&s.config).err(), Some(Error::Unsupported(why)), "{name}: from the av1C record");
        // without a configuration, the first picture tells
        let mut dec = Decoder::new(&[]).unwrap();
        assert_eq!(dec.decode(&s.samples[0].0, 0.0).err(), Some(Error::Unsupported(why)), "{name}: from the picture");
    }
}

/// Decoding from each later key frame gives the pictures a decode from the
/// start gives from there on (the app decodes GOPs in parallel).
#[test]
fn from_a_key_frame_in_the_middle() {
    for name in ["aom_odd.mp4", "aom_altref_tiles.mp4"] {
        let s = load(&media(name));
        let all = decode(&s);
        let keys: Vec<usize> = (1..s.samples.len()).filter(|&i| s.samples[i].2).collect();
        assert!(!keys.is_empty(), "{name}: a key frame in the middle");
        for k in keys {
            let part = decode_from(&s, k);
            let tail: Vec<&Frame> = all.iter().filter(|f| f.pts >= s.samples[k].1).collect();
            assert_eq!(part.len(), tail.len(), "{name} from sample {k}: picture count");
            for (a, b) in part.iter().zip(tail) {
                assert_eq!(a, b, "{name} from sample {k}: picture at {}", a.pts);
            }
        }
    }
}

/// Whatever the pts, a picture gives back exactly the one its sample had.
#[test]
fn pts_ride_through_exactly() {
    let s = load(&media("aom_altref_tiles.mp4"));
    let mut dec = Decoder::new(&s.config).unwrap();
    for (i, (bytes, _, _)) in s.samples.iter().enumerate() {
        let pts = i as f64 * 1001.0 / 30000.0 + 1e-12 - 7.0;
        let frames = dec.decode(bytes, pts).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].pts.to_bits(), pts.to_bits());
    }
}

/// Streams one after another in one decoder: a new sequence header with
/// another size at each key frame.
#[test]
fn streams_spliced_at_key_frames() {
    let names = ["aom_odd.mp4", "svt_grain.mp4", "rav1e.mp4"];
    let first = load(&media(names[0]));
    let mut dec = Decoder::new(&first.config).unwrap();
    let mut got = Vec::new();
    let mut want = Vec::new();
    for name in names {
        for (bytes, pts, _) in &load(&media(name)).samples {
            got.extend(dec.decode(bytes, *pts).unwrap());
        }
        want.extend(oracle(name));
    }
    got.extend(dec.flush().unwrap());
    check_frames("spliced", &got, &want);
}

/// A sample that fails: its picture is the last good one, marked damaged,
/// and so is every picture up to the next key frame; from there on the
/// pictures are exact again.
#[test]
fn damaged_sample_until_the_next_key_frame() {
    let name = "aom_odd.mp4";
    let s = load(&media(name));
    let want = oracle(name);
    let key = (1..s.samples.len()).find(|&i| s.samples[i].2).unwrap();
    let bad = 4;
    assert!(bad < key);
    let mut dec = Decoder::new(&s.config).unwrap();
    let mut frames = Vec::new();
    for (i, (bytes, pts, _)) in s.samples.iter().enumerate() {
        let bytes = if i == bad { &bytes[..bytes.len() / 2] } else { &bytes[..] };
        let out = dec.decode(bytes, *pts).unwrap();
        assert_eq!(out.len(), 1, "sample {i}");
        frames.extend(out);
    }
    for (i, f) in frames.iter().enumerate() {
        assert_eq!(f.pts, s.samples[i].1);
        if i < bad {
            assert!(!f.damaged, "picture {i}");
            assert_eq!(md5_frame(f), want[i].1, "picture {i}");
        } else if i < key {
            assert!(f.damaged, "picture {i} after the failed sample");
        } else {
            assert!(!f.damaged, "picture {i} from the key frame");
            assert_eq!(md5_frame(f), want[i].1, "picture {i} from the key frame");
        }
    }
    assert_eq!((frames[bad].y.clone(), frames[bad].u.clone()), (frames[bad - 1].y.clone(), frames[bad - 1].u.clone()), "the failed sample's picture is the last good one");
}

/// After `flush` the decoder starts over at a key frame, with the
/// configuration's sequence header (Matroska files may carry it nowhere
/// else).
#[test]
fn flush_then_start_over() {
    for name in ["aom_altref_tiles.mp4", "svt_10bit.mkv"] {
        let s = load(&media(name));
        let all = decode(&s);
        let key = (1..s.samples.len()).find(|&i| s.samples[i].2).unwrap_or(0);
        let mut dec = Decoder::new(&s.config).unwrap();
        for (bytes, pts, _) in &s.samples[..s.samples.len() / 2 + 1] {
            dec.decode(bytes, *pts).unwrap();
        }
        assert!(dec.flush().unwrap().is_empty(), "{name}: nothing held back");
        let mut again = Vec::new();
        for (bytes, pts, _) in &s.samples[key..] {
            again.extend(dec.decode(bytes, *pts).unwrap());
        }
        assert_eq!(again.as_slice(), &all[key..], "{name}: from sample {key} after a flush");
    }
}

/// tests/media/av1.mp4 (made by tests/media/gen.sh, so possibly anew on
/// each run) against ffmpeg's libdav1d decoding of it, when ffmpeg is here.
#[test]
fn repo_av1_mp4_matches_ffmpeg() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/media/av1.mp4");
    if !path.exists() {
        eprintln!("{} missing (tests/media/gen.sh): skipped", path.display());
        return;
    }
    let out = match Command::new("ffmpeg").args(["-v", "error", "-c:v", "libdav1d", "-i"]).arg(&path).args(["-autoscale", "0", "-fps_mode", "passthrough", "-f", "framemd5", "-pix_fmt", "yuv420p", "-"]).output() {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            eprintln!("ffmpeg could not decode it with libdav1d ({}): skipped", String::from_utf8_lossy(&o.stderr).trim());
            return;
        }
        Err(e) => {
            eprintln!("ffmpeg not run ({e}): skipped");
            return;
        }
    };
    let want = parse_framemd5(&String::from_utf8_lossy(&out.stdout));
    let s = load(&path);
    check_frames("av1.mp4", &decode(&s), &want);
}
