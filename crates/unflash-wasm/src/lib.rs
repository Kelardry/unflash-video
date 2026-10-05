//! JavaScript-facing API. Everything structured crosses the boundary as
//! JSON strings (small, and the app keeps the parsed objects); bulk data
//! (frames, sample tables) crosses as typed arrays.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use serde::Serialize;
use unflash_core::blend;
use unflash_core::config::{DetectorConfig, Profile};
use unflash_core::detector::{CpuStage, Detector as CoreDetector};
use unflash_core::editing::{self, Edits, FrameSource, Prefer, SuggestStep};
use unflash_core::grid::{FrameInput, GridGeometry};
use unflash_core::resample::Shrink;
use unflash_core::temporal::{AnalysisResult, FrameRecord, Violation};
use unflash_core::yuv::YuvLayout;
use unflash_core::{sections, timeline};
use unflash_gpu::{FrameSource as GpuSource, GpuContext, GpuStage};
use unflash_h264::yuv::to_i420;
use unflash_mp4::demux::TrackKind;
use wasm_bindgen::prelude::*;

fn js_err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

fn to_json<T: Serialize>(v: &T) -> Result<String, JsValue> {
    serde_json::to_string(v).map_err(js_err)
}

fn parse_cfg(json: &str) -> Result<DetectorConfig, JsValue> {
    serde_json::from_str(json).map_err(|e| js_err(format!("bad detector config: {e}")))
}

fn parse_edits(json: &str) -> Result<Edits, JsValue> {
    if json.trim().is_empty() {
        return Ok(Edits::new());
    }
    serde_json::from_str(json).map_err(|e| js_err(format!("bad edits: {e}")))
}

fn parse_only(json: Option<String>) -> Result<Option<BTreeSet<usize>>, JsValue> {
    match json {
        None => Ok(None),
        Some(s) if s.trim().is_empty() || s.trim() == "null" => Ok(None),
        Some(s) => {
            let v: Vec<usize> = serde_json::from_str(&s).map_err(|e| js_err(format!("bad selection: {e}")))?;
            Ok(Some(v.into_iter().collect()))
        }
    }
}

/// Frames marked keep, as a JSON array of ordinals (absent: none).
fn parse_keep(json: Option<String>) -> Result<BTreeSet<usize>, JsValue> {
    Ok(parse_only(json)?.unwrap_or_default())
}

fn parse_violations(json: &str) -> Result<Vec<Violation>, JsValue> {
    serde_json::from_str(json).map_err(|e| js_err(format!("bad violations: {e}")))
}

fn parse_result(json: &str) -> Result<AnalysisResult, JsValue> {
    serde_json::from_str(json).map_err(|e| js_err(format!("bad analysis result: {e}")))
}

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

// ---- configuration --------------------------------------------------------

#[wasm_bindgen]
pub fn profile_config(name: &str) -> Result<String, JsValue> {
    let p = Profile::from_name(name).ok_or_else(|| js_err(format!("unknown profile {name}")))?;
    to_json(&p.config())
}

#[wasm_bindgen]
pub fn profile_name(config_json: &str) -> Result<String, JsValue> {
    Ok(parse_cfg(config_json)?.profile_name().to_string())
}

#[wasm_bindgen]
pub fn config_signature(config_json: &str) -> Result<String, JsValue> {
    Ok(parse_cfg(config_json)?.signature())
}

#[wasm_bindgen]
pub fn context_seconds(config_json: &str) -> Result<f64, JsValue> {
    Ok(sections::context_seconds(&parse_cfg(config_json)?))
}

#[wasm_bindgen]
pub fn safe_picture_rate(config_json: &str) -> Result<f64, JsValue> {
    Ok(sections::safe_picture_rate(&parse_cfg(config_json)?).0)
}

#[wasm_bindgen]
pub fn rate_is_guaranteed(config_json: &str, fps: f64) -> Result<bool, JsValue> {
    Ok(sections::rate_is_guaranteed(&parse_cfg(config_json)?, fps))
}

// ---- sections and timelines ----------------------------------------------

#[wasm_bindgen]
pub fn violations_to_sections(violations_json: &str, config_json: &str, ts_min: f64, ts_max: f64, keyframes: &[f64]) -> Result<String, JsValue> {
    let v = parse_violations(violations_json)?;
    let cfg = parse_cfg(config_json)?;
    to_json(&sections::violations_to_sections(&v, &cfg, (ts_min, ts_max), keyframes))
}

/// Join the results of segments scanned in parallel (JSON `[{from, result}]`,
/// in order, each result with its per-frame statistics) into the result one
/// run over the whole file gives.
#[wasm_bindgen]
pub fn merge_scan_segments(config_json: &str, src_width: u32, src_height: u32, segments_json: &str) -> Result<String, JsValue> {
    let cfg = parse_cfg(config_json)?;
    let segs: Vec<unflash_core::temporal::Segment> = serde_json::from_str(segments_json).map_err(|e| js_err(format!("bad segments: {e}")))?;
    let (aw, ah) = cfg.analysis_dims(src_width, src_height);
    to_json(&unflash_core::temporal::merge_segments(&cfg, &GridGeometry::new(&cfg, aw, ah), &segs))
}

#[wasm_bindgen]
pub fn timeline_summary(result_json: &str, ts_min: f64, ts_max: f64, bin_seconds: f64) -> Result<String, JsValue> {
    let r = parse_result(result_json)?;
    to_json(&sections::timeline_summary(&r, (ts_min, ts_max), bin_seconds))
}

#[wasm_bindgen]
pub fn sanitize_deltas(times: &[f64], max_gap: f64) -> String {
    let (out, fixed) = timeline::sanitize_deltas(times, max_gap);
    serde_json::json!({ "times": out, "fixed": fixed }).to_string()
}

#[wasm_bindgen]
pub fn shown_pts(pts: &[f64], start: f64, end: f64) -> Vec<f64> {
    timeline::shown_pts(pts, start, end)
}

#[wasm_bindgen]
pub fn section_timeline(pts: &[f64], start: f64, end: f64) -> Result<String, JsValue> {
    to_json(&timeline::section_timeline(pts, start, end))
}

#[wasm_bindgen]
pub fn median_dt(pts: &[f64]) -> f64 {
    timeline::median_dt(pts)
}

#[wasm_bindgen]
pub fn format_time(t: f64) -> String {
    timeline::format_time(t)
}

#[wasm_bindgen]
pub fn parse_time(s: &str) -> Option<f64> {
    timeline::parse_time(s)
}

// ---- editing ---------------------------------------------------------------

#[wasm_bindgen]
pub fn replacement_map(edits_json: &str, n: u32) -> Result<Vec<u32>, JsValue> {
    let e = parse_edits(edits_json)?;
    Ok(editing::replacement_map(&e, n as usize).into_iter().map(|v| v as u32).collect())
}

#[wasm_bindgen]
pub fn edited_sequence(rel_pts: &[f64], edits_json: &str, extension_seconds: f64) -> Result<String, JsValue> {
    let e = parse_edits(edits_json)?;
    let seq = editing::edited_sequence(rel_pts, &e, extension_seconds);
    let t: Vec<f64> = seq.iter().map(|s| s.0).collect();
    let src: Vec<usize> = seq.iter().map(|s| s.1).collect();
    Ok(serde_json::json!({ "t": t, "src": src }).to_string())
}

/// A section's holds (see `editing::holds`) as JSON `[{at, seconds}]`, in
/// the section's relative time; `end` is its length (where its last
/// frame's hold goes).
#[wasm_bindgen]
pub fn section_holds(rel_pts: &[f64], edits_json: &str, extension_seconds: f64, end: f64) -> Result<String, JsValue> {
    let e = parse_edits(edits_json)?;
    serde_json::to_string(&editing::holds(rel_pts, &e, extension_seconds, end)).map_err(js_err)
}

#[wasm_bindgen]
pub fn flagged_frames(seq_times: &[f64], violations_json: &str) -> Result<Vec<u32>, JsValue> {
    let v = parse_violations(violations_json)?;
    let seq: Vec<(f64, usize)> = seq_times.iter().enumerate().map(|(i, &t)| (t, i)).collect();
    Ok(editing::flagged_frames(&seq, &v).into_iter().map(|x| x as u32).collect())
}

/// A section check's violations by where they land (see `editing::classify`).
#[wasm_bindgen]
pub fn classify(config_json: &str, result_json: &str, end_disp: f64, next_at: Option<f64>) -> Result<String, JsValue> {
    let cfg = parse_cfg(config_json)?;
    let r = parse_result(result_json)?;
    to_json(&editing::classify(&r, end_disp, next_at, cfg.area_accum_window))
}

#[wasm_bindgen]
pub fn rate_proposal(
    config_json: &str,
    rel_pts: &[f64],
    edits_json: &str,
    only_json: Option<String>,
    fps: Option<f64>,
    extension_seconds: f64,
    keep_json: Option<String>,
) -> Result<String, JsValue> {
    let cfg = parse_cfg(config_json)?;
    let e = parse_edits(edits_json)?;
    let only = parse_only(only_json)?;
    let keep = parse_keep(keep_json)?;
    to_json(&editing::rate_proposal(&cfg, rel_pts, &e, only.as_ref(), &keep, fps, extension_seconds).map_err(js_err)?)
}

/// The note for a `rate_proposal` once its check is in.
#[wasm_bindgen]
pub fn rate_note(proposal_json: &str, safe: bool) -> Result<String, JsValue> {
    let p: editing::RateProposal = serde_json::from_str(proposal_json).map_err(|e| js_err(format!("bad rate proposal: {e}")))?;
    Ok(editing::rate_note(&p, safe))
}

#[wasm_bindgen]
/// Merge a suggestion into a section's marks: it replaces the marks inside
/// its scope, except on frames marked keep, whose marks stay.
pub fn apply_suggestion(existing_json: &str, suggested_json: &str, only_json: Option<String>, keep_json: Option<String>) -> Result<String, JsValue> {
    let ex = parse_edits(existing_json)?;
    let su = parse_edits(suggested_json)?;
    let only = parse_only(only_json)?;
    let keep = parse_keep(keep_json)?;
    to_json(&editing::apply_suggestion(&ex, &su, only.as_ref(), &keep))
}

// ---- frame cache -----------------------------------------------------------

/// Analysis-resolution frames of a section (RGB8), in WASM memory.
#[wasm_bindgen]
pub struct FrameCache {
    width: u32,
    height: u32,
    /// RGB8, three bytes per pixel: a quarter less memory than the RGBA the
    /// captures arrive as, and all the detector reads.
    data: Vec<u8>,
    n: usize,
}

#[wasm_bindgen]
impl FrameCache {
    #[wasm_bindgen(constructor)]
    pub fn new(width: u32, height: u32) -> FrameCache {
        FrameCache { width, height, data: Vec::new(), n: 0 }
    }

    /// Add a frame, given as RGBA8 (as captures come) or RGB8.
    pub fn push(&mut self, pixels: &[u8]) -> Result<u32, JsValue> {
        let px = (self.width * self.height) as usize;
        if pixels.len() == px * 4 {
            self.data.reserve(px * 3);
            for p in pixels.chunks_exact(4) {
                self.data.extend_from_slice(&p[..3]);
            }
        } else if pixels.len() == px * 3 {
            self.data.extend_from_slice(pixels);
        } else {
            return Err(js_err(format!("frame has {} bytes, expected {} (RGBA) or {} (RGB)", pixels.len(), px * 4, px * 3)));
        }
        self.n += 1;
        Ok(self.n as u32 - 1)
    }

    pub fn len(&self) -> u32 {
        self.n as u32
    }
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn byte_length(&self) -> f64 {
        self.data.len() as f64
    }

    /// A copy of frame `i` as RGBA8 (opaque), ready for an ImageData.
    pub fn frame(&self, i: u32) -> Result<Vec<u8>, JsValue> {
        let f = self.frame_ref(i as usize).ok_or_else(|| js_err("no such frame"))?;
        let mut out = Vec::with_capacity(f.len() / 3 * 4);
        for p in f.chunks_exact(3) {
            out.extend_from_slice(p);
            out.push(255);
        }
        Ok(out)
    }

    /// Move every frame of `other` (same size) onto the end of this cache,
    /// leaving `other` empty: how the spans of a section decoded side by
    /// side are joined in order.
    pub fn append(&mut self, other: &mut FrameCache) -> Result<(), JsValue> {
        if other.width != self.width || other.height != self.height {
            return Err(js_err(format!("cannot join a {}×{} cache onto a {}×{} one", other.width, other.height, self.width, self.height)));
        }
        if self.n == 0 {
            std::mem::swap(&mut self.data, &mut other.data);
        } else {
            self.data.append(&mut other.data);
        }
        self.n += other.n;
        other.data = Vec::new();
        other.n = 0;
        Ok(())
    }

    /// A copy of this cache with the frames whose `mask` entry is non-zero
    /// blended with their unmarked neighbours at `strength` (0 to 1; see
    /// `unflash_core::blend`).
    pub fn blended(&self, mask: &[u8], strength: f32) -> FrameCache {
        let marked: Vec<bool> = (0..self.n).map(|i| mask.get(i).map(|&m| m != 0).unwrap_or(false)).collect();
        let sources = blend::blend_sources(&marked);
        let fs = (self.width * self.height * 3) as usize;
        let frame = |i: usize| &self.data[i * fs..(i + 1) * fs];
        let mut out = FrameCache { width: self.width, height: self.height, data: Vec::with_capacity(self.data.len()), n: self.n };
        let mut tmp = Vec::new();
        for (i, src) in sources.iter().enumerate() {
            match src {
                Some(src) => {
                    blend::mix(&mut tmp, frame(i), src.prev.map(frame), src.next.map(frame), blend::blend_weights(src, strength));
                    out.data.extend_from_slice(&tmp);
                }
                None => out.data.extend_from_slice(frame(i)),
            }
        }
        out
    }

    /// A copy of this cache with the frames whose `mask` entry is non-zero
    /// blurred (three box passes of `radius`); the others are copied as
    /// they are. A short or empty mask blurs every frame.
    pub fn blurred(&self, radius: u32, mask: &[u8]) -> FrameCache {
        let fs = (self.width * self.height * 3) as usize;
        let mut out = FrameCache { width: self.width, height: self.height, data: Vec::with_capacity(self.data.len()), n: self.n };
        let mut tmp = Vec::new();
        for i in 0..self.n {
            let f = &self.data[i * fs..(i + 1) * fs];
            if mask.is_empty() || i >= mask.len() || mask[i] != 0 {
                unflash_core::resample::blur_rgb(f, self.width, self.height, radius, &mut tmp);
                out.data.extend_from_slice(&tmp);
            } else {
                out.data.extend_from_slice(f);
            }
        }
        out
    }
}

impl FrameCache {
    fn frame_ref(&self, i: usize) -> Option<&[u8]> {
        let fs = (self.width * self.height * 3) as usize;
        if i >= self.n {
            return None;
        }
        Some(&self.data[i * fs..(i + 1) * fs])
    }
}

impl FrameSource for FrameCache {
    fn frame(&self, i: usize) -> &[u8] {
        self.frame_ref(i).expect("frame index out of range")
    }
    fn bpp(&self) -> usize {
        3
    }
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
}

/// The frames of a section to blend, from a check of it (`result_json`):
/// `{"frames": [...], "side": "light" | "dark"}`, the frames on the
/// flashing's minority side (`tight`: only the most extreme of them).
#[wasm_bindgen]
pub fn blend_candidates(rel_pts: &[f64], result_json: &str, frames: &FrameCache, only_json: Option<String>, keep_json: Option<String>, tight: bool) -> Result<String, JsValue> {
    let result = parse_result(result_json)?;
    let only = parse_only(only_json)?;
    let keep = parse_keep(keep_json)?;
    let (idx, side) = editing::flash_frames(rel_pts.to_vec(), frames, &result, only, keep, tight);
    Ok(serde_json::json!({ "frames": idx, "side": side }).to_string())
}

// ---- suggester -------------------------------------------------------------

#[wasm_bindgen]
pub struct Suggester {
    inner: editing::Suggester,
}

#[wasm_bindgen]
impl Suggester {
    #[wasm_bindgen(constructor)]
    /// `only_json`: the ordinals it may touch (absent: all); `keep_json`:
    /// frames it must never remove.
    pub fn new(rel_pts: &[f64], edits_json: &str, prefer: &str, only_json: Option<String>, keep_json: Option<String>) -> Result<Suggester, JsValue> {
        let e = parse_edits(edits_json)?;
        let prefer = match prefer {
            "light" => Prefer::Light,
            "fewest" => Prefer::Fewest,
            "dark" => Prefer::Dark,
            other => return Err(js_err(format!("prefer must be light, dark or fewest, not {other}"))),
        };
        let only = parse_only(only_json)?;
        let keep = parse_keep(keep_json)?;
        Ok(Suggester { inner: editing::Suggester::new(rel_pts.to_vec(), &e, prefer, only, keep) })
    }

    /// For the fewest removals: let frames back into long runs of removed
    /// frames, `min_gap` seconds apart (the safe picture rate's spacing),
    /// each try checked.
    pub fn thin_long_gaps(&mut self, min_gap: f64) {
        self.inner.thin_long_gaps(min_gap);
    }

    /// Returns `{"simulate": edits}` (run the check on these and call again
    /// with the classified result) or `{"done": suggestion}`.
    pub fn step(&mut self, frames: &FrameCache, result_json: Option<String>) -> Result<String, JsValue> {
        let result = match result_json {
            Some(s) => Some(parse_result(&s)?),
            None => None,
        };
        match self.inner.step(frames, result.as_ref()) {
            SuggestStep::Simulate(edits) => Ok(serde_json::json!({ "simulate": edits }).to_string()),
            SuggestStep::Done(s) => Ok(serde_json::json!({ "done": s }).to_string()),
        }
    }
}

// ---- pictures made small off the page -------------------------------------

/// Shrinks decoded pictures to the detector's analysis size where they are
/// decoded (a decode worker), so the page and the GPU never handle the
/// full-size picture: `input` hands out room in this module's memory to copy
/// a picture into (`VideoFrame.copyTo` straight into it), `packed` / `yuv`
/// shrink what is there to RGBA8 (see `unflash_core::resample::Shrink`).
#[wasm_bindgen]
pub struct Shrinker {
    inner: Shrink,
    input: Vec<u8>,
    out: Vec<u8>,
}

#[wasm_bindgen]
impl Shrinker {
    /// For pictures of `width`×`height` (neither 0).
    #[wasm_bindgen(constructor)]
    pub fn new(width: u32, height: u32, analysis_width: u32, analysis_height: u32) -> Result<Shrinker, JsValue> {
        if width == 0 || height == 0 {
            return Err(js_err(format!("a {width}×{height} picture has nothing to make small")));
        }
        Ok(Shrinker { inner: Shrink::new(width, height, analysis_width, analysis_height), input: Vec::new(), out: Vec::new() })
    }

    /// Whether it was made for these sizes.
    pub fn fits(&self, width: u32, height: u32, analysis_width: u32, analysis_height: u32) -> bool {
        self.inner.fits(width, height, analysis_width, analysis_height)
    }

    /// Room for `len` bytes of picture: its address in this module's memory
    /// (valid until the next call that may grow the memory).
    pub fn input(&mut self, len: usize) -> usize {
        if self.input.len() < len {
            self.input.resize(len, 0);
        }
        self.input.as_ptr() as usize
    }

    /// The packed picture in the input (four bytes a pixel, B first with
    /// `bgr`), rows `stride` bytes apart from `offset`: RGBA8 at the analysis size.
    pub fn packed(&mut self, offset: usize, stride: usize, bgr: bool) -> Result<Vec<u8>, JsValue> {
        let (w, h) = self.inner.source_size();
        if stride < w * 4 || self.input.len() < offset + (h - 1) * stride + w * 4 {
            return Err(js_err("picture data too short for its size"));
        }
        self.inner.packed(&self.input, offset, stride, bgr, &mut self.out);
        Ok(self.out.clone())
    }

    /// The 4:2:0 picture in the input (`layout`: the words Detector.feed_yuv takes).
    pub fn yuv(&mut self, layout: &[u32]) -> Result<Vec<u8>, JsValue> {
        let layout = YuvLayout::from_words(layout).ok_or_else(|| js_err("bad picture layout"))?;
        let (w, h) = self.inner.source_size();
        if !layout.fits(self.input.len(), w, h) {
            return Err(js_err("picture data too short for its layout"));
        }
        self.inner.yuv420(&self.input, &layout, &mut self.out);
        Ok(self.out.clone())
    }
}

// ---- demuxer ---------------------------------------------------------------

#[wasm_bindgen]
pub struct Demuxer {
    inner: unflash_mp4::Demuxer,
}

#[derive(Serialize)]
struct TrackSummary {
    index: usize,
    id: u32,
    kind: TrackKind,
    fourcc: String,
    codec: String,
    timescale: u32,
    width: u32,
    height: u32,
    sample_rate: u32,
    channels: u32,
    samples: usize,
    /// What the edit list adds to the composition times (negative for a
    /// media_time that skips into the track): the `pts` columns have it
    /// applied, so a sample's composition time in the file is pts -
    /// edit_shift.
    edit_shift: i64,
    /// Nominal ticks per sample (0: not stated).
    frame_duration: u32,
    /// Bytes to put in front of every sample read (Matroska header stripping).
    prefix: Vec<u8>,
    /// Whether an MP4 can carry the track as it is (its sample entry).
    copyable: bool,
    language: String,
    /// Why the track cannot be used, when it cannot.
    note: String,
}

#[wasm_bindgen]
impl Demuxer {
    #[wasm_bindgen(constructor)]
    pub fn new(file_size: f64) -> Demuxer {
        Demuxer { inner: unflash_mp4::Demuxer::new(file_size as u64) }
    }

    /// `[offset, length]` of the next range to feed, or an empty array when
    /// done.
    pub fn need(&self) -> Vec<f64> {
        match self.inner.need() {
            Some((o, l)) => vec![o as f64, l as f64],
            None => vec![],
        }
    }

    pub fn feed(&mut self, offset: f64, data: &[u8]) -> Result<(), JsValue> {
        self.inner.feed(offset as u64, data).map_err(js_err)
    }

    pub fn is_done(&self) -> bool {
        self.inner.is_done()
    }

    /// How far through reading the index, 0 to 1 (a Matroska file is read
    /// through, an MP4 only at its index).
    pub fn progress(&self) -> f64 {
        self.inner.progress()
    }

    /// `mp4`, `matroska` or `mpegts` once the first bytes are in.
    pub fn container(&self) -> String {
        self.inner.container().into()
    }

    /// Movie summary as JSON (tracks without their sample tables).
    pub fn movie_json(&self) -> Result<String, JsValue> {
        let m = self.inner.movie().ok_or_else(|| js_err("not parsed yet"))?;
        let tracks: Vec<TrackSummary> = m
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| TrackSummary {
                index: i,
                id: t.id,
                kind: t.kind,
                fourcc: t.fourcc.clone(),
                codec: t.codec.clone(),
                timescale: t.timescale,
                width: t.width,
                height: t.height,
                sample_rate: t.sample_rate,
                channels: t.channels,
                samples: t.samples.len(),
                edit_shift: t.edit_shift,
                frame_duration: t.frame_duration,
                prefix: t.prefix.clone(),
                copyable: t.copyable(),
                language: t.language.clone(),
                note: t.note.clone(),
            })
            .collect();
        Ok(serde_json::json!({
            "fragmented": m.fragmented,
            "format": m.format,
            "packet_size": m.packet_size,
            "tracks": tracks,
        })
        .to_string())
    }

    fn track(&self, index: u32) -> Result<&unflash_mp4::Track, JsValue> {
        let m = self.inner.movie().ok_or_else(|| js_err("not parsed yet"))?;
        m.tracks.get(index as usize).ok_or_else(|| js_err("no such track"))
    }

    pub fn track_description(&self, index: u32) -> Result<Vec<u8>, JsValue> {
        Ok(self.track(index)?.description.clone().unwrap_or_default())
    }

    pub fn track_sample_entry(&self, index: u32) -> Result<Vec<u8>, JsValue> {
        Ok(self.track(index)?.sample_entry.clone())
    }

    /// One column of a track's sample table: `offset`, `size`, `pts_us`,
    /// `dts_us`, `duration_us`, `pts_ticks`, `dts_ticks`, `duration_ticks` or
    /// `sync` (0/1).
    pub fn sample_table(&self, index: u32, field: &str) -> Result<Vec<f64>, JsValue> {
        let t = self.track(index)?;
        let v: Vec<f64> = match field {
            "offset" => t.samples.iter().map(|s| s.offset as f64).collect(),
            "size" => t.samples.iter().map(|s| s.size as f64).collect(),
            "pts_us" => t.samples.iter().map(|s| t.to_us(s.pts) as f64).collect(),
            "dts_us" => t.samples.iter().map(|s| t.to_us(s.dts) as f64).collect(),
            "duration_us" => t.samples.iter().map(|s| t.to_us(s.duration as i64) as f64).collect(),
            "pts_ticks" => t.samples.iter().map(|s| s.pts as f64).collect(),
            "dts_ticks" => t.samples.iter().map(|s| s.dts as f64).collect(),
            "duration_ticks" => t.samples.iter().map(|s| s.duration as f64).collect(),
            "sync" => t.samples.iter().map(|s| if s.sync { 1.0 } else { 0.0 }).collect(),
            other => return Err(js_err(format!("unknown sample field {other}"))),
        };
        Ok(v)
    }

    pub fn keyframe_times(&self, index: u32) -> Result<Vec<f64>, JsValue> {
        Ok(self.track(index)?.keyframe_times())
    }

    /// Decode-order index of the last keyframe at or before `t` seconds.
    pub fn sync_before(&self, index: u32, t: f64) -> Result<u32, JsValue> {
        Ok(self.track(index)?.sync_before(t) as u32)
    }
}

// ---- muxer -----------------------------------------------------------------

#[wasm_bindgen]
pub struct Muxer {
    inner: unflash_mp4::Muxer,
    patch: Option<(u64, [u8; 8])>,
}

#[wasm_bindgen]
impl Muxer {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Muxer {
        Muxer { inner: unflash_mp4::Muxer::new(), patch: None }
    }

    pub fn add_video_track(&mut self, codec: &str, width: u32, height: u32, timescale: u32, description: &[u8]) -> u32 {
        self.inner.add_track(unflash_mp4::TrackDesc::Video {
            codec: codec.to_string(),
            width,
            height,
            timescale,
            description: description.to_vec(),
        }) as u32
    }

    /// `kind` is "video" or "audio"; `sample_entry` is the source track's
    /// sample entry box (see `Demuxer.track_sample_entry`).
    pub fn add_copy_track(&mut self, kind: &str, sample_entry: &[u8], timescale: u32, width: u32, height: u32) -> u32 {
        let kind = match kind {
            "video" => TrackKind::Video,
            "audio" => TrackKind::Audio,
            _ => TrackKind::Other,
        };
        self.inner.add_track(unflash_mp4::TrackDesc::Copy { kind, sample_entry: sample_entry.to_vec(), timescale, width, height }) as u32
    }

    /// The file head to write first.
    pub fn start(&mut self) -> Vec<u8> {
        self.inner.start()
    }

    /// Record a sample whose `size` bytes the caller appended to the file.
    pub fn add_sample(&mut self, track: u32, dts: f64, pts: f64, duration: f64, sync: bool, size: f64) -> Result<(), JsValue> {
        self.inner.add_sample(track as usize, dts as i64, pts as i64, duration as u32, sync, size as u32).map_err(js_err)
    }

    /// The `moov` box to append. Afterwards `patch_offset()` / `patch_bytes()`
    /// say which 8 bytes of the head to overwrite.
    pub fn finish(&mut self) -> Result<Vec<u8>, JsValue> {
        let (moov, patch) = self.inner.finish().map_err(js_err)?;
        self.patch = Some(patch);
        Ok(moov)
    }

    pub fn patch_offset(&self) -> f64 {
        self.patch.map(|p| p.0 as f64).unwrap_or(-1.0)
    }

    pub fn patch_bytes(&self) -> Vec<u8> {
        self.patch.map(|p| p.1.to_vec()).unwrap_or_default()
    }
}

impl Default for Muxer {
    fn default() -> Self {
        Self::new()
    }
}

/// An MP4 sample entry for audio an encoder made: `codec` and
/// `description` as WebCodecs' decoderConfig gives them (AAC, Opus, FLAC).
#[wasm_bindgen]
pub fn audio_sample_entry(codec: &str, description: &[u8], sample_rate: u32, channels: u32) -> Result<Vec<u8>, JsValue> {
    unflash_mp4::entry::encoded_audio_entry(codec, description, sample_rate, channels).map_err(js_err)
}

// ---- detector --------------------------------------------------------------

enum Stage {
    Cpu(CpuStage),
    Gpu(Box<GpuStage>),
}

/// The flash detector, fed frame by frame.
#[wasm_bindgen]
pub struct Detector {
    det: CoreDetector,
    stage: Stage,
    records: Vec<FrameRecord>,
    captures: BTreeMap<usize, Vec<u8>>,
    /// capture flags of frames submitted to the GPU, in order
    pending_capture: std::collections::VecDeque<bool>,
    /// The CPU detector's pictures made the analysis size, as the decode
    /// workers make them, and the room for the last one.
    shrink: Option<Shrink>,
    small: Vec<u8>,
    /// The resolve function of the promise `gpu_wait` handed out last.
    waiter: Rc<RefCell<Option<js_sys::Function>>>,
}

#[wasm_bindgen]
impl Detector {
    /// CPU detector for a source of the given size.
    #[wasm_bindgen(constructor)]
    pub fn new(config_json: &str, src_width: u32, src_height: u32) -> Result<Detector, JsValue> {
        let cfg = parse_cfg(config_json)?;
        let det = CoreDetector::for_source(cfg.clone(), src_width, src_height);
        let stage = CpuStage::new(&cfg, det.geometry().clone());
        Ok(Detector::with_stage(det, Stage::Cpu(stage), Default::default()))
    }

    /// WebGPU detector; resolves to a `Detector` or rejects when there is no
    /// adapter. `batch` frames share one command buffer and one readback
    /// (results arrive when a batch is full or after `flush`); 1 gives a
    /// result after every frame, for the live monitor.
    #[wasm_bindgen(js_name = createGpu)]
    pub fn create_gpu(config_json: String, src_width: u32, src_height: u32, batch: Option<u32>) -> js_sys::Promise {
        wasm_bindgen_futures::future_to_promise(async move {
            let cfg = parse_cfg(&config_json)?;
            let ctx = GpuContext::new().await.map_err(js_err)?;
            let det = CoreDetector::for_source(cfg.clone(), src_width, src_height);
            let batch = batch.map(|b| b.max(1) as usize).unwrap_or(unflash_gpu::DEFAULT_BATCH);
            let mut stage = GpuStage::with_options(&ctx, &cfg, det.geometry().clone(), unflash_gpu::DEFAULT_SLOTS, batch).map_err(js_err)?;
            let waiter: Rc<RefCell<Option<js_sys::Function>>> = Default::default();
            // (in a browser; a native build of these bindings only runs tests)
            #[cfg(target_arch = "wasm32")]
            {
                let w = waiter.clone();
                stage.set_notify(Some(Rc::new(move || {
                    if let Some(resolve) = w.borrow_mut().take() {
                        let _ = resolve.call0(&JsValue::UNDEFINED);
                    }
                })));
            }
            Ok(JsValue::from(Detector::with_stage(det, Stage::Gpu(Box::new(stage)), waiter)))
        })
    }

    pub fn analysis_width(&self) -> u32 {
        self.det.geometry().aw
    }
    pub fn analysis_height(&self) -> u32 {
        self.det.geometry().ah
    }
    pub fn window_width(&self) -> u32 {
        self.det.geometry().ww
    }
    pub fn window_height(&self) -> u32 {
        self.det.geometry().wh
    }
    pub fn area_thresh(&self) -> u32 {
        self.det.geometry().area_thresh
    }
    /// Pixels a regular pattern has to cover to count.
    pub fn pattern_thresh(&self) -> u32 {
        self.det.temporal().pattern_thresh()
    }
    /// Frames submitted but not yet completed (GPU in flight).
    pub fn pending(&self) -> u32 {
        self.det.pending() as u32
    }
    pub fn can_submit(&self) -> bool {
        match &self.stage {
            Stage::Cpu(_) => true,
            Stage::Gpu(g) => g.can_submit(),
        }
    }

    /// A promise that settles once `poll` has results to collect (at once
    /// when it already has, when nothing is in flight, or on the CPU
    /// detector). Waiting on this instead of a timer keeps a scan going at
    /// full speed in a hidden tab, where timers fire once a second at most.
    pub fn gpu_wait(&self) -> js_sys::Promise {
        let ready = match &self.stage {
            Stage::Cpu(_) => true,
            Stage::Gpu(g) => g.ready(),
        };
        if ready {
            return js_sys::Promise::resolve(&JsValue::UNDEFINED);
        }
        let w = self.waiter.clone();
        js_sys::Promise::new(&mut |resolve, _reject| {
            // one waiter at a time: an earlier one is let go to look again
            if let Some(old) = w.borrow_mut().replace(resolve) {
                let _ = old.call0(&JsValue::UNDEFINED);
            }
        })
    }
    /// Approximate bytes of GPU memory traffic per frame (bandwidth model).
    pub fn bytes_per_frame(&self) -> f64 {
        match &self.stage {
            Stage::Cpu(_) => (self.det.geometry().npix() * 130) as f64,
            Stage::Gpu(g) => g.bytes_per_frame() as f64,
        }
    }

    /// Feed an RGBA8 picture of any size, stamped `t` seconds (native pts).
    pub fn feed_rgba(&mut self, rgba: &[u8], width: u32, height: u32, t: f64, capture: bool) -> Result<(), JsValue> {
        if width == 0 || height == 0 {
            return Err(js_err("empty picture"));
        }
        if rgba.len() < (width * height * 4) as usize {
            return Err(js_err("frame data too short"));
        }
        if !matches!(self.stage, Stage::Cpu(_)) {
            return self.submit(GpuSource::Rgba8 { data: rgba, width, height }, t, capture);
        }
        let g = self.det.geometry();
        if (width, height) == (g.aw, g.ah) {
            self.run_cpu(FrameInput::rgba(rgba), t, capture);
        } else {
            self.run_cpu_shrunk(width, height, t, capture, |k, out| k.packed(rgba, 0, width as usize * 4, false, out));
        }
        Ok(())
    }

    /// Feed a BGRX / BGRA picture (as some browsers' decoders give them) as
    /// it came: the GPU swaps the channels while it reads them, and so does
    /// the CPU detector's shrink.
    pub fn feed_bgra(&mut self, bgra: &[u8], width: u32, height: u32, t: f64, capture: bool) -> Result<(), JsValue> {
        if width == 0 || height == 0 {
            return Err(js_err("empty picture"));
        }
        if bgra.len() < (width * height * 4) as usize {
            return Err(js_err("frame data too short"));
        }
        if !matches!(self.stage, Stage::Cpu(_)) {
            return self.submit(GpuSource::Bgra8 { data: bgra, width, height }, t, capture);
        }
        // (at the analysis size too: it copies, swapping the channels)
        self.run_cpu_shrunk(width, height, t, capture, |k, out| k.packed(bgra, 0, width as usize * 4, true, out));
        Ok(())
    }

    /// Feed 8-bit 4:2:0 planes (I420 or NV12) of any size, as WebCodecs'
    /// `VideoFrame.copyTo` and the built-in decoder lay them out. `layout`
    /// is [format (0 I420, 1 NV12), y_off, y_stride, u_off, u_stride, v_off,
    /// v_stride, matrix (0 BT.601, 1 BT.709), full_range]. The GPU converts
    /// to RGB in a shader; the CPU detector converts and shrinks as the
    /// decode workers do.
    pub fn feed_yuv(&mut self, data: &[u8], width: u32, height: u32, layout: &[u32], t: f64, capture: bool) -> Result<(), JsValue> {
        let layout = YuvLayout::from_words(layout).ok_or_else(|| js_err("bad picture layout"))?;
        if width == 0 || height == 0 || !layout.fits(data.len(), width as usize, height as usize) {
            return Err(js_err("picture data too short for its layout"));
        }
        if !matches!(self.stage, Stage::Cpu(_)) {
            return self.submit(GpuSource::Yuv420 { data, width, height, layout }, t, capture);
        }
        self.run_cpu_shrunk(width, height, t, capture, |k, out| k.yuv420(data, &layout, out));
        Ok(())
    }

    /// Feed frame `index` of a [`FrameCache`] (already at analysis
    /// resolution) without copying it out of WASM memory.
    pub fn feed_cached(&mut self, cache: &FrameCache, index: u32, t: f64) -> Result<(), JsValue> {
        let f = cache.frame_ref(index as usize).ok_or_else(|| js_err("no such cached frame"))?;
        let (aw, ah) = (self.det.geometry().aw, self.det.geometry().ah);
        if (cache.width, cache.height) != (aw, ah) {
            return Err(js_err(format!("cache is {}x{} but the detector analyses at {aw}x{ah}", cache.width, cache.height)));
        }
        if !matches!(self.stage, Stage::Cpu(_)) {
            return self.submit(GpuSource::Rgb8 { data: f, width: aw, height: ah }, t, false);
        }
        self.run_cpu(FrameInput::rgb(f), t, false);
        Ok(())
    }

    /// Feed the current picture of a `<video>` element (GPU detector only).
    #[cfg(target_arch = "wasm32")]
    pub fn feed_video_element(&mut self, video: &web_sys::HtmlVideoElement, t: f64, capture: bool) -> Result<(), JsValue> {
        let (w, h) = (video.video_width(), video.video_height());
        if w == 0 || h == 0 {
            return Err(js_err("video has no frame yet"));
        }
        self.feed_external(unflash_gpu::wgpu::ExternalImageSource::HTMLVideoElement(video.clone()), w, h, t, capture)
    }

    /// Feed a WebCodecs `VideoFrame` (GPU detector only). The caller closes
    /// the frame afterwards.
    #[cfg(target_arch = "wasm32")]
    pub fn feed_video_frame(&mut self, frame: &web_sys::VideoFrame, t: f64, capture: bool) -> Result<(), JsValue> {
        let (w, h) = match frame.visible_rect() {
            Some(r) => (r.width() as u32, r.height() as u32),
            None => (frame.coded_width(), frame.coded_height()),
        };
        if w == 0 || h == 0 {
            return Err(js_err("empty video frame"));
        }
        // a second JS handle to the same frame, not a WebCodecs clone (which
        // would need its own close())
        let handle: web_sys::VideoFrame = wasm_bindgen::JsCast::unchecked_into(wasm_bindgen::JsValue::from(frame));
        self.feed_external(unflash_gpu::wgpu::ExternalImageSource::VideoFrame(handle), w, h, t, capture)
    }

    /// Feed the picture of an `OffscreenCanvas` (GPU detector only). This is
    /// the route for browsers whose WebGPU does not take `VideoFrame` or
    /// `<video>` as a copy source (Firefox): the caller draws the frame into
    /// the canvas first.
    #[cfg(target_arch = "wasm32")]
    pub fn feed_canvas(&mut self, canvas: &web_sys::OffscreenCanvas, t: f64, capture: bool) -> Result<(), JsValue> {
        let (w, h) = (canvas.width(), canvas.height());
        if w == 0 || h == 0 {
            return Err(js_err("empty canvas"));
        }
        self.feed_external(unflash_gpu::wgpu::ExternalImageSource::OffscreenCanvas(canvas.clone()), w, h, t, capture)
    }

    /// Collect finished GPU frames. Returns how many completed.
    pub fn poll(&mut self) -> Result<u32, JsValue> {
        let Stage::Gpu(stage) = &mut self.stage else { return Ok(0) };
        let mut n = 0;
        while let Some(r) = stage.poll() {
            let frame = r.map_err(js_err)?;
            let capture = self.pending_capture.pop_front().unwrap_or(false);
            let rec = self.det.complete_frame(&frame.stats);
            if capture {
                if let Some(rgba) = frame.rgba {
                    self.captures.insert(rec.index, rgba);
                }
            }
            self.records.push(rec);
            n += 1;
        }
        Ok(n)
    }

    /// Records completed since the last drain, as a JSON array.
    pub fn drain_records(&mut self) -> Result<String, JsValue> {
        let out = to_json(&self.records)?;
        self.records.clear();
        Ok(out)
    }

    /// The captured analysis-resolution picture of frame `index`, once.
    pub fn take_capture(&mut self, index: u32) -> Option<Vec<u8>> {
        self.captures.remove(&(index as usize))
    }

    /// The verdict over everything fed so far; `include_stats` adds every
    /// frame's statistics. (What a scan reads after each chunk, and the live
    /// monitor at each check, is `partial_verdict`.)
    pub fn finish(&self, include_stats: bool) -> Result<String, JsValue> {
        to_json(&self.det.finish(include_stats))
    }

    /// The verdict so far for reading while frames are still coming in: the
    /// violations, which kinds the profile reports and how many frames (and
    /// repeats) it has seen, under `finish`'s names. Unlike `finish(false)`
    /// it neither copies nor writes out the events, which grow with the
    /// video (and a scan asks after every chunk).
    pub fn partial_verdict(&self) -> Result<String, JsValue> {
        to_json(&self.det.temporal().partial_verdict())
    }

    pub fn reset(&mut self) {
        self.det.reset();
        self.records.clear();
        self.captures.clear();
        self.pending_capture.clear();
        if let Stage::Gpu(stage) = &mut self.stage {
            // frames still on the GPU belong to the old run
            stage.abandon();
        }
    }

    /// Run the detector over the frames fed since the last batch went out
    /// (a no-op on the CPU, whose frames are done as they are fed). Call
    /// before waiting for `pending()` to reach zero.
    pub fn flush(&mut self) {
        if let Stage::Gpu(stage) = &mut self.stage {
            stage.flush();
        }
    }
}

impl Detector {
    fn with_stage(det: CoreDetector, stage: Stage, waiter: Rc<RefCell<Option<js_sys::Function>>>) -> Detector {
        Detector { det, stage, records: Vec::new(), captures: BTreeMap::new(), pending_capture: Default::default(), shrink: None, small: Vec::new(), waiter }
    }

    /// Hand a picture to the GPU detector: it runs when its batch is full,
    /// or on `flush`.
    fn submit(&mut self, source: GpuSource<'_>, t: f64, capture: bool) -> Result<(), JsValue> {
        let Stage::Gpu(stage) = &mut self.stage else { unreachable!("the GPU detector's pictures") };
        if !stage.can_submit() {
            return Err(js_err("detector busy: poll() before submitting more frames"));
        }
        let params = self.det.begin_frame(t);
        stage.submit(params, source, capture).map_err(js_err)?;
        self.pending_capture.push_back(capture);
        Ok(())
    }

    /// Run the CPU detector over a picture at the analysis size (RGBA8
    /// when it is to be captured).
    fn run_cpu(&mut self, frame: FrameInput<'_>, t: f64, capture: bool) {
        let Stage::Cpu(stage) = &mut self.stage else { unreachable!("the CPU detector's pictures") };
        let params = self.det.begin_frame(t);
        let stats = stage.run(params, frame);
        let rec = self.det.complete_frame(&stats);
        if capture {
            self.captures.insert(rec.index, frame.data[..self.det.geometry().npix() * 4].to_vec());
        }
        self.records.push(rec);
    }

    /// Run the CPU detector over a `width`×`height` picture made the
    /// analysis size by `shrink` (given the shrink for that size and room
    /// for the result), as a decode worker makes it.
    fn run_cpu_shrunk(&mut self, width: u32, height: u32, t: f64, capture: bool, shrink: impl FnOnce(&mut Shrink, &mut Vec<u8>)) {
        let (aw, ah) = (self.det.geometry().aw, self.det.geometry().ah);
        let mut small = std::mem::take(&mut self.small);
        shrink(Shrink::reuse(&mut self.shrink, width, height, aw, ah), &mut small);
        self.run_cpu(FrameInput::rgba(&small), t, capture);
        self.small = small;
    }

    /// Note: `wgpu` unwraps the result of `copyExternalImageToTexture`, so a
    /// source the browser's WebGPU rejects (Firefox takes neither
    /// `VideoFrame` nor `<video>`) would abort the whole WASM instance. The
    /// JS side (`web/detector.js`) probes every kind of source on a throwaway
    /// device before it lets one through here.
    #[cfg(target_arch = "wasm32")]
    fn feed_external(&mut self, source: unflash_gpu::wgpu::ExternalImageSource, w: u32, h: u32, t: f64, capture: bool) -> Result<(), JsValue> {
        use unflash_gpu::wgpu;
        let Stage::Gpu(stage) = &mut self.stage else {
            return Err(js_err("external sources need the GPU detector; use feed_rgba"));
        };
        // (before the copy, so that a frame turned away costs nothing)
        if !stage.can_submit() {
            return Err(js_err("detector busy: poll() before submitting more frames"));
        }
        let tex = stage.source_texture(w, h).clone();
        stage.queue().copy_external_image_to_texture(
            &wgpu::CopyExternalImageSourceInfo { source, origin: wgpu::Origin2d::ZERO, flip_y: false },
            wgpu::CopyExternalImageDestInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
                color_space: wgpu::PredefinedColorSpace::Srgb,
                premultiplied_alpha: false,
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.submit(GpuSource::SourceTexture, t, capture)
    }
}

// ---- built-in H.264 decoder --------------------------------------------------

/// What the built-in decoder makes of an `avcC` record: JSON with the
/// profile, level and cropped size, or an error saying why the stream
/// cannot be decoded (4:2:2, high bit depth, slice groups, ...).
#[wasm_bindgen]
pub fn h264_probe(avcc: &[u8]) -> Result<String, JsValue> {
    let mut d = unflash_h264::Decoder::new();
    d.configure_avcc(avcc).map_err(js_err)?;
    let sps = d.first_sps().ok_or_else(|| js_err("no sequence parameter set in the file"))?;
    let (w, h) = sps.cropped_size();
    Ok(serde_json::json!({
        "profile_idc": sps.profile_idc,
        "level_idc": sps.level_idc,
        "width": w,
        "height": h,
    })
    .to_string())
}

/// The WebAssembly linear memory, so JavaScript can read decoded pictures
/// in place (`H264Decoder::frame_ptr`).
#[wasm_bindgen]
pub fn wasm_memory() -> JsValue {
    wasm_bindgen::memory()
}

/// A software H.264 decoder for one track: samples in decode order in,
/// I420 pictures (at the cropped size) out, one per sample. The picture
/// stays in WebAssembly memory; `frame_ptr` / `frame_len` locate it for a
/// `VideoFrame` of format I420 (which copies it).
#[wasm_bindgen]
pub struct H264Decoder {
    inner: unflash_h264::Decoder,
    frame: Vec<u8>,
    pts: f64,
    damaged: bool,
    width: u32,
    height: u32,
    color: String,
    /// Pictures made small here instead of handed out whole (see `set_shrink`).
    shrink: Option<SmallPictures>,
}

/// The analysis size pictures are made small to, and the last one.
struct SmallPictures {
    aw: u32,
    ah: u32,
    shrink: Option<Shrink>,
    last: Vec<u8>,
}

#[wasm_bindgen]
impl H264Decoder {
    /// `fast` leaves the deblocking filter out (about a fifth of the
    /// decoding time): pictures good for statistics, not for showing or
    /// re-encoding.
    #[wasm_bindgen(constructor)]
    pub fn new(avcc: &[u8], fast: bool) -> Result<H264Decoder, JsValue> {
        let mut inner = unflash_h264::Decoder::new();
        inner.set_skip_deblock(fast);
        inner.configure_avcc(avcc).map_err(js_err)?;
        let (width, height) = inner.first_sps().map(|s| s.cropped_size()).ok_or_else(|| js_err("no sequence parameter set in the file"))?;
        let color = color_space_json(inner.first_sps().unwrap());
        Ok(H264Decoder { inner, frame: Vec::new(), pts: 0.0, damaged: false, width, height, color, shrink: None })
    }

    /// From now on make each picture `analysis_width`×`analysis_height`
    /// RGBA8 here, straight from the decoder's own picture (converted with
    /// the matrix and range its sequence says, as the page would convert
    /// it: see `conversion`), instead of copying it out whole: `small` has
    /// it.
    pub fn set_shrink(&mut self, analysis_width: u32, analysis_height: u32) {
        self.shrink = Some(SmallPictures { aw: analysis_width.max(1), ah: analysis_height.max(1), shrink: None, last: Vec::new() });
    }

    /// The last picture made small (RGBA8 at the analysis size).
    pub fn small(&self) -> Vec<u8> {
        self.shrink.as_ref().map(|s| s.last.clone()).unwrap_or_default()
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Decode one sample; returns true when its picture is ready.
    pub fn decode(&mut self, sample: &[u8], pts: f64) -> Result<bool, JsValue> {
        match self.inner.decode_sample(sample, pts).map_err(js_err)? {
            None => Ok(false),
            Some(f) => {
                self.take(&f)?;
                Ok(true)
            }
        }
    }

    /// A finished picture: copied out whole, or made small.
    fn take(&mut self, f: &unflash_h264::DecodedFrame) -> Result<(), JsValue> {
        let sps = self.inner.sps().ok_or_else(|| js_err("no active sequence"))?;
        let (cx, cy, w, h) = f.crop;
        let (w, h) = (w as u32, h as u32);
        match &mut self.shrink {
            Some(s) => {
                let (bt709, full_range) = conversion(sps);
                let pic = &f.pic;
                let (lw, cw) = (pic.width, pic.width / 2);
                let k = Shrink::reuse(&mut s.shrink, w, h, s.aw, s.ah);
                k.yuv420_planes(&pic.y[cy * lw + cx..], lw, &pic.u[cy / 2 * cw + cx / 2..], cw, &pic.v[cy / 2 * cw + cx / 2..], cw, bt709, full_range, &mut s.last);
            }
            None => to_i420(&f.pic, f.crop, &mut self.frame),
        }
        if self.width != w || self.height != h {
            self.color = color_space_json(sps);
        }
        self.width = w;
        self.height = h;
        self.pts = f.pic.pts;
        self.damaged = f.damaged;
        Ok(())
    }

    /// Flush the picture in progress at the end of the stream (Annex B
    /// input only; MP4 samples always complete their picture).
    pub fn flush(&mut self) -> Result<bool, JsValue> {
        match self.inner.flush().map_err(js_err)? {
            None => Ok(false),
            Some(f) => {
                self.take(&f)?;
                Ok(true)
            }
        }
    }

    /// The last decoded picture as packed I420 (Y then Cb then Cr, no
    /// padding) in WebAssembly memory: its address and length in bytes.
    pub fn frame_ptr(&self) -> *const u8 {
        self.frame.as_ptr()
    }
    pub fn frame_len(&self) -> u32 {
        self.frame.len() as u32
    }
    pub fn frame_pts(&self) -> f64 {
        self.pts
    }
    pub fn frame_damaged(&self) -> bool {
        self.damaged
    }
    /// The picture's colour space as a `VideoColorSpaceInit` JSON object.
    pub fn color_json(&self) -> String {
        self.color.clone()
    }
}

// ---- H.264 parameter sets for spliced tracks ---------------------------------

/// The parameter sets of an exported H.264 track that copies the source's
/// samples and splices re-encoded spans in: starts from the source's
/// `avcC`, takes each encoder's record in (`register`) and hands back the
/// rewriter for that encoder's samples; `record()` is the merged `avcC`
/// for the track.
#[wasm_bindgen]
pub struct AvcRegistry {
    inner: unflash_h264::AvcRegistry,
}

#[wasm_bindgen]
impl AvcRegistry {
    #[wasm_bindgen(constructor)]
    pub fn new(base_avcc: &[u8]) -> Result<AvcRegistry, JsValue> {
        Ok(AvcRegistry { inner: unflash_h264::AvcRegistry::new(base_avcc).map_err(js_err)? })
    }

    pub fn register(&mut self, avcc: &[u8]) -> Result<AvcRewriter, JsValue> {
        Ok(AvcRewriter { inner: self.inner.register(avcc).map_err(js_err)? })
    }

    pub fn record(&self) -> Vec<u8> {
        self.inner.record()
    }
}

/// Makes one encoder's samples fit the merged track (see `AvcRegistry`).
#[wasm_bindgen]
pub struct AvcRewriter {
    inner: unflash_h264::Rewriter,
}

#[wasm_bindgen]
impl AvcRewriter {
    /// Whether samples pass through unchanged.
    pub fn is_identity(&self) -> bool {
        self.inner.is_identity()
    }

    pub fn rewrite_sample(&mut self, sample: &[u8]) -> Result<Vec<u8>, JsValue> {
        self.inner.rewrite_sample(sample).map_err(js_err)
    }
}

/// The type of the first slice NAL unit in a sample: 5 for an IDR picture
/// (a decoder can start there cold), 1 otherwise, 0 when the bytes given
/// hold no slice yet (read more of the sample).
#[wasm_bindgen]
pub fn h264_first_vcl_nal_type(sample: &[u8], nal_length_size: u32) -> u32 {
    unflash_h264::rewrite::first_vcl_nal_type(sample, nal_length_size.clamp(1, 4) as usize) as u32
}

/// The NAL length size of an `avcC` record's samples.
#[wasm_bindgen]
pub fn avcc_nal_length_size(avcc: &[u8]) -> u32 {
    avcc.get(4).map(|b| (b & 3) as u32 + 1).unwrap_or(4)
}

/// An `avcC` record (4-byte NAL lengths) for one SPS and one PPS (NAL
/// units, each with its header byte), with the chroma format and bit
/// depths a High profile's record carries after the sets: for an encoder
/// that gives its stream as Annex B and no record. Nothing when they are
/// not an SPS and a PPS, or the SPS can't be read.
#[wasm_bindgen]
pub fn avcc_record(sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    unflash_mp4::avcc_record(sps, pps)
}

/// The colour conversion the page gives a picture of this sequence, as
/// (BT.709, full range): `web/media.js`'s yuvLayoutWords reading the
/// matrix of `color_space_json` and the picture's height. BT.709 for
/// matrix 1 (or 9, BT.2020's), BT.601 for 5 or 6, otherwise by height
/// (BT.709 above 576 lines); full range as the VUI says, else limited.
fn conversion(sps: &unflash_h264::Sps) -> (bool, bool) {
    let hd = sps.cropped_size().1 > 576;
    match &sps.vui {
        Some(v) => (
            match v.matrix_coefficients {
                1 | 9 => true,
                5 | 6 => false,
                _ => hd,
            },
            v.video_full_range,
        ),
        None => (hd, false),
    }
}

/// The VideoColorSpace of a sequence, from its VUI or the usual defaults
/// by picture size.
fn color_space_json(sps: &unflash_h264::Sps) -> String {
    let (_, h) = sps.cropped_size();
    let hd = h > 576;
    let (mut primaries, mut transfer, mut matrix, mut full) = (if hd { "bt709" } else { "smpte170m" }, if hd { "bt709" } else { "smpte170m" }, if hd { "bt709" } else { "smpte170m" }, false);
    if let Some(v) = &sps.vui {
        full = v.video_full_range;
        primaries = match v.colour_primaries {
            1 => "bt709",
            5 => "bt470bg",
            6 => "smpte170m",
            9 => "bt2020",
            _ => primaries,
        };
        transfer = match v.transfer_characteristics {
            1 => "bt709",
            6 => "smpte170m",
            8 => "linear",
            13 => "iec61966-2-1",
            16 => "pq",
            18 => "hlg",
            _ => transfer,
        };
        matrix = match v.matrix_coefficients {
            0 => "rgb",
            1 => "bt709",
            5 => "bt470bg",
            6 => "smpte170m",
            9 => "bt2020-ncl",
            _ => matrix,
        };
    }
    serde_json::json!({ "primaries": primaries, "transfer": transfer, "matrix": matrix, "fullRange": full }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture of `bpp` bytes a pixel, smooth in places and noisy in
    /// others, with a light square over half of it on odd `i` (flashing).
    fn picture(w: usize, h: usize, bpp: usize, i: usize) -> Vec<u8> {
        let mut x = (i as u32 + 1).wrapping_mul(2654435761) | 1;
        let mut out = vec![255u8; w * h * bpp];
        for y in 0..h {
            for c in 0..w {
                let lit = i % 2 == 1 && c < w * 3 / 4 && y < h * 3 / 4;
                for k in 0..bpp.min(3) {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    let v = if lit { 200 + x % 40 } else if y % 3 == 0 { (c * 255 / w) as u32 } else { 30 + x % 60 };
                    out[(y * w + c) * bpp + k] = v as u8;
                }
            }
        }
        out
    }

    /// The CPU detector makes a picture the analysis size as a decode
    /// worker's Shrinker does, whatever form it comes in, so a scan gives
    /// the same records wherever its pictures were made small: full-size
    /// RGBA, BGRA and I420 pictures give the records and captures of the
    /// worker-made small ones. (2:1 boxes average exact halves, which the
    /// float area average it used before rounded away from zero.)
    #[test]
    fn cpu_detector_shrinks_as_the_workers_do() {
        let cfg = serde_json::to_string(&Profile::WcagExt.config()).unwrap();
        let (w, h) = (512u32, 288u32);
        let det = || Detector::new(&cfg, w, h).unwrap();
        let (mut rgba, mut bgra, mut yuv, mut want_rgb, mut want_yuv) = (det(), det(), det(), det(), det());
        let (aw, ah) = (want_rgb.analysis_width(), want_rgb.analysis_height());
        assert_eq!((aw, ah), (w / 2, h / 2));
        let mut k = Shrink::new(w, h, aw, ah);
        let mut small = Vec::new();
        let (ws, hs) = (w as usize, h as usize);
        let l = YuvLayout::packed_i420(ws, hs, true, false);
        let words = [0, l.y_off as u32, l.y_stride as u32, l.u_off as u32, l.u_stride as u32, l.v_off as u32, l.v_stride as u32, 1, 0];
        for i in 0..36 {
            let (t, capture) = (i as f64 / 24.0, i % 5 == 2);
            let px = picture(ws, hs, 4, i / 3);
            k.packed(&px, 0, ws * 4, false, &mut small);
            want_rgb.feed_rgba(&small, aw, ah, t, capture).unwrap();
            rgba.feed_rgba(&px, w, h, t, capture).unwrap();
            let swapped: Vec<u8> = px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 7]).collect();
            bgra.feed_bgra(&swapped, w, h, t, capture).unwrap();
            let planes = picture(ws * 3 / 2, hs, 1, i / 3);
            k.yuv420(&planes, &l, &mut small);
            want_yuv.feed_rgba(&small, aw, ah, t, capture).unwrap();
            yuv.feed_yuv(&planes, w, h, &words, t, capture).unwrap();
        }
        let want = want_rgb.drain_records().unwrap();
        let records: Vec<FrameRecord> = serde_json::from_str(&want).unwrap();
        assert!(records.len() == 36 && records.iter().any(|r| r.up_area > 0) && records.iter().any(|r| r.down_area > 0), "the pictures flash: {want}");
        assert_eq!(rgba.drain_records().unwrap(), want, "RGBA");
        assert_eq!(bgra.drain_records().unwrap(), want, "BGRA");
        assert_eq!(yuv.drain_records().unwrap(), want_yuv.drain_records().unwrap(), "I420");
        for i in (0..36).filter(|i| i % 5 == 2) {
            let shot = want_rgb.take_capture(i).unwrap();
            assert_eq!(rgba.take_capture(i).unwrap(), shot, "RGBA capture {i}");
            assert_eq!(bgra.take_capture(i).unwrap(), shot, "BGRA capture {i}");
            assert_eq!(yuv.take_capture(i), want_yuv.take_capture(i), "I420 capture {i}");
        }
        assert_eq!(rgba.finish(false).unwrap(), want_rgb.finish(false).unwrap());
    }

    /// A partial verdict is the whole verdict's violations, flags and frame
    /// counts under the same names, without the events: the page reads
    /// either alike (its `reports` and `counts` look at the flags by name).
    #[test]
    fn partial_verdict_is_the_verdict_less_its_events() {
        let cfg = serde_json::to_string(&Profile::WcagExt.config()).unwrap();
        let mut det = Detector::new(&cfg, 512, 288).unwrap();
        let (aw, ah) = (det.analysis_width(), det.analysis_height());
        for i in 0..96 {
            det.feed_rgba(&picture(aw as usize, ah as usize, 4, i / 3), aw, ah, i as f64 / 24.0, false).unwrap();
        }
        let full: serde_json::Value = serde_json::from_str(&det.finish(false).unwrap()).unwrap();
        let part: serde_json::Value = serde_json::from_str(&det.partial_verdict().unwrap()).unwrap();
        assert!(!full["violations"].as_array().unwrap().is_empty() && !full["events"].as_array().unwrap().is_empty(), "{full}");
        let part = part.as_object().unwrap();
        assert_eq!(part.keys().map(|k| k.as_str()).collect::<Vec<_>>(), ["flag_extended", "flag_patterns", "frames", "held", "violations"]);
        for (k, v) in part {
            assert_eq!(v, &full[k], "{k}");
        }
    }

    /// `web/media.js`'s yuvLayoutWords: the matrix and range it gives a
    /// picture of colour space `color` (a VideoColorSpaceInit) and height.
    fn page_conversion(color: &str, height: u32) -> (bool, bool) {
        let cs: serde_json::Value = serde_json::from_str(color).unwrap();
        let bt709 = match cs["matrix"].as_str() {
            Some("bt709" | "bt2020-ncl") => true,
            Some("smpte170m" | "bt470bg" | "fcc") => false,
            _ => height > 576,
        };
        (bt709, cs["fullRange"].as_bool().unwrap_or(false))
    }

    /// The H.264 decoder makes its pictures small with the conversion the
    /// page would pick for them (which it used to be told), for every
    /// matrix a VUI may name, both ranges, no VUI, and both sides of the
    /// height rule.
    #[test]
    fn h264_shrink_converts_as_the_page_would() {
        // splice_a.mp4's avcC: High profile, 64×48
        let avcc = [
            0x01, 0x64, 0x00, 0x0a, 0xff, 0xe1, 0x00, 0x1c, 0x67, 0x64, 0x00, 0x0a, 0xac, 0xd9, 0x46, 0x26, 0xff, 0xc0, 0x05, 0x80, 0x06, 0xc4, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf0, 0x3c,
            0x48, 0x96, 0x58, 0x01, 0x00, 0x06, 0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0, 0xfd, 0xf8, 0xf8, 0x00,
        ];
        let mut d = unflash_h264::Decoder::new();
        d.configure_avcc(&avcc).unwrap();
        let base = d.first_sps().unwrap().clone();
        let mut cases = 0;
        for lines in [480u32, 576, 720, 1080] {
            for vui in std::iter::once(None).chain((0..=12u8).flat_map(|m| [false, true].map(|full| Some(unflash_h264::ps::Vui { matrix_coefficients: m, video_full_range: full, ..Default::default() })))) {
                let mut sps = base.clone();
                sps.height_mbs = lines.div_ceil(16);
                sps.crop = (0, 0, 0, sps.height_mbs * 16 - lines);
                sps.vui = vui;
                assert_eq!(sps.cropped_size().1, lines);
                assert_eq!(conversion(&sps), page_conversion(&color_space_json(&sps), lines), "{lines} lines, {:?}", sps.vui);
                cases += 1;
            }
        }
        assert_eq!(cases, 4 * 27);
    }
}
