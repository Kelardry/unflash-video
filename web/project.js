// The project: sections, marks and verdicts, persisted in IndexedDB as the
// work goes on, and to a project file on request (to move it to another
// browser or computer, or keep it past clearing the site's data). Frame
// caches live in memory only and are rebuilt by preparing a section again.

const DB_NAME = 'unflash';
const STORE = 'projects';
/** The kinds of violation a scan names a section for. */
const KINDS = ['flash', 'red', 'extended', 'pattern'];

/**
 * Where the trace of a project's scan is kept: beside its record, under a
 * key of its own (28 bytes a frame, 3-6 MB for an hour of video), so that
 * the saves after every check and mark write the record alone. (A
 * project's own key ends with its file's modified time, a number.)
 */
const traceKey = (key) => `${key}:trace`;
const isTraceKey = (key) => typeof key === 'string' && key.endsWith(':trace');

let opened = null;

/**
 * The page's connection to the database, opened on first use and kept for
 * every call after it (each call used to open one of its own and never
 * close it). Opened again after the browser closes it, or after another
 * page asks for a new version of the database (this one is closed then, so
 * as not to hold that page up).
 */
function openDb() {
  if (opened) return opened;
  const opening = new Promise((resolve, reject) => {
    const forget = () => {
      if (opened === opening) opened = null;
    };
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => {
      req.result.createObjectStore(STORE);
    };
    req.onsuccess = () => {
      const db = req.result;
      db.onclose = forget;
      db.onversionchange = () => {
        db.close();
        forget();
      };
      resolve(db);
    };
    req.onerror = () => {
      forget();
      reject(req.error);
    };
  });
  opened = opening;
  return opening;
}

async function idbGet(key) {
  const db = await openDb();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, 'readonly');
    const req = tx.objectStore(STORE).get(key);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

/**
 * `value` under `key`, and in the same transaction each `[key, value]` of
 * `more` (a null value deletes its key): all of them are written, or none.
 */
async function idbPut(key, value, more = []) {
  const db = await openDb();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, 'readwrite');
    const store = tx.objectStore(STORE);
    store.put(value, key);
    for (const [k, v] of more) {
      if (v == null) store.delete(k);
      else store.put(v, k);
    }
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
    // (a transaction that fails as it commits, out of space say, says so only by its abort)
    tx.onabort = () => reject(tx.error || new Error('the save was aborted'));
  });
}

/** `fn(key, value)` for every project saved in this browser (not the traces kept beside them). */
async function idbEach(fn) {
  const db = await openDb();
  return new Promise((resolve, reject) => {
    const req = db.transaction(STORE, 'readonly').objectStore(STORE).openCursor();
    req.onsuccess = () => {
      const cur = req.result;
      if (!cur) return resolve();
      if (!isTraceKey(cur.key)) fn(cur.key, cur.value);
      cur.continue();
    };
    req.onerror = () => reject(req.error);
  });
}

/** When a project was last saved in this browser (ms), 0 when none was: roughly when it was last used. */
export async function lastSavedAt() {
  let last = 0;
  await idbEach((key, value) => {
    const at = value && value.savedAt;
    if (at > last) last = at;
  });
  return last;
}

/**
 * The most recently saved project whose key starts with `prefix`, as
 * { key, value }, or null: the same file under another modified time (a
 * copy, a download again). Never a trace (idbEach passes over those).
 */
async function idbLatestLike(prefix) {
  let best = null;
  await idbEach((key, value) => {
    if (typeof key === 'string' && key.startsWith(prefix) && (!best || (value && value.savedAt) > (best.value.savedAt || 0))) best = { key, value };
  });
  return best;
}

export function projectKey(file) {
  return `${file.name}:${file.size}:${file.lastModified}`;
}

/** The same file whatever its modified time: its name and size. */
function projectKeyPrefix(key) {
  const parts = key.split(':');
  return parts.length >= 3 ? parts.slice(0, -1).join(':') + ':' : null;
}

/**
 * Drop a section's decoded frames (its caches and context); its marks and
 * frame times stay. The caches are freed, not cleared: clearing keeps the
 * allocation, and WebAssembly memory only ever grows, so a freed cache is
 * what lets the next prepare reuse the space.
 */
export function dropCaches(sec) {
  if (sec.cache) sec.cache.free();
  if (sec.softCache) sec.softCache.free();
  if (sec.blendCache) sec.blendCache.free();
  if (sec.ctx) {
    sec.ctx.lead.free();
    sec.ctx.tail.free();
  }
  sec.cache = null;
  sec.softCache = null;
  sec.softKey = null;
  sec.blendCache = null;
  sec.blendKey = null;
  sec.ctx = null;
  sec.prepared = false;
}

export class Project {
  constructor(key, bounds, keyframes) {
    this.key = key;
    this.profile = 'wcag_ext';
    this.sections = [];
    this.nextId = 1;
    this.scan = null; // { violations, summary, sections, frames, elapsedMs, profile, trace, ... }
    this.bounds = bounds;
    this.keyframes = keyframes;
    this.notifyMinutes = 0;
    // the trace kept under traceKey(key) as far as this page knows (null:
    // none; undefined: not known, so the next save writes it there or clears
    // the key) and its stamp (the time of the save that wrote it)
    this.traceKept = undefined;
    this.traceAt = 0;
  }

  /**
   * The project saved in this browser for this file; when there is none
   * under its exact key, the latest saved for the same name and size (the
   * file copied or downloaded again: `restoredFrom` says so), its scan's
   * trace with it.
   */
  static async load(key, bounds, keyframes) {
    const p = new Project(key, bounds, keyframes);
    try {
      let from = key;
      let saved = await idbGet(key);
      if (!saved) {
        const prefix = projectKeyPrefix(key);
        const like = prefix ? await idbLatestLike(prefix) : null;
        if (like && like.value) {
          saved = like.value;
          from = p.restoredFrom = like.key;
        }
      }
      if (saved) {
        p.restore(saved);
        // the trace, from beside the record (a record saved before it had a
        // key of its own holds it: the next save moves it out). Its stamp
        // must be the one the record names: a trace saved there since, by
        // another tab for a scan of its own, is not this scan's
        if (p.scan && !p.scan.trace && saved.traceAt) {
          const kept = await idbGet(traceKey(from));
          if (kept && kept.at === saved.traceAt) {
            p.scan.trace = kept.trace;
            // (restored from another key, the first save puts it under this one)
            if (from === key) {
              p.traceKept = kept.trace;
              p.traceAt = kept.at;
            }
          }
        }
      }
    } catch (e) {
      console.warn('could not load project', e);
    }
    return p;
  }

  /**
   * Take on what `toSaved` made (from this browser or a project file). A
   * project file is text anyone can write, and the page puts some of it into
   * its HTML: a section's id is made a number (one that is none is left
   * out), its kinds the ones a scan names, its verdict what toSaved keeps.
   */
  restore(saved) {
    this.profile = saved.profile || 'wcag_ext';
    this.nextId = saved.nextId || 1;
    this.scan = saved.scan || null;
    const sections = (saved.sections || []).filter((s) => s && Number.isFinite(Number(s.id)));
    this.sections = sections.map((s) => ({
      ...s,
      id: Number(s.id),
      kinds: Array.isArray(s.kinds) ? s.kinds.filter((k) => KINDS.includes(k)) : [],
      prepared: false,
      cache: null,
      softCache: null,
      blendCache: null,
      ctx: null,
      check: s.check ? summarizeCheck(s.check) : null,
      edits: s.edits || {},
      keep: s.keep || [],
      blend: s.blend || [],
      blendStrength: s.blendStrength == null ? null : s.blendStrength,
      soften: !!s.soften,
      pattern: s.pattern || null,
    }));
    this.nextId = Math.max(this.nextId, ...this.sections.map((s) => s.id + 1));
  }

  /**
   * Keep the project in this browser. The scan's trace goes under a key of
   * its own with the first save after it changes (a scan, a project file
   * loaded, a project restored from another key's), in the same transaction
   * as the record, stamped with that save's time, which every record after
   * it names (see load); a save after that writes the record alone.
   */
  async save() {
    const record = this.toSaved();
    const trace = (this.scan && this.scan.trace) || null;
    const fresh = trace !== this.traceKept;
    const at = fresh ? record.savedAt : this.traceAt;
    if (trace) record.traceAt = at;
    try {
      await idbPut(this.key, record, fresh ? [[traceKey(this.key), trace && { at, trace }]] : []);
      if (fresh) {
        this.traceKept = trace;
        this.traceAt = at;
      }
    } catch (e) {
      console.warn('could not save project', e);
    }
  }

  /**
   * What is kept of the project: everything but the decoded frames, and the
   * scan without its trace unless `trace` (a project file carries it; this
   * browser keeps it beside the record, see save).
   */
  toSaved({ trace = false } = {}) {
    const sections = this.sections.map((s) => ({
      id: s.id,
      start: s.start,
      end: s.end,
      kinds: s.kinds || [],
      edits: s.edits || {},
      keep: s.keep || [],
      blend: s.blend || [],
      blendStrength: s.blendStrength == null ? null : s.blendStrength,
      check: s.check ? summarizeCheck(s.check) : null,
      nFrames: s.nFrames || 0,
      pts: s.pts || null,
      warnings: s.warnings || [],
      custom: !!s.custom,
      soften: !!s.soften,
      pattern: s.pattern || null,
    }));
    let scan = this.scan;
    if (scan && scan.trace && !trace) {
      scan = { ...scan };
      delete scan.trace;
    }
    return { profile: this.profile, nextId: this.nextId, scan, sections, savedAt: Date.now() };
  }

  section(id) {
    return this.sections.find((s) => s.id === id) || null;
  }

  sectionsSorted() {
    return [...this.sections].sort((a, b) => a.start - b.start);
  }

  addSection(start, end, kinds = [], custom = false) {
    const [lo, hi] = this.bounds;
    start = Math.max(lo, Math.min(start, hi));
    end = Math.max(lo, Math.min(end, hi));
    if (end <= start) return null;
    const sec = {
      id: this.nextId++,
      start: Math.round(start * 1e6) / 1e6,
      end: Math.round(end * 1e6) / 1e6,
      kinds,
      edits: {},
      keep: [],
      blend: [],
      blendStrength: null,
      prepared: false,
      cache: null,
      softCache: null,
      blendCache: null,
      ctx: null,
      check: null,
      custom,
      soften: false,
      pattern: null,
    };
    this.sections.push(sec);
    return sec;
  }

  deleteSection(id) {
    const sec = this.section(id);
    if (!sec) return;
    dropCaches(sec);
    this.sections = this.sections.filter((s) => s.id !== id);
  }

  /** Sections whose checks read this one's edits: they need re-checking. */
  invalidateNeighbours(sec, seconds) {
    for (const o of this.sections) {
      if (o.id === sec.id) continue;
      if (o.end > sec.start - seconds && o.start < sec.end + seconds) {
        if (o.check) o.check.stale = true;
      }
    }
  }

  /** Bytes held by frame caches. */
  cacheBytes() {
    let b = 0;
    for (const s of this.sections) {
      if (s.cache) b += s.cache.byte_length();
      if (s.softCache) b += s.softCache.byte_length();
      if (s.blendCache) b += s.blendCache.byte_length();
      if (s.ctx) b += s.ctx.lead.byte_length() + s.ctx.tail.byte_length();
    }
    return b;
  }

  /** Drop caches of sections other than `keep` until under `budget` bytes. */
  evictCaches(keep, budget) {
    const order = this.sections.filter((s) => s.prepared && s.id !== (keep && keep.id)).sort((a, b) => (a.usedAt || 0) - (b.usedAt || 0));
    for (const s of order) {
      if (this.cacheBytes() <= budget) break;
      dropCaches(s);
    }
  }
}

/** The part of a check verdict worth keeping across reloads. */
function summarizeCheck(c) {
  return {
    safe: c.safe,
    wcag_safe: c.wcag_safe,
    flag_extended: c.flag_extended,
    flag_patterns: c.flag_patterns,
    pattern_thresh: c.pattern_thresh,
    soften: c.soften,
    soft_frames: c.soft_frames,
    soft_sigma: c.soft_sigma,
    violations: c.violations,
    after: c.after,
    elsewhere: c.elsewhere,
    flagged: c.flagged,
    spills: c.spills,
    profile: c.profile,
    detector_sig: c.detector_sig,
    context_notes: c.context_notes,
    frames: c.frames,
    stale: !!c.stale,
  };
}

// ---- project files ------------------------------------------------------------

const FILE_KIND = 'unflash-project';
const FILE_VERSION = 1;
const TYPED = { Float64Array, Float32Array, Uint32Array, Int32Array, Uint16Array, Int16Array, Uint8Array };

function toBase64(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

function fromBase64(b64) {
  const s = atob(b64);
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
  return out;
}

/** What identifies the video a project belongs to. */
function videoFingerprint(movie) {
  return {
    name: movie.name,
    size: movie.file.size,
    lastModified: movie.file.lastModified,
    frames: movie.frameCount,
    duration: Math.round(movie.duration * 1e6) / 1e6,
    width: movie.width,
    height: movie.height,
    codec: movie.video && movie.video.codec,
  };
}

/**
 * The project as a file's text: JSON, the video it belongs to, and the
 * project as the browser keeps it (typed arrays, such as the scan's
 * per-frame trace, as base64 of their bytes).
 */
export function projectFileText(project, movie) {
  const doc = { kind: FILE_KIND, version: FILE_VERSION, saved: new Date().toISOString(), video: videoFingerprint(movie), project: project.toSaved({ trace: true }) };
  return JSON.stringify(doc, (k, v) => (ArrayBuffer.isView(v) && !(v instanceof DataView) ? { $typed: v.constructor.name, b64: toBase64(new Uint8Array(v.buffer, v.byteOffset, v.byteLength)) } : v));
}

/** A project file's text read back: { video, saved } (saved as `toSaved` makes it). Throws with a message on anything else. */
export function readProjectFile(text) {
  let doc;
  try {
    doc = JSON.parse(text, (k, v) => {
      if (v && typeof v === 'object' && typeof v.$typed === 'string' && typeof v.b64 === 'string') {
        const T = TYPED[v.$typed];
        if (!T) throw new Error(`an array of an unknown type (${v.$typed})`);
        const bytes = fromBase64(v.b64);
        return new T(bytes.buffer, 0, bytes.byteLength / T.BYTES_PER_ELEMENT);
      }
      return v;
    });
  } catch (e) {
    throw new Error(`This is not an Unflash project file (${e.message}).`);
  }
  if (!doc || doc.kind !== FILE_KIND || !doc.project || !doc.video) throw new Error('This is not an Unflash project file.');
  if (doc.version > FILE_VERSION) throw new Error('This project file comes from a newer Unflash; reload the page to get the newest, then load it again.');
  if (!Array.isArray(doc.project.sections)) throw new Error('This project file has no sections list.');
  return { video: doc.video, saved: doc.project };
}

/**
 * Whether a project file's video is the open one: its frames and length must
 * match (the marks are on frames); the name and the size only say whether it
 * is the very same file. Returns { ok, same, why }.
 */
export function matchVideo(fp, movie) {
  const here = videoFingerprint(movie);
  const oneFrame = movie.medianDelta || 1 / 30;
  const framesOk = fp.frames === here.frames;
  const lengthOk = Math.abs((fp.duration || 0) - here.duration) <= oneFrame;
  const same = fp.name === here.name && fp.size === here.size;
  if (framesOk && lengthOk) return { ok: true, same };
  return { ok: false, same, why: `The project is for ${fp.name} (${fp.frames} frames, ${(fp.duration || 0).toFixed(3)} s); the open video has ${here.frames} frames and lasts ${here.duration.toFixed(3)} s. Open the video it was made for, then load it again.` };
}
