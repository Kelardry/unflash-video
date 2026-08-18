"""Rendering: apply frame edits to a section, and assemble the final video.

VFR handling: edits never re-time surviving frames (removed frames are
*replaced*, not dropped), so the output keeps the source's exact frame timing.
Frames are emitted onto a fine constant-rate grid (>= 2x the source rate, so
placement error is bounded by half a grid slot, a few ms, and never
accumulates). Extensions insert `extension_seconds` of held frame + silence,
shifting everything after them equally in video and audio.
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


def _audio_graph(cut_times, total, ext, rate):
    """filter_complex for input [1:a]: original audio with `ext` seconds of
    silence inserted at each cut time (section-relative).

    The output duration is forced to exactly total + len(cuts)*ext: leading
    gaps are filled with silence (sources sometimes have audio starting later
    than video), the tail is padded, and the result is trimmed exactly. If
    audio and video durations differ between parts, the final concat offsets
    subsequent parts by the longer stream and the video freezes — so exact
    equality here is load-bearing.
    """
    fix = (f"aresample=async=1:first_pts=0,"
           f"aformat=sample_rates={rate}:channel_layouts=stereo")
    cuts = sorted(max(0.0, min(c, total)) for c in cut_times)
    expected = total + ext * len(cuts)
    tail = f"apad,atrim=0:{expected:.6f},asetpts=PTS-STARTPTS[outa]"
    if not cuts:
        return f"[1:a]{fix},{tail}"
    nseg = len(cuts) + 1
    parts = [f"[1:a]{fix},asplit={nseg}" +
             "".join(f"[a{i}]" for i in range(nseg)) + ";"]
    labels = []
    prev = 0.0
    for i, c in enumerate(cuts):
        c = max(prev, c)
        parts.append(f"[a{i}]apad,atrim={prev:.6f}:{c:.6f},"
                     f"asetpts=PTS-STARTPTS[s{i}];")
        labels.append(f"[s{i}]")
        parts.append(f"anullsrc=r={rate}:cl=stereo,atrim=0:{ext:.3f}[sil{i}];")
        labels.append(f"[sil{i}]")
        prev = c
    i = len(cuts)
    parts.append(f"[a{i}]atrim=start={prev:.6f},asetpts=PTS-STARTPTS[s{i}];")
    labels.append(f"[s{i}]")
    parts.append("".join(labels) + f"concat=n={len(labels)}:v=0:a=1[cat];")
    parts.append(f"[cat]{tail}")
    return "".join(parts)


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
    rel_pts = [float(t) for t in (sec["pts"] or [])]
    grid = _pick_grid_fps(rel_pts)
    total = (sec["end"] - sec["start"])
    if rel_pts:
        deltas = np.diff(rel_pts)
        med_delta = float(np.median(deltas[deltas > 1e-9])) \
            if len(deltas) and (deltas > 1e-9).any() else 1.0 / 30
        # both streams target exactly the sanitized timeline length
        total = rel_pts[-1] + med_delta
    else:
        med_delta = 1.0 / 30

    # audio cut points: where extensions insert silence
    cut_times = [rel_pts[i] for i, e in sorted(edits.items())
                 if e.get("extended") and not e.get("removed")
                 and i < len(rel_pts)]

    cmd = [FFMPEG, "-hide_banner", "-nostdin", "-y",
           "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", f"{w}x{h}",
           "-r", str(grid), "-i", "pipe:0"]
    if has_audio:
        cmd += ["-ss", f"{seek:.6f}", "-t", f"{dur:.6f}", "-i", src]
        cmd += ["-filter_complex",
                _audio_graph(cut_times, total, ext, rc.audio_rate),
                "-map", "0:v:0", "-map", "[outa]",
                "-c:a", "aac", "-b:a", rc.audio_bitrate,
                "-ar", str(rc.audio_rate), "-ac", "2"]
    else:
        cmd += ["-map", "0:v:0", "-an"]
    cmd += ["-c:v", "libx264", "-preset", rc.preset, "-crf", str(rc.crf),
            "-pix_fmt", "yuv420p",
            "-video_track_timescale", str(MUX_TIMESCALE),
            "-movflags", "+faststart", out_path]

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
            end = max(total, last_rel + med_delta) + offset
            slot = _emit(proc.stdin, pending, slot, end, grid)
        proc.stdin.close()
    except (BrokenPipeError, OSError):
        proc.wait()
        raise FFError("Encoder pipe closed early:\n" + "".join(tail[-10:]))
    proc.wait()
    if proc.returncode != 0:
        raise FFError("Encode failed:\n" + "".join(tail[-10:]))

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


def _normalized_part(path, parts_dir, tag, rc, has_audio):
    """A copy of `path` that matches every other part: video on the shared
    MUX_TIMESCALE, audio at the project's sample rate.

    Parts rendered before those were pinned (or under different settings) would
    otherwise mismatch, and the concat demuxer mis-stamps whatever disagrees
    with the first file. Video is always a stream copy, so sections never need
    re-rendering; only a stale audio rate costs a (cheap) re-encode.
    """
    fix_video = ffio.video_timescale(path) != MUX_TIMESCALE
    fix_audio = has_audio and ffio.probe(path)["audio_rate"] != rc.audio_rate
    if not (fix_video or fix_audio):
        return path
    out = os.path.join(parts_dir, f"norm_{tag}.mp4")
    stale = (os.path.exists(out)
             and os.path.getmtime(out) < os.path.getmtime(path))
    if stale or not os.path.exists(out):
        tmp = out + ".part.mp4"
        cmd = [FFMPEG, "-hide_banner", "-nostdin", "-y",
               "-i", path, "-map", "0", "-c:v", "copy"]
        if fix_audio:
            cmd += ["-c:a", "aac", "-b:a", rc.audio_bitrate,
                    "-ar", str(rc.audio_rate), "-ac", "2"]
        else:
            cmd += ["-c:a", "copy"]
        cmd += ["-video_track_timescale", str(MUX_TIMESCALE),
                "-movflags", "+faststart", tmp]
        ffio.run_ffmpeg(cmd)
        os.replace(tmp, out)
    return out


def _sanitized_span(src, a, b, max_gap, med):
    """(duration, n_bridged) of source span [a, b) once its timestamps have
    been through the sanitize rule -- i.e. what ffio.sanitize_setpts will
    produce for the same span. Packet-level probe, no decoding.

    Kept in lockstep with the filter: the audio in each part is trimmed to
    this length, and if the two disagree the concat leaves a hole.
    """
    pts = ffio.span_video_pts(src, a, b)
    if not pts:
        return b - a, 0
    rel, fixed = ffio.sanitize_deltas([t - pts[0] for t in pts], max_gap, med)
    return rel[-1] + med, fixed


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
        [mode, rc.to_dict(), max_gap, round(med_delta, 6), MUX_TIMESCALE],
        sort_keys=True).encode()).hexdigest()[:8]


def _filter_cmd(files, out_path, rc, cwd, has_audio):
    """One ffmpeg run that decodes every part and concatenates them on the
    filter graph. Timestamps are rebuilt from scratch, so nothing here depends
    on the parts agreeing about timebases -- at the cost of re-encoding."""
    cmd = [FFMPEG, "-hide_banner", "-nostdin", "-y"]
    for f in files:
        cmd += ["-i", os.path.relpath(f, cwd)]
    n = len(files)
    if has_audio:
        graph = "".join(f"[{i}:v][{i}:a]" for i in range(n))
        graph += f"concat=n={n}:v=1:a=1[outv][outa]"
        maps = ["-map", "[outv]", "-map", "[outa]",
                "-c:a", "aac", "-b:a", rc.audio_bitrate,
                "-ar", str(rc.audio_rate), "-ac", "2"]
    else:
        graph = "".join(f"[{i}:v]" for i in range(n))
        graph += f"concat=n={n}:v=1:a=0[outv]"
        maps = ["-map", "[outv]", "-an"]
    cmd += ["-filter_complex", graph] + maps
    cmd += ["-c:v", "libx264", "-preset", rc.preset, "-crf", str(rc.crf),
            "-pix_fmt", "yuv420p", "-fps_mode", "passthrough",
            "-video_track_timescale", str(MUX_TIMESCALE),
            "-movflags", "+faststart", out_path]
    return cmd


def _cmd_chars(cmd):
    """Length of the command line the OS will see (quoted, space separated)."""
    return sum(len(a) + 3 for a in cmd)


def _filter_batches(files, out_path, rc, cwd, has_audio):
    """Split `files` into the fewest runs whose commands each fit the limit."""
    batches, cur = [], []
    for f in files:
        trial = cur + [f]
        over = _cmd_chars(_filter_cmd(trial, out_path, rc, cwd,
                                      has_audio)) > CMD_CHAR_LIMIT
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
    chars = _cmd_chars(_filter_cmd(files, out, rc, project.workdir,
                                   info["has_audio"]))
    batches = _filter_batches(files, out, rc, project.workdir,
                              info["has_audio"])
    return {"parts": len(files), "chars": chars, "limit": CMD_CHAR_LIMIT,
            "batches": len(batches), "sections": len(sections)}


def _assemble_copy(files, out_path, parts_dir, list_name="concat.txt"):
    """Stream-copy join through the concat demuxer: lossless and fast, but it
    needs every part to agree about timebases (see MUX_TIMESCALE)."""
    list_path = os.path.join(parts_dir, list_name)
    with open(list_path, "w", encoding="utf-8") as f:
        for p in files:
            f.write("file '" + _concat_escape(os.path.abspath(p)) + "'\n")
    ffio.run_ffmpeg([FFMPEG, "-hide_banner", "-nostdin", "-y",
                     "-f", "concat", "-safe", "0", "-i", list_path,
                     "-c", "copy", "-movflags", "+faststart", out_path])


def _assemble_filter(project, files, out_path, rc, parts_dir, part_sig,
                     job, prog, total_dur):
    """Filter join: re-encodes (one extra generation) but rebuilds every
    timestamp, so parts that disagree about timebases cannot desynchronise it.
    Splits into batches when one command would not fit the OS limit."""
    cwd = project.workdir
    has_audio = project.data["info"]["has_audio"]
    cancel = job.cancelled if job else None
    batches = _filter_batches(files, out_path, rc, cwd, has_audio)
    if len(batches) == 1:
        ffio.run_ffmpeg(_filter_cmd(files, out_path, rc, cwd, has_audio),
                        duration=total_dur, cwd=cwd, cancel=cancel,
                        progress=lambda p: prog(0.9 + 0.08 * p,
                                                "assembling (filter)"))
        return []
    # Too many parts for one command: assemble each batch, then join the
    # batches. They leave the same encoder with the same settings, timescale
    # and sample rate, so that join is a stream copy -- no second generation.
    outs = []
    for i, batch in enumerate(batches):
        if job and job.cancelled():
            return []
        bp = os.path.join(parts_dir, f"batch_{i:03d}_{part_sig}.mp4")
        base = 0.9 + 0.08 * (i / len(batches))
        ffio.run_ffmpeg(
            _filter_cmd(batch, bp, rc, cwd, has_audio), cwd=cwd, cancel=cancel,
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
        if (f.startswith(("gap_", "norm_", "batch_"))
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
    n_bridged = 0
    for item in plan:
        if job and job.cancelled():
            return None
        if item[0] == "section":
            sec = item[1]
            path = sec["render"]["path"]
            if assembly != "filter":
                # the filter join decodes everything, so a part that disagrees
                # about timescale or sample rate costs it nothing
                path = _normalized_part(path, parts_dir,
                                        f"sec{sec['id']}_{part_sig}",
                                        rc, info["has_audio"])
            files.append(path)
            continue
        _, a, b = item
        # the span's length *after* bridging any source timestamp anomalies —
        # what the re-encoded video will actually be, and therefore what the
        # audio has to be trimmed to
        if mode == "smartcut":
            span, fixed = b - a, 0      # stream copy keeps the source timing
        else:
            span, fixed = _sanitized_span(project.video_path, a, b,
                                          max_gap, med_delta)
        n_bridged += fixed
        part = os.path.join(parts_dir, f"gap_{a:.3f}_{b:.3f}_{part_sig}.mp4")
        if not os.path.exists(part):
            tmp = part + ".part.mp4"
            cmd = [FFMPEG, "-hide_banner", "-nostdin", "-y"]
            if a > ts_min + 1e-3:
                cmd += ["-ss", f"{a:.6f}"]
            cmd += ["-t", f"{b - a:.6f}",
                    "-i", project.video_path,
                    "-map", "0:v:0", "-map", "0:a:0?"]
            if mode == "smartcut":
                cmd += ["-c:v", "copy", "-avoid_negative_ts", "make_zero"]
            else:
                cmd += ["-vf", ffio.sanitize_setpts(max_gap, med_delta),
                        "-fps_mode", "passthrough",
                        "-c:v", "libx264", "-preset", rc.preset,
                        "-crf", str(rc.crf), "-pix_fmt", "yuv420p"]
            if info["has_audio"]:
                # force audio duration == video duration; mismatched stream
                # lengths make the final concat leave frozen-video holes
                cmd += ["-af",
                        ("aresample=async=1:first_pts=0,"
                         f"aformat=sample_rates={rc.audio_rate}:"
                         "channel_layouts=stereo,"
                         f"apad,atrim=0:{span:.6f}"),
                        "-c:a", "aac", "-b:a", rc.audio_bitrate,
                        "-ar", str(rc.audio_rate), "-ac", "2"]
            cmd += ["-video_track_timescale", str(MUX_TIMESCALE),
                    "-movflags", "+faststart", tmp]
            base_p = done_gaps / max(1, n_gaps) * 0.9
            ffio.run_ffmpeg(
                cmd, duration=span,
                progress=lambda p: prog(base_p + p * 0.9 / max(1, n_gaps),
                                        f"encoding untouched span "
                                        f"{done_gaps + 1}/{n_gaps}"),
                cancel=(job.cancelled if job else None))
            # only now is it a usable cache entry
            os.replace(tmp, part)
        done_gaps += 1
        if assembly != "filter":
            part = _normalized_part(part, parts_dir,
                                    f"gap{done_gaps}_{part_sig}",
                                    rc, info["has_audio"])
        files.append(part)

    if n_bridged:
        warnings.append(
            f"{n_bridged} source timestamp anomalies were bridged in the "
            "untouched spans (this video's timestamps jump); the export is "
            "correspondingly shorter than the source's nominal duration.")
    elif mode == "smartcut" and (project.data.get("index") or {}).get(
            "discontinuities"):
        warnings.append(
            "The source has timestamp discontinuities and smart-cut copies "
            "untouched spans as-is, so they carry through to the export. "
            "Re-encode mode repairs them.")

    prog(0.9, "concatenating")
    if assembly == "filter":
        warnings += _assemble_filter(project, files, out_path, rc, parts_dir,
                                     part_sig, job, prog, ts_max - ts_min)
        if job and job.cancelled():
            return None
    else:
        _assemble_copy(files, out_path, parts_dir)

    prog(0.98, "sanity-checking output timing")
    warnings += _timing_sanity(out_path, files)

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


def _timing_sanity(path, part_files):
    """Catch broken concat output (frozen-video holes): the final file's
    video span must equal the sum of the actual parts, with no big pts gaps."""
    warnings = []
    try:
        expected = 0.0
        for f in part_files:
            pidx = ffio.index_video(f)
            expected += pidx["ts_max"] - pidx["ts_min"]
        idx = ffio.index_video(path)
    except Exception as e:  # noqa: BLE001 - advisory check only
        return [f"Could not sanity-check output timing: {e}"]
    got = idx["ts_max"] - idx["ts_min"]
    if expected and abs(got - expected) > max(0.5, expected * 0.02):
        warnings.append(
            f"Output video span {got:.1f}s differs from the sum of its "
            f"parts {expected:.1f}s — concatenation misaligned the streams; "
            "check the file for frozen spans.")
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
