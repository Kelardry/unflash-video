//! The temporal stage: everything the reference `FlashDetector` does that is
//! not per pixel. It consumes one [`GridStats`] per frame and keeps the
//! window-mean coherence gate, the event log, the per-frame chart statistics
//! and, at `finish()`, turns them into violations.
//!
//! Times here are f64 seconds exactly as in the reference; only the
//! per-pixel kernels run on the integer clock.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::config::DetectorConfig;
use crate::grid::{
    GridGeometry, GridStats, MASK_EXT_GEN, MASK_EXT_RED, MASK_POOL_GEN_DN, MASK_POOL_GEN_UP,
    MASK_POOL_RED_DN, MASK_POOL_RED_UP, MASK_STROBE_GEN, MASK_STROBE_RED,
};
use crate::pixel::MAX_RUN_SECONDS;

const NEVER: f64 = -1e12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViolationKind {
    Extended,
    Flash,
    /// A hazardous regular pattern (stripes); not a WCAG failure.
    Pattern,
    Red,
}

impl ViolationKind {
    /// True for kinds that are WCAG failures (extended flashes are not).
    pub fn is_wcag(self) -> bool {
        matches!(self, ViolationKind::Flash | ViolationKind::Red)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    General,
    Red,
}

/// A strobing moment: the window where concurrently flashing pixels covered
/// the area threshold while the window mean flashed too.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransitionEvent {
    /// Native pts (for cutting / sectioning).
    pub t: f64,
    /// Internal monotonic clock.
    pub tc: f64,
    pub kind: EventKind,
    /// Qualifying pixels in the best window.
    pub area: u32,
    /// (x0, y0, x1, y1) of the best window, analysis px.
    pub bbox: [u32; 4],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Violation {
    pub start: f64,
    pub end: f64,
    pub kind: ViolationKind,
    /// Severity: area over the threshold (or coverage for extended).
    pub count: f64,
    /// First frame whose flashing feeds this failure.
    pub onset: f64,
    /// Worst moment inside it.
    pub peak: f64,
}

impl Violation {
    pub fn wcag(&self) -> bool {
        self.kind.is_wcag()
    }
}

/// Per-frame chart statistics (parallel arrays).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameStats {
    pub t: Vec<f64>,
    pub tc: Vec<f64>,
    pub lum: Vec<f32>,
    pub up_area: Vec<u32>,
    pub down_area: Vec<u32>,
    pub red_area: Vec<u32>,
    pub hazard: Vec<u32>,
    pub hazard_red: Vec<u32>,
    pub ext: Vec<u32>,
    pub ext_red: Vec<u32>,
    pub held: Vec<bool>,
    /// Internal-clock time of the earliest transition still feeding the
    /// failure window on this frame (tc when the frame is not strobing).
    pub hazard_onset: Vec<f64>,
    pub hazard_red_onset: Vec<f64>,
    /// Pixels inside a regular pattern.
    pub pattern: Vec<u32>,
    /// Mean half-period of the pattern's stripes, analysis px (0 if none).
    pub pattern_period: Vec<f32>,
}

impl FrameStats {
    pub fn len(&self) -> usize {
        self.t.len()
    }
    pub fn is_empty(&self) -> bool {
        self.t.is_empty()
    }
}

/// What one fed frame produced, for live displays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameRecord {
    pub index: usize,
    pub t: f64,
    pub tc: f64,
    pub held: bool,
    pub lum: f32,
    pub up_area: u32,
    pub down_area: u32,
    pub red_area: u32,
    pub hazard: u32,
    pub hazard_red: u32,
    pub ext: u32,
    pub ext_red: u32,
    pub hazard_onset: f64,
    pub hazard_red_onset: f64,
    pub pattern: u32,
    pub pattern_period: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnalysisResult {
    pub events: Vec<TransitionEvent>,
    pub violations: Vec<Violation>,
    pub frames: usize,
    pub duration: f64,
    /// Timestamp anomalies bridged during analysis.
    pub anomalies: usize,
    /// Frames that only repeated the picture before them.
    pub held: usize,
    pub frame_stats: FrameStats,
    /// Profile treats extended flashes as violations to fix.
    pub flag_extended: bool,
    /// Profile treats regular patterns as violations to fix.
    pub flag_patterns: bool,
    /// Pixels a flash has to cover (for severity display).
    pub area_thresh: u32,
    /// Pixels a pattern has to cover.
    pub pattern_thresh: u32,
}

impl AnalysisResult {
    /// No WCAG general-flash or red-flash failure.
    pub fn wcag_safe(&self) -> bool {
        self.violations.iter().all(|v| !v.wcag())
    }

    /// Does the active profile report this kind?
    pub fn reports(&self, kind: ViolationKind) -> bool {
        match kind {
            ViolationKind::Extended => self.flag_extended,
            ViolationKind::Pattern => self.flag_patterns,
            _ => true,
        }
    }

    /// Passes everything the active profile flags.
    pub fn safe(&self) -> bool {
        self.violations.iter().all(|v| !self.reports(v.kind))
    }
}

/// The verdict over the frames fed so far as the page reads it while more
/// are coming in (after every chunk of a scan, every early look and every
/// live-monitor check): the violations, which kinds the profile reports, and
/// how many frames it has seen, named as in [`AnalysisResult`] so that the
/// page reads either the same way.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartialVerdict {
    pub violations: Vec<Violation>,
    pub frames: usize,
    /// Frames that only repeated the picture before them.
    pub held: usize,
    /// Profile treats extended flashes as violations to fix.
    pub flag_extended: bool,
    /// Profile treats regular patterns as violations to fix.
    pub flag_patterns: bool,
}

/// One segment of a file scanned in parallel with the others: its result,
/// and where its own span begins. Everything the detector saw before `from`
/// was the segment's run-up (the run-up plus run-out a section check uses),
/// decoded so that its state at `from` is the state a run from the start of
/// the file would have reached; those frames belong to the previous segment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Segment {
    pub from: f64,
    pub result: AnalysisResult,
}

/// Join the results of segments scanned in parallel into what one run over
/// the whole file gives: the per-frame statistics are concatenated (run-ups
/// dropped, the internal clock made continuous across the seams, the onsets
/// shifted with it) and the violations are derived from them once, exactly
/// as [`Temporal::finish`] derives them for one run. The segments must be in
/// order and each must carry its per-frame statistics.
pub fn merge_segments(cfg: &DetectorConfig, geom: &GridGeometry, segments: &[Segment]) -> AnalysisResult {
    let mut stats = FrameStats::default();
    let mut events = Vec::new();
    let mut held = 0usize;
    let mut anomalies = 0usize;
    let mut last: Option<(f64, f64)> = None; // native time and clock of the last frame kept
    for (si, seg) in segments.iter().enumerate() {
        let s = &seg.result.frame_stats;
        let n = s.len();
        let first = if si == 0 { 0 } else { (0..n).find(|&i| s.t[i] >= seg.from - 1e-9).unwrap_or(n) };
        if first >= n {
            continue;
        }
        // the clock a run from the start would show at this segment's first
        // frame: the previous frame's clock plus the native step between them
        let shift = match last {
            Some((lt, ltc)) => ltc + (s.t[first] - lt).max(0.0).min(cfg.max_frame_gap) - s.tc[first],
            None => 0.0,
        };
        for i in first..n {
            stats.t.push(s.t[i]);
            stats.tc.push(s.tc[i] + shift);
            stats.lum.push(s.lum[i]);
            stats.up_area.push(s.up_area[i]);
            stats.down_area.push(s.down_area[i]);
            stats.red_area.push(s.red_area[i]);
            stats.hazard.push(s.hazard[i]);
            stats.hazard_red.push(s.hazard_red[i]);
            stats.ext.push(s.ext[i]);
            stats.ext_red.push(s.ext_red[i]);
            stats.held.push(s.held[i]);
            stats.hazard_onset.push(s.hazard_onset[i] + shift);
            stats.hazard_red_onset.push(s.hazard_red_onset[i] + shift);
            stats.pattern.push(s.pattern[i]);
            stats.pattern_period.push(s.pattern_period[i]);
            if s.held[i] {
                held += 1;
            }
        }
        for e in &seg.result.events {
            if si == 0 || e.t >= seg.from - 1e-9 {
                let mut e = e.clone();
                e.tc += shift;
                events.push(e);
            }
        }
        anomalies += seg.result.anomalies;
        last = Some((s.t[n - 1], s.tc[n - 1] + shift));
    }
    Temporal::with_outcome(cfg.clone(), geom.clone(), stats, events, held, anomalies).finish(true)
}

/// The internal monotonic clock that bridges source timestamp
/// discontinuities.
#[derive(Clone, Debug)]
pub struct Clock {
    last_native: Option<f64>,
    clock: f64,
    recent: VecDeque<f64>,
    pub anomalies: usize,
    max_gap: f64,
}

impl Clock {
    pub fn new(max_gap: f64) -> Self {
        Clock { last_native: None, clock: 0.0, recent: VecDeque::new(), anomalies: 0, max_gap }
    }

    pub fn reset(&mut self) {
        self.last_native = None;
        self.clock = 0.0;
        self.recent.clear();
        self.anomalies = 0;
    }

    /// The step from the last frame to one stamped `t` (0 for the first):
    /// the timestamps' own difference or, where they go backwards or jump
    /// by more than `max_gap` (an anomaly), the median of the last 120 good
    /// steps ([`DEFAULT_STEP`] before there is one).
    pub fn step(&mut self, t: f64) -> f64 {
        let Some(last) = self.last_native.replace(t) else { return 0.0 };
        let dt = t - last;
        if dt <= 0.0 || dt > self.max_gap {
            self.anomalies += 1;
            return median(self.recent.iter().copied()).unwrap_or(DEFAULT_STEP);
        }
        self.recent.push_back(dt);
        if self.recent.len() > 120 {
            self.recent.pop_front();
        }
        dt
    }

    /// The clock at the frame stamped `t`: 0 at the first, then each
    /// [`step`](Self::step) on.
    pub fn advance(&mut self, t: f64) -> f64 {
        self.clock += self.step(t);
        self.clock
    }
}

/// The frame interval taken where none has been measured.
pub const DEFAULT_STEP: f64 = 1.0 / 30.0;

/// numpy-style median (mean of the two middle values for even counts).
pub fn median(vals: impl IntoIterator<Item = f64>) -> Option<f64> {
    let mut v: Vec<f64> = vals.into_iter().collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

/// numpy.interp with clamping at both ends; `xp` ascending.
fn interp(x: f64, xp: &[f64], fp: &[f64]) -> f64 {
    if xp.is_empty() {
        return 0.0;
    }
    if x <= xp[0] {
        return fp[0];
    }
    let n = xp.len();
    if x >= xp[n - 1] {
        return fp[n - 1];
    }
    let j = xp.partition_point(|&v| v <= x); // first xp[j] > x, 1 <= j < n
    let (x0, x1) = (xp[j - 1], xp[j]);
    if x1 == x0 {
        return fp[j];
    }
    fp[j - 1] + (fp[j] - fp[j - 1]) * (x - x0) / (x1 - x0)
}

/// Vectorised monotonic-run tracker over the grid of window positions,
/// float64 times, no aux value (port of `_ExtremaTracker` as used for the
/// window means).
#[derive(Clone, Debug)]
struct MeanTracker {
    eps: f32,
    max_run: f64,
    init: bool,
    dir: Vec<i8>,
    base: Vec<f32>,
    ext: Vec<f32>,
    base_t: Vec<f64>,
}

impl MeanTracker {
    fn new(n: usize, eps: f32) -> Self {
        MeanTracker {
            eps,
            max_run: MAX_RUN_SECONDS,
            init: false,
            dir: vec![0; n],
            base: vec![0.0; n],
            ext: vec![0.0; n],
            base_t: vec![0.0; n],
        }
    }

    fn reset(&mut self) {
        self.init = false;
    }

    fn settle(&mut self, t: f64) {
        if !self.init {
            return;
        }
        for i in 0..self.dir.len() {
            if t - self.base_t[i] > self.max_run {
                self.base[i] = self.ext[i];
                self.base_t[i] = t;
            }
        }
    }

    /// Returns per-cell (rev_up, rev_dn, base_snap, ext_snap).
    fn feed(&mut self, x: &[f32], t: f64, out: &mut [(bool, bool, f32, f32)]) {
        let n = self.dir.len();
        if !self.init {
            self.init = true;
            for i in 0..n {
                self.dir[i] = 0;
                self.base[i] = x[i];
                self.ext[i] = x[i];
                self.base_t[i] = t;
                out[i] = (false, false, x[i], x[i]);
            }
            return;
        }
        let eps = self.eps;
        for i in 0..n {
            let xi = x[i];
            let mut rev_up = false;
            let mut rev_dn = false;
            let d = self.dir[i];
            if d == 1 {
                if xi >= self.ext[i] {
                    self.ext[i] = xi;
                } else if xi < self.ext[i] - eps {
                    rev_up = true;
                }
            } else if d == -1 {
                if xi <= self.ext[i] {
                    self.ext[i] = xi;
                } else if xi > self.ext[i] + eps {
                    rev_dn = true;
                }
            }
            let snap = (self.base[i], self.ext[i]);
            if rev_up || rev_dn {
                self.base[i] = self.ext[i];
                self.ext[i] = xi;
                self.base_t[i] = t;
                self.dir[i] = if rev_up { -1 } else { 1 };
            } else if d == 0 {
                if xi > self.base[i] + eps {
                    self.dir[i] = 1;
                    self.ext[i] = xi;
                } else if xi < self.base[i] - eps {
                    self.dir[i] = -1;
                    self.ext[i] = xi;
                }
            }
            let stale = t - self.base_t[i] > self.max_run;
            if self.dir[i] == 0 && stale {
                self.ext[i] = xi;
            }
            if stale {
                self.base[i] = self.ext[i];
                self.base_t[i] = t;
            }
            out[i] = (rev_up, rev_dn, snap.0, snap.1);
        }
    }
}

/// Flash pairing and rate ring over the grid (port of `_FlashCounter` with
/// float64 times; the opening-time ring is not needed here).
#[derive(Clone, Debug)]
struct MeanCounter {
    k: usize,
    n: usize,
    ring: Vec<f64>,
    last: Vec<f64>,
    pend_pol: Vec<i8>,
    pend_t: Vec<f64>,
}

impl MeanCounter {
    fn new(n: usize, k: usize) -> Self {
        MeanCounter {
            k,
            n,
            ring: vec![NEVER; n * k],
            last: vec![NEVER; n],
            pend_pol: vec![0; n],
            pend_t: vec![NEVER; n],
        }
    }

    fn reset(&mut self) {
        self.ring.iter_mut().for_each(|v| *v = NEVER);
        self.last.iter_mut().for_each(|v| *v = NEVER);
        self.pend_pol.iter_mut().for_each(|v| *v = 0);
        self.pend_t.iter_mut().for_each(|v| *v = NEVER);
    }

    fn transitions(&mut self, mask: impl Fn(usize) -> bool, pol: i8, tc: f64) {
        let (n, k) = (self.n, self.k);
        for i in 0..n {
            if !mask(i) {
                continue;
            }
            if self.pend_pol[i] == -pol && tc - self.pend_t[i] <= 1.0 {
                let mut s = k - 1;
                while s > 0 {
                    self.ring[s * n + i] = self.ring[(s - 1) * n + i];
                    s -= 1;
                }
                self.ring[i] = tc;
                self.last[i] = tc;
                self.pend_pol[i] = 0;
                self.pend_t[i] = NEVER;
            } else {
                self.pend_pol[i] = pol;
                self.pend_t[i] = tc;
            }
        }
    }

    /// Cells flashing at least `k1` (1-based, clamped to K) times a second
    /// AND flashed within `fresh` seconds.
    fn strobing(&self, tc: f64, fresh: f64, k1: usize, out: &mut [bool]) {
        let k = k1.clamp(1, self.k);
        let n = self.n;
        for i in 0..n {
            let over_rate = tc - self.ring[(k - 1) * n + i] < 1.0 - 1e-3;
            out[i] = over_rate && tc - self.last[i] <= fresh;
        }
    }
}

/// The temporal stage of the detector.
#[derive(Clone, Debug)]
pub struct Temporal {
    cfg: DetectorConfig,
    geom: GridGeometry,
    pub clock: Clock,
    mtrack_gen: MeanTracker,
    mtrack_red: MeanTracker,
    mflash_gen: MeanCounter,
    mflash_red: MeanCounter,
    mean_swing: f32,
    mean_swing_red: f32,
    k_fail: usize,
    ext_rate: usize,
    events: Vec<TransitionEvent>,
    last_event_tc: [f64; 2],
    above: [bool; 2],
    stats: FrameStats,
    n: usize,
    held: usize,
    // scratch
    m_l: Vec<f32>,
    m_v: Vec<f32>,
    tr: Vec<(bool, bool, f32, f32)>,
    coh: Vec<bool>,
}

impl Temporal {
    pub fn new(cfg: DetectorConfig, geom: GridGeometry) -> Self {
        let ng = geom.ncells();
        let mean_swing = (cfg.swing_threshold * cfg.area_fraction as f32).max(0.02);
        // (a window whose area share of pixels made the least qualifying red swing)
        let mean_swing_red = crate::lut::RED_LEAST_SWING * cfg.area_fraction as f32;
        let k = cfg.k_fail() as usize;
        Temporal {
            clock: Clock::new(cfg.max_frame_gap),
            mtrack_gen: MeanTracker::new(ng, mean_swing * 0.3),
            mtrack_red: MeanTracker::new(ng, mean_swing_red * 0.3),
            mflash_gen: MeanCounter::new(ng, k),
            mflash_red: MeanCounter::new(ng, k),
            mean_swing,
            mean_swing_red,
            k_fail: k,
            ext_rate: cfg.ext_rate() as usize,
            events: Vec::new(),
            last_event_tc: [NEVER; 2],
            above: [false; 2],
            stats: FrameStats::default(),
            n: 0,
            held: 0,
            m_l: vec![0.0; ng],
            m_v: vec![0.0; ng],
            tr: vec![(false, false, 0.0, 0.0); ng],
            coh: vec![false; ng],
            cfg,
            geom,
        }
    }

    pub fn reset(&mut self) {
        self.clock.reset();
        self.mtrack_gen.reset();
        self.mtrack_red.reset();
        self.mflash_gen.reset();
        self.mflash_red.reset();
        self.events.clear();
        self.last_event_tc = [NEVER; 2];
        self.above = [false; 2];
        self.stats = FrameStats::default();
        self.n = 0;
        self.held = 0;
    }

    fn record(&self, i: usize) -> FrameRecord {
        let s = &self.stats;
        FrameRecord {
            index: i,
            t: s.t[i],
            tc: s.tc[i],
            held: s.held[i],
            lum: s.lum[i],
            up_area: s.up_area[i],
            down_area: s.down_area[i],
            red_area: s.red_area[i],
            hazard: s.hazard[i],
            hazard_red: s.hazard_red[i],
            ext: s.ext[i],
            ext_red: s.ext_red[i],
            hazard_onset: s.hazard_onset[i],
            hazard_red_onset: s.hazard_red_onset[i],
            pattern: s.pattern[i],
            pattern_period: s.pattern_period[i],
        }
    }

    /// Feed the reduction of one frame stamped `t` (native) / `tc` (clock).
    pub fn feed(&mut self, t: f64, tc: f64, st: &GridStats) -> FrameRecord {
        let cfg = &self.cfg;
        let ng = self.geom.ncells();
        let s = &mut self.stats;
        if st.held && self.n > 0 {
            self.held += 1;
            // the runs still age: time passes while a picture is held
            self.mtrack_gen.settle(tc);
            self.mtrack_red.settle(tc);
            let last = self.n - 1;
            s.t.push(t);
            s.tc.push(tc);
            s.lum.push(s.lum[last]);
            s.up_area.push(s.up_area[last]);
            s.down_area.push(s.down_area[last]);
            s.red_area.push(s.red_area[last]);
            s.hazard.push(0);
            s.hazard_red.push(0);
            s.ext.push(0);
            s.ext_red.push(0);
            s.held.push(true);
            s.hazard_onset.push(tc);
            s.hazard_red_onset.push(tc);
            s.pattern.push(st.pattern_count);
            s.pattern_period.push(pattern_period(st));
            self.n += 1;
            return self.record(self.n - 1);
        }
        assert_eq!(st.cells.len(), ng, "grid stats do not match the geometry");

        // --- coherence gate: window-mean flash tracking -------------------
        let npix = self.geom.window_pixels() as f64;
        for g in 0..ng {
            self.m_l[g] = (st.cells[g].sum_l as f64 / npix) as f32;
            self.m_v[g] = (st.cells[g].sum_v as f64 / npix) as f32;
        }
        self.mtrack_gen.feed(&self.m_l, tc, &mut self.tr);
        {
            let tr = &self.tr;
            let sw = self.mean_swing;
            self.mflash_gen.transitions(|g| tr[g].0 && (tr[g].3 - tr[g].2) >= sw, 1, tc);
            self.mflash_gen.transitions(|g| tr[g].1 && (tr[g].2 - tr[g].3) >= sw, -1, tc);
        }
        self.mtrack_red.feed(&self.m_v, tc, &mut self.tr);
        {
            let tr = &self.tr;
            let sw = self.mean_swing_red;
            self.mflash_red.transitions(|g| tr[g].0 && (tr[g].3 - tr[g].2) >= sw, 1, tc);
            self.mflash_red.transitions(|g| tr[g].1 && (tr[g].2 - tr[g].3) >= sw, -1, tc);
        }

        // --- flashes: per-pixel rate + concurrent area + coherent mean ----
        let fresh_w = cfg.area_accum_window;
        let coh_w = fresh_w.max(0.2);
        let area_thresh = self.geom.area_thresh;
        let mut haz = [0u32; 2];
        let mut ext = [0u32; 2];
        let mut onsets = [tc; 2];
        for (ki, (strobe_bit, ext_bit, kind)) in [
            (MASK_STROBE_GEN, MASK_EXT_GEN, EventKind::General),
            (MASK_STROBE_RED, MASK_EXT_RED, EventKind::Red),
        ]
        .into_iter()
        .enumerate()
        {
            let mflash = if ki == 0 { &self.mflash_gen } else { &self.mflash_red };
            let bit = strobe_bit.trailing_zeros() as usize;
            let ebit = ext_bit.trailing_zeros() as usize;
            mflash.strobing(tc, coh_w, self.k_fail, &mut self.coh);
            let mut best = 0u32;
            let mut best_g = usize::MAX;
            for g in 0..ng {
                let c = st.cells[g].cnt[bit];
                if c >= area_thresh && self.coh[g] && c > best {
                    best = c;
                    best_g = g;
                }
            }
            let mut best_onset = tc;
            if best_g != usize::MAX {
                let bbox = self.geom.bbox(best_g);
                let cell = &st.cells[best_g];
                let onset_age = if ki == 0 { cell.onset_gen } else { cell.onset_red };
                if onset_age > 0 {
                    // the opening transition itself ramps over a few frames,
                    // and completions are pooled, so back off by the pooling
                    // window to reach the frame the swing actually started from
                    best_onset = (tc - onset_age as f64 * 1e-6) - cfg.area_accum_window;
                }
                if !self.above[ki] || tc - self.last_event_tc[ki] >= 0.25 {
                    self.events.push(TransitionEvent { t, tc, kind, area: best, bbox });
                    self.last_event_tc[ki] = tc;
                }
            }
            self.above[ki] = best > 0;
            // extended flash: the identical test one step below the failure rate
            mflash.strobing(tc, coh_w, self.ext_rate, &mut self.coh);
            let mut ext_best = 0u32;
            for g in 0..ng {
                if self.coh[g] {
                    ext_best = ext_best.max(st.cells[g].cnt[ebit]);
                }
            }
            haz[ki] = best;
            ext[ki] = ext_best;
            onsets[ki] = best_onset;
        }

        // pooled transition areas: chart statistics only (best grid window)
        let pool_max = |bit: u32| -> u32 {
            let b = bit.trailing_zeros() as usize;
            st.cells.iter().map(|c| c.cnt[b]).max().unwrap_or(0)
        };
        let s = &mut self.stats;
        s.t.push(t);
        s.tc.push(tc);
        s.lum.push((st.sum_l / self.geom.npix() as f64) as f32);
        s.up_area.push(pool_max(MASK_POOL_GEN_UP));
        s.down_area.push(pool_max(MASK_POOL_GEN_DN));
        s.red_area.push(pool_max(MASK_POOL_RED_UP).max(pool_max(MASK_POOL_RED_DN)));
        s.hazard.push(haz[0]);
        s.hazard_red.push(haz[1]);
        s.ext.push(ext[0]);
        s.ext_red.push(ext[1]);
        s.held.push(false);
        s.hazard_onset.push(onsets[0]);
        s.hazard_red_onset.push(onsets[1]);
        s.pattern.push(st.pattern_count);
        s.pattern_period.push(pattern_period(st));
        self.n += 1;
        self.record(self.n - 1)
    }

    /// Pixels a regular pattern has to cover.
    pub fn pattern_thresh(&self) -> u32 {
        ((self.cfg.pattern_area_fraction * self.geom.npix() as f64).round() as u32).max(1)
    }

    /// Internal-clock times -> the native pts of the frames they fell on.
    pub fn to_native(&self, tc: f64) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        interp(tc, &self.stats.tc, &self.stats.t)
    }

    /// A temporal state holding only the outcome of a run (per-frame
    /// statistics, events, counts), enough for [`finish`](Self::finish).
    fn with_outcome(cfg: DetectorConfig, geom: GridGeometry, stats: FrameStats, events: Vec<TransitionEvent>, held: usize, anomalies: usize) -> Self {
        let mut t = Temporal::new(cfg, geom);
        t.n = stats.len();
        t.stats = stats;
        t.events = events;
        t.held = held;
        t.clock.anomalies = anomalies;
        t
    }

    /// The [`PartialVerdict`] over the frames fed so far:
    /// [`finish`](Self::finish) without its copy of every event, which grows
    /// with the video, so that asking after every chunk of a scan costs only
    /// the finding of the violations.
    pub fn partial_verdict(&self) -> PartialVerdict {
        let cfg = &self.cfg;
        let mut v = self.strobe_violations(&self.stats.hazard, &self.stats.hazard_onset, ViolationKind::Flash);
        v.extend(self.strobe_violations(&self.stats.hazard_red, &self.stats.hazard_red_onset, ViolationKind::Red));
        v.extend(self.extended_violations());
        v.extend(self.pattern_violations());
        v.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
        PartialVerdict { violations: v, frames: self.n, held: self.held, flag_extended: cfg.flag_extended(), flag_patterns: cfg.flag_patterns() }
    }

    /// The verdict over the frames fed so far: the
    /// [partial verdict](Self::partial_verdict), every event, and with
    /// `with_stats` a copy of every frame's statistics (for the chart, and
    /// for [`merge_segments`] to join).
    pub fn finish(&self, with_stats: bool) -> AnalysisResult {
        let PartialVerdict { violations, frames, held, flag_extended, flag_patterns } = self.partial_verdict();
        AnalysisResult {
            events: self.events.clone(),
            violations,
            frames,
            duration: if self.n > 0 { self.stats.tc[self.n - 1] - self.stats.tc[0] } else { 0.0 },
            anomalies: self.clock.anomalies,
            held,
            frame_stats: if with_stats { self.stats.clone() } else { FrameStats::default() },
            flag_extended,
            flag_patterns,
            area_thresh: self.geom.area_thresh,
            pattern_thresh: self.pattern_thresh(),
        }
    }

    /// Frames where concurrently-strobing pixels cover the area threshold,
    /// merged into intervals (native time).
    fn strobe_violations(&self, areas: &[u32], onsets: &[f64], kind: ViolationKind) -> Vec<Violation> {
        let mut out: Vec<Violation> = Vec::new();
        let mut cur_end_tc = NEVER;
        let thresh = self.geom.area_thresh;
        for i in 0..self.n {
            if areas[i] < thresh {
                continue;
            }
            let (t, tc) = (self.stats.t[i], self.stats.tc[i]);
            let sev = round2(areas[i] as f64 / thresh as f64);
            let ons = t.min(self.to_native(onsets[i]));
            match out.last_mut() {
                Some(cur) if tc - cur_end_tc <= 1.0 => {
                    cur.end = cur.end.max(t);
                    if sev > cur.count {
                        cur.count = sev;
                        cur.peak = t;
                    }
                    cur.onset = cur.onset.min(ons);
                }
                _ => out.push(Violation { start: t, end: t, kind, count: sev, onset: ons, peak: t }),
            }
            cur_end_tc = tc;
        }
        out
    }

    /// Regular patterns: frames whose patterned pixels cover the area
    /// threshold, merged where they are closer than `pattern_hold`, kept
    /// where the pattern stays on screen for `pattern_min_seconds`.
    fn pattern_violations(&self) -> Vec<Violation> {
        let cfg = &self.cfg;
        if self.n == 0 || !cfg.flag_patterns() {
            return vec![];
        }
        let thresh = self.pattern_thresh();
        let s = &self.stats;
        let mut out: Vec<(usize, usize, usize)> = Vec::new(); // first, last, peak frame
        for i in 0..self.n {
            if s.pattern[i] < thresh {
                continue;
            }
            match out.last_mut() {
                Some(run) if s.tc[i] - s.tc[run.1] <= cfg.pattern_hold => {
                    run.1 = i;
                    if s.pattern[i] > s.pattern[run.2] {
                        run.2 = i;
                    }
                }
                _ => out.push((i, i, i)),
            }
        }
        if out.is_empty() {
            return vec![];
        }
        // (a run's last frame stays up for a frame interval; sorting every
        // interval for it is worth it only once there is a run)
        let dt = crate::timeline::median_dt(&s.tc);
        out.into_iter()
            .filter(|&(a, b, _)| s.tc[b] - s.tc[a] + dt >= cfg.pattern_min_seconds)
            .map(|(a, b, p)| Violation {
                start: s.t[a],
                end: s.t[b],
                kind: ViolationKind::Pattern,
                count: round2(s.pattern[p] as f64 / thresh as f64),
                onset: s.t[a],
                peak: s.t[p],
            })
            .collect()
    }

    /// ITC/Ofcom-style extended flash: flashing that meets every failure
    /// criterion except the rate, sustained for `extended_window` seconds.
    fn extended_violations(&self) -> Vec<Violation> {
        let cfg = &self.cfg;
        if self.n == 0 || !cfg.flag_extended() {
            return vec![];
        }
        let area = self.geom.area_thresh as f64 * cfg.extended_area_ratio;
        let tc = &self.stats.tc;
        let hits: Vec<f64> = (0..self.n)
            .filter(|&i| self.stats.ext[i] as f64 >= area || self.stats.ext_red[i] as f64 >= area)
            .map(|i| tc[i])
            .collect();
        if hits.is_empty() {
            return vec![];
        }
        let (lo, hi) = (tc[0], tc[self.n - 1]);
        let lit = merge_spans(hits.iter().map(|&h| (h, h + cfg.extended_hold)), lo, hi);
        if lit.is_empty() {
            return vec![];
        }
        let w = cfg.extended_window;
        let (width, x_hi) = if hi - lo >= w {
            (w, hi - w)
        } else if hi - lo >= w * 0.9 {
            // too short to hold a whole window: judge it on all there is
            (hi - lo, lo)
        } else {
            return vec![];
        };
        let ab: Vec<f64> = lit.iter().map(|s| s.0).collect();
        let be: Vec<f64> = lit.iter().map(|s| s.1).collect();
        let cover = Coverage::new(&ab, &be);
        // coverage(x) is piecewise linear, cornering only where a span opens
        // or where one closed a window-length earlier
        let mut xs: Vec<f64> = vec![lo, x_hi];
        xs.extend(ab.iter().copied());
        xs.extend(be.iter().map(|&b| b - width));
        xs.retain(|&x| x >= lo && x <= x_hi);
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        xs.dedup();
        if xs.is_empty() {
            return vec![];
        }
        let mut merged: Vec<(f64, f64, f64)> = Vec::new();
        for &x in &xs {
            let frac = cover.inside(x, width) / width;
            if frac < cfg.extended_coverage {
                continue;
            }
            // report the flashing, not the window that measured it
            let i0 = be.partition_point(|&b| b <= x);
            let i1 = ab.partition_point(|&a| a < x + width);
            if i1 <= i0 {
                continue;
            }
            let s = ab[i0].max(x);
            let e = be[i1 - 1].min(x + width);
            if e <= s {
                continue;
            }
            match merged.last_mut() {
                Some(m) if s <= m.1 => {
                    m.1 = m.1.max(e);
                    m.2 = m.2.max(frac);
                }
                _ => merged.push((s, e, frac)),
            }
        }
        if merged.is_empty() {
            return vec![];
        }
        // the longest unbroken stretch of flashing inside a report is the
        // moment worth going and watching
        let mut out = Vec::with_capacity(merged.len());
        for &(s, e, frac) in &merged {
            let i0 = be.partition_point(|&b| b <= s);
            let i1 = ab.partition_point(|&a| a < e);
            let mut best: Option<(f64, f64)> = None;
            for k in i0..i1 {
                let run = (be[k].min(e) - ab[k].max(s), ab[k].max(s));
                best = Some(match best {
                    Some(b) if b.0 > run.0 || (b.0 == run.0 && b.1 >= run.1) => b,
                    _ => run,
                });
            }
            let peak = best.map(|b| b.1).unwrap_or(s);
            // The lit spans run on for the hold after each hit, which is how
            // hits a second apart join up; the flashing itself stops at the
            // last hit. Reported to the hold's end, flashing that stops where
            // a section begins reads as a second of that section's own.
            let last = hits[hits.partition_point(|&h| h <= e).max(1) - 1].max(s);
            out.push(Violation {
                start: self.to_native(s),
                end: self.to_native(last),
                kind: ViolationKind::Extended,
                count: frac,
                onset: self.to_native(s),
                peak: self.to_native(peak),
            });
        }
        out
    }
}

fn pattern_period(st: &GridStats) -> f32 {
    if st.pattern_spacing_n == 0 {
        0.0
    } else {
        st.pattern_spacing_sum as f32 / st.pattern_spacing_n as f32
    }
}

/// Python's `round(x, 2)` to a good approximation.
fn round2(x: f64) -> f64 {
    (x * 100.0).round_ties_even() / 100.0
}

/// Spans clipped to [lo, hi] and merged where they touch or overlap,
/// returned sorted and disjoint.
fn merge_spans(spans: impl IntoIterator<Item = (f64, f64)>, lo: f64, hi: f64) -> Vec<(f64, f64)> {
    let mut v: Vec<(f64, f64)> = spans.into_iter().collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<(f64, f64)> = Vec::new();
    for (a, b) in v {
        let (a, b) = (a.max(lo), b.min(hi));
        if b <= a {
            continue;
        }
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// How many seconds of a set of disjoint spans fall inside a window.
struct Coverage {
    a: Vec<f64>,
    b: Vec<f64>,
    cum: Vec<f64>,
}

impl Coverage {
    fn new(starts: &[f64], ends: &[f64]) -> Self {
        let mut cum = vec![0.0];
        for (s, e) in starts.iter().zip(ends) {
            let last = *cum.last().unwrap();
            cum.push(last + (e - s));
        }
        Coverage { a: starts.to_vec(), b: ends.to_vec(), cum }
    }

    /// Covered seconds before `y`.
    fn upto(&self, y: f64) -> f64 {
        let k = self.b.partition_point(|&b| b <= y);
        let part = if k < self.a.len() { (y - self.a[k]).clamp(0.0, self.b[k] - self.a[k]) } else { 0.0 };
        self.cum[k] + part
    }

    /// Covered seconds in [x, x + width].
    fn inside(&self, x: f64, width: f64) -> f64 {
        self.upto(x + width) - self.upto(x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_bridges_anomalies() {
        let mut c = Clock::new(5.0);
        assert_eq!(c.advance(10.0), 0.0);
        assert!((c.advance(10.04) - 0.04).abs() < 1e-12);
        assert!((c.advance(10.08) - 0.08).abs() < 1e-12);
        // a backwards jump is bridged with the median delta
        let tc = c.advance(3.0);
        assert!((tc - 0.12).abs() < 1e-9);
        assert_eq!(c.anomalies, 1);
        // a huge jump likewise
        let tc = c.advance(100.0);
        assert!((tc - 0.16).abs() < 1e-9);
        assert_eq!(c.anomalies, 2);
    }

    #[test]
    fn interp_clamps() {
        let xp = [0.0, 1.0, 2.0];
        let fp = [10.0, 20.0, 40.0];
        assert_eq!(interp(-1.0, &xp, &fp), 10.0);
        assert_eq!(interp(0.5, &xp, &fp), 15.0);
        assert_eq!(interp(1.5, &xp, &fp), 30.0);
        assert_eq!(interp(5.0, &xp, &fp), 40.0);
    }

    #[test]
    fn coverage_counts_seconds() {
        let c = Coverage::new(&[0.0, 2.0], &[1.0, 3.0]);
        assert!((c.inside(0.0, 3.0) - 2.0).abs() < 1e-12);
        assert!((c.inside(0.5, 2.0) - 1.0).abs() < 1e-12);
        assert!((c.inside(1.0, 1.0)).abs() < 1e-12);
    }

    #[test]
    fn spans_merge() {
        let m = merge_spans([(0.0, 1.0), (0.5, 2.0), (3.0, 4.0), (-1.0, 0.2)], 0.0, 3.5);
        assert_eq!(m, vec![(0.0, 2.0), (3.0, 3.5)]);
    }

    #[test]
    fn median_even_and_odd() {
        assert_eq!(median([3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median([4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median([]), None);
    }
}
