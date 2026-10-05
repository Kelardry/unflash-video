// Export: the edited timeline back into an MP4. The frames no section's
// marks touch are copied from the source as they are, whole GOPs at a
// time, without decoding or encoding ("smart cut"); the spans the marked
// sections touch are decoded, edited and re-encoded with WebCodecs, several
// at once, each from a keyframe the decoder can start at cold. When the
// encoder's codec cannot share a track with the source's (or smart cut is
// off), the whole video is re-encoded, still in parallel pieces. The sound
// is copied from the source as it is, unless frames are held: then it is
// re-encoded with silence under each held frame (see sound.js). The WASM
// muxer writes the file.

import { decodeRange, waker } from './media.js';
import { profile } from './profile.js';
import { softenPlan, blendMarks, blendStrength, blendSources, blendWeights, hasMarks, sectionSequence } from './analysis.js';
import { SoundRun, audioData, soundName } from './sound.js';
import { noSoundDecoder, soundChunk, soundConfig, soundDecoderFor } from './audiodec.js';
import { builtInFor } from './codecs.js';

function avcLevel(w, h, fps) {
  const mbs = Math.ceil(w / 16) * Math.ceil(h / 16);
  const rate = mbs * fps;
  if (mbs <= 8192 && rate <= 245760) return '28'; // 4.0
  if (mbs <= 8192 && rate <= 522240) return '2A'; // 4.2
  if (mbs <= 22080 && rate <= 589824) return '32'; // 5.0
  if (mbs <= 36864 && rate <= 983040) return '33'; // 5.1
  return '34'; // 5.2
}

/**
 * VP9's levels (the WebM project's table, as libvpx has it): level × 10,
 * then the most luma samples a second, a picture's luma samples and its
 * longer side.
 */
const VP9_LEVELS = [
  [10, 829440, 36864, 512],
  [11, 2764800, 73728, 768],
  [20, 4608000, 122880, 960],
  [21, 9216000, 245760, 1344],
  [30, 20736000, 552960, 2048],
  [31, 36864000, 983040, 2752],
  [40, 83558400, 2228224, 4160],
  [41, 160432128, 2228224, 4160],
  [50, 311951360, 8912896, 8384],
  [51, 588251136, 8912896, 8384],
  [52, 1176502272, 8912896, 8384],
  [60, 1176502272, 35651584, 16832],
  [61, 2353004544, 35651584, 16832],
  [62, 4706009088, 35651584, 16832],
];

/**
 * AV1's levels (its specification, annex A.3): seq_level_idx, then
 * MaxPicSize, MaxHSize, MaxVSize and MaxDisplayRate. (Levels 5.3 and 6.3
 * differ from 5.2 and 6.2 only in the decode rate, which counts the frames
 * not shown too; an encoder that hides none needs neither.)
 */
const AV1_LEVELS = [
  [0, 147456, 2048, 1152, 4423680],
  [1, 278784, 2816, 1584, 8363520],
  [4, 665856, 4352, 2448, 19975680],
  [5, 1065024, 5504, 3096, 31950720],
  [8, 2359296, 6144, 3456, 70778880],
  [9, 2359296, 6144, 3456, 141557760],
  [12, 8912896, 8192, 4352, 267386880],
  [13, 8912896, 8192, 4352, 534773760],
  [14, 8912896, 8192, 4352, 1069547520],
  [16, 35651584, 16384, 8704, 1069547520],
  [17, 35651584, 16384, 8704, 2139095040],
  [18, 35651584, 16384, 8704, 4278190080],
];

/**
 * HEVC's levels (H.265 tables A.8 and A.9, the Main tier): general_level_idc
 * (30 × the level), then MaxLumaPs and MaxLumaSr. Neither side of a picture
 * may be more than √(8 × MaxLumaPs).
 */
const HEVC_LEVELS = [
  [30, 36864, 552960],
  [60, 122880, 3686400],
  [63, 245760, 7372800],
  [90, 552960, 16588800],
  [93, 983040, 33177600],
  [120, 2228224, 66846720],
  [123, 2228224, 133693440],
  [150, 8912896, 267386880],
  [153, 8912896, 534773760],
  [156, 8912896, 1069547520],
  [180, 35651584, 1069547520],
  [183, 35651584, 2139095040],
  [186, 35651584, 4278190080],
];

/**
 * The lowest VP9 level a `w`×`h` picture at `fps` fits, as its codec string
 * has it ('21' for 2.1), as avcLevel finds H.264's (the highest when none
 * does). The muxer writes it into the file's vpcC box as it is.
 */
export function vp9Level(w, h, fps) {
  const size = w * h;
  const fit = VP9_LEVELS.find(([, rate, pic, side]) => size <= pic && Math.max(w, h) <= side && size * fps <= rate);
  return String((fit || VP9_LEVELS[VP9_LEVELS.length - 1])[0]);
}

/** The lowest AV1 level a `w`×`h` picture at `fps` fits: its seq_level_idx, two digits as an av01 codec string has it ('01' for 2.1). */
export function av1Level(w, h, fps) {
  const size = w * h;
  const fit = AV1_LEVELS.find(([, pic, maxW, maxH, rate]) => size <= pic && w <= maxW && h <= maxH && size * fps <= rate);
  return String((fit || AV1_LEVELS[AV1_LEVELS.length - 1])[0]).padStart(2, '0');
}

/**
 * The lowest HEVC level a `w`×`h` picture at `fps` fits: its
 * general_level_idc (63 for 2.1). The picture is counted as it is coded,
 * in whole blocks of 8 by 8 (the smallest coding block HEVC has).
 */
export function hevcLevel(w, h, fps) {
  const cw = Math.ceil(w / 8) * 8;
  const ch = Math.ceil(h / 8) * 8;
  const size = cw * ch;
  const fit = HEVC_LEVELS.find(([, pic, rate]) => size <= pic && Math.max(cw, ch) <= Math.sqrt(8 * pic) && size * fps <= rate);
  return (fit || HEVC_LEVELS[HEVC_LEVELS.length - 1])[0];
}

/** The family of a WebCodecs codec string, as codecs.js knows it ('h264', 'hevc', 'vp9', 'vp8', 'av1'), or 'other'. */
const codecFamily = (codec) => (builtInFor(codec) || { id: 'other' }).id;

/** A codec string's family by the name the export dialog gives it. */
function familyName(codec) {
  const b = builtInFor(codec);
  return !b ? 'other' : b.id === 'hevc' ? 'H.265 (HEVC)' : b.name;
}

const FAMILY_WHERE = {
  h264: 'plays everywhere: phones, QuickTime, Windows, every browser',
  vp9: 'plays in browsers and VLC, but not in QuickTime or on older iPhones',
  av1: 'the smallest file, but slow to make, and only newer players and browsers play it',
  hevc: 'a small file that plays on Apple devices, but not in every browser',
  other: '',
};

/**
 * What an output format means for this file, in plain words: a short label
 * for the menu and a sentence for the dialog. `copies`: the parts no section
 * touches can be copied from the source as they are (the same format).
 */
export function formatInfo(cand, movie) {
  const family = codecFamily(cand.config.codec);
  const src = codecFamily(movie.video.codec);
  const copies = family === src && (family === 'h264' || family === 'vp9');
  const name = familyName(cand.config.codec);
  const how = copies
    ? 'Only the stretches around the sections you marked are re-encoded; everything else is copied from your file as it is (fast, and no quality lost anywhere else).'
    : `Your file is ${familyName(movie.video.codec)}, so every frame is re-encoded (slower, and a little quality is lost everywhere).`;
  const short = family === 'h264' ? 'plays everywhere' : family === 'vp9' ? 'browsers and VLC' : family === 'av1' ? 'smallest, slow, newer players' : family === 'hevc' ? 'small, Apple devices' : '';
  return { family, name, copies, label: `${name}: ${short}${copies ? ', copies what you did not edit' : ''}`, note: `${name} ${FAMILY_WHERE[family] ? `${FAMILY_WHERE[family]}. ` : ''}${how}` };
}

/**
 * The formats worth offering, best first: one of each family (the first
 * H.264 profile the encoder takes; Main and Baseline are only there for an
 * encoder without High).
 */
export function formatChoices(cands) {
  const seen = new Set();
  return cands.filter((c) => {
    const f = codecFamily(c.config.codec);
    if (seen.has(f)) return false;
    seen.add(f);
    return true;
  });
}

/** The bits a pixel of each picture an encoder aims for at `quality` (1 to 10). */
const bpp = (quality) => 0.03 + (quality / 10) * 0.25;

/** Encoder configurations to try, best first. */
export async function encoderCandidates(width, height, fps, quality) {
  const bitrate = Math.round(width * height * fps * bpp(quality));
  const base = { width, height, framerate: fps, bitrate, latencyMode: 'quality' };
  const level = avcLevel(width, height, fps);
  const list = [
    { label: 'H.264 (AVC)', config: { ...base, codec: `avc1.6400${level}`, avc: { format: 'avc' } } },
    { label: 'H.264 (AVC, Main)', config: { ...base, codec: `avc1.4D40${level}`, avc: { format: 'avc' } } },
    { label: 'H.264 (AVC, Baseline)', config: { ...base, codec: `avc1.42E0${level}`, avc: { format: 'avc' } } },
    { label: 'VP9', config: { ...base, codec: `vp09.00.${vp9Level(width, height, fps)}.08` } },
    { label: 'AV1', config: { ...base, codec: `av01.0.${av1Level(width, height, fps)}M.08` } },
    { label: 'H.265 (HEVC)', config: { ...base, codec: `hvc1.1.6.L${hevcLevel(width, height, fps)}.B0`, hevc: { format: 'hevc' } } },
  ];
  const out = [];
  if (typeof VideoEncoder === 'undefined') return out;
  for (const c of list) {
    try {
      const r = await VideoEncoder.isConfigSupported(c.config);
      if (r.supported) out.push({ ...c, config: r.config || c.config });
    } catch (e) {
      /* unsupported */
    }
  }
  return out;
}

class MemorySink {
  constructor() {
    this.parts = [];
    this.size = 0;
  }
  async write(bytes) {
    this.parts.push(bytes);
    this.size += bytes.byteLength;
  }
  async patch(offset, bytes) {
    // the head is always the first part
    this.parts[0].set(bytes, offset);
  }
  async close() {
    return new Blob(this.parts, { type: 'video/mp4' });
  }
}

/**
 * Rough size of an export: with a plan, the copied samples' bytes plus the
 * encoder's target bitrate over the re-encoded seconds; without one, the
 * bitrate over the whole duration. Plus the copied audio.
 */
export function estimateExportBytes(movie, quality, plan = null) {
  const perSecond = (movie.width * movie.height * movie.fps * bpp(quality)) / 8;
  let video = perSecond * movie.duration;
  if (plan && plan.mode === 'smart') video = plan.copiedBytes + perSecond * plan.encodedSeconds;
  let audio = 0;
  if (movie.a) for (let i = 0; i < movie.a.size.length; i++) audio += movie.a.size[i];
  return video + audio;
}

/** Whether this browser can write an export to its private storage on disk. */
export function privateStorageAvailable() {
  return !!(typeof navigator !== 'undefined' && navigator.storage && navigator.storage.getDirectory && typeof FileSystemFileHandle !== 'undefined' && FileSystemFileHandle.prototype.createWritable);
}

const PRIVATE_PREFIX = 'unflash-export-';

/** A short tag for the video an export was made from (its project key: name, size, modified time). */
function ownerTag(key) {
  let h = 0x811c9dc5;
  for (let i = 0; i < key.length; i++) {
    h ^= key.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h.toString(16).padStart(8, '0');
}

/**
 * A sink in the browser's private storage (the origin-private file system):
 * on disk, without a dialog or a user gesture, so an export need not fit in
 * memory. Chrome and Firefox; Safari's private storage cannot be written from
 * the page, so it falls back. Earlier exports there are removed first. Null
 * when unavailable or when `bytesNeeded` would not fit the storage quota.
 * `owner` (the video's project key) names it, so that it can be found again
 * when the same video is opened after the page is reloaded.
 */
export async function privateFileSink(bytesNeeded = 0, owner = '') {
  try {
    if (!privateStorageAvailable()) return null;
    const root = await navigator.storage.getDirectory();
    await discardPrivateExport(root);
    if (navigator.storage.estimate) {
      const { quota, usage } = await navigator.storage.estimate();
      if (quota && bytesNeeded && quota - (usage || 0) < bytesNeeded * 1.2) return null;
    }
    const handle = await root.getFileHandle(`${PRIVATE_PREFIX}${ownerTag(owner)}-${Date.now()}.mp4`, { create: true });
    const writable = await handle.createWritable({ keepExistingData: false });
    return { sink: new FileSink(writable), handle, private: true };
  } catch (e) {
    return null;
  }
}

/**
 * Remove every export written to private storage, but the one made from the
 * video `keep` (a project key), if any: a reload that opens the same video
 * again finds its export where it was.
 */
export async function discardPrivateExport(root = null, keep = null) {
  try {
    if (!privateStorageAvailable()) return;
    const dir = root || (await navigator.storage.getDirectory());
    const kept = keep ? `${PRIVATE_PREFIX}${ownerTag(keep)}-` : null;
    const names = [];
    for await (const name of dir.keys()) if (name.startsWith(PRIVATE_PREFIX) && !(kept && name.startsWith(kept))) names.push(name);
    for (const name of names) await dir.removeEntry(name).catch(() => {});
  } catch (e) {
    /* nothing to remove, or no private storage */
  }
}

/**
 * The export in private storage made from the video `owner` (a project key):
 * { file, madeAt } of the latest, or null. (One the page went away in the
 * middle of is empty: what a writable writes lands in the file only when it
 * is closed.)
 */
export async function findPrivateExport(owner) {
  try {
    if (!privateStorageAvailable() || !owner) return null;
    const dir = await navigator.storage.getDirectory();
    const prefix = `${PRIVATE_PREFIX}${ownerTag(owner)}-`;
    let best = null;
    for await (const [name, handle] of dir.entries()) {
      const m = name.startsWith(prefix) ? /-(\d+)\.mp4$/.exec(name) : null;
      if (m && (!best || +m[1] > best.madeAt)) best = { handle, madeAt: +m[1] };
    }
    if (!best) return null;
    const file = await best.handle.getFile();
    return file.size > 0 ? { file, madeAt: best.madeAt } : null;
  } catch (e) {
    return null;
  }
}

class FileSink {
  constructor(writable) {
    this.w = writable;
    this.size = 0;
  }
  // every write names its position: a patch moves the stream's cursor to
  // just past the patched bytes, and a plain write after it would land there
  // instead of at the end of the file
  async write(bytes) {
    await this.w.write({ type: 'write', position: this.size, data: bytes });
    this.size += bytes.byteLength;
  }
  async patch(offset, bytes) {
    await this.w.write({ type: 'write', position: offset, data: bytes });
  }
  /** Start the file over: later writes go from its beginning, and close() cuts off what is left. */
  async reset() {
    this.size = 0;
  }
  async close() {
    // a file started over may hold more than was written the second time
    await this.w.write({ type: 'truncate', size: this.size });
    await this.w.close();
    return null;
  }
  async abort() {
    try {
      await this.w.abort();
    } catch (e) {
      /* already closed */
    }
  }
}

// ---- the plan ------------------------------------------------------------------

/**
 * How one prepared section is rendered: its edited sequence (display time,
 * source ordinal), where it starts and ends, how many of its frames it
 * shows, its holds and the seconds they add, and the frames "soften
 * stripes" blurs. With `edited` false, the section as it is (the section
 * player's "original").
 */
export function sectionRenderPlan(env, movie, s, { edited = true } = {}) {
  const { tl, shown, seq, holds: sectionHolds } = sectionSequence(env.wasm, s, edited ? s.edits : null);
  // where the section waits for its held frames, in source seconds: the
  // frame times above come from the same list, and so does the silence the
  // export puts into the sound, so the two cannot disagree
  const holds = sectionHolds.map((h) => ({ at: s.start + tl.base + h.at, seconds: h.seconds }));
  const extra = holds.reduce((sum, h) => sum + h.seconds, 0);
  const needCount = new Map();
  const need = (i) => needCount.set(i, (needCount.get(i) || 0) + 1);
  for (const src of seq.src) need(src);
  // "lower contrast": marked frames mixed with the unmarked frames either
  // side of them, which must be at hand when they show
  let blend = null;
  const marks = edited ? blendMarks(s) : [];
  if (marks.length) {
    const marked = new Array(shown.length).fill(false);
    for (const i of marks) if (i < marked.length) marked[i] = true;
    const sources = blendSources(marked);
    blend = { sources, strength: blendStrength(s) };
    for (const src of seq.src) {
      const b = sources[src];
      if (!b) continue;
      if (b.prev !== null) need(b.prev);
      if (b.next !== null) need(b.next);
    }
  }
  // "soften stripes": blur the patterned frames at source resolution with
  // the σ the section's check used, scaled up from analysis pixels
  let soft = null;
  if (edited && s.soften) {
    const plan = softenPlan(s);
    if (plan) soft = { frames: plan.frames, sigma: plan.sigma * (movie.width / env.feeder.aw) };
  }
  // where the section ends on the source's clock (where its next frame would come)
  const end = s.start + tl.base + tl.total;
  return { sec: s, base: tl.base, seq, nOut: tl.n_out, needCount, holds, extra, end, soft, blend };
}

/**
 * The render plans of the sections that have marks to apply (hasMarks) and
 * were prepared once (they have frame times), in order: marks were made
 * against a section's frame times, which outlive its frame cache (dropped
 * to save memory, or not rebuilt since the project was restored). A section
 * without marks is left out: its frames are copied where the export copies
 * (re-encoded as they are, they would only lose quality), and go through as
 * they are in a span re-encoded for another. Sections with marks but no
 * frame times are named in `warnings`: theirs cannot be applied.
 */
function sectionPlans(env, movie, project, warnings) {
  const sections = project
    .sectionsSorted()
    .filter((s) => s.pts && s.pts.length && hasMarks(s))
    .map((s) => sectionRenderPlan(env, movie, s));
  const unprepared = project.sections.filter((s) => !(s.pts && s.pts.length) && hasMarks(s));
  if (unprepared.length) warnings.push(`Sections ${unprepared.map((s) => '#' + s.id).join(', ')} have marks but were never prepared; their marks were not applied. Prepare them and export again.`);
  return sections;
}

/**
 * Whether an encoder's output (`codec`) can share a track with the source's
 * samples: H.264 into an `avc1` source (the parameter sets are merged, see
 * AvcRegistry), VP9 into a VP9 source of the same profile (VP9 keeps its
 * headers in the frames). Returns { kind, lenSize } or null.
 */
function copyableCodec(movie, codec) {
  const src = movie.video;
  if (!src || !codec) return null;
  if (/^avc1/.test(codec) && src.fourcc === 'avc1' && /^avc1/.test(src.codec)) {
    const desc = movie.dx.track_description(src.index);
    if (desc.length) return { kind: 'avc', lenSize: movie.wasm.avcc_nal_length_size(desc) };
  }
  if (/^vp09/.test(codec) && src.fourcc === 'vp09' && /^vp09/.test(src.codec)) {
    if ((src.codec.split('.')[1] || '00') === (codec.split('.')[1] || '00')) return { kind: 'vp9', lenSize: 0 };
  }
  return null;
}

/** Seconds of frames a re-encoded piece has at least (each starts with a keyframe anyway). */
const MIN_PIECE_S = 2.0;
/** Re-encoded pieces per parallel worker to aim for, so the workers stay evenly busy. */
const PIECES_PER_WORKER = 3;

/**
 * Where a decoder can start cold: a sync sample that, for H.264, holds an
 * IDR picture (an open-GOP I picture is marked sync too, but the B pictures
 * after it lean on what came before). One small read per sample asked.
 */
class CutPoints {
  constructor(movie, avcLenSize) {
    this.movie = movie;
    this.lenSize = avcLenSize; // 0: not H.264, every sync sample will do
    this.cache = new Map();
    this.reader = movie.reader;
  }
  async ok(i) {
    const v = this.movie.v;
    if (!v.sync[i]) return false;
    if (!this.lenSize) return true;
    if (this.cache.has(i)) return this.cache.get(i);
    let type = 0;
    let len = Math.min(v.size[i], 4096);
    while (type === 0) {
      const bytes = await this.reader.read(v.offset[i], len);
      type = this.movie.wasm.h264_first_vcl_nal_type(bytes, this.lenSize);
      if (len >= v.size[i]) break;
      len = v.size[i];
    }
    const idr = type === 5;
    this.cache.set(i, idr);
    return idr;
  }
}

/**
 * The plan of an export: pieces in file order, each either a copy of a run of
 * source samples (decode order `from`..`to`) or a span to decode, edit and
 * re-encode (`from` a keyframe, `startSec`..`endSec` in source time, with the
 * sections inside it whose marks it applies: see sectionPlans). Smart cut
 * needs the encoder's `codec` to be able to share a track with the source's;
 * otherwise everything is re-encoded, cut into pieces at keyframes for the
 * parallel workers. `spans` adds source intervals to re-encode besides the
 * sections' (tests).
 */
export async function exportPlan(env, movie, project, { codec = null, smartCut = true, parallel = 0, spans = null } = {}) {
  const warnings = [];
  const sections = sectionPlans(env, movie, project, warnings);
  const v = movie.v;
  const n = v.pts.length;
  const K = parallel > 0 ? parallel : movie.software ? 1 : Math.min(4, Math.max(1, Math.floor((navigator.hardwareConcurrency || 4) / 2)));
  const copyable = smartCut ? copyableCodec(movie, codec) : null;
  const cuts = new CutPoints(movie, copyable && copyable.kind === 'avc' ? copyable.lenSize : /^avc1/.test(movie.video.codec) ? movie.wasm.avcc_nal_length_size(movie.dx.track_description(movie.video.index)) : 0);
  // sync samples in decode order (their times rise with their index)
  const syncs = [];
  for (let i = 0; i < n; i++) if (v.sync[i]) syncs.push(i);
  // the source intervals whose frames change
  const touched = sections.map((p) => [p.sec.start, p.sec.end]);
  if (spans) for (const [a, b] of spans) touched.push([a, b]);
  touched.sort((a, b) => a[0] - b[0]);
  const inTouched = (tSec) => touched.some(([a, b]) => tSec > a - 1e-9 && tSec < b - 1e-9);

  let mode = 'full';
  let ranges = []; // re-encoded spans [from, to) in decode order
  if (copyable && n > 0 && (await cuts.ok(0))) {
    mode = 'smart';
    for (const [t0, t1] of touched) {
      const us0 = t0 * 1e6 - 0.5;
      const us1 = t1 * 1e6 - 0.5;
      // the last cut point at or before the interval, the first at or after it
      let a = 0;
      for (let k = syncs.length - 1; k >= 0; k--) {
        if (v.pts[syncs[k]] <= us0 && (await cuts.ok(syncs[k]))) {
          a = syncs[k];
          break;
        }
      }
      let b = n;
      for (let k = 0; k < syncs.length; k++) {
        if (v.pts[syncs[k]] >= us1 && (await cuts.ok(syncs[k]))) {
          b = syncs[k];
          break;
        }
      }
      if (ranges.length && a <= ranges[ranges.length - 1][1]) ranges[ranges.length - 1][1] = Math.max(ranges[ranges.length - 1][1], b);
      else ranges.push([a, b]);
    }
  } else if (n > 0) {
    ranges = [[0, n]];
  }
  // pieces: copies between the spans; long spans cut at keyframes outside
  // the sections so several workers can take them
  let encodedFrames = 0;
  for (const [a, b] of ranges) encodedFrames += b - a;
  const fps = movie.fps || 30;
  const chunk = Math.max(Math.round(MIN_PIECE_S * fps), Math.ceil(encodedFrames / Math.max(1, K * PIECES_PER_WORKER)));
  const pieces = [];
  let pos = 0;
  const timeOf = (i) => (i < n ? v.pts[i] / 1e6 : movie.tsMax + 1);
  const pushEncode = (from, to) => {
    const startSec = timeOf(from);
    const endSec = timeOf(to);
    pieces.push({ kind: 'encode', from, to, startSec, endSec, frames: to - from, sections: sections.filter((p) => p.sec.start >= startSec - 1e-9 && p.sec.end <= endSec + 1e-9) });
  };
  for (const [a, b] of ranges) {
    if (a > pos) pieces.push({ kind: 'copy', from: pos, to: a, frames: a - pos });
    let from = a;
    if (K > 1 && b - a > 2 * chunk) {
      let next = a + chunk;
      for (const s of syncs) {
        if (s <= from || s >= b) continue;
        if (s < next) continue;
        if (b - s < chunk / 2) break;
        if (inTouched(timeOf(s))) continue;
        if (!(await cuts.ok(s))) continue;
        pushEncode(from, s);
        from = s;
        next = s + chunk;
      }
    }
    pushEncode(from, b);
    pos = b;
  }
  if (pos < n) pieces.push({ kind: 'copy', from: pos, to: n, frames: n - pos });
  // a section's timing offsets (held frames) reach everything after it
  let offset = 0;
  // (sectionPlans has them in order)
  const sorted = sections.slice();
  for (const piece of pieces) {
    const startSec = piece.kind === 'copy' ? timeOf(piece.from) : piece.startSec;
    while (sorted.length && sorted[0].sec.end <= startSec + 1e-9) offset += sorted.shift().extra;
    piece.offset = offset;
    if (piece.kind === 'encode') {
      for (const p of piece.sections) {
        const k = sorted.indexOf(p);
        if (k >= 0) {
          sorted.splice(k, 1);
          offset += p.extra;
        }
      }
    }
  }
  let copiedFrames = 0;
  let copiedBytes = 0;
  let encodedSeconds = 0;
  for (const piece of pieces) {
    if (piece.kind === 'copy') {
      copiedFrames += piece.frames;
      for (let i = piece.from; i < piece.to; i++) copiedBytes += v.size[i];
    } else encodedSeconds += Math.min(movie.tsMax, piece.endSec) - piece.startSec;
  }
  // every held frame of the export, in source seconds and in order: the sound's silences
  const holds = sections.flatMap((p) => p.holds).sort((x, y) => x.at - y.at);
  return { mode, copyable, pieces, sections, holds, spans: pieces.filter((p) => p.kind === 'encode').length, encoded: encodedFrames, copied: copiedFrames, copiedBytes, encodedSeconds, parallel: K, warnings };
}

// ---- the edited timeline, frame by frame ------------------------------------------

/**
 * Decode a piece of the source (`startSec`..`endSec`, from sample `from` when
 * given) and hand its edited timeline to `emit(frame, tSec, info)` in order:
 * inside a section the slots of its edited sequence (removed frames showing
 * their stand-in, held frames held, blended frames mixed with the frames
 * around them, softened frames blurred), outside the frames as they are,
 * every time shifted by the holds before it. `emit` must not keep the frame
 * past its return (clone it if needed). `info` is `{ sec, slot, src,
 * softened, blended }` (sec null and slot -1 outside a section). Returns
 * `{ softened, blended, warnings }`. The export encodes what it is handed;
 * the section player paces it onto a canvas.
 */
export async function walkEdited(movie, piece, emit, { cancel, reader = null } = {}) {
  let softened = 0;
  let blended = 0;
  const warnings = [];
  let offset = piece.offset || 0; // cumulative extension seconds
  const sections = piece.sections;
  let si = 0;
  let cur = null;
  const enterSection = (p) => ({ p, ordinal: 0, next: 0, frames: new Map(), need: new Map(p.needCount) });
  const release = (st, i) => {
    const left = st.need.get(i) - 1;
    st.need.set(i, left);
    if (left <= 0) {
      st.frames.get(i).close();
      st.frames.delete(i);
    }
  };
  const flushSection = async (st) => {
    const { p } = st;
    while (st.next < p.seq.t.length) {
      const src = p.seq.src[st.next];
      const mix = p.blend ? p.blend.sources[src] : null;
      // a blended frame waits for the frame after it
      if (!st.frames.has(src) || (mix && ((mix.prev !== null && !st.frames.has(mix.prev)) || (mix.next !== null && !st.frames.has(mix.next))))) break;
      const t = p.sec.start + p.base + p.seq.t[st.next] + offset;
      let frame = st.frames.get(src);
      const made = [];
      try {
        if (mix) {
          frame = blendFrame(frame, mix.prev !== null ? st.frames.get(mix.prev) : null, mix.next !== null ? st.frames.get(mix.next) : null, blendWeights(mix, p.blend.strength));
          made.push(frame);
          blended++;
        }
        const soft = !!(p.soft && p.soft.frames.has(src));
        if (soft) {
          frame = blurFrame(frame, p.soft.sigma);
          made.push(frame);
          softened++;
        }
        await emit(frame, t, { sec: p.sec, slot: st.next, src, softened: soft, blended: !!mix });
      } finally {
        for (const f of made) f.close();
      }
      release(st, src);
      if (mix && mix.prev !== null) release(st, mix.prev);
      if (mix && mix.next !== null) release(st, mix.next);
      st.next++;
    }
  };
  const endSection = async (st) => {
    try {
      await flushSection(st);
    } finally {
      for (const f of st.frames.values()) f.close();
      st.frames.clear();
    }
    if (st.next < st.p.seq.t.length && !(cancel && cancel())) warnings.push(`Section #${st.p.sec.id}: ${st.p.seq.t.length - st.next} slots could not be filled (the decode returned fewer frames than when it was prepared).`);
    offset += st.p.extra;
    cur = null;
  };
  try {
    await decodeRange(
      movie,
      piece.startSec,
      piece.endSec,
      async (frame, t) => {
        // leave a section whose frames are exhausted
        if (cur && (cur.ordinal >= cur.p.nOut || t >= cur.p.sec.end - 1e-9)) await endSection(cur);
        // enter a section?
        while (!cur && si < sections.length && t >= sections[si].sec.end - 1e-9) si++; // skipped entirely (empty)
        if (!cur && si < sections.length && t >= sections[si].sec.start - 1e-9 && t < sections[si].sec.end - 1e-9) {
          cur = enterSection(sections[si]);
          si++;
        }
        if (cur) {
          const j = cur.ordinal++;
          if (cur.need.has(j) && cur.need.get(j) > 0) cur.frames.set(j, frame);
          else frame.close();
          await flushSection(cur);
          return;
        }
        try {
          await emit(frame, t + offset, { sec: null, slot: -1, src: -1, softened: false, blended: false });
        } finally {
          frame.close();
        }
      },
      { cancel, reader, fromIndex: piece.from == null ? null : piece.from }
    );
    if (cur) await endSection(cur);
  } finally {
    // an error or a cancel mid-section: its held frames go too
    if (cur) for (const f of cur.frames.values()) f.close();
  }
  return { softened, blended, warnings };
}

// ---- H.264 in Annex B ----------------------------------------------------------------

/** Whether H.264 data starts with an Annex B start code rather than a NAL length. */
function startsWithStartCode(b) {
  return b.length > 4 && b[0] === 0 && b[1] === 0 && (b[2] === 1 || (b[2] === 0 && b[3] === 1));
}

/**
 * An Annex B access unit as MP4 wants it (every NAL unit behind a 4-byte
 * length), with the parameter sets it carries: what an encoder that gives
 * no avcC record (WebCodecs' way of saying its stream is Annex B) hands over.
 */
function annexbToLengthPrefixed(b) {
  const starts = [];
  for (let i = 0; i + 2 < b.length; i++) {
    if (b[i] === 0 && b[i + 1] === 0 && b[i + 2] === 1) {
      starts.push(i + 3);
      i += 2;
    }
  }
  const nals = [];
  for (let k = 0; k < starts.length; k++) {
    let end = k + 1 < starts.length ? starts[k + 1] - 3 : b.length;
    while (end > starts[k] && b[end - 1] === 0) end--; // the next start code's zero byte, trailing zeros
    if (end > starts[k]) nals.push(b.subarray(starts[k], end));
  }
  const out = new Uint8Array(nals.reduce((a, n) => a + 4 + n.length, 0));
  let o = 0;
  let sps = null;
  let pps = null;
  for (const n of nals) {
    out[o] = (n.length >>> 24) & 255;
    out[o + 1] = (n.length >>> 16) & 255;
    out[o + 2] = (n.length >>> 8) & 255;
    out[o + 3] = n.length & 255;
    out.set(n, o + 4);
    o += 4 + n.length;
    if ((n[0] & 0x1f) === 7 && !sps) sps = n;
    if ((n[0] & 0x1f) === 8 && !pps) pps = n;
  }
  return { bytes: out, sps, pps };
}

// ---- one re-encoded piece --------------------------------------------------------

/**
 * Decodes one piece of the source, lays its sections' edited sequences onto
 * the timeline and encodes the frames; the encoded chunks are kept until the
 * writer takes them (a piece is a few seconds, so a few megabytes).
 */
class PieceEncoder {
  constructor(ctx, piece) {
    this.ctx = ctx;
    this.piece = piece;
    this.chunks = []; // { pts, sync, bytes, desc, codec }
    this.frames = 0;
    this.softened = 0;
    this.blended = 0;
    this.warnings = [];
    this.error = null;
    this.description = null; // the encoder's current decoderConfig description
    this.codec = ctx.chosen.config.codec;
  }

  async run() {
    const { ctx, piece } = this;
    const { movie } = ctx;
    // woken by the encoder taking a frame or handing one back, not polled
    // (a timer crawls in a hidden tab; the one here is a safety net)
    const { wait, wake } = waker(50);
    const enc = ctx.makeEncoder(ctx.chosen.config, {
      output: (chunk, meta) => {
        wake();
        if (meta && meta.decoderConfig) {
          if (meta.decoderConfig.description) this.description = new Uint8Array(meta.decoderConfig.description.slice ? meta.decoderConfig.description.slice(0) : meta.decoderConfig.description);
          if (meta.decoderConfig.codec) this.codec = meta.decoderConfig.codec;
        }
        let buf = new Uint8Array(chunk.byteLength);
        chunk.copyTo(buf);
        // H.264 with no avcC record is Annex B (the WebCodecs rule): MP4 wants
        // lengths in front of its NAL units, and a record, which the first
        // keyframe's parameter sets make (with a High profile's chroma format
        // and bit depths after them)
        if (/^avc[13]/.test(this.codec) && !this.description && (this.annexb || startsWithStartCode(buf))) {
          this.annexb = true;
          const conv = annexbToLengthPrefixed(buf);
          buf = conv.bytes;
          if (conv.sps && conv.pps && !this.madeDescription) this.madeDescription = movie.wasm.avcc_record(conv.sps, conv.pps) || null;
        }
        this.chunks.push({ pts: chunk.timestamp, sync: chunk.type === 'key', bytes: buf, desc: this.description || this.madeDescription || null, codec: this.codec });
      },
      error: (e) => {
        this.error = e;
        wake();
      },
    });
    if ('ondequeue' in enc) enc.addEventListener('dequeue', wake);
    let lastKey = -Infinity;
    let pending = null; // { frame, tUs } waiting for its duration
    const encodeOne = async (frame, tUs, durUs) => {
      while (enc.encodeQueueSize > 8 && !this.error) await wait();
      if (this.error) throw this.error;
      const f = new VideoFrame(frame, { timestamp: tUs, duration: durUs });
      frame.close();
      const key = tUs - lastKey >= 2e6 || this.frames === 0;
      if (key) lastKey = tUs;
      enc.encode(f, { keyFrame: key });
      f.close();
      this.frames++;
      ctx.onFrame();
    };
    const emit = async (frame, tSec) => {
      const tUs = Math.round(tSec * 1e6);
      if (pending) {
        const dur = Math.max(1, tUs - pending.tUs);
        await encodeOne(pending.frame, pending.tUs, dur);
        pending = null;
      }
      pending = { frame: new VideoFrame(frame, { timestamp: tUs }), tUs };
    };
    try {
      // a reader of its own: pieces decode at the same time as the writer copies
      const walked = await walkEdited(movie, piece, emit, { cancel: ctx.cancel, reader: movie.reader.fork() });
      this.softened += walked.softened;
      this.blended += walked.blended;
      this.warnings.push(...walked.warnings);
      if (pending) {
        await encodeOne(pending.frame, pending.tUs, ctx.medianUs);
        pending = null;
      }
      if (this.error) throw this.error;
      await enc.flush();
    } finally {
      if (pending) pending.frame.close();
      try {
        enc.close();
      } catch (e) {
        /* already closed */
      }
    }
    if (this.error) throw this.error;
    if (ctx.cancel && ctx.cancel()) throw new Error('cancelled');
    return this;
  }
}

/** Bytes copied at a time, at most, where samples are copied as they are. */
const MAX_RUN = 8 * 1024 * 1024;

/**
 * Copy samples `from` to `to` - 1 (decode order) of a track's table (`s`:
 * movie.v or movie.a) into `out` as they are, a run of samples that sit
 * next to each other in the file at a time; with `prefix` (Matroska's
 * header stripping: the bytes every packet starts with, put back in front)
 * one at a time. `done(i, j)` hears of each run (samples i to j - 1) once
 * it is written, and may throw to stop the copy (a cancel).
 */
async function copySamples(reader, out, s, from, to, done, prefix = null) {
  for (let i = from; i < to; ) {
    const off = s.offset[i];
    let len = s.size[i];
    let j = i + 1;
    if (!prefix) while (j < to && s.offset[j] === off + len && len + s.size[j] <= MAX_RUN) len += s.size[j++];
    const bytes = await reader.read(off, len);
    if (prefix) await out.write(prefix);
    await out.write(bytes.slice());
    done(i, j);
    i = j;
  }
}

/** A WebCodecs encoder, configured. Tests substitute their own. */
function defaultEncoder(config, callbacks) {
  const enc = new VideoEncoder(callbacks);
  enc.configure(config);
  return enc;
}

// ---- the export ----------------------------------------------------------------

/**
 * Export the project. Returns { blob (null when streamed to a file), warnings,
 * frames (re-encoded), copied, spans, mode, elapsedMs, ... }. `parallel`
 * sets how many pieces are re-encoded at once (0: by the machine);
 * `smartCut` off re-encodes everything; `plan` reuses a plan made for the
 * same encoder. Tests pass their own encoder (`candidate` with its config,
 * `makeEncoder` building it) and extra `spans` to re-encode.
 */
export async function exportMovie(env, movie, project, opts = {}) {
  try {
    return await exportOnce(env, movie, project, opts);
  } catch (e) {
    // the encoder's H.264 parameter sets could not be joined to the source's
    // (or to another encoder's): do it the plain way, the whole video through
    // one encoder, whose stream needs no joining
    if (!(e && e.splice) || (opts.smartCut === false && opts.parallel === 1)) throw e;
    if (opts.sink && opts.sink.reset) await opts.sink.reset();
    const res = await exportOnce(env, movie, project, { ...opts, smartCut: false, parallel: 1, plan: null });
    res.warnings.unshift(`This browser's H.264 encoder wrote its stream in a way Unflash could not join to the source (${e.message}), so the whole video was re-encoded by one encoder instead of copying the parts no section touches.`);
    return res;
  }
}

/** An error joining H.264 streams: the export is redone as a plain re-encode. */
class SpliceError extends Error {
  constructor(message) {
    super(message);
    this.splice = true;
  }
}

const hex = (b) => Array.from(b || [], (x) => x.toString(16).padStart(2, '0')).join(' ');

async function exportOnce(env, movie, project, { encoder, quality, sink = null, onProgress, cancel, smartCut = true, parallel = 0, spans = null, makeEncoder = null, candidate = null, plan: given = null } = {}) {
  const { wasm } = env;
  const fps = movie.fps;
  const cands = candidate ? [candidate] : await encoderCandidates(movie.width, movie.height, fps, quality);
  const chosen = encoder ? cands.find((c) => c.label === encoder) || cands[0] : cands[0];
  if (!chosen) throw new Error('This browser has no video encoder WebCodecs can use.');
  const plan = given || (await exportPlan(env, movie, project, { codec: chosen.config.codec, smartCut, parallel, spans }));
  const warnings = plan.warnings.slice();
  const total = plan.encoded + plan.copied;
  const started = performance.now();
  profile.reset();

  // --- the pieces: re-encoded ones run K at a time, in order -------------------
  let encodedFrames = 0;
  let copiedFrames = 0;
  let failed = null;
  const cancelled = () => !!failed || (cancel && cancel());
  const ctx = { movie, chosen, medianUs: Math.round(movie.medianDelta * 1e6), cancel: cancelled, makeEncoder: makeEncoder || defaultEncoder, onFrame: () => {} };
  const report = () => {
    if (onProgress) onProgress((encodedFrames + copiedFrames) / Math.max(1, total), encodedFrames, performance.now() - started, copiedFrames);
  };
  let tickCount = 0;
  ctx.onFrame = () => {
    encodedFrames++;
    if (++tickCount % 15 === 0) report();
  };
  const encodePieces = plan.pieces.filter((p) => p.kind === 'encode');
  const queue = encodePieces.slice();
  let running = 0;
  const K = Math.max(1, plan.parallel);
  const pump = () => {
    while (running < K && queue.length && !failed) {
      const piece = queue.shift();
      running++;
      const pe = new PieceEncoder(ctx, piece);
      piece.done = pe
        .run()
        .catch((e) => {
          failed = failed || e;
          throw e;
        })
        .finally(() => {
          running--;
          pump();
        });
      piece.done.catch(() => {});
    }
  };
  pump();

  // --- the writer: pieces in file order --------------------------------------
  const out = sink || new MemorySink();
  const mx = new wasm.Muxer();
  await out.write(mx.start());
  const samples = []; // { pts, sync, size } in file order
  const reader = movie.reader;
  const v = movie.v;
  // H.264: the track's parameter sets. With smart cut, the source's plus
  // each encoder's under ids of their own (samples renumbered to match);
  // without, the first encoder's, with any other encoder's renumbered. The
  // records are read (and repaired, see AvcRegistry) as they come.
  let registry = null;
  let rawOnly = null; // a lone encoder's record that could not be read: used as it is
  const rewriters = new Map(); // description bytes -> AvcRewriter | null
  let description = null;
  let codecString = chosen.config.codec;
  let softened = 0;
  let blended = 0;
  const rewriterFor = (desc) => {
    if (!/^avc1/.test(codecString)) return null;
    // H.264 with no record: its samples cannot be told apart from the source's
    if (!desc) {
      if (plan.mode === 'smart') throw new SpliceError('the encoder gave no parameter sets');
      return null;
    }
    const key = hex(desc);
    if (rewriters.has(key)) return rewriters.get(key);
    try {
      if (!registry && !rawOnly) {
        if (plan.mode === 'smart') registry = new wasm.AvcRegistry(movie.dx.track_description(movie.video.index));
        else {
          try {
            registry = new wasm.AvcRegistry(desc);
          } catch (e) {
            // one encoder's stream needs no joining: its record goes in as it came
            console.warn(`[unflash] the H.264 encoder's record could not be read (${e && e.message ? e.message : e}); it is used as it is:`, hex(desc));
            rawOnly = key;
            rewriters.set(key, null);
            return null;
          }
        }
      }
      if (rawOnly) throw new Error('a second encoder record next to one that could not be read');
      const rw = registry.register(desc);
      const use = rw.is_identity() ? null : rw;
      if (!use) rw.free();
      rewriters.set(key, use);
      return use;
    } catch (e) {
      const why = e && e.message ? e.message : String(e);
      console.warn(`[unflash] could not join the H.264 encoder's stream (${why}); its record:`, hex(desc), plan.mode === 'smart' ? '; the source record: ' + hex(movie.dx.track_description(movie.video.index)) : '');
      throw new SpliceError(why);
    }
  };
  const rewrite = (rw, bytes) => {
    try {
      return rw.rewrite_sample(bytes);
    } catch (e) {
      const why = e && e.message ? e.message : String(e);
      console.warn(`[unflash] could not renumber an H.264 sample (${why}); its first bytes:`, hex(bytes.subarray(0, 48)));
      throw new SpliceError(why);
    }
  };
  let blob;
  try {
    for (const piece of plan.pieces) {
      if (cancelled()) throw failed || new Error('cancelled');
      if (piece.kind === 'copy') {
        const offUs = Math.round(piece.offset * 1e6);
        await copySamples(reader, out, v, piece.from, piece.to, (i, j) => {
          for (let k = i; k < j; k++) samples.push({ pts: v.pts[k] + offUs, sync: !!v.sync[k], size: v.size[k] });
          copiedFrames += j - i;
          report();
          if (cancelled()) throw failed || new Error('cancelled');
        });
      } else {
        const pe = await piece.done;
        softened += pe.softened;
        blended += pe.blended;
        warnings.push(...pe.warnings);
        if (pe.codec) codecString = pe.codec;
        for (const c of pe.chunks) {
          if (c.desc && !description) description = c.desc;
          const rw = rewriterFor(c.desc);
          const bytes = rw ? rewrite(rw, c.bytes) : c.bytes;
          await out.write(bytes);
          samples.push({ pts: c.pts, sync: c.sync, size: bytes.byteLength });
        }
        pe.chunks.length = 0;
      }
    }
    if (cancel && cancel()) throw new Error('cancelled');

    // --- video samples: decode order, dts from the sorted presentation times ----
    const medianUs = ctx.medianUs;
    const sorted = samples.map((c) => c.pts).sort((a, b) => a - b);
    let shift = 0;
    for (let i = 0; i < samples.length; i++) shift = Math.max(shift, sorted[i] - samples[i].pts);
    if (registry) {
      // the merged (and repaired) record, and a codec string that covers it
      description = registry.record();
      codecString = `avc1.${Array.from(description.subarray(1, 4), (b) => b.toString(16).padStart(2, '0')).join('').toUpperCase()}`;
    } else if (plan.mode === 'smart' && !description) description = movie.dx.track_description(movie.video.index);
    const vt = mx.add_video_track(codecString, movie.width, movie.height, 1000000, description || new Uint8Array());
    // the video ends as much later as all the holds say (the last frame's own too)
    const endUs = Math.round((movie.tsMax + plan.holds.reduce((sum, h) => sum + h.seconds, 0)) * 1e6);
    for (let i = 0; i < samples.length; i++) {
      const dts = sorted[i] - shift;
      const dur = i + 1 < samples.length ? sorted[i + 1] - sorted[i] : Math.max(medianUs, endUs - sorted[i]);
      mx.add_sample(vt, dts, samples[i].pts, Math.max(1, dur), samples[i].sync, samples[i].size);
    }

    // --- audio: copied as it is when an MP4 can hold it and no frame is held;
    // else re-encoded, with silence under the held frames ------------------------
    if (movie.audio && movie.a) {
      const holds = plan.holds;
      let copy = movie.audio.copyable && !holds.length;
      if (!copy) {
        // (the frames are done by now: the sound has a progress figure of its own)
        const onSound = (f) => onProgress && onProgress(1, encodedFrames, performance.now() - started, copiedFrames, f);
        const res = await reencodeAudio(wasm, movie, reader, mx, out, { cancel: cancelled, holds, onProgress: onSound });
        if (res.warning) warnings.push(res.warning);
        // could not re-encode at all: the sound as it is beats none
        copy = !res.wrote && movie.audio.copyable;
      }
      if (copy) {
        const at = mx.add_copy_track('audio', movie.dx.track_sample_entry(movie.audio.index), movie.audio.timescale, 0, 0);
        const a = movie.a;
        // Matroska header stripping: the bytes every packet starts with go back in front
        const prefix = movie.audio.prefix && movie.audio.prefix.length ? Uint8Array.from(movie.audio.prefix) : null;
        await copySamples(
          reader,
          out,
          a,
          0,
          a.offset.length,
          (i, j) => {
            for (let k = i; k < j; k++) mx.add_sample(at, a.dtsTicks[k], a.ptsTicks[k], a.durTicks[k], true, a.size[k] + (prefix ? prefix.length : 0));
            if (cancelled()) throw new Error('cancelled');
          },
          prefix
        );
      }
    }
    if (movie.audioTracks > 1) {
      const label = (t) => (t.language && t.language !== 'und' ? `${t.language}, ${soundName(t.codec)}` : soundName(t.codec));
      const skipped = movie.audioSkipped || [];
      warnings.push(`The source has ${movie.audioTracks} audio tracks; the export keeps one (${label(movie.audio)})${skipped.length ? `, the first that can be played here: not ${skipped.map(label).join(' or ')}` : ''}.`);
    }
    if (movie.subtitleTracks) warnings.push(`The source's subtitle track${movie.subtitleTracks === 1 ? ' is' : 's are'} left out, as the original tool leaves them out: an MP4 export carries the picture and the sound.`);

    const moov = mx.finish();
    await out.patch(mx.patch_offset(), mx.patch_bytes());
    await out.write(moov);
    blob = await out.close();
  } catch (e) {
    failed = failed || e;
    // let the running pieces wind down before the caller aborts the sink
    await Promise.allSettled(encodePieces.map((p) => p.done).filter(Boolean));
    // what went wrong first: a piece that failed stops the others, which
    // then say 'cancelled', and the one the writer waits on may be one of them
    throw failed;
  } finally {
    // (the muxer and the rewriters live in WebAssembly memory, however the export ends)
    for (const rw of rewriters.values()) if (rw) rw.free();
    if (registry) registry.free();
    mx.free();
  }
  const elapsedMs = performance.now() - started;
  profile.report(`export (${chosen.label}, ${plan.mode})`, encodedFrames, elapsedMs);
  return { blob, warnings, frames: encodedFrames, copied: copiedFrames, spans: encodePieces.length, mode: plan.mode, parallel: K, softened, blended, elapsedMs, codec: codecString, encoderLabel: chosen.label, holds: plan.holds };
}

/** Seconds as the export's notes say them: "1 s", "0.5 s". */
const secs = (x) => `${Math.round(x * 100) / 100} s`;

/**
 * The sound re-encoded with WebCodecs (AAC where this browser has an AAC
 * encoder, else Opus) and written after the video as it comes out. Two
 * reasons: frames are held (`holds`, the plan's, in source seconds), and
 * each hold puts `seconds` of silence into the sound at `at`, the moment
 * the held frame's next frame would have come, so the sound waits exactly
 * where the picture does; or an MP4 can't hold the source's sound as it is
 * (Vorbis, PCM; DTS from an MKV or a transport stream). Returns
 * `{ warning, wrote }`: what was done, or why there is no sound; `wrote`
 * false means nothing went into the file.
 */
async function reencodeAudio(wasm, movie, reader, mx, out, { cancel, holds = [], onProgress = null } = {}) {
  const at = movie.audio;
  const a = movie.a;
  const name = soundName(at.codec); // (as people know it: DTS, not dtsc)
  const held = holds.length > 0;
  const lengths = [...new Set(holds.map((h) => h.seconds))];
  const silence = lengths.length === 1 ? `${secs(lengths[0])} of silence` : 'silence';
  // why the sound is re-encoded, and what happens when it can't be
  const why = held ? `Each held frame (E mark) needs ${silence} under it` : `The audio (${name}) can't go into an MP4 as it is`;
  const without = held && at.copyable ? `so the sound was copied as it is and runs ahead of the picture after each held frame (by ${secs(holds.reduce((s, h) => s + h.seconds, 0))} at the end)` : 'so the export has no sound';
  if (typeof AudioEncoder === 'undefined') return { warning: `${why}, and this browser can't re-encode audio, ${without}.`, wrote: false };
  const dcfg = soundConfig(at, movie.dx.track_description(at.index));
  // (WebCodecs', or the app's own for AC-3, E-AC-3 and DTS, whose sound comes mixed down to stereo, and PCM)
  const found = await soundDecoderFor(dcfg);
  if (!found) return { warning: `${why}, and ${noSoundDecoder(at.codec, `the audio (${name})`)}, ${without}.`, wrote: false };
  const channelsIn = found.channels;
  const pick = async (rate, channels) => {
    for (const c of [
      { codec: 'mp4a.40.2', sampleRate: rate, numberOfChannels: channels, bitrate: 96000 * Math.min(2, channels) },
      { codec: 'opus', sampleRate: rate, numberOfChannels: channels, bitrate: 80000 * Math.min(2, channels) },
    ]) {
      try {
        if ((await AudioEncoder.isConfigSupported(c)).supported) return c;
      } catch (e) {
        /* try the next */
      }
    }
    return null;
  };
  let ecfg = await pick(at.sample_rate, channelsIn);
  if (!ecfg) return { warning: `${why}, and this browser has no encoder for its ${channelsIn} channels, ${without}.`, wrote: false };
  let error = null;
  // woken by the decoder or the encoder (the timer a safety net)
  const { wait, wake } = waker(50);
  const chunks = []; // encoded, waiting to be written
  const decoded = []; // decoded, waiting to be placed
  let outCfg = null;
  let enc = null;
  const dec = new found.Decoder({
    output: (data) => {
      decoded.push(data);
      wake();
    },
    error: (e) => {
      error = error || e;
      wake();
    },
  });
  dec.configure(dcfg);
  // the track is added once the encoder has said what it makes
  let track = -1;
  let rate = 0;
  let written = 0;
  const flushOut = async (all) => {
    // keep the last chunk back until the end: its duration comes from the next
    while (chunks.length > (all ? 0 : 1)) {
      if (track < 0) {
        const cfg = outCfg || ecfg;
        rate = (cfg && cfg.sampleRate) || (ecfg.codec === 'opus' ? 48000 : at.sample_rate);
        const d = outCfg && outCfg.description ? new Uint8Array(outCfg.description.slice ? outCfg.description.slice(0) : outCfg.description) : new Uint8Array();
        const entry = wasm.audio_sample_entry(ecfg.codec, d, rate, (cfg && cfg.numberOfChannels) || at.channels);
        track = mx.add_copy_track('audio', entry, rate, 0, 0);
      }
      const c = chunks.shift();
      const next = chunks[0];
      const ticks = (us) => Math.round((us * rate) / 1e6);
      const dur = next ? ticks(next.ts) - ticks(c.ts) : ticks(c.dur) || 1;
      await out.write(c.bytes);
      mx.add_sample(track, ticks(c.ts), ticks(c.ts), Math.max(1, dur), true, c.bytes.byteLength);
      written++;
    }
  };
  // the decoded sound placed on the edited timeline (silence under the holds)
  let run = null;
  const place = async () => {
    while (decoded.length && !error) {
      const data = decoded.shift();
      try {
        if (!run) {
          // the encoder takes what the decoder makes (HE-AAC decodes at twice its declared rate)
          if (data.sampleRate !== ecfg.sampleRate || data.numberOfChannels !== ecfg.numberOfChannels) {
            const other = await pick(data.sampleRate, data.numberOfChannels);
            if (other) ecfg = other;
          }
          enc = new AudioEncoder({
            output: (chunk, meta) => {
              if (meta && meta.decoderConfig && !outCfg) outCfg = meta.decoderConfig;
              const b = new Uint8Array(chunk.byteLength);
              chunk.copyTo(b);
              chunks.push({ bytes: b, ts: chunk.timestamp, dur: chunk.duration || 0 });
              wake();
            },
            error: (e) => {
              error = error || e;
              wake();
            },
          });
          enc.configure(ecfg);
          run = new SoundRun(ecfg.sampleRate, ecfg.numberOfChannels, holds, (planes, n, start) => {
            const d = audioData(planes, n, start, ecfg.sampleRate);
            enc.encode(d);
            d.close();
          });
        }
        run.add(data);
      } finally {
        data.close();
      }
    }
  };
  try {
    for (let i = 0; i < a.offset.length && !error; i++) {
      if (cancel && cancel()) throw new Error('cancelled');
      while ((dec.decodeQueueSize > 16 || (enc && enc.encodeQueueSize > 16)) && !error) {
        await place();
        await wait();
      }
      dec.decode(await soundChunk(movie, reader, i));
      await place();
      await flushOut(false);
      if (onProgress && i % 200 === 0) onProgress(i / a.offset.length);
    }
    if (!error) await dec.flush();
    if (!error) await place();
    if (!error && run) run.finish();
    if (!error && enc) await enc.flush();
    if (error) throw error;
    await flushOut(true);
  } catch (e) {
    if (String(e && e.message).includes('cancelled')) throw e;
    return { warning: `${why}, and re-encoding the sound failed (${e && e.message ? e.message : e})${written ? ', so part of it is missing' : `, ${without}`}.`, wrote: written > 0 };
  } finally {
    for (const d of decoded) d.close();
    try {
      dec.close();
    } catch (e) {
      /* closed */
    }
    try {
      if (enc) enc.close();
    } catch (e) {
      /* closed */
    }
  }
  // (a surround sound the built-in decoder read comes out in stereo)
  const to = `${ecfg.codec === 'opus' ? 'Opus' : 'AAC'}${found.channels < at.channels ? `, in stereo (its ${at.channels} channels mixed down)` : ''}`;
  const placed = run ? run.placed : 0;
  if (held) {
    const after = holds.length - placed;
    return { warning: `The sound was re-encoded to ${to} to put ${silence} under each held frame (E marks), where the picture waits.${after ? ` ${after} of them come${after === 1 ? 's' : ''} after the sound ends.` : ''}`, wrote: true };
  }
  return { warning: `The audio (${name}) can't go into an MP4 as it is, so it was re-encoded to ${to}.`, wrote: true };
}

let blurCanvas = null;
let blendCanvas = null;

/**
 * A frame mixed with the frames either side of it, by weights `w` (the
 * frame's own, `prev`'s, `next`'s; they add to one): the canvas lays each
 * over what is there at the share that makes the running mix come out
 * right. Mixed in 8-bit sRGB values, as the check mixes its small copies.
 */
function blendFrame(frame, prev, next, w) {
  const width = frame.displayWidth || frame.codedWidth;
  const height = frame.displayHeight || frame.codedHeight;
  if (!blendCanvas || blendCanvas.width !== width || blendCanvas.height !== height) blendCanvas = new OffscreenCanvas(width, height);
  const ctx = blendCanvas.getContext('2d');
  ctx.globalAlpha = 1;
  ctx.drawImage(frame, 0, 0, width, height);
  let sum = w[0];
  for (const [f, wk] of [[prev, w[1]], [next, w[2]]]) {
    if (!f || !(wk > 0)) continue;
    sum += wk;
    ctx.globalAlpha = Math.min(1, wk / sum);
    ctx.drawImage(f, 0, 0, width, height);
  }
  ctx.globalAlpha = 1;
  return new VideoFrame(blendCanvas, { timestamp: frame.timestamp || 0 });
}
/**
 * A Gaussian-blurred copy of a frame (σ in source pixels). The sharp frame
 * is drawn first so the blur's transparent fringe at the picture edge shows
 * the original there rather than black.
 */
function blurFrame(frame, sigma) {
  const w = frame.displayWidth || frame.codedWidth;
  const h = frame.displayHeight || frame.codedHeight;
  if (!blurCanvas || blurCanvas.width !== w || blurCanvas.height !== h) blurCanvas = new OffscreenCanvas(w, h);
  const ctx = blurCanvas.getContext('2d');
  ctx.filter = 'none';
  ctx.drawImage(frame, 0, 0, w, h);
  ctx.filter = `blur(${Math.max(0.5, sigma).toFixed(2)}px)`;
  ctx.drawImage(frame, 0, 0, w, h);
  ctx.filter = 'none';
  return new VideoFrame(blurCanvas, { timestamp: frame.timestamp || 0 });
}

export async function pickSaveSink(suggestedName) {
  if (!window.showSaveFilePicker) return null;
  const t0 = performance.now();
  try {
    const handle = await window.showSaveFilePicker({ suggestedName, types: [{ description: 'MP4 video', accept: { 'video/mp4': ['.mp4'] } }] });
    const writable = await handle.createWritable();
    return { sink: new FileSink(writable), handle };
  } catch (e) {
    // a real cancel takes the user a moment; an instant AbortError means the
    // picker could not be shown at all, so fall back to an in-memory file
    if (e && e.name === 'AbortError' && performance.now() - t0 > 400) return { cancelled: true };
    return null;
  }
}
