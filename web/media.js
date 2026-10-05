import { profile } from './profile.js';
import { builtInFor, loadDecoders } from './codecs.js';
import { canPlaySound, soundConfig } from './audiodec.js';
import { TS_BASE, readTsSample } from './ts.js';
// Demuxing (through the WASM MP4 parser, served byte ranges from the File)
// and decoding through WebCodecs.

export const tick = () => new Promise((r) => setTimeout(r, 0));

// A turn of the event loop that a hidden tab does not slow down: timers
// there fire once a second at most (Firefox, Chrome), messages at once.
const yieldChannel = typeof MessageChannel !== 'undefined' ? new MessageChannel() : null;
const yieldQueue = [];
if (yieldChannel) yieldChannel.port1.onmessage = () => yieldQueue.shift()();
const yieldTask = () =>
  yieldChannel
    ? new Promise((r) => {
        yieldQueue.push(r);
        yieldChannel.port2.postMessage(0);
      })
    : tick();

// The page's turns. A loop that feeds pictures to the detector one after
// another awaits promises that are already settled, which never lets the
// page draw, take a click or hear from its workers: with pictures waiting
// (a scan holds hundreds, decoded ahead) it can keep the thread for seconds,
// and Firefox then offers to stop the page. Such a loop calls breathe()
// after each picture, which gives the page a turn once the loop has had the
// thread TURN_MS on end. A ping through the yield channel says when the page
// last had a turn, whoever gave it, so a loop that waits anyway (for a
// decoder, the GPU) is not held up further. 30 ms: no task grows long (the
// browsers' mark is 50) and a scan is as fast as without the turns; at 12 a
// scan lost a tenth of its speed to the page drawing its progress.
const TURN_MS = 30;
let turnAt = 0;
let pinging = false;
const pinged = () => {
  pinging = false;
  turnAt = performance.now();
};
export async function breathe() {
  if (!yieldChannel) return;
  if (!pinging) {
    pinging = true;
    yieldQueue.push(pinged);
    yieldChannel.port2.postMessage(0);
  }
  if (performance.now() - turnAt >= TURN_MS) await yieldTask();
}

/**
 * Settles when `promise` does or after `ms`, whichever is first: a safety
 * net for waits on events that should come but might not (a lost device).
 */
export const orTimeout = (promise, ms) => Promise.race([promise, new Promise((r) => setTimeout(r, ms))]);

/**
 * A wait that an event ends: `wait()` sleeps until `wake()` (a wake with
 * no wait under way does nothing), or, given `ms`, until a timer that long
 * runs out, a safety net for an event that should come but might not. The
 * timer is cleared once the wait is over: one left running would end a
 * later wait for nothing.
 */
export function waker(ms = 0) {
  let resolve = null;
  let timer = 0;
  const wake = () => {
    if (resolve) {
      const r = resolve;
      resolve = null;
      clearTimeout(timer);
      r();
    }
  };
  const wait = () =>
    new Promise((r) => {
      resolve = r;
      if (ms) timer = setTimeout(wake, ms);
    });
  return { wait, wake };
}

/**
 * Reads sample bytes out of a File. A file up to `wholeLimit` bytes is read
 * whole the first time and kept, so every later pass over it (prepare,
 * export, verify) costs no file access at all; a larger one is read through
 * a window that follows the reads. A Movie keeps one reader for all its
 * passes: some browsers charge a good fraction of a second for the first
 * read of a file, and that is paid once rather than per pass. For a
 * transport stream (`ts`, as Movie.ts describes it) a sample's offset is a
 * place its bytes are gathered from (ts.js).
 */
export class ChunkReader {
  constructor(file, chunkSize = 8 * 1024 * 1024, wholeLimit = 64 * 1024 * 1024, parent = null, ts = null) {
    this.file = file;
    this.chunk = chunkSize;
    this.wholeLimit = wholeLimit;
    this.parent = parent;
    this.ts = ts || (parent && parent.ts) || null;
    this.whole = null; // the whole small file, read once (a promise, so readers at the same time share it)
    this.buf = null;
    this.start = 0;
    this.end = 0;
  }
  /** The bytes of a sample (at a place, for a transport stream), a view good until the next read. */
  read(offset, size) {
    if (this.ts && offset >= TS_BASE) return readTsSample((o, n) => this.readFile(o, n), this.ts, offset, size);
    return this.readFile(offset, size);
  }
  /** `size` bytes of the file from `offset`. */
  async readFile(offset, size) {
    if (this.file.size <= this.wholeLimit) {
      const root = this.parent || this;
      if (!root.whole) root.whole = root.file.slice(0, root.file.size).arrayBuffer().then((b) => new Uint8Array(b));
      const all = await root.whole;
      return all.subarray(offset, offset + size);
    }
    if (!(this.buf && offset >= this.start && offset + size <= this.end)) {
      const start = offset;
      const end = Math.min(this.file.size, Math.max(offset + size, offset + this.chunk));
      this.buf = new Uint8Array(await this.file.slice(start, end).arrayBuffer());
      this.start = start;
      this.end = end;
    }
    return this.buf.subarray(offset - this.start, offset - this.start + size);
  }
  /**
   * A reader of the same file with a window of its own (a small file read
   * whole is shared): for passes that read different parts of the file at
   * the same time, which would otherwise take turns re-reading one window.
   */
  fork() {
    return new ChunkReader(this.file, this.chunk, this.wholeLimit, this.parent || this, this.ts);
  }
  /** Forget the bytes read so far. */
  release() {
    this.whole = null;
    this.buf = null;
    this.start = 0;
    this.end = 0;
  }
}

function median(arr) {
  if (!arr.length) return 0;
  const v = Array.from(arr).sort((a, b) => a - b);
  const n = v.length;
  return n % 2 ? v[(n - 1) / 2] : (v[n / 2 - 1] + v[n / 2]) / 2;
}

/**
 * An opened video file (MP4 / MOV / M4V, MKV / WebM, an MPEG transport
 * stream): track info and sample tables. An MP4's index is read on its
 * own; a Matroska file or a transport stream keeps none, so it is read
 * through once (`onProgress(fraction)` follows that).
 */
export class Movie {
  static async open(file, wasm, { onProgress = null } = {}) {
    const dx = new wasm.Demuxer(file.size);
    // (the demuxer holds the index in WebAssembly memory: a file that can't
    // be opened lets go of it, an opened one on close())
    try {
      // through the reader the movie keeps: a window of the file at a time (a
      // small file whole), not a read for each box. A fragmented MP4 keeps its
      // index in a piece before every second or two of video; read box by box,
      // a two-hour recording took Firefox a minute and a half to open
      const reader = new ChunkReader(file);
      while (!dx.is_done()) {
        const need = dx.need();
        if (!need.length) break;
        const [off, len] = need;
        const buf = await reader.read(off, len);
        dx.feed(off, buf);
        if (onProgress) onProgress(dx.progress(), dx.container());
      }
      if (!dx.is_done()) throw new Error('The file ended before its index could be read');
      const info = JSON.parse(dx.movie_json());
      // a transport stream's samples are gathered from its packets (ts.js)
      const ts = info.format === 'mpegts' ? { packet: info.packet_size, size: file.size, tracks: info.tracks.map((t) => ({ pid: t.id, annexb: t.kind === 'video' })) } : null;
      reader.ts = ts;
      const vt = info.tracks.find((t) => t.kind === 'video' && t.samples > 0);
      if (!vt) {
        const unusable = info.tracks.find((t) => t.kind === 'video' && t.note);
        throw new Error(unusable ? `This file's video can't be read: ${unusable.note}` : 'No video track found in this file');
      }
      // the sound: the first track that can be played here (the file's default
      // first: the demuxer puts it first), else the first
      const audios = info.tracks.filter((t) => t.kind === 'audio' && t.samples > 0);
      let at = null;
      for (const t of audios) {
        if (!t.note && (await canPlaySound(soundConfig(t, dx.track_description(t.index))))) {
          at = t;
          break;
        }
      }
      const m = new Movie();
      // the tracks passed over for it, which can't be played here
      m.audioSkipped = at ? audios.slice(0, audios.indexOf(at)) : [];
      at = at || audios[0] || null;
      m.format = info.format || 'mp4';
      // tracks an MP4 export leaves out: subtitles, the other audio tracks
      m.subtitleTracks = info.tracks.filter((t) => t.kind === 'other' && /^S_/.test(t.codec)).length;
      m.audioTracks = audios.length;
      m.file = file;
      m.ts = ts;
      m.reader = reader;
      m.name = file.name;
      m.wasm = wasm;
      m.software = false;
      m.dx = dx;
      m.info = info;
      m.video = vt;
      m.audio = at;
      const table = (idx) => ({
        offset: dx.sample_table(idx, 'offset'),
        size: dx.sample_table(idx, 'size'),
        pts: dx.sample_table(idx, 'pts_us'),
        dts: dx.sample_table(idx, 'dts_us'),
        dur: dx.sample_table(idx, 'duration_us'),
        ptsTicks: dx.sample_table(idx, 'pts_ticks'),
        dtsTicks: dx.sample_table(idx, 'dts_ticks'),
        durTicks: dx.sample_table(idx, 'duration_ticks'),
        sync: dx.sample_table(idx, 'sync'),
      });
      m.v = table(vt.index);
      m.a = at ? table(at.index) : null;
      m.keyframes = Array.from(dx.keyframe_times(vt.index));
      // the real timeline: presentation order, not decode order
      const pts = Array.from(m.v.pts).sort((a, b) => a - b);
      const deltas = [];
      for (let i = 1; i < pts.length; i++) deltas.push(pts[i] - pts[i - 1]);
      let med = median(deltas.filter((d) => d > 0)) || 1e6 / 30;
      // Matroska rounds times to its tick (a millisecond, usually): 29.97 fps
      // shows as gaps of 33 and 34 ms, so the file's own frame duration, when
      // it gives one close to the median, is the better figure
      const nominal = vt.frame_duration > 0 ? (vt.frame_duration / vt.timescale) * 1e6 : 0;
      if (nominal > 0 && Math.abs(nominal - med) <= 0.15 * med) med = nominal;
      m.medianDelta = med / 1e6;
      m.fps = 1e6 / med;
      m.tsMin = pts.length ? pts[0] / 1e6 : 0;
      m.tsMax = pts.length ? (pts[pts.length - 1] + med) / 1e6 : 0;
      m.bounds = [m.tsMin, m.tsMax];
      m.duration = m.tsMax - m.tsMin;
      m.frameCount = pts.length;
      m.width = vt.width;
      m.height = vt.height;
      return m;
    } catch (e) {
      dx.free();
      throw e instanceof Error ? e : new Error(String(e));
    }
  }

  /** Release the decoder workers (if any) and the index; the Movie is not usable afterwards. */
  close() {
    this.closed = true;
    if (this.pool) {
      this.pool.close();
      this.pool = null;
    }
    for (const w of this.decodeWorkers || []) w.worker.terminate();
    this.decodeWorkers = [];
    this.poolPromise = null;
    this.reader.release();
    if (this.dx) {
      this.dx.free();
      this.dx = null;
    }
  }

  /** A WebCodecs decode worker for one pass: an idle one, or a new one (kept for the next pass). */
  decodeWorker() {
    this.decodeWorkers = this.decodeWorkers || [];
    const idle = this.decodeWorkers.find((w) => !w.busy);
    if (idle) {
      idle.busy = true;
      return idle;
    }
    const slot = { worker: new Worker(new URL('./decodeworker.js', import.meta.url), { type: 'module' }), busy: true };
    this.decodeWorkers.push(slot);
    return slot;
  }

  /**
   * The worker pool for the built-in decoder, created on first use; null
   * when workers cannot run it (the page then decodes inline).
   */
  async softwarePool() {
    // (one whose workers have all stopped, each at an error, makes way for a new one)
    if (this.pool && !this.pool.workers.length && !this.pool.busy) {
      this.pool.close();
      this.pool = null;
      this.poolPromise = null;
    }
    if (this.pool) return this.pool;
    if (this.poolFailed || this.closed) return null;
    if (!this.poolPromise) {
      this.poolPromise = (async () => {
        try {
          const { SoftwarePool } = await import('./h264pool.js');
          const pool = await SoftwarePool.create(this);
          // (closed meanwhile: its workers go too)
          if (this.closed) {
            pool.close();
            return null;
          }
          this.pool = pool;
          return pool;
        } catch (e) {
          console.warn(`built-in ${(builtInFor(this.video.codec) || { name: '' }).name} decoder: decoding on the page instead of in workers:`, e && e.message ? e.message : e);
          this.poolFailed = true;
          return null;
        }
      })();
    }
    return this.poolPromise;
  }

  decoderConfig() {
    const desc = this.dx.track_description(this.video.index);
    const cfg = {
      codec: this.video.codec,
      codedWidth: this.video.width,
      codedHeight: this.video.height,
      hardwareAcceleration: 'no-preference',
      optimizeForLatency: false,
    };
    if (desc.length) cfg.description = desc;
    return cfg;
  }

  /**
   * Whether the file can be decoded: by WebCodecs, or, for a codec the
   * browser has no decoder for (H.264 in some Chromium builds, HEVC in most
   * browsers, VP9 or AV1 in some), by the app's built-in decoder for it
   * (`software: true`; `builtIn` names it). With `forceBuiltIn` set, the
   * built-in decoder is used whenever there is one. VideoFrame itself has
   * to exist either way.
   */
  async decoderSupport() {
    this.software = false;
    this.builtIn = null;
    if (typeof VideoDecoder === 'undefined' || typeof VideoFrame === 'undefined') return { supported: false, software: false, reason: 'WebCodecs is not available in this browser' };
    let reason = '';
    const b = builtInFor(this.video.codec);
    if (this.forceBuiltIn && b) reason = 'the built-in decoder was asked for';
    else {
      try {
        const r = await VideoDecoder.isConfigSupported(this.decoderConfig());
        if (r.supported) return { supported: true, software: false, reason: '' };
        reason = `this browser cannot decode ${this.video.codec}`;
      } catch (e) {
        reason = String(e);
      }
    }
    if (b) {
      try {
        const desc = this.dx.track_description(this.video.index);
        const info = JSON.parse(b.id === 'h264' ? this.wasm.h264_probe(desc) : (await loadDecoders()).probe(b.id, desc));
        this.software = true;
        this.builtIn = b;
        this.softwareInfo = info;
        return { supported: true, software: true, reason: '' };
      } catch (e) {
        reason += `, and the built-in ${b.name} decoder cannot read it: ${e && e.message ? e.message : e}`;
      }
    }
    return { supported: false, software: false, reason };
  }
}

/**
 * Decode every frame with presentation time in [startSec, endSec) and hand
 * each VideoFrame to `onFrame(frame, tSec)` (which must close it). Frames
 * arrive in presentation order. With `raw`, the built-in decoder's pictures
 * come as plain I420 buffers (see rawPicture) instead of VideoFrames.
 * Decoding starts at the last keyframe at or before `startSec`, or at
 * sample `fromIndex` (decode order) when given. With `inline`, the built-in
 * decoder decodes on the page rather than in its workers. With `raw` and
 * `shrink` ({aw, ah}: the detector's analysis size) the pictures may come
 * made that small; with `workers` as well they always do, the browser's
 * decoder running in a decode worker (pictures a scan holds on to: a
 * VideoFrame held stops its decoder).
 */
export async function decodeRange(movie, startSec, endSec, onFrame, { cancel, raw = false, fromIndex = null, reader = null, shrink = null, inline = false, workers = false } = {}) {
  if (movie.software) return decodeRangeSoftware(movie, startSec, endSec, onFrame, { cancel, raw, fromIndex, reader, shrink, inline });
  // pictures for the detector alone: decoded and copied in a worker where
  // the detector would copy them on the page anyway (and, with `shrink`
  // ({aw, ah}), made that small there)
  if (raw && (movie.decodeInWorkers || workers) && typeof Worker !== 'undefined') return decodeRangeWorker(movie, startSec, endSec, onFrame, { cancel, fromIndex, shrink });
  const cfg = movie.decoderConfig();
  reader = reader || movie.reader;
  const { pts, offset, size, sync, dur } = movie.v;
  const { startIdx, endIdx } = sampleRange(movie, startSec, endSec, fromIndex);
  let error = null;
  const queue = [];
  // the loop sleeps until the decoder does something: a picture out, an
  // input taken off its queue, an error (a timer polling instead would
  // cost the 4 ms browsers clamp repeated timeouts to, many times a second;
  // the safety timer is for a browser that sends no dequeue events)
  const { wait, wake } = waker(20);
  const decoder = new VideoDecoder({
    output: (f) => {
      queue.push(f);
      wake();
    },
    error: (e) => {
      error = e;
      wake();
    },
  });
  if ('ondequeue' in decoder) decoder.addEventListener('dequeue', wake);
  decoder.configure(cfg);
  let frames = 0;
  const pump = async () => {
    while (queue.length) {
      const f = queue.shift();
      const t = f.timestamp / 1e6;
      if (!shown(t, startSec, endSec)) {
        f.close();
        continue;
      }
      await onFrame(f, t);
      frames++;
      await breathe();
    }
  };
  let i = startIdx;
  try {
    while (i < endIdx && !error && !(cancel && cancel())) {
      const full = () => decoder.decodeQueueSize > 12 || queue.length > 6;
      while (full() && !error) {
        await pump();
        if (!full() || error) break;
        const tw = performance.now();
        await wait();
        profile.add('decode.wait', performance.now() - tw);
      }
      if (error) break;
      const tr = performance.now();
      const data = await reader.read(offset[i], size[i]);
      profile.add('read', performance.now() - tr);
      decoder.decode(new EncodedVideoChunk({ type: sync[i] ? 'key' : 'delta', timestamp: pts[i], duration: dur[i], data }));
      i++;
      await pump();
    }
    if (!error && !(cancel && cancel())) {
      // the last pictures, handed on as they come rather than all at the end
      const tf = performance.now();
      let flushed = false;
      decoder.flush().then(
        () => {
          flushed = true;
          wake();
        },
        (e) => {
          error = error || e;
          flushed = true;
          wake();
        }
      );
      while (!flushed) {
        await pump();
        if (!flushed && !queue.length) await wait();
      }
      profile.add('decode.flush', performance.now() - tf);
    }
    await pump();
  } finally {
    while (queue.length) queue.shift().close();
    try {
      decoder.close();
    } catch (e) {
      /* already closed */
    }
  }
  if (error) throw error instanceof Error ? error : new Error(String(error));
  return frames;
}

/**
 * The samples (decode order) a decode of the pictures shown in [startSec,
 * endSec) runs over: from sample `fromIndex`, or else the last sync sample
 * at or before `startSec`, up to the first sample both decoded and shown at
 * or after `endSec` (so the leading pictures of the next GOP, shown before
 * `endSec` but decoded after its sync sample, are included).
 */
export function sampleRange(movie, startSec, endSec, fromIndex = null) {
  const { pts, dts } = movie.v;
  const n = pts.length;
  const startIdx = fromIndex !== null && fromIndex !== undefined ? fromIndex : movie.dx.sync_before(movie.video.index, Math.max(startSec, movie.tsMin));
  const endUs = endSec * 1e6;
  let endIdx = startIdx;
  while (endIdx < n && !(dts[endIdx] >= endUs && pts[endIdx] >= endUs)) endIdx++;
  return { startIdx, endIdx };
}

/** Whether a picture at `t` (seconds) is one of those shown in [startSec, endSec), give or take a time's rounding. */
export const shown = (t, startSec, endSec) => t >= startSec - 1e-6 && t < endSec - 1e-9;

let workerJobs = 0;

/**
 * decodeRange through a decode worker (decodeworker.js): the worker
 * decodes and copies, the page feeds the copies on and hands each buffer
 * back. At most four pictures wait for the page at a time (32 made small).
 */
async function decodeRangeWorker(movie, startSec, endSec, onFrame, { cancel, fromIndex = null, shrink = null } = {}) {
  const { pts, offset, size, sync, dur } = movie.v;
  const { startIdx, endIdx } = sampleRange(movie, startSec, endSec, fromIndex);
  const endUs = endSec * 1e6;
  const slot = movie.decodeWorker();
  const worker = slot.worker;
  const id = ++workerJobs;
  const small = smallSize(true, shrink);
  const inbox = [];
  let done = false;
  let failed = null;
  const { wait, wake } = waker(250);
  const onMessage = (e) => {
    const m = e.data;
    if (m.id !== id) return;
    if (m.type === 'frame') inbox.push(m);
    else if (m.type === 'done') done = true;
    else if (m.type === 'error') failed = new Error(m.message);
    wake();
  };
  const onError = (e) => {
    failed = new Error(`the decode worker stopped: ${e.message || e}`);
    wake();
  };
  worker.addEventListener('message', onMessage);
  worker.addEventListener('error', onError);
  worker.postMessage({
    type: 'decode',
    id,
    file: movie.file,
    ts: movie.ts || null,
    config: movie.decoderConfig(),
    offset: offset.slice(startIdx, endIdx),
    size: size.slice(startIdx, endIdx),
    pts: pts.slice(startIdx, endIdx),
    dur: dur.slice(startIdx, endIdx),
    sync: sync.slice(startIdx, endIdx),
    startUs: startSec * 1e6,
    endUs,
    // pictures made small cost little to hold: the decoder runs further
    // ahead of the detector
    window: small ? 32 : 4,
    shrink: small,
  });
  const credit = (buffer) => (buffer ? worker.postMessage({ type: 'credit', n: 1, buffer }, [buffer]) : worker.postMessage({ type: 'credit', n: 1 }));
  let frames = 0;
  let stopped = false;
  let healthy = true;
  const stop = () => {
    if (!stopped) {
      stopped = true;
      worker.postMessage({ type: 'cancel' });
    }
  };
  try {
    for (;;) {
      if (cancel && cancel()) stop();
      if (inbox.length) {
        const m = inbox.shift();
        if (stopped) {
          if (m.frame) m.frame.close();
          credit(m.pic ? m.pic.data : null);
          continue;
        }
        const t = (m.frame ? m.frame.timestamp : m.pic.timestamp) / 1e6;
        if (m.frame) {
          try {
            await onFrame(m.frame, t);
          } finally {
            credit(null);
          }
        } else {
          await onFrame(workerPicture(m.pic, credit), t);
        }
        frames++;
        await breathe();
        continue;
      }
      if (failed) throw failed;
      if (done) break;
      // the page waiting for the worker (decoding is what holds it up)
      const t0 = performance.now();
      await wait();
      profile.add('worker.wait', performance.now() - t0);
    }
  } catch (e) {
    stop();
    // the worker's job is waited out before it takes another
    const until = performance.now() + 5000;
    while (!done && !failed && performance.now() < until) {
      while (inbox.length) {
        const m = inbox.shift();
        if (m.frame) m.frame.close();
        credit(m.pic ? m.pic.data : null);
      }
      await wait();
    }
    healthy = done || !!failed;
    throw e;
  } finally {
    worker.removeEventListener('message', onMessage);
    worker.removeEventListener('error', onError);
    if (healthy && !failed) slot.busy = false;
    else {
      worker.terminate();
      movie.decodeWorkers = (movie.decodeWorkers || []).filter((w) => w !== slot);
    }
  }
  return frames;
}

/** A picture a decode worker copied, shaped for Feeder.feedRaw; closing it hands the buffer back. */
function workerPicture(p, credit) {
  let returned = false;
  if (p.shrunk) {
    profile.add('worker.copy', p.shrunk.copyMs);
    profile.add('worker.shrink', p.shrunk.shrinkMs);
  }
  return {
    raw: true,
    kind: p.kind,
    format: p.format,
    codedWidth: p.width,
    codedHeight: p.height,
    displayWidth: p.width,
    displayHeight: p.height,
    timestamp: p.timestamp,
    colorSpace: p.colorSpace,
    data: new Uint8Array(p.data, 0, p.bytes),
    // the buffer goes back to the worker on close: copy what is to be kept
    lent: true,
    layout: p.layout,
    detail: p.shrunk ? `${p.format} ${p.shrunk.from[0]}×${p.shrunk.from[1]}, made ${p.width}×${p.height} in a decode worker` : `${p.format}, copied in a decode worker`,
    close() {
      if (returned) return;
      returned = true;
      credit(p.data);
    },
  };
}

/**
 * The layout words Detector.feed_yuv takes: [format (0 I420, 1 NV12), y_off,
 * y_stride, u_off, u_stride, v_off, v_stride, matrix (0 BT.601, 1 BT.709),
 * full_range], from WebCodecs plane layouts and a VideoColorSpace (the
 * matrix is guessed from the picture height when unknown, as players do).
 */
export function yuvLayoutWords(format, planes, colorSpace, height) {
  const nv12 = format === 'NV12';
  const cs = colorSpace || {};
  let bt709;
  if (cs.matrix === 'bt709' || cs.matrix === 'bt2020-ncl') bt709 = 1;
  else if (cs.matrix === 'smpte170m' || cs.matrix === 'bt470bg' || cs.matrix === 'fcc') bt709 = 0;
  else bt709 = height > 576 ? 1 : 0;
  const u = planes[1];
  const v = nv12 ? planes[1] : planes[2];
  return Uint32Array.from([nv12 ? 1 : 0, planes[0].offset, planes[0].stride, u.offset, u.stride, v.offset, v.stride, bt709, cs.fullRange ? 1 : 0]);
}

/**
 * A picture as packed I420 planes in memory (from the built-in decoder),
 * shaped enough like a VideoFrame for the detector's Feeder (`raw` marks
 * it); toVideoFrame() makes a real one for the encoder or a canvas.
 */
export function rawPicture(data, width, height, timestampUs, colorSpace) {
  const cw = (width + 1) >> 1;
  const ch = (height + 1) >> 1;
  const planes = [
    { offset: 0, stride: width },
    { offset: width * height, stride: cw },
    { offset: width * height + cw * ch, stride: cw },
  ];
  return {
    raw: true,
    format: 'I420',
    codedWidth: width,
    codedHeight: height,
    displayWidth: width,
    displayHeight: height,
    timestamp: timestampUs,
    colorSpace,
    data,
    layout: yuvLayoutWords('I420', planes, colorSpace, height),
    close() {},
    toVideoFrame() {
      const init = { format: 'I420', codedWidth: width, codedHeight: height, timestamp: timestampUs };
      if (colorSpace) init.colorSpace = colorSpace;
      return new VideoFrame(data, init);
    },
  };
}

/** The analysis size pictures for the detector alone are made small to, or null. */
export function smallSize(raw, shrink) {
  return raw && shrink && shrink.aw > 0 && shrink.ah > 0 ? { aw: shrink.aw, ah: shrink.ah } : null;
}

/**
 * A picture the built-in decoder made the detector's size (RGBA, `aw`×`ah`
 * from `fromW`×`fromH`), in its workers or on the page, shaped like a raw
 * one for the Feeder.
 */
export function smallPicture(data, aw, ah, timestampUs, fromW, fromH) {
  return {
    raw: true,
    kind: 'rgba',
    format: 'RGBA',
    codedWidth: aw,
    codedHeight: ah,
    displayWidth: aw,
    displayHeight: ah,
    timestamp: timestampUs,
    data,
    detail: `${fromW}×${fromH}, made ${aw}×${ah} by the built-in decoder`,
    close() {},
  };
}

/**
 * The picture built-in decoder `dec` just produced, copied out of the
 * memory of its module `mod`: { data (packed I420), width, height,
 * colorSpace }, a record a worker can send as it is.
 */
export function decodedPicture(mod, dec) {
  let colorSpace = null;
  try {
    colorSpace = JSON.parse(dec.color_json());
  } catch (e) {
    /* default colour space */
  }
  return { data: new Uint8Array(mod.wasm_memory().buffer, dec.frame_ptr(), dec.frame_len()).slice(), width: dec.width(), height: dec.height(), colorSpace };
}

/**
 * Decode stretch after stretch with the built-in decoder through `pool` (a
 * SoftwarePool of its own), whatever decoder the movie normally uses: a
 * hybrid scan's built-in lane. `next()` hands out { startSec, endSec,
 * fromIndex } (as decodeRange takes them) or null; the pictures reach
 * `onFrame` in order, made the detector's size (`shrink`) in the workers,
 * fully decoded (the deblocking filter too, so they are the pictures the
 * browser's decoder gives). With `strict` (as it is unless said otherwise)
 * a damaged picture fails the pass. `onStretchDone(stretch)` hears when a
 * stretch (next()'s record, with anything else it carries) has handed on
 * its last picture.
 */
export function decodeStretchesBuiltIn(movie, pool, next, onFrame, { cancel, shrink = null, strict = true, onStretchDone = null } = {}) {
  const stretch = () => {
    const s = next();
    return s ? { ...s, ...sampleRange(movie, s.startSec, s.endSec, s.fromIndex) } : null;
  };
  return pool.decodeStretches(stretch, onFrame, { cancel, raw: true, shrink, strict, onStretchDone });
}

/**
 * The built-in decoder for `codec` (BUILT_IN's id) from its module `mod`,
 * for a track with configuration record `desc`. (H.264's keeps its
 * deblocking filter: the fast mode is bench.html's.)
 */
export function builtInDecoder(mod, codec, desc) {
  return codec === 'h264' ? new mod.H264Decoder(desc, false) : new mod.SoftDecoder(codec, desc);
}

/**
 * Samples `from` to `to` (decode order) through built-in decoder `dec`
 * (H.264's when `h264`, else the decoders module's SoftDecoder), their
 * pictures handed to `give(pic)` in presentation order: the decode loop of
 * the workers and of the page. `read(i)` gives the bytes of sample i, and
 * `picture(i)` makes the decoder's current picture, which sample i shows
 * (at `pts[i]`), into the one handed on, for the samples `wanted(i)`.
 * H.264's pictures come one a sample, in decode order, and each waits for
 * every one shown before it (the samples told apart by index: two may
 * share a time); the other decoders tag each picture with the sample that
 * shows it, and give at most their reorder depth of them before one shown
 * earlier. A picture the decoder calls damaged says so (`damaged`), as
 * does every one after a sample it could not take. Stops once `stopped()`;
 * `timed(ms)` hears how long each decode took. Returns how many pictures
 * were damaged and samples could not be taken.
 */
export async function decodeBuiltIn(dec, h264, pts, from, to, { read, wanted, picture, give, stopped, timed }) {
  let damaged = 0;
  // after a sample the decoder could not take, what refers to it is not to be trusted
  let tainted = false;
  // H.264's: the wanted samples in presentation order, and their pictures
  // as they come (null: none); the others': pictures by presentation time
  const order = [];
  const ready = new Map();
  let next = 0;
  const held = [];
  if (h264) {
    for (let i = from; i < to; i++) if (wanted(i)) order.push(i);
    order.sort((a, b) => pts[a] - pts[b]);
  }
  const reorder = h264 ? 0 : dec.reorder_depth();
  /** The decoder's current picture, which sample `i` shows. */
  const take = (i) => {
    // each picture says whether it is damaged, so that a scan can stop at it
    const bad = dec.frame_damaged() || tainted;
    if (bad) damaged++;
    if (!wanted(i)) return;
    const pic = picture(i);
    if (bad) pic.damaged = true;
    if (h264) ready.set(i, pic);
    else held.push(pic);
  };
  const drain = (n) => {
    for (let k = 0; k < n && dec.next(); k++) take(dec.frame_pts());
  };
  /** Hand on the pictures whose turn it is (`all`: every one left). */
  const release = async (all) => {
    if (h264) {
      while (next < order.length && ready.has(order[next]) && !stopped()) {
        const pic = ready.get(order[next]);
        ready.delete(order[next++]);
        if (pic) await give(pic);
      }
      return;
    }
    held.sort((a, b) => a.timestamp - b.timestamp);
    while (held.length > (all ? 0 : reorder) && !stopped()) await give(held.shift());
  };
  for (let i = from; i < to && !stopped(); i++) {
    const data = await read(i);
    const t0 = performance.now();
    let n = 0;
    try {
      // (H.264's says whether the sample's picture is ready, the others' how many pictures are)
      n = h264 ? Number(dec.decode(data, pts[i] / 1e6)) : dec.decode(data, i);
    } catch (e) {
      damaged++;
      tainted = true;
    }
    timed(performance.now() - t0);
    if (!h264) drain(n);
    else if (n) take(i);
    // no picture for this sample: none to wait for
    else if (wanted(i)) ready.set(i, null);
    await release(false);
  }
  if (!h264 && !stopped()) {
    try {
      drain(dec.flush());
    } catch (e) {
      damaged++;
    }
    await release(true);
  }
  return damaged;
}

/**
 * The same as decodeRange, through the built-in decoder in WebAssembly: in
 * its workers, or on the page when they cannot run or are busy (or with
 * `inline`). Samples are decoded in file (decode) order and the pictures
 * handed out in presentation order (decodeBuiltIn).
 */
async function decodeRangeSoftware(movie, startSec, endSec, onFrame, { cancel, raw = false, fromIndex = null, reader = null, shrink = null, inline = false } = {}) {
  const { startIdx, endIdx } = sampleRange(movie, startSec, endSec, fromIndex);
  // `inline`: on the page even with workers to hand (a simulated hybrid scan's lanes)
  const pool = inline ? null : await movie.softwarePool();
  // (pictures for the detector are made small in the workers)
  if (pool && !pool.busy) return pool.decodeRange(startIdx, endIdx, startSec, endSec, onFrame, { cancel, raw, shrink });
  reader = reader || movie.reader;
  const { id, name } = movie.builtIn;
  const mod = id === 'h264' ? movie.wasm : await loadDecoders();
  const { pts, offset, size } = movie.v;
  const d = builtInDecoder(mod, id, movie.dx.track_description(movie.video.index));
  // pictures for the detector alone made small here, as the workers make them
  const small = smallSize(raw, shrink);
  let frames = 0;
  let damaged = 0;
  try {
    if (small) d.set_shrink(small.aw, small.ah);
    damaged = await decodeBuiltIn(d, id === 'h264', pts, startIdx, endIdx, {
      read: async (i) => {
        // (the page has a turn every few samples)
        if (i % 4 === 0) await yieldTask();
        return reader.read(offset[i], size[i]);
      },
      wanted: (i) => shown(pts[i] / 1e6, startSec, endSec),
      picture: (i) => {
        if (small) return smallPicture(d.small(), small.aw, small.ah, pts[i], d.width(), d.height());
        const p = decodedPicture(mod, d);
        return rawPicture(p.data, p.width, p.height, pts[i], p.colorSpace);
      },
      give: async (pic) => {
        await onFrame(raw ? pic : pic.toVideoFrame(), pic.timestamp / 1e6);
        frames++;
      },
      stopped: () => !!(cancel && cancel()),
      timed: (ms) => profile.add('sw.decode', ms),
    });
  } finally {
    d.free();
  }
  if (damaged) console.warn(`built-in ${name} decoder: ${damaged} damaged pictures`);
  return frames;
}
