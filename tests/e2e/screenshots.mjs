// The README's screenshots, made from the test clips (not a test): the start
// page with the tour's first steps, a scanned video with its chart, a
// section with what still fails, the same section fixed, and the export.
// Written to web/screenshots/ (the site serves them too).
//   node tests/e2e/screenshots.mjs
import fs from 'node:fs';
import path from 'node:path';
import { WEB, MEDIA, chromium, idle, open, scan, still } from './playwright.mjs';

const OUT = path.join(WEB, 'screenshots');
fs.mkdirSync(OUT, { recursive: true });

const { browser, port, close } = await chromium();
const VIEW = { width: 1280, height: 800 };
const shot = async (page, name, clip) => {
  await still(page);
  const file = path.join(OUT, `${name}.png`);
  await page.screenshot({ path: file, clip });
  console.log(`${name}.png: ${Math.round(fs.statSync(file).size / 1024)} KB`);
};

try {
  // the start page, the tour on its first steps
  let ctx = await browser.newContext({ viewport: VIEW });
  let page = await ctx.newPage();
  await page.goto(`http://127.0.0.1:${port}/?cpu=1&auto=0&tour=1`);
  await page.waitForSelector('.tour-card', { timeout: 30000 });
  await page.keyboard.press('ArrowRight');
  await shot(page, 'tour');
  await ctx.close();

  // a scanned video: the whole video, charted
  ctx = await browser.newContext({ viewport: VIEW });
  page = await ctx.newPage();
  await page.goto(`http://127.0.0.1:${port}/?cpu=1&auto=0`);
  await page.waitForFunction(() => window.__unflash && window.__unflash.changes, null, { timeout: 30000 });
  await open(page, path.join(MEDIA, 'flash.mp4'));
  await scan(page);
  await page.click('#chartSpan button[data-span="0"]');
  // the pointer over the flashing, for the readout
  const box = await page.$eval('#chart', (c) => {
    const r = c.getBoundingClientRect();
    return { x: r.x, y: r.y, w: r.width, h: r.height };
  });
  await page.mouse.move(box.x + box.w * 0.45, box.y + box.h * 0.6);
  await page.evaluate(() => document.querySelector('#toast').classList.add('hidden'));
  await shot(page, 'scan');

  // a section of the red-flash clip: what still fails
  await open(page, path.join(MEDIA, 'redflash.mp4'));
  await scan(page);
  await page.click('#sectionList .sec-item');
  await page.waitForFunction(() => /passes|fails/.test(document.querySelector('#wsVerdict').textContent), null, { timeout: 180000 });
  await idle(page);
  await page.evaluate(() => document.querySelector('#toast').classList.add('hidden'));
  await page.evaluate(() => document.querySelector('#workspace').scrollIntoView({ block: 'start' }));
  await shot(page, 'section');

  // fixed: the fewest removals, and the section passes
  await page.click('#btnSuggestFewest');
  await page.waitForFunction(() => /^passes/.test(document.querySelector('#wsVerdict').textContent), null, { timeout: 180000 });
  await idle(page);
  await page.evaluate(() => document.querySelector('#toast').classList.add('hidden'));
  await page.evaluate(() => document.querySelector('#workspace').scrollIntoView({ block: 'start' }));
  await shot(page, 'fixed');

  // the export, done
  await page.click('#btnExport');
  await page.waitForSelector('#exportModal', { state: 'visible' });
  await page.click('#btnDoExport');
  await page.waitForFunction(() => !document.querySelector('#btnVerifyExport').disabled, null, { timeout: 300000 });
  await idle(page);
  await page.click('#btnVerifyExport');
  await page.waitForFunction(() => /Passes WCAG/.test(document.querySelector('#exportResult').textContent), null, { timeout: 300000 });
  await idle(page);
  const modal = await page.$eval('#exportModal .modal-box', (m) => {
    const r = m.getBoundingClientRect();
    return { x: Math.max(0, r.x - 12), y: Math.max(0, r.y - 12), width: r.width + 24, height: r.height + 24 };
  });
  await shot(page, 'export', modal);
  await ctx.close();
} catch (e) {
  console.error(e);
  process.exitCode = 1;
} finally {
  await close();
}
