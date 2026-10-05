//! Section editing: replacement maps, the edited timeline, thinning to a
//! frame rate, the keep-light / keep-dark suggester, and classifying a
//! context-aware check's violations (port of `editing.py`).
//!
//! Anything that needs a detector run is expressed as a step machine
//! ([`Suggester`]) so the caller can simulate on whichever pixel stage it
//! has, synchronously on the CPU or asynchronously on the GPU.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::config::DetectorConfig;
use crate::lut::{lut, luminance};
use crate::sections::{rate_is_guaranteed, safe_picture_rate, MAX_TARGET_FPS};
use crate::temporal::{AnalysisResult, Violation, ViolationKind};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fill {
    #[default]
    Prev,
    Next,
}

/// A frame mark. Serialises as `{"removed": bool, "extended": bool,
/// "fill": "prev"|"next"}`, compatible with the reference's project files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameEdit {
    pub removed: bool,
    pub extended: bool,
    pub fill: Fill,
}

impl FrameEdit {
    pub fn removed(fill: Fill) -> Self {
        FrameEdit { removed: true, extended: false, fill }
    }
    pub fn extended() -> Self {
        FrameEdit { removed: false, extended: true, fill: Fill::Prev }
    }
}

/// Marks by frame ordinal. JSON keys are strings ("12"), as in the
/// reference's project files.
pub type Edits = BTreeMap<usize, FrameEdit>;

/// For each of a section's `n` frames, the ordinal whose picture it shows.
///
/// A surviving frame shows itself. A removed one shows the nearest survivor
/// in its fill direction, falling back to the other direction where that
/// runs out. A section with nothing left at all holds its first frame.
pub fn replacement_map(edits: &Edits, n: usize) -> Vec<usize> {
    let gone: Vec<bool> = (0..n).map(|i| edits.get(&i).map(|e| e.removed).unwrap_or(false)).collect();
    let mut prev = vec![None; n];
    let mut seen = None;
    for i in 0..n {
        if !gone[i] {
            seen = Some(i);
        }
        prev[i] = seen;
    }
    let mut nxt = vec![None; n];
    seen = None;
    for i in (0..n).rev() {
        if !gone[i] {
            seen = Some(i);
        }
        nxt[i] = seen;
    }
    (0..n)
        .map(|i| {
            if !gone[i] {
                return i;
            }
            let forward = edits.get(&i).map(|e| e.fill == Fill::Next).unwrap_or(false);
            let (first, other) = if forward { (nxt[i], prev[i]) } else { (prev[i], nxt[i]) };
            first.or(other).unwrap_or(0)
        })
        .collect()
}

/// A held frame's extra time. The picture of a frame marked E stays up for
/// `seconds` more from `at` (section-relative source time, as `rel_pts`:
/// the moment its next frame would have come), and everything from `at`
/// on comes that much later, picture and sound alike.
///
/// The edited timeline's frame times ([`edited_sequence`]) are computed
/// from this list, and the export's audio puts `seconds` of silence at
/// `at` from the same list, so the picture and the sound cannot disagree
/// about where a hold is. (The original tool built the two separately and
/// put the silence at the end of the section.)
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hold {
    pub at: f64,
    pub seconds: f64,
}

/// The holds of a section's marks, in order: one for each frame marked E
/// (and not removed), at its next frame's time, or at `end` (the
/// section's length) for its last frame.
pub fn holds(rel_pts: &[f64], edits: &Edits, extension_seconds: f64, end: f64) -> Vec<Hold> {
    let n = rel_pts.len();
    edits
        .range(..n)
        .filter(|(_, e)| e.extended && !e.removed)
        .map(|(&i, _)| Hold { at: if i + 1 < n { rel_pts[i + 1] } else { end }, seconds: extension_seconds })
        .collect()
}

/// The section's edited timeline as (display_time, source_ordinal).
/// Removed frames stand in for the survivor `replacement_map` picks; each
/// frame comes as much later as the [`holds`] before it say.
pub fn edited_sequence(rel_pts: &[f64], edits: &Edits, extension_seconds: f64) -> Vec<(f64, usize)> {
    let n = rel_pts.len();
    let rep = replacement_map(edits, n);
    // (the last frame's hold comes after every frame of the section)
    let hs = holds(rel_pts, edits, extension_seconds, f64::INFINITY);
    let mut k = 0;
    let mut offset = 0.0;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        while k < hs.len() && hs[k].at <= rel_pts[i] {
            offset += hs[k].seconds;
            k += 1;
        }
        out.push((rel_pts[i] + offset, rep[i]));
    }
    out
}

/// When each slot's picture first reaches the screen, per slot. A removal
/// marked "next" shows the survivor that follows it, so that survivor's
/// picture is already up before its own slot comes round.
pub fn picture_times(rel_pts: &[f64], edits: &Edits, extension_seconds: f64) -> Vec<f64> {
    let seq = edited_sequence(rel_pts, edits, extension_seconds);
    let mut first: BTreeMap<usize, f64> = BTreeMap::new();
    for &(t, src) in &seq {
        first.entry(src).or_insert(t);
    }
    seq.iter().map(|&(t, src)| *first.get(&src).unwrap_or(&t)).collect()
}

/// Which frames to remove so no two surviving pictures reach the screen
/// closer than `min_gap` seconds apart. Timestamps only.
///
/// `gone` are slots already removed (they neither space nor consume the
/// gap); `scope` restricts which slots may be removed (None = all). Frames
/// outside the scope are kept and still set the pace.
fn rate_limited_removals(
    times: &[f64],
    min_gap: f64,
    scope: Option<&BTreeSet<usize>>,
    gone: &BTreeSet<usize>,
) -> BTreeSet<usize> {
    let mut removed = BTreeSet::new();
    let mut last: Option<f64> = None;
    for (i, &t) in times.iter().enumerate() {
        if gone.contains(&i) {
            continue;
        }
        if let Some(l) = last {
            if t - l < min_gap - 1e-9 && scope.map(|s| s.contains(&i)).unwrap_or(true) {
                removed.insert(i);
                continue;
            }
        }
        last = Some(t);
    }
    removed
}

/// The ordinals of the frames inside failing windows (from each
/// violation's onset to its end, with a small tolerance).
pub fn flagged_frames(seq: &[(f64, usize)], violations: &[Violation]) -> Vec<usize> {
    let mut out = BTreeSet::new();
    for v in violations {
        let lo = v.onset.min(v.start);
        let hi = v.end;
        for (i, &(t, _)) in seq.iter().enumerate() {
            if lo - 0.05 <= t && t <= hi + 0.05 {
                out.insert(i);
            }
        }
    }
    out.into_iter().collect()
}

/// A context-aware simulation's violations split by where they land.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Classified {
    /// Overlapping the section: the ones its edits can act on.
    pub inside: Vec<Violation>,
    /// Starting after its last frame, in untouched footage.
    pub after: Vec<Violation>,
    /// Starting inside the *next* section: that section's to fix.
    pub elsewhere: Vec<Violation>,
    /// Finished before the section's first frame: the warm-up's own.
    pub before: Vec<Violation>,
}

/// Which side of the boundary a violation falls on is decided by `start`
/// and `end` -- where the flashing actually is -- never by `onset`.
///
/// Flashing that runs up to the section's first frame reaches into it by
/// the change into its first picture, which completes one more flash when
/// that picture differs enough from the last one before it, and a flash
/// completed counts for `boundary` seconds after (the detector's pooling
/// window). The section has to start with some picture of its own, so a
/// flash or an extended flash that is already flashing before it and
/// reaches no further than that is the run-up's, not something its edits
/// can clear. (A pattern is on the frames that show it: one reaching the
/// section is on its frames.)
pub fn classify(result: &AnalysisResult, end_disp: f64, next_at: Option<f64>, boundary: f64) -> Classified {
    let mut c = Classified::default();
    for v in &result.violations {
        if !result.reports(v.kind) {
            continue;
        }
        let run_up = v.kind != ViolationKind::Pattern && v.start < -1e-6 && v.end <= boundary + 1e-6;
        if v.end < -1e-6 || run_up {
            c.before.push(v.clone());
        } else if v.start > end_disp + 1e-6 {
            match next_at {
                Some(na) if v.start >= end_disp + na - 1e-6 => c.elsewhere.push(v.clone()),
                _ => c.after.push(v.clone()),
            }
        } else {
            c.inside.push(v.clone());
        }
    }
    c
}

/// The flashing a check reports for the profile: every violation it
/// reports but a pattern (not something removing frames can fix, so the
/// suggesters leave patterns alone).
fn flashing(result: &AnalysisResult) -> Vec<&Violation> {
    result.violations.iter().filter(|v| v.kind != ViolationKind::Pattern && result.reports(v.kind)).collect()
}

/// Whether the stretch from `t0` to `t1` reaches a failing window (from a
/// violation's onset to its end, a tenth of a second either side).
fn reaches(failing: &[&Violation], t0: f64, t1: f64) -> bool {
    failing.iter().any(|v| t1 >= v.onset.min(v.start) - 0.1 && t0 <= v.end + 0.1)
}

/// Time spans the suggester should work on, from each violation's onset.
fn violation_spans(result: &AnalysisResult, pad: f64) -> Vec<(f64, f64)> {
    let mut spans: Vec<(f64, f64)> = flashing(result).into_iter().map(|v| (v.onset.min(v.start) - pad, v.end + pad)).collect();
    spans.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (s, e) in spans {
        match merged.last_mut() {
            Some(m) if s <= m.1 => m.1 = m.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// Union of the event windows inside a span (whole frame if none).
fn span_bbox(result: &AnalysisResult, s: f64, e: f64, aw: u32, ah: u32) -> [u32; 4] {
    let mut bb = [aw, ah, 0, 0];
    let mut found = false;
    for ev in &result.events {
        if s - 0.5 <= ev.t && ev.t <= e + 0.5 {
            found = true;
            bb[0] = bb[0].min(ev.bbox[0]);
            bb[1] = bb[1].min(ev.bbox[1]);
            bb[2] = bb[2].max(ev.bbox[2]);
            bb[3] = bb[3].max(ev.bbox[3]);
        }
    }
    if found {
        bb
    } else {
        [0, 0, aw, ah]
    }
}

/// Access to a section's cached analysis-resolution frames.
pub trait FrameSource {
    /// 8-bit sRGB pixels of frame `i`, row-major, `bpp()` bytes per pixel.
    fn frame(&self, i: usize) -> &[u8];
    fn bpp(&self) -> usize;
    fn width(&self) -> u32;
    fn height(&self) -> u32;
}

/// Mean relative luminance of the bbox region for each of the given frames.
fn region_metric(frames: &dyn FrameSource, idxs: &[usize], bbox: [u32; 4]) -> Vec<f32> {
    let t = lut();
    let w = frames.width() as usize;
    let bpp = frames.bpp();
    let [x0, y0, x1, y1] = bbox.map(|v| v as usize);
    let count = ((x1 - x0) * (y1 - y0)).max(1) as f64;
    idxs.iter()
        .map(|&i| {
            let f = frames.frame(i);
            let mut sum = 0f64;
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = &f[(y * w + x) * bpp..];
                    let l = luminance(t[p[0] as usize], t[p[1] as usize], t[p[2] as usize]);
                    sum += l as f64;
                }
            }
            (sum / count) as f32
        })
        .collect()
}

/// numpy.percentile with linear interpolation.
fn percentile(vals: &[f32], p: f64) -> f32 {
    if vals.is_empty() {
        return 0.0;
    }
    let mut v = vals.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let rank = p / 100.0 * (v.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let frac = (rank - lo as f64) as f32;
    v[lo] + (v[hi] - v[lo]) * frac
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Prefer {
    Light,
    Dark,
    /// Remove as few frames as possible: take out whichever of the light or
    /// dark frames are fewer, then put back as many flashes as the rules
    /// allow (see [`Suggester`]).
    Fewest,
}

/// Flashes a second the fewest-removals suggester tries to leave in, in
/// order: WCAG's three, then fewer where the profile still objects
/// (extended flashing).
const FEWEST_RATES: [usize; 3] = [3, 2, 1];

/// The fewest-removals suggester's second phase: from a passing removal,
/// flashes (runs of consecutive removed frames) put back.
#[derive(Clone, Debug)]
struct Restore {
    /// The passing removal it started from.
    full: BTreeSet<usize>,
    /// Runs of consecutive frames of `full`: each is one flash taken out.
    pulses: Vec<Vec<usize>>,
    /// Index into FEWEST_RATES.
    rate: usize,
    /// Pulses put back.
    restored: BTreeSet<usize>,
    /// A failing try at this rate has had its failing windows taken out.
    fixed: bool,
}

/// A long run of removed frames the fewest-removals suggester lets some
/// frames back into: from the picture on screen before it (`t0`) to the one
/// after (`t1`), and the frames let back (`back`).
#[derive(Clone, Debug)]
struct ThinRun {
    t0: f64,
    t1: f64,
    back: Vec<usize>,
}

/// The fewest-removals suggester's last phase: where a passing removal
/// leaves a picture frozen for a while, frames come back at a rate that
/// cannot fail by itself, so it moves instead (each try checked).
#[derive(Clone, Debug)]
struct Thin {
    /// The passing removal it started from, and what was said about it.
    base: BTreeSet<usize>,
    note: String,
    runs: Vec<ThinRun>,
    /// The runs still let back into (one inside a failing window is removed again).
    live: BTreeSet<usize>,
    tries: usize,
}

/// Checks the thinning of long removed runs gets before it settles for the
/// removal it started from.
const THIN_TRIES: usize = 3;

/// Rounds of propose, simulate and escalate before the suggester gives up.
const MAX_ROUNDS: usize = 5;

/// What a suggester run produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    /// The removals proposed (inside the scope only).
    pub edits: Edits,
    pub safe: bool,
    pub rounds: usize,
    pub note: String,
}

/// What the suggester wants next.
#[derive(Clone, Debug, PartialEq)]
pub enum SuggestStep {
    /// Simulate these edits (base edits plus proposal) and call `step` again
    /// with the classified result (`inside + after`).
    Simulate(Edits),
    Done(Suggestion),
}

/// Keep-light / keep-dark: propose removals so the section passes.
/// Iterates propose -> simulate -> escalate, up to `MAX_ROUNDS` rounds.
pub struct Suggester {
    rel_pts: Vec<f64>,
    prefer: Prefer,
    /// Remove as few frames as possible (`prefer` then holds the side kept).
    fewest: bool,
    restore: Option<Restore>,
    /// Seconds between pictures that cannot fail (see [`Suggester::thin_long_gaps`]).
    thin_gap: Option<f64>,
    thin: Option<Thin>,
    only: Option<BTreeSet<usize>>,
    /// Frames the user marked "keep": never removed, and their own marks
    /// (a hold, say) stay in force.
    keep: BTreeSet<usize>,
    base_edits: Edits,
    removed: BTreeSet<usize>,
    attempt: usize,
    last_proposal: Edits,
}

impl Suggester {
    /// `existing` are the section's current marks; with `only` set, marks
    /// outside the selection stay in force and are included in every
    /// simulation, and so do the marks on `keep` frames, which are never
    /// removed.
    pub fn new(rel_pts: Vec<f64>, existing: &Edits, prefer: Prefer, only: Option<BTreeSet<usize>>, keep: BTreeSet<usize>) -> Self {
        let base_edits = base_marks(existing, only.as_ref(), &keep);
        Suggester {
            rel_pts,
            prefer: if prefer == Prefer::Fewest { Prefer::Dark } else { prefer },
            fewest: prefer == Prefer::Fewest,
            restore: None,
            thin_gap: None,
            thin: None,
            only,
            keep,
            base_edits: base_edits.clone(),
            removed: BTreeSet::new(),
            attempt: 0,
            last_proposal: base_edits,
        }
    }

    /// For the fewest removals: once the removal passes, let frames back
    /// into long runs of removed frames (a picture frozen for half a second
    /// or more), `min_gap` seconds apart and as far from the pictures either
    /// side, the spacing of a rate that cannot fail by itself (the safe
    /// picture rate). Each try is checked; a run inside a window that still
    /// fails is removed again.
    pub fn thin_long_gaps(&mut self, min_gap: f64) {
        if self.fewest && min_gap.is_finite() && min_gap > 0.0 {
            self.thin_gap = Some(min_gap);
        }
    }

    fn allowed(&self, i: usize) -> bool {
        i != 0 && self.only.as_ref().map(|o| o.contains(&i)).unwrap_or(true) && !self.keep.contains(&i)
    }

    fn indices_in(&self, s: f64, e: f64) -> Vec<usize> {
        self.rel_pts.iter().enumerate().filter(|(_, &t)| t >= s && t <= e).map(|(i, _)| i).collect()
    }

    fn apply_percentile_pass(&mut self, frames: &dyn FrameSource, result: &AnalysisResult, tight: bool) {
        let (aw, ah) = (frames.width(), frames.height());
        for (s, e) in violation_spans(result, 0.3) {
            let idxs = self.indices_in(s, e);
            if idxs.len() < 2 {
                continue;
            }
            let bbox = span_bbox(result, s, e, aw, ah);
            let m = region_metric(frames, &idxs, bbox);
            let hi = percentile(&m, 85.0);
            let lo = percentile(&m, 15.0);
            if hi - lo < 1e-4 {
                continue;
            }
            let cut = if tight {
                if self.prefer == Prefer::Light { hi - 0.15 * (hi - lo) } else { lo + 0.15 * (hi - lo) }
            } else {
                (hi + lo) / 2.0
            };
            for (k, &i) in idxs.iter().enumerate() {
                if !self.allowed(i) {
                    continue;
                }
                let bad = if self.prefer == Prefer::Light { m[k] < cut } else { m[k] > cut };
                if bad {
                    self.removed.insert(i);
                }
            }
        }
    }

    fn apply_hold_all(&mut self, frames: &dyn FrameSource, result: &AnalysisResult) {
        let (aw, ah) = (frames.width(), frames.height());
        for (s, e) in violation_spans(result, 0.3) {
            let idxs = self.indices_in(s, e);
            if idxs.is_empty() {
                continue;
            }
            let bbox = span_bbox(result, s, e, aw, ah);
            let kept: Vec<usize> = idxs.iter().copied().filter(|i| !self.removed.contains(i)).collect();
            if kept.is_empty() {
                continue;
            }
            let m = region_metric(frames, &kept, bbox);
            let anchor = {
                let mut best = 0;
                for k in 1..m.len() {
                    let better = if self.prefer == Prefer::Light { m[k] > m[best] } else { m[k] < m[best] };
                    if better {
                        best = k;
                    }
                }
                kept[best]
            };
            for &i in &idxs {
                if i != anchor && self.allowed(i) {
                    self.removed.insert(i);
                }
            }
        }
    }

    /// For the fewest removals: keep the side with more frames in the
    /// flashing (take out the minority), leaning to keeping the dark ones.
    fn pick_side(&self, frames: &dyn FrameSource, result: &AnalysisResult) -> Prefer {
        let (aw, ah) = (frames.width(), frames.height());
        let (mut light, mut dark) = (0usize, 0usize);
        for (s, e) in violation_spans(result, 0.3) {
            let idxs = self.indices_in(s, e);
            if idxs.len() < 2 {
                continue;
            }
            let m = region_metric(frames, &idxs, span_bbox(result, s, e, aw, ah));
            let cut = (percentile(&m, 85.0) + percentile(&m, 15.0)) / 2.0;
            for (k, &i) in idxs.iter().enumerate() {
                if self.allowed(i) {
                    if m[k] > cut {
                        light += 1;
                    } else {
                        dark += 1;
                    }
                }
            }
        }
        // light frames are the ones removed when dark ones are kept
        if (dark as f64) < 0.8 * light as f64 {
            Prefer::Light
        } else {
            Prefer::Dark
        }
    }

    /// Typical gap between frames, seconds.
    fn frame_gap(&self) -> f64 {
        let mut gaps: Vec<f64> = self.rel_pts.windows(2).map(|w| w[1] - w[0]).filter(|&g| g > 0.0).collect();
        if gaps.is_empty() {
            return 1.0 / 30.0;
        }
        gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
        gaps[gaps.len() / 2]
    }

    /// The pulses put back at the current rate: as many as leave no more
    /// than `rate` in any second (with two frames to spare).
    fn choose_restored(&self, r: &Restore) -> BTreeSet<usize> {
        let per = FEWEST_RATES[r.rate];
        let span = 1.0 + 2.0 * self.frame_gap();
        let mut out: Vec<usize> = Vec::new();
        for (k, p) in r.pulses.iter().enumerate() {
            let t = self.rel_pts[p[0]];
            let n = out.len();
            if n < per || t - self.rel_pts[r.pulses[out[n - per]][0]] > span {
                out.push(k);
            }
        }
        out.into_iter().collect()
    }

    fn apply_restored(&mut self) {
        let r = self.restore.as_ref().unwrap();
        let mut removed = r.full.clone();
        for &k in &r.restored {
            for i in &r.pulses[k] {
                removed.remove(i);
            }
        }
        self.removed = removed;
    }

    /// From a passing removal to the fewest removals: flashes put back.
    fn start_restore(&mut self) -> SuggestStep {
        let full = self.removed.clone();
        let mut pulses: Vec<Vec<usize>> = Vec::new();
        for &i in &full {
            match pulses.last_mut() {
                Some(p) if *p.last().unwrap() + 1 == i => p.push(i),
                _ => pulses.push(vec![i]),
            }
        }
        let mut r = Restore { full, pulses, rate: 0, restored: BTreeSet::new(), fixed: false };
        r.restored = self.choose_restored(&r);
        self.restore = Some(r);
        self.apply_restored();
        self.last_proposal = self.proposal();
        SuggestStep::Simulate(self.last_proposal.clone())
    }

    fn step_restore(&mut self, result: &AnalysisResult) -> SuggestStep {
        let failing = flashing(result);
        let r = self.restore.as_ref().unwrap();
        let kept_side = if self.prefer == Prefer::Light { "light" } else { "dark" };
        if failing.is_empty() {
            let total = r.pulses.len();
            let back = r.restored.len();
            let note = if back == 0 {
                format!("Passes after removing {} frames (every flash taken out: none could stay).", self.removed.len())
            } else {
                format!(
                    "Passes after removing {} frames where taking out every flash would remove {}: {back} of the {total} flashes stay, no more than {} a second{}. The {kept_side} frames are the ones kept.",
                    self.removed.len(),
                    r.full.len(),
                    FEWEST_RATES[r.rate],
                    if FEWEST_RATES[r.rate] == 3 { " (the most WCAG allows)" } else { "" }
                )
            };
            return self.finish_fewest(note);
        }
        self.attempt += 1;
        let mut r = self.restore.take().unwrap();
        if !r.fixed {
            // take back out the flashes put back inside any window that fails
            r.fixed = true;
            let pulses = &r.pulses;
            let rel = &self.rel_pts;
            let before = r.restored.len();
            r.restored.retain(|&k| !reaches(&failing, rel[pulses[k][0]], rel[*pulses[k].last().unwrap()]));
            if r.restored.len() == before {
                // nothing to blame: fewer a second everywhere
                r.fixed = false;
                r.rate += 1;
                if r.rate < FEWEST_RATES.len() {
                    r.restored = self.choose_restored(&r);
                }
            }
        } else {
            r.rate += 1;
            r.fixed = false;
            if r.rate < FEWEST_RATES.len() {
                r.restored = self.choose_restored(&r);
            }
        }
        // nothing left to put back at this rate: fewer a second
        while r.restored.is_empty() && r.rate + 1 < FEWEST_RATES.len() {
            r.rate += 1;
            r.fixed = false;
            r.restored = self.choose_restored(&r);
        }
        if r.rate >= FEWEST_RATES.len() || r.restored.is_empty() {
            // no flash can stay: the passing removal it started from
            self.removed = r.full.clone();
            let note = format!("Passes after removing {} frames: every flash had to go (putting any back failed the check). The {kept_side} frames are the ones kept.", self.removed.len());
            self.restore = Some(r);
            return self.finish_fewest(note);
        }
        self.restore = Some(r);
        self.apply_restored();
        self.last_proposal = self.proposal();
        SuggestStep::Simulate(self.last_proposal.clone())
    }

    /// The fewest removals pass: done, or first some frames let back into
    /// the long runs of removed frames.
    fn finish_fewest(&mut self, note: String) -> SuggestStep {
        if let (Some(gap), None) = (self.thin_gap, &self.thin) {
            let runs = self.thin_runs(gap);
            if !runs.is_empty() {
                let live = (0..runs.len()).collect();
                self.thin = Some(Thin { base: self.removed.clone(), note, runs, live, tries: 0 });
                self.apply_thin();
                self.last_proposal = self.proposal();
                return SuggestStep::Simulate(self.last_proposal.clone());
            }
        }
        self.done(true, note)
    }

    /// The suggestion: the removals so far, `safe` or not, and what to say.
    fn done(&self, safe: bool, note: String) -> SuggestStep {
        SuggestStep::Done(Suggestion { edits: self.removals(), safe, rounds: self.attempt, note })
    }

    /// The runs of removed frames with room for frames `gap` apart, and
    /// `gap` from the pictures on screen either side, with those frames.
    fn thin_runs(&self, gap: f64) -> Vec<ThinRun> {
        let rel = &self.rel_pts;
        let n = rel.len();
        let fg = self.frame_gap();
        let removed: Vec<usize> = self.removed.iter().copied().collect();
        let mut out = Vec::new();
        let mut k = 0;
        while k < removed.len() {
            let mut j = k;
            while j + 1 < removed.len() && removed[j + 1] == removed[j] + 1 {
                j += 1;
            }
            let (a, b) = (removed[k], removed[j]);
            // the pictures either side (a frame removed outside the
            // selection only moves them further off)
            let t0 = if a > 0 { rel[a - 1] } else { rel[a] - fg };
            let t1 = if b + 1 < n { rel[b + 1] } else { rel[b] + fg };
            let mut last = t0;
            let mut back = Vec::new();
            for (i, &t) in rel.iter().enumerate().take(b + 1).skip(a) {
                if t - last >= gap - 1e-9 && t1 - t >= gap - 1e-9 {
                    back.push(i);
                    last = t;
                }
            }
            if !back.is_empty() {
                out.push(ThinRun { t0, t1, back });
            }
            k = j + 1;
        }
        out
    }

    fn apply_thin(&mut self) {
        let th = self.thin.as_ref().unwrap();
        let mut removed = th.base.clone();
        for &k in &th.live {
            for i in &th.runs[k].back {
                removed.remove(i);
            }
        }
        self.removed = removed;
    }

    fn step_thin(&mut self, result: &AnalysisResult) -> SuggestStep {
        let failing = flashing(result);
        let mut th = self.thin.take().unwrap();
        if failing.is_empty() {
            let runs = th.live.len();
            let back: usize = th.live.iter().map(|&k| th.runs[k].back.len()).sum();
            let rate = self.thin_gap.map(|g| 1.0 / g).unwrap_or(0.0);
            let note = format!(
                "{} Then {back} frame{} came back into {runs} long stretch{} where the picture would have frozen, {:.1} a second (a rate that cannot flash by itself): {} removed in all.",
                th.note,
                if back == 1 { "" } else { "s" },
                if runs == 1 { "" } else { "es" },
                (rate * 10.0).floor() / 10.0,
                self.removed.len()
            );
            self.thin = Some(th);
            return self.done(true, note);
        }
        self.attempt += 1;
        th.tries += 1;
        let before = th.live.len();
        let runs = &th.runs;
        th.live.retain(|&k| !reaches(&failing, runs[k].t0, runs[k].t1));
        if th.live.len() == before || th.live.is_empty() || th.tries >= THIN_TRIES {
            // nothing to blame, nothing left, or enough tries: as it was
            self.removed = th.base.clone();
            let note = th.note.clone();
            self.thin = Some(th);
            return self.done(true, note);
        }
        self.thin = Some(th);
        self.apply_thin();
        self.last_proposal = self.proposal();
        SuggestStep::Simulate(self.last_proposal.clone())
    }

    fn proposal(&self) -> Edits {
        let mut edits = self.base_edits.clone();
        for &i in &self.removed {
            edits.insert(i, FrameEdit::removed(Fill::Prev));
        }
        edits
    }

    fn removals(&self) -> Edits {
        self.removed.iter().map(|&i| (i, FrameEdit::removed(Fill::Prev))).collect()
    }

    /// Drive the suggester. Call first with `None`, then with the simulated
    /// result of the last [`SuggestStep::Simulate`]. `frames` is the
    /// section's frame cache.
    pub fn step(&mut self, frames: &dyn FrameSource, result: Option<&AnalysisResult>) -> SuggestStep {
        let Some(result) = result else {
            // round 0: does the section already pass with the base edits?
            self.last_proposal = self.base_edits.clone();
            return SuggestStep::Simulate(self.last_proposal.clone());
        };
        if self.thin.is_some() {
            return self.step_thin(result);
        }
        if self.restore.is_some() {
            return self.step_restore(result);
        }
        let flashes_ok = flashing(result).is_empty();
        // the fewest removals: a passing removal first, then flashes put back
        if flashes_ok && self.fewest && self.attempt > 0 && !self.removed.is_empty() {
            return self.start_restore();
        }
        if flashes_ok {
            let patterns = result.violations.iter().any(|v| v.kind == ViolationKind::Pattern && result.reports(v.kind));
            let mut note = if self.attempt == 0 {
                "Already passes, nothing to remove.".to_string()
            } else {
                format!("Passes after removing {} frames.", self.removed.len())
            };
            if patterns {
                note = if self.attempt == 0 {
                    "No flashing to remove.".to_string()
                } else {
                    format!("No flashing left after removing {} frames.", self.removed.len())
                };
                note.push_str(" A regular pattern (stripes) remains; removing frames cannot fix that. Turn on “soften stripes” for this section instead.");
            }
            // (nothing is removed before the first round)
            return self.done(true, note);
        }
        if self.attempt >= MAX_ROUNDS {
            let note = format!(
                "Still failing after {} removals. {}",
                self.removed.len(),
                if self.only.is_some() {
                    "Try widening the selection, or edit by hand."
                } else if !self.keep.is_empty() {
                    "The frames marked keep were left alone; unkeep some, or edit by hand."
                } else {
                    "Edit this one by hand."
                }
            );
            return self.done(false, note);
        }
        if self.fewest && self.attempt == 0 {
            self.prefer = self.pick_side(frames, result);
        }
        match self.attempt {
            0 => self.apply_percentile_pass(frames, result, false),
            1 => self.apply_percentile_pass(frames, result, true),
            _ => self.apply_hold_all(frames, result),
        }
        self.attempt += 1;
        self.last_proposal = self.proposal();
        SuggestStep::Simulate(self.last_proposal.clone())
    }
}

/// The frames that make the flashing, on its minority side: the light
/// frames among dark ones, or the other way round (the side the
/// fewest-removals suggester would take out), with the side they are on.
/// `tight`: only the most extreme of them. For blending rather than
/// removing (see [`crate::blend`]).
pub fn flash_frames(rel_pts: Vec<f64>, frames: &dyn FrameSource, result: &AnalysisResult, only: Option<BTreeSet<usize>>, keep: BTreeSet<usize>, tight: bool) -> (Vec<usize>, Prefer) {
    let mut s = Suggester::new(rel_pts, &Edits::new(), Prefer::Fewest, only, keep);
    s.prefer = s.pick_side(frames, result);
    s.apply_percentile_pass(frames, result, tight);
    // the side kept is the one not blended
    let blended_side = if s.prefer == Prefer::Dark { Prefer::Light } else { Prefer::Dark };
    (s.removed.into_iter().collect(), blended_side)
}

/// A "reduce FPS" proposal: removals that thin the section down to `fps`
/// pictures a second, from timestamps alone. (Read back with every field
/// optional, as the page hands it back to [`rate_note`].)
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateProposal {
    /// Base edits (outside the scope) plus the thinning removals: what to
    /// simulate.
    pub edits: Edits,
    /// The thinning removals alone.
    pub removals: Edits,
    pub fps: f64,
    pub safe_fps: f64,
    pub guaranteed: bool,
    pub pool: usize,
    pub n_removed: usize,
    pub only: bool,
    /// Frames marked keep (never removed).
    pub kept: usize,
}

/// Build the thinning proposal. `fps = None` uses the profile's safe rate.
pub fn rate_proposal(
    cfg: &DetectorConfig,
    rel_pts: &[f64],
    existing: &Edits,
    only: Option<&BTreeSet<usize>>,
    keep: &BTreeSet<usize>,
    fps: Option<f64>,
    extension_seconds: f64,
) -> Result<RateProposal, String> {
    let n = rel_pts.len();
    if n == 0 {
        return Err("Section has no frames".into());
    }
    let (safe_fps, safe_gap) = safe_picture_rate(cfg);
    let (fps, min_gap) = match fps {
        None => {
            if safe_fps <= 0.0 {
                return Err("This profile treats a single flash as a violation, so no frame rate is safe by timing alone. Choose a rate yourself and let the check judge it.".into());
            }
            (safe_fps, safe_gap)
        }
        Some(f) => {
            if !(f > 0.0 && f <= MAX_TARGET_FPS) {
                return Err(format!("A target rate has to be between 0 and {MAX_TARGET_FPS} pictures a second."));
            }
            (f, 1.0 / f)
        }
    };
    let guaranteed = rate_is_guaranteed(cfg, fps);
    let base_edits = base_marks(existing, only, keep);
    let times = picture_times(rel_pts, &base_edits, extension_seconds);
    let gone: BTreeSet<usize> = base_edits.iter().filter(|(_, e)| e.removed).map(|(k, _)| *k).collect();
    // kept frames still set the pace; they are just never the ones removed
    let scope: Option<BTreeSet<usize>> = if keep.is_empty() {
        only.cloned()
    } else {
        Some((0..n).filter(|i| !keep.contains(i) && only.map(|o| o.contains(i)).unwrap_or(true)).collect())
    };
    let removed = rate_limited_removals(&times, min_gap, scope.as_ref(), &gone);
    let removals: Edits = removed.iter().map(|&i| (i, FrameEdit::removed(Fill::Prev))).collect();
    let mut edits = base_edits;
    edits.extend(removals.iter().map(|(k, v)| (*k, *v)));
    let pool = (0..n).filter(|i| !gone.contains(i) && only.map(|o| o.contains(i)).unwrap_or(true)).count();
    Ok(RateProposal {
        edits,
        removals,
        fps,
        safe_fps,
        guaranteed,
        pool,
        n_removed: removed.len(),
        only: only.is_some(),
        kept: keep.len(),
    })
}

/// The marks a suggestion leaves in force: those outside the selection (when
/// there is one) and those on frames marked keep.
fn base_marks(existing: &Edits, only: Option<&BTreeSet<usize>>, keep: &BTreeSet<usize>) -> Edits {
    existing.iter().filter(|(k, _)| keep.contains(k) || only.map(|o| !o.contains(k)).unwrap_or(false)).map(|(k, v)| (*k, *v)).collect()
}

/// The note for a rate proposal once its simulation is in.
pub fn rate_note(p: &RateProposal, safe: bool) -> String {
    let where_ = if p.only { " in the selection" } else { "" };
    let mut note = format!(
        "Thinned to {}/s: {} of {} frames{} kept, {} removed.",
        fmt_g(p.fps),
        p.pool - p.n_removed,
        p.pool,
        where_,
        p.n_removed
    );
    if safe && !p.guaranteed {
        note += &format!(
            " That is above the guaranteed-safe {}/s, so it passes on what these frames actually do rather than by arithmetic. Re-check it if you edit anywhere near it.",
            fmt_g(p.safe_fps)
        );
    } else if !safe {
        if !p.guaranteed {
            note += &format!(
                " Still failing above the guaranteed-safe {}/s. Drop to that and it cannot fail on the frames this was allowed to touch.",
                fmt_g(p.safe_fps)
            );
        } else if p.only {
            note += " Still failing. The flashing that is left runs outside the selection. Widen it, or run this on the whole section.";
        } else if p.kept > 0 {
            note += " Still failing. The frames marked keep were left alone, and the flashing that is left may be theirs; unkeep some, or edit by hand.";
        } else {
            note += " Still failing. The flashing that is left runs into the footage either side of this section, which nothing in here can reach.";
        }
    }
    note
}

/// Python's `{:g}`-ish formatting for rates.
fn fmt_g(x: f64) -> String {
    if (x - x.round()).abs() < 1e-9 {
        format!("{}", x.round() as i64)
    } else {
        let s = format!("{:.6}", x);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Merge suggested removals into a section's marks, replacing any previous
/// suggestion inside the scope (the reference's behaviour: whichever
/// suggestion ran last wins).
pub fn apply_suggestion(existing: &Edits, suggested: &Edits, only: Option<&BTreeSet<usize>>, keep: &BTreeSet<usize>) -> Edits {
    let mut out = base_marks(existing, only, keep);
    out.extend(suggested.iter().filter(|(k, _)| !keep.contains(k)).map(|(k, v)| (*k, *v)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn removed(i: usize) -> (usize, FrameEdit) {
        (i, FrameEdit::removed(Fill::Prev))
    }

    #[test]
    fn replacement_map_directions() {
        let mut e = Edits::new();
        e.insert(1, FrameEdit::removed(Fill::Prev));
        e.insert(2, FrameEdit::removed(Fill::Next));
        e.insert(0, FrameEdit::removed(Fill::Prev)); // nothing before: falls forward
        e.insert(4, FrameEdit::removed(Fill::Next)); // nothing after: falls back
        assert_eq!(replacement_map(&e, 5), vec![3, 3, 3, 3, 3]);
        let mut e = Edits::new();
        e.insert(1, FrameEdit::removed(Fill::Prev));
        e.insert(2, FrameEdit::removed(Fill::Next));
        assert_eq!(replacement_map(&e, 4), vec![0, 0, 3, 3]);
        let all: Edits = (0..3).map(removed).collect();
        assert_eq!(replacement_map(&all, 3), vec![0, 0, 0]);
    }

    #[test]
    fn holds_are_where_the_sequence_waits() {
        let pts: Vec<f64> = (0..12).map(|i| i as f64 / 30.0).collect();
        let mut e = Edits::new();
        e.insert(2, FrameEdit::extended());
        e.insert(5, FrameEdit::removed(Fill::Prev));
        e.insert(6, FrameEdit { removed: true, extended: true, fill: Fill::Next });
        e.insert(9, FrameEdit::extended());
        e.insert(11, FrameEdit::extended());
        let end = 12.0 / 30.0;
        let hs = holds(&pts, &e, 1.0, end);
        // frame 2 holds until frame 3's time, 9 until 10's, the last until the end; a removed frame holds nothing
        assert_eq!(hs, vec![Hold { at: pts[3], seconds: 1.0 }, Hold { at: pts[10], seconds: 1.0 }, Hold { at: end, seconds: 1.0 }]);
        // how much later the holds make a moment of the source: every hold at or before it
        let held_before = |t: f64| hs.iter().take_while(|h| h.at <= t).map(|h| h.seconds).sum::<f64>();
        let seq = edited_sequence(&pts, &e, 1.0);
        for (i, &(t, _)) in seq.iter().enumerate() {
            assert_eq!(t, pts[i] + held_before(pts[i]), "frame {i}");
        }
        assert_eq!(seq[3].0 - seq[2].0, 1.0 + 1.0 / 30.0);
        // the last frame's hold is counted: everything after the section comes three seconds late
        assert_eq!(hs.iter().map(|h| h.seconds).sum::<f64>(), 3.0);
        assert_eq!(held_before(end), 3.0);
        assert_eq!(held_before(pts[3] - 1e-9), 0.0);
    }

    #[test]
    fn sequence_and_picture_times() {
        let pts = [0.0, 0.1, 0.2, 0.3];
        let mut e = Edits::new();
        e.insert(1, FrameEdit::extended());
        e.insert(2, FrameEdit::removed(Fill::Next));
        let seq = edited_sequence(&pts, &e, 1.0);
        assert_eq!(seq, vec![(0.0, 0), (0.1, 1), (1.2, 3), (1.3, 3)]);
        let pt = picture_times(&pts, &e, 1.0);
        assert_eq!(pt, vec![0.0, 0.1, 1.2, 1.2]);
    }

    #[test]
    fn thinning_by_time() {
        let times: Vec<f64> = (0..10).map(|i| i as f64 * 0.1).collect();
        let r = rate_limited_removals(&times, 0.25, None, &BTreeSet::new());
        // keep 0.0, 0.3, 0.6, 0.9 -> remove the rest
        assert_eq!(r, [1, 2, 4, 5, 7, 8].into_iter().collect());
        let scope: BTreeSet<usize> = (3..7).collect();
        let r = rate_limited_removals(&times, 0.25, Some(&scope), &BTreeSet::new());
        // frames outside the scope pace it: 0.2 is kept, so 0.3 and 0.4 go,
        // 0.5 survives, 0.6 goes, 0.7 is outside
        assert_eq!(r, [3, 4, 6].into_iter().collect());
    }

    #[test]
    fn classification() {
        let v = |s: f64, e: f64| Violation { start: s, end: e, kind: ViolationKind::Flash, count: 1.0, onset: s - 0.5, peak: s };
        let res = AnalysisResult {
            violations: vec![v(-2.0, -1.0), v(0.5, 1.0), v(3.0, 3.5), v(5.0, 6.0)],
            flag_extended: true,
            ..Default::default()
        };
        // next section starts 2.5 s after the section's last frame
        let c = classify(&res, 2.0, Some(2.5), 0.125);
        assert_eq!(c.before.len(), 1);
        assert_eq!(c.inside.len(), 1);
        assert_eq!(c.after.len(), 1);
        assert_eq!(c.elsewhere.len(), 1);
        let c = classify(&res, 2.0, None, 0.125);
        assert_eq!(c.after.len(), 2);
    }

    #[test]
    fn flashing_that_only_reaches_the_first_picture_is_the_run_ups() {
        let v = |kind: ViolationKind, s: f64, e: f64| Violation { start: s, end: e, kind, count: 1.0, onset: s, peak: s };
        let res = AnalysisResult {
            violations: vec![
                // flashing before the section whose last flash the change into its first picture completes
                v(ViolationKind::Extended, -4.0, 0.0),
                v(ViolationKind::Flash, -1.0, 0.1),
                // ... and flashing that goes on in the section's own frames
                v(ViolationKind::Extended, -4.0, 0.5),
                v(ViolationKind::Red, -1.0, 0.2),
                // flashing that starts at the section's first frame is its own
                v(ViolationKind::Flash, 0.0, 0.1),
                // stripes reaching the section are on its frames
                v(ViolationKind::Pattern, -2.0, 0.05),
            ],
            flag_extended: true,
            flag_patterns: true,
            ..Default::default()
        };
        let c = classify(&res, 5.0, None, 0.125);
        let ends = |vs: &[Violation]| vs.iter().map(|v| (v.kind, v.end)).collect::<Vec<_>>();
        assert_eq!(ends(&c.before), vec![(ViolationKind::Extended, 0.0), (ViolationKind::Flash, 0.1)]);
        assert_eq!(ends(&c.inside), vec![(ViolationKind::Extended, 0.5), (ViolationKind::Red, 0.2), (ViolationKind::Flash, 0.1), (ViolationKind::Pattern, 0.05)]);
    }

    #[test]
    fn flagged() {
        let seq = vec![(0.0, 0), (0.1, 1), (0.2, 2), (0.3, 3)];
        let v = Violation { start: 0.2, end: 0.2, kind: ViolationKind::Flash, count: 1.0, onset: 0.1, peak: 0.2 };
        assert_eq!(flagged_frames(&seq, &[v]), vec![1, 2]);
    }

    #[test]
    fn percentiles() {
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 50.0), 2.5);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 85.0), 3.55);
    }

    #[test]
    fn kept_frames_are_never_removed_and_keep_their_marks() {
        let cfg = DetectorConfig::default();
        let pts: Vec<f64> = (0..48).map(|i| i as f64 / 24.0).collect();
        let mut existing = Edits::new();
        existing.insert(5, FrameEdit::extended());
        existing.insert(6, FrameEdit::removed(Fill::Prev));
        let keep: BTreeSet<usize> = [5, 7, 8, 9].into_iter().collect();
        let p = rate_proposal(&cfg, &pts, &existing, None, &keep, None, 1.0).unwrap();
        for k in &keep {
            assert!(!p.removals.contains_key(k), "kept frame {k} removed");
        }
        // the hold on the kept frame stays in force; the removal elsewhere is replaced
        assert_eq!(p.edits.get(&5), Some(&FrameEdit::extended()));
        assert!(p.n_removed > 0);
        assert_eq!(p.kept, 4);
        let merged = apply_suggestion(&existing, &p.removals, None, &keep);
        assert_eq!(merged.get(&5), Some(&FrameEdit::extended()));
        assert!(keep.iter().all(|k| !merged.get(k).map(|e| e.removed).unwrap_or(false)));
        // the suggester leaves kept frames alone too
        let mut s = Suggester::new(pts.clone(), &existing, Prefer::Dark, None, keep.clone());
        assert!(!s.allowed(7) && !s.allowed(0) && s.allowed(10));
        assert_eq!(s.base_edits.get(&5), Some(&FrameEdit::extended()));
        assert!(!s.base_edits.contains_key(&6));
        s.removed.insert(10);
        assert!(s.proposal().contains_key(&5) && s.proposal().contains_key(&10));
        // without keep marks nothing changes: a suggestion replaces every mark
        assert!(base_marks(&existing, None, &BTreeSet::new()).is_empty());
    }

    /// A strobe, 24 fps: one light frame in three (8 flashes a second) over
    /// half the picture, for two seconds.
    struct Strobe {
        frames: Vec<Vec<u8>>,
        w: u32,
        h: u32,
    }

    impl FrameSource for Strobe {
        fn frame(&self, i: usize) -> &[u8] {
            &self.frames[i]
        }
        fn bpp(&self) -> usize {
            3
        }
        fn width(&self) -> u32 {
            self.w
        }
        fn height(&self) -> u32 {
            self.h
        }
    }

    fn strobe(n: usize) -> Strobe {
        let (w, h) = (64u32, 48u32);
        let frames = (0..n)
            .map(|i| {
                let code = if i % 3 == 1 { 220u8 } else { 20 };
                let mut f = vec![10u8; (w * h * 3) as usize];
                for y in 0..h {
                    for x in 0..w / 2 + 8 {
                        let k = ((y * w + x) * 3) as usize;
                        f[k..k + 3].copy_from_slice(&[code, code, code]);
                    }
                }
                f
            })
            .collect();
        Strobe { frames, w, h }
    }

    /// The section check the app runs, on the CPU: the edited sequence
    /// through the detector.
    fn simulate(src: &Strobe, pts: &[f64], edits: &Edits) -> AnalysisResult {
        let seq = edited_sequence(pts, edits, 1.0);
        crate::detector::CpuDetector::analyze(crate::config::Profile::Wcag.config(), src.w, src.h, seq.iter().map(|&(t, i)| (t, crate::grid::FrameInput::rgb(&src.frames[i]))))
    }

    fn run(src: &Strobe, pts: &[f64], prefer: Prefer) -> (Suggestion, usize) {
        let mut s = Suggester::new(pts.to_vec(), &Edits::new(), prefer, None, BTreeSet::new());
        let mut step = s.step(src, None);
        let mut sims = 0;
        while let SuggestStep::Simulate(e) = step {
            let r = simulate(src, pts, &e);
            sims += 1;
            step = s.step(src, Some(&r));
        }
        match step {
            SuggestStep::Done(d) => (d, sims),
            _ => unreachable!(),
        }
    }

    #[test]
    fn fewest_removals_keeps_what_the_rules_allow() {
        let src = strobe(48);
        let pts: Vec<f64> = (0..48).map(|i| i as f64 / 24.0).collect();
        assert!(!simulate(&src, &pts, &Edits::new()).safe(), "the strobe fails as it is");
        let (dark, _) = run(&src, &pts, Prefer::Dark);
        let (few, sims) = run(&src, &pts, Prefer::Fewest);
        assert!(dark.safe && few.safe, "{} | {}", dark.note, few.note);
        // the result really passes, with fewer frames gone than keep-dark takes
        assert!(simulate(&src, &pts, &few.edits).safe(), "{}", few.note);
        assert!(few.edits.len() < dark.edits.len(), "fewest {} vs keep dark {}: {}", few.edits.len(), dark.edits.len(), few.note);
        // the light frames are the minority: only they go
        assert!(few.edits.keys().all(|&i| i % 3 == 1), "{:?}", few.edits.keys().collect::<Vec<_>>());
        assert!(few.note.contains("flashes stay"), "{}", few.note);
        eprintln!("keep dark: {} | fewest ({sims} checks): {}", dark.note, few.note);
        assert!(sims <= 12, "{sims} simulations");
        // no more than three flashes stay in any second
        let stay: Vec<f64> = (0..48).filter(|i| i % 3 == 1 && !few.edits.contains_key(i)).map(|i| pts[i]).collect();
        for w in stay.windows(4) {
            assert!(w[3] - w[0] > 1.0, "four flashes within a second: {w:?}");
        }
    }

    /// Steady dark frames with a strobe over most of the picture from 1 s
    /// to 3 s (30 fps): the removal that passes is the whole strobe, the
    /// picture frozen for two seconds.
    fn strobe_burst() -> (Strobe, Vec<f64>, BTreeSet<usize>) {
        let (w, h) = (64u32, 48u32);
        let frames = (0..120)
            .map(|i| {
                let code = if (30..90).contains(&i) && i % 3 == 1 { 220u8 } else { 20 };
                let mut f = vec![10u8; (w * h * 3) as usize];
                for y in 0..h {
                    for x in 0..w / 2 + 8 {
                        let k = ((y * w + x) * 3) as usize;
                        f[k..k + 3].copy_from_slice(&[code, code, code]);
                    }
                }
                f
            })
            .collect();
        let pts: Vec<f64> = (0..120).map(|i| i as f64 / 30.0).collect();
        (Strobe { frames, w, h }, pts, (30..90).collect())
    }

    /// Drive a fewest-removals suggester from its last phase: `removed`
    /// passing, frames let back `gap` apart.
    fn thin_from(src: &Strobe, pts: &[f64], removed: &BTreeSet<usize>, gap: f64) -> Suggestion {
        let mut s = Suggester::new(pts.to_vec(), &Edits::new(), Prefer::Fewest, None, BTreeSet::new());
        s.thin_long_gaps(gap);
        s.removed = removed.clone();
        let mut step = s.finish_fewest("Passes.".into());
        while let SuggestStep::Simulate(e) = step {
            step = s.step(src, Some(&simulate(src, pts, &e)));
        }
        match step {
            SuggestStep::Done(d) => d,
            _ => unreachable!(),
        }
    }

    #[test]
    fn fewest_removals_let_frames_back_into_long_gaps() {
        let (src, pts, burst) = strobe_burst();
        let removal: Edits = burst.iter().map(|&i| (i, FrameEdit::removed(Fill::Prev))).collect();
        assert!(!simulate(&src, &pts, &Edits::new()).safe() && simulate(&src, &pts, &removal).safe());
        let gap = 1.0 / 3.8;
        let d = thin_from(&src, &pts, &burst, gap);
        assert!(d.safe && simulate(&src, &pts, &d.edits).safe(), "{}", d.note);
        // frames came back, at the safe spacing, from each other and the pictures either side
        let back: Vec<usize> = burst.iter().copied().filter(|i| !d.edits.contains_key(i)).collect();
        assert!(back.len() >= 5, "{back:?}: {}", d.note);
        let mut times = vec![pts[29]];
        times.extend(back.iter().map(|&i| pts[i]));
        times.push(pts[90]);
        assert!(times.windows(2).all(|w| w[1] - w[0] >= gap - 1e-9), "{times:?}");
        assert!(d.note.contains("came back into 1 long stretch") && d.note.contains("3.8 a second"), "{}", d.note);
        // too close together, the strobe is back: the run is removed again
        let d = thin_from(&src, &pts, &burst, 1.0 / 15.0);
        assert!(d.safe && simulate(&src, &pts, &d.edits).safe(), "{}", d.note);
        assert_eq!(d.edits.keys().copied().collect::<BTreeSet<_>>(), burst, "{}", d.note);
        assert_eq!(d.note, "Passes.");
        // short runs (a flash's own frames) are left as they are
        let d = thin_from(&src, &pts, &(30..90).filter(|i| i % 3 == 1).collect(), gap);
        assert!(d.note == "Passes." && d.edits.len() == 20, "{}", d.note);
    }

    #[test]
    fn rate_proposal_defaults_to_safe_rate() {
        let cfg = DetectorConfig::default();
        let pts: Vec<f64> = (0..48).map(|i| i as f64 / 24.0).collect();
        let p = rate_proposal(&cfg, &pts, &Edits::new(), None, &BTreeSet::new(), None, 1.0).unwrap();
        assert_eq!(p.fps, 3.80);
        assert!(p.guaranteed);
        // 2 s at 3.8/s keeps ~8 pictures
        assert!(p.pool - p.n_removed <= 9 && p.pool - p.n_removed >= 7);
        assert!(rate_note(&p, true).starts_with("Thinned to 3.8/s"));
        assert!(rate_proposal(&cfg, &pts, &Edits::new(), None, &BTreeSet::new(), Some(0.0), 1.0).is_err());
    }

    #[test]
    fn edits_json_round_trip() {
        let mut e = Edits::new();
        e.insert(3, FrameEdit::removed(Fill::Next));
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, r#"{"3":{"removed":true,"extended":false,"fill":"next"}}"#);
        let back: Edits = serde_json::from_str(r#"{"3":{"removed":true}}"#).unwrap();
        assert_eq!(back[&3], FrameEdit::removed(Fill::Prev));
    }
}
