// The built-in HEVC, VP9, VP8 and AV1 decoders (the decoders module,
// web/pkg-dec) end to end: each flash clip scans with its built-in decoder
// (HEVC because the test browser has no decoder for it, the others because
// `?builtin=1` asks for one) and must find the flashing the browser's own
// decoder finds; and a hybrid scan of the VP9 clip, the browser's decoder
// and the built-in one side by side in chunks, must give exactly what one
// lane of the browser's decoder gives, the built-in decoder having decoded
// part of it. Then the workers of the built-in decoders, H.264's and the
// decoders module's, in trouble: a job that fails, a video closed while
// they decode, two samples at the same time.
//   node tests/e2e/decoders.mjs        (after ./build.sh)
import fs from 'node:fs';
import path from 'node:path';
import { MEDIA, OUT, assert, chromium, watch, open, scan, flashesAsInTheMp4, sameViolations } from './playwright.mjs';

const { browser, port, close } = await chromium();
const page = await browser.newPage({ viewport: { width: 1400, height: 1000 } });
const errors = [];
// (the player turning down a file it cannot play is expected)
watch(page, errors, /Failed to load resource|MEDIA_ERR|DEMUXER_ERROR|PIPELINE_ERROR/i);

/** Open `name` at `?query` and scan it: what decoded it and what the scan found. */
async function scanned(name, query) {
  await page.goto(`http://127.0.0.1:${port}/?auto=0${query ? '&' + query : ''}`);
  await page.waitForFunction(() => document.querySelector('#support').textContent.includes('WebGPU'), null, { timeout: 60000 });
  await open(page, path.join(MEDIA, name));
  const r = await scan(page);
  const decoded = await page.evaluate(() => {
    const u = window.__unflash;
    return { software: !!(u.state.decode && u.state.decode.software), builtIn: u.state.movie.builtIn ? u.state.movie.builtIn.name : null };
  });
  return { ...r, ...decoded };
}

const results = {};
try {
  for (const [name, query, codec] of [
    ['flash_hevc.mp4', '', 'HEVC'],
    ['flash.webm', 'builtin=1', 'VP9'],
    ['flash_vp8.webm', 'builtin=1', 'VP8'],
    ['flash_av1.mp4', 'builtin=1', 'AV1'],
  ]) {
    const r = await scanned(name, query);
    results[name] = r;
    console.log(`${name} ?${query}:`, JSON.stringify({ software: r.software, builtIn: r.builtIn, frames: r.frames }));
    assert(r.software && r.builtIn === codec, `${name}: decoded by the built-in ${codec} decoder: ${JSON.stringify(r)}`);
    assert(r.frames === 300, `${name}: every frame: ${r.frames}`);
    flashesAsInTheMp4(name, r.violations);
    if (!query) continue;
    // the browser's own decoder finds the same (its pictures reach the
    // detector by another route: a frame's difference at the edges at most)
    const b = await scanned(name, '');
    console.log(`${name} with the browser's decoder:`, JSON.stringify(b.violations.map((v) => [v.kind, v.start, v.end])));
    assert(!b.software, `${name}: the browser's decoder decodes it: ${JSON.stringify(b)}`);
    sameViolations(`${name}, the browser's decoder against the built-in one`, b.violations, r.violations, 0.05, ['start', 'end']);
  }

  // a hybrid scan in chunks of a second: one lane of the browser's decoder
  // and the built-in VP9 decoder's two workers, against the browser's
  // decoder on its own, in chunks as well (both hand the detector pictures
  // made small in workers): exactly the same
  const one = await scanned('flash.webm', 'hybrid=0&chunk=1&order=file');
  const two = await scanned('flash.webm', 'hybrid=1,2&chunk=1&order=file');
  console.log('VP9 in chunks, one lane:', JSON.stringify({ violations: one.violations.map((v) => [v.kind, v.start, v.end]), lanes: one.chunked && one.chunked.lanes.map((l) => [l.kind, l.frames]) }));
  console.log('VP9 in chunks, hybrid:', JSON.stringify({ violations: two.violations.map((v) => [v.kind, v.start, v.end]), lanes: two.chunked && two.chunked.lanes.map((l) => [l.kind, l.frames, l.failed]) }));
  assert(one.chunked && two.chunked && !one.chunked.fallback && !two.chunked.fallback, 'both scans ran in chunks: ' + JSON.stringify([one.chunked, two.chunked]));
  const builtInLane = two.chunked.lanes.find((l) => l.kind === 'built-in');
  assert(builtInLane && builtInLane.frames > 0 && !builtInLane.failed, 'the built-in VP9 decoder decoded part of the file: ' + JSON.stringify(two.chunked.lanes));
  assert(one.frames === two.frames && one.held === two.held, 'the same frames: ' + JSON.stringify([one.frames, one.held, two.frames, two.held]));
  sameViolations('one lane against the hybrid', one.violations, two.violations);

  // the built-in decoders' workers in trouble. A job whose samples can't be
  // read is answered, and the pass fails with its error, where it used to
  // wait for ever; the worker leaves the pool, and the video decodes again
  // through a new one. A video closed while its pool decodes (another file
  // opened while the frame viewer decodes) ends the pass, which also used
  // to wait for ever. And two samples at the same time each keep their
  // picture, in the workers and on the page (with H.264 the rest of their
  // group of pictures was lost)
  for (const name of ['flash_h264.mp4', 'flash_hevc.mp4']) {
    const r = await page.evaluate(
      async ([name, b64]) => {
        const wasm = await import('./pkg/unflash.js');
        const { Movie, decodeRange } = await import('./media.js');
        const { SoftwarePool } = await import('./h264pool.js');
        const file = new File([Uint8Array.from(atob(b64), (c) => c.charCodeAt(0))], name);
        const open = async () => {
          const m = await Movie.open(file, wasm);
          m.forceBuiltIn = true;
          await m.decoderSupport();
          return m;
        };
        // what a pass came to: its frames, its error, or (cut off here) none
        const outcome = (p) => Promise.race([p.then((frames) => ({ frames }), (e) => ({ error: e.message })), new Promise((r) => setTimeout(() => r({ hung: true }), 30000))]);
        const out = {};
        {
          const m = await open();
          m.pool = await SoftwarePool.create(m, 2);
          const workers = m.pool.workers.slice();
          // every sample at a place in a transport stream with no streams
          m.pool.movie = { ...m, ts: { packet: 188, size: file.size, tracks: [] }, v: { ...m.v, offset: m.v.offset.map(() => 2 ** 52) } };
          out.failed = await outcome(m.pool.decodeRange(0, m.v.pts.length, m.tsMin, m.tsMax, (f) => f.close()));
          out.left = m.pool.workers.filter((w) => workers.includes(w)).length;
          m.pool.movie = m;
          out.after = await outcome(decodeRange(m, m.tsMin, m.tsMax, (f) => f.close()));
          m.close();
        }
        {
          const m = await open();
          m.pool = await SoftwarePool.create(m, 1);
          let n = 0;
          out.closed = await outcome(
            decodeRange(m, m.tsMin, m.tsMax, (f) => {
              f.close();
              if (++n === 1) m.close();
            })
          );
        }
        {
          const m = await open();
          // the second and third samples (decode order) at one time: both are
          // decoded before the fourth, which is shown before them, and wait for it
          m.v.pts[2] = m.v.pts[1];
          out.sameTime = await outcome(decodeRange(m, m.tsMin, m.tsMax, (f) => f.close()));
          out.sameTimePage = await outcome(decodeRange(m, m.tsMin, m.tsMax, (f) => f.close(), { inline: true }));
          m.close();
        }
        return out;
      },
      [name, fs.readFileSync(path.join(MEDIA, name)).toString('base64')]
    );
    console.log(`${name}, its built-in decoder's workers in trouble:`, JSON.stringify(r));
    assert(/no stream for track 0/.test(r.failed.error || ''), `${name}: a job whose samples can't be read fails the pass with its error: ${JSON.stringify(r.failed)}`);
    assert(r.left === 0 && r.after.frames === 300, `${name}: the workers whose jobs failed leave the pool, and the video decodes again: ${JSON.stringify(r)}`);
    assert(r.closed.error === 'the decoder pool was closed', `${name}: a video closed while its pool decodes ends the pass: ${JSON.stringify(r.closed)}`);
    assert(r.sameTime.frames === 300 && r.sameTimePage.frames === 300, `${name}: two samples at one time each keep their picture, in the workers and on the page: ${JSON.stringify([r.sameTime, r.sameTimePage])}`);
  }

  if (errors.length) throw new Error('page errors:\n' + errors.join('\n'));
  console.log('DECODERS OK');
} catch (e) {
  console.error(e);
  await page.screenshot({ path: path.join(OUT, 'decoders-failure.png') }).catch(() => {});
  process.exitCode = 1;
} finally {
  await close();
}
