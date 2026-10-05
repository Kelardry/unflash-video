// Triage: where in a film to look first. From what the file's index already
// holds (every frame's size and which frames are keyframes), with nothing
// decoded, each chunk of a scan (analysis.js scanChunks) gets a score for how
// likely it is to hold flashing: a picture that changes all over costs an
// encoder many bits, and encoders start a new GOP at a scene cut, which a
// flash looks like to them. A scan takes early looks at the likeliest chunks
// first, so that what flashes turns up early; what it finds does not depend
// on the order (every frame is still decoded and checked, by one detector
// that takes them all in file order), and the scores say nothing about what
// is safe.

/** The value at quantile `q` (0..1) of `arr` (sorted in place). */
function quantile(arr, q) {
  if (!arr.length) return 0;
  arr.sort((a, b) => a - b);
  return arr[Math.min(arr.length - 1, Math.floor(q * arr.length))];
}

/** Each value's rank among `vals` in 0..1 (ties share their mean rank; all equal gives 0.5). */
function ranks(vals) {
  const n = vals.length;
  const order = vals.map((v, i) => [v, i]).sort((a, b) => a[0] - b[0]);
  const out = new Float64Array(n);
  for (let i = 0; i < n; ) {
    let j = i;
    while (j + 1 < n && order[j + 1][0] === order[i][0]) j++;
    const r = n > 1 ? (i + j) / 2 / (n - 1) : 0.5;
    for (let k = i; k <= j; k++) out[order[k][1]] = r;
    i = j + 1;
  }
  return out;
}

/** The share of chunks taken first, by score (the rest go in file order). */
const HOT_SHARE = 0.25;

/** Keyframes under this many bytes a pixel count as flat pictures. */
const FLAT_BYTES_PER_PIXEL = 0.002;

/**
 * Scores for the chunks of `movie` (`chunks` from scanChunks): { score
 * (0..1 per chunk), order (chunk indices, likeliest first), hot (how many
 * of `order` to take first), features (per chunk: bytes per frame relative
 * to the film's median frame; keyframes per second; "spikes", frames other
 * than keyframes that cost over three times the chunk's upper quartile, the
 * price of a picture unlike the one before; and "flat" keyframes, under a
 * quarter of the film's median keyframe or 0.002 bytes a pixel, a picture
 * with next to nothing in it: a white or black frame, a fade, a flash) }.
 *
 * Bytes alone would miss the worst case: an encoder predicting each frame of
 * a strobe from the frame two back codes it cheaper than a still shot.
 */
export function triageChunks(movie, chunks) {
  const { size, sync } = movie.v;
  const nc = chunks.length;
  const all = [];
  const keySizes = [];
  for (let i = 0; i < size.length; i++) {
    all.push(size[i]);
    if (sync[i]) keySizes.push(size[i]);
  }
  const median = Math.max(1, quantile(all, 0.5));
  const keyMedian = quantile(keySizes, 0.5);
  // a keyframe this small is a picture with next to nothing in it, whatever
  // the rest of the film is like (a real 1080p keyframe runs 0.02 to 0.15
  // bytes a pixel, a flat white or black one a few thousandths)
  const flatBytes = Math.max(keyMedian / 4, FLAT_BYTES_PER_PIXEL * Math.max(1, movie.width * movie.height));
  const features = [];
  for (let c = 0; c < nc; c++) {
    const a = chunks.idx[c];
    const b = c + 1 < nc ? chunks.idx[c + 1] : size.length;
    const secs = Math.max(0.001, chunks.t[c + 1] - chunks.t[c]);
    let bytes = 0;
    let keys = 0;
    let flat = 0;
    const rest = [];
    for (let i = a; i < b; i++) {
      bytes += size[i];
      if (sync[i]) {
        keys++;
        if (size[i] < flatBytes) flat++;
      } else rest.push(size[i]);
    }
    // (sorted in place: the spikes are counted in any order)
    const p75 = quantile(rest, 0.75);
    let spikes = 0;
    for (const s of rest) if (s > 3 * p75) spikes++;
    features.push({ bits: bytes / Math.max(1, b - a) / median, keys: keys / secs, spikes: spikes / secs, flat: flat / secs });
  }
  const rb = ranks(features.map((f) => f.bits));
  const rk = ranks(features.map((f) => f.keys));
  const rs = ranks(features.map((f) => f.spikes));
  const rf = ranks(features.map((f) => f.flat));
  const score = new Float64Array(nc);
  for (let c = 0; c < nc; c++) score[c] = 0.25 * rb[c] + 0.2 * rk[c] + 0.3 * rs[c] + 0.25 * rf[c];
  const order = Array.from({ length: nc }, (_, c) => c).sort((x, y) => score[y] - score[x] || x - y);
  const hot = nc < 4 ? 0 : Math.max(1, Math.round(nc * HOT_SHARE));
  return { score, order, hot, features };
}
