// Time the built-in (WebAssembly) H.264 decoder in headless Chromium.
//   node tests/e2e/h264bench.mjs [clip-url-relative-to-web ...]
import { chromium } from './playwright.mjs';
const { browser, port, close } = await chromium();
const page = await browser.newPage();
page.on('console', (m) => { if (m.type() === 'error') console.log('[browser error]', m.text()); });
page.on('pageerror', (e) => console.log('[pageerror]', e.message));
const clips = process.argv.slice(2);
if (!clips.length) clips.push('clips/flash_h264.mp4');
// H264_FAST=1 benchmarks the fast (no deblocking) decode used for scans
await page.goto(`http://127.0.0.1:${port}/bench.html${process.env.H264_FAST ? '?fast' : ''}`);
await page.waitForFunction(() => typeof window.runDecodeBench === 'function', null, { timeout: 60000 });
const workers = parseInt(process.env.H264_WORKERS || '0', 10) || 0;
for (const c of clips) {
  await page.evaluate((u) => window.runDecodeBench(u), c);
  console.log((await page.textContent('#dout')).trim().split('\n').pop());
  await page.evaluate(([u, w]) => window.runPoolBench(u, w), [c, workers]);
  console.log((await page.textContent('#dout')).trim().split('\n').pop());
}
await close();
