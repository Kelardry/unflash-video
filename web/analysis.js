// Scanning, section preparation, the instant check, the suggesters: the
// same flow as the Python reference (analysis.py / editing.py), driving the
// WASM detector over WebCodecs frames or cached section frames.

import { breathe, decodeRange, decodeStretchesBuiltIn, sampleRange } from './media.js';
import { profile } from './profile.js';
import { dropCaches } from './project.js';
import { triageChunks } from './triage.js';

/** A segment shorter than this many run-ups is not worth its run-up. */
const MIN_SEGMENT_RUNUPS = 4;

/**
 * Run `run(k, stop)` for k from 0 to n - 1 at the same time and wait for
 * every one of them. Once one fails, `stop()` turns true for the others,
 * as it does on a cancel, so they end at their next picture (a decode told
 * to stop returns quietly) instead of going on with detectors and caches
 * the caller frees or uses again after. Resolves to what each returned;
 * throws the first failure, or 'cancelled' when `cancel()` turned true.
 */
async function together(n, run, cancel) {
  let failed = null;
  const stop = () => failed !== null || !!(cancel && cancel());
  const settled = await Promise.allSettled(
    Array.from({ length: n }, (_, k) =>
      run(k, stop).catch((e) => {
        if (failed === null) failed = e;
        throw e;
      })
    )
  );
  if (failed !== null) throw failed;
  if (cancel && cancel()) throw new Error('cancelled');
  return settled.map((r) => r.value);
}

/** What a scan draws of each frame on the timeline and the monitor. */
const newTrace = () => ({ t: [], hazard: [], hazardRed: [], ext: [], lum: [], pattern: [] });

/** The detector's records onto `trace`, those from `from` (seconds) on. */
function addRecords(trace, records, from = -Infinity) {
  for (const r of records) {
    if (r.t < from - 1e-9) continue;
    trace.t.push(r.t);
    trace.hazard.push(r.hazard);
    trace.hazardRed.push(r.hazard_red);
    trace.ext.push(Math.max(r.ext, r.ext_red));
    trace.lum.push(r.lum);
    trace.pattern.push(r.pattern);
  }
}

/**
 * How pictures decoded in workers reach the detector `f`: made its
 * analysis size there (unless tests say not to), all it needs.
 */
const shrinkFor = (movie, f) => (movie.shrinkInWorkers === false ? null : { aw: f.aw, ah: f.ah });

/**
 * What every scan returns, from its detector's `result` and its trace: the
 * sections around the violations and the timeline's summary, and the
 * profile, reported and kept for the debug report (`what`: how the scan
 * ran, as the profile says it).
 */
function scanResult(env, movie, result, trace, frames, elapsedMs, what) {
  const { wasm, config } = env;
  profile.report(`scan of ${(movie.file && movie.file.name) || 'the file'} (${what})`, frames, elapsedMs);
  // kept for the debug report (the next job starts the profile afresh)
  const profileText = profile.text(`scan (${what})`, frames, elapsedMs);
  const profileOps = profile.summary();
  // no keyframe snapping: the export re-encodes, so sections can follow the
  // flashing exactly instead of growing to the nearest keyframes
  const sections = JSON.parse(wasm.violations_to_sections(JSON.stringify(result.violations), config, movie.tsMin, movie.tsMax, new Float64Array()));
  const summary = JSON.parse(wasm.timeline_summary(JSON.stringify(result), movie.tsMin, movie.tsMax, 1.0));
  return { result, sections, summary, trace, frames, elapsedMs, profileText, profileOps };
}

/**
 * Whole-video scan. With `segments` above one and a `makeFeeder`, the file
 * is cut into that many spans scanned at the same time, each by its own
 * decoder and detector, every span after the first starting a run-up early
 * (the same run-up a section check gets) so that the detector's state at
 * the seam is the state a run from the start would have reached; the
 * results are then joined exactly. Returns { result, sections, summary,
 * trace, frames, elapsedMs, segments }; a cancelled scan, which saw only
 * part of the file, throws instead.
 */
export async function scanMovie(env, movie, { onProgress, onPartial = null, cancel, segments = 1, makeFeeder = null, moreFeeders = null, forceSegments = false, chunked = null } = {}) {
  if (chunked) return scanChunked(env, movie, { onProgress, onPartial, cancel, makeFeeder, moreFeeders, chunked });
  const { wasm, config, feeder } = env;
  profile.reset();
  const started = performance.now();
  const runup = wasm.context_seconds(config);
  const end = movie.tsMax + 1;
  const span = end - movie.tsMin;
  let nseg = Math.max(1, Math.floor(segments));
  if (!makeFeeder && !moreFeeders) nseg = 1;
  // a forced count (tests, benchmarks) is taken as given
  if (!forceSegments) nseg = Math.min(nseg, Math.max(1, Math.floor(span / (MIN_SEGMENT_RUNUPS * runup))));
  const bounds = [];
  for (let k = 0; k < nseg; k++) bounds.push({ from: movie.tsMin + (span * k) / nseg, to: k + 1 < nseg ? movie.tsMin + (span * (k + 1)) / nseg : end });
  // detectors from `moreFeeders` are lent (kept by the caller), from `makeFeeder` made for this scan
  const feeders = [feeder];
  if (nseg > 1 && moreFeeders) feeders.push(...(await moreFeeders(nseg - 1)));
  else for (let k = 1; k < nseg; k++) feeders.push(await makeFeeder());
  const traces = bounds.map(newTrace);
  const counts = bounds.map(() => 0);
  const total = Math.max(1, movie.frameCount);
  // the spans' traces in order: the scan's own, made once at the end
  const merged = () => {
    const out = newTrace();
    for (const tr of traces) for (const key of Object.keys(out)) for (const v of tr[key]) out[key].push(v);
    return out;
  };
  // what the timeline draws while the scan runs: one trace that grows by the
  // points each span adds (in no particular order, which a drawing does not
  // need). Merging every span afresh at each report cost time in proportion
  // to the square of the video's length: half a minute of an hour's scan.
  const live = newTrace();
  const sent = bounds.map(() => 0);
  const grown = () => {
    for (let k = 0; k < traces.length; k++) {
      const tr = traces[k];
      for (const key of Object.keys(live)) for (let i = sent[k]; i < tr.t.length; i++) live[key].push(tr[key][i]);
      sent[k] = tr.t.length;
    }
    return live;
  };
  const report = () => {
    const count = counts.reduce((a, b) => a + b, 0);
    if (onProgress) onProgress(count / total, grown(), count, performance.now() - started);
    profile.reportEvery(5000, 'scan so far', count, performance.now() - started);
  };
  const runOne = async (k, stop) => {
    const f = feeders[k];
    const { from, to } = bounds[k];
    const tr = traces[k];
    f.reset();
    let count = 0;
    // segments read different parts of the file: a window each
    const reader = k > 0 ? movie.reader.fork() : null;
    await decodeRange(
      movie,
      k === 0 ? from : Math.max(movie.tsMin, from - runup),
      to,
      async (frame, t) => {
        await f.videoFrame(frame, t, false);
        if (t >= from - 1e-9) counts[k] = ++count;
        if (count % 30 === 0) {
          // (not the run-up's: those are the previous segment's frames)
          addRecords(tr, f.records(), from);
          report();
        }
      },
      // the detector needs only its analysis size: pictures decoded in
      // workers are made that small there
      { cancel: stop, raw: true, reader, shrink: shrinkFor(movie, f) }
    );
    await f.drain();
    addRecords(tr, f.records(), from);
    return { from, result: f.finish(nseg > 1) };
  };
  let parts;
  try {
    // (a segment that fails stops the others before their detectors are freed, or used by the next job)
    parts = await together(nseg, runOne, cancel);
  } finally {
    if (!moreFeeders) for (let k = 1; k < feeders.length; k++) feeders[k].det.free();
  }
  const count = counts.reduce((a, b) => a + b, 0);
  const elapsed = performance.now() - started;
  let result;
  if (nseg === 1) {
    result = parts[0].result;
  } else {
    result = JSON.parse(wasm.merge_scan_segments(config, movie.width, movie.height, JSON.stringify(parts)));
    // the merged run's own statistics are the trace; the rest need not stay
    result.frame_stats = {};
  }
  return { ...scanResult(env, movie, result, merged(), count, elapsed, `${movie.width}×${movie.height}, ${feeder.backend}${nseg > 1 ? `, ${nseg} segments` : ''}`), segments: nseg };
}

/** A chunked scan cuts the file into chunks about this long (seconds of video): what a decoder takes at a time. */
export const CHUNK_S = 10;

/**
 * Bytes of pictures a chunked scan may hold for its detector, decoded ahead
 * of it: 96 MB for each GB of memory the browser says the machine has, at
 * least 384 MB, or 512 MB when it does not say. Browsers say 8 GB at most,
 * so that comes to 768 MB at most (the cap of 1 GB is for one that says
 * more).
 */
function holdBudget() {
  const gb = typeof navigator !== 'undefined' ? navigator.deviceMemory : 0;
  return (gb ? Math.min(1024, Math.max(384, gb * 96)) : 512) * 1024 * 1024;
}

/**
 * Where a chunked scan cuts the file: at sync samples about `chunkS` seconds
 * apart (none within half that of the end). Chunk c holds the pictures shown
 * in [t[c], t[c + 1]); its decode starts at sample idx[c] (a sync sample,
 * but for the first chunk), and it holds about n[c] pictures.
 */
export function scanChunks(movie, chunkS = CHUNK_S) {
  const { pts, sync } = movie.v;
  const count = pts.length;
  const t = [movie.tsMin];
  const idx = [0];
  for (let i = 1; i < count; i++) {
    if (!sync[i]) continue;
    const ti = pts[i] / 1e6;
    if (ti - t[t.length - 1] >= chunkS && movie.tsMax - ti >= chunkS / 2) {
      t.push(ti);
      idx.push(i);
    }
  }
  // (past the last picture, so that a decode up to there takes it in)
  t.push(movie.tsMax + 1);
  const n = idx.map((a, c) => (c + 1 < idx.length ? idx[c + 1] : count) - a);
  return { t, idx, n, length: idx.length };
}

/**
 * Which chunk the lanes of a chunked scan decode next. The detector takes
 * the chunks in file order, from `cur` on (see advance), so the lanes
 * decode the chunks it will want next and their pictures wait for it:
 * `bytes` a picture, and at most `budget` bytes of pictures held for chunks
 * after the detector's own. `n` is each chunk's pictures, `t` where each
 * starts (and, last, where the file ends). Triage (`order`, likeliest to
 * flash first, the first `hot` of them hot) adds early looks: a hot chunk
 * decoded long before the detector gets there, with the chunks before it
 * as the run-up (`runup` seconds) an early look needs and the hot chunks
 * straight after it, all held for the detector like any others, using at
 * most `hotShare` of the budget.
 */
export class ChunkPicker {
  constructor(n, t, { bytes = 0, budget = Infinity, order = null, hot = 0, runup = 0, hotShare = 0.6 } = {}) {
    this.n = n;
    this.t = t;
    this.nc = n.length;
    this.bytes = bytes;
    this.budget = budget;
    this.hotBudget = budget * hotShare;
    this.runup = runup;
    this.taken = new Uint8Array(this.nc);
    // what each chunk's pictures may hold until the detector has taken them, and whether an early look's
    this.held = new Float64Array(this.nc);
    this.early = new Uint8Array(this.nc);
    this.used = 0;
    this.usedEarly = 0;
    this.cur = 0;
    this.left = this.nc;
    this.hotList = order ? order.slice(0, hot) : [];
    this.isHot = new Uint8Array(this.nc);
    for (const c of this.hotList) this.isHot[c] = 1;
    /** The chunks in the order they were taken. */
    this.log = [];
    /** The early looks: { first, from, last }. */
    this.runs = [];
  }

  need(c) {
    return this.n[c] * this.bytes;
  }

  take(c, early) {
    this.taken[c] = 1;
    this.left--;
    this.log.push(c);
    // the detector's own chunk passes straight through it
    const b = c === this.cur ? 0 : this.need(c);
    this.held[c] = b;
    this.used += b;
    if (early) {
      this.early[c] = 1;
      this.usedEarly += b;
    }
    return c;
  }

  drop(c) {
    this.used -= this.held[c];
    if (this.early[c]) this.usedEarly -= this.held[c];
    this.held[c] = 0;
    this.early[c] = 0;
  }

  /** The detector is on chunk `c`: the chunks before it hold nothing any more. */
  advance(c) {
    for (let k = this.cur; k < c && k < this.nc; k++) this.drop(k);
    this.cur = c;
  }

  /**
   * The first chunk nobody has, for a lane decoding for the detector; -1
   * when every chunk is taken, -2 when the budget has no room for it yet
   * (the detector's own chunk always has room). With `fits`, the first free
   * chunk, in order, that `fits(c)` and the budget has room for beside the
   * free chunks before it (which other lanes will take); -3 when none in
   * the budget's reach does (-2 still when the first has no room).
   */
  ahead(fits = null) {
    let c = this.cur;
    while (c < this.nc && this.taken[c]) c++;
    if (c >= this.nc) return -1;
    if (!fits) {
      if (c !== this.cur && this.used + this.need(c) > this.budget) return -2;
      return this.take(c, false);
    }
    // (the chunks passed over are the other lanes' to take, and need room too)
    let passed = 0;
    let passedOver = false;
    for (; c < this.nc; c++) {
      if (this.taken[c]) continue;
      const need = c === this.cur ? 0 : this.need(c);
      if (this.used + passed + need > this.budget) return passedOver ? -3 : -2;
      if (fits(c)) return this.take(c, false);
      passedOver = true;
      passed += need;
    }
    return -3;
  }

  /**
   * An early look at the likeliest hot chunk still free that the lanes
   * would not get to soon anyway (`near`: how many chunks after the first
   * free one they will), with its run-up and the hot chunks after it free
   * too: { first, from, last } (first..from-1 the run-up), or null.
   */
  hotRun(near = 1) {
    let lo = this.cur;
    while (lo < this.nc && this.taken[lo]) lo++;
    lo += near;
    for (const h of this.hotList) {
      if (this.taken[h] || h < lo) continue;
      let first = h;
      while (first > 0 && this.t[h] - this.t[first] < this.runup && !this.taken[first - 1]) first--;
      // a run-up cut short by a chunk someone else has (the start of the file will do)
      if (first > 0 && this.t[h] - this.t[first] < this.runup) continue;
      let last = h;
      while (last + 1 < this.nc && !this.taken[last + 1] && this.isHot[last + 1]) last++;
      let cost = 0;
      for (let c = first; c <= last; c++) cost += c === this.cur ? 0 : this.need(c);
      if (this.usedEarly + cost > this.hotBudget || this.used + cost > this.budget) continue;
      for (let c = first; c <= last; c++) this.take(c, true);
      const run = { first, from: h, last };
      this.runs.push(run);
      return run;
    }
    return null;
  }

  /** Chunk `c` back to be taken again (its decoder failed part way). */
  release(c) {
    if (!this.taken[c]) return;
    this.taken[c] = 0;
    this.left++;
    this.drop(c);
  }
}

/** A lane's decoder gave a picture that cannot wait for the detector (a VideoFrame held stops its decoder). */
class NotHoldable extends Error {}

/**
 * A whole-video scan in chunks: the file is cut into chunks at sync
 * samples (scanChunks) and decoded by several lanes at once, `chunked.hw`
 * of the browser's decoder (each in a decode worker) and, given
 * `chunked.pool` (a SoftwarePool of the scan's own), one of the built-in
 * decoder, each picture made the detector's size where it is decoded. The
 * scan's detector (env.feeder) takes the chunks in file order, one after
 * another, so the result is exactly what a scan in one piece gives,
 * whichever lane decoded what and in whatever order: a ChunkPicker has the
 * lanes decode the chunks it will want next, and the pictures wait for it
 * (at most `chunked.budget`, else holdBudget(), bytes of them). With
 * `chunked.order` 'triage' a lane first takes early looks at the chunks
 * triage.js finds likeliest to flash, each with the chunks before it as a
 * run-up, on a detector of its own (`moreFeeders` / `makeFeeder`): what
 * they hold is known long before the scan gets there, and their pictures
 * wait for the scan's detector like any others, so nothing is decoded
 * twice. `onPartial({ until, violations, early })` hears what the scan has
 * found so far: exactly up to `until` (seconds), what the early looks found
 * after that (`early`: an early look has just ended). A built-in decoder
 * that fails (a damaged picture) gives its chunks back to the browser's
 * decoder, which goes on from its last picture. Pictures that cannot be
 * held (a decoder that gives only VideoFrames) make it a scan in one piece.
 * Returns what scanMovie does (and throws, as it does, when cancelled), and
 * `chunked`: the chunks, the order they were taken in, the early looks and
 * what each lane did. For tests, `chunked.sim` stands the built-in
 * decoder, on the page, in for the browser's (a browser without H.264 in
 * WebCodecs, such as the test browser, can then run a hybrid scan),
 * `chunked.failBuiltIn` makes the built-in decoder fail at its tenth
 * picture, `chunked.slow` (ms) holds back every picture of the browser's
 * lanes, as Firefox's copies out of the GPU do, and `chunked.steal` false
 * keeps a lane from taking over another's chunk.
 */
async function scanChunked(env, movie, { onProgress, onPartial = null, cancel, makeFeeder = null, moreFeeders = null, chunked }) {
  const { wasm, config, feeder } = env;
  profile.reset();
  const started = performance.now();
  const now = () => performance.now();
  const runup = wasm.context_seconds(config);
  const chunkS = chunked.chunkS || CHUNK_S;
  const chunks = scanChunks(movie, chunkS);
  const nc = chunks.length;
  const triage = chunked.order === 'triage' ? triageChunks(movie, chunks) : null;
  const pool = chunked.pool || null;
  const nhw = Math.max(1, Math.floor(chunked.hw || 1));
  const nlanes = nhw + (pool ? 1 : 0);
  // pictures made the detector's size where they are decoded (unless tests say not to)
  const shrink = shrinkFor(movie, feeder);
  const picBytes = shrink ? shrink.aw * shrink.ah * 4 : Math.ceil(movie.width * movie.height * 1.5);
  const budget = chunked.budget || holdBudget();
  const picker = new ChunkPicker(chunks.n, chunks.t, { bytes: picBytes, budget, order: triage ? triage.order : null, hot: triage ? triage.hot : 0, runup });
  // the early looks' detectors, lent (`moreFeeders`) or made for this scan
  const lookers = triage && triage.hot ? Math.min(2, nlanes) : 0;
  const lookFeeders = [];
  if (lookers && moreFeeders) lookFeeders.push(...(await moreFeeders(lookers)));
  else if (lookers && makeFeeder) for (let k = 0; k < lookers; k++) lookFeeders.push(await makeFeeder());
  const freeLookers = lookFeeders.slice();

  const lanes = [];
  for (let k = 0; k < nlanes; k++) {
    const builtIn = !!pool && k === nlanes - 1;
    lanes.push({
      kind: builtIn ? 'built-in' : 'browser',
      frames: 0, // pictures it gave the detector
      chunks: 0,
      looks: 0,
      ms: 0,
      t0: 0,
      failed: null,
      // in its loop, picking work (only such a lane takes a chunk the rebalancing gives it)
      running: false,
      // stretches handed to its decoder and not yet done, in order; how many of them feed the detector next
      stretches: [],
      feeding: 0,
      // lanes that decode on the page read different parts of the file: a window each
      reader: builtIn ? null : movie.reader.fork(),
      tested: false,
      // time spent decoding (ms; the pass under way from passT0), for its speed
      decodeMs: 0,
      passT0: 0,
      // waiting for work, and work handed to it (a chunk taken over from a slower lane)
      idle: false,
      pending: null,
      steals: 0,
      stolen: 0,
      // set aside by the rebalancing: the other lanes finish every chunk in reach sooner
      aside: false,
      asideMs: 0,
      asideT0: 0,
      gave: false,
    });
  }

  // each chunk's pictures, in order, until the detector takes them
  const slots = [];
  for (let c = 0; c < nc; c++) slots.push({ pics: [], head: 0, got: 0, done: false, lastT: -Infinity, wake: null });
  const wakeSlot = (s) => {
    if (s.wake) {
      const w = s.wake;
      s.wake = null;
      w();
    }
  };
  const wakeAll = () => slots.forEach(wakeSlot);
  // lanes waiting for room in the budget
  let waiting = [];
  const changed = () => {
    const ws = waiting;
    waiting = [];
    for (const w of ws) w();
  };
  const whenChanged = () => new Promise((r) => waiting.push(r));

  let failure = null;
  const stop = () => failure !== null || !!(cancel && cancel());
  const halt = (e) => {
    if (!failure) failure = e;
    changed();
    wakeAll();
  };
  const chunkOf = (t) => {
    let lo = 0;
    let hi = nc - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (chunks.t[mid] <= t) lo = mid;
      else hi = mid - 1;
    }
    return lo;
  };

  const total = Math.max(1, movie.frameCount);
  const trace = newTrace();
  let pushed = 0; // pictures the lanes have decoded for the detector
  let fed = 0; // and it has taken
  let heldBytes = 0;
  let peak = 0;
  // the frames decoded so far (while a lone lane looks early the detector waits)
  const report = () => {
    if (onProgress) onProgress(Math.min(1, (pushed + fed) / (2 * total)), trace, pushed, now() - started);
    profile.reportEvery(5000, 'scan so far', pushed, now() - started);
  };
  const reports = (res, v) => (v.kind === 'extended' ? res.flag_extended : v.kind === 'pattern' ? res.flag_patterns : true);
  // what the early looks found, and when
  const early = [];
  const looks = [];
  const partial = (fromLook) => {
    if (!onPartial) return;
    const res = feeder.partialVerdict();
    const until = trace.t.length ? trace.t[trace.t.length - 1] : movie.tsMin;
    const found = res.violations.filter((v) => reports(res, v));
    onPartial({ until, violations: [...found, ...early.filter((v) => v.start > until)], early: fromLook });
  };

  /** Chunks `first..last` are `lane`'s to decode, as one stretch (`look`: an early look's). */
  const give = (lane, first, last, look = null) => {
    const s = { startSec: chunks.t[first], endSec: chunks.t[last + 1], fromIndex: chunks.idx[first], first, last, look };
    lane.stretches.push(s);
    if (!look) lane.feeding++;
    return s;
  };
  /**
   * The next piece of work for `lane`: a stretch to decode, 'wait' (for
   * room), or null (nothing left for it). A lone lane takes early looks
   * first, while they fit; with more lanes, the chunk the detector is on
   * comes first, and a lane looks early while another decodes for the
   * detector.
   */
  const pick = (lane) => {
    if (stop() || lane.failed) return null;
    if (lane.pending) {
      const s = lane.pending;
      lane.pending = null;
      lane.stretches.push(s);
      lane.feeding++;
      setAside(lane, false);
      return s;
    }
    const curFree = picker.cur < nc && !picker.taken[picker.cur];
    if (freeLookers.length && (nlanes === 1 || (!curFree && lanes.some((l) => l !== lane && l.feeding > 0)))) {
      const run = picker.hotRun(nlanes);
      if (run) {
        const f = freeLookers.pop();
        f.reset();
        return give(lane, run.first, run.last, { from: run.from, feeder: f, t0: now() });
      }
    }
    const c = picker.ahead(rebalancing ? soonest(lane) : null);
    if (c >= 0) {
      setAside(lane, false);
      return give(lane, c, c);
    }
    if (c === -3) {
      setAside(lane, true);
      return 'wait';
    }
    if (c === -2) return 'wait';
    // every chunk is taken: a lane stays while another decodes one, to take it over if that one is slow
    return stealing && lanes.some((l) => l !== lane && l.stretches.some((s) => !s.look && !s.cut)) ? 'wait' : null;
  };

  /**
   * The rebalancing: which chunks `lane`, free now, should take. The free
   * chunks go, in order, each to the lane that would finish it first (what
   * each lane already has to finish counted, and its pictures a second so
   * far): `lane` takes the first that falls to it within the budget's
   * reach, and none if the others would finish them all sooner (a lane of
   * Firefox's decoder, whose copies are slow, beside the built-in decoder);
   * then it is set aside, and the built-in decoder gets a worker for the
   * core it leaves. Until every lane has been timed, the chunks go in
   * order, as they come.
   */
  const soonest = (lane) => {
    const mine = rate(lane);
    // (a lane that has left its loop picks nothing more: a chunk given back after it left must not wait for it)
    const others = lanes.filter((l) => l !== lane && l.running && !l.failed);
    if (!mine || !others.length || others.some((l) => !rate(l))) return null;
    // when each lane will be free, and the lane each free chunk falls to
    const free = new Map(others.map((l) => [l, busyFor(l)]));
    free.set(lane, 0);
    const rates = new Map([...others.map((l) => [l, rate(l)]), [lane, mine]]);
    return (c) => {
      let best = null;
      let bestAt = Infinity;
      for (const [l, at] of free) {
        const done = at + chunks.n[c] / rates.get(l);
        if (done < bestAt - 1e-9 || (done <= bestAt + 1e-9 && l === lane)) {
          best = l;
          bestAt = done;
        }
      }
      if (best === lane) return true;
      free.set(best, bestAt);
      return false;
    };
  };
  /** How long `lane` has yet to decode what it has (s), at its rate so far. */
  const busyFor = (lane) => {
    const r = rate(lane);
    let left = 0;
    for (const s of lane.stretches) {
      if (s.cut) continue;
      for (let c = s.first; c <= s.last; c++) left += Math.max(0, chunks.n[c] - slots[c].got);
    }
    if (lane.pending) left += chunks.n[lane.pending.first];
    return r ? left / r : 0;
  };
  const rebalancing = chunked.rebalance !== false && nlanes > 1;
  let grown = 0;
  /** Set `lane` aside (or back to work); a lane of the browser's decoder set aside gives its core to the built-in decoder. */
  const setAside = (lane, aside) => {
    if (aside === lane.aside) return;
    lane.aside = aside;
    if (aside) lane.asideT0 = now();
    else lane.asideMs += now() - lane.asideT0;
    if (aside && !lane.gave && lane.kind === 'browser' && pool && !pool.closed && pool.workers.length < (chunked.poolMax || 0)) {
      lane.gave = true;
      pool.grow().then((ok) => {
        if (ok) grown++;
      });
    }
  };

  /** A lane's pictures a second while it decodes, once there is enough to tell. */
  const rate = (lane) => {
    const ms = lane.decodeMs + (lane.passT0 ? now() - lane.passT0 : 0);
    return lane.frames >= 10 && ms >= 50 ? (lane.frames * 1000) / ms : null;
  };
  const stealing = chunked.steal !== false;
  let steals = 0;
  /**
   * Chunk `c`, the one the detector waits for, taken over from the
   * browser's lane decoding it by an idle lane that will finish it well
   * before: the slow lane stops at its next picture, and the other goes on
   * from the keyframe at or before the last picture in (what it decodes
   * again up to there is dropped, as a chunk another decoder began always
   * is). In Firefox, whose browser lanes copy every picture out of the GPU,
   * the built-in decoder otherwise sits idle: the pictures the slow lanes
   * will hold have the budget, and the detector waits on them.
   */
  const steal = (c) => {
    if (!stealing || stop() || c >= nc || slots[c].done) return;
    const owner = lanes.find((l) => l.kind === 'browser' && l.stretches.length && !l.stretches[0].look && !l.stretches[0].cut && l.stretches[0].first === c);
    const ro = owner ? rate(owner) : null;
    if (!ro) return;
    // the fastest idle lane; one not yet timed (kept idle by the budget
    // from the start) only if no timed one is idle, and for a long wait
    let thief = null;
    let rt = 0;
    for (const l of lanes) {
      if (l === owner || !l.idle || l.failed || l.pending) continue;
      const r = rate(l);
      if (r ? r > rt : !thief) {
        thief = l;
        rt = r || 0;
      }
    }
    if (!thief) return;
    const slot = slots[c];
    const fps = movie.fps || 30;
    const resumed = slot.lastT > chunks.t[c];
    const from = resumed ? slot.lastT : chunks.t[c];
    const idx = resumed ? Math.max(chunks.idx[c], movie.dx.sync_before(movie.video.index, slot.lastT)) : chunks.idx[c];
    const left = Math.max(1, (chunks.t[c + 1] - from) * fps);
    const again = Math.max(0, (from - movie.v.pts[idx] / 1e6) * fps);
    // (a quarter of a second for the other lane to get going)
    const ownerLeft = left / ro;
    if (rt ? (left + again) / rt + 0.25 > 0.7 * ownerLeft : ownerLeft < 1.5) return;
    owner.stretches[0].cut = true;
    owner.stolen++;
    thief.pending = { startSec: chunks.t[c], endSec: chunks.t[c + 1], fromIndex: idx, first: c, last: c, look: null };
    thief.steals++;
    steals++;
    changed();
  };

  /** A picture kept for the detector: a decode worker's buffer goes back to it, so that one is copied. */
  const hold = (pic) => {
    const data = pic.lent ? pic.data.slice() : pic.data;
    pic.close();
    return { raw: true, kind: pic.kind, format: pic.format, codedWidth: pic.codedWidth, codedHeight: pic.codedHeight, displayWidth: pic.displayWidth, displayHeight: pic.displayHeight, timestamp: pic.timestamp, colorSpace: pic.colorSpace, layout: pic.layout, detail: pic.detail, data, close() {} };
  };
  /** Chunks `from..to` of a stretch have all their pictures. */
  const chunksDone = (from, to) => {
    for (let c = from; c <= to; c++) {
      if (slots[c].done) continue;
      slots[c].done = true;
      wakeSlot(slots[c]);
    }
  };
  const endLook = async (lane, s) => {
    const f = s.look.feeder;
    await f.drain();
    const res = f.partialVerdict();
    const from = chunks.t[s.look.from];
    const to = chunks.t[s.last + 1];
    const found = res.violations.filter((v) => v.end >= from && v.start < to && reports(res, v));
    early.push(...found);
    looks.push({ first: s.first, from: s.look.from, last: s.last, found: found.length, ms: now() - s.look.t0 });
    freeLookers.push(f);
    s.look = null;
    lane.looks++;
    partial(true);
  };
  const stretchDone = async (lane, s) => {
    if (s.cut) {
      // taken over: its chunk goes on with the other lane
      lane.stretches.shift();
      lane.feeding--;
      changed();
      return;
    }
    chunksDone(s.first, s.last);
    lane.chunks += s.last - s.first + 1;
    lane.stretches.shift();
    if (s.look) await endLook(lane, s);
    else lane.feeding--;
    changed();
  };
  const onFrameFor = (lane) => async (pic, t) => {
    if (chunked.slow && lane.kind === 'browser') await new Promise((r) => setTimeout(r, chunked.slow));
    const s = lane.stretches[0];
    const c = chunkOf(t);
    const slot = slots[c];
    // a chunk another decoder began: its pictures up to where that one stopped are in
    if (t <= slot.lastT) {
      pic.close();
      return;
    }
    if (!pic.raw) {
      pic.close();
      throw new NotHoldable("this browser's decoder gives pictures a chunked scan cannot hold");
    }
    if (chunked.failBuiltIn && lane.kind === 'built-in' && !lane.tested && lane.frames === 10) {
      lane.tested = true;
      pic.close();
      throw new Error('the built-in decoder failed (a test)');
    }
    // the chunks of the stretch before this one have all their pictures
    if (s && c > s.first) chunksDone(s.first, c - 1);
    const held = hold(pic);
    slot.pics.push({ pic: held, t });
    slot.got++;
    slot.lastT = t;
    heldBytes += held.data.byteLength;
    if (heldBytes > peak) peak = heldBytes;
    lane.frames++;
    pushed++;
    wakeSlot(slot);
    if (s && s.look) await s.look.feeder.videoFrame(held, t, false);
    if (pushed % 30 === 0) report();
  };

  /** One decode pass of `lane`, from stretch `first` on, as long as there is work for it without waiting. */
  const runPass = async (lane, first) => {
    let pending = first;
    const next = () => {
      if (pending) {
        const s = pending;
        pending = null;
        return s;
      }
      const p = pick(lane);
      return p && p !== 'wait' ? p : null;
    };
    const onFrame = onFrameFor(lane);
    lane.passT0 = now();
    try {
      // a browser without a decoder for the codec decodes with the built-in one, whose workers are kept busy across the pass's chunks too
      const own = lane.kind === 'built-in' ? pool : movie.software && !chunked.sim ? await movie.softwarePool() : null;
      if (own && !own.busy) {
        await decodeStretchesBuiltIn(movie, own, next, onFrame, { cancel: stop, shrink, strict: lane.kind === 'built-in', onStretchDone: (s) => stretchDone(lane, s) });
      } else {
        for (let s = next(); s; s = next()) {
          await decodeRange(movie, s.startSec, s.endSec, onFrame, { cancel: () => stop() || !!s.cut, raw: true, reader: lane.reader, fromIndex: s.fromIndex, shrink, inline: !!chunked.sim, workers: true });
          if (stop()) break;
          await stretchDone(lane, s);
        }
      }
    } catch (e) {
      // what it had not finished goes back, to go on from its last picture
      for (const s of lane.stretches) {
        for (let c = s.first; c <= s.last; c++) if (!slots[c].done) picker.release(c);
        if (s.look) freeLookers.push(s.look.feeder);
      }
      lane.stretches = [];
      lane.feeding = 0;
      changed();
      throw e;
    } finally {
      lane.decodeMs += now() - lane.passT0;
      lane.passT0 = 0;
    }
  };
  const laneLoop = async (lane) => {
    const t0 = now();
    if (!lane.t0) lane.t0 = t0;
    lane.running = true;
    try {
      while (!stop() && !lane.failed) {
        const p = pick(lane);
        if (!p) break;
        if (p === 'wait') {
          lane.idle = true;
          steal(picker.cur);
          if (!lane.pending) await whenChanged();
          lane.idle = false;
          continue;
        }
        try {
          await runPass(lane, p);
        } catch (e) {
          if (e instanceof NotHoldable || lane.kind !== 'built-in' || stop()) throw e;
          // the built-in decoder could not decode a chunk: the browser's decoder goes on from there
          lane.failed = e && e.message ? e.message : String(e);
          console.warn(`chunked scan: ${lane.failed}; the browser's decoder takes the built-in decoder's chunks`);
        }
      }
    } catch (e) {
      halt(e);
      throw e;
    } finally {
      lane.running = false;
      lane.ms += now() - t0;
      // whoever waits on this lane looks again
      changed();
      wakeAll();
    }
  };

  // the detector: every chunk in file order
  const collect = () => addRecords(trace, feeder.records());
  const detect = async () => {
    feeder.reset();
    for (let c = 0; c < nc; c++) {
      picker.advance(c);
      changed();
      const slot = slots[c];
      for (;;) {
        if (stop()) return;
        if (slot.head < slot.pics.length) {
          const { pic, t } = slot.pics[slot.head];
          slot.pics[slot.head++] = null;
          heldBytes -= pic.data.byteLength;
          await feeder.videoFrame(pic, t, false);
          fed++;
          if (fed % 30 === 0) {
            collect();
            report();
          }
          // (the lanes decode ahead: hundreds of pictures can be waiting here)
          await breathe();
          continue;
        }
        if (slot.done) break;
        steal(c);
        await new Promise((r) => (slot.wake = r));
      }
      slot.pics = [];
      slot.head = 0;
      collect();
      partial(false);
    }
    picker.advance(nc);
    changed();
    await feeder.drain();
    collect();
  };

  let fallback = null;
  try {
    const detecting = detect().catch((e) => {
      halt(e);
      throw e;
    });
    let settled = await Promise.allSettled(lanes.map(laneLoop));
    // chunks a built-in decoder gave back after the other lanes had finished: they all go on
    while (!stop() && picker.left > 0 && settled.every((r) => r.status === 'fulfilled') && lanes.some((l) => !l.failed)) settled = await Promise.allSettled(lanes.filter((l) => !l.failed).map(laneLoop));
    const bad = settled.find((r) => r.status === 'rejected');
    if (bad) {
      await detecting.catch(() => {});
      throw bad.reason;
    }
    await detecting;
    // (the detector stopped where the cancel found it: the scan saw part of the file)
    if (cancel && cancel()) throw new Error('cancelled');
    if (!stop() && picker.left > 0) throw new Error('chunked scan: every decoder gave up');
  } catch (e) {
    if (!(e instanceof NotHoldable) || (cancel && cancel())) throw e;
    fallback = e.message;
  } finally {
    if (!moreFeeders) for (const f of lookFeeders) f.det.free();
  }
  if (fallback) {
    console.warn(`chunked scan: ${fallback}; scanning in one piece`);
    const r = await scanMovie(env, movie, { onProgress, cancel });
    r.chunked = { chunks: nc, chunkS, fallback };
    return r;
  }
  const elapsed = now() - started;
  const what = `${movie.width}×${movie.height}, ${feeder.backend}, ${nc} chunks in order${triage ? `, ${looks.length} early looks` : ''}, ${nhw} lane${nhw === 1 ? '' : 's'} of the browser's decoder${pool ? ` + the built-in ×${pool.workers.length}` : ''}`;
  const result = feeder.finish(false);
  for (const l of lanes) setAside(l, false);
  const laneStats = lanes.map((l) => ({ kind: l.kind, workers: l.kind === 'built-in' ? pool.workers.length : 1, frames: l.frames, chunks: l.chunks, looks: l.looks, ms: l.ms, failed: l.failed, steals: l.steals, stolen: l.stolen, aside: Math.round(l.asideMs), fps: rate(l) }));
  return {
    ...scanResult(env, movie, result, trace, fed, elapsed, what),
    segments: 1,
    chunked: { chunks: nc, chunkS, order: triage ? 'triage' : 'file', hot: triage ? triage.order.slice(0, triage.hot) : [], taken: picker.log.slice(), looks, lanes: laneStats, budget, peak, steals, rebalanced: rebalancing, grown },
  };
}

/**
 * How a decode of [from, to) splits into at most `k` spans that can run at
 * the same time: each span after the first starts at a keyframe, so it
 * needs no run-up of its own, and the cuts share out the samples to decode
 * (the first span's lead-in from its keyframe included). Spans are
 * { from, to, fromIndex } (seconds, and the sample the decode starts at).
 */
function decodeSpans(movie, from, to, k) {
  const v = movie.v;
  const { startIdx: start, endIdx: end } = sampleRange(movie, from, to);
  const whole = [{ from, to, fromIndex: start }];
  if (k <= 1) return whole;
  const total = end - start;
  // not worth a second decoder for a couple of seconds of video
  if (total < 120) return whole;
  const share = total / k;
  const cuts = [];
  let last = start;
  for (let i = start + 1; i < end && cuts.length < k - 1; i++) {
    if (!v.sync[i]) continue;
    const t = v.pts[i] / 1e6;
    if (t <= from || t >= to) continue;
    if (i - last >= share * 0.75 && end - i >= share * 0.5) {
      cuts.push(i);
      last = i;
    }
  }
  if (!cuts.length) return whole;
  const out = [];
  let prevIdx = start;
  let prevT = from;
  for (const c of cuts) {
    const t = v.pts[c] / 1e6;
    out.push({ from: prevT, to: t, fromIndex: prevIdx });
    prevIdx = c;
    prevT = t;
  }
  out.push({ from: prevT, to, fromIndex: prevIdx });
  return out;
}

/**
 * Decode a section plus its run-up and run-out, cache the analysis-size
 * pictures, and record the frame times. Fills sec.cache / sec.ctx / sec.pts.
 * With `spans` above one and `moreFeeders` (n => that many more detectors
 * like env.feeder), the range is cut at keyframes into spans decoded side by
 * side and joined in order: a picture's capture and pattern figures depend
 * on that picture alone, so the join is exact. A prepare that is cancelled
 * or fails throws, and leaves `sec` as it was.
 */
export async function prepareSection(env, movie, sec, { onProgress, cancel, spans = 1, moreFeeders = null } = {}) {
  const { wasm, config } = env;
  const need = wasm.context_seconds(config);
  const leadFrom = Math.max(movie.tsMin, sec.start - need);
  const tailTo = Math.min(movie.tsMax, sec.end + need);
  let plan = decodeSpans(movie, leadFrom, tailTo, moreFeeders ? spans : 1);
  let feeders = [env.feeder];
  if (plan.length > 1) {
    try {
      feeders = feeders.concat(await moreFeeders(plan.length - 1));
    } catch (e) {
      console.warn('[unflash] preparing on one decoder: no second detector', e);
      plan = decodeSpans(movie, leadFrom, tailTo, 1);
    }
  }
  profile.reset();
  const prepStarted = performance.now();
  let parts;
  try {
    parts = await prepareSpans(env, movie, sec, plan, feeders, { onProgress, cancel });
  } catch (e) {
    // a span that starts mid-stream can trip a decoder that one pass from
    // the section's own keyframe does not: try that before giving up
    if (plan.length === 1 || (cancel && cancel()) || String(e && e.message).includes('cancelled')) throw e;
    console.warn(`[unflash] preparing in ${plan.length} spans failed (${e && e.message ? e.message : e}); preparing in one`);
    plan = decodeSpans(movie, leadFrom, tailTo, 1);
    parts = await prepareSpans(env, movie, sec, plan, [env.feeder], { onProgress, cancel });
  }
  const count = parts.reduce((a, p) => a + p.count, 0);
  profile.report(`section prepare (${count} frames with captures${plan.length > 1 ? `, ${plan.length} spans` : ''})`, count, performance.now() - prepStarted);
  // join the spans in order
  const [first, ...rest] = parts;
  const { cache, lead, tail } = first;
  for (const p of rest) {
    lead.append(p.lead);
    cache.append(p.cache);
    tail.append(p.tail);
    p.lead.free();
    p.cache.free();
    p.tail.free();
  }
  const joined = (key) => [].concat(...parts.map((p) => p[key]));
  const rawPts = joined('rawPts');
  const rawPat = joined('rawPat');
  const rawPer = joined('rawPer');
  const leadPts = joined('leadPts');
  const tailPts = joined('tailPts');
  if (cache.len() === 0) {
    cache.free();
    lead.free();
    tail.free();
    throw new Error('Section decoded zero frames');
  }
  // the section works on a sanitised timeline (timestamp anomalies bridged)
  const san = JSON.parse(wasm.sanitize_deltas(Float64Array.from(rawPts), JSON.parse(config).max_frame_gap));
  const relPts = san.times.map((t) => Math.round(t * 1e6) / 1e6);
  const warnings = [];
  if (san.fixed) {
    warnings.push(`Bridged ${san.fixed} timestamp glitches in the source (its timestamps jump backwards or by several seconds). Output timing uses the repaired timeline.`);
  }
  const was = sec.nFrames || 0;
  if (sec.prepared && Object.keys(sec.edits || {}).length && was && was !== relPts.length) {
    warnings.push(`Re-prepared with ${relPts.length} frames where the marks were made against ${was}. They were kept, but they now sit on different frames, so check them before exporting.`);
  }
  dropCaches(sec);
  sec.prepared = true;
  sec.nFrames = relPts.length;
  sec.pts = relPts;
  sec.pattern = { counts: rawPat, periods: rawPer, thresh: env.feeder.det.pattern_thresh() };
  sec.cache = cache;
  sec.ctx = { lead, leadPts, tail, tailPts, seconds: need };
  sec.warnings = warnings;
  sec.edits = sec.edits || {};
  sec.preparedAt = Date.now();
  sec.preparedSpans = plan.length;
  return sec;
}

/**
 * Decode the spans of a prepare at the same time, span k on feeders[k];
 * returns each span's caches, times and pattern figures. When a span fails
 * or the prepare is cancelled, it throws once every span has stopped, the
 * caches freed again.
 */
async function prepareSpans(env, movie, sec, plan, feeders, { onProgress, cancel }) {
  const { wasm } = env;
  const aw = env.feeder.aw;
  const ah = env.feeder.ah;
  const parts = plan.map(() => ({
    cache: new wasm.FrameCache(aw, ah),
    lead: new wasm.FrameCache(aw, ah),
    tail: new wasm.FrameCache(aw, ah),
    rawPts: [],
    rawPat: [], // patterned pixels per section frame
    rawPer: [], // mean stripe half-period per section frame (analysis px)
    leadPts: [],
    tailPts: [],
    count: 0,
  }));
  const report = () => {
    if (onProgress) onProgress(parts.reduce((a, p) => a + p.count, 0));
  };
  const runOne = async (k, stop) => {
    const f = feeders[k];
    const part = parts[k];
    const { from, to, fromIndex } = plan[k];
    f.reset();
    const toTail = (rgba, t) => {
      part.tail.push(rgba);
      part.tailPts.push(Math.round((t - sec.start) * 1e6) / 1e6);
    };
    const settle = () => {
      for (const r of f.records()) {
        const rgba = f.det.take_capture(r.index);
        if (!rgba) continue;
        const t = r.t;
        if (t < sec.start - 1e-6) {
          part.lead.push(rgba);
          part.leadPts.push(Math.round((t - sec.start) * 1e6) / 1e6);
        } else if (t < sec.end + 1e-6) {
          part.cache.push(rgba);
          part.rawPts.push(t - sec.start);
          part.rawPat.push(r.pattern || 0);
          part.rawPer.push(r.pattern_period || 0);
          // a frame at the section's very end gives it its length, but is
          // not one of its own (section_timeline): the export shows it
          // straight after the section (walkEdited leaves a section there),
          // so the check's run-out starts with it
          if (t >= sec.end - 1e-9) toTail(rgba, t);
        } else toTail(rgba, t);
      }
    };
    await decodeRange(
      movie,
      from,
      to,
      async (frame, t) => {
        await f.videoFrame(frame, t, true);
        if (++part.count % 16 === 0) {
          settle();
          report();
        }
      },
      { cancel: stop, raw: true, fromIndex, reader: k > 0 ? movie.reader.fork() : null, shrink: shrinkFor(movie, f) }
    );
    await f.drain();
    settle();
  };
  try {
    // (a span that fails stops the others before their caches are freed, and
    // before prepareSection tries again on env.feeder, span 0's detector)
    await together(plan.length, runOne, cancel);
  } catch (e) {
    for (const p of parts) {
      p.cache.free();
      p.lead.free();
      p.tail.free();
    }
    throw e;
  }
  return parts;
}

/**
 * Seconds a frame marked E is held for (as the export holds it): what
 * edited_sequence, section_holds and rate_proposal are told.
 */
const EXT_S = 1.0;

/** The times of the frames a prepared section shows (section_timeline's first n_out, rebased onto the first). */
export function shownPts(wasm, sec) {
  return wasm.shown_pts(Float64Array.from(sec.pts), sec.start, sec.end);
}

/**
 * A section as the export shows it under `edits` (its marks; null for
 * none): its timeline (section_timeline), the times of the frames it
 * shows, the edited sequence of them (display time and source ordinal of
 * each slot) and the holds, all in section time. The check, the export and
 * a neighbour's check of its edge all read it from here.
 */
export function sectionSequence(wasm, sec, edits) {
  const tl = JSON.parse(wasm.section_timeline(Float64Array.from(sec.pts), sec.start, sec.end));
  const shown = Float64Array.from(tl.rel.slice(0, tl.n_out));
  const marks = JSON.stringify(edits || {});
  return { tl, shown, seq: JSON.parse(wasm.edited_sequence(shown, marks, EXT_S)), holds: JSON.parse(wasm.section_holds(shown, marks, EXT_S, tl.total)) };
}

/** Blur strength that takes a stripe pattern under the detector's swing. */
const SOFTEN_SIGMA_PER_HALF_PERIOD = 1.0;
/** Seconds of frames around a patterned frame that are softened with it. */
const SOFTEN_MARGIN_S = 0.25;

/**
 * Which of a prepared section's frames the "soften stripes" option blurs,
 * and how much: the frames where a regular pattern covered at least half
 * the area threshold (plus a short margin either side), blurred with a
 * Gaussian of σ = the stripes' mean half-period, which flattens them. In
 * analysis pixels; scale by the source/analysis width for the export.
 * Returns null when the section has no patterned frames.
 */
export function softenPlan(sec) {
  const p = sec.pattern;
  if (!p || !p.counts || !p.counts.length || !p.thresh) return null;
  const bar = p.thresh / 2;
  const hot = [];
  for (let i = 0; i < p.counts.length; i++) if (p.counts[i] >= bar) hot.push(i);
  if (!hot.length) return null;
  const pts = sec.pts || [];
  const at = (i) => (pts[i] === undefined ? i / 30 : pts[i]);
  const frames = new Set();
  for (const i of hot) {
    frames.add(i);
    for (let j = i - 1; j >= 0 && at(j) >= at(i) - SOFTEN_MARGIN_S; j--) frames.add(j);
    for (let j = i + 1; j < p.counts.length && at(j) <= at(i) + SOFTEN_MARGIN_S; j++) frames.add(j);
  }
  let wsum = 0;
  let psum = 0;
  for (const i of hot) {
    if (p.periods[i] > 0) {
      psum += p.periods[i] * p.counts[i];
      wsum += p.counts[i];
    }
  }
  const period = wsum ? psum / wsum : 2;
  const sigma = Math.max(1, SOFTEN_SIGMA_PER_HALF_PERIOD * period);
  // three box passes of radius r have σ² = ((2r+1)² − 1) / 4
  const radius = Math.max(1, Math.round((Math.sqrt(4 * sigma * sigma + 1) - 1) / 2));
  return { frames, hot, period, sigma, radius };
}

/** How far a blend mark made by hand mixes a frame with the frames around it, to begin with. */
export const BLEND_DEFAULT = 0.8;

/** How far a section's blend marks mix their frames with the frames around them (0 to 1). */
export function blendStrength(sec) {
  return sec.blendStrength == null ? BLEND_DEFAULT : sec.blendStrength;
}

/** The blend `blend` names ({ marks, strength }: a suggestion's trial), else the section's own. */
const blendOf = (sec, blend) => blend || { marks: sec.blend, strength: blendStrength(sec) };

/** A section's blend marks that apply (on its frames, at some strength), in order; `blend`'s in place of its own. */
export function blendMarks(sec, blend = null) {
  const n = sec.cache ? sec.cache.len() : sec.nFrames || 0;
  const { marks, strength } = blendOf(sec, blend);
  if (!(strength > 0)) return [];
  return [...new Set(marks || [])].filter((i) => i >= 0 && i < n).sort((a, b) => a - b);
}

/** Whether a section has marks that change what it shows (stripes softened, frames blended, removed or held). */
export function hasMarks(sec) {
  return !!sec.soften || blendMarks(sec).length > 0 || Object.values(sec.edits || {}).some((e) => e.removed || e.extended);
}

/**
 * What each of `marked.length` frames is blended with: null when it is not
 * marked (or no frame around it is unmarked), else `{ prev, next, u }`, the
 * nearest unmarked frames before and after it (null past the ends) and how
 * far from `prev` to `next` it sits. The same rule as the check's
 * (`unflash_core::blend`), so the export shows what was checked.
 */
export function blendSources(marked) {
  const n = marked.length;
  const out = new Array(n).fill(null);
  let prev = null;
  let i = 0;
  while (i < n) {
    if (!marked[i]) {
      prev = i++;
      continue;
    }
    let j = i;
    while (j < n && marked[j]) j++;
    const next = j < n ? j : null;
    if (prev !== null || next !== null) {
      for (let k = i; k < j; k++) {
        const u = prev !== null && next !== null ? (k - prev) / (next - prev) : prev === null ? 1 : 0;
        out[k] = { prev, next, u };
      }
    }
    i = j;
  }
  return out;
}

/** The weights of (the frame itself, `prev`, `next`) at strength `s`. */
export function blendWeights(src, s) {
  s = Math.max(0, Math.min(1, s));
  if (src.prev !== null && src.next !== null) return [1 - s, s * (1 - src.u), s * src.u];
  if (src.prev !== null) return [1 - s, s, 0];
  if (src.next !== null) return [1 - s, 0, s];
  return [1, 0, 0];
}

/**
 * A section's frames with its blend marks applied (its own cache when it
 * has none), or `blend`'s ({ marks, strength }) in place of its own: one
 * blended copy is kept, for the marks it was last asked for.
 */
export function blendedFrames(sec, blend = null) {
  if (!sec.cache) return sec.cache;
  const marks = blendMarks(sec, blend);
  if (!marks.length) {
    if (sec.blendCache) sec.blendCache.free();
    sec.blendCache = null;
    sec.blendKey = null;
    return sec.cache;
  }
  const s = blendOf(sec, blend).strength;
  const key = `${sec.preparedAt}:${s}:${marks.join(',')}`;
  if (sec.blendCache && sec.blendKey === key) return sec.blendCache;
  if (sec.blendCache) sec.blendCache.free();
  const mask = new Uint8Array(sec.cache.len());
  for (const i of marks) mask[i] = 1;
  sec.blendCache = sec.cache.blended(mask, s);
  sec.blendKey = key;
  return sec.blendCache;
}

/**
 * The frames a check or suggestion should read: blended and softened as
 * marked (blended as `blend` says, a suggestion's trial, when given).
 */
function sectionFrames(sec, blend = null) {
  const frames = blendedFrames(sec, blend);
  if (!frames || !sec.soften) return frames;
  const plan = softenPlan(sec);
  if (!plan) return frames;
  // (blendKey names the marks and strength the frames were blended with, whoever asked)
  const key = `${plan.radius}:${sec.blendKey || sec.preparedAt}:${[...plan.frames].join(',')}`;
  if (sec.softCache && sec.softKey === key) return sec.softCache;
  if (sec.softCache) sec.softCache.free();
  const mask = new Uint8Array(sec.cache.len());
  for (const i of plan.frames) if (i < mask.length) mask[i] = 1;
  sec.softCache = frames.blurred(plan.radius, mask);
  sec.softKey = key;
  sec.softPlan = plan;
  return sec.softCache;
}

function sectionsIn(project, sec, tLo, tHi) {
  return project
    .sectionsSorted()
    .filter((s) => s.id !== sec.id && s.end > tLo + 1e-6 && s.start < tHi - 1e-6);
}

/** The last/first `seconds` of another section as the export will contain it. */
function editedEdge(wasm, o, seconds, side) {
  if (!o.prepared || !o.cache || !o.pts || !o.pts.length) return null;
  const { seq } = sectionSequence(wasm, o, o.edits);
  if (!seq.t.length) return null;
  const cache = sectionFrames(o);
  const idx = [];
  const last = seq.t[seq.t.length - 1];
  const first = seq.t[0];
  for (let k = 0; k < seq.t.length; k++) {
    if (side === 'lead' ? seq.t[k] > last - seconds : seq.t[k] < first + seconds) idx.push(k);
  }
  if (!idx.length) return null;
  return { frames: idx.map((k) => ({ cache, i: seq.src[k] })), times: idx.map((k) => seq.t[k]) };
}

function compose(wasm, project, sec, src, secStart, tLo, tHi, need, side, notes) {
  const others = sectionsIn(project, sec, tLo, tHi);
  const frames = src ? src.frames : [];
  const times = src ? src.times : [];
  const parts = [];
  const original = (lo, hi) => {
    const f = [];
    const ts = [];
    for (let k = 0; k < frames.length; k++) {
      const t = times[k] + secStart;
      if (lo - 1e-6 <= t && t < hi - 1e-6) {
        f.push(frames[k]);
        ts.push(times[k]);
      }
    }
    if (f.length) parts.push({ frames: f, times: ts });
  };
  let cursor = tLo;
  for (const o of others) {
    original(cursor, o.start);
    const edge = editedEdge(wasm, o, need, side);
    if (edge) parts.push(edge);
    else {
      original(Math.max(cursor, o.start), Math.min(tHi, o.end));
      notes.push(`Section #${o.id} is inside this one's ${side === 'lead' ? 'run-up' : 'run-out'} but is not prepared, so this check reads its original frames. Edits made there are not included here.`);
    }
    cursor = Math.max(cursor, o.end);
  }
  original(cursor, tHi);
  return parts;
}

function join(parts, dt) {
  const frames = [];
  const times = [];
  let cursor = 0;
  for (const p of parts) {
    if (!p.frames.length) continue;
    const base = p.times[0];
    for (let k = 0; k < p.frames.length; k++) {
      frames.push(p.frames[k]);
      times.push(cursor + (p.times[k] - base));
    }
    cursor = times[times.length - 1] + dt;
  }
  return { frames, times };
}

/** Run-up and run-out for a section's check, as the export will contain them. */
function sectionContext(env, project, sec) {
  const { wasm, config } = env;
  const need = wasm.context_seconds(config);
  const notes = [];
  let leadSrc = null;
  let tailSrc = null;
  if (sec.ctx) {
    if (sec.ctx.lead.len()) leadSrc = { frames: Array.from({ length: sec.ctx.lead.len() }, (_, i) => ({ cache: sec.ctx.lead, i })), times: sec.ctx.leadPts };
    if (sec.ctx.tail.len()) tailSrc = { frames: Array.from({ length: sec.ctx.tail.len() }, (_, i) => ({ cache: sec.ctx.tail, i })), times: sec.ctx.tailPts };
    if ((sec.ctx.seconds || 0) + 1e-6 < need) {
      notes.push('This profile needs a longer run-up than the section was prepared with. Prepare it again so its check sees what a verify of the whole export sees.');
    }
  } else {
    notes.push('This section has no cached run-up, so its check starts cold and cannot see flashing in its opening second. Prepare it again for a full check.');
  }
  const dt = wasm.median_dt(Float64Array.from(sec.pts || []));
  const [tsMin, tsMax] = project.bounds;
  const leadParts = compose(wasm, project, sec, leadSrc, sec.start, Math.max(tsMin, sec.start - need), sec.start, need, 'lead', notes);
  const lead = join(leadParts, dt);
  if (lead.frames.length) {
    const shift = lead.times[lead.times.length - 1] + dt;
    lead.times = lead.times.map((t) => t - shift);
  }
  const tailParts = compose(wasm, project, sec, tailSrc, sec.start, sec.end, Math.min(tsMax, sec.end + need), need, 'tail', notes);
  const tail = join(tailParts, dt);
  tail.times = tail.times.map((t) => t + dt);
  const nxt = sectionsIn(project, sec, sec.end, sec.end + need);
  const nextAt = nxt.length ? nxt[0].start - sec.end : null;
  return { lead, tail, notes, nextAt };
}

/**
 * The instant safety check for a section's (or the given) edits, and its
 * blend marks (or `blend`'s, { marks, strength }: a suggestion's trial, the
 * section left as it is). A cancel (`cancel()` true) makes it throw
 * 'cancelled' before it starts.
 */
export async function checkSection(env, project, sec, edits, { onProgress, cancel = null, blend = null } = {}) {
  const { wasm, feeder } = env;
  // a turn for the page between checks (a suggestion runs dozens), never
  // during one: what a check reads stays put while it runs
  await breathe();
  if (cancel && cancel()) throw new Error('cancelled');
  if (!sec.prepared || !sec.cache) throw new Error('Section not prepared');
  const ctx = sectionContext(env, project, sec);
  const { tl, seq, holds } = sectionSequence(wasm, sec, edits || sec.edits);
  // the frames after the section come after all of its holds, its last frame's too
  const total = tl.total;
  const lastHold = holds.filter((h) => h.at >= total - 1e-9).reduce((sum, h) => sum + h.seconds, 0);
  const frames = sectionFrames(sec, blend);
  feeder.reset();
  let fed = 0;
  const count = ctx.lead.frames.length + seq.t.length + ctx.tail.frames.length;
  for (let k = 0; k < ctx.lead.frames.length; k++) {
    await feeder.cached(ctx.lead.frames[k].cache, ctx.lead.frames[k].i, ctx.lead.times[k]);
    if (onProgress && ++fed % 60 === 0) onProgress(fed / count);
  }
  for (let k = 0; k < seq.t.length; k++) {
    await feeder.cached(frames, seq.src[k], seq.t[k]);
    if (onProgress && ++fed % 60 === 0) onProgress(fed / count);
  }
  const endDisp = seq.t.length ? seq.t[seq.t.length - 1] : 0;
  for (let k = 0; k < ctx.tail.frames.length; k++) {
    await feeder.cached(ctx.tail.frames[k].cache, ctx.tail.frames[k].i, endDisp + lastHold + ctx.tail.times[k]);
    if (onProgress && ++fed % 60 === 0) onProgress(fed / count);
  }
  await feeder.drain();
  const result = feeder.finish(true);
  const cls = JSON.parse(wasm.classify(env.config, JSON.stringify(result), endDisp, ctx.nextAt === null ? undefined : ctx.nextAt + lastHold));
  const violations = [...cls.inside, ...cls.after];
  const wcagSafe = !violations.some((v) => v.kind === 'flash' || v.kind === 'red');
  const extendedBad = result.flag_extended && violations.some((v) => v.kind === 'extended');
  const patternBad = result.flag_patterns && violations.some((v) => v.kind === 'pattern');
  const safe = wcagSafe && !extendedBad && !patternBad;
  const soft = sec.soften && sec.softCache && frames === sec.softCache ? sec.softPlan : null;
  const flagged = Array.from(wasm.flagged_frames(Float64Array.from(seq.t), JSON.stringify(cls.inside)));
  const spills = cls.inside.filter((v) => v.end > endDisp + 1e-6);
  // chart statistics for the section's own frames
  const lo = ctx.lead.frames.length;
  const hi = lo + seq.t.length;
  const fs = result.frame_stats;
  const slice = (a) => (a ? a.slice(lo, hi) : []);
  const stats = {
    t: slice(fs.t),
    lum: slice(fs.lum),
    up: slice(fs.up_area),
    down: slice(fs.down_area),
    red: slice(fs.red_area),
    hazard: slice(fs.hazard),
    hazardRed: slice(fs.hazard_red),
    ext: slice(fs.ext),
    pattern: slice(fs.pattern),
  };
  return {
    safe,
    wcag_safe: wcagSafe,
    flag_extended: result.flag_extended,
    flag_patterns: result.flag_patterns,
    pattern_thresh: result.pattern_thresh,
    soften: !!sec.soften,
    soft_frames: soft ? [...soft.frames] : [],
    soft_sigma: soft ? soft.sigma : 0,
    violations,
    inside: cls.inside,
    after: cls.after,
    elsewhere: cls.elsewhere,
    before: cls.before,
    flagged,
    spills,
    stats,
    seq,
    endDisp,
    events: result.events,
    area_thresh: result.area_thresh,
    context_notes: ctx.notes,
    context_lead: ctx.lead.times.length ? -ctx.lead.times[0] : 0,
    context_tail: ctx.tail.times.length ? ctx.tail.times[ctx.tail.times.length - 1] : 0,
    profile: wasm.profile_name(env.config),
    detector_sig: wasm.config_signature(env.config),
    raw: { ...result, violations },
    frames: hi - lo,
  };
}

/** The frames marked keep, as the suggesters take them (undefined: none). */
export function keepJson(sec) {
  return sec.keep && sec.keep.length ? JSON.stringify(sec.keep) : undefined;
}

/**
 * Keep-light / keep-dark / fewest-removals suggestion. `only` is an array of
 * ordinals or null; frames marked keep are never removed. The fewest
 * removals, once passing, let frames back into long runs of removed frames
 * at the safe picture rate (a frozen picture moves again), each try checked.
 * A cancel (`cancel()` true) makes it, and the suggesters below, throw at
 * the next check.
 */
export async function suggestEdits(env, project, sec, prefer, only, { onProgress, cancel = null } = {}) {
  const { wasm, config } = env;
  const sug = new wasm.Suggester(shownPts(wasm, sec), JSON.stringify(sec.edits || {}), prefer, only ? JSON.stringify(only) : undefined, keepJson(sec));
  let step;
  try {
    // the fewest removals end by letting frames back into long removed
    // stretches at the rate that cannot fail by itself
    const safe = wasm.safe_picture_rate(config);
    if (prefer === 'fewest' && safe > 0) sug.thin_long_gaps(1 / safe);
    const frames = sectionFrames(sec);
    step = JSON.parse(sug.step(frames, undefined));
    let round = 0;
    while (step.simulate) {
      if (onProgress) onProgress(round);
      const tried = await checkSection(env, project, sec, step.simulate, { cancel });
      step = JSON.parse(sug.step(frames, JSON.stringify(tried.raw)));
      round++;
    }
  } finally {
    sug.free();
  }
  // the verdict is the check of the marks as the section will have them: the
  // last one tried may have been taken back (a flash put back that failed,
  // frames let back into a long removed stretch), and what it hands back
  // then is an earlier set, whose check says nothing about the last
  const done = step.done;
  const marks = JSON.parse(wasm.apply_suggestion(JSON.stringify(sec.edits || {}), JSON.stringify(done.edits), only ? JSON.stringify(only) : undefined, keepJson(sec)));
  const verdict = await checkSection(env, project, sec, marks, { cancel });
  const note = done.safe && !verdict.safe ? `${done.note} Checked again as applied, though, it does not pass: see the verdict.` : done.note;
  return { ...done, note, verdict };
}

/** "Reduce FPS": thin to a rate from timestamps alone, then check. */
export async function suggestFrameRate(env, project, sec, only, fps, { cancel = null } = {}) {
  const { wasm, config } = env;
  const p = JSON.parse(wasm.rate_proposal(config, shownPts(wasm, sec), JSON.stringify(sec.edits || {}), only ? JSON.stringify(only) : undefined, fps == null ? undefined : fps, EXT_S, keepJson(sec)));
  const verdict = await checkSection(env, project, sec, p.edits, { cancel });
  const note = wasm.rate_note(JSON.stringify(p), verdict.safe);
  return { edits: p.removals, safe: verdict.safe, rounds: 1, fps: p.fps, safe_fps: p.safe_fps, guaranteed: p.guaranteed, note, verdict };
}

/** Each step of the frame-rate search keeps this share of the last rate. */
const RATE_STEP = 0.9;

/**
 * The rates the frame-rate search tries, highest first: twice the
 * guaranteed-safe rate (never more than the section's own rate), then a
 * tenth less each time, to the guaranteed rate. A profile with no
 * guaranteed rate (one flash already fails) goes from half the section's
 * rate down to one picture a second.
 */
export function rateLadder(safe, sourceFps) {
  const round = (r) => Math.round(r * 10) / 10;
  const floor = safe > 0 ? safe : 1;
  let top = safe > 0 ? 2 * safe : sourceFps / 2;
  top = Math.min(top, sourceFps * RATE_STEP);
  // a tenth at a time, or bigger steps where that would take more than ten checks
  const step = Math.min(RATE_STEP, Math.pow(floor / Math.max(top, floor), 1 / 9));
  const out = [];
  for (let r = round(top); r > floor + 0.05; r = round(r * step)) out.push(r);
  out.push(floor);
  return out;
}

/**
 * "Reduce FPS" the way an editor does it by hand: try twice the guaranteed
 * rate and step down until the check passes, so the section keeps as many
 * pictures as it can. Returns the first rate that passes (or the last tried)
 * with the rates that failed before it.
 */
export async function searchFrameRate(env, project, sec, only, { sourceFps = 30, onProgress, cancel = null } = {}) {
  const { wasm, config } = env;
  const safe = wasm.safe_picture_rate(config);
  const ladder = rateLadder(safe, sourceFps);
  const failed = [];
  let res = null;
  for (let i = 0; i < ladder.length; i++) {
    if (onProgress) onProgress(i / ladder.length, ladder[i]);
    res = await suggestFrameRate(env, project, sec, only, ladder[i], { cancel });
    if (res.safe) break;
    failed.push(ladder[i]);
  }
  const tried = failed.length ? ` Tried ${failed.map((r) => `${r}/s`).join(', ')} first; ${failed.length === 1 ? 'it fails' : 'they fail'}.` : '';
  return { ...res, note: res.note + tried, ladder, failed };
}

/** Blend strengths are searched in steps of this much. */
const BLEND_STEP = 0.05;
/** What a suggested blend leaves of a flash: this share of what just passes, for room to spare. */
const BLEND_ROOM = 0.8;
/** Rounds of adding the frames a check still flags, when blending all the way is not enough. */
const BLEND_ROUNDS = 3;

/**
 * "Lower contrast": take the contrast out of the flashing rather than
 * frames out of it. The frames on the flashing's minority side (the light
 * ones among dark ones, or the other way round) are marked to blend with the
 * frames around them; if that does not pass at full strength, the frames the
 * check still flags join them (a few rounds at most). Then the least
 * strength that passes is found (in 5% steps) and a little added, so the
 * flash that stays is at most 80% of what just passes. Removals within
 * reach (the selection, with "selection only") make way for it; holds and
 * keep marks stay, and frames marked keep are never blended. Resolves to
 * `{ edits, blend, strength, least, safe, note, verdict }`; each check is
 * handed the blend it tries, and the section's own marks are left alone.
 */
export async function suggestBlend(env, project, sec, only, { onProgress, cancel = null } = {}) {
  const { wasm } = env;
  const reach = only ? new Set(only) : null;
  const inReach = (i) => !reach || reach.has(i);
  const edits = {};
  for (const [k, e] of Object.entries(sec.edits || {})) if (!(e.removed && inReach(+k))) edits[k] = e;
  const outside = (sec.blend || []).filter((i) => !inReach(i));
  const shown = shownPts(wasm, sec);
  let checks = 0;
  const check = (marks, strength) => {
    if (onProgress) onProgress(checks++);
    return checkSection(env, project, sec, edits, { cancel, blend: { marks, strength } });
  };
  const candidates = (verdict) => JSON.parse(wasm.blend_candidates(shown, JSON.stringify(verdict.raw), sec.cache, only ? JSON.stringify(only) : undefined, keepJson(sec), false)).frames;
  const pct = (s) => `${Math.round(s * 100)}%`;
  const removed = Object.keys(sec.edits || {}).length - Object.keys(edits).length;
  const s0 = blendStrength(sec);
  const base = await check(outside, s0);
  if (base.safe) return { edits, blend: outside, strength: s0, least: 0, safe: true, verdict: base, note: removed ? 'It passes with the removals taken off and nothing blended.' : 'It passes as it is: nothing to blend.' };
  // the frames to blend: at full strength until it passes
  const marks = new Set(outside);
  let verdict = base;
  for (let round = 0; round < BLEND_ROUNDS; round++) {
    const before = marks.size;
    for (const i of candidates(verdict)) marks.add(i);
    if (marks.size === before) break;
    verdict = await check([...marks].sort((a, b) => a - b), 1);
    if (verdict.safe) break;
  }
  const list = [...marks].sort((a, b) => a - b);
  if (!list.length) return { edits, blend: outside, strength: 1, least: 1, safe: false, verdict: base, note: 'Found no flashing frames to blend here. Try a removal suggestion, or mark frames with B.' };
  if (!verdict.safe) {
    return { edits, blend: list, strength: 1, least: 1, safe: false, verdict, note: `Even blended all the way, ${list.length} frames do not take the flashing out here. They are left marked at 100% to go on from; a removal suggestion may do better.` };
  }
  // the least strength that passes, in steps: lo fails, hi passes
  const steps = Math.round(1 / BLEND_STEP);
  let lo = 0;
  let hi = steps;
  let found = verdict;
  while (hi - lo > 1) {
    const mid = (lo + hi) >> 1;
    const v = await check(list, mid / steps);
    if (v.safe) {
      hi = mid;
      found = v;
    } else lo = mid;
  }
  const least = hi / steps;
  // room to spare: leave at most BLEND_ROOM of the flash that just passes
  const chosen = Math.min(steps, Math.ceil((1 - BLEND_ROOM * (1 - least)) * steps - 1e-9));
  let strength = chosen / steps;
  if (chosen !== hi) {
    const v = await check(list, strength);
    if (v.safe) found = v;
    else strength = least;
  }
  const note = `Blended ${list.length} frame${list.length === 1 ? '' : 's'} with the frames around ${list.length === 1 ? 'it' : 'them'} at ${pct(strength)} (${pct(least)} is the least that passes)${removed ? `, in place of ${removed} removal${removed === 1 ? '' : 's'}` : ''}: it passes, and no frame is taken out.`;
  return { edits, blend: list, strength, least, safe: true, verdict: found, note };
}
