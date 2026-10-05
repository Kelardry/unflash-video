// Unflash web app: wiring between the WASM detector, WebCodecs and the UI.

import init, * as wasm from './pkg/unflash.js';
import { defaultWorkerCount, SoftwarePool } from './h264pool.js';
import { builtInFor } from './codecs.js';
import { Movie, decodeRange } from './media.js';
import { createDetector, gpuAdapter } from './detector.js';
import { profile } from './profile.js';
import { scanMovie, scanChunks, CHUNK_S, prepareSection, checkSection, suggestEdits, suggestFrameRate, searchFrameRate, suggestBlend, rateLadder, keepJson, shownPts, softenPlan, blendMarks, blendStrength, blendedFrames, hasMarks, BLEND_DEFAULT } from './analysis.js';
import { Project, projectKey, dropCaches, lastSavedAt, projectFileText, readProjectFile, matchVideo } from './project.js';
import { exportMovie, exportPlan, encoderCandidates, formatChoices, formatInfo, pickSaveSink, privateFileSink, privateStorageAvailable, discardPrivateExport, findPrivateExport, estimateExportBytes } from './export.js';
import { SectionPlayer } from './preview.js';
import { SectionSound, soundName } from './sound.js';
import { FrameViewer, MIN_GAP_MS } from './viewer.js';
import { loadAlertSettings, saveAlertSettings, beep, askNotifyPermission, notifyState, systemNotify, titleProgress, titleMark } from './alerts.js';
import { loadChangelog, changesSeen, markChangesSeen, hadEarlierSettings, changesSince, newestChange, renderDay, wireShots, escapeHtml } from './changes.js';
import { TourGuide, TOURS, PARTS } from './tours.js';
import { watchPage, noteError, noteJob, noteFileName, debugReport } from './debug.js';
import { stored, store } from './storage.js';

/** The page's query: settings for tests and for trying things out (`?cpu=1`, `?segments=3`). */
const QUERY = new URLSearchParams(location.search);

/** `?name=1` or `on`: true; `0` or `off`: false; anything else, or none: null. */
function onOff(name) {
  const v = QUERY.get(name);
  return v === '1' || v === 'on' ? true : v === '0' || v === 'off' ? false : null;
}

/**
 * The build this page runs. The published site keeps each build's code in
 * a folder of its own, v/<build>/ (site.mjs), which this module was loaded
 * from; where the code is served as it stands (a copy of the repository,
 * the tests) there is none, unless ?build= names one to act as.
 */
const BUILD = (new URL(import.meta.url).pathname.match(/\/v\/([^/]+)\/[^/]+$/) || [])[1] || QUERY.get('build') || null;

const $ = (id) => document.getElementById(id);

/**
 * Bytes of decoded section frames kept in memory (older sections are dropped
 * and re-prepared when opened again): a share of the device's memory where
 * the browser says how much there is, a quarter of a gigabyte otherwise.
 */
function cacheBudget() {
  const gb = navigator.deviceMemory || 4;
  return Math.min(700, Math.max(192, gb * 64)) * 1024 * 1024;
}

/** The largest export worth assembling in memory when no disk sink is available. */
function memoryExportLimit() {
  const gb = navigator.deviceMemory || 4;
  return Math.min(1, gb / 4) * 1024 * 1024 * 1024;
}

function fmtBytes(b) {
  return b >= 1e9 ? `${(b / 1e9).toFixed(1)} GB` : `${Math.max(1, Math.round(b / 1e6))} MB`;
}

const state = {
  movie: null,
  project: null,
  config: null,
  env: null, // { wasm, config, feeder, spares } for scans and checks (spares: spareFeeders')
  liveFeeder: null,
  current: null,
  selection: new Set(),
  anchor: null,
  job: null,
  live: { on: false, fromScan: false, t: [], hazard: [], hazardRed: [], lastCheck: 0 },
  decode: { supported: false, reason: '' },
  exportBlob: null,
  exportHolds: null, // the export's held frames ({ at, seconds } on the video's clock), when it was made here
  selectOnOpen: null, // { id, lo, hi, kind }: the frames to select once section `id` is prepared (a verify found `kind` at lo..hi in the video)
  checkTimer: null,
  checkRunning: false,
  checkAgain: false,
  // a running scan's own: { violations: what it has found so far (chunked
  // scans), partials: when, trace: its per-frame trace so far }, else null
  scanning: null,
  traceNorm: null, // the scan trace as area fractions, for the timeline and the monitor
  auto: null, // the unattended scan -> fix -> export -> verify run (see autopilot)
  // what the player shows: 'video' (the whole file) or the open section, 'edited' or 'original';
  // gen counts requests to it, so a poster that was overtaken never draws
  player: { mode: 'video', lastDraw: 0, gen: 0, playingTile: null, restartTimer: null },
  history: new Map(), // section id -> { undo: [], redo: [] } of mark snapshots
  // seconds of the whole video the chart shows around the playhead (0: all of it)
  chartSpan: 30,
  changes: null, // what's new: { log, newest, seen } (see initChanges)
};

let sectionPlayer = null;
const sectionSound = new SectionSound();

// read before this visit writes settings of its own: were they here already?
const EARLIER_SETTINGS = hadEarlierSettings();
watchPage();

// ---- small helpers -----------------------------------------------------------

function fmt(t) {
  return wasm.format_time(t);
}

function toast(msg, ms = 3500) {
  const el = $('toast');
  if (chain.active) chain.toast = msg;
  el.textContent = msg;
  el.classList.remove('hidden');
  clearTimeout(el._t);
  el._t = setTimeout(() => el.classList.add('hidden'), ms);
}

function banner(msg, kind = 'error') {
  const b = $('banner');
  $('bannerText').textContent = msg;
  b.classList.toggle('info', kind === 'info');
  b.classList.remove('hidden');
}

function setStatus(parts) {
  $('status').innerHTML = parts.map((p) => `<span>${p}</span>`).join('');
}

// Checks, scans, prepares and suggestions all feed the one detector, and a
// check the UI schedules is not a job: everything that touches the feeder is
// serialised here.
let feederLock = Promise.resolve();
function withFeeder(fn) {
  const run = feederLock.then(() => fn());
  feederLock = run.then(
    () => {},
    () => {}
  );
  return run;
}

/**
 * Whether a job is under way, telling the visitor so (`then`: what they
 * wanted, to do once it is over). Asked before anything is changed: a start
 * that runJob turns away must leave the running job's state alone.
 */
function busy(then) {
  if (!state.job) return false;
  toast(`${state.job.name} is under way: ${then} once it has finished (or cancel it).`, 6000);
  return true;
}

async function runJob(name, fn) {
  if (state.job) {
    toast('Another job is running');
    return null;
  }
  const job = { name, cancelled: false, t0: performance.now(), pct: 0 };
  // (settled once the job is over: what waits for it, afterJobs, waits on this)
  job.ended = new Promise((r) => (job.end = r));
  state.job = job;
  const note = noteJob(name);
  chainStart(name);
  $('jobName').textContent = name;
  $('jobBar').style.width = '0%';
  $('jobMsg').textContent = '';
  $('jobbar').classList.remove('hidden');
  let shownPct = -1;
  let shownAt = 0;
  const progress = (p, msg) => {
    if (msg !== undefined) note.step(msg);
    const pct = Math.round(Math.min(1, Math.max(0, p)) * 100);
    job.pct = pct;
    // a long scan reports thousands of times: the page shows ten a second
    const now = performance.now();
    if (pct === shownPct && now - shownAt < 100) return;
    shownAt = now;
    $('jobBar').style.width = `${pct}%`;
    if (msg !== undefined) $('jobMsg').textContent = msg;
    // the tab's title too, to be seen from another tab
    if (pct !== shownPct) {
      shownPct = pct;
      titleProgress(`${pct}% · ${name} · Unflash`);
    }
  };
  titleProgress(`${name} · Unflash`);
  let outcome = 'ok';
  let message = '';
  try {
    const res = await withFeeder(() => fn(progress, () => job.cancelled));
    // (a job that does not ask `cancelled` runs to its end: what it made after a cancel is not wanted)
    if (job.cancelled) throw new Error('cancelled');
    return res;
  } catch (e) {
    if (job.cancelled) {
      outcome = 'cancelled';
      toast(`${name}: cancelled`);
      return null;
    }
    console.error(e);
    outcome = 'failed';
    message = `${name} failed: ${e && e.message ? e.message : e}`;
    noteError(message);
    banner(message);
    return null;
  } finally {
    state.job = null;
    state.jobEndedAt = performance.now();
    $('jobbar').classList.add('hidden');
    titleProgress('');
    note(job.cancelled ? 'cancelled' : outcome);
    chainEnd(job.cancelled ? 'cancelled' : outcome, message);
    job.end();
  }
}

// ---- telling someone who looked away that the wait is over ----------------------
//
// Jobs that follow one another (opening then scanning; every stage of an
// auto-fix run) are one wait: the beep and the notification come once, when
// no further job has started a moment after the last one ended, and only if
// the whole wait took longer than the setting.

let alertSettings = loadAlertSettings();
const CHAIN_GRACE_MS = 1500;
const chain = { active: false, start: 0, end: 0, names: [], ok: true, cancelled: false, error: '', toast: '', timer: null };

function chainStart(name) {
  clearTimeout(chain.timer);
  if (!chain.active) Object.assign(chain, { active: true, start: performance.now(), names: [], ok: true, cancelled: false, error: '', toast: '' });
  chain.names.push(name);
}

function chainEnd(outcome, message) {
  if (!chain.active) return;
  if (outcome === 'failed') {
    chain.ok = false;
    chain.error = message;
  } else if (outcome === 'cancelled') chain.cancelled = true;
  chain.end = performance.now();
  clearTimeout(chain.timer);
  chain.timer = setTimeout(finishChain, CHAIN_GRACE_MS);
}

function fmtWait(secs) {
  if (secs < 90) return `${Math.round(secs)} s`;
  const m = Math.floor(secs / 60);
  const s = Math.round(secs % 60);
  return m < 60 ? `${m} min ${s} s` : `${Math.floor(m / 60)} h ${m % 60} min`;
}

async function finishChain() {
  // an auto-fix run ends its own wait (its stages come with gaps)
  if (!chain.active || state.job || (state.auto && state.auto.running)) return;
  chain.active = false;
  const secs = (chain.end - chain.start) / 1000;
  // stopped by hand: whoever stopped it is looking
  if (chain.cancelled && chain.ok) return;
  if (secs < alertSettings.after) return;
  const last = chain.names[chain.names.length - 1] || 'job';
  const autoSummary = state.auto && state.auto.summary;
  const title = chain.ok ? `Unflash: ${chain.names.length > 1 ? chain.names.join(', then ').toLowerCase() : last.toLowerCase()} done` : `Unflash: ${last.toLowerCase()} failed`;
  // (an auto-fix run whose export still fails failed with no job failing: its summary says so)
  const body = chain.ok ? `${autoSummary || chain.toast || `Finished after ${fmtWait(secs)}.`} (${fmtWait(secs)})` : chain.error || autoSummary || '';
  const alert = { title, body, secs, ok: chain.ok, beeped: false, notified: false };
  state.lastAlert = alert;
  titleMark(chain.ok);
  if (alertSettings.notify && (document.hidden || !document.hasFocus())) alert.notified = systemNotify(title, body);
  if (alertSettings.beep) alert.beeped = await beep(chain.ok);
}

function renderAlertNote() {
  const parts = ['While a job runs, the tab title shows how far it has got; one that ends while you are in another tab leaves a ✓ (or ✗) there.'];
  const st = notifyState();
  if (st === 'denied') parts.push('Notifications from this page are blocked in the browser; allow them in its site settings to have them.');
  else if (st === 'unsupported') parts.push('This browser has no system notifications.');
  $('alertNote').textContent = parts.join(' ');
}

function wireAlerts() {
  $('alertBeep').checked = alertSettings.beep;
  $('alertAfter').value = alertSettings.after;
  $('alertNotify').checked = alertSettings.notify && notifyState() === 'granted';
  const save = () => {
    saveAlertSettings(alertSettings);
    renderAlertNote();
  };
  $('btnAlerts').addEventListener('click', (e) => {
    e.stopPropagation();
    renderAlertNote();
    toggleMenu('alertsMenu');
  });
  $('alertBeep').addEventListener('change', () => {
    alertSettings.beep = $('alertBeep').checked;
    save();
  });
  $('alertAfter').addEventListener('change', () => {
    const v = parseFloat($('alertAfter').value);
    alertSettings.after = Number.isFinite(v) && v >= 0 ? v : 60;
    $('alertAfter').value = alertSettings.after;
    save();
  });
  $('alertNotify').addEventListener('change', async () => {
    if ($('alertNotify').checked && (await askNotifyPermission()) !== 'granted') $('alertNotify').checked = false;
    alertSettings.notify = $('alertNotify').checked;
    save();
  });
  $('btnAlertTest').addEventListener('click', () => beep(true));
  renderAlertNote();
}

function profileConfig(name) {
  return wasm.profile_config(name);
}

/**
 * Whether the profile behind `r` (a scan, stored or just made, a check, the
 * live monitor's result) counts violation `v`: profiles differ on extended
 * flashes and stripe patterns (a scan stored before scans said which goes
 * by its profile's name).
 */
function counts(r, v) {
  if (v.kind === 'extended') return r.flag_extended !== undefined ? !!r.flag_extended : r.profile !== 'wcag';
  if (v.kind === 'pattern') return !!r.flag_patterns;
  return true;
}

/** Close every menu (the project's, the alerts', the frame rate's). */
function closeMenus() {
  for (const m of document.querySelectorAll('.menu')) m.classList.add('hidden');
}

/** Open menu `id`, or close it when it is open: the others close either way. */
function toggleMenu(id) {
  const open = !$(id).classList.contains('hidden');
  closeMenus();
  $(id).classList.toggle('hidden', open);
}

const KIND_LABEL = { flash: 'flash', red: 'red flash', extended: 'extended flash', pattern: 'stripes' };

// The page's colours and type, for the canvases: style.css keeps them
// (:root), so the timeline and the charts draw in the same palette as the
// rest of the page.
const ink = (() => {
  const cs = getComputedStyle(document.documentElement);
  const v = (k) => cs.getPropertyValue(`--${k}`).trim();
  const out = {};
  for (const k of ['bg', 'line2', 'fg', 'fg2', 'flash', 'red', 'pat', 'ext', 'held', 'blend', 'sel', 'ok', 'bad']) out[k] = v(k);
  const mono = v('mono');
  out.font = (px) => `${px}px ${mono}`;
  return out;
})();
/** Colour `hex` (#rrggbb) at opacity `a`. */
function tint(hex, a) {
  const n = parseInt(hex.slice(1), 16);
  return `rgba(${n >> 16}, ${(n >> 8) & 255}, ${n & 255}, ${a})`;
}
/** A violation's or a section's colour, by its kind. */
const kindInk = (kind) => ({ flash: ink.flash, red: ink.red, extended: ink.ext, pattern: ink.pat })[kind] || ink.fg2;

/** `?cpu=1` forces the WebAssembly detector (for comparison and tests). */
function preferGpuSetting() {
  return !QUERY.has('cpu');
}

/**
 * `?extsrc=canvas` (or `videoframe,video`, or `none`) pretends the browser's
 * WebGPU accepts only those kinds of picture as copy sources, to exercise
 * the routes other browsers need (Firefox: canvas only).
 */
function externalSourcesSetting() {
  const v = QUERY.get('extsrc');
  if (v === null) return null;
  return v === 'none' ? [] : v.split(',');
}

/** `?route=yuv` (videoframe, yuv, rgba, canvas or pixels) forces one way of feeding pictures to the detector. */
function routeSetting() {
  return QUERY.get('route');
}

// ---- what's new ----------------------------------------------------------------
//
// The changes since this browser was last here, from CHANGELOG.md: a card on
// the start page and a dot on the header's button until they are seen, the
// whole list behind the button.

/** Changes the start page's card lists; the rest are behind "everything that changed". */
const NEWS_SHOWN = 6;

/**
 * Load the changelog and work out what this browser has not seen. One that
 * has never been shown it is taken to have been here when it last saved a
 * project, or, with only earlier settings to show for a visit, to have
 * missed it all; a first visit is shown nothing and starts from here.
 */
async function initChanges() {
  let log;
  try {
    log = await loadChangelog();
  } catch (e) {
    console.warn('no changelog', e);
    return;
  }
  const newest = newestChange(log);
  if (!newest) return;
  let seen = changesSeen();
  let firstVisit = false;
  if (seen === null) {
    let last = 0;
    try {
      last = await lastSavedAt();
    } catch (e) {
      /* no storage */
    }
    if (last) seen = last;
    else if (EARLIER_SETTINGS) seen = 0;
    else {
      seen = newest;
      markChangesSeen(newest);
      firstVisit = true;
    }
  }
  state.changes = { log, newest, seen };
  renderChanges();
  // the tours: getting started on a first visit, the new things' own after an update
  const fresh = changesSince(log, seen).flatMap((d) => d.items.map((it) => it.tour).filter(Boolean));
  tourGuide.plan({ firstVisit, tours: fresh });
}

// ---- the guided tour ------------------------------------------------------------
//
// tours.js says what each tour shows and when; here is where the app is (a
// video open, a section ready) and whether it is busy.

/** `?tour=0` never starts a tour by itself, `?tour=1` does even in an automated browser (tests). */
function tourAutoSetting() {
  const v = onOff('tour');
  return v !== null ? v : !navigator.webdriver;
}

/** The last click, key or scroll (ms), and whether a button is down: a tour waits for a quiet moment. */
const input = { at: 0, down: false };

const tourGuide = new TourGuide({
  auto: tourAutoSetting(),
  // (`byHand`: as it will be once the guide is put away for the tour; a
  // video's part waits for its scan unless it is asked for)
  where: (byHand = false) => {
    const guide = !byHand && document.body.classList.contains('guide-open');
    const sec = currentSection();
    return {
      any: true,
      start: !state.movie,
      video: !!state.movie && !guide && (byHand || !!(state.project && state.project.scan)),
      section: !!(sec && sec.prepared && !guide && !$('wsBody').classList.contains('hidden') && /passes|fails/.test($('wsVerdict').textContent)),
    };
  },
  quiet: () =>
    !state.job &&
    // (a job that ends is often followed by another: opening, then scanning)
    performance.now() - (state.jobEndedAt || 0) > CHAIN_GRACE_MS &&
    !(state.auto && state.auto.running) &&
    !input.down &&
    performance.now() - input.at > 1500 &&
    document.visibilityState === 'visible' &&
    ![...document.querySelectorAll('.modal, .viewer, .menu')].some((m) => !m.classList.contains('hidden')),
  ready: (context) => {
    if (context !== 'start' && context !== 'any' && state.movie) setGuide(false);
  },
});

function wireTours() {
  window.addEventListener('pointerdown', () => Object.assign(input, { at: performance.now(), down: true }), true);
  window.addEventListener('pointerup', () => Object.assign(input, { at: performance.now(), down: false }), true);
  window.addEventListener('keydown', () => (input.at = performance.now()), true);
  window.addEventListener('wheel', () => (input.at = performance.now()), { capture: true, passive: true });
  $('btnTour').addEventListener('click', () => tourGuide.gettingStarted());
  // the guide's list of the screen's parts, each with its "show me"
  $('guideParts').innerHTML = PARTS.map((g) => `<h3>${g.group}</h3><ul>${g.parts.map((p) => `<li><b>${p.name}</b>: ${p.short}<button class="small show-part" data-part="${p.id}" title="Light this part up on the page">show me</button></li>`).join('')}</ul>`).join('');
  $('guideParts').addEventListener('click', (e) => {
    const b = e.target.closest('.show-part');
    if (!b || tourGuide.showPart(b.dataset.part)) return;
    const g = PARTS.find((x) => x.parts.some((p) => p.id === b.dataset.part));
    const part = g.parts.find((p) => p.id === b.dataset.part);
    toast(g.context === 'section' && !(currentSection() && currentSection().prepared) ? `${part.name} is part of a section: open one (in the list on the left) and it is there.` : `${part.name} is not on screen just now.`, 5000);
  });
  // "show me" beside a change in What's new
  for (const box of [$('newsList'), $('changesList')]) {
    box.addEventListener('click', (e) => {
      const b = e.target.closest('.show-me');
      if (!b) return;
      closeChanges();
      if (tourGuide.request(b.dataset.tour)) return;
      const t = TOURS[b.dataset.tour];
      toast(`${t && t.context === 'section' ? 'Open a section of a video' : 'Open a video'} and the tour of this starts.`);
    });
  }
}

function renderChanges() {
  const c = state.changes;
  if (!c) return;
  const fresh = changesSince(c.log, c.seen);
  const count = fresh.reduce((a, d) => a + d.items.length, 0);
  $('changesDot').classList.toggle('hidden', !count);
  $('btnChanges').title = count ? `What has changed in Unflash: ${count} change${count === 1 ? '' : 's'} since you were last here` : 'What has changed in Unflash, newest first';
  $('newsCard').classList.toggle('hidden', !count);
  if (!count) return;
  const days = [];
  let left = NEWS_SHOWN;
  for (const d of fresh) {
    if (left <= 0) break;
    days.push({ ...d, items: d.items.slice(0, left) });
    left -= d.items.length;
  }
  const more = count - Math.min(count, NEWS_SHOWN);
  // (the headlines: each opens to say more, and What's new has them all in full)
  $('newsList').innerHTML = days.map((d) => renderDay(d, () => false, true, c.log.shots)).join('') + (more ? `<p class="news-more">…and ${more} more.</p>` : '');
  wireShots($('newsList'));
}

/** The changes so far count as seen: the card and the dot go until there are new ones. */
function changesAcknowledged() {
  const c = state.changes;
  if (!c) return;
  c.seen = c.newest;
  markChangesSeen(c.newest);
  renderChanges();
}

function openChanges() {
  const c = state.changes;
  if (!c) return toast('The list of changes could not be loaded');
  const seen = c.seen;
  $('changesList').innerHTML = c.log.days.map((d) => renderDay(d, (it) => it.at > seen, false, c.log.shots)).join('');
  $('changesModal').classList.remove('hidden');
  $('changesList').scrollTop = 0;
  wireShots($('changesList'), $('changesList'));
  changesAcknowledged();
}

function closeChanges() {
  $('changesModal').classList.add('hidden');
  // (the films stop with it)
  for (const v of $('changesList').querySelectorAll('video')) v.pause();
}

// ---- debug info --------------------------------------------------------------------

function makeDebugReport() {
  return debugReport({ version: wasm.version(), build: BUILD, state, profile, gpu: gpuAdapter, segments: scanSegments(), hybrid: state.env ? hybridPlan(state.movie, state.env.feeder) : null, sound: { status: soundStatus(), failed: sectionSound.failed, output: sectionSound.ctx ? sectionSound.ctx.state : null, builtIn: sectionSound.builtIn } });
}

// ---- a newer build published -------------------------------------------------------

/** How often, at most, the page asks whether a newer build is out. */
const UPDATE_EVERY_MS = 10 * 60 * 1000;
const update = { checkedAt: 0, said: null };

/**
 * Whether the site has a newer build than the one this page runs (its
 * versions.json names the current build), asked at most every 10 minutes
 * (`force`: now). The page goes on with its own build's files until it is
 * reloaded, so it says a newer one is out: a note once for each (not over
 * a message about something that went wrong: then at the next asking),
 * and a button in the header that stays, to reload with.
 */
async function checkForUpdate(force = false) {
  if (!BUILD || (!force && Date.now() - update.checkedAt < UPDATE_EVERY_MS)) return;
  update.checkedAt = Date.now();
  let current = null;
  try {
    const r = await fetch('versions.json', { cache: 'no-cache' });
    if (r.ok) current = (await r.json()).current || null;
  } catch (e) {
    return; // (offline, say: nothing to go on)
  }
  if (!current || current === BUILD) return;
  $('btnUpdate').classList.remove('hidden');
  if (update.said === current) return;
  const b = $('banner');
  if (!b.classList.contains('hidden') && !b.classList.contains('info')) return;
  update.said = current;
  banner("A newer version of Unflash is out: reload the page to use it, when you're ready (What's new says what changed). Until then this page carries on as it is; your sections and marks come back when you open the video again.", 'info');
}

/** The report in a dialog, copied to the clipboard at once where the browser lets it. */
function openDebug() {
  const text = makeDebugReport();
  $('debugText').value = text;
  const save = $('btnDebugSave');
  if (save.href.startsWith('blob:')) URL.revokeObjectURL(save.href);
  save.href = URL.createObjectURL(new Blob([text + '\n'], { type: 'text/plain' }));
  $('debugNote').textContent = 'What this browser, its GPU and the open video are, and how long each job took. Paste it into a message to whoever is helping you; it names no files.';
  $('debugModal').classList.remove('hidden');
  copyDebug(text);
}

function copyDebug(text) {
  const note = $('debugNote');
  const manual = () => {
    const t = $('debugText');
    t.focus();
    t.select();
    note.textContent = `Select the text below and copy it (${/Mac/.test(navigator.platform) ? '⌘' : 'Ctrl'}+C), or save it as a file, and paste it into a message to whoever is helping you. It names no files.`;
  };
  if (!navigator.clipboard || !navigator.clipboard.writeText) return manual();
  navigator.clipboard.writeText(text).then(
    () => {
      note.textContent = 'Copied: paste it into a message to whoever is helping you. It names no files.';
    },
    () => manual()
  );
}

function closeDebug() {
  $('debugModal').classList.add('hidden');
}

// ---- boot ---------------------------------------------------------------------

async function boot() {
  await init();
  const hasGpu = !!navigator.gpu;
  const hasCodecs = typeof VideoDecoder !== 'undefined';
  $('support').textContent = `Unflash ${wasm.version()}${BUILD ? `, build ${BUILD}` : ''} · WebGPU: ${hasGpu ? 'available' : 'not available (CPU detector will be used)'} · WebCodecs: ${hasCodecs ? 'available' : 'not available (only the live monitor will work)'}`;
  setStatus([`ready · WebGPU ${hasGpu ? 'yes' : 'no'} · WebCodecs ${hasCodecs ? 'yes' : 'no'}`]);
  $('fileInput').addEventListener('change', (e) => {
    const f = e.target.files && e.target.files[0];
    // (emptied, so that the same file picked again opens again)
    e.target.value = '';
    if (f) openFile(f);
  });
  // a file dropped anywhere on the page opens like one picked with the button
  document.addEventListener('dragover', (e) => {
    if (e.dataTransfer && Array.from(e.dataTransfer.types).includes('Files')) {
      e.preventDefault();
      e.dataTransfer.dropEffect = 'copy';
    }
  });
  document.addEventListener('drop', (e) => {
    const f = e.dataTransfer && e.dataTransfer.files && e.dataTransfer.files[0];
    if (!f) return;
    e.preventDefault();
    openFile(f);
  });
  // the guide: the start page until a file is open, then a drawer beside the
  // work that the same button, its close button or Esc put away again
  $('btnHome').addEventListener('click', () => {
    if (!state.movie) return $('welcome').scrollTo(0, 0);
    setGuide(!document.body.classList.contains('guide-open'));
  });
  $('btnCloseGuide').addEventListener('click', () => setGuide(false));
  $('btnChanges').addEventListener('click', openChanges);
  $('btnNewsAll').addEventListener('click', openChanges);
  $('btnNewsSeen').addEventListener('click', changesAcknowledged);
  $('btnCloseChanges').addEventListener('click', closeChanges);
  wireTours();
  $('changesModal').addEventListener('click', (e) => {
    if (e.target === $('changesModal')) closeChanges();
  });
  $('btnDebug').addEventListener('click', openDebug);
  $('btnDebugCopy').addEventListener('click', () => copyDebug($('debugText').value));
  $('btnCloseDebug').addEventListener('click', closeDebug);
  $('debugModal').addEventListener('click', (e) => {
    if (e.target === $('debugModal')) closeDebug();
  });
  initChanges();
  // a newer build published while the page is open: asked now, and when the tab comes back into view
  $('btnUpdate').addEventListener('click', () => {
    if (state.job && !confirm(`${state.job.name} is running, and reloading the page stops it. Reload now?`)) return;
    location.reload();
  });
  checkForUpdate(true);
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') checkForUpdate();
  });
  $('btnCloseBanner').addEventListener('click', () => $('banner').classList.add('hidden'));
  $('btnCancelJob').addEventListener('click', () => {
    if (state.job) state.job.cancelled = true;
  });
  $('profileSel').addEventListener('change', () => setProfile($('profileSel').value));
  $('btnScan').addEventListener('click', scan);
  $('autoToggle').checked = initialAutoSetting();
  $('autoToggle').addEventListener('change', () => {
    store('unflash:auto', $('autoToggle').checked ? '1' : '0');
    // ticked with a file open: the run starts, once whatever job is under way (the scan, say) is done
    if ($('autoToggle').checked && state.movie && !(state.auto && state.auto.running)) {
      afterJobs().then(() => {
        if ($('autoToggle').checked && state.movie && !(state.auto && state.auto.running)) autopilot();
      });
    }
  });
  $('btnAutoStop').addEventListener('click', () => stopAuto());
  $('btnAutoRerun').addEventListener('click', () => autopilot({ rescan: true }));
  $('liveToggle').addEventListener('change', () => setLive($('liveToggle').checked));
  $('dimToggle').addEventListener('change', () => {
    applyDim();
    renderPlayerWarning();
  });
  applyDim();
  wirePlayer();
  $('btnExport').addEventListener('click', openExport);
  wireProjectMenu();
  // a menu closes with a click anywhere else on the page
  for (const m of document.querySelectorAll('.menu')) m.addEventListener('click', (e) => e.stopPropagation());
  document.addEventListener('click', closeMenus);
  $('btnCloseExport').addEventListener('click', () => $('exportModal').classList.add('hidden'));
  $('btnDoExport').addEventListener('click', doExport);
  $('exportName').addEventListener('input', () => onExportNameInput(false));
  $('exportName').addEventListener('change', () => onExportNameInput(true));
  $('btnVerifyExport').addEventListener('click', verifyExport);
  // a verify's "section #N": that section, with the frames it found flashing selected
  $('exportResult').addEventListener('click', (e) => {
    const b = e.target.closest('button[data-open-at]');
    if (!b) return;
    const [id, lo, hi] = b.dataset.openAt.split(':').map(Number);
    $('exportModal').classList.add('hidden');
    state.selectOnOpen = { id, lo, hi, kind: b.dataset.kind };
    openSection(id);
  });
  $('verifyFileInput').addEventListener('change', (e) => {
    const f = e.target.files && e.target.files[0];
    e.target.value = '';
    verifySavedFile(f);
  });
  // the setting and the scale ("7 of 10"), and what the slider holds when the
  // page starts (a browser can put back the last value on a reload)
  const showQuality = () => ($('exportQualityText').textContent = `${$('exportQuality').value} of ${$('exportQuality').max}`);
  $('exportQuality').addEventListener('input', showQuality);
  showQuality();
  $('exportQuality').addEventListener('change', () => state.movie && renderExportChoice());
  $('exportCodec').addEventListener('change', () => renderExportChoice());
  $('btnAddSection').addEventListener('click', () => {
    const s = wasm.parse_time($('addStart').value);
    const e = wasm.parse_time($('addEnd').value);
    if (s == null || e == null || e <= s) return toast('Enter a start and an end, like 1:23.5 and 1:30');
    addSection(s, e);
  });
  for (const b of document.querySelectorAll('[data-clip]')) b.addEventListener('click', () => openClip(b.dataset.clip));
  $('btnPrepareAll').addEventListener('click', prepareAll);
  $('btnCheckAll').addEventListener('click', checkAll);
  $('btnDeleteAll').addEventListener('click', async () => {
    if (!state.project || busy('delete the sections') || !confirm('Delete every section, including its marks?')) return;
    // (their frames are freed once no check reads them)
    await withFeeder(() => {
      for (const s of [...state.project.sections]) state.project.deleteSection(s.id);
    });
    state.current = null;
    state.project.save();
    renderAll();
  });
  wireWorkspace();
  wireTimeline();
  wireAlerts();
  window.addEventListener('resize', () => {
    drawTimeline();
    drawChart();
  });
  const player = $('player');
  player.addEventListener('timeupdate', () => {
    drawTimeline();
    if (chartMode() === 'video') drawChart();
  });
  player.addEventListener('play', () => {
    if (state.live.on && state.player.mode === 'video') startLiveLoop();
  });
  player.addEventListener('pause', flushVerdict);
  // (a seek's timeupdate, which comes first, has drawn the timeline and the chart)
  player.addEventListener('seeked', () => {
    if (state.live.on && state.live.fromScan) monitorFromScan(player.currentTime);
  });
}

// ---- opening a file -------------------------------------------------------------

async function openFile(file) {
  // an unattended run gives way to the new file; a job started by hand is waited for
  await stopAuto();
  if (busy('open the video')) return;
  noteFileName(file.name);
  $('banner').classList.add('hidden');
  if (sectionPlayer) await sectionPlayer.stop();
  closeViewer();
  const opened = await runJob('Opening video', async (progress, cancelled) => {
    const { movie, key, decode, project, kept, made } = await readVideo(file, progress, cancelled);
    // From here on nothing waits: the page goes over to the new video in one
    // go. The last file, its unattended run and its export go (an export of
    // this same video, kept on disk, stays: it is offered again)
    if (state.movie) state.movie.close();
    forgetExport(exportOwner(key));
    // (a name typed for the last video's export is not this one's)
    state.exportFileName = null;
    if (state.project) for (const s of state.project.sections) dropCaches(s);
    state.lastScan = null;
    state.lastVerify = null;
    state.traceNorm = null;
    state.movie = movie;
    state.decode = decode;
    state.project = project;
    state.config = made.config;
    $('profileSel').value = project.profile;
    if (kept) offerKeptExport(kept);
    // (the player first: the live monitor, if it is on, starts afresh on it)
    loadPlayer(file, movie);
    useDetectors(made);
    movie.decodeInWorkers = decodeWorkersSetting(made.feeder);
    movie.shrinkInWorkers = shrinkSetting();
    $('videoInfo').textContent = `${file.name} · ${movie.width}×${movie.height} · ${movie.fps.toFixed(2)} fps · ${fmt(movie.duration)} · ${movie.video.codec}${movie.audio ? ' + ' + movie.audio.codec : ''}${movie.audioTracks > 1 ? ` (sound ${movie.audioSkipped.length + 1} of ${movie.audioTracks})` : ''}`;
    sectionSound.failed = null;
    state.player.soundFailSaid = null;
    renderSoundButton();
    $('btnScan').disabled = !state.decode.supported;
    $('btnScan').title = state.decode.supported ? 'Decode every frame with WebCodecs and run the detector over it' : `Scanning needs WebCodecs: ${state.decode.reason}`;
    $('liveToggle').disabled = false;
    $('btnExport').disabled = false;
    $('btnProject').disabled = false;
    document.body.classList.add('has-movie');
    setGuide(false);
    $('stage').classList.remove('hidden');
    $('sections').classList.remove('hidden');
    $('timelineWrap').classList.remove('hidden');
    if (!state.decode.supported) banner(`This browser cannot decode ${movie.video.codec} with WebCodecs (${state.decode.reason}). The live monitor still works while the player plays; scanning and section editing need a decodable file (H.264 in most browsers).`, 'info');
    else if (state.decode.software) {
      const info = movie.softwareInfo || {};
      const about = movie.builtIn.id === 'h264' ? ` (profile ${info.profile_idc}, level ${info.level_idc})` : '';
      const why = movie.forceBuiltIn ? 'You asked for the built-in decoder' : `This browser cannot decode ${movie.video.codec} with WebCodecs`;
      banner(`${why}, so Unflash uses its built-in ${movie.builtIn.name} decoder${about} for scanning, sections and export, decoding in ${defaultWorkerCount()} parallel workers.${movie.forceBuiltIn ? '' : ' The player cannot play this file here, so the live monitor is off.'}`, 'info');
    }
    // the live monitor watches the player, which plays what the browser can decode
    $('liveToggle').disabled = !!state.decode.software && !movie.forceBuiltIn;
    state.current = null;
    state.history.clear();
    setPlayerSource('video');
    renderAll();
    updateStatus();
    return true;
  });
  if (opened && state.project.restoredFrom) {
    // the same name and size under another modified time: a copy, or the file downloaded again
    toast(`Restored the project saved for ${state.project.restoredFrom.split(':')[0]} (the same name and size) in this browser.`, 8000);
    state.project.save();
  }
  if (!opened || !state.decode.supported) return;
  // the scan starts on its own; the fixes, the export and its check too with auto-fix on
  if (autoEnabled()) autopilot();
  else if (autoScanEnabled()) autoScan();
}

/**
 * What opening `file` needs, read and made before anything on the page
 * changes: the Movie, how it decodes here, its project, its export kept in
 * private storage (or null) and its detectors. A cancel (`cancelled()`)
 * makes it throw 'cancelled' at its next step, the read through a file
 * that keeps no index included, having let go of what it had made.
 */
async function readVideo(file, progress, cancelled) {
  const stop = () => {
    if (cancelled()) throw new Error('cancelled');
  };
  progress(0.05, 'reading the index');
  const movie = await Movie.open(file, wasm, {
    // a Matroska file or a transport stream keeps no index: it is read through once
    onProgress: (p, container) => {
      stop();
      progress(0.05 + 0.3 * p, container === 'matroska' || container === 'mpegts' ? `reading through the file for its frames (${container === 'mpegts' ? 'transport streams' : 'MKV / WebM'} keep no index): ${Math.round(p * 100)}%` : 'reading the index');
    },
  });
  let made = null;
  try {
    // `?builtin=1`: the app's built-in decoder for the codec even where
    // WebCodecs has one (tests; a browser whose decoder misbehaves)
    movie.forceBuiltIn = onOff('builtin') === true;
    progress(0.36, 'asking the browser about its decoder');
    const decode = await movie.decoderSupport();
    stop();
    progress(0.38, 'loading the project kept for this video');
    const key = projectKey(file);
    const project = await Project.load(key, movie.bounds, movie.keyframes);
    stop();
    progress(0.39, 'looking for an export of it');
    const kept = await findPrivateExport(exportOwner(key));
    stop();
    progress(0.4, 'starting the detector');
    made = await makeDetectors(movie, profileConfig(project.profile), progress);
    stop();
    return { movie, key, decode, project, kept, made };
  } catch (e) {
    if (made) freeDetectors(made);
    movie.close();
    throw e;
  }
}

/**
 * Resolves once no job is running: at a job's end, after whoever started it
 * has gone on (what it keeps, and a job it starts next, as the open starts
 * the scan, are seen: that job is waited for too).
 */
async function afterJobs() {
  while (state.job) {
    await state.job.ended;
    // (the job's caller was handed its result as the job ended: it goes on first)
    await null;
  }
}

/** Scan a freshly opened file, unless a scan of it under this profile is stored from a previous visit. */
async function autoScan() {
  const project = state.project;
  const prior = project && project.scan;
  if (prior && prior.sig === wasm.config_signature(state.config) && prior.profile === project.profile && (prior.safe || project.sections.length)) return;
  await scan();
}

/** Open or close the guide beside the work (with no file open it is the whole page). */
function setGuide(open) {
  document.body.classList.toggle('guide-open', !!open);
  // (asked for, the guide is the whole of it; the start page folds it away)
  if (open) $('guideMore').open = true;
}

/** Open one of the test clips published next to the app. */
async function openClip(name) {
  // (an unattended run gives way to a new video: openFile stops it)
  if (!(state.auto && state.auto.running) && busy('open the clip')) return;
  toast(`Fetching ${name}…`, 10000);
  try {
    const r = await fetch(`clips/${name}`);
    if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
    const blob = await r.blob();
    const lm = Date.parse(r.headers.get('last-modified') || '') || 0;
    await openFile(new File([blob], name, { type: 'video/mp4', lastModified: lm }));
  } catch (e) {
    banner(`Could not fetch the test clip ${name}: ${e && e.message ? e.message : e}. The clips are generated by the Pages build; for a local copy run python3 tests/media/gen_e2e.py web/clips.`);
  }
}

/** How the page's detectors are made: the GPU unless `?cpu`, and the routes the query forces. */
function detectorSettings() {
  return { preferGpu: preferGpuSetting(), externalSources: externalSourcesSetting(), route: routeSetting() };
}

/**
 * The two detectors a video needs, made for `movie` under `config` and not
 * yet put to use (useDetectors does that): the main one, which scans,
 * prepares and checks feed, and the live monitor's, a picture at a time.
 */
async function makeDetectors(movie, config, progress = null) {
  const feeder = await createDetector(wasm, config, movie.width, movie.height, detectorSettings());
  try {
    if (progress) progress(0.8, `${feeder.backend} detector at ${feeder.aw}×${feeder.ah}`);
    const live = await createDetector(wasm, config, movie.width, movie.height, { ...detectorSettings(), batch: 1 });
    // (the player's pictures, converted as the decoder's are)
    live.decoderColorSpace = () => decoderColorSpace(movie);
    return { config, feeder, live };
  } catch (e) {
    feeder.det.free();
    throw e;
  }
}

const decoderColorSpaces = new WeakMap();

/**
 * The colour space the decoder gives `movie`'s pictures, as a scan sees
 * them ({} when it names none), or null when its first picture cannot be
 * decoded: what the live monitor converts the player's pictures by. Read
 * off the first picture, once a video, when first asked.
 */
function decoderColorSpace(movie) {
  if (!decoderColorSpaces.has(movie)) decoderColorSpaces.set(movie, firstColorSpace(movie).catch(() => null));
  return decoderColorSpaces.get(movie);
}

async function firstColorSpace(movie) {
  let found = null;
  // (a reader of its own: a scan may be reading the file meanwhile)
  const take = async (pic) => {
    if (!found) found = (pic.colorSpace && pic.colorSpace.toJSON ? pic.colorSpace.toJSON() : pic.colorSpace) || {};
    pic.close();
  };
  await decodeRange(movie, movie.tsMin, movie.tsMin + 0.001, take, { reader: movie.reader.fork() });
  return found;
}

/** Free detectors `made` (makeDetectors) that were never put to use. */
function freeDetectors(made) {
  made.feeder.det.free();
  made.live.det.free();
}

/**
 * Put detectors `made` (makeDetectors) to use, then free the ones they
 * replace, the spares made for spans too: in one go, so that nothing that
 * runs between frames (the live monitor, the timeline) finds its detector
 * freed. The monitor, if it is on, starts afresh on its new detector.
 */
function useDetectors(made) {
  const old = state.env ? [state.env.feeder, ...(state.env.spares || [])] : [];
  if (state.liveFeeder) old.push(state.liveFeeder);
  state.env = { wasm, config: made.config, feeder: made.feeder };
  state.liveFeeder = made.live;
  for (const f of old) f.det.free();
  // (the trace's levels are shares of the detector's thresholds, which a profile sets)
  state.traceNorm = null;
  if (made.feeder.note) banner(made.feeder.note, 'info');
  if (state.live.on) setLive(true);
}

/** New detectors for the open video under the current profile, in place of the ones it has (under the feeder lock). */
async function createFeeders() {
  useDetectors(await makeDetectors(state.movie, state.config));
}

async function setProfile(name) {
  if (!state.project) return;
  await stopAuto();
  // (a job under way runs on the detector it started with: what it found would be kept as the new profile's)
  if (busy('change the profile')) {
    $('profileSel').value = state.project.profile;
    return;
  }
  state.project.profile = name;
  state.config = profileConfig(name);
  await withFeeder(() => createFeeders());
  for (const s of state.project.sections) if (s.check) s.check.stale = true;
  renderAll();
  updateStatus();
  await state.project.save();
  toast('Profile changed. Sections need re-checking (they are re-checked when opened).');
  if (!state.decode.supported) return;
  if (autoEnabled()) autopilot({ rescan: true });
  else if (autoScanEnabled()) autoScan();
}

function updateStatus() {
  const parts = [];
  if (state.env) {
    const f = state.env.feeder;
    parts.push(`detector: <b>${f.backend === 'webgpu' ? 'WebGPU' : 'CPU (WASM)'}</b> at ${f.aw}×${f.ah} (window ${f.det.window_width()}×${f.det.window_height()}, area ≥ ${f.det.area_thresh()} px)`);
    if (state.decode.software) parts.push(`decoder: <b>built-in ${state.movie.builtIn.name}</b> (${state.movie.forceBuiltIn ? 'asked for' : 'no WebCodecs decoder for this codec'})`);
    else if (state.movie && state.movie.decodeInWorkers) parts.push('decoder: WebCodecs <b>in workers</b> (this WebGPU takes no decoded frame, so pictures are copied out of the decoder off the page)');
    if (state.lastScan) {
      const s = state.lastScan;
      const fps = (s.frames / (s.elapsedMs / 1000)).toFixed(0);
      const gbs = ((f.det.bytes_per_frame() * (s.frames / (s.elapsedMs / 1000))) / 1e9).toFixed(2);
      parts.push(`last scan: ${s.frames} frames in ${(s.elapsedMs / 1000).toFixed(1)} s = ${fps} fps (${(fps / state.movie.fps).toFixed(1)}× realtime${s.segments > 1 ? `, ${s.segments} segments` : ''}), ≈${gbs} GB/s of detector state traffic`);
    }
  }
  if (state.project) parts.push(`caches: ${(state.project.cacheBytes() / 1048576).toFixed(0)} MB`);
  setStatus(parts);
}

// ---- scanning -------------------------------------------------------------------

/** `?segments=N`: the count scanSegments gives, whatever the machine (0: none forced). */
const SEGMENTS = parseInt(QUERY.get('segments') || '', 10) || 0;

/**
 * Spans a scan is cut into and scanned at once: one per two logical cores,
 * at most four, on the GPU detector with the browser's own decoder (the
 * built-in decoder already spreads over workers, and the CPU detector has
 * one thread); decoding in workers, all but two cores, at most six.
 * `?segments=N` forces a count.
 */
function scanSegments() {
  if (SEGMENTS > 0) return SEGMENTS;
  if (!state.env || state.env.feeder.backend !== 'webgpu' || state.decode.software) return 1;
  const cores = navigator.hardwareConcurrency || 4;
  // decoding in workers, each segment's pictures are copied and made small
  // on a core of its own, and the page does little per frame: more of them
  if (state.movie && state.movie.decodeInWorkers) return Math.min(6, Math.max(1, cores - 2));
  return Math.min(4, Math.max(1, Math.floor(cores / 2)));
}

/**
 * Whether a scan decodes with the browser's decoder and the built-in one at
 * the same time (a hybrid scan, analysis.js scanChunked), and with how many
 * of each: for a codec the browser decodes itself and the app has a
 * decoder for (H.264, HEVC, VP9, VP8, AV1), on the GPU detector, on a
 * machine with six cores or more, for a file of two minutes or more (a
 * shorter one scans in seconds anyway). The browser's decoder gets two to
 * four lanes and the built-in decoder a worker for each core left over
 * (two stay for the page and the browser); where the browser's pictures
 * are copied out of it (Firefox), H.264 gets one lane of the browser's
 * decoder and the built-in decoder the rest, less a few cores for the
 * other tabs. `?hybrid=0` turns it off,
 * `?hybrid=1` on for any file, `?hybrid=H,S` makes H lanes for the
 * browser's decoder and S workers for the built-in one (0: none).
 */
function hybridPlan(movie, feeder) {
  if (onOff('hybrid') === false) return null;
  let h = QUERY.get('hybrid');
  // tests: `?hybrid=sim:H,S` runs the browser's lanes on the built-in
  // decoder (the test browser has no H.264 in WebCodecs), `?hybridfail=1`
  // fails the built-in decoder's first run
  const sim = !!h && h.startsWith('sim');
  if (sim) h = h.slice(4) || '1';
  // (the simulation runs with the CPU detector too: SwiftShader's WebGPU is slower than any decoder)
  if (!movie || (movie.software && !sim) || !feeder || (feeder.backend !== 'webgpu' && !sim)) return null;
  if (!builtInFor(movie.video.codec)) return null;
  const cores = navigator.hardwareConcurrency || 4;
  const counts = /^(\d+),(\d+)$/.exec(h || '');
  let hw;
  let sw;
  if (counts) {
    hw = Math.max(1, Math.min(8, +counts[1]));
    sw = Math.min(16, +counts[2]);
  } else {
    if (!(h === '1' || h === 'on') && (cores < 6 || movie.duration < 120)) return null;
    if (movie.decodeInWorkers && builtInFor(movie.video.codec).id === 'h264') {
      // Where every picture of the browser's decoder is copied out whole
      // (Firefox), a lane of it costs a core and a readback through the GPU
      // process that draws every tab, for some 17 fps of 1080p; a built-in
      // worker decodes some 40 on a core of its own. So one lane, and the
      // built-in decoder on the cores left once the page, the browser and
      // the rest of the computer have theirs (32 MB a worker at 1080p).
      hw = 1;
      sw = Math.max(1, Math.min(movie.width * movie.height <= 1920 * 1088 ? 12 : 8, cores - hw - 3 - Math.floor(cores / 8)));
    } else {
      // decoding in workers, each lane also copies and shrinks its pictures on a core of its own
      hw = movie.decodeInWorkers ? Math.min(4, Math.max(2, Math.round(cores * 0.4))) : Math.min(3, Math.max(2, Math.floor(cores / 4)));
      sw = Math.max(1, Math.min(8, cores - hw - 2));
    }
  }
  // a lane of the browser's decoder that the scan sets aside, as slower than the rest, gives its core to the built-in decoder
  return { hw, sw, poolMax: sw > 0 ? Math.min(16, sw + hw) : 0, sim, failBuiltIn: QUERY.get('hybridfail') === '1' };
}

/**
 * How a scan runs: a file long enough for two chunks or more is scanned in
 * chunks (analysis.js scanChunked): decoded by as many lanes of the
 * browser's decoder as a segmented scan would use, plus the built-in
 * decoder's lane when hybridPlan makes one (its workers start for the scan
 * and stop after it, and when they cannot start the scan goes on without
 * them), taken by the detector in file order, with early looks at the
 * chunks triage finds likeliest to flash (`?order=file`: none); a shorter
 * file, or with `?chunked=0`, as before. `?chunk=S` sets the chunks' length
 * and `?hold=MB` how much of the pictures decoded ahead may be held;
 * `?steal=0` keeps a lane from taking over a slower lane's chunk,
 * `?rebalance=0` hands the chunks out in turn rather than each to the lane
 * that would finish it first, and (tests) `?slowlanes=MS` holds back every
 * picture of the browser's lanes. `opts` as scanMovie's.
 */
async function scanWithPlan(env, movie, opts) {
  const chunk = parseFloat(QUERY.get('chunk') || '');
  const chunkS = chunk > 0 ? chunk : CHUNK_S;
  const hold = parseFloat(QUERY.get('hold') || '');
  if (QUERY.get('chunked') === '0' || scanChunks(movie, chunkS).length < 2) return scanMovie(env, movie, opts);
  const plan = hybridPlan(movie, env.feeder);
  let pool = null;
  if (plan && plan.sw > 0) {
    try {
      pool = await SoftwarePool.create(movie, plan.sw);
    } catch (e) {
      const why = e && e.message ? e.message : String(e);
      console.warn('hybrid scan: the built-in decoder cannot start:', why);
      noteError(`hybrid scan without the built-in decoder: ${why}`);
    }
  }
  const order = QUERY.get('order') === 'file' ? 'file' : 'triage';
  try {
    return await scanMovie(env, movie, {
      ...opts,
      // (a plan that leans on the built-in decoder, without it, goes back to the browser's lanes)
      chunked: { hw: plan && (pool || !plan.sw || plan.sim) ? plan.hw : scanSegments(), pool, poolMax: plan ? plan.poolMax : 0, chunkS, order, budget: hold > 0 ? hold * 1024 * 1024 : null, sim: !!(plan && plan.sim), failBuiltIn: !!(plan && plan.failBuiltIn), slow: parseFloat(QUERY.get('slowlanes') || '') || 0, steal: QUERY.get('steal') !== '0', rebalance: QUERY.get('rebalance') !== '0' },
    });
  } finally {
    if (pool) pool.close();
  }
}

/**
 * Whether scans and prepares decode in workers: where WebGPU takes no
 * VideoFrame (Firefox), every picture is copied out of the decoder before
 * the GPU sees it, and that copy is better made off the page, several at
 * once. `?decodeworkers=1` / `=0` forces it.
 */
function decodeWorkersSetting(feeder) {
  const forced = onOff('decodeworkers');
  if (forced !== null) return forced;
  // a forced picture route is one taken on the page
  if (routeSetting()) return false;
  return !feeder.takesFrames;
}

/** `?shrink=0`: pictures decoded in workers reach the page at full size (the detector shrinks them) rather than at its size. */
function shrinkSetting() {
  return onOff('shrink') !== false;
}

/** `?smartcut=0`: an export re-encodes the whole video instead of copying the GOPs no section touches. */
function smartCutSetting() {
  return onOff('smartcut') !== false;
}

/** `?parallel=N`: how many spans an export re-encodes at once (0: by the machine). */
function parallelSetting() {
  return Math.max(0, parseInt(QUERY.get('parallel') || '0', 10) || 0);
}

/** The export dialog's line about what an export does with this plan. */
function describePlan(plan, movie, softened, blended = []) {
  const secs = (t) => `${t.toFixed(1)} s`;
  let text;
  if (plan.mode === 'smart') {
    text = plan.spans
      ? `${plan.spans} span${plan.spans === 1 ? '' : 's'} around the marked sections (${secs(plan.encodedSeconds)}, ${plan.encoded} frames) ${plan.spans === 1 ? 'is' : 'are'} decoded and re-encoded${plan.parallel > 1 && plan.spans > 1 ? `, ${Math.min(plan.parallel, plan.spans)} at a time` : ''}; the other ${plan.copied} frames are copied from the source as they are`
      : `Nothing to re-encode: all ${plan.copied} frames are copied from the source as they are`;
  } else {
    const why = !smartCutSetting() ? 'smart cut is off' : plan.copyable ? 'the file does not start at a keyframe' : `${movie.video.codec.split('.')[0]} frames cannot be copied into a track of this encoder's codec`;
    text = `The whole video is decoded and re-encoded${plan.spans > 1 ? ` in ${plan.spans} pieces, ${Math.min(plan.parallel, plan.spans)} at a time` : ''} (${why})`;
  }
  const held = (plan.holds || []).length;
  text += !movie.audio
    ? '.'
    : held
      ? `; the sound is re-encoded (AAC, or Opus) to put silence under the ${held === 1 ? 'held frame' : `${held} held frames`} (E marks), where the picture waits.`
      : movie.audio.copyable
        ? '; audio is copied without re-encoding.'
        : `; the audio (${soundName(movie.audio.codec)}) can't go into an MP4 as it is, so it is re-encoded (AAC, or Opus) where this browser can.`;
  if (softened.length) text += ` Section${softened.length === 1 ? '' : 's'} ${softened.map((s) => '#' + s.id).join(', ')} ${softened.length === 1 ? 'is' : 'are'} softened (blurred) where stripes were found.`;
  if (blended.length) text += ` In section${blended.length === 1 ? '' : 's'} ${blended.map((s) => '#' + s.id).join(', ')} the frames marked B are blended with the frames around them.`;
  return text;
}

/** Another detector like the current one, for a scan segment (or a verify). */
function makeFeeder(width, height) {
  return createDetector(wasm, state.config, width, height, detectorSettings());
}

/**
 * `n` more detectors like state.env.feeder, for the spans of a scan or a
 * prepare that run side by side: made on first use and kept (a GPU
 * detector takes a moment to set up) until the detector is re-created.
 */
async function spareFeeders(n) {
  const env = state.env;
  env.spares = env.spares || [];
  while (env.spares.length < n) env.spares.push(await makeFeeder(state.movie.width, state.movie.height));
  return env.spares.slice(0, n);
}

async function scan() {
  if (!state.movie || !state.env || busy('scan')) return null;
  const t0 = performance.now();
  // (the profile the scan runs under, which it is kept as)
  const profileName = state.project.profile;
  const sig = wasm.config_signature(state.config);
  // what this scan has found so far, and its trace, until the whole result
  // is in: its own, made and dropped inside its job, so nothing outside it
  // can clear it under a running scan (nor keep it after one that stopped)
  const found = { violations: [], partials: [], trace: null };
  // when the timeline was last drawn (0: draw it at the next report)
  let drawnAt = 0;
  const res = await runJob('Scanning for flashes', async (progress, cancelled) => {
    setLive(false);
    state.scanning = found;
    state.partials = found.partials;
    try {
      const r = await scanWithPlan(state.env, state.movie, {
        cancel: cancelled,
        segments: scanSegments(),
        forceSegments: SEGMENTS > 0,
        moreFeeders: spareFeeders,
        // exactly what the scan has found up to where it has got, and what its early looks found after that
        onPartial: (part) => {
          found.violations = part.violations;
          found.partials.push({ ms: performance.now() - t0, until: part.until, found: part.violations.length, early: !!part.early });
          drawnAt = 0;
        },
        onProgress: (p, trace, count, ms) => {
          found.trace = trace;
          const n = found.violations.length;
          progress(p, `${count} frames · ${(count / (ms / 1000)).toFixed(0)} fps${n ? ` · ${n} violation${n === 1 ? '' : 's'} found so far` : ''}`);
          // the timeline twice a second, not per so many frames
          const now = performance.now();
          if (!(now - drawnAt < 500)) {
            drawnAt = now;
            drawTimeline();
            if (chartMode() === 'video') drawChart();
          }
        },
      });
      // (a cancelled scan, which saw only part of the file, throws instead)
      return r;
    } finally {
      if (state.scanning === found) state.scanning = null;
    }
  });
  if (!res) {
    // (what it saw goes with it: the last finished scan is drawn again)
    renderSectionList();
    drawTimeline();
    if (chartMode() === 'video') drawChart();
    return null;
  }
  state.lastScan = res;
  const project = state.project;
  const counted = res.result.violations.filter((v) => counts(res.result, v));
  const n = counted.length;
  const np = counted.filter((v) => v.kind === 'pattern').length;
  project.scan = {
    violations: res.result.violations,
    summary: res.summary,
    frames: res.frames,
    elapsedMs: res.elapsedMs,
    profile: profileName,
    sig,
    safe: n === 0,
    counted: n,
    patterns: np,
    flag_patterns: !!res.result.flag_patterns,
    flag_extended: !!res.result.flag_extended,
    // the per-frame trace, packed: the timeline and the monitor read it, this visit or the next
    trace: packTrace(res.trace),
    anomalies: res.result.anomalies,
    held: res.result.held,
  };
  let added = 0;
  for (const s of res.sections) {
    const overlaps = project.sections.some((o) => o.end > s.start && o.start < s.end);
    if (overlaps) continue;
    project.addSection(s.start, s.end, s.kinds, false);
    added++;
  }
  state.traceNorm = null;
  // drawn before the save, in the same turn as the job's end: nothing (a
  // click on the list, say) can come between the job ending and its
  // sections being on the page, where the list said "No sections yet"
  renderAll();
  updateStatus();
  await project.save();
  const fps = (res.frames / (res.elapsedMs / 1000)).toFixed(0);
  toast(
    n
      ? `${n} violation${n === 1 ? '' : 's'} found${np ? ` (${np} regular pattern${np === 1 ? '' : 's'}, ${n - np} flash)` : ''}, ${added} new section${added === 1 ? '' : 's'} added (${fps} fps).`
      : `No flashing${res.result.flag_patterns ? ' or hazardous patterns' : ''} found in ${res.frames} frames.`,
    6000
  );
  return res;
}

// ---- live monitor ---------------------------------------------------------------

function setLive(on) {
  state.live.on = on;
  $('liveToggle').checked = on;
  $('hud').classList.toggle('hidden', !on);
  if (!on) {
    setVerdict('live-verdict idle', 'monitor off', true);
    state.live.fromScan = false;
    return;
  }
  // the section player: the meter reads the section's check (edited) or the scan (original) as it plays
  if (state.player.mode !== 'video') {
    state.live.fromScan = false;
    setVerdict('live-verdict idle', 'meter on: press play', true);
    setHudBars(0, 0, 0);
    $('hudInfo').textContent = state.player.mode === 'edited' ? 'from the section check, as it plays' : 'from the scan, as it plays';
    return;
  }
  // a finished scan of this file under this profile already holds every frame's
  // numbers: the meter reads those, exactly and completely, instead of
  // detecting again on whatever frames the player happens to show
  state.live.fromScan = !!monitorTrace();
  if (state.live.fromScan) {
    monitorFromScan($('player').currentTime);
    startLiveLoop();
    return;
  }
  state.liveFeeder.reset();
  state.live.t = [];
  state.live.hazard = [];
  state.live.hazardRed = [];
  setVerdict('live-verdict ok', 'watching', true);
  startLiveLoop();
}

let liveLoopActive = false;
function startLiveLoop() {
  const player = $('player');
  if (liveLoopActive || !state.live.on) return;
  liveLoopActive = true;
  const step = (now, meta) => {
    // (the detector of the moment: a new profile, or a new file, brings new ones, the old freed)
    const feeder = state.liveFeeder;
    if (!state.live.on || !feeder) {
      liveLoopActive = false;
      return;
    }
    if (state.live.fromScan) {
      monitorFromScan(meta.mediaTime);
    } else {
      feeder.poll();
      // (a picture the yuv route still copies holds the next back, as a full GPU does)
      if (feeder.videoReady()) feeder.videoElementNow(player, meta.mediaTime, false).then(drainLive, (e) => console.warn(e));
      drainLive();
    }
    if (!player.paused && !player.ended) player.requestVideoFrameCallback(step);
    else {
      liveLoopActive = false;
      setTimeout(drainLive, 50);
    }
  };
  player.requestVideoFrameCallback(step);
}

/** Pack a scan's per-frame trace into typed arrays (28 bytes a frame) for the project store, which keeps it beside the project's record (Project.save). */
function packTrace(tr) {
  return { t: Float64Array.from(tr.t), hazard: Uint32Array.from(tr.hazard), hazardRed: Uint32Array.from(tr.hazardRed), ext: Uint32Array.from(tr.ext), pattern: Uint32Array.from(tr.pattern), lum: Float32Array.from(tr.lum) };
}

/**
 * The scan trace as fractions of the area thresholds, built once and
 * extended as a scan goes on (never recomputed per draw): the trace of the
 * scan under way, else of the project's scan.
 */
function normTrace() {
  const tr = state.scanning ? state.scanning.trace : state.project && state.project.scan && state.project.scan.trace;
  if (!tr || !state.env) return null;
  let n = state.traceNorm;
  if (!n || n.src !== tr) {
    // (peaks: the highest general / red / at-the-limit level anywhere, and when)
    n = { src: tr, t: [], h: [], r: [], e: [], p: [], l: [], peak: { h: [0, 0], r: [0, 0], e: [0, 0] } };
    state.traceNorm = n;
  }
  const thresh = state.env.feeder.det.area_thresh() || 1;
  const pthresh = state.env.feeder.det.pattern_thresh() || 1;
  for (let i = n.t.length; i < tr.t.length; i++) {
    const t = tr.t[i];
    const h = tr.hazard[i] / thresh;
    const r = tr.hazardRed[i] / thresh;
    const e = tr.ext[i] / thresh;
    n.t.push(t);
    n.h.push(h);
    n.r.push(r);
    n.e.push(e);
    n.p.push(tr.pattern[i] / pthresh);
    n.l.push(tr.lum ? tr.lum[i] : 0);
    if (h > n.peak.h[0]) n.peak.h = [h, t];
    if (r > n.peak.r[0]) n.peak.r = [r, t];
    if (e > n.peak.e[0]) n.peak.e = [e, t];
  }
  return n;
}

/**
 * The finished scan of this file under the current profile, as a trace the
 * monitor can read; null to detect live instead (`?monitor=detect`: always,
 * even where there is a scan).
 */
function monitorTrace() {
  if (QUERY.get('monitor') === 'detect' || !state.project || !state.project.scan || state.scanning) return null;
  if (state.project.scan.sig !== wasm.config_signature(state.config)) return null;
  const n = normTrace();
  return n && n.t.length ? n : null;
}

// The meter over the player rises at once and falls back slowly (a full bar
// in a second and a quarter), as a sound meter does: bars that followed the
// video frame by frame would flash along with it.
const HUD_FALL_PER_S = 1.6;
const hud = { haz: 0, red: 0, pat: 0, at: 0, streaming: false };

function setHudBars(haz, red, pat) {
  const now = performance.now();
  const dt = (now - hud.at) / 1000;
  // (a reading after a pause, a seek, is shown as it is)
  hud.streaming = dt <= 0.3;
  const fall = hud.streaming ? dt * HUD_FALL_PER_S : Infinity;
  hud.at = now;
  hud.haz = Math.max(haz, hud.haz - fall);
  hud.red = Math.max(red, hud.red - fall);
  hud.pat = Math.max(pat, hud.pat - fall);
  $('hudHaz').style.width = `${Math.min(100, hud.haz * 50)}%`;
  $('hudRed').style.width = `${Math.min(100, hud.red * 50)}%`;
  $('hudPat').style.width = `${Math.min(100, hud.pat * 50)}%`;
  $('hudHazText').textContent = `${Math.round(hud.haz * 100)}%`;
  $('hudRedText').textContent = `${Math.round(hud.red * 100)}%`;
  $('hudPatText').textContent = `${Math.round(hud.pat * 100)}%`;
}

// The monitor's verdict changes at most every 0.7 s (the live detector's
// own pace); what it would have said meanwhile waits, the latest of it
// shown when the time comes, at once after a pause in the readings, or when
// the picture stops (flushVerdict).
const VERDICT_EVERY_MS = 700;
const verdict = { at: 0, cls: '', text: '', waiting: null };

function setVerdict(cls, text, force = false) {
  const now = performance.now();
  verdict.waiting = null;
  if (cls === verdict.cls && text === verdict.text) return;
  if (!force && hud.streaming && now - verdict.at < VERDICT_EVERY_MS) {
    verdict.waiting = { cls, text };
    return;
  }
  verdict.at = now;
  verdict.cls = cls;
  verdict.text = text;
  const v = $('liveVerdict');
  v.className = cls;
  v.textContent = text;
}

/** The verdict held back by the pace, shown now that the picture has stopped. */
function flushVerdict() {
  const w = verdict.waiting;
  if (w) setVerdict(w.cls, w.text, true);
}

/**
 * The monitor's verdict on a frame: flashing (`kind`, the violation it is
 * in, or null), flashing below the limit or stripes (levels `haz`, `ext`,
 * `pat`, as shares of the limit; stripes count where `patterns`), or how
 * many violations there were before it (`count`).
 */
function meterVerdict(kind, haz, ext, pat, patterns, count) {
  if (kind) setVerdict('live-verdict bad', kind === 'pattern' ? 'hazardous pattern: stripes' : `flashing: ${kind === 'red' ? 'red flash' : kind === 'extended' ? 'extended flash' : 'general flash'}`);
  else if (haz > 0 || ext >= 1 || (pat >= 1 && patterns)) setVerdict('live-verdict warn', haz > 0 || ext >= 1 ? 'flashing below the limit' : 'stripes on screen');
  else setVerdict('live-verdict ok', count ? `${count} violation${count === 1 ? '' : 's'} so far` : 'no flashing so far');
}

/** The meter and the verdict at time `t`, read from the scan. */
function monitorFromScan(t) {
  const n = monitorTrace();
  if (!n) return;
  // the scanned frame at or before t
  let lo = 0;
  let hi = n.t.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (n.t[mid] <= t) lo = mid;
    else hi = mid - 1;
  }
  const i = lo;
  const haz = n.h[i];
  const red = n.r[i];
  const ext = n.e[i];
  const pat = n.p[i];
  setHudBars(haz, red, pat);
  const scan = state.project.scan;
  const viol = scan.violations.filter((v) => counts(scan, v));
  const inside = viol.filter((v) => v.start <= t && t <= v.end);
  const before = viol.filter((v) => v.end < t).length;
  meterVerdict(inside.length ? inside[inside.length - 1].kind : null, haz, ext, pat, scan.flag_patterns, before);
  $('hudInfo').textContent = `from the scan · frame ${i + 1} of ${n.t.length}`;
}

function drainLive() {
  const feeder = state.liveFeeder;
  if (!feeder || !state.live.on || state.live.fromScan) return;
  const det = feeder.det;
  feeder.poll();
  profile.reportEvery(5000, `live monitor (${feeder.fed} pictures watched)`, 0, 0);
  const recs = feeder.records();
  if (!recs.length) return;
  const thresh = det.area_thresh();
  const pthresh = det.pattern_thresh() || 1;
  const L = state.live;
  for (const r of recs) {
    L.t.push(r.t);
    L.hazard.push(r.hazard / thresh);
    L.hazardRed.push(r.hazard_red / thresh);
  }
  const last = recs[recs.length - 1];
  const haz = last.hazard / thresh;
  const red = last.hazard_red / thresh;
  const ext = Math.max(last.ext, last.ext_red) / thresh;
  const pat = last.pattern / pthresh;
  setHudBars(haz, red, pat);
  const now = performance.now();
  if (now - L.lastCheck > 700) {
    L.lastCheck = now;
    const res = feeder.partialVerdict();
    const viol = res.violations.filter((v) => counts(res, v));
    const recent = viol.length && viol[viol.length - 1].end >= last.t - 1.5;
    meterVerdict(recent ? viol[viol.length - 1].kind : null, haz, ext, pat, res.flag_patterns, viol.length);
    $('hudInfo').textContent = `${res.frames} frames watched · ${res.held} re-shown · ${feeder.backend === 'webgpu' ? 'GPU' : 'CPU'} ${(feeder.busyNs / 1e6 / Math.max(1, feeder.fed)).toFixed(2)} ms/frame on the main thread`;
    drawTimeline();
  }
}

// ---- the player: the whole video, or the open section edited or as it is ---------------

const PLAYER_SIZES = { s: 320, m: 480, l: 720 };

function playerSizeSetting() {
  const v = stored('unflash:playerSize');
  return v && PLAYER_SIZES[v] ? v : 'm';
}

function setPlayerSize(size) {
  if (!PLAYER_SIZES[size]) size = 'm';
  $('playerBox').style.setProperty('--player-w', `${PLAYER_SIZES[size]}px`);
  for (const b of document.querySelectorAll('.size-switch [data-size]')) b.classList.toggle('on', b.dataset.size === size);
  store('unflash:playerSize', size);
  drawChart();
}

/** The dim switch darkens every picture of the video: the player, the frames in the grid, the frame at full size. */
function applyDim() {
  const on = $('dimToggle').checked;
  $('player').classList.toggle('dim', on);
  $('preview').classList.toggle('dim', on);
  $('frameGrid').classList.toggle('dim', on);
  $('viewerCanvas').classList.toggle('dim', on);
}

/** Whether the section player plays sound: off unless turned on (remembered in this browser). */
function soundSetting() {
  return stored('unflash.sectionSound') === '1';
}

/**
 * How the section player's sound stands, as its button says it: 'off',
 * 'on', 'slow' (on, but it plays at 1× only), 'failed' (no sound:
 * sectionSound.failed says why) or 'held' (the browser has not started it).
 * On from before, and not yet started, counts as on: the next ▶ starts it.
 */
function soundStatus() {
  if (!soundSetting()) return 'off';
  const s = sectionSound.on ? sectionSound.status() : 'on';
  return s === 'on' && (parseFloat($('previewSpeed').value) || 1) !== 1 ? 'slow' : s;
}

/** The section player's sound button: it says "sound on" only when the sound can play. */
function renderSoundButton() {
  const b = $('btnPreviewSound');
  const movie = state.movie;
  b.classList.toggle('hidden', !movie || !movie.audio);
  if (!movie || !movie.audio) return;
  const s = soundStatus();
  const held = 'Held frames are silent while they wait, as in the export.';
  b.setAttribute('aria-pressed', s === 'off' ? 'false' : 'true');
  b.textContent = { off: 'sound off', on: 'sound on', slow: 'sound at 1× only', failed: 'no sound', held: 'sound held back' }[s];
  b.title = {
    off: `Play the section's sound too (off to start with). ${held}`,
    on: `The section's sound plays along. ${held} Click to turn it off.`,
    slow: `The section's sound plays at 1× only: at this speed there is none. ${held} Click to turn it off.`,
    failed: `No sound: ${sectionSound.failed}. Click to turn it off.`,
    held: "The browser hasn't started the sound: it may be waiting for a click on the page, or have nothing to play it on (no speakers or headphones it can use). ▶ tries again. Click to turn it off.",
  }[s];
  b.classList.toggle('warn', s === 'failed' || s === 'held');
}

function wirePlayer() {
  sectionPlayer = new SectionPlayer($('preview'), { onFrame: onPreviewFrame, onState: onPreviewState, sound: sectionSound });
  $('btnPreviewSound').addEventListener('click', async () => {
    const on = !soundSetting();
    store('unflash.sectionSound', on ? '1' : '0');
    sectionSound.failed = null;
    renderSoundButton();
    await sectionSound.setOn(on);
  });
  frameViewer = new FrameViewer($('viewerCanvas'), {
    onShown: (i) => {
      // (the draws are kept for tests: how often a new picture came)
      (state.viewerDraws = state.viewerDraws || []).push({ i, t: performance.now() });
      if (state.viewerDraws.length > 50) state.viewerDraws.shift();
    },
  });
  $('btnViewFrame').addEventListener('click', () => openViewer(state.selection.size ? Math.min(...state.selection) : 0));
  $('btnCloseViewer').addEventListener('click', closeViewer);
  // a click outside the picture closes it too
  $('frameViewer').addEventListener('click', (e) => {
    if (e.target === $('frameViewer')) closeViewer();
  });
  setThumbSize(thumbSizeSetting(), false);
  for (const b of document.querySelectorAll('.thumb-size [data-thumb]')) b.addEventListener('click', () => setThumbSize(b.dataset.thumb));
  setPlayerSize(playerSizeSetting());
  for (const b of document.querySelectorAll('.size-switch [data-size]')) b.addEventListener('click', () => setPlayerSize(b.dataset.size));
  $('playerSource').addEventListener('change', () => setPlayerSource($('playerSource').value));
  $('btnPreviewPlay').addEventListener('click', () => {
    // sound starts only after a click: this one, when it was left on (or the browser held it back)
    if (soundSetting()) {
      if (!sectionSound.on) sectionSound.setOn(true);
      else sectionSound.wake();
    }
    if (sectionPlayer.active && !sectionPlayer.run.once && !sectionPlayer.paused) sectionPlayer.pause();
    else if (sectionPlayer.paused) sectionPlayer.resume();
    else playSection(state.selection.size ? Math.min(...state.selection) : 0);
  });
  $('btnPreviewStop').addEventListener('click', async () => {
    state.player.gen++;
    await sectionPlayer.stop();
    posterSection();
  });
  $('previewSpeed').addEventListener('change', () => {
    sectionPlayer.setSpeed(parseFloat($('previewSpeed').value) || 1);
    renderSoundButton();
  });
  sectionSound.onChange = () => renderSoundButton();
  // (why there is no sound, said once for each reason, and on the button for as long as it lasts)
  sectionSound.onFail = (why) => {
    renderSoundButton();
    if (why === state.player.soundFailSaid) return;
    state.player.soundFailSaid = why;
    toast(`No sound: ${why}.`, 9000);
  };
}

/** Whether a section can be played: it needs the frame times its preparation records. */
function sectionPlayable(sec) {
  return !!(sec && sec.pts && sec.pts.length && state.decode.supported && state.env);
}

/**
 * Put `mode` in the player: 'video' (the whole file in the <video>, where the
 * live monitor can detect) or the open section, 'edited' (with its marks, as
 * the export renders it) or 'original', in the section player.
 */
function setPlayerSource(mode) {
  const sec = currentSection();
  if (mode !== 'video' && !sec) mode = 'video';
  const changed = state.player.mode !== mode;
  state.player.mode = mode;
  $('playerSource').value = mode;
  for (const o of $('playerSource').options) if (o.value !== 'video') o.disabled = !sec;
  const video = mode === 'video';
  $('player').classList.toggle('hidden', !video);
  $('preview').classList.toggle('hidden', video);
  $('previewBar').classList.toggle('hidden', video);
  if (!video) $('player').pause();
  markPlaying(-1);
  $('previewInfo').textContent = '';
  if (sectionPlayer) {
    state.player.gen++;
    if (video) sectionPlayer.stop();
    else posterSection();
  }
  if (changed && state.live.on) setLive(true);
  renderPlayerWarning();
  drawChart();
}

/**
 * The file into the page's <video> (the whole-video player and the live
 * monitor). A browser that turns down a Matroska file under its own type
 * often plays it offered as WebM (the same format, narrowed); when neither
 * works, the whole-video view says so, and the section player (which
 * decodes with WebCodecs) still plays sections.
 */
function loadPlayer(file, movie) {
  const player = $('player');
  if (player.src) URL.revokeObjectURL(player.src);
  const token = (state.player.loadToken = (state.player.loadToken || 0) + 1);
  const types = movie.format === 'matroska' || movie.format === 'webm' ? [null, 'video/webm'] : [null];
  let k = 0;
  state.player.playable = true;
  const next = () => {
    if (token !== state.player.loadToken) return;
    if (k >= types.length) {
      state.player.playable = false;
      if (state.live.on && state.player.mode === 'video') setLive(false);
      renderPlayerWarning();
      return;
    }
    const t = types[k++];
    if (player.src) URL.revokeObjectURL(player.src);
    player.src = URL.createObjectURL(t ? new Blob([file], { type: t }) : file);
  };
  player.onerror = () => next();
  player.onloadedmetadata = () => {
    if (token !== state.player.loadToken) return;
    state.player.playable = true;
    renderPlayerWarning();
  };
  next();
}

/** The section's first picture in the section player, when nothing is playing. */
function posterSection() {
  const sec = currentSection();
  const canvas = $('preview');
  if (state.player.mode === 'video') return;
  const gen = ++state.player.gen;
  if (!sectionPlayable(sec)) {
    sectionPlayer.stop();
    canvas.getContext('2d').clearRect(0, 0, canvas.width, canvas.height);
    return;
  }
  sectionPlayer.stop().then(() => {
    if (gen !== state.player.gen || currentSection() !== sec || state.player.mode === 'video') return;
    sectionPlayer.play({ env: state.env, movie: state.movie, sec, edited: state.player.mode === 'edited', fromSlot: 0, once: true });
  });
}

/** Play the open section in the player from slot `fromSlot` (edited unless the player is on original). */
function playSection(fromSlot = 0) {
  const sec = currentSection();
  if (!sectionPlayable(sec)) return toast(sec ? 'The section plays once it is prepared.' : 'Open a section first.');
  if (state.player.mode === 'video') setPlayerSource('edited');
  state.player.gen++;
  sectionPlayer.setSpeed(parseFloat($('previewSpeed').value) || 1);
  return sectionPlayer.play({ env: state.env, movie: state.movie, sec, edited: state.player.mode === 'edited', fromSlot, loop: () => $('previewLoop').checked });
}

const reducedMotion = window.matchMedia ? window.matchMedia('(prefers-reduced-motion: reduce)') : { matches: false };

/**
 * Mark the tile of the slot on screen (-1: none). While a section plays the
 * mark moves at most every MIN_GAP_MS, the frame viewer's pace (not at all
 * for anyone who asks their system for less motion), and it is a dim bar,
 * not a bright frame: a mark running from tile to tile with every picture
 * would flicker across the grid, which is the last thing this page should
 * do. `exact`: where the player has stopped, at once.
 */
function markPlaying(k, exact = false) {
  const now = performance.now();
  if (!exact && k >= 0 && (reducedMotion.matches || now - (state.player.markAt || 0) < MIN_GAP_MS)) return;
  const grid = $('frameGrid');
  const prev = state.player.playingTile;
  if (prev === k) return;
  if (prev != null && grid.children[prev]) grid.children[prev].classList.remove('playing');
  state.player.playingTile = k >= 0 ? k : null;
  if (k >= 0 && grid.children[k]) grid.children[k].classList.add('playing');
  state.player.markAt = now;
}

function onPreviewFrame(info, t, plan) {
  const sec = currentSection();
  if (!sec || plan.sec !== sec) return;
  const k = info.sec ? info.slot : -1;
  markPlaying(k);
  const n = plan.seq.t.length;
  const rel = t - (sec.start + plan.base + plan.seq.t[0]);
  const total = plan.seq.t[n - 1] - plan.seq.t[0];
  let what = '';
  if (k >= 0) {
    const e = state.player.mode === 'edited' ? (sec.edits || {})[k] || {} : {};
    what = `frame ${k} at ${fmt(frameVideoTime(sec, k))}${info.src !== k ? `, showing ${info.src}` : ''}${e.removed ? ' (removed)' : e.extended ? ' (held 1 s)' : ''}${info.blended ? `, blended ${Math.round(blendStrength(sec) * 100)}%` : ''}${info.softened ? ', softened' : ''}`;
  }
  $('previewInfo').textContent = `${fmt(Math.max(0, rel))} / ${fmt(total)} · ${what}`;
  if (state.live.on) previewMeter(sec, k, t);
  const now = performance.now();
  if (now - state.player.lastDraw > 250) {
    state.player.lastDraw = now;
    drawTimeline();
  }
}

function onPreviewState(s, detail) {
  const b = $('btnPreviewPlay');
  b.textContent = s === 'playing' ? '❚❚ pause' : s === 'paused' ? '▶ resume' : '▶ play';
  if (s === 'stopped' || s === 'ended') markPlaying(-1, true);
  // paused: the mark where the picture is
  else if (s === 'paused' && sectionPlayer.slot >= 0) markPlaying(sectionPlayer.slot, true);
  if (s !== 'playing') flushVerdict();
  if (s === 'error') banner(`The section player stopped: ${detail && detail.message ? detail.message : detail}`);
}

/** The meter for the frame the section player shows: the section's check (edited) or the scan (original). */
function previewMeter(sec, k, t) {
  if (state.player.mode === 'original') {
    if (monitorTrace()) monitorFromScan(t);
    else {
      setVerdict('live-verdict idle', 'no scan to read', true);
    }
    return;
  }
  const c = sec.check;
  if (!c || c.stale || !c.stats || k < 0 || k >= c.stats.hazard.length) {
    setVerdict('live-verdict idle', c && c.stale ? 'checking the change…' : 'not checked yet', true);
    return;
  }
  const thr = c.area_thresh || 1;
  const pthr = c.pattern_thresh || 1;
  setHudBars(c.stats.hazard[k] / thr, c.stats.hazardRed[k] / thr, (c.stats.pattern[k] || 0) / pthr);
  $('hudInfo').textContent = `from the section check · frame ${k} of ${c.stats.hazard.length}`;
  if (c.flagged && c.flagged.includes(k)) setVerdict('live-verdict bad', 'still failing here');
  else if (c.safe) setVerdict('live-verdict ok', 'passes the check');
  else setVerdict('live-verdict warn', 'fails elsewhere in the section');
}

/** The line above the player: what it shows, whether that passes, whether it is dimmed. */
function renderPlayerWarning() {
  const w = $('playerWarning');
  const sec = currentSection();
  const dim = $('dimToggle').checked ? ' Dimmed.' : ' Not dimmed: full brightness.';
  const mode = state.player.mode;
  let text = '';
  let ok = false;
  if (!state.movie) text = '';
  else if (mode === 'video' && state.player.playable === false) text = `This browser's video player can't play this ${{ webm: 'WebM', matroska: 'MKV', mpegts: 'transport stream (.ts, .m2ts, .mts)' }[state.movie.format] || ''} file, so the whole video can't be shown here. Scanning, sections (their player shows them, marks and all) and the export work as usual.`;
  else if (mode === 'video') text = `⚠ The whole video as it is: it may flash.${dim}`;
  else if (!sec) text = '';
  else if (mode === 'original') text = `⚠ Section #${sec.id} as it is: it may flash.${dim}`;
  else {
    const marked = hasMarks(sec);
    const c = sec.check;
    if (!sectionPlayable(sec)) text = `Section #${sec.id} plays here once it is prepared.`;
    else if (!marked) text = `⚠ Section #${sec.id} has no marks yet, so this is how it is: it may flash.${dim}`;
    else if (!c || c.stale) text = `Section #${sec.id} with your marks: not checked yet.${dim}`;
    else if (c.safe) {
      text = `✓ Section #${sec.id} with your marks: passes the check.${dim}`;
      ok = true;
    } else if (c.wcag_safe) text = `Section #${sec.id} with your marks: passes WCAG; ${remains(remainingKinds(c))}.${dim}`;
    else text = `⚠ Section #${sec.id} with your marks: still fails the check.${dim}`;
  }
  w.textContent = text;
  w.classList.toggle('ok', ok);
  $('btnPreviewPlay').disabled = mode !== 'video' && !sectionPlayable(sec);
}

// ---- timeline -------------------------------------------------------------------

function wireTimeline() {
  const c = $('timeline');
  let drag = null;
  const xToT = (x) => {
    const r = c.getBoundingClientRect();
    const [lo, hi] = state.project.bounds;
    return lo + ((x - r.left) / r.width) * (hi - lo);
  };
  c.addEventListener('mousedown', (e) => {
    if (!state.project) return;
    drag = { x0: e.clientX, t0: xToT(e.clientX), moved: false };
  });
  c.addEventListener('mousemove', (e) => {
    if (!drag) return;
    if (Math.abs(e.clientX - drag.x0) > 4) drag.moved = true;
    drag.t1 = xToT(e.clientX);
    drawTimeline(drag.moved ? [Math.min(drag.t0, drag.t1), Math.max(drag.t0, drag.t1)] : null);
  });
  window.addEventListener('mouseup', (e) => {
    if (!drag) return;
    const d = drag;
    drag = null;
    if (!state.project) return;
    if (d.moved) {
      const a = Math.min(d.t0, d.t1);
      const b = Math.max(d.t0, d.t1);
      if (b - a >= 0.3) addSection(a, b);
      drawTimeline();
      return;
    }
    const t = xToT(e.clientX);
    const hit = state.project.sectionsSorted().find((s) => t >= s.start && t <= s.end);
    if (hit) openSection(hit.id);
    else {
      setPlayerSource('video');
      $('player').currentTime = t;
    }
    drawTimeline();
  });
}

function drawTimeline(dragSpan = null) {
  const c = $('timeline');
  if (!state.project || c.classList.contains('hidden')) return;
  const dpr = window.devicePixelRatio || 1;
  const W = c.clientWidth;
  const H = c.clientHeight;
  if (c.width !== Math.round(W * dpr)) {
    c.width = Math.round(W * dpr);
    c.height = Math.round(H * dpr);
  }
  const g = c.getContext('2d');
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, W, H);
  const [lo, hi] = state.project.bounds;
  const span = Math.max(1e-6, hi - lo);
  const x = (t) => ((t - lo) / span) * W;
  // heat from the scan
  const sum = state.project.scan && state.project.scan.summary;
  if (sum) {
    const n = sum.general.length;
    const bw = Math.max(1, W / n);
    for (let i = 0; i < n; i++) {
      const gcount = sum.general[i];
      const rcount = sum.red[i];
      if (gcount) {
        g.fillStyle = tint(ink.flash, Math.min(1, 0.25 + gcount / 6));
        g.fillRect(x(sum.t0 + i * sum.bin), 6, bw + 0.5, H - 30);
      }
      if (rcount) {
        g.fillStyle = tint(ink.red, Math.min(1, 0.25 + rcount / 6));
        g.fillRect(x(sum.t0 + i * sum.bin), 6, bw + 0.5, (H - 30) / 2);
      }
      const pcount = sum.pattern ? sum.pattern[i] : 0;
      if (pcount) {
        g.fillStyle = tint(ink.pat, Math.min(1, 0.3 + pcount / 4));
        g.fillRect(x(sum.t0 + i * sum.bin), 6 + (H - 30) / 2, bw + 0.5, (H - 30) / 2);
      }
    }
  }
  // the live monitor's trace while it detects, else the scan's (the one under way, or the last)
  const norm = state.live.on && !state.live.fromScan ? null : normTrace();
  const trace = state.live.on && !state.live.fromScan ? { t: state.live.t, h: state.live.hazard, r: state.live.hazardRed } : norm ? { t: norm.t, h: norm.h, r: norm.r } : null;
  if (trace && trace.t.length) {
    const base = H - 24;
    // one bar per pixel column, the tallest there: an hour has a hundred
    // thousand frames and the timeline a thousand pixels
    const cols = Math.max(1, Math.ceil(W));
    const hmax = new Float32Array(cols);
    const rmax = new Float32Array(cols);
    for (let i = 0; i < trace.t.length; i++) {
      const c = Math.floor(x(trace.t[i]));
      if (c < 0 || c >= cols) continue;
      if (trace.h[i] > hmax[c]) hmax[c] = trace.h[i];
      if (trace.r[i] > rmax[c]) rmax[c] = trace.r[i];
    }
    for (const [col, vals] of [
      [tint(ink.flash, 0.9), hmax],
      [tint(ink.red, 0.9), rmax],
    ]) {
      g.strokeStyle = col;
      g.beginPath();
      for (let c = 0; c < cols; c++) {
        const h = Math.min(1.5, vals[c]);
        if (h <= 0) continue;
        g.moveTo(c + 0.5, base);
        g.lineTo(c + 0.5, base - h * 14);
      }
      g.stroke();
    }
  }
  // the baseline the trace stands on
  g.fillStyle = ink.line2;
  g.fillRect(0, H - 24, W, 1);
  // what a running scan has found so far (dashed: its edges may still move)
  const provisional = state.scanning ? state.scanning.violations : null;
  if (provisional && provisional.length) {
    g.save();
    g.setLineDash([4, 3]);
    g.lineWidth = 1.5;
    for (const v of provisional) {
      const x0 = x(v.start);
      const x1 = Math.max(x0 + 4, x(v.end));
      g.strokeStyle = kindInk(v.kind);
      g.strokeRect(x0 + 0.5, 4.5, x1 - x0 - 1, H - 27);
    }
    g.restore();
  }
  // sections
  for (const s of state.project.sectionsSorted()) {
    const x0 = x(s.start);
    const x1 = Math.max(x0 + 3, x(s.end));
    const isCur = state.current === s.id;
    const kind = kindInk(['red', 'flash', 'extended', 'pattern'].find((k) => (s.kinds || []).includes(k)));
    g.fillStyle = isCur ? tint(ink.fg, 0.2) : tint(ink.fg, 0.07);
    g.fillRect(x0, 4, x1 - x0, H - 26);
    g.strokeStyle = s.check && !s.check.stale ? (s.check.safe ? ink.ok : ink.bad) : kind;
    g.lineWidth = isCur ? 2 : 1;
    g.strokeRect(x0 + 0.5, 4.5, x1 - x0 - 1, H - 27);
    g.fillStyle = ink.fg;
    g.font = ink.font(11);
    g.fillText(`#${s.id}`, x0 + 3, 16);
  }
  if (dragSpan) {
    g.fillStyle = tint(ink.fg, 0.25);
    g.fillRect(x(dragSpan[0]), 4, x(dragSpan[1]) - x(dragSpan[0]), H - 26);
  }
  // time ticks
  g.fillStyle = ink.fg2;
  g.font = ink.font(10);
  const step = niceStep(span / Math.max(2, W / 90));
  for (let t = Math.ceil(lo / step) * step; t <= hi; t += step) {
    g.fillRect(x(t), H - 22, 1, 4);
    g.fillText(fmt(t), x(t) + 2, H - 12);
  }
  // playhead: the section player's, while it shows a section
  const ct = state.player.mode !== 'video' && sectionPlayer && sectionPlayer.run && sectionPlayer.run.t != null ? sectionPlayer.run.t : $('player').currentTime;
  if (ct >= lo && ct <= hi) {
    g.fillStyle = ink.fg;
    g.fillRect(x(ct), 0, 1.5, H);
  }
}

function niceStep(raw) {
  const steps = [0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1200, 1800, 3600];
  for (const s of steps) if (s >= raw) return s;
  return 3600;
}

// ---- sections list ----------------------------------------------------------------

function addSection(a, b) {
  const sec = state.project.addSection(a, b, [], true);
  if (!sec) return toast('That range is outside the video');
  state.project.save();
  renderAll();
  openSection(sec.id);
}

function sectionBadge(s) {
  if (!s.prepared && !s.check) return `<span class="badge">unprepared</span>`;
  if (!s.check) return `<span class="badge">unchecked</span>`;
  if (s.check.stale) return `<span class="badge stale">re-check</span>`;
  if (s.check.safe) return `<span class="badge safe">safe</span>`;
  if (s.check.wcag_safe) {
    const kinds = remainingKinds(s.check);
    return kinds.includes('stripes') ? `<span class="badge pat">stripes</span>` : `<span class="badge ext">extended flash</span>`;
  }
  return `<span class="badge unsafe">unsafe</span>`;
}

/** The non-WCAG problems a check still reports, as labels. */
function remainingKinds(c) {
  const inside = c.inside || c.violations || [];
  const out = [];
  if (inside.some((v) => v.kind === 'extended' && counts(c, v))) out.push('extended flash');
  if (inside.some((v) => v.kind === 'pattern' && counts(c, v))) out.push('stripes');
  return out;
}

/** Problems `kinds` (remainingKinds) as what remains: "extended flash remains", "stripes remain". */
function remains(kinds) {
  if (!kinds.length) return 'something the profile flags remains';
  return `${kinds.join(' and ')} remain${kinds.length === 1 && kinds[0] === 'extended flash' ? 's' : ''}`;
}

/** How many of a section's frames its marks change, as the export applies them: removed, held or blended. */
function markCount(s) {
  return Object.values(s.edits || {}).filter((e) => e.removed || e.extended).length + blendMarks(s).length;
}

/** Back to the whole video: no section open, the player on the file, the chart around the playhead. */
function openWholeVideo() {
  if (state.current != null) closeViewer();
  state.current = null;
  state.selection.clear();
  state.anchor = null;
  setPlayerSource('video');
  renderAll();
}

function renderSectionList() {
  const list = $('sectionList');
  list.innerHTML = '';
  const st = scanStatus();
  const [lo, hi] = state.project.bounds;
  const whole = document.createElement('div');
  // (its own class: .sec-item are the sections)
  whole.className = 'sec-whole' + (state.current == null ? ' current' : '');
  whole.title = 'The whole video in the player, and its flashing charted around the playhead (under the limit too)';
  whole.innerHTML = `<div class="sec-title">Whole video ${st ? st.badge : ''}</div><div class="sec-times">${fmt(lo)} – ${fmt(hi)}${st ? ` · ${st.text}` : ''}</div>`;
  whole.addEventListener('click', openWholeVideo);
  list.appendChild(whole);
  for (const s of state.project.sectionsSorted()) {
    const el = document.createElement('div');
    el.className = 'sec-item' + (state.current === s.id ? ' current' : '');
    // (the kinds a scan names: what a project file says goes no further into the page)
    const kinds = (s.kinds || [])
      .filter((k) => KIND_LABEL[k])
      .map((k) => `<span class="badge kind-${k}">${KIND_LABEL[k]}</span>`)
      .join(' ');
    const marks = markCount(s);
    el.innerHTML = `<div class="sec-title">#${s.id} ${sectionBadge(s)}</div><div class="sec-times">${fmt(s.start)} – ${fmt(s.end)} · ${(s.end - s.start).toFixed(1)} s${marks ? ` · ${marks} marks` : ''}${s.custom ? ' · custom' : ''}</div><div>${kinds}</div>`;
    el.addEventListener('click', () => openSection(s.id));
    list.appendChild(el);
  }
  if (!state.project.sections.length) {
    const none = document.createElement('div');
    none.className = 'sec-item';
    none.style.color = 'var(--fg2)';
    none.textContent = st && st.safe ? 'No sections: the scan found nothing to fix. To look at a stretch anyway, drag over it on the timeline.' : 'No sections yet. Scan the video, or drag on the timeline.';
    list.appendChild(none);
  }
}

function renderAll() {
  renderSectionList();
  drawTimeline();
  renderWorkspace();
}

async function prepareAll() {
  for (const s of state.project.sectionsSorted()) {
    if (s.prepared) continue;
    // (one cancelled, or failed, ends it: the rest are not started)
    if (!(await doPrepare(s))) break;
  }
}

async function checkAll() {
  for (const s of state.project.sectionsSorted()) {
    if (!s.prepared) continue;
    // (as Prepare all)
    if (!(await checkJob(s))) break;
  }
  renderAll();
  await state.project.save();
}

/**
 * Check section `sec` as a job and keep the verdict: stale when its marks
 * changed while it ran (it judged the ones before). Resolves to it, or null
 * when cancelled or failed.
 */
async function checkJob(sec) {
  const marks = markSnapshot(sec);
  const c = await runJob(`Checking section #${sec.id}`, (progress, cancelled) => checkSection(state.env, state.project, sec, null, { cancel: cancelled }));
  if (!c) return null;
  if (markSnapshot(sec) !== marks) c.stale = true;
  sec.check = c;
  return c;
}

// ---- workspace ----------------------------------------------------------------------

function currentSection() {
  return state.project && state.current != null ? state.project.section(state.current) : null;
}

function openSection(id) {
  const changed = state.current !== id;
  if (state.selectOnOpen && state.selectOnOpen.id !== id) state.selectOnOpen = null;
  if (changed) closeViewer();
  state.current = id;
  state.selection.clear();
  state.anchor = null;
  const sec = currentSection();
  if (sec) {
    sec.usedAt = Date.now();
    $('player').currentTime = sec.start;
  }
  // the player follows: this section, edited, from its first frame
  if (changed || state.player.mode === 'video') setPlayerSource(sec ? 'edited' : 'video');
  renderAll();
  if (sec && sec.prepared && (!sec.check || sec.check.stale)) scheduleCheck(0);
  // a section prepares itself when opened (unless something else holds the decoder)
  if (sec && !sec.prepared && state.decode.supported && !state.job && !(state.auto && state.auto.running)) doPrepare(sec);
  $('workspace').scrollIntoView({ block: 'nearest' });
}

function wireWorkspace() {
  $('btnPrepare').addEventListener('click', () => doPrepare(currentSection()));
  $('btnReprepare').addEventListener('click', () => doPrepare(currentSection()));
  $('btnDeleteSection').addEventListener('click', async () => {
    const sec = currentSection();
    if (!sec || busy('delete the section') || !confirm(`Delete section #${sec.id}?`)) return;
    // (its frames are freed once no check reads them)
    await withFeeder(() => state.project.deleteSection(sec.id));
    if (state.current === sec.id) state.current = null;
    state.project.save();
    renderAll();
  });
  $('btnApplyRange').addEventListener('click', async () => {
    const sec = currentSection();
    if (!sec || busy('apply the new range')) return;
    const s = wasm.parse_time($('secStart').value);
    const e = wasm.parse_time($('secEnd').value);
    if (s == null || e == null || e <= s) return toast('Enter valid times');
    const [lo, hi] = state.project.bounds;
    const ctxS = wasm.context_seconds(state.config);
    // Another range is another section: its frames go (once no check reads
    // them), and its marks and verdict with them, which were made on those
    // frames; the checks that read its edges, before and after, are stale
    await withFeeder(() => {
      state.project.invalidateNeighbours(sec, ctxS);
      sec.start = Math.max(lo, s);
      sec.end = Math.min(hi, e);
      dropCaches(sec);
      Object.assign(sec, { check: null, edits: {}, keep: [], blend: [], pts: null, nFrames: 0, pattern: null, warnings: [] });
      state.project.invalidateNeighbours(sec, ctxS);
    });
    state.history.delete(sec.id);
    if (state.current === sec.id) {
      closeViewer();
      state.selection.clear();
      state.anchor = null;
      posterSection();
    }
    state.project.save();
    renderAll();
  });
  $('btnCheck').addEventListener('click', () => scheduleCheck(0, true));
  $('softenToggle').addEventListener('change', () => {
    const sec = currentSection();
    if (!sec) return;
    pushHistory(sec);
    sec.soften = $('softenToggle').checked;
    afterEdit(sec);
  });
  $('btnClearEdits').addEventListener('click', () => {
    const sec = currentSection();
    if (!sec) return;
    pushHistory(sec);
    sec.edits = {};
    sec.blend = [];
    afterEdit(sec);
  });
  $('blendStrength').addEventListener('input', () => {
    $('blendValue').textContent = `${$('blendStrength').value}%`;
  });
  $('blendStrength').addEventListener('change', () => {
    const sec = currentSection();
    if (!sec) return;
    pushHistory(sec);
    sec.blendStrength = +$('blendStrength').value / 100;
    afterEdit(sec);
  });
  $('btnUndo').addEventListener('click', undo);
  $('btnRedo').addEventListener('click', redo);
  $('btnSuggestLight').addEventListener('click', () => doSuggest('light'));
  $('btnSuggestDark').addEventListener('click', () => doSuggest('dark'));
  $('btnSuggestFewest').addEventListener('click', () => doSuggest('fewest'));
  $('btnSuggestBlend').addEventListener('click', () => doSuggestBlend());
  $('btnSuggestFps').addEventListener('click', () => doSuggestFps());
  $('btnFpsMenu').addEventListener('click', (e) => {
    e.stopPropagation();
    toggleMenu('fpsMenu');
  });
  $('btnFpsSafe').addEventListener('click', () => {
    $('fpsInput').value = wasm.safe_picture_rate(state.config).toString();
    updateFpsNote();
  });
  $('btnFpsExact').addEventListener('click', () => {
    $('fpsMenu').classList.add('hidden');
    doSuggestFpsExact();
  });
  $('fpsInput').addEventListener('input', updateFpsNote);
  $('fpsInput').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      $('fpsMenu').classList.add('hidden');
      doSuggestFpsExact();
    }
  });
  $('btnMarkRemoved').addEventListener('click', () => markSelection('R'));
  $('btnMarkRemovedNext').addEventListener('click', () => markSelection('F'));
  $('btnMarkExtended').addEventListener('click', () => markSelection('E'));
  $('btnMarkKeep').addEventListener('click', () => markSelection('K'));
  $('btnMarkBlend').addEventListener('click', () => markSelection('B'));
  $('btnUnmark').addEventListener('click', () => markSelection('U'));
  $('btnSelectUnsafe').addEventListener('click', () => {
    const sec = currentSection();
    if (!sec || !sec.check) return;
    selectFrames(sec.check.flagged || [], 'everything still failing');
  });
  $('wsFindings').addEventListener('click', (e) => {
    const go = e.target.closest('button[data-open-section]');
    if (go) return openSection(+go.dataset.openSection);
    const b = e.target.closest('button[data-finding]');
    const sec = currentSection();
    if (!b || !sec || !sec.check) return;
    const f = findings(sec)[+b.dataset.finding];
    if (f) selectFrames(f.frames, f.label);
  });
  const grid = $('frameGrid');
  grid.addEventListener('keydown', (e) => {
    const k = e.key.toUpperCase();
    if (k === 'A' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      const sec = currentSection();
      if (sec) state.selection = new Set(Array.from({ length: shownCount(sec) }, (_, i) => i));
      renderGridMarks();
    }
  });
  $('chart').addEventListener('click', (e) => {
    if (chartMode() === 'video') {
      if (!state.project) return;
      const r = $('chart').getBoundingClientRect();
      const [t0, t1] = videoChartRange();
      $('player').currentTime = t0 + ((e.clientX - r.left) / r.width) * (t1 - t0);
      drawTimeline();
      drawChart();
      return;
    }
    const sec = currentSection();
    if (!sec || !sec.prepared) return;
    const r = $('chart').getBoundingClientRect();
    const shown = shownPts(wasm, sec);
    const i = Math.min(shown.length - 1, Math.max(0, Math.floor(((e.clientX - r.left) / r.width) * shown.length)));
    state.selection = new Set([i]);
    state.anchor = i;
    renderGridMarks();
    const tile = $('frameGrid').children[i];
    if (tile) tile.scrollIntoView({ block: 'nearest' });
    $('player').currentTime = sec.start + shown[i] + (sec.pts ? sec.pts[0] : 0);
  });
  $('chart').addEventListener('mousemove', chartTip);
  $('chart').addEventListener('mouseleave', () => $('chartTip').classList.add('hidden'));
  setChartSpan(chartSpanSetting(), false);
  for (const b of document.querySelectorAll('#chartSpan [data-span]')) b.addEventListener('click', () => setChartSpan(+b.dataset.span));
  // the marking keys work anywhere on the page (as in the original tool), not
  // only with the frame grid focused; typing in a field is left alone
  document.addEventListener('keydown', onKey);
}

function onKey(e) {
  // the text dialogs close with Esc wherever the focus is (the debug text, say)
  if (e.key === 'Escape' && !$('changesModal').classList.contains('hidden')) return closeChanges();
  if (e.key === 'Escape' && !$('debugModal').classList.contains('hidden')) return closeDebug();
  const tag = e.target && e.target.tagName;
  if (tag === 'INPUT' || tag === 'SELECT' || tag === 'TEXTAREA') return;
  if (e.key === 'Escape') {
    if (viewerOpen()) return closeViewer();
    if (document.body.classList.contains('guide-open')) return setGuide(false);
    if (!$('exportModal').classList.contains('hidden')) return;
    state.selection.clear();
    state.anchor = null;
    renderGridMarks();
    return;
  }
  if ([...document.querySelectorAll('.modal')].some((m) => !m.classList.contains('hidden'))) return;
  const sec = currentSection();
  if (!sec || !sec.prepared) return;
  const k = e.key.toLowerCase();
  if (e.ctrlKey || e.metaKey) {
    if (k === 'z') {
      e.preventDefault();
      if (e.shiftKey) redo();
      else undo();
    } else if (k === 'y') {
      e.preventDefault();
      redo();
    }
    return;
  }
  if (e.altKey) return;
  if (['r', 'f', 'e', 'k', 'b', 'u'].includes(k)) {
    e.preventDefault();
    markSelection(k.toUpperCase());
    renderViewerInfo();
    return;
  }
  if (k === 'z') {
    e.preventDefault();
    if (viewerOpen()) closeViewer();
    else openViewer(state.selection.size ? Math.min(...state.selection) : 0);
    return;
  }
  // the arrows step through the frames (one, or ten with shift)
  if (e.key === 'ArrowLeft' || e.key === 'ArrowRight') {
    const from = viewerOpen() ? state.viewerAt : state.selection.size ? Math.min(...state.selection) : null;
    if (from == null) return;
    e.preventDefault();
    const i = Math.max(0, Math.min(shownCount(sec) - 1, from + (e.key === 'ArrowLeft' ? -1 : 1) * (e.shiftKey ? 10 : 1)));
    if (viewerOpen()) return openViewer(i);
    state.selection.clear();
    state.selection.add(i);
    state.anchor = i;
    renderGridMarks();
    const tile = $('frameGrid').children[i];
    if (tile) tile.scrollIntoView({ block: 'nearest' });
  }
}

function updateFpsNote() {
  const v = parseFloat($('fpsInput').value);
  const safe = wasm.safe_picture_rate(state.config);
  const note = $('fpsNote');
  if (!(v > 0)) note.textContent = '';
  else if (wasm.rate_is_guaranteed(state.config, v)) note.textContent = `${v}/s is at or under the guaranteed-safe ${safe}/s: no arrangement of pictures at that rate can fail this profile.`;
  else note.textContent = `${v}/s is above the guaranteed-safe ${safe}/s, so the result is a proposal that the check judges, not a promise.`;
  const ladder = rateLadder(safe, state.movie ? state.movie.fps : 30);
  $('fpsSearchNote').textContent = safe > 0
    ? `The button tries ${ladder[0]}/s first (twice the guaranteed-safe ${safe}/s) and steps down a tenth at a time, ${ladder.join(', ')}/s, keeping the first rate that passes the check.`
    : `This profile has no rate that can never fail, so the button tries ${ladder.join(', ')}/s in turn and keeps the first that passes the check.`;
  const sec = currentSection();
  $('fpsShown').textContent = sec && sec.fpsFound ? `(${sec.fpsFound}/s)` : '';
}

function renderWorkspace() {
  const sec = currentSection();
  const ws = $('workspace');
  if (!sec) {
    ws.classList.add('hidden');
    if (state.player.mode !== 'video') setPlayerSource('video');
    drawChart();
    return;
  }
  ws.classList.remove('hidden');
  $('wsTitle').textContent = `Section #${sec.id}: ${fmt(sec.start)} – ${fmt(sec.end)}`;
  $('secStart').value = fmt(sec.start);
  $('secEnd').value = fmt(sec.end);
  $('wsUnprepared').classList.toggle('hidden', !!sec.prepared);
  $('wsBody').classList.toggle('hidden', !sec.prepared);
  const warn = $('wsWarnings');
  const notes = [...(sec.warnings || []), ...((sec.check && sec.check.context_notes) || [])];
  warn.textContent = notes.join('\n');
  warn.classList.toggle('hidden', !notes.length);
  renderVerdict(sec);
  if (!$('fpsInput').value) $('fpsInput').value = wasm.safe_picture_rate(state.config).toString();
  updateFpsNote();
  renderSoften(sec);
  renderBlend(sec);
  renderUndo();
  if (sec.prepared) renderGrid(sec);
  drawChart();
}

/** The blend strength: shown when the section has frames marked B. */
function renderBlend(sec) {
  const marks = (sec.blend || []).length;
  $('blendWrap').classList.toggle('hidden', !marks);
  const pct = Math.round(blendStrength(sec) * 100);
  $('blendStrength').value = String(pct);
  $('blendValue').textContent = `${pct}%`;
  $('blendNote').textContent = marks ? `${marks} frame${marks === 1 ? '' : 's'}` : '';
}

/** The "soften stripes" switch: shown when the section has a pattern. */
function renderSoften(sec) {
  const plan = sec.prepared && sec.cache ? softenPlan(sec) : null;
  const relevant = !!plan || !!sec.soften || (sec.kinds || []).includes('pattern');
  $('softenWrap').classList.toggle('hidden', !relevant);
  $('softenToggle').checked = !!sec.soften;
  $('softenToggle').disabled = !sec.prepared;
  let note = '';
  if (plan) {
    const scale = state.movie ? state.movie.width / sec.cache.width() : 1;
    note = `${plan.frames.size} of ${sec.nFrames} frames · blur σ ≈ ${(plan.sigma * scale).toFixed(1)} px (stripes ${(2 * plan.period * scale).toFixed(0)} px apart)`;
  } else if (sec.prepared) note = 'no stripes found in this section';
  $('softenNote').textContent = note;
}

/** Section `sec`'s verdict, the button to select what fails and the findings, when it is the section open. */
function renderVerdict(sec) {
  // (a check that ends after another section was opened draws nothing there)
  if (sec !== currentSection()) return;
  const v = $('wsVerdict');
  const c = sec.check;
  if (!c) {
    v.className = 'verdict';
    v.textContent = sec.prepared ? 'unchecked' : 'not prepared';
  } else if (c.stale) {
    v.className = 'verdict';
    v.textContent = 'needs re-check';
  } else if (c.safe) {
    v.className = 'verdict safe';
    v.textContent = 'passes';
  } else if (c.wcag_safe) {
    const kinds = remainingKinds(c);
    v.className = 'verdict ' + (kinds.includes('stripes') ? 'pat' : 'ext');
    v.textContent = kinds.length ? `passes WCAG, ${remains(kinds)}` : 'passes WCAG';
  } else {
    v.className = 'verdict unsafe';
    v.textContent = describeFailure(sec, c);
  }
  $('btnSelectUnsafe').classList.toggle('hidden', !(c && !c.safe && c.flagged && c.flagged.length));
  renderFindings(sec);
  renderPlayerWarning();
}

/**
 * What a section's check still objects to inside it, one entry per
 * violation: its kind, the frames it covers (in the edited sequence, the
 * grid's numbering) and when, for the list under the verdict.
 */
function findings(sec) {
  const c = sec.check;
  if (!c || c.stale || !c.seq || !c.seq.t) return [];
  const out = [];
  for (const v of (c.inside || []).filter((v) => counts(c, v))) {
    const frames = framesOf(c.seq.t, v);
    if (!frames.length) continue;
    out.push({ v, kind: v.kind, frames, first: frames[0], last: frames[frames.length - 1], label: `the ${KIND_LABEL[v.kind] || v.kind} at frames ${frames[0]}–${frames[frames.length - 1]}` });
  }
  return out;
}

/** The frames of a checked sequence (times `t`) that violation `v` covers, in order (editing::flagged_frames, for the one). */
function framesOf(t, v) {
  return Array.from(wasm.flagged_frames(Float64Array.from(t), JSON.stringify([v])));
}

/** When frame `i` of a prepared section is, on the whole video's clock. */
function frameVideoTime(sec, i) {
  return sec.start + sec.pts[i];
}

/** How many frames the grid shows of prepared section `sec` (a frame on its very end gives it its length, but shows after it). */
function shownCount(sec) {
  return shownPts(wasm, sec).length;
}

/**
 * A moment on a section's checked timeline (seconds from its first frame,
 * the seconds its held frames add included), on the whole video's clock:
 * while a frame is held, that frame's own time.
 */
function videoTimeAt(sec, t) {
  const seqT = sec.check && sec.check.seq ? sec.check.seq.t : null;
  if (!seqT || !seqT.length || !sec.pts || t <= seqT[0]) return sec.start + (sec.pts && sec.pts.length ? sec.pts[0] : 0) + t;
  let i = 0;
  while (i + 1 < seqT.length && seqT[i + 1] <= t) i++;
  const own = i + 1 < seqT.length ? sec.pts[i + 1] - sec.pts[i] : Infinity;
  return frameVideoTime(sec, i) + Math.min(t - seqT[i], own);
}

/** A moment on a section's checked timeline, in words: "0:03.503 into the section". */
function intoSection(t) {
  return t < -0.0005 ? `${fmt(-t)} before the section` : `${fmt(t)} into the section`;
}

/**
 * The other sections over a stretch of the video's time `a`–`b` (seconds
 * from the start of `sec`), nearest `sec` first.
 */
function sectionsNear(sec, a, b) {
  const lo = sec.start + a;
  const hi = sec.start + b;
  return state.project
    .sectionsSorted()
    .filter((o) => o.id !== sec.id && o.end > lo + 1e-6 && o.start < hi - 1e-6)
    .sort((x, y) => Math.abs(x.start - sec.start) - Math.abs(y.start - sec.start));
}

/** "at least " where a violation starts as far back as the check's run-up reaches (it may start further back). */
function atLeast(c, v) {
  return c.context_lead && v.start <= -c.context_lead + 0.05 ? 'at least ' : '';
}

/**
 * Where a violation's flashing lies outside `sec`, in words ("it starts
 * 3.0 s before this section, in section #2"), and the section to open for
 * it (null if none).
 */
function outsideOf(sec, v) {
  const c = sec.check;
  const parts = [];
  let open = null;
  const where = (list, video) => {
    if (!list.length) return `, in ${video}, which no section covers`;
    if (!open) open = list[0];
    return `, in section${list.length > 1 ? 's' : ''} ${list.map((o) => '#' + o.id).join(' and ')}`;
  };
  if (v.start < -0.05) parts.push(`it starts ${atLeast(c, v)}${(-v.start).toFixed(1)} s before this section${where(sectionsNear(sec, v.start, 0), 'the video before it')}`);
  const end = c.endDisp || 0;
  // (the check sees the run-out's few seconds: flashing that goes on to their end may go on further)
  const beyond = c.context_tail && v.end >= end + c.context_tail - 0.05 ? 'at least ' : '';
  if (v.end > end + 0.05) parts.push(`it runs on ${beyond}${(v.end - end).toFixed(1)} s past its end${where(sectionsNear(sec, sec.end - sec.start, sec.end - sec.start + v.end - end), 'the video after it')}`);
  return { text: parts.join('; '), open };
}

/** Select `frames` of the open section, bring the first into view and say what was selected. */
function selectFrames(frames, what) {
  if (!frames.length) return toast('Nothing to select: the last check found nothing failing in this section.');
  state.selection = new Set(frames);
  state.anchor = frames[0];
  renderGridMarks();
  const tile = $('frameGrid').children[frames[0]];
  // (once the rest of this turn has scrolled: opening a section brings its top into view)
  if (tile) requestAnimationFrame(() => tile.scrollIntoView({ block: 'center', behavior: 'smooth' }));
  toast(`Selected ${frames.length} frame${frames.length === 1 ? '' : 's'}: ${what}. A Suggest button with "selection only" ticked works on just these.`, 6000);
  drawChart();
}

/**
 * Flashing just before the section that runs up to its first picture and is
 * the run-up's to fix (the check leaves it out of this section's verdict).
 */
function runUpFlashing(sec) {
  const c = sec.check;
  if (!c || c.stale || !c.before) return [];
  return c.before.filter((v) => v.kind !== 'pattern' && counts(c, v) && v.end >= -1);
}

/** The list under the verdict: each remaining problem, where it is, and what fixes it. */
function renderFindings(sec) {
  const box = $('wsFindings');
  const list = sec.prepared ? findings(sec) : [];
  const runUp = sec.prepared ? runUpFlashing(sec) : [];
  box.classList.toggle('hidden', !list.length && !runUp.length);
  if (!list.length && !runUp.length) {
    box.innerHTML = '';
    return;
  }
  const label = (kind) => (KIND_LABEL[kind] || kind).replace(/^./, (m) => m.toUpperCase());
  const openButton = (o) => (o ? `<button class="small" data-open-section="${o.id}">open section #${o.id}</button>` : '');
  const dur = (f) => `${fmt(frameVideoTime(sec, f.first))} – ${fmt(frameVideoTime(sec, f.last))} in the video, ${fmt(sec.check.seq.t[f.first])} – ${fmt(sec.check.seq.t[f.last])} into the section`;
  const how = {
    flash: 'Flashing faster than 3 times a second over enough of the screen: remove (R, F) or blend (B) frames, or let a Suggest button pick them.',
    red: 'Red flashing faster than 3 times a second: remove or blend frames, or let a Suggest button pick them.',
    extended: 'Flashing at the limit rate (3 a second) for 5 seconds or more. WCAG allows it; this profile flags it because long runs of it affect some viewers. It clears once the flashing is broken into stretches shorter than 5 seconds by pauses of more than a second (remove or hold frames), or once its contrast is low enough: select it, tick "selection only" and use a Suggest button.',
    pattern: 'A stationary stripe pattern over a quarter of the screen: tick "soften stripes" above.',
  };
  const own = list.map((f, k) => {
    const out = outsideOf(sec, f.v);
    const more = out.text ? `; ${out.text}` : '';
    return `<div class="finding ${f.kind}"><b>${label(f.kind)}</b>, frames ${f.first}–${f.last} (${dur(f)}, ${(sec.check.seq.t[f.last] - sec.check.seq.t[f.first]).toFixed(1)} s)${more}<button class="small" data-finding="${k}">select these frames</button>${openButton(out.open)}<div class="how">${how[f.kind] || ''}</div></div>`;
  });
  const before = runUp.map((v) => {
    const near = sectionsNear(sec, v.start, 0);
    const where = near.length ? `in section #${near[0].id}` : 'in the video before it, which no section covers';
    const what = v.end >= -0.05 ? 'up to its first picture' : `up to ${(-v.end).toFixed(1)} s before it`;
    const fix = near.length ? `It is fixed there: nothing in this section's frames can clear it.` : `Nothing in this section's frames can clear it: widen this section back over it, or add a section there.`;
    return `<div class="finding ${v.kind} run-up"><b>${label(v.kind)} before this section</b>, from ${atLeast(sec.check, v)}${(-v.start).toFixed(1)} s before it ${what}, ${where}${openButton(near[0])}<div class="how">${fix} This section's verdict leaves it out.</div></div>`;
  });
  box.innerHTML = own.join('') + before.join('');
}

function describeFailure(sec, c) {
  const parts = [];
  const inside = c.inside || c.violations || [];
  for (const v of inside.slice(0, 3)) {
    const kind = KIND_LABEL[v.kind] || v.kind;
    let s = `${kind} at ${fmt(videoTimeAt(sec, v.start))}, ${intoSection(v.start)}`;
    if (v.kind === 'pattern') s += ` (${v.count.toFixed(1)}× the area limit)`;
    else if (v.kind !== 'extended' && v.count < 1.08) s += ` (only ${Math.round((v.count - 1) * 100)}% over the area threshold, just over the line)`;
    parts.push(s);
  }
  if (inside.length > 3) parts.push(`+${inside.length - 3} more`);
  if (c.after && c.after.length) parts.push(`${c.after.length} past the end of this section`);
  return `fails: ${parts.join('; ')}`;
}

// ---- frame grid ---------------------------------------------------------------------

let tileObserver = null;
let scratch = null; // one analysis-size canvas; tiles are drawn at thumbnail size
/** Smallest tile width per thumbnail size (the grid fills the row with them). */
const THUMB_SIZES = { s: 110, m: 146, l: 220, xl: 320 };
let THUMB_W = 160;

function thumbSizeSetting() {
  const v = stored('unflash:thumbSize');
  return THUMB_SIZES[v] ? v : 'm';
}

function setThumbSize(size, redraw = true) {
  if (!THUMB_SIZES[size]) size = 'm';
  const min = THUMB_SIZES[size];
  $('frameGrid').style.setProperty('--thumb-min', `${min}px`);
  // drawn a little over the tile's size for a sharp picture, never far past the cache's own
  THUMB_W = Math.round(Math.max(160, min * 1.3));
  for (const b of document.querySelectorAll('.thumb-size [data-thumb]')) b.classList.toggle('on', b.dataset.thumb === size);
  store('unflash:thumbSize', size);
  const sec = currentSection();
  if (redraw && sec && sec.prepared && sec.cache) renderGrid(sec);
}

// ---- one frame at full size -------------------------------------------------------

let frameViewer = null;

function viewerOpen() {
  return !$('frameViewer').classList.contains('hidden');
}

/** Show frame `i` of the open section at full size (and select it). */
function openViewer(i) {
  const sec = currentSection();
  if (!sec || !sec.prepared || !state.movie) return;
  i = Math.max(0, Math.min(shownCount(sec) - 1, i));
  state.selection.clear();
  state.selection.add(i);
  state.anchor = i;
  state.viewerAt = i;
  $('viewerCanvas').classList.toggle('dim', $('dimToggle').checked);
  $('frameViewer').classList.remove('hidden');
  renderViewerInfo();
  frameViewer.show(state.movie, sec, i);
  renderGridMarks();
  const tile = $('frameGrid').children[i];
  if (tile) tile.scrollIntoView({ block: 'nearest' });
}

function closeViewer() {
  if (!viewerOpen()) return;
  $('frameViewer').classList.add('hidden');
  frameViewer.clear();
  $('frameGrid').focus({ preventScroll: true });
}

function renderViewerInfo() {
  const sec = currentSection();
  const i = state.viewerAt;
  if (!sec || i == null || !viewerOpen()) return;
  const e = (sec.edits || {})[i] || {};
  const marks = [];
  if (e.removed) marks.push(`removed: frame ${e.fill === 'next' ? 'after' : 'before'} it shows instead`);
  if (e.extended) marks.push('held for 1 s');
  if ((sec.keep || []).includes(i)) marks.push('keep');
  if ((sec.blend || []).includes(i)) marks.push(`blended ${Math.round(blendStrength(sec) * 100)}% with the frames around it in the export (shown here as it is)`);
  $('viewerInfo').textContent = `Section #${sec.id}, frame ${i} of ${shownCount(sec)} · ${fmt(frameVideoTime(sec, i))} in the video, ${(sec.pts[i] - sec.pts[0]).toFixed(3)} s into the section${marks.length ? ' · ' + marks.join(' · ') : ''} · ${state.movie.width}×${state.movie.height}`;
}

/**
 * A tile's thumbnail from the section's small copies: blended, for a frame
 * marked B, as the check sees it (`tile.dataset.want` names that version;
 * the blur of softened frames is a CSS filter on top).
 */
function drawTile(sec, tile) {
  const i = +tile.dataset.i;
  const want = tile.dataset.want || '';
  const canvas = tile.querySelector('canvas');
  try {
    const cache = want ? blendedFrames(sec) : sec.cache;
    const aw = cache.width();
    const ah = cache.height();
    if (!scratch || scratch.width !== aw || scratch.height !== ah) scratch = new OffscreenCanvas(aw, ah);
    const rgba = cache.frame(i);
    const img = new ImageData(new Uint8ClampedArray(rgba.buffer, rgba.byteOffset, rgba.byteLength), aw, ah);
    scratch.getContext('2d').putImageData(img, 0, 0);
    canvas.getContext('2d').drawImage(scratch, 0, 0, canvas.width, canvas.height);
    tile.dataset.drawnKey = want;
  } catch (e) {
    /* cache gone */
  }
}

function renderGrid(sec) {
  const grid = $('frameGrid');
  grid.innerHTML = '';
  if (tileObserver) tileObserver.disconnect();
  const shown = shownPts(wasm, sec);
  $('frameCount').textContent = `(${shown.length})`;
  const aw = sec.cache.width();
  const ah = sec.cache.height();
  const tw = THUMB_W;
  const th = Math.max(1, Math.round((THUMB_W * ah) / aw));
  tileObserver = new IntersectionObserver(
    (entries) => {
      for (const en of entries) {
        if (!en.isIntersecting) continue;
        const tile = en.target;
        if (tile.dataset.drawn) continue;
        tile.dataset.drawn = '1';
        drawTile(sec, tile);
        tileObserver.unobserve(tile);
      }
    },
    { root: null, rootMargin: '200px' }
  );
  const frag = document.createDocumentFragment();
  for (let i = 0; i < shown.length; i++) {
    const tile = document.createElement('div');
    tile.className = 'frame';
    tile.dataset.i = i;
    const at = fmt(frameVideoTime(sec, i));
    tile.title = `Frame ${i}, at ${at} in the video, ${shown[i].toFixed(3)} s into the section: double-click (or select it and press Z) to see it at full size, decoded from the file, to read a subtitle, say`;
    const canvas = document.createElement('canvas');
    canvas.width = tw;
    canvas.height = th;
    tile.appendChild(canvas);
    const no = document.createElement('span');
    no.className = 'fno';
    no.textContent = i;
    tile.appendChild(no);
    // when it is in the whole video (as a verify of the export gives it), and into the section
    const fv = document.createElement('span');
    fv.className = 'fv';
    fv.textContent = at;
    tile.appendChild(fv);
    const ft = document.createElement('span');
    ft.className = 'ft';
    ft.textContent = shown[i].toFixed(3);
    tile.appendChild(ft);
    const fb = document.createElement('span');
    fb.className = 'fb hidden';
    tile.appendChild(fb);
    tile.addEventListener('mousedown', (e) => {
      e.preventDefault();
      onTileClick(i, e);
    });
    tile.addEventListener('dblclick', () => openViewer(i));
    frag.appendChild(tile);
    tileObserver.observe(tile);
  }
  grid.appendChild(frag);
  renderGridMarks();
  // opened from a verify: the frames on screen while the export flashed
  const want = state.selectOnOpen;
  if (want && want.id === sec.id) {
    state.selectOnOpen = null;
    const frames = [];
    for (let i = 0; i < shown.length; i++) {
      const next = i + 1 < sec.pts.length ? frameVideoTime(sec, i + 1) : Infinity;
      if (frameVideoTime(sec, i) <= want.hi && next > want.lo) frames.push(i);
    }
    if (frames.length) selectFrames(frames, `the frames at ${fmt(want.lo)}–${fmt(want.hi)} in the video, where the check of the export found ${KIND_LABEL[want.kind] ? `the ${KIND_LABEL[want.kind]}` : 'flashing'}`);
  }
}

function onTileClick(i, e) {
  const sel = state.selection;
  const sec = currentSection();
  if (e.shiftKey && state.anchor != null) {
    const caps = e.getModifierState && e.getModifierState('CapsLock');
    if (caps) {
      const cols = Math.max(1, Math.round($('frameGrid').clientWidth / ($('frameGrid').children[0].offsetWidth + 6)));
      const [r0, c0] = [Math.floor(state.anchor / cols), state.anchor % cols];
      const [r1, c1] = [Math.floor(i / cols), i % cols];
      const n = shownCount(sec);
      sel.clear();
      for (let r = Math.min(r0, r1); r <= Math.max(r0, r1); r++) for (let c = Math.min(c0, c1); c <= Math.max(c0, c1); c++) if (r * cols + c < n) sel.add(r * cols + c);
    } else {
      sel.clear();
      for (let k = Math.min(state.anchor, i); k <= Math.max(state.anchor, i); k++) sel.add(k);
    }
  } else if (e.ctrlKey || e.metaKey) {
    if (sel.has(i)) sel.delete(i);
    else sel.add(i);
    state.anchor = i;
  } else {
    sel.clear();
    sel.add(i);
    state.anchor = i;
  }
  $('frameGrid').focus({ preventScroll: true });
  renderGridMarks();
}

function renderGridMarks() {
  const sec = currentSection();
  if (!sec || !sec.prepared) return;
  const grid = $('frameGrid');
  // (what a removed frame shows instead, as the export's sequence of the frames shown has it)
  const rep = Array.from(wasm.replacement_map(JSON.stringify(sec.edits || {}), shownCount(sec)));
  const c = sec.check;
  // the frames each kind of violation covers (a stale check's too: until the next one, the best guess)
  const of = { flash: new Set(), red: new Set(), pattern: new Set(), extended: new Set() };
  if (c && c.inside && c.seq && c.seq.t) for (const v of c.inside) if (of[v.kind]) for (const i of framesOf(c.seq.t, v)) of[v.kind].add(i);
  const redFlag = of.red;
  const patFlag = of.pattern;
  const extFlag = of.extended;
  // (a frame in a flash too shows the flash: that is the one to fix first)
  const genFlag = new Set([...(c && !c.stale ? c.flagged || [] : [])].filter((i) => !extFlag.has(i)));
  for (const i of of.flash) genFlag.add(i);
  const soft = new Set(sec.soften && sec.check && !sec.check.stale ? sec.check.soft_frames || [] : []);
  const kept = new Set(sec.keep || []);
  const blendSet = new Set(blendMarks(sec));
  // the blended thumbnails are redrawn when the marks or the strength change
  const blendKey = blendSet.size && sec.cache && blendedFrames(sec) !== sec.cache ? sec.blendKey : '';
  const blendPct = Math.round(blendStrength(sec) * 100);
  const softBlur = soft.size && sec.cache ? `blur(${((sec.check.soft_sigma || 1) * THUMB_W) / sec.cache.width()}px)` : '';
  for (let i = 0; i < grid.children.length; i++) {
    const tile = grid.children[i];
    const e = (sec.edits || {})[i] || {};
    tile.classList.toggle('removed', !!e.removed);
    tile.classList.toggle('extended', !!e.extended && !e.removed);
    tile.classList.toggle('selected', state.selection.has(i));
    tile.classList.toggle('flagged', genFlag.has(i) && !redFlag.has(i) && !patFlag.has(i));
    tile.classList.toggle('flagged-red', redFlag.has(i));
    tile.classList.toggle('flagged-pat', patFlag.has(i) && !redFlag.has(i));
    tile.classList.toggle('flagged-ext', extFlag.has(i) && !genFlag.has(i) && !redFlag.has(i) && !patFlag.has(i));
    tile.classList.toggle('soft', soft.has(i));
    tile.classList.toggle('kept', kept.has(i));
    const blended = blendSet.has(i) && !e.removed;
    tile.classList.toggle('blended', blended);
    tile.dataset.want = blended ? blendKey : '';
    if (tile.dataset.drawn && (tile.dataset.drawnKey || '') !== tile.dataset.want) drawTile(sec, tile);
    tile.querySelector('canvas').style.filter = soft.has(i) ? softBlur : '';
    const fb = tile.querySelector('.fb');
    if (e.removed) {
      fb.textContent = `shows ${rep[i]}`;
      fb.classList.remove('hidden');
    } else if (e.extended) {
      fb.textContent = blended ? `held 1 s · blend ${blendPct}%` : 'held 1 s';
      fb.classList.remove('hidden');
    } else if (blended) {
      fb.textContent = `blend ${blendPct}%`;
      fb.classList.remove('hidden');
    } else fb.classList.add('hidden');
  }
}

/**
 * R, F, E, K and B toggle their own mark on the selected frames, as in the
 * original tool: pressed on frames that already carry it (judged by the
 * first frame selected) they take it off, otherwise they put it on. R and F
 * each toggle their own direction, so one pressed on a removal marked the
 * other way flips it; a removal and a blend (B) exclude each other. U takes
 * every mark off.
 */
function markSelection(key) {
  const sec = currentSection();
  if (!sec || !sec.prepared) return;
  if (!state.selection.size) return toast('Select frames first (click, or shift-click for a range)');
  pushHistory(sec);
  sec.edits = sec.edits || {};
  const keep = new Set(sec.keep || []);
  const blend = new Set(sec.blend || []);
  const items = [...state.selection];
  const first = sec.edits[items[0]] || null;
  const removedAs = (fill) => !!(first && first.removed && ((first.fill || 'prev') === 'next') === (fill === 'next'));
  const set = (i, removed, extended, fill) => {
    if (!removed && !extended) delete sec.edits[i];
    else sec.edits[i] = { removed, extended: extended && !removed, fill: fill || 'prev' };
  };
  if (key === 'R' || key === 'F') {
    const fill = key === 'F' ? 'next' : 'prev';
    const on = !removedAs(fill);
    for (const i of items) {
      const e = sec.edits[i] || {};
      set(i, on, on ? false : !!e.extended, fill);
      if (on) {
        keep.delete(i);
        blend.delete(i);
      }
    }
  } else if (key === 'E') {
    const on = !(first && first.extended && !first.removed);
    for (const i of items) {
      const e = sec.edits[i] || {};
      set(i, on ? false : !!e.removed, on, e.fill);
    }
  } else if (key === 'K') {
    const on = !keep.has(items[0]);
    for (const i of items) {
      if (!on) keep.delete(i);
      else {
        keep.add(i);
        // a kept frame stays on screen: a removal goes, a hold stays
        const e = sec.edits[i];
        if (e && e.removed) delete sec.edits[i];
      }
    }
  } else if (key === 'B') {
    const on = !blend.has(items[0]);
    for (const i of items) {
      if (!on) blend.delete(i);
      else {
        blend.add(i);
        // a blended frame stays on screen: a removal goes, a hold stays
        const e = sec.edits[i];
        if (e && e.removed) delete sec.edits[i];
      }
    }
    if (on && sec.blendStrength == null) sec.blendStrength = BLEND_DEFAULT;
  } else {
    for (const i of items) {
      delete sec.edits[i];
      keep.delete(i);
      blend.delete(i);
    }
  }
  sec.keep = [...keep].sort((a, b) => a - b);
  sec.blend = [...blend].sort((a, b) => a - b);
  afterEdit(sec);
}

/** After a change of the open section's marks: what shows them, its verdict (stale until checked again) and its neighbours'. */
function afterEdit(sec) {
  if (sec.check) sec.check.stale = true;
  state.project.invalidateNeighbours(sec, wasm.context_seconds(state.config));
  state.project.save();
  renderSoften(sec);
  renderBlend(sec);
  renderGridMarks();
  renderVerdict(sec);
  renderSectionList();
  renderUndo();
  drawChart();
  if ($('autoCheck').checked) scheduleCheck(120);
  // (a run of keys restarts the player once)
  replayEdited(sec, 150);
}

/** A section playing edited picks a change of its marks up where it is: it restarts, from the slot on screen, `ms` later. */
function replayEdited(sec, ms = 0) {
  if (sec.id !== state.current || state.player.mode !== 'edited' || !sectionPlayer || !sectionPlayer.active || sectionPlayer.paused) return;
  clearTimeout(state.player.restartTimer);
  state.player.restartTimer = setTimeout(() => playSection(Math.max(0, sectionPlayer.slot)), ms);
}

// ---- undo / redo of a section's marks --------------------------------------------------

function historyOf(sec) {
  let h = state.history.get(sec.id);
  if (!h) {
    h = { undo: [], redo: [] };
    state.history.set(sec.id, h);
  }
  return h;
}

function markSnapshot(sec) {
  return JSON.stringify({ edits: sec.edits || {}, keep: sec.keep || [], soften: !!sec.soften, blend: sec.blend || [], blendStrength: sec.blendStrength == null ? null : sec.blendStrength });
}

/** Remember a section's marks before a change, for undo. */
function pushHistory(sec) {
  const h = historyOf(sec);
  const snap = markSnapshot(sec);
  if (h.undo[h.undo.length - 1] !== snap) h.undo.push(snap);
  if (h.undo.length > 200) h.undo.shift();
  h.redo = [];
  renderUndo();
}

function restoreMarks(sec, snap) {
  const o = JSON.parse(snap);
  sec.edits = o.edits || {};
  sec.keep = o.keep || [];
  sec.soften = !!o.soften;
  sec.blend = o.blend || [];
  sec.blendStrength = o.blendStrength == null ? null : o.blendStrength;
}

/** Take the open section's marks a step back through its history (`back`: undo) or forward again (redo). */
function stepHistory(back) {
  const sec = currentSection();
  if (!sec || !sec.prepared) return;
  const h = historyOf(sec);
  const from = back ? h.undo : h.redo;
  if (!from.length) return toast(back ? 'Nothing to undo' : 'Nothing to redo');
  (back ? h.redo : h.undo).push(markSnapshot(sec));
  restoreMarks(sec, from.pop());
  afterEdit(sec);
}

function undo() {
  return stepHistory(true);
}

function redo() {
  return stepHistory(false);
}

function renderUndo() {
  const sec = currentSection();
  const h = sec ? state.history.get(sec.id) : null;
  $('btnUndo').disabled = !h || !h.undo.length;
  $('btnRedo').disabled = !h || !h.redo.length;
}

// ---- checking -------------------------------------------------------------------------

function scheduleCheck(delay, announce = false) {
  clearTimeout(state.checkTimer);
  state.checkTimer = setTimeout(() => runCheck(announce), delay);
}

async function runCheck(announce) {
  const sec = currentSection();
  if (!sec || !sec.prepared) return;
  if (state.checkRunning) {
    state.checkAgain = true;
    return;
  }
  state.checkRunning = true;
  const v = $('wsVerdict');
  v.className = 'verdict';
  v.textContent = 'checking…';
  const t0 = performance.now();
  try {
    // once no job is under way, and the last one's caller has taken in what
    // it made (a suggestion's marks go in as it ends): those are the marks
    // it checks, and the ones it compares with when it is done
    while (state.job) await afterJobs();
    const marks = markSnapshot(sec);
    const c = await withFeeder(() => checkSection(state.env, state.project, sec, null));
    // (the marks changed while it ran: it judged the ones before, and says so)
    if (markSnapshot(sec) !== marks) c.stale = true;
    sec.check = c;
    sec.checkMs = performance.now() - t0;
    if (announce && !c.stale) toast(`${c.safe ? 'Passes' : 'Fails'} (${c.frames} frames checked in ${(sec.checkMs / 1000).toFixed(2)} s)`);
  } catch (e) {
    console.error(e);
    banner(`Check failed: ${e.message || e}`);
  } finally {
    state.checkRunning = false;
  }
  renderVerdict(sec);
  renderGridMarks();
  renderSectionList();
  drawTimeline();
  drawChart();
  await state.project.save();
  if (state.checkAgain) {
    state.checkAgain = false;
    scheduleCheck(0);
  }
}

async function doPrepare(sec) {
  if (!sec) return;
  if (!state.decode.supported) return banner(`Preparing needs WebCodecs to decode ${state.movie.video.codec}: ${state.decode.reason}`);
  if (busy('prepare the section')) return false;
  const ok = await runJob(`Preparing section #${sec.id}`, async (progress, cancelled) => {
    // room for its frames: other sections' go, the least lately used first
    // (here, in the job, under the feeder lock: no check is reading them)
    state.project.evictCaches(sec, cacheBudget());
    await prepareSection(state.env, state.movie, sec, {
      cancel: cancelled,
      spans: scanSegments(),
      moreFeeders: spareFeeders,
      onProgress: (n) => progress(Math.min(0.95, n / Math.max(1, (sec.end - sec.start + 2 * wasm.context_seconds(state.config)) * state.movie.fps)), `${n} frames decoded`),
    });
    return true;
  });
  // cancelled or failed: the section is as it was (a prepare changes it only once every frame is in)
  if (!ok) return false;
  sec.check = null;
  renderAll();
  updateStatus();
  await state.project.save();
  if (state.current === sec.id) {
    scheduleCheck(0);
    if (state.player.mode !== 'video' && !sectionPlayer.active) posterSection();
  }
  return true;
}


/**
 * Take a suggester's result into the section (undoably): its edits (frames
 * marked keep keep theirs), its verdict (or none), and the neighbours'
 * checks go stale.
 */
function applySuggestion(sec, res, only) {
  pushHistory(sec);
  sec.edits = JSON.parse(wasm.apply_suggestion(JSON.stringify(sec.edits || {}), JSON.stringify(res.edits), only ? JSON.stringify(only) : undefined, keepJson(sec)));
  sec.check = res.verdict || null;
  state.project.invalidateNeighbours(sec, wasm.context_seconds(state.config));
  replayEdited(sec);
}

/** The blend suggestion's result into the section (undoably): its marks and their strength in place of the section's, and its verdict. */
function applyBlend(sec, res) {
  pushHistory(sec);
  sec.edits = res.edits;
  sec.blend = res.blend;
  sec.blendStrength = res.strength;
  sec.check = res.verdict || null;
  state.project.invalidateNeighbours(sec, wasm.context_seconds(state.config));
  replayEdited(sec);
}

/** A frame rate's suggestion into the section, and the rate (the menu names it). */
function applyRate(sec, res, only) {
  applySuggestion(sec, res, only);
  sec.fpsFound = res.fps;
}

/**
 * Run suggester `run(sec, only, progress, cancelled)` on the open section
 * (on the frames selected, with "selection only") as job `name`, take its
 * result in with `take` (undoably) and say its note for `ms`. A suggestion
 * is made from the marks the section had when it started: when they change
 * while it runs (a key pressed meanwhile), it is not taken, so that nothing
 * marked meanwhile is overwritten.
 */
async function suggestJob(name, run, ms, take = applySuggestion) {
  const sec = currentSection();
  if (!sec || !sec.prepared) return;
  const only = $('suggestSelOnly').checked && state.selection.size ? Array.from(state.selection) : null;
  const marks = markSnapshot(sec);
  const res = await runJob(name, (progress, cancelled) => run(sec, only, progress, cancelled));
  if (!res) return;
  if (markSnapshot(sec) !== marks) return toast(`${name}: the section's marks changed while it ran, so its suggestion was not applied. Suggest again.`, 8000);
  take(sec, res, only);
  renderAll();
  await state.project.save();
  toast(res.note, ms);
}

function doSuggest(prefer) {
  const what = prefer === 'fewest' ? 'fewest removals' : `keep ${prefer}`;
  return suggestJob(`Suggesting (${what})`, (sec, only, progress, cancel) => suggestEdits(state.env, state.project, sec, prefer, only, { cancel, onProgress: (r) => progress(Math.min(0.95, 0.1 + r * 0.08), `check ${r + 1}`) }), 6000);
}

/** Blend frames ("lower contrast"): blend the flashing frames with the frames around them, as little as passes. */
function doSuggestBlend() {
  return suggestJob('Suggesting (blend frames)', (sec, only, progress, cancel) => suggestBlend(state.env, state.project, sec, only, { cancel, onProgress: (r) => progress(Math.min(0.95, 0.05 + r * 0.09), `check ${r + 1}`) }), 8000, applyBlend);
}

/** Reduce FPS: from twice the guaranteed-safe rate down, a tenth at a time, to the first rate that passes. */
function doSuggestFps() {
  const take = (sec, res, only) => {
    applyRate(sec, res, only);
    $('fpsInput').value = String(res.fps);
  };
  return suggestJob('Reducing the frame rate', (sec, only, progress, cancel) => searchFrameRate(state.env, state.project, sec, only, { sourceFps: state.movie.fps, cancel, onProgress: (p, r) => progress(p, `checking ${r} pictures/s`) }), 9000, take);
}

/** Reduce FPS to exactly the rate typed in the menu. */
function doSuggestFpsExact() {
  const open = currentSection();
  if (!open || !open.prepared) return;
  const v = parseFloat($('fpsInput').value);
  if (!(v > 0)) return toast('Type a rate, in pictures a second');
  return suggestJob(`Thinning to ${v} pictures/s`, (sec, only, progress, cancel) => suggestFrameRate(state.env, state.project, sec, only, v, { cancel }), 7000, applyRate);
}

// ---- chart -------------------------------------------------------------------------------

/**
 * What the chart shows: the open section while the player plays it, else
 * the whole video around the playhead.
 */
function chartMode() {
  return currentSection() && state.player.mode !== 'video' ? 'section' : 'video';
}

const CHART_SPANS = [10, 30, 120, 0];

function chartSpanSetting() {
  const v = stored('unflash.chartSpan');
  return v !== null && CHART_SPANS.includes(+v) ? +v : 30;
}

function setChartSpan(span, redraw = true) {
  state.chartSpan = span;
  store('unflash.chartSpan', String(span));
  for (const b of document.querySelectorAll('#chartSpan [data-span]')) b.classList.toggle('on', +b.dataset.span === span);
  if (redraw) drawChart();
}

/** The stretch of the video the whole-video chart shows: `chartSpan` seconds around the playhead, or all of it. */
function videoChartRange() {
  const [lo, hi] = state.project.bounds;
  const span = state.chartSpan;
  if (!span || span >= hi - lo) return [lo, hi];
  const ct = $('player').currentTime || lo;
  const t0 = Math.max(lo, Math.min(ct - span / 2, hi - span));
  return [t0, t0 + span];
}

/** Index of the first of `ts` (ascending) at or after `t`. */
function lowerBound(ts, t) {
  let lo = 0;
  let hi = ts.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (ts[mid] < t) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}


/** The scan's verdict on the whole video, for the section list and the chart. */
function scanStatus() {
  const p = state.project;
  if (!p) return null;
  if (state.scanning) return { badge: '<span class="badge">scanning…</span>', text: 'scanning…', scanning: true };
  const s = p.scan;
  if (!s) return { badge: '<span class="badge">not scanned</span>', text: state.decode.supported ? 'not scanned yet' : 'this browser cannot scan it' };
  if (s.sig !== wasm.config_signature(state.config)) return { badge: '<span class="badge stale">scan again</span>', text: 'scanned under another profile: scan again' };
  // (numbers: a scan can come from a project file, and these go into the page as HTML)
  const frames = Number(s.frames);
  const n = Number(s.counted);
  const kinds = `flashing${s.flag_extended ? ' (extended flashes included)' : ''}${s.flag_patterns ? ' or stripe patterns' : ''}`;
  if (s.safe) return { badge: '<span class="badge safe">nothing found</span>', text: `✓ no ${kinds} found in ${frames} frames`, safe: true, kinds };
  return { badge: `<span class="badge unsafe">${n} found</span>`, text: `${n} violation${n === 1 ? '' : 's'} found in ${frames} frames` };
}

function drawChart() {
  const c = $('chart');
  const sec = currentSection();
  const dpr = window.devicePixelRatio || 1;
  const W = c.clientWidth;
  const H = c.clientHeight;
  if (c.width !== Math.round(W * dpr)) {
    c.width = Math.round(W * dpr);
    c.height = Math.round(H * dpr);
  }
  const g = c.getContext('2d');
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, W, H);
  const video = chartMode() === 'video';
  $('chartSpan').classList.toggle('hidden', !video || !state.project);
  if (video) return drawVideoChart(g, W, H);
  $('chartTitle').textContent = `Section #${sec.id}, frame by frame`;
  if (!sec.prepared || !sec.check || !sec.check.stats || !sec.check.stats.t.length) {
    $('chartHint').textContent = 'The chart appears once the section has been checked.';
    return;
  }
  const st = sec.check.stats;
  const n = st.t.length;
  const thresh = sec.check.area_thresh || 1;
  const mid = H * 0.62;
  const bw = W / n;
  $('chartHint').textContent = 'Brightness (line) and how much of the window is changing (bars: brightening up, darkening down, magenta for red, orange when the flash rate is over the limit). The blue line is flashing at the limit rate (extended flashes are made of it; shaded blue where one is), the teal line how much of the picture is a stripe pattern. Red shading is removed, blue is held, violet is blended. Click to jump to a frame.';
  const blendSet = new Set(blendMarks(sec));
  // frames only an extended flash flags
  const extOnly = new Set();
  const inFlash = new Set();
  for (const f of findings(sec)) for (const i of f.frames) (f.kind === 'extended' ? extOnly : inFlash).add(i);
  for (const i of inFlash) extOnly.delete(i);
  for (let i = 0; i < n; i++) {
    const e = (sec.edits || {})[i] || {};
    if (blendSet.has(i) && !e.removed) {
      g.fillStyle = tint(ink.blend, 0.22);
      g.fillRect(i * bw, 0, bw + 0.5, H);
    }
    if (e.removed) {
      g.fillStyle = tint(ink.bad, 0.22);
      g.fillRect(i * bw, 0, bw + 0.5, H);
    } else if (e.extended) {
      g.fillStyle = tint(ink.held, 0.22);
      g.fillRect(i * bw, 0, bw + 0.5, H);
    }
    if (sec.check.flagged && sec.check.flagged.includes(i)) {
      g.fillStyle = extOnly.has(i) ? tint(ink.ext, 0.14) : tint(ink.flash, 0.12);
      g.fillRect(i * bw, 0, bw + 0.5, H);
    }
  }
  const scale = (H * 0.36) / (thresh * 1.5);
  for (let i = 0; i < n; i++) {
    const x = i * bw;
    const up = st.up[i] * scale;
    const dn = st.down[i] * scale;
    g.fillStyle = st.hazard[i] >= thresh ? ink.flash : tint(ink.flash, 0.5);
    g.fillRect(x, mid - up, Math.max(1, bw - 0.5), up);
    g.fillRect(x, mid, Math.max(1, bw - 0.5), dn);
    const red = st.red[i] * scale;
    if (red > 0) {
      g.fillStyle = st.hazardRed[i] >= thresh ? ink.red : tint(ink.red, 0.6);
      g.fillRect(x, mid - red, Math.max(1, bw - 0.5), red);
    }
  }
  g.strokeStyle = tint(ink.fg, 0.35);
  g.beginPath();
  g.moveTo(0, mid - thresh * scale);
  g.lineTo(W, mid - thresh * scale);
  g.moveTo(0, mid + thresh * scale);
  g.lineTo(W, mid + thresh * scale);
  g.stroke();
  g.strokeStyle = ink.fg;
  g.lineWidth = 1.2;
  g.beginPath();
  for (let i = 0; i < n; i++) {
    const y = H * 0.55 - st.lum[i] * H * 0.5;
    if (i === 0) g.moveTo(i * bw + bw / 2, y);
    else g.lineTo(i * bw + bw / 2, y);
  }
  g.stroke();
  if (st.ext && st.ext.length === n) {
    g.strokeStyle = ink.ext;
    g.lineWidth = 1.2;
    g.beginPath();
    for (let i = 0; i < n; i++) {
      const yv = mid - st.ext[i] * scale;
      if (i === 0) g.moveTo(i * bw + bw / 2, yv);
      else g.lineTo(i * bw + bw / 2, yv);
    }
    g.stroke();
  }
  if (st.pattern && st.pattern.length === n && sec.check.pattern_thresh) {
    const pt = sec.check.pattern_thresh;
    g.strokeStyle = ink.pat;
    g.lineWidth = 1.2;
    g.beginPath();
    for (let i = 0; i < n; i++) {
      const y = H * 0.98 - Math.min(1.5, st.pattern[i] / pt) * H * 0.3;
      if (i === 0) g.moveTo(i * bw + bw / 2, y);
      else g.lineTo(i * bw + bw / 2, y);
    }
    g.stroke();
  }
  for (const i of state.selection) {
    g.fillStyle = tint(ink.sel, 0.35);
    g.fillRect(i * bw, 0, bw + 0.5, H);
  }
}

/**
 * The whole video around the playhead, from the scan: per pixel column, how
 * much of the screen flashed faster than the limit (bars; general and red,
 * as a share of the area limit), how much flashed at the limit rate (the
 * blue line: extended flashes are built from it), the stripes (teal) and
 * the brightness (grey), under the sections and what the scan found. It
 * shows the flashing that stays under the limit too, to see what is close
 * to a section or close to failing.
 */
function drawVideoChart(g, W, H) {
  const hint = $('chartHint');
  const title = $('chartTitle');
  if (!state.project || !state.movie) {
    title.textContent = '';
    hint.textContent = 'Open a video to see its flashing over time.';
    return;
  }
  const [t0, t1] = videoChartRange();
  const span = Math.max(1e-6, t1 - t0);
  const x = (t) => ((t - t0) / span) * W;
  const scan = state.project.scan;
  const [lo, hi] = state.project.bounds;
  title.textContent = t0 <= lo + 1e-6 && t1 >= hi - 1e-6 ? 'Whole video' : `Whole video, ${fmt(t0)} – ${fmt(t1)}`;
  const top = 13;
  const bottom = H - 16;
  const plotH = bottom - top;
  // per column of the chart: the most any frame showing in it reached, and
  // its mean brightness (a frame fills the columns it is on screen for, so a
  // short video draws unbroken lines)
  const n = normTrace();
  const cols = Math.max(1, Math.floor(W));
  const hmax = new Float32Array(cols);
  const rmax = new Float32Array(cols);
  const emax = new Float32Array(cols);
  const pmax = new Float32Array(cols);
  const lsum = new Float32Array(cols);
  const lcnt = new Uint32Array(cols);
  let peak = 0;
  if (n && n.t.length) {
    const med = state.movie.medianDelta || 1 / 30;
    const first = Math.max(0, lowerBound(n.t, t0) - 1);
    const env = limitRateEnvelope(n, first, t1);
    for (let i = first; i < n.t.length && n.t[i] <= t1; i++) {
      const ta = n.t[i];
      const tb = i + 1 < n.t.length ? Math.min(n.t[i + 1], ta + 1) : ta + med;
      const c0 = Math.max(0, Math.floor(x(ta)));
      const c1 = Math.min(cols - 1, Math.max(c0, Math.ceil(x(tb)) - 1));
      const e = env[i - first];
      for (let c = c0; c <= c1; c++) {
        if (n.h[i] > hmax[c]) hmax[c] = n.h[i];
        if (n.r[i] > rmax[c]) rmax[c] = n.r[i];
        if (e > emax[c]) emax[c] = e;
        if (n.p[i] > pmax[c]) pmax[c] = n.p[i];
        lsum[c] += n.l[i];
        lcnt[c]++;
      }
      peak = Math.max(peak, n.h[i], n.r[i], e, scan && scan.flag_patterns ? n.p[i] : 0);
    }
  }
  // levels as shares of the limit: room for the highest in view, up to eight times it
  const MAXL = Math.min(8, Math.max(1.5, peak * 1.1));
  const y = (lev) => bottom - (Math.min(MAXL, Math.max(0, lev)) / MAXL) * plotH;
  // the sections
  g.font = ink.font(10);
  for (const s of state.project.sectionsSorted()) {
    if (s.end < t0 || s.start > t1) continue;
    const x0 = Math.max(0, x(s.start));
    const x1 = Math.min(W, Math.max(x0 + 2, x(s.end)));
    g.fillStyle = state.current === s.id ? tint(ink.fg, 0.16) : tint(ink.fg, 0.06);
    g.fillRect(x0, top, x1 - x0, plotH);
    g.fillStyle = ink.fg2;
    g.fillText(`#${s.id}`, x0 + 3, top + 10);
  }
  // what the scan found, along the top
  const found = state.scanning ? state.scanning.violations : scan ? scan.violations.filter((v) => counts(scan, v)) : [];
  for (const v of found) {
    if (v.end < t0 || v.start > t1) continue;
    g.fillStyle = kindInk(v.kind);
    const x0 = Math.max(0, x(v.start));
    g.fillRect(x0, 2, Math.max(2, Math.min(W, x(v.end)) - x0), 7);
  }
  if (n && n.t.length) {
    // bars: flashing faster than the limit, general then red
    for (let c = 0; c < cols; c++) {
      if (hmax[c] > 0) {
        g.fillStyle = hmax[c] >= 1 ? ink.flash : tint(ink.flash, 0.55);
        g.fillRect(c, y(hmax[c]), 1, bottom - y(hmax[c]));
      }
      if (rmax[c] > 0) {
        g.fillStyle = rmax[c] >= 1 ? ink.red : tint(ink.red, 0.6);
        g.fillRect(c, y(rmax[c]), 1, bottom - y(rmax[c]));
      }
    }
    // lines: brightness, flashing at the limit rate, stripes
    const line = (vals, colour, level, cnt = null) => {
      g.strokeStyle = colour;
      g.lineWidth = 1.2;
      g.beginPath();
      let on = false;
      for (let c = 0; c < cols; c++) {
        if (cnt && !cnt[c]) {
          on = false;
          continue;
        }
        const v = level(vals[c], c);
        if (v == null) {
          on = false;
          continue;
        }
        if (on) g.lineTo(c + 0.5, v);
        else g.moveTo(c + 0.5, v);
        on = true;
      }
      g.stroke();
    };
    line(lsum, tint(ink.fg, 0.45), (v, c) => bottom - (v / lcnt[c]) * plotH * 0.9, lcnt);
    line(emax, ink.ext, (v) => y(v), lcnt);
    if (scan && scan.flag_patterns) line(pmax, ink.pat, (v) => y(v), lcnt);
  }
  // the limit
  g.save();
  g.setLineDash([4, 3]);
  g.strokeStyle = tint(ink.fg, 0.45);
  g.beginPath();
  g.moveTo(0, y(1) + 0.5);
  g.lineTo(W, y(1) + 0.5);
  g.stroke();
  g.restore();
  g.fillStyle = tint(ink.fg, 0.6);
  const limitLabel = MAXL > 1.6 ? `limit (top: ${Math.round(MAXL * 10) / 10}×)` : 'limit';
  g.fillText(limitLabel, W - g.measureText(limitLabel).width - 4, y(1) - 3);
  // time ticks and the playhead
  g.fillStyle = ink.fg2;
  const step = niceStep(span / Math.max(2, W / 90));
  for (let t = Math.ceil(t0 / step) * step; t <= t1; t += step) {
    g.fillRect(x(t), bottom, 1, 3);
    g.fillText(fmt(t), x(t) + 2, H - 3);
  }
  const ct = $('player').currentTime;
  if (ct >= t0 && ct <= t1) {
    g.fillStyle = ink.fg;
    g.fillRect(x(ct), 0, 1.5, H);
  }
  // the words under it
  const st = scanStatus();
  let lead = '';
  if (!n || !n.t.length) lead = st && st.scanning ? 'Scanning… ' : 'Scan the video to chart its flashing. ';
  else if (st && st.safe) {
    const pk = n.peak;
    const worst = pk.h[0] >= pk.r[0] ? ['general', pk.h] : ['red', pk.r];
    lead = `<span class="found-none">✓ No ${st.kinds} found.</span> ${worst[1][0] > 0 ? `The closest it came: ${Math.round(worst[1][0] * 100)}% of the area limit (${worst[0]} flashing, at ${fmt(worst[1][1])}). ` : 'Nothing flashes faster than the limit anywhere. '}`;
  }
  hint.innerHTML = `${lead}Bars: how much of the screen flashes faster than the limit (orange; magenta for red), as a share of the area limit (the dashed line). Blue: flashing at the limit rate, which extended flashes are made of; grey: brightness${scan && scan.flag_patterns ? '; teal: stripes' : ''}. Boxes are sections; the strip along the top is what the scan found. Click to play from there.`;
}

/** Seconds either side over which "flashing at the limit rate" is read: it comes in spikes, one per transition. */
const ENVELOPE_S = 0.25;

/**
 * The trace's flashing at the limit rate as an envelope, from frame `first`
 * to the last at or before `t1`: at each frame the most within
 * ENVELOPE_S either side, so a stretch of it reads as one.
 */
function limitRateEnvelope(n, first, t1) {
  const out = [];
  const q = []; // frames in the window, their values falling
  let hi = first;
  let lo = first;
  for (let i = first; i < n.t.length && n.t[i] <= t1; i++) {
    while (hi < n.t.length && n.t[hi] <= n.t[i] + ENVELOPE_S) {
      while (q.length && n.e[q[q.length - 1]] <= n.e[hi]) q.pop();
      q.push(hi++);
    }
    while (lo < i && n.t[lo] < n.t[i] - ENVELOPE_S) lo++;
    while (q.length && q[0] < lo) q.shift();
    // (frames before `first` count too)
    let e = q.length ? n.e[q[0]] : 0;
    for (let j = first - 1; j >= 0 && n.t[j] >= n.t[i] - ENVELOPE_S; j--) e = Math.max(e, n.e[j]);
    out.push(e);
  }
  return out;
}

/** The readout under the pointer on the whole-video chart. */
function chartTip(e) {
  const tip = $('chartTip');
  const n = chartMode() === 'video' && state.project ? normTrace() : null;
  if (!n || !n.t.length) return tip.classList.add('hidden');
  const r = $('chart').getBoundingClientRect();
  const [t0, t1] = videoChartRange();
  const t = t0 + ((e.clientX - r.left) / r.width) * (t1 - t0);
  const i = Math.min(n.t.length - 1, lowerBound(n.t, t));
  const pct = (v) => `${Math.round(v * 100)}%`;
  const sec = state.project.sectionsSorted().find((s) => t >= s.start && t <= s.end);
  const atLimit = limitRateEnvelope(n, i, n.t[i])[0];
  tip.textContent = `${fmt(n.t[i])} · faster than the limit ${pct(n.h[i])}${n.r[i] > 0 ? `, red ${pct(n.r[i])}` : ''} · at the limit rate ${pct(atLimit)}${n.p[i] > 0 ? ` · stripes ${pct(n.p[i])}` : ''}${sec ? ` · section #${sec.id}` : ''}`;
  tip.classList.remove('hidden');
  const w = tip.offsetWidth;
  tip.style.left = `${Math.max(2, Math.min(r.width - w - 2, e.clientX - r.left + 10))}px`;
}

// ---- export ----------------------------------------------------------------------------

async function openExport() {
  const p = state.project;
  if (!p) return;
  const rows = p.sectionsSorted().map((s) => {
    const marks = markCount(s);
    const applies = marks || (s.soften && softenPlan(s));
    const status = !applies ? 'nothing to apply' : !(s.pts && s.pts.length) ? 'has marks but was never prepared: marks will NOT be applied' : s.check && !s.check.stale ? (s.check.safe ? 'passes' : 'still failing') : 'unchecked';
    return `<tr><td>#${s.id}</td><td>${fmt(s.start)} – ${fmt(s.end)}</td><td>${marks} marks</td><td>${status}</td></tr>`;
  });
  $('exportSummary').innerHTML = rows.length ? `<table><tr><th>section</th><th>range</th><th>edits</th><th>status</th></tr>${rows.join('')}</table>` : '<p class="hint">No sections. The export re-encodes the video unchanged.</p>';
  const sel = $('exportCodec');
  const previous = sel.value;
  sel.innerHTML = '';
  const cands = formatChoices(await encoderCandidates(state.movie.width, state.movie.height, state.movie.fps, +$('exportQuality').value));
  state.exportCands = cands;
  for (const c of cands) {
    const info = formatInfo(c, state.movie);
    const o = document.createElement('option');
    o.value = c.label;
    o.textContent = info.label + (c === cands[0] ? ' (recommended)' : '');
    o.title = c.config.codec;
    sel.appendChild(o);
  }
  if (cands.some((c) => c.label === previous)) sel.value = previous;
  await renderExportChoice();
  $('btnDoExport').disabled = !cands.length || !state.decode.supported;
  if (!state.exportBlob) {
    const kept = privateStorageAvailable() && !window.showSaveFilePicker;
    $('exportResult').innerHTML = `<p class="hint">An export stays here, to download or verify, until another video is opened${kept ? ', and comes back with this video after the page is reloaded' : ''}. A file you saved can be checked with <b>verify a saved file…</b>.</p>`;
  }
  $('exportName').value = chosenExportName();
  $('exportWhere').textContent = exportWhere();
  $('btnVerifyExport').title = state.exportBlob ? 'Scan the exported file again, every frame, with the current profile' : 'Export first (or check a file you saved with "verify a saved file…")';
  $('exportModal').classList.remove('hidden');
}

/** The export dialog's lines for the chosen format: what it means, what will be re-encoded, how big. */
async function renderExportChoice() {
  const cands = state.exportCands || [];
  const chosen = cands.find((c) => c.label === $('exportCodec').value) || cands[0];
  const softened = state.project.sectionsSorted().filter((s) => s.soften && softenPlan(s));
  const blended = state.project.sectionsSorted().filter((s) => s.pts && s.pts.length && blendMarks(s).length);
  const plan = chosen ? await exportPlan(state.env, state.movie, state.project, { codec: chosen.config.codec, smartCut: smartCutSetting(), parallel: parallelSetting() }) : null;
  state.exportPlan = plan;
  $('exportFormatNote').textContent = chosen ? formatInfo(chosen, state.movie).note : '';
  $('exportPlan').textContent = plan ? describePlan(plan, state.movie, softened, blended) : 'This browser has no WebCodecs video encoder, so it cannot export.';
  const need = estimateExportBytes(state.movie, +$('exportQuality').value, plan);
  $('exportSize').textContent = chosen ? `About ${fmtBytes(need)}. ${window.showSaveFilePicker ? 'You will be asked where to save it.' : privateStorageAvailable() ? "It is written to the browser's private storage on disk and offered for download." : `It is assembled in memory and offered for download${need > memoryExportLimit() ? ', which is more than this browser is likely to hold' : ''}.`}` : '';
}

/**
 * Where the export goes, and how to choose the folder: this browser's save
 * dialog where it has one for pages (Chrome, Edge); elsewhere the download
 * goes where the browser puts downloads, unless its settings have it ask.
 */
function exportWhere() {
  if (window.showSaveFilePicker) return 'Export asks where to save it, starting from this name.';
  const ua = navigator.userAgent;
  if (/Firefox\//.test(ua)) return 'Firefox saves it to your Downloads folder. To choose the folder (and the name) each time, turn on "Always ask you where to save files" in Firefox\'s settings (General, Files and Applications): Download then asks.';
  if (/Safari\//.test(ua) && !/Chrom(e|ium)\//.test(ua)) return 'Safari saves it to your Downloads folder. To choose the folder each time, set "File download location" to "Ask for each download" in Safari\'s settings (General).';
  return "It goes where this browser puts downloads; its settings can have it ask where to save each one.";
}

/** The name the export is saved under: the one typed in the dialog, made fit for a file (an .mp4), or the video's own name with .unflashed. */
function chosenExportName() {
  const typed = state.exportFileName;
  if (!typed || !state.movie) return state.movie ? exportName(state.movie) : '';
  return typed;
}

/** `raw` as a file name: no characters a file system refuses, no path, ending in .mp4 (null: nothing left). */
function cleanExportName(raw) {
  let n = String(raw || '')
    .replace(/[\u0000-\u001f<>:"/\\|?*]+/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .replace(/^[.\s]+|[.\s]+$/g, '');
  if (!n) return null;
  if (!/\.mp4$/i.test(n)) n = n.replace(/\.(mov|m4v|mkv|webm|avi|mp4v)$/i, '') + '.mp4';
  return n;
}

/** The name typed in the export dialog: kept for this video, and on an export already made. */
function onExportNameInput(final) {
  const clean = cleanExportName($('exportName').value);
  state.exportFileName = clean;
  const name = chosenExportName();
  if (final) $('exportName').value = name;
  const a = $('exportDownload');
  if (a.getAttribute('href')) a.download = name;
}

async function doExport() {
  if (busy('export')) return;
  const movie = state.movie;
  const quality = +$('exportQuality').value;
  // a file of the user's choosing where the browser has the dialog, private
  // storage on disk where it has that, memory as the last resort
  onExportNameInput(true);
  const name = chosenExportName();
  let sinkInfo = await pickSaveSink(name);
  if (sinkInfo && sinkInfo.cancelled) return;
  if (!sinkInfo) sinkInfo = await privateFileSink(estimateExportBytes(movie, quality, state.exportPlan), exportOwner(state.project.key));
  $('exportModal').classList.add('hidden');
  const res = await exportJob(movie, state.project, { encoder: $('exportCodec').value, quality, smartCut: smartCutSetting(), parallel: parallelSetting() }, sinkInfo);
  $('exportModal').classList.remove('hidden');
  if (!res) return;
  showExportResult(res, name, sinkInfo && !sinkInfo.private && sinkInfo.handle ? sinkInfo.handle.name : null);
}

/**
 * Export `project` as the job 'Exporting' (`opts` as exportMovie's), to
 * `sinkInfo`'s sink, if any, and read back what went to disk. Resolves to
 * the export, or null (cancelled, or failed): what it wrote is thrown away.
 */
async function exportJob(movie, project, opts, sinkInfo) {
  const res = await runJob('Exporting', (progress, cancelled) =>
    exportMovie(state.env, movie, project, {
      ...opts,
      sink: sinkInfo ? sinkInfo.sink : null,
      cancel: cancelled,
      onProgress: (p, frames, ms, copied, sound) => progress(p, exportProgressText(frames, ms, copied, sound)),
    })
  );
  if (!res) {
    if (sinkInfo) await sinkInfo.sink.abort();
    return null;
  }
  await readBackExport(res, sinkInfo);
  return res;
}

function exportProgressText(frames, ms, copied, sound) {
  return `${frames} frames encoded${copied ? ` · ${copied} copied` : ''} · ${(frames / (ms / 1000)).toFixed(0)} fps${sound != null ? ` · the sound, re-encoded: ${Math.round(sound * 100)}%` : ''}`;
}

/** What an export did, for the dialog and the auto-fix strip. */
function exportSummary(res) {
  const total = res.frames + (res.copied || 0);
  const parts = [`${res.frames} re-encoded with ${res.encoderLabel} (${res.codec})${res.spans > 1 ? ` in ${res.spans} spans` : ''}`];
  if (res.copied) parts.push(`${res.copied} copied from the source as they are`);
  return `Exported ${total} frames in ${(res.elapsedMs / 1000).toFixed(1)} s: ${parts.join(', ')}${res.blended ? `; ${res.blended} blended` : ''}${res.softened ? `; ${res.softened} softened` : ''}.`;
}

/**
 * An export that streamed to disk is read back as a File: from private
 * storage it is what the dialog offers for download (`res.blob`); from the
 * user's own file it is only there to verify (`res.saved`).
 */
async function readBackExport(res, sinkInfo) {
  if (res.blob || !sinkInfo || !sinkInfo.handle) return;
  try {
    const file = await sinkInfo.handle.getFile();
    if (sinkInfo.private) res.blob = file;
    else res.saved = file;
  } catch (e) {
    /* no read access to the chosen file */
  }
}

function exportName(movie) {
  return movie.name.replace(/\.[^.]+$/, '') + '.unflashed.mp4';
}

// ---- project files ------------------------------------------------------------

function wireProjectMenu() {
  $('btnProject').addEventListener('click', (e) => {
    e.stopPropagation();
    toggleMenu('projectMenu');
    renderProjectNote();
  });
  $('btnProjectSave').addEventListener('click', saveProjectFile);
  $('projectInput').addEventListener('change', async () => {
    const f = $('projectInput').files[0];
    $('projectInput').value = '';
    $('projectMenu').classList.add('hidden');
    if (f) await loadProjectFile(f);
  });
}

/** What the project here holds, under the menu's buttons. */
function renderProjectNote() {
  const p = state.project;
  if (!p) return ($('projectNote').textContent = '');
  const marks = p.sections.reduce((n, s) => n + markCount(s), 0);
  $('projectNote').textContent = `Here now: ${p.sections.length} section${p.sections.length === 1 ? '' : 's'}${marks ? `, ${marks} marks` : ''}${p.scan ? ', a scan' : ''}.`;
}

/** Download the project as a file. */
async function saveProjectFile() {
  const p = state.project;
  if (!p || !state.movie) return;
  $('projectMenu').classList.add('hidden');
  await p.save();
  const blob = new Blob([projectFileText(p, state.movie)], { type: 'application/json' });
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = state.movie.name.replace(/\.[^.]+$/, '') + '.unflash.json';
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 60000);
  state.lastProjectFile = blob; // (tests read it back)
  toast(`Saved the project (${p.sections.length} section${p.sections.length === 1 ? '' : 's'}) as ${a.download}.`);
}

/** Load a project file for the open video: its sections, marks and scan replace the ones here. */
async function loadProjectFile(f) {
  const p = state.project;
  if (!p || !state.movie) return toast('Open the video the project is for, then load the project.');
  let doc;
  try {
    doc = readProjectFile(await f.text());
  } catch (e) {
    return banner(e.message);
  }
  const m = matchVideo(doc.video, state.movie);
  if (!m.ok) return banner(m.why);
  const n = doc.saved.sections.length;
  const here = p.sections.length;
  if (here && !confirm(`Replace the ${here} section${here === 1 ? '' : 's'} here (and their marks) with the ${n} in ${f.name}?`)) return;
  await stopAuto();
  if (state.job) return toast('Wait for the job under way to finish (or cancel it), then load the project.');
  if (sectionPlayer) await sectionPlayer.stop();
  closeViewer();
  const before = p.profile;
  // (the frames of the sections here are freed once no check reads them)
  await withFeeder(() => {
    for (const s of p.sections) dropCaches(s);
    p.restore(doc.saved);
  });
  state.current = null;
  state.history.clear();
  state.lastScan = null;
  state.traceNorm = null;
  if (p.profile !== before) {
    $('profileSel').value = p.profile;
    state.config = profileConfig(p.profile);
    await withFeeder(() => createFeeders());
  }
  await p.save();
  setPlayerSource('video');
  renderAll();
  updateStatus();
  toast(`Loaded ${f.name}: ${n} section${n === 1 ? '' : 's'}${p.scan ? ' and the scan' : ''}${m.same ? '' : '. It was saved for another copy of this video (its frames match)'}. Sections are prepared again when opened.`, 8000);
}

/** Show a finished export in the export dialog: what was written (and, `savedAs`, the file it went to), a download link, and the verify button. */
function showExportResult(res, name, savedAs = null) {
  const lines = [exportSummary(res) + (savedAs ? ` Saved as ${savedAs}.` : ''), ...res.warnings];
  // (as text: a warning can quote the file, a sound track's language, say)
  $('exportResult').innerHTML = lines.map((l) => `<p>${escapeHtml(l)}</p>`).join('');
  // the dialog offers this export and no earlier one (a file saved where the visitor chose, to verify only)
  offerExport(res.blob || res.saved || null, res.holds || null, res.blob ? name : null);
}

/**
 * The export the dialog offers (`blob`; null: none) to verify, and to
 * download as `name` (null: to verify only), with where its frames were
 * held (`holds`, on the video's clock: its times run that much later than
 * the video's).
 */
function offerExport(blob, holds = null, name = chosenExportName()) {
  state.exportBlob = blob;
  state.exportHolds = holds;
  const a = $('exportDownload');
  if (a.getAttribute('href')) {
    URL.revokeObjectURL(a.href);
    a.removeAttribute('href');
  }
  const download = !!blob && name !== null;
  a.classList.toggle('hidden', !download);
  if (download) {
    a.href = URL.createObjectURL(blob);
    a.download = name;
  }
  $('btnVerifyExport').disabled = !blob;
}

/** Whose an export kept on disk is: the video's name and size, as a project is found again whatever its modified time. */
function exportOwner(key) {
  return key.replace(/:[^:]*$/, '');
}

/**
 * Drop the last file's unattended run and export, object URLs included; an
 * export in private storage made from the video `keep` (exportOwner) stays
 * on disk.
 */
function forgetExport(keep = null) {
  if (state.auto && state.auto.blobUrl) URL.revokeObjectURL(state.auto.blobUrl);
  state.auto = null;
  renderAuto();
  offerExport(null);
  discardPrivateExport(null, keep);
}

/**
 * An export of this video from before the page was reloaded, kept in private
 * storage (`found`, as findPrivateExport gives it): offered again, to
 * download or verify, as if just made.
 */
function offerKeptExport(found) {
  offerExport(found.file);
  const at = new Date(found.madeAt);
  const when = at.toDateString() === new Date().toDateString() ? at.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) : at.toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' });
  $('exportResult').innerHTML = `<p>The export made at ${when} (${fmtBytes(found.file.size)}) is still here, kept in this browser's storage: download it, or verify it. Exporting again replaces it.</p>`;
}

/** Check a file the user saved (an earlier export, say): scanned like the export, with the current profile. */
async function verifySavedFile(file) {
  if (!file || busy('verify a file')) return;
  noteFileName(file.name);
  $('exportModal').classList.add('hidden');
  const v = await verifyBlob(file);
  $('exportModal').classList.remove('hidden');
  if (!v) return;
  $('exportResult').innerHTML += `<p>${escapeHtml(file.name)}: ${v.html}</p>`;
}

async function verifyExport() {
  if (!state.exportBlob) return;
  $('exportModal').classList.add('hidden');
  const v = await verifyBlob(state.exportBlob, state.exportHolds);
  $('exportModal').classList.remove('hidden');
  if (!v) return;
  $('exportResult').innerHTML += `<p>${v.html}</p>`;
}

/**
 * Re-scan an exported file with the current profile. Returns the scan, the
 * verdict as HTML for the dialog and as plain text, and the WCAG failures.
 */
async function verifyBlob(blob, holds = null) {
  const res = await runJob('Verifying the exported file', async (progress, cancelled) => {
    const m = await Movie.open(blob, wasm);
    const feeder = await makeFeeder(m.width, m.height);
    m.decodeInWorkers = decodeWorkersSetting(feeder);
    m.shrinkInWorkers = shrinkSetting();
    try {
      return await scanWithPlan({ wasm, config: state.config, feeder }, m, {
        cancel: cancelled,
        segments: scanSegments(),
        forceSegments: SEGMENTS > 0,
        makeFeeder: () => makeFeeder(m.width, m.height),
        onProgress: (p, _t, count, ms) => progress(p, `${count} frames · ${(count / (ms / 1000)).toFixed(0)} fps`),
      });
    } finally {
      feeder.det.free();
      m.close();
    }
  });
  if (!res) return null;
  state.lastVerify = res;
  const v = res.result.violations;
  const wcagBad = v.filter((x) => x.kind === 'flash' || x.kind === 'red');
  const ext = v.filter((x) => x.kind === 'extended' && counts(res.result, x));
  const pat = v.filter((x) => x.kind === 'pattern');
  // where each is in the video, and the section to fix it in: held frames
  // push the file's times later than the video's (by the export's holds, or,
  // for a file made elsewhere, the ones marked here)
  const known = Array.isArray(holds);
  const shifts = known ? holds : state.exportPlan ? state.exportPlan.holds : [];
  const where = (x, html) => {
    const w = exportToVideo(x.start, x.end, shifts);
    const moved = Math.abs(w.lo - x.start) > 0.0005 || Math.abs(w.hi - x.end) > 0.0005;
    const video = moved ? ` (${fmt(w.lo)}–${fmt(w.hi)} in the video${known ? '' : ', if it was exported with the frames held here'})` : '';
    if (!w.sec) return `${video}, which no section covers`;
    return `${video} in ${html ? `<button class="small" data-open-at="${w.sec.id}:${w.lo}:${w.hi}" data-kind="${x.kind}" title="Open the section with these frames selected">section #${w.sec.id}</button>` : `section #${w.sec.id}`}`;
  };
  const list = (xs, html, kind) => xs.slice(0, 8).map((x) => `${kind ? x.kind + ' ' : ''}${fmt(x.start)}–${fmt(x.end)}${where(x, html)}`).join(', ');
  const describe = (html) => {
    let msg = wcagBad.length ? `<b>Fails WCAG:</b> ${wcagBad.length} violation${wcagBad.length === 1 ? '' : 's'}: ` + list(wcagBad, html, true) : '<b>Passes WCAG.</b>';
    if (ext.length) msg += ` ${ext.length} extended flash${ext.length === 1 ? '' : 'es'} remain${ext.length === 1 ? 's' : ''}: ` + list(ext, html);
    if (res.result.flag_patterns) {
      if (pat.length) msg += ` ${pat.length} hazardous stripe pattern${pat.length === 1 ? '' : 's'} remain${pat.length === 1 ? 's' : ''}: ` + list(pat, html);
      else msg += ' No hazardous stripe patterns.';
    }
    return `${msg} (${res.frames} frames re-scanned in ${(res.elapsedMs / 1000).toFixed(1)} s)`;
  };
  return { res, wcagBad, html: describe(true), text: describe(false).replace(/<[^>]+>/g, '') };
}

/**
 * Where a stretch of an exported file (`a`–`b`, seconds on its clock) is in
 * the video: its times less the seconds that the frames held before them
 * added (`holds`, sorted, on the video's clock), and the section over it.
 */
function exportToVideo(a, b, holds) {
  const back = (t) => {
    let shift = 0;
    for (const h of holds) {
      if (t < h.at + shift) break;
      // (while a frame is held: that frame, the one just before h.at)
      if (t < h.at + shift + h.seconds) return h.at - 0.001;
      shift += h.seconds;
    }
    return t - shift;
  };
  const lo = back(a);
  const hi = back(b);
  const sec = state.project ? state.project.sectionsSorted().find((s) => s.start <= hi && s.end > lo) || null : null;
  return { lo, hi, sec };
}

// ---- auto-fix: open a file, and the scan, the fixes, the export and its check follow -----

/**
 * The switch in the header starts as `?auto=0` / `?auto=1` says, else as it
 * was last left, else off: what auto-fix makes is a starting point that
 * hand editing beats, so it is something to ask for.
 */
function initialAutoSetting() {
  const forced = onOff('auto');
  return forced !== null ? forced : stored('unflash:auto') === '1';
}

/** Whether a file is scanned as soon as it is opened: always, except with `?auto=0` (nothing automatic at all). */
function autoScanEnabled() {
  return onOff('auto') !== false;
}

/** Whether opening a file (or changing the profile) starts the unattended run: the switch decides. */
function autoEnabled() {
  return $('autoToggle').checked;
}

/** The unattended run's steps, in order, as its strip names them. */
const AUTO_STEPS = ['scan', 'fix', 'export', 'verify'];

function autoStep(auto, name, status, text = '') {
  auto.steps[name] = { status, text };
  if (state.auto === auto) renderAuto();
}

function renderAuto() {
  const a = state.auto;
  const box = $('auto');
  if (!a) {
    box.classList.add('hidden');
    return;
  }
  box.classList.remove('hidden');
  const steps = $('autoSteps');
  steps.innerHTML = '';
  for (const key of AUTO_STEPS) {
    const s = a.steps[key] || { status: 'pending', text: '' };
    const el = document.createElement('span');
    el.className = `auto-step ${s.status}`;
    el.dataset.step = key;
    const b = document.createElement('b');
    b.textContent = key;
    el.appendChild(b);
    if (s.text) {
      const t = document.createElement('span');
      t.textContent = s.text;
      el.appendChild(t);
    }
    steps.appendChild(el);
  }
  $('autoSummary').textContent = a.summary || '';
  $('btnAutoStop').classList.toggle('hidden', !a.running);
  $('btnAutoRerun').classList.toggle('hidden', a.running);
  $('autoDownload').classList.toggle('hidden', !a.blobUrl);
}

/** One line about a stored scan. */
function describeScan(s) {
  if (!s) return '';
  const n = s.counted != null ? s.counted : s.violations.length;
  const np = s.patterns || 0;
  if (!n) return `nothing found in ${s.frames} frames`;
  return `${n} violation${n === 1 ? '' : 's'}${np ? ` (${np} stripe pattern${np === 1 ? '' : 's'})` : ''} in ${s.frames} frames`;
}

/** Stop the unattended run (cancelling the job it is on) and wait for it to end. */
async function stopAuto() {
  const a = state.auto;
  if (!a || !a.running) return;
  a.stopped = true;
  // (the run asks `stopped` before each job it starts: only the one under way needs cancelling)
  if (state.job) state.job.cancelled = true;
  await a.ended;
}

/**
 * The whole job without a click: scan the file (or reuse this profile's scan
 * from a previous visit), prepare every section and make it pass, export the
 * result and re-scan the export. Every stage is an ordinary job, so the job
 * bar shows its progress and "stop" cancels it. Stops short, saying why,
 * when a section still fails; that section is opened for editing by hand.
 */
async function autopilot({ rescan = false } = {}) {
  if (!state.movie || !state.env || !state.project || !state.decode.supported) return;
  if (state.job || (state.auto && state.auto.running)) return;
  const movie = state.movie;
  const project = state.project;
  if (state.auto && state.auto.blobUrl) URL.revokeObjectURL(state.auto.blobUrl);
  const auto = { running: true, stopped: false, steps: {}, summary: '', blobUrl: null, fileName: null };
  // (settled once the run is over: stopAuto waits on it)
  auto.ended = new Promise((r) => (auto.end = r));
  state.auto = auto;
  renderAuto();
  const halted = () => auto.stopped || state.movie !== movie;
  const bail = (step, why) => {
    autoStep(auto, step, halted() ? 'stopped' : 'failed', halted() ? '' : why);
    auto.summary = halted() ? 'Stopped. The buttons do each step by hand; "run again" starts over.' : why;
  };
  try {
    // 1. scan
    const sig = wasm.config_signature(state.config);
    const prior = project.scan;
    if (!rescan && prior && prior.sig === sig && prior.profile === project.profile && (prior.safe || project.sections.length)) {
      autoStep(auto, 'scan', 'done', `${describeScan(prior)} (${state.lastScan ? 'scanned when the file was opened' : 'from the last visit'})`);
    } else {
      autoStep(auto, 'scan', 'running', 'decoding every frame');
      const res = await scan();
      if (!res || halted()) return bail('scan', 'The scan did not finish.');
      autoStep(auto, 'scan', 'done', describeScan(project.scan));
    }
    if (project.scan.safe && !project.sections.some(hasMarks)) {
      autoStep(auto, 'fix', 'skipped', 'nothing to fix');
      autoStep(auto, 'export', 'skipped', 'the file passes as it is');
      autoStep(auto, 'verify', 'skipped');
      auto.summary = 'Nothing to fix: the file passes as it is.';
      return;
    }
    // 2. fix every section
    const secs = project.sectionsSorted();
    const notes = [];
    const failing = [];
    const partial = [];
    for (const sec of secs) {
      if (halted()) return bail('fix', '');
      autoStep(auto, 'fix', 'running', `section #${sec.id} of ${secs.length}${notes.length ? ' · ' + notes.join(' · ') : ''}`);
      const r = await autoFixSection(sec, auto);
      if (!r) return bail('fix', `Section #${sec.id} could not be fixed automatically.`);
      notes.push(`#${sec.id}: ${r.note}`);
      if (!r.wcagSafe) failing.push(sec);
      else if (!r.safe) partial.push(sec);
      renderAll();
    }
    await project.save();
    updateStatus();
    if (failing.length) {
      autoStep(auto, 'fix', 'failed', notes.join(' · '));
      autoStep(auto, 'export', 'skipped', 'not while a section fails');
      autoStep(auto, 'verify', 'skipped');
      const ids = failing.map((s) => '#' + s.id).join(', ');
      auto.summary = `Section${failing.length === 1 ? '' : 's'} ${ids} still fail${failing.length === 1 ? 's' : ''} WCAG: edit ${failing.length === 1 ? 'it' : 'them'} by hand, then Export.`;
      openSection(failing[0].id);
      return;
    }
    autoStep(auto, 'fix', 'done', notes.join(' · '));
    if (halted()) return bail('export', '');
    // 3. export
    const quality = +$('exportQuality').value;
    const cands = await encoderCandidates(movie.width, movie.height, movie.fps, quality);
    if (!cands.length) {
      autoStep(auto, 'export', 'failed', 'this browser has no WebCodecs video encoder');
      autoStep(auto, 'verify', 'skipped');
      auto.summary = 'The sections are fixed, but this browser has no video encoder to export with.';
      return;
    }
    // to disk when the browser has private storage; in memory otherwise, and
    // only when it will fit
    const plan = await exportPlan(state.env, movie, project, { codec: cands[0].config.codec, smartCut: smartCutSetting(), parallel: parallelSetting() });
    const need = estimateExportBytes(movie, quality, plan);
    const sinkInfo = await privateFileSink(need, exportOwner(project.key));
    if (!sinkInfo && need > memoryExportLimit()) {
      autoStep(auto, 'export', 'skipped', `about ${fmtBytes(need)}: more than this browser can build in memory`);
      autoStep(auto, 'verify', 'skipped');
      auto.summary = `The sections are fixed. The export would be about ${fmtBytes(need)}, more than this browser can hold in memory: ${window.showSaveFilePicker ? 'use Export… to write it to a file of your choice' : 'export from a browser with a save dialog or private storage'}.`;
      return;
    }
    // (stopped while the export was planned: no job is under way for the stop to cancel)
    if (halted()) {
      if (sinkInfo) await sinkInfo.sink.abort();
      return bail('export', '');
    }
    autoStep(auto, 'export', 'running', `${plan.mode === 'smart' ? `re-encoding ${plan.spans} span${plan.spans === 1 ? '' : 's'} with ${cands[0].label}, copying ${plan.copied} frames` : `encoding with ${cands[0].label}`}${sinkInfo ? ' to private storage on disk' : ''}`);
    const res = await exportJob(movie, project, { encoder: cands[0].label, quality, plan }, sinkInfo);
    if (!res || halted()) return bail('export', 'The export did not finish.');
    if (!res.blob) return bail('export', 'The exported file could not be read back.');
    auto.fileName = exportName(movie);
    auto.blobUrl = URL.createObjectURL(res.blob);
    const dl = $('autoDownload');
    dl.href = auto.blobUrl;
    dl.download = auto.fileName;
    showExportResult(res, auto.fileName);
    autoStep(auto, 'export', 'done', `${exportSummary(res)}${res.warnings.length ? ' · ' + res.warnings.join(' ') : ''}`);
    // 4. verify
    if (halted()) return bail('verify', '');
    autoStep(auto, 'verify', 'running', 're-scanning the exported file');
    const v = await verifyBlob(res.blob, res.holds);
    if (!v || halted()) return bail('verify', 'The check of the exported file did not finish.');
    autoStep(auto, 'verify', v.wcagBad.length ? 'failed' : 'done', v.text);
    const ids = partial.map((s) => '#' + s.id).join(', ');
    const left = partial.length ? ` Section${partial.length === 1 ? '' : 's'} ${ids} pass${partial.length === 1 ? 'es' : ''} WCAG but keep${partial.length === 1 ? 's' : ''} something the profile flags.` : '';
    auto.summary = v.wcagBad.length ? `The exported file still fails WCAG. ${v.text}` : `Done: the fixed video is ready to download. ${v.text}${left}`;
  } catch (e) {
    console.error(e);
    const step = AUTO_STEPS.find((k) => auto.steps[k] && auto.steps[k].status === 'running') || 'scan';
    autoStep(auto, step, 'failed', e && e.message ? e.message : String(e));
    auto.summary = 'Auto-fix stopped on an error; the buttons do each step by hand.';
  } finally {
    auto.running = false;
    auto.end();
    if (state.auto === auto) renderAuto();
    // the run was one wait: its alert comes now (an export that still fails fails it, though no job failed)
    if (chain.active && auto.steps.verify && auto.steps.verify.status === 'failed') chain.ok = false;
    chainEnd('ok', '');
  }
}

/**
 * Prepare one section and make it pass: soften its stripes, then take the
 * flashing out by removing as few frames as it can (the fewest removals,
 * which end by letting frames back into long removed stretches at the
 * rate that cannot fail), else keep dark, then keep light, and when that is
 * not enough, by thinning to a rate that cannot flash fast enough to fail.
 * Returns what was done and where the section stands, or null when a step
 * failed or the run was stopped.
 */
async function autoFixSection(sec, auto) {
  const env = state.env;
  const project = state.project;
  const ctxS = wasm.context_seconds(state.config);
  const hadMarks = hasMarks(sec);
  if (!sec.prepared && !(await doPrepare(sec))) return null;
  const check = () => (auto.stopped ? null : checkJob(sec));
  const standing = (note) => ({ note, safe: !!(sec.check && sec.check.safe), wcagSafe: !!(sec.check && sec.check.wcag_safe) });
  // a check's verdict counts what runs past the section's end too
  const violations = (x) => x.violations || x.inside || [];
  let c = await check();
  if (!c) return null;
  if (c.safe) return standing(hadMarks ? 'passes with the marks from before' : 'already passes');
  const did = [];
  // stripes cannot be removed a frame at a time: soften them
  if (violations(c).some((v) => v.kind === 'pattern' && counts(c, v)) && !sec.soften && softenPlan(sec)) {
    pushHistory(sec);
    sec.soften = true;
    project.invalidateNeighbours(sec, ctxS);
    did.push('softened the stripes');
    c = await check();
    if (!c) return null;
    if (c.safe) return standing(did.join(' · '));
  }
  // flashing: the gentle suggesters first, the guaranteed one last
  const flashing = violations(c).some((v) => v.kind !== 'pattern' && counts(c, v));
  if (flashing) {
    const rounds = (progress) => (r) => progress(Math.min(0.95, 0.2 + r * 0.06), `check ${r + 1}`);
    const tries = [
      ['fewest removals', (progress, cancel) => suggestEdits(env, project, sec, 'fewest', null, { cancel, onProgress: rounds(progress) })],
      ['keep dark', (progress, cancel) => suggestEdits(env, project, sec, 'dark', null, { cancel, onProgress: rounds(progress) })],
      ['keep light', (progress, cancel) => suggestEdits(env, project, sec, 'light', null, { cancel, onProgress: rounds(progress) })],
      ['reduce the frame rate', (progress, cancel) => searchFrameRate(env, project, sec, null, { sourceFps: state.movie.fps, cancel, onProgress: (p, r) => progress(p, `checking ${r} pictures/s`) })],
    ];
    // (the marks the suggestions are made from: changed meanwhile, as suggestJob, none is taken)
    const marks = markSnapshot(sec);
    let last = null;
    for (const [label, run] of tries) {
      if (auto.stopped) return null;
      const res = await runJob(`Fixing section #${sec.id}: ${label}`, (progress, cancelled) => run(progress, cancelled));
      if (!res) return null;
      last = [label, res];
      if (res.safe) break;
    }
    const [label, res] = last;
    if (markSnapshot(sec) !== marks) {
      toast(`Section #${sec.id}: its marks changed while auto-fix worked on it, so its suggestion was not applied.`, 8000);
      return null;
    }
    applySuggestion(sec, res, null);
    did.push(`${label}: ${res.note}`);
    if (!sec.check && !(await check())) return null;
  }
  await project.save();
  const kinds = remainingKinds(sec.check);
  return standing(did.join(' · ') || (kinds.length ? `no automatic fix for the ${kinds.join(' and ')}` : 'no automatic fix'));
}

// a small surface for tests and debugging
window.__unflash = {
  get state() {
    return state;
  },
  get lastScan() {
    return state.lastScan;
  },
  get auto() {
    return state.auto;
  },
  currentSection,
  softenPlan,
  openClip,
  autopilot,
  stopAuto,
  profile,
  get sectionPlayer() {
    return sectionPlayer;
  },
  sectionSound,
  loadProjectFile,
  tours: tourGuide,
  get lastProjectFile() {
    return state.lastProjectFile;
  },
  get changes() {
    return state.changes;
  },
  debugReport: makeDebugReport,
  exportToVideo,
  checkForUpdate,
  build: BUILD,
  setPlayerSource,
  playSection,
  undo,
  redo,
  /**
   * Prepare a copy of section `id` in `spans` spans and describe what came
   * out (frame counts, times, a checksum of the cached pictures), leaving
   * the section itself alone (tests: a prepare in spans must match one in
   * a single pass).
   */
  async prepareDigest(id, spans) {
    const sec = state.project.sections.find((s) => s.id === id);
    const copy = { id: sec.id, start: sec.start, end: sec.end, edits: {} };
    const t0 = performance.now();
    await withFeeder(() => prepareSection(state.env, state.movie, copy, { spans, moreFeeders: spareFeeders }));
    const ms = performance.now() - t0;
    const sum = (cache) => {
      let h = 0;
      for (let i = 0; i < cache.len(); i++) {
        const f = cache.frame(i);
        for (let k = 0; k < f.length; k += 7) h = (h * 31 + f[k] + k) >>> 0;
      }
      return h;
    };
    const out = { ms, spans: copy.preparedSpans, frames: copy.cache.len(), lead: copy.ctx.lead.len(), tail: copy.ctx.tail.len(), pts: copy.pts, leadPts: copy.ctx.leadPts, tailPts: copy.ctx.tailPts, pattern: copy.pattern.counts, hash: [sum(copy.cache), sum(copy.ctx.lead), sum(copy.ctx.tail)] };
    dropCaches(copy);
    return out;
  },
  /**
   * Prepare a copy of section `id` through the decode workers twice, the
   * pictures made small there and made small by the detector, and compare
   * the cached pictures (tests: the workers' shrink is the GPU's, to a code).
   */
  async compareShrink(id) {
    const sec = state.project.sections.find((s) => s.id === id);
    const m = state.movie;
    const was = { workers: m.decodeInWorkers, shrink: m.shrinkInWorkers };
    const prepare = async (shrink) => {
      const copy = { id: sec.id, start: sec.start, end: sec.end, edits: {} };
      m.decodeInWorkers = true;
      m.shrinkInWorkers = shrink;
      try {
        await withFeeder(() => prepareSection(state.env, m, copy, { spans: 1 }));
      } finally {
        m.decodeInWorkers = was.workers;
        m.shrinkInWorkers = was.shrink;
      }
      return copy;
    };
    const big = await prepare(false);
    const bigRoute = state.env.feeder.routeDetail;
    const small = await prepare(true);
    const smallRoute = state.env.feeder.routeDetail;
    let n = 0;
    let differ = 0;
    let max = 0;
    for (let i = 0; i < Math.min(big.cache.len(), small.cache.len()); i++) {
      const a = big.cache.frame(i);
      const b = small.cache.frame(i);
      for (let k = 0; k < a.length; k++) {
        if ((k & 3) === 3) continue;
        const d = Math.abs(a[k] - b[k]);
        n++;
        if (d) differ++;
        if (d > max) max = d;
      }
    }
    const out = { frames: [big.cache.len(), small.cache.len()], n, differ, max, routes: [bigRoute, smallRoute] };
    dropCaches(big);
    dropCaches(small);
    return out;
  },
  /**
   * Show the frame at `t` in the (paused) player and have the live monitor
   * watch it: after the player's `seeked`, the picture waits for a free
   * detector slot instead of being skipped as it is while playing, and its
   * result is in before this resolves, with the verdict it gives. Tests step
   * through a clip this way, so that every frame is seen however slow the
   * GPU is.
   */
  async liveStep(t) {
    const player = $('player');
    const feeder = state.liveFeeder;
    if (!feeder || !state.live.on || state.live.fromScan) throw new Error('the live monitor is not detecting');
    player.pause();
    await new Promise((resolve) => {
      player.addEventListener('seeked', resolve, { once: true });
      player.currentTime = t;
    });
    await feeder.videoElement(player, player.currentTime, false);
    await feeder.drain();
    state.live.lastCheck = 0;
    drainLive();
    return $('liveVerdict').textContent;
  },
  /** What the last scan had found as it finished each chunk and each early look, and when (tests). */
  get partials() {
    return state.partials || [];
  },
  /** Change the finish-alert settings for this visit (tests). */
  setAlerts(s) {
    Object.assign(alertSettings, s);
  },
  get lastAlert() {
    return state.lastAlert || null;
  },
};

boot().catch((e) => {
  console.error(e);
  banner(`Unflash could not start: ${e.message || e}`);
});
