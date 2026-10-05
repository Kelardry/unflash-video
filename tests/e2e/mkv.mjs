// Matroska and WebM end to end: each clip (the flash clip's pictures as
// H.264 + AAC in MKV, VP9 + Opus in WebM, VP9 + Vorbis in WebM) opens, is
// scanned and must show the flashing where the MP4 does; one section is
// prepared, fixed and exported, and the exported MP4 must carry the audio
// (copied, or re-encoded when an MP4 cannot hold it: Vorbis) and pass its
// verification. A file in a container Unflash does not read gets a message
// that says what it is.
//   node tests/e2e/mkv.mjs
import path from 'node:path';
import { MEDIA, OUT, assert, chromium, watch, idle, job, cancelledJob, open, scan, flashesAsInTheMp4 } from './playwright.mjs';

const { browser, port, close } = await chromium(['--autoplay-policy=no-user-gesture-required']);
const page = await browser.newPage({ viewport: { width: 1400, height: 1000 } });
const errors = [];
// (the player turning a file down, and the AVI this test opens on purpose, are expected)
watch(page, errors, /Failed to load resource|MEDIA_ERR|DEMUXER_ERROR|PIPELINE_ERROR|This is an AVI file/i);
const bannerText = () => page.evaluate(() => (document.querySelector('#banner').classList.contains('hidden') ? '' : document.querySelector('#bannerText').textContent));
const results = {};

try {
  await page.goto(`http://127.0.0.1:${port}/?cpu=1&auto=0`);
  await page.waitForFunction(() => document.querySelector('#support').textContent.includes('WebGPU'), null, { timeout: 60000 });

  for (const [name, expect] of [
    ['flash.mkv', { format: 'matroska', video: /^avc1\./, audio: 'mp4a.40.2', copyable: true, exportAudio: /^mp4a\.40\.2$/ }],
    ['flash.webm', { format: 'webm', video: /^vp09\./, audio: 'opus', copyable: true, exportAudio: /^opus$/ }],
    ['flash_vorbis.webm', { format: 'webm', video: /^vp09\./, audio: 'vorbis', copyable: false, exportAudio: /^(opus|mp4a\.40\.2)$/ }],
  ]) {
    const r = (results[name] = {});
    await open(page, path.join(MEDIA, name));
    r.movie = await page.evaluate(() => {
      const m = window.__unflash.state.movie;
      return { format: m.format, video: m.video.codec, audio: m.audio && m.audio.codec, copyable: m.audio && m.audio.copyable, frames: m.frameCount, fps: m.fps, duration: m.duration };
    });
    console.log(name, JSON.stringify(r.movie));
    assert(r.movie.format === expect.format && expect.video.test(r.movie.video) && r.movie.audio === expect.audio && r.movie.copyable === expect.copyable, `${name}: tracks ${JSON.stringify(r.movie)}`);
    assert(r.movie.frames === 300 && Math.abs(r.movie.fps - 30) < 0.01 && Math.abs(r.movie.duration - 10) < 0.05, `${name}: 300 frames at 30 fps: ${JSON.stringify(r.movie)}`);

    // the scan finds what it finds in the MP4
    flashesAsInTheMp4(name, (await scan(page)).violations);
    if (name === 'flash.webm') continue;

    // a section: prepared, fixed (keep dark), exported, verified
    await page.click('#sectionList .sec-item');
    await page.waitForFunction(() => /passes|fails/.test(document.querySelector('#wsVerdict').textContent), null, { timeout: 180000 });
    await idle(page);
    await page.click('#btnSuggestDark');
    await page.waitForFunction(() => document.querySelector('#wsVerdict').textContent === 'passes', null, { timeout: 180000 });
    await page.click('#btnExport');
    await page.waitForSelector('#exportModal', { state: 'visible' });
    r.plan = await page.textContent('#exportPlan');
    await job(page, () => page.click('#btnDoExport'), 600000);
    await page.waitForFunction(() => !document.querySelector('#btnVerifyExport').disabled, null, { timeout: 30000 });
    r.exportResult = await page.textContent('#exportResult');
    console.log(`${name}: plan: ${r.plan}\n  export: ${r.exportResult}`);
    if (!expect.copyable) {
      assert(/re-encoded (it )?to|re-encoded/.test(r.plan) && /re-encoded to (Opus|AAC)/.test(r.exportResult), `${name}: the Vorbis audio is re-encoded: ${r.exportResult}`);
    } else {
      assert(r.plan.includes('audio is copied'), `${name}: the plan says the audio is copied: ${r.plan}`);
    }
    // what the exported file holds
    r.exported = await page.evaluate(async () => {
      const wasm = await import('./pkg/unflash.js');
      const { Movie } = await import('./media.js');
      const blob = await (await fetch(document.querySelector('#exportDownload').href)).blob();
      const m = await Movie.open(new File([blob], 'exported.mp4'), wasm);
      const at = m.audio;
      const a = m.a;
      let aDur = 0;
      if (a) for (let i = 0; i < a.dur.length; i++) aDur += a.dur[i] / 1e6;
      return { format: m.format, video: m.video.codec, frames: m.frameCount, audio: at && at.codec, audioPackets: a ? a.size.length : 0, audioSeconds: aDur, rate: at && at.sample_rate };
    });
    console.log(`${name}: exported`, JSON.stringify(r.exported));
    assert(r.exported.format === 'mp4' && r.exported.audio && expect.exportAudio.test(r.exported.audio), `${name}: the export carries the audio: ${JSON.stringify(r.exported)}`);
    assert(Math.abs(r.exported.audioSeconds - 10) < 0.2, `${name}: all of the audio: ${JSON.stringify(r.exported)}`);
    await job(page, () => page.click('#btnVerifyExport'), 600000);
    r.verify = await page.textContent('#exportResult');
    assert(r.verify.includes('Passes WCAG'), `${name}: the export passes: ${r.verify}`);
    await page.click('#btnCloseExport');
  }

  // the whole-video player: a WebM plays here; an MKV of H.264 this Chromium
  // cannot play is said to be so
  await open(page, path.join(MEDIA, 'flash.webm'));
  await page.evaluate(() => window.__unflash.setPlayerSource('video'));
  await page.waitForFunction(() => document.querySelector('#player').readyState >= 1, null, { timeout: 30000 });
  results.webmPlayer = await page.textContent('#playerWarning');
  assert(/whole video as it is/.test(results.webmPlayer), 'the WebM plays in the page: ' + results.webmPlayer);
  // (the live monitor on: it goes off, its switch with it, when the player cannot play the next file)
  await page.check('#liveToggle');
  const h264Playable = await page.evaluate(() => document.createElement('video').canPlayType('video/mp4; codecs="avc1.42E01E"') !== '');
  // an MKV's open cancelled as it reads the file through (an MKV keeps no
  // index) leaves the WebM open, as it was
  {
    const cancelled = await cancelledJob(page, () => page.setInputFiles('#fileInput', path.join(MEDIA, 'flash.mkv')), 10);
    results.mkvCancelled = await page.evaluate(() => ({ toast: document.querySelector('#toast').textContent, video: document.querySelector('#videoInfo').textContent, movie: window.__unflash.state.movie.name, live: window.__unflash.state.live.on }));
    console.log('an MKV open cancelled:', JSON.stringify(results.mkvCancelled));
    assert(cancelled === 'Opening video' && results.mkvCancelled.toast === 'Opening video: cancelled', 'the open was cancelled as it read the file: ' + JSON.stringify({ cancelled, toast: results.mkvCancelled.toast }));
    assert(/flash\.webm/.test(results.mkvCancelled.video) && results.mkvCancelled.movie === 'flash.webm' && results.mkvCancelled.live, 'an open cancelled as it reads the file leaves the video that was open: ' + JSON.stringify(results.mkvCancelled));
  }
  await open(page, path.join(MEDIA, 'flash.mkv'));
  await page.evaluate(() => window.__unflash.setPlayerSource('video'));
  await page.waitForFunction(() => window.__unflash.state.player.playable === false || document.querySelector('#player').readyState >= 1, null, { timeout: 30000 });
  results.mkvPlayer = { playable: await page.evaluate(() => window.__unflash.state.player.playable), text: await page.textContent('#playerWarning'), h264Playable };
  console.log('player:', JSON.stringify(results.mkvPlayer));
  if (!results.mkvPlayer.playable) {
    assert(/can't play this MKV file/.test(results.mkvPlayer.text), 'the player says it cannot play the MKV: ' + results.mkvPlayer.text);
    const live = await page.evaluate(() => ({ on: window.__unflash.state.live.on, ticked: document.querySelector('#liveToggle').checked }));
    assert(!live.on && !live.ticked, 'the live monitor is off, and its switch says so: ' + JSON.stringify(live));
  }

  // a container Unflash does not read
  await page.setInputFiles('#fileInput', { name: 'old.avi', mimeType: 'video/x-msvideo', buffer: Buffer.concat([Buffer.from('RIFF'), Buffer.alloc(4), Buffer.from('AVI LIST'), Buffer.alloc(2000)]) });
  await page.waitForFunction(() => !document.querySelector('#banner').classList.contains('hidden'), null, { timeout: 30000 });
  results.avi = await bannerText();
  console.log('avi:', results.avi);
  assert(/This is an AVI file/.test(results.avi) && /MKV/.test(results.avi), 'an AVI file is named as such: ' + results.avi);

  if (errors.length) throw new Error('page errors:\n' + errors.join('\n'));
  console.log('MKV OK');
} catch (e) {
  console.error(e);
  await page.screenshot({ path: path.join(OUT, 'mkv-failure.png') }).catch(() => {});
  process.exitCode = 1;
} finally {
  await close();
}
