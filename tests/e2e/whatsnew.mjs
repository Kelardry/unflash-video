// What's new, in motion: for each change in CHANGELOG.md, a short film of
// it in the app, as someone would use it (not a test). A scene
// (whatsnew-scenes.mjs) sets the page up, then is filmed: the frames the
// browser draws, with a pointer and the keys pressed drawn in, and a camera
// that takes in the part of the page the change is about and can glide to
// another. Most scenes are filmed in Chromium; the changes that are about
// Firefox, in Firefox. A race films the same scene twice, the old way and
// the new, one above the other. Written to web/whatsnew/: NAME.webm (VP9,
// which every browser Unflash runs in plays), which What's new plays muted
// and looped, NAME.webp (the last picture: the poster, and all that shows
// for whoever asks their system for less motion) and shots.json (each
// one's size, a description of it, and its check). A change names its film
// in CHANGELOG.md: `<!-- HH:MM shot:NAME -->`.
//
// Nothing in What's new may flash: each film, as encoded, is scanned by
// Unflash itself under its strictest profile, and one that fails is not
// kept.
//   node tests/e2e/whatsnew.mjs [NAME...]     (every scene by default)
//   node tests/e2e/whatsnew.mjs --list
// (the Firefox scenes need FIREFOX=/path/to/firefox and puppeteer-core, as
// tests/e2e/firefox.mjs does; without them they are left as they are)
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { SCENES } from './whatsnew-scenes.mjs';
import { WEB, MEDIA, OUT as TEST_OUT, chromium, loadPuppeteer, FIREFOX_PREFS, FirefoxPage, jobEnd, job, open } from './playwright.mjs';

const OUT = path.join(WEB, 'whatsnew');
const WORK = path.join(TEST_OUT, 'whatsnew');
const MANIFEST = path.join(OUT, 'shots.json');
/** Pictures a second the films are sampled at. */
const FPS = 30;
/** A film's size: What's new's list, 1:1. */
const SIZE = { width: 720, height: 450 };
const VIEW = { width: 1280, height: 800 };

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---- what is drawn into the page for the film: the pointer, a ring where it
// clicks, the keys pressed, a caption, and a mark while time runs faster ----
const OVERLAY = `(() => {
  const css = \`
    #demo-pointer { position: fixed; left: 0; top: 0; z-index: 2147483647; pointer-events: none; display: none; will-change: transform; }
    #demo-pointer svg { display: block; filter: drop-shadow(0 1px 1.5px rgba(0,0,0,.7)); }
    .demo-ring { position: fixed; z-index: 2147483646; pointer-events: none; width: 26px; height: 26px; margin: -13px 0 0 -13px; border: 2px solid #e4e4e4; border-radius: 50%; animation: demo-ring .5s ease-out forwards; }
    @keyframes demo-ring { from { transform: scale(.35); opacity: .9; } to { transform: scale(1.15); opacity: 0; } }
    #demo-keys, #demo-caption, #demo-fast { position: fixed; z-index: 2147483647; pointer-events: none; font: 700 13px/1 ui-monospace, "DejaVu Sans Mono", monospace; letter-spacing: .04em; }
    #demo-keys { display: flex; gap: 5px; align-items: center; color: #e4e4e4; }
    #demo-keys kbd { font: inherit; background: #e4e4e4; color: #0d0d0d; padding: 6px 9px 5px; border-bottom: 3px solid #8f8f8f; min-width: 12px; text-align: center; }
    #demo-caption { background: rgba(13,13,13,.94); color: #e4e4e4; border: 1px solid #8f8f8f; padding: 6px 9px; font-weight: 400; max-width: 520px; line-height: 1.4; }
    #demo-fast { background: #e4e4e4; color: #0d0d0d; padding: 4px 7px; }
    #demo-pointer .demo-file { position: absolute; left: 14px; top: 20px; white-space: nowrap; font: 400 12px/1 ui-monospace, "DejaVu Sans Mono", monospace; background: #1e1e1e; color: #e4e4e4; border: 1px solid #8f8f8f; padding: 5px 7px; }
  \`;
  const make = () => {
    if (document.getElementById('demo-pointer') || !document.body) return;
    const style = document.createElement('style');
    style.textContent = css;
    document.head.appendChild(style);
    const p = document.createElement('div');
    p.id = 'demo-pointer';
    p.innerHTML = '<svg width="16" height="24" viewBox="0 0 16 24"><path d="M1.5 1.5v18.2l4.6-4.4 3 6.9 3.2-1.4-3-6.7h6.2z" fill="#f4f4f4" stroke="#0d0d0d" stroke-width="1.4" stroke-linejoin="round"/></svg>';
    document.body.appendChild(p);
    for (const id of ['demo-keys', 'demo-caption', 'demo-fast']) {
      const e = document.createElement('div');
      e.id = id;
      e.style.display = 'none';
      document.body.appendChild(e);
    }
    const at = (e) => {
      p.style.display = 'block';
      p.style.transform = 'translate(' + (e.clientX - 1.5) + 'px,' + (e.clientY - 1.5) + 'px)';
    };
    addEventListener('mousemove', at, true);
    addEventListener('pointermove', at, true);
    addEventListener('dragover', at, true);
    addEventListener('mousedown', (e) => {
      const r = document.createElement('div');
      r.className = 'demo-ring';
      r.style.left = e.clientX + 'px';
      r.style.top = e.clientY + 'px';
      document.body.appendChild(r);
      setTimeout(() => r.remove(), 600);
    }, true);
  };
  const place = (e, where) => {
    e.style.left = e.style.right = e.style.top = e.style.bottom = '';
    for (const k of Object.keys(where)) e.style[k] = where[k] + 'px';
  };
  let keysTimer = 0;
  window.__demo = {
    // (the corner of the filmed part of the page the keys and captions sit in)
    corner: { right: 16, bottom: 16 },
    keys(labels, ms = 900) {
      make();
      const e = document.getElementById('demo-keys');
      e.innerHTML = labels.map((l) => l === '+' ? '<span>+</span>' : '<kbd>' + l + '</kbd>').join('');
      place(e, this.corner);
      e.style.display = 'flex';
      clearTimeout(keysTimer);
      keysTimer = setTimeout(() => (e.style.display = 'none'), ms);
    },
    caption(text, where) {
      make();
      const e = document.getElementById('demo-caption');
      if (!text) return void (e.style.display = 'none');
      e.textContent = text;
      place(e, where || this.opposite());
      e.style.display = 'block';
    },
    fast(label) {
      make();
      const e = document.getElementById('demo-fast');
      if (!label) return void (e.style.display = 'none');
      e.textContent = label;
      place(e, this.corner);
      e.style.display = 'block';
    },
    // (captions go in the corner across from the keys)
    opposite() {
      const c = this.corner;
      const o = {};
      if (c.right !== undefined) o.left = this.view.x + 14;
      else o.right = innerWidth - (this.view.x + this.view.width) + 14;
      if (c.bottom !== undefined) o.top = this.view.y + 14;
      else o.bottom = innerHeight - (this.view.y + this.view.height) + 14;
      return o;
    },
    view: { x: 0, y: 0, width: innerWidth, height: innerHeight },
    // a ring where the pointer is, as a click makes
    ring(x, y) {
      make();
      const r = document.createElement('div');
      r.className = 'demo-ring';
      r.style.left = x + 'px';
      r.style.top = y + 'px';
      document.body.appendChild(r);
      setTimeout(() => r.remove(), 600);
    },
    // a file carried by the pointer (none: put down)
    carry(name) {
      make();
      const p = document.getElementById('demo-pointer');
      let f = p.querySelector('.demo-file');
      if (!name) return void (f && f.remove());
      if (!f) {
        f = document.createElement('span');
        f.className = 'demo-file';
        p.appendChild(f);
      }
      f.textContent = '▤ ' + name;
    },
    // a caption with the time since it started (stopped: it keeps the time it stopped at)
    clock(label) {
      make();
      clearInterval(this.ticking);
      if (label === null) return;
      const t0 = performance.now();
      const draw = () => this.caption(label + ' ' + ((performance.now() - t0) / 1000).toFixed(1) + ' s');
      draw();
      this.ticking = setInterval(draw, 100);
    },
  };
  if (document.readyState === 'loading') addEventListener('DOMContentLoaded', make);
  else make();
})();`;

// ---- the film ----------------------------------------------------------------
class Film {
  constructor() {
    /** `{ t, png }`: a picture of the page and when it was drawn (s). */
    this.frames = [];
    /** Stretches filmed faster: `{ a, b, f }`. */
    this.fast = [];
    /** Where the camera goes: `{ t, rect, ms }`, the first where it starts. */
    this.camera = [];
  }

  /**
   * The film's pictures at FPS, a still stretch as one picture: `[{ i, ms,
   * rect }]` (`i` the frame, `ms` how long it shows, `rect` the part of the
   * page in view). A stretch filmed at `f`× lasts 1/f as long.
   */
  timeline() {
    const frames = this.frames.filter((f) => f.t <= this.t1 + 0.05);
    if (!frames.length) throw new Error('nothing was filmed');
    const t0 = Math.min(this.t0, frames[0].t);
    const speed = (t) => {
      const s = this.fast.find((x) => t >= x.a && t < x.b);
      return s ? s.f : 1;
    };
    // film time to the film's own time, piece by piece, and back
    const cuts = [t0, this.t1, ...this.fast.flatMap((x) => [x.a, x.b])].filter((t) => t >= t0 && t <= this.t1).sort((a, b) => a - b);
    const pieces = [];
    let out = 0;
    for (let k = 0; k + 1 < cuts.length; k++) {
      const a = cuts[k];
      const b = cuts[k + 1];
      if (b <= a) continue;
      const f = speed((a + b) / 2);
      pieces.push({ a, b, f, o: out });
      out += (b - a) / f;
    }
    const toFilm = (o) => {
      const p = pieces.find((x) => o < x.o + (x.b - x.a) / x.f) || pieces[pieces.length - 1];
      return p.a + (o - p.o) * p.f;
    };
    const shown = [];
    let j = 0;
    for (let k = 0; k / FPS < out; k++) {
      const t = toFilm(k / FPS);
      while (j + 1 < frames.length && frames[j + 1].t <= t) j++;
      const rect = this.cameraAt(t);
      const ms = Math.round(Math.min(1 / FPS, out - k / FPS) * 1000);
      const last = shown[shown.length - 1];
      if (last && last.i === j && sameRect(last.rect, rect)) last.ms += ms;
      else shown.push({ i: j, ms, rect });
    }
    return { frames, shown, seconds: out };
  }

  /** Where the camera is at film time `t`: it glides from one place to the next. */
  cameraAt(t) {
    let rect = this.camera[0].rect;
    for (let k = 1; k < this.camera.length; k++) {
      const c = this.camera[k];
      if (t < c.t) break;
      const p = c.ms > 0 ? Math.min(1, (t - c.t) / (c.ms / 1000)) : 1;
      const e = p < 0.5 ? 4 * p * p * p : 1 - (-2 * p + 2) ** 3 / 2;
      if (p < 1) {
        rect = { x: rect.x + (c.rect.x - rect.x) * e, y: rect.y + (c.rect.y - rect.y) * e, width: rect.width + (c.rect.width - rect.width) * e, height: rect.height + (c.rect.height - rect.height) * e };
        break;
      }
      rect = c.rect;
    }
    return { x: Math.round(rect.x), y: Math.round(rect.y), width: Math.round(rect.width), height: Math.round(rect.height) };
  }
}

const sameRect = (a, b) => a.x === b.x && a.y === b.y && a.width === b.width && a.height === b.height;

/** Chromium: the frames as the compositor draws them (a screencast), each with its time. */
class ScreencastFilm extends Film {
  constructor(page, cdp) {
    super();
    this.page = page;
    this.cdp = cdp;
    this.onFrame = ({ data, metadata, sessionId }) => {
      this.frames.push({ t: metadata.timestamp, png: Buffer.from(data, 'base64') });
      this.cdp.send('Page.screencastFrameAck', { sessionId }).catch(() => {});
    };
  }

  async start() {
    this.cdp.on('Page.screencastFrame', this.onFrame);
    await this.cdp.send('Page.startScreencast', { format: 'png', everyNthFrame: 1 });
    this.t0 = Date.now() / 1000;
    // (a frame straight away, whether or not anything moves)
    await this.page.evaluate(() => {
      document.body.style.outline = '0px solid transparent';
      requestAnimationFrame(() => (document.body.style.outline = ''));
    });
  }

  async stop() {
    this.t1 = Date.now() / 1000;
    await this.cdp.send('Page.stopScreencast');
    this.cdp.off('Page.screencastFrame', this.onFrame);
  }
}

/** Firefox, which has no screencast: screenshots, one after another, as fast as they come. */
class ScreenshotFilm extends Film {
  constructor(page) {
    super();
    this.page = page;
  }

  async start() {
    this.t0 = Date.now() / 1000;
    this.running = true;
    this.loop = (async () => {
      while (this.running) {
        const a = Date.now();
        let png;
        try {
          png = await this.page.screenshot({ type: 'png' });
        } catch (e) {
          // (a page halfway through a reload has nothing to take: the picture before it stays up)
          await sleep(30);
          continue;
        }
        this.frames.push({ t: (a + Date.now()) / 2000, png: Buffer.from(png) });
      }
    })();
  }

  async stop() {
    this.t1 = Date.now() / 1000;
    this.running = false;
    await this.loop;
  }
}

// ---- what a scene can do: the pointer, clicks, keys, captions, the camera ----
export class Demo {
  constructor(page, port, film, size = SIZE, browser = 'chromium') {
    this.page = page;
    this.port = port;
    this.film = film;
    /** 'chromium' or 'firefox'. */
    this.browser = browser;
    /** The film's size. */
    this.size = size;
    this.view = null;
    this.cornerName = 'bottom-right';
    this.x = null;
    this.y = null;
  }

  url(query = '') {
    return `http://127.0.0.1:${this.port}/${query ? '?' + query : ''}`;
  }

  async goto(query = '') {
    await this.page.goto(this.url(query));
    await this.loaded();
  }

  /** Reload the page, as its reload button does (a browser may put back what its form fields held). */
  async reload() {
    await this.page.reload();
    await this.loaded();
  }

  async loaded() {
    await this.page.waitForFunction(() => window.__unflash && window.__unflash.changes && window.__demo, null, { timeout: 60000 });
    await this.page.waitForFunction(() => /WebGPU|CPU/.test(document.querySelector('#support').textContent), null, { timeout: 60000 }).catch(() => {});
    this.x = this.y = null;
    if (this.view) await this.corner(this.view, this.cornerName);
  }

  wait(ms) {
    return sleep(ms);
  }

  /** Run `fn(arg)` in the page. */
  eval(fn, arg) {
    return this.page.evaluate(fn, arg);
  }

  /** Until `fn(arg)` holds in the page. */
  until(fn, arg = null, timeout = 180000) {
    return this.page.waitForFunction(fn, arg, { timeout, polling: 100 });
  }

  /**
   * From now on, record what the section player's sound plays (it must be
   * on: its audio exists once "sound off" has been clicked), as the film's
   * sound (a scene with `sound`). Each block of it keeps the moment it was
   * heard, on the clock the film's pictures keep.
   */
  async recordSound() {
    await this.eval(async () => {
      const s = window.__unflash.sectionSound;
      const ctx = s.ctx;
      const code = "registerProcessor('demo-tap', class extends AudioWorkletProcessor { process(inputs) { const i = inputs[0]; if (i.length) this.port.postMessage({ t: currentTime, ch: i.map((c) => c.slice()) }); return true; } });";
      await ctx.audioWorklet.addModule(URL.createObjectURL(new Blob([code], { type: 'text/javascript' })));
      const tap = new AudioWorkletNode(ctx, 'demo-tap', { channelCount: 2, channelCountMode: 'explicit', channelInterpretation: 'speakers' });
      // (it has to reach the speakers to be run, and must add nothing to them)
      const mute = ctx.createGain();
      mute.gain.value = 0;
      tap.connect(mute);
      mute.connect(ctx.destination);
      s.out.connect(tap);
      // a sample played at context time T is heard at ms (T - ctx0 + lat) after perf0, as sound.js reckons
      const rec = { rate: ctx.sampleRate, blocks: [], lat: ctx.outputLatency || ctx.baseLatency || 0, perf0: performance.now(), ctx0: ctx.currentTime };
      tap.port.onmessage = (e) => rec.blocks.push(e.data);
      window.__demoSound = rec;
    });
  }

  /** The sound recorded (recordSound), as 16-bit stereo: `{ rate, at (s since 1970, when its first sample is heard), pcm }`, or null. */
  async recordedSound() {
    const got = await this.eval(async () => {
      const rec = window.__demoSound;
      if (!rec || !rec.blocks.length) return null;
      const blocks = rec.blocks.slice().sort((a, b) => a.t - b.t);
      const t0 = blocks[0].t;
      const last = blocks[blocks.length - 1];
      const n = Math.round((last.t - t0) * rec.rate) + last.ch[0].length;
      const pcm = new Int16Array(n * 2);
      for (const b of blocks) {
        const o = Math.round((b.t - t0) * rec.rate);
        for (let j = 0; j < b.ch[0].length && o + j < n; j++) {
          for (let c = 0; c < 2; c++) pcm[(o + j) * 2 + c] = Math.max(-32768, Math.min(32767, Math.round((b.ch[c] || b.ch[0])[j] * 32767)));
        }
      }
      const url = await new Promise((r) => {
        const f = new FileReader();
        f.onload = () => r(f.result);
        f.readAsDataURL(new Blob([pcm.buffer]));
      });
      return { rate: rec.rate, at: (performance.timeOrigin + rec.perf0 + (t0 - rec.ctx0 + rec.lat) * 1000) / 1000, data: url.slice(url.indexOf(',') + 1) };
    });
    return got && { rate: got.rate, at: got.at, pcm: Buffer.from(got.data, 'base64') };
  }

  /** Until no job runs. */
  async idle(timeout = 300000) {
    await this.until(() => !window.__unflash.state.job && document.querySelector('#jobbar').classList.contains('hidden') && !(window.__unflash.state.auto && window.__unflash.state.auto.running), null, timeout);
  }

  /**
   * Do `act` (a click), which starts a job, until the job is over, however
   * quickly it went, and nothing runs: what comes after the click filmed
   * `f` times as fast.
   */
  async job(act, f = 1) {
    const end = await jobEnd(this.page);
    await act();
    const over = async () => {
      await end();
      await this.idle();
    };
    await (f > 1 ? this.faster(over, f) : over());
  }

  /** Open a clip from the test media (or one made from them, `DERIVED`), as the file picker would. */
  async open(name, { wait = true } = {}) {
    await this.page.setInputFiles('#fileInput', media(name));
    if (wait) await this.opened(name);
  }

  /** Until `name` is open and whatever opening it started (its scan) is done. */
  async opened(name) {
    await this.until((n) => document.querySelector('#videoInfo').textContent.includes(n), name, 120000);
    await this.settled();
  }

  /**
   * Until no job has run for `ms`: one job often starts another a moment
   * after it ends (opening, then the scan), and a section opened between
   * the two would not prepare itself.
   */
  async settled(ms = 700) {
    for (;;) {
      await this.idle();
      await sleep(ms);
      if (await this.eval(() => !window.__unflash.state.job)) return;
    }
  }

  /** Open the section list's `n`th section (from 1), prepared and checked. */
  async section(n = 1) {
    await this.settled();
    await this.eval((k) => document.querySelectorAll('#sectionList .sec-item')[k].click(), n - 1);
    await this.verdict();
  }

  async verdict(re = /passes|fails/) {
    await this.until((src) => new RegExp(src).test(document.querySelector('#wsVerdict').textContent), re.source);
    await this.idle();
  }

  /** The middle of an element (scrolled into view first), in the viewport. */
  async centre(sel, dx = 0.5, dy = 0.5) {
    const loc = this.page.locator(sel).first();
    await loc.scrollIntoViewIfNeeded();
    const b = await loc.boundingBox();
    if (!b) throw new Error(`nothing to point at: ${sel}`);
    return { x: b.x + b.width * dx, y: b.y + b.height * dy };
  }

  /** A rectangle round the given elements (with a margin), inside the viewport. */
  async around(sels, pad = 12) {
    const boxes = [];
    for (const s of [].concat(sels)) {
      const b = await this.page.locator(s).first().boundingBox();
      if (b && b.width && b.height) boxes.push(b);
    }
    if (!boxes.length) throw new Error('nothing to film: ' + sels);
    const { width: vw, height: vh } = this.page.viewportSize();
    const x0 = Math.max(0, Math.floor(Math.min(...boxes.map((b) => b.x)) - pad));
    const y0 = Math.max(0, Math.floor(Math.min(...boxes.map((b) => b.y)) - pad));
    const x1 = Math.min(vw, Math.ceil(Math.max(...boxes.map((b) => b.x + b.width)) + pad));
    const y1 = Math.min(vh, Math.ceil(Math.max(...boxes.map((b) => b.y + b.height)) + pad));
    return { x: x0, y: y0, width: x1 - x0, height: y1 - y0 };
  }

  /**
   * A rectangle of the page the camera can show: the film's shape, round
   * `rect` (centred on it, or against the side `align` names), no smaller
   * than the film (which is never enlarged) and inside the viewport.
   */
  fit(rect, align = '') {
    const { width: W, height: H } = this.size;
    const { width: vw, height: vh } = this.page.viewportSize();
    let w = Math.max(W, rect.width, (rect.height * W) / H);
    let h = (w * H) / W;
    if (w > vw) {
      w = vw;
      h = (w * H) / W;
    }
    if (h > vh) {
      h = vh;
      w = (h * W) / H;
    }
    let x = rect.x + rect.width / 2 - w / 2;
    let y = rect.y + rect.height / 2 - h / 2;
    if (align.includes('left')) x = rect.x;
    if (align.includes('right')) x = rect.x + rect.width - w;
    if (align.includes('top')) y = rect.y;
    if (align.includes('bottom')) y = rect.y + rect.height - h;
    x = Math.min(Math.max(0, x), vw - w);
    y = Math.min(Math.max(0, y), vh - h);
    return { x: Math.round(x), y: Math.round(y), width: Math.round(w), height: Math.round(h) };
  }

  /** The film's size of the page (1:1), with the element's top-left corner `pad` inside it. */
  async from(sel, pad = 12) {
    const b = await this.page.locator(sel).first().boundingBox();
    if (!b) throw new Error('nothing to film: ' + sel);
    return { x: Math.max(0, Math.round(b.x - pad)), y: Math.max(0, Math.round(b.y - pad)), width: this.size.width, height: this.size.height };
  }

  /**
   * Glide the camera to take in the given elements (or a rectangle; with
   * `anchor`, the film's size from the element's top-left corner) over
   * `ms`; the keys and captions move to its corner.
   */
  async camera(what, { pad = 12, ms = 800, align = '', corner, anchor = false } = {}) {
    const rect = this.fit(what.width !== undefined ? what : anchor ? await this.from([].concat(what)[0], pad) : await this.around(what, pad), align);
    this.view = rect;
    this.film.camera.push({ t: Date.now() / 1000, rect, ms: this.film.camera.length ? ms : 0 });
    await this.corner(rect, corner || this.cornerName);
    return rect;
  }

  /** Glide the pointer to (x, y). */
  async moveTo(x, y, ms = 500) {
    if (this.x === null) {
      this.x = x + 90;
      this.y = y + 70;
      await this.page.mouse.move(this.x, this.y);
    }
    const n = Math.max(1, Math.round(ms / 20));
    const x0 = this.x;
    const y0 = this.y;
    for (let i = 1; i <= n; i++) {
      const t = i / n;
      const e = t < 0.5 ? 2 * t * t : 1 - (-2 * t + 2) ** 2 / 2;
      await this.page.mouse.move(x0 + (x - x0) * e, y0 + (y - y0) * e);
      await sleep(14);
    }
    this.x = x;
    this.y = y;
  }

  async point(sel, { dx = 0.5, dy = 0.5, ms = 500 } = {}) {
    const c = await this.centre(sel, dx, dy);
    await this.moveTo(c.x, c.y, ms);
    return c;
  }

  async click(sel, { dx = 0.5, dy = 0.5, ms = 500, after = 350, dbl = false, modifiers = [] } = {}) {
    await this.point(sel, { dx, dy, ms });
    await sleep(160);
    for (const m of modifiers) await this.page.keyboard.down(m);
    await this.page.mouse.click(this.x, this.y, dbl ? { clickCount: 2 } : {});
    for (const m of modifiers) await this.page.keyboard.up(m);
    await sleep(after);
  }

  /** Click the text field `sel`, clear it and type `text` into it, a key at a time. */
  async type(sel, text, { after = 500 } = {}) {
    await this.click(sel, { after: 200 });
    await this.eval((s) => document.querySelector(s).select(), sel);
    await sleep(250);
    await this.page.keyboard.type(text, { delay: 70 });
    await sleep(after);
  }

  /** A key, shown in the corner as it is pressed ('Control+Z' shows as Ctrl + Z). */
  async press(key, { after = 500, label } = {}) {
    const names = { Control: 'Ctrl', Shift: 'Shift', ArrowLeft: '←', ArrowRight: '→', ArrowUp: '↑', ArrowDown: '↓', Escape: 'Esc' };
    const shown = label || key.split('+').map((k) => names[k] || k).flatMap((k, i) => (i ? ['+', k] : [k]));
    await this.eval((l) => window.__demo.keys(l), [].concat(shown));
    await sleep(120);
    await this.page.keyboard.press(key);
    await sleep(after);
  }

  /** Pick the clip `name` in the file chooser that clicking `sel` opens. */
  pick(sel, name, opts = {}) {
    return this.pickPath(sel, media(name), opts);
  }

  /**
   * Pick the file at `file` in the file chooser that clicking `sel` opens.
   * (Firefox, driven over WebDriver BiDi, opens no chooser to answer: the
   * pointer goes there and clicks as it would, and the file is given to
   * the chooser's input.)
   */
  async pickPath(sel, file, opts = {}) {
    if (this.browser === 'firefox') {
      await this.point(sel, opts);
      await sleep(160);
      await this.eval(([x, y]) => window.__demo.ring(x, y), [this.x, this.y]);
      await sleep(opts.after === undefined ? 350 : opts.after);
      const input = await this.eval((s) => {
        const el = document.querySelector(s);
        const i = el.tagName === 'INPUT' ? el : el.querySelector('input[type=file]');
        return i && i.id ? '#' + i.id : null;
      }, sel);
      await this.page.setInputFiles(input, file);
    } else {
      const [fc] = await Promise.all([this.page.waitForEvent('filechooser'), this.click(sel, opts)]);
      await fc.setFiles(file);
    }
  }

  /** Click `sel`, which downloads a file: its path. */
  async download(sel, opts = {}) {
    const [dl] = await Promise.all([this.page.waitForEvent('download'), this.click(sel, opts)]);
    const to = path.join(WORK, 'downloads', dl.suggestedFilename());
    fs.mkdirSync(path.dirname(to), { recursive: true });
    await dl.saveAs(to);
    return to;
  }

  /** A caption that counts the seconds since it started, until `clock(null)`. */
  async clock(label) {
    await this.eval((l) => window.__demo.clock(l), label);
  }

  /** A file carried by the pointer (none: put down). */
  async carry(name) {
    await this.eval((n) => window.__demo.carry(n), name || '');
  }

  /** A caption in the corner across from the keys (none: take it away). */
  async caption(text, where) {
    await this.eval(([t, w]) => window.__demo.caption(t, w), [text || '', where || null]);
  }

  /** Where in the page the keys and captions go: a corner of the filmed rectangle. */
  async corner(view, which = 'bottom-right') {
    this.cornerName = which;
    const { width: vw, height: vh } = this.page.viewportSize();
    const c = {};
    if (which.includes('right')) c.right = vw - (view.x + view.width) + 14;
    else c.left = view.x + 14;
    if (which.includes('bottom')) c.bottom = vh - (view.y + view.height) + 14;
    else c.top = view.y + 14;
    await this.eval(([x, v]) => Object.assign(window.__demo, { corner: x, view: v }), [c, view]);
  }

  /** Film what `fn` does `f` times as fast, with a mark saying so. */
  async faster(fn, f = 4) {
    await this.eval((l) => window.__demo.fast(l), `▸▸ ${f}×`);
    const a = Date.now() / 1000;
    try {
      return await fn();
    } finally {
      const b = Date.now() / 1000;
      this.film.fast.push({ a, b, f });
      await this.eval(() => window.__demo.fast(''));
    }
  }

  /** Scroll `box` smoothly, over `ms`, until `sel` is `offset` below its top. */
  async scrollTo(sel, { box = '#stage', ms = 900, offset = 12 } = {}) {
    const dy = await this.eval(([s, b, o]) => document.querySelector(s).getBoundingClientRect().top - document.querySelector(b).getBoundingClientRect().top - o, [sel, box, offset]);
    await this.scroll(box, dy, ms);
  }

  /** Scroll an element's scroll box (or the page) by `dy` over `ms`, as a wheel would. */
  async scroll(sel, dy, ms = 600) {
    const n = Math.max(1, Math.round(ms / 30));
    for (let i = 0; i < n; i++) {
      await this.eval(([s, d]) => {
        const e = s ? document.querySelector(s) : document.scrollingElement;
        e.scrollTop += d;
      }, [sel, dy / n]);
      await sleep(30);
    }
  }
}

// ---- clips made from the test media for a scene ---------------------------
const DERIVED = {
  // a minute of the flash clip, over and over: a scan long enough to watch
  'flash-minute.mp4': (out) => concat(out, Array(6).fill('flash.mp4')),
  // ten minutes of it
  'flash-10min.mp4': (out) => concat(out, Array(60).fill('flash.mp4')),
  // the minute, with its index spread through the file in fragments (as OBS can record)
  'recording.mp4': (out) => ffmpeg(['-i', media('flash-minute.mp4'), '-c', 'copy', '-movflags', 'frag_keyframe+empty_moov+default_base_moof', '-frag_duration', '500000', out]),
  // most of a minute of nothing, then the flashing: where a scan should look first
  'late-flash.mp4': (out) => concat(out, [...Array(8).fill('steady.mp4'), 'flash.mp4']),
  // two minutes and more of H.264
  'flash-long-h264.mp4': (out) => concat(out, Array(13).fill('flash_h264.mp4')),
  // the flash clip at 1920×1080, twice over
  'flash-1080p.mp4': (out) => ffmpeg(['-stream_loop', '1', '-i', path.join(MEDIA, 'flash.mp4'), '-vf', 'scale=1920:1080:flags=bicubic', '-c:v', 'libx264', '-preset', 'veryfast', '-crf', '20', '-g', '60', '-pix_fmt', 'yuv420p', '-c:a', 'copy', out]),
  // six hours of MKV, of a still picture (550 MB): it keeps no index, so opening it reads it through
  'six-hours.mkv': (out) => {
    const list = out + '.txt';
    fs.writeFileSync(list, Array(3600).fill(`file '${path.join(MEDIA, 'steady.mp4')}'`).join('\n') + '\n');
    ffmpeg(['-f', 'concat', '-safe', '0', '-i', list, '-c', 'copy', out]);
  },
  // an AVI, which Unflash recognises and says how to convert
  'clip.avi': (out) => ffmpeg(['-i', path.join(MEDIA, 'flash.mp4'), '-t', '2', '-c:v', 'mpeg4', '-q:v', '5', '-an', out]),
  // two recordings of a transport stream joined end to end, the first an hour
  // on in its clock: where the second begins, the clock goes back an hour
  'joined.ts': (out) => {
    const first = out + '.first.ts';
    ffmpeg(['-i', path.join(MEDIA, 'flash.ts'), '-c', 'copy', '-output_ts_offset', '3600', first]);
    fs.writeFileSync(out, Buffer.concat([fs.readFileSync(first), fs.readFileSync(path.join(MEDIA, 'flash.ts'))]));
  },
};

function concat(out, names) {
  const list = out + '.txt';
  fs.writeFileSync(list, names.map((n) => `file '${path.join(MEDIA, n)}'`).join('\n') + '\n');
  ffmpeg(['-f', 'concat', '-safe', '0', '-i', list, '-c', 'copy', out]);
}

/** A clip's path: the test media's, or one made from them (once). */
export function media(name) {
  if (!DERIVED[name]) return path.join(MEDIA, name);
  const out = path.join(WORK, 'media', name);
  if (!fs.existsSync(out)) {
    fs.mkdirSync(path.dirname(out), { recursive: true });
    DERIVED[name](out);
  }
  return out;
}

// ---- making the files ------------------------------------------------------
function ffmpeg(args) {
  execFileSync('ffmpeg', ['-hide_banner', '-v', 'error', '-y', ...args], { stdio: ['ignore', 'inherit', 'inherit'] });
}

/** Pictures (`{ png, ms }`) as files, with a concat list of how long each shows. */
function writeSequence(dir, pictures) {
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { recursive: true });
  const lines = ['ffconcat version 1.0'];
  pictures.forEach((p, k) => {
    const f = `f${String(k).padStart(5, '0')}.png`;
    fs.writeFileSync(path.join(dir, f), p.png);
    lines.push(`file '${f}'`, `duration ${(p.ms / 1000).toFixed(3)}`);
  });
  // (the concat demuxer gives the last picture its duration only when it is listed again)
  lines.push(`file 'f${String(pictures.length - 1).padStart(5, '0')}.png'`);
  fs.writeFileSync(path.join(dir, 'list.txt'), lines.join('\n') + '\n');
  return path.join(dir, 'list.txt');
}

/** A film's pictures, each cut out where the camera was and made the film's size: `[{ png, ms }]`. */
function cutOut(dir, film, size) {
  const { frames, shown, seconds } = film.timeline();
  const cut = path.join(dir, 'cut');
  fs.rmSync(cut, { recursive: true, force: true });
  fs.mkdirSync(cut, { recursive: true });
  // each run of pictures with the camera still, in one go
  const pictures = [];
  for (let a = 0; a < shown.length; ) {
    let b = a + 1;
    while (b < shown.length && sameRect(shown[b].rect, shown[a].rect)) b++;
    const src = path.join(dir, 'src');
    fs.rmSync(src, { recursive: true, force: true });
    fs.mkdirSync(src, { recursive: true });
    for (let k = a; k < b; k++) fs.writeFileSync(path.join(src, `${String(k - a).padStart(5, '0')}.png`), frames[shown[k].i].png);
    const r = shown[a].rect;
    const scale = r.width === size.width && r.height === size.height ? '' : `,scale=${size.width}:${size.height}:flags=lanczos`;
    ffmpeg(['-start_number', '0', '-i', path.join(src, '%05d.png'), '-vf', `crop=${r.width}:${r.height}:${r.x}:${r.y}${scale}`, '-start_number', String(a), path.join(cut, '%05d.png')]);
    for (let k = a; k < b; k++) pictures.push({ png: fs.readFileSync(path.join(cut, `${String(k).padStart(5, '0')}.png`)), ms: shown[k].ms });
    a = b;
  }
  return { pictures, seconds };
}

/**
 * The film files from one film's pictures, or from a race's two (one above
 * the other, the shorter holding its last picture until the longer ends).
 */
function encode(name, parts) {
  const dir = path.join(WORK, name);
  const total = Math.max(...parts.map((p) => p.pictures.reduce((a, x) => a + x.ms, 0)));
  const lists = parts.map((p, k) => {
    const pics = p.pictures.slice();
    const ms = pics.reduce((a, x) => a + x.ms, 0);
    pics[pics.length - 1] = { ...pics[pics.length - 1], ms: pics[pics.length - 1].ms + (total - ms) };
    return writeSequence(path.join(dir, `frames${k}`), pics);
  });
  const out = (ext) => path.join(OUT, `${name}.${ext}`);
  const inputs = lists.flatMap((l) => ['-f', 'concat', '-safe', '0', '-i', l]);
  const graph = parts.length === 1 ? ['-vf', `fps=${FPS},format=yuv420p`] : ['-filter_complex', `${parts.map((_, k) => `[${k}]fps=${FPS}[v${k}]`).join(';')};${parts.map((_, k) => `[v${k}]`).join('')}vstack=inputs=${parts.length},format=yuv420p`];
  const sound = parts.length === 1 && parts[0].sound;
  let audio = ['-an'];
  if (sound) {
    const ms = Math.round(sound.offset * 1000);
    const place = ms >= 0 ? `adelay=${ms}|${ms}` : `atrim=start=${(-ms / 1000).toFixed(3)},asetpts=PTS-STARTPTS`;
    inputs.push('-f', 's16le', '-ar', String(sound.rate), '-ac', '2', '-i', sound.file);
    audio = ['-map', '0:v', '-map', `${lists.length}:a`, '-af', `${place},apad,atrim=end=${(total / 1000).toFixed(3)}`, '-c:a', 'libopus', '-b:a', '48k'];
  }
  const common = [...inputs, ...graph, ...audio];
  ffmpeg([...common, '-c:v', 'libvpx-vp9', '-crf', '33', '-b:v', '0', '-deadline', 'good', '-cpu-used', '1', '-row-mt', '1', out('webm')]);
  // the poster: the last picture
  const last = path.join(dir, 'last.png');
  ffmpeg(['-sseof', '-0.1', '-i', out('webm'), '-update', '1', last]);
  ffmpeg(['-i', last, '-c:v', 'libwebp', '-quality', '85', '-compression_level', '6', out('webp')]);
  return { files: { webm: out('webm'), webp: out('webp') }, seconds: total / 1000, pictures: parts.reduce((a, p) => a + p.pictures.length, 0), sound: !!sound };
}

/**
 * A film, as encoded, scanned by Unflash under its strictest profile, on
 * the page `page` (one page does every check): `{ frames, profile,
 * violations }`.
 */
async function check(page, file) {
  await open(page, file);
  await page.selectOption('#profileSel', 'strict');
  await page.waitForFunction(() => !window.__unflash.state.job && window.__unflash.state.project.profile === 'strict', null, { timeout: 60000 });
  await job(page, () => page.click('#btnScan'));
  return page.evaluate(() => {
    const s = window.__unflash.lastScan;
    return { frames: s.frames, profile: window.__unflash.state.project.profile, violations: s.result.violations.map((v) => ({ kind: v.kind, start: +v.start.toFixed(2), end: +v.end.toFixed(2) })) };
  });
}

// ---- the browsers ------------------------------------------------------------
/** A page in a fresh browser context, the overlay in it, and its film. */
async function newPage(browsers, scene, part) {
  const viewport = scene.viewport || VIEW;
  const errors = [];
  if ((part.browser || scene.browser) === 'firefox') {
    const browser = await browsers.firefox();
    const ctx = await browser.createBrowserContext();
    const p = await ctx.newPage();
    await p.setViewport(viewport);
    await p.evaluateOnNewDocument(OVERLAY);
    if (scene.init) await p.evaluateOnNewDocument(scene.init);
    p.on('pageerror', (e) => errors.push(e.message));
    p.on('dialog', (d) => d.accept());
    const page = new FirefoxPage(p, viewport);
    return { page, film: new ScreenshotFilm(page), errors, close: () => ctx.close(), browser: 'firefox' };
  }
  const ctx = await browsers.chromium.newContext({ viewport, acceptDownloads: true });
  await ctx.addInitScript(OVERLAY);
  if (scene.init) await ctx.addInitScript(scene.init);
  const page = await ctx.newPage();
  page.on('pageerror', (e) => errors.push(e.message));
  page.on('dialog', (d) => d.accept());
  const cdp = await ctx.newCDPSession(page);
  return { page, film: new ScreencastFilm(page, cdp), errors, close: () => ctx.close(), browser: 'chromium' };
}

/** Film a scene (or one part of a race): its pictures, cut out, and their times. */
async function film(browsers, port, scene, part = {}) {
  const size = part.size || scene.size || SIZE;
  const { page, film: f, errors, close, browser } = await newPage(browsers, scene, part);
  const d = new Demo(page, port, f, size, browser);
  d.part = part;
  try {
    await d.goto(part.query !== undefined ? part.query : scene.query !== undefined ? scene.query : 'tour=0');
    if (scene.setup) await scene.setup(d);
    d.cornerName = scene.corner || 'bottom-right';
    const view = scene.view ? (typeof scene.view === 'function' ? await scene.view(d) : scene.view) : { x: 0, y: 0, ...(scene.viewport || VIEW) };
    await d.camera(view, { align: scene.align || '' });
    if (part.label) await d.caption(part.label);
    await f.start();
    await sleep(scene.lead === undefined ? 700 : scene.lead);
    await scene.play(d);
    await sleep(scene.hold === undefined ? 2200 : scene.hold);
    await f.stop();
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    const dir = path.join(WORK, scene.name, `part${part.k || 0}`);
    const cut = cutOut(dir, f, size);
    if (scene.sound) {
      // (a film with its sound is filmed at its own pace throughout: no stretch sped up)
      if (f.fast.length) throw new Error('a film with sound has no faster stretches');
      const sound = await d.recordedSound();
      if (!sound) throw new Error('the scene was to record its sound, and nothing played');
      const file = path.join(dir, 'sound.s16');
      fs.writeFileSync(file, sound.pcm);
      cut.sound = { file, rate: sound.rate, offset: sound.at - Math.min(f.t0, f.frames[0].t) };
    }
    return cut;
  } catch (e) {
    // (what the page looked like when it went wrong)
    const shot = path.join(WORK, `${scene.name}-failed.png`);
    await page.screenshot({ path: shot }).then((b) => b && !fs.existsSync(shot) && fs.writeFileSync(shot, b)).catch(() => {});
    const state = await page.evaluate(() => ({ job: window.__unflash.state.job && window.__unflash.state.job.name, jobMsg: document.querySelector('#jobMsg').textContent, verdict: document.querySelector('#wsVerdict').textContent, toast: document.querySelector('#toast').textContent, banner: document.querySelector('#banner').classList.contains('hidden') ? '' : document.querySelector('#bannerText').textContent })).catch(() => null);
    throw new Error(`${e.message}\n  page: ${JSON.stringify(state)}; errors: ${errors.join('; ')}; screenshot: ${shot}`);
  } finally {
    await close();
  }
}

// ---- the run -----------------------------------------------------------------
if (process.argv.includes('--list')) {
  for (const s of SCENES) console.log(`${s.name}${s.browser === 'firefox' ? ' (Firefox)' : ''}${s.race ? ' (a race)' : ''}: ${s.alt}`);
  process.exit(0);
}
const wanted = process.argv.slice(2).filter((a) => !a.startsWith('--'));
const unknown = wanted.filter((w) => !SCENES.some((s) => s.name === w));
if (unknown.length) throw new Error('no such scene: ' + unknown.join(', '));
const scenes = wanted.length ? SCENES.filter((s) => wanted.includes(s.name)) : SCENES;

fs.mkdirSync(OUT, { recursive: true });
fs.mkdirSync(WORK, { recursive: true });
// the clips the scenes use, made now rather than while the camera runs
for (const name of Object.keys(DERIVED)) if (scenes.some((s) => [s.setup, s.play].some((f) => f && String(f).includes(`'${name}'`)))) media(name);
const manifest = fs.existsSync(MANIFEST) ? JSON.parse(fs.readFileSync(MANIFEST, 'utf8')) : {};
// (films play by themselves, with their sound; ForceEagerMeasureMemory: performance.measureUserAgentSpecificMemory() answers at once)
const { browser: chromiumBrowser, port, close } = await chromium(['--autoplay-policy=no-user-gesture-required', '--enable-blink-features=ForceEagerMeasureMemory']);
let firefox = null;
const browsers = {
  chromium: chromiumBrowser,
  async firefox() {
    if (firefox) return firefox;
    const puppeteer = await loadPuppeteer();
    if (!puppeteer || !process.env.FIREFOX) throw new Error('a Firefox scene: set FIREFOX to its binary and install puppeteer-core');
    firefox = await puppeteer.launch({ browser: 'firefox', executablePath: process.env.FIREFOX, headless: true, extraPrefsFirefox: { ...FIREFOX_PREFS, 'media.autoplay.default': 0 }, protocolTimeout: 600000 });
    return firefox;
  },
};
const failed = [];
const checker = await (await browsers.chromium.newContext({ viewport: VIEW })).newPage();
await checker.goto(`http://127.0.0.1:${port}/?auto=0&tour=0&cpu=1`);
await checker.waitForFunction(() => window.__unflash && window.__unflash.changes, null, { timeout: 60000 });
try {
  for (const scene of scenes) {
    const t = Date.now();
    try {
      let parts;
      if (scene.race) {
        // the old way above, the new below: each half the film's height
        const size = { width: SIZE.width, height: SIZE.height / scene.race.length };
        parts = [];
        for (const [k, part] of scene.race.entries()) parts.push(await film(browsers, port, scene, { ...part, k, size }));
      } else parts = [await film(browsers, port, scene)];
      const made = encode(scene.name, parts);
      const checked = await check(checker, made.files.webm);
      const kb = (p) => Math.round(fs.statSync(p).size / 1024);
      console.log(`${scene.name}: ${made.seconds.toFixed(1)} s, ${made.pictures} pictures; ${kb(made.files.webm)} KB film, ${kb(made.files.webp)} KB poster; scanned: ${checked.frames} frames, ${checked.violations.length ? JSON.stringify(checked.violations) : 'nothing found'} (${((Date.now() - t) / 1000).toFixed(0)} s)`);
      if (checked.violations.length || checked.profile !== 'strict') {
        for (const p of Object.values(made.files)) fs.rmSync(p, { force: true });
        delete manifest[scene.name];
        throw new Error(`${scene.name} flashes: ${JSON.stringify(checked)}`);
      }
      // (the pictures it was made from go, unless asked to stay)
      if (!process.env.WHATSNEW_KEEP) for (const f of fs.readdirSync(path.join(WORK, scene.name))) if (!f.endsWith('.png')) fs.rmSync(path.join(WORK, scene.name, f), { recursive: true, force: true });
      const sha = (p) => crypto.createHash('sha256').update(fs.readFileSync(p)).digest('hex');
      manifest[scene.name] = { w: SIZE.width, h: SIZE.height, seconds: +made.seconds.toFixed(1), alt: scene.alt, ...(made.sound ? { sound: true } : {}), sha256: sha(made.files.webm), checked: { profile: 'strict', frames: checked.frames, violations: 0 } };
      fs.writeFileSync(MANIFEST, JSON.stringify(Object.fromEntries(Object.entries(manifest).sort(([a], [b]) => a.localeCompare(b))), null, 1) + '\n');
    } catch (e) {
      console.error(`${scene.name}: FAILED: ${e.stack || e}`);
      failed.push(scene.name);
    }
  }
} finally {
  await close();
  if (firefox) await firefox.close();
}
if (failed.length) {
  console.error('failed: ' + failed.join(' '));
  process.exitCode = 1;
}
