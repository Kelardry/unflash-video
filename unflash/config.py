"""Detector and pipeline configuration.

WCAG 2.x / PEAT reference thresholds:
  - general flash: pair of opposing relative-luminance changes >= 0.10 of max,
    darker state < 0.80, covering >= 1/4 of any 341x256 window at 1024x768
  - red flash: pair of opposing transitions where either state has
    R/(R+G+B) >= 0.8 and |delta (R-G-B)*320| > 20
  - failure: more than 3 flashes (of either kind) in any 1-second period
  - extended flash warning: 5 s where >= 80% of frames flash at >= 1/3 of the
    area threshold

The default profile is exact WCAG; strict_config() tightens the thresholds
for extra margin (lower swing/area thresholds, 2 flashes/s limit).
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
    # --- extended flash warning ---
    extended_area_ratio: float = 1.0 / 3.0
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

    def to_dict(self):
        return asdict(self)

    @classmethod
    def from_dict(cls, d):
        known = {f.name for f in fields(cls)}
        return cls(**{k: v for k, v in (d or {}).items() if k in known})


def wcag_config() -> DetectorConfig:
    return DetectorConfig()


def strict_config() -> DetectorConfig:
    return DetectorConfig(
        swing_threshold=0.08,
        area_fraction=0.20,
        flash_limit=2.0,
    )


def profile_name(detector_dict) -> str:
    if detector_dict == wcag_config().to_dict():
        return "wcag"
    if detector_dict == strict_config().to_dict():
        return "strict"
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
