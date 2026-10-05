//! Shared by both decoders' tests and examples: ffmpeg as the reference
//! decoder, a bit writer and a random generator for the frames and the
//! damage made here, and the checks of what both decoders promise of
//! damaged and unusual input (run by `ac3_robust.rs` and `dts_robust.rs`).
//! Each decoder's own comparison with ffmpeg, and the frames written for
//! it, are in `ac3` and `dts`.
#![allow(dead_code)]

pub mod ac3;
pub mod dts;

use std::ops::Range;
use std::path::Path;
use std::process::Command;

use unflash_sound::{Decoded, Output};

pub fn ffmpeg_available() -> bool {
    let ok = |cmd: &str| Command::new(cmd).arg("-version").output().map(|o| o.status.success()).unwrap_or(false);
    ok("ffmpeg") && ok("ffprobe")
}

/// Whether to skip `what` (it compares with ffmpeg), saying so.
pub fn skip(what: &str) -> bool {
    if ffmpeg_available() {
        return false;
    }
    eprintln!("SKIPPED {what}: ffmpeg and ffprobe are not installed, so there is no reference to compare with");
    true
}

/// ffprobe's options among `input`: the format (`-f`), and of the
/// decoder's, `-core_only` (which changes a DTS stream's channel count).
fn format_options<'a>(input: &[&'a str]) -> Vec<&'a str> {
    input.windows(2).filter(|w| w[0] == "-f" || w[0] == "-core_only").flat_map(|w| [w[0], w[1]]).collect()
}

/// The channel count ffprobe gives the first audio stream.
pub fn ffprobe_channels(path: &Path, input: &[&str]) -> Option<usize> {
    let out = Command::new("ffprobe")
        .args(["-v", "error"])
        .args(format_options(input))
        .args(["-select_streams", "a:0", "-show_entries", "stream=channels", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()
}

/// Interleaved little endian float samples (ffmpeg's output, a FATE
/// reference) as one vector per channel.
pub fn planar(bytes: &[u8], channels: usize) -> Vec<Vec<f32>> {
    let samples: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    let n = samples.len() / channels;
    (0..channels).map(|c| (0..n).map(|i| samples[i * channels + c]).collect()).collect()
}

/// ffmpeg's float decode of `path` (with `input` options before `-i`,
/// such as `-core_only 1` or `-downmix stereo`, and `output` options after
/// it, such as `-ac 2`), one vector per channel in ffmpeg's order.
pub fn ffmpeg_decode(path: &Path, input: &[&str], output: &[&str]) -> Option<Vec<Vec<f32>>> {
    let channels = match output.iter().position(|&a| a == "-ac") {
        Some(i) => output.get(i + 1)?.parse().ok()?,
        None => match input.iter().position(|&a| a == "-downmix") {
            Some(_) => 2,
            None => ffprobe_channels(path, input)?,
        },
    };
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error"])
        .args(input)
        .arg("-i")
        .arg(path)
        .args(output)
        .args(["-f", "f32le", "-c:a", "pcm_f32le", "-"])
        .output()
        .ok()?;
    if !out.status.success() && out.stdout.is_empty() {
        return None;
    }
    Some(planar(&out.stdout, channels))
}

/// ffmpeg's decode of `data` written to a temporary file named `name`.
pub fn ffmpeg_decode_bytes(data: &[u8], name: &str, input: &[&str], output: &[&str]) -> Option<Vec<Vec<f32>>> {
    let path = std::env::temp_dir().join(format!("unflash-sound-{}-{name}", std::process::id()));
    std::fs::write(&path, data).ok()?;
    let r = ffmpeg_decode(&path, input, output);
    let _ = std::fs::remove_file(&path);
    r
}

// ---------------------------------------------------------------------
// Building frames, and damaging them.

/// A most-significant-bit-first writer.
#[derive(Clone, Default)]
pub struct BitWriter {
    pub bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    pub fn put(&mut self, n: u32, v: u32) {
        for k in (0..n).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (v >> k) & 1 != 0 {
                *self.bytes.last_mut().unwrap() |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }

    pub fn bits(&self) -> usize {
        self.bits
    }

    /// Zero bits up to the next multiple of `n` bits.
    pub fn align(&mut self, n: usize) {
        while !self.bits.is_multiple_of(n) {
            self.put(1, 0);
        }
    }

    /// Keep the first `bits` bits.
    pub fn truncate(&mut self, bits: usize) {
        self.bytes.truncate(bits.div_ceil(8));
        if !bits.is_multiple_of(8) {
            *self.bytes.last_mut().unwrap() &= 0xff << (8 - bits % 8);
        }
        self.bits = bits;
    }

    /// Overwrite the `n` bits at bit position `pos` (already written).
    pub fn set(&mut self, pos: usize, n: u32, v: u32) {
        for k in 0..n as usize {
            let bit = pos + k;
            let mask = 0x80 >> (bit % 8);
            if (v >> (n as usize - 1 - k)) & 1 != 0 {
                self.bytes[bit / 8] |= mask;
            } else {
                self.bytes[bit / 8] &= !mask;
            }
        }
    }
}

/// A small deterministic generator, for the frames built at random and
/// the damage done to streams.
pub struct Lcg(pub u64);

impl Lcg {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    pub fn below(&mut self, n: u32) -> u32 {
        (self.next() % n as u64) as u32
    }
    pub fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + self.below((hi - lo + 1) as u32) as i32
    }
    pub fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < percent
    }
    /// A position in something `n` long (0 when it is empty).
    pub fn index(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

pub fn rms(v: &[f32]) -> f64 {
    (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt()
}

// ---------------------------------------------------------------------
// What both decoders promise: damaged frames are silence of their length
// and are counted, the decoder carries on, nothing in any input makes it
// panic, a layout change resizes the output, and decoding frame by frame
// is decoding all at once.

/// Either decoder, as those checks drive it.
pub trait Sound: Sized {
    type Error: std::fmt::Debug + PartialEq;
    fn new(output: Output) -> Self;
    /// AC-3's dither and spectral extension noise on (the default) or off;
    /// DTS has no noise to turn off.
    fn set_noise(&mut self, on: bool);
    fn decode(&mut self, data: &[u8], out: &mut Vec<Vec<f32>>) -> Result<Decoded, Self::Error>;
    /// The output channels and sample rate `info` gives.
    fn layout(&self) -> Option<(usize, u32)>;
}

// (each calls the decoder's own method of that name)
impl Sound for unflash_sound::ac3::Decoder {
    type Error = unflash_sound::ac3::Error;
    fn new(output: Output) -> Self {
        unflash_sound::ac3::Decoder::new(output)
    }
    fn set_noise(&mut self, on: bool) {
        unflash_sound::ac3::Decoder::set_noise(self, on)
    }
    fn decode(&mut self, data: &[u8], out: &mut Vec<Vec<f32>>) -> Result<Decoded, Self::Error> {
        unflash_sound::ac3::Decoder::decode(self, data, out)
    }
    fn layout(&self) -> Option<(usize, u32)> {
        self.info().map(|i| (i.channels, i.sample_rate))
    }
}

impl Sound for unflash_sound::dts::Decoder {
    type Error = unflash_sound::dts::Error;
    fn new(output: Output) -> Self {
        unflash_sound::dts::Decoder::new(output)
    }
    fn set_noise(&mut self, _: bool) {}
    fn decode(&mut self, data: &[u8], out: &mut Vec<Vec<f32>>) -> Result<Decoded, Self::Error> {
        unflash_sound::dts::Decoder::decode(self, data, out)
    }
    fn layout(&self) -> Option<(usize, u32)> {
        self.info().map(|i| (i.channels, i.sample_rate))
    }
}

/// Decode `data` all at once, with a new decoder of `Output::Native`.
pub fn decode_all<S: Sound>(data: &[u8], noise: bool) -> (Result<Decoded, S::Error>, Vec<Vec<f32>>) {
    let mut dec = S::new(Output::Native);
    dec.set_noise(noise);
    let mut out = Vec::new();
    let r = dec.decode(data, &mut out);
    (r, out)
}

/// `data` with its middle frame (of the frames `fr`) damaged by `damage`
/// (given the frame and the stream's channel count) so that it does not
/// decode: that frame is silence of its length and counted, the frames
/// before it are as they were, and the output is as it was again from
/// `settle(samples a frame)` samples past the frame's end, where `settle`
/// gives a number (the frames after it start from silence).
pub fn damaged_frame_is_silence<S: Sound>(
    name: &str,
    data: &[u8],
    fr: &[Range<usize>],
    damage: impl FnOnce(&mut [u8], Range<usize>, usize),
    settle: impl FnOnce(usize) -> Option<usize>,
) {
    let (clean, clean_out) = decode_all::<S>(data, false);
    let clean = clean.unwrap();
    let k = fr.len() / 2;
    let mut bad = data.to_vec();
    damage(&mut bad, fr[k].clone(), clean_out.len());
    let (r, out) = decode_all::<S>(&bad, false);
    let r = r.unwrap();
    assert_eq!(r.damaged, 1, "{name}");
    assert_eq!(r.samples, clean.samples, "{name}: the damaged frame keeps its length");
    let spf = clean.samples / fr.len();
    let settled = settle(spf).map(|n| (k + 1) * spf + n);
    for (c, (o, co)) in out.iter().zip(&clean_out).enumerate() {
        assert!(o[k * spf..(k + 1) * spf].iter().all(|&v| v == 0.0), "{name} channel {c}: not silent");
        assert_eq!(&o[..k * spf], &co[..k * spf], "{name} channel {c}: the frames before changed");
        if let Some(from) = settled {
            assert_eq!(&o[from..], &co[from..], "{name} channel {c}: the frames after changed");
        }
    }
}

/// `data` cut inside its frame 3 (`at` bytes into it, in its middle, a
/// byte before its end) ends with that frame as silence of its length,
/// counted, after the frames before it as they were; cut inside frame 2's
/// first `sync` bytes, which are not a frame, it ends with frame 1.
pub fn cut_mid_frame<S: Sound>(name: &str, data: &[u8], fr: &[Range<usize>], at: usize, sync: usize) {
    let (clean, clean_out) = decode_all::<S>(data, false);
    let spf = clean.unwrap().samples / fr.len();
    for cut in [fr[3].start + at, fr[3].end - 1, (fr[3].start + fr[3].end) / 2] {
        let (r, out) = decode_all::<S>(&data[..cut], false);
        let r = r.unwrap();
        assert_eq!(r.damaged, 1, "{name} cut at {cut}");
        assert_eq!(r.samples, 4 * spf, "{name} cut at {cut}");
        for (o, co) in out.iter().zip(&clean_out) {
            assert_eq!(&o[..3 * spf], &co[..3 * spf]);
            assert!(o[3 * spf..].iter().all(|&v| v == 0.0));
        }
    }
    let (r, _) = decode_all::<S>(&data[..fr[2].start + sync], false);
    assert_eq!(r.unwrap().samples, 2 * spf, "{name}: {sync} bytes are not a frame");
}

/// Random bytes, from 1 to 100 000 of them, decode or give `no_sync`.
pub fn random_bytes<S: Sound>(rng: &mut Lcg, no_sync: S::Error) {
    for size in [1, 5, 6, 100, 1000, 5000, 100_000] {
        let data: Vec<u8> = (0..size).map(|_| rng.next() as u8).collect();
        // random bytes may happen to hold a sync word and a plausible
        // header: that is a damaged frame, not an error
        match decode_all::<S>(&data, true) {
            (Ok(d), out) => assert!(out.iter().all(|c| c.len() == d.samples)),
            (Err(e), _) => assert_eq!(e, no_sync),
        }
    }
}

/// A frame of `data` at random (of the frames `fr`), with the frame before
/// it so that what one frame leaves the next is in play, and damage of one
/// kind at random done to it: bits flipped, random bytes, a bit of its
/// header (in its bytes `header`), a run of zeros or of ones, its data
/// shifted by some bits, or bytes replaced here and there. `fix` may then
/// edit the piece (the frame starts at the offset it is given) before the
/// frame after it is added as it was, which goes on from what the mutated
/// frame left behind.
pub fn mutated_piece(rng: &mut Lcg, data: &[u8], fr: &[Range<usize>], header: Range<usize>, fix: impl FnOnce(&mut Vec<u8>, usize)) -> Vec<u8> {
    let k = rng.index(fr.len());
    let start = fr[k.saturating_sub(1)].start;
    let mut piece = data[start..fr[k].end].to_vec();
    let f0 = fr[k].start - start;
    let len = fr[k].len();
    match rng.index(6) {
        0 => {
            for _ in 0..1 + rng.index(8) {
                let p = f0 + 5 + rng.index(len - 5);
                piece[p] ^= 1 << rng.index(8);
            }
        }
        1 => {
            let p = f0 + 5 + rng.index(len - 5);
            let n = 1 + rng.index(40);
            for b in piece.iter_mut().skip(p).take(n) {
                *b = rng.next() as u8;
            }
        }
        2 => {
            let p = f0 + header.start + rng.index(header.len());
            piece[p] ^= 1 << rng.index(8);
        }
        3 => {
            let p = f0 + 5 + rng.index(len - 5);
            let v = if rng.index(2) == 0 { 0 } else { 0xff };
            for b in piece.iter_mut().skip(p).take(1 + rng.index(64)) {
                *b = v;
            }
        }
        4 => {
            // shift the frame's data by some bits
            let p = f0 + 5 + rng.index(len - 6);
            let s = 1 + rng.index(7);
            for i in (p..f0 + len - 1).rev() {
                piece[i] = piece[i] >> s | piece[i - 1] << (8 - s);
            }
        }
        _ => {
            for b in piece.iter_mut().skip(f0 + 5) {
                if rng.index(20) == 0 {
                    *b = rng.next() as u8;
                }
            }
        }
    }
    fix(&mut piece, f0);
    if let Some(next) = fr.get(k + 1) {
        piece.extend_from_slice(&data[next.clone()]);
    }
    piece
}

/// What a decoder gave for damaged data, when it gave anything: every
/// channel as long as it says, every sample finite.
pub fn assert_whole(name: &str, decoded: Option<Decoded>, out: &[Vec<f32>]) {
    if let Some(d) = decoded {
        assert!(out.iter().all(|c| c.len() == d.samples), "{name}: channels of unequal length");
        assert!(out.iter().flatten().all(|v| v.is_finite()), "{name}: not finite");
    }
}

/// `data` cut short, with up to 3000 bytes taken out, up to 2000 random
/// bytes put in, or 20 bytes replaced, and fed to a new decoder in random
/// pieces, as a demuxer might: whatever it gives has every channel as long
/// as the others, every sample finite.
pub fn damaged_stream<S: Sound>(rng: &mut Lcg, name: &str, data: &[u8]) {
    let mut d = data.to_vec();
    match rng.index(4) {
        0 => d.truncate(rng.index(d.len())),
        1 => {
            let p = rng.index(d.len());
            let n = rng.index(3000).min(d.len() - p);
            d.drain(p..p + n);
        }
        2 => {
            let p = rng.index(d.len());
            let junk: Vec<u8> = (0..rng.index(2000)).map(|_| rng.next() as u8).collect();
            d.splice(p..p, junk);
        }
        _ => {
            for _ in 0..20 {
                let p = rng.index(d.len());
                d[p] = rng.next() as u8;
            }
        }
    }
    let mut dec = S::new(Output::Native);
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < d.len() {
        let n = 1 + rng.index(8000);
        let end = (pos + n).min(d.len());
        let _ = dec.decode(&d[pos..end], &mut out);
        pos = end;
    }
    let len = out.first().map_or(0, |c| c.len());
    assert!(out.iter().all(|c| c.len() == len), "{name}");
    assert!(out.iter().flatten().all(|v| v.is_finite()), "{name}");
}

/// A stream whose layout changes, `stereo`'s frames then `five`'s (5.1):
/// `out` follows the latest frame's layout, new channels starting with
/// silence, every channel as long as the rest. Then `mono`'s frames, in a
/// later call, take it down to one channel (of `mono_rate`); in
/// `Output::Stereo` it stays stereo, mono as two identical channels.
pub fn layout_change<S: Sound>(stereo: &[u8], five: &[u8], mono: &[u8], mono_rate: u32) {
    let (s, s_out) = decode_all::<S>(stereo, false);
    let (f, _) = decode_all::<S>(five, false);
    let (s, f) = (s.unwrap().samples, f.unwrap().samples);
    let mut both = stereo.to_vec();
    both.extend_from_slice(five);
    let (r, out) = decode_all::<S>(&both, false);
    assert_eq!(r.unwrap().samples, s + f);
    assert_eq!(out.len(), 6);
    assert!(out.iter().all(|c| c.len() == s + f));
    assert_eq!(&out[0][..s], &s_out[0][..]);
    assert_eq!(&out[1][..s], &s_out[1][..]);
    assert!(out[2..].iter().all(|c| c[..s].iter().all(|&v| v == 0.0)));
    assert!(out[5][s..].iter().any(|&v| v != 0.0));
    // and down to one channel, in a later call
    let mut dec = S::new(Output::Native);
    let mut out = Vec::new();
    dec.decode(&both, &mut out).unwrap();
    let d = dec.decode(mono, &mut out).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].len(), s + f + d.samples);
    assert_eq!(dec.layout(), Some((1, mono_rate)));
    // Stereo stays stereo; mono becomes two identical channels
    let mut dec = S::new(Output::Stereo);
    let mut out = Vec::new();
    dec.decode(&both, &mut out).unwrap();
    let d = dec.decode(mono, &mut out).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].len(), out[1].len());
    let n = out[0].len();
    assert_eq!(&out[0][n - d.samples..], &out[1][n - d.samples..]);
}

/// Decoding `data` frame by frame (`fr`: as the blocks of a Matroska track
/// or the samples of an MP4 one come) gives what decoding it all at once
/// gives.
pub fn frame_by_frame<S: Sound>(name: &str, data: &[u8], fr: &[Range<usize>]) {
    let (_, all) = decode_all::<S>(data, false);
    let mut dec = S::new(Output::Native);
    dec.set_noise(false);
    let mut out = Vec::new();
    for f in fr {
        dec.decode(&data[f.clone()], &mut out).unwrap();
    }
    assert_eq!(out, all, "{name}");
}
