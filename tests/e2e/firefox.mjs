// Unflash in Firefox: stock Firefox, headless, WebGPU on a software Vulkan
// (lavapipe), driven over WebDriver BiDi. Firefox is where the page runs
// most unlike the Chromium the other tests use: its WebGPU takes no
// VideoFrame, so the pictures reach the detector through the decode
// workers, copied out of its decoder; it has no long-task API; and it
// offers to stop a page that keeps its thread for long; and it puts a
// form's values back on a reload. Here, in Firefox: the export quality says
// what the slider holds after a reload; a scan on the GPU finds exactly
// what the CPU detector does; the page
// answers all through a scan, and a second Scan click is turned away while
// the first goes on; a section prepares, checks and is fixed by a Suggest
// button; in a scan in chunks, the built-in decoder takes over the
// chunks Firefox's slow lanes hold up, with the same result, and, rebalanced,
// sets the slow lanes aside and grows into their cores; a transport
// stream is read as ffmpeg reads it, scanned, and its AC-3 sound decoded;
// and the CPU detector's live monitor reports the flashing in the player,
// by whichever route Firefox's <video> allows (it makes BGRA VideoFrames,
// not the 4:2:0 ones the yuv route copies: pixels).
//   FIREFOX=/path/to/firefox node tests/e2e/firefox.mjs
// (puppeteer-core found locally or globally: npm install -g puppeteer-core)
import { execSync } from 'node:child_process';
import path from 'node:path';
import { serve } from './server.mjs';
import { readsAsTheMp4 } from './ts-parity.mjs';
import { WEB, MEDIA, assert, loadPuppeteer, FIREFOX_PREFS, FirefoxPage, idle, job, open, scan, sameViolations } from './playwright.mjs';

const CLIP = 'flash.webm';

function firefoxPath() {
  if (process.env.FIREFOX) return process.env.FIREFOX;
  try {
    return execSync('command -v firefox', { encoding: 'utf8', shell: '/bin/sh' }).trim();
  } catch (e) {
    throw new Error('no Firefox: set FIREFOX to its binary');
  }
}

const puppeteer = await loadPuppeteer();
if (!puppeteer) throw new Error('puppeteer-core not found; npm install -g puppeteer-core');
const { srv, port } = await serve(WEB);
const browser = await puppeteer.launch({ browser: 'firefox', executablePath: firefoxPath(), headless: true, extraPrefsFirefox: FIREFOX_PREFS, protocolTimeout: 600000 });
const errors = [];
const results = {};

/** A page at `?query`, as Playwright's (FirefoxPage), once the app is up. */
async function newPage(query) {
  const p = await browser.newPage();
  p.on('pageerror', (e) => errors.push(`pageerror (${query}): ${e.message}`));
  const page = new FirefoxPage(p);
  await page.goto(`http://127.0.0.1:${port}/?${query}`);
  await page.waitForFunction(() => window.__unflash && window.__unflash.changes, null, { timeout: 60000 });
  return page;
}
const openClip = (page, name = CLIP) => open(page, path.join(MEDIA, name));
function same(name, r, w) {
  assert(r.frames === w.frames && r.held === w.held, `${name}: frames ${r.frames} (held ${r.held}) vs ${w.frames} (held ${w.held})`);
  sameViolations(name, r.violations, w.violations);
}

try {
  // --- the CPU detector's reading of the clip, for the GPU's to match --------
  const cpu = await newPage('cpu=1&auto=0&hybrid=0&monitor=detect');
  await openClip(cpu);
  results.cpu = await scan(cpu);
  // the live monitor on the CPU detector, stepped through the flashing a frame at a time
  await cpu.click('#liveToggle');
  await cpu.waitForFunction(() => window.__unflash.state.live.on);
  const liveSeen = new Set();
  for (let k = 75; k <= 180; k++) liveSeen.add(await cpu.evaluate((k) => window.__unflash.liveStep((k + 0.5) / 30), k));
  results.live = { seen: [...liveSeen], route: await cpu.evaluate(() => window.__unflash.state.liveFeeder.route) };
  console.log('the CPU live monitor:', JSON.stringify(results.live));
  assert(results.live.seen.some((s) => /^flashing|violations? so far/.test(s)), 'the CPU live monitor reports the flashing: ' + JSON.stringify(results.live));
  assert(['yuv', 'pixels'].includes(results.live.route), "it reads the player's pictures by a route the CPU detector has: " + results.live.route);
  await cpu.close();
  const kinds = results.cpu.violations.map((v) => v.kind);
  assert(kinds.includes('flash') && kinds.includes('red'), 'the clip flashes, and red: ' + JSON.stringify(results.cpu.violations));

  // --- the export quality after a reload: Firefox puts a form's values back
  // on a reload, and put the slider back where it was (10, say) with the
  // number beside it at the page's 7; the export went by the slider --------
  {
    const q = await newPage('cpu=1&auto=0');
    const set = await q.evaluate(() => {
      const s = document.querySelector('#exportQuality');
      s.value = '10';
      s.dispatchEvent(new Event('input', { bubbles: true }));
      return document.querySelector('#exportQualityText').textContent;
    });
    await q.reload();
    await q.waitForFunction(() => window.__unflash && window.__unflash.changes, null, { timeout: 60000 });
    results.quality = { set, after: await q.evaluate(() => [document.querySelector('#exportQuality').value, document.querySelector('#exportQualityText').textContent]) };
    await q.close();
    console.log('export quality, set and after a reload:', JSON.stringify(results.quality));
    assert(set === '10 of 10' && results.quality.after[0] === '7' && results.quality.after[1] === '7 of 10', 'the quality shows its scale, and says what the slider holds after a reload: ' + JSON.stringify(results.quality));
  }

  // --- WebGPU, the pictures through the decode workers -----------------------
  const page = await newPage('auto=0&hybrid=0');
  await openClip(page);
  results.env = await page.evaluate(() => {
    const u = window.__unflash.state;
    const f = u.env.feeder;
    return { ua: navigator.userAgent, support: document.querySelector('#support').textContent, backend: f.backend, route: f.routeDetail || f.route || '', workers: !!u.movie.decodeInWorkers };
  });
  console.log('firefox:', JSON.stringify(results.env));
  assert(/Firefox\//.test(results.env.ua) && results.env.backend === 'webgpu' && /WebGPU/.test(results.env.support), 'Firefox runs the detector on WebGPU: ' + JSON.stringify(results.env));
  assert(results.env.workers, "Firefox's WebGPU takes no VideoFrame: the clip is decoded in workers");
  // the page's thread through a scan: the longest gap between 10 ms ticks
  // (no long-task API here); and a second Scan click while it runs
  await page.evaluate(() => {
    window.__gaps = [];
    let last = performance.now();
    window.__tick = setInterval(() => {
      const now = performance.now();
      window.__gaps.push(now - last);
      last = now;
    }, 10);
    window.__toasts = [];
    const t = document.querySelector('#toast');
    new MutationObserver(() => window.__toasts.push(t.textContent)).observe(t, { childList: true, characterData: true, subtree: true });
  });
  await page.click('#btnScan');
  await page.waitForFunction(() => window.__unflash.state.job, null, { timeout: 30000, polling: 20 });
  await page.click('#btnScan');
  await idle(page);
  results.gpu = await page.evaluate(() => {
    const s = window.__unflash.lastScan;
    return { ms: Math.round(s.elapsedMs), frames: s.frames, held: s.result.held, violations: s.result.violations };
  });
  const held = await page.evaluate(() => {
    clearInterval(window.__tick);
    return { gaps: window.__gaps.slice().sort((a, b) => b - a).slice(0, 5).map(Math.round), toasts: window.__toasts };
  });
  console.log('scan on WebGPU:', results.gpu.ms, 'ms |', results.gpu.frames, 'frames | longest gaps between ticks:', held.gaps.join(', '), 'ms');
  same('WebGPU in Firefox against the CPU detector', results.gpu, results.cpu);
  assert(held.toasts.some((t) => /^Scanning for flashes is under way: scan once it has finished/.test(t)), 'the second Scan click is turned away with a word: ' + JSON.stringify(held.toasts));
  // (the page used to be held for seconds at a time, and Firefox offered to stop it)
  assert(held.gaps[0] < 1000, 'the page answers all through the scan: the longest gap between ticks was ' + held.gaps[0] + ' ms');

  // --- a section: prepared through the workers, checked, fixed ---------------
  await page.click('#sectionList .sec-item');
  await page.waitForFunction(() => /passes|fails/.test(document.querySelector('#wsVerdict').textContent), null, { timeout: 180000, polling: 200 });
  const verdict = () => page.evaluate(() => document.querySelector('#wsVerdict').textContent);
  results.sectionBefore = await verdict();
  assert(/^fails/.test(results.sectionBefore), 'the first section fails as it is: ' + results.sectionBefore);
  await job(page, () => page.click('#btnSuggestFewest'));
  await page.waitForFunction(() => /passes|fails/.test(document.querySelector('#wsVerdict').textContent), null, { timeout: 60000, polling: 200 });
  results.sectionAfter = await verdict();
  console.log('section:', results.sectionBefore, '→ after fewest removals:', results.sectionAfter);
  assert(/^passes/.test(results.sectionAfter), 'fewest removals fixes it: ' + results.sectionAfter);
  // the export dialog: the file's name, and how to have Firefox ask where to save it (it has no save dialog for pages)
  await page.click('#btnExport');
  await page.waitForFunction(() => !document.querySelector('#exportModal').classList.contains('hidden') && document.querySelector('#exportName').value, null, { timeout: 30000 });
  results.exportWhere = await page.evaluate(() => ({ name: document.querySelector('#exportName').value, where: document.querySelector('#exportWhere').textContent }));
  assert(/\.unflashed\.mp4$/.test(results.exportWhere.name) && /Always ask you where to save files/.test(results.exportWhere.where), "in Firefox the export dialog says how to choose the folder: " + JSON.stringify(results.exportWhere));
  await page.click('#btnCloseExport');
  await page.close();

  // --- a scan in chunks: Firefox's lanes made slow, the built-in decoder's idle
  // lane takes over the chunks the detector waits for, from the last picture in
  const lanes = await newPage('auto=0&hybrid=2,2&chunk=1&order=file&hold=13&slowlanes=150&rebalance=0');
  await openClip(lanes);
  results.takenOver = await scan(lanes);
  const h = results.takenOver.chunked;
  console.log('scan in chunks, slow Firefox lanes:', results.takenOver.ms, 'ms |', JSON.stringify(h && { chunks: h.chunks, steals: h.steals, lanes: h.lanes.map((l) => [l.kind, l.workers, l.frames, l.chunks, l.steals, l.stolen]) }));
  assert(h && h.chunks >= 8 && !h.fallback, 'a scan in chunks: ' + JSON.stringify(h));
  same('taken over', results.takenOver, results.cpu);
  assert(h.lanes.reduce((a, l) => a + l.frames, 0) === results.takenOver.frames, 'each picture decoded once: ' + JSON.stringify(h.lanes));
  const builtIn = h.lanes.find((l) => l.kind === 'built-in');
  assert(builtIn && h.steals >= 1 && builtIn.steals === h.steals && h.lanes.some((l) => l.kind !== 'built-in' && l.stolen > 0), "the built-in lane took chunks over from Firefox's: " + JSON.stringify(h));
  await lanes.close();

  // --- the same, rebalanced: once the lanes are timed, Firefox's slow ones are
  // set aside and the built-in decoder grows into their cores, with the same result
  const balanced = await newPage('auto=0&hybrid=2,2&chunk=1&order=file&hold=13&slowlanes=150');
  await openClip(balanced);
  results.rebalanced = await scan(balanced);
  const b = results.rebalanced.chunked;
  console.log('scan in chunks, rebalanced:', results.rebalanced.ms, 'ms |', JSON.stringify(b && { grown: b.grown, lanes: b.lanes.map((l) => [l.kind, l.workers, l.frames, l.chunks, l.steals, l.stolen, l.aside]) }));
  same('rebalanced', results.rebalanced, results.cpu);
  assert(b.lanes.reduce((a, l) => a + l.frames, 0) === results.rebalanced.frames, 'each picture decoded once: ' + JSON.stringify(b.lanes));
  assert(b.rebalanced && b.lanes.filter((l) => l.kind !== 'built-in').every((l) => l.aside > 0) && b.grown >= 1, "Firefox's slow lanes were set aside and the built-in decoder grew: " + JSON.stringify(b));
  await balanced.close();

  // --- a transport stream (Blu-ray's .m2ts): read as ffmpeg reads it, scanned,
  // its AC-3 sound decoded by the app's own decoder
  const ts = await newPage('cpu=1&auto=0&hybrid=0');
  await ts.evaluate(() => {
    const i = document.createElement('input');
    i.type = 'file';
    i.id = 'probeFiles';
    i.multiple = true;
    i.hidden = true;
    document.body.append(i);
  });
  await ts.setInputFiles('#probeFiles', [path.join(MEDIA, 'flash_ac3.m2ts'), path.join(MEDIA, 'flash_ac3_m2ts.mp4')]);
  results.tsParity = await ts.evaluate(readsAsTheMp4, '#probeFiles');
  console.log('a transport stream, read as ffmpeg reads it:', JSON.stringify(results.tsParity));
  const p = results.tsParity[0];
  assert(p && p.format === 'mpegts' && p.packet === 192 && p.bad.length === 0 && p.videoSamples === 300 && p.audioSamples > 300, 'every sample of the .m2ts as in ffmpeg\'s MP4: ' + JSON.stringify(p));
  await openClip(ts, 'flash_ac3.m2ts');
  results.ts = await scan(ts);
  const tsKinds = results.ts.violations.map((v) => `${v.kind} ${v.start.toFixed(1)}`);
  console.log('the .m2ts scanned:', results.ts.frames, 'frames |', tsKinds.join(', '));
  assert(results.ts.frames === 300 && results.ts.violations.some((v) => v.kind === 'flash' && Math.abs(v.start - 4) < 0.3) && results.ts.violations.some((v) => v.kind === 'red' && Math.abs(v.start - 8) < 0.3), 'the .m2ts flashes where the clip does: ' + JSON.stringify(results.ts.violations));
  results.tsSound = await ts.evaluate(async () => {
    const { soundConfig, soundDecoderFor } = await import('./audiodec.js');
    const m = window.__unflash.state.movie;
    const cfg = soundConfig(m.audio, m.dx.track_description(m.audio.index));
    const found = await soundDecoderFor(cfg);
    if (!found) return { none: true };
    let frames = 0;
    let error = null;
    const dec = new found.Decoder({
      output: (d) => {
        frames += d.numberOfFrames;
        d.close();
      },
      error: (e) => (error = String(e)),
    });
    dec.configure(cfg);
    for (let i = 0; i < m.a.size.length; i++) dec.decode(new EncodedAudioChunk({ type: 'key', timestamp: m.a.pts[i], data: (await m.reader.read(m.a.offset[i], m.a.size[i])).slice() }));
    await dec.flush();
    return { name: found.name, seconds: frames / 48000, error };
  });
  console.log('its sound:', JSON.stringify(results.tsSound));
  assert(results.tsSound.name === 'AC-3' && !results.tsSound.error && Math.abs(results.tsSound.seconds - 10) < 0.1, "the .m2ts's AC-3 sound decodes by the app's own decoder: " + JSON.stringify(results.tsSound));
  await ts.close();

  if (errors.length) throw new Error('page errors:\n' + errors.join('\n'));
  console.log('FIREFOX OK');
} catch (e) {
  console.error(e);
  process.exitCode = 1;
} finally {
  await browser.close();
  srv.close();
}
