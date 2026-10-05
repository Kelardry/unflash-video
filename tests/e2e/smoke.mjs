// Smoke-test a deployed copy of the app: load it, open the published
// stripes test clip with its "open" button and scan it (one pattern), then
// open the local synthetic flashing clip and scan it (two violations).
//   node tests/e2e/smoke.mjs https://l1n.github.io/unflash-video/ [--gpu] [--extsrc=canvas|none]
// --extsrc makes the GPU detector take its pictures the way Firefox's
// WebGPU needs them (through a canvas) or as RGBA pixels.
import path from 'node:path';
import { MEDIA, chromium, errorBanner } from './playwright.mjs';

const url = process.argv[2] || 'http://127.0.0.1:8765/';
const useGpu = process.argv.includes('--gpu');
// SMOKE_CHROMIUM_ARGS adds launch flags (e.g. --disable-http2 behind a proxy that resets h2 tunnels)
const extraArgs = (process.env.SMOKE_CHROMIUM_ARGS || '').split(/\s+/).filter(Boolean);
const { browser } = await chromium(extraArgs, { root: null });
// SMOKE_IGNORE_TLS=1 for sandboxes whose outbound HTTPS is intercepted by a proxy CA
const page = await browser.newPage({ ignoreHTTPSErrors: !!process.env.SMOKE_IGNORE_TLS });
const errors = [];
page.on('pageerror', (e) => errors.push(e.message));
page.on('requestfailed', (r) => {
  if (!r.url().startsWith('blob:')) console.log('request failed:', r.url(), r.failure() && r.failure().errorText);
});
if (process.env.SMOKE_VERBOSE) page.on('console', (m) => console.log('[browser]', m.type(), m.text()));
try {
  const target = new URL(url);
  if (!useGpu) target.searchParams.set('cpu', '1');
  target.searchParams.set('auto', '0'); // these scans are pressed by hand; auto-fix is tried at the end
  const extsrc = process.argv.find((a) => a.startsWith('--extsrc='));
  if (extsrc) target.searchParams.set('extsrc', extsrc.slice('--extsrc='.length));
  const t0 = Date.now();
  await page.goto(target.toString(), { timeout: 120000 });
  console.log('page loaded', `(${Date.now() - t0} ms)`);
  try {
    await page.waitForFunction(() => document.querySelector('#support') && document.querySelector('#support').textContent.includes('WebGPU'), null, { timeout: 120000 });
  } catch (e) {
    const state = await page.evaluate(() => ({ support: document.querySelector('#support').textContent, banner: document.querySelector('#bannerText').textContent, status: document.querySelector('#status').textContent }));
    throw new Error('the app did not start: ' + JSON.stringify(state));
  }
  console.log('loaded:', await page.textContent('#support'), `(${Date.now() - t0} ms)`);
  const scan = async (expect) => {
    await page.waitForFunction(() => !document.querySelector('#btnScan').disabled, null, { timeout: 60000 });
    console.log('opened:', await page.textContent('#videoInfo'));
    console.log('status:', await page.textContent('#status'));
    const before = await page.textContent('#toast');
    await page.click('#btnScan');
    await page.waitForFunction((b) => document.querySelector('#toast').textContent !== b && document.querySelector('#toast').textContent.includes('found'), before, { timeout: 300000 });
    const toast = await page.textContent('#toast');
    console.log('scan:', toast);
    if (!expect.test(toast)) throw new Error('unexpected scan result: ' + toast);
  };
  // the published test clip, through the welcome page's button
  await page.click('[data-clip="stripes.mp4"]');
  await page.waitForFunction(() => document.querySelector('#videoInfo').textContent.includes('stripes.mp4') || !document.querySelector('#banner').classList.contains('hidden'), null, { timeout: 120000 });
  const banner = await errorBanner(page);
  if (banner) throw new Error('banner: ' + banner);
  await scan(/1 violation found \(1 regular pattern/);
  // a local file through the file input
  await page.setInputFiles('#fileInput', path.join(MEDIA, 'flash.mp4'));
  await page.waitForFunction(() => document.querySelector('#videoInfo').textContent.includes('flash.mp4'), null, { timeout: 60000 });
  await scan(/2 violations found/);
  // the published H.264 clip: WebCodecs where the browser has H.264, the built-in decoder otherwise
  await page.evaluate(() => window.__unflash.openClip('flash_h264.mp4'));
  await page.waitForFunction(() => document.querySelector('#videoInfo').textContent.includes('flash_h264.mp4') || !document.querySelector('#banner').classList.contains('hidden'), null, { timeout: 120000 });
  console.log('h264 status:', await page.textContent('#status'));
  await scan(/2 violations found/);
  // the unattended run (asked for: it is off unless ticked): open a file, and a fixed, checked export appears
  target.searchParams.set('auto', '1');
  await page.goto(target.toString(), { timeout: 120000 });
  await page.waitForFunction(() => document.querySelector('#support') && document.querySelector('#support').textContent.includes('WebGPU'), null, { timeout: 120000 });
  await page.setInputFiles('#fileInput', path.join(MEDIA, 'flash.mp4'));
  await page.waitForFunction(() => window.__unflash && window.__unflash.auto && Object.keys(window.__unflash.auto.steps).length > 0, null, { timeout: 120000 });
  await page.waitForFunction(() => !window.__unflash.auto.running, null, { timeout: 900000 });
  const auto = await page.evaluate(() => ({ steps: window.__unflash.auto.steps, summary: window.__unflash.auto.summary }));
  console.log('auto-fix:', JSON.stringify(auto));
  if (!(auto.steps.verify && auto.steps.verify.status === 'done')) throw new Error('auto-fix did not reach a checked export: ' + JSON.stringify(auto));
  console.log('SMOKE OK');
} finally {
  if (errors.length) console.log('page errors:', errors);
  await browser.close();
}
if (errors.length) process.exit(1);
