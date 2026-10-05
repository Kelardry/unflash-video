// Project files and keeping a project in the browser: the WebM clip is
// scanned and one section marked; the project is saved to a file, every
// section deleted, and the file loaded back (sections, marks and the scan
// return); the same video opened again as a new copy (another modified
// time) finds its project; a project file for another video is turned down.
// The page keeps one connection to the database, and a save whose
// transaction fails as it commits settles all the same. The scan's trace
// is kept beside the project's record, written once rather than with every
// save; it comes back with a project file, with a copy's project, and from
// a record saved before it had a key of its own; a trace beside no project
// is taken for none.
//   node tests/e2e/project.mjs
import fs from 'node:fs';
import path from 'node:path';
import { MEDIA, OUT, assert, chromium, watch, idle, job, open } from './playwright.mjs';

const { browser, port, close } = await chromium();
const page = await browser.newPage({ viewport: { width: 1400, height: 1000 } });
const errors = [];
watch(page, errors, /Failed to load resource/i);
page.on('dialog', (d) => d.accept());
// how many connections the page opens to IndexedDB, and what each put
// writes: its key, and the bytes of a scan's trace in it (in the project's
// record, or beside it). The test reads and writes the database itself on
// connections of its own, which are not counted (__idbGet, __idbPut).
await page.addInitScript(() => {
  const open = IDBFactory.prototype.open;
  const put = IDBObjectStore.prototype.put;
  window.__dbOpens = 0;
  IDBFactory.prototype.open = function (...args) {
    window.__dbOpens++;
    return open.apply(this, args);
  };
  window.__puts = [];
  IDBObjectStore.prototype.put = function (value, key) {
    const trace = value && ((value.scan && value.scan.trace) || value.trace);
    window.__puts.push({ key: String(key), trace: trace ? Object.values(trace).reduce((n, a) => n + (ArrayBuffer.isView(a) ? a.byteLength : 0), 0) : 0 });
    return put.apply(this, arguments);
  };
  const store = async (mode, fn) => {
    const db = await new Promise((resolve, reject) => {
      const r = open.call(indexedDB, 'unflash', 1);
      r.onsuccess = () => resolve(r.result);
      r.onerror = () => reject(r.error);
    });
    try {
      return await new Promise((resolve, reject) => {
        const tx = db.transaction('projects', mode);
        const out = fn(tx.objectStore('projects'));
        tx.oncomplete = () => resolve(out);
        tx.onerror = () => reject(tx.error);
      });
    } finally {
      db.close();
    }
  };
  window.__idbGet = (keys) => store('readonly', (s) => keys.map((k) => s.get(k))).then((reqs) => reqs.map((r) => r.result));
  window.__idbPut = (key, value) => store('readwrite', (s) => void put.call(s, value, key));
});
/** What the database holds for project `key`: whether its record holds the trace itself and the stamp it names, and the trace kept beside it. */
const kept = (key) =>
  page.evaluate(async (key) => {
    const [v, tr] = await window.__idbGet([key, key + ':trace']);
    return { record: v ? { inline: !!(v.scan && v.scan.trace), traceAt: v.traceAt || null } : null, trace: tr ? { at: tr.at, frames: tr.trace.t.length } : null };
  }, key);
/** A trace of `n` frames, as a scan packs it (28 bytes a frame). */
const TRACE_BYTES = (n) => n * 28;
const bannerText = () => page.evaluate(() => (document.querySelector('#banner').classList.contains('hidden') ? '' : document.querySelector('#bannerText').textContent));
const sectionsNow = () => page.evaluate(() => window.__unflash.state.project.sections.map((s) => ({ id: s.id, start: s.start, end: s.end, edits: s.edits })));
const clip = fs.readFileSync(path.join(MEDIA, 'flash.webm'));
// the clip as a fresh file: each one gets its own modified time, as a copy would
const openClip = (name, buffer) => open(page, { name, mimeType: 'video/webm', buffer });
const results = {};

try {
  await page.goto(`http://127.0.0.1:${port}/?cpu=1&auto=0`);
  await page.waitForFunction(() => document.querySelector('#support').textContent.includes('WebGPU'), null, { timeout: 60000 });
  await openClip('flash.webm', clip);
  await job(page, () => page.click('#btnScan'));
  await page.click('#sectionList .sec-item');
  await page.waitForFunction(() => /passes|fails/.test(document.querySelector('#wsVerdict').textContent), null, { timeout: 180000 });
  await idle(page);
  // an E and an R
  for (const [slot, key] of [
    [5, 'e'],
    [6, 'r'],
  ]) {
    await page.click(`#frameGrid .frame:nth-child(${slot + 1})`);
    await page.click('#wsTitle');
    await page.keyboard.press(key);
  }
  results.before = await sectionsNow();
  const marked = results.before.find((s) => s.edits[5] && s.edits[6]);
  assert(marked && marked.edits[5].extended && marked.edits[6].removed, 'slot 5 held and slot 6 removed: ' + JSON.stringify(results.before));
  // the trace went into the database once, beside the record, with the save
  // after the scan; the saves after the check and each mark wrote the record
  // alone (each used to write the whole trace again)
  const key = await page.evaluate(() => window.__unflash.state.project.key);
  results.puts = await page.evaluate(() => window.__puts);
  {
    const withTrace = results.puts.filter((p) => p.trace > 0);
    const records = results.puts.filter((p) => p.key === key);
    console.log(`puts so far: ${results.puts.length}, ${records.length} of the record; with a trace: ${JSON.stringify(withTrace)}`);
    assert(withTrace.length === 1 && withTrace[0].key === key + ':trace' && withTrace[0].trace === TRACE_BYTES(300) && records.length >= 4, 'the trace is written once, beside the record, and the saves after it write the record alone: ' + JSON.stringify(results.puts));
  }

  // saved to a file
  await page.click('#btnProject');
  await page.waitForSelector('#projectMenu:not(.hidden)');
  results.note = await page.textContent('#projectNote');
  assert(/1 section, 2 marks, a scan/.test(results.note), 'the menu says what the project holds: ' + results.note);
  await page.click('#btnProjectSave');
  await page.waitForFunction(() => window.__unflash.lastProjectFile, null, { timeout: 10000 });
  const text = await page.evaluate(() => window.__unflash.lastProjectFile.text());
  const doc = JSON.parse(text);
  results.file = { bytes: text.length, kind: doc.kind, version: doc.version, video: doc.video, sections: doc.project.sections.length, trace: doc.project.scan && doc.project.scan.trace && doc.project.scan.trace.t && doc.project.scan.trace.t.$typed };
  console.log('project file:', JSON.stringify(results.file));
  assert(doc.kind === 'unflash-project' && doc.version === 1 && doc.video.name === 'flash.webm' && doc.video.frames === 300, 'the file names the video: ' + JSON.stringify(doc.video));
  assert(results.file.sections === results.before.length && results.file.trace === 'Float64Array', 'and holds the sections and the scan trace');

  // every section deleted, then the file loaded back
  await page.click('#btnDeleteAll');
  await page.waitForFunction(() => window.__unflash.state.project.sections.length === 0);
  await page.setInputFiles('#projectInput', { name: 'flash.unflash.json', mimeType: 'application/json', buffer: Buffer.from(text) });
  // (the toast comes last, once the project is kept in the browser too)
  await page.waitForFunction(() => /^Loaded /.test(document.querySelector('#toast').textContent), null, { timeout: 30000 });
  results.loaded = await sectionsNow();
  results.loadToast = await page.textContent('#toast');
  console.log('loaded:', results.loadToast);
  assert(JSON.stringify(results.loaded) === JSON.stringify(results.before), 'the sections and marks come back as they were: ' + JSON.stringify(results.loaded));
  assert(await page.evaluate(() => { const s = window.__unflash.state.project.scan; return !!(s && s.trace && s.trace.t.length === 300); }), 'and the scan trace');
  assert(/^Loaded flash\.unflash\.json: 1 section and the scan/.test(results.loadToast), 'the toast says what was loaded: ' + results.loadToast);
  // and the browser keeps the file's trace beside the record it saved, the stamp the record names on it
  results.loadedKept = await kept(key);
  assert(results.loadedKept.record && !results.loadedKept.record.inline && results.loadedKept.trace && results.loadedKept.trace.frames === 300 && results.loadedKept.trace.at === results.loadedKept.record.traceAt, "the loaded file's trace is kept beside its record: " + JSON.stringify(results.loadedKept));
  // the loaded section prepares and checks again when opened
  await page.click('#sectionList .sec-item');
  await page.waitForFunction(() => /passes|fails/.test(document.querySelector('#wsVerdict').textContent), null, { timeout: 180000 });
  await idle(page);
  assert(await page.evaluate(() => window.__unflash.currentSection().prepared), 'the loaded section prepares when opened');
  await page.evaluate(() => window.__unflash.state.project.save());
  // every save and load so far through one connection (one each, never
  // closed, they used to pile up)
  results.dbOpens = await page.evaluate(() => window.__dbOpens);
  assert(results.dbOpens === 1, 'the page opens the database once: ' + results.dbOpens);
  // a save whose transaction fails as it commits (out of space, say: only
  // its abort event tells) settles, a turn after the abort at the latest,
  // rather than holding up whatever waits on it
  results.abortedSave = await page.evaluate(async () => {
    const proto = IDBObjectStore.prototype;
    const put = proto.put;
    let hung;
    const afterAbort = new Promise((r) => (hung = r));
    proto.put = function (...args) {
      const req = put.apply(this, args);
      const tx = this.transaction;
      tx.addEventListener('abort', () => setTimeout(() => hung('still waiting a turn after the abort'), 0));
      req.addEventListener('success', () => tx.abort());
      return req;
    };
    try {
      return await Promise.race([window.__unflash.state.project.save().then(() => 'settled'), afterAbort]);
    } finally {
      proto.put = put;
    }
  });
  assert(results.abortedSave === 'settled', 'a save whose transaction aborts settles: ' + results.abortedSave);
  await page.evaluate(() => window.__unflash.state.project.save());

  // an export, kept on disk: after a reload the same video (a new copy of it)
  // finds it again, to download or verify, as does a file saved elsewhere
  await page.click('#btnExport');
  await page.waitForFunction(() => !document.querySelector('#exportModal').classList.contains('hidden') && !document.querySelector('#btnDoExport').disabled, null, { timeout: 30000 });
  results.beforeExport = await page.textContent('#exportResult');
  assert(/An export stays here/.test(results.beforeExport) && (await page.$eval('#btnVerifyExport', (b) => b.disabled)), 'before an export the dialog says where one will be: ' + results.beforeExport);
  await job(page, () => page.click('#btnDoExport'));
  await page.waitForFunction(() => !document.querySelector('#btnVerifyExport').disabled, null, { timeout: 30000 });
  const exportedBytes = await page.evaluate(() => window.__unflash.state.exportBlob.size);
  await page.click('#btnCloseExport');
  await page.reload();
  await page.waitForFunction(() => document.querySelector('#support').textContent.includes('WebGPU'), null, { timeout: 60000 });
  await openClip('flash.webm', clip);
  await page.click('#btnExport');
  await page.waitForFunction(() => !document.querySelector('#exportModal').classList.contains('hidden'), null, { timeout: 30000 });
  results.kept = { text: await page.textContent('#exportResult'), verify: !(await page.$eval('#btnVerifyExport', (b) => b.disabled)), download: !(await page.$eval('#exportDownload', (a) => a.classList.contains('hidden'))), bytes: await page.evaluate(() => window.__unflash.state.exportBlob && window.__unflash.state.exportBlob.size) };
  console.log('after a reload:', JSON.stringify(results.kept));
  assert(results.kept.verify && results.kept.download && results.kept.bytes === exportedBytes && /The export made at .* is still here/.test(results.kept.text), 'the export comes back with the video after a reload: ' + JSON.stringify(results.kept));
  await job(page, () => page.click('#btnVerifyExport'));
  await page.waitForFunction(() => /frames re-scanned/.test(document.querySelector('#exportResult').textContent), null, { timeout: 60000 });
  // a file saved elsewhere, checked from the dialog
  await job(page, () => page.setInputFiles('#verifyFileInput', path.join(MEDIA, 'steady.mp4')));
  await page.waitForFunction(() => /steady\.mp4: .*Passes WCAG/.test(document.querySelector('#exportResult').textContent), null, { timeout: 60000 });
  await page.click('#btnCloseExport');
  await page.evaluate(() => window.__unflash.state.project.save());

  // the same video again, as a new copy: its project is found by name and size
  await openClip('flash.webm', clip);
  results.reopen = { sections: await sectionsNow(), toast: await page.textContent('#toast') };
  console.log('reopened:', results.reopen.toast);
  assert(JSON.stringify(results.reopen.sections) === JSON.stringify(results.before), 'a copy of the video gets its project back: ' + JSON.stringify(results.reopen.sections));
  assert(/Restored the project saved for flash\.webm/.test(results.reopen.toast), 'and says so: ' + results.reopen.toast);
  // with its scan's trace, which the save after the restore puts beside it under the copy's own key
  await page.waitForFunction(() => {
    const p = window.__unflash.state.project;
    return p.scan && p.scan.trace && p.traceKept === p.scan.trace;
  }, null, { timeout: 30000 });
  {
    const copyKey = await page.evaluate(() => window.__unflash.state.project.key);
    results.reopenKept = { frames: await page.evaluate(() => window.__unflash.state.project.scan.trace.t.length), from: await page.evaluate(() => window.__unflash.state.project.restoredFrom), ...(await kept(copyKey)) };
    console.log('the copy\'s trace:', JSON.stringify(results.reopenKept));
    assert(copyKey !== results.reopenKept.from && results.reopenKept.frames === 300 && !results.reopenKept.record.inline && results.reopenKept.trace && results.reopenKept.trace.frames === 300 && results.reopenKept.trace.at === results.reopenKept.record.traceAt, "the copy's project brings its scan's trace: " + JSON.stringify(results.reopenKept));
  }

  // a project file for another video is turned down
  await open(page, path.join(MEDIA, 'steady.mp4'));
  await page.setInputFiles('#projectInput', { name: 'flash.unflash.json', mimeType: 'application/json', buffer: Buffer.from(text) });
  await page.waitForFunction(() => !document.querySelector('#banner').classList.contains('hidden'), null, { timeout: 10000 });
  results.mismatch = await bannerText();
  // another video opened: the export kept for the first is removed from the disk
  results.leftOver = await page.evaluate(async () => {
    const names = [];
    for await (const n of (await navigator.storage.getDirectory()).keys()) if (n.startsWith('unflash-export-')) names.push(n);
    return names;
  });
  assert(results.leftOver.length === 0, "another video's opening removes the kept export: " + results.leftOver);
  console.log('mismatch:', results.mismatch);
  assert(/The project is for flash\.webm \(300 frames/.test(results.mismatch) && (await sectionsNow()).length === 0, 'a project for another video is turned down: ' + results.mismatch);
  // and a file that is not a project
  await page.setInputFiles('#projectInput', { name: 'notes.json', mimeType: 'application/json', buffer: Buffer.from('{"hello": 1}') });
  await page.waitForFunction(() => /not an Unflash project file/.test(document.querySelector('#bannerText').textContent), null, { timeout: 10000 });

  // a project file is text anyone can write: markup in it does not become the
  // page's (a section's kinds and its id went into the list as HTML)
  await openClip('flash.webm', clip);
  const crafted = JSON.parse(text);
  const markup = (n) => `<img src="x" onerror="window.__ran = ${n}">`;
  const own = crafted.project.sections[0];
  own.kinds = [...own.kinds, markup(1)];
  crafted.project.sections.push({ ...own, id: markup(2), start: 9, end: 9.5 });
  await page.setInputFiles('#projectInput', { name: 'crafted.unflash.json', mimeType: 'application/json', buffer: Buffer.from(JSON.stringify(crafted)) });
  await page.waitForFunction(() => /^Loaded crafted\.unflash\.json/.test(document.querySelector('#toast').textContent), null, { timeout: 30000 });
  results.crafted = await page.evaluate(() => ({ images: document.querySelectorAll('#sectionList img, #workspace img').length, sections: window.__unflash.state.project.sections.map((s) => ({ id: s.id, kinds: s.kinds })), list: document.querySelector('#sectionList').textContent }));
  console.log('a crafted project file:', JSON.stringify(results.crafted));
  assert(results.crafted.images === 0 && results.crafted.sections.length === 1 && results.crafted.sections[0].kinds.every((k) => ['flash', 'red', 'extended', 'pattern'].includes(k)), 'markup in a project file does not become the page\'s: ' + JSON.stringify(results.crafted));

  // a record saved before the trace had a key of its own, the trace in it,
  // loads with its trace; the next save moves the trace out beside it (the
  // clip as a file on disk: opened twice, it is the same file, the same key)
  const onDisk = path.join(OUT, 'project-inline.webm');
  fs.mkdirSync(OUT, { recursive: true });
  fs.writeFileSync(onDisk, clip);
  await open(page, onDisk);
  const inlineKey = await page.evaluate(() => window.__unflash.state.project.key);
  await page.evaluate(async ([key, text]) => {
    const { readProjectFile } = await import('./project.js');
    await window.__idbPut(key, { ...readProjectFile(text).saved, savedAt: Date.now() });
  }, [inlineKey, text]);
  await open(page, onDisk);
  results.inline = await page.evaluate(() => {
    const p = window.__unflash.state.project;
    return { key: p.key, from: p.restoredFrom || null, sections: p.sections.length, frames: p.scan && p.scan.trace ? p.scan.trace.t.length : 0 };
  });
  assert(results.inline.key === inlineKey && !results.inline.from && results.inline.sections === 1 && results.inline.frames === 300, 'a record with the trace in it loads with its trace: ' + JSON.stringify(results.inline));
  await page.evaluate(() => window.__unflash.state.project.save());
  results.inlineMoved = await kept(inlineKey);
  console.log('a record with the trace in it, saved again:', JSON.stringify(results.inlineMoved));
  assert(!results.inlineMoved.record.inline && results.inlineMoved.trace && results.inlineMoved.trace.frames === 300 && results.inlineMoved.trace.at === results.inlineMoved.record.traceAt, 'and its next save moves the trace beside it: ' + JSON.stringify(results.inlineMoved));

  // a trace beside no project (its record gone) is no project: a copy of the
  // file finds nothing to restore, and it is no visit's last save (it says
  // it was saved later than anything, which a trace never does)
  await page.evaluate((key) => window.__idbPut(key, { at: 1, trace: { t: Float64Array.of(0), hazard: Uint32Array.of(0), hazardRed: Uint32Array.of(0), ext: Uint32Array.of(0), pattern: Uint32Array.of(0), lum: Float32Array.of(0) }, savedAt: Date.now() + 1e10 }), `orphan.webm:${clip.length}:1:trace`);
  await openClip('orphan.webm', clip);
  results.orphan = await page.evaluate(async () => {
    const p = window.__unflash.state.project;
    const { lastSavedAt } = await import('./project.js');
    return { from: p.restoredFrom || null, scan: !!p.scan, sections: p.sections.length, lastSaved: await lastSavedAt(), now: Date.now() };
  });
  console.log('a trace beside no project:', JSON.stringify(results.orphan));
  assert(!results.orphan.from && !results.orphan.scan && results.orphan.sections === 0, 'a trace is never restored as a project: ' + JSON.stringify(results.orphan));
  assert(results.orphan.lastSaved > 0 && results.orphan.lastSaved <= results.orphan.now, "nor counted as a project's last save: " + JSON.stringify(results.orphan));

  if (errors.length) throw new Error('page errors:\n' + errors.join('\n'));
  console.log('PROJECT OK');
} catch (e) {
  console.error(e);
  await page.screenshot({ path: path.join(OUT, 'project-failure.png') }).catch(() => {});
  process.exitCode = 1;
} finally {
  await close();
}
