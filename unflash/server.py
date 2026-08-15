"""Local web UI server.

  python -m unflash.server [--port 8765] [--no-browser]
"""

import argparse
import os
import subprocess
import sys
import threading
import webbrowser

from flask import (Flask, jsonify, request, send_file, send_from_directory,
                   abort)

from . import ffio
from .analysis import analyze_file, violations_to_sections, timeline_summary
from .config import wcag_config, strict_config
from .editing import prepare_section, suggest_edits, check_section
from .jobs import JobManager
from .project import Project
from .render import render_section, export_video, verify_file

app = Flask(__name__, static_folder=None)
STATIC_DIR = os.path.join(os.path.dirname(__file__), "static")

state = {"project": None}
jobs = JobManager()


def proj() -> Project:
    p = state["project"]
    if p is None:
        abort(400, description="No video opened yet")
    return p


def _err(e, code=400):
    return jsonify({"error": str(e)}), code


@app.errorhandler(Exception)
def handle_error(e):
    from werkzeug.exceptions import HTTPException
    if isinstance(e, HTTPException):
        return jsonify({"error": e.description}), e.code
    import traceback
    traceback.print_exc()
    return jsonify({"error": str(e)}), 500


# --- static ------------------------------------------------------------------

@app.get("/")
def index():
    return send_from_directory(STATIC_DIR, "index.html")


@app.get("/static/<path:name>")
def static_files(name):
    return send_from_directory(STATIC_DIR, name)


# --- project -----------------------------------------------------------------

@app.post("/api/pick")
def pick_file():
    """Native file dialog via a helper process (browsers hide real paths)."""
    code = (
        "import tkinter as tk, tkinter.filedialog as fd\n"
        "r=tk.Tk(); r.withdraw(); r.attributes('-topmost', True)\n"
        "p=fd.askopenfilename(title='Select video', filetypes=["
        "('Video files','*.mp4 *.mkv *.avi *.mov *.webm *.ts *.flv *.m4v'),"
        "('All files','*.*')])\n"
        "print(p or '')"
    )
    r = subprocess.run([sys.executable, "-c", code], capture_output=True,
                       text=True, timeout=300)
    path = (r.stdout or "").strip()
    return jsonify({"path": path or None})


@app.post("/api/pick_save")
def pick_save():
    data = request.get_json(force=True) or {}
    initial = data.get("initial", "output.mp4")
    code = (
        "import tkinter as tk, tkinter.filedialog as fd\n"
        "r=tk.Tk(); r.withdraw(); r.attributes('-topmost', True)\n"
        f"p=fd.asksaveasfilename(title='Save as', initialfile={initial!r},"
        "defaultextension='.mp4', filetypes=[('MP4','*.mp4')])\n"
        "print(p or '')"
    )
    r = subprocess.run([sys.executable, "-c", code], capture_output=True,
                       text=True, timeout=300)
    path = (r.stdout or "").strip()
    return jsonify({"path": path or None})


@app.post("/api/open")
def open_video():
    data = request.get_json(force=True) or {}
    path = data.get("path", "")
    if not path or not os.path.exists(path):
        return _err(f"File not found: {path}", 404)
    state["project"] = Project(path)
    return project_state()


@app.get("/api/project")
def project_state():
    p = state["project"]
    if p is None:
        return jsonify({"project": None})
    d = dict(p.data)
    # trim heavy per-section fields for the overview
    d["sections"] = {sid: _section_summary(s)
                     for sid, s in p.data["sections"].items()}
    d["workdir"] = p.workdir
    d["n_keyframes"] = len(p.data["keyframes"])
    d["bounds"] = list(p.bounds)
    d.pop("keyframes", None)
    return jsonify({"project": d})


def _section_summary(s):
    out = {k: s.get(k) for k in
           ("id", "start", "end", "kinds", "prepared", "n_frames",
            "warnings")}
    out["n_edits"] = len(s.get("edits") or {})
    out["check_safe"] = (s.get("check") or {}).get("safe")
    out["has_preview"] = bool(s.get("preview"))
    out["has_render"] = bool(s.get("render"))
    pv = s.get("preview") or {}
    rv = s.get("render") or {}
    out["preview_safe"] = (pv.get("verdict") or {}).get("safe")
    out["render_safe"] = (rv.get("verdict") or {}).get("safe")
    out["render_stale"] = Project.render_stale(s)
    return out


@app.post("/api/settings")
def update_settings():
    data = request.get_json(force=True) or {}
    if data.get("profile") == "wcag":
        proj().data["detector"] = wcag_config().to_dict()
        proj().save()
    elif data.get("profile") == "strict":
        proj().data["detector"] = strict_config().to_dict()
        proj().save()
    proj().update_settings(detector=data.get("detector"),
                           render=data.get("render"))
    return jsonify({"detector": proj().data["detector"],
                    "render": proj().data["render"]})


# --- scan --------------------------------------------------------------------

@app.post("/api/scan")
def scan():
    p = proj()

    def run(job):
        cfg = p.detector_config
        info = p.data["info"]
        job.set_progress(0.0, "indexing packets/keyframes")
        p.data["index"] = None   # refresh on each scan
        p.ensure_index()
        bounds = p.bounds
        job.set_progress(0.02, "scanning for flashes")
        res = analyze_file(
            p.video_path, cfg, info=info,
            progress=lambda pr: job.set_progress(0.02 + pr * 0.95,
                                                 "scanning for flashes"),
            cancel=job.cancelled)
        if job.cancelled():
            return None
        sections = violations_to_sections(res.violations, cfg, bounds,
                                          p.data["keyframes"])
        with p.lock:
            p.data["scan"] = {
                "safe": res.safe,
                "anomalies": res.anomalies,
                "violations": [
                    {"start": v.start, "end": v.end, "kind": v.kind,
                     "count": v.count} for v in res.violations],
                "timeline": timeline_summary(res, bounds),
                "suggested_sections": sections,
            }
            # create sections for any suggested range not already covered
            existing = [(s["start"], s["end"])
                        for s in p.data["sections"].values()]
            created = 0
            for sec in sections:
                overlaps = any(sec["start"] < e and sec["end"] > s
                               for s, e in existing)
                if not overlaps:
                    p.add_section(sec["start"], sec["end"], sec["kinds"],
                                  snap=False)
                    created += 1
            p.save()
        return {"safe": res.safe, "sections_created": created,
                "violations": len(res.violations)}

    job = jobs.start("scan", run)
    return jsonify({"job": job.id})


# --- sections ----------------------------------------------------------------

@app.post("/api/sections")
def add_section():
    data = request.get_json(force=True) or {}
    sec = proj().add_section(float(data["start"]), float(data["end"]),
                             kinds=data.get("kinds") or ["manual"],
                             snap=data.get("snap", True))
    return jsonify({"section": _section_summary(sec)})


@app.delete("/api/section/<sid>")
def delete_section(sid):
    proj().remove_section(sid)
    return jsonify({"ok": True})


@app.patch("/api/section/<sid>")
def patch_section(sid):
    data = request.get_json(force=True) or {}
    sec = proj().update_section_bounds(sid, float(data["start"]),
                                       float(data["end"]),
                                       snap=data.get("snap", True))
    return jsonify({"section": _section_summary(sec)})


@app.post("/api/prepare_all")
def prepare_all():
    p = proj()
    todo = [s["id"] for s in p.sections_sorted() if not s.get("prepared")]
    if not todo:
        return _err("All sections are already prepared", 400)

    def run(job):
        done = 0
        for k, sid in enumerate(todo):
            if job.cancelled():
                break
            base = k / len(todo)

            class _Sub:
                def set_progress(self, pr, msg=None):
                    job.set_progress(base + pr / len(todo),
                                     f"section #{sid} ({k + 1}/{len(todo)}): "
                                     f"{msg or ''}")

                def cancelled(self):
                    return job.cancelled()

            prepare_section(p, sid, job=_Sub())
            done += 1
        return {"prepared": done, "total": len(todo)}

    job = jobs.start("prepare all", run)
    return jsonify({"job": job.id})


@app.get("/api/section/<sid>")
def section_detail(sid):
    p = proj()
    s = p.section(sid)
    out = dict(s)
    # per-frame chart data comes from the stored analysis stats
    return jsonify({"section": out})


@app.post("/api/section/<sid>/prepare")
def prepare(sid):
    p = proj()
    p.section(sid)  # validate
    job = jobs.start(f"prepare {sid}",
                     lambda job: _section_summary(
                         prepare_section(p, sid, job=job)))
    return jsonify({"job": job.id})


@app.post("/api/section/<sid>/edits")
def set_edits(sid):
    data = request.get_json(force=True) or {}
    clean = proj().set_edits(sid, data.get("edits") or {})
    return jsonify({"edits": clean})


@app.post("/api/render_all")
def render_all():
    p = proj()
    todo = []
    skipped = []
    unprepared = []
    for s in p.sections_sorted():
        if not s.get("prepared"):
            unprepared.append(s["id"])
            continue
        r = s.get("render")
        if (r and r.get("path") and os.path.exists(r["path"])
                and not Project.render_stale(s)):
            skipped.append(s["id"])
        else:
            todo.append(s["id"])
    if not todo:
        msg = "Nothing to render — all prepared sections are up to date."
        if unprepared:
            msg += " Unprepared: #" + ", #".join(unprepared)
        return _err(msg, 400)

    def run(job):
        done = 0
        for k, sid in enumerate(todo):
            if job.cancelled():
                break
            base = k / len(todo)

            class _Sub:
                def set_progress(self, pr, msg=None):
                    job.set_progress(base + pr / len(todo),
                                     f"section #{sid} ({k + 1}/{len(todo)}): "
                                     f"{msg or ''}")

                def cancelled(self):
                    return job.cancelled()

            out = os.path.join(p.section_dir(sid), "render.mp4")
            render_section(p, sid, "original", out, job=_Sub())
            done += 1
        return {"rendered": done, "total": len(todo),
                "skipped": len(skipped), "unprepared": unprepared}

    job = jobs.start("render all", run)
    return jsonify({"job": job.id})


@app.post("/api/section/<sid>/suggest")
def suggest(sid):
    p = proj()
    data = request.get_json(force=True) or {}
    prefer = data.get("prefer", "light")
    only = data.get("only") or None
    job = jobs.start(f"suggest {sid}",
                     lambda job: suggest_edits(p, sid, prefer=prefer,
                                               only=only, job=job))
    return jsonify({"job": job.id})


@app.post("/api/section/<sid>/check")
def check(sid):
    p = proj()
    data = request.get_json(force=True) or {}
    job = jobs.start(f"check {sid}",
                     lambda job: check_section(p, sid,
                                               edits=data.get("edits")))
    return jsonify({"job": job.id})


@app.post("/api/section/<sid>/preview")
def preview(sid):
    p = proj()
    out = os.path.join(p.section_dir(sid), "preview.mp4")
    job = jobs.start(f"preview {sid}",
                     lambda job: render_section(p, sid, "preview", out,
                                                job=job))
    return jsonify({"job": job.id})


@app.post("/api/section/<sid>/render")
def render_full(sid):
    p = proj()
    out = os.path.join(p.section_dir(sid), "render.mp4")
    job = jobs.start(f"render {sid}",
                     lambda job: render_section(p, sid, "original", out,
                                                job=job))
    return jsonify({"job": job.id})


@app.get("/thumb/<sid>/<int:n>")
def thumb(sid, n):
    s = proj().section(sid)
    if not s.get("thumbs"):
        abort(404)
    f = os.path.join(s["thumbs"], f"{n:06d}.jpg")
    if not os.path.exists(f):
        abort(404)
    return send_file(f, mimetype="image/jpeg", max_age=3600)


@app.get("/media/<sid>/<name>")
def media(sid, name):
    s = proj().section(sid)
    mapping = {
        "proxy.mp4": s.get("proxy"),
        "preview.mp4": (s.get("preview") or {}).get("path"),
        "render.mp4": (s.get("render") or {}).get("path"),
    }
    f = mapping.get(name)
    if not f or not os.path.exists(f):
        abort(404)
    return send_file(f, mimetype="video/mp4", conditional=True)


# --- export ------------------------------------------------------------------

@app.post("/api/export")
def export():
    p = proj()
    data = request.get_json(force=True) or {}
    mode = data.get("mode", "reencode")
    out = data.get("path")
    if not out:
        base, _ = os.path.splitext(p.video_path)
        out = base + ".unflashed.mp4"
    job = jobs.start("export",
                     lambda job: export_video(p, out, mode=mode, job=job))
    return jsonify({"job": job.id, "path": out})


@app.post("/api/verify_export")
def verify_export():
    p = proj()
    exp = p.data.get("export")
    if not exp or not os.path.exists(exp.get("path", "")):
        return _err("No exported file to verify", 404)

    def run(job):
        verdict = verify_file(p, exp["path"], job=job)
        with p.lock:
            p.data["export"]["verify"] = verdict
            p.save()
        return verdict

    job = jobs.start("verify export", run)
    return jsonify({"job": job.id})


# --- jobs --------------------------------------------------------------------

@app.get("/api/job/<jid>")
def job_status(jid):
    j = jobs.get(jid)
    if j is None:
        return _err("No such job", 404)
    return jsonify(j.to_dict())


@app.post("/api/job/<jid>/cancel")
def job_cancel(jid):
    j = jobs.get(jid)
    if j is None:
        return _err("No such job", 404)
    j.cancel()
    return jsonify(j.to_dict())


@app.get("/api/jobs")
def jobs_list():
    return jsonify({"jobs": [j.to_dict() for j in jobs.active()]})


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8765)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--no-browser", action="store_true")
    ap.add_argument("--video", help="open this video on startup")
    args = ap.parse_args(argv)

    if args.video:
        state["project"] = Project(args.video)

    url = f"http://{args.host}:{args.port}/"
    if not args.no_browser:
        threading.Timer(1.0, lambda: webbrowser.open(url)).start()
    print(f"Unflash running at {url}")
    app.run(host=args.host, port=args.port, threaded=True)


if __name__ == "__main__":
    main()
