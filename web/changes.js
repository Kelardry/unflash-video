// What's new: the changelog (CHANGELOG.md, copied beside the page by
// build.sh) as days of changes, each change with the time it went live, and
// which of them a visitor coming back has not seen yet.

import { stored, store } from './storage.js';

const SEEN_KEY = 'unflash:changesSeen';
/** Where the films of the changes are (whatsnew.mjs in the tests makes them). */
const SHOTS = 'whatsnew/';
/** Settings earlier versions kept: a browser that has them has been here before. */
const OLD_KEYS = ['unflash:auto', 'unflash:playerSize', 'unflash:thumbSize', 'unflash.alerts'];

export function escapeHtml(s) {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

/** The changelog's inline markdown (bold, italics, code, links) as HTML. */
function inline(s) {
  return escapeHtml(s)
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<b>$1</b>')
    .replace(/\*([^*]+)\*/g, '<i>$1</i>')
    .replace(/\[([^\]]+)\]\((https?:\/\/[^)\s"]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
}

/**
 * The changelog as `{ days: [{ date, label, items: [{ at, timed, tour, shot, html }] }] }`,
 * newest first as the file has them: a day per `## YYYY-MM-DD` heading, a
 * change per `- ` line (lines indented under it continue it), `at` the time
 * it went live in ms, from the `<!-- HH:MM -->` (UTC) it starts with, or the
 * day's noon when it has none (`timed` false). After the time, `tour:id`
 * names the tour (tours.js) that shows the change and `shot:name` the film
 * of it (whatsnew/). Comments on lines of their own are notes to the file's
 * editors.
 */
export function parseChangelog(md) {
  const days = [];
  let day = null;
  let item = null;
  const flush = () => {
    if (item && day) day.items.push({ at: item.at, timed: item.timed, tour: item.tour, shot: item.shot, html: inline(item.text.trim()) });
    item = null;
  };
  for (const line of md.replace(/^\s*<!--[\s\S]*?-->[ \t]*$/gm, '').split(/\r?\n/)) {
    const head = /^##\s+(\d{4})-(\d{2})-(\d{2})\b\s*(.*)$/.exec(line);
    if (head) {
      flush();
      const [, y, m, d, rest] = head;
      const noon = Date.UTC(+y, +m - 1, +d, 12);
      const date = new Date(noon).toLocaleDateString(undefined, { day: 'numeric', month: 'long', year: 'numeric', timeZone: 'UTC' });
      const note = rest.replace(/^[—–-]\s*/, '').trim();
      day = { date: `${y}-${m}-${d}`, label: note ? `${date}: ${note}` : date, items: [], y: +y, m: +m, d: +d };
      days.push(day);
      continue;
    }
    const bullet = /^[-*]\s+(?:<!--\s*(\d{1,2}):(\d{2})((?:\s+\w+:[\w-]+)*)\s*-->\s*)?(.*)$/.exec(line);
    if (bullet && day) {
      flush();
      const timed = bullet[1] !== undefined;
      const tag = (name) => (new RegExp(`\\b${name}:([\\w-]+)`).exec(bullet[3] || '') || [])[1] || null;
      item = { at: timed ? Date.UTC(day.y, day.m - 1, day.d, +bullet[1], +bullet[2]) : Date.UTC(day.y, day.m - 1, day.d, 12), timed, tour: tag('tour'), shot: tag('shot'), text: bullet[4] };
      continue;
    }
    if (item && /^\s+\S/.test(line)) item.text += ' ' + line.trim();
    else flush();
  }
  flush();
  return { days: days.map(({ date, label, items }) => ({ date, label, items })).filter((d) => d.items.length) };
}

/** The time of the newest change in a changelog (0 when it has none). */
export function newestChange(log) {
  let at = 0;
  for (const d of log.days) for (const it of d.items) at = Math.max(at, it.at);
  return at;
}

/** The changes that went live after `since` (ms), by day, newest first. */
export function changesSince(log, since) {
  return log.days.map((d) => ({ ...d, items: d.items.filter((it) => it.at > since) })).filter((d) => d.items.length);
}

export async function loadChangelog(url = 'CHANGELOG.md') {
  const res = await fetch(url, { cache: 'no-cache' });
  if (!res.ok) throw new Error(`${url}: ${res.status}`);
  const log = parseChangelog(await res.text());
  // the films of the changes: their sizes and what each shows (none: the changes go without)
  log.shots = {};
  try {
    const s = await fetch(SHOTS + 'shots.json', { cache: 'no-cache' });
    if (s.ok) log.shots = await s.json();
  } catch (e) {
    /* no films */
  }
  return log;
}

/** The newest change this browser has been shown (ms), or null. */
export function changesSeen() {
  const v = Number(stored(SEEN_KEY));
  return v > 0 ? v : null;
}

/** The changes up to `at` (ms) have been shown here (with storage blocked, the notice comes back next time). */
export function markChangesSeen(at) {
  store(SEEN_KEY, String(at));
}

/** Whether the settings of an earlier visit are here (read it before this visit writes its own). */
export function hadEarlierSettings() {
  return OLD_KEYS.some((k) => stored(k) !== null);
}

/**
 * A day of changes as HTML (`isNew` marks the changes to highlight; a
 * change with a tour gets a "show me" button, `data-tour` its id; one with
 * a film in `shots` (shots.json) gets the film, which wireShots plays).
 */
export function renderDay(day, isNew = () => false, brief = false, shots = {}) {
  const items = day.items
    .map((it) => {
      const tour = it.tour ? ` <button class="small show-me" data-tour="${escapeHtml(it.tour)}" title="A short tour of this">show me</button>` : '';
      const film = shotHtml(it.shot, shots);
      return `<li${isNew(it) ? ' class="new"' : ''}>${brief ? briefly(it.html, film) + tour : it.html + tour + film}</li>`;
    })
    .join('');
  return `<h3>${escapeHtml(day.label)}</h3><ul>${items}</ul>`;
}

/**
 * A change's film: muted, looped, loaded only once it is in view; its last
 * picture stands for it until then (and for good, for whoever asks their
 * system for less motion). A click plays or pauses it. A film with sound
 * has a button of its own for it, off to start with.
 */
function shotHtml(name, shots) {
  const s = name && shots[name];
  if (!s) return '';
  const src = SHOTS + encodeURIComponent(name);
  const alt = escapeHtml(s.alt || '').replace(/"/g, '&quot;');
  const sound = s.sound ? `<button type="button" class="small shot-sound" aria-pressed="false" title="Hear the film's sound (it starts again from the beginning)">sound off</button>` : '';
  return (
    `<figure class="shot${s.sound ? ' has-sound' : ''}" style="max-width:${s.w + 14}px"><button type="button" class="shot-film paused" aria-label="${alt} (a film: play or pause it)" title="${alt}">` +
    `<video muted loop playsinline preload="none" poster="${src}.webp" width="${s.w}" height="${s.h}">` +
    `<source src="${src}.webm" type="video/webm"></video></button>${sound}</figure>`
  );
}

/**
 * A change as its headline, which opens to the rest (and its film, `film`):
 * its opening bold words (the whole of it when there are none). Where the
 * headline is not a sentence of its own, the rest is the whole change again.
 */
function briefly(html, film = '') {
  const m = /^<b>([\s\S]*?)<\/b>([\s\S]*)$/.exec(html);
  if (!m || (!m[2].trim() && !film)) return html + film;
  const head = m[1];
  const own = /[.!?]\s*$/.test(head) || /^\s*:/.test(m[2]);
  const rest = own ? m[2].replace(/^\s*:?\s*/, '') : html;
  return `<details class="brief"><summary>${head.replace(/[.:]\s*$/, '')}</summary>${rest}${film}</details>`;
}

/**
 * Play the films in `box` while they are in view (at least half of one)
 * and pause them when they leave; with less motion asked for, none plays
 * by itself. A click plays or pauses one, and it stays as it was left. A
 * film's sound button plays it from the beginning with its sound (the
 * others' sound goes off), or turns the sound off again.
 */
export function wireShots(box, root = null) {
  // (the films the box held before are gone, and what watched them with them)
  if (box.shotWatch) box.shotWatch.disconnect();
  box.shotWatch = null;
  const films = [...box.querySelectorAll('.shot-film')];
  if (!films.length) return;
  const still = !!(window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches);
  const play = (v) => {
    const p = v.play();
    if (p && p.catch) p.catch(() => {});
  };
  const hush = (k) => {
    k.setAttribute('aria-pressed', 'false');
    k.textContent = 'sound off';
    k.parentNode.querySelector('video').muted = true;
  };
  for (const b of films) {
    const v = b.querySelector('video');
    v.addEventListener('play', () => b.classList.remove('paused'));
    v.addEventListener('pause', () => b.classList.add('paused'));
    b.addEventListener('click', () => {
      b.dataset.chosen = v.paused ? 'play' : 'pause';
      if (v.paused) play(v);
      else v.pause();
    });
    const k = b.parentNode.querySelector('.shot-sound');
    if (!k) continue;
    k.addEventListener('click', () => {
      if (!v.muted) return hush(k);
      for (const other of box.querySelectorAll('.shot-sound[aria-pressed="true"]')) hush(other);
      k.setAttribute('aria-pressed', 'true');
      k.textContent = 'sound on';
      v.muted = false;
      v.currentTime = 0;
      b.dataset.chosen = 'play';
      play(v);
    });
  }
  if (typeof IntersectionObserver === 'undefined') return;
  const seen = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        const v = e.target.querySelector('video');
        const wanted = e.target.dataset.chosen ? e.target.dataset.chosen === 'play' : !still;
        if (e.isIntersecting && e.intersectionRatio >= 0.5) {
          if (wanted && v.paused) play(v);
        } else if (!v.paused) v.pause();
      }
    },
    { root, threshold: [0, 0.5] }
  );
  for (const b of films) seen.observe(b);
  box.shotWatch = seen;
}
