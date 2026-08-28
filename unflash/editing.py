"""Section preparation, edit suggestion (keep-light / keep-dark) and fast
safety simulation.

A prepared section caches its analysis-resolution frames in an .npy file, so
"will my current edits pass?" is answered in a second or two without
rendering: the edited frame sequence is reconstructed in memory (removed
frames replaced by the last kept frame, extensions shifting time) and run
through the same detector used for scanning.
"""

import os

import numpy as np

from . import ffio
from .analysis import FlashDetector, _LUT, analyze_frames
from .config import detector_signature, profile_name

# How many frames are compared when checking that a fresh decode of a section
# still hands back the pictures its marks were made against, and how far out
# of step a decode is still recognised.
ALIGN_PROBE = 64
ALIGN_CAP = 900
ALIGN_MAX_SHIFT = 8
# Both sides of the comparison are decoded at analysis resolution, so the
# right alignment fits almost exactly (a fifth of a level, measured) while the
# next best sits a level or more away even on a near-still section. A fit
# worse than ALIGN_FIT means the two decodes are not of the same material at
# all and nothing should be concluded from them.
ALIGN_FIT = 1.0
ALIGN_TOL = 0.5


ALIGN_GRID = 8


def frame_signatures(frames, grid=ALIGN_GRID):
    """One fingerprint per frame: the mean RGB of each cell of a `grid`-square
    covering the picture, flattened.

    The cells are proportional, and area scaling averages the pixels it
    merges, so a frame signs the same at analysis resolution as at full size.
    That is what lets a render check the pictures it just decoded against the
    ones the editor showed without decoding the section a second time. Cells
    rather than one whole-frame mean because a shot can move a great deal
    without its average changing, and an alignment that cannot be read is as
    good as no check at all.
    """
    n = len(frames)
    out = np.empty((n, grid * grid * 3), np.float32)
    for i in range(0, n, 128):      # chunked: `frames` is usually a memmap
        blk = np.asarray(frames[i:i + 128], dtype=np.float32)
        m = blk.shape[0]
        ys = np.linspace(0, blk.shape[1], grid + 1).astype(int)
        xs = np.linspace(0, blk.shape[2], grid + 1).astype(int)
        cells = np.empty((m, grid, grid, 3), np.float32)
        for r in range(grid):
            for c in range(grid):
                cells[:, r, c] = blk[:, ys[r]:ys[r + 1],
                                     xs[c]:xs[c + 1]].mean(axis=(1, 2))
        out[i:i + m] = cells.reshape(m, -1)
    return out


def best_shift(fresh, cached, max_shift=ALIGN_MAX_SHIFT):
    """The s for which fresh[i] is the same picture as cached[i + s].

    Returns (shift, decisive). Alignment can only be read off a span that
    changes: over a still scene every offset fits as well as every other, and
    `decisive` is False to say the span has not answered the question. That
    is the whole reason a probe has to keep reading until it reaches motion --
    the opening seconds of a section are often a held shot, and they would
    happily report "in step" whatever the truth is.
    """
    scores = {}
    for s in range(-max_shift, max_shift + 1):
        i0 = max(0, -s)
        i1 = min(len(fresh), len(cached) - s)
        if i1 - i0 < 4:
            continue
        scores[s] = float(np.abs(fresh[i0:i1] - cached[i0 + s:i1 + s]).mean())
    if 0 not in scores or len(scores) < 2:
        return 0, False
    order = sorted(scores, key=scores.get)
    best, runner = order[0], order[1]
    if scores[best] > ALIGN_FIT:
        return 0, False          # not the same material; conclude nothing
    if scores[runner] <= 2 * scores[best] + ALIGN_TOL:
        return 0, False          # nothing here tells the two apart
    return best, True


def _cached_means(path, count, shape):
    """First `count` fingerprints of a cached analysis file, or None if it
    does not describe frames of `shape`. The mapping is closed before
    returning: on Windows a live memmap would block rewriting the file."""
    arr = np.load(path, mmap_mode="r")
    try:
        if arr.ndim != 4 or arr.shape[1:] != shape:
            return None
        return frame_signatures(arr[:min(count, arr.shape[0])])
    finally:
        mm = getattr(arr, "_mmap", None)
        if mm is not None:
            mm.close()


def cache_shift(project, sid, probe=ALIGN_PROBE, cap=ALIGN_CAP):
    """How far a fresh decode of a section has slipped against its cached
    analysis frames, in frames (0 = in step).

    A frame mark is an ordinal, and an ordinal only names a picture for as
    long as every decode of `-ss start -t duration` returns the same frames.
    That is not a promise ffmpeg makes: where `start` falls between two frames
    -- which is every section not cut exactly on a frame boundary -- whether
    the earlier frame survives the seek is its decision, and it has changed
    under prepared projects here. When it moves, the marks keep their numbers
    but the numbers now name the neighbouring picture, so a render holds the
    frames that were removed and drops the ones that were kept, and nothing
    downstream can tell: the frame count is identical either way. Measuring
    the slip costs one short decode.
    """
    sec = project.section(sid)
    cached = load_cache(project, sid)
    n = min(cap, cached.shape[0])
    if n < 8:
        return 0
    info = project.data["info"]
    aw, ah = ffio.analysis_dims(info["width"], info["height"],
                                project.detector_config)
    if cached.shape[1:] != (ah, aw, 3):
        return 0            # analysis scale changed; the cache is stale anyway
    ref = frame_signatures(cached[:n])
    fresh = []
    shift = 0
    for _, fr in ffio.iter_frames(project.video_path, aw, ah,
                                  start=sec["start"],
                                  duration=sec["end"] - sec["start"]):
        fresh.append(frame_signatures(fr[None, ...])[0])
        if len(fresh) >= n:
            break
        # stop as soon as the span read so far settles the question, which on
        # a section worth editing is within a second or two of its first
        # movement; a section that never settles it has nothing to correct
        if len(fresh) >= probe and len(fresh) % probe == 0:
            shift, decisive = best_shift(np.asarray(fresh), ref)
            if decisive:
                return shift
    if len(fresh) < 8:
        return 0
    return best_shift(np.asarray(fresh), ref)[0]


def prepare_section(project, sid, job=None):
    """Analyze the section, cache analysis frames, build proxy + thumbnails."""
    sec = project.section(sid)
    cfg = project.detector_config
    rc = project.render_config
    info = project.data["info"]
    start = sec["start"]
    dur = sec["end"] - sec["start"]
    sdir = project.section_dir(sid)
    aw, ah = ffio.analysis_dims(info["width"], info["height"], cfg)

    def prog(p, msg):
        if job:
            job.set_progress(p, msg)

    cancel = (job.cancelled if job else None)

    # pass 1: decode at analysis resolution; detect + cache
    det = FlashDetector(cfg, aw, ah)
    frames = []
    raw_pts = []
    est = max(1, int(dur * (info.get("fps") or 30)))
    for i, (t, fr) in enumerate(ffio.iter_frames(
            project.video_path, aw, ah, start=start, duration=dur,
            cancel=cancel)):
        det.feed(t, fr)
        frames.append(fr.copy())
        raw_pts.append(t - start)
        if job and i % 100 == 0:
            prog(0.5 * min(1.0, (i + 1) / est), f"analyzing frame {i + 1}")
    if job and job.cancelled():
        return None
    if not frames:
        raise RuntimeError("Section decoded zero frames")
    result = det.finish()
    # the section works on a sanitized timeline: source timestamp anomalies
    # (backward jumps, multi-second discontinuities) are bridged so rendering
    # never freezes on one frame; this same timeline drives render.py
    rel_pts, n_fixed = ffio.sanitize_deltas(raw_pts, cfg.max_frame_gap)
    rel_pts = [round(t, 6) for t in rel_pts]

    cache_npy = os.path.join(sdir, "analysis_frames.npy")
    # Re-preparing keeps the frame marks, so this decode has to land on the
    # same pictures the previous one did. If ffmpeg placed the seek elsewhere
    # this time, the marks would silently move to their neighbours; compare
    # the two decodes and carry the marks across by the measured amount.
    shift = 0
    if sec.get("prepared") and sec.get("edits") and os.path.exists(cache_npy):
        try:
            was_means = _cached_means(cache_npy, ALIGN_CAP, (ah, aw, 3))
        except (ValueError, OSError):
            was_means = None
        if was_means is not None:
            shift, _ = best_shift(frame_signatures(frames[:ALIGN_CAP]), was_means)
    np.save(cache_npy, np.stack(frames))
    del frames

    prog(0.5, "encoding proxy")
    proxy = os.path.join(sdir, "proxy.mp4")
    ffio.make_proxy(project.video_path, proxy, start, dur, rc,
                    progress=lambda p: prog(0.5 + 0.35 * p, "encoding proxy"),
                    cancel=cancel)

    prog(0.85, "extracting thumbnails")
    thumbs = os.path.join(sdir, "thumbs")
    n_thumbs = ffio.make_thumbnails(project.video_path, thumbs, start, dur,
                                    rc.thumb_width, cancel=cancel)

    warnings = []
    if n_fixed:
        warnings.append(
            f"{n_fixed} source timestamp anomalies were bridged "
            "(this video's timestamps jump backwards or by several seconds); "
            "output timing uses the repaired timeline.")
    if n_thumbs != len(rel_pts):
        warnings.append(
            f"Thumbnail count ({n_thumbs}) != decoded frame count "
            f"({len(rel_pts)}); some grid images may be missing or shifted.")
    # re-preparing keeps the frame marks, which are held by ordinal: if this
    # pass decoded a different number of frames, they no longer line up
    was = sec.get("n_frames") or 0
    if sec.get("prepared") and sec.get("edits") and was and was != len(rel_pts):
        warnings.append(
            f"Re-prepared with {len(rel_pts)} frames where the marks were "
            f"made against {was}; your marks were kept but now sit on "
            "different frames — check them before rendering.")
    edits = sec.get("edits") or {}
    if shift:
        edits = {str(int(k) - shift): v for k, v in edits.items()
                 if 0 <= int(k) - shift < len(rel_pts)}
        warnings.append(
            f"This decode of the section begins {shift:+d} frame(s) from the "
            "one your marks were made against (ffmpeg placed the seek "
            "differently), so every mark was moved by that much to stay on "
            "the picture you chose. Look over the section before rendering.")

    with project.lock:
        sec["prepared"] = True
        sec["edits"] = edits
        sec["n_frames"] = len(rel_pts)
        sec["pts"] = rel_pts
        sec["proxy"] = proxy
        sec["thumbs"] = thumbs
        sec["n_thumbs"] = n_thumbs
        sec["cache_npy"] = cache_npy
        sec["analysis"] = result.to_dict(include_stats=True)
        sec["warnings"] = warnings
        project.save()
    prog(1.0, "prepared")
    return sec


def load_cache(project, sid):
    sec = project.section(sid)
    if not sec.get("cache_npy") or not os.path.exists(sec["cache_npy"]):
        raise RuntimeError("Section not prepared (no cached analysis frames)")
    return np.load(sec["cache_npy"], mmap_mode="r")


def _parse_edits(edits):
    out = {}
    for k, v in (edits or {}).items():
        out[int(k)] = (bool(v.get("removed")), bool(v.get("extended")))
    return out


def simulate_edits(frames, rel_pts, edits, cfg, aw, ah,
                   extension_seconds=1.0):
    """Run the detector over the *edited* frame sequence without rendering."""
    det = FlashDetector(cfg, aw, ah)
    ed = _parse_edits(edits)
    n = len(rel_pts)
    # if the section starts with removed frames, they are backfilled from the
    # first kept frame (render.py does the same)
    first_kept = next((i for i in range(n) if not ed.get(i, (False, False))[0]),
                      0)
    last_kept = first_kept
    offset = 0.0
    for i in range(n):
        removed, extended = ed.get(i, (False, False))
        if removed:
            src = last_kept
        else:
            src = i
            last_kept = i
        det.feed(rel_pts[i] + offset, np.ascontiguousarray(frames[src]))
        if extended and not removed:
            # frame held still for extension_seconds: no transitions occur,
            # but everything after shifts later in time
            offset += extension_seconds
    return det.finish()


def _region_metric(frames, idxs, bbox):
    """Mean relative luminance of bbox region for the given frame ordinals."""
    x0, y0, x1, y1 = bbox
    sub = np.asarray(frames[idxs, y0:y1, x0:x1, :])
    lin = _LUT[sub]
    L = (0.2126 * lin[..., 0] + 0.7152 * lin[..., 1] + 0.0722 * lin[..., 2])
    return L.mean(axis=(1, 2))


def _violation_spans(result, pad=0.3):
    """Time spans the suggester should work on: WCAG failures always, plus
    extended flashes when the active profile flags them."""
    spans = []
    for v in result.violations:
        if v.kind == "extended" and not result.flag_extended:
            continue
        spans.append([v.start - pad, v.end + pad])
    spans.sort()
    merged = []
    for s, e in spans:
        if merged and s <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], e)
        else:
            merged.append([s, e])
    return merged


def _span_bbox(result, s, e, aw, ah):
    xs0, ys0, xs1, ys1 = aw, ah, 0, 0
    found = False
    for ev in result.events:
        if s - 0.5 <= ev.t <= e + 0.5 and ev.bbox:
            found = True
            xs0 = min(xs0, ev.bbox[0]); ys0 = min(ys0, ev.bbox[1])
            xs1 = max(xs1, ev.bbox[2]); ys1 = max(ys1, ev.bbox[3])
    if not found:
        return (0, 0, aw, ah)
    return (xs0, ys0, xs1, ys1)


def suggest_edits(project, sid, prefer="light", only=None, job=None):
    """Propose a set of frame removals so the section passes the detector.

    prefer='light': hold bright frames, drop dark phases (and vice versa).
    only: optional collection of ordinals — removals are restricted to these
    frames (the user's selection); existing edits elsewhere are preserved
    and included in the simulation.
    Iterates: propose -> simulate -> escalate, up to 5 rounds.
    """
    sec = project.section(sid)
    cfg = project.detector_config
    info = project.data["info"]
    aw, ah = ffio.analysis_dims(info["width"], info["height"], cfg)
    frames = load_cache(project, sid)
    rel_pts = sec["pts"]
    n = len(rel_pts)
    t_arr = np.asarray(rel_pts)
    only_set = set(int(i) for i in only) if only else None
    # existing edits outside the restricted scope stay in force
    base_edits = {}
    if only_set is not None:
        base_edits = {k: v for k, v in (sec.get("edits") or {}).items()
                      if int(k) not in only_set}

    def prog(p, msg):
        if job:
            job.set_progress(p, msg)

    prog(0.05, "analyzing section")
    ext_s = project.render_config.extension_seconds
    base = simulate_edits(frames, rel_pts, base_edits, cfg, aw, ah, ext_s)
    if base.safe:
        return {"edits": {}, "safe": True, "rounds": 0,
                "note": "Already passes — nothing to remove."}

    removed = set()

    def allowed(i):
        return i != 0 and (only_set is None or i in only_set)

    def spans_to_indices(s, e):
        return np.nonzero((t_arr >= s) & (t_arr <= e))[0].tolist()

    def apply_percentile_pass(result, tight):
        for s, e in _violation_spans(result):
            idxs = spans_to_indices(s, e)
            if len(idxs) < 2:
                continue
            bbox = _span_bbox(result, s, e, aw, ah)
            m = _region_metric(frames, idxs, bbox)
            hi = float(np.percentile(m, 85))
            lo = float(np.percentile(m, 15))
            if hi - lo < 1e-4:
                continue
            if tight:
                cut = hi - 0.15 * (hi - lo) if prefer == "light" \
                    else lo + 0.15 * (hi - lo)
            else:
                cut = (hi + lo) / 2.0
            for k, i in enumerate(idxs):
                if not allowed(i):
                    continue
                bad = m[k] < cut if prefer == "light" else m[k] > cut
                if bad:
                    removed.add(i)

    def apply_hold_all(result):
        for s, e in _violation_spans(result):
            idxs = spans_to_indices(s, e)
            if not idxs:
                continue
            bbox = _span_bbox(result, s, e, aw, ah)
            kept = [i for i in idxs if i not in removed]
            if not kept:
                continue
            m = _region_metric(frames, kept, bbox)
            anchor = kept[int(np.argmax(m) if prefer == "light"
                              else np.argmin(m))]
            for i in idxs:
                if i != anchor and allowed(i):
                    removed.add(i)

    result = base
    rounds = 0
    note = ""
    for attempt in range(5):
        rounds = attempt + 1
        if attempt == 0:
            apply_percentile_pass(result, tight=False)
        elif attempt == 1:
            apply_percentile_pass(result, tight=True)
        else:
            apply_hold_all(result)
        prog(0.2 + 0.15 * attempt, f"verifying proposal (round {rounds})")
        edits = dict(base_edits)
        edits.update({str(i): {"removed": True, "extended": False}
                      for i in removed})
        result = simulate_edits(frames, rel_pts, edits, cfg, aw, ah, ext_s)
        if result.safe:
            note = f"Passes after removing {len(removed)} frames."
            break
    else:
        note = (f"Still failing after {len(removed)} removals — "
                + ("try widening the selection or edit manually."
                   if only_set is not None else "needs manual attention."))

    return {
        "edits": {str(i): {"removed": True, "extended": False}
                  for i in sorted(removed)},
        "safe": bool(result.safe),
        "rounds": rounds,
        "note": note,
        "verdict": result.to_dict(),
    }


def check_section(project, sid, edits=None):
    """Fast safety verdict for the current (or given) edits. The verdict
    includes `flagged_frames`: the ordinals inside failing windows, so the UI
    can highlight where the remaining problems are."""
    sec = project.section(sid)
    cfg = project.detector_config
    info = project.data["info"]
    ext_s = project.render_config.extension_seconds
    aw, ah = ffio.analysis_dims(info["width"], info["height"], cfg)
    frames = load_cache(project, sid)
    use_edits = edits if edits is not None else sec["edits"]
    result = simulate_edits(frames, sec["pts"], use_edits, cfg, aw, ah, ext_s)
    verdict = result.to_dict()
    # record the settings behind the verdict, so a later profile change can be
    # told from a verdict that still describes the project as it stands
    verdict["profile"] = profile_name(project.data["detector"])
    verdict["detector_sig"] = detector_signature(project.data["detector"])

    # map violation windows back to frame ordinals via the simulated
    # display timeline (extensions shift everything after them)
    ed = _parse_edits(use_edits)
    disp = []
    off = 0.0
    for i, t in enumerate(sec["pts"]):
        disp.append(t + off)
        removed, extended = ed.get(i, (False, False))
        if extended and not removed:
            off += ext_s
    flagged = set()
    for v in result.violations:
        if v.kind == "extended" and not result.flag_extended:
            continue
        for i, t in enumerate(disp):
            if v.start - 0.05 <= t <= v.end + 0.05:
                flagged.add(i)
    verdict["flagged_frames"] = sorted(flagged)

    with project.lock:
        sec["check"] = verdict
        project.save()
    return verdict
