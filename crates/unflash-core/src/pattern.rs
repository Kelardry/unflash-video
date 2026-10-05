//! Regular-pattern (stripe / grating) detection, after the Ofcom guidance
//! and ITU-R BT.1702: a pattern is potentially harmful when it shows more
//! than five clearly discernible light–dark stripe pairs in any orientation,
//! the stripes differ by at least the flash luminance threshold, and the
//! pattern covers a quarter of the screen or more. Stationary patterns are
//! the case the flash detector cannot see at all; flashing or reversing
//! patterns are caught by both.
//!
//! Method: the luminance image is walked along parallel lines in eight
//! orientations (22.5° apart, so every stripe orientation is crossed within
//! 11.25° of perpendicular). Along a line the same monotonic-run tracker the
//! flash detector uses turns the profile into a sequence of runs; a run
//! qualifies when its swing is at least `swing` and its darker end is below
//! `dark`. A stretch of at least `min_transitions` consecutive qualifying
//! runs with regular spacing (longest spacing at most `reg_num/reg_den`
//! times the shortest) marks the pixels it crosses as patterned. The frame's
//! pattern area is the count of pixels marked in any orientation.
//!
//! Positions are fixed point (16.16) and the state machine is integer /
//! f32 only, so the WGSL version in `unflash-gpu` produces the identical
//! mask, count and spacing statistics.

use crate::config::DetectorConfig;

pub const ORIENTATIONS: usize = 8;

/// (cos, sin) of k·22.5° in 16.16 fixed point; identical table in the shader.
pub const DIRS: [(i32, i32); ORIENTATIONS] = [
    (65536, 0),
    (60547, 25080),
    (46341, 46341),
    (25080, 60547),
    (0, 65536),
    (-25080, 60547),
    (-46341, 46341),
    (-60547, 25080),
];

const RING: usize = 16;
const UP: u32 = 1;
const DN: u32 = 2;

/// A stripe is uniform along its length: each extremum must agree with the
/// pixel one step perpendicular to the sampling line to within this
/// fraction of the swing. Noise fails it; gratings pass it.
pub const COHERENCE_RATIO: f32 = 0.5;

/// Kernel parameters (all comparisons are f32 or integer).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PatternParams {
    pub swing: f32,
    pub dark: f32,
    pub eps: f32,
    /// Max luminance difference to the perpendicular neighbour at an
    /// extremum.
    pub coherence: f32,
    /// Consecutive qualifying runs needed: 2 * pairs + 1.
    pub min_transitions: u32,
    /// Longest spacing may be at most reg_num / reg_den times the shortest.
    pub reg_num: u32,
    pub reg_den: u32,
}

/// The most stripe pairs a stretch may be asked for: the extrema ring holds
/// `RING` positions and a stretch of `2 * pairs + 1` runs needs one more.
pub const MAX_PAIRS: u32 = (RING as u32 - 2) / 2;

impl PatternParams {
    pub fn from_config(cfg: &DetectorConfig) -> Self {
        PatternParams {
            swing: cfg.pattern_swing,
            dark: cfg.dark_threshold,
            eps: cfg.noise_eps,
            coherence: cfg.pattern_swing * COHERENCE_RATIO,
            min_transitions: 2 * cfg.pattern_pairs.clamp(1, MAX_PAIRS) + 1,
            reg_num: (cfg.pattern_regularity * 2.0).round().max(2.0) as u32,
            reg_den: 2,
        }
    }
}

/// Per-frame totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PatternOut {
    /// Pixels marked in at least one orientation.
    pub count: u32,
    /// Sum of spacings (in line steps) between qualifying extrema inside
    /// marked stretches, and how many were summed: their ratio is the mean
    /// half-period of the stripes, which sizes the export blur.
    pub spacing_sum: u32,
    pub spacing_n: u32,
}

/// Half-extent of the line family that covers a w×h image from its centre.
pub fn line_radius(aw: u32, ah: u32) -> i32 {
    (((aw as f64).powi(2) + (ah as f64).powi(2)).sqrt() / 2.0).ceil() as i32 + 1
}

/// Nearest pixel of a fixed-point position, or None outside the image.
#[inline(always)]
fn sample_index(px: i32, py: i32, aw: i32, ah: i32) -> Option<usize> {
    let xi = (px + 32768) >> 16;
    let yi = (py + 32768) >> 16;
    if xi < 0 || xi >= aw || yi < 0 || yi >= ah {
        None
    } else {
        Some((yi * aw + xi) as usize)
    }
}

/// The state machine of one line; `mark` is called for every line step in a
/// stretch that qualifies (in order, each step at most once).
struct LineState {
    init: bool,
    dir: u32,
    base: f32,
    ext: f32,
    ext_t: i32,
    n_q: u32,
    ring: [i32; RING],
    /// number of extrema pushed (the ring holds the last RING of them)
    n_ext: u32,
    marked_upto: i32,
    /// a qualifying stretch is open (a run that keeps it going adds only
    /// its own spacing to the statistics)
    in_stretch: bool,
    pub spacing_sum: u32,
    pub spacing_n: u32,
}

impl LineState {
    fn new() -> Self {
        LineState {
            init: false,
            dir: 0,
            base: 0.0,
            ext: 0.0,
            ext_t: 0,
            n_q: 0,
            ring: [0; RING],
            n_ext: 0,
            marked_upto: i32::MIN,
            in_stretch: false,
            spacing_sum: 0,
            spacing_n: 0,
        }
    }

    #[inline(always)]
    fn reset(&mut self) {
        self.init = false;
        self.in_stretch = false;
    }

    #[inline(always)]
    fn push(&mut self, t: i32) {
        self.ring[(self.n_ext as usize) % RING] = t;
        self.n_ext += 1;
    }

    #[inline(always)]
    fn ext_at(&self, back: u32) -> i32 {
        // extremum `back` places before the newest (0 = newest)
        self.ring[((self.n_ext - 1 - back) as usize) % RING]
    }

    /// Returns Some((t0, t1)) to mark when the newest run completes a
    /// qualifying regular stretch.
    #[inline(always)]
    fn finish_run(&mut self, qualifies: bool, ext_t: i32, p: &PatternParams) -> Option<(i32, i32)> {
        self.push(ext_t);
        if qualifies {
            self.n_q += 1;
        } else {
            self.n_q = 0;
            self.in_stretch = false;
            return None;
        }
        if self.n_q < p.min_transitions {
            return None;
        }
        // spacings over the last min_transitions runs
        let mut smin = u32::MAX;
        let mut smax = 0u32;
        for b in 0..p.min_transitions {
            let s = (self.ext_at(b) - self.ext_at(b + 1)) as u32;
            smin = smin.min(s);
            smax = smax.max(s);
        }
        if smax * p.reg_den > smin * p.reg_num {
            self.in_stretch = false;
            return None;
        }
        let first = self.ext_at(p.min_transitions);
        if self.in_stretch {
            // extend by the newest spacing only
            self.spacing_sum += (ext_t - self.ext_at(1)) as u32;
            self.spacing_n += 1;
        } else {
            self.spacing_sum += (ext_t - first) as u32;
            self.spacing_n += p.min_transitions;
            self.in_stretch = true;
        }
        let t0 = if self.marked_upto == i32::MIN { first } else { first.max(self.marked_upto + 1) };
        self.marked_upto = ext_t;
        Some((t0, ext_t))
    }

    /// Feed one in-image sample at line step `t`. `coherent(t)` says whether
    /// the sample at step `t` agrees with its perpendicular neighbour.
    #[inline(always)]
    fn step(&mut self, t: i32, v: f32, p: &PatternParams, coherent: &dyn Fn(i32) -> bool) -> Option<(i32, i32)> {
        if !self.init {
            self.init = true;
            self.dir = 0;
            self.base = v;
            self.ext = v;
            self.ext_t = t;
            self.n_q = 0;
            self.n_ext = 0;
            self.marked_upto = i32::MIN;
            self.in_stretch = false;
            self.push(t);
            return None;
        }
        let mut out = None;
        if self.dir == UP {
            if v >= self.ext {
                self.ext = v;
                self.ext_t = t;
            } else if v < self.ext - p.eps {
                let q = (self.ext - self.base) >= p.swing && self.base < p.dark && coherent(self.ext_t);
                out = self.finish_run(q, self.ext_t, p);
                self.base = self.ext;
                self.ext = v;
                self.ext_t = t;
                self.dir = DN;
            }
        } else if self.dir == DN {
            if v <= self.ext {
                self.ext = v;
                self.ext_t = t;
            } else if v > self.ext + p.eps {
                let q = (self.base - self.ext) >= p.swing && self.ext < p.dark && coherent(self.ext_t);
                out = self.finish_run(q, self.ext_t, p);
                self.base = self.ext;
                self.ext = v;
                self.ext_t = t;
                self.dir = UP;
            }
        } else if v > self.base + p.eps {
            self.dir = UP;
            self.ext = v;
            self.ext_t = t;
        } else if v < self.base - p.eps {
            self.dir = DN;
            self.ext = v;
            self.ext_t = t;
        }
        out
    }
}

/// Run the detector over a luminance plane. `mask` (npix words) receives bit
/// `k` for every pixel crossed by a qualifying stretch of orientation `k`;
/// it is cleared first.
pub fn detect(l: &[f32], aw: u32, ah: u32, p: &PatternParams, mask: &mut [u32]) -> PatternOut {
    let (w, h) = (aw as i32, ah as i32);
    debug_assert_eq!(l.len(), (w * h) as usize);
    mask.iter_mut().for_each(|m| *m = 0);
    let r = line_radius(aw, ah);
    let cx = w * 32768;
    let cy = h * 32768;
    let mut out = PatternOut::default();
    for (k, &(dx, dy)) in DIRS.iter().enumerate() {
        let (nx, ny) = (-dy, dx);
        let bit = 1u32 << k;
        for j in -r..=r {
            let ox = cx + j * nx;
            let oy = cy + j * ny;
            let coherent = |t: i32| -> bool {
                match (sample_index(ox + t * dx, oy + t * dy, w, h), sample_index(ox + nx + t * dx, oy + ny + t * dy, w, h)) {
                    (Some(a), Some(b)) => (l[a] - l[b]).abs() <= p.coherence,
                    _ => false,
                }
            };
            let mut st = LineState::new();
            for t in -r..=r {
                let Some(idx) = sample_index(ox + t * dx, oy + t * dy, w, h) else {
                    st.reset();
                    continue;
                };
                if let Some((t0, t1)) = st.step(t, l[idx], p, &coherent) {
                    for tt in t0..=t1 {
                        if let Some(i) = sample_index(ox + tt * dx, oy + tt * dy, w, h) {
                            mask[i] |= bit;
                        }
                    }
                }
            }
            out.spacing_sum += st.spacing_sum;
            out.spacing_n += st.spacing_n;
        }
    }
    out.count = mask.iter().filter(|&&m| m != 0).count() as u32;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;

    fn params() -> PatternParams {
        PatternParams::from_config(&Profile::WcagExt.config())
    }

    /// Stripes of the given period (pixels) at angle `deg`, covering `frac`
    /// of the width from the left, between luminances `lo` and `hi`.
    fn stripes(aw: u32, ah: u32, period: f64, deg: f64, frac: f64, lo: f32, hi: f32) -> Vec<f32> {
        let (c, s) = (deg.to_radians().cos(), deg.to_radians().sin());
        let mut l = vec![0.3f32; (aw * ah) as usize];
        for y in 0..ah {
            for x in 0..aw {
                if (x as f64) < aw as f64 * frac {
                    let u = x as f64 * c + y as f64 * s;
                    let phase = (u / period).rem_euclid(1.0);
                    l[(y * aw + x) as usize] = if phase < 0.5 { lo } else { hi };
                }
            }
        }
        l
    }

    fn frac(aw: u32, ah: u32, l: &[f32]) -> f64 {
        let mut mask = vec![0u32; (aw * ah) as usize];
        let o = detect(l, aw, ah, &params(), &mut mask);
        o.count as f64 / (aw * ah) as f64
    }

    #[test]
    fn full_screen_fine_stripes_are_found_in_every_orientation() {
        for deg in [0.0, 15.0, 30.0, 45.0, 60.0, 90.0, 120.0, 160.0] {
            let l = stripes(256, 144, 6.0, deg, 1.0, 0.05, 0.6);
            let f = frac(256, 144, &l);
            assert!(f > 0.9, "{deg}°: only {f:.2} of the picture marked");
        }
    }

    #[test]
    fn coarse_stripes_with_more_than_five_pairs_are_found() {
        // 12 pairs across 256 px
        let l = stripes(256, 144, 256.0 / 12.0, 0.0, 1.0, 0.05, 0.6);
        assert!(frac(256, 144, &l) > 0.9);
    }

    #[test]
    fn five_pairs_or_fewer_are_not_a_pattern() {
        // 5 pairs across the picture: 10 stripes, 9 transitions
        let l = stripes(256, 144, 256.0 / 5.0, 0.0, 1.0, 0.05, 0.6);
        assert_eq!(frac(256, 144, &l), 0.0);
    }

    #[test]
    fn low_contrast_and_bright_only_stripes_do_not_count() {
        let l = stripes(256, 144, 6.0, 0.0, 1.0, 0.30, 0.36);
        assert_eq!(frac(256, 144, &l), 0.0, "0.06 swing is under the threshold");
        let l = stripes(256, 144, 6.0, 0.0, 1.0, 0.85, 1.0);
        assert_eq!(frac(256, 144, &l), 0.0, "both stripes above the dark threshold");
    }

    #[test]
    fn area_is_measured_on_the_whole_picture() {
        let l = stripes(256, 144, 6.0, 90.0, 0.4, 0.05, 0.6);
        let f = frac(256, 144, &l);
        assert!(f > 0.35 && f < 0.45, "{f}");
    }

    #[test]
    fn irregular_texture_is_not_a_pattern() {
        let mut seed = 7u64;
        let mut l = vec![0f32; 256 * 144];
        for v in l.iter_mut() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *v = ((seed >> 33) % 1000) as f32 / 1000.0 * 0.7;
        }
        let f = frac(256, 144, &l);
        assert!(f < 0.05, "noise marked {f:.3} of the picture");
    }

    #[test]
    fn spacing_statistics_give_the_half_period() {
        let l = stripes(256, 144, 8.0, 0.0, 1.0, 0.05, 0.6);
        let mut mask = vec![0u32; 256 * 144];
        let o = detect(&l, 256, 144, &params(), &mut mask);
        // orientation 0 sees a half-period of 4 steps; the oblique
        // orientations see it stretched, so the mean lands a little above 4
        let mean = o.spacing_sum as f64 / o.spacing_n as f64;
        assert!(mean > 3.8 && mean < 6.0, "{mean}");
    }
}
