// The decoders built into the app, for the codecs a browser's WebCodecs may
// lack: H.264 in the main WebAssembly module, the others in the decoders
// module (pkg-dec); each runs in the decode workers (softworker.js), or on
// the page where they cannot run.

const BUILT_IN = [
  { id: 'h264', name: 'H.264', test: /^avc[13]/ },
  { id: 'hevc', name: 'HEVC', test: /^(hev1|hvc1)/ },
  { id: 'vp9', name: 'VP9', test: /^vp09/ },
  { id: 'vp8', name: 'VP8', test: /^vp8/ },
  { id: 'av1', name: 'AV1', test: /^av01/ },
];

/** The built-in decoder for a WebCodecs codec string, or null. */
export function builtInFor(codec) {
  return BUILT_IN.find((b) => b.test.test(codec || '')) || null;
}

let decoders = null;

/** The decoders module (HEVC, VP9, VP8, AV1, and the AC-3 / E-AC-3 and DTS sound decoder), loaded on first use. */
export function loadDecoders() {
  if (!decoders)
    decoders = (async () => {
      const mod = await import('./pkg-dec/unflash_decoders.js');
      await mod.default();
      return mod;
    })();
  return decoders;
}
