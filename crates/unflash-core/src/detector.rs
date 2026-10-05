//! Composition: a [`Detector`] owns the clock and the temporal stage and
//! hands out per-frame kernel parameters; a *pixel stage* (CPU here, GPU in
//! `unflash-gpu`) turns each frame into [`GridStats`], which come back
//! through [`Detector::complete_frame`]. The split is what lets the GPU
//! stage run asynchronously with several frames in flight.

use std::collections::VecDeque;

use crate::config::DetectorConfig;
use crate::grid::{aggregate, FrameInput, GridCell, GridGeometry, GridStats};
use crate::pixel::{
    held_count, run_frame_scalar, FramePlanes, KernelParams, PixelOutputs, PixelState, MODE_FIRST, MODE_HELD,
    MODE_SATURATE,
};
use crate::temporal::{AnalysisResult, FrameRecord, Temporal};
use crate::time::{age, secs_to_us, SATURATE_EVERY_FRAMES, SATURATE_EVERY_US};

/// Clock, bookkeeping and the temporal stage; pixel-stage agnostic.
#[derive(Clone, Debug)]
pub struct Detector {
    geom: GridGeometry,
    temporal: Temporal,
    template: KernelParams,
    pending: VecDeque<(f64, f64)>,
    submitted: u32,
    last_sat_us: u32,
    frames_since_sat: u32,
}

impl Detector {
    pub fn new(cfg: DetectorConfig, aw: u32, ah: u32) -> Self {
        let geom = GridGeometry::new(&cfg, aw, ah);
        let template = KernelParams::template(&cfg, &geom);
        let temporal = Temporal::new(cfg, geom.clone());
        Detector {
            geom,
            temporal,
            template,
            pending: VecDeque::new(),
            submitted: 0,
            last_sat_us: 0,
            frames_since_sat: 0,
        }
    }

    /// Analysis dimensions for a source size under this config.
    pub fn for_source(cfg: DetectorConfig, width: u32, height: u32) -> Self {
        let (aw, ah) = cfg.analysis_dims(width, height);
        Self::new(cfg, aw, ah)
    }

    pub fn geometry(&self) -> &GridGeometry {
        &self.geom
    }
    pub fn temporal(&self) -> &Temporal {
        &self.temporal
    }
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Advance the clock for a frame stamped `t` (native pts) and return the
    /// kernel parameters to run it under.
    pub fn begin_frame(&mut self, t: f64) -> KernelParams {
        let tc = self.temporal.clock.advance(t);
        let now = secs_to_us(tc);
        let mut mode = 0;
        if self.submitted == 0 {
            mode |= MODE_FIRST;
            self.last_sat_us = now;
            self.frames_since_sat = 0;
        } else {
            self.frames_since_sat += 1;
            if self.frames_since_sat >= SATURATE_EVERY_FRAMES || age(now, self.last_sat_us) >= SATURATE_EVERY_US {
                mode |= MODE_SATURATE;
                self.last_sat_us = now;
                self.frames_since_sat = 0;
            }
        }
        self.submitted += 1;
        self.pending.push_back((t, tc));
        let mut p = self.template;
        p.now = now;
        p.mode = mode;
        p
    }

    /// Hand back the reduction of the oldest pending frame.
    pub fn complete_frame(&mut self, stats: &GridStats) -> FrameRecord {
        let (t, tc) = self.pending.pop_front().expect("complete_frame without begin_frame");
        self.temporal.feed(t, tc, stats)
    }

    /// The verdict over the frames completed so far; `with_stats` adds the
    /// per-frame statistics (see [`Temporal::finish`]).
    pub fn finish(&self, with_stats: bool) -> AnalysisResult {
        self.temporal.finish(with_stats)
    }

    pub fn reset(&mut self) {
        self.temporal.reset();
        self.pending.clear();
        self.submitted = 0;
        self.frames_since_sat = 0;
    }
}

/// The CPU pixel stage (scalar or SIMD kernel).
#[derive(Clone, Debug)]
pub struct CpuStage {
    geom: GridGeometry,
    state: PixelState,
    planes: FramePlanes,
    out: PixelOutputs,
    cells: Vec<GridCell>,
    pat_mask: Vec<u32>,
    pub use_simd: bool,
}

impl CpuStage {
    pub fn new(cfg: &DetectorConfig, geom: GridGeometry) -> Self {
        let n = geom.npix();
        let k = cfg.k_fail() as usize;
        CpuStage {
            state: PixelState::new(n, k),
            planes: FramePlanes::new(n),
            out: PixelOutputs::new(n),
            cells: Vec::new(),
            pat_mask: vec![0; n],
            use_simd: cfg!(feature = "simd"),
            geom,
        }
    }

    /// The pattern mask of the last frame (bit k = orientation k).
    pub fn pattern_mask(&self) -> &[u32] {
        &self.pat_mask
    }

    pub fn state(&self) -> &PixelState {
        &self.state
    }
    pub fn planes(&self) -> &FramePlanes {
        &self.planes
    }
    /// Direct access to the input planes, so a caller can run the kernel on
    /// planes produced elsewhere (the GPU cross-check does this).
    pub fn planes_mut(&mut self) -> &mut FramePlanes {
        &mut self.planes
    }
    pub fn outputs(&self) -> &PixelOutputs {
        &self.out
    }

    /// Run the kernel on already-linearised planes (used by the GPU
    /// cross-check tests, which want identical inputs on both sides).
    pub fn run_planes(&mut self, mut params: KernelParams) -> GridStats {
        let first = params.mode & MODE_FIRST != 0;
        let moved = if first { 0 } else { held_count(&self.planes.l, &self.state.prev_l, self.geom.held_delta, &self.planes.v, &self.state.prev_v, self.geom.held_delta_v) };
        let held = !first && self.geom.is_held(moved);
        if held {
            params.mode |= MODE_HELD;
        }
        #[cfg(feature = "simd")]
        if self.use_simd {
            crate::pixel_simd::run_frame_simd(&mut self.state, &self.planes, &params, &mut self.out);
        } else {
            run_frame_scalar(&mut self.state, &self.planes, &params, &mut self.out);
        }
        #[cfg(not(feature = "simd"))]
        run_frame_scalar(&mut self.state, &self.planes, &params, &mut self.out);

        let sum_l: f64 = self.planes.l.iter().map(|&x| x as f64).sum();
        let pat = if params.pat_enabled != 0 {
            crate::pattern::detect(&self.planes.l, self.geom.aw, self.geom.ah, &params.pattern_params(), &mut self.pat_mask)
        } else {
            crate::pattern::PatternOut::default()
        };
        if held {
            self.cells.clear();
        } else {
            aggregate(
                &self.geom,
                &self.planes.l,
                &self.planes.v,
                &self.out.mask,
                &self.out.onset_gen,
                &self.out.onset_red,
                &mut self.cells,
            );
        }
        GridStats {
            held,
            held_count: moved,
            sum_l,
            cells: self.cells.clone(),
            pattern_count: pat.count,
            pattern_spacing_sum: pat.spacing_sum,
            pattern_spacing_n: pat.spacing_n,
        }
    }

    /// Reduce one frame under `params` (whose `mode` carries FIRST /
    /// SATURATE; the stage decides HELD itself).
    pub fn run(&mut self, params: KernelParams, frame: FrameInput<'_>) -> GridStats {
        self.planes.ingest(frame.data, frame.bpp, params.red_saturation, params.red_flare);
        self.run_planes(params)
    }
}

/// Detector + CPU stage: feed frames, get verdicts. The equivalent of the
/// reference's `FlashDetector`.
#[derive(Clone, Debug)]
pub struct CpuDetector {
    pub det: Detector,
    pub stage: CpuStage,
}

impl CpuDetector {
    pub fn new(cfg: DetectorConfig, aw: u32, ah: u32) -> Self {
        let det = Detector::new(cfg.clone(), aw, ah);
        let stage = CpuStage::new(&cfg, det.geometry().clone());
        CpuDetector { det, stage }
    }

    pub fn geometry(&self) -> &GridGeometry {
        self.det.geometry()
    }

    /// Feed one frame (8-bit sRGB at analysis resolution) stamped `t`.
    pub fn feed(&mut self, t: f64, frame: FrameInput<'_>) -> FrameRecord {
        let params = self.det.begin_frame(t);
        let stats = self.stage.run(params, frame);
        self.det.complete_frame(&stats)
    }

    pub fn finish(&self) -> AnalysisResult {
        self.det.finish(true)
    }

    pub fn reset(&mut self) {
        self.det.reset();
    }

    /// Run over a whole sequence of (t, rgb) frames.
    pub fn analyze<'a>(cfg: DetectorConfig, aw: u32, ah: u32, frames: impl IntoIterator<Item = (f64, FrameInput<'a>)>) -> AnalysisResult {
        let mut d = Self::new(cfg, aw, ah);
        for (t, f) in frames {
            d.feed(t, f);
        }
        d.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;
    use crate::temporal::ViolationKind;

    /// A synthetic frame: `frac` of the picture (a centred rectangle) at
    /// grey level `lum` (sRGB code), the rest black.
    fn frame(aw: u32, ah: u32, frac: f64, code: u8) -> Vec<u8> {
        let mut f = vec![0u8; (aw * ah * 3) as usize];
        let side = frac.sqrt();
        let rw = (aw as f64 * side) as u32;
        let rh = (ah as f64 * side) as u32;
        let x0 = (aw - rw) / 2;
        let y0 = (ah - rh) / 2;
        for y in y0..y0 + rh {
            for x in x0..x0 + rw {
                let i = ((y * aw + x) * 3) as usize;
                f[i] = code;
                f[i + 1] = code;
                f[i + 2] = code;
            }
        }
        f
    }

    fn square_wave(cfg: DetectorConfig, hz: f64, fps: f64, secs: f64, frac: f64, lo: u8, hi: u8) -> AnalysisResult {
        let (aw, ah) = cfg.analysis_dims(1024, 768);
        let mut d = CpuDetector::new(cfg, aw, ah);
        let n = (secs * fps) as usize;
        let a = frame(aw, ah, frac, lo);
        let b = frame(aw, ah, frac, hi);
        for i in 0..n {
            let t = i as f64 / fps;
            let phase = ((t * hz * 2.0).floor() as i64) % 2;
            d.feed(t, FrameInput::rgb(if phase == 0 { &a } else { &b }));
        }
        d.finish()
    }

    #[test]
    fn four_hz_fails_three_passes() {
        let r = square_wave(Profile::Wcag.config(), 4.0, 24.0, 4.0, 0.5, 20, 200);
        assert!(!r.safe(), "4 Hz over half the screen must fail");
        assert!(r.violations.iter().any(|v| v.kind == ViolationKind::Flash));
        let r = square_wave(Profile::Wcag.config(), 3.0, 24.0, 4.0, 0.5, 20, 200);
        assert!(r.safe(), "3 Hz is at the limit, WCAG passes it: {:?}", r.violations);
    }

    #[test]
    fn extended_flash_is_flagged_by_default_profile_only() {
        let r = square_wave(Profile::WcagExt.config(), 3.0, 24.0, 8.0, 0.5, 20, 200);
        assert!(r.wcag_safe());
        assert!(!r.safe(), "8 s at 3 Hz is an extended flash");
        assert!(r.violations.iter().any(|v| v.kind == ViolationKind::Extended));
        let r = square_wave(Profile::Strict.config(), 3.0, 24.0, 8.0, 0.5, 20, 200);
        assert!(!r.wcag_safe(), "strict fails outright at 3 Hz");
    }

    #[test]
    fn area_threshold() {
        // 15% of the window passes, 35% fails (whole-frame fractions here:
        // the 4:3 model window is a ninth of the picture, so scale up)
        let r = square_wave(Profile::Wcag.config(), 4.0, 24.0, 4.0, 0.15 / 9.0 * 9.0 * 0.15, 20, 200);
        assert!(r.safe(), "small area must pass: {:?}", r.violations);
        let r = square_wave(Profile::Wcag.config(), 4.0, 24.0, 4.0, 0.35, 20, 200);
        assert!(!r.safe());
    }

    #[test]
    fn bright_only_flicker_passes() {
        // both states above 0.8 relative luminance
        let r = square_wave(Profile::Wcag.config(), 4.0, 24.0, 4.0, 0.5, 235, 255);
        assert!(r.safe(), "{:?}", r.violations);
    }

    #[test]
    fn held_frames_do_not_count() {
        // 4 Hz at 24 fps, but every picture repeated 5 times at 120 fps must
        // give the same verdict (a render on a constant-rate grid)
        let cfg = Profile::Wcag.config();
        let (aw, ah) = cfg.analysis_dims(1024, 768);
        let a = frame(aw, ah, 0.5, 20);
        let b = frame(aw, ah, 0.5, 200);
        let mut d = CpuDetector::new(cfg.clone(), aw, ah);
        for i in 0..(4 * 120) {
            let t = i as f64 / 120.0;
            let phase = ((t * 8.0).floor() as i64) % 2;
            d.feed(t, FrameInput::rgb(if phase == 0 { &a } else { &b }));
        }
        let r = d.finish();
        assert!(!r.safe());
        assert!(r.held > 300, "most 120 fps frames repeat a picture: held={}", r.held);
        // and a still picture is never a flash
        let mut d = CpuDetector::new(cfg, aw, ah);
        for i in 0..96 {
            d.feed(i as f64 / 24.0, FrameInput::rgb(&a));
        }
        assert!(d.finish().safe());
    }

    #[test]
    fn red_flash() {
        let cfg = Profile::Wcag.config();
        let (aw, ah) = cfg.analysis_dims(1024, 768);
        let n = (aw * ah) as usize;
        // saturated red (L 0.21) <-> grey 144 (L 0.28): the luminance swing
        // is under the 0.1 general threshold but over the held-frame bar,
        // so only the red criterion can catch it
        let red: Vec<u8> = (0..n).flat_map(|_| [255u8, 0, 0]).collect();
        let grey: Vec<u8> = (0..n).flat_map(|_| [144u8, 144, 144]).collect();
        let mut d = CpuDetector::new(cfg, aw, ah);
        for i in 0..96 {
            let t = i as f64 / 24.0;
            let phase = ((t * 8.0).floor() as i64) % 2;
            d.feed(t, FrameInput::rgb(if phase == 0 { &red } else { &grey }));
        }
        let r = d.finish();
        assert!(r.violations.iter().any(|v| v.kind == ViolationKind::Red), "{:?}", r.violations);
    }

    #[test]
    fn equiluminant_red_flash_is_detected() {
        let cfg = Profile::Wcag.config();
        let (aw, ah) = cfg.analysis_dims(1280, 720);
        let n = (aw * ah) as usize;
        // saturated red (250, 0, 0) <-> grey (122, 124, 122): the same relative
        // luminance (0.203 vs 0.200), so a luminance-only held-frame gate took
        // every swap for a repeat of the last picture and never examined it
        let red: Vec<u8> = (0..n).flat_map(|_| [250u8, 0, 0]).collect();
        let grey: Vec<u8> = (0..n).flat_map(|_| [122u8, 124, 122]).collect();
        let black = vec![0u8; n * 3];
        let mut d = CpuDetector::new(cfg, aw, ah);
        for i in 0..240 {
            let t = i as f64 / 30.0;
            let f = if t < 3.0 {
                &black
            } else if ((t * 10.0).floor() as i64) % 2 == 0 {
                &red
            } else {
                &grey
            };
            d.feed(t, FrameInput::rgb(f));
        }
        let r = d.finish();
        assert!(!r.safe(), "{:?}", r.violations);
        assert!(r.violations.iter().any(|v| v.kind == ViolationKind::Red), "{:?}", r.violations);
        // the 89 black repeats and the 2 repeats inside each 3-frame run are
        // held; the 50 colour swaps must not be
        assert_eq!(r.held, 189, "{:?}", r.frame_stats.held);
    }

    /// Two segments, the second started a run-up early, merge into exactly
    /// the sequential run: a general flash straddling the seam, a red flash
    /// after it, repeats, and a pan.
    #[test]
    fn parallel_segments_merge_into_the_sequential_run() {
        use crate::sections::context_seconds;
        use crate::temporal::{merge_segments, Segment};
        let cfg = Profile::WcagExt.config();
        let (aw, ah) = cfg.analysis_dims(640, 360);
        let n = (aw * ah) as usize;
        let frame = |i: usize| -> Vec<u8> {
            let t = i as f64 / 30.0;
            let mut f = vec![90u8; n * 3];
            // a drifting texture (new pictures, no flashing)
            for y in 0..ah as usize {
                for x in 0..aw as usize {
                    let c = if ((x + i / 3) / 20 + y / 20) % 2 == 0 { 110 } else { 80 };
                    let k = (y * aw as usize + x) * 3;
                    f[k] = c;
                    f[k + 1] = c;
                    f[k + 2] = c;
                }
            }
            if (8.0..12.0).contains(&t) {
                let c = if ((t * 8.0).floor() as i64) % 2 == 0 { 30 } else { 200 };
                f.iter_mut().for_each(|p| *p = c);
            } else if (16.0..18.0).contains(&t) {
                let red = ((t * 8.0).floor() as i64) % 2 == 0;
                for px in f.chunks_exact_mut(3) {
                    px.copy_from_slice(if red { &[255, 0, 0] } else { &[144, 144, 144] });
                }
            } else if (13.0..14.0).contains(&t) {
                // a held picture
                f = vec![100u8; n * 3];
            }
            f
        };
        let total = 20 * 30;
        let mut seq = CpuDetector::new(cfg.clone(), aw, ah);
        for i in 0..total {
            seq.feed(i as f64 / 30.0, FrameInput::rgb(&frame(i)));
        }
        let want = seq.finish();
        assert!(want.violations.iter().any(|v| v.kind == ViolationKind::Flash) && want.violations.iter().any(|v| v.kind == ViolationKind::Red), "{:?}", want.violations);

        let seam = 10.0;
        let runup = context_seconds(&cfg);
        let mut a = CpuDetector::new(cfg.clone(), aw, ah);
        let mut b = CpuDetector::new(cfg.clone(), aw, ah);
        for i in 0..total {
            let t = i as f64 / 30.0;
            if t < seam {
                a.feed(t, FrameInput::rgb(&frame(i)));
            }
            if t >= seam - runup {
                b.feed(t, FrameInput::rgb(&frame(i)));
            }
        }
        let geom = a.geometry().clone();
        let got = merge_segments(&cfg, &geom, &[Segment { from: 0.0, result: a.finish() }, Segment { from: seam, result: b.finish() }]);
        assert_eq!(got.frames, want.frames);
        assert_eq!(got.held, want.held);
        assert_eq!(got.frame_stats.t, want.frame_stats.t);
        assert_eq!(got.frame_stats.hazard, want.frame_stats.hazard);
        assert_eq!(got.frame_stats.hazard_red, want.frame_stats.hazard_red);
        assert_eq!(got.frame_stats.ext, want.frame_stats.ext);
        assert_eq!(got.frame_stats.held, want.frame_stats.held);
        for (x, y) in got.frame_stats.tc.iter().zip(&want.frame_stats.tc) {
            assert!((x - y).abs() < 1e-9, "clock {x} vs {y}");
        }
        assert_eq!(got.events.len(), want.events.len(), "events");
        assert_eq!(got.violations, want.violations);
        assert_eq!(got.safe(), want.safe());
    }

    #[test]
    fn detector_split_api_matches_cpu_detector() {
        let cfg = Profile::Wcag.config();
        let (aw, ah) = cfg.analysis_dims(640, 480);
        let mut whole = CpuDetector::new(cfg.clone(), aw, ah);
        let mut det = Detector::new(cfg.clone(), aw, ah);
        let mut stage = CpuStage::new(&cfg, det.geometry().clone());
        let a = frame(aw, ah, 0.5, 20);
        let b = frame(aw, ah, 0.5, 200);
        for i in 0..72 {
            let t = i as f64 / 24.0;
            let f = if (i / 3) % 2 == 0 { &a } else { &b };
            whole.feed(t, FrameInput::rgb(f));
            let p = det.begin_frame(t);
            let s = stage.run(p, FrameInput::rgb(f));
            det.complete_frame(&s);
        }
        assert_eq!(whole.finish(), det.finish(true));
    }

    /// A verdict without the per-frame statistics is the full verdict less
    /// them: the same violations and events, and no copy of every frame's
    /// statistics. A partial verdict (what the page asks for after every
    /// chunk of a scan and at every live-monitor check) is the full verdict's
    /// violations, frame counts and flags alone.
    #[test]
    fn finish_without_stats_is_the_verdict_alone() {
        let cfg = Profile::WcagExt.config();
        let (aw, ah) = cfg.analysis_dims(640, 360);
        let mut det = Detector::new(cfg.clone(), aw, ah);
        let mut stage = CpuStage::new(&cfg, det.geometry().clone());
        let a = frame(aw, ah, 0.5, 20);
        let b = frame(aw, ah, 0.5, 200);
        // stripes over the whole picture for the last second: a pattern too
        let stripes: Vec<u8> = (0..(aw * ah) as usize).flat_map(|i| if ((i % aw as usize) / 4).is_multiple_of(2) { [20u8; 3] } else { [220u8; 3] }).collect();
        for i in 0..150 {
            let t = i as f64 / 30.0;
            let f = if i >= 120 { &stripes } else if (i / 3) % 2 == 0 { &a } else { &b };
            let p = det.begin_frame(t);
            let s = stage.run(p, FrameInput::rgb(f));
            det.complete_frame(&s);
        }
        let full = det.finish(true);
        let lean = det.finish(false);
        assert_eq!(full.frame_stats.len(), 150);
        assert!(lean.frame_stats.is_empty() && lean.frame_stats == Default::default());
        assert!(full.violations.iter().any(|v| v.kind == ViolationKind::Flash) && full.violations.iter().any(|v| v.kind == ViolationKind::Pattern), "{:?}", full.violations);
        assert_eq!(lean.violations, full.violations);
        assert_eq!(lean.events, full.events);
        assert_eq!((lean.frames, lean.held, lean.duration, lean.anomalies), (full.frames, full.held, full.duration, full.anomalies));
        let part = det.temporal().partial_verdict();
        assert!(!full.events.is_empty() && full.held > 0, "events and repeats to leave out and count: {:?}", full.events);
        assert_eq!(part.violations, full.violations);
        assert_eq!((part.frames, part.held, part.flag_extended, part.flag_patterns), (full.frames, full.held, full.flag_extended, full.flag_patterns));
    }
}
