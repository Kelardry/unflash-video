// The page while a job runs. A second Scan click while a scan runs is
// turned away with a word, and the running scan finishes as if nothing had
// happened (it used to kill it: "can't access property "length",
// state.provisional is null"); so is a video opened meanwhile, and another
// profile chosen (the scan, made under the first, was kept as the second's).
// And a scan never keeps the page's thread for long, however far its
// decoders get ahead of the detector: a chunked scan fed every picture it
// held in one go, for seconds on a slow machine, and Firefox offered to stop
// the page; nor does a scan through the built-in decoder, whose workers
// decode ahead too. The detector is made slow here (a few milliseconds more
// a picture), as a slow machine's is, so that the decoders get ahead on a
// short clip.
//   node tests/e2e/busy.mjs
import path from 'node:path';
import { MEDIA, assert, chromium, idle, job, open } from './playwright.mjs';

const { browser, port, close } = await chromium();
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
const errors = [];
page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));
/** From now on, the tasks that keep the page's thread 50 ms or more (window.__long), and a slow machine's detector: 6 ms more a picture. */
const slowDetector = () =>
  page.evaluate(() => {
    window.__long = [];
    new PerformanceObserver((l) => {
      for (const e of l.getEntries()) window.__long.push(Math.round(e.duration));
    }).observe({ type: 'longtask' });
    const f = window.__unflash.state.env.feeder;
    const feed = f.videoFrame.bind(f);
    f.videoFrame = async (...a) => {
      const t = performance.now();
      while (performance.now() - t < 6);
      return feed(...a);
    };
    window.__unslow = () => delete f.videoFrame;
  });

try {
  // chunks of 2 s, so that the scan is a chunked one, and three decode lanes
  // (as Firefox's WebGPU route has up to six), which decode chunks ahead of
  // the one the detector is on
  await page.goto(`http://127.0.0.1:${port}/?cpu=1&auto=0&chunk=2&segments=3`);
  await page.waitForFunction(() => window.__unflash && window.__unflash.changes, null, { timeout: 30000 });
  await open(page, path.join(MEDIA, 'flash.webm'));
  await slowDetector();
  await page.evaluate(() => {
    // every word the page says, as it says it
    window.__toasts = [];
    const t = document.querySelector('#toast');
    new MutationObserver(() => window.__toasts.push(t.textContent)).observe(t, { childList: true, characterData: true, subtree: true });
  });

  await page.click('#btnScan');
  await page.waitForFunction(() => window.__unflash.state.job && window.__unflash.state.job.name === 'Scanning for flashes', null, { timeout: 10000 });
  // Scan again, another video and another profile, while it runs
  await page.click('#btnScan');
  await page.setInputFiles('#fileInput', path.join(MEDIA, 'steady.mp4'));
  await page.selectOption('#profileSel', 'wcag');
  const during = await page.evaluate(() => !!(window.__unflash.state.job && window.__unflash.state.scanning));
  await idle(page, 180000);
  const r = await page.evaluate(() => {
    window.__unslow();
    const u = window.__unflash;
    return {
      banner: document.querySelector('#banner').classList.contains('hidden') ? '' : document.querySelector('#bannerText').textContent,
      video: document.querySelector('#videoInfo').textContent,
      frames: u.lastScan ? u.lastScan.frames : 0,
      violations: u.lastScan ? u.lastScan.result.violations.map((v) => v.kind) : [],
      chunks: u.lastScan && u.lastScan.chunked ? u.lastScan.chunked.chunks : 0,
      peak: u.lastScan && u.lastScan.chunked ? u.lastScan.chunked.peak : 0,
      scanning: u.state.scanning,
      profile: [u.state.project.profile, document.querySelector('#profileSel').value, u.state.project.scan && u.state.project.scan.profile],
      long: window.__long.slice().sort((a, b) => b - a),
      toasts: window.__toasts.slice(),
    };
  });
  console.log(`scan: ${r.frames} frames in ${r.chunks} chunks, ${Math.round(r.peak / 1024)} KB of pictures held at most; violations ${r.violations.join(', ')}`);
  console.log(`tasks of 50 ms or more during it: ${r.long.length ? r.long.join(', ') + ' ms' : 'none'}`);
  assert(!r.banner, 'the scan was not killed: ' + r.banner);
  assert(r.frames === 300 && r.violations.join() === 'flash,red' && r.chunks > 1, 'the first scan finished, whole: ' + JSON.stringify(r));
  assert(/flash\.webm/.test(r.video) && r.scanning === null, 'the video opened meanwhile was not, and the scan left nothing behind: ' + r.video);
  assert(r.profile.every((p) => p === 'wcag_ext'), 'the profile chosen meanwhile was not, and the scan is kept as the profile it ran under: ' + JSON.stringify(r.profile));
  // (the decoders were ahead: dozens of pictures waited for the detector)
  assert(r.peak > 40 * 256 * 144 * 4, 'the decoders got ahead of the slowed detector: ' + r.peak);
  assert(!r.long.length || r.long[0] < 250, 'the scan never kept the page for long: ' + r.long.join(', '));
  assert(during, 'the first scan was still running when both were turned away');
  const said = (re) => r.toasts.some((x) => re.test(x));
  assert(said(/^Scanning for flashes is under way: scan once it has finished/) && said(/^Scanning for flashes is under way: open the video once it has finished/) && said(/^Scanning for flashes is under way: change the profile once it has finished/), 'and the page said why: ' + JSON.stringify(r.toasts));

  // the built-in decoder (HEVC, which this browser has no decoder for): its
  // workers decode ahead of the slowed detector, and the pictures they had
  // decoded were fed to it one after another, without a turn for the page
  await page.goto(`http://127.0.0.1:${port}/?cpu=1&auto=0`);
  await page.waitForFunction(() => window.__unflash && window.__unflash.changes, null, { timeout: 30000 });
  await open(page, path.join(MEDIA, 'flash_hevc.mp4'));
  await slowDetector();
  await job(page, () => page.click('#btnScan'), 180000);
  const b = await page.evaluate(() => {
    window.__unslow();
    const u = window.__unflash;
    return { builtIn: u.state.movie.builtIn ? u.state.movie.builtIn.name : null, frames: u.lastScan ? u.lastScan.frames : 0, long: window.__long.slice().sort((a, b) => b - a) };
  });
  console.log(`built-in ${b.builtIn} scan: ${b.frames} frames; tasks of 50 ms or more during it: ${b.long.length ? b.long.join(', ') + ' ms' : 'none'}`);
  assert(b.builtIn === 'HEVC' && b.frames === 300, 'the clip was scanned through the built-in decoder: ' + JSON.stringify(b));
  assert(!b.long.length || b.long[0] < 250, 'the scan through the built-in decoder never kept the page for long: ' + b.long.join(', '));

  if (errors.length) throw new Error('page errors:\n' + errors.join('\n'));
  console.log('BUSY OK');
} catch (e) {
  console.error(e);
  process.exitCode = 1;
} finally {
  await close();
}
