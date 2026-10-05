//! The built-in decoders for the codecs a browser's WebCodecs may lack
//! (HEVC, VP9, VP8 and AV1; H.264's is in the main module; and the sound
//! decoders for AC-3, E-AC-3 and DTS, `SoundDecoder`), as a WebAssembly
//! module of their own: the page loads it only for a file that needs one,
//! and each decode worker (`web/softworker.js`) runs one decoder over a
//! group of pictures at a time. Every decoder gives 8-bit 4:2:0 pictures
//! (deeper ones rounded), bit-exact with ffmpeg's for what it supports; here
//! each is made the detector's size (`set_shrink`, from the decoder's own
//! planes, as a decode worker makes a browser decoder's picture small) or
//! copied out as packed I420.

use std::collections::VecDeque;

use unflash_core::resample::Shrink;
use wasm_bindgen::prelude::*;

fn js_err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// A picture as every decoder here gives it: tightly packed 8-bit 4:2:0
/// planes (chroma (w + 1) / 2 by (h + 1) / 2), and the sample it was shown
/// by (the decoders carry the number `decode` was given as its time).
struct Picture {
    width: u32,
    height: u32,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    index: u32,
    damaged: bool,
    bt709: bool,
    full_range: bool,
}

macro_rules! pictures {
    ($frames:expr) => {
        $frames.into_iter().map(|f| Picture { width: f.width, height: f.height, y: f.y, u: f.u, v: f.v, index: f.pts as u32, damaged: f.damaged, bt709: f.bt709, full_range: f.full_range })
    };
}

enum Inner {
    Hevc(unflash_hevc::Decoder),
    Vp9(unflash_vp9::Decoder),
    Vp8(unflash_vp8::Decoder),
    Av1(unflash_av1::Decoder),
}

impl Inner {
    /// A decoder for `codec` (the app's name: hevc, vp9, vp8, av1) and the
    /// track's configuration record (hvcC, vpcC, av1C or Matroska's
    /// CodecPrivate; may be empty).
    fn new(codec: &str, config: &[u8]) -> Result<Inner, String> {
        Ok(match codec {
            "hevc" => Inner::Hevc(unflash_hevc::Decoder::new(config).map_err(|x| x.to_string())?),
            "vp9" => Inner::Vp9(unflash_vp9::Decoder::new(config).map_err(|x| x.to_string())?),
            "vp8" => Inner::Vp8(unflash_vp8::Decoder::new(config).map_err(|x| x.to_string())?),
            "av1" => Inner::Av1(unflash_av1::Decoder::new(config).map_err(|x| x.to_string())?),
            other => return Err(format!("there is no built-in decoder for {other}")),
        })
    }

    /// Decode sample number `index`; the pictures it completes join `out`.
    fn decode(&mut self, sample: &[u8], index: u32, out: &mut VecDeque<Picture>) -> Result<(), String> {
        let t = index as f64;
        match self {
            Inner::Hevc(d) => out.extend(pictures!(d.decode(sample, t).map_err(|e| e.to_string())?)),
            Inner::Vp9(d) => out.extend(pictures!(d.decode(sample, t).map_err(|e| e.to_string())?)),
            Inner::Vp8(d) => out.extend(pictures!(d.decode(sample, t).map_err(|e| e.to_string())?)),
            Inner::Av1(d) => out.extend(pictures!(d.decode(sample, t).map_err(|e| e.to_string())?)),
        }
        Ok(())
    }

    /// The pictures still held at the end of the stream join `out`.
    fn flush(&mut self, out: &mut VecDeque<Picture>) -> Result<(), String> {
        match self {
            Inner::Hevc(d) => out.extend(pictures!(d.flush().map_err(|e| e.to_string())?)),
            Inner::Vp9(d) => out.extend(pictures!(d.flush().map_err(|e| e.to_string())?)),
            Inner::Vp8(d) => out.extend(pictures!(d.flush().map_err(|e| e.to_string())?)),
            Inner::Av1(d) => out.extend(pictures!(d.flush().map_err(|e| e.to_string())?)),
        }
        Ok(())
    }

    /// How many pictures may come out before one shown earlier: HEVC's in
    /// decoding order; the others' come in presentation order.
    fn reorder_depth(&self) -> u32 {
        match self {
            Inner::Hevc(d) => d.reorder_depth(),
            _ => 0,
        }
    }
}

/// One of the built-in decoders, sample by sample: `decode` (or `flush`)
/// says how many pictures are ready, `next` takes the next one, and the
/// picture's accessors read it: made small (`small`, after `set_shrink`) or
/// whole (packed I420 at `frame_ptr`, in this module's memory).
#[wasm_bindgen]
pub struct SoftDecoder {
    inner: Inner,
    ready: VecDeque<Picture>,
    cur: Option<Picture>,
    /// The current picture as packed I420, when pictures are not made small.
    frame: Vec<u8>,
    /// The analysis size pictures are made small to, and the last one made.
    shrink: Option<(u32, u32)>,
    shrinker: Option<Shrink>,
    small: Vec<u8>,
}

#[wasm_bindgen]
impl SoftDecoder {
    /// A decoder for `codec` (hevc, vp9, vp8 or av1) and the track's
    /// configuration record (`config`).
    #[wasm_bindgen(constructor)]
    pub fn new(codec: &str, config: &[u8]) -> Result<SoftDecoder, JsValue> {
        Ok(SoftDecoder { inner: Inner::new(codec, config).map_err(js_err)?, ready: VecDeque::new(), cur: None, frame: Vec::new(), shrink: None, shrinker: None, small: Vec::new() })
    }

    /// From now on make each picture `analysis_width`×`analysis_height`
    /// RGBA8 here, with the colour conversion its stream says (BT.709 or
    /// BT.601, full or limited range), instead of copying it out whole.
    pub fn set_shrink(&mut self, analysis_width: u32, analysis_height: u32) {
        self.shrink = Some((analysis_width.max(1), analysis_height.max(1)));
    }

    /// Decode one sample (`index`: its number, which the picture it shows
    /// carries back in `frame_pts`). Returns how many pictures are ready.
    pub fn decode(&mut self, sample: &[u8], index: u32) -> Result<u32, JsValue> {
        self.inner.decode(sample, index, &mut self.ready).map_err(js_err)?;
        Ok(self.ready.len() as u32)
    }

    /// The pictures still held at the end; returns how many are ready.
    pub fn flush(&mut self) -> Result<u32, JsValue> {
        self.inner.flush(&mut self.ready).map_err(js_err)?;
        Ok(self.ready.len() as u32)
    }

    /// How many pictures may come out before one shown earlier (a caller
    /// holds that many back to hand them on in presentation order).
    pub fn reorder_depth(&self) -> u32 {
        self.inner.reorder_depth()
    }

    /// Take the next ready picture; false when there is none.
    pub fn next(&mut self) -> bool {
        let Some(p) = self.ready.pop_front() else {
            self.cur = None;
            return false;
        };
        let (w, h) = (p.width as usize, p.height as usize);
        let cw = w.div_ceil(2);
        match self.shrink {
            Some((aw, ah)) => {
                let k = Shrink::reuse(&mut self.shrinker, p.width, p.height, aw, ah);
                k.yuv420_planes(&p.y, w, &p.u, cw, &p.v, cw, p.bt709, p.full_range, &mut self.small);
            }
            None => {
                self.frame.clear();
                self.frame.extend_from_slice(&p.y[..w * h]);
                let ch = h.div_ceil(2);
                self.frame.extend_from_slice(&p.u[..cw * ch]);
                self.frame.extend_from_slice(&p.v[..cw * ch]);
            }
        }
        self.cur = Some(p);
        true
    }

    pub fn width(&self) -> u32 {
        self.cur.as_ref().map_or(0, |p| p.width)
    }
    pub fn height(&self) -> u32 {
        self.cur.as_ref().map_or(0, |p| p.height)
    }
    /// The number of the sample the current picture was shown by.
    pub fn frame_pts(&self) -> u32 {
        self.cur.as_ref().map_or(0, |p| p.index)
    }
    pub fn frame_damaged(&self) -> bool {
        self.cur.as_ref().is_some_and(|p| p.damaged)
    }
    /// The current picture as packed I420 (Y, then Cb, then Cr, no padding)
    /// in this module's memory: its address and length in bytes.
    pub fn frame_ptr(&self) -> *const u8 {
        self.frame.as_ptr()
    }
    pub fn frame_len(&self) -> u32 {
        self.frame.len() as u32
    }
    /// The current picture made small (RGBA8 at the analysis size).
    pub fn small(&self) -> Vec<u8> {
        self.small.clone()
    }
    /// The current picture's colour space as a `VideoColorSpaceInit` JSON object.
    pub fn color_json(&self) -> String {
        let (bt709, full) = self.cur.as_ref().map_or((true, false), |p| (p.bt709, p.full_range));
        let m = if bt709 { "bt709" } else { "smpte170m" };
        serde_json::json!({ "primaries": m, "transfer": m, "matrix": m, "fullRange": full }).to_string()
    }
}

/// Whether the built-in decoder for `codec` can take a track with this
/// configuration record: an empty JSON object (the page reads nothing from
/// it), or an error saying why not (an unsupported profile, say).
#[wasm_bindgen]
pub fn probe(codec: &str, config: &[u8]) -> Result<String, JsValue> {
    Inner::new(codec, config).map_err(js_err)?;
    Ok("{}".into())
}

/// The sound no browser's WebCodecs decodes, for the section player to play
/// and the export to re-encode (`web/audiodec.js`): AC-3 and E-AC-3 (Dolby
/// Digital, Dolby Digital Plus), and DTS (the core of DTS Coherent
/// Acoustics, which every DTS, DTS-ES and DTS-HD stream with a core
/// carries; DTS-HD's extensions are stepped over, and a stream without a
/// core, DTS-HD Master Audio or DTS Express without one, is an error that
/// says so). Each `decode` takes a whole number of frames (an MP4 sample, a
/// Matroska block, a transport stream's PES payload) and gives their
/// samples mixed down to stereo (DTS's Lo/Ro with the stream's own
/// coefficients when it has them) as f32 planes, one after the other, as an
/// AudioData of format f32-planar takes them.
#[wasm_bindgen]
pub struct SoundDecoder {
    dec: Sound,
    out: Vec<Vec<f32>>,
    samples: usize,
    rate: u32,
}

enum Sound {
    // (boxed: the decoders' states differ in size, about 20 and 5 KB)
    Ac3(Box<unflash_sound::ac3::Decoder>),
    Dts(Box<unflash_sound::dts::Decoder>),
}

#[wasm_bindgen]
impl SoundDecoder {
    /// A decoder for `codec`: `ac3` (AC-3 and E-AC-3) or `dts`.
    #[wasm_bindgen(constructor)]
    pub fn new(codec: &str) -> Result<SoundDecoder, JsValue> {
        let dec = match codec {
            "ac3" => Sound::Ac3(Box::new(unflash_sound::ac3::Decoder::new(unflash_sound::Output::Stereo))),
            "dts" => Sound::Dts(Box::new(unflash_sound::dts::Decoder::new(unflash_sound::Output::Stereo))),
            other => return Err(js_err(format!("there is no built-in sound decoder for {other}"))),
        };
        Ok(SoundDecoder { dec, out: Vec::new(), samples: 0, rate: 0 })
    }

    /// Decode `data`'s frames: every channel's samples, one plane after another.
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<f32>, JsValue> {
        for plane in &mut self.out {
            plane.clear();
        }
        let (samples, rate) = match &mut self.dec {
            Sound::Ac3(d) => (d.decode(data, &mut self.out).map_err(js_err)?.samples, d.info().map(|i| i.sample_rate)),
            Sound::Dts(d) => (d.decode(data, &mut self.out).map_err(js_err)?.samples, d.info().map(|i| i.sample_rate)),
        };
        self.samples = samples;
        if let Some(rate) = rate {
            self.rate = rate;
        }
        Ok(self.out.iter().flat_map(|p| p.iter().copied()).collect())
    }

    /// Samples each channel had in the last `decode`.
    pub fn samples(&self) -> u32 {
        self.samples as u32
    }

    /// Channels of the last `decode`'s output.
    pub fn channels(&self) -> u32 {
        self.out.len() as u32
    }

    /// The sample rate (Hz) of the last frame decoded (0 before a frame has
    /// decoded: those before came out as silence).
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }
}

/// This module's memory, so JavaScript can read a picture in place
/// (`SoftDecoder::frame_ptr`).
#[wasm_bindgen]
pub fn wasm_memory() -> JsValue {
    wasm_bindgen::memory()
}
