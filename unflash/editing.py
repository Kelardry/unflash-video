"""Section preparation, edit suggestion (keep-light / keep-dark) and fast
safety simulation.

A prepared section caches its analysis-resolution frames in an .npy file, so
"will my current edits pass?" is answered in a second or two without
rendering: the edited frame sequence is reconstructed in memory (removed
frames replaced by the last kept frame, extensions shifting time) and run
through the same detector used for scanning.
"""

import os
from dataclasses import asdict

import numpy as np

from . import ffio
from .analysis import (FlashDetector, _LUT, analyze_frames,
                       context_seconds)
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


def _cache_context(project, sec, sdir, aw, ah, cancel=None):
    """Decode and cache the footage either side of a section.

    The detector needs a run-up before its verdict at the section's first
    frame matches what a pass over the whole video says there, and a run-out
    before it can tell whether an edit has pushed flashing past the section's
    last frame. Both are decoded once, here, so the fast check stays fast.
    """
    cfg = project.detector_config
    need = context_seconds(cfg)
    ts_min, ts_max = project.bounds
    start, end = sec["start"], sec["end"]
    lead_from = max(ts_min, start - need)
    tail_to = min(ts_max, end + need)
    lead, lead_pts, tail, tail_pts = [], [], [], []
    if start - lead_from > 1e-3:
        for t, fr in ffio.iter_frames(project.video_path, aw, ah,
                                      start=lead_from,
                                      duration=start - lead_from,
                                      cancel=cancel):
            if t >= start - 1e-6:
                break
            lead.append(fr.copy())
            lead_pts.append(round(t - start, 6))     # negative
    if tail_to - end > 1e-3:
        for t, fr in ffio.iter_frames(project.video_path, aw, ah,
                                      start=end, duration=tail_to - end,
                                      cancel=cancel):
            if t <= end + 1e-6:
                continue
            tail.append(fr.copy())
            tail_pts.append(round(t - start, 6))
    path = os.path.join(sdir, "context_frames.npy")
    if lead or tail:
        np.save(path, np.stack(lead + tail))
    else:
        path = None
    return {"npy": path, "lead_n": len(lead), "tail_n": len(tail),
            "lead_pts": lead_pts, "tail_pts": tail_pts,
            "seconds": need}


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

    prog(0.48, "decoding run-up frames")
    ctx_meta = _cache_context(project, sec, sdir, aw, ah, cancel)

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
        sec["context"] = ctx_meta
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


class SectionContext:
    """The footage on either side of a section, as the export will contain it.

    `lead` runs up to the section's first frame (times are negative, ending
    one frame before 0) and `tail` runs on from its last (times are relative
    to the end of the edited section). Frames are at analysis resolution.
    `notes` records anything that made the context less than complete, so a
    verdict built on it can say so.
    """

    __slots__ = ("lead", "lead_pts", "tail", "tail_pts", "notes", "next_at")

    def __init__(self, lead, lead_pts, tail, tail_pts, notes=None,
                 next_at=None):
        self.lead = lead
        self.lead_pts = lead_pts
        self.tail = tail
        self.tail_pts = tail_pts
        self.notes = notes or []
        # how far into the run-out the *next* section starts, or None if none
        # does. Flashing from there on is that section's to fix; before it the
        # material is untouched by any section, so flashing the edits pushed
        # out there is nobody else's problem.
        self.next_at = next_at

    def __bool__(self):
        return bool(len(self.lead) or len(self.tail))


def _median_dt(pts, fallback=1.0 / 30):
    if len(pts) < 2:
        return fallback
    d = np.diff(np.asarray(pts, float))
    d = d[d > 0]
    return float(np.median(d)) if d.size else fallback


def _npy_len(path):
    """Frame count of a cached .npy, without leaving the file mapped."""
    arr = np.load(path, mmap_mode="r")
    try:
        return int(arr.shape[0])
    finally:
        mm = getattr(arr, "_mmap", None)
        if mm is not None:
            mm.close()


def _take_frames(path, idxs):
    """Copy the given frames out of a cached .npy and let go of the file.

    The copy is the point: a numpy memmap (or any view onto one) keeps the
    file open, and on Windows that blocks preparing the section again, which
    rewrites exactly these caches.
    """
    arr = np.load(path, mmap_mode="r")
    try:
        return [np.array(arr[i]) for i in idxs]
    finally:
        mm = getattr(arr, "_mmap", None)
        if mm is not None:
            mm.close()


def _sections_in(project, sid, t_lo, t_hi):
    """Other sections overlapping a source-time window, in time order."""
    return [s for s in project.sections_sorted()
            if str(s["id"]) != str(sid)
            and s["end"] > t_lo + 1e-6 and s["start"] < t_hi - 1e-6]


def _edited_edge(project, sec, seconds, side, ext_s):
    """The last (side='lead') or first (side='tail') `seconds` of another
    section as the export will contain it -- its cached frames with its own
    edits applied.

    A section close enough to fall inside this one's run-up or run-out is
    material the user has already worked on. Reading the original frames
    there would report flashing that no longer exists in the export and that
    this section cannot remove, so the neighbour's edited frames stand in.
    Returns (frames, times) on the neighbour's own edited timeline, or None
    if it is not prepared.
    """
    if not sec.get("prepared") or not sec.get("cache_npy") or not sec.get("pts"):
        return None
    if not os.path.exists(sec["cache_npy"]):
        return None
    seq = edited_sequence(sec["pts"], sec.get("edits"), ext_s)
    if not seq:
        return None
    if side == "lead":
        want = [(t, src) for t, src in seq if t > seq[-1][0] - seconds]
    else:
        want = [(t, src) for t, src in seq if t < seq[0][0] + seconds]
    if not want:
        return None
    return (_take_frames(sec["cache_npy"], [src for _, src in want]),
            [t for t, _ in want])


def _compose(project, sid, src, sec_start, t_lo, t_hi, need, ext_s, side,
             notes):
    """Lay out the material between source times `t_lo` and `t_hi` the way the
    export will contain it: cached original frames where no section covers
    them, and each covering section's edited frames where one does.

    `src` is (frames, times) of the cached original run-up/run-out, times
    relative to `sec_start`. Returns a list of (frames, times) runs in order.
    """
    others = _sections_in(project, sid, t_lo, t_hi)
    frames, times = src if src else ([], [])
    parts = []

    def original(lo, hi):
        seg = [(f, t) for f, t in zip(frames, times)
               if lo - 1e-6 <= t + sec_start < hi - 1e-6]
        if seg:
            parts.append(([f for f, _ in seg], [t for _, t in seg]))

    cursor = t_lo
    for o in others:
        original(cursor, o["start"])
        edge = _edited_edge(project, o, need, side, ext_s)
        if edge:
            parts.append(edge)
        else:
            original(max(cursor, o["start"]), min(t_hi, o["end"]))
            where = "run-up" if side == "lead" else "run-out"
            notes.append(
                f"Section #{o['id']} lies within this one's {where} but is "
                "not prepared, so this check reads its original frames "
                "— edits made there are not reflected here.")
        cursor = max(cursor, o["end"])
    original(cursor, t_hi)
    return parts


def _join(parts, dt):
    """Lay runs of time-stamped frames end to end on one timeline, each
    starting `dt` after the one before. Returns (frames, times) with the
    first frame at time 0."""
    frames, times = [], []
    cursor = 0.0
    for fr, ts in parts:
        if not len(fr):
            continue
        base = ts[0]
        for f, t in zip(fr, ts):
            frames.append(f)
            times.append(cursor + (t - base))
        cursor = times[-1] + dt
    return frames, times


def section_context(project, sid, ext_s=None):
    """Build the run-up and run-out for a section's safety check.

    The frames come from the cache `prepare_section` laid down, except where
    they fall inside another section, which contributes its edited frames
    instead (see _edited_edge) -- what the check has to reason about is the
    exported video, not the original.
    """
    sec = project.section(sid)
    cfg = project.detector_config
    if ext_s is None:
        ext_s = project.render_config.extension_seconds
    need = context_seconds(cfg)
    notes = []
    ctx = sec.get("context")
    lead_src = tail_src = None
    if isinstance(ctx, dict) and ctx.get("npy") and os.path.exists(ctx["npy"]):
        n_lead = int(ctx.get("lead_n") or 0)
        n_tail = int(ctx.get("tail_n") or 0)
        if _npy_len(ctx["npy"]) >= n_lead + n_tail:
            if n_lead:
                lead_src = (_take_frames(ctx["npy"], range(n_lead)),
                            list(ctx.get("lead_pts") or []))
            if n_tail:
                tail_src = (_take_frames(ctx["npy"],
                                         range(n_lead, n_lead + n_tail)),
                            list(ctx.get("tail_pts") or []))
    elif isinstance(ctx, dict) and not ctx.get("npy"):
        pass        # nothing to cache: the section spans the whole video
    else:
        # no cache at all -- prepared by a version that did not keep one, or
        # the file has gone from the work folder
        notes.append(
            "This section has no cached run-up, so its check starts cold at "
            "its first frame and cannot see flashing in its opening second "
            "— that is the flashing a verify of the whole export finds "
            "inside a section that checked out safe. Prepare it again for a "
            "full check.")
        ctx = ctx if isinstance(ctx, dict) else {}
    if ctx and (ctx.get("seconds") or 0) + 1e-6 < need:
        notes.append(
            "The detection profile now needs a longer run-up than this "
            "section was prepared with; prepare it again so its check sees "
            "everything a pass over the whole export sees.")

    dt = _median_dt(sec.get("pts") or [])
    ts_min, ts_max = project.bounds

    lead_parts = _compose(project, sid, lead_src, sec["start"],
                          max(ts_min, sec["start"] - need), sec["start"],
                          need, ext_s, "lead", notes)
    lead, lead_t = _join(lead_parts, dt)
    if lead:
        lead_t = [t - (lead_t[-1] + dt) for t in lead_t]   # ends before 0

    tail_parts = _compose(project, sid, tail_src, sec["start"],
                          sec["end"], min(ts_max, sec["end"] + need),
                          need, ext_s, "tail", notes)
    tail, tail_t = _join(tail_parts, dt)
    tail_t = [t + dt for t in tail_t]                      # starts after 0

    # where the next section begins inside the run-out. Flashing from there
    # on is that section's to fix; before it the material is untouched by
    # any section, so flashing the edits push out there is this one's.
    nxt = _sections_in(project, sid, sec["end"], sec["end"] + need)
    next_at = (nxt[0]["start"] - sec["end"]) if nxt else None
    return SectionContext(lead, lead_t, tail, tail_t, notes, next_at)


def edited_sequence(rel_pts, edits, extension_seconds=1.0):
    """The section's edited timeline as [(display_time, source_ordinal)].

    Removed frames are backfilled from the last kept frame and extended ones
    push everything after them later, exactly as render.py lays them out, so
    this is the frame order the exported file will actually contain.
    """
    ed = _parse_edits(edits)
    n = len(rel_pts)
    # if the section starts with removed frames, they are backfilled from the
    # first kept frame (render.py does the same)
    first_kept = next((i for i in range(n) if not ed.get(i, (False, False))[0]),
                      0)
    last_kept = first_kept
    offset = 0.0
    out = []
    for i in range(n):
        removed, extended = ed.get(i, (False, False))
        if removed:
            src = last_kept
        else:
            src = i
            last_kept = i
        out.append((rel_pts[i] + offset, src))
        if extended and not removed:
            # frame held still for extension_seconds: no transitions occur,
            # but everything after shifts later in time
            offset += extension_seconds
    return out


def simulate_edits(frames, rel_pts, edits, cfg, aw, ah,
                   extension_seconds=1.0, context=None):
    """Run the detector over the *edited* frame sequence without rendering.

    `context` (from section_context) supplies the real footage either side of
    the section. Without it the detector starts cold at the section's first
    frame and is blind for its first second or so -- it takes a second's
    worth of flashes to know the rate has been exceeded -- so flashing left
    at the head of a section passes this check and then shows up in a pass
    over the whole export, which arrives there already warmed up. Feeding the
    run-up first puts the detector in the state the whole-video pass would
    hand over in; feeding the run-out afterwards catches flashing the edit
    pushes just past the section's end.
    """
    det = FlashDetector(cfg, aw, ah)
    seq = edited_sequence(rel_pts, edits, extension_seconds)
    if context:
        for t, fr in zip(context.lead_pts, context.lead):
            det.feed(t, np.ascontiguousarray(fr))
    for t, src in seq:
        det.feed(t, np.ascontiguousarray(frames[src]))
    if context:
        end = seq[-1][0] if seq else 0.0
        for t, fr in zip(context.tail_pts, context.tail):
            det.feed(end + t, np.ascontiguousarray(fr))
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
    extended flashes when the active profile flags them.

    Spans run from each violation's onset, not the frame where the rate was
    first exceeded -- the flashes that add up to a failure begin up to a
    second earlier, and removing only the frames from the announcement
    onwards leaves the run-in that caused it.
    """
    spans = []
    for v in result.violations:
        if v.kind == "extended" and not result.flag_extended:
            continue
        spans.append([min(v.onset, v.start) - pad, v.end + pad])
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
    # the proposal has to satisfy the same context-aware test check_section
    # applies, or it would "pass" only because the detector started cold
    ctx = section_context(project, sid, ext_s)

    def sim(ed):
        r = simulate_edits(frames, rel_pts, ed, cfg, aw, ah, ext_s,
                           context=ctx)
        seq = edited_sequence(rel_pts, ed, ext_s)
        inside, after, _, _ = _classify(r, seq[-1][0] if seq else 0.0,
                                        ctx.next_at)
        r.violations = inside + after
        return r

    base = sim(base_edits)
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
        result = sim(edits)
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


def analyze_rendered_section(project, sid, path):
    """Detector pass over a rendered section file, with the same run-up and
    run-out its fast check uses.

    Without them this reads the file cold and repeats the fast check's old
    blind spot at a higher cost: a rendered section can be pronounced safe
    and still be flashing in its opening second, which only shows up when the
    whole export is verified.
    """
    cfg = project.detector_config
    info = project.data["info"]
    aw, ah = ffio.analysis_dims(info["width"], info["height"], cfg)
    ctx = section_context(project, sid)
    det = FlashDetector(cfg, aw, ah)
    for t, fr in zip(ctx.lead_pts, ctx.lead):
        det.feed(t, np.ascontiguousarray(fr))
    end = 0.0
    dt = _median_dt(project.section(sid).get("pts") or [])
    for t, fr in ffio.iter_frames(path, aw, ah):
        det.feed(t, fr)
        end = t
    for t, fr in zip(ctx.tail_pts, ctx.tail):
        det.feed(end + t, np.ascontiguousarray(fr))
    result = det.finish()
    inside, after, _, _ = _classify(result, end + dt * 0.5, ctx.next_at)
    result.violations = inside + after
    return result, after


def _classify(result, end_disp, next_at=None):
    """Split a context-aware simulation's violations by where they land.

    Anything finished before the section's first frame belongs to earlier
    material and is only there to warm the detector up. Anything whose
    flashing starts after its last frame is in the run-out: the section
    cannot edit it, but the user still needs to know, because it is flashing
    the export will contain and this section's own edits may have caused it.
    Run-out flashing inside the *next* section is that section's to fix, so
    it is handed back as `elsewhere` and left out of this verdict.

    Which side of the boundary a violation falls on is decided by `start`
    and `end` -- where the flashing actually is -- never by `onset`. Onset
    reaches deliberately backwards, up to a failure window before the
    flashing is announced, so that a section can be padded far enough to
    contain the frames that feed it. Letting it decide ownership instead
    blames a section for flashing that begins after its last frame merely
    because the reach crosses the boundary, and then offers its final
    frames as the ones to remove -- frames that have nothing to do with it.
    """
    inside, after, elsewhere, before = [], [], [], []
    for v in result.violations:
        if v.kind == "extended" and not result.flag_extended:
            continue
        if v.end < -1e-6:
            before.append(v)
        elif v.start > end_disp + 1e-6:
            (elsewhere
             if next_at is not None and v.start >= end_disp + next_at - 1e-6
             else after).append(v)
        else:
            inside.append(v)
    return inside, after, elsewhere, before


def check_section(project, sid, edits=None):
    """Fast safety verdict for the current (or given) edits. The verdict
    includes `flagged_frames`: the ordinals inside failing windows, so the UI
    can highlight where the remaining problems are.

    The simulation is fed the real footage either side of the section (see
    section_context), so it reaches the section's first frame in the state a
    pass over the whole export arrives in and carries the section's edits on
    past its last frame. Violations that fall wholly in the run-up are the
    warm-up's own and are dropped; ones in the run-out are reported
    separately as `after`, since they are real but not editable here.
    """
    sec = project.section(sid)
    cfg = project.detector_config
    info = project.data["info"]
    ext_s = project.render_config.extension_seconds
    aw, ah = ffio.analysis_dims(info["width"], info["height"], cfg)
    frames = load_cache(project, sid)
    use_edits = edits if edits is not None else sec["edits"]
    ctx = section_context(project, sid, ext_s)
    result = simulate_edits(frames, sec["pts"], use_edits, cfg, aw, ah, ext_s,
                            context=ctx)

    seq = edited_sequence(sec["pts"], use_edits, ext_s)
    end_disp = seq[-1][0] if seq else 0.0
    inside, after, elsewhere, _ = _classify(result, end_disp, ctx.next_at)
    result.violations = inside + after

    verdict = result.to_dict()
    # record the settings behind the verdict, so a later profile change can be
    # told from a verdict that still describes the project as it stands
    verdict["profile"] = profile_name(project.data["detector"])
    verdict["detector_sig"] = detector_signature(project.data["detector"])
    verdict["after"] = [asdict(v) for v in after]
    verdict["elsewhere"] = [asdict(v) for v in elsewhere]
    verdict["context_seconds"] = context_seconds(cfg)
    verdict["context_notes"] = list(ctx.notes)
    verdict["context_lead"] = (round(-ctx.lead_pts[0], 3)
                               if ctx.lead_pts else 0.0)
    verdict["context_tail"] = (round(ctx.tail_pts[-1], 3)
                               if ctx.tail_pts else 0.0)

    # map violation windows back to frame ordinals via the simulated
    # display timeline (extensions shift everything after them)
    disp = [t for t, _ in seq]
    flagged = set()
    for v in inside:
        lo = min(v.onset, v.start)
        for i, t in enumerate(disp):
            if lo - 0.05 <= t <= v.end + 0.05:
                flagged.add(i)
    verdict["flagged_frames"] = sorted(flagged)
    # a violation that carries on past the section's last frame cannot be
    # cleared from inside it; say so rather than leaving the user to work out
    # why the frames on offer make no difference
    verdict["spills"] = [asdict(v) for v in inside if v.end > end_disp + 1e-6]

    with project.lock:
        sec["check"] = verdict
        project.save()
    return verdict
