"""WCAG 2.x / PEAT-style flash analysis.

Method:
  1. Frames are downscaled to a model of the content viewed at 1024x768
     (analysis_scale of that, default 1/4 => window 341x256 becomes ~85x64).
  2. Per pixel, sRGB is linearized; relative luminance L and the saturated-red
     value V = max(0, R-G-B)*320 are tracked through a per-pixel extremum
     tracker, so a flash ramping over several frames still counts as one
     transition (accumulated monotonic change, with a small noise deadband).
  3. When a pixel's direction reverses, the completed swing qualifies as a
     luminance transition if |swing| >= swing_threshold and the darker
     extremum < dark_threshold; as a red transition if |swing| > 20 on the V
     scale and either extremum was saturated red (R/(R+G+B) >= 0.8).
  4. Because pixels complete a multi-frame transition at slightly different
     times, completions are pooled over area_accum_window seconds. A frame
     produces a *transition event* (up or down, general or red) when pooled
     qualifying pixels cover >= area_fraction of any window_w x window_h
     region (sliding window via integral image).
  5. A *flash* is a pair of opposing transitions whose regions overlap
     (opposing changes in unrelated screen regions are motion, not flashing).
     Any 1-second period with more than `flash_limit` flashes (general or
     red, counted separately) is a violation. A 5-second period where >= 80%
     of frames flash at >= 1/3 of the area threshold is an extended-flash
     warning.

Timing: event positions are reported in the source's native pts (that is what
seeking/cutting uses), but flash-frequency windows run on an internal
monotonic clock that bridges source timestamp discontinuities.
"""

from dataclasses import dataclass, field, asdict

import numpy as np

from .config import DetectorConfig
from . import ffio

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


@dataclass
class AnalysisResult:
    events: list = field(default_factory=list)         # TransitionEvent
    violations: list = field(default_factory=list)     # Violation
    frames: int = 0
    duration: float = 0.0
    anomalies: int = 0          # timestamp anomalies bridged during analysis
    frame_stats: dict = field(default_factory=dict)    # arrays for UI charts

    @property
    def safe(self):
        return not any(v.kind in ("flash", "red") for v in self.violations)

    def to_dict(self, include_stats=False):
        d = {
            "safe": self.safe,
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
    """Vectorized per-pixel monotonic-run tracker with noise deadband."""

    def __init__(self, eps):
        self.eps = eps
        self.dir = None
        self.base = None   # value at the start of the current run
        self.ext = None    # extremum of the current run
        self.aux_base = None
        self.aux_ext = None

    def feed(self, x, aux=None):
        """Feed a new value plane. Returns (rev_up, rev_down, base, ext,
        aux_base, aux_ext) where the masks flag pixels whose upward/downward
        run just completed; base/ext are snapshots valid at those pixels."""
        if self.dir is None:
            self.dir = np.zeros(x.shape, np.int8)
            self.base = x.copy()
            self.ext = x.copy()
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


class _Pool:
    """Pools per-pixel transition completions over a short time window, so a
    flash whose pixels complete on neighbouring frames is still one event."""

    def __init__(self, shape, window):
        self.window = window
        self.time = np.full(shape, -1e12, np.float32)
        self.pol = np.zeros(shape, np.int8)

    def add(self, mask, pol, tc):
        self.time[mask] = tc
        self.pol[mask] = pol

    def active(self, pol, tc):
        return (self.pol == pol) & ((tc - self.time) <= self.window)

    def clear(self, mask):
        self.pol[mask] = 0


class _FlashRing:
    """Per-pixel flash counter.

    A pixel *flashes* when it completes two opposing qualifying transitions
    within a second of each other (disjoint pairing, per WCAG's "pair of
    opposing changes"). The ring stores each pixel's last K flash times; a
    pixel is *hazardous* when it has flashed more than `limit` times within
    the trailing second. This is inherently robust against motion: an edge
    sweeping across the screen brightens each pixel only once per pass.
    """

    def __init__(self, shape, limit):
        self.K = int(np.floor(limit)) + 1
        self.ring = np.full((self.K,) + shape, -1e12, np.float32)
        self.pend_pol = np.zeros(shape, np.int8)
        self.pend_t = np.full(shape, -1e12, np.float32)

    def transitions(self, mask, pol, tc):
        if not mask.any():
            return
        flash = mask & (self.pend_pol == -pol) & ((tc - self.pend_t) <= 1.0)
        rest = mask & ~flash
        if flash.any():
            sub = np.roll(self.ring[:, flash], 1, axis=0)
            sub[0] = tc
            self.ring[:, flash] = sub
            self.pend_pol[flash] = 0
            self.pend_t[flash] = -1e12
        if rest.any():
            self.pend_pol[rest] = pol
            self.pend_t[rest] = tc

    def hazard(self, tc):
        """Pixels whose (limit+1)-th most recent flash is under a second old.
        The 1e-3 epsilon keeps content at exactly the limit (e.g. exactly
        3 flashes/s under WCAG) on the passing side of the boundary."""
        return (tc - self.ring[self.K - 1]) < (1.0 - 1e-3)


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
        self.pool_gen = _Pool(shape, cfg.area_accum_window)
        self.pool_red = _Pool(shape, cfg.area_accum_window)
        self.flash_gen = _FlashRing(shape, cfg.flash_limit)
        self.flash_red = _FlashRing(shape, cfg.flash_limit)
        self.events = []
        from array import array
        self.stat_t = array("d")     # native pts
        self.stat_tc = array("d")    # monotonic clock
        self.stat_lum = array("f")
        self.stat_up = array("i")
        self.stat_dn = array("i")
        self.stat_red = array("i")
        self.stat_haz = array("i")       # hazard area (general)
        self.stat_haz_red = array("i")   # hazard area (red)
        self.n = 0
        self.anomalies = 0
        self._last_native = None
        self._clock = 0.0
        self._recent_dt = []

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
        total = R + G + B
        sat = (total > 1e-5) & (R >= cfg.red_saturation * total)
        V = np.maximum(R - G - B, 0.0) * 320.0

        rev_up, rev_dn, base, ext, _, _ = self.lum.feed(L)
        # upward run: base is darker end; downward run: ext is darker end
        q_up = rev_up & ((ext - base) >= cfg.swing_threshold) & \
            (base < cfg.dark_threshold)
        q_dn = rev_dn & ((base - ext) >= cfg.swing_threshold) & \
            (ext < cfg.dark_threshold)

        r_up, r_dn, rbase, rext, raux_b, raux_e = self.red.feed(V, aux=sat)
        # the transition must go INTO or OUT OF saturated red (the states at
        # the two ends of the swing differ). Brightness wobble within a
        # continuously-red scene changes V but is not a red flash — genuine
        # red<->dark luminance flashing is caught by the general criterion.
        sat_changed = raux_b != raux_e
        rq_up = r_up & ((rext - rbase) > cfg.red_delta_threshold) & sat_changed
        rq_dn = r_dn & ((rbase - rext) > cfg.red_delta_threshold) & sat_changed

        # per-pixel flash counting drives the actual violations
        self.flash_gen.transitions(q_up, 1, tc)
        self.flash_gen.transitions(q_dn, -1, tc)
        self.flash_red.transitions(rq_up, 1, tc)
        self.flash_red.transitions(rq_dn, -1, tc)
        haz = haz_red = 0
        hz = self.flash_gen.hazard(tc)
        if hz.any():
            haz, _ = _best_window(hz, self.ww, self.wh)
        hzr = self.flash_red.hazard(tc)
        if hzr.any():
            haz_red, _ = _best_window(hzr, self.ww, self.wh)

        # pooled transition events are kept for the UI (charts, timeline)
        self.pool_gen.add(q_up, 1, tc)
        self.pool_gen.add(q_dn, -1, tc)
        self.pool_red.add(rq_up, 1, tc)
        self.pool_red.add(rq_dn, -1, tc)

        areas = {}
        for pool, kind in ((self.pool_gen, "general"), (self.pool_red, "red")):
            for pol in (1, -1):
                act = pool.active(pol, tc)
                count = int(act.sum())
                if count == 0:
                    areas[(kind, pol)] = 0
                    continue
                best, bbox = _best_window(act, self.ww, self.wh)
                areas[(kind, pol)] = best
                if best >= self.area_thresh:
                    self.events.append(
                        TransitionEvent(t, tc, pol, kind, best, bbox))
                    pool.clear(act)

        self.stat_t.append(t)
        self.stat_tc.append(tc)
        self.stat_lum.append(float(L.mean()))
        self.stat_up.append(areas[("general", 1)])
        self.stat_dn.append(areas[("general", -1)])
        self.stat_red.append(max(areas[("red", 1)], areas[("red", -1)]))
        self.stat_haz.append(haz)
        self.stat_haz_red.append(haz_red)
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
        res.violations += self._hazard_violations(self.stat_haz, "flash")
        res.violations += self._hazard_violations(self.stat_haz_red, "red")
        res.violations += self._extended_warnings()
        res.violations.sort(key=lambda v: v.start)
        return res

    def _hazard_violations(self, areas, kind):
        """Frames where pixels flashing above the frequency limit cover the
        area threshold, merged into intervals (native time)."""
        out = []
        cur = None
        cur_end_tc = None
        for i in range(self.n):
            if areas[i] < self.area_thresh:
                continue
            t, tc = self.stat_t[i], self.stat_tc[i]
            sev = round(areas[i] / self.area_thresh, 2)
            if cur is not None and tc - cur_end_tc <= 1.0:
                cur.end = max(cur.end, t)
                cur.count = max(cur.count, sev)
            else:
                cur = Violation(t, t, kind, sev)
                out.append(cur)
            cur_end_tc = tc
        return out

    def _extended_warnings(self):
        cfg = self.cfg
        if self.n == 0:
            return []
        third = self.area_thresh * cfg.extended_area_ratio
        t = np.asarray(self.stat_t)
        tc = np.asarray(self.stat_tc)
        flashy = (np.maximum(np.asarray(self.stat_up), np.asarray(self.stat_dn))
                  >= third) | (np.asarray(self.stat_red) >= third)
        out = []
        n = self.n
        j = 0
        cur = None
        for i in range(n):
            while j < n and tc[j] <= tc[i] + cfg.extended_window:
                j += 1
            span = j - i
            if span < 10 or (tc[min(j, n - 1)] - tc[i]) < cfg.extended_window * 0.9:
                continue
            cov = flashy[i:j].mean()
            if cov >= cfg.extended_coverage:
                s, e = float(t[i]), float(t[j - 1])
                if cur and s <= cur.end:
                    cur.end = max(cur.end, e)
                    cur.count = max(cur.count, float(cov))
                else:
                    cur = Violation(s, e, "extended", float(cov))
                    out.append(cur)
        return out


def _pair_flashes(events):
    """Pair opposing transitions into flashes. A pair must overlap spatially
    (opposing changes in unrelated regions are motion, not a flash) and occur
    within 1 second of each other on the internal clock.

    Returns a list of (tc, t_native) flash times.
    """
    flashes = []
    pending = []
    for e in sorted(events, key=lambda e: e.tc):
        pending = [p for p in pending if e.tc - p.tc <= 1.0]
        match = None
        for p in pending:
            if p.polarity != e.polarity and _bbox_overlap(p.bbox, e.bbox):
                match = p
                break
        if match is not None:
            pending.remove(match)
            flashes.append((e.tc, e.t))
        else:
            pending.append(e)
            if len(pending) > 12:
                pending.pop(0)
    return flashes


def _windows_over_limit(flashes, limit, kind):
    """1-second sliding windows containing more than `limit` flashes.
    `flashes` is a list of (tc, t_native); windows run on tc, violations are
    reported in native time."""
    out = []
    n = len(flashes)
    j = 0
    cur = None
    for i in range(n):
        while j < n and flashes[j][0] <= flashes[i][0] + 1.0:
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


def violations_to_sections(violations, cfg, bounds, keyframes=None):
    """Merge violations into padded, keyframe-snapped work sections.
    `bounds` = (ts_min, ts_max): the video's real native-pts range."""
    ts_min, ts_max = bounds
    intervals = []
    for v in violations:
        s = max(ts_min, v.start - cfg.section_pad)
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
        for _tc, t in _pair_flashes([e for e in result.events
                                     if e.kind == kind]):
            b = min(nbins - 1, max(0, int((t - ts_min) / bin_seconds)))
            arr[b] += 1
    return {"bin": bin_seconds, "t0": ts_min,
            "general": general.tolist(),
            "red": red.tolist()}
