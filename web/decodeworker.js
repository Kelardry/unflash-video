// A Web Worker that decodes one span of the video with WebCodecs and hands
// each picture over as plain bytes: its own YUV planes, or its pixels when
// the decoder gives RGB (BGRX on a Mac), copied here rather than on the
// page. For browsers whose WebGPU takes no VideoFrame (Firefox), where
// that copy is otherwise the page's biggest cost per frame; several
// workers copy at once.
//
// With `shrink` ({aw, ah}: the detector's analysis size) each picture is
// made that small here, as the GPU ingest pass would (the WebAssembly
// module's Shrinker, fed by VideoFrame.copyTo straight into its memory), so
// the page hands the GPU 128 KB a frame instead of the whole picture: in
// Firefox every upload also crosses to its GPU process.
//
// Protocol (page -> worker): {type:'decode', id, file, ts, config, offset,
// size, pts, dur, sync, startUs, endUs, window, shrink?}: decode those
// samples (in decode order) and send back the pictures with startUs <=
// timestamp < endUs, at most `window` of them unconsumed; {type:'credit',
// n, buffer?} after consuming a picture (its buffer comes back to be filled
// again); {type:'cancel'}.
// Worker -> page: {type:'frame', id, pic} (a transferred record: kind
// 'yuv' | 'rgba' | 'bgra', data, width, height, timestamp, layout,
// colorSpace, format, and with `shrink` shrunk: {from, copyMs, shrinkMs})
// or {type:'frame', id, frame} (a VideoFrame that can't be copied,
// transferred as it is); {type:'done', id} | {type:'error', id, message}.
import { ChunkReader, waker, yuvLayoutWords } from './media.js';

// the WebAssembly module, loaded when a job first asks for small pictures
let wasmReady = null;
function loadWasm() {
  if (!wasmReady) wasmReady = import('./pkg/unflash.js').then(async (m) => ({ m, exports: await m.default() }));
  return wasmReady;
}
let shrinker = null;
let shrinkFailed = false;

let credits = 0;
let cancelled = false;
// buffers the page gave back, to be filled again: a few at most (a job
// has at most `window` whole pictures out; a picture made small comes in a
// buffer of its own, so those would otherwise pile up here, one a frame)
const spare = [];
const SPARE = 8;
// woken by the decoder, a credit or a cancel (the timer a safety net only)
const { wait, wake } = waker(100);

self.onmessage = (e) => {
  const m = e.data;
  if (m.type === 'decode') {
    run(m).catch((err) => self.postMessage({ type: 'error', id: m.id, message: err && err.message ? err.message : String(err) }));
  } else if (m.type === 'credit') {
    credits += m.n;
    if (m.buffer && spare.length < SPARE) spare.push(m.buffer);
    wake();
  } else if (m.type === 'cancel') {
    cancelled = true;
    wake();
  }
};

function bufferOf(size) {
  for (let i = 0; i < spare.length; i++) {
    if (spare[i].byteLength >= size) return spare.splice(i, 1)[0];
  }
  // none fits: they were for pictures of another size
  spare.length = 0;
  return new ArrayBuffer(size);
}

const COPYABLE = ['I420', 'I420A', 'NV12', 'RGBA', 'RGBX', 'BGRA', 'BGRX'];

/**
 * One picture made small (`shrink`: the analysis size) and sent; false when
 * it could not be (the picture then goes as it is).
 */
async function sendSmall(f, id, shrink, wasm) {
  const fmt = f.format;
  const w = f.visibleRect ? f.visibleRect.width : f.codedWidth;
  const h = f.visibleRect ? f.visibleRect.height : f.codedHeight;
  if (shrinkFailed || !COPYABLE.includes(fmt) || fmt === 'I420A') return false;
  try {
    const t0 = performance.now();
    if (!shrinker || !shrinker.fits(w, h, shrink.aw, shrink.ah)) {
      if (shrinker) shrinker.free();
      shrinker = new wasm.m.Shrinker(w, h, shrink.aw, shrink.ah);
    }
    const size = f.allocationSize();
    const at = shrinker.input(size);
    // straight into the module's memory (nothing else runs there meanwhile)
    const planes = await f.copyTo(new Uint8Array(wasm.exports.memory.buffer, at, size));
    const t1 = performance.now();
    const packed = fmt[0] === 'R' || fmt[0] === 'B';
    const small = packed ? shrinker.packed(planes[0].offset, planes[0].stride, fmt[0] === 'B') : shrinker.yuv(Uint32Array.from(yuvLayoutWords(fmt, planes, f.colorSpace, h)));
    const t2 = performance.now();
    const pic = { kind: 'rgba', data: small.buffer, bytes: small.byteLength, width: shrink.aw, height: shrink.ah, timestamp: f.timestamp, layout: null, colorSpace: null, format: fmt, shrunk: { from: [w, h], copyMs: t1 - t0, shrinkMs: t2 - t1 } };
    f.close();
    self.postMessage({ type: 'frame', id, pic }, [small.buffer]);
    return true;
  } catch (e) {
    // never again in this worker: the pictures go as they are
    shrinkFailed = true;
    console.warn('[unflash] a decode worker cannot shrink pictures; sending them whole', e);
    return false;
  }
}

/** One decoded picture to the page, copied out when its format allows. */
async function send(f, id, shrink = null, wasm = null) {
  if (shrink && wasm && (await sendSmall(f, id, shrink, wasm))) return;
  const fmt = f.format;
  const w = f.visibleRect ? f.visibleRect.width : f.codedWidth;
  const h = f.visibleRect ? f.visibleRect.height : f.codedHeight;
  if (COPYABLE.includes(fmt)) {
    const size = f.allocationSize();
    const buf = bufferOf(size);
    const planes = await f.copyTo(buf);
    const packed = fmt[0] === 'R' || fmt[0] === 'B';
    if (!packed || (planes[0].offset === 0 && planes[0].stride === w * 4)) {
      const colorSpace = f.colorSpace && f.colorSpace.toJSON ? f.colorSpace.toJSON() : null;
      const pic = { kind: packed ? (fmt[0] === 'B' ? 'bgra' : 'rgba') : 'yuv', data: buf, bytes: size, width: w, height: h, timestamp: f.timestamp, layout: packed ? null : yuvLayoutWords(fmt, planes, f.colorSpace, h), colorSpace, format: fmt };
      f.close();
      self.postMessage({ type: 'frame', id, pic }, [buf]);
      return;
    }
    spare.push(buf);
  }
  // opaque (or oddly laid out): the frame itself
  self.postMessage({ type: 'frame', id, frame: f }, [f]);
}

async function run(job) {
  const { id, file, config, offset, size, pts, dur, sync, startUs, endUs, window, shrink } = job;
  cancelled = false;
  credits = window;
  let wasm = null;
  if (shrink && !shrinkFailed) {
    try {
      wasm = await loadWasm();
    } catch (e) {
      shrinkFailed = true;
      console.warn('[unflash] a decode worker cannot load WebAssembly; sending pictures whole', e);
    }
  }
  // a window of its own over the file; a small file read whole
  const reader = new ChunkReader(file, undefined, 16 * 1024 * 1024, null, job.ts);
  const queue = [];
  let error = null;
  const decoder = new VideoDecoder({
    output: (f) => {
      queue.push(f);
      wake();
    },
    error: (e) => {
      error = e;
      wake();
    },
  });
  if ('ondequeue' in decoder) decoder.addEventListener('dequeue', wake);
  decoder.configure(config);
  // pictures out while there are credits for them
  const pump = async () => {
    while (queue.length && !cancelled && !error) {
      const t = queue[0].timestamp;
      if (t < startUs - 1 || t >= endUs - 0.001) {
        queue.shift().close();
        continue;
      }
      if (credits <= 0) return;
      credits--;
      await send(queue.shift(), id, shrink, wasm);
    }
  };
  try {
    const n = offset.length;
    for (let i = 0; i < n && !error && !cancelled; i++) {
      while ((decoder.decodeQueueSize > 8 || queue.length > 4) && !error && !cancelled) {
        await pump();
        if (!(decoder.decodeQueueSize > 8 || queue.length > 4) || error || cancelled) break;
        await wait();
      }
      if (error || cancelled) break;
      const data = await reader.read(offset[i], size[i]);
      decoder.decode(new EncodedVideoChunk({ type: sync[i] ? 'key' : 'delta', timestamp: pts[i], duration: dur[i], data }));
      await pump();
    }
    if (!error && !cancelled) {
      let flushed = false;
      decoder.flush().then(
        () => {
          flushed = true;
          wake();
        },
        (e) => {
          error = error || e;
          flushed = true;
          wake();
        }
      );
      while (!flushed && !cancelled) {
        await pump();
        if (!flushed) await wait();
      }
    }
    // the last pictures, as the page makes room for them
    while (queue.length && !cancelled && !error) {
      await pump();
      if (queue.length) await wait();
    }
    if (error) throw error;
    self.postMessage({ type: 'done', id });
  } finally {
    while (queue.length) queue.shift().close();
    try {
      decoder.close();
    } catch (e) {
      /* already closed */
    }
    reader.release();
  }
}
