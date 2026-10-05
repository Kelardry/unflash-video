// Run web/bench.html in headless Chromium and print the numbers.
import { chromium } from './playwright.mjs';
const { browser, port, close } = await chromium();
const page = await browser.newPage();
page.on('console', (m) => { if (m.type() === 'error') console.log('[browser error]', m.text()); });
const n = process.argv[2] || '120';
const src = process.argv[3] || '1920x1080';
const batch = process.argv[4]; // frames per GPU batch (default 16)
await page.goto(`http://127.0.0.1:${port}/bench.html${batch ? `?batch=${batch}` : ''}`);
await page.waitForFunction(() => typeof window.runBench === 'function', null, { timeout: 60000 });
await page.fill('#n', n);
await page.fill('#src', src);
await page.evaluate(() => window.runBench());
console.log(await page.textContent('#out'));
await close();
