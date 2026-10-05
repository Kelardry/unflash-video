#!/usr/bin/env node
// The published site, made from web/ once build.sh has run. The page's
// code (its modules, style sheet and WebAssembly) goes in a folder of its
// own, v/<build>/, and index.html and bench.html point there; the data
// (the test clips, What's new's films, the changelog) stay beside them. A
// page so only ever loads the files of the build it started with: a copy of
// one build's file in the browser's cache can't meet another build's, and
// a page left open over an update goes on loading its own build's files
// when it next needs one (a decoder, a worker).
//
// Builds replaced lately stay published next to the new one, for the pages
// still open on them: --keep-from names the site as it stands (its URL, or
// a folder), whose versions.json lists its builds and their files, and
// every build replaced less than --keep-days ago (--keep builds at most,
// the new one included) is copied over file by file, each checked against
// its hash (a build that can't be copied whole is left out). A site from
// before this layout has its code at the top: those files are kept there
// the same way, as the build "unversioned". versions.json also names the
// current build, which the app compares with its own.
//
//   node site.mjs OUT [--build ID] [--keep-from URL|DIR] [--keep-days 3] [--keep 20] [--now ISO]
//
// The build is named --build, else by $GITHUB_SHA, else by the commit
// checked out (12 characters).
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

// (a file path, not a URL's: a checkout under a name with a space in it works too)
const ROOT = path.dirname(fileURLToPath(import.meta.url));
const WEB = path.join(ROOT, 'web');
/** Where the code goes, under the site: the app finds its build from this. */
const CODE_DIR = 'v/';

function options(argv) {
  const o = { out: null, build: null, keepFrom: null, keepDays: 3, keep: 20, now: null };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const value = () => {
      if (i + 1 >= argv.length) throw new Error(`${a} needs a value`);
      return argv[++i];
    };
    if (a === '--build') o.build = value();
    else if (a === '--keep-from') o.keepFrom = value();
    else if (a === '--keep-days') o.keepDays = +value();
    else if (a === '--keep') o.keep = +value();
    else if (a === '--now') o.now = value();
    else if (a.startsWith('--')) throw new Error(`unknown option ${a}`);
    else if (!o.out) o.out = a;
    else throw new Error(`one output folder only: ${a}`);
  }
  if (!o.out) throw new Error('usage: node site.mjs OUT [--build ID] [--keep-from URL|DIR] [--keep-days 3] [--keep 20]');
  return o;
}

function buildName(given) {
  const id = given || (process.env.GITHUB_SHA || '').slice(0, 12) || execSync('git rev-parse --short=12 HEAD', { cwd: ROOT, encoding: 'utf8' }).trim();
  if (!/^[0-9A-Za-z._-]{1,64}$/.test(id) || id === 'unversioned') throw new Error(`not a build name: ${id}`);
  return id;
}

/** Every file under `dir`, as paths relative to it with forward slashes. */
function walk(dir, rel = '') {
  const out = [];
  for (const e of fs.readdirSync(path.join(dir, rel), { withFileTypes: true })) {
    const p = rel ? `${rel}/${e.name}` : e.name;
    if (e.isDirectory()) out.push(...walk(dir, p));
    else if (e.isFile()) out.push(p);
  }
  return out;
}

/** The page's code, of the files in web/: what runs, rather than what it shows or reads. */
function isCode(p) {
  return /^[^/]+\.(js|css)$/.test(p) || /^pkg(-dec)?\//.test(p);
}

const sha256 = (buf) => crypto.createHash('sha256').update(buf).digest('hex');

function write(out, rel, buf) {
  const to = path.join(out, rel);
  fs.mkdirSync(path.dirname(to), { recursive: true });
  fs.writeFileSync(to, buf);
}

/**
 * index.html and bench.html with their references to the code (a script's
 * src, a style sheet's href, a module's import) pointed into its folder.
 */
function pointAtCode(html, code, dir) {
  let s = html;
  for (const p of code) {
    for (const q of ['"', "'"]) {
      s = s.split(`${q}./${p}${q}`).join(`${q}./${dir}${p}${q}`);
      s = s.split(`${q}${p}${q}`).join(`${q}${dir}${p}${q}`);
    }
  }
  for (const p of code) {
    const left = new RegExp(`(["'])(\\./)?${p.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\1`);
    if (left.test(s)) throw new Error(`a reference to ${p} was left pointing at the top of the site`);
  }
  return s;
}

/** The site as it stands (a URL or a folder): its file at `rel`, or null. */
function source(from) {
  if (/^https?:\/\//.test(from)) {
    const base = from.endsWith('/') ? from : from + '/';
    return async (rel) => {
      for (let attempt = 0; attempt < 3; attempt++) {
        try {
          const r = await fetch(base + rel, { cache: 'no-store' });
          if (r.status === 404) return null;
          if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
          return Buffer.from(await r.arrayBuffer());
        } catch (e) {
          if (attempt === 2) throw new Error(`${base + rel}: ${e.message}`);
          await new Promise((res) => setTimeout(res, 1000 * (attempt + 1)));
        }
      }
      return null;
    };
  }
  return async (rel) => {
    const p = path.join(from, rel);
    return fs.existsSync(p) ? fs.readFileSync(p) : null;
  };
}

/** Run `fn` over `items`, `n` at a time. */
async function pool(items, n, fn) {
  const out = new Array(items.length);
  let next = 0;
  await Promise.all(
    Array.from({ length: Math.min(n, items.length) }, async () => {
      while (next < items.length) {
        const k = next++;
        out[k] = await fn(items[k], k);
      }
    })
  );
  return out;
}

/**
 * The builds of the site at `from` still worth keeping at `now`, their
 * files fetched and checked: `{ build, files: Map(rel -> buffer) }` each.
 */
async function keptBuilds(from, now, { keepDays, keep, current }, code) {
  const get = source(from);
  const listed = await get('versions.json');
  let builds;
  if (listed) {
    builds = JSON.parse(listed.toString('utf8')).builds || [];
  } else {
    // a site from before this layout: its code at the top, as it was
    // until now (the files it has of those this build names)
    const files = {};
    await pool(code, 8, async (p) => {
      const buf = await get(p);
      if (buf) files[p] = sha256(buf);
    });
    builds = Object.keys(files).length ? [{ id: 'unversioned', dir: '', since: null, until: null, files }] : [];
    if (builds.length) console.log(`${from} has no versions.json: its ${Object.keys(files).length} code files at the top are kept as the build "unversioned"`);
  }
  const maxAge = keepDays * 86400e3;
  const candidates = builds
    .filter((b) => b.id !== current)
    .map((b) => ({ ...b, until: b.until || now.toISOString() }))
    .filter((b) => now - new Date(b.until) < maxAge)
    .sort((a, b) => new Date(b.until) - new Date(a.until))
    .slice(0, Math.max(0, keep - 1));
  const kept = [];
  for (const b of candidates) {
    const names = Object.keys(b.files);
    const files = new Map();
    let whole = true;
    await pool(names, 8, async (rel) => {
      const buf = await get(b.dir + rel).catch(() => null);
      if (!buf || sha256(buf) !== b.files[rel]) whole = false;
      else files.set(rel, buf);
    });
    if (!whole) {
      console.log(`build ${b.id}: not every file could be copied as it was published; left out`);
      continue;
    }
    kept.push({ build: b, files });
  }
  return kept;
}

async function main() {
  const o = options(process.argv.slice(2));
  const out = path.resolve(o.out);
  const build = buildName(o.build);
  const now = o.now ? new Date(o.now) : new Date();
  const dir = `${CODE_DIR}${build}/`;
  for (const need of ['pkg/unflash.js', 'pkg-dec/unflash_decoders.js', 'CHANGELOG.md', 'index.html']) {
    if (!fs.existsSync(path.join(WEB, need))) throw new Error(`web/${need} is missing: run ./build.sh first`);
  }
  const all = walk(WEB).filter((p) => !p.startsWith('node_modules/') && p !== 'versions.json' && !p.startsWith(CODE_DIR));
  const code = all.filter(isCode);
  // the builds to keep, fetched before anything is written (OUT may be where they come from)
  const kept = o.keepFrom ? await keptBuilds(o.keepFrom, now, { keepDays: o.keepDays, keep: o.keep, current: build }, code) : [];
  fs.rmSync(out, { recursive: true, force: true });
  fs.mkdirSync(out, { recursive: true });
  const files = {};
  for (const p of all) {
    let buf = fs.readFileSync(path.join(WEB, p));
    if (isCode(p)) {
      write(out, dir + p, buf);
      files[p] = sha256(buf);
    } else {
      if (/^[^/]+\.html$/.test(p)) buf = Buffer.from(pointAtCode(buf.toString('utf8'), code, dir));
      write(out, p, buf);
    }
  }
  for (const k of kept) for (const [rel, buf] of k.files) write(out, k.build.dir + rel, buf);
  const versions = {
    current: build,
    builds: [{ id: build, dir, since: now.toISOString(), until: null, files }, ...kept.map((k) => k.build)],
  };
  fs.writeFileSync(path.join(out, 'versions.json'), JSON.stringify(versions, null, 1) + '\n');
  const mb = (n) => (n / 1e6).toFixed(1);
  const size = (b) => Object.keys(b.files).reduce((s, rel) => s + fs.statSync(path.join(out, b.dir + rel)).size, 0);
  console.log(`site in ${out}: build ${build} in ${dir} (${code.length} files, ${mb(size(versions.builds[0]))} MB)`);
  for (const k of kept) console.log(`  kept build ${k.build.id} in ${k.build.dir || 'the top'} (replaced ${k.build.until}; ${k.files.size} files, ${mb(size(k.build))} MB)`);
}

main().catch((e) => {
  console.error(e.message || e);
  process.exit(1);
});
