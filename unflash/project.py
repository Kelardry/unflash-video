"""Project state: one project per source video, stored as JSON in a work
directory next to the video (``<name>.unflash/``)."""

import hashlib
import json
import os
import threading
import time

from .config import (DEFAULT_PROFILE, PROFILES, DetectorConfig,
                     RenderConfig, profile_config, profile_name)
from . import ffio


# The extended-flash settings are not chosen directly -- they come with the
# profile -- and their definition has changed since projects started being
# saved, so a project is identified by the thresholds that name a profile and
# then takes that profile's current extended settings.
PROFILE_FIELDS = tuple(f for f in DetectorConfig().to_dict()
                       if not f.startswith("extended_"))


def _snap_to_profile(merged, saved):
    """The named profile whose thresholds this project already uses, as a
    full settings dict. wcag and wcag_ext share thresholds, so a saved
    extended_mode breaks the tie; a project from before that setting existed
    takes the current default profile. No match -> left as it was."""
    matches = [name for name, factory in PROFILES.items()
               if all(merged[f] == factory().to_dict()[f]
                      for f in PROFILE_FIELDS)]
    if not matches:
        return merged
    if len(matches) > 1:
        mode = saved.get("extended_mode")
        tie = [n for n in matches if profile_config(n).extended_mode == mode]
        if not tie:
            tie = [n for n in matches if n == DEFAULT_PROFILE]
        matches = tie or matches
    return profile_config(matches[0]).to_dict()


VIDEO_EXTS = (".mp4", ".mkv", ".avi", ".mov", ".webm", ".ts", ".flv", ".m4v",
              ".wmv", ".mpg", ".mpeg", ".m2ts", ".ogv", ".3gp")

# what each stored path points at, and what it is called by default, so a
# moved project folder can be re-pointed at its own copy of the file
SECTION_FILES = (
    ("proxy", "proxy.mp4", False, "proxy video"),
    ("thumbs", "thumbs", True, "thumbnails"),
    ("cache_npy", "analysis_frames.npy", False, "cached frame analysis"),
)
SECTION_RENDERS = (
    ("preview", "preview.mp4", "preview render"),
    ("render", "render.mp4", "full-res render"),
)


def same_path(a, b):
    """Whether two paths name the same file. Plain string comparison is not
    enough: Windows paths differ in case and separators, and the same file is
    reachable through a mapped drive, a UNC path or a junction."""
    if not a or not b:
        return False
    na = os.path.normcase(os.path.normpath(os.path.abspath(a)))
    nb = os.path.normcase(os.path.normpath(os.path.abspath(b)))
    if na == nb:
        return True
    try:
        return os.path.exists(a) and os.path.exists(b) and os.path.samefile(a, b)
    except OSError:
        return False


def workdir_for(video_path):
    base = os.path.splitext(os.path.basename(video_path))[0]
    d = os.path.join(os.path.dirname(os.path.abspath(video_path)),
                     base + ".unflash")
    try:
        os.makedirs(d, exist_ok=True)
        probe_file = os.path.join(d, ".write_test")
        with open(probe_file, "w") as f:
            f.write("ok")
        os.remove(probe_file)
        return d
    except OSError:
        h = hashlib.sha1(os.path.abspath(video_path).encode()).hexdigest()[:10]
        d = os.path.join(os.path.expanduser("~"), ".unflash", f"{base}-{h}")
        os.makedirs(d, exist_ok=True)
        return d


def project_dir_video(workdir):
    """Best guess at the video a ``.unflash`` folder belongs to: the path it
    records, else a file next to the folder carrying the folder's own name
    (that is how the folder was named in the first place).

    Returns ``(path_or_None, recorded_path_or_None)``."""
    stored = None
    try:
        with open(os.path.join(workdir, "project.json"), "r",
                  encoding="utf-8") as f:
            stored = (json.load(f) or {}).get("video_path")
    except (OSError, json.JSONDecodeError, AttributeError):
        stored = None

    workdir = os.path.abspath(os.path.normpath(workdir))
    base = os.path.basename(workdir)
    stem = base[:-len(".unflash")] if base.lower().endswith(".unflash") else base
    parent = os.path.dirname(workdir)

    # a video sitting beside the folder and carrying the folder's own name
    # beats the recorded path: if the project was copied elsewhere, that
    # recorded path may still point at the original copy
    cands = []
    for ext in VIDEO_EXTS:
        cands.append(os.path.join(parent, stem + ext))
        cands.append(os.path.join(parent, stem + ext.upper()))
    if stored:
        cands.append(os.path.join(parent, os.path.basename(stored)))
        cands.append(stored)
    for c in cands:
        if c and os.path.isfile(c):
            return c, stored
    return None, stored


class Project:
    def __init__(self, video_path, workdir=None, adopt=False):
        """``workdir`` overrides the ``<name>.unflash`` folder beside the
        video (used when opening a project folder directly). ``adopt`` loads
        that folder even though it records a different video, re-pointing
        every stored path at where things actually are now."""
        self.video_path = os.path.abspath(video_path)
        self.workdir = (os.path.abspath(workdir) if workdir
                        else workdir_for(video_path))
        self.notes = []        # user-facing messages about the load/repair
        self.lock = threading.RLock()
        self.data = {
            "video_path": self.video_path,
            "workdir": self.workdir,
            "info": None,
            "keyframes": [],
            "index": None,         # packet index: ts_min/ts_max/median_delta/...
            "detector": DetectorConfig().to_dict(),
            "render": RenderConfig().to_dict(),
            "scan": None,          # {timeline, violations, at}
            "sections": {},        # id -> section dict
            "next_section_id": 1,
            "export": None,        # {path, mode, at, verify}
        }
        self._load(adopt=adopt)
        if self.data["info"] is None:
            self.data["info"] = ffio.probe(self.video_path)
            self.save()

    # --- persistence ---------------------------------------------------------
    @property
    def path(self):
        return os.path.join(self.workdir, "project.json")

    def _load(self, adopt=False):
        """Load the saved project, tolerating a moved video or work folder.

        The recorded video path is not required to match character for
        character — Windows spellings differ (case, mapped drive vs UNC), and
        a project whose video has moved away with its folder should still be
        picked up rather than silently starting from scratch."""
        if not os.path.exists(self.path):
            return
        try:
            with open(self.path, "r", encoding="utf-8") as f:
                stored = json.load(f)
        except (json.JSONDecodeError, OSError) as e:
            self.notes.append(f"The project file in {self.workdir} could not "
                              f"be read ({e}); starting a fresh project.")
            return
        if not isinstance(stored, dict):
            return

        old_video = stored.get("video_path") or ""
        old_workdir = stored.get("workdir") or ""
        matches = same_path(old_video, self.video_path)
        if not matches and not adopt and os.path.exists(old_video):
            # a copied project folder: it still names the original video. The
            # folder is always <video name>.unflash beside its video, so this
            # one does belong to the video we were handed -- say so, and check
            # below that the two files really are the same video.
            self.notes.append(
                f"This work folder was made from '{old_video}', which still "
                f"exists; it has been relinked to the video you opened.")

        self.data.update(stored)
        self.data["video_path"] = self.video_path
        self.data["workdir"] = self.workdir
        self._migrate_settings()

        moved_dir = bool(old_workdir) and not same_path(old_workdir,
                                                        self.workdir)
        if moved_dir:
            self.notes.append(f"This project folder was moved here from "
                              f"{old_workdir}; the file paths saved inside it "
                              f"were updated to match.")
        if not matches:
            if os.path.basename(old_video) != os.path.basename(self.video_path):
                self.notes.append(
                    f"The project was made from '{os.path.basename(old_video)}'"
                    f" and is now attached to "
                    f"'{os.path.basename(self.video_path)}'.")
            self._relink_video(old_video)
        if not matches or moved_dir or not old_workdir:
            self.notes += self._repair_paths()
        self.save()

    def _relink_video(self, old_video):
        """The project was recorded against a different path. Re-probe, and
        say so if the file there is not the same video."""
        base = os.path.basename(self.video_path)
        try:
            info = ffio.probe(self.video_path)
        except Exception as e:  # noqa: BLE001 - surfaced to the UI
            self.notes.append(f"Could not re-probe '{base}' after relinking "
                              f"the project to it: {e}")
            return
        old = self.data.get("info") or {}
        self.data["info"] = info
        if not old:
            return
        diffs = []
        if (old.get("width"), old.get("height")) != (info.get("width"),
                                                     info.get("height")):
            diffs.append(f"{old.get('width')}×{old.get('height')} → "
                         f"{info.get('width')}×{info.get('height')}")
        if abs((old.get("duration") or 0) - (info.get("duration") or 0)) > 0.5:
            diffs.append(f"duration {old.get('duration', 0):.1f}s → "
                         f"{info.get('duration', 0):.1f}s")
        if round(old.get("fps") or 0, 2) != round(info.get("fps") or 0, 2):
            diffs.append(f"{old.get('fps', 0):.2f} → {info.get('fps', 0):.2f} fps")
        if diffs:
            self.notes.append(
                f"'{base}' is not quite the video this project was made "
                f"from ({'; '.join(diffs)}). Section times may not line up — "
                f"re-scan, and re-prepare any section you keep.")

    def _repair_paths(self):
        """Re-point stored file paths at this work folder. A project folder
        that was moved (or reached by a different spelling of the same path)
        still names the old location in every proxy/thumbnail/render path."""
        notes = []
        def order(kv):
            return (0, int(kv[0])) if kv[0].isdigit() else (1, 0)

        for sid, sec in sorted((self.data.get("sections") or {}).items(),
                               key=order):
            sdir = os.path.join(self.workdir, f"section_{sid}")
            lost = []

            def here(cur, default):
                name = os.path.basename(
                    str(cur or "").replace("\\", "/").rstrip("/")) or default
                return os.path.join(sdir, name)

            for key, default, isdir, label in SECTION_FILES:
                cur = sec.get(key)
                if not cur:
                    continue
                new = here(cur, default)
                if (os.path.isdir(new) if isdir else os.path.isfile(new)):
                    sec[key] = new
                else:
                    sec[key] = None
                    lost.append(label)

            for key, default, label in SECTION_RENDERS:
                ent = sec.get(key)
                if not isinstance(ent, dict) or not ent.get("path"):
                    continue
                new = here(ent["path"], default)
                if os.path.isfile(new):
                    ent["path"] = new
                else:
                    sec[key] = None
                    lost.append(label)

            if not sec.get("cache_npy") and sec.get("prepared"):
                # editing needs the cached frames; offer preparation again
                sec["prepared"] = False
            if lost:
                notes.append(f"Section #{sid}: {', '.join(lost)} missing from "
                             f"the project folder — prepare/render it again "
                             f"(your frame marks were kept).")

        exp = self.data.get("export")
        if isinstance(exp, dict) and exp.get("path"):
            if not os.path.exists(exp["path"]):
                beside = os.path.join(os.path.dirname(self.video_path),
                                      os.path.basename(exp["path"]))
                if os.path.exists(beside):
                    exp["path"] = beside
                else:
                    notes.append("The previously exported file is no longer "
                                 "where it was saved; export again when the "
                                 "sections are rendered.")
        return notes

    def _migrate_settings(self):
        """Fill in settings added after the project was saved, so an older
        project still matches a named profile instead of reading as custom.

        A project whose profile-identifying thresholds match a named profile
        also adopts that profile's current extended-flash settings: those are
        not chosen directly (they come with the profile) and their definition
        has changed. Genuinely custom thresholds match no profile and are
        left exactly as saved."""
        for key, defaults in (("detector", DetectorConfig().to_dict()),
                              ("render", RenderConfig().to_dict())):
            saved = {k: v for k, v in (self.data.get(key) or {}).items()
                     if k in defaults}
            merged = {**defaults, **saved}
            if key == "detector" and profile_name(merged) == "custom":
                merged = _snap_to_profile(merged, saved)
            self.data[key] = merged

    def save(self):
        with self.lock:
            tmp = self.path + ".tmp"
            with open(tmp, "w", encoding="utf-8") as f:
                json.dump(self.data, f)
            os.replace(tmp, self.path)

    # --- config --------------------------------------------------------------
    @property
    def detector_config(self):
        return DetectorConfig.from_dict(self.data["detector"])

    @property
    def render_config(self):
        return RenderConfig.from_dict(self.data["render"])

    def update_settings(self, detector=None, render=None):
        with self.lock:
            if detector:
                d = self.data["detector"]
                d.update({k: v for k, v in detector.items() if k in d})
            if render:
                r = self.data["render"]
                r.update({k: v for k, v in render.items() if k in r})
            self.save()

    # --- timeline --------------------------------------------------------------
    @property
    def bounds(self):
        """(ts_min, ts_max): the video's real native-pts range. Falls back to
        container duration before the packet index exists."""
        idx = self.data.get("index")
        if idx:
            return idx["ts_min"], idx["ts_max"]
        return 0.0, (self.data["info"] or {}).get("duration", 0.0) or 0.0

    def ensure_index(self, progress=None):
        if not self.data.get("index"):
            idx = ffio.index_video(self.video_path, progress=progress)
            with self.lock:
                self.data["index"] = {k: v for k, v in idx.items()
                                      if k != "keyframes"}
                self.data["keyframes"] = idx["keyframes"]
                self.save()
        return self.data["index"]

    # --- sections -------------------------------------------------------------
    def add_section(self, start, end, kinds=None, snap=True):
        with self.lock:
            if snap:
                start, end = ffio.snap_to_keyframes(
                    self.data["keyframes"], start, end, self.bounds)
            else:
                lo, hi = self.bounds
                start = max(lo, min(float(start), hi))
                end = max(lo, min(float(end), hi))
                if end <= start:
                    raise ValueError("Section end must be after start "
                                     "(after clamping to the video range)")
            sid = str(self.data["next_section_id"])
            self.data["next_section_id"] += 1
            self.data["sections"][sid] = {
                "id": sid,
                "start": round(float(start), 6),
                "end": round(float(end), 6),
                "kinds": sorted(kinds or []),
                "created": time.time(),
                "prepared": False,
                "n_frames": 0,
                "pts": [],             # relative to section start
                "proxy": None,
                "thumbs": None,
                "cache_npy": None,
                "edits": {},           # ordinal(str) -> {"removed": b, "extended": b}
                "analysis": None,      # original-section analysis summary
                "check": None,         # last simulate verdict
                "preview": None,       # {path, verdict}
                "render": None,        # {path, verdict}
            }
            self.save()
            return self.data["sections"][sid]

    def section(self, sid):
        sec = self.data["sections"].get(str(sid))
        if sec is None:
            raise KeyError(f"No section {sid}")
        return sec

    def remove_section(self, sid):
        with self.lock:
            self.data["sections"].pop(str(sid), None)
            self.save()

    def update_section_bounds(self, sid, start, end, snap=True):
        """Change a section's time range. Invalidates everything derived from
        the old range (preparation, edits stay but ordinals shift, so they are
        cleared too — the range change means different frames)."""
        with self.lock:
            sec = self.section(sid)
            if snap:
                start, end = ffio.snap_to_keyframes(
                    self.data["keyframes"], float(start), float(end),
                    self.bounds)
            else:
                lo, hi = self.bounds
                start = max(lo, min(float(start), hi))
                end = max(lo, min(float(end), hi))
            if end <= start:
                raise ValueError("Section end must be after start")
            sec["start"] = round(float(start), 6)
            sec["end"] = round(float(end), 6)
            sec.update({"prepared": False, "n_frames": 0, "pts": [],
                        "proxy": None, "thumbs": None, "cache_npy": None,
                        "edits": {}, "analysis": None, "check": None,
                        "preview": None, "render": None})
            self.save()
            return sec

    @staticmethod
    def edits_signature(edits):
        return "|".join(
            f"{k}:{'r' if v.get('removed') else ''}{'e' if v.get('extended') else ''}"
            for k, v in sorted((edits or {}).items(), key=lambda kv: int(kv[0])))

    @staticmethod
    def render_stale(sec):
        """True if edits changed since the last full-res render."""
        r = sec.get("render")
        if not r:
            return False
        return r.get("edits_sig") != Project.edits_signature(sec.get("edits"))

    def sections_sorted(self):
        return sorted(self.data["sections"].values(), key=lambda s: s["start"])

    def section_dir(self, sid):
        d = os.path.join(self.workdir, f"section_{sid}")
        os.makedirs(d, exist_ok=True)
        return d

    def set_edits(self, sid, edits):
        with self.lock:
            sec = self.section(sid)
            clean = {}
            for k, v in edits.items():
                removed = bool(v.get("removed"))
                extended = bool(v.get("extended")) and not removed
                if removed or extended:
                    clean[str(int(k))] = {"removed": removed,
                                          "extended": extended}
            sec["edits"] = clean
            sec["check"] = None    # edits changed; old verdict is stale
            self.save()
            return clean
