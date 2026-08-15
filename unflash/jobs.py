"""Tiny background-job runner for the local server."""

import threading
import time
import traceback
import uuid


class Job:
    def __init__(self, name, fn):
        self.id = uuid.uuid4().hex[:12]
        self.name = name
        self.fn = fn
        self.status = "queued"       # queued | running | done | error | cancelled
        self.progress = 0.0
        self.message = ""
        self.result = None
        self.error = None
        self.created = time.time()
        self.finished = None
        self._cancel = threading.Event()

    def set_progress(self, p, message=None):
        self.progress = max(0.0, min(1.0, float(p)))
        if message is not None:
            self.message = message

    def cancelled(self):
        return self._cancel.is_set()

    def cancel(self):
        self._cancel.set()

    def run(self):
        self.status = "running"
        try:
            self.result = self.fn(self)
            self.status = "cancelled" if self.cancelled() else "done"
            self.progress = 1.0
        except Exception as e:  # noqa: BLE001 - reported to the UI
            self.status = "error"
            self.error = f"{e}"
            traceback.print_exc()
        finally:
            self.finished = time.time()

    def to_dict(self):
        return {
            "id": self.id, "name": self.name, "status": self.status,
            "progress": round(self.progress, 4), "message": self.message,
            "error": self.error, "result": self.result,
        }


class JobManager:
    def __init__(self):
        self.jobs = {}
        self.lock = threading.Lock()

    def start(self, name, fn):
        job = Job(name, fn)
        with self.lock:
            self.jobs[job.id] = job
        t = threading.Thread(target=job.run, daemon=True, name=f"job-{name}")
        t.start()
        return job

    def get(self, job_id):
        return self.jobs.get(job_id)

    def active(self):
        return [j for j in self.jobs.values() if j.status in ("queued", "running")]
