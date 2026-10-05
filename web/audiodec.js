// The sound's decoder: WebCodecs' AudioDecoder where the browser has one for
// the codec; otherwise one built into the app, behind the same interface, so
// that the section player's sound and the export's re-encode use either the
// same way. The app's own: AC-3 and E-AC-3 (Dolby Digital, Dolby Digital
// Plus) and DTS, which no browser's WebCodecs decodes (crates/unflash-sound
// in the decoders module; their sound comes mixed down to stereo), and PCM
// in every form (big and little endian, 8 to 32 bits, float, G.711), read
// here.

import { loadDecoders } from './codecs.js';

/**
 * The sounds the app decodes itself: `wasm` names the codec of the decoders
 * module's SoundDecoder (whose sound comes mixed down to stereo); PCM is
 * read in JS, every channel kept.
 */
const BUILT_IN_SOUND = [
  { name: 'AC-3', test: /^(ac-3|ec-3|mp4a\.a5|mp4a\.a6)$/i, wasm: 'ac3' },
  // (DTS Express, dtse, and DTS-HD without a core are not: only the core is read)
  { name: 'DTS', test: /^(dtsc|dtsh|dtsl)$/i, wasm: 'dts' },
  { name: 'PCM', test: /^(pcm-(u8|s8|s16|s16be|s24|s24be|s32|s32be|f32|f32be|f64|f64be)|ulaw|alaw)$/, pcm: true },
];

function builtInKind(codec) {
  return BUILT_IN_SOUND.find((k) => k.test.test(codec || '')) || null;
}

/** Whether the app has a decoder of its own for the sound `codec`. */
function builtInSound(codec) {
  return !!builtInKind(codec);
}

/** The decoder config for the sound of `track` (a Movie's track summary), its `description` given (empty for none). */
export function soundConfig(track, description) {
  const cfg = { codec: track.codec, sampleRate: track.sample_rate, numberOfChannels: track.channels };
  if (description && description.length) cfg.description = description;
  return cfg;
}

/**
 * Sample `i` of the movie's sound as an EncodedAudioChunk, read through
 * `reader` (its bytes as the track stores them, with what Matroska's header
 * stripping took off each frame put back in front; the chunk keeps a copy
 * of its own).
 */
export async function soundChunk(movie, reader, i) {
  const at = movie.audio;
  const a = movie.a;
  let data = await reader.read(a.offset[i], a.size[i]);
  if (at.prefix && at.prefix.length) {
    const b = new Uint8Array(at.prefix.length + data.length);
    b.set(at.prefix);
    b.set(data, at.prefix.length);
    data = b;
  }
  return new EncodedAudioChunk({ type: 'key', timestamp: Math.round((a.ptsTicks[i] * 1e6) / at.timescale), duration: Math.round((a.durTicks[i] * 1e6) / at.timescale), data });
}

/**
 * Whether the sound configured by `cfg` can be played here: the browser's
 * WebCodecs decodes it, or the app has a decoder of its own for it (which
 * every build has: not loaded to find out).
 */
export async function canPlaySound(cfg) {
  const kind = builtInKind(cfg.codec);
  if (kind && kind.pcm) return typeof AudioData !== 'undefined';
  if (typeof AudioDecoder !== 'undefined') {
    try {
      if ((await AudioDecoder.isConfigSupported(cfg)).supported) return true;
    } catch (e) {
      /* not this way */
    }
  }
  return !!kind && typeof AudioData !== 'undefined' && typeof EncodedAudioChunk !== 'undefined';
}

/** Why the built-in decoder was not there, the last time it was wanted. */
let missing = '';

/**
 * A decoder for the sound configured by `cfg` ({ codec, sampleRate,
 * numberOfChannels, description }): `{ Decoder, builtIn, name, channels }`,
 * the class to make it from (AudioDecoder, or BuiltInAudioDecoder), the
 * name of the app's own decoder when it is one ("AC-3", "DTS", "PCM"), and
 * the channels its sound comes in (the source's; two where the app's own
 * mixes it down); or null where neither can decode it (and noSoundDecoder
 * says why).
 */
export async function soundDecoderFor(cfg) {
  const kind = builtInKind(cfg.codec);
  // PCM is read here whatever the browser has: it needs no decoding, and
  // Chromium's own PCM decoder gives integer sound that crashed the page
  // when copied out as float, as the player and the export copy it
  if (typeof AudioDecoder !== 'undefined' && !(kind && kind.pcm)) {
    try {
      if ((await AudioDecoder.isConfigSupported(cfg)).supported) return { Decoder: AudioDecoder, builtIn: false, name: '', channels: cfg.numberOfChannels };
    } catch (e) {
      /* not this way */
    }
  }
  if (kind) {
    if (typeof AudioData === 'undefined' || typeof EncodedAudioChunk === 'undefined') {
      missing = 'webcodecs';
      return null;
    }
    if (kind.pcm) return { Decoder: BuiltInAudioDecoder, builtIn: true, name: kind.name, channels: cfg.numberOfChannels };
    missing = 'module';
    try {
      const mod = await loadDecoders();
      if (mod.SoundDecoder) return { Decoder: BuiltInAudioDecoder, builtIn: true, name: kind.name, channels: 2 };
    } catch (e) {
      /* the module did not load */
    }
  }
  return null;
}

/**
 * Why soundDecoderFor found nothing for `codec`, in words, `what` naming the
 * sound ("this video's sound (E-AC-3 …)"): the browser can't decode it; or,
 * for a sound the app decodes itself, its decoder did not load (every build
 * has it, so its download failed) or the browser lacks the WebCodecs sound
 * it hands its sound to.
 */
export function noSoundDecoder(codec, what) {
  if (builtInSound(codec) && missing === 'module') return `the app's own decoder for ${what} did not load (reloading the page fetches it again)`;
  if (builtInSound(codec) && missing === 'webcodecs') return `this browser lacks the WebCodecs sound support that the app's own decoder for ${what} needs`;
  return `this browser can't decode ${what}`;
}

// ---- PCM ------------------------------------------------------------------------

/** G.711 µ-law and A-law bytes as 16-bit samples. */
const G711 = (() => {
  const ulaw = new Int16Array(256);
  const alaw = new Int16Array(256);
  for (let i = 0; i < 256; i++) {
    const u = ~i & 0xff;
    const t = (((u & 0x0f) << 3) + 0x84) << ((u & 0x70) >> 4);
    ulaw[i] = u & 0x80 ? 0x84 - t : t - 0x84;
    const a = i ^ 0x55;
    const seg = (a & 0x70) >> 4;
    let v = (a & 0x0f) << 4;
    v = seg === 0 ? v + 8 : seg === 1 ? v + 0x108 : (v + 0x108) << (seg - 1);
    alaw[i] = a & 0x80 ? v : -v;
  }
  return { ulaw, alaw };
})();

/** Bytes per sample, and how one reads, of each PCM codec string. */
const PCM_FORMATS = {
  'pcm-u8': [1, (d, o) => (d.getUint8(o) - 128) / 128],
  'pcm-s8': [1, (d, o) => d.getInt8(o) / 128],
  'pcm-s16': [2, (d, o) => d.getInt16(o, true) / 32768],
  'pcm-s16be': [2, (d, o) => d.getInt16(o, false) / 32768],
  'pcm-s24': [3, (d, o) => ((d.getInt8(o + 2) << 16) | (d.getUint8(o + 1) << 8) | d.getUint8(o)) / 8388608],
  'pcm-s24be': [3, (d, o) => ((d.getInt8(o) << 16) | (d.getUint8(o + 1) << 8) | d.getUint8(o + 2)) / 8388608],
  'pcm-s32': [4, (d, o) => d.getInt32(o, true) / 2147483648],
  'pcm-s32be': [4, (d, o) => d.getInt32(o, false) / 2147483648],
  'pcm-f32': [4, (d, o) => d.getFloat32(o, true)],
  'pcm-f32be': [4, (d, o) => d.getFloat32(o, false)],
  'pcm-f64': [8, (d, o) => d.getFloat64(o, true)],
  'pcm-f64be': [8, (d, o) => d.getFloat64(o, false)],
  ulaw: [1, (d, o) => G711.ulaw[d.getUint8(o)] / 32768],
  alaw: [1, (d, o) => G711.alaw[d.getUint8(o)] / 32768],
};

/**
 * Interleaved PCM read into planes, with the decoders module's sound
 * decoder's interface (decode, samples, channels, sample_rate, free): a
 * chunk is any whole number of sample frames (a part frame at its end is
 * dropped).
 */
export class PcmDecoder {
  constructor(codec, channels, sampleRate) {
    [this.bytes, this.read] = PCM_FORMATS[codec];
    this.ch = Math.max(1, channels | 0);
    this.rate = sampleRate;
    this.n = 0;
  }
  decode(data) {
    const frame = this.bytes * this.ch;
    const n = Math.floor(data.length / frame);
    const out = new Float32Array(n * this.ch);
    const d = new DataView(data.buffer, data.byteOffset, data.byteLength);
    const { read, bytes, ch } = this;
    for (let c = 0; c < ch; c++) {
      const plane = c * n;
      for (let i = 0, o = c * bytes; i < n; i++, o += frame) out[plane + i] = read(d, o);
    }
    this.n = n;
    return out;
  }
  samples() {
    return this.n;
  }
  channels() {
    return this.ch;
  }
  sample_rate() {
    return this.rate;
  }
  free() {}
}

/**
 * A sound the app decodes itself (AC-3, E-AC-3 and DTS through the
 * decoders module, PCM here), with as much of WebCodecs' AudioDecoder as
 * the app uses: configure, decode (EncodedAudioChunk; each a whole number
 * of frames, as an MP4 sample, a Matroska block or a transport stream's
 * packet is), flush, close, decodeQueueSize, and `output` given AudioData
 * (f32-planar, timed as the chunk was: Dolby and DTS mixed down to
 * stereo). A chunk that won't decode at all goes to `error`; frames within
 * one that are damaged come out as silence.
 */
class BuiltInAudioDecoder {
  constructor({ output, error }) {
    this.output = output;
    this.error = error;
    this.queue = [];
    this.decodeQueueSize = 0;
    this.dec = null;
    this.ready = null;
    this.running = false;
    this.closed = false;
    this.idle = []; // flush() waiting for the queue to empty
    this.name = '';
    this.rate = 0;
  }

  configure(cfg) {
    const kind = builtInKind(cfg.codec);
    if (!kind) throw new Error(`the app has no decoder of its own for ${cfg.codec}`);
    this.name = kind.name;
    // the track's rate, for sound before the decoder has read one (a chunk
    // whose frames all came out as silence)
    this.rate = cfg.sampleRate;
    if (kind.pcm) {
      this.dec = new PcmDecoder(cfg.codec, cfg.numberOfChannels, cfg.sampleRate);
      this.ready = Promise.resolve();
      return;
    }
    this.ready = loadDecoders().then((mod) => {
      if (!this.closed) this.dec = new mod.SoundDecoder(kind.wasm);
    });
  }

  decode(chunk) {
    if (this.closed) throw new Error('the decoder is closed');
    const bytes = new Uint8Array(chunk.byteLength);
    chunk.copyTo(bytes);
    this.queue.push({ bytes, timestamp: chunk.timestamp });
    this.decodeQueueSize = this.queue.length;
    this.run();
  }

  async run() {
    if (this.running) return;
    this.running = true;
    try {
      await this.ready;
      while (this.queue.length && !this.closed) {
        const c = this.queue.shift();
        this.decodeQueueSize = this.queue.length;
        let planes;
        try {
          planes = this.dec.decode(c.bytes);
        } catch (e) {
          this.error(new Error(`the built-in ${this.name} decoder: ${e && e.message ? e.message : e}`));
          continue;
        }
        const n = this.dec.samples();
        if (!n) continue;
        this.output(new AudioData({ format: 'f32-planar', sampleRate: this.dec.sample_rate() || this.rate, numberOfFrames: n, numberOfChannels: this.dec.channels(), timestamp: c.timestamp, data: planes }));
        // (a long run of chunks lets the page breathe between them)
        if (this.queue.length && this.queue.length % 64 === 0) await new Promise((r) => setTimeout(r, 0));
      }
    } catch (e) {
      // the chunks still queued go too, as on close(): until another
      // decode() nothing would run them, and a flush() would wait for ever
      this.queue.length = 0;
      this.decodeQueueSize = 0;
      this.error(e);
    } finally {
      this.running = false;
      if (!this.queue.length) for (const r of this.idle.splice(0)) r();
    }
  }

  flush() {
    if (!this.queue.length && !this.running) return Promise.resolve();
    return new Promise((r) => this.idle.push(r));
  }

  close() {
    this.closed = true;
    this.queue.length = 0;
    this.decodeQueueSize = 0;
    for (const r of this.idle.splice(0)) r();
    if (this.dec) {
      this.dec.free();
      this.dec = null;
    }
  }
}
