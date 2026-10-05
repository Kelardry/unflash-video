"""WCAG 2.x / PEAT-style flash analysis.

Method:
  1. Frames are downscaled to a model of the content viewed at 1024x768
     (analysis_scale of that, default 1/4 => window 341x256 becomes ~85x64).
  2. Per pixel, sRGB is linearized; relative luminance L and S, the
     colour's distance from sRGB's red primary in the CIE 1976 UCS diagram
     (u'v', after a small flare: black has no colour), are tracked through a
     per-pixel extremum tracker, so a flash ramping over several frames still
     counts as one transition (accumulated monotonic change, with a small
     noise deadband). The red run keeps the colour's chromaticity at its two
     ends.
  3. When a pixel's direction reverses, the completed swing qualifies as a
     luminance transition if |swing| >= swing_threshold and the darker
     extremum < dark_threshold; as a red transition (WCAG 2.2's working
     definition) if either end was saturated red (R/(R+G+B) >= 0.8) and the
     two ends are more than 0.2 apart in u'v'.
  4. Because pixels complete a multi-frame transition at slightly different
     times, completions are pooled over area_accum_window seconds. A frame
     produces a *transition event* (up or down, general or red) when pooled
     qualifying pixels cover >= area_fraction of any window_w x window_h
     region (sliding window via integral image).
  5. A *flash* is a pair of opposing transitions whose regions overlap
     (opposing changes in unrelated screen regions are motion, not flashing).
     Any 1-second period with more than `flash_limit` flashes (general or
     red, counted separately) is a violation.
  6. An *extended flash* is the same thing one step below the failure rate:
     flashes passing every criterion above (swing, dark state, concurrent
     area, coherent window mean) at exactly `flash_limit` per second, whose
     qualifying moments keep recurring (no gap longer than extended_hold)
     for >= 80% of a 5-second period. WCAG permits it; sustained flashing at
     the limit is an ITC/Ofcom hazard, so profiles with
     extended_mode="section" report it as a violation (its own work section,
     counted in the safe/unsafe verdict) and extended_mode="off" ignores it.

Timing: event positions are reported in the source's native pts (that is what
seeking/cutting uses), but flash-frequency windows run on an internal
monotonic clock that bridges source timestamp discontinuities.
"""

import math
from dataclasses import dataclass, field, asdict

import numpy as np

from .config import DetectorConfig
from . import ffio

# How long a pixel may go on accumulating one monotonic run before the run
# is re-anchored to where it has got to.
#
# The tracker exists so that a flash ramping over a few frames still counts as
# one transition. Left uncapped it will also add up a drift that takes half a
# minute -- a scene slowly brightening, a fade, a camera adjusting exposure --
# and report the whole accumulated swing as a "transition" the moment the
# pixel finally turns. Nothing that slow is a flash: a flash is a pair of
# opposing changes inside a second, so a swing that took longer than that
# cannot be half of one.
#
# It also makes the detector's memory finite, and that is what section
# editing rests on. A section is checked by starting the detector cold a
# little before it (see context_seconds); if a pixel's run can reach back
# arbitrarily far, the check measures its swing from a different value than
# a pass over the whole video does, and the two disagree -- a section that
# checks safe, fails on the exported file, and passes again when you put a
# fresh section over the spot and check that. Measured on real footage, a
# fifth of the picture was mid-run from more than six seconds earlier, some
# of it from twenty-six seconds earlier.
MAX_RUN_SECONDS = 2.0

# How much of the picture has to move before a frame counts as showing
# something new, as a fraction of the area a flash has to cover.
#
# A picture that is merely being held on screen again is not a fresh
# observation, and rendering produces a great many of them: a section is
# written onto a constant-rate grid (render._pick_grid_fps), so each source
# picture is repeated three to five times. Comparing frames for exact
# equality only catches the repeats an encoder happened to code as skips --
# measured on real 1080p renders, one in seven -- and the rest are counted
# as observations in their own right. That is what let a section pass its
# own check and then fail the render of the very same frames: the render
# samples the hazard five times as often, so it catches peaks the check
# steps over and stretches every burst by up to a frame.
#
# So "the same picture" is decided by how much of the picture has moved far
# enough to be part of a flash: pixels whose luminance differs by more than
# HELD_DELTA_RATIO of swing_threshold from the last frame that was *not*
# held, or whose distance from red (S) differs by more than HELD_DELTA_RATIO
# of RED_LEAST_SWING -- a saturated red swapped for a
# grey of the same luminance moves no luminance at all and is still a new
# picture (and a red flash). Comparing against the last distinct frame
# rather than the previous one is what keeps a slow ramp accumulating
# instead of being held for ever
# -- but it also means an encoder's drift accumulates, which is why the bar
# is on magnitude rather than on the tracker's own noise deadband. x264
# codes the first copy of a new picture roughly and refines it over the
# copies that follow, so the third copy can differ from the first across
# hundreds of pixels while none of them has moved anywhere near a flash.
#
# Measured over 2417 known repeats in three rendered 1080p sections: drift
# from the first copy of a picture moved at most two pixels past the delta,
# while a genuine picture change moved a median of 4672 and a qualifying
# flash has to move area_thresh (1360 here). Both margins are wide.
HELD_DELTA_RATIO = 0.5      # of swing_threshold: well under a qualifying
                            # swing, well over anything compression does
HELD_AREA_RATIO = 0.10      # of the area a flash has to cover

# --- WCAG 2.2's red flash --------------------------------------------------
# sRGB's red primary in u'v' (from the sRGB-to-XYZ matrix); a state's
# chromaticity as a run keeps it, u' in 15 bits and v' in 16 (steps of
# 1/32768 and 1/65536) with the saturated-red flag in the top bit. All of it
# float32 in the same order as the Rust port (core::lut::red_values).
RED_U = np.float32(0.4507966)
RED_V = np.float32(0.5228869)
QU = np.float32(32768.0)
QV = np.float32(65536.0)
SAT_BIT = np.uint32(1 << 31)
# The least a qualifying red transition moves S: one end saturated (within
# 0.143 of the red primary), the ends more than 0.2 apart, and the sRGB gamut
# a 60-degree wedge at the red primary leave it at 0.085 or more. The held
# test and the window-mean gate are set against it.
RED_LEAST_SWING = 0.08


def red_values(R, G, B, red_saturation, flare):
    """S (distance from red in u'v') and C (packed chromaticity and
    saturation) of linear colour planes."""
    f = np.float32(flare)
    Rf = R + f
    Gf = G + f
    Bf = B + f
    X = np.float32(0.4124) * Rf + np.float32(0.3576) * Gf + np.float32(0.1805) * Bf
    Y = np.float32(0.2126) * Rf + np.float32(0.7152) * Gf + np.float32(0.0722) * Bf
    Z = np.float32(0.0193) * Rf + np.float32(0.1192) * Gf + np.float32(0.9505) * Bf
    d = X + np.float32(15.0) * Y + np.float32(3.0) * Z
    u = np.float32(4.0) * X / d
    v = np.float32(9.0) * Y / d
    du = u - RED_U
    dv = v - RED_V
    S = np.sqrt(du * du + dv * dv)
    sat = Rf >= np.float32(red_saturation) * (Rf + Gf + Bf)
    qu = np.minimum(np.floor(u * QU + np.float32(0.5)).astype(np.uint32), np.uint32(0x7fff))
    qv = np.minimum(np.floor(v * QV + np.float32(0.5)).astype(np.uint32), np.uint32(0xffff))
    C = (sat.astype(np.uint32) << np.uint32(31)) | (qu << np.uint32(16)) | qv
    return S.astype(np.float32), C


def red_transition(a, b, delta):
    """Where two packed states make a red transition: either saturated red,
    and more than `delta` apart in u'v'."""
    sat = ((a | b) & SAT_BIT) != 0
    du = (((a >> np.uint32(16)) & np.uint32(0x7fff)).astype(np.float32)
          - ((b >> np.uint32(16)) & np.uint32(0x7fff)).astype(np.float32)) / QU
    dv = ((a & np.uint32(0xffff)).astype(np.float32)
          - (b & np.uint32(0xffff)).astype(np.float32)) / QV
    dl = np.float32(delta)
    return sat & ((du * du + dv * dv) > dl * dl)


# --- sRGB -> linear lookup table -------------------------------------------
_LUT = np.empty(256, np.float32)
for _c in range(256):
    _v = _c / 255.0
    _LUT[_c] = _v / 12.92 if _v <= 0.04045 else ((_v + 0.055) / 1.055) ** 2.4


@dataclass
class TransitionEvent:
    t: float               # native pts (for cutting/sectioning)
    tc: float              # internal monotonic clock (for frequency windows)
    polarity: int          # +1 up, -1 down
    kind: str              # "general" | "red"
    area: int              # qualifying pixels in the best window
    bbox: tuple            # (x0, y0, x1, y1) of the best window, analysis px


@dataclass
class Violation:
    start: float
    end: float
    kind: str              # "flash" | "red" | "extended"
    count: float = 0.0     # flashes in the window (or coverage for extended)
    onset: float = None    # first frame whose flashing feeds this failure
    peak: float = None     # worst moment inside it (highest count/coverage)

    def __post_init__(self):
        # a violation is *reported* at `start`, the frame where the failure
        # rate is first exceeded -- but the flashes that add up to it began
        # up to a failure window earlier. `onset` is where the hazard really
        # begins, and it is what sectioning has to pad from: a section that
        # starts after `onset` cannot contain the frames responsible for its
        # own violation, so editing it can never remove one.
        if self.onset is None:
            self.onset = self.start
        if self.peak is None:
            self.peak = self.start

    @property
    def wcag(self):
        """True for kinds that are WCAG failures (extended flashes are not)."""
        return self.kind in ("flash", "red")


@dataclass
class AnalysisResult:
    events: list = field(default_factory=list)         # TransitionEvent
    violations: list = field(default_factory=list)     # Violation
    frames: int = 0
    duration: float = 0.0
    anomalies: int = 0          # timestamp anomalies bridged during analysis
    frame_stats: dict = field(default_factory=dict)    # arrays for UI charts
    flag_extended: bool = False   # profile treats extended flashes as
                                  # violations to fix, not just advisories

    @property
    def wcag_safe(self):
        """No WCAG general-flash or red-flash failure."""
        return all(not v.wcag for v in self.violations)

    @property
    def safe(self):
        """Passes everything the active profile flags (WCAG failures, plus
        extended flashes when the profile flags them)."""
        if not self.wcag_safe:
            return False
        return not (self.flag_extended and
                    any(v.kind == "extended" for v in self.violations))

    def to_dict(self, include_stats=False):
        d = {
            "safe": self.safe,
            "wcag_safe": self.wcag_safe,
            "flag_extended": self.flag_extended,
            "frames": self.frames,
            "duration": self.duration,
            "anomalies": self.anomalies,
            "violations": [asdict(v) for v in self.violations],
            "events": [{"t": e.t, "polarity": e.polarity, "kind": e.kind,
                        "area": e.area} for e in self.events],
        }
        if include_stats:
            d["frame_stats"] = {k: [round(float(x), 5) for x in v]
                                for k, v in self.frame_stats.items()}
        return d


class _ExtremaTracker:
    """Vectorized per-pixel monotonic-run tracker with noise deadband.

    Each run is re-anchored once it is `max_run` seconds old, so the swing a
    pixel eventually reports is always one it made recently and the tracker's
    state stops depending on footage older than that. See MAX_RUN_SECONDS.
    """

    def __init__(self, eps, max_run=MAX_RUN_SECONDS):
        self.eps = eps
        self.max_run = max_run
        self.dir = None
        self.base = None   # value at the start of the current run
        self.ext = None    # extremum of the current run
        self.base_t = None  # when the run's base was set
        self.aux_base = None
        self.aux_ext = None

    def settle(self, t):
        """Age the runs on a frame that only repeated the one before it.

        Nothing moved, so there is nothing to track -- but time passed, and
        the cap is measured in time. Without this, a picture held on screen
        (a still passage, a removed frame, a source that repeats every fifth
        picture) would let a run that was already old sail through it
        untouched, and the bound the section check relies on would not hold.
        """
        if self.dir is None:
            return
        stale = (t - self.base_t) > self.max_run
        if stale.any():
            self.base[stale] = self.ext[stale]
            self.base_t[stale] = t
            if self.aux_base is not None:
                self.aux_base[stale] = self.aux_ext[stale]

    def feed(self, x, t, aux=None):
        """Feed a new value plane, stamped `t` on the internal clock. Returns
        (rev_up, rev_down, base, ext, aux_base, aux_ext) where the masks flag
        pixels whose upward/downward run just completed; base/ext are
        snapshots valid at those pixels."""
        if self.dir is None:
            self.dir = np.zeros(x.shape, np.int8)
            self.base = x.copy()
            self.ext = x.copy()
            self.base_t = np.full(x.shape, t, np.float64)
            if aux is not None:
                self.aux_base = aux.copy()
                self.aux_ext = aux.copy()
            z = np.zeros(x.shape, bool)
            return z, z, self.base, self.ext, self.aux_base, self.aux_ext

        d = self.dir
        rising = d == 1
        falling = d == -1
        flat = d == 0

        # extend ongoing runs (extremum only moves in the run direction)
        new_hi = rising & (x >= self.ext)
        new_lo = falling & (x <= self.ext)
        moved_ext = new_hi | new_lo
        self.ext[moved_ext] = x[moved_ext]
        if aux is not None:
            self.aux_ext[moved_ext] = aux[moved_ext]

        # reversals: opposite move beyond the deadband
        rev_up = rising & (x < self.ext - self.eps)      # upward run ended
        rev_down = falling & (x > self.ext + self.eps)   # downward run ended

        base_snap = self.base.copy()
        ext_snap = self.ext.copy()
        aux_base_snap = self.aux_base.copy() if aux is not None else None
        aux_ext_snap = self.aux_ext.copy() if aux is not None else None

        rev = rev_up | rev_down
        if rev.any():
            self.base[rev] = self.ext[rev]
            self.ext[rev] = x[rev]
            self.base_t[rev] = t
            self.dir[rev_up] = -1
            self.dir[rev_down] = 1
            if aux is not None:
                self.aux_base[rev] = self.aux_ext[rev]
                self.aux_ext[rev] = aux[rev]

        # start runs on previously-flat pixels
        go_up = flat & (x > self.base + self.eps)
        go_dn = flat & (x < self.base - self.eps)
        started = go_up | go_dn
        if started.any():
            self.dir[go_up] = 1
            self.dir[go_dn] = -1
            self.ext[started] = x[started]
            if aux is not None:
                self.aux_ext[started] = aux[started]

        # Re-anchor runs that have gone on too long, so nothing the tracker
        # still holds is older than max_run. A pixel that has been drifting
        # (or sitting inside the deadband) since long before this pass began
        # would otherwise measure its next swing from that distant value.
        # A pixel with no run has no extremum to fall back on, so its
        # reference is simply where it is now; settle() does the rest.
        idle = (self.dir == 0) & ((t - self.base_t) > self.max_run)
        if idle.any():
            self.ext[idle] = x[idle]
            if aux is not None:
                self.aux_ext[idle] = aux[idle]
        self.settle(t)

        return rev_up, rev_down, base_snap, ext_snap, aux_base_snap, aux_ext_snap


def _best_window(mask, ww, wh):
    """Max count of True pixels in any ww x wh window, plus its bbox."""
    h, w = mask.shape
    ww = min(ww, w)
    wh = min(wh, h)
    ii = np.zeros((h + 1, w + 1), np.int64)
    ii[1:, 1:] = mask.astype(np.int64).cumsum(0).cumsum(1)
    sums = (ii[wh:, ww:] - ii[:-wh, ww:] - ii[wh:, :-ww] + ii[:-wh, :-ww])
    idx = int(np.argmax(sums))
    y0, x0 = divmod(idx, sums.shape[1])
    return int(sums.flat[idx]), (x0, y0, x0 + ww, y0 + wh)


def _bbox_overlap(a, b, frac=0.2):
    """True if the intersection covers >= frac of the smaller bbox."""
    ix = max(0, min(a[2], b[2]) - max(a[0], b[0]))
    iy = max(0, min(a[3], b[3]) - max(a[1], b[1]))
    inter = ix * iy
    amin = min((a[2] - a[0]) * (a[3] - a[1]), (b[2] - b[0]) * (b[3] - b[1]))
    return amin > 0 and inter >= frac * amin


# Every per-pixel *time* the detector keeps is float64, and that is not
# incidental. These are positions on a clock that runs for the length of the
# recording, and a float32 holding 4259 seconds resolves to a quarter of a
# millisecond -- against comparisons like `(tc - last) <= area_accum_window`
# where the window is 0.125 s and a 24 fps frame is 41.7 ms, so 0.125 lands
# 2.997 frames back and a quarter-millisecond nudge decides whether a third
# frame is still "just now". The upshot was a detector whose answers drifted
# with how far into the file it was: a section checked on its own (clock near
# zero) passed, and the identical frames at the identical times inside a
# seventy-minute export failed. Every failure in that export was in its
# second half. Keep these float64.
TIME_DTYPE = np.float64


class _Pool:
    """Pools per-pixel transition completions over a short time window, so a
    flash whose pixels complete on neighbouring frames is still one event."""

    def __init__(self, shape, window):
        self.window = window
        self.time = np.full(shape, -1e12, TIME_DTYPE)
        self.pol = np.zeros(shape, np.int8)

    def add(self, mask, pol, tc):
        self.time[mask] = tc
        self.pol[mask] = pol

    def active(self, pol, tc):
        return (self.pol == pol) & ((tc - self.time) <= self.window)

    def clear(self, mask):
        self.pol[mask] = 0


class _FlashCounter:
    """Per-pixel flash pairing and rate tracking.

    A pixel *flashes* when it completes two opposing qualifying transitions
    within a second (disjoint pairing, per WCAG's "pair of opposing
    changes"). The ring stores each pixel's last K flash times so its flash
    *rate* is known; a pixel is over the limit when it has flashed more than
    `limit` times within the trailing second.

    Rate is per pixel, so a single edge sweeping the screen (each pixel
    brightens once per pass) never exceeds the limit — but a scrolling
    high-contrast grating that really does flicker every pixel several times
    a second correctly does.
    """

    def __init__(self, shape, limit):
        self.K = int(np.floor(limit)) + 1
        self.ring = np.full((self.K,) + shape, -1e12, TIME_DTYPE)
        # when each of those flashes *opened* -- a flash is timed by its
        # closing transition, but the pair starts at the opening one, and
        # that earlier moment is what has to be inside a work section for
        # the section's edits to be able to remove the flash.
        self.ring_open = np.full((self.K,) + shape, -1e12, TIME_DTYPE)
        self.last = np.full(shape, -1e12, TIME_DTYPE)   # most recent flash
        self.pend_pol = np.zeros(shape, np.int8)
        self.pend_t = np.full(shape, -1e12, TIME_DTYPE)

    def transitions(self, mask, pol, tc):
        if not mask.any():
            return
        flash = mask & (self.pend_pol == -pol) & ((tc - self.pend_t) <= 1.0)
        rest = mask & ~flash
        if flash.any():
            sub = np.roll(self.ring[:, flash], 1, axis=0)
            sub[0] = tc
            self.ring[:, flash] = sub
            subo = np.roll(self.ring_open[:, flash], 1, axis=0)
            subo[0] = self.pend_t[flash]     # read before it is cleared
            self.ring_open[:, flash] = subo
            self.last[flash] = tc
            self.pend_pol[flash] = 0
            self.pend_t[flash] = -1e12
        if rest.any():
            self.pend_pol[rest] = pol
            self.pend_t[rest] = tc

    def window_onset(self, strobing, y0, y1, x0, x1, rate=None):
        """Earliest opening transition still inside the failure window, over
        the strobing pixels of one window position (internal clock)."""
        k = self.K if rate is None else max(1, min(self.K, int(rate)))
        sel = strobing[y0:y1, x0:x1]
        if not sel.any():
            return None
        vals = self.ring_open[k - 1, y0:y1, x0:x1][sel]
        vals = vals[vals > -1e11]
        return float(vals.min()) if vals.size else None

    def strobing(self, tc, fresh_window, rate=None):
        """Pixels flashing at least `rate` times a second AND flashed just now.

        `rate` defaults to K = limit + 1, i.e. over the limit: the K-th most
        recent flash under a second old (with an epsilon so exactly-at-limit
        content passes). Pass rate=limit for the extended-flash test, which
        wants content flashing *at* the permitted rate.

        The freshness check makes the area test *concurrent*: during a pan,
        pixels exceed neither test together — the freshly-crossed band is
        thin and mostly under-rate — while a real strobe lights the whole
        region at once, every cycle.
        """
        k = self.K if rate is None else max(1, min(self.K, int(rate)))
        over_rate = (tc - self.ring[k - 1]) < (1.0 - 1e-3)
        return over_rate & ((tc - self.last) <= fresh_window)


class FlashDetector:
    """Feed frames (t, HxWx3 uint8 RGB at analysis resolution), then finish()."""

    def __init__(self, cfg: DetectorConfig, aw: int, ah: int):
        self.cfg = cfg
        self.aw, self.ah = aw, ah
        s = cfg.analysis_scale
        self.ww = max(2, min(aw, int(round(cfg.window_w * s))))
        self.wh = max(2, min(ah, int(round(cfg.window_h * s))))
        self.area_thresh = max(1, int(round(cfg.area_fraction * self.ww * self.wh)))
        self.lum = _ExtremaTracker(cfg.noise_eps)
        self.red = _ExtremaTracker(cfg.red_noise_eps)
        shape = (ah, aw)
        # transition pools: chart statistics only
        self.pool_gen = _Pool(shape, cfg.area_accum_window)
        self.pool_red = _Pool(shape, cfg.area_accum_window)
        # flash machinery: per-pixel pairing + rate rings
        self.flash_gen = _FlashCounter(shape, cfg.flash_limit)
        self.flash_red = _FlashCounter(shape, cfg.flash_limit)
        # coherence gate: scalar flash trackers on the MEAN luminance of a
        # grid of window positions. A quarter-area flash of >=0.1 amplitude
        # swings the window mean by >= 0.1*0.25; a dark limb swinging across
        # a bright background flicks pixels without moving the mean.
        sx = max(1, self.ww // 8)
        sy = max(1, self.wh // 8)
        self.gxs = np.unique(np.append(np.arange(0, aw - self.ww + 1, sx),
                                       aw - self.ww)).astype(int)
        self.gys = np.unique(np.append(np.arange(0, ah - self.wh + 1, sy),
                                       ah - self.wh)).astype(int)
        gshape = (len(self.gys), len(self.gxs))
        self.mean_swing = max(0.02, cfg.swing_threshold * cfg.area_fraction)
        # (a window whose area share of pixels made the least qualifying red
        # swing; in float32, as the port computes it)
        self.mean_swing_red = float(np.float32(RED_LEAST_SWING) * np.float32(cfg.area_fraction))
        self.mtrack_gen = _ExtremaTracker(self.mean_swing * 0.3)
        self.mtrack_red = _ExtremaTracker(float(np.float32(self.mean_swing_red) * np.float32(0.3)))
        self.mflash_gen = _FlashCounter(gshape, cfg.flash_limit)
        self.mflash_red = _FlashCounter(gshape, cfg.flash_limit)
        # extended flashes run one step below the failure rate: "more than
        # `limit` per second" fails, "at least `limit` per second" sustained
        # is an extended flash (3/s under exact WCAG, 2/s under strict).
        self.ext_rate = max(1, int(np.ceil(cfg.flash_limit)))
        self.events = []          # strobing moments (area-qualified)
        self._last_event_tc = {"general": -1e12, "red": -1e12}
        self._above = {"general": False, "red": False}
        from array import array
        self.stat_t = array("d")     # native pts
        self.stat_tc = array("d")    # monotonic clock
        self.stat_lum = array("f")
        self.stat_up = array("i")
        self.stat_dn = array("i")
        self.stat_red = array("i")
        self.stat_haz = array("i")       # synchronized flash area (general)
        self.stat_haz_red = array("i")   # synchronized flash area (red)
        self.stat_ext = array("i")       # area strobing at the permitted
                                         # rate (extended flash)
        self.stat_ext_red = array("i")
        # internal-clock time of the earliest transition still feeding the
        # failure window on this frame (0 when the frame is not strobing)
        self.stat_haz_onset = array("d")
        self.stat_haz_red_onset = array("d")
        self.n = 0
        self.anomalies = 0
        self._last_native = None
        self._clock = 0.0
        self._recent_dt = []
        self._prev_L = None     # luminance of the last frame not held
        self._prev_V = None     # its distance from red (S)
        self.held = 0           # frames that only repeated the picture
        # what makes a frame a new picture rather than a re-show of the last
        # one: this much of it moved this far (see HELD_DELTA_RATIO)
        self._held_delta = HELD_DELTA_RATIO * cfg.swing_threshold
        self._held_delta_v = float(np.float32(HELD_DELTA_RATIO) * np.float32(RED_LEAST_SWING))
        self._held_bar = max(1.0, HELD_AREA_RATIO * self.area_thresh)

    def _window_sums(self, arr):
        """Sum of `arr` over every grid window position -> (gy, gx) array."""
        h, w = arr.shape
        ii = np.zeros((h + 1, w + 1), np.float64)
        ii[1:, 1:] = arr.astype(np.float64).cumsum(0).cumsum(1)
        ys, xs = self.gys, self.gxs
        return (ii[np.ix_(ys + self.wh, xs + self.ww)]
                - ii[np.ix_(ys, xs + self.ww)]
                - ii[np.ix_(ys + self.wh, xs)]
                + ii[np.ix_(ys, xs)])

    def _advance_clock(self, t):
        if self._last_native is None:
            self._clock = 0.0
        else:
            dt = t - self._last_native
            if dt <= 0 or dt > self.cfg.max_frame_gap:
                dt = (float(np.median(self._recent_dt))
                      if self._recent_dt else 1.0 / 30)
                self.anomalies += 1
            else:
                self._recent_dt.append(dt)
                if len(self._recent_dt) > 120:
                    self._recent_dt.pop(0)
            self._clock += dt
        self._last_native = t
        return self._clock

    def feed(self, t, frame):
        cfg = self.cfg
        tc = self._advance_clock(t)
        lin = _LUT[frame]                       # HxWx3 float32, linear
        R, G, B = lin[..., 0], lin[..., 1], lin[..., 2]
        L = 0.2126 * R + 0.7152 * G + 0.0722 * B

        # A picture merely being held on screen again is not a fresh
        # observation. Nothing the tracker reacts to has moved, so no
        # transition can complete -- and the only thing tracking it could
        # still do is re-report the flashing of the frame it repeats, one
        # frame-time later than that frame.
        #
        # Held frames are not rare: rendering re-times a section onto a
        # constant-rate grid and repeats every picture three to five times,
        # a removal holds the frame before it, and animation holds drawings
        # for two and three frames at a time. See HELD_AREA_RATIO for how
        # "the same picture" is decided and why it cannot be exact equality.
        #
        # The clock still advances: the picture really was on screen for
        # that long, and the time it occupies has to count against the
        # sustained-flashing windows like any other quiet moment.
        # A picture is a repeat only if neither its luminance nor its red
        # value moved: a saturated red swapped for a grey of the same
        # luminance is a new picture (and a red flash) although no pixel
        # changed luminance.
        V, C = red_values(R, G, B, cfg.red_saturation, cfg.red_flare)
        if self._prev_L is not None and np.count_nonzero(
                (np.abs(L - self._prev_L) > self._held_delta)
                | (np.abs(V - self._prev_V) > self._held_delta_v)
                ) < self._held_bar:
            self.held += 1
            # the runs still age: time passes while a picture is held, and a
            # long still passage must not carry an old run across it
            for tr in (self.lum, self.red, self.mtrack_gen, self.mtrack_red):
                tr.settle(tc)
            self.stat_t.append(t)
            self.stat_tc.append(tc)
            self.stat_lum.append(self.stat_lum[-1])
            self.stat_up.append(self.stat_up[-1])
            self.stat_dn.append(self.stat_dn[-1])
            self.stat_red.append(self.stat_red[-1])
            self.stat_haz.append(0)
            self.stat_haz_red.append(0)
            self.stat_ext.append(0)
            self.stat_ext_red.append(0)
            self.stat_haz_onset.append(tc)
            self.stat_haz_red_onset.append(tc)
            self.n += 1
            return
        # compared against the last frame that was not held, so a drift too
        # slow to trip the bar in one step still trips it eventually
        self._prev_L = L
        self._prev_V = V

        rev_up, rev_dn, base, ext, _, _ = self.lum.feed(L, tc)
        # upward run: base is darker end; downward run: ext is darker end
        q_up = rev_up & ((ext - base) >= cfg.swing_threshold) & \
            (base < cfg.dark_threshold)
        q_dn = rev_dn & ((base - ext) >= cfg.swing_threshold) & \
            (ext < cfg.dark_threshold)

        r_up, r_dn, rbase, rext, raux_b, raux_e = self.red.feed(V, tc, aux=C)
        # WCAG 2.2: to or from saturated red, the two states more than 0.2
        # apart in u'v'. Brightness wobble within a red scene keeps its
        # chromaticity and is no red flash (red<->dark red flashing is caught
        # by the general criterion, as a luminance flash).
        red_q = red_transition(raux_b, raux_e, cfg.red_delta_threshold)
        rq_up = r_up & red_q
        rq_dn = r_dn & red_q

        # --- coherence gate: window-mean flash tracking ---------------------
        npix = self.ww * self.wh
        mL = self._window_sums(L) / npix
        mu, md, mb, me, _, _ = self.mtrack_gen.feed(mL.astype(np.float32), tc)
        self.mflash_gen.transitions(mu & ((me - mb) >= self.mean_swing),
                                    1, tc)
        self.mflash_gen.transitions(md & ((mb - me) >= self.mean_swing),
                                    -1, tc)
        mV = self._window_sums(V) / npix
        ru2, rd2, rb2, re2, _, _ = self.mtrack_red.feed(
            mV.astype(np.float32), tc)
        self.mflash_red.transitions(
            ru2 & ((re2 - rb2) >= self.mean_swing_red), 1, tc)
        self.mflash_red.transitions(
            rd2 & ((rb2 - re2) >= self.mean_swing_red), -1, tc)

        # --- flashes: per-pixel rate + concurrent area + coherent mean ------
        haz = haz_red = 0
        ext_g = ext_r = 0
        haz_onset = haz_red_onset = tc
        fresh_w = cfg.area_accum_window
        for counter, mflash, kind in (
                (self.flash_gen, self.mflash_gen, "general"),
                (self.flash_red, self.mflash_red, "red")):
            masks = ((q_up, 1), (q_dn, -1)) if kind == "general" \
                else ((rq_up, 1), (rq_dn, -1))
            for q, pol in masks:
                counter.transitions(q, pol, tc)
            best = 0
            best_onset = tc
            strobe = counter.strobing(tc, fresh_w)
            if strobe.any():
                counts = self._window_sums(strobe)
                coherent = mflash.strobing(tc, max(fresh_w, 0.2))
                cond = (counts >= self.area_thresh) & coherent
                if cond.any():
                    k = int(np.argmax(np.where(cond, counts, 0)))
                    gy, gx = divmod(k, counts.shape[1])
                    best = int(counts[gy, gx])
                    bbox = (int(self.gxs[gx]), int(self.gys[gy]),
                            int(self.gxs[gx] + self.ww),
                            int(self.gys[gy] + self.wh))
                    onset = counter.window_onset(strobe, bbox[1], bbox[3],
                                                 bbox[0], bbox[2])
                    if onset is not None:
                        # the opening transition itself ramps over a few
                        # frames, and completions are pooled, so back off by
                        # the pooling window to reach the frame the swing
                        # actually started from
                        best_onset = onset - cfg.area_accum_window
                    if (not self._above[kind]
                            or tc - self._last_event_tc[kind] >= 0.25):
                        self.events.append(
                            TransitionEvent(t, tc, 0, kind, best, bbox))
                        self._last_event_tc[kind] = tc
            self._above[kind] = best > 0
            # extended flash: the identical test one step below the failure
            # rate — pixels flashing AT the permitted rate (not above it),
            # concurrent, covering the area, with the window mean flashing
            # at that rate too. Anything the failure test rejects as motion
            # (pans, scrolling text, cuts, mouth-flaps) is rejected here for
            # the same reason, so this only fires on real sustained flashing.
            ext_best = 0
            ext_strobe = counter.strobing(tc, fresh_w, rate=self.ext_rate)
            if ext_strobe.any():
                ecounts = self._window_sums(ext_strobe)
                ecoh = mflash.strobing(tc, max(fresh_w, 0.2),
                                       rate=self.ext_rate)
                if ecoh.any():
                    ext_best = int(np.where(ecoh, ecounts, 0).max())
            if kind == "general":
                haz, ext_g, haz_onset = best, ext_best, best_onset
            else:
                haz_red, ext_r, haz_red_onset = best, ext_best, best_onset

        # pooled transition areas: chart statistics only
        self.pool_gen.add(q_up, 1, tc)
        self.pool_gen.add(q_dn, -1, tc)
        self.pool_red.add(rq_up, 1, tc)
        self.pool_red.add(rq_dn, -1, tc)
        areas = {}
        for pool, kind in ((self.pool_gen, "general"), (self.pool_red, "red")):
            for pol in (1, -1):
                act = pool.active(pol, tc)
                count = int(act.sum())
                areas[(kind, pol)] = 0 if count == 0 else \
                    _best_window(act, self.ww, self.wh)[0]

        self.stat_t.append(t)
        self.stat_tc.append(tc)
        self.stat_lum.append(float(L.mean()))
        self.stat_up.append(areas[("general", 1)])
        self.stat_dn.append(areas[("general", -1)])
        self.stat_red.append(max(areas[("red", 1)], areas[("red", -1)]))
        self.stat_haz.append(haz)
        self.stat_haz_red.append(haz_red)
        self.stat_ext.append(ext_g)
        self.stat_ext_red.append(ext_r)
        self.stat_haz_onset.append(haz_onset)
        self.stat_haz_red_onset.append(haz_red_onset)
        self.n += 1

    def finish(self) -> AnalysisResult:
        cfg = self.cfg
        res = AnalysisResult()
        res.events = self.events
        res.frames = self.n
        res.duration = (self.stat_tc[-1] - self.stat_tc[0]) if self.n else 0.0
        res.anomalies = self.anomalies
        res.frame_stats = {
            "t": self.stat_t,
            "lum": self.stat_lum,
            "up_area": self.stat_up,
            "down_area": self.stat_dn,
            "red_area": self.stat_red,
            "hazard": self.stat_haz,
            "hazard_red": self.stat_haz_red,
        }
        res.flag_extended = cfg.flag_extended
        res.violations += self._strobe_violations(
            self.stat_haz, self.stat_haz_onset, "flash")
        res.violations += self._strobe_violations(
            self.stat_haz_red, self.stat_haz_red_onset, "red")
        res.violations += self._extended_violations()
        res.violations.sort(key=lambda v: v.start)
        return res

    def _to_native(self, tcs):
        """Internal-clock times -> the native pts of the frames they fell on.

        The two clocks only differ where the source's timestamps jump, but
        everything downstream (sectioning, seeking, cutting) speaks native
        pts, so onsets measured on the monotonic clock have to come back.
        Vectorized over a whole array: a long video has tens of thousands of
        strobing frames and rebuilding the clock per frame is quadratic."""
        if self.n == 0:
            return np.zeros(len(tcs))
        return np.interp(tcs, np.asarray(self.stat_tc),
                         np.asarray(self.stat_t))

    def _strobe_violations(self, areas, onsets, kind):
        """Frames where concurrently-strobing pixels cover the area
        threshold, merged into intervals (native time)."""
        out = []
        cur = None
        cur_end_tc = None
        native_onsets = self._to_native(np.asarray(onsets))
        for i in range(self.n):
            if areas[i] < self.area_thresh:
                continue
            t, tc = self.stat_t[i], self.stat_tc[i]
            sev = round(areas[i] / self.area_thresh, 2)
            ons = min(t, float(native_onsets[i]))
            if cur is not None and tc - cur_end_tc <= 1.0:
                cur.end = max(cur.end, t)
                if sev > cur.count:
                    cur.count = sev
                    cur.peak = t
                cur.onset = min(cur.onset, ons)
            else:
                cur = Violation(t, t, kind, sev, onset=ons, peak=t)
                out.append(cur)
            cur_end_tc = tc
        return out

    def _extended_violations(self):
        """ITC/Ofcom-style extended flash: flashing that meets every failure
        criterion except the rate -- it runs at `flash_limit` per second
        instead of above it -- sustained for `extended_window` seconds.

        Each qualifying strobe holds the content "flashing" for
        `extended_hold` seconds, so the separate strobe moments of a real
        3 Hz flicker join up, while a one-off transition decays after a
        second and cannot fill a 5-second window on its own.

        Both the coverage and the search over window positions are measured
        in the time domain rather than over frames, which is what makes the
        verdict depend on the footage and not on how densely the file
        carrying it happens to sample it. Frames are not evenly spaced:
        rendering re-times a section onto a constant-rate grid (see
        render._pick_grid_fps), so a rendered section carries three or four
        times the frames per second of the untouched material either side of
        it, and plenty of sources -- screen and game captures especially --
        are variable-rate to begin with. Counting frames lets the dense side
        of a window outvote the sparse side several times over, reading
        flashing as far more sustained than it is; sampling window positions
        at frame times lets a dense file find peaks a sparse one steps over.
        Either one is enough for a section its own check calls safe to fail
        once it has been rendered, on what is frame for frame the same
        footage.

        Profiles with extended_mode="off" skip this entirely, so exact-WCAG
        runs never report a hazard the WCAG verdict does not act on."""
        cfg = self.cfg
        if self.n == 0 or not cfg.flag_extended:
            return []
        area = self.area_thresh * cfg.extended_area_ratio
        tc = np.asarray(self.stat_tc)
        hit = (np.asarray(self.stat_ext) >= area) | \
            (np.asarray(self.stat_ext_red) >= area)
        if not hit.any():
            return []
        lo, hi = float(tc[0]), float(tc[-1])
        lit = _merge_spans([(h, h + cfg.extended_hold) for h in tc[hit]],
                           lo, hi)
        if not lit:
            return []
        W = cfg.extended_window
        if hi - lo >= W:
            width, x_hi = W, hi - W
        elif hi - lo >= W * 0.9:
            # too short to hold a whole window: judge it on all there is,
            # rather than on a window sliced off the end, whose shorter span
            # would let less flashing clear the same coverage bar
            width, x_hi = hi - lo, lo
        else:
            return []
        ab = np.array([s[0] for s in lit])
        be = np.array([s[1] for s in lit])
        cover = _Coverage(ab, be)
        # coverage(x) is piecewise linear in x, cornering only where a
        # flashing span opens or where one closed a window-length earlier,
        # so testing those corners finds every window position that can
        # qualify -- no matter where the frames happen to fall
        xs = np.unique(np.concatenate(([lo, x_hi], ab, be - width)))
        xs = np.clip(xs[(xs >= lo) & (xs <= x_hi)], lo, x_hi)
        if not xs.size:
            return []
        fracs = cover.inside(xs, width) / width
        ok = fracs >= cfg.extended_coverage
        if not ok.any():
            return []

        merged = []
        for x, frac in zip(xs[ok], fracs[ok]):
            # report the flashing, not the window that measured it. The
            # window slides, so it can open a window-length before the first
            # flashing moment and close after the last; a span that wide
            # reads as a hazard where there is none, and a section only
            # overlapping that padding gets blamed for flashing that is
            # entirely somebody else's.
            i0 = int(np.searchsorted(be, x, side="right"))
            i1 = int(np.searchsorted(ab, x + width, side="left"))
            if i1 <= i0:
                continue
            s = max(float(ab[i0]), x)
            e = min(float(be[i1 - 1]), x + width)
            if e <= s:
                continue
            if merged and s <= merged[-1][1]:
                merged[-1][1] = max(merged[-1][1], e)
                merged[-1][2] = max(merged[-1][2], float(frac))
            else:
                merged.append([s, e, float(frac)])
        if not merged:
            return []

        # the longest unbroken stretch of flashing inside a report is the
        # moment worth going and watching, and it is the one thing a span
        # covering half a minute of merged bursts cannot tell you
        peaks = []
        for s, e, _ in merged:
            i0 = int(np.searchsorted(be, s, side="right"))
            i1 = int(np.searchsorted(ab, e, side="left"))
            runs = [(min(float(be[k]), e) - max(float(ab[k]), s),
                     max(float(ab[k]), s)) for k in range(i0, i1)]
            peaks.append(max(runs)[1] if runs else s)
        # The lit spans run on for the hold after each hit, which is how hits
        # a second apart join up; the flashing itself stops at the last hit.
        # Reported to the hold's end, flashing that stops where a section
        # begins reads as a second of that section's own.
        hits = tc[hit]
        last = [max(float(hits[max(int(np.searchsorted(hits, m[1], side="right")), 1) - 1]), m[0])
                for m in merged]
        starts = self._to_native(np.array([m[0] for m in merged]))
        ends = self._to_native(np.array(last))
        pk = self._to_native(np.array(peaks))
        return [Violation(float(a), float(b), "extended", m[2],
                          onset=float(a), peak=float(p))
                for m, a, b, p in zip(merged, starts, ends, pk)]


def _merge_spans(spans, lo, hi):
    """`spans` clipped to [lo, hi] and merged where they touch or overlap,
    returned sorted and disjoint."""
    out = []
    for a, b in sorted(spans):
        a, b = max(float(a), lo), min(float(b), hi)
        if b <= a:
            continue
        if out and a <= out[-1][1]:
            out[-1][1] = max(out[-1][1], b)
        else:
            out.append([a, b])
    return out


class _Coverage:
    """How many seconds of a set of disjoint spans fall inside a window.

    The question is "how much of this window was flashing", and the answer
    is a duration. A count of flashing frames stands in for it only where
    the frames are evenly spaced, which across a render, or on any
    variable-rate source, they are not.
    """

    def __init__(self, starts, ends):
        self.a = np.asarray(starts, float)
        self.b = np.asarray(ends, float)
        self.cum = np.concatenate(([0.0], np.cumsum(self.b - self.a)))

    def upto(self, y):
        """Covered seconds before each time in `y`."""
        y = np.asarray(y, float)
        k = np.searchsorted(self.b, y, side="right")
        part = np.zeros(y.shape, float)
        inside = k < self.a.size
        if inside.any():
            ki = k[inside]
            part[inside] = np.clip(y[inside] - self.a[ki], 0.0,
                                   self.b[ki] - self.a[ki])
        return self.cum[k] + part

    def inside(self, x, width):
        """Covered seconds in [x, x + width] for each x."""
        x = np.asarray(x, float)
        return self.upto(x + width) - self.upto(x)


def _windows_over_limit(flashes, limit, kind):
    """1-second sliding windows containing more than `limit` flashes.
    `flashes` is a list of (tc, t_native); windows run on tc, violations are
    reported in native time."""
    out = []
    n = len(flashes)
    j = 0
    cur = None
    for i in range(n):
        # strictly-inside-one-second window (epsilon keeps content at exactly
        # the limit, e.g. exactly 3 flashes/s under WCAG, on the passing side)
        while j < n and flashes[j][0] < flashes[i][0] + 1.0 - 1e-3:
            j += 1
        count = j - i
        if count > limit:
            s, e = flashes[i][1], flashes[j - 1][1]
            if s > e:
                s, e = e, s
            if cur and s <= cur.end + 1.0:
                cur.end = max(cur.end, e)
                cur.count = max(cur.count, count)
            else:
                cur = Violation(s, e, kind, count)
                out.append(cur)
    return out


# --- high-level entry points -------------------------------------------------

def analyze_frames(frame_iter, cfg, aw, ah, progress=None, total_hint=None,
                   cancel=None):
    det = FlashDetector(cfg, aw, ah)
    for i, (t, frame) in enumerate(frame_iter):
        if cancel is not None and cancel():
            break
        det.feed(t, frame)
        if progress and total_hint and i % 200 == 0:
            progress(min(1.0, (i + 1) / total_hint))
    return det.finish()


def analyze_file(path, cfg, start=None, duration=None, progress=None,
                 cancel=None, info=None):
    info = info or ffio.probe(path)
    aw, ah = ffio.analysis_dims(info["width"], info["height"], cfg)
    total = None
    if info.get("fps"):
        span = duration if duration is not None else info.get("duration", 0)
        total = max(1, int(span * info["fps"])) if span else None

    def gen():
        yield from ffio.iter_frames(path, aw, ah, start=start,
                                    duration=duration, cancel=cancel)

    return analyze_frames(gen(), cfg, aw, ah, progress=progress,
                          total_hint=total, cancel=cancel)


def context_seconds(cfg) -> float:
    """How much of the run-up a detector has to have seen before its verdict
    at a given moment matches the verdict a pass over the whole video gives
    at that same moment.

    Everything the detector carries forward is bounded: a flash pairs two
    transitions at most 1 s apart, the failure test looks back over 1 s of
    flashes, and a per-pixel monotonic run is re-anchored once it reaches
    MAX_RUN_SECONDS, so no tracker holds a value older than that. Feed it
    that much real footage first and its state at the hand-over is the state
    a full pass would have arrived with -- start it cold instead and it is
    simply blind for that long, which is why a section that its own check
    calls safe can still be flashing in its opening second.

    The run cap is the reason the last term is a constant rather than a
    guess. Without it a pixel could still be part-way through a swing it
    began half a minute earlier, no run-up would be long enough to guarantee
    the same state, and a section could check safe, fail on the export, and
    check safe again on a fresh section drawn over the very same frames.

    An extended flash is measured over a whole `extended_window`, so profiles
    that flag those need the run-up to cover one.
    """
    base = 1.0 + 1.0 + MAX_RUN_SECONDS + 0.5   # pairing + failure window
                                              # + run cap + margin
    if cfg.flag_extended:
        base = max(base, cfg.extended_window + cfg.extended_hold + 0.5)
    return base


# Extra span asked of every flash window on top of the second the detector
# measures against, so a rate-reduced section keeps its margin through the
# render. The render writes a section onto a constant-rate grid of at least
# 100 slots a second (render._pick_grid_fps) and a picture can only land on
# the nearest slot, so a gap measured on the export can come back up to a
# whole slot -- 10 ms -- shorter than the one the editor laid out.
RATE_SAFETY_MARGIN = 0.05


def flash_window_frames(cfg) -> int:
    """How many frame intervals a burst of flashing needs to trip `cfg`.

    Nothing here looks at pictures; this is a bound that falls out of how
    flashes are counted, and it is what makes a purely timing-based edit
    able to promise anything.

    A pixel's monotonic run reverses at most once per frame (_ExtremaTracker
    hands back `rev_up` and `rev_down` from a single `dir`, so they can never
    both be set), and a picture merely held on screen again is not a fresh
    observation at all (FlashDetector.feed returns early). So every
    qualifying transition lands on a *distinct new picture*, and a flash --
    a pair of opposing transitions, paired disjointly -- costs two of them.

    A verdict needs `k` flashes inside a second, timed by their closing
    transitions: the first closes on one picture, and the `k - 1` flashes
    after it cost two pictures each. The flashes are therefore spread over at
    least 2 * (k - 1) frame intervals, whatever those pictures contain --
    and if that many consecutive intervals cannot fit inside a second, the
    verdict cannot be reached.

    `k` is `_FlashCounter.K` = floor(flash_limit) + 1 for a WCAG failure
    (more than the limit) and `ext_rate` = ceil(flash_limit) for an extended
    flash (at the limit), where the profile reports those. The smaller
    window governs: satisfying it satisfies the longer one, which contains
    it.

    Returns 0 when no frame rate can be safe -- a limit under 1 makes a
    single flash a violation, and two pictures a second can carry one.
    """
    k_fail = int(np.floor(cfg.flash_limit)) + 1
    m = 2 * (k_fail - 1)
    if cfg.flag_extended:
        k_ext = max(1, min(k_fail, int(np.ceil(cfg.flash_limit))))
        m = min(m, 2 * (k_ext - 1))
    return max(0, m)


def safe_picture_rate(cfg, margin=RATE_SAFETY_MARGIN):
    """(pictures per second, seconds between them) that `cfg` cannot fail.

    Space new pictures at least this far apart and the tightest flash window
    of `flash_window_frames` spans more than the second the rate tests
    measure over, so no arrangement of those pictures -- however violently
    they flash -- can be counted as flashing too fast.

    The rate is rounded *down* to two decimals, which is how it is written on
    a button and typed into a box. That makes the number the user sees the
    same number the arithmetic was done on: a rate offered as 3.81 and then
    sent back as 3.81 would be a hair above the rate that was checked, and
    "is this still guaranteed?" would need a fudge factor to answer. Rounding
    down instead means anything at or under the quoted figure is safe, full
    stop, and `rate_is_guaranteed` is a plain comparison.

    Returns (0.0, inf) for a configuration no rate can satisfy.
    """
    m = flash_window_frames(cfg)
    if m < 1:
        return 0.0, float("inf")
    fps = math.floor(m / (1.0 + margin) * 100.0) / 100.0
    return fps, 1.0 / fps


def rate_is_guaranteed(cfg, fps) -> bool:
    """Is thinning to `fps` pictures a second safe by arithmetic alone?

    Above the safe rate a reduction is still often the right edit -- footage
    that is not flashing violently passes at far more frames than the
    worst-case bound allows -- but it stops being a promise and becomes a
    proposal like any other, to be settled by the check that follows it.
    """
    safe, _ = safe_picture_rate(cfg)
    return safe > 0 and fps <= safe + 1e-9


def violations_to_sections(violations, cfg, bounds, keyframes=None):
    """Merge violations into padded work sections.
    `bounds` = (ts_min, ts_max): the video's real native-pts range.

    Sections are padded out from each violation's `onset` -- the first
    transition that feeds it -- not from `start`, the frame where the rate
    was first exceeded. Those are different moments: a failure is a second's
    worth of flashes, so it is only announced once the last of them lands,
    and a section padded from the announcement can begin *after* some of the
    flashes it is supposed to let you remove. Padding from the onset is what
    makes "every section edited safe" mean "the whole video is safe"; it
    costs about a second at the head of a section and flags nothing new.

    Extended flashes create sections only when the profile flags them
    (extended_mode="section"); otherwise they are not reported at all.
    """
    ts_min, ts_max = bounds
    intervals = []
    for v in violations:
        if v.kind == "extended" and not cfg.flag_extended:
            continue
        s = max(ts_min, min(v.onset, v.start) - cfg.section_pad)
        e = min(ts_max, v.end + cfg.section_pad)
        if e - s < cfg.section_min_len:
            mid = (s + e) / 2
            s = max(ts_min, mid - cfg.section_min_len / 2)
            e = min(ts_max, s + cfg.section_min_len)
        if e > s:
            intervals.append([s, e, {v.kind}])
    intervals.sort(key=lambda x: x[0])

    merged = []
    for s, e, kinds in intervals:
        if merged and s <= merged[-1][1] + cfg.section_merge_gap:
            merged[-1][1] = max(merged[-1][1], e)
            merged[-1][2] |= kinds
        else:
            merged.append([s, e, set(kinds)])

    # split over-long sections
    split = []
    for s, e, kinds in merged:
        length = e - s
        if length <= cfg.section_max_len:
            split.append((s, e, kinds))
        else:
            parts = int(np.ceil(length / cfg.section_max_len))
            step = length / parts
            for k in range(parts):
                split.append((s + k * step, min(e, s + (k + 1) * step), kinds))

    out = []
    for s, e, kinds in split:
        ks, ke = ffio.snap_to_keyframes(keyframes or [], s, e, bounds)
        out.append({"start": round(ks, 6), "end": round(ke, 6),
                    "kinds": sorted(kinds)})
    # snapping can make neighbours touch/overlap; merge those
    dedup = []
    for sec in out:
        if dedup and sec["start"] < dedup[-1]["end"] - 1e-6:
            dedup[-1]["end"] = max(dedup[-1]["end"], sec["end"])
            dedup[-1]["kinds"] = sorted(set(dedup[-1]["kinds"]) | set(sec["kinds"]))
        else:
            dedup.append(sec)
    return dedup


def timeline_summary(result, bounds, bin_seconds=1.0):
    """Per-second flash counts for the whole-video heatmap (native time)."""
    ts_min, ts_max = bounds
    span = max(1e-6, ts_max - ts_min)
    nbins = max(1, int(np.ceil(span / bin_seconds)))
    general = np.zeros(nbins, np.float32)
    red = np.zeros(nbins, np.float32)
    for kind, arr in (("general", general), ("red", red)):
        for e in result.events:
            if e.kind != kind:
                continue
            b = min(nbins - 1, max(0, int((e.t - ts_min) / bin_seconds)))
            arr[b] += 1
    return {"bin": bin_seconds, "t0": ts_min,
            "general": general.tolist(),
            "red": red.tolist()}
