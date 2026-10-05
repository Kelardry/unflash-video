// Samples of an MPEG transport stream (.ts, .m2ts, .mts), read where the
// demuxer (crates/unflash-mp4/src/ts.rs) placed them. A transport stream
// cuts each stream into the payloads of packets interleaved with the
// others', so a sample's offset is not a file offset but a place: TS_BASE +
// track × 2^47 + 256 × the file offset of the packet (its sync byte) that
// holds the sample's first byte + that byte's index in the packet. Its
// bytes are that stream's packets' payloads from there on, PES headers
// passed over: `size` of them as they are for sound; for video (Annex B)
// its NAL units, each given a 4-byte length instead of a start code (its
// trailing zeros left out), until they come to `size` bytes, as an MP4's
// sample holds them. The same steps as ts::read_sample, which the Rust
// tests check against ffmpeg; the browser tests check this against that.

/** Where places begin: above any file offset. */
export const TS_BASE = 2 ** 52;
const TS_TRACK = 2 ** 47;
const SYNC = 0x47;
/** The bytes of a packet from its sync byte: what is read of it. */
const TS = 188;
/** Bytes of the file asked for at a time. */
const SPAN = 1 << 20;

/** A packet's PID, whether a PES starts in it, and where its payload starts (188: none). */
function packetHead(b, o) {
  const pid = ((b[o + 1] & 0x1f) << 8) | b[o + 2];
  const pusi = (b[o + 1] & 0x40) !== 0;
  const afc = (b[o + 3] >> 4) & 3;
  const start = afc & 2 ? 5 + b[o + 4] : 4;
  return [pid, pusi, !(afc & 1) || start > 188 ? 188 : start];
}

const optionless = (id) => id === 0xbc || id === 0xbe || id === 0xbf || id === 0xf0 || id === 0xf1 || id === 0xf2 || id === 0xf8 || id === 0xff;

/** A PES header being passed over, which may span packets. */
class PesHead {
  constructor() {
    this.buf = [];
    this.active = false;
    this.bad = false;
  }
  begin() {
    this.buf = [];
    this.active = true;
    this.bad = false;
  }
  need() {
    const b = this.buf;
    if (b.length < 4) return 4;
    if (optionless(b[3])) return 6;
    if (b.length < 9) return 9;
    return 9 + b[8];
  }
  /** Header bytes taken from the front of `b` (a Uint8Array): how many. */
  take(b) {
    let used = 0;
    while (this.active && used < b.length) {
      const n = Math.min(this.need() - this.buf.length, b.length - used);
      for (let k = 0; k < n; k++) this.buf.push(b[used + k]);
      used += n;
      const h = this.buf;
      if (h.length >= 3 && !(h[0] === 0 && h[1] === 0 && h[2] === 1)) {
        this.active = false;
        this.bad = true;
        return b.length;
      }
      if (h.length >= 4 && h.length === this.need() && (optionless(h[3]) || h.length >= 9)) this.active = false;
    }
    return used;
  }
}

/**
 * Annex B turned into lengths as it comes, into exactly `size` bytes: a
 * NAL unit ends where the next start code begins (its trailing zeros left
 * out); two start codes together make none.
 */
class Lengths {
  constructor(size) {
    this.out = new Uint8Array(size);
    this.size = size;
    this.start = 0; // where the NAL unit in hand's length goes
    this.w = 4;
    this.zeros = 0;
  }
  put(b) {
    if (this.w + b.length > this.size) throw new Error(`a sample came out longer than its ${this.size} bytes`);
    this.out.set(b, this.w);
    this.w += b.length;
  }
  /** Bytes of the stream: whether the sample is whole. */
  push(b) {
    let i = 0;
    const n = b.length;
    while (i < n) {
      if (this.zeros === 0) {
        // a run without zeros is the NAL unit's, as it is
        let k = b.indexOf(0, i);
        if (k < 0) k = n;
        if (k > i) {
          this.put(b.subarray(i, k));
          i = k;
          if (i >= n) break;
        }
      }
      const c = b[i++];
      if (c === 0) this.zeros++;
      else if (c === 1 && this.zeros >= 2) {
        this.zeros = 0;
        if (this.end()) return true;
      } else {
        if (this.zeros) {
          if (this.w + this.zeros > this.size) throw new Error(`a sample came out longer than its ${this.size} bytes`);
          this.out.fill(0, this.w, this.w + this.zeros);
          this.w += this.zeros;
          this.zeros = 0;
        }
        this.put(b.subarray(i - 1, i));
      }
    }
    return false;
  }
  /** The NAL unit in hand ends: whether the sample is whole (else room is kept for the next one's length). */
  end() {
    const len = this.w - this.start - 4;
    if (len === 0) this.w = this.start;
    else {
      this.out[this.start] = len >>> 24;
      this.out[this.start + 1] = (len >>> 16) & 255;
      this.out[this.start + 2] = (len >>> 8) & 255;
      this.out[this.start + 3] = len & 255;
    }
    if (this.w === this.size) return true;
    if (this.w + 4 > this.size) throw new Error(`a sample came out ${this.w} bytes long, not ${this.size}`);
    this.start = this.w;
    this.w += 4;
    return false;
  }
}

/**
 * The `size` bytes of the sample at `place`, through `read(offset, length)`
 * (bytes of the file, a view that stays good until the next read), of a
 * stream `ts` describes: { packet (188, 192 or 204), size (the file's),
 * tracks: [{ pid, annexb }] by track index }.
 */
export async function readTsSample(read, ts, place, size) {
  const v = place - TS_BASE;
  const track = Math.floor(v / TS_TRACK);
  const inTrack = v - track * TS_TRACK;
  let pos = Math.floor(inTrack / 256);
  let from = inTrack - pos * 256;
  const t = ts.tracks[track];
  if (!t) throw new Error(`no stream for track ${track} of this transport stream`);
  if (!size) return new Uint8Array(0);
  const fileSize = ts.size;
  const packet = ts.packet;
  // a window of the file: its bytes from `at`
  let buf = null;
  let at = 0;
  const cover = async (p, n) => {
    if (!buf || p < at || p + n > at + buf.length) {
      at = p;
      buf = await read(p, Math.min(fileSize - p, Math.max(n, SPAN)));
    }
    return p - at;
  };
  const nals = t.annexb ? new Lengths(size) : null;
  const raw = t.annexb ? null : new Uint8Array(size);
  let got = 0;
  const pes = new PesHead();
  let first = true;
  while (pos + TS <= fileSize) {
    let o = await cover(pos, TS);
    if (buf[o] !== SYNC) {
      // lost: the next sync byte with another a packet on (or the file's end there)
      let q = pos + 1;
      let found = -1;
      while (q + TS <= fileSize) {
        const k = await cover(q, 1);
        if (buf[k] === SYNC) {
          if (q + packet >= fileSize) {
            found = q;
            break;
          }
          const j = await cover(q + packet, 1);
          if (buf[j] === SYNC) {
            found = q;
            break;
          }
        }
        q++;
      }
      if (found < 0) break;
      pos = found;
      continue;
    }
    const [pid, pusi, start] = packetHead(buf, o);
    if (pid === t.pid && start < 188) {
      let b = buf.subarray(o + start, o + TS);
      if (first) {
        b = buf.subarray(o + Math.max(from, start), o + TS);
        first = false;
      } else {
        if (pusi) pes.begin();
        if (pes.active) b = b.subarray(pes.take(b));
        if (pes.bad || pes.active) b = b.subarray(b.length);
      }
      if (nals) {
        if (nals.push(b)) return nals.out;
      } else {
        const n = Math.min(size - got, b.length);
        raw.set(b.subarray(0, n), got);
        got += n;
        if (got === size) return raw;
      }
    }
    from = 0;
    pos += packet;
  }
  if (nals && nals.end()) return nals.out;
  throw new Error(`the file ended ${nals ? nals.w : got} bytes into a sample of ${size}`);
}
