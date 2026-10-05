// The guided tour. A first visit gets the getting-started tour in three
// parts, each where it belongs: the start page, then the first video, then
// the first section; every step lights up its part of the page, with the
// card on screen beside it; the page's own keys wait while it shows; each
// part comes once, and the guide's button runs it again. A visitor back
// after an update gets the tours of the changes since, in their places, and
// What's new offers a "show me" for each. An automated browser gets no tour
// unless it asks (?tour=1), so the other tests see the page as it is.
//   node tests/e2e/tour.mjs
import fs from 'node:fs';
import path from 'node:path';
import { MEDIA, assert, chromium, open, still } from './playwright.mjs';

const { browser, port, close } = await chromium();
const errors = [];
const results = {};
const VIEW = { width: 1280, height: 800 };

/** The tour on screen: its card, its light, and what the light should be on. */
const tourNow = (page) =>
  page.evaluate(() => {
    const t = document.querySelector('.tour');
    if (!t) return null;
    const c = t.querySelector('.tour-card').getBoundingClientRect();
    const s = t.querySelector('.tour-spot').getBoundingClientRect();
    return {
      count: t.querySelector('.tour-count').textContent,
      title: t.querySelector('.tour-title').textContent,
      centred: t.classList.contains('centred'),
      card: { left: c.left, top: c.top, right: c.right, bottom: c.bottom },
      spot: { left: s.left, top: s.top, right: s.right, bottom: s.bottom },
      vw: document.documentElement.clientWidth,
      vh: document.documentElement.clientHeight,
    };
  });

/** Step through the tour on screen with the keyboard, checking every step; returns the titles. */
async function walk(page, name) {
  const titles = [];
  for (let k = 0; k < 20; k++) {
    await still(page);
    const t = await tourNow(page);
    if (!t) break;
    titles.push(t.title);
    const inView = t.card.left >= 0 && t.card.top >= 0 && t.card.right <= t.vw + 0.5 && t.card.bottom <= t.vh + 0.5;
    assert(inView, `${name}, "${t.title}": the card is on screen: ${JSON.stringify(t)}`);
    if (!t.centred) {
      assert(t.spot.right - t.spot.left > 4 && t.spot.bottom - t.spot.top > 4, `${name}, "${t.title}": something is lit`);
      // a card beside a light it fits beside does not cover it
      const small = t.spot.bottom - t.spot.top < t.vh / 3 && t.spot.right - t.spot.left < t.vw / 2;
      const overlap = !(t.card.right <= t.spot.left || t.card.left >= t.spot.right || t.card.bottom <= t.spot.top || t.card.top >= t.spot.bottom);
      if (small) assert(!overlap, `${name}, "${t.title}": the card leaves what it points at in view: ${JSON.stringify(t)}`);
    }
    await page.keyboard.press('ArrowRight');
  }
  await page.waitForFunction(() => !document.querySelector('.tour'), null, { timeout: 5000 });
  console.log(`${name}: ${titles.length} steps: ${titles.join(' · ')}`);
  return titles;
}

const openClip = (page, name) => open(page, { name, mimeType: 'video/webm', buffer: fs.readFileSync(path.join(MEDIA, name)) });

try {
  // --- a first visit ------------------------------------------------------------
  const ctx = await browser.newContext({ viewport: VIEW });
  const page = await ctx.newPage();
  page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));
  // under automation, nothing unless asked (the plan is made with the changes)
  await page.goto(`http://127.0.0.1:${port}/?cpu=1&auto=0`);
  await page.waitForFunction(() => window.__unflash && window.__unflash.tours && window.__unflash.changes, null, { timeout: 30000 });
  assert(await page.evaluate(() => !window.__unflash.tours.auto && !window.__unflash.tours.timer && !document.querySelector('.tour')), 'an automated browser gets no tour unless it asks');
  await page.evaluate(() => localStorage.clear());
  await page.goto(`http://127.0.0.1:${port}/?cpu=1&auto=0&tour=1`);
  await page.waitForSelector('.tour-card', { timeout: 30000 });
  results.start = await walk(page, 'start page');
  assert(results.start.length === 6 && results.start[0] === 'Welcome to Unflash', 'the start page tour: ' + results.start);
  const stored = await page.evaluate(() => JSON.parse(localStorage.getItem('unflash:tours')));
  assert(stored.plan.join() === 'start,video,section' && stored.done.start && !stored.done.video, 'the rest of the tour waits for its places: ' + JSON.stringify(stored));
  await page.reload();
  await page.waitForFunction(() => window.__unflash && window.__unflash.changes, null, { timeout: 30000 });
  const waiting = await page.evaluate(() => window.__unflash.tours.pending.join());
  assert(waiting === 'video,section' && !(await tourNow(page)), 'the start page tour comes once: ' + waiting);

  // the first video: its part of the tour once the scan is done
  await openClip(page, 'flash.webm');
  assert(await page.evaluate(() => !window.__unflash.tours.where().video && window.__unflash.tours.where(true).video), "a video's part waits for its scan (unless asked for)");
  await page.click('#btnScan');
  await page.waitForSelector('.tour-card', { timeout: 60000 });
  assert(await page.evaluate(() => !window.__unflash.state.job), 'the tour waits for the scan to finish');
  results.video = await walk(page, 'first video');
  assert(results.video.length === 6 && results.video[0] === 'The whole video at a glance', 'the video tour: ' + results.video);

  // the first section: its part once the section is ready; the marking keys wait
  await page.click('#sectionList .sec-item');
  await page.waitForSelector('.tour-card', { timeout: 120000 });
  assert(/passes|fails/.test(await page.textContent('#wsVerdict')), 'the section tour waits for the check');
  await page.evaluate(() => {
    const u = window.__unflash;
    u.state.selection.clear();
    u.state.selection.add(3);
  });
  await page.keyboard.press('r');
  assert(!(await page.evaluate(() => (window.__unflash.currentSection().edits || {})[3])), 'R does not mark a frame while the tour shows');
  assert(await tourNow(page), 'and the tour stays');
  // the browser's own shortcuts are left to it while the tour shows (zoom,
  // find, reload: it held them back too); the page's own keys wait
  const held = await page.evaluate(() =>
    [{ key: '=', ctrlKey: true }, { key: 'f', ctrlKey: true }, { key: 'F5' }, { key: 'r' }].map((k) => {
      const e = new KeyboardEvent('keydown', { ...k, bubbles: true, cancelable: true });
      document.body.dispatchEvent(e);
      return e.defaultPrevented;
    })
  );
  assert(JSON.stringify(held) === '[false,false,false,true]', 'while the tour shows, Ctrl+=, Ctrl+F and F5 are left to the browser, R is not: ' + JSON.stringify(held));
  results.section = await walk(page, 'first section');
  assert(results.section.length >= 4 && results.section[0] === 'Does it pass?', 'the section tour: ' + results.section);
  const done = await page.evaluate(() => JSON.parse(localStorage.getItem('unflash:tours')).done);
  assert(done.start && done.video && done.section, 'every part has had its turn: ' + JSON.stringify(done));

  // the guide's button runs it again, for where the page is (Esc ends it)
  await page.click('#btnHome');
  await page.click('#btnTour');
  await page.waitForSelector('.tour-card', { timeout: 5000 });
  const again = await tourNow(page);
  assert(/^Fixing a section/.test(again.count) && !(await page.evaluate(() => document.body.classList.contains('guide-open'))), 'the guide closes and the section part runs again: ' + again.count);
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => !document.querySelector('.tour'), null, { timeout: 5000 });

  // the guide over a video lists every part of the screen, and each one's
  // "show me" lights it up with its card (the switches a section shows only
  // when it needs them say they are not on screen)
  results.parts = [];
  const partIds = await page.evaluate(() => [...document.querySelectorAll('#guideParts .show-part')].map((b) => b.dataset.part));
  assert(partIds.length >= 20, 'the guide lists the parts of the screen: ' + partIds);
  for (const id of partIds) {
    await page.click('#btnHome');
    await page.waitForFunction(() => document.body.classList.contains('guide-open'), null, { timeout: 5000 });
    const before = await page.textContent('#toast');
    await page.click(`#guideParts .show-part[data-part="${id}"]`);
    await page.waitForFunction((t) => document.querySelector('.tour-card') || document.querySelector('#toast').textContent !== t, before, { timeout: 5000 });
    const card = await tourNow(page);
    results.parts.push({ id, title: card && card.title, guide: await page.evaluate(() => document.body.classList.contains('guide-open')) });
    if (card) {
      assert(!results.parts[results.parts.length - 1].guide, `"${id}": the guide makes way for the part`);
      await page.keyboard.press('Escape');
      await page.waitForFunction(() => !document.querySelector('.tour'), null, { timeout: 5000 });
    } else await page.keyboard.press('Escape');
  }
  const unseen = results.parts.filter((p) => !p.title).map((p) => p.id);
  console.log('the screen, part by part:', results.parts.map((p) => p.title || `(${p.id}: not on screen)`).join(' · '));
  assert(unseen.join() === 'soften,blend', 'every part of the screen lights up but the switches this section does not need: ' + unseen);
  await ctx.close();

  // --- back after an update ----------------------------------------------------------
  const ctx2 = await browser.newContext({ viewport: VIEW });
  const p2 = await ctx2.newPage();
  p2.on('pageerror', (e) => errors.push('pageerror (back): ' + e.message));
  await p2.goto(`http://127.0.0.1:${port}/?cpu=1&auto=0`);
  await p2.waitForFunction(() => window.__unflash && window.__unflash.changes, null, { timeout: 30000 });
  // the tours of the changes after this time are what's new
  const before = await p2.evaluate(() => {
    const items = window.__unflash.changes.log.days.flatMap((d) => d.items).filter((it) => it.tour);
    return { at: Math.min(...items.map((it) => it.at)) - 60000, tours: items.map((it) => it.tour) };
  });
  assert(before.tours.includes('tours') && before.tours.includes('whole-video'), "the changelog names this round's tours: " + before.tours);
  // (as a browser that was here before there were tours keeps it: the last visit and nothing else)
  await p2.evaluate((at) => {
    localStorage.clear();
    localStorage.setItem('unflash:changesSeen', String(at));
  }, before.at);
  await p2.goto(`http://127.0.0.1:${port}/?cpu=1&auto=0&tour=1`);
  await p2.waitForSelector('.tour-card', { timeout: 30000 });
  results.back = { pending: await p2.evaluate(() => window.__unflash.tours.pending.slice()) };
  const first = await tourNow(p2);
  assert(first.title === 'A guided tour' && /^New/.test(first.count), 'the start page shows what is new there: ' + JSON.stringify(first));
  await p2.keyboard.press('Escape');
  // "show me" in What's new: a section's tour, with no video open, waits for its place
  await p2.waitForFunction(() => !document.querySelector('.tour'));
  // (the card on the start page lists the newest few, and those with a tour
  // among them have their "show me"; everything that changed has the rest)
  const onCard = await p2.$$eval('#newsList .show-me', (b) => b.map((x) => x.dataset.tour));
  assert(onCard.every((id) => before.tours.includes(id)), 'the card of what is new offers "show me" for tours there are: ' + onCard);
  await p2.click('#btnNewsAll');
  const showMe = await p2.$$eval('#changesList .show-me', (b) => b.map((x) => x.dataset.tour));
  assert(['tours', 'section-sound', 'whole-video', 'findings', 'project-files'].every((id) => showMe.includes(id)), 'What\'s new offers "show me" beside each: ' + showMe);
  await p2.click('#changesList .show-me[data-tour="section-sound"]');
  await p2.waitForFunction(() => /Open a section/.test(document.querySelector('#toast').textContent), null, { timeout: 5000 });
  await openClip(p2, 'flash.webm');
  await p2.click('#btnScan');
  await p2.waitForSelector('.tour-card', { timeout: 60000 });
  results.backVideo = await walk(p2, 'back, a video');
  assert(results.backVideo.includes('Back to the whole video') && results.backVideo.includes('Project files'), 'the new things of the video page: ' + results.backVideo);
  await p2.click('#sectionList .sec-item');
  await p2.waitForSelector('.tour-card', { timeout: 120000 });
  results.backSection = await walk(p2, 'back, a section');
  assert(results.backSection.includes('Sound in the section player') && results.backSection.includes('What still fails, and how to fix it'), 'the new things of a section: ' + results.backSection);
  assert((await p2.evaluate(() => window.__unflash.tours.pending.length)) === 0, 'and nothing waits');
  await ctx2.close();

  if (errors.length) throw new Error('page errors:\n' + errors.join('\n'));
  console.log('TOUR OK');
} catch (e) {
  console.error(e);
  process.exitCode = 1;
} finally {
  await close();
}
