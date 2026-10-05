// A check for the browser tests, run in the page (page.evaluate): the files
// of the file input `input` come in pairs, a transport stream (.ts, .m2ts)
// and then ffmpeg's MP4 of it; each stream is opened as the app opens it,
// and every sample the app reads out of it (ts.js, gathering it from the
// stream's packets) is compared with the MP4's: its bytes (the MP4's NAL
// units without their trailing zeros, as the app leaves them out), its key
// frame flag and its time. What differs is listed in `bad`, per stream.
export async function readsAsTheMp4(input) {
  const wasm = await import('./pkg/unflash.js');
  const { Movie } = await import('./media.js');
  const files = Array.from(document.querySelector(input).files);
  // a length-prefixed sample with its NAL units' trailing zeros left out
  const lean = (b) => {
    const out = [];
    for (let i = 0; i + 4 <= b.length; ) {
      const n = ((b[i] << 24) | (b[i + 1] << 16) | (b[i + 2] << 8) | b[i + 3]) >>> 0;
      let e = i + 4 + n;
      while (e > i + 4 && b[e - 1] === 0) e--;
      const len = e - i - 4;
      out.push(len >>> 24, (len >>> 16) & 255, (len >>> 8) & 255, len & 255, ...b.subarray(i + 4, e));
      i += 4 + n;
    }
    return Uint8Array.from(out);
  };
  const report = [];
  for (let k = 0; k + 1 < files.length; k += 2) {
    const ts = await Movie.open(files[k], wasm);
    const mp4 = await Movie.open(files[k + 1], wasm);
    const r = { name: files[k].name, format: ts.format, packet: ts.ts && ts.ts.packet, width: ts.width, height: ts.height, frames: ts.frameCount, audio: ts.audio && ts.audio.codec, bad: [] };
    for (const [kind, x, y] of [
      ['video', ts.v, mp4.v],
      ['audio', ts.a, mp4.a],
    ]) {
      if (x.size.length !== y.size.length) {
        r.bad.push(`${kind}: ${x.size.length} samples, the MP4 ${y.size.length}`);
        continue;
      }
      r[kind + 'Samples'] = x.size.length;
      const x0 = x.pts[0];
      const y0 = y.pts[0];
      for (let i = 0; i < x.size.length && r.bad.length <= 5; i++) {
        const a = (await ts.reader.read(x.offset[i], x.size[i])).slice();
        let b = (await mp4.reader.read(y.offset[i], y.size[i])).slice();
        if (kind === 'video') b = lean(b);
        if (a.length !== b.length || a.some((v, j) => v !== b[j])) r.bad.push(`${kind} sample ${i}: ${a.length} bytes, the MP4's ${b.length}`);
        if (!!x.sync[i] !== !!y.sync[i]) r.bad.push(`${kind} sample ${i}: key frame ${x.sync[i]} vs ${y.sync[i]}`);
        if (Math.abs(x.pts[i] - x0 - (y.pts[i] - y0)) > 40) r.bad.push(`${kind} sample ${i}: at ${x.pts[i] - x0} µs, the MP4's ${y.pts[i] - y0}`);
      }
    }
    ts.close();
    mp4.close();
    report.push(r);
  }
  return report;
}
