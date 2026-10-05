// Scan one local video file through the web app in headless Chromium and
// print what the detector found: violations, the summary line and, with
// --trace, the per-frame hazard / red-hazard / luminance trace.
//   node tests/e2e/scanfile.mjs file.mp4 [--cpu] [--trace] [--profile wcag|wcag_ext] [--segments N]
import path from 'node:path';
import { chromium, open, scan } from './playwright.mjs';

const file = path.resolve(process.argv[2]);
const cpu = process.argv.includes('--cpu');
const trace = process.argv.includes('--trace');
const pi = process.argv.indexOf('--profile');
const profile = pi > 0 ? process.argv[pi + 1] : null;
const si = process.argv.indexOf('--segments');
const segments = si > 0 ? parseInt(process.argv[si + 1], 10) : 0;
const { browser, port, close } = await chromium();
const page = await browser.newPage();
page.on('pageerror', (e) => console.log('[pageerror]', e.message));
page.on('console', (m) => { if (m.type() === 'error' || process.env.E2E_VERBOSE) console.log('[browser]', m.type(), m.text()); });
try {
  await page.goto(`http://127.0.0.1:${port}/?auto=0${cpu ? '&cpu=1' : ''}${segments > 0 ? `&segments=${segments}` : ''}`);
  await page.waitForFunction(() => document.querySelector('#support').textContent.includes('WebGPU'), null, { timeout: 60000 });
  await open(page, file);
  if (profile) {
    await page.selectOption('#profileSel', profile);
    await page.waitForFunction(() => document.querySelector('#toast').textContent.includes('Profile changed'), null, { timeout: 30000 });
  }
  console.log('info:', await page.textContent('#videoInfo'));
  console.log('status:', await page.textContent('#status'));
  const banner = await page.evaluate(() => (document.querySelector('#banner').classList.contains('hidden') ? '' : document.querySelector('#bannerText').textContent));
  if (banner) console.log('banner:', banner);
  const r = await scan(page, 600000);
  console.log('toast:', r.toast);
  const s = await page.evaluate(() => {
    const u = window.__unflash.state;
    return { trace: u.project.scan && u.project.scan.trace, route: u.env.feeder.route, area: u.env.feeder.det.area_thresh() };
  });
  console.log('route:', s.route, '| segments:', r.segments, '| area threshold (px):', s.area);
  console.log('violations:', JSON.stringify(r.violations, null, 1));
  if (trace && s.trace) {
    const t = s.trace;
    console.log('t, hazard, hazardRed, ext, lum, pattern');
    for (let i = 0; i < t.t.length; i++) console.log(t.t[i].toFixed(3), t.hazard[i], t.hazardRed[i], t.ext[i], t.lum[i] === undefined ? '' : t.lum[i].toFixed ? t.lum[i].toFixed(4) : t.lum[i], t.pattern[i]);
  }
  console.log(await page.evaluate(() => window.__unflash.profile.summary(1)));
} finally {
  await close();
}
