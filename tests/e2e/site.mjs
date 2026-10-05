// The site as it is published (site.mjs): each build's code in a folder of
// its own, v/<build>/, and the builds replaced lately kept beside it. Made
// here twice, build aaaa (keeping web/ as a site from before this layout)
// and then build bbbb keeping that, and served:
//   - the page loads its code from v/bbbb/ only (the modules, the style
//     sheet, the WebAssembly, the workers and, for a clip no decoder of the
//     browser's reads, the built-in decoders, the sound's included), and
//     knows its build;
//   - a page still open on build aaaa (its index.html, as the browser had
//     it) goes on loading aaaa's files when it next needs one, and says a
//     newer version is out;
//   - so does a page from before the layout, from the files kept at the top;
//   - a build replaced more than three days ago, or one that can't be
//     copied as it was published, is left out; --keep bounds how many stay.
//   node tests/e2e/site.mjs    (after ./build.sh)
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { ROOT, MEDIA, assert, chromium, open as openFile, scan as scanVideo } from './playwright.mjs';

const OUT = path.join(ROOT, 'tests/e2e/out/site');
const site = (...args) => execFileSync('node', [path.join(ROOT, 'site.mjs'), ...args], { cwd: ROOT, encoding: 'utf8' });
const versions = (dir) => JSON.parse(fs.readFileSync(path.join(dir, 'versions.json'), 'utf8'));

fs.rmSync(OUT, { recursive: true, force: true });
fs.mkdirSync(OUT, { recursive: true });
fs.copyFileSync(path.join(ROOT, 'CHANGELOG.md'), path.join(ROOT, 'web/CHANGELOG.md'));
const A = path.join(OUT, 'a');
const B = path.join(OUT, 'b');
const tA = new Date(Date.now() - 3600e3);
const tB = new Date();
console.log(site(A, '--build', 'aaaa', '--keep-from', path.join(ROOT, 'web'), '--now', tA.toISOString()).trim());
console.log(site(B, '--build', 'bbbb', '--keep-from', A, '--now', tB.toISOString()).trim());

// ---- what was made -----------------------------------------------------------
const vB = versions(B);
console.log('versions.json:', JSON.stringify(vB.builds.map((b) => [b.id, b.dir, b.until])));
assert(vB.current === 'bbbb' && vB.builds.map((b) => b.id).join() === 'bbbb,aaaa,unversioned', 'build bbbb, keeping aaaa and the files from before the layout: ' + JSON.stringify(vB.builds.map((b) => b.id)));
assert(vB.builds[1].until === tB.toISOString() && vB.builds[2].until === tA.toISOString(), 'each kept build says when it was replaced');
const index = fs.readFileSync(path.join(B, 'index.html'), 'utf8');
assert(index.includes('src="v/bbbb/app.js"') && index.includes('href="v/bbbb/style.css"'), 'index.html loads the code of build bbbb');
const bench = fs.readFileSync(path.join(B, 'bench.html'), 'utf8');
assert(bench.includes("'./v/bbbb/pkg/unflash.js'") && bench.includes("'./v/bbbb/media.js'"), 'bench.html too');
for (const b of vB.builds) for (const [rel, sha] of Object.entries(b.files)) assert(fs.existsSync(path.join(B, b.dir + rel)), `${b.id}: ${b.dir + rel} is there`);
// (the test clips are in web/clips where a test run or the Pages build put them)
const clip = fs.existsSync(path.join(ROOT, 'web/clips/flash.mp4'));
assert(fs.existsSync(path.join(B, 'whatsnew/shots.json')) && fs.existsSync(path.join(B, 'CHANGELOG.md')) && fs.existsSync(path.join(B, 'screenshots/scan.png')) && (!clip || fs.existsSync(path.join(B, 'clips/flash.mp4'))), 'the data stay at the top');
// the page as a browser had it from build aaaa, and from before the layout
fs.copyFileSync(path.join(A, 'index.html'), path.join(B, 'index-aaaa.html'));
fs.copyFileSync(path.join(ROOT, 'web/index.html'), path.join(B, 'index-unversioned.html'));

// ---- in the browser ------------------------------------------------------------
const { browser, port, close } = await chromium(['--autoplay-policy=no-user-gesture-required'], { root: B });
const results = {};
try {
  /** A page of the site, its requests for code (by folder) and anything not found. */
  async function open(page = '', query = 'cpu=1&auto=0&tour=0') {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const p = await ctx.newPage();
    const seen = { code: [], missing: [], errors: [] };
    // (requests from the page and from its workers)
    ctx.on('request', (r) => {
      const u = new URL(r.url());
      if (/\.(js|css|wasm)$/.test(u.pathname)) seen.code.push(u.pathname);
    });
    ctx.on('response', (r) => {
      if (r.status() >= 400) seen.missing.push(`${r.status()} ${new URL(r.url()).pathname}`);
    });
    p.on('pageerror', (e) => seen.errors.push(e.message));
    await p.goto(`http://127.0.0.1:${port}/${page}?${query}`);
    await p.waitForFunction(() => /WebGPU/.test(document.querySelector('#support').textContent), null, { timeout: 60000 });
    return { ctx, p, seen };
  }
  async function scan(p, name) {
    await openFile(p, path.join(MEDIA, name));
    const { violations } = await scanVideo(p);
    return { builtIn: await p.evaluate(() => (window.__unflash.state.movie.builtIn ? window.__unflash.state.movie.builtIn.name : null)), violations: violations.length };
  }
  /** The decoders module's sound decoder, loaded as the app loads it (from the folder of the code it runs). */
  const soundDecoder = (p) =>
    p.evaluate(async () => {
      const base = document.querySelector('script[type=module][src]').src;
      const { loadDecoders } = await import(new URL('codecs.js', base).href);
      return typeof (await loadDecoders()).SoundDecoder;
    });
  const folders = (code) => [...new Set(code.map((u) => (u.match(/^\/v\/[^/]+\//) || ['(top)'])[0]))];

  // --- the current build --------------------------------------------------------
  {
    const { ctx, p, seen } = await open();
    const h264 = await scan(p, 'flash_h264.mp4');
    const hevc = await scan(p, 'flash_hevc.mp4');
    const sound = await soundDecoder(p);
    const r = { support: await p.textContent('#support'), report: await p.evaluate(() => window.__unflash.debugReport()), banner: await p.evaluate(() => (document.querySelector('#banner').classList.contains('hidden') ? '' : document.querySelector('#bannerText').textContent)) };
    results.current = { h264, hevc, sound, folders: folders(seen.code), missing: seen.missing, errors: seen.errors };
    console.log('build bbbb:', JSON.stringify(results.current), '|', r.support);
    assert(h264.builtIn === 'H.264' && hevc.builtIn === 'HEVC' && h264.violations > 0 && hevc.violations > 0, 'the built-in decoders load and scan, from the folder of the build: ' + JSON.stringify([h264, hevc]));
    assert(sound === 'function', 'the decoders module has the sound decoder');
    assert(results.current.folders.join() === '/v/bbbb/' && seen.code.some((u) => u.endsWith('/softworker.js')) && seen.code.some((u) => u.endsWith('unflash_decoders_bg.wasm')), 'every piece of code comes from v/bbbb/: ' + JSON.stringify(results.current.folders));
    assert(!seen.missing.length && !seen.errors.length, 'nothing missing, no errors: ' + JSON.stringify([seen.missing, seen.errors]));
    assert(/, build bbbb/.test(r.support) && /App .*, build bbbb/.test(r.report), 'the page and its debug report name the build');
    assert(!/newer version/.test(r.banner), 'the current build says nothing about a newer one: ' + r.banner);
    await ctx.close();
  }

  // --- a page still open on build aaaa ---------------------------------------------
  {
    const { ctx, p, seen } = await open('index-aaaa.html');
    await p.waitForFunction(() => /newer version of Unflash is out/.test(document.querySelector('#bannerText').textContent), null, { timeout: 30000 });
    const hevc = await scan(p, 'flash_hevc.mp4');
    const sound = await soundDecoder(p);
    results.old = { hevc, sound, support: await p.textContent('#support'), folders: folders(seen.code), missing: seen.missing, errors: seen.errors, banner: await p.textContent('#bannerText') };
    console.log('a page on build aaaa:', JSON.stringify(results.old));
    assert(/, build aaaa/.test(results.old.support) && hevc.builtIn === 'HEVC' && hevc.violations > 0 && sound === 'function', 'the page of build aaaa runs, and decodes with its own built-in decoders: ' + JSON.stringify(results.old));
    assert(results.old.folders.join() === '/v/aaaa/', 'it loads every piece of code from v/aaaa/, its own: ' + JSON.stringify(results.old.folders));
    assert(!seen.missing.length && !seen.errors.length, 'nothing missing, no errors: ' + JSON.stringify([seen.missing, seen.errors]));
    // and says it once: dismissed and asked again, with the same build current, it doesn't repeat itself
    await p.click('#btnCloseBanner');
    await p.evaluate(() => window.__unflash.checkForUpdate(true));
    assert(await p.evaluate(() => document.querySelector('#banner').classList.contains('hidden')), 'the note about a newer build comes once for each');
    assert(await p.evaluate(() => !document.querySelector('#btnUpdate').classList.contains('hidden')), 'and the header keeps a button to reload with');
    await Promise.all([p.waitForNavigation(), p.click('#btnUpdate')]);
    await p.waitForFunction(() => /WebGPU/.test(document.querySelector('#support').textContent), null, { timeout: 60000 });
    results.reloaded = await p.textContent('#support');
    assert(/, build aaaa/.test(results.reloaded), 'the button reloads the page it is on (a browser with the newer index.html runs the newer build)');
    await ctx.close();
  }

  // --- a page from before the layout (its code at the top) ----------------------------
  {
    const { ctx, p, seen } = await open('index-unversioned.html');
    const hevc = await scan(p, 'flash_hevc.mp4');
    results.unversioned = { hevc, folders: folders(seen.code), missing: seen.missing, errors: seen.errors, build: await p.evaluate(() => window.__unflash.build) };
    console.log('a page from before the layout:', JSON.stringify(results.unversioned));
    assert(hevc.builtIn === 'HEVC' && hevc.violations > 0 && results.unversioned.folders.join() === '(top)' && results.unversioned.build === null, 'it runs on the files kept at the top: ' + JSON.stringify(results.unversioned));
    assert(!seen.missing.length && !seen.errors.length, 'nothing missing, no errors: ' + JSON.stringify([seen.missing, seen.errors]));
    await ctx.close();
  }
} finally {
  await close();
}

// ---- what is kept, and what not ---------------------------------------------------
{
  // four days on, only the build just replaced stays
  const C = path.join(OUT, 'c');
  const tC = new Date(tB.getTime() + 4 * 86400e3);
  site(C, '--build', 'cccc', '--keep-from', B, '--now', tC.toISOString());
  results.later = versions(C).builds.map((b) => b.id);
  assert(results.later.join() === 'cccc,bbbb' && !fs.existsSync(path.join(C, 'v/aaaa')) && !fs.existsSync(path.join(C, 'app.js')), 'builds replaced more than three days ago go: ' + results.later);
  // a day on, all three, unless --keep says fewer
  const D = path.join(OUT, 'd');
  site(D, '--build', 'dddd', '--keep-from', B, '--now', new Date(tB.getTime() + 86400e3).toISOString(), '--keep', '2');
  results.bounded = versions(D).builds.map((b) => b.id);
  assert(results.bounded.join() === 'dddd,bbbb', '--keep bounds how many builds stay: ' + results.bounded);
  // a build whose files can't be copied as they were published is left out, whole
  fs.appendFileSync(path.join(B, 'v/aaaa/app.js'), '\n// changed\n');
  const E = path.join(OUT, 'e');
  const log = site(E, '--build', 'eeee', '--keep-from', B, '--now', new Date(tB.getTime() + 3600e3).toISOString());
  results.damaged = versions(E).builds.map((b) => b.id);
  assert(results.damaged.join() === 'eeee,bbbb,unversioned' && !fs.existsSync(path.join(E, 'v/aaaa')) && /build aaaa: not every file/.test(log), 'a build that changed since it was published is left out: ' + results.damaged);
  console.log('kept later:', JSON.stringify({ later: results.later, bounded: results.bounded, damaged: results.damaged }));
}
console.log('SITE OK');
