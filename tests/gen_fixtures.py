#!/usr/bin/env python3
"""Reference fixtures for the Rust port.

Runs the Python detector (unflash/analysis.py, the reference) over
procedurally generated frame sequences and writes what it saw to
tests/fixtures/*.json. The Rust integration test
crates/unflash-core/tests/reference_fixtures.rs regenerates the identical
frames (checked by CRC-32) and asserts the port produces the same per-frame
hazard areas, events and violations.

    python3 tests/gen_fixtures.py

Needs numpy only.
"""
import json
import math
import os
import sys
import zlib

import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)

from unflash.analysis import FlashDetector  # noqa: E402
from unflash.config import profile_config  # noqa: E402

W, H = 128, 96
OUT = os.path.join(ROOT, "tests", "fixtures")


def hash_noise(i, amp):
    """Per-pixel hash noise in [-amp, amp], identical to the Rust generator."""
    yy, xx = np.meshgrid(np.arange(H, dtype=np.uint32), np.arange(W, dtype=np.uint32), indexing="ij")
    ii = np.full((H, W), i, np.uint32)
    with np.errstate(over="ignore"):
        h = ii * np.uint32(2654435761) + yy * np.uint32(2246822519) + xx * np.uint32(3266489917)
        h = h ^ (h >> np.uint32(15))
        h = h * np.uint32(2246822519)
        h = h ^ (h >> np.uint32(13))
        h = h >> np.uint32(24)
        n = (h % np.uint32(2 * amp + 1)).astype(np.int32) - amp
    return n


def solid(code):
    f = np.empty((H, W, 3), np.uint8)
    f[...] = code
    return f


def rect_centred(frac):
    rw = int(W * frac ** 0.5)
    rh = int(H * frac ** 0.5)
    x0 = (W - rw) // 2
    y0 = (H - rh) // 2
    return x0, y0, x0 + rw, y0 + rh


def phase(t, hz):
    return int(math.floor(t * hz * 2)) % 2


def times_2997(secs):
    """The frame times of `secs` seconds at 29.97 fps."""
    n = int(round(secs * 30000 / 1001))
    return [i * 1001 / 30000 for i in range(n)]


def gen_square(t, hz, frac, lo, hi, bg=20):
    f = solid(bg)
    x0, y0, x1, y1 = rect_centred(frac)
    f[y0:y1, x0:x1] = lo if phase(t, hz) == 0 else hi
    return f


def scenario_square(hz, secs, frac, lo, hi):
    ts = times_2997(secs)
    return ts, [gen_square(t, hz, frac, lo, hi) for t in ts]


def scenario_noise_drift(secs, amp):
    ts = times_2997(secs)
    n = len(ts)
    frames = []
    for i in range(n):
        base = 30 + (i * 170) // n
        v = np.clip(base + hash_noise(i, amp), 0, 255).astype(np.uint8)
        frames.append(np.repeat(v[..., None], 3, axis=2))
    return ts, frames


def scenario_red_pair(secs, hz, a, b):
    """The whole picture swapping between two colours."""
    ts = times_2997(secs)
    frames = []
    for t in ts:
        f = np.empty((H, W, 3), np.uint8)
        f[...] = a if phase(t, hz) == 0 else b
        frames.append(f)
    return ts, frames


def scenario_pan_bar(secs):
    ts = times_2997(secs)
    bw = W // 10
    frames = []
    for t in ts:
        f = solid(20)
        x = int(math.floor(t * W)) % W
        for k in range(bw):
            f[:, (x + k) % W] = 230
        frames.append(f)
    return ts, frames


def scenario_repeat120(secs, hz):
    n = int(round(secs * 120))
    ts = [i / 120 for i in range(n)]
    return ts, [gen_square(t, hz, 0.5, 20, 200) for t in ts]


def scenario_vfr(secs, hz):
    base = times_2997(secs)
    ts = []
    for i, t in enumerate(base):
        if 60 <= i < 120:
            t = t - 0.5
        if i >= 150:
            t = t + 10.0
        ts.append(t)
    # the *pictures* follow the schedule the frames were shot on
    return ts, [gen_square(t, hz, 0.5, 20, 200) for t in base]


def scenario_ramp(secs):
    ts = times_2997(secs)
    codes = [40, 75, 110, 145, 180, 145, 110, 75]
    return ts, [solid(codes[i % 8]) for i in range(len(ts))]


def scenario_partial(secs, hz, rw, rh):
    ts = times_2997(secs)
    frames = []
    for t in ts:
        f = solid(20)
        f[0:rh, 0:rw] = 20 if phase(t, hz) == 0 else 200
        frames.append(f)
    return ts, frames


def scenario_red_noise(secs, hz, amp):
    ts = times_2997(secs)
    x0, y0, x1, y1 = rect_centred(0.5)
    frames = []
    for i, t in enumerate(ts):
        v = np.clip(60 + hash_noise(i, amp), 0, 255).astype(np.uint8)
        f = np.repeat(v[..., None], 3, axis=2)
        if phase(t, hz) == 0:
            f[y0:y1, x0:x1, 0] = 255
            f[y0:y1, x0:x1, 1] = 0
            f[y0:y1, x0:x1, 2] = 0
        else:
            f[y0:y1, x0:x1] = 144
        frames.append(f)
    return ts, frames


SCENARIOS = {
    "square4hz": (lambda: scenario_square(4.0, 6.0, 0.5, 20, 200), ["wcag"]),
    "square3hz": (lambda: scenario_square(3.0, 10.0, 0.5, 20, 200), ["wcag_ext", "wcag", "strict"]),
    "noise_drift": (lambda: scenario_noise_drift(8.0, 12), ["wcag_ext"]),
    "red_grey": (lambda: scenario_red_pair(5.0, 4.0, (255, 0, 0), 144), ["wcag"]),
    # saturated red against a grey of the same relative luminance: no
    # luminance flash at all, only the colour moves
    "red_equilum": (lambda: scenario_red_pair(5.0, 5.0, (250, 0, 0), (122, 124, 122)), ["wcag"]),
    "pan_bar": (lambda: scenario_pan_bar(6.0), ["wcag_ext"]),
    "repeat120": (lambda: scenario_repeat120(4.0, 4.0), ["wcag"]),
    "vfr": (lambda: scenario_vfr(8.0, 4.0), ["wcag"]),
    "ramp": (lambda: scenario_ramp(6.0), ["wcag"]),
    "partial15": (lambda: scenario_partial(6.0, 4.0, 33, 25), ["wcag"]),
    "partial35": (lambda: scenario_partial(6.0, 4.0, 50, 38), ["wcag"]),
    "red_noise": (lambda: scenario_red_noise(6.0, 4.0, 8), ["wcag_ext"]),
    # red against a darker red: one chromaticity, so no red flash under WCAG
    # 2.2 (2.0's formula called it one); it flashes in luminance
    "red_darkred": (lambda: scenario_red_pair(5.0, 4.0, (255, 0, 0), (110, 0, 0)), ["wcag"]),
    # red against a green of the same luminance: a red flash only
    "red_green": (lambda: scenario_red_pair(5.0, 4.0, (200, 0, 0), (0, 116, 0)), ["wcag"]),
}


def run(name, ts, frames, profile):
    cfg = profile_config(profile)
    det = FlashDetector(cfg, W, H)
    held = []
    crc = 0
    for t, fr in zip(ts, frames):
        fr = np.ascontiguousarray(fr)
        crc = zlib.crc32(fr.tobytes(), crc)
        before = det.held
        det.feed(t, fr)
        held.append(int(det.held - before))
    res = det.finish()
    out = {
        "name": name,
        "profile": profile,
        "w": W,
        "h": H,
        "frames_crc32": crc & 0xFFFFFFFF,
        "t": ts,
        "stats": {
            "tc": list(det.stat_tc),
            "lum": [round(float(x), 6) for x in det.stat_lum],
            "hazard": list(det.stat_haz),
            "hazard_red": list(det.stat_haz_red),
            "ext": list(det.stat_ext),
            "ext_red": list(det.stat_ext_red),
            "up": list(det.stat_up),
            "down": list(det.stat_dn),
            "red": list(det.stat_red),
            "onset": list(det.stat_haz_onset),
            "onset_red": list(det.stat_haz_red_onset),
            "held": held,
        },
        "events": [
            {"t": e.t, "tc": e.tc, "kind": e.kind, "area": e.area, "bbox": list(e.bbox)}
            for e in res.events
        ],
        "violations": [
            {"start": v.start, "end": v.end, "kind": v.kind, "count": v.count,
             "onset": v.onset, "peak": v.peak}
            for v in res.violations
        ],
        "anomalies": res.anomalies,
        "held": det.held,
        "safe": res.safe,
        "wcag_safe": res.wcag_safe,
        "area_thresh": det.area_thresh,
        "ww": det.ww,
        "wh": det.wh,
    }
    path = os.path.join(OUT, f"{name}_{profile}.json")
    with open(path, "w") as f:
        json.dump(out, f, separators=(",", ":"))
    print(f"{name:12s} {profile:9s} frames={len(ts):4d} held={det.held:4d} "
          f"violations={[(v.kind, round(v.start, 2), round(v.end, 2)) for v in res.violations]}")


def main():
    os.makedirs(OUT, exist_ok=True)
    names = sys.argv[1:] or list(SCENARIOS)
    for name in names:
        gen, profiles = SCENARIOS[name]
        ts, frames = gen()
        for profile in profiles:
            run(name, ts, frames, profile)


if __name__ == "__main__":
    main()
