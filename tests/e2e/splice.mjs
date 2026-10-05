// The H.264 smart-cut export end to end, with a stand-in encoder: the
// source is splice_a (High profile, CABAC, B-frames); the spans the export
// "re-encodes" come from an encoder that hands back splice_b's samples
// (Main profile, CAVLC) for the same frames. The exported file, read with
// the built-in decoder (this Chromium has no H.264), shows splice_a's
// pictures outside the spans and splice_b's inside them, from one track
// whose record holds both streams' parameter sets. Also the full
// re-encode, two spans two at a time, and encoders that hand over Annex B
// with no record (a High-profile one's record must carry the High fields).
// Then how an export ends when the later of two spans fails, the sound
// copied as it is (from flash.mp4: AAC beside VP9), and the levels the VP9,
// AV1 and HEVC encoders are asked for, against the tables of their specs.
//   node tests/e2e/splice.mjs
import path from 'node:path';
import fs from 'node:fs';
import { ROOT, WEB, MEDIA, assert, chromium, watch } from './playwright.mjs';

const CLIPS = path.join(WEB, 'clips');
fs.mkdirSync(CLIPS, { recursive: true });
for (const f of ['splice_a.mp4', 'splice_b.mp4']) fs.copyFileSync(path.join(ROOT, 'tests/media/h264', f), path.join(CLIPS, f));
fs.copyFileSync(path.join(MEDIA, 'flash.mp4'), path.join(CLIPS, 'flash.mp4'));

const { browser, port, close } = await chromium();
const page = await browser.newPage();
const errors = [];
watch(page, errors);
await page.goto(`http://127.0.0.1:${port}/?auto=0&cpu=1`);
await page.waitForFunction(() => document.querySelector('#support').textContent.includes('WebGPU'), null, { timeout: 60000 });

// a picture's size and frame rate, and the lowest level of each codec it
// fits: VP9's by its luma samples a second, a picture's and its longer side
// (the WebM project's table), AV1's seq_level_idx by MaxPicSize, MaxHSize,
// MaxVSize and MaxDisplayRate (annex A.3), HEVC's general_level_idc by
// MaxLumaPs, MaxLumaSr and a side of at most √(8 × MaxLumaPs) (tables A.8
// and A.9; the picture in whole 8 × 8 blocks)
const LEVELS = [
  [640, 360, 30, '21', '01', 63], // 2.1 for all three (flash.mp4 declares VP9 2.1)
  [320, 240, 30, '20', '00', 60], // 2.0 for all three
  [640, 360, 60, '30', '04', 90], // more samples a second than 2.1 has: 3.0
  [1280, 720, 30, '31', '05', 93], // 3.1
  [1280, 720, 60, '40', '08', 120], // 4.0
  [1920, 1080, 30, '40', '08', 120], // 4.0
  [1920, 1080, 29.97, '40', '08', 120],
  [1080, 1920, 30, '40', '08', 120], // upright: AV1's MaxVSize is 3456 at 4.0
  [1920, 1080, 60, '41', '09', 123], // 4.1
  [3840, 2160, 30, '50', '12', 150], // 5.0
  [3840, 2160, 60, '51', '13', 153], // 5.1
  [7680, 4320, 60, '61', '17', 183], // 6.1
  [1400, 100, 30, '30', '00', 63], // VP9 2.1 takes 1344 a side; HEVC 2.1 √(8 × 245760) = 1402
  [1410, 100, 30, '30', '00', 90], // (1416 coded: past HEVC 2.1's side too)
  [100, 1200, 30, '21', '01', 63], // VP9 2.0 takes 960 a side, AV1 2.0 1152 down
  [16384, 16384, 120, '62', '18', 186], // past every level: the highest
];
const r = await page.evaluate(async (LEVELS) => {
  const wasm = await import('./pkg/unflash.js');
  const { Movie, decodeRange } = await import('./media.js');
  const { exportMovie, exportPlan, vp9Level, av1Level, hevcLevel } = await import('./export.js');
  const load = async (name) => {
    const blob = await (await fetch(`clips/${name}`)).blob();
    const m = await Movie.open(new File([blob], name, { type: 'video/mp4' }), wasm);
    const sup = await m.decoderSupport();
    if (!sup.supported) throw new Error(`${name}: ${sup.reason}`);
    return m;
  };
  const hash = (bytes) => {
    let h = 2166136261;
    for (let i = 0; i < bytes.length; i++) h = Math.imul(h ^ bytes[i], 16777619) >>> 0;
    return h;
  };
  // every frame's picture, presentation order
  const pictures = async (m) => {
    const out = [];
    await decodeRange(m, m.tsMin, m.tsMax + 1, async (pic, t) => out.push({ t, h: hash(pic.data) }), { raw: true });
    return out;
  };
  const a = await load('splice_a.mp4');
  const b = await load('splice_b.mp4');
  const software = a.software;
  const ha = await pictures(a);
  const hb = await pictures(b);
  // splice_b's samples, to hand out as "encoder output" by timestamp
  const bDesc = b.dx.track_description(b.video.index);
  const bSamples = new Map();
  for (let i = 0; i < b.v.pts.length; i++) bSamples.set(b.v.pts[i], { bytes: (await b.reader.read(b.v.offset[i], b.v.size[i])).slice(), sync: !!b.v.sync[i] });
  const encoded = [];
  // a stand-in encoder that hands out splice_b's samples; `desc` is the record
  // it reports, `inband` parameter sets it puts in front of its IDR samples
  const standIn = (desc, inband = []) => (config, { output }) => {
    let first = true;
    return {
      encodeQueueSize: 0,
      configure() {},
      encode(frame, opts) {
        const s = bSamples.get(frame.timestamp);
        if (!s) throw new Error('no stand-in sample at ' + frame.timestamp);
        encoded.push({ t: frame.timestamp, key: !!(opts && opts.keyFrame) });
        let bytes = s.bytes;
        if (s.sync && inband.length) {
          const parts = [];
          for (const n of inband) parts.push(Uint8Array.of(0, 0, (n.length >> 8) & 255, n.length & 255), n);
          parts.push(bytes);
          bytes = new Uint8Array(parts.reduce((a, x) => a + x.length, 0));
          let o = 0;
          for (const x of parts) {
            bytes.set(x, o);
            o += x.length;
          }
        }
        output({ byteLength: bytes.length, copyTo: (dst) => dst.set(bytes), timestamp: frame.timestamp, duration: frame.duration, type: s.sync ? 'key' : 'delta' }, first ? { decoderConfig: { codec: b.video.codec, description: desc } } : {});
        first = false;
      },
      async flush() {},
      close() {},
    };
  };
  const makeEncoder = standIn(bDesc);
  // the record's parameter sets, and the same record written the way
  // Firefox's Windows encoder writes it: every set with its header byte twice
  const avccSets = (d) => {
    const out = { sps: [], pps: [] };
    let p = 6;
    for (let k = 0; k < (d[5] & 31); k++) {
      const len = (d[p] << 8) | d[p + 1];
      out.sps.push(d.slice(p + 2, p + 2 + len));
      p += 2 + len;
    }
    const npps = d[p++];
    for (let k = 0; k < npps; k++) {
      const len = (d[p] << 8) | d[p + 1];
      out.pps.push(d.slice(p + 2, p + 2 + len));
      p += 2 + len;
    }
    return out;
  };
  const bSets = avccSets(bDesc);
  const firefoxDesc = (() => {
    const bytes = [1, bDesc[1], bDesc[2], bDesc[3], 3, bSets.sps.length];
    for (const n of bSets.sps) bytes.push(((n.length + 1) >> 8) & 255, (n.length + 1) & 255, n[0], ...n);
    bytes.push(bSets.pps.length);
    for (const n of bSets.pps) bytes.push(((n.length + 1) >> 8) & 255, (n.length + 1) & 255, n[0], ...n);
    return Uint8Array.from(bytes);
  })();
  const candidate = { label: 'stand-in H.264', config: { codec: 'avc1.64000A', width: a.width, height: a.height } };
  const project = { sectionsSorted: () => [], sections: [] };
  const env = { wasm, feeder: null };
  // what a record carries after its parameter sets (a High profile's chroma format and bit depths)
  const recordTail = (desc) => {
    let p = 6;
    for (let i = 0; i < (desc[5] & 31); i++) p += 2 + ((desc[p] << 8) | desc[p + 1]);
    const npps = desc[p++];
    for (let i = 0; i < npps; i++) p += 2 + ((desc[p] << 8) | desc[p + 1]);
    return Array.from(desc.subarray(p));
  };
  const run = async (name, opts, expectB) => {
    encoded.length = 0;
    const plan = await exportPlan(env, a, project, { codec: candidate.config.codec, ...opts });
    const res = await exportMovie(env, a, project, { quality: 7, candidate, makeEncoder, plan, ...opts });
    if (res.warnings.length) console.log(name + ': ' + res.warnings.join(' | '));
    const m = await Movie.open(new File([res.blob], 'out.mp4', { type: 'video/mp4' }), wasm);
    const sup = await m.decoderSupport();
    if (!sup.supported) throw new Error(`${name}: exported file: ${sup.reason}`);
    const ho = await pictures(m);
    const mismatches = [];
    for (let k = 0; k < ho.length; k++) {
      const want = expectB(k) ? hb[k] : ha[k];
      if (ho[k].h !== want.h || Math.abs(ho[k].t - want.t) > 1e-6) mismatches.push(k);
    }
    // decode times rise, and no frame is composed before it is decoded (the
    // edit list moves the presentation earlier by the reordering delay: the
    // demuxer's pts are the file's composition times plus edit_shift)
    let timing = true;
    for (let i = 0; i < m.v.dtsTicks.length; i++) {
      if (m.v.ptsTicks[i] - m.video.edit_shift < m.v.dtsTicks[i] || (i > 0 && m.v.dtsTicks[i] <= m.v.dtsTicks[i - 1])) timing = false;
    }
    const timingInfo = { editShift: m.video.edit_shift, first: Array.from({ length: 6 }, (_, i) => [m.v.ptsTicks[i], m.v.dtsTicks[i]]) };
    const record = m.dx.track_description(m.video.index);
    const sets = avccSets(record);
    return { name, mode: res.mode, spans: res.spans, frames: res.frames, copied: res.copied, parallel: res.parallel, codec: res.codec, plan: plan.pieces.map((p) => `${p.kind} ${p.from}-${p.to}`), paramSets: [sets.sps.length, sets.pps.length], tail: recordTail(record), outFrames: ho.length, mismatches, timing, timingInfo, keyRequests: encoded.filter((e) => e.key).map((e) => e.t), size: res.blob.size, hasAudio: !!m.audio };
  };
  const results = {};
  // one span in the middle: GOP 1 (frames 10..19, an IDR every 10 frames)
  results.one = await run('one span', { spans: [[0.4, 0.5]], parallel: 1 }, (k) => k >= 10 && k < 20);
  // two spans, two at a time: GOPs 1 and 3
  results.two = await run('two spans', { spans: [[0.4, 0.5], [1.05, 1.1]], parallel: 2 }, (k) => (k >= 10 && k < 20) || k >= 30);
  // everything re-encoded (smart cut off), in parallel pieces
  results.full = await run('full', { smartCut: false, parallel: 2 }, () => true);
  // Firefox's Windows encoder: a record with every header byte twice, the
  // parameter sets again in front of each IDR sample
  results.firefox = await run('firefox-style encoder', { spans: [[0.4, 0.5], [1.05, 1.1]], parallel: 2, makeEncoder: standIn(firefoxDesc, [...bSets.sps, ...bSets.pps]) }, (k) => (k >= 10 && k < 20) || k >= 30);
  // a record nothing can read: the export is redone the plain way (one
  // encoder, no joining) instead of failing
  const garbage = Uint8Array.of(1, 0x4d, 0x40, 0x1e, 0xff, 0xe1, 0x00, 0x04, 0x67, 0xff, 0xff, 0xff, 0x01, 0x00, 0x02, 0x68, 0xff);
  const fb = await exportMovie(env, a, project, { quality: 7, candidate, makeEncoder: standIn(garbage), spans: [[0.4, 0.5]], parallel: 2 });
  const fbMovie = await Movie.open(new File([fb.blob], 'fallback.mp4', { type: 'video/mp4' }), wasm);
  results.fallback = { mode: fb.mode, frames: fb.frames, copied: fb.copied, spans: fb.spans, parallel: fb.parallel, warnings: fb.warnings, samples: fbMovie.v.pts.length, record: Array.from(fbMovie.dx.track_description(fbMovie.video.index)) };
  // an encoder that gives no record (WebCodecs' Annex B: start codes, the
  // parameter sets in front of each IDR picture): the record is made from
  // the first keyframe and the samples are converted, so it splices as well
  const annexB = (config, cb) => {
    const inner = standIn(undefined, [...bSets.sps, ...bSets.pps])(config, {
      output: (chunk, meta) => {
        const data = new Uint8Array(chunk.byteLength);
        chunk.copyTo(data);
        // 4-byte lengths -> start codes
        const out = [];
        for (let p = 0; p + 4 <= data.length; ) {
          const len = (data[p] << 24) | (data[p + 1] << 16) | (data[p + 2] << 8) | data[p + 3];
          out.push(0, 0, 0, 1, ...data.subarray(p + 4, p + 4 + len));
          p += 4 + len;
        }
        const bytes = Uint8Array.from(out);
        cb.output({ byteLength: bytes.length, copyTo: (dst) => dst.set(bytes), timestamp: chunk.timestamp, duration: chunk.duration, type: chunk.type }, meta);
      },
      error: cb.error,
    });
    return inner;
  };
  results.annexB = await run('annex-b encoder', { spans: [[0.4, 0.5]], parallel: 1, makeEncoder: annexB }, (k) => k >= 10 && k < 20);
  // a High-profile encoder with B-frames that gives no record: splice_a's
  // own samples as Annex B, its sets in front of each IDR picture, handed
  // out in decode order once the pictures before them are in. The record
  // made of its first keyframe's sets must carry, after them, the chroma
  // format and bit depths ISO/IEC 14496-15 asks of a High profile's
  const aSets = avccSets(a.dx.track_description(a.video.index));
  const aOrder = [];
  for (let i = 0; i < a.v.pts.length; i++) aOrder.push({ pts: a.v.pts[i], bytes: (await a.reader.read(a.v.offset[i], a.v.size[i])).slice(), sync: !!a.v.sync[i] });
  const annexBHigh = (config, { output }) => {
    let next = 0;
    let latest = -Infinity;
    const emit = (upTo) => {
      for (; next < aOrder.length && aOrder[next].pts <= upTo; next++) {
        const s = aOrder[next];
        const out = [];
        if (s.sync) for (const n of [...aSets.sps, ...aSets.pps]) out.push(0, 0, 0, 1, ...n);
        for (let p = 0; p + 4 <= s.bytes.length; ) {
          const len = (s.bytes[p] << 24) | (s.bytes[p + 1] << 16) | (s.bytes[p + 2] << 8) | s.bytes[p + 3];
          out.push(0, 0, 0, 1, ...s.bytes.subarray(p + 4, p + 4 + len));
          p += 4 + len;
        }
        const bytes = Uint8Array.from(out);
        output({ byteLength: bytes.length, copyTo: (dst) => dst.set(bytes), timestamp: s.pts, type: s.sync ? 'key' : 'delta' }, next === 0 ? { decoderConfig: { codec: a.video.codec } } : {});
      }
    };
    return {
      encodeQueueSize: 0,
      configure() {},
      encode(frame, opts) {
        encoded.push({ t: frame.timestamp, key: !!(opts && opts.keyFrame) });
        latest = Math.max(latest, frame.timestamp);
        emit(latest);
      },
      async flush() {
        emit(Infinity);
      },
      close() {},
    };
  };
  results.annexBHigh = await run('annex-b High-profile encoder', { smartCut: false, parallel: 1, makeEncoder: annexBHigh }, () => false);
  results.sourceTail = recordTail(a.dx.track_description(a.video.index));
  // two spans two at a time, the later one's encoder failing at its first
  // frame: the export fails with that error. The earlier span, stopped by
  // the failure, ends with 'cancelled', and the writer waits on it: its
  // encoder takes no frame until the later one has failed and closed
  {
    let laterGone;
    const gone = new Promise((r) => (laterGone = r));
    let made = 0;
    const laterFails = (config, cb) => {
      const enc = makeEncoder(config, cb);
      if (made++ === 0) {
        let held = true;
        gone.then(() => (held = false));
        return {
          ...enc,
          get encodeQueueSize() {
            return held ? 9 : 0;
          },
        };
      }
      return {
        ...enc,
        encode() {
          cb.error(new Error('the encoder failed (a test)'));
        },
        close() {
          enc.close();
          laterGone();
        },
      };
    };
    let error = null;
    try {
      await exportMovie(env, a, project, { quality: 7, candidate, makeEncoder: laterFails, spans: [[0.4, 0.5], [1.05, 1.1]], parallel: 2 });
    } catch (e) {
      error = e.message;
    }
    results.laterFails = { error, encoders: made };
  }
  // the sound copied as it is: byte for byte, a run of the samples that sit
  // together in the file at a time (not a read and a write a packet; this
  // file puts a picture between most of its packets), and a cancel stops it
  {
    const blob = await (await fetch('clips/flash.mp4')).blob();
    const f = await Movie.open(new File([blob], 'flash.mp4', { type: 'video/mp4' }), wasm);
    const samplesOf = async (m, s) => {
      const out = [];
      for (let i = 0; i < s.offset.length; i++) out.push(hash(await m.reader.read(s.offset[i], s.size[i])));
      return out;
    };
    const sound = await samplesOf(f, f.a);
    const pictures = await samplesOf(f, f.v);
    let runs = 0;
    for (let i = 0; i < f.a.offset.length; i++) if (i === 0 || f.a.offset[i] !== f.a.offset[i - 1] + f.a.size[i - 1]) runs++;
    // the export's reads of the sound's samples, as they come
    const soundAt = new Set(Array.from(f.a.offset));
    let soundReads = 0;
    const read = f.reader.read.bind(f.reader);
    f.reader.read = (offset, size) => {
      if (soundAt.has(offset)) soundReads++;
      return read(offset, size);
    };
    // VP9 into VP9 with nothing to re-encode: every sample is copied
    const vp9 = { label: 'stand-in VP9', config: { codec: 'vp09.00.10.08', width: f.width, height: f.height } };
    const noEncoder = () => {
      throw new Error('nothing here is re-encoded');
    };
    const res = await exportMovie(env, f, project, { quality: 7, candidate: vp9, makeEncoder: noEncoder });
    const out = await Movie.open(new File([res.blob], 'copy.mp4', { type: 'video/mp4' }), wasm);
    const copied = { mode: res.mode, copied: res.copied, soundReads, runs, packets: f.a.offset.length, sound: JSON.stringify(await samplesOf(out, out.a)) === JSON.stringify(sound), pictures: JSON.stringify(await samplesOf(out, out.v)) === JSON.stringify(pictures), warnings: res.warnings };
    // cancelled at the sound's first read
    soundReads = 0;
    let cancelled = null;
    try {
      await exportMovie(env, f, project, { quality: 7, candidate: vp9, makeEncoder: noEncoder, cancel: () => soundReads > 0 });
    } catch (e) {
      cancelled = e.message;
    }
    results.soundCopy = { ...copied, cancelled, readsBeforeStop: soundReads };
  }
  results.levels = LEVELS.map(([w, h, fps]) => [vp9Level(w, h, fps), av1Level(w, h, fps), hevcLevel(w, h, fps)]);
  return { software, frames: ha.length, results };
}, LEVELS);
console.log(JSON.stringify(r, null, 1));
assert(r.software, 'this Chromium decodes H.264 with the built-in decoder, which the check needs');
assert(r.frames === 40, 'splice_a has 40 frames');
const one = r.results.one;
assert(one.mode === 'smart' && one.spans === 1 && one.frames === 10 && one.copied === 30, 'one span: GOP 1 re-encoded, the rest copied: ' + JSON.stringify(one));
assert(one.plan.join(',') === 'copy 0-10,encode 10-20,copy 20-40', 'one span: the pieces: ' + one.plan.join(','));
assert(one.paramSets[0] === 2 && one.paramSets[1] === 2, 'one span: the record holds both streams\' parameter sets: ' + one.paramSets);
assert(one.outFrames === 40 && one.mismatches.length === 0, 'one span: every frame decodes as its source: mismatches ' + JSON.stringify(one.mismatches));
assert(one.timing, 'one span: decode times are consistent');
assert(one.keyRequests.length === 1 && one.keyRequests[0] === 333333, 'one span: the span starts with a keyframe request: ' + one.keyRequests);
assert(one.hasAudio === false, 'no audio in the test streams');
const two = r.results.two;
assert(two.mode === 'smart' && two.spans === 2 && two.frames === 20 && two.copied === 20 && two.parallel === 2, 'two spans: ' + JSON.stringify(two));
assert(two.plan.join(',') === 'copy 0-10,encode 10-20,copy 20-30,encode 30-40', 'two spans: the pieces: ' + two.plan.join(','));
assert(two.outFrames === 40 && two.mismatches.length === 0 && two.timing, 'two spans: every frame decodes as its source: ' + JSON.stringify(two.mismatches));
assert(two.paramSets[0] === 2 && two.paramSets[1] === 2, 'two spans: one extra set of parameter sets, shared by both spans: ' + two.paramSets);
const full = r.results.full;
assert(full.mode === 'full' && full.frames === 40 && full.copied === 0, 'full: everything re-encoded: ' + JSON.stringify(full));
assert(full.outFrames === 40 && full.mismatches.length === 0 && full.timing, 'full: every frame is the stand-in encoder\'s: ' + JSON.stringify(full.mismatches));
assert(full.paramSets[0] === 1 && full.paramSets[1] === 1, 'full: only the encoder\'s parameter sets: ' + full.paramSets);
const ff = r.results.firefox;
assert(ff.mode === 'smart' && ff.spans === 2 && ff.frames === 20 && ff.copied === 20, 'firefox-style encoder: the spans are spliced in: ' + JSON.stringify(ff));
assert(ff.outFrames === 40 && ff.mismatches.length === 0 && ff.timing, 'firefox-style encoder: every frame decodes as its source: ' + JSON.stringify(ff.mismatches));
assert(ff.paramSets[0] === 2 && ff.paramSets[1] === 2, 'firefox-style encoder: the repaired sets join the source\'s: ' + ff.paramSets);
const fbr = r.results.fallback;
assert(fbr.mode === 'full' && fbr.frames === 40 && fbr.copied === 0 && fbr.spans === 1 && fbr.parallel === 1, 'an unreadable record: the export is redone by one encoder: ' + JSON.stringify(fbr));
assert(fbr.samples === 40 && /re-encoded by one encoder/.test(fbr.warnings[0] || ''), 'and says so: ' + JSON.stringify(fbr));
assert(JSON.stringify(fbr.record) === JSON.stringify([1, 0x4d, 0x40, 0x1e, 0xff, 0xe1, 0x00, 0x04, 0x67, 0xff, 0xff, 0xff, 0x01, 0x00, 0x02, 0x68, 0xff]), 'the unreadable record goes in as it came: ' + JSON.stringify(fbr.record));
assert(errors.length === 0, 'no page errors: ' + errors.join(' | '));
const ab = r.results.annexB;
assert(ab.mode === 'smart' && ab.frames === 10 && ab.copied === 30, 'an Annex B encoder with no record: spliced all the same: ' + JSON.stringify(ab));
assert(ab.outFrames === 40 && ab.mismatches.length === 0 && ab.timing, 'an Annex B encoder: every frame decodes as its source: ' + JSON.stringify(ab.mismatches));
assert(ab.paramSets[0] === 2 && ab.paramSets[1] === 2, 'an Annex B encoder: its parameter sets join the source\'s: ' + ab.paramSets);
const abh = r.results.annexBHigh;
assert(abh.mode === 'full' && abh.frames === 40 && abh.copied === 0, 'a High-profile Annex B encoder: everything re-encoded: ' + JSON.stringify(abh));
assert(abh.outFrames === 40 && abh.mismatches.length === 0 && abh.timing, 'a High-profile Annex B encoder: every frame decodes as its source: ' + JSON.stringify(abh.mismatches));
// (4:2:0, 8-bit luma and chroma, no SPS extensions: what x264 wrote after splice_a's own sets)
assert(JSON.stringify(r.results.sourceTail) === '[253,248,248,0]' && JSON.stringify(abh.tail) === JSON.stringify(r.results.sourceTail), 'a High-profile Annex B encoder: the record made for it carries the High profile\'s fields: ' + JSON.stringify([abh.tail, r.results.sourceTail]));
const lf = r.results.laterFails;
assert(lf.encoders === 2 && lf.error === 'the encoder failed (a test)', 'an export whose later span fails says why, not "cancelled": ' + JSON.stringify(lf));
const sc = r.results.soundCopy;
assert(sc.mode === 'smart' && sc.copied > 0 && sc.sound && sc.pictures, 'the samples are copied byte for byte: ' + JSON.stringify(sc));
assert(sc.runs < sc.packets && sc.soundReads === sc.runs, `the sound is copied a run of samples at a time: ${sc.soundReads} reads for ${sc.runs} runs of ${sc.packets} packets`);
assert(sc.cancelled === 'cancelled' && sc.readsBeforeStop === 1, 'a cancel stops the copy of the sound: ' + JSON.stringify({ cancelled: sc.cancelled, reads: sc.readsBeforeStop }));
LEVELS.forEach(([w, h, fps, vp9, av1, hevc], i) => {
  const got = r.results.levels[i];
  assert(JSON.stringify(got) === JSON.stringify([vp9, av1, hevc]), `${w}×${h} at ${fps} fps: VP9 ${vp9}, AV1 ${av1}, HEVC ${hevc}, not ${JSON.stringify(got)}`);
});
console.log('SPLICE OK');
await close();
