//! AV1 decoding for Unflash: container samples in, 8-bit 4:2:0 pictures out,
//! with the same interface as the other built-in decoders.
//!
//! The decoding is rav1d, the Rust port of the dav1d decoder, vendored in
//! `third_party/rav1d` as pure Rust (see its UNFLASH.md) and driven through
//! its dav1d-compatible API with one thread and a frame delay of one:
//! wasm32 cannot start threads here, and the app runs several decoders in
//! Web Workers over different GOPs instead. As the browsers' decoders do,
//! it synthesises film grain; so do the tests' reference, ffmpeg's libdav1d
//! decoding, which the pictures match bit for bit (`tests/`).
//!
//! - 8- and 10-bit 4:2:0 streams, and monochrome ones (grey chroma). 10-bit
//!   samples are rounded to 8 bits: `(v + 2) >> 2`, at most 255. 4:2:2,
//!   4:4:4 and 12-bit streams are [`Error::Unsupported`].
//! - A picture carries the pts of the sample that made it shown: a hidden
//!   frame makes no picture of its own, a `show_existing_frame` makes one.
//! - A sample dav1d rejects makes a copy of the last picture, marked
//!   `damaged`, and so is every picture after it until the next key frame
//!   (they may predict from what was lost).

use std::ffi::c_int;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dLogger, Dav1dSettings, DAV1D_INLOOPFILTER_ALL};
use rav1d::include::dav1d::headers::{
    DAV1D_FRAME_TYPE_KEY, DAV1D_MC_BT2020_CL, DAV1D_MC_BT2020_NCL, DAV1D_MC_BT470BG, DAV1D_MC_BT601, DAV1D_MC_BT709, DAV1D_MC_FCC, DAV1D_MC_SMPTE240, DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_flush, dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data};

/// dav1d's "not now" answer, `-EAGAIN`, with the errno value the vendored
/// rav1d uses: the platform's, or on wasm (whose `libc` has no errno
/// values) the Linux one its patch picks (third_party/rav1d/src/error.rs).
#[cfg(not(target_family = "wasm"))]
const EAGAIN: c_int = -libc::EAGAIN;
#[cfg(target_family = "wasm")]
const EAGAIN: c_int = -11;

/// Why a stream or a sample could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A stream this decoder does not turn into pictures (4:2:2 or 4:4:4
    /// chroma, 12-bit samples).
    Unsupported(&'static str),
    /// Malformed data: a codec configuration that is not an `av1C` record,
    /// or a sample the decoder rejected with no earlier picture to stand in.
    Bitstream(&'static str),
    /// The decoder itself failed (it could not start, or ran out of
    /// memory): dav1d's error code.
    Decoder(i32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unsupported(s) => write!(f, "unsupported AV1 stream: {s}"),
            Error::Bitstream(s) => write!(f, "invalid AV1 data: {s}"),
            Error::Decoder(code) => write!(f, "the AV1 decoder failed (error {code})"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// A decoded picture.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    /// Display size: the frame's (upscaled) size, which may be odd.
    pub width: u32,
    pub height: u32,
    /// 8-bit 4:2:0 planes, tightly packed; chroma is (width + 1) / 2 by
    /// (height + 1) / 2.
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    /// The pts of the sample that produced it, seconds.
    pub pts: f64,
    /// Decoding hit an error: the picture may be concealed or garbage.
    pub damaged: bool,
    /// BT.709 matrix (BT.709, BT.2020 and SMPTE 240M streams, and
    /// unspecified ones taller than 576 lines, as players guess); BT.601
    /// otherwise.
    pub bt709: bool,
    pub full_range: bool,
}

/// A picture at the stream's own bit depth, for tests and tools: samples as
/// decoded (0..=255 or 0..=1023), planes tightly packed as in [`Frame`].
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub y: Vec<u16>,
    pub u: Vec<u16>,
    pub v: Vec<u16>,
    pub pts: f64,
    pub damaged: bool,
}

/// An AV1 decoder for one stream.
pub struct Decoder {
    /// dav1d's context, open for the life of the decoder.
    ctx: Dav1dContext,
    /// The configuration record's OBUs (a sequence header), fed again after
    /// every restart: Matroska files may carry it nowhere else.
    config_obus: Vec<u8>,
    /// A sample failed since the last key frame: later pictures may predict
    /// from what was lost.
    damaged: bool,
    /// The last picture made shown, to stand in for a sample that fails.
    last: Option<Picture>,
}

impl Decoder {
    /// A decoder for a stream whose container gives `config` as its codec
    /// configuration: the MP4 `av1C` box payload or the Matroska
    /// CodecPrivate (an `av1C` record whose configOBUs may carry the
    /// sequence header). It may be empty: the samples' own sequence headers
    /// then configure the decoder.
    pub fn new(config: &[u8]) -> Result<Decoder> {
        let obus = config_obus(config)?;
        let mut d = Decoder { ctx: open()?, config_obus: obus.to_vec(), damaged: false, last: None };
        d.send_config();
        Ok(d)
    }

    /// Decode one container sample (a temporal unit): the pictures it makes
    /// shown, usually one; none for a sample that only carries hidden frames
    /// or headers.
    pub fn decode(&mut self, sample: &[u8], pts: f64) -> Result<Vec<Frame>> {
        self.run(sample, pts, to_frame)
    }

    /// [`Decoder::decode`] with the pictures at the stream's own bit depth.
    #[doc(hidden)]
    pub fn decode_raw(&mut self, sample: &[u8], pts: f64) -> Result<Vec<RawFrame>> {
        self.run(sample, pts, to_raw)
    }

    /// The pictures still held back at the end. With one frame thread there
    /// are none (every picture comes out with its sample); the decoder then
    /// starts over, ready for a key frame of this stream (after a seek).
    pub fn flush(&mut self) -> Result<Vec<Frame>> {
        let mut pics = Vec::new();
        let damaged = self.take_pictures(&mut pics).is_some() || self.damaged;
        let frames = pics.iter().map(|p| to_frame(p, p.pts(), damaged)).collect::<Result<Vec<_>>>();
        self.last = None;
        drop(pics);
        // SAFETY: `ctx` is open (from `open`, not yet closed).
        unsafe { dav1d_flush(self.ctx) };
        self.damaged = false;
        self.send_config();
        frames
    }

    /// Feed the configuration's OBUs. Errors are left to the samples: their
    /// own sequence headers configure the decoder just as well.
    fn send_config(&mut self) {
        if !self.config_obus.is_empty() {
            let obus = std::mem::take(&mut self.config_obus);
            let _ = self.send(&obus, 0.0);
            self.config_obus = obus;
        }
    }

    fn run<T>(&mut self, sample: &[u8], pts: f64, convert: fn(&Picture, f64, bool) -> Result<T>) -> Result<Vec<T>> {
        let (pics, error) = self.send(sample, pts)?;
        let failed = error.is_some();
        if failed {
            self.damaged = true;
        }
        let mut out = Vec::with_capacity(pics.len().max(1));
        for pic in pics {
            if !failed && pic.is_key_frame() {
                // a shown key frame refreshes every reference
                self.damaged = false;
            }
            out.push(convert(&pic, pic.pts(), self.damaged)?);
            self.last = Some(pic);
        }
        if failed && out.is_empty() {
            // the sample's picture is lost: the last one stands in for it
            match &self.last {
                Some(last) => out.push(convert(last, pts, true)?),
                None => return Err(Error::Bitstream("the decoder rejected the sample")),
            }
        }
        Ok(out)
    }

    /// Feed one sample and take out every picture it makes shown; also the
    /// error dav1d reported on the way, if it did (the rest of the sample is
    /// then dropped, as ffmpeg does).
    fn send(&mut self, sample: &[u8], pts: f64) -> Result<(Vec<Picture>, Option<c_int>)> {
        let mut pics = Vec::new();
        if sample.is_empty() {
            return Ok((pics, None));
        }
        let mut data = Dav1dData::default();
        // SAFETY: `data` is valid to write; on success it owns a new buffer
        // of `sample.len()` bytes that `buf` points to.
        let buf = unsafe { dav1d_data_create(Some(NonNull::from(&mut data)), sample.len()) };
        if buf.is_null() {
            // (-ENOMEM)
            return Err(Error::Decoder(-12));
        }
        // SAFETY: `buf` is `sample.len()` writable bytes of dav1d's, apart
        // from `sample`.
        unsafe { std::ptr::copy_nonoverlapping(sample.as_ptr(), buf, sample.len()) };
        // the pts rides through dav1d bit for bit (dav1d only copies it)
        data.m.timestamp = pts.to_bits() as i64;
        let mut stalled = false;
        let error = loop {
            // SAFETY: `ctx` is open; `data` is valid to read and write, and
            // dav1d leaves it empty once it has taken the data.
            let r = unsafe { dav1d_send_data(Some(self.ctx), Some(NonNull::from(&mut data))) }.0;
            if r != 0 && r != EAGAIN {
                break Some(r);
            }
            // taken (0), or not yet (EAGAIN: dav1d holds pictures or data from
            // before): take the pictures out, and send again what it left
            let before = pics.len();
            if let Some(e) = self.take_pictures(&mut pics) {
                break Some(e);
            }
            if r == 0 {
                break None;
            }
            if pics.len() > before {
                stalled = false;
            } else if std::mem::replace(&mut stalled, true) {
                // (dav1d takes the data once what it held has come out: no
                // progress twice in a row would be a bug) give up on the sample
                break Some(EAGAIN);
            }
        };
        // SAFETY: `data` is valid; this frees what dav1d did not take (a no-op
        // when it took everything).
        unsafe { dav1d_data_unref(Some(NonNull::from(&mut data))) };
        Ok((pics, error))
    }

    /// Take out every picture dav1d has ready; the error it reported, if any.
    fn take_pictures(&mut self, pics: &mut Vec<Picture>) -> Option<c_int> {
        loop {
            let mut p = Picture(Dav1dPicture::default());
            // SAFETY: `ctx` is open; `p.0` is valid to write (dav1d writes a
            // picture, or an empty one on failure, over the empty default).
            let r = unsafe { dav1d_get_picture(Some(self.ctx), Some(NonNull::from(&mut p.0))) }.0;
            if r != 0 {
                return (r != EAGAIN).then_some(r);
            }
            pics.push(p);
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // pictures go back to the context's pool before it closes
        self.last = None;
        close(self.ctx);
    }
}

/// A dav1d context: one thread, pictures out as soon as they are decoded,
/// every in-loop filter and film grain applied, the highest spatial layer
/// only (as ffmpeg and the browsers), no logging.
fn open() -> Result<Dav1dContext> {
    let mut s = MaybeUninit::<Dav1dSettings>::uninit();
    // SAFETY: `dav1d_default_settings` writes a whole `Dav1dSettings` to the
    // (possibly uninitialised) memory it is given.
    let mut s = unsafe {
        dav1d_default_settings(NonNull::from(&mut s).cast());
        s.assume_init()
    };
    s.n_threads = 1;
    s.max_frame_delay = 1;
    s.apply_grain = 1;
    s.all_layers = 0;
    s.inloop_filters = DAV1D_INLOOPFILTER_ALL;
    // SAFETY: a logger without a callback is never called.
    s.logger = unsafe { Dav1dLogger::new(None, None) };
    let mut ctx: Option<Dav1dContext> = None;
    // SAFETY: both pointers are valid for the call: dav1d reads the settings
    // and writes the new context (or `None`) to `ctx`.
    let r = unsafe { dav1d_open(Some(NonNull::from(&mut ctx)), Some(NonNull::from(&mut s))) }.0;
    match ctx {
        Some(ctx) if r == 0 => Ok(ctx),
        _ => Err(Error::Decoder(r)),
    }
}

fn close(ctx: Dav1dContext) {
    let mut ctx = Some(ctx);
    // SAFETY: `ctx` is from `dav1d_open` and closed only here, once; the
    // decoder no longer holds any of its pictures.
    unsafe { dav1d_close(Some(NonNull::from(&mut ctx))) };
}

/// The OBUs of a codec configuration, after checking the format it gives.
fn config_obus(config: &[u8]) -> Result<&[u8]> {
    if config.is_empty() {
        return Ok(config);
    }
    if config[0] & 0x80 == 0 {
        // no av1C marker bit: bare OBUs, as some early Matroska files have
        return Ok(config);
    }
    if config.len() < 4 {
        return Err(Error::Bitstream("av1C record too short"));
    }
    // seq_tier_0, high_bitdepth, twelve_bit, mono_chrome, chroma_subsampling_x/y, ...
    let b = config[2];
    let (twelve, mono, ssx, ssy) = (b & 0x20 != 0, b & 0x10 != 0, b & 0x08 != 0, b & 0x04 != 0);
    if twelve {
        return Err(Error::Unsupported("12-bit samples"));
    }
    if !mono && !(ssx && ssy) {
        return Err(Error::Unsupported(if ssx { "4:2:2 chroma" } else { "4:4:4 chroma" }));
    }
    Ok(&config[4..])
}

/// A picture from dav1d: holds a reference to its buffers until dropped.
struct Picture(Dav1dPicture);

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `dav1d_get_picture` (or is empty) and
        // is released once, here.
        unsafe { dav1d_picture_unref(Some(NonNull::from(&mut self.0))) };
    }
}

impl Picture {
    fn size(&self) -> (usize, usize) {
        (self.0.p.w.max(0) as usize, self.0.p.h.max(0) as usize)
    }

    /// The pts of the sample that made it shown (see `Decoder::send`).
    fn pts(&self) -> f64 {
        f64::from_bits(self.0.m.timestamp as u64)
    }

    fn is_key_frame(&self) -> bool {
        // SAFETY: a picture's frame header lives as long as the picture.
        self.0.frame_hdr.is_some_and(|h| unsafe { h.as_ref() }.frame_type == DAV1D_FRAME_TYPE_KEY)
    }

    /// (bt709, full_range) from the sequence header's colour config.
    fn colour(&self) -> (bool, bool) {
        // SAFETY: a picture's sequence header lives as long as the picture.
        let Some(seq) = self.0.seq_hdr.map(|s| unsafe { s.as_ref() }) else { return (self.size().1 > 576, false) };
        let bt709 = match seq.mtrx {
            // (SMPTE 240M's coefficients are within 0.015 of BT.709's)
            DAV1D_MC_BT709 | DAV1D_MC_BT2020_NCL | DAV1D_MC_BT2020_CL | DAV1D_MC_SMPTE240 => true,
            DAV1D_MC_BT470BG | DAV1D_MC_BT601 | DAV1D_MC_FCC => false,
            // unspecified (and the rest): as players guess
            _ => self.size().1 > 576,
        };
        (bt709, seq.color_range != 0)
    }

    /// Bits per sample, after checking the picture is a format we convert.
    fn check(&self) -> Result<u8> {
        match self.0.p.layout {
            DAV1D_PIXEL_LAYOUT_I420 | DAV1D_PIXEL_LAYOUT_I400 => {}
            DAV1D_PIXEL_LAYOUT_I422 => return Err(Error::Unsupported("4:2:2 chroma")),
            _ => return Err(Error::Unsupported("4:4:4 chroma")),
        }
        match self.0.p.bpc {
            8 => Ok(8),
            10 => Ok(10),
            _ => Err(Error::Unsupported("12-bit samples")),
        }
    }

    fn has_chroma(&self) -> bool {
        self.0.p.layout != DAV1D_PIXEL_LAYOUT_I400
    }

    /// Plane `i` (0 luma, 1 and 2 chroma) of `rows` rows of `row_bytes`
    /// bytes: its bytes and its stride.
    fn plane(&self, i: usize, rows: usize, row_bytes: usize) -> Result<(&[u8], usize)> {
        if rows == 0 || row_bytes == 0 {
            return Ok((&[], 0));
        }
        let (Some(ptr), stride) = (self.0.data[i], self.0.stride[(i > 0) as usize]) else {
            return Err(Error::Bitstream("picture without a plane"));
        };
        if stride < row_bytes as isize {
            // (never from dav1d's own allocator)
            return Err(Error::Unsupported("picture layout"));
        }
        let stride = stride as usize;
        // SAFETY: dav1d's planes hold `rows` rows `stride` bytes apart, each
        // at least `row_bytes` long, alive while this picture holds its
        // reference (the returned slice borrows `self`).
        let bytes = unsafe { std::slice::from_raw_parts(ptr.as_ptr() as *const u8, stride * (rows - 1) + row_bytes) };
        Ok((bytes, stride))
    }
}

/// A picture's planes, tightly packed as in [`Frame`] (chroma `grey` where
/// it has none), each row of samples made by `row` from the picture's bytes
/// of it (two a sample above 8 bits).
fn read_planes<T: Copy + Default>(pic: &Picture, bits: u8, grey: T, row: impl Fn(&[u8], &mut [T])) -> Result<[Vec<T>; 3]> {
    let (w, h) = pic.size();
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let bytes = if bits == 8 { 1 } else { 2 };
    let mut planes = [vec![T::default(); w * h], vec![grey; cw * ch], vec![grey; cw * ch]];
    let n = if pic.has_chroma() { 3 } else { 1 };
    for (i, dst) in planes.iter_mut().enumerate().take(n) {
        let (pw, ph) = if i == 0 { (w, h) } else { (cw, ch) };
        let (src, stride) = pic.plane(i, ph, pw * bytes)?;
        for (r, out) in dst.chunks_exact_mut(pw.max(1)).take(ph).enumerate() {
            row(&src[r * stride..][..pw * bytes], out);
        }
    }
    Ok(planes)
}

fn to_frame(pic: &Picture, pts: f64, damaged: bool) -> Result<Frame> {
    let bits = pic.check()?;
    let (w, h) = pic.size();
    let (bt709, full_range) = pic.colour();
    let [y, u, v] = read_planes(pic, bits, 128, |row, out| {
        if bits == 8 {
            out.copy_from_slice(row);
        } else {
            for (o, s) in out.iter_mut().zip(row.chunks_exact(2)) {
                *o = to_8bit(u16::from_ne_bytes([s[0], s[1]]));
            }
        }
    })?;
    Ok(Frame { width: w as u32, height: h as u32, y, u, v, pts, damaged, bt709, full_range })
}

/// A 10-bit sample rounded to 8 bits.
#[inline]
pub fn to_8bit(v: u16) -> u8 {
    ((v as u32 + 2) >> 2).min(255) as u8
}

fn to_raw(pic: &Picture, pts: f64, damaged: bool) -> Result<RawFrame> {
    let bits = pic.check()?;
    let (w, h) = pic.size();
    let [y, u, v] = read_planes(pic, bits, 1u16 << (bits - 1), |row, out| {
        if bits == 8 {
            out.iter_mut().zip(row).for_each(|(o, &s)| *o = s as u16);
        } else {
            out.iter_mut().zip(row.chunks_exact(2)).for_each(|(o, s)| *o = u16::from_ne_bytes([s[0], s[1]]));
        }
    })?;
    Ok(RawFrame { width: w as u32, height: h as u32, bit_depth: bits, y, u, v, pts, damaged })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_to_8_bits() {
        assert_eq!(to_8bit(0), 0);
        assert_eq!(to_8bit(1), 0);
        assert_eq!(to_8bit(2), 1);
        assert_eq!(to_8bit(5), 1);
        assert_eq!(to_8bit(6), 2);
        assert_eq!(to_8bit(512), 128);
        assert_eq!(to_8bit(1020), 255);
        assert_eq!(to_8bit(1021), 255);
        assert_eq!(to_8bit(1023), 255);
    }

    #[test]
    fn configuration_records() {
        // 8-bit 4:2:0 without configOBUs; with one; bare OBUs; empty
        assert_eq!(config_obus(&[0x81, 0x08, 0x0c, 0x00]).unwrap(), &[] as &[u8]);
        assert_eq!(config_obus(&[0x81, 0x08, 0x0c, 0x00, 0x0a, 0x01, 0x00]).unwrap(), &[0x0a, 0x01, 0x00]);
        assert_eq!(config_obus(&[0x0a, 0x01, 0x00]).unwrap(), &[0x0a, 0x01, 0x00]);
        assert!(config_obus(&[]).unwrap().is_empty());
        // 10-bit is fine, monochrome too; 12-bit, 4:2:2 and 4:4:4 are not
        assert!(config_obus(&[0x81, 0x08, 0x4c, 0x00]).is_ok());
        assert!(config_obus(&[0x81, 0x08, 0x1c, 0x00]).is_ok());
        assert_eq!(config_obus(&[0x81, 0x48, 0x6c, 0x00]), Err(Error::Unsupported("12-bit samples")));
        assert_eq!(config_obus(&[0x81, 0x48, 0x08, 0x00]), Err(Error::Unsupported("4:2:2 chroma")));
        assert_eq!(config_obus(&[0x81, 0x28, 0x00, 0x00]), Err(Error::Unsupported("4:4:4 chroma")));
        assert_eq!(config_obus(&[0x81, 0x08]), Err(Error::Bitstream("av1C record too short")));
    }

    #[test]
    fn empty_config_and_empty_samples() {
        let mut d = Decoder::new(&[]).unwrap();
        assert!(d.decode(&[], 0.0).unwrap().is_empty());
        assert!(d.flush().unwrap().is_empty());
    }

    #[test]
    fn garbage_without_a_picture_is_an_error() {
        let mut d = Decoder::new(&[]).unwrap();
        // a frame OBU with nothing before it: no sequence header
        let r = d.decode(&[0x32, 0x04, 0x10, 0x00, 0x00, 0x00], 0.5);
        assert!(matches!(r, Err(Error::Bitstream(_))), "{r:?}");
    }
}
