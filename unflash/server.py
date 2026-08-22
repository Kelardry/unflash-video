"""Local web UI server.

  python -m unflash.server [--port 8765] [--no-browser]

One server per signed-in account: it picks a port no one else holds and only
serves requests carrying this account's token (see instance.py), so accounts
sharing a PC cannot see each other's projects or pop file dialogs onto each
other's desktops.
"""

import argparse
import os
import secrets
import subprocess
import sys
import threading
import webbrowser

from flask import (Flask, Response, jsonify, redirect, request, send_file,
                   send_from_directory, abort)

from . import ffio, instance
from .analysis import analyze_file, violations_to_sections, timeline_summary
from .config import profile_config, profile_name
from .editing import prepare_section, suggest_edits, check_section
from .jobs import JobManager
from .project import Project, project_dir_video
from .render import (render_section, export_video, verify_file,
                     filter_assembly_estimate)

app = Flask(__name__, static_folder=None)
STATIC_DIR = os.path.join(os.path.dirname(__file__), "static")

state = {"project": None}
jobs = JobManager()

# this server's identity; token None means the check is off (--no-token)
INSTANCE = {"token": None, "port": None, "host": "127.0.0.1"}


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


# --- access ------------------------------------------------------------------

BLOCKED_HTML = """<!DOCTYPE html><html><head><meta charset="utf-8">
<title>Unflash — not your session</title>
<style>body{font:15px/1.5 system-ui,sans-serif;max-width:34em;margin:12vh auto;
padding:0 1.5em;color:#ddd;background:#1b1c1f}code{background:#000;padding:
.1em .4em;border-radius:3px}</style></head><body>
<h1>This Unflash server is not yours</h1>
<p>It was started by a different account signed in to this PC. Its projects,
its file dialogs and its browse windows belong to that account, so it will not
serve this page.</p>
<p>Start your own copy (<code>run_unflash.bat</code>) — it will take the next
free port and open the right address for you.</p>
</body></html>"""


def _cookie_name():
    # cookies ignore the port, so two accounts on 127.0.0.1 would otherwise
    # share (and clobber) one cookie
    return f"unflash_token_{INSTANCE.get('port') or 0}"


@app.before_request
def _check_token():
    tok = INSTANCE.get("token")
    if not tok:
        return None
    # any of the three carriers may hold it: a stale token in a bookmarked
    # URL must not lock out a browser whose cookie is still good
    offered = (request.headers.get("X-Unflash-Token"),
               request.args.get("token"),
               request.cookies.get(_cookie_name()))
    if any(g and secrets.compare_digest(g, tok) for g in offered):
        return None
    if request.path.startswith(("/api/", "/media/", "/thumb/")):
        return jsonify({"error": "This Unflash server belongs to another "
                                 "account signed in to this PC."}), 403
    return Response(BLOCKED_HTML, status=403, mimetype="text/html")


@app.get("/api/instance")
def instance_info():
    """Reached only with a valid token, so a 200 here means "this server is
    yours" — that is how a second launch finds its own server."""
    return jsonify({"unflash": True, "pid": os.getpid(),
                    "port": INSTANCE.get("port")})


# --- static ------------------------------------------------------------------

@app.get("/")
def index():
    tok = INSTANCE.get("token")
    if tok and request.args.get("token"):
        # keep the token out of the address bar / history; the cookie carries
        # it from here on
        resp = redirect("/")
    else:
        resp = send_from_directory(STATIC_DIR, "index.html")
    if tok:
        resp.set_cookie(_cookie_name(), tok, max_age=90 * 24 * 3600,
                        httponly=True, samesite="Lax", path="/")
    return resp


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
    try:
        r = subprocess.run([sys.executable, "-c", code], capture_output=True,
                           text=True, timeout=300)
    except subprocess.TimeoutExpired:
        return jsonify({"path": None, "error": "File dialog timed out"})
    path = (r.stdout or "").strip()
    err = None
    if not path and r.returncode != 0:
        err = ("File dialog failed: "
               + (r.stderr or "").strip()[-300:] or "unknown error")
    return jsonify({"path": path or None, "error": err})


@app.post("/api/pick_dir")
def pick_dir():
    """Native folder dialog, for choosing an existing .unflash project."""
    data = request.get_json(silent=True) or {}
    initial = data.get("initial") or ""
    code = (
        "import tkinter as tk, tkinter.filedialog as fd\n"
        "r=tk.Tk(); r.withdraw(); r.attributes('-topmost', True)\n"
        "p=fd.askdirectory(title='Select an existing .unflash project "
        f"folder', initialdir={initial!r} or None, mustexist=True)\n"
        "print(p or '')"
    )
    try:
        r = subprocess.run([sys.executable, "-c", code], capture_output=True,
                           text=True, timeout=300)
    except subprocess.TimeoutExpired:
        return jsonify({"path": None, "error": "Folder dialog timed out"})
    path = (r.stdout or "").strip()
    err = None
    if not path and r.returncode != 0:
        err = ("Folder dialog failed: "
               + (r.stderr or "").strip()[-300:] or "unknown error")
    return jsonify({"path": os.path.normpath(path) if path else None,
                    "error": err})


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


def _activate(project, label):
    """Make a freshly built project the open one, indexing it if the
    container has no usable duration."""
    info = project.data["info"]
    if not info.get("duration") or info["duration"] <= 0:
        # some files (e.g. streamed webm) have no duration metadata; index the
        # packets right away so the timeline has real bounds
        try:
            project.ensure_index()
        except Exception as e:  # noqa: BLE001
            return _err(f"'{label}' has no duration and packet indexing "
                        f"failed: {e}", 400)
    state["project"] = project
    return jsonify({"project": _project_payload(), "notes": project.notes,
                    "recoverable": project.orphan_sections()})


@app.post("/api/open")
def open_video():
    data = request.get_json(force=True) or {}
    path = (data.get("path") or "").strip().strip('"')
    if not path or not os.path.exists(path):
        return _err(f"File not found: {path}", 404)
    try:
        project = Project(path)
    except Exception as e:  # noqa: BLE001 - reported to the UI
        return _err(f"Could not open '{os.path.basename(path)}': {e}", 400)
    return _activate(project, os.path.basename(path))


@app.post("/api/open_project")
def open_project():
    """Open an existing ``<name>.unflash`` folder chosen by the user, e.g.
    one that was moved away from its video or never picked up automatically.
    Paths recorded inside it are re-pointed at where the files are now."""
    data = request.get_json(force=True) or {}
    d = (data.get("path") or "").strip().strip('"')
    if not d or not os.path.isdir(d):
        return _err(f"Not a folder: {d}", 404)
    d = os.path.abspath(os.path.normpath(d))
    name = os.path.basename(d)
    if not os.path.exists(os.path.join(d, "project.json")):
        return _err(f"'{name}' is not an Unflash project folder — it has no "
                    f"project.json inside. Pick the '<video name>.unflash' "
                    f"folder itself, not the folder containing it.", 400)

    video = (data.get("video") or "").strip().strip('"') or None
    if video and not os.path.isfile(video):
        return _err(f"Video file not found: {video}", 404)
    if not video:
        video, stored = project_dir_video(d)
        if not video:
            missing = f" It was made from '{stored}'." if stored else ""
            return jsonify({
                "error": f"Could not find the video for '{name}'.{missing} "
                         f"Pick the video file it belongs to.",
                "need_video": True, "stored": stored}), 400
    try:
        project = Project(video, workdir=d, adopt=True)
    except Exception as e:  # noqa: BLE001 - reported to the UI
        return _err(f"Could not open project '{name}': {e}", 400)
    return _activate(project, os.path.basename(video))


@app.post("/api/recover_sections")
def recover_sections():
    """Rebuild section entries from folders left in the work directory when
    the project file lost them."""
    p = proj()
    recovered, notes = p.recover_sections()
    return jsonify({"recovered": recovered, "notes": notes,
                    "project": _project_payload(),
                    "recoverable": p.orphan_sections()})


@app.get("/api/project")
def project_state():
    return jsonify({"project": _project_payload()})


def _project_payload():
    p = state["project"]
    if p is None:
        return None
    d = dict(p.data)
    # trim heavy per-section fields for the overview
    d["sections"] = {sid: _section_summary(s)
                     for sid, s in p.data["sections"].items()}
    d["workdir"] = p.workdir
    d["n_keyframes"] = len(p.data["keyframes"])
    d["bounds"] = list(p.bounds)
    d["profile"] = profile_name(p.data["detector"])
    d.pop("keyframes", None)
    return d


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
    if data.get("profile"):
        cfg = profile_config(data["profile"])
        if cfg is None:
            return _err(f"Unknown detection profile {data['profile']!r}", 400)
        proj().data["detector"] = cfg.to_dict()
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
        # exact section bounds (no keyframe snap: a 1.4s problem should not
        # become a 14s section; only smart-cut needs alignment)
        sections = violations_to_sections(res.violations, cfg, bounds, None)
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
                    # exact bounds — keyframe snapping inflated sections
                    # (only smart-cut needs alignment; it checks at export)
                    p.add_section(sec["start"], sec["end"], sec["kinds"],
                                  snap=False)
                    created += 1
            p.save()
        n_ext = sum(1 for v in res.violations if v.kind == "extended")
        return {"safe": res.safe, "wcag_safe": res.wcag_safe,
                "sections_created": created,
                "violations": len(res.violations) - n_ext,
                "extended": n_ext,
                "flag_extended": res.flag_extended}

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


@app.delete("/api/sections")
def delete_all_sections():
    p = proj()
    import shutil
    with p.lock:
        ids = list(p.data["sections"].keys())
        for sid in ids:
            d = os.path.join(p.workdir, f"section_{sid}")
            shutil.rmtree(d, ignore_errors=True)
        p.data["sections"] = {}
        p.save()
    return jsonify({"deleted": len(ids)})


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

def _split_mode(raw):
    """UI sends one string; '<mode>-filter' selects the filter join."""
    mode, _, asm = (raw or "reencode").partition("-")
    return (mode or "reencode"), ("filter" if asm == "filter" else "copy")


@app.get("/api/export_plan")
def export_plan():
    """How the chosen assembly would run: part count, command length, and
    whether that forces batching. Used to warn before the export starts."""
    p = proj()
    mode, assembly = _split_mode(request.args.get("mode"))
    info = filter_assembly_estimate(p, mode)
    info["assembly"] = assembly
    return jsonify(info)


@app.post("/api/export")
def export():
    p = proj()
    data = request.get_json(force=True) or {}
    mode, assembly = _split_mode(data.get("mode"))
    out = data.get("path")
    if not out:
        base, _ = os.path.splitext(p.video_path)
        out = base + ".unflashed.mp4"
    job = jobs.start("export",
                     lambda job: export_video(p, out, mode=mode,
                                              assembly=assembly, job=job))
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


def _bind(host, ports):
    """First port in ``ports`` we can actually listen on."""
    from werkzeug.serving import BaseWSGIServer, make_server
    if os.name == "nt":
        # Windows would otherwise let us bind a port another account is
        # already serving on, and then split connections between the two
        BaseWSGIServer.allow_reuse_address = False
    last = None
    tried = 0
    for p in ports:
        tried += 1
        try:
            return make_server(host, p, app, threaded=True), p
        except OSError as e:
            last = e
        except SystemExit as e:
            # werkzeug turns "address in use" into its own message + exit;
            # keep scanning instead of giving up on the first collision
            last = e.code if isinstance(e.code, str) else f"port {p} in use"
    raise SystemExit(
        f"Could not open a port for the Unflash server after {tried} "
        f"attempt(s): {last or 'every candidate port was already in use'}"
        + ("\nDrop --port to let it pick a free one." if tried == 1 else ""))


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=None,
                    help=f"fixed port; the default is "
                         f"{instance.DEFAULT_PORT}, or the next free one up "
                         f"if another account is already holding it")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--no-browser", action="store_true")
    ap.add_argument("--video", help="open this video on startup")
    ap.add_argument("--new", action="store_true",
                    help="start a second server even if this account already "
                         "has one running")
    ap.add_argument("--no-token", action="store_true",
                    help="serve without the per-account token: any account "
                         "signed in to this PC can then use this server, its "
                         "projects and its file dialogs")
    args = ap.parse_args(argv)

    token = None if args.no_token else instance.user_token()

    # already running for this account? hand the video over and re-open the
    # browser on it rather than starting a second copy
    if token and not args.new and args.port is None:
        own = instance.find_own(args.host, token)
        if own:
            url = f"http://{args.host}:{own}/"
            print(f"Unflash is already running for this account at {url}")
            if args.video:
                err = instance.post(args.host, own, token, "/api/open",
                                    {"path": os.path.abspath(args.video)})
                if err:
                    print(f"Could not open {args.video} there: {err}")
            if not args.no_browser:
                webbrowser.open(f"{url}?token={token}")
            return 0

    if args.video:
        state["project"] = Project(args.video)

    if args.port is not None:
        ports = [args.port]
    else:
        ports = instance.candidate_ports(args.host, instance.DEFAULT_PORT)
    srv, port = _bind(args.host, ports)
    INSTANCE.update({"token": token, "port": port, "host": args.host})
    if token:
        instance.save_state(host=args.host, port=port, pid=os.getpid())

    url = f"http://{args.host}:{port}/"
    print(f"Unflash running at {url}")
    if port != instance.DEFAULT_PORT and args.port is None:
        print(f"(port {instance.DEFAULT_PORT} was taken — another account "
              f"signed in to this PC is probably running Unflash too)")
    if token:
        print("This server is private to your account. If the page ever says "
              "it is not yours, re-open it with:")
        print(f"  {url}?token={token}")
    else:
        print("WARNING: --no-token — any account signed in to this PC can "
              "open this server, see your projects, and make file dialogs "
              "appear on your desktop.")
    if not args.no_browser:
        open_url = url + (f"?token={token}" if token else "")
        threading.Timer(1.0, lambda: webbrowser.open(open_url)).start()
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    main()
