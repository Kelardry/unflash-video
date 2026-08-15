"""Project state: one project per source video, stored as JSON in a work
directory next to the video (``<name>.unflash/``)."""

import hashlib
import json
import os
import threading
import time

from .config import DetectorConfig, RenderConfig
from . import ffio


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


class Project:
    def __init__(self, video_path):
        self.video_path = os.path.abspath(video_path)
        self.workdir = workdir_for(video_path)
        self.lock = threading.RLock()
        self.data = {
            "video_path": self.video_path,
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
        self._load()
        if self.data["info"] is None:
            self.data["info"] = ffio.probe(self.video_path)
            self.save()

    # --- persistence ---------------------------------------------------------
    @property
    def path(self):
        return os.path.join(self.workdir, "project.json")

    def _load(self):
        if os.path.exists(self.path):
            try:
                with open(self.path, "r", encoding="utf-8") as f:
                    stored = json.load(f)
                if os.path.abspath(stored.get("video_path", "")) == self.video_path:
                    self.data.update(stored)
            except (json.JSONDecodeError, OSError):
                pass

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
