"""Detector and pipeline configuration.

WCAG 2.x / PEAT reference thresholds:
  - general flash: pair of opposing relative-luminance changes >= 0.10 of max,
    darker state < 0.80, covering >= 1/4 of any 341x256 window at 1024x768
  - red flash: pair of opposing transitions where either state has
    R/(R+G+B) >= 0.8 and |delta (R-G-B)*320| > 20
  - failure: more than 3 flashes (of either kind) in any 1-second period
  - extended flash: >= 5 s of flashing that meets every failure criterion
    except the rate — it runs *at* the permitted rate (flash_limit per
    second) rather than above it. Not a WCAG failure, but sustained flashing
    at the limit is an ITC/Ofcom hazard and still affects some viewers.

Profiles:
  wcag_flag_extended_config()  exact WCAG thresholds, extended flashes flagged
                               as fixable sections (the default)
  wcag_config()                exact WCAG only; extended flashes are neither
                               detected nor reported
  strict_config()              tighter thresholds for extra margin (lower
                               swing/area, 2 flashes/s). No extended flagging:
                               strict already *fails* content at 3 flashes/s,
                               which is the only rate at which the extended
                               test separates flashing from ordinary motion —
                               at its own 2/s band, scrolling credits and
                               shot-cut dialogue are indistinguishable from
                               flashing by any per-pixel or window-mean
                               measure.
"""

from dataclasses import dataclass, asdict, fields


@dataclass
class DetectorConfig:
    # --- thresholds (exact WCAG defaults) ---
    swing_threshold: float = 0.10        # relative luminance swing
    dark_threshold: float = 0.80         # darker state must be below this
    area_fraction: float = 0.25          # of the 341x256 window
    flash_limit: float = 3.0             # fail when flashes/s > limit
    red_delta_threshold: float = 20.0    # on (R-G-B)*320 scale
    red_saturation: float = 0.80         # R/(R+G+B)
    # --- extended flash ---
    # Same detector as a failure (swing, dark state, area, concurrency and
    # window-mean coherence) at flash_limit flashes/s instead of above it,
    # sustained: qualifying strobes must recur within extended_hold seconds
    # of each other for extended_coverage of a extended_window-second period.
    extended_mode: str = "section"       # "section": flag extended flashes as
                                         # violations that get their own work
                                         # sections; "off": ignore them
    extended_area_ratio: float = 1.0     # of the failure area (1.0 = same)
    extended_hold: float = 1.0           # max gap between qualifying strobes
    extended_window: float = 5.0
    extended_coverage: float = 0.80
    # --- analysis model ---
    screen_w: int = 1024
    screen_h: int = 768
    window_w: int = 341
    window_h: int = 256
    analysis_scale: float = 0.25         # model resolution multiplier
    noise_eps: float = 0.02              # deadband for luminance extrema
    red_noise_eps: float = 4.0           # deadband on the 0..320 red scale
    area_accum_window: float = 0.125     # seconds to pool transition area
                                         # (a flash ramping over several frames
                                         # completes per-pixel at slightly
                                         # different times)
    max_frame_gap: float = 5.0           # frame deltas beyond this are treated
                                         # as source timestamp discontinuities
    # --- sectioning ---
    section_pad: float = 1.5             # seconds of context around a violation
    section_merge_gap: float = 3.0       # merge sections closer than this
    section_min_len: float = 2.0
    section_max_len: float = 45.0        # split longer regions

    @property
    def flag_extended(self) -> bool:
        """Extended flashes count as violations and get their own sections."""
        return self.extended_mode == "section"

    def to_dict(self):
        return asdict(self)

    @classmethod
    def from_dict(cls, d):
        known = {f.name for f in fields(cls)}
        return cls(**{k: v for k, v in (d or {}).items() if k in known})


def wcag_flag_extended_config() -> DetectorConfig:
    return DetectorConfig()


def wcag_config() -> DetectorConfig:
    return DetectorConfig(extended_mode="off")


def strict_config() -> DetectorConfig:
    return DetectorConfig(
        swing_threshold=0.08,
        area_fraction=0.20,
        flash_limit=2.0,
        extended_mode="off",
    )


PROFILES = {
    "wcag_ext": wcag_flag_extended_config,
    "wcag": wcag_config,
    "strict": strict_config,
}

DEFAULT_PROFILE = "wcag_ext"


def profile_config(name) -> DetectorConfig:
    factory = PROFILES.get(name)
    return factory() if factory else None


def profile_name(detector_dict) -> str:
    for name, factory in PROFILES.items():
        if detector_dict == factory().to_dict():
            return name
    return "custom"


@dataclass
class RenderConfig:
    crf: int = 18
    preset: str = "veryfast"
    audio_bitrate: str = "160k"
    audio_rate: int = 48000
    proxy_height: int = 540
    proxy_crf: int = 26
    thumb_width: int = 160
    extension_seconds: float = 1.0

    def to_dict(self):
        return asdict(self)

    @classmethod
    def from_dict(cls, d):
        known = {f.name for f in fields(cls)}
        return cls(**{k: v for k, v in (d or {}).items() if k in known})
