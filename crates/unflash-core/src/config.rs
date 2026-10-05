//! Detector configuration and the three profiles (port of `config.py`).
//!
//! WCAG 2.x / PEAT reference thresholds:
//!  - general flash: pair of opposing relative-luminance changes >= 0.10 of
//!    max, darker state < 0.80, covering >= 1/4 of any 341x256 window at
//!    1024x768
//!  - red flash (WCAG 2.2, and 2.1 as now published): pair of opposing
//!    transitions, each to or from a state with R/(R+G+B) >= 0.8, whose
//!    states are more than 0.2 apart in the CIE 1976 UCS diagram (u′v′)
//!  - failure: more than 3 flashes (of either kind) in any 1-second period
//!  - extended flash: >= 5 s of flashing that meets every failure criterion
//!    except the rate — it runs *at* the permitted rate rather than above it.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtendedMode {
    /// Flag extended flashes as violations that get their own work sections.
    Section,
    /// Ignore extended flashes entirely.
    Off,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectorConfig {
    // --- thresholds (exact WCAG defaults) ---
    /// Relative luminance swing that qualifies a transition.
    pub swing_threshold: f32,
    /// The darker state of a qualifying transition must be below this.
    pub dark_threshold: f32,
    /// Fraction of the window that flashing pixels must cover.
    pub area_fraction: f64,
    /// Fail when flashes per second exceed this.
    pub flash_limit: f64,
    /// Distance in the CIE 1976 UCS diagram (u′v′) between the two states
    /// of a red transition.
    pub red_delta_threshold: f32,
    /// R/(R+G+B) at or above this is saturated red.
    pub red_saturation: f32,
    /// Share of white added to every channel before a colour's
    /// chromaticity is taken (a screen's black is never black; black itself
    /// has no chromaticity). 0.0035 makes pure red count against black from
    /// where WCAG 2.0's formula counts it (linear 0.0625, code 71).
    pub red_flare: f32,
    // --- extended flash ---
    pub extended_mode: ExtendedMode,
    /// Of the failure area (1.0 = same).
    pub extended_area_ratio: f64,
    /// Max gap (s) between qualifying strobes.
    pub extended_hold: f64,
    pub extended_window: f64,
    pub extended_coverage: f64,
    // --- regular patterns (stripes, gratings) ---
    /// Flag hazardous static patterns as violations with their own sections,
    /// or ignore them.
    pub pattern_mode: ExtendedMode,
    /// More than this many light–dark stripe pairs make a pattern.
    pub pattern_pairs: u32,
    /// Relative-luminance difference between stripes that counts.
    pub pattern_swing: f32,
    /// Of the whole screen.
    pub pattern_area_fraction: f64,
    /// A pattern has to stay on screen this long, seconds.
    pub pattern_min_seconds: f64,
    /// Patterned frames closer than this are one pattern, seconds.
    pub pattern_hold: f64,
    /// Longest / shortest stripe spacing allowed inside one pattern.
    pub pattern_regularity: f64,
    // --- analysis model ---
    pub screen_w: u32,
    pub screen_h: u32,
    pub window_w: u32,
    pub window_h: u32,
    /// Model resolution multiplier (1024x768 * this is the analysis frame).
    pub analysis_scale: f64,
    /// Deadband for luminance extrema.
    pub noise_eps: f32,
    /// Deadband on the distance from the red primary in u′v′ (the red
    /// tracker's value). A qualifying red transition moves it by 0.085 at
    /// the least.
    pub red_noise_eps: f32,
    /// Seconds to pool transition area (a flash ramping over several frames
    /// completes per-pixel at slightly different times).
    pub area_accum_window: f64,
    /// Frame deltas beyond this are treated as source timestamp
    /// discontinuities.
    pub max_frame_gap: f64,
    // --- sectioning ---
    pub section_pad: f64,
    pub section_merge_gap: f64,
    pub section_min_len: f64,
    pub section_max_len: f64,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        DetectorConfig {
            swing_threshold: 0.10,
            dark_threshold: 0.80,
            area_fraction: 0.25,
            flash_limit: 3.0,
            red_delta_threshold: 0.2,
            red_saturation: 0.80,
            red_flare: 0.0035,
            extended_mode: ExtendedMode::Section,
            extended_area_ratio: 1.0,
            extended_hold: 1.0,
            extended_window: 5.0,
            extended_coverage: 0.80,
            pattern_mode: ExtendedMode::Section,
            pattern_pairs: 5,
            pattern_swing: 0.10,
            pattern_area_fraction: 0.25,
            pattern_min_seconds: 0.5,
            pattern_hold: 0.5,
            pattern_regularity: 2.5,
            screen_w: 1024,
            screen_h: 768,
            window_w: 341,
            window_h: 256,
            analysis_scale: 0.25,
            noise_eps: 0.02,
            red_noise_eps: 0.04,
            area_accum_window: 0.125,
            max_frame_gap: 5.0,
            section_pad: 1.5,
            section_merge_gap: 3.0,
            section_min_len: 2.0,
            section_max_len: 45.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// Exact WCAG thresholds, extended flashes flagged as fixable sections
    /// (the default).
    WcagExt,
    /// Exact WCAG only; extended flashes are neither detected nor reported.
    Wcag,
    /// Tighter thresholds for extra margin (0.08 swing, 1/5 area,
    /// 2 flashes/s). No extended flagging: strict already fails at 3/s.
    Strict,
}

impl Profile {
    pub const ALL: [Profile; 3] = [Profile::WcagExt, Profile::Wcag, Profile::Strict];

    pub fn name(self) -> &'static str {
        match self {
            Profile::WcagExt => "wcag_ext",
            Profile::Wcag => "wcag",
            Profile::Strict => "strict",
        }
    }

    pub fn from_name(name: &str) -> Option<Profile> {
        match name {
            "wcag_ext" => Some(Profile::WcagExt),
            "wcag" => Some(Profile::Wcag),
            "strict" => Some(Profile::Strict),
            _ => None,
        }
    }

    pub fn config(self) -> DetectorConfig {
        match self {
            Profile::WcagExt => DetectorConfig::default(),
            Profile::Wcag => DetectorConfig {
                extended_mode: ExtendedMode::Off,
                pattern_mode: ExtendedMode::Off,
                ..DetectorConfig::default()
            },
            Profile::Strict => DetectorConfig {
                swing_threshold: 0.08,
                area_fraction: 0.20,
                flash_limit: 2.0,
                extended_mode: ExtendedMode::Off,
                ..DetectorConfig::default()
            },
        }
    }
}

impl DetectorConfig {
    /// Extended flashes count as violations and get their own sections.
    pub fn flag_extended(&self) -> bool {
        self.extended_mode == ExtendedMode::Section
    }

    /// Regular patterns count as violations and get their own sections.
    pub fn flag_patterns(&self) -> bool {
        self.pattern_mode == ExtendedMode::Section
    }

    /// Which profile this configuration is, if any.
    pub fn profile_name(&self) -> &'static str {
        for p in Profile::ALL {
            if *self == p.config() {
                return p.name();
            }
        }
        "custom"
    }

    /// Frame size in the analysis model: content fit inside the WCAG screen
    /// (1024x768 by default), then scaled by `analysis_scale`, kept even.
    pub fn analysis_dims(&self, width: u32, height: u32) -> (u32, u32) {
        let f = (self.screen_w as f64 / width.max(1) as f64)
            .min(self.screen_h as f64 / height.max(1) as f64);
        let aw = ((width as f64 * f * self.analysis_scale).round_ties_even() as i64).max(2) as u32;
        let ah = ((height as f64 * f * self.analysis_scale).round_ties_even() as i64).max(2) as u32;
        (aw - aw % 2, ah - ah % 2)
    }

    /// Short stable id for the detection settings a verdict was produced
    /// under: every field except the `section_*` ones, which only steer how a
    /// scan is carved into work sections and cannot change a verdict.
    pub fn signature(&self) -> String {
        let mut v = serde_json::to_value(self).expect("config is serialisable");
        if let serde_json::Value::Object(map) = &mut v {
            map.retain(|k, _| !k.starts_with("section_"));
        }
        // canonical: serde_json::Value objects serialise with sorted keys
        // only when the `preserve_order` feature is off (it is).
        let blob = serde_json::to_string(&v).expect("json");
        // FNV-1a 64 is plenty for "did the settings change" and needs no
        // extra dependency.
        let mut h: u64 = 0xcbf29ce484222325;
        for b in blob.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}")[..12].to_string()
    }

    /// How many flashes inside a second fail (`K` in the reference).
    pub fn k_fail(&self) -> u32 {
        (self.flash_limit.floor() as i64 + 1).max(1) as u32
    }

    /// Flashes per second that count as an extended flash (at the limit).
    pub fn ext_rate(&self) -> u32 {
        (self.flash_limit.ceil() as i64).max(1) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dims_match_reference() {
        let cfg = DetectorConfig::default();
        // 1920x1080: f = min(1024/1920, 768/1080) = 0.5333; 1920*0.5333*0.25 = 256, 1080*... = 144
        assert_eq!(cfg.analysis_dims(1920, 1080), (256, 144));
        // 4:3 source fills the model
        assert_eq!(cfg.analysis_dims(1024, 768), (256, 192));
        // portrait
        assert_eq!(cfg.analysis_dims(1080, 1920), (108, 192));
        let full = DetectorConfig { analysis_scale: 1.0, ..cfg };
        assert_eq!(full.analysis_dims(1920, 1080), (1024, 576));
    }

    #[test]
    fn profiles_round_trip() {
        for p in Profile::ALL {
            assert_eq!(Profile::from_name(p.name()), Some(p));
            assert_eq!(p.config().profile_name(), p.name());
        }
        let custom = DetectorConfig { swing_threshold: 0.09, ..Default::default() };
        assert_eq!(custom.profile_name(), "custom");
    }

    #[test]
    fn signature_ignores_sectioning() {
        let a = DetectorConfig::default();
        let b = DetectorConfig { section_pad: 9.0, ..Default::default() };
        let c = DetectorConfig { swing_threshold: 0.09, ..Default::default() };
        assert_eq!(a.signature(), b.signature());
        assert_ne!(a.signature(), c.signature());
    }

    #[test]
    fn rates() {
        assert_eq!(DetectorConfig::default().k_fail(), 4);
        assert_eq!(DetectorConfig::default().ext_rate(), 3);
        assert_eq!(Profile::Strict.config().k_fail(), 3);
        assert_eq!(Profile::Strict.config().ext_rate(), 2);
    }
}
