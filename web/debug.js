// "Debug info": a plain-text account of this browser, its GPU, the open
// video and the jobs of this visit (with the last scan's time per
// operation), for someone helping with a slow or failing run. File names
// are left out.

const errors = [];
const jobs = [];
const names = new Set(); // the files opened this visit, which the report leaves out
let hiddenSince = document.visibilityState === 'hidden' ? performance.now() : null;
let hiddenTotal = 0;

/** Time the tab has spent out of sight this visit, in ms. */
function hiddenMs() {
  return hiddenTotal + (hiddenSince === null ? 0 : performance.now() - hiddenSince);
}

/** Keep the page's last errors and how long it was out of sight, for the report. */
export function watchPage() {
  window.addEventListener('error', (e) => noteError(e.message || String(e.error)));
  window.addEventListener('unhandledrejection', (e) => noteError(`unhandled: ${e.reason && e.reason.message ? e.reason.message : e.reason}`));
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'hidden') {
      if (hiddenSince === null) hiddenSince = performance.now();
    } else if (hiddenSince !== null) {
      hiddenTotal += performance.now() - hiddenSince;
      hiddenSince = null;
    }
  });
}

/** A file opened this visit: the report will not name it. */
export function noteFileName(name) {
  if (name) names.add(name);
}

export function noteError(text) {
  errors.push(`${clock(Date.now())} ${text}`);
  if (errors.length > 20) errors.shift();
}

/**
 * A job starts: returns what to call when it ends, with its outcome. Its
 * `.step(message)` notes each thing the job says it is doing, so that the
 * report of a long job says where the time went (a message whose numbers
 * change as it goes, a count or a percentage, is one step, and so is one
 * whose running status, after a " · ", changes: a scan's counts are one).
 */
export function noteJob(name) {
  const at = Date.now();
  const t0 = performance.now();
  const h0 = hiddenMs();
  const steps = [];
  const ended = (outcome) => {
    const end = performance.now();
    const timed = steps.map((s, k) => ({ what: s.varied ? s.label.replace(/#/g, '…') : s.first, ms: (k + 1 < steps.length ? steps[k + 1].t : end) - s.t }));
    jobs.push({ name, at, ms: end - t0, hidden: hiddenMs() - h0, outcome, steps: timed });
    if (jobs.length > 40) jobs.shift();
  };
  ended.step = (message) => {
    const text = String(message).split(' · ')[0].trim();
    if (!text) return;
    const label = text.replace(/\d[\d.,]*/g, '#');
    const last = steps[steps.length - 1];
    if (last && last.label === label) {
      if (text !== last.first) last.varied = true;
      return;
    }
    steps.push({ label, first: text, varied: false, t: performance.now() });
  };
  return ended;
}

/** The section player's sound, as its button says it, and the browser's audio output. */
function soundLine({ status, failed, output, builtIn }) {
  const what = { off: 'off', on: 'on', slow: 'on (at 1× only; the player is slowed)', failed: `none: ${failed}`, held: 'held back by the browser' }[status] || status;
  return `${what}${builtIn ? `, decoded by the built-in ${builtIn} decoder` : ''}${output ? ` (audio output ${output})` : ''}`;
}

/** Where a job of more than two seconds spent its time: the steps that took a good part of it, in order. */
function stepLine(j) {
  if (j.ms < 2000 || !j.steps || j.steps.length < 2) return null;
  const big = j.steps.filter((s) => s.ms >= Math.max(200, j.ms * 0.05)).slice(0, 6);
  return big.length ? '  ' + big.map((s) => `${s.what} ${secs(s.ms)}`).join(' · ') : null;
}

const clock = (ms) => new Date(ms).toISOString().slice(11, 19);
const secs = (ms) => `${(ms / 1000).toFixed(1)} s`;
const int = (n) => Math.round(n).toLocaleString('en');

function duration(s) {
  // (rounded to tenths first: 59.96 s is 1:00.0, not 0:60.0)
  const t = Math.round(s * 10) / 10;
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  const r = t - h * 3600 - m * 60;
  return `${h ? `${h}:${String(m).padStart(2, '0')}` : m}:${r.toFixed(1).padStart(4, '0')}`;
}

function bytes(n) {
  return n >= 1e9 ? `${(n / 1e9).toFixed(2)} GB` : `${(n / 1e6).toFixed(1)} MB`;
}

/** A labelled block of lines, the ones after the first indented under the label. */
const LABEL = 10;
function block(label, lines) {
  const out = [];
  lines.filter((l) => l !== null && l !== undefined && l !== '').forEach((l, i) => out.push(`${(i ? '' : label).padEnd(LABEL)}${l}`));
  return out.length ? out : [`${label.padEnd(LABEL)}none`];
}

/** How a chunked scan went: its chunks, the early looks, the pictures held and what each decoder did. */
function chunkedLines(h) {
  if (h.fallback) return [`chunked: ${h.chunks} chunks of about ${h.chunkS} s, but ${h.fallback}: scanned in one piece`];
  const lanes = h.lanes.map((l) => {
    const fps = l.frames / Math.max(0.001, l.ms / 1000);
    const looks = l.looks ? `, ${l.looks} early look${l.looks === 1 ? '' : 's'}` : '';
    const over = `${l.steals ? `, took over ${l.steals} from slower lanes` : ''}${l.stolen ? `, ${l.stolen} taken over by faster lanes` : ''}`;
    // (set aside by the rebalancing: the other lanes would finish every chunk in reach sooner)
    const aside = l.aside > 500 ? `, set aside ${secs(l.aside)}${l.fps ? ` (it decoded ${l.fps.toFixed(0)} fps)` : ''}` : '';
    return `${l.kind === 'built-in' ? `built-in decoder ×${l.workers}` : "browser's decoder"}: ${int(l.frames)} frames, ${fps.toFixed(0)} fps, ${l.chunks} chunk${l.chunks === 1 ? '' : 's'}${looks}${over}${aside}${l.failed ? `, gave up: ${l.failed}` : ''}`;
  });
  const looks = h.order === 'triage' ? `, early looks at ${h.looks.length} of the ${h.hot.length} likeliest to flash` : '';
  const balance = h.rebalanced ? `; each chunk to the lane that would finish it first${h.grown ? `, the built-in decoder grown by ${h.grown} worker${h.grown === 1 ? '' : 's'} for the lanes set aside` : ''}` : '';
  return [`chunked: ${h.chunks} chunks of about ${h.chunkS} s, detected in file order${looks}${balance}; at most ${bytes(h.peak)} of pictures held (${bytes(h.budget)} allowed)`, ...lanes];
}

/** The usual time between keyframes (the median, s), or null: how far apart a decoder can start. */
function keyframeSpacing(m) {
  const v = m.v;
  if (!v || !v.sync || !v.pts) return null;
  const keys = [];
  for (let i = 0; i < v.sync.length; i++) if (v.sync[i]) keys.push(v.pts[i]);
  if (keys.length < 2) return null;
  keys.sort((a, b) => a - b);
  const gaps = keys
    .slice(1)
    .map((t, i) => t - keys[i])
    .sort((a, b) => a - b);
  return gaps[gaps.length >> 1] / 1e6;
}

/** A scan's lines: frames, time, pace, and how its chunks were decoded. */
function scanLines(s, fps0) {
  const fps = s.frames / Math.max(0.001, s.elapsedMs / 1000);
  const head = `${int(s.frames)} frames in ${secs(s.elapsedMs)} = ${fps.toFixed(0)} fps${fps0 ? ` (${(fps / fps0).toFixed(1)}× real time)` : ''}`;
  return s.chunked ? [head, ...chunkedLines(s.chunked)] : [`${head} · ${s.segments || 1} segment${(s.segments || 1) === 1 ? '' : 's'}`];
}

/**
 * The report, from the page's `state` and `profile`, the app's `version`
 * and `build` (the published site's name for this build of its code), the
 * WebGPU adapter's description of itself (`gpu`), the number of
 * segments a scan would use and the hybrid plan it would follow (if any).
 */
export function debugReport({ version, build = null, state, profile, gpu, segments, hybrid = null, sound = null }) {
  const lines = [`Unflash debug info · ${new Date().toISOString().slice(0, 19).replace('T', ' ')} UTC`];
  lines.push(...block('App', [`${version}${build ? `, build ${build}` : ''} · ${location.origin}${location.pathname}${location.search ? ` · options ${location.search}` : ''}`]));
  const nav = navigator;
  const ua = nav.userAgentData;
  lines.push(
    ...block('Browser', [
      nav.userAgent,
      [ua && ua.platform, `${nav.hardwareConcurrency || '?'} logical cores`, nav.deviceMemory ? `${nav.deviceMemory} GB of memory or more` : null, `screen ×${window.devicePixelRatio || 1}`, `tab ${document.visibilityState === 'hidden' ? 'out of sight' : 'in front'}`, window.crossOriginIsolated ? 'cross-origin isolated' : null].filter(Boolean).join(' · '),
    ])
  );
  const g = gpu && gpu.info;
  const noAdapter = state.env && /no WebGPU adapter/.test(state.env.feeder.detail || '');
  lines.push(
    ...block('GPU', [
      !navigator.gpu ? 'no WebGPU in this browser' : noAdapter ? 'WebGPU, but no adapter given to the page' : g ? [g.vendor, g.architecture, g.device, g.description].filter(Boolean).join(' · ') || 'WebGPU (the adapter gives no description)' : 'WebGPU (adapter not described)',
      gpu && gpu.fallback ? 'a fallback (software) adapter' : null,
    ])
  );
  const env = state.env;
  const m = state.movie;
  if (env) {
    const f = env.feeder;
    const det = [];
    det.push(`${f.backend === 'webgpu' ? 'WebGPU' : 'CPU (WebAssembly)'} at ${f.aw}×${f.ah}${f.detail || f.note ? ` (${f.detail || f.note})` : ''}`);
    det.push(`pictures reach it as: ${f.route || 'nothing yet'}${f.routeDetail ? ` (${f.routeDetail})` : ''}${f.takesFrames === false && f.backend === 'webgpu' ? ' · this WebGPU takes no decoded frame' : ''}`);
    const lanes = hybrid ? hybrid.hw : segments;
    const scans = `long files scanned in chunks by ${lanes} lane${lanes === 1 ? '' : 's'} of the browser's decoder${hybrid && hybrid.sw ? ` + the built-in decoder in ${hybrid.sw} worker${hybrid.sw === 1 ? '' : 's'}` : ''}, one detector in order`;
    det.push(`decoding: ${state.decode && state.decode.software && m && m.builtIn ? `the built-in ${m.builtIn.name} decoder` : m && m.decodeInWorkers ? 'WebCodecs, in workers' : 'WebCodecs, on the page'} · ${scans}`);
    lines.push(...block('Detector', det));
  }
  if (m) {
    const v = m.video || {};
    const a = m.audio;
    const spacing = keyframeSpacing(m);
    lines.push(
      ...block('Video', [
        [`${String(m.format || '?').toUpperCase()}${m.info && m.info.fragmented ? ' (fragmented)' : ''}`, v.codec, `${m.width}×${m.height}`, `${(m.fps || 0).toFixed(3)} fps`, duration(m.duration || 0), `${int(m.frameCount || 0)} frames`, m.file ? bytes(m.file.size) : null, spacing ? `keyframes every ${spacing.toFixed(1)} s` : null].filter(Boolean).join(' · '),
        a ? `audio ${a.codec}${a.copyable === false ? ' (re-encoded on export)' : ''}${m.audioTracks > 1 ? ` · track ${(m.audioSkipped || []).length + 1} of ${m.audioTracks}${(m.audioSkipped || []).length ? ` (${m.audioSkipped.map((t) => t.codec).join(', ')} before it can't be played here)` : ''}` : ''}${sound ? ` · the section player's sound: ${soundLine(sound)}` : ''}` : 'no audio',
        state.decode && !state.decode.supported && state.decode.reason ? `cannot decode: ${state.decode.reason}` : null,
      ])
    );
  } else lines.push(...block('Video', ['none open']));
  const s = state.lastScan;
  if (s) lines.push(...block('Scan', scanLines(s, m && m.fps)));
  // (the check of an export is a scan of its own, of the exported file)
  if (state.lastVerify) lines.push(...block('Verify', scanLines(state.lastVerify, m && m.fps)));
  lines.push(...block('Jobs', jobs.flatMap((j) => [`${clock(j.at)} ${j.name}: ${secs(j.ms)} ${j.outcome}${j.hidden > 500 ? ` (${secs(j.hidden)} of it out of sight)` : ''}`, stepLine(j)])));
  const job = state.job;
  if (job) lines.push(...block('Running', [`${job.name}: ${secs(performance.now() - (job.t0 || performance.now()))} so far, ${job.pct || 0}% done (the report of it comes when it ends)`]));
  if (state.project) {
    const n = state.project.sections.length;
    lines.push(...block('Project', [`${n} section${n === 1 ? '' : 's'} · caches ${bytes(state.project.cacheBytes())}`]));
  }
  if (s && s.profileText) lines.push('', 'Last scan, time per operation:', s.profileText);
  const now = profile.summary();
  if (now && !(s && now === s.profileOps)) lines.push('', 'The last job, time per operation:', now);
  lines.push('', ...block('Errors', errors.slice()));
  let text = lines.join('\n');
  // no file names
  for (const name of [...names].sort((a, b) => b.length - a.length)) text = text.split(name).join('<a file>');
  return text;
}
