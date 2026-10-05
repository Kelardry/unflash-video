// The WebAssembly build of the decode workers' shrink (resample::Shrink, with
// its simd128 row conversion) against a plain JavaScript statement of what it
// must compute: every pixel converted from YUV as the GPU converts it, the
// boxes of the area average summed exactly, halves rounded to even.
//   node tests/e2e/shrink.mjs        (after ./build.sh)
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { WEB } from './playwright.mjs';

const mod = await import(pathToFileURL(path.join(WEB, 'pkg/unflash.js')));
await mod.default({ module_or_path: fs.readFileSync(path.join(WEB, 'pkg/unflash_bg.wasm')) });

// yuv::YuvLayout::coefficients: [ky, kr, kgu, kgv, kb, yoff] by (bt709, full range)
const COEF = { '1,0': [76309, 117489, 13975, 34925, 138438, 16], '0,0': [76309, 104597, 25675, 53279, 132201, 16], '1,1': [65536, 103206, 12276, 30679, 121608, 0], '0,1': [65536, 91881, 22553, 46802, 116129, 0] };
const gcd = (a, b) => (b ? gcd(b, a % b) : a);
function axis(src, out) {
  const g = gcd(src, out) || 1;
  const boxes = [];
  for (let k = 0; k < out; k++) {
    const lo = k * src;
    const hi = (k + 1) * src;
    const first = Math.floor(lo / out);
    const last = Math.floor((hi - 1) / out);
    const w = [];
    for (let i = first; i <= last; i++) w.push((Math.min(hi, (i + 1) * out) - Math.max(lo, i * out)) / g);
    boxes.push([first, w]);
  }
  return { boxes, total: src / g };
}
const clamp = (v) => (v < 0 ? 0 : v > 255 ? 255 : v);
function roundDiv(n, d) {
  let q = Math.floor(n / d);
  const r = n - q * d;
  if (2 * r > d || (2 * r === d && q % 2 === 1)) q++;
  return Math.min(q, 255);
}
/** The reference: RGBA8 at aw × ah from 4:2:0 data with layout `l`. */
function reference(data, w, h, aw, ah, l) {
  const [ky, kr, kgu, kgv, kb, yoff] = COEF[`${l.bt709},${l.full}`];
  const rgb = new Int32Array(w * h * 3);
  for (let y = 0; y < h; y++)
    for (let x = 0; x < w; x++) {
      const cx = x >> 1;
      const cy = y >> 1;
      const u = (l.nv12 ? data[l.uOff + cy * l.uStride + 2 * cx] : data[l.uOff + cy * l.uStride + cx]) - 128;
      const v = (l.nv12 ? data[l.uOff + cy * l.uStride + 2 * cx + 1] : data[l.vOff + cy * l.vStride + cx]) - 128;
      const yy = (data[l.yOff + y * l.yStride + x] - yoff) * ky;
      const o = (y * w + x) * 3;
      rgb[o] = clamp((yy + kr * v + 32768) >> 16);
      rgb[o + 1] = clamp((yy - kgu * u - kgv * v + 32768) >> 16);
      rgb[o + 2] = clamp((yy + kb * u + 32768) >> 16);
    }
  const cols = axis(w, aw);
  const rows = axis(h, ah);
  const total = cols.total * rows.total;
  const out = new Uint8Array(aw * ah * 4).fill(255);
  rows.boxes.forEach(([fy, wy], oy) =>
    cols.boxes.forEach(([fx, wx], ox) => {
      for (let c = 0; c < 3; c++) {
        let sum = 0;
        wy.forEach((a, j) => wx.forEach((b, i) => (sum += a * b * rgb[((fy + j) * w + fx + i) * 3 + c])));
        out[(oy * aw + ox) * 4 + c] = roundDiv(sum, total);
      }
    })
  );
  return out;
}

let seed = 12345;
const rand = () => ((seed = (seed * 1103515245 + 12345) >>> 0) >>> 16) & 255;
let checked = 0;
for (const [w, h, aw, ah] of [
  [640, 360, 256, 144],
  [1920, 960, 256, 128],
  [97, 61, 13, 7],
  [37, 23, 9, 5],
  [48, 32, 48, 32],
  [1918, 1078, 256, 143],
])
  for (const nv12 of [false, true])
    for (const [bt709, full] of [
      [1, 0],
      [0, 0],
      [1, 1],
      [0, 1],
    ]) {
      if (w > 1000 && (nv12 || bt709 !== 1 || full)) continue; // the big sizes once each
      const cw = Math.ceil(w / 2);
      const ch = Math.ceil(h / 2);
      // rows padded, planes apart, as a decoder or copyTo lays them out
      const yStride = w + 5;
      const l = { nv12, bt709, full, yOff: 3, yStride, uOff: 0, uStride: 0, vOff: 0, vStride: 0 };
      l.uOff = l.yOff + yStride * h + 7;
      l.uStride = nv12 ? 2 * cw + 3 : cw + 3;
      l.vOff = nv12 ? 0 : l.uOff + l.uStride * ch + 2;
      l.vStride = nv12 ? 0 : cw + 1;
      const size = (nv12 ? l.uOff + l.uStride * ch : l.vOff + l.vStride * ch) + 4;
      const data = new Uint8Array(size);
      // noise, smooth ramps and saturated colours (the clamp both ways)
      for (let i = 0; i < size; i++) data[i] = i % 7 === 0 ? (i * 37) & 255 : i % 11 === 0 ? (i % 2 ? 255 : 0) : rand();
      const s = new mod.Shrinker(w, h, aw, ah);
      const ptr = s.input(size);
      new Uint8Array(mod.wasm_memory().buffer, ptr, size).set(data);
      const words = Uint32Array.from([nv12 ? 1 : 0, l.yOff, l.yStride, l.uOff, l.uStride, l.vOff, l.vStride, bt709, full]);
      const got = s.yuv(words);
      const want = reference(data, w, h, aw, ah, l);
      s.free();
      let bad = -1;
      for (let i = 0; i < want.length; i++)
        if (got[i] !== want[i]) {
          bad = i;
          break;
        }
      if (bad >= 0 || got.length !== want.length) {
        console.error(`MISMATCH ${w}x${h} -> ${aw}x${ah} ${nv12 ? 'NV12' : 'I420'} bt709=${bt709} full=${full}: value ${bad} is ${got[bad]}, want ${want[bad]}`);
        process.exit(1);
      }
      checked++;
    }
console.log(`shrink: the WebAssembly build matches the reference exactly in ${checked} cases`);
console.log('SHRINK OK');
