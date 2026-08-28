"""Rendering: apply frame edits to a section, and assemble the final video.

VFR handling: edits never re-time surviving frames (removed frames are
*replaced*, not dropped), so the output keeps the source's exact frame timing.
Frames are emitted onto a fine constant-rate grid (>= 2x the source rate, so
placement error is bounded by half a grid slot, a few ms, and never
accumulates). Extensions insert `extension_seconds` of held frame + silence,
shifting everything after them equally in video and audio.

Assembly is picture-first: every part is video-only, so the join can never be
thrown off by an audio stream that disagrees with its own video, and the whole
soundtrack is encoded once at the end against the finished picture. Audio cut
into per-part pieces leaves an encoder boundary at every section edge, which
is audible; joining the samples before they reach the encoder is not.
"""

import hashlib
import json
import os
import subprocess
import threading

import numpy as np

from . import ffio
from .analysis import analyze_file
from .ffio import FFMPEG, FFError, CREATE_NO_WINDOW

# Every part handed to the concat demuxer must share one mp4 timescale. With
# `-c copy` it does not reconcile differing timebases: the per-file offset is
# applied in the wrong scale, so a part that disagrees with the first one lands
# at timestamps far too small, the muxer's monotonicity guard squashes its
# frames one tick apart, and the slot it should have filled becomes a hole.
# Sections are constant-rate at the grid fps and gaps inherit the source rate,
# so the two disagree by construction; 90000 is a multiple of every grid rate
# used here and of the common frame rates.
MUX_TIMESCALE = 90000

# Windows caps a process command line at 32767 characters. The concat *filter*
# names every part as its own input, so a long edit can outgrow that; parts are
# passed relative to the work directory to keep them short, and anything still
# over the limit is assembled in batches.
CMD_CHAR_LIMIT = 30000

# Bumped whenever the way a cached export part is built changes, so parts left
# over from an older build are re-encoded instead of silently reused.
PART_FORMAT = 4

# How far the export's audio may drift from its picture before it is pulled
# back into line. Re-anchoring costs a splice — a few ms of audio dropped or
# repeated, i.e. a faint click — so it is only worth it against a drift
# someone could actually notice, and lip-sync error stays unnoticed well past
# this. Parts rendered by the current version tile the source closely enough
# never to reach it.
AUDIO_RESYNC = 0.05


def _pick_grid_fps(rel_pts):
    if len(rel_pts) < 2:
        return 120
    durs = np.diff(np.asarray(rel_pts))
    durs = durs[durs > 1e-6]
    if len(durs) == 0:
        return 120
    fps = 1.0 / float(np.median(durs))
    grid = 100 if (abs(fps - 25) < 1.5 or abs(fps - 50) < 2.5) else 120
    while grid < fps * 1.8:
        grid *= 2
    return grid


def _audio_filter(segments, rate, target, label="1:a"):
    """filter_complex text: the listed pieces, concatenated, then padded and
    trimmed to exactly `target` seconds.

    `segments` is a list of ("src", a, b) source ranges — times relative to
    whatever the input was seeked to — and ("sil", seconds) silences. Leading
    gaps are filled with silence (sources sometimes have audio starting later
    than video) and the tail is padded, so the result is always exactly
    `target` long however short the source runs.
    """
    fix = (f"aresample=async=1:first_pts=0,"
           f"aformat=sample_rates={rate}:channel_layouts=stereo")
    segments = [g for g in segments
                if (g[2] - g[1] if g[0] == "src" else g[1]) > 1e-6]
    if not segments:
        segments = [("src", 0.0, target)]
    n_src = sum(1 for g in segments if g[0] == "src")
    # only split the input when something actually reads it: an unconnected
    # asplit output is a filter-graph error, not a no-op
    parts = ([f"[{label}]{fix},asplit={n_src}"
              + "".join(f"[a{i}]" for i in range(n_src)) + ";"]
             if n_src else [])
    labels = []
    k = 0
    for i, seg in enumerate(segments):
        if seg[0] == "src":
            _, a, b = seg
            # apad first: a range running past the end of the source still has
            # to come out its full length, or everything after it shifts
            parts.append(f"[a{k}]apad,atrim={a:.6f}:{max(a, b):.6f},"
                         f"asetpts=PTS-STARTPTS[g{i}];")
            k += 1
        else:
            parts.append(f"anullsrc=r={rate}:cl=stereo,"
                         f"atrim=0:{seg[1]:.6f}[g{i}];")
        labels.append(f"[g{i}]")
    parts.append("".join(labels) + f"concat=n={len(labels)}:v=0:a=1[cat];")
    parts.append(f"[cat]apad,atrim=0:{target:.6f},asetpts=PTS-STARTPTS[outa]")
    return "".join(parts)


def _section_timeline(sec):
    """How a section maps onto the source: (rel_pts, n_out, total, base, med).

    `rel_pts` are the prepared frame times rebased onto the section's first
    frame, which is what the render emits; `base` is how far that frame falls
    after the section's nominal start; `n_out` is how many of them the section
    actually shows (see render_section); `total` is its exact length.

    The renderer and the exporter both go through here so they cannot disagree
    about where a section begins and ends in the source -- the exporter needs
    that to place the export's audio.
    """
    rel = [float(t) for t in (sec["pts"] or [])]
    dur = sec["end"] - sec["start"]
    if not rel:
        return [], 0, dur, 0.0, 1.0 / 30
    deltas = np.diff(rel)
    ok = len(deltas) and (deltas > 1e-9).any()
    med = float(np.median(deltas[deltas > 1e-9])) if ok else 1.0 / 30
    # Frames at or past the section's end belong to the untouched span that
    # follows it: `-t` is enforced on decode timestamps, so the decode runs a
    # frame or so past the end, and the next span -- seeking to that same end
    # -- opens with that very frame. Showing it in both plays it twice and
    # starts everything after the section late.
    n_out = sum(1 for t in rel if t < dur - 1e-9) or len(rel)
    base = rel[0]
    out = [t - base for t in rel]
    total = out[n_out] if n_out < len(out) else out[-1] + med
    return out, n_out, total, base, med


def _section_cuts(sec, rel_pts, n_out):
    """Section-relative times at which an extension holds a frame."""
    return [rel_pts[i] for i, e in
            sorted(((int(k), v) for k, v in (sec.get("edits") or {}).items()))
            if e.get("extended") and not e.get("removed") and i < n_out]


def _span_segments(start, length, cuts, ext):
    """Audio segments covering one part: `length` seconds of output beginning
    at source time `start`, with `ext` of silence inserted at each cut.

    The source is consumed for length - len(cuts)*ext, since the silences make
    up the rest -- the same arithmetic the video does when it holds a frame.
    """
    segs = []
    prev = start
    for c in sorted(cuts):
        c = max(prev, min(start + length, start + c))
        segs.append(("src", prev, c))
        segs.append(("sil", ext))
        prev = c
    segs.append(("src", prev, start + length - ext * len(cuts)))
    return segs


def render_section(project, sid, source, out_path, job=None,
                   analyze_after=True):
    """Render one section with its edits applied.

    source: 'original' (full resolution) or 'preview' (fast, proxy-sized).
    Both decode from the ORIGINAL video so frame ordinals always match the
    prepared analysis/thumbnails — re-encoded proxies cannot be trusted to
    keep frame counts on messy VFR sources.
    """
    sec = project.section(sid)
    rc = project.render_config
    info = project.data["info"]
    edits = {int(k): v for k, v in sec.get("edits", {}).items()}
    ext = rc.extension_seconds

    src = project.video_path
    seek = sec["start"]
    dur = sec["end"] - sec["start"]
    has_audio = info["has_audio"]
    w, h = info["width"], info["height"]
    if source != "original" and h > rc.proxy_height:
        # preview render: decode scaled down for speed
        w = int(round(w * rc.proxy_height / h / 2)) * 2
        h = rc.proxy_height

    # the section's canonical (sanitized) timeline drives output timing —
    # identical to what the safety simulation used
    grid = _pick_grid_fps([float(t) for t in (sec["pts"] or [])])
    rel_pts, n_out, total, base, med_delta = _section_timeline(sec)
    cut_times = _section_cuts(sec, rel_pts, n_out)

    # Video first, on its own; the audio follows in a second pass, trimmed to
    # whatever the video actually came out as rather than to the length the
    # grid arithmetic says it should be. This audio is for the UI to play --
    # the export lays down its own (see _mux_audio) -- but the two streams
    # still have to agree, or the player drifts against the edit list.
    vid_path = (out_path + ".v.mp4") if has_audio else out_path
    cmd = [FFMPEG, "-hide_banner", "-nostdin", "-y",
           "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", f"{w}x{h}",
           "-r", str(grid), "-i", "pipe:0",
           "-map", "0:v:0", "-an",
           "-c:v", "libx264", "-preset", rc.preset, "-crf", str(rc.crf),
           "-pix_fmt", "yuv420p",
           "-video_track_timescale", str(MUX_TIMESCALE)]
    if not has_audio:
        cmd += ["-movflags", "+faststart"]
    cmd += [vid_path]

    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE,
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.PIPE,
                            creationflags=CREATE_NO_WINDOW)
    tail = []

    def _drain():
        for raw in proc.stderr:
            tail.append(raw.decode("utf-8", "replace"))
            if len(tail) > 30:
                tail.pop(0)

    threading.Thread(target=_drain, daemon=True).start()

    def prog(p, msg):
        if job:
            job.set_progress(p, msg)

    n_expected = len(rel_pts)
    # leading removed frames are backfilled: nothing is emitted until the
    # first kept frame arrives, which then fills the slots from t=0
    first_kept = next(
        (i for i in range(n_expected)
         if not edits.get(i, {}).get("removed")), 0)
    last_kept = None
    slot = 0
    offset = 0.0
    last_rel = 0.0
    i = -1
    try:
        it = ffio.iter_frames(src, w, h, start=seek, duration=dur,
                              cancel=(job.cancelled if job else None))
        pending = None  # frame bytes held until we know the next frame's time
        for i, (t, frame) in enumerate(it):
            if i >= n_out:
                continue        # the next part covers it (see n_out above)
            # canonical timeline: sanitized pts recorded at prepare time
            rel = rel_pts[i] if i < n_expected else last_rel + med_delta
            e = edits.get(i, {})
            if e.get("removed") and (last_kept is not None or i < first_kept):
                out_frame = last_kept          # may be None while backfilling
            else:
                out_frame = frame.tobytes()
                last_kept = out_frame
            # flush the previous frame now that its end time is known
            if pending is not None:
                slot = _emit(proc.stdin, pending, slot, rel + offset, grid)
            if e.get("extended") and not e.get("removed"):
                offset += ext
            if out_frame is not None:
                pending = out_frame
            last_rel = rel
            if job and i % 100 == 0:
                p = 0.9 * min(1.0, (i + 1) / max(1, n_expected))
                prog(p, f"rendering frame {i + 1}"
                     + (f"/{n_expected}" if n_expected else ""))
        # last frame runs to exactly the timeline end (matches the audio)
        if pending is not None:
            end = (total if n_out < n_expected
                   else max(total, last_rel + med_delta)) + offset
            slot = _emit(proc.stdin, pending, slot, end, grid)
        proc.stdin.close()
    except (BrokenPipeError, OSError):
        proc.wait()
        raise FFError("Encoder pipe closed early:\n" + "".join(tail[-10:]))
    proc.wait()
    if proc.returncode != 0:
        raise FFError("Encode failed:\n" + "".join(tail[-10:]))

    if has_audio:
        prog(0.9, "encoding audio")
        vdur = ffio.stream_duration(vid_path) or (total + ext * len(cut_times))
        try:
            ffio.run_ffmpeg(
                [FFMPEG, "-hide_banner", "-nostdin", "-y",
                 "-i", vid_path,
                 # seek to the first frame, not to the section boundary: the
                 # output's t=0 is that frame, so starting the audio at the
                 # boundary would run it `base` ahead of the picture
                 "-ss", f"{seek + base:.6f}", "-t", f"{dur:.6f}", "-i", src,
                 "-filter_complex",
                 _audio_filter(_span_segments(0.0, total, cut_times, ext),
                               rc.audio_rate, vdur),
                 "-map", "0:v:0", "-map", "[outa]",
                 "-c:v", "copy",
                 "-c:a", "aac", "-b:a", rc.audio_bitrate,
                 "-ar", str(rc.audio_rate), "-ac", "2",
                 "-video_track_timescale", str(MUX_TIMESCALE),
                 "-movflags", "+faststart", out_path],
                duration=vdur, cancel=(job.cancelled if job else None),
                progress=lambda p: prog(0.9 + 0.02 * p, "encoding audio"))
        finally:
            _unlink(vid_path)

    n_got = i + 1
    warn = None
    if n_expected and n_got != n_expected:
        warn = (f"Decoded {n_got} frames but section was prepared with "
                f"{n_expected}; edits are applied by ordinal. Re-prepare "
                "the section if this persists.")

    verdict = None
    if analyze_after:
        prog(0.92, "verifying rendered output")
        cfg = project.detector_config
        res = analyze_file(out_path, cfg)
        verdict = res.to_dict()
        verdict["profile"] = _profile_name(project)

    entry = {"path": out_path, "verdict": verdict, "warning": warn,
             "source": source, "grid_fps": grid,
             "edits_sig": project.edits_signature(sec.get("edits"))}
    with project.lock:
        sec["preview" if source != "original" else "render"] = entry
        project.save()
    prog(1.0, "done")
    return entry


def _unlink(path):
    try:
        os.remove(path)
    except OSError:
        pass


def _profile_name(project):
    from .config import profile_name
    return profile_name(project.data["detector"])


def _emit(pipe, frame_bytes, slot, end_t, grid):
    end_slot = int(round(end_t * grid))
    while slot < end_slot:
        pipe.write(frame_bytes)
        slot += 1
    return slot


# --- final assembly ----------------------------------------------------------

def _concat_escape(path):
    return path.replace("\\", "/").replace("'", "'\\''")


def _video_part(path, parts_dir, tag):
    """A video-only copy of `path` on the shared MUX_TIMESCALE.

    Every file handed to the concat demuxer has to agree about stream layout
    and timebase. Section renders carry their own audio because the UI plays
    them, but the export does not use it -- its audio is laid down in one
    piece at the end (see _mux_audio) -- and a part that still had an audio
    stream would put that stream's length back in charge of where the next
    part starts, which is what used to freeze the video at every section.

    Stream copy only, so a section never needs re-rendering for this.
    """
    if (ffio.video_timescale(path) == MUX_TIMESCALE
            and not ffio.has_audio(path)):
        return path
    out = os.path.join(parts_dir, f"vonly_{tag}.mp4")
    stale = (os.path.exists(out)
             and os.path.getmtime(out) < os.path.getmtime(path))
    if stale or not os.path.exists(out):
        tmp = out + ".part.mp4"
        ffio.run_ffmpeg([FFMPEG, "-hide_banner", "-nostdin", "-y",
                         "-i", path, "-map", "0:v:0", "-an", "-c:v", "copy",
                         "-video_track_timescale", str(MUX_TIMESCALE),
                         "-movflags", "+faststart", tmp])
        os.replace(tmp, out)
    return out


def _encode_gap(src, a, b, out_path, mode, rc, max_gap, med_delta, ts_min,
                progress=None, cancel=None):
    """Encode one untouched span [a, b) as a video-only export part. Returns
    its exact duration.

    Two passes: the video is encoded, then re-muxed by itself. The re-mux is
    what settles its final length -- a freshly encoded stream leaves its last
    frame with no duration at all, and only the re-mux gives it one -- and
    that length is what the concat demuxer will use to place everything after
    this part. It is a stream copy, so it costs no quality and little time.

    No audio: the export's audio is laid down in one piece at the end (see
    _mux_audio). A part carrying its own audio track was the original defect
    here, because the concat demuxer offsets the next part by whichever
    stream is longer.
    """
    tmp_v = out_path + ".v.mp4"
    tmp = out_path + ".part.mp4"
    span = b - a
    cmd = [FFMPEG, "-hide_banner", "-nostdin", "-y"]
    if a > ts_min + 1e-3:
        cmd += ["-ss", f"{a:.6f}"]
    cmd += ["-t", f"{span:.6f}", "-i", src, "-map", "0:v:0", "-an"]
    if mode == "smartcut":
        cmd += ["-c:v", "copy", "-avoid_negative_ts", "make_zero"]
    else:
        cmd += ["-vf", ffio.sanitize_setpts(max_gap, med_delta),
                "-fps_mode", "passthrough",
                "-c:v", "libx264", "-preset", rc.preset,
                "-crf", str(rc.crf), "-pix_fmt", "yuv420p"]
    cmd += ["-video_track_timescale", str(MUX_TIMESCALE), tmp_v]
    ffio.run_ffmpeg(cmd, duration=span, cancel=cancel,
                    progress=(lambda p: progress(0.9 * p)) if progress
                    else None)
    try:
        ffio.run_ffmpeg(
            [FFMPEG, "-hide_banner", "-nostdin", "-y", "-i", tmp_v,
             "-map", "0:v:0", "-an", "-c:v", "copy",
             "-video_track_timescale", str(MUX_TIMESCALE),
             "-movflags", "+faststart", tmp],
            duration=span, cancel=cancel,
            progress=(lambda p: progress(0.9 + 0.1 * p)) if progress else None)
    finally:
        _unlink(tmp_v)
    # only now is it a usable cache entry
    os.replace(tmp, out_path)
    return ffio.stream_duration(out_path) or span


def _part_anchors(plan, ts_min):
    """Source time of each part's first frame, plus the source time the export
    ends at.

    Read off the sections' prepared frame lists rather than off the encoded
    parts, so it stays right even when a section was rendered by an older
    version and came out slightly the wrong length. A section starts at its
    own first frame; the untouched span after it starts at the first frame the
    section does not show, which is the very frame the span will decode first.
    """
    anchors = []
    cursor = ts_min
    for item in plan:
        if item[0] == "section":
            sec = item[1]
            rel, n_out, total, base, _ = _section_timeline(sec)
            anchors.append(sec["start"] + base)
            cursor = sec["start"] + base + total
        else:
            anchors.append(cursor)
            cursor = item[2]
    return anchors


def _expected_durations(plan, anchors, ts_max, ext):
    """How long each part ought to come out: the stretch of source timeline it
    covers, plus the silence-and-held-frame its extensions add on purpose."""
    out = []
    for item, a, nxt in zip(plan, anchors, anchors[1:] + [ts_max]):
        n_cuts = 0
        if item[0] == "section":
            rel, n_out, _, _, _ = _section_timeline(item[1])
            n_cuts = len(_section_cuts(item[1], rel, n_out))
        out.append(nxt - a + ext * n_cuts)
    return out


def _merge_segments(segs):
    """Join source ranges that meet, and drop empty ones.

    This is what makes a run with nothing spliced into it come out as a single
    trim of the original track rather than a row of pieces butted together:
    no join, so nothing to hear, and no chance of atrim rounding a sample in
    or out at each seam.
    """
    out = []
    for g in segs:
        if (out and g[0] == "src" and out[-1][0] == "src"
                and abs(out[-1][2] - g[1]) < 1e-6):
            out[-1] = ("src", out[-1][1], g[2])
        else:
            out.append(g)
    return [g for g in out
            if (g[2] - g[1] if g[0] == "src" else g[1]) > 1e-6]


def _audio_segments(plan, anchors, durations, ext):
    """The whole export's audio as source ranges and silences.

    The source track is kept whole. The only thing deliberately spliced into
    it is `ext` of silence at each extension, matching the frame the picture
    holds there; everything else runs continuously, so a section edge has no
    join in the audio at all.

    Sync is still watched. A part can come out a shade shorter or longer than
    the stretch of source it covers -- a section's length is quantised to its
    render grid, worth a few ms -- and those add up across an edit. The
    running discrepancy is tracked, and the audio is re-anchored to the
    picture only if it would otherwise drift far enough to notice. A
    re-anchoring is a splice, so it is the lesser evil, not a free one; parts
    rendered by the current version never need it.
    """
    segs = []
    src = anchors[0] if anchors else 0.0
    for item, anchor, dur in zip(plan, anchors, durations):
        if dur <= 1e-6:      # nothing of this part reached the output
            continue
        if abs(src - anchor) > AUDIO_RESYNC:
            src = anchor
        cuts = []
        if item[0] == "section":
            sec = item[1]
            rel, n_out, _, _, _ = _section_timeline(sec)
            cuts = _section_cuts(sec, rel, n_out)
        segs += _span_segments(src, dur, cuts, ext)
        src += dur - ext * len(cuts)
    return _merge_segments(segs)


def _mux_audio(video_path, out_path, src, plan, anchors, durations, rc,
               parts_dir, progress=None, cancel=None):
    """Give the assembled (video-only) export its audio, in a single encode.

    Audio built per part carries an AAC boundary at every section edge --
    encoder priming, a truncated final frame, and whatever padding that part
    needed to match its own video -- and those are plainly audible as a bump
    in anything continuous, which is what music and dialogue are. Encoding the
    whole track in one pass puts no boundary anywhere: the pieces are joined
    as samples, before they ever reach the encoder.
    """
    vdur = ffio.stream_duration(video_path)
    start = anchors[0] if anchors else 0.0
    segs = [(g[0], g[1] - start, g[2] - start) if g[0] == "src" else g
            for g in _audio_segments(plan, anchors, durations,
                                     rc.extension_seconds)]
    # the graph names every piece, so it can outgrow a command line; ffmpeg
    # reads it from a file instead (see CMD_CHAR_LIMIT for the other case)
    script = os.path.join(parts_dir, "audio.filter")
    with open(script, "w", encoding="utf-8") as f:
        f.write(_audio_filter(segs, rc.audio_rate, vdur))
    cmd = [FFMPEG, "-hide_banner", "-nostdin", "-y", "-i", video_path]
    if start > 1e-3:
        cmd += ["-ss", f"{start:.6f}"]
    # read past the end rather than let `apad` invent the tail: padding is
    # silence, and silence in the middle of a scene is what a listener hears
    cmd += ["-t", f"{vdur + 1.0:.6f}", "-i", src,
            "-filter_complex_script", script,
            "-map", "0:v:0", "-map", "[outa]", "-c:v", "copy",
            "-c:a", "aac", "-b:a", rc.audio_bitrate,
            "-ar", str(rc.audio_rate), "-ac", "2",
            "-video_track_timescale", str(MUX_TIMESCALE),
            "-movflags", "+faststart", out_path]
    ffio.run_ffmpeg(cmd, duration=vdur, progress=progress, cancel=cancel)


def _span_plan(sections, ts_min, ts_max):
    """gap, section, gap, section, ..., gap"""
    plan = []
    cursor = ts_min
    for s in sections:
        if s["start"] - cursor > 0.02:
            plan.append(("gap", cursor, s["start"]))
        plan.append(("section", s))
        cursor = max(cursor, s["end"])
    if ts_max - cursor > 0.02:
        plan.append(("gap", cursor, ts_max))
    return plan


def _part_signature(mode, rc, max_gap, med_delta):
    return hashlib.sha1(json.dumps(
        [mode, rc.to_dict(), max_gap, round(med_delta, 6), MUX_TIMESCALE,
         PART_FORMAT],
        sort_keys=True).encode()).hexdigest()[:8]


def _filter_cmd(files, out_path, rc, cwd):
    """One ffmpeg run that decodes every part and concatenates them on the
    filter graph. Timestamps are rebuilt from scratch, so nothing here depends
    on the parts agreeing about timebases -- at the cost of re-encoding.
    Video only, like every other assembly step; the audio comes later."""
    cmd = [FFMPEG, "-hide_banner", "-nostdin", "-y"]
    for f in files:
        cmd += ["-i", os.path.relpath(f, cwd)]
    n = len(files)
    graph = "".join(f"[{i}:v]" for i in range(n))
    graph += f"concat=n={n}:v=1:a=0[outv]"
    cmd += ["-filter_complex", graph, "-map", "[outv]", "-an"]
    cmd += ["-c:v", "libx264", "-preset", rc.preset, "-crf", str(rc.crf),
            "-pix_fmt", "yuv420p", "-fps_mode", "passthrough",
            "-video_track_timescale", str(MUX_TIMESCALE),
            "-movflags", "+faststart", out_path]
    return cmd


def _cmd_chars(cmd):
    """Length of the command line the OS will see (quoted, space separated)."""
    return sum(len(a) + 3 for a in cmd)


def _filter_batches(files, out_path, rc, cwd):
    """Split `files` into the fewest runs whose commands each fit the limit."""
    batches, cur = [], []
    for f in files:
        trial = cur + [f]
        over = _cmd_chars(_filter_cmd(trial, out_path, rc,
                                      cwd)) > CMD_CHAR_LIMIT
        if cur and over:
            batches.append(cur)
            cur = [f]
        else:
            cur = trial
    if cur:
        batches.append(cur)
    return batches


def filter_assembly_estimate(project, mode="reencode"):
    """What a filter-assembled export would cost, for the UI to warn with: how
    many parts, how long the command would be, how many batches that forces.
    Derived from the plan, so it works before anything has been encoded."""
    rc = project.render_config
    info = project.data["info"]
    ts_min, ts_max = project.bounds
    sections = project.sections_sorted()
    src_idx = project.data.get("index") or {}
    med = float(src_idx.get("median_delta") or 0.0) or \
        1.0 / ((info.get("fps") or 30) or 30)
    sig = _part_signature(mode, rc, project.detector_config.max_frame_gap, med)
    parts_dir = os.path.join(project.workdir, "export_parts")
    files = []
    for item in _span_plan(sections, ts_min, ts_max):
        if item[0] == "section":
            r = item[1].get("render") or {}
            files.append(r.get("path")
                         or os.path.join(project.section_dir(item[1]["id"]),
                                         "render.mp4"))
        else:
            _, a, b = item
            files.append(os.path.join(parts_dir,
                                      f"gap_{a:.3f}_{b:.3f}_{sig}.mp4"))
    if not files:
        return {"parts": 0, "chars": 0, "limit": CMD_CHAR_LIMIT, "batches": 0,
                "sections": 0}
    out = os.path.join(project.workdir, "export.mp4")
    chars = _cmd_chars(_filter_cmd(files, out, rc, project.workdir))
    batches = _filter_batches(files, out, rc, project.workdir)
    return {"parts": len(files), "chars": chars, "limit": CMD_CHAR_LIMIT,
            "batches": len(batches), "sections": len(sections)}


def _assemble_copy(files, out_path, parts_dir, durations=None,
                   list_name="concat.txt"):
    """Stream-copy join through the concat demuxer: lossless and fast, but it
    needs every part to agree about timebases (see MUX_TIMESCALE).

    `durations` states how long each part should occupy, overriding what its
    container claims. That matters on variable-framerate sources: the muxer
    has to invent a duration for a part's final frame, and on VFR its guess is
    routinely tens of ms short of the interval the source actually had there,
    which would otherwise pull every later part forward. Stating the interval
    holds that last frame for exactly as long as the source did.

    Only ever used to lengthen a part. Claiming a part is shorter than it is
    would overlap it with the next one, and the muxer resolves that by
    throwing frames away.
    """
    list_path = os.path.join(parts_dir, list_name)
    with open(list_path, "w", encoding="utf-8") as f:
        for i, p in enumerate(files):
            f.write("file '" + _concat_escape(os.path.abspath(p)) + "'\n")
            if durations:
                f.write(f"duration {durations[i]:.6f}\n")
    ffio.run_ffmpeg([FFMPEG, "-hide_banner", "-nostdin", "-y",
                     "-f", "concat", "-safe", "0", "-i", list_path,
                     "-c", "copy", "-movflags", "+faststart", out_path])


def _assemble_filter(project, files, out_path, rc, parts_dir, part_sig,
                     job, prog, total_dur):
    """Filter join: re-encodes (one extra generation) but rebuilds every
    timestamp, so parts that disagree about timebases cannot desynchronise it.
    Splits into batches when one command would not fit the OS limit."""
    cwd = project.workdir
    cancel = job.cancelled if job else None
    batches = _filter_batches(files, out_path, rc, cwd)
    if len(batches) == 1:
        ffio.run_ffmpeg(_filter_cmd(files, out_path, rc, cwd),
                        duration=total_dur, cwd=cwd, cancel=cancel,
                        progress=lambda p: prog(0.9 + 0.08 * p,
                                                "assembling (filter)"))
        return []
    # Too many parts for one command: assemble each batch, then join the
    # batches. They leave the same encoder with the same settings and
    # timescale, so that join is a stream copy -- no second generation.
    outs = []
    for i, batch in enumerate(batches):
        if job and job.cancelled():
            return []
        bp = os.path.join(parts_dir, f"batch_{i:03d}_{part_sig}.mp4")
        base = 0.9 + 0.08 * (i / len(batches))
        ffio.run_ffmpeg(
            _filter_cmd(batch, bp, rc, cwd), cwd=cwd, cancel=cancel,
            progress=lambda p, b=base, n=i: prog(
                b + 0.08 * p / len(batches),
                f"assembling batch {n + 1}/{len(batches)}"))
        outs.append(bp)
    _assemble_copy(outs, out_path, parts_dir, list_name="concat_batches.txt")
    return [f"{len(files)} parts is more than one ffmpeg command can name, so "
            f"the export was assembled in {len(batches)} batches. The batches "
            "are joined by stream copy, so this costs no extra quality: the "
            "output still has exactly one re-encode generation."]


def export_video(project, out_path, mode="reencode", assembly="copy",
                 job=None):
    """Assemble rendered sections + untouched spans into the final video.

    mode 'reencode': every span re-encoded with the same settings as the
    sections (robust). mode 'smartcut': untouched spans are stream-copied at
    keyframe boundaries (fast; needs a well-behaved h264 source).

    assembly 'copy': the parts are joined by the concat demuxer without
    re-encoding -- fast and lossless, but it relies on every part agreeing
    about timebases and stream layout. assembly 'filter': the parts are
    decoded and concatenated on the filter graph, which rebuilds every
    timestamp and cannot be thrown off by a mismatched part, at the cost of
    one extra encode generation over the whole video.
    """
    rc = project.render_config
    info = project.data["info"]
    ts_min, ts_max = project.bounds
    sections = project.sections_sorted()
    if not sections:
        raise RuntimeError("No sections to export")
    missing = [s["id"] for s in sections
               if not (s.get("render") and s["render"].get("path")
                       and os.path.exists(s["render"]["path"]))]
    if missing:
        raise RuntimeError(
            "Sections not yet rendered at full resolution: #"
            + ", #".join(missing)
            + ". Use 'Render full-res' on each section first.")
    warnings = []
    stale = [s["id"] for s in sections if project.render_stale(s)]
    if stale:
        warnings.append(
            "Sections rendered before their latest edits (re-render to "
            "include them): #" + ", #".join(stale))
    if mode == "smartcut":
        if info["video_codec"] not in ("h264",):
            raise RuntimeError(
                f"smartcut needs an h264 source (got {info['video_codec']}); "
                "use re-encode mode")
        kfs = project.data.get("keyframes") or []
        unaligned = [
            s["id"] for s in sections
            if not (_near_keyframe(s["start"], kfs, ts_min)
                    and _near_keyframe(s["end"], kfs, ts_max))]
        if unaligned:
            raise RuntimeError(
                "smartcut needs keyframe-aligned section boundaries, but "
                "these were placed at exact timestamps: #"
                + ", #".join(unaligned) + ". Use re-encode mode instead.")

    # untouched spans are re-timed with the same rule the sections use, so a
    # source timestamp jump inside one is bridged instead of being copied into
    # the export (where it becomes a frozen hole with the audio running ahead)
    src_idx = project.data.get("index") or {}
    med_delta = float(src_idx.get("median_delta") or 0.0)
    if med_delta <= 0:
        med_delta = 1.0 / ((info.get("fps") or 30) or 30)
    max_gap = project.detector_config.max_frame_gap

    parts_dir = os.path.join(project.workdir, "export_parts")
    os.makedirs(parts_dir, exist_ok=True)
    # cached parts are only reusable if they were built the same way: mode and
    # encode settings are part of the name, and anything else is swept out so
    # a mode switch (or a half-written part from a cancelled run) can never be
    # silently reused
    part_sig = _part_signature(mode, rc, max_gap, med_delta)
    for f in os.listdir(parts_dir):
        if (f.startswith(("gap_", "vonly_", "batch_", "silent_"))
                and not f.endswith(f"_{part_sig}.mp4")):
            try:
                os.remove(os.path.join(parts_dir, f))
            except OSError:
                pass

    plan = _span_plan(sections, ts_min, ts_max)

    def prog(p, msg):
        if job:
            job.set_progress(p, msg)

    files = []
    n_gaps = sum(1 for p in plan if p[0] == "gap")
    done_gaps = 0
    bridged = 0.0
    for item in plan:
        if job and job.cancelled():
            return None
        if item[0] == "section":
            sec = item[1]
            # a section render keeps its audio so the UI can play it; the
            # export wants every part video-only and on the shared timescale
            files.append(_video_part(sec["render"]["path"], parts_dir,
                                     f"sec{sec['id']}_{part_sig}"))
            continue
        _, a, b = item
        part = os.path.join(parts_dir, f"gap_{a:.3f}_{b:.3f}_{part_sig}.mp4")
        if not os.path.exists(part):
            base_p = done_gaps / max(1, n_gaps) * 0.9
            _encode_gap(
                project.video_path, a, b, part, mode, rc, max_gap, med_delta,
                ts_min,
                progress=lambda p: prog(base_p + p * 0.9 / max(1, n_gaps),
                                        f"encoding untouched span "
                                        f"{done_gaps + 1}/{n_gaps}"),
                cancel=(job.cancelled if job else None))
        done_gaps += 1
        # How much of the span the sanitize rule swallowed, measured on the
        # part that was actually encoded rather than predicted from packets.
        # A span always misses the nominal length by up to a frame either way
        # (its edges are not on frame boundaries), so only a real shortfall
        # counts -- otherwise a long edit accumulates a warning out of noise.
        short = (b - a) - (ffio.stream_duration(part) or (b - a))
        if short > 2 * med_delta:
            bridged += short
        files.append(part)

    if bridged > 0.5:
        warnings.append(
            f"{bridged:.1f}s of source timestamp anomalies were bridged in "
            "the untouched spans (this video's timestamps jump); the export "
            "is correspondingly shorter than the source's nominal duration.")
    elif mode == "smartcut" and (project.data.get("index") or {}).get(
            "discontinuities"):
        warnings.append(
            "The source has timestamp discontinuities and smart-cut copies "
            "untouched spans as-is, so they carry through to the export. "
            "Re-encode mode repairs them.")

    anchors = _part_anchors(plan, ts_min)
    encoded = [ffio.stream_duration(f) for f in files]
    wanted = _expected_durations(plan, anchors, ts_max, rc.extension_seconds)
    # What each part will actually occupy. A part that came out short of the
    # stretch of source it covers is held to that stretch by the concat list
    # (see _assemble_copy) -- routine on VFR, where the muxer has to guess the
    # last frame's duration. A part that came out *long* cannot be fixed here;
    # it is used as-is and warned about below.
    stated = [max(w, e) for w, e in zip(wanted, encoded)]

    prog(0.9, "concatenating")
    # the parts are joined picture-first; the audio goes on afterwards, in one
    # continuous piece, so no join lands in the middle of an audio stream
    silent = (os.path.join(parts_dir, f"silent_{part_sig}.mp4")
              if info["has_audio"] else out_path)
    if assembly == "filter":
        # the filter join rebuilds every timestamp from the decoded frames, so
        # there is no per-file duration to state
        stated = encoded
        warnings += _assemble_filter(project, files, silent, rc, parts_dir,
                                     part_sig, job, prog, ts_max - ts_min)
        if job and job.cancelled():
            return None
    else:
        _assemble_copy(files, silent, parts_dir, stated)

    # A section rendered by an older version comes out a frame or two longer
    # than the source it covers, which is where a stubbornly-too-long export
    # comes from. The audio copes (it re-anchors itself), but the picture can
    # only be put right by re-rendering.
    off = sum(e - w for e, w in zip(encoded, wanted) if e > w)
    if off > 0.25:
        warnings.append(
            f"The parts run {off:+.2f}s longer than the source's own timing. "
            "Sections rendered by an older version of the tool are slightly "
            "too long; re-render them ('Render full-res') and export again "
            "to get the timing exact.")

    if info["has_audio"]:
        prog(0.95, "adding audio")
        try:
            _mux_audio(silent, out_path, project.video_path, plan, anchors,
                       stated, rc, parts_dir,
                       progress=lambda p: prog(0.95 + 0.03 * p,
                                               "adding audio"),
                       cancel=(job.cancelled if job else None))
        finally:
            _unlink(silent)

    prog(0.98, "sanity-checking output timing")
    warnings += _timing_sanity(out_path, files, sum(stated))

    entry = {"path": out_path, "mode": mode, "assembly": assembly,
             "verify": None, "warnings": warnings}
    with project.lock:
        project.data["export"] = entry
        project.save()
    prog(1.0, "done")
    return entry


def _near_keyframe(t, keyframes, edge, tol=0.005):
    if abs(t - edge) <= tol:
        return True
    import bisect
    i = bisect.bisect_left(keyframes, t - tol)
    return i < len(keyframes) and abs(keyframes[i] - t) <= tol


def _timing_sanity(path, part_files, expected):
    """Catch broken concat output (frozen-video holes): the final file's
    video span must be the length the parts were laid out to occupy, with no
    pts gaps beyond the ones the parts already had."""
    warnings = []
    try:
        part_holes = 0
        for f in part_files:
            part_holes += ffio.index_video(f).get("holes", 0)
        idx = ffio.index_video(path)
    except Exception as e:  # noqa: BLE001 - advisory check only
        return [f"Could not sanity-check output timing: {e}"]
    got = idx["ts_max"] - idx["ts_min"]
    # tight on purpose: the failure this exists to catch (parts joined by
    # their longer stream) adds only a second or two per part, and a
    # percentage-of-runtime tolerance is wide enough to swallow all of it
    if expected and abs(got - expected) > max(0.25, expected * 0.002):
        warnings.append(
            f"Output video span {got:.1f}s differs from the sum of its "
            f"parts {expected:.1f}s — concatenation misaligned the streams; "
            "check the file for frozen spans.")
    new_holes = idx.get("holes", 0) - part_holes
    if new_holes > 0:
        warnings.append(
            f"Output has {new_holes} video gap(s) that none of its parts "
            "have — the join inserted them, and the video freezes there. "
            "Re-export with 'filter' assembly if this persists.")
    if idx["discontinuities"]:
        warnings.append(
            f"Output has {idx['discontinuities']} timestamp gap(s) >5s — "
            "players may freeze there; check the file.")
    return warnings


def verify_file(project, path, job=None):
    """Full detector pass over an arbitrary rendered file, using the
    project's currently selected detector profile."""
    cfg = project.detector_config
    res = analyze_file(
        path, cfg,
        progress=(lambda p: job.set_progress(p, "verifying")) if job else None,
        cancel=(job.cancelled if job else None))
    out = res.to_dict()
    out["profile"] = _profile_name(project)
    return out
