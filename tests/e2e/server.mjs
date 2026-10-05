// A tiny static server for the web app (WebGPU / WebCodecs need a secure
// context, and localhost counts).
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';

const MIME = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css', '.wasm': 'application/wasm', '.json': 'application/json', '.mp4': 'video/mp4', '.ts': 'text/plain', '.webp': 'image/webp', '.png': 'image/png', '.webm': 'video/webm' };

export function serve(root, port = 0) {
  const srv = http.createServer((req, res) => {
    let p = decodeURIComponent(new URL(req.url, 'http://x').pathname);
    if (p.endsWith('/')) p += 'index.html';
    const file = path.join(root, p);
    if (!file.startsWith(root) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
      res.statusCode = 404;
      res.end('not found');
      return;
    }
    res.setHeader('content-type', MIME[path.extname(file)] || 'application/octet-stream');
    res.setHeader('cross-origin-opener-policy', 'same-origin');
    res.setHeader('cross-origin-embedder-policy', 'require-corp');
    fs.createReadStream(file).pipe(res);
  });
  return new Promise((resolve) => srv.listen(port, '127.0.0.1', () => resolve({ srv, port: srv.address().port })));
}

if (process.argv[1] && process.argv[1].endsWith('server.mjs') && process.argv.includes('--serve')) {
  const root = path.resolve(process.argv[process.argv.indexOf('--serve') + 1] || 'web');
  serve(root, 8765).then(({ port }) => console.log(`serving ${root} on http://127.0.0.1:${port}/`));
}
