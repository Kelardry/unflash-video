"""ffmpeg/ffprobe helpers.

Frame identity model: a *section* is defined by (start, duration) and always
decoded with the same input-seek command (`-ss start -i file -t duration`),
which is deterministic — the Nth decoded frame of a section is the same frame
regardless of output scale. Per-frame presentation timestamps come from the
showinfo filter, so variable-framerate sources are handled exactly.
"""

import json
import os
import re
import subprocess
import threading
from bisect import bisect_left, bisect_right

import numpy as np

FFMPEG = "ffmpeg"
FFPROBE = "ffprobe"

CREATE_NO_WINDOW = 0x08000000 if os.name == "nt" else 0

_PTS_RE = re.compile(r"pts_time:\s*(-?[0-9]+(?:\.[0-9]+)?(?:[eE][-+]?[0-9]+)?)")

# a video timestamp step this large is a hole, not just an uneven frame
HOLE_SECONDS = 0.25


class FFError(RuntimeError):
    pass


def _run(cmd, **kw):
    return subprocess.run(
        cmd, capture_output=True, text=True, creationflags=CREATE_NO_WINDOW, **kw
    )


def probe(path):
    """Return a dict of the video's key properties."""
    r = _run([FFPROBE, "-v", "error", "-print_format", "json",
              "-show_format", "-show_streams", path])
    if r.returncode != 0:
        raise FFError(f"ffprobe failed: {r.stderr.strip()[:500]}")
    data = json.loads(r.stdout)
    vstream = next((s for s in data.get("streams", [])
                    if s.get("codec_type") == "video"), None)
    if vstream is None:
        raise FFError("No video stream found")
    astream = next((s for s in data.get("streams", [])
                    if s.get("codec_type") == "audio"), None)

    def _fps(s):
        try:
            num, den = s.get("avg_frame_rate", "0/1").split("/")
            num, den = float(num), float(den)
            if den and num:
                return num / den
        except (ValueError, ZeroDivisionError):
            pass
        try:
            num, den = s.get("r_frame_rate", "0/1").split("/")
            return float(num) / float(den) if float(den) else 0.0
        except (ValueError, ZeroDivisionError):
            return 0.0

    duration = 0.0
    for src in (data.get("format", {}).get("duration"), vstream.get("duration")):
        try:
            duration = float(src)
            break
        except (TypeError, ValueError):
            continue

    return {
        "path": os.path.abspath(path),
        "duration": duration,
        "width": int(vstream["width"]),
        "height": int(vstream["height"]),
        "fps": _fps(vstream),
        "video_codec": vstream.get("codec_name", ""),
        "pix_fmt": vstream.get("pix_fmt", ""),
        "has_audio": astream is not None,
        "audio_codec": astream.get("codec_name", "") if astream else "",
        "audio_rate": int(astream.get("sample_rate", 0) or 0) if astream else 0,
        "audio_channels": int(astream.get("channels", 0) or 0) if astream else 0,
        "container": data.get("format", {}).get("format_name", ""),
        "size_bytes": int(data.get("format", {}).get("size", 0) or 0),
    }


def index_video(path, progress=None):
    """One packet pass (no decode): keyframe times, the *real* timeline bounds
    (native pts can start negative or contain jumps on janky recordings — the
    container's duration metadata is not trusted), and discontinuities."""
    cmd = [FFPROBE, "-v", "error", "-select_streams", "v:0",
           "-show_entries", "packet=pts_time,dts_time,flags",
           "-of", "csv=p=0", path]
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL,
                            text=True, creationflags=CREATE_NO_WINDOW)
    keyframes = []
    all_pts = []
    n = 0
    for line in proc.stdout:
        n += 1
        parts = line.strip().split(",")
        if len(parts) < 3:
            continue
        pts_s, dts_s, flags = parts[0], parts[1], parts[2]
        t = None
        for v in (pts_s, dts_s):
            try:
                t = float(v)
                break
            except ValueError:
                continue
        if t is None:
            continue
        all_pts.append(t)
        if "K" in flags:
            keyframes.append(t)
        if progress and n % 50000 == 0:
            progress(n)
    proc.stdout.close()
    proc.wait()
    keyframes.sort()
    if not all_pts:
        return {"keyframes": [], "ts_min": 0.0, "ts_max": 0.0,
                "median_delta": 1.0 / 30, "n_packets": 0,
                "discontinuities": 0, "holes": 0}
    all_pts.sort()
    arr = np.asarray(all_pts)
    deltas = np.diff(arr)
    deltas = deltas[deltas > 1e-9]
    med = float(np.median(deltas)) if len(deltas) else 1.0 / 30
    disc = int(np.count_nonzero(deltas > 5.0)) if len(deltas) else 0
    # anything an eye would read as a freeze rather than a stutter; used to
    # tell an export's own gaps apart from ones its parts already had
    holes = int(np.count_nonzero(deltas > HOLE_SECONDS)) if len(deltas) else 0
    return {
        "keyframes": keyframes,
        "ts_min": float(arr[0]),
        "ts_max": float(arr[-1] + med),
        "median_delta": med,
        "n_packets": len(all_pts),
        "discontinuities": disc,
        "holes": holes,
    }


def snap_to_keyframes(keyframes, start, end, bounds):
    """Snap [start, end] outward to keyframes, clamped into the video's real
    timeline bounds (ts_min, ts_max)."""
    ts_min, ts_max = bounds
    start = max(ts_min, min(start, ts_max))
    end = max(ts_min, min(end, ts_max))
    if keyframes:
        i = bisect_right(keyframes, start + 1e-6) - 1
        if i >= 0:
            start = keyframes[i]
        j = bisect_left(keyframes, end - 1e-6)
        if j < len(keyframes):
            end = keyframes[j]
        else:
            end = ts_max
    start = max(ts_min, min(start, ts_max))
    end = max(ts_min, min(end, ts_max))
    if end <= start:
        end = ts_max
    return start, end


def sanitize_deltas(times, max_gap, fallback=1.0 / 30):
    """Make a frame-time list strictly sane: non-positive deltas and deltas
    beyond max_gap (source timestamp discontinuities) are replaced with the
    running median delta. Returns (new_times, n_fixed).

    Deltas are measured input-to-input. Measuring them against the *output*
    instead would make every frame after the first bridged anomaly look like
    another jump (the output trails the input by the bridged amount from then
    on), silently flattening the rest of the timeline to the median delta.
    """
    if not len(times):
        return list(times), 0
    out = [float(times[0])]
    prev_in = float(times[0])
    recent = []
    fixed = 0
    for t in times[1:]:
        t = float(t)
        d = t - prev_in
        prev_in = t
        if d <= 0 or d > max_gap:
            d = float(np.median(recent)) if recent else fallback
            fixed += 1
        else:
            recent.append(d)
            if len(recent) > 120:
                recent.pop(0)
        out.append(out[-1] + d)
    return out, fixed


def sanitize_setpts(max_gap, median_delta):
    """A `setpts` expression applying the same rule as sanitize_deltas, for
    spans that are re-encoded by ffmpeg directly instead of frame-by-frame.

    The first frame is pinned to 0 and each following frame advances by its
    own input delta, except non-positive deltas and deltas beyond max_gap,
    which advance by median_delta instead. Unlike sanitize_deltas this uses a
    fixed median (the source's, from the packet index) rather than a running
    one -- filter expressions have no history beyond the previous frame.
    """
    d = "PTS-PREV_INPTS"
    # commas inside filter arguments must be escaped for the graph parser
    return ("setpts="
            r"if(eq(N\,0)\,0\,PREV_OUTPTS+"
            rf"if(lte({d}\,0)+gt({d}\,{max_gap:.6f}/TB)\,"
            rf"{median_delta:.6f}/TB\,{d}))")


def has_audio(path):
    """True if the file carries an audio stream."""
    r = _run([FFPROBE, "-v", "error", "-select_streams", "a",
              "-show_entries", "stream=index", "-of", "csv=p=0", path])
    return r.returncode == 0 and bool(r.stdout.strip())


def video_timescale(path):
    """The file's video stream timescale (the denominator of its time_base),
    or 0 if it can't be read."""
    r = _run([FFPROBE, "-v", "error", "-select_streams", "v:0",
              "-show_entries", "stream=time_base", "-of", "csv=p=0", path])
    if r.returncode != 0:
        return 0
    tb = (r.stdout.strip().splitlines() or [""])[0].strip()
    if "/" not in tb:
        return 0
    try:
        num, den = tb.split("/")
        return int(den) if int(num) == 1 else 0
    except ValueError:
        return 0


def stream_duration(path, kind="v"):
    """Exact duration in seconds of the file's first video (or audio) stream.

    Read as duration_ts * time_base -- the muxer's own number -- rather than
    the rounded decimal ffprobe prints, because parts are joined by adding up
    these durations and a millisecond of slop per part becomes a visible hole
    in the export.
    """
    r = _run([FFPROBE, "-v", "error", "-select_streams", f"{kind}:0",
              "-show_entries", "stream=duration_ts,time_base,duration",
              "-print_format", "json", path])
    if r.returncode != 0:
        return 0.0
    try:
        st = (json.loads(r.stdout).get("streams") or [])[0]
    except (ValueError, IndexError, AttributeError):
        return 0.0
    ts, tb = st.get("duration_ts"), str(st.get("time_base") or "")
    if ts and "/" in tb:
        try:
            num, den = tb.split("/")
            return float(ts) * float(num) / float(den)
        except (ValueError, ZeroDivisionError):
            pass
    try:
        return float(st.get("duration"))
    except (TypeError, ValueError):
        return 0.0


# NB: there is deliberately no "what will this span's timeline look like"
# helper here. `-read_intervals` does not return the frame set ffmpeg decodes
# from the same seek (it stops on decode order, and the count drifts), so a
# span's length can only be found by encoding it and measuring — see
# render._encode_gap.


def iter_frames(path, out_w, out_h, start=None, duration=None,
                cancel=None):
    """Yield (pts_seconds, HxWx3 uint8 RGB array) for every frame.

    pts is absolute-ish: relative to the seek point plus `start`, taken from
    the showinfo filter so VFR timing is exact.
    """
    cmd = [FFMPEG, "-hide_banner", "-nostdin", "-v", "info"]
    if start is not None and start > 0:
        cmd += ["-ss", f"{start:.6f}"]
    if duration is not None:
        cmd += ["-t", f"{duration:.6f}"]   # input option: limits demuxing
    cmd += ["-i", path]
    vf = "showinfo" if (out_w is None) else f"scale={out_w}:{out_h}:flags=area,showinfo"
    # -fps_mode passthrough is essential: without it ffmpeg duplicates/drops
    # frames on VFR sources AFTER the showinfo filter, so piped frames would
    # no longer match the timestamp lines 1:1
    cmd += ["-map", "0:v:0", "-an", "-sn", "-dn",
            "-vf", vf, "-fps_mode", "passthrough",
            "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"]

    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            creationflags=CREATE_NO_WINDOW)
    pts_list = []
    stderr_tail = []
    done = threading.Event()

    def _drain():
        try:
            for raw in proc.stderr:
                line = raw.decode("utf-8", "replace")
                m = _PTS_RE.search(line)
                if m and "Parsed_showinfo" in line:
                    pts_list.append(float(m.group(1)))
                else:
                    stderr_tail.append(line)
                    if len(stderr_tail) > 40:
                        stderr_tail.pop(0)
        finally:
            done.set()

    t = threading.Thread(target=_drain, daemon=True)
    t.start()

    if out_w is None:
        # caller wants native size; need actual dims from probe
        info = probe(path)
        out_w, out_h = info["width"], info["height"]
    frame_bytes = out_w * out_h * 3
    base = start or 0.0
    idx = 0
    timeouts = 0
    last_pts = 0.0
    delta = 1.0 / 30
    try:
        while True:
            if cancel is not None and cancel():
                break
            buf = proc.stdout.read(frame_bytes)
            if not buf or len(buf) < frame_bytes:
                break
            # wait briefly for the matching showinfo line; if timestamps stop
            # matching (shouldn't happen with -fps_mode passthrough), fall
            # back to extrapolation rather than crawling
            if timeouts < 3:
                waited = 0.0
                while len(pts_list) <= idx and not done.is_set():
                    done.wait(0.05)
                    waited += 0.05
                    if waited > 2.0:
                        timeouts += 1
                        break
            if len(pts_list) > idx:
                p = pts_list[idx]
                if idx > 0 and p > last_pts:
                    delta = p - last_pts
                last_pts = p
                pts = base + p
            else:
                last_pts = last_pts + delta
                pts = base + last_pts
            frame = np.frombuffer(buf, np.uint8).reshape(out_h, out_w, 3)
            yield pts, frame
            idx += 1
    finally:
        try:
            proc.stdout.close()
        except OSError:
            pass
        proc.terminate()
        proc.wait(timeout=10)
        done.wait(timeout=2)


def analysis_dims(width, height, cfg):
    """Frame size in the analysis model: content fit inside the WCAG screen
    (1024x768 by default), then scaled by analysis_scale."""
    f = min(cfg.screen_w / width, cfg.screen_h / height)
    aw = max(2, int(round(width * f * cfg.analysis_scale)))
    ah = max(2, int(round(height * f * cfg.analysis_scale)))
    # rawvideo pipe needs even-ish sizes only for yuv; rgb24 is fine as-is,
    # but keep dims even to be safe with scalers
    return aw - (aw % 2), ah - (ah % 2)


def make_proxy(path, out_path, start, duration, rc, progress=None, cancel=None):
    """Encode a browser-playable proxy of a section, preserving frame
    count/timestamps (-fps_mode passthrough)."""
    cmd = [FFMPEG, "-hide_banner", "-nostdin", "-y",
           "-ss", f"{start:.6f}", "-t", f"{duration:.6f}", "-i", path,
           "-map", "0:v:0", "-map", "0:a:0?",
           "-vf", f"scale=-2:{rc.proxy_height}",
           "-fps_mode", "passthrough",
           "-c:v", "libx264", "-crf", str(rc.proxy_crf), "-preset", "veryfast",
           "-pix_fmt", "yuv420p",
           "-c:a", "aac", "-b:a", "128k",
           "-ar", str(rc.audio_rate), "-ac", "2",
           "-movflags", "+faststart",
           out_path]
    return run_ffmpeg(cmd, duration=duration, progress=progress, cancel=cancel)


def make_thumbnails(path, out_dir, start, duration, thumb_w, progress=None,
                    cancel=None):
    """Dump one JPEG per frame of the section (ordinal-aligned with decode)."""
    os.makedirs(out_dir, exist_ok=True)
    pattern = os.path.join(out_dir, "%06d.jpg")
    cmd = [FFMPEG, "-hide_banner", "-nostdin", "-y",
           "-ss", f"{start:.6f}", "-t", f"{duration:.6f}", "-i", path,
           "-map", "0:v:0", "-an",
           "-vf", f"scale={thumb_w}:-2",
           "-fps_mode", "passthrough",
           "-q:v", "5", "-start_number", "0",
           pattern]
    run_ffmpeg(cmd, duration=duration, progress=progress, cancel=cancel)
    return len([f for f in os.listdir(out_dir) if f.endswith(".jpg")])


_TIME_RE = re.compile(r"time=(\d+):(\d+):(\d+(?:\.\d+)?)")


def run_ffmpeg(cmd, duration=None, progress=None, cancel=None, cwd=None):
    """Run an ffmpeg command, reporting progress (0..1) parsed from stderr.

    `cwd` lets callers pass short relative paths, which matters when the
    command names many inputs (see render.CMD_CHAR_LIMIT).
    """
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL,
                            stderr=subprocess.PIPE, cwd=cwd,
                            creationflags=CREATE_NO_WINDOW)
    tail = []
    for raw in proc.stderr:
        line = raw.decode("utf-8", "replace")
        tail.append(line)
        if len(tail) > 40:
            tail.pop(0)
        if cancel is not None and cancel():
            proc.terminate()
            proc.wait()
            raise FFError("cancelled")
        if progress and duration:
            m = _TIME_RE.search(line)
            if m:
                h, mnt, s = float(m.group(1)), float(m.group(2)), float(m.group(3))
                progress(min(1.0, (h * 3600 + mnt * 60 + s) / max(duration, 1e-6)))
    proc.wait()
    if proc.returncode != 0:
        raise FFError("ffmpeg failed:\n" + "".join(tail[-15:]))
    return True
