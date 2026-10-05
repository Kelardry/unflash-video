// Transport streams, PCM sound, and the choice of sound track.
// First, in Node, the app's own PCM decoder on every form it reads. Then in
// the browser: the flash clip as an MPEG transport stream (.ts, H.264 +
// AAC) and as Blu-ray's .m2ts (192-byte packets, AC-3): every sample the
// app reads out of each (ts.js, gathering it from the stream's packets) is
// the sample ffmpeg's MP4 of the same file holds, byte for byte, with the
// same time and key frame flag; opened as the user opens them, they scan
// to the flashing the MP4 has, and the .m2ts's sound plays through the
// app's own AC-3 decoder. A MOV whose sound is 24-bit PCM, and an MKV
// whose sound is DTS: the sound decodes, by the app's own decoder, to the
// whole ten seconds of tone, and an export re-encodes it, saying why by
// the sound's name (PCM, DTS).
// An MKV with two sound tracks, the first ALAC (which no browser plays),
// takes the second, and the export says which it kept and why.
//   node tests/e2e/streams.mjs
import path from 'node:path';
import { MEDIA, OUT, assert, chromium, watch, job, open as openFile, scan, flashesAsInTheMp4 } from './playwright.mjs';
import { PcmDecoder } from '../../web/audiodec.js';
import { readsAsTheMp4 } from './ts-parity.mjs';

// --- the PCM decoder on its own: each form, two channels ---------------------
{
  // (sample value, then each form's bytes of it: left 0.5, right -0.25)
  const forms = {
    'pcm-u8': [[0xc0, 0x60], 1 / 128],
    'pcm-s8': [[0x40, 0xe0], 1 / 128],
    'pcm-s16': [[0x00, 0x40, 0x00, 0xe0], 1 / 32768],
    'pcm-s16be': [[0x40, 0x00, 0xe0, 0x00], 1 / 32768],
    'pcm-s24': [[0x00, 0x00, 0x40, 0x00, 0x00, 0xe0], 1 / 8388608],
    'pcm-s24be': [[0x40, 0x00, 0x00, 0xe0, 0x00, 0x00], 1 / 8388608],
    'pcm-s32': [[0, 0, 0, 0x40, 0, 0, 0, 0xe0], 1 / 2147483648],
    'pcm-s32be': [[0x40, 0, 0, 0, 0xe0, 0, 0, 0], 1 / 2147483648],
    'pcm-f32': [[0, 0, 0, 0x3f, 0, 0, 0x80, 0xbe], 1e-7],
    'pcm-f32be': [[0x3f, 0, 0, 0, 0xbe, 0x80, 0, 0], 1e-7],
    'pcm-f64': [[0, 0, 0, 0, 0, 0, 0xe0, 0x3f, 0, 0, 0, 0, 0, 0, 0xd0, 0xbf], 1e-12],
    'pcm-f64be': [[0x3f, 0xe0, 0, 0, 0, 0, 0, 0, 0xbf, 0xd0, 0, 0, 0, 0, 0, 0], 1e-12],
    // G.711: µ-law 0x8f is +0.4978 (16311), A-law 0xd5 is -0.00024 (-8)... their tables' own values
    ulaw: [[0x8f, 0x1f], 0.02],
    alaw: [[0xa5, 0x25], 0.02],
  };
  for (const [codec, [bytes, tol]] of Object.entries(forms)) {
    const d = new PcmDecoder(codec, 2, 48000);
    // three frames of the same pair, and half a frame over (left out)
    const one = Uint8Array.from(bytes);
    const data = new Uint8Array(one.length * 3 + one.length / 2);
    for (let k = 0; k < 3; k++) data.set(one, k * one.length);
    const out = d.decode(data);
    assert(d.samples() === 3 && d.channels() === 2 && d.sample_rate() === 48000 && out.length === 6, `${codec}: three frames of two channels: ${d.samples()}`);
    const left = out.subarray(0, 3);
    const right = out.subarray(3, 6);
    const want = codec === 'ulaw' || codec === 'alaw' ? [left[0], right[0]] : [0.5, -0.25];
    for (let k = 0; k < 3; k++) assert(Math.abs(left[k] - want[0]) <= tol && Math.abs(right[k] - want[1]) <= tol, `${codec}: frame ${k}: ${left[k]}, ${right[k]}`);
    if (codec === 'ulaw' || codec === 'alaw') assert(left[0] > 0.3 && left[0] < 0.7 && right[0] < 0 && right[0] > -0.7, `${codec}: a loud positive and a negative sample: ${left[0]}, ${right[0]}`);
  }
  console.log('PCM decoder: every form OK');
}

const { browser, port, close } = await chromium(['--autoplay-policy=no-user-gesture-required']);
const page = await browser.newPage({ viewport: { width: 1400, height: 1000 } });
const errors = [];
// (a crashed page leaves what waits on it waiting: end the run instead)
page.on('crash', () => {
  console.error('the page crashed');
  process.exitCode = 1;
  browser.close();
});
watch(page, errors, /Failed to load resource|MEDIA_ERR|DEMUXER_ERROR|PIPELINE_ERROR/i);
const results = {};

/** Open `name` as the user does, and wait until it is open: what it is. */
async function open(name) {
  await openFile(page, path.join(MEDIA, name));
  return page.evaluate(() => {
    const m = window.__unflash.state.movie;
    return { format: m.format, video: m.video.codec, audio: m.audio && m.audio.codec, copyable: m.audio && m.audio.copyable, frames: m.frameCount, fps: m.fps, duration: m.duration, info: document.querySelector('#videoInfo').textContent };
  });
}

/** Scan the open video; its violations. */
const violations = async () => (await scan(page)).violations;

/**
 * The open video's sound, decoded as the section player and the export
 * decode it: seconds of it, and its peak; with `damageFirst`, a byte in the
 * middle of its first chunk flipped (`first`: that chunk's sound).
 */
const decodedSound = (damageFirst = false) =>
  page.evaluate(async (damageFirst) => {
    const { soundConfig, soundDecoderFor } = await import('./audiodec.js');
    const m = window.__unflash.state.movie;
    const at = m.audio;
    const a = m.a;
    const found = await soundDecoderFor(soundConfig(at, m.dx.track_description(at.index)));
    if (!found) return { none: true };
    let frames = 0;
    let peak = 0;
    let rate = 0;
    let channels = 0;
    let error = null;
    let first = null;
    const dec = new found.Decoder({
      output: (d) => {
        const x = new Float32Array(d.numberOfFrames);
        d.copyTo(x, { planeIndex: 0, format: 'f32-planar' });
        for (const v of x) peak = Math.max(peak, Math.abs(v));
        if (!first) first = { rate: d.sampleRate, frames: d.numberOfFrames, peak: x.reduce((p, v) => Math.max(p, Math.abs(v)), 0) };
        frames += d.numberOfFrames;
        rate = d.sampleRate;
        channels = d.numberOfChannels;
        d.close();
      },
      error: (e) => (error = e),
    });
    dec.configure(soundConfig(at, m.dx.track_description(at.index)));
    const reader = m.reader.fork();
    for (let i = 0; i < a.offset.length; i++) {
      const bytes = (await reader.read(a.offset[i], a.size[i])).slice();
      if (damageFirst && i === 0) bytes[bytes.length >> 1] ^= 0xff;
      dec.decode(new EncodedAudioChunk({ type: 'key', timestamp: a.pts[i], duration: a.dur[i], data: bytes }));
    }
    await dec.flush();
    dec.close();
    if (error) return { error: String(error.message || error) };
    return { builtIn: found.builtIn ? found.name : '', seconds: frames / rate, peak, rate, channels, first, firstChunk: a.size[0] };
  }, damageFirst);

/** Export the open video as it is (no sections: every frame copied); the export's notes and its sound. */
async function exportAsItIs() {
  await page.click('#btnExport');
  await page.waitForSelector('#exportModal', { state: 'visible' });
  const plan = await page.textContent('#exportPlan');
  await job(page, () => page.click('#btnDoExport'), 600000);
  await page.waitForFunction(() => !document.querySelector('#btnVerifyExport').disabled, null, { timeout: 60000 });
  const result = await page.textContent('#exportResult');
  const exported = await page.evaluate(async () => {
    const wasm = await import('./pkg/unflash.js');
    const { Movie } = await import('./media.js');
    const blob = await (await fetch(document.querySelector('#exportDownload').href)).blob();
    const m = await Movie.open(new File([blob], 'exported.mp4'), wasm);
    let seconds = 0;
    if (m.a) for (let i = 0; i < m.a.dur.length; i++) seconds += m.a.dur[i] / 1e6;
    return { frames: m.frameCount, audio: m.audio && m.audio.codec, seconds };
  });
  await page.click('#btnCloseExport');
  return { plan, result, exported };
}

try {
  await page.goto(`http://127.0.0.1:${port}/?cpu=1&auto=0&tour=0`);
  await page.waitForFunction(() => document.querySelector('#support').textContent.includes('WebGPU'), null, { timeout: 60000 });

  // --- the app's reading of a transport stream against ffmpeg's MP4 of it ---
  await page.evaluate(() => {
    const i = document.createElement('input');
    i.type = 'file';
    i.id = 'probeFiles';
    i.multiple = true;
    i.hidden = true;
    document.body.append(i);
  });
  await page.setInputFiles('#probeFiles', ['flash.ts', 'flash_ts.mp4', 'flash_ac3.m2ts', 'flash_ac3_m2ts.mp4'].map((f) => path.join(MEDIA, f)));
  results.parity = await page.evaluate(readsAsTheMp4, '#probeFiles');
  console.log('transport streams read as ffmpeg reads them:', JSON.stringify(results.parity));
  for (const r of results.parity) {
    assert(r.format === 'mpegts' && r.bad.length === 0 && r.videoSamples === 300 && r.audioSamples > 300, `${r.name}: every sample as in ffmpeg's MP4: ${JSON.stringify(r)}`);
    assert(r.packet === (r.name.endsWith('.m2ts') ? 192 : 188) && r.width === 640 && r.height === 360, `${r.name}: packets and picture size: ${JSON.stringify(r)}`);
  }

  // --- a .ts opened and scanned as the user does ------------------------------
  results.ts = await open('flash.ts');
  console.log('flash.ts', JSON.stringify(results.ts));
  assert(results.ts.format === 'mpegts' && /^avc1\./.test(results.ts.video) && results.ts.audio === 'mp4a.40.2' && results.ts.copyable, `flash.ts: its tracks: ${JSON.stringify(results.ts)}`);
  assert(results.ts.frames === 300 && Math.abs(results.ts.fps - 30) < 0.01 && Math.abs(results.ts.duration - 10) < 0.05, `flash.ts: 300 frames at 30 fps: ${JSON.stringify(results.ts)}`);
  flashesAsInTheMp4('flash.ts', await violations());

  // --- the .m2ts: scanned, its AC-3 sound played by the app's decoder, exported --
  results.m2ts = await open('flash_ac3.m2ts');
  console.log('flash_ac3.m2ts', JSON.stringify(results.m2ts));
  assert(results.m2ts.format === 'mpegts' && results.m2ts.audio === 'ac-3' && results.m2ts.frames === 300, `flash_ac3.m2ts: its tracks: ${JSON.stringify(results.m2ts)}`);
  flashesAsInTheMp4('flash_ac3.m2ts', await violations());
  results.m2tsSound = await decodedSound();
  console.log('flash_ac3.m2ts sound:', JSON.stringify(results.m2tsSound));
  assert(results.m2tsSound.builtIn === 'AC-3' && Math.abs(results.m2tsSound.seconds - 10) < 0.1 && results.m2tsSound.peak > 0.1, `the AC-3 sound decodes, by the app's own decoder, to ten seconds of tone: ${JSON.stringify(results.m2tsSound)}`);
  // a first chunk whose one frame is damaged comes out as silence, at the
  // track's rate: the decoder has read none yet (an AudioData at 0 Hz threw)
  results.m2tsDamaged = await decodedSound(true);
  console.log('flash_ac3.m2ts sound, its first frame damaged:', JSON.stringify(results.m2tsDamaged));
  const df = results.m2tsDamaged.first;
  assert(!results.m2tsDamaged.error && results.m2tsSound.first.peak > 0 && df && df.rate === 48000 && df.peak === 0 && df.frames === results.m2tsSound.first.frames && Math.abs(results.m2tsDamaged.seconds - 10) < 0.1, `a damaged first frame plays as silence at 48 kHz: ${JSON.stringify(results.m2tsDamaged)}`);
  results.m2tsExport = await exportAsItIs();
  console.log('flash_ac3.m2ts export:', JSON.stringify(results.m2tsExport));
  assert(/audio is copied/.test(results.m2tsExport.plan) && results.m2tsExport.exported.frames === 300 && results.m2tsExport.exported.audio === 'ac-3' && Math.abs(results.m2tsExport.exported.seconds - 10) < 0.1, `the export copies the pictures and the AC-3 sound: ${JSON.stringify(results.m2tsExport)}`);

  // --- PCM in a MOV: read by the app, re-encoded on export ---------------------
  results.pcm = await open('flash_pcm.mov');
  console.log('flash_pcm.mov', JSON.stringify(results.pcm));
  assert(results.pcm.audio === 'pcm-s24' && results.pcm.copyable === false, `flash_pcm.mov: 24-bit PCM, not copied: ${JSON.stringify(results.pcm)}`);
  results.pcmSound = await decodedSound();
  console.log('flash_pcm.mov sound:', JSON.stringify(results.pcmSound));
  // (always the app's own reading of PCM: Chromium's decoder gives 32-bit integers, and copying them out as float crashed the page)
  assert(results.pcmSound.builtIn === 'PCM' && Math.abs(results.pcmSound.seconds - 10) < 0.05 && results.pcmSound.peak > 0.1 && results.pcmSound.rate === 48000, `the PCM decodes, by the app's own reading of it, to ten seconds of tone: ${JSON.stringify(results.pcmSound)}`);
  results.pcmExport = await exportAsItIs();
  console.log('flash_pcm.mov export:', JSON.stringify(results.pcmExport));
  assert(/the audio \(PCM\) can't go into an MP4 as it is/.test(results.pcmExport.plan) && /The audio \(PCM\) can't go into an MP4 as it is, so it was re-encoded to (Opus|AAC)/.test(results.pcmExport.result) && /^(opus|mp4a\.40\.2)$/.test(results.pcmExport.exported.audio) && Math.abs(results.pcmExport.exported.seconds - 10) < 0.2, `the export re-encodes the PCM: ${JSON.stringify(results.pcmExport)}`);

  // --- DTS in an MKV: the app's own decoder, re-encoded on export -----------------
  results.dts = await open('flash_dts.mkv');
  console.log('flash_dts.mkv', JSON.stringify(results.dts));
  assert(results.dts.audio === 'dtsc' && results.dts.copyable === false, `flash_dts.mkv: DTS, not copied: ${JSON.stringify(results.dts)}`);
  results.dtsSound = await decodedSound();
  console.log('flash_dts.mkv sound:', JSON.stringify(results.dtsSound));
  assert(results.dtsSound.builtIn === 'DTS' && Math.abs(results.dtsSound.seconds - 10) < 0.1 && results.dtsSound.peak > 0.1, `the DTS sound decodes, by the app's own decoder, to ten seconds of tone: ${JSON.stringify(results.dtsSound)}`);
  results.dtsExport = await exportAsItIs();
  console.log('flash_dts.mkv export:', JSON.stringify(results.dtsExport));
  assert(/the audio \(DTS\) can't go into an MP4 as it is/.test(results.dtsExport.plan) && /The audio \(DTS\) can't go into an MP4 as it is, so it was re-encoded to (Opus|AAC)/.test(results.dtsExport.result) && /^(opus|mp4a\.40\.2)$/.test(results.dtsExport.exported.audio) && Math.abs(results.dtsExport.exported.seconds - 10) < 0.2, `the export re-encodes the DTS sound: ${JSON.stringify(results.dtsExport)}`);

  // --- two sound tracks, the first one no browser plays ---------------------------
  results.two = await open('flash_2audio.mkv');
  console.log('flash_2audio.mkv', JSON.stringify(results.two));
  assert(results.two.audio === 'opus' && /\(sound 2 of 2\)/.test(results.two.info), `the sound that can be played is taken, and said to be the second: ${JSON.stringify(results.two)}`);
  results.twoSound = await decodedSound();
  assert(Math.abs(results.twoSound.seconds - 10) < 0.1 && results.twoSound.peak > 0.1, `and it plays: ${JSON.stringify(results.twoSound)}`);
  results.twoReport = await page.evaluate(() => window.__unflash.debugReport().split('\n').find((l) => /^\s*audio /.test(l)) || '');
  assert(/track 2 of 2 \(alac before it can't be played here\)/.test(results.twoReport), `the debug report says which track: ${results.twoReport}`);
  results.twoExport = await exportAsItIs();
  console.log('flash_2audio.mkv export:', JSON.stringify(results.twoExport));
  assert(/2 audio tracks; the export keeps one \(Opus\), the first that can be played here: not ALAC\./.test(results.twoExport.result) && results.twoExport.exported.audio === 'opus', `the export keeps the Opus track and says why: ${JSON.stringify(results.twoExport)}`);

  if (errors.length) throw new Error('page errors:\n' + errors.join('\n'));
  console.log('STREAMS OK');
} catch (e) {
  console.error(e);
  console.error(JSON.stringify(results));
  await page.screenshot({ path: path.join(OUT, 'streams-failure.png') }).catch(() => {});
  process.exitCode = 1;
} finally {
  await close();
}
