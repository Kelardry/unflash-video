// The section player: one section of the source played on a canvas, either
// with its marks applied exactly as the export renders them (removed frames
// showing their stand-in, held frames held, blended frames blended,
// softened frames blurred) or as it is, paced to the frames' own times. It
// decodes the section afresh for every pass, so it plays at the file's full
// resolution and needs nothing prepared beyond the section's frame times;
// the marks are read when a pass starts, so an edit shows from the next
// pass (or at once, when the caller restarts it). A SectionSound, when
// given, plays the section's sound along with it (see sound.js).

import { walkEdited, sectionRenderPlan } from './export.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
/** Longest side of the canvas: bigger sources are drawn scaled down. */
const MAX_SIDE = 1920;

export class SectionPlayer {
  /**
   * `onFrame(info, t, plan)` follows every picture drawn (info as walkEdited
   * gives it); `onState(state, detail)` every change of 'playing', 'paused',
   * 'ended', 'stopped' or 'error'. `sound` (a SectionSound) is kept in step
   * with the clock.
   */
  constructor(canvas, { onFrame = null, onState = null, sound = null } = {}) {
    this.canvas = canvas;
    this.onFrame = onFrame;
    this.onState = onState;
    this.sound = sound;
    this.run = null;
    this.speed = 1;
  }

  /** A pass is under way (playing or paused). */
  get active() {
    return !!(this.run && !this.run.done);
  }

  get paused() {
    return !!(this.run && !this.run.done && this.run.paused);
  }

  /** The slot on screen, or -1. */
  get slot() {
    return this.run ? this.run.slot : -1;
  }

  /**
   * Play prepared section `sec` from slot `fromSlot`, `edited` or original.
   * `loop()` is asked at the end of each pass. With `once`, draw the first
   * picture and stop (a poster). Returns once playback has been set going.
   */
  async play({ env, movie, sec, edited = true, fromSlot = 0, loop = () => false, once = false }) {
    // two calls close together: each waits out whatever pass is under way
    while (this.run && !this.run.done) await this.stop();
    const run = { cancelled: false, paused: false, wake: null, base: null, pausedAt: null, done: false, slot: -1, t: null, once };
    this.run = run;
    if (!once) this._state('playing');
    run.promise = (async () => {
      let from = fromSlot;
      try {
        for (;;) {
          await this._pass(run, { env, movie, sec, edited, fromSlot: from });
          if (run.cancelled || once || !loop()) break;
          from = 0;
          run.base = null; // the next pass sets the clock (and the sound) going at its first frame
        }
      } catch (e) {
        if (!run.cancelled) {
          console.error(e);
          this._state('error', e);
        }
      } finally {
        run.done = true;
        if (this.run === run && !once) this._state(run.cancelled ? 'stopped' : 'ended');
      }
    })();
  }

  /** Stop the pass under way, if any, and wait until its decoder has let go. */
  async stop() {
    const run = this.run;
    if (!run) return;
    if (this.sound) this.sound.halt();
    run.cancelled = true;
    if (run.wake) run.wake();
    await run.promise;
    if (this.run === run) this.run = null;
  }

  pause() {
    const run = this.run;
    if (!run || run.done || run.paused) return;
    run.pausedAt = this._media(run, performance.now());
    run.paused = true;
    if (this.sound) this.sound.halt();
    this._state('paused');
  }

  resume() {
    const run = this.run;
    if (!run || run.done || !run.paused) return;
    run.paused = false;
    if (run.pausedAt != null) this._setBase(run, { wall: performance.now(), media: run.pausedAt });
    if (run.wake) run.wake();
    this._state('playing');
  }

  setSpeed(s) {
    const run = this.run;
    const now = performance.now();
    const media = run && run.base && !run.paused ? this._media(run, now) : null;
    this.speed = s;
    if (media !== null) this._setBase(run, { wall: now, media });
  }

  /** The clock reads `base.media` at wall time `base.wall` from now on; the sound follows. */
  _setBase(run, base) {
    run.base = base;
    if (this.sound && !run.once && !run.paused) this.sound.follow(base, this.speed);
  }

  /**
   * Wait for the moment `t` on the clock (a pause or a speed change can come
   * meanwhile). With `slip`, a clock fallen behind (a slow decode) slips
   * rather than rushing. False if the run was cancelled.
   */
  async _until(run, t, slip) {
    for (;;) {
      if (run.cancelled) return false;
      if (run.paused) {
        await new Promise((r) => (run.wake = r));
        run.wake = null;
        continue;
      }
      const now = performance.now();
      if (!run.base) this._setBase(run, { wall: now, media: t });
      const due = run.base.wall + ((t - run.base.media) * 1000) / this.speed;
      if (slip && due < now - 120) this._setBase(run, { wall: now, media: t });
      else if (due <= now + 2) return true;
      else await sleep(Math.min(40, due - now));
    }
  }

  /** Where in the source's time the clock is, at wall time `now`. */
  _media(run, now) {
    if (!run.base) return run.t;
    return run.base.media + ((now - run.base.wall) * this.speed) / 1000;
  }

  _state(s, detail) {
    if (this.onState) this.onState(s, detail);
  }

  _size(movie) {
    const w = movie.width || 640;
    const h = movie.height || 360;
    const k = Math.min(1, MAX_SIDE / Math.max(w, h));
    const cw = Math.max(2, Math.round(w * k));
    const ch = Math.max(2, Math.round(h * k));
    if (this.canvas.width !== cw || this.canvas.height !== ch) {
      this.canvas.width = cw;
      this.canvas.height = ch;
    }
  }

  async _pass(run, { env, movie, sec, edited, fromSlot }) {
    const plan = sectionRenderPlan(env, movie, sec, { edited });
    const n = plan.seq.t.length;
    if (!n) return;
    const first = Math.max(0, Math.min(fromSlot, n - 1));
    const tFirst = sec.start + plan.base + plan.seq.t[first];
    const piece = { startSec: sec.start, endSec: sec.end, from: null, sections: [plan], offset: 0 };
    this._size(movie);
    if (this.sound && !run.once) this.sound.use(movie, plan);
    const g = this.canvas.getContext('2d');
    await walkEdited(
      movie,
      piece,
      async (frame, t, info) => {
        if (run.cancelled) return;
        // skipping ahead to the slot asked for: decode, don't show
        if (info.sec ? info.slot < first : t < tFirst - 1e-9) return;
        // wait for the picture's moment
        if (!(await this._until(run, t, true))) return;
        g.drawImage(frame, 0, 0, this.canvas.width, this.canvas.height);
        run.slot = info.sec ? info.slot : -1;
        run.t = t;
        if (this.onFrame) this.onFrame(info, t, plan);
        if (run.once) run.cancelled = true;
      },
      { cancel: () => run.cancelled }
    );
    // the last frame stays up for its time, and for its hold if it is held
    if (!run.cancelled && run.base) await this._until(run, plan.end + plan.extra, false);
    if (this.sound) this.sound.halt();
  }
}
