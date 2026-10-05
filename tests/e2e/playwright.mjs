// What the browser tests share: where things are, the browsers (Chromium
// through Playwright, Firefox through puppeteer-core) and what a test does
// on the page (open a file, scan it, wait for a job, read the banner). The
// page helpers take a Playwright page, or a puppeteer one wrapped in
// FirefoxPage. ci.yml loads Playwright through loadPlaywright.
import { createRequire } from 'node:module';
import { execSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { serve } from './server.mjs';

// (a file path, not a URL's: a checkout whose path has a space in it is found)
export const ROOT = path.resolve(fileURLToPath(new URL('../..', import.meta.url)));
export const WEB = path.join(ROOT, 'web');
export const MEDIA = path.join(ROOT, 'tests/media/e2e');
export const OUT = path.join(ROOT, 'tests/e2e/out');

export function assert(cond, msg) {
  if (!cond) throw new Error('ASSERT: ' + msg);
}

/** Playwright, whether it is installed locally, globally, or in this container's Node prefix. */
export async function loadPlaywright() {
  const require = createRequire(import.meta.url);
  const candidates = [];
  try {
    candidates.push(require.resolve('playwright'));
  } catch (e) {
    /* not local */
  }
  try {
    const root = execSync('npm root -g', { encoding: 'utf8' }).trim();
    candidates.push(`${root}/playwright/index.mjs`);
  } catch (e) {
    /* no npm */
  }
  candidates.push('/opt/node22/lib/node_modules/playwright/index.mjs', '/usr/lib/node_modules/playwright/index.mjs', '/usr/local/lib/node_modules/playwright/index.mjs');
  for (const c of candidates) {
    if (fs.existsSync(c)) return import(c);
  }
  throw new Error('playwright not found; npm install -g playwright && npx playwright install chromium');
}

/** puppeteer-core, found locally or globally (npm install -g puppeteer-core), or null. */
export async function loadPuppeteer() {
  const require = createRequire(import.meta.url);
  const candidates = [];
  try {
    candidates.push(require.resolve('puppeteer-core'));
  } catch (e) {
    /* not local */
  }
  try {
    candidates.push(`${execSync('npm root -g', { encoding: 'utf8' }).trim()}/puppeteer-core/lib/puppeteer/puppeteer-core.js`);
  } catch (e) {
    /* no npm */
  }
  for (const c of candidates) if (fs.existsSync(c)) return (await import(c)).default;
  return null;
}

/** WebGPU on SwiftShader, the software GPU Chromium brings. */
export const CHROMIUM_ARGS = ['--enable-unsafe-webgpu', '--use-angle=swiftshader', '--ignore-gpu-blocklist', '--enable-features=Vulkan', '--use-vulkan=swiftshader'];
/** WebGPU on the software adapter lavapipe gives it, where it is not on by default. */
export const FIREFOX_PREFS = { 'dom.webgpu.enabled': true, 'gfx.webgpu.ignore-blocklist': true, 'gfx.webgpu.force-enabled': true, 'dom.webgpu.allow-software-adapter': true };

/**
 * Chromium with WebGPU on SwiftShader (and `more`, a test's own flags), and
 * the folder `root` served to it (null: nothing served): `{ browser, port,
 * close() }`.
 */
export async function chromium(more = [], { root = WEB, headless = true } = {}) {
  const pw = await loadPlaywright();
  const browser = await pw.chromium.launch({ headless, channel: 'chromium', args: [...CHROMIUM_ARGS, ...more] });
  if (!root) return { browser, close: () => browser.close() };
  const { srv, port } = await serve(root);
  return {
    browser,
    port,
    async close() {
      await browser.close();
      srv.close();
    },
  };
}

/**
 * What goes wrong on `page`, into `errors`: what it throws, and what it
 * logs as an error unless `expected` (a RegExp) says that is to be expected.
 * E2E_VERBOSE shows all it logs.
 */
export function watch(page, errors, expected = null) {
  page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));
  page.on('console', (m) => {
    if (m.type() === 'error' && !(expected && expected.test(m.text()))) errors.push('console: ' + m.text());
    if (process.env.E2E_VERBOSE) console.log('[browser]', m.type(), m.text());
  });
}

/** A Firefox page (puppeteer-core) with the few calls of Playwright's that the tests and the films make. */
export class FirefoxPage {
  constructor(page, viewport = null) {
    this.p = page;
    this.vp = viewport;
    this.mouse = {
      move: (x, y) => page.mouse.move(x, y),
      down: () => page.mouse.down(),
      up: () => page.mouse.up(),
      click: (x, y, o = {}) => page.mouse.click(x, y, o.clickCount ? { count: o.clickCount, clickCount: o.clickCount } : {}),
      wheel: (dx, dy) => page.mouse.wheel({ deltaX: dx, deltaY: dy }),
    };
    this.keyboard = page.keyboard;
  }

  goto(url) {
    return this.p.goto(url, { waitUntil: 'load' });
  }

  reload() {
    return this.p.reload({ waitUntil: 'load' });
  }

  waitForFunction(fn, arg, opts = {}) {
    return this.p.waitForFunction(fn, { timeout: opts.timeout || 30000, polling: opts.polling || 100 }, arg);
  }

  evaluate(fn, arg) {
    return this.p.evaluate(fn, arg);
  }

  click(sel) {
    return this.p.click(sel);
  }

  /** The file (or files) at `files` given to the file input `sel`. */
  async setInputFiles(sel, files) {
    await (await this.p.$(sel)).uploadFile(...[].concat(files));
  }

  async selectOption(sel, value) {
    await this.p.select(sel, value);
  }

  locator(sel) {
    const p = this.p;
    const one = {
      scrollIntoViewIfNeeded: async () => {
        const h = await p.$(sel);
        if (h) await h.evaluate((e) => e.scrollIntoView({ block: 'nearest', inline: 'nearest' }));
      },
      boundingBox: async () => {
        const h = await p.$(sel);
        return h ? h.boundingBox() : null;
      },
    };
    return { first: () => one };
  }

  viewportSize() {
    return this.vp;
  }

  screenshot(o) {
    return this.p.screenshot(o);
  }

  close() {
    return this.p.close();
  }
}

/** Until no job runs. */
export const idle = (page, timeout = 300000) => page.waitForFunction(() => !window.__unflash.state.job && document.querySelector('#jobbar').classList.contains('hidden'), null, { timeout, polling: 100 });

/**
 * From now on: what waits until a job has ended and none runs, for a job
 * started after this (by job(), or by the click of a scene that films it).
 * It waits on `state.jobEndedAt`, which the page keeps for this: a job
 * that ends before anything looks is not missed.
 */
export async function jobEnd(page) {
  await idle(page);
  const before = await page.evaluate(() => window.__unflash.state.jobEndedAt || 0);
  return (timeout = 300000) => page.waitForFunction((b) => !window.__unflash.state.job && (window.__unflash.state.jobEndedAt || 0) > b, before, { timeout, polling: 100 });
}

/**
 * Do `act` (a click, a file given to an input), which starts a job, and
 * wait until that job is over, however quickly it went; and a job it
 * starts in turn, without a wait between them (the scan an open starts).
 * What the page does as the job ends is done by then, up to its next wait:
 * a word it says once the project is saved may not be.
 */
export async function job(page, act, timeout = 300000) {
  const end = await jobEnd(page);
  await act();
  await end(timeout);
}

/**
 * Do `act`, which starts a job, and cancel the job with the job bar's
 * button as soon as its progress reaches `pct` percent (as its bar moves):
 * the name of the job cancelled (null: it ended before), once it is over.
 */
export async function cancelledJob(page, act, pct = 1) {
  await idle(page);
  await page.evaluate((at) => {
    const u = window.__unflash;
    window.__cancelled = null;
    window.__cancelWatch = new MutationObserver(() => {
      const job = u.state.job;
      if (!job || job.pct < at) return;
      window.__cancelWatch.disconnect();
      window.__cancelled = job.name;
      document.querySelector('#btnCancelJob').click();
    });
    window.__cancelWatch.observe(document.querySelector('#jobBar'), { attributes: true, attributeFilter: ['style'] });
  }, pct);
  await job(page, act);
  return page.evaluate(() => {
    window.__cancelWatch.disconnect();
    return window.__cancelled;
  });
}

/** The error banner's text, '' for none (an info banner, such as the built-in decoder's notice, is no error). */
export const errorBanner = (page) =>
  page.evaluate(() => {
    const b = document.querySelector('#banner');
    return b.classList.contains('hidden') || b.classList.contains('info') ? '' : document.querySelector('#bannerText').textContent;
  });

/** Open `file` (a path, or Playwright's `{ name, mimeType, buffer }`) as the file picker would, until the job of opening it is over. */
export async function open(page, file, timeout = 120000) {
  await job(page, () => page.setInputFiles('#fileInput', file), timeout);
  const banner = await errorBanner(page);
  if (banner) throw new Error(`opening ${typeof file === 'string' ? path.basename(file) : file.name} raised: ${banner}`);
}

/**
 * Scan the open video with the Scan button: `{ ms, frames, held,
 * violations, chunked, segments, toast }`, `toast` what the scan says it
 * found. (It says so once the project is saved, after its job: the toast
 * is cleared first, and waited for.)
 */
export async function scan(page, timeout = 300000) {
  await page.evaluate(() => (document.querySelector('#toast').textContent = ''));
  await job(page, () => page.click('#btnScan'), timeout);
  await page.waitForFunction(
    () => {
      const b = document.querySelector('#banner');
      return document.querySelector('#toast').textContent || !(b.classList.contains('hidden') || b.classList.contains('info'));
    },
    null,
    { timeout: 30000 }
  );
  const banner = await errorBanner(page);
  if (banner) throw new Error('the scan raised: ' + banner);
  return page.evaluate(() => {
    const s = window.__unflash.lastScan;
    return { ms: Math.round(s.elapsedMs), frames: s.frames, held: s.result.held, violations: s.result.violations, chunked: s.chunked || null, segments: s.segments, toast: document.querySelector('#toast').textContent };
  });
}

/**
 * Until the page is still: nothing of the tour gliding (its light and card
 * glide a fifth of a second to a new part, and again whenever the part they
 * light moves: a section's player takes its size a moment after it opens;
 * measured mid-glide, the card can be crossing the light), and the tour and
 * the thumbnails drawn in the frame grid the same three frames running.
 */
export async function still(page, timeout = 10000) {
  await page.evaluate(() => {
    window.__stillKey = null;
    window.__still = 0;
  });
  await page.waitForFunction(
    () => {
      const t = document.querySelector('.tour');
      if (t && t.getAnimations({ subtree: true }).some((a) => a.playState === 'running')) {
        window.__still = 0;
        return false;
      }
      const key = [document.querySelectorAll('#frameGrid .frame[data-drawn]').length];
      if (t) {
        const c = t.querySelector('.tour-card').getBoundingClientRect();
        const s = t.querySelector('.tour-spot').getBoundingClientRect();
        key.push(...[c.left, c.top, s.left, s.top, s.width, s.height].map(Math.round));
      }
      if (key.join() !== window.__stillKey) {
        window.__stillKey = key.join();
        window.__still = 0;
        return false;
      }
      return ++window.__still >= 3;
    },
    null,
    { timeout, polling: 'raf' }
  );
}

/** The flash clip's flashing, as the MP4 has it, in violations `v` read from another copy of it: a general flash at 3.9-5.5 s, a red one at 7.9-8.5 s. */
export function flashesAsInTheMp4(name, v) {
  const gen = v.find((x) => x.kind === 'flash');
  const red = v.find((x) => x.kind === 'red');
  console.log(`${name}: violations`, JSON.stringify(v.map((x) => [x.kind, x.start.toFixed(2), x.end.toFixed(2)])));
  assert(gen && gen.start > 3.5 && gen.start < 4.6 && gen.end > 5.2 && gen.end < 5.8, `${name}: general flash at 3.9-5.5 s: ${JSON.stringify(gen)}`);
  assert(red && red.start > 7.5 && red.start < 8.6 && red.end > 8.2 && red.end < 8.8, `${name}: red flash at 7.9-8.5 s: ${JSON.stringify(red)}`);
}

/**
 * Violations `a`, at least one, the same as `b`: kind for kind, each of
 * `keys` within `tol` (two decoders' or routes' colour conversions can put
 * the edges of a flash a frame apart: 0.05 s on start and end).
 */
export function sameViolations(name, a, b, tol = 1e-6, keys = ['start', 'end', 'onset', 'peak', 'count']) {
  assert(a.length > 0 && a.length === b.length, `${name}: the same violations: ${JSON.stringify(a)} vs ${JSON.stringify(b)}`);
  a.forEach((v, i) => {
    const w = b[i];
    assert(v.kind === w.kind && keys.every((k) => Math.abs(v[k] - w[k]) < tol), `${name}: violation ${i} differs: ${JSON.stringify(v)} vs ${JSON.stringify(w)}`);
  });
}
