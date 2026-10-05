// What the guided tour says, and when it comes. A first visit gets the
// getting-started tour in three parts, each the first time its part of the
// app is on screen: the start page, then the first video, then the first
// section. Coming back after an update, each change that brought something
// to see gets a short tour of its own (CHANGELOG.md names it:
// `<!-- HH:MM tour:id -->`), again once its part of the app is on screen.
// A tour starts only when nothing else is going on (no job running, no
// dialog open, no click or key in the last moment), each one once; the
// guide has the whole tour again, and What's new a "show me" for each.

import { Tour, stepShowable } from './tour.js';
import { stored, store } from './storage.js';

const GETTING_STARTED = ['start', 'video', 'section'];

/** The element a part of the screen lives in (its group, its label). */
const around = (sel, up) => () => {
  const el = document.querySelector(sel);
  return el && el.closest(up);
};

/** The tours: where each can be shown (`context`), and its steps. */
export const TOURS = {
  start: {
    label: 'Getting started',
    context: 'start',
    steps: [
      {
        title: 'Welcome to Unflash',
        html: 'Unflash finds the flashing and the stripe patterns in a video that can trigger seizures, and helps you take them out without wrecking the footage. It all happens in this browser: nothing is uploaded.<br><br>This tour takes a minute. <b>Esc</b> ends it, and the guide has it again whenever you want it.',
      },
      { target: 'label.filebtn.primary', title: 'Open a video', html: 'Start here: open a video from your disk (MP4, MOV, MKV, WebM or a transport stream: .ts, .m2ts, .mts), or drop one anywhere on the page. It is scanned as soon as it opens, every frame of it.' },
      { target: 'table.clips', placement: 'top', title: 'Or try a test clip', html: 'No video to hand? Each of these short clips has a known problem in it. <b>open</b> loads one straight in.' },
      { target: '#profileSel', title: 'What counts as a problem', html: 'The default flags WCAG failures, and also extended flashes and stripe patterns: WCAG lets those through, but they still affect some viewers. <i>Exact WCAG only</i> and <i>Stricter than WCAG</i> are the other two.' },
      { target: around('#autoToggle', 'label'), title: 'Auto-fix', html: 'Tick this and a video is fixed unattended as soon as it opens: scanned, every problem fixed, exported and checked. Treat what it makes as a starting point: fixing by hand gives better results.' },
      { target: '#btnHome', title: 'The guide', html: "Everything explained: with a video open, every part of the screen, each with a <b>show me</b>, and the keys; and this tour again. <b>What's new</b>, beside it, lists every change, and anything new gets a short tour of its own." },
    ],
  },
  video: {
    label: 'Your video',
    context: 'video',
    steps: [
      { target: '#timelineWrap', title: 'The whole video at a glance', html: 'What the scan found, along the video: orange for general flashes, magenta for red ones, teal for stripe patterns. The numbered boxes are sections. Drag across an empty stretch to make a section of your own.' },
      { target: '#sectionList', placement: 'right', title: 'Sections', html: 'One around each problem the scan found: open one to fix it. <b>Whole video</b>, at the top, brings back the whole video.' },
      { target: '.chart-box', title: 'The flashing, charted', html: 'How much of the picture flashes, around the playhead: the bars are flashing faster than the limit (the dashed line), the blue line flashing at the limit rate, which is what extended flashes are made of. Point at it for the numbers; click it to play from there.' },
      { target: '#playerBox', title: 'The player', html: 'Dimmed and small to start with (the switches above it change that). Tick <b>live monitor</b> in the header for a meter over the picture: how much of it is flashing, frame by frame.' },
      { target: '#btnProject', title: 'Your work is kept', html: 'Sections and marks are kept in this browser as you work, and come back when you open the same video again. <b>Project…</b> saves them to a file, to keep or to carry to another computer.' },
      { target: '#btnExport', title: 'Export', html: 'Once the sections pass, <b>Export…</b> writes the fixed video (only the stretches around the sections you marked are re-encoded) and <b>Verify</b> scans what it wrote.' },
    ],
  },
  section: {
    label: 'Fixing a section',
    context: 'section',
    steps: [
      { target: '#wsVerdict', title: 'Does it pass?', html: 'The section is checked the moment anything in it changes, with the video either side of it, exactly as the export will have it.' },
      { target: '#wsFindings', title: 'What still fails', html: 'Each problem that is left, with its frames and times and how to fix it. <b>select these frames</b> picks them out below.' },
      { target: '#frameGrid', placement: 'top', title: 'Every frame', html: 'A double-click shows a frame at full size, decoded from the file (to read a subtitle); S to XL make the thumbnails bigger. Click a frame to select it (shift-click for a run). Then <b>R</b> removes it (the frame before shows in its place), <b>F</b> the same with the frame after, <b>E</b> holds it for a second, <b>B</b> blends it with the frames either side, <b>K</b> keeps it out of every suggestion. The same key again takes the mark off; <b>Ctrl+Z</b> undoes.' },
      { target: around('#btnSuggestLight', '.group'), title: 'Suggestions', html: 'A first pass to work from. They choose by brightness alone, so look at what they did: they can take out a line of subtitles or an image that matters. Tick <b>selection only</b> to have them work on the frames you selected.' },
      { target: '#previewBar', title: 'Watch it', html: 'Plays the section with your marks, exactly as the export renders it; switch the player to <i>original</i> to compare. The sound is off to start with.' },
    ],
  },
  // new things, by the change that brought them
  'whole-video': {
    label: 'New',
    context: 'video',
    steps: [
      { target: '.sec-whole', placement: 'right', title: 'Back to the whole video', html: '<b>Whole video</b> brings back the whole video, with its flashing charted, under the limit too.' },
      { target: '#chartSpan', title: 'The whole video, charted', html: '10 s, 30 s, 2 min or all of it, around the playhead. Point at the chart for the numbers; click it to play from there.' },
    ],
  },
  'project-files': {
    label: 'New',
    context: 'video',
    steps: [{ target: '#btnProject', title: 'Project files', html: 'Save the sections, marks and scan to a file, and load them back: on another computer or browser, or after this site’s data is cleared.' }],
  },
  findings: {
    label: 'New',
    context: 'section',
    steps: [
      { target: '#wsFindings', title: 'What still fails, and how to fix it', html: 'Each problem left in the section, extended flashes too, with its frames and times, a button to select them, and what fixes it.' },
      { target: '#btnSelectUnsafe', title: 'Extended flashes, selected', html: '<b>select unsafe frames</b> now takes in extended flashes and scrolls to them. Frames inside an extended flash have a blue outline.' },
    ],
  },
  'section-sound': {
    label: 'New',
    context: 'section',
    steps: [{ target: '#btnPreviewSound', title: 'Sound in the section player', html: 'Off to start with. Held frames are silent while they wait, as they are in the export. It plays at 1× only, and when there is no sound to be had the button says so, and why.' }],
  },
  tours: {
    label: 'New',
    context: 'any',
    steps: [{ target: '#btnHome', title: 'A guided tour', html: 'Like this one. The guide has the whole tour whenever you want it, and anything new gets a short tour of its own the first time you are where it is.' }],
  },
};

/**
 * The screen, part by part: the guide's list when a video is open, each
 * part with a "show me" that lights it up with its card. `short` is the
 * line in the list and the card both.
 */
export const PARTS = [
  {
    group: 'Along the top',
    context: 'video',
    parts: [
      { id: 'open', target: 'label.filebtn.primary', name: 'Open video…', short: 'a video from your disk (MP4, MOV, MKV, WebM or a transport stream: .ts, .m2ts, .mts), or drop one anywhere on the page.' },
      { id: 'scan', target: '#btnScan', name: 'Scan', short: 'every frame through the detector; a numbered section goes round each problem it finds.' },
      { id: 'profile', target: '#profileSel', name: 'Profile', short: 'what counts as a problem: WCAG failures and extended flashes and stripes (the default), <i>Exact WCAG only</i>, or <i>Stricter than WCAG</i>.' },
      { id: 'auto', target: around('#autoToggle', 'label'), name: 'Auto-fix', short: 'ticked, a video is fixed unattended as it opens: scanned, every problem fixed, exported and verified.' },
      { id: 'live', target: around('#liveToggle', 'label'), name: 'Live monitor', short: 'a meter over the player: how much of the picture is flashing, frame by frame.' },
      { id: 'export', target: '#btnExport', name: 'Export…', short: 'writes the fixed video (only the stretches round the sections are re-encoded), and verifies it.' },
      { id: 'project', target: '#btnProject', name: 'Project…', short: 'the sections and marks to a file and back. They are kept in this browser as you work, too.' },
      { id: 'alerts', target: '#btnAlerts', name: 'Alerts', short: 'a sound or a notification when a long job ends.' },
      { id: 'debug', target: '#btnDebug', name: 'debug info', short: 'at the bottom: this browser, the video and each job, as text to paste when you ask for help.' },
    ],
  },
  {
    group: 'The whole video',
    context: 'video',
    parts: [
      { id: 'timeline', target: '#timelineWrap', name: 'Timeline', short: 'what the scan found along the video (orange: flashes, magenta: red flashes, teal: stripes) and the numbered sections. Click to go there; drag across an empty stretch, or type times at the end, to add a section.' },
      { id: 'sections', target: '#sections', placement: 'right', name: 'Sections', short: 'the whole video, then each section and where it stands: unsafe, passes, extended flash, stripes, re-check. Prepare, check or delete them all at once.' },
      { id: 'chart', target: '.chart-box', name: 'Chart', short: 'how much of the picture flashes around the playhead: bars above the limit (the dashed line), the blue line at the limit rate, the brightness, stripes. 10 s to all of it; point at it for the numbers, click to play from there.' },
      { id: 'player', target: '#playerBox', name: 'Player', short: 'a section with your marks, as the export will have it (the menu above it: the original, or the whole video), dimmed and small to start with; the sound is off until you turn it on.' },
    ],
  },
  {
    group: 'A section',
    context: 'section',
    parts: [
      { id: 'verdict', target: '#wsVerdict', name: 'Verdict', short: 'passes, fails, or passes WCAG with extended flashes or stripes left; checked again at every change, with the video either side of it.' },
      { id: 'range', target: '.range-edit', name: 'Range', short: "the section's start and end (apply range); beside it, re-prepare and delete." },
      { id: 'findings', target: '#wsFindings', name: 'What still fails', short: 'each problem left, its frames and times, a button to select them, and what fixes it.' },
      { id: 'suggest', target: around('#btnSuggestLight', '.group'), name: 'Suggest', short: 'a first pass: keep light, keep dark, fewest removals, blend frames, reduce FPS; with selection only, on the selected frames alone. Look at what it did.' },
      { id: 'check', target: around('#btnCheck', '.group'), name: 'Check', short: 'auto-check checks every change as you make it (Check safety does it by hand); beside it, undo, redo and clear all marks.' },
      { id: 'grid', target: '#frameGrid', placement: 'top', name: 'Frames', short: 'every frame: click one, shift-click a run, ctrl-click one more, Esc to clear; S to XL for bigger thumbnails.' },
      { id: 'marks', target: '.grid-actions', name: 'Marks', short: 'R removes the selected frames (the frame before shows in their place), F the same with the frame after, E holds a frame a second, K keeps it from the suggestions, B blends it with its neighbours; the same key again takes the mark off, U every mark.' },
      { id: 'legend', target: '.legend', name: 'The colours', short: "what the grid's outlines and badges mean: removed, held, keep, blended, in a flash, in a red flash, in an extended flash, stripes, softened, selected." },
      { id: 'viewer', target: '#btnViewFrame', name: 'A frame at full size', short: 'double-click a frame, or Z: decoded from the file, to read a subtitle; ← → step through, and the mark keys work there too.' },
      { id: 'soften', target: '#softenWrap', name: 'Soften stripes', short: 'there when the section has stripes: blurs them just enough, and only where they are.' },
      { id: 'blend', target: '#blendWrap', name: 'Blend strength', short: 'there when frames are blended: how far.' },
    ],
  },
];
for (const g of PARTS) {
  for (const p of g.parts) TOURS[`part:${p.id}`] = { label: g.group, context: g.context, steps: [{ target: p.target, placement: p.placement, title: p.name, html: p.short.replace(/^./, (c) => c.toUpperCase()) }] };
}

const KEY = 'unflash:tours';

function load() {
  try {
    const v = JSON.parse(stored(KEY) || '{}');
    return { plan: Array.isArray(v.plan) ? v.plan : [], done: v.done && typeof v.done === 'object' ? v.done : {} };
  } catch (e) {
    // (what is kept there is not JSON, or not an object)
    return { plan: [], done: {} };
  }
}

/** Keep where the tours are (with storage blocked, they come again next time). */
function save(s) {
  store(KEY, JSON.stringify(s));
}

/**
 * When the tours come. `where(byHand)` says which contexts are on screen
 * ({ any, start, video, section }; `byHand`: once the page is ready for a
 * tour started by hand), `quiet()` whether nothing is going on, and
 * `ready(context)` gets the page ready for a tour started by hand (the
 * guide put away over a video, say).
 */
export class TourGuide {
  constructor({ where, quiet, ready = () => {}, auto = true }) {
    this.where = where;
    this.quiet = quiet;
    this.ready = ready;
    this.auto = auto;
    this.state = load();
    this.pending = [];
    this.tour = null;
    this.timer = null;
  }

  /** Whether a tour is on screen. */
  get showing() {
    return !!(this.tour && this.tour.open);
  }

  /**
   * What this visit should be shown: `firstVisit` starts the
   * getting-started tour; `tours` are the ids of the tours of the changes
   * since the last visit.
   */
  plan({ firstVisit = false, tours = [] } = {}) {
    if (firstVisit && !this.state.plan.length) {
      this.state.plan = GETTING_STARTED.slice();
      save(this.state);
    }
    // the getting-started parts not yet seen, then the new things
    const ids = [...this.state.plan, ...tours].filter((id, k, a) => TOURS[id] && !this.state.done[id] && a.indexOf(id) === k);
    this.pending = ids;
    if (this.auto && this.pending.length && !this.timer) this.timer = setInterval(() => this.poll(), 700);
  }

  /** Start whichever waiting tours can be shown now (all those of one place, as one tour). */
  poll() {
    if (!this.pending.length) {
      clearInterval(this.timer);
      this.timer = null;
      return;
    }
    if (this.showing || !this.quiet()) return;
    const on = this.where();
    const now = this.pending.filter((id) => on[TOURS[id].context] && TOURS[id].steps.some(stepShowable));
    if (now.length) this.show(now);
  }

  /** Show tours `ids` now, as one (by hand: the guide's button, a "show me"). */
  show(ids, { byHand = false } = {}) {
    if (this.showing) this.tour.end('skipped');
    if (byHand) this.ready(TOURS[ids[0]].context);
    const steps = ids.flatMap((id) => TOURS[id].steps.map((s) => ({ ...s, label: TOURS[id].label })));
    const tour = new Tour(steps, {
      onEnd: (how) => {
        if (this.tour === tour) this.tour = null;
        // shown, or turned down: either way it has had its turn (a tour
        // with nothing on screen waits for its place)
        if (how !== 'empty') this.markDone(ids);
      },
    });
    this.tour = tour;
    return tour.start();
  }

  /**
   * By hand: tour `id` now if its place is on screen (true), else it waits
   * for its place (false).
   */
  request(id) {
    const t = TOURS[id];
    if (!t) return false;
    const on = this.where(true);
    if (on[t.context] && t.steps.some(stepShowable)) return this.show([id], { byHand: true });
    if (!this.pending.includes(id)) this.pending.push(id);
    delete this.state.done[id];
    if (!this.timer) this.timer = setInterval(() => this.poll(), 700);
    return false;
  }

  /** By hand, from the guide: part `id` of the screen lit up now, if it is on screen (true). */
  showPart(id) {
    const t = TOURS[`part:${id}`];
    if (!t || !this.where(true)[t.context] || !t.steps.some(stepShowable)) return false;
    this.show([`part:${id}`], { byHand: true });
    return true;
  }

  /** The getting-started part for where the page is now, by hand. */
  gettingStarted() {
    const on = this.where(true);
    const id = on.section ? 'section' : on.video ? 'video' : 'start';
    return this.show([id], { byHand: true });
  }

  markDone(ids) {
    const at = Date.now();
    for (const id of ids) this.state.done[id] = at;
    this.pending = this.pending.filter((id) => !ids.includes(id));
    save(this.state);
  }
}
