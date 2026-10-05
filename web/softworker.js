// A Web Worker running one of the app's built-in decoders over one group of
// pictures at a time (see h264pool.js for the protocol): H.264's, from the
// main WebAssembly module, or HEVC's, VP9's, VP8's or AV1's, from the
// decoders module, as `init` names the codec. Pictures leave as plain I420
// buffers (transferred, not copied) or, for a job with `shrink`, made the
// detector's size here straight from the decoder's own picture, so the
// full-size picture never leaves WebAssembly memory.
import { ChunkReader, builtInDecoder, decodeBuiltIn, decodedPicture, waker } from './media.js';

let mod = null; // the module with the codec's decoder
let codec = null;
let desc = null;
let credits = 0;
let cancelled = false;
const { wait, wake } = waker();

/** The decoder's current picture as a transferable record, at `timestamp` (µs). */
function picture(d, shrink, timestamp) {
  if (shrink) return { kind: 'rgba', data: d.small(), width: shrink.aw, height: shrink.ah, timestamp, colorSpace: null, from: [d.width(), d.height()] };
  return { ...decodedPicture(mod, d), timestamp };
}

async function run(job) {
  const { id, file, offset, size, pts, minPts, maxPts, shrink } = job;
  cancelled = false;
  // each worker keeps its own window over the file; a whole copy per worker would cost too much
  const reader = new ChunkReader(file, undefined, 16 * 1024 * 1024, null, job.ts);
  const d = builtInDecoder(mod, codec, desc);
  let damaged = 0;
  let decodeMs = 0;
  let decoded = 0;
  try {
    if (shrink) d.set_shrink(shrink.aw, shrink.ah);
    damaged = await decodeBuiltIn(d, codec === 'h264', pts, 0, pts.length, {
      read: (i) => reader.read(offset[i], size[i]),
      wanted: (i) => pts[i] >= minPts && pts[i] < maxPts,
      picture: (i) => picture(d, shrink, pts[i]),
      give: async (pic) => {
        while (credits <= 0 && !cancelled) await wait();
        if (cancelled) return;
        credits--;
        postMessage({ type: 'frame', id, pic }, [pic.data.buffer]);
      },
      stopped: () => cancelled,
      timed: (ms) => {
        decodeMs += ms;
        decoded++;
      },
    });
  } finally {
    try {
      d.free();
    } catch (e) {
      /* after a trap even this fails: the error that counts is the one before */
    }
  }
  postMessage({ type: 'done', id, damaged, decodeMs, decoded });
}

self.onmessage = async (e) => {
  const m = e.data;
  switch (m.type) {
    case 'init':
      try {
        codec = m.codec;
        desc = m.desc;
        // (H.264's decoder is in the main module)
        mod = await import(codec === 'h264' ? './pkg/unflash.js' : './pkg-dec/unflash_decoders.js');
        await mod.default();
        builtInDecoder(mod, codec, desc).free();
        postMessage({ type: 'ready' });
      } catch (err) {
        postMessage({ type: 'error', message: String(err && err.message ? err.message : err) });
      }
      break;
    case 'credit':
      credits = m.reset ? m.n : credits + m.n;
      wake();
      break;
    case 'cancel':
      cancelled = true;
      wake();
      break;
    case 'decode':
      // every job is answered: one that failed (a sample that could not be
      // read, a trap in the decoder) with its error, which fails the pass
      try {
        await run(m);
      } catch (err) {
        postMessage({ type: 'done', id: m.id, damaged: 0, decodeMs: 0, decoded: 0, error: String(err && err.message ? err.message : err) });
      }
      break;
    default:
      break;
  }
};
