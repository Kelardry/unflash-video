//! Turning violations into work sections, and the arithmetic bounds a
//! section edit can rely on (port of the section/rate parts of
//! `analysis.py`).

use serde::{Deserialize, Serialize};

use crate::config::DetectorConfig;
use crate::pixel::MAX_RUN_SECONDS;
use crate::temporal::{AnalysisResult, EventKind, Violation, ViolationKind};

/// Extra span asked of every flash window on top of the second the detector
/// measures against, so a rate-reduced section keeps its margin through the
/// render (a picture can land up to a whole grid slot early).
const RATE_SAFETY_MARGIN: f64 = 0.05;

/// Ceiling on a hand-typed target rate for "reduce FPS".
pub(crate) const MAX_TARGET_FPS: f64 = 1000.0;

/// How much of the run-up a detector has to have seen before its verdict at
/// a given moment matches the verdict a pass over the whole video gives at
/// that same moment.
pub fn context_seconds(cfg: &DetectorConfig) -> f64 {
    // pairing + failure window + run cap + margin
    let mut base = 1.0 + 1.0 + MAX_RUN_SECONDS + 0.5;
    if cfg.flag_extended() {
        base = base.max(cfg.extended_window + cfg.extended_hold + 0.5);
    }
    base
}

/// How many frame intervals a burst of flashing needs to trip `cfg`.
/// Returns 0 when no frame rate can be safe.
fn flash_window_frames(cfg: &DetectorConfig) -> u32 {
    let k_fail = cfg.k_fail() as i64;
    let mut m = 2 * (k_fail - 1);
    if cfg.flag_extended() {
        let k_ext = cfg.ext_rate() as i64;
        m = m.min(2 * (k_ext - 1));
    }
    m.max(0) as u32
}

/// (pictures per second, seconds between them) that `cfg` cannot fail,
/// with `RATE_SAFETY_MARGIN` to spare, rounded *down* to two decimals.
/// `(0.0, inf)` when no rate can satisfy it.
pub fn safe_picture_rate(cfg: &DetectorConfig) -> (f64, f64) {
    let m = flash_window_frames(cfg);
    if m < 1 {
        return (0.0, f64::INFINITY);
    }
    let fps = (m as f64 / (1.0 + RATE_SAFETY_MARGIN) * 100.0).floor() / 100.0;
    (fps, 1.0 / fps)
}

/// Is thinning to `fps` pictures a second safe by arithmetic alone?
pub fn rate_is_guaranteed(cfg: &DetectorConfig, fps: f64) -> bool {
    let (safe, _) = safe_picture_rate(cfg);
    safe > 0.0 && fps <= safe + 1e-9
}

/// Snap [start, end] outward to keyframes, clamped into the video's real
/// timeline bounds.
fn snap_to_keyframes(keyframes: &[f64], start: f64, end: f64, bounds: (f64, f64)) -> (f64, f64) {
    let (ts_min, ts_max) = bounds;
    let clamp = |x: f64| x.min(ts_max).max(ts_min);
    let mut start = clamp(start);
    let mut end = clamp(end);
    if !keyframes.is_empty() {
        // bisect_right(keyframes, start + 1e-6) - 1
        let i = keyframes.partition_point(|&k| k <= start + 1e-6);
        if i >= 1 {
            start = keyframes[i - 1];
        }
        // bisect_left(keyframes, end - 1e-6)
        let j = keyframes.partition_point(|&k| k < end - 1e-6);
        end = if j < keyframes.len() { keyframes[j] } else { ts_max };
    }
    start = clamp(start);
    end = clamp(end);
    if end <= start {
        end = ts_max;
    }
    (start, end)
}

/// A work section proposed by a scan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SectionSpan {
    pub start: f64,
    pub end: f64,
    /// Sorted by name ("extended" < "flash" < "red"), like the reference.
    pub kinds: Vec<ViolationKind>,
}

/// Merge violations into padded work sections. `bounds` is the video's
/// real native-pts range. Sections are padded out from each violation's
/// `onset`, so a section always contains the frames responsible for its own
/// violation.
pub fn violations_to_sections(
    violations: &[Violation],
    cfg: &DetectorConfig,
    bounds: (f64, f64),
    keyframes: &[f64],
) -> Vec<SectionSpan> {
    let (ts_min, ts_max) = bounds;
    let mut intervals: Vec<(f64, f64, Vec<ViolationKind>)> = Vec::new();
    for v in violations {
        if v.kind == ViolationKind::Extended && !cfg.flag_extended() {
            continue;
        }
        if v.kind == ViolationKind::Pattern && !cfg.flag_patterns() {
            continue;
        }
        let mut s = (v.onset.min(v.start) - cfg.section_pad).max(ts_min);
        let mut e = (v.end + cfg.section_pad).min(ts_max);
        if e - s < cfg.section_min_len {
            let mid = (s + e) / 2.0;
            s = (mid - cfg.section_min_len / 2.0).max(ts_min);
            e = (s + cfg.section_min_len).min(ts_max);
        }
        if e > s {
            intervals.push((s, e, vec![v.kind]));
        }
    }
    intervals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut merged: Vec<(f64, f64, Vec<ViolationKind>)> = Vec::new();
    for (s, e, kinds) in intervals {
        match merged.last_mut() {
            Some(m) if s <= m.1 + cfg.section_merge_gap => {
                m.1 = m.1.max(e);
                for k in kinds {
                    if !m.2.contains(&k) {
                        m.2.push(k);
                    }
                }
            }
            _ => merged.push((s, e, kinds)),
        }
    }

    // split over-long sections
    let mut split: Vec<(f64, f64, Vec<ViolationKind>)> = Vec::new();
    for (s, e, kinds) in merged {
        let length = e - s;
        if length <= cfg.section_max_len {
            split.push((s, e, kinds));
        } else {
            let parts = (length / cfg.section_max_len).ceil() as usize;
            let step = length / parts as f64;
            for k in 0..parts {
                split.push((s + k as f64 * step, e.min(s + (k + 1) as f64 * step), kinds.clone()));
            }
        }
    }

    let mut out: Vec<SectionSpan> = Vec::new();
    for (s, e, mut kinds) in split {
        let (ks, ke) = snap_to_keyframes(keyframes, s, e, bounds);
        kinds.sort();
        kinds.dedup();
        let sec = SectionSpan { start: round6(ks), end: round6(ke), kinds };
        // snapping can make neighbours touch/overlap; merge those
        match out.last_mut() {
            Some(last) if sec.start < last.end - 1e-6 => {
                last.end = last.end.max(sec.end);
                for k in sec.kinds {
                    if !last.kinds.contains(&k) {
                        last.kinds.push(k);
                    }
                }
                last.kinds.sort();
            }
            _ => out.push(sec),
        }
    }
    out
}

fn round6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

/// Per-second flash counts for the whole-video heatmap (native time).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TimelineSummary {
    pub bin: f64,
    pub t0: f64,
    pub general: Vec<f32>,
    pub red: Vec<f32>,
    /// Bins inside a reported regular-pattern violation (its severity).
    #[serde(default)]
    pub pattern: Vec<f32>,
}

pub fn timeline_summary(result: &AnalysisResult, bounds: (f64, f64), bin_seconds: f64) -> TimelineSummary {
    let (ts_min, ts_max) = bounds;
    let span = (ts_max - ts_min).max(1e-6);
    let nbins = ((span / bin_seconds).ceil() as usize).max(1);
    let mut general = vec![0f32; nbins];
    let mut red = vec![0f32; nbins];
    let mut pattern = vec![0f32; nbins];
    for e in &result.events {
        let b = (((e.t - ts_min) / bin_seconds) as i64).clamp(0, nbins as i64 - 1) as usize;
        match e.kind {
            EventKind::General => general[b] += 1.0,
            EventKind::Red => red[b] += 1.0,
        }
    }
    for v in result.violations.iter().filter(|v| v.kind == ViolationKind::Pattern && result.reports(v.kind)) {
        let b0 = (((v.start - ts_min) / bin_seconds) as i64).clamp(0, nbins as i64 - 1) as usize;
        let b1 = (((v.end - ts_min) / bin_seconds) as i64).clamp(0, nbins as i64 - 1) as usize;
        for b in b0..=b1 {
            pattern[b] = pattern[b].max(v.count as f32);
        }
    }
    TimelineSummary { bin: bin_seconds, t0: ts_min, general, red, pattern }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;

    #[test]
    fn safe_rates_match_documentation() {
        let ext = Profile::WcagExt.config();
        let wcag = Profile::Wcag.config();
        let strict = Profile::Strict.config();
        assert_eq!(flash_window_frames(&ext), 4);
        assert_eq!(flash_window_frames(&wcag), 6);
        assert_eq!(flash_window_frames(&strict), 4);
        assert_eq!(safe_picture_rate(&ext).0, 3.80);
        assert_eq!(safe_picture_rate(&wcag).0, 5.71);
        assert_eq!(safe_picture_rate(&strict).0, 3.80);
        assert!(rate_is_guaranteed(&ext, 3.8));
        assert!(!rate_is_guaranteed(&ext, 3.81));
        assert_eq!(context_seconds(&ext), 6.5);
        assert_eq!(context_seconds(&wcag), 4.5);
    }

    #[test]
    fn keyframe_snapping() {
        let kf = [0.0, 2.0, 4.0, 6.0];
        assert_eq!(snap_to_keyframes(&kf, 2.5, 3.5, (0.0, 10.0)), (2.0, 4.0));
        assert_eq!(snap_to_keyframes(&kf, 5.0, 9.0, (0.0, 10.0)), (4.0, 10.0));
        assert_eq!(snap_to_keyframes(&[], 5.0, 9.0, (0.0, 10.0)), (5.0, 9.0));
        assert_eq!(snap_to_keyframes(&[], 9.0, 5.0, (0.0, 10.0)), (9.0, 10.0));
    }

    #[test]
    fn sections_pad_merge_and_split() {
        let cfg = DetectorConfig::default();
        let v = |s: f64, e: f64, onset: f64, kind| Violation { start: s, end: e, kind, count: 1.0, onset, peak: s };
        let vs = vec![
            v(10.0, 10.5, 9.2, ViolationKind::Flash),
            v(12.0, 12.2, 12.0, ViolationKind::Red),
            v(30.0, 30.1, 30.0, ViolationKind::Extended),
        ];
        let secs = violations_to_sections(&vs, &cfg, (0.0, 100.0), &[]);
        assert_eq!(secs.len(), 2);
        assert!((secs[0].start - 7.7).abs() < 1e-6);
        assert!((secs[0].end - 13.7).abs() < 1e-6);
        assert_eq!(secs[0].kinds, vec![ViolationKind::Flash, ViolationKind::Red]);
        assert_eq!(secs[1].kinds, vec![ViolationKind::Extended]);
        // extended flashes are dropped when the profile does not flag them
        let wcag = Profile::Wcag.config();
        assert_eq!(violations_to_sections(&vs, &wcag, (0.0, 100.0), &[]).len(), 1);
        // a 100 s violation splits into three
        let long = vec![v(0.0, 100.0, 0.0, ViolationKind::Flash)];
        assert_eq!(violations_to_sections(&long, &cfg, (0.0, 200.0), &[]).len(), 3);
    }
}
