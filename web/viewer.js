// One frame of a section at full size, decoded from the file: the grid's
// thumbnails are the detector's small copies, too small to read a subtitle
// on. The viewer follows the grid's selection and steps with the arrow
// keys. Holding a key down over flashing footage must not play it back as a
// flash, so a new picture appears no more often than every MIN_GAP_MS (the
// last one asked for always does, once the keys rest).
import { decodeRange } from './media.js';

/** Seconds decoded either side of the frame asked for, kept for stepping. */
const WINDOW_S = 0.35;
/** At most 2.5 new pictures a second: slower than any flash that counts. */
export const MIN_GAP_MS = 400;
/** Pictures kept (as bitmaps at the viewer's size). */
const KEEP = 48;

export class FrameViewer {
  /** `onShown(i)` after frame `i` is drawn. */
  constructor(canvas, { onShown = null } = {}) {
    this.canvas = canvas;
    this.onShown = onShown;
    this.cache = new Map(); // timestamp (µs) -> ImageBitmap
    this.want = null;
    this.lastShown = 0;
    this.timer = null;
    this.busy = null;
    this.movie = null;
  }

  /** Ask for frame `i` of `sec` (drawn now, or as soon as the rate allows). */
  show(movie, sec, i) {
    if (movie !== this.movie) {
      this.clear();
      this.movie = movie;
      // the picture's own size, up to 1920 wide (enough for any screen's share of it)
      const scale = Math.min(1, 1920 / movie.width);
      this.canvas.width = Math.round(movie.width * scale);
      this.canvas.height = Math.round(movie.height * scale);
    }
    this.want = { movie, sec, i, t: sec.start + sec.pts[i] };
    if (this.timer) return;
    const wait = Math.max(0, this.lastShown + MIN_GAP_MS - performance.now());
    this.timer = setTimeout(() => {
      this.timer = null;
      this.render();
    }, wait);
  }

  async render() {
    // one decode at a time; the latest request wins
    if (this.busy) {
      await this.busy;
      if (this.timer) return;
    }
    const w = this.want;
    if (!w) return;
    this.busy = (async () => {
      let bmp = this.lookup(w.t, w.movie);
      if (!bmp) {
        await this.fill(w.movie, w.t);
        bmp = this.lookup(w.t, w.movie);
      }
      if (bmp && this.want === w) {
        this.canvas.getContext('2d').drawImage(bmp, 0, 0, this.canvas.width, this.canvas.height);
        this.lastShown = performance.now();
        if (this.onShown) this.onShown(w.i);
      }
    })();
    try {
      await this.busy;
    } catch (e) {
      console.warn('[unflash] the frame viewer could not decode that frame:', e);
    } finally {
      this.busy = null;
    }
    // asked for another while this one decoded (not cleared meanwhile)
    if (this.want && this.want !== w && !this.timer) this.show(this.want.movie, this.want.sec, this.want.i);
  }

  /** The decoded picture nearest `t` (within half a frame), if there is one. */
  lookup(t, movie) {
    const us = t * 1e6;
    let best = null;
    let d = Infinity;
    for (const [k, b] of this.cache) {
      const e = Math.abs(k - us);
      if (e < d) {
        d = e;
        best = b;
      }
    }
    return d <= (movie.medianDelta * 1e6) / 2 + 1 ? best : null;
  }

  async fill(movie, t) {
    const w = this.canvas.width;
    const h = this.canvas.height;
    // the viewer cleared meanwhile (a new file, the viewer closed): the
    // decode stops, and its pictures are not kept
    const gone = () => this.movie !== movie;
    await decodeRange(
      movie,
      Math.max(movie.tsMin, t - WINDOW_S),
      t + WINDOW_S,
      async (frame, ft) => {
        try {
          const us = Math.round(ft * 1e6);
          if (this.cache.has(us)) return;
          const bmp = await createImageBitmap(frame, { resizeWidth: w, resizeHeight: h, resizeQuality: 'high' });
          if (gone()) bmp.close();
          else this.cache.set(us, bmp);
        } finally {
          frame.close();
        }
      },
      { cancel: gone }
    );
    // the pictures furthest from here go first
    if (this.cache.size > KEEP) {
      const keys = [...this.cache.keys()].sort((a, b) => Math.abs(b - t * 1e6) - Math.abs(a - t * 1e6));
      for (const k of keys.slice(0, this.cache.size - KEEP)) {
        this.cache.get(k).close();
        this.cache.delete(k);
      }
    }
  }

  /** Forget the pictures (a new file, the viewer closed). */
  clear() {
    for (const b of this.cache.values()) b.close();
    this.cache.clear();
    this.want = null;
    this.movie = null;
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
  }
}
