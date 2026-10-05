// Parallel decoding with a built-in decoder: the sample range is split into
// groups of pictures at sync samples, each group goes to a Web Worker
// (softworker.js, which runs every one of them), and the pictures come
// back in presentation order.
//
// Protocol (main -> worker): {type:'init', desc, codec} once; {type:'decode', id,
// file, ts, offset, size, pts, minPts, maxPts, shrink} per group (samples in
// decode order; only pictures with minPts <= pts < maxPts are sent back, so
// the leading pictures of an open GOP come from the group that holds their
// references; `shrink`, {aw, ah} or null, has them made that small);
// {type:'credit', n} after consuming n pictures (the worker holds at most
// `window` unconsumed pictures; `reset: true` starts a pass with exactly
// n); {type:'cancel'}.
// Worker -> main: {type:'ready'} | {type:'error', message} |
// {type:'frame', id, pic} (a transferred I420 picture record: data, width,
// height, timestamp, colorSpace; or, for a job with `shrink`, kind 'rgba'
// at that size) | {type:'done', id, damaged, decodeMs, decoded, error?},
// which ends every job, `error` saying why one failed (the worker is then
// stopped: after a trap its WebAssembly module is not to be trusted).
import { breathe, rawPicture, shown, smallPicture, smallSize, waker } from './media.js';
import { profile } from './profile.js';
import { builtInFor } from './codecs.js';

/** How many decoder workers to run: leave a core for the page itself. */
export function defaultWorkerCount() {
  const forced = parseInt(new URLSearchParams(typeof location !== 'undefined' ? location.search : '').get('workers') || '', 10);
  if (forced > 0) return Math.min(forced, 16);
  const cores = (typeof navigator !== 'undefined' && navigator.hardwareConcurrency) || 2;
  return Math.max(1, Math.min(6, cores - 1));
}

/** A picture as a worker sent it: made small, or whole. */
function workerPicture(p) {
  const pic = p.kind === 'rgba' ? smallPicture(p.data, p.width, p.height, p.timestamp, p.from[0], p.from[1]) : rawPicture(p.data, p.width, p.height, p.timestamp, p.colorSpace);
  pic.damaged = !!p.damaged;
  return pic;
}

/** A worker of the built-in decoder for `codec`, started; `ready` settles once it can decode. */
function startWorker(movie, codec) {
  const w = new Worker(new URL('./softworker.js', import.meta.url), { type: 'module' });
  w.ready = new Promise((resolve, reject) => {
    const onMessage = (e) => {
      if (e.data.type === 'ready') {
        cleanup();
        resolve();
      } else if (e.data.type === 'error') {
        cleanup();
        reject(new Error(e.data.message));
      }
    };
    const onError = (e) => {
      cleanup();
      reject(new Error(e.message || 'the decoder worker failed to start'));
    };
    const cleanup = () => {
      w.removeEventListener('message', onMessage);
      w.removeEventListener('error', onError);
    };
    w.addEventListener('message', onMessage);
    w.addEventListener('error', onError);
    w.postMessage({ type: 'init', desc: movie.dx.track_description(movie.video.index), codec: codec.id });
  });
  return w;
}

export class SoftwarePool {
  constructor(movie, workers, codec) {
    this.movie = movie;
    this.workers = workers;
    this.codec = codec;
    this.busy = false;
  }

  /** Spawn and initialise the workers (rejects when workers cannot run the decoder). */
  static async create(movie, size = defaultWorkerCount()) {
    if (typeof Worker === 'undefined') throw new Error('Web Workers are not available');
    const codec = builtInFor(movie.video.codec);
    if (!codec) throw new Error(`there is no built-in decoder for ${movie.video.codec}`);
    const workers = [];
    try {
      for (let k = 0; k < size; k++) workers.push(startWorker(movie, codec));
      await Promise.all(workers.map((w) => w.ready));
    } catch (e) {
      for (const w of workers) w.terminate();
      throw e;
    }
    return new SoftwarePool(movie, workers, codec);
  }

  /**
   * One more worker, for a scan that finds it has a core to spare (a lane
   * of the browser's decoder it has set aside): it joins the pass under way
   * too. Resolves to whether it could start.
   */
  async grow() {
    if (this.closed) return false;
    const w = startWorker(this.movie, this.codec);
    try {
      await w.ready;
    } catch (e) {
      w.terminate();
      return false;
    }
    if (this.closed) {
      w.terminate();
      return false;
    }
    this.workers.push(w);
    if (this.joining) this.joining(w);
    return true;
  }

  close() {
    this.closed = true;
    for (const w of this.workers) w.terminate();
    this.workers = [];
    // a pass under way ends: its workers will not answer
    if (this.stopPass) this.stopPass();
  }

  /**
   * Split the samples [startIdx, endIdx) (decode order) into groups that
   * start at sync samples; small groups are merged with the next one.
   */
  groups(startIdx, endIdx) {
    const { pts, sync } = this.movie.v;
    const n = pts.length;
    const bounds = [startIdx];
    for (let i = startIdx + 1; i < endIdx; i++) if (sync[i] && i - bounds[bounds.length - 1] >= 4) bounds.push(i);
    bounds.push(endIdx);
    const groups = [];
    for (let k = 0; k + 1 < bounds.length; k++) {
      const a = bounds[k];
      const b = bounds[k + 1];
      let ext = b;
      let maxPts = Infinity;
      if (k + 2 < bounds.length) {
        // the next group's leading pictures (pts before its sync sample)
        // reference this group: decode them here, the next group drops them
        maxPts = pts[b];
        let j = b + 1;
        while (j < n && pts[j] < maxPts) j++;
        if (j > b + 1) ext = j;
      }
      groups.push({ a, ext, minPts: pts[a], maxPts });
    }
    return groups;
  }

  /**
   * How many decoded pictures each worker may hold for the page: a memory
   * budget shared by the workers, so that a worker can decode a group or
   * more ahead while the page consumes an earlier one (the page takes the
   * groups in order: a worker that has to stop a few pictures into its
   * group leaves the pool little faster than one worker). Pictures made
   * `small` ({aw, ah}, RGBA) cost little to hold: a long GOP's worth.
   */
  window(small = null) {
    const frameBytes = small ? small.aw * small.ah * 4 : Math.max(1, this.movie.width * this.movie.height * 1.5);
    const gb = (typeof navigator !== 'undefined' && navigator.deviceMemory) || 4;
    const budget = Math.min(768, Math.max(192, gb * 96)) * 1024 * 1024;
    const fit = Math.floor(budget / Math.max(1, this.workers.length) / Math.max(1, frameBytes));
    return small ? Math.max(64, Math.min(512, fit)) : Math.max(4, Math.min(256, fit));
  }

  /**
   * Decode the samples [startIdx, endIdx) and hand every picture with a
   * presentation time in [startSec, endSec) to `onFrame(frame, tSec)` in
   * presentation order. Returns the number of frames delivered.
   */
  async decodeRange(startIdx, endIdx, startSec, endSec, onFrame, opts = {}) {
    let given = false;
    const next = () => {
      if (given) return null;
      given = true;
      return { startIdx, endIdx, startSec, endSec };
    };
    return this.decodeStretches(next, onFrame, opts);
  }

  /**
   * Decode stretch after stretch as one pass: `next()` hands out the next
   * stretch, { startIdx, endIdx, startSec, endSec } (samples in decode
   * order; the pictures shown in [startSec, endSec) are kept), or null when
   * there are no more. It is asked only when a worker is free for more, so
   * what it has not handed out yet can still go elsewhere (a hybrid scan
   * gives it to another decoder). The pictures of consecutive stretches
   * reach `onFrame(frame, tSec)` in presentation order, with no pause for
   * the workers at the seams, and `onStretchDone(stretch)` hears when the
   * last picture of a stretch has been handed on. With `strict`, a damaged
   * picture fails the pass instead of being handed on (the pictures before
   * it have been); a job that failed fails it whatever `strict` says.
   * Returns the number of frames delivered.
   */
  async decodeStretches(next, onFrame, { cancel, raw = false, shrink = null, strict = false, onStretchDone = null } = {}) {
    if (this.busy) throw new Error('the decoder pool is busy');
    // (closed, or each of its workers stopped at an error)
    if (!this.workers.length) throw new Error(this.closed ? 'the decoder pool was closed' : 'the decoder pool has no workers left');
    this.busy = true;
    // pictures made small (raw ones for the detector only) cost little to hold
    const small = smallSize(raw, shrink);
    const window = this.window(small);
    const { pts, offset, size } = this.movie.v;
    const groups = [];
    const out = [];
    let exhausted = false;
    // the next stretch's groups, when a worker needs one
    const more = () => {
      while (!exhausted) {
        const s = next();
        if (!s) {
          exhausted = true;
          break;
        }
        const gs = this.groups(s.startIdx, s.endIdx);
        // a stretch with nothing to decode still ends, in its turn
        if (!gs.length) gs.push({ a: s.startIdx, ext: s.startIdx, minPts: 0, maxPts: 0 });
        gs.forEach((g, k) => {
          groups.push({ ...g, startSec: s.startSec, endSec: s.endSec, stretch: s, last: k === gs.length - 1 });
          out.push({ queue: [], done: false, damaged: 0, error: null, worker: null });
        });
        return true;
      }
      return false;
    };
    let nextJob = 0;
    let inflight = 0;
    const { wait, wake } = waker();
    // (close() ends the pass)
    this.stopPass = wake;
    const assign = (w) => {
      if (nextJob >= groups.length && !more()) return;
      const k = nextJob++;
      const g = groups[k];
      out[k].worker = w;
      inflight++;
      w.postMessage({ type: 'decode', id: k, file: this.movie.file, ts: this.movie.ts || null, offset: offset.slice(g.a, g.ext), size: size.slice(g.a, g.ext), pts: pts.slice(g.a, g.ext), minPts: g.minPts, maxPts: g.maxPts, shrink: small });
    };
    const handlers = new Map(); // worker -> its listener for this pass
    const listen = (w) => {
      const h = (e) => {
        const m = e.data;
        if (m.type === 'frame') out[m.id].queue.push(workerPicture(m.pic));
        else if (m.type === 'done') {
          const o = out[m.id];
          o.done = true;
          o.damaged = m.damaged;
          o.error = m.error || null;
          if (m.decoded) profile.add('sw.decode', m.decodeMs, m.decoded);
          inflight--;
          if (m.error) {
            console.warn(`built-in ${this.codec.name} decoder:`, m.error);
            // no more work for it: after a trap its module is not to be trusted
            w.terminate();
            this.workers = this.workers.filter((x) => x !== w);
          } else assign(w);
        }
        wake();
      };
      w.addEventListener('message', h);
      handlers.set(w, h);
      // this pass's window, whatever an earlier pass left unused
      w.postMessage({ type: 'credit', n: window, reset: true });
    };
    this.workers.forEach(listen);
    // a worker the pool gains while this pass runs (grow) joins it
    this.joining = (w) => {
      listen(w);
      assign(w);
    };
    let frames = 0;
    let damaged = 0;
    let stopped = false;
    try {
      for (const w of this.workers) assign(w);
      // a worker that finishes a group takes the next one, or asks for the
      // next stretch: once the page is past the last group there is no more
      for (let k = 0; k < groups.length; k++) {
        const o = out[k];
        const g = groups[k];
        for (;;) {
          if (cancel && cancel()) {
            stopped = true;
            break;
          }
          if (o.queue.length) {
            const pic = o.queue.shift();
            const t = pic.timestamp / 1e6;
            if (shown(t, g.startSec, g.endSec)) {
              if (strict && pic.damaged) throw new Error(`the built-in ${this.codec.name} decoder damaged the picture at ${t.toFixed(3)} s`);
              await onFrame(raw ? pic : pic.toVideoFrame(), t);
              frames++;
              await breathe();
            }
            o.worker.postMessage({ type: 'credit', n: 1 });
          } else if (o.done) break;
          else {
            if (this.closed) throw new Error('the decoder pool was closed');
            const tw = performance.now();
            await wait();
            profile.add('sw.wait', performance.now() - tw);
          }
        }
        if (stopped) break;
        damaged += o.damaged;
        if (o.error || (strict && o.damaged)) throw new Error(o.error ? `the built-in decoder failed: ${o.error}` : `the built-in decoder damaged ${o.damaged} picture${o.damaged === 1 ? '' : 's'}`);
        if (g.last && onStretchDone) await onStretchDone(g.stretch);
      }
    } finally {
      // stop whatever is still running and take the workers back (a closed
      // pool's are gone: nothing to wait for)
      exhausted = true;
      nextJob = groups.length;
      for (const w of this.workers) w.postMessage({ type: 'cancel' });
      while (inflight > 0 && !this.closed) await wait();
      for (const o of out) o.queue.length = 0;
      this.joining = null;
      this.stopPass = null;
      for (const [w, h] of handlers) w.removeEventListener('message', h);
      this.busy = false;
    }
    if (damaged) console.warn(`built-in ${this.codec.name} decoder: ${damaged} damaged pictures`);
    return frames;
  }
}
