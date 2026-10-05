// The sound of held frames. A frame marked E is held a second longer, and
// the sound must wait with it: silence under the held frame, the rest of the
// sound after it as much later as the pictures. First the placing itself
// (SoundRun, sample for sample, in Node); then in the browser: the WebM clip
// (VP9 + a 440 Hz tone in Opus) gets one E mark and is exported, and the
// exported file's sound is decoded and must be silent exactly while the held
// frame waits, and nowhere else. The section player's sound, turned on,
// must have the same silence, keep to the pictures' clock and reach the
// speakers (the browser as it comes: sound only after a click); its button
// says "sound on" only while there is sound to be had: at ½× it plays at
// 1× only, and where the browser holds its audio back or can't decode the
// sound, the button says so; turned on while the player is paused, it
// waits for it. E-AC-3 sound (Dolby Digital Plus), which no
// browser's WebCodecs decodes, plays through the app's own decoder, and an
// export with a held frame re-encodes it with the silence in place.
//   node tests/e2e/sound.mjs
import path from 'node:path';
import { MEDIA, OUT, assert, chromium, watch, job, open } from './playwright.mjs';
import { SoundRun } from '../../web/sound.js';

// --- SoundRun on its own: where every sample lands ---------------------------
{
  const rate = 1000;
  const holds = [
    { at: 0.25, seconds: 0.1 }, // inside the first piece
    { at: 0.5, seconds: 0.2 }, // exactly between two pieces
  ];
  const out = new Float32Array(2000).fill(NaN);
  let first = null;
  const run = new SoundRun(rate, 1, holds, (planes, n, start) => {
    if (first === null) first = start;
    out.set(planes[0].subarray(0, n), start);
  }, { block: 64 });
  // source sample i carries the value 1 + i (so each is recognisable), in pieces of 100
  for (let p = 0; p < 10; p++) {
    const plane = new Float32Array(100);
    for (let j = 0; j < 100; j++) plane[j] = 1 + p * 100 + j;
    run.addPlanes([plane], 100, p / 10, rate);
  }
  run.finish();
  const fade = run.fade; // 5 samples at 1 kHz
  assert(first === 0 && run.placed === 2, `the run starts at the first sample and places both holds: ${first}, ${run.placed}`);
  // source sample i lands at i + the hold samples at or before it
  const where = (i) => i + (i >= 250 ? 100 : 0) + (i >= 500 ? 200 : 0);
  for (let i = 0; i < 1000; i++) {
    const v = out[where(i)];
    const near = [250, 500].some((h) => (i >= h - fade && i < h) || (i >= h && i < h + fade));
    if (near) assert(v > 0 && v < 1 + i, `sample ${i} is faded: ${v}`);
    else assert(v === 1 + i, `source sample ${i} at ${where(i)}: ${v}`);
  }
  for (let k = 250; k < 350; k++) assert(out[k] === 0, `silence under the first hold at ${k}: ${out[k]}`);
  for (let k = 600; k < 800; k++) assert(out[k] === 0, `silence under the second hold at ${k}: ${out[k]}`);
  assert(Number.isNaN(out[1300]) && out[1299] === 1000, 'and nothing after the sound');
  // the rule the frames follow
  assert(SoundRun.outTime(holds, 0.2) === 0.2 && SoundRun.outTime(holds, 0.25) === 0.35 && Math.abs(SoundRun.outTime(holds, 0.6) - 0.9) < 1e-12, 'outTime adds every hold at or before');
  // a gap in the source stays silent; an overlap keeps what came first
  const got = [];
  const g = new SoundRun(rate, 1, [], (planes, n, start) => got.push([start, Array.from(planes[0].subarray(0, n))]), { block: 8 });
  g.addPlanes([Float32Array.from([1, 2, 3, 4])], 4, 0, rate);
  g.addPlanes([Float32Array.from([5, 6])], 2, 0.006, rate); // two samples late
  g.addPlanes([Float32Array.from([7, 8, 9])], 3, 0.007, rate); // one sample early
  g.finish();
  const flat = [];
  for (const [start, v] of got) v.forEach((x, j) => (flat[start + j] = x));
  assert(JSON.stringify(flat) === JSON.stringify([1, 2, 3, 4, 0, 0, 5, 6, 8, 9]), 'gaps silent, overlaps trimmed: ' + JSON.stringify(flat));
  console.log('SoundRun OK');
}

/**
 * What an export holds (on page `p`, its download link): the pictures'
 * times, and its sound decoded, with where it is quiet (5 ms windows).
 */
const exportedSound = (p) =>
  p.evaluate(async () => {
    const wasm = await import('./pkg/unflash.js');
    const { Movie } = await import('./media.js');
    const blob = await (await fetch(document.querySelector('#exportDownload').href)).blob();
    const m = await Movie.open(new File([blob], 'exported.mp4'), wasm);
    const pts = Array.from(m.v.pts).sort((a, b) => a - b).map((t) => t / 1e6);
    const at = m.audio;
    const a = m.a;
    const desc = m.dx.track_description(at.index);
    const cfg = { codec: at.codec, sampleRate: at.sample_rate, numberOfChannels: at.channels };
    if (desc.length) cfg.description = desc;
    const pieces = [];
    let error = null;
    const dec = new AudioDecoder({
      output: (d) => {
        const x = new Float32Array(d.numberOfFrames);
        d.copyTo(x, { planeIndex: 0, format: 'f32-planar' });
        pieces.push({ t: d.timestamp / 1e6, rate: d.sampleRate, x });
        d.close();
      },
      error: (e) => (error = e),
    });
    dec.configure(cfg);
    for (let i = 0; i < a.offset.length; i++) {
      const bytes = await m.reader.read(a.offset[i], a.size[i]);
      dec.decode(new EncodedAudioChunk({ type: 'key', timestamp: a.pts[i], duration: a.dur[i], data: bytes.slice() }));
    }
    await dec.flush();
    if (error) throw error;
    // loud or quiet, in 5 ms windows along the sound's own timeline
    const rate = pieces[0].rate;
    const win = Math.round(rate * 0.005);
    const t0 = pieces[0].t;
    const total = pieces.reduce((s, p) => s + p.x.length, 0);
    const all = new Float32Array(total);
    let o = 0;
    for (const p of pieces) {
      all.set(p.x, o);
      o += p.x.length;
    }
    const quiet = [];
    for (let k = 0; k + win <= total; k += win) {
      let peak = 0;
      for (let j = k; j < k + win; j++) peak = Math.max(peak, Math.abs(all[j]));
      quiet.push(peak < 0.01);
    }
    // the quiet stretches, in seconds
    const runs = [];
    for (let k = 0; k < quiet.length; ) {
      if (!quiet[k]) {
        k++;
        continue;
      }
      let j = k;
      while (j < quiet.length && quiet[j]) j++;
      runs.push([t0 + (k * win) / rate, t0 + (j * win) / rate]);
      k = j;
    }
    return { codec: at.codec, rate, soundStart: t0, soundSeconds: total / rate, quiet: runs.filter(([s, e]) => e - s >= 0.05), pts, duration: m.duration };
  });

// --- in the browser: an E mark exported -----------------------------------------
// (no flag lets this Chromium play sound before a click: sound comes after one, as for anyone)
const { browser, port, close } = await chromium();
const page = await browser.newPage({ viewport: { width: 1400, height: 1000 } });
const errors = [];
watch(page, errors, /Failed to load resource/i);
const results = {};

/** Page `p` at the app, the clip `name` opened in it and scanned, and its first section prepared and checked. */
async function inSection(p, name) {
  await p.goto(`http://127.0.0.1:${port}/?cpu=1&auto=0&tour=0`);
  await p.waitForFunction(() => window.__unflash && window.__unflash.changes, null, { timeout: 60000 });
  await open(p, path.join(MEDIA, name));
  await job(p, () => p.click('#btnScan'));
  await p.click('#sectionList .sec-item');
  await p.waitForFunction(() => /passes|fails/.test(document.querySelector('#wsVerdict').textContent) && !window.__unflash.state.job, null, { timeout: 180000 });
}

/** From now on, what reaches the speakers of page `p` (its section player's sound, turned on): an analyser on it. */
const tap = (p) =>
  p.evaluate(() => {
    const s = window.__unflash.sectionSound;
    s.tap = s.ctx.createAnalyser();
    s.tap.fftSize = 2048;
    s.out.connect(s.tap);
    window.__peak = 0;
  });

/**
 * Listened to (the tap) until it is loud, or the sound has failed, 4 s at
 * most (the section holds a frame early on, and the second of silence
 * under it may come first): how loud it was, and what the page says.
 */
async function heard(p) {
  await p
    .waitForFunction(
      () => {
        const s = window.__unflash.sectionSound;
        const buf = new Float32Array(s.tap.fftSize);
        s.tap.getFloatTimeDomainData(buf);
        for (const x of buf) window.__peak = Math.max(window.__peak, Math.abs(x));
        return window.__peak > 0.05 || s.failed;
      },
      null,
      { timeout: 4000, polling: 40 }
    )
    .catch(() => {});
  return p.evaluate(() => {
    const s = window.__unflash.sectionSound;
    return { peak: window.__peak, failed: s.failed, output: s.ctx.state, button: document.querySelector('#btnPreviewSound').textContent, report: window.__unflash.debugReport().split('\n').find((l) => /section player's sound/.test(l)) || '' };
  });
}

try {
  // the first section, prepared; one frame in it marked E
  await inSection(page, 'flash.webm');
  const canOpus = await page.evaluate(async () => typeof AudioEncoder !== 'undefined' && (await AudioEncoder.isConfigSupported({ codec: 'opus', sampleRate: 48000, numberOfChannels: 1, bitrate: 64000 })).supported);
  assert(canOpus, 'this Chromium encodes Opus');
  const slot = 12;
  await page.click(`#frameGrid .frame:nth-child(${slot + 1})`);
  await page.click('#wsTitle'); // the keys work with the focus anywhere but a field
  await page.keyboard.press('e');
  results.mark = await page.evaluate((i) => window.__unflash.currentSection().edits[i] || null, slot);
  assert(results.mark && results.mark.extended, `slot ${slot} is marked E: ${JSON.stringify(results.mark)}`);
  // where the export will hold it, from the plan the export itself makes
  results.holds = await page.evaluate(async () => {
    const { sectionRenderPlan } = await import('./export.js');
    const u = window.__unflash;
    const sec = u.currentSection();
    const plan = sectionRenderPlan(u.state.env, u.state.movie, sec);
    // the held frame's own time, and the time its next frame had
    return { holds: plan.holds, extra: plan.extra };
  });
  console.log('holds:', JSON.stringify(results.holds));
  assert(results.holds.holds.length === 1 && results.holds.holds[0].seconds === 1 && results.holds.extra === 1, 'one hold of a second: ' + JSON.stringify(results.holds));
  const hold = results.holds.holds[0];

  await page.click('#btnExport');
  await page.waitForSelector('#exportModal', { state: 'visible' });
  results.plan = await page.textContent('#exportPlan');
  assert(/silence under the held frame/.test(results.plan), 'the plan says the sound gets silence under the held frame: ' + results.plan);
  await job(page, () => page.click('#btnDoExport'), 600000);
  await page.waitForFunction(() => !document.querySelector('#btnVerifyExport').disabled, null, { timeout: 30000 });
  results.exportResult = await page.textContent('#exportResult');
  console.log('export:', results.exportResult);
  assert(/re-encoded to (Opus|AAC) to put 1 s of silence under each held frame/.test(results.exportResult), 'the export says where the silence went: ' + results.exportResult);

  // what the exported file holds: the pictures' times, and the sound decoded
  results.exported = await exportedSound(page);
  const ex = results.exported;
  console.log('exported:', JSON.stringify({ codec: ex.codec, rate: ex.rate, soundStart: ex.soundStart, soundSeconds: ex.soundSeconds, quiet: ex.quiet, duration: ex.duration }));
  // the pictures: the held frame stays up a second longer, from the hold on
  const held = ex.pts.findIndex((t, i) => i + 1 < ex.pts.length && ex.pts[i + 1] - t > 0.9);
  assert(held >= 0, 'a frame is held in the export');
  const heldFrom = ex.pts[held];
  const heldTo = ex.pts[held + 1];
  console.log(`held frame: ${heldFrom.toFixed(3)} to ${heldTo.toFixed(3)} s; hold at ${hold.at.toFixed(3)} s`);
  assert(Math.abs(heldTo - (hold.at + 1)) < 0.002 && heldFrom < hold.at && hold.at - heldFrom < 0.05, `the picture waits from ${hold.at} for a second: ${heldFrom}..${heldTo}`);
  assert(Math.abs(ex.duration - 11) < 0.05, 'the export is a second longer: ' + ex.duration);
  // the sound: silent exactly under the hold, and nowhere else
  assert(ex.quiet.length === 1, 'one silence in the sound: ' + JSON.stringify(ex.quiet));
  const [qs, qe] = ex.quiet[0];
  assert(Math.abs(qs - hold.at) < 0.02 && Math.abs(qe - (hold.at + 1)) < 0.02, `the silence runs from ${hold.at.toFixed(3)} to ${(hold.at + 1).toFixed(3)} s: ${qs.toFixed(3)}..${qe.toFixed(3)}`);
  assert(Math.abs(ex.soundStart + ex.soundSeconds - 11) < 0.1, `the sound lasts as long as the pictures: ${ex.soundStart} + ${ex.soundSeconds}`);

  await page.click('#btnCloseExport');

  // --- the section player's sound: off to start with; on, the same silence ---------
  assert((await page.getAttribute('#btnPreviewSound', 'aria-pressed')) === 'false', 'the section player starts without sound');
  await page.click('#btnPreviewSound');
  assert((await page.getAttribute('#btnPreviewSound', 'aria-pressed')) === 'true' && (await page.textContent('#btnPreviewSound')) === 'sound on', 'the sound button turns it on');
  // at ½× there is none (it plays at 1× only), and the button says so
  await page.selectOption('#previewSpeed', '0.5');
  assert((await page.textContent('#btnPreviewSound')) === 'sound at 1× only', 'slowed, the button says the sound plays at 1× only: ' + (await page.textContent('#btnPreviewSound')));
  await page.selectOption('#previewSpeed', '1');
  await tap(page);
  await page.click('#btnPreviewPlay');
  await page.waitForFunction(() => window.__unflash.sectionSound.log.length > 0, null, { timeout: 60000 });
  results.heard = await heard(page);
  console.log('heard:', JSON.stringify(results.heard));
  assert(results.heard.output === 'running' && results.heard.peak > 0.05, `the sound reaches the speakers: ${JSON.stringify(results.heard)}`);
  assert(results.heard.button === 'sound on' && /section player's sound: on \(audio output running\)/.test(results.heard.report), 'the button and the debug report say it is on: ' + JSON.stringify(results.heard));
  results.player = await page.evaluate(async () => {
    const s = window.__unflash.sectionSound;
    const w = await s.windows.get(0);
    const x = w.buffer.getChannelData(0);
    const rate = w.buffer.sampleRate;
    const win = Math.round(rate * 0.005);
    const runs = [];
    let from = -1;
    for (let k = 0; k + win <= x.length; k += win) {
      let peak = 0;
      for (let j = k; j < k + win; j++) peak = Math.max(peak, Math.abs(x[j]));
      if (peak < 0.01 && from < 0) from = k;
      if (peak >= 0.01 && from >= 0) {
        runs.push([w.a + from / rate, w.a + k / rate]);
        from = -1;
      }
    }
    if (from >= 0) runs.push([w.a + from / rate, w.a + x.length / rate]);
    const u = window.__unflash;
    const run = u.sectionPlayer.run;
    const now = performance.now();
    return { a: w.a, b: w.b, rate, quiet: runs.filter(([p, q]) => q - p >= 0.05), log: s.log.slice(), clock: s.clock, media: run.base.media + (now - run.base.wall) / 1000, soundMedia: s.clock.media + (now - s.clock.wall) / 1000 };
  });
  const pl = results.player;
  console.log('section sound:', JSON.stringify({ a: pl.a, b: pl.b, rate: pl.rate, quiet: pl.quiet, media: pl.media, soundMedia: pl.soundMedia }));
  assert(pl.quiet.length === 1 && Math.abs(pl.quiet[0][0] - hold.at) < 0.02 && Math.abs(pl.quiet[0][1] - (hold.at + 1)) < 0.02, `the section player's sound is silent under the hold, as the export's: ${JSON.stringify(pl.quiet)}`);
  assert(Math.abs(pl.media - pl.soundMedia) < 0.2, `the sound keeps to the pictures' clock: ${pl.media} vs ${pl.soundMedia}`);
  await page.evaluate(() => window.__unflash.sectionPlayer.pause());
  assert(await page.evaluate(() => window.__unflash.sectionSound.sources.length === 0 && !window.__unflash.sectionSound.clock), 'a pause stops the sound');
  // turned off and on again while paused, it waits for the player (it used to play at once)
  results.onWhilePaused = await page.evaluate(async () => {
    const s = window.__unflash.sectionSound;
    await s.setOn(false);
    await s.setOn(true);
    await Promise.all(s.windows.values());
    await new Promise((r) => setTimeout(r, 0));
    return { sources: s.sources.length, clock: s.clock };
  });
  assert(results.onWhilePaused.sources === 0 && !results.onWhilePaused.clock, 'turned on while the player is paused, the sound waits for it: ' + JSON.stringify(results.onWhilePaused));
  await page.evaluate(() => window.__unflash.sectionPlayer.stop());
  await page.click('#btnPreviewSound');
  assert((await page.getAttribute('#btnPreviewSound', 'aria-pressed')) === 'false', 'and the button turns it off again');

  // --- no sound to be had: the button says so, and why ------------------------
  // (a browser whose audio never starts, as with nothing to play it on, and
  // that can't decode the sound)
  const quietContext = await browser.newContext({ viewport: { width: 1400, height: 1000 } });
  const quiet = await quietContext.newPage();
  await quiet.addInitScript(() => {
    const Real = window.AudioContext;
    window.AudioContext = class extends Real {
      constructor(o) {
        super(o);
        this.suspend();
      }
      resume() {
        return new Promise(() => {});
      }
    };
    AudioDecoder.isConfigSupported = async (cfg) => ({ supported: false, config: cfg });
  });
  quiet.on('pageerror', (e) => errors.push('pageerror: ' + e.message));
  await inSection(quiet, 'flash.webm');
  await quiet.click('#btnPreviewSound');
  await quiet.waitForFunction(() => document.querySelector('#btnPreviewSound').textContent === 'sound held back', null, { timeout: 10000 });
  assert(/hasn't started the sound/.test(await quiet.getAttribute('#btnPreviewSound', 'title')), 'held back, the button says the browser has not started the sound');
  await quiet.click('#btnPreviewPlay');
  await quiet.waitForFunction(() => document.querySelector('#btnPreviewSound').textContent === 'no sound', null, { timeout: 30000 });
  results.undecodable = await quiet.evaluate(() => ({ title: document.querySelector('#btnPreviewSound').title, toast: document.querySelector('#toast').textContent, report: window.__unflash.debugReport().split('\n').find((l) => /section player's sound/.test(l)) || '' }));
  console.log('no sound:', JSON.stringify(results.undecodable));
  assert(/can't decode this video's sound \(Opus, opus\)/.test(results.undecodable.title) && /^No sound: this browser can't decode/.test(results.undecodable.toast), "where the sound can't be decoded, the button and a note say so: " + JSON.stringify(results.undecodable));
  assert(/section player's sound: none: this browser can't decode/.test(results.undecodable.report), 'and so does the debug report: ' + results.undecodable.report);
  // nor does the finish alert's beep: it gives up after a moment and lets
  // its audio go (it waited for ever, and every long job kept one more open)
  results.heldBeep = await quiet.evaluate(async () => {
    const { beep } = await import('./alerts.js');
    const Held = window.AudioContext;
    const made = [];
    window.AudioContext = class extends Held {
      constructor(o) {
        super(o);
        made.push(this);
      }
      close() {
        this.closed = true;
        return super.close();
      }
    };
    try {
      const beeped = await Promise.race([beep(), new Promise((r) => setTimeout(() => r('still waiting after 5 s'), 5000))]);
      return { beeped, made: made.length, closed: made.every((c) => c.closed) };
    } finally {
      window.AudioContext = Held;
    }
  });
  console.log('a beep held back:', JSON.stringify(results.heldBeep));
  assert(results.heldBeep.beeped === false && results.heldBeep.made === 1 && results.heldBeep.closed, 'a beep the browser holds back gives up, and closes its audio: ' + JSON.stringify(results.heldBeep));
  await quietContext.close();

  // --- Dolby sound (E-AC-3), which no browser's WebCodecs decodes: the app's own
  // decoder plays it in the section player, and re-encodes it for an export with a held frame
  const dolbyContext = await browser.newContext({ viewport: { width: 1400, height: 1000 } });
  const dolby = await dolbyContext.newPage();
  dolby.on('pageerror', (e) => errors.push('pageerror: ' + e.message));
  await inSection(dolby, 'flash_eac3.mp4');
  results.eac3Native = await dolby.evaluate(async () => (await AudioDecoder.isConfigSupported({ codec: 'ec-3', sampleRate: 48000, numberOfChannels: 1 }).catch(() => ({ supported: false }))).supported);
  assert(!results.eac3Native, "this browser's WebCodecs does not decode E-AC-3 (else this test would not reach the built-in decoder)");
  await dolby.click(`#frameGrid .frame:nth-child(${slot + 1})`);
  await dolby.click('#wsTitle');
  await dolby.keyboard.press('e');
  await dolby.keyboard.press('Escape');
  await dolby.click('#btnPreviewSound');
  await tap(dolby);
  await dolby.click('#btnPreviewPlay');
  await dolby.waitForFunction(() => window.__unflash.sectionSound.log.length > 0 || window.__unflash.sectionSound.failed, null, { timeout: 60000 });
  results.dolbyHeard = await heard(dolby);
  console.log('E-AC-3 heard:', JSON.stringify(results.dolbyHeard));
  assert(!results.dolbyHeard.failed && results.dolbyHeard.peak > 0.05 && results.dolbyHeard.button === 'sound on', 'the E-AC-3 sound plays in the section player: ' + JSON.stringify(results.dolbyHeard));
  assert(/decoded by the built-in AC-3 decoder/.test(results.dolbyHeard.report), 'by the built-in decoder, as the debug report says: ' + results.dolbyHeard.report);
  await dolby.evaluate(() => window.__unflash.sectionPlayer.stop());
  // the export: the held frame's second of silence goes into the sound it re-encodes
  const dolbyHold = await dolby.evaluate(async () => {
    const { sectionRenderPlan } = await import('./export.js');
    const u = window.__unflash;
    return sectionRenderPlan(u.state.env, u.state.movie, u.currentSection()).holds[0];
  });
  await dolby.click('#btnExport');
  await dolby.waitForSelector('#exportModal', { state: 'visible' });
  await dolby.click('#btnDoExport');
  await dolby.waitForFunction(() => !document.querySelector('#btnVerifyExport').disabled && !window.__unflash.state.job, null, { timeout: 600000 });
  results.dolbyExport = await dolby.textContent('#exportResult');
  console.log('E-AC-3 export:', results.dolbyExport);
  assert(/re-encoded to (Opus|AAC) to put 1 s of silence under each held frame/.test(results.dolbyExport), 'the export re-encodes the E-AC-3 sound with silence under the held frame: ' + results.dolbyExport);
  const dx = await exportedSound(dolby);
  console.log('E-AC-3 exported:', JSON.stringify({ codec: dx.codec, quiet: dx.quiet, soundSeconds: dx.soundSeconds, hold: dolbyHold }));
  assert(dx.quiet.length === 1 && Math.abs(dx.quiet[0][0] - dolbyHold.at) < 0.03 && Math.abs(dx.quiet[0][1] - (dolbyHold.at + 1)) < 0.03, `its sound is silent exactly under the hold: ${JSON.stringify(dx.quiet)} for ${JSON.stringify(dolbyHold)}`);
  await dolbyContext.close();

  if (errors.length) throw new Error('page errors:\n' + errors.join('\n'));
  console.log('SOUND OK');
} catch (e) {
  console.error(e);
  await page.screenshot({ path: path.join(OUT, 'sound-failure.png') }).catch(() => {});
  process.exitCode = 1;
} finally {
  await close();
}
