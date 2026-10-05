//! Frame-time helpers shared by sectioning, editing and export (port of the
//! timeline parts of `ffio.py` / `editing.py`).

use serde::Serialize;

use crate::temporal::{median, Clock, DEFAULT_STEP};

/// Make a frame-time list strictly sane: non-positive deltas and deltas
/// beyond `max_gap` (source timestamp discontinuities) are replaced with the
/// running median delta, as the detector's clock bridges them. Returns
/// (new_times, n_fixed).
///
/// Deltas are measured input-to-input, so every frame after a bridged
/// anomaly does not itself look like another jump.
pub fn sanitize_deltas(times: &[f64], max_gap: f64) -> (Vec<f64>, usize) {
    let mut clock = Clock::new(max_gap);
    let mut at = times.first().copied().unwrap_or(0.0);
    let out = times
        .iter()
        .map(|&t| {
            at += clock.step(t);
            at
        })
        .collect();
    (out, clock.anomalies)
}

/// Median positive frame interval ([`DEFAULT_STEP`] for fewer than two
/// frames).
pub fn median_dt(pts: &[f64]) -> f64 {
    median(pts.windows(2).map(|w| w[1] - w[0]).filter(|d| *d > 0.0)).unwrap_or(DEFAULT_STEP)
}

/// How a section maps onto the source.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SectionTimeline {
    /// Frame times rebased onto the section's first frame.
    pub rel: Vec<f64>,
    /// How many of them the section actually shows.
    pub n_out: usize,
    /// Its exact length.
    pub total: f64,
    /// How far the first frame falls after the section's nominal start.
    pub base: f64,
    /// Median frame interval.
    pub med: f64,
}

/// `pts` are the section's decoded frame times relative to its nominal
/// start (`start`..`end` in source time).
pub fn section_timeline(pts: &[f64], start: f64, end: f64) -> SectionTimeline {
    let dur = end - start;
    if pts.is_empty() {
        return SectionTimeline { rel: vec![], n_out: 0, total: dur, base: 0.0, med: DEFAULT_STEP };
    }
    let med = median_dt(pts);
    // Frames at or past the section's end belong to the untouched span that
    // follows it.
    let mut n_out = pts.iter().filter(|&&t| t < dur - 1e-9).count();
    if n_out == 0 {
        n_out = pts.len();
    }
    let base = pts[0];
    let out: Vec<f64> = pts.iter().map(|t| t - base).collect();
    let total = if n_out < out.len() { out[n_out] } else { out[out.len() - 1] + med };
    SectionTimeline { rel: out, n_out, total, base, med }
}

/// The section's frame times exactly as an export emits them: rebased onto
/// its first frame, and stopping where the render stops.
pub fn shown_pts(pts: &[f64], start: f64, end: f64) -> Vec<f64> {
    let tl = section_timeline(pts, start, end);
    tl.rel[..tl.n_out].to_vec()
}

/// Parse "1:23.5", "83.5", "0:01:23.5" into seconds.
pub fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total = 0.0;
    for part in s.split(':') {
        let v: f64 = part.trim().parse().ok()?;
        total = total * 60.0 + v;
    }
    // (Rust reads "nan" and "inf" as numbers; no section starts there)
    Some(total).filter(|t| t.is_finite())
}

/// Format seconds as m:ss.mmm (or h:mm:ss.mmm).
pub fn format_time(t: f64) -> String {
    let t = t.max(0.0);
    let h = (t / 3600.0).floor();
    let m = ((t - h * 3600.0) / 60.0).floor();
    let s = t - h * 3600.0 - m * 60.0;
    if h > 0.0 {
        format!("{}:{:02}:{:06.3}", h as u64, m as u64, s)
    } else {
        format!("{}:{:06.3}", m as u64, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_bridges_jumps() {
        let (out, fixed) = sanitize_deltas(&[0.0, 0.04, 0.08, 0.02, 9.0, 9.04], 5.0);
        assert_eq!(fixed, 2);
        let want = [0.0, 0.04, 0.08, 0.12, 0.16, 0.20];
        for (a, b) in out.iter().zip(want) {
            assert!((a - b).abs() < 1e-9, "{a} vs {b}");
        }
        // starting where the times start; a jump before any good step bridges by a 30th of a second
        let (out, fixed) = sanitize_deltas(&[3.5, 1.0, 1.04], 5.0);
        assert_eq!((out, fixed), (vec![3.5, 3.5 + DEFAULT_STEP, 3.5 + DEFAULT_STEP + (1.04 - 1.0)], 1));
        assert_eq!(sanitize_deltas(&[], 5.0), (vec![], 0));
        // (a repeated time is not an interval)
        assert_eq!(median_dt(&[0.0, 0.25, 0.25, 0.5, 1.0]), 0.25);
        assert_eq!(median_dt(&[2.0]), DEFAULT_STEP);
    }

    #[test]
    fn timeline_drops_trailing_frame() {
        let tl = section_timeline(&[0.01, 0.05, 0.09, 0.13], 0.0, 0.12);
        assert_eq!(tl.n_out, 3);
        assert!((tl.total - 0.12).abs() < 1e-9);
        assert!((tl.base - 0.01).abs() < 1e-9);
        assert_eq!(shown_pts(&[0.01, 0.05, 0.09, 0.13], 0.0, 0.12).len(), 3);
    }

    #[test]
    fn time_parsing() {
        assert_eq!(parse_time("83.5"), Some(83.5));
        assert_eq!(parse_time("1:23.5"), Some(83.5));
        assert_eq!(parse_time("0:01:23.5"), Some(83.5));
        assert_eq!(parse_time("x"), None);
        // numbers Rust reads but no time is
        for s in ["nan", "NaN", "inf", "-inf", "infinity", "1:inf", "0:nan", "1e400"] {
            assert_eq!(parse_time(s), None, "{s}");
        }
        assert_eq!(format_time(83.5), "1:23.500");
        assert_eq!(format_time(3683.5), "1:01:23.500");
    }
}
