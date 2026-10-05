// The scenes whatsnew.mjs films: one for each change in CHANGELOG.md, named
// there as `shot:NAME`. A scene sets the page up (`setup`, not filmed), says
// where the camera starts (`view`: elements to take in, or a rectangle) and
// then plays (`play`, filmed) with the pointer, clicks, keys and camera of
// `d` (whatsnew.mjs Demo). `alt` says what it shows, for whoever cannot see
// it. `browser: 'firefox'` films it in Firefox; `race` films it once for
// each of its parts (a query each), one above the other; `sound` puts in
// the film what the section player's sound played (d.recordSound, once the
// sound is on). Nothing in a scene may flash: what plays in a player is a
// section once it passes.

/** The CPU detector (SwiftShader's WebGPU, in the test browser, leaves the page no time to draw), no tour. */
const Q = 'cpu=1&tour=0';
/** ...and nothing automatic: the scene presses Scan. */
const QA = Q + '&auto=0';
/** Firefox, on its GPU. */
const FQ = 'tour=0';

const hideToast = (d) => d.eval(() => document.querySelector('#toast').classList.add('hidden'));
const click = (d, sel) => d.eval((s) => document.querySelector(s).click(), sel);
/** Scroll the stage so `sel` is at its top. */
const toTop = (d, sel) => d.eval((s) => document.querySelector(s).scrollIntoView({ block: 'start' }), sel);

/** A clip open and scanned, its `n`th section open, prepared and checked. */
async function inSection(d, clip = 'flash.mp4', n = 1) {
  await d.open(clip);
  await d.section(n);
  await hideToast(d);
}

/**
 * ...and fixed: it passes. With the fewest removals, flashes the rules
 * allow are left (three a second at most); `still` keeps the dark frames
 * instead, and nothing flashes at all: for a section a player plays.
 */
async function fixedSection(d, clip = 'flash.mp4', n = 1, still = false) {
  await inSection(d, clip, n);
  await d.job(() => click(d, still ? '#btnSuggestDark' : '#btnSuggestFewest'));
  await d.verdict(/^passes/);
  await hideToast(d);
}

/** A section that passes with nothing flashing left in it, for the player to play. */
const playable = (d, clip = 'flash.mp4') => fixedSection(d, clip, 1, true);

/** The debug report open, with the lines matching `re` selected (and scrolled to). */
async function report(d, re) {
  if (await d.eval(() => document.querySelector('#debugModal').classList.contains('hidden'))) {
    await d.click('#btnDebug');
    await d.until(() => !document.querySelector('#debugModal').classList.contains('hidden'));
    await d.camera('#debugModal .modal-box', { pad: 4 });
    await d.wait(900);
  }
  await d.eval((src) => {
    const t = document.querySelector('#debugText');
    const m = new RegExp(src, 'm').exec(t.value);
    if (!m) return;
    const a = t.value.lastIndexOf('\n', m.index) + 1;
    let b = t.value.indexOf('\n', m.index + m[0].length);
    if (b < 0) b = t.value.length;
    t.focus();
    t.setSelectionRange(a, b);
    // (the selected lines in the middle of the box)
    const line = parseFloat(getComputedStyle(t).lineHeight) || 16;
    t.scrollTop = Math.max(0, t.value.slice(0, a).split('\n').length * line - t.clientHeight / 2);
  }, re.source);
}

/** A race's play: Scan, with a stopwatch, to the end. */
async function race(d) {
  await d.click('#btnScan', { after: 0 });
  await d.clock(d.part.label);
  await d.idle();
  await d.clock(null);
}

/** A race's page: narrower, so that its job bar and timeline read at a scale that fits two. */
const RACE = {
  viewport: { width: 960, height: 700 },
  view: (d) => d.around(['#btnScan', '#timelineWrap'], 4),
  align: 'left bottom',
  corner: 'top-right',
  play: race,
};

export const SCENES = [
  // ======== 2026-10-02 ==========================================================
  {
    name: 'unmarked-copied',
    alt: "A one-minute film with two sections prepared: the first fixed, the second opened and left as it was. The export dialog's plan re-encodes one span, around the first, and copies every other frame, the second section's included.",
    query: Q,
    async setup(d) {
      await fixedSection(d, 'flash-minute.mp4');
      // the second section prepared and checked, no marks made in it
      await d.section(2);
      await hideToast(d);
    },
    view: (d) => d.around(['#sectionList', '#btnExport'], 8),
    async play(d) {
      await d.point('#sectionList', { dy: 0.3, ms: 600 });
      await d.wait(1800);
      await d.click('#btnExport', { after: 0 });
      await d.until(() => !document.querySelector('#btnDoExport').disabled);
      await d.camera('#exportModal .modal-box', { pad: 6, ms: 700 });
      await d.point('#exportPlan', { dx: 0.3, ms: 600 });
      await d.wait(3200);
    },
  },
  {
    name: 'cancel',
    alt: 'A failing section of 247 frames is prepared again, and the job cancelled halfway: Unflash says it was cancelled, and the section keeps its 247 frames and its verdict.',
    query: Q,
    async setup(d) {
      await inSection(d);
      await toTop(d, '.ws-head');
    },
    view: { x: 264, y: 0, width: 1016, height: 635 },
    async play(d) {
      await d.point('#frameCount', { ms: 600 });
      await d.wait(900);
      await d.job(async () => {
        await d.click('#btnReprepare', { after: 0 });
        // (the pointer waits on cancel, which is clicked once the job is
        // halfway: a prepare can take less than a second)
        await d.point('#btnCancelJob', { ms: 300 });
        await d.until(() => !window.__unflash.state.job || window.__unflash.state.job.pct >= 40);
        await d.page.mouse.click(d.x, d.y);
      });
      if (!/cancelled/.test(await d.eval(() => document.querySelector('#toast').textContent))) throw new Error('the prepare ended before the cancel');
      await d.camera({ x: 264, y: 165, width: 1016, height: 635 }, { ms: 900 });
      await d.point('#frameCount', { ms: 700 });
      await d.wait(2600);
    },
  },
  {
    name: 'marks-kept',
    alt: 'A frame of a failing section is selected and Suggest: blend frames started; while it runs, R marks the frame for removal. The suggestion is not applied: Unflash says the marks changed while it ran, and the frame keeps its R.',
    query: Q,
    async setup(d) {
      await inSection(d);
      await toTop(d, '.ws-head');
    },
    view: { x: 264, y: 165, width: 1016, height: 635 },
    async play(d) {
      await d.click('#frameGrid .frame[data-i="2"]', { after: 600 });
      await d.job(async () => {
        await d.click('#btnSuggestBlend', { after: 250 });
        await d.press('R', { after: 0 });
      });
      if (!/marks changed while it ran/.test(await d.eval(() => document.querySelector('#toast').textContent))) throw new Error('the suggestion ended before the mark');
      await d.wait(3600);
    },
  },
  {
    name: 'profile-wait',
    alt: 'During a scan, another profile is picked: Unflash says the scan is under way and to change the profile once it has finished, the choice goes back to the profile the scan runs under, and the scan goes on to the end.',
    query: QA,
    setup: (d) => d.open('flash-minute.mp4'),
    view: (d) => d.around(['#profileSel', '#btnScan']),
    async play(d) {
      await d.click('#btnScan', { after: 900 });
      await d.point('#profileSel', { ms: 600 });
      await d.eval(() => {
        const s = document.querySelector('#profileSel');
        s.value = 'strict';
        s.dispatchEvent(new Event('change', { bubbles: true }));
      });
      await d.wait(300);
      await d.camera('#toast', { ms: 900 });
      await d.wait(2800);
      await d.camera(['#profileSel', '#btnScan'], { ms: 900 });
      await d.idle();
      await d.wait(600);
    },
  },
  {
    name: 'joined-ts',
    alt: 'A transport stream made of two recordings joined end to end, the second with its clock an hour back: it opens at 0:00, twenty seconds long, and the scan marks the flashing in both halves on the timeline.',
    query: QA,
    view: { x: 0, y: 0, width: 1280, height: 800 },
    async play(d) {
      await d.camera({ x: 0, y: 0, width: 880, height: 550 }, { ms: 0 });
      await d.pick('label.filebtn.primary', 'joined.ts');
      await d.opened('joined.ts');
      await d.point('#videoInfo', { dx: 0.62, ms: 700 });
      await d.wait(1600);
      await d.camera({ x: 0, y: 0, width: 1280, height: 800 }, { ms: 900 });
      await d.job(() => d.click('#btnScan'));
      await d.point('#timeline', { dx: 0.4, dy: 0.4, ms: 700 });
      await d.wait(1200);
      await d.point('#timeline', { dx: 0.8, dy: 0.4, ms: 700 });
      await d.wait(1800);
    },
  },

  // ======== 2026-09-24 ==========================================================
  {
    name: 'guide-parts',
    alt: 'With a section open, the guide opens beside it at "The screen, part by part"; "show me" beside Frames closes the guide and lights up the frames on the page, with a card that says what they are for.',
    query: Q,
    setup: (d) => inSection(d),
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.click('#btnHome', { after: 700 });
      await d.camera({ x: 560, y: 0, width: 720, height: 450 }, { ms: 900 });
      await d.wait(600);
      await d.eval(() => document.querySelector('#guideParts').scrollIntoView({ block: 'start' }));
      await d.wait(1200);
      await d.eval(() => document.querySelector('.show-part[data-part="grid"]').scrollIntoView({ block: 'center' }));
      await d.wait(900);
      await d.click('.show-part[data-part="grid"]', { after: 900 });
      await d.camera('.tour-card', { pad: 90, ms: 900 });
      await d.wait(2600);
    },
  },

  // ======== 2026-09-23 ==========================================================
  {
    name: 'export-kept',
    browser: 'firefox',
    alt: 'In Firefox, a section fixed and exported; the page is reloaded and the same video opened again; the export dialog has the export still there, ready to download or verify, and Verify checks it.',
    query: FQ,
    async setup(d) {
      await fixedSection(d);
      await click(d, '#btnExport');
      await d.until(() => !document.querySelector('#btnDoExport').disabled);
      await d.job(() => click(d, '#btnDoExport'));
      await d.until(() => !document.querySelector('#btnVerifyExport').disabled);
      await click(d, '#btnCloseExport');
      await d.eval(() => window.__unflash.state.project.save());
    },
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.goto(FQ);
      await d.caption('the page, reloaded');
      await d.wait(1800);
      await d.caption('');
      await d.pick('label.filebtn.primary', 'flash.mp4');
      await d.opened('flash.mp4');
      await hideToast(d);
      await d.click('#btnExport', { after: 400 });
      await d.camera('#exportModal .modal-box', { pad: 6, ms: 900 });
      await d.wait(2400);
      await d.job(() => d.click('#btnVerifyExport'));
      await d.until(() => /frames re-scanned/.test(document.querySelector('#exportResult').textContent));
      await d.camera('#exportModal .modal-box', { pad: 6, ms: 500 });
    },
  },
  {
    name: 'firefox-lanes',
    browser: 'firefox',
    alt: "In Firefox, a two-minute H.264 film is scanned in chunks; the debug report then says how: a lane of Firefox's decoder beside Unflash's own, and how far apart the film's keyframes are.",
    query: FQ + '&auto=0&hybrid=1',
    setup: (d) => d.open('flash-long-h264.mp4'),
    view: (d) => d.around('#btnScan'),
    async play(d) {
      await d.click('#btnScan');
      await d.faster(() => d.idle(), 6);
      await d.wait(600);
      await report(d, /long files scanned in chunks.*/);
      await d.wait(2600);
      await report(d, /keyframes every.*/);
    },
  },
  {
    name: 'start-page',
    alt: 'The start page as someone coming back sees it: what is new as headlines, one opened to the rest; three short steps; and the whole guide folded underneath, opened.',
    // (a visit before these changes)
    init: `try { localStorage.setItem('unflash:changesSeen', String(Date.UTC(2026, 8, 23, 21, 0))); } catch (e) {}`,
    query: 'tour=0',
    view: { x: 0, y: 40, width: 720, height: 450 },
    async play(d) {
      await d.wait(600);
      await d.click('#newsList details.brief > summary', { after: 1800 });
      await d.click('#newsList details.brief > summary', { after: 700 });
      await d.click('#btnNewsSeen', { after: 900 });
      await d.click('#guideMore > summary', { after: 900 });
      await d.scroll('#welcome', 260, 900);
      await d.wait(1500);
    },
  },
  {
    name: 'run-up',
    alt: "Section #3 begins where an extended flash in section #2 ends. Fixed, #3 passes, and its list says the flashing just before it is section #2's, with a button that opens #2, where the extended flash is.",
    query: Q,
    async setup(d) {
      await d.open('extended.mp4');
      await click(d, '#btnDeleteAll');
      for (const [a, b] of [[0, 5], [5, 10]]) {
        await d.eval(([x, y]) => {
          document.querySelector('#addStart').value = String(x);
          document.querySelector('#addEnd').value = String(y);
          document.querySelector('#btnAddSection').click();
        }, [a, b]);
        await d.until(() => !document.querySelector('#wsBody').classList.contains('hidden'));
        await d.verdict();
      }
      await d.job(() => click(d, '#btnSuggestFewest'));
      await d.verdict(/^passes/);
      await hideToast(d);
      await toTop(d, '.ws-head');
    },
    view: (d) => d.from('.ws-head'),
    async play(d) {
      await d.point('#wsFindings .finding.run-up', { dx: 0.3 });
      await d.wait(2200);
      await d.click('#wsFindings button[data-open-section]', { after: 300 });
      await d.verdict();
      await toTop(d, '.ws-head');
      await d.camera('.ws-head', { anchor: true, ms: 500 });
      await d.point('#wsFindings .finding', { dx: 0.3 });
      await d.wait(600);
    },
  },
  {
    name: 'calm-player',
    alt: "A fixed section plays: a dim bar moves along the frames at most every 0.4 s, and the live meter's bars fall back slowly. The dim switch turns the thumbnails and the player up, then down again.",
    query: Q,
    async setup(d) {
      await playable(d);
      await click(d, '#liveToggle');
      await click(d, '.thumb-size button[data-thumb="s"]');
      await d.eval(() => {
        const s = document.querySelector('#previewSpeed');
        s.value = '0.5';
        s.dispatchEvent(new Event('change', { bubbles: true }));
      });
      await d.eval(() => (document.querySelector('#stage').scrollTop = 0));
    },
    view: (d) => d.from('#playerBox'),
    async play(d) {
      await d.click('#btnPreviewPlay', { after: 1200 });
      await d.scrollTo('#frameGrid');
      await d.wait(4200);
      await d.scrollTo('#playerBox');
      await d.click('#dimToggle', { after: 1800 });
      await d.click('#dimToggle', { after: 1400 });
      await d.click('#btnPreviewStop', { after: 300 });
    },
  },
  {
    name: 'blend-frames',
    alt: 'Suggest: blend frames on a failing section: the flashing frames are blended with their neighbours (marked B), nothing is removed, the blend slider appears and the section passes. A double-click shows one of them at full size.',
    query: Q,
    async setup(d) {
      await inSection(d);
      await toTop(d, '.ws-head');
    },
    view: (d) => d.from('.ws-head'),
    async play(d) {
      await d.job(() => d.click('#btnSuggestBlend'));
      await d.verdict(/^passes/);
      await d.wait(900);
      await d.eval(() => document.querySelector('#frameGrid .frame[data-i="42"]').scrollIntoView({ block: 'start' }));
      await d.camera('#frameGrid .frame[data-i="42"]', { anchor: true, pad: 30, ms: 700 });
      await d.wait(2200);
      await d.click('#frameGrid .frame[data-i="44"]', { dbl: true, after: 400 });
      await d.until(() => !document.querySelector('#frameViewer').classList.contains('hidden'));
      await d.camera('#frameViewer .viewer-box', { pad: 4, ms: 700 });
      await d.wait(2400);
      await d.press('Escape');
    },
  },
  {
    name: 'fragmented',
    alt: 'A recording whose index is spread through the file in fragments opens; the debug report says it is fragmented.',
    query: QA,
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.pick('label.filebtn.primary', 'recording.mp4');
      await d.opened('recording.mp4');
      await d.wait(500);
      await report(d, /\(fragmented\).*/);
      await d.wait(1200);
    },
  },
  {
    name: 'take-over',
    browser: 'firefox',
    alt: 'The same scan in Firefox twice, its lanes as slow as on a Windows PC: above, the scan waits on each slow lane; below, an idle lane takes over the stretch the scan waits for, and it ends sooner.',
    ...RACE,
    // (as it was then: before the rebalancing)
    race: [
      { label: 'before: waiting on a slow lane', query: FQ + '&auto=0&hybrid=2,2&chunk=1&order=file&hold=13&slowlanes=150&steal=0&rebalance=0' },
      { label: 'now: an idle lane takes over', query: FQ + '&auto=0&hybrid=2,2&chunk=1&order=file&hold=13&slowlanes=150&rebalance=0' },
    ],
    setup: (d) => d.open('flash.mp4'),
  },
  {
    name: 'plainer-look',
    alt: "The look: keys that light up when they are on, grey around the picture, colour kept for what it means. A failing section's verdict is lit red; after a Suggest, green. Sound off lights up as sound on.",
    query: Q,
    async setup(d) {
      await inSection(d);
      await d.eval(() => (document.querySelector('#stage').scrollTop = 0));
    },
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.point('#btnScan', { ms: 700 });
      await d.wait(400);
      await d.point('#btnAlerts', { ms: 700 });
      await d.wait(400);
      await d.camera('#playerBox', { anchor: true, ms: 900 });
      await d.click('#btnPreviewSound', { after: 900 });
      await d.scrollTo('.ws-head');
      await d.point('#wsVerdict', { ms: 600 });
      await d.wait(900);
      await d.job(() => d.click('#btnSuggestFewest'));
      await d.verdict(/^passes/);
      await hideToast(d);
      await d.point('#wsVerdict', { ms: 400 });
      await d.wait(600);
    },
  },
  {
    name: 'scan-twice',
    alt: 'A scan under way. Scan is clicked again: a note says a scan is under way, and the scan goes on to the end and marks the flashing on the timeline.',
    query: QA,
    setup: (d) => d.open('flash-minute.mp4'),
    view: (d) => d.around('#btnScan'),
    async play(d) {
      await d.click('#btnScan', { after: 250 });
      await d.click('#btnScan', { ms: 0, after: 300 });
      await d.camera('#toast', { ms: 900 });
      await d.wait(2600);
      await d.camera('#btnScan', { ms: 900 });
      await d.idle();
    },
  },
  {
    name: 'responsive',
    browser: 'firefox',
    alt: 'In Firefox, during a scan: the guide opens and closes, and the chart answers the pointer, while the scan goes on.',
    query: FQ + '&auto=0',
    setup: (d) => d.open('flash-minute.mp4'),
    view: { x: 0, y: 0, width: 1024, height: 640 },
    async play(d) {
      await d.click('#btnScan', { after: 1200 });
      await d.click('#btnHome', { after: 1400 });
      await d.press('Escape', { after: 700 });
      const c = await d.centre('#chart', 0.2, 0.6);
      await d.moveTo(c.x, c.y, 500);
      for (let k = 1; k <= 12; k++) {
        await d.moveTo(c.x + k * 22, c.y - (k % 3) * 6, 180);
        await d.wait(120);
      }
      await d.idle();
    },
  },
  {
    name: 'memory',
    alt: "A ten-minute film scanned in a browser without WebGPU, the tab's memory measured by the browser as it goes: it stays the same from start to end.",
    query: QA,
    setup: (d) => d.open('flash-10min.mp4'),
    view: (d) => d.around('#btnScan'),
    async play(d) {
      // the tab's memory, as the browser measures it, once a second
      await d.eval(() => {
        const show = async () => {
          try {
            const m = await performance.measureUserAgentSpecificMemory();
            window.__demo.caption(`tab memory: ${Math.round(m.bytes / 1048576)} MB (measured by the browser)`);
          } catch (e) {
            /* not measurable */
          }
        };
        show();
        window.__memory = setInterval(show, 1000);
      });
      await d.wait(1500);
      await d.click('#btnScan');
      await d.faster(() => d.idle(), 10);
      await d.wait(2500);
      await d.eval(() => clearInterval(window.__memory));
    },
  },
  {
    name: 'tour',
    alt: 'A first visit: the tour starts on the start page and walks through it, step by step, a light on each part with a card beside it.',
    query: 'tour=1',
    async setup(d) {
      await d.until(() => !!document.querySelector('.tour-card'), null, 30000);
      // (the light glides to its first part)
      await d.wait(600);
    },
    view: (d) => d.around('.tour-card', 110),
    async play(d) {
      for (let k = 0; k < 4; k++) {
        await d.wait(1700);
        await d.click('.tour-next', { after: 350 });
        await d.camera(['.tour-card', '.tour-spot'], { pad: 30, ms: 700 });
      }
      await d.wait(1500);
    },
  },
  {
    name: 'red-wcag22',
    alt: 'A red flash (saturated red against grey) found by a scan, judged as WCAG 2.2 has it; its section lists the red flash and marks its frames in magenta.',
    query: QA,
    setup: (d) => d.open('redflash.mp4'),
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.click('#btnScan');
      await d.idle();
      await d.wait(700);
      await d.click('#sectionList .sec-item', { after: 300 });
      await d.verdict();
      await toTop(d, '.ws-head');
      await d.camera('.ws-head', { anchor: true });
      await d.wait(2200);
    },
  },
  {
    name: 'held-silence',
    alt: 'A frame is marked E (held a second); the export dialog then says the sound is re-encoded to put silence under the held frame, where the picture waits.',
    query: Q,
    async setup(d) {
      await fixedSection(d);
      await toTop(d, '.grid-head');
    },
    view: (d) => d.from('.grid-head'),
    async play(d) {
      await d.click('#frameGrid .frame[data-i="20"]', { after: 500 });
      await d.press('E', { after: 1400 });
      await d.click('#btnExport', { after: 500 });
      await d.camera('#exportModal .modal-box', { pad: 6, ms: 800 });
      await d.point('#exportPlan', { dx: 0.8 });
      await d.wait(2600);
    },
  },
  {
    name: 'section-sound',
    sound: true,
    alt: "Sound off, under the section player, is clicked and lights up as sound on; the fixed section plays with its sound (a steady tone), silent for the second a held frame waits. The film has its sound: turn it on with the film's own sound button.",
    query: Q,
    async setup(d) {
      // (a clip whose sound this browser decodes: Opus)
      await playable(d, 'flash.webm');
      // a frame before the flashing, held a second (E)
      await d.page.click('#frameGrid .frame:nth-child(13)');
      await d.page.click('#wsTitle');
      await d.page.keyboard.press('e');
      await d.until(() => (window.__unflash.currentSection().edits[12] || {}).extended);
      // (none selected: ▶ plays from the section's start, not from the held frame)
      await d.page.keyboard.press('Escape');
      await d.verdict(/^passes/);
      await hideToast(d);
    },
    view: (d) => d.around('#playerBox', 8),
    async play(d) {
      await d.click('#btnPreviewSound', { after: 900 });
      await d.recordSound();
      await d.click('#btnPreviewPlay', { after: 4200 });
      await d.click('#btnPreviewStop');
    },
  },
  {
    name: 'whole-video',
    alt: 'From a section, Whole video at the top of the list charts the whole video around the playhead: 10 s, 30 s or all of it, and the pointer on the chart reads out its numbers.',
    query: Q,
    setup: (d) => inSection(d),
    view: { x: 0, y: 160, width: 1024, height: 640 },
    async play(d) {
      await d.click('#sectionList .sec-whole', { after: 900 });
      await d.click('#chartSpan button[data-span="10"]', { after: 900 });
      await d.click('#chartSpan button[data-span="0"]', { after: 700 });
      const c = await d.centre('#chart', 0.35, 0.5);
      await d.moveTo(c.x, c.y, 600);
      for (let k = 1; k <= 8; k++) await d.moveTo(c.x + k * 14, c.y, 160);
      await d.wait(1600);
    },
  },
  {
    name: 'clean-scan',
    alt: 'A video with no flashing is scanned: "No flashing found", under the chart and in the sections list, with how close it came to the limit.',
    query: QA,
    setup: (d) => d.open('steady.mp4'),
    view: { x: 0, y: 0, width: 1024, height: 640 },
    async play(d) {
      await d.click('#btnScan');
      await d.idle();
      await d.camera({ x: 0, y: 150, width: 1024, height: 640 }, { ms: 900 });
      await d.wait(2000);
    },
  },
  {
    name: 'findings',
    alt: 'A section with an extended flash lists it, with its frames and times and how to fix it; "select these frames" selects them, and the frames inside the extended flash have a blue outline.',
    query: Q,
    async setup(d) {
      await inSection(d, 'extended.mp4');
      await toTop(d, '.ws-head');
    },
    view: (d) => d.from('.ws-head'),
    async play(d) {
      await d.point('#wsFindings .finding', { dx: 0.25 });
      await d.wait(1400);
      await d.click('#wsFindings button[data-finding]', { after: 800 });
      await d.camera('#frameGrid .frame.selected', { anchor: true, pad: 40, ms: 900 });
      await d.wait(2400);
    },
  },
  {
    name: 'project-files',
    alt: 'Project…, in the header: the sections, marks and scan are saved to a file, then loaded back from it.',
    query: Q,
    setup: (d) => fixedSection(d),
    view: { x: 560, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.click('#btnProject', { after: 900 });
      const file = await d.download('#btnProjectSave');
      await d.wait(1200);
      if (await d.eval(() => document.querySelector('#projectMenu').classList.contains('hidden'))) await d.click('#btnProject', { after: 600 });
      await d.pickPath('#projectMenu label.filebtn', file);
      await d.until(() => /^Loaded /.test(document.querySelector('#toast').textContent));
      await d.camera('#toast', { ms: 800 });
      await d.wait(2200);
    },
  },
  {
    name: 'h264-faster',
    alt: "A 1080p H.264 film scanned with Unflash's own H.264 decoder, in several workers; the debug report shows the time it took per picture.",
    query: QA + '&builtin=1',
    setup: (d) => d.open('flash-1080p.mp4'),
    view: (d) => d.around('#btnScan'),
    async play(d) {
      await d.click('#btnScan');
      await d.faster(() => d.idle(), 4);
      await report(d, /^decoding: .*/);
      await d.wait(2200);
      await report(d, /^\s*sw\.decode.*/);
      await d.wait(1200);
    },
  },
  {
    name: 'hevc',
    alt: 'An HEVC film opens in a browser that cannot decode HEVC: Unflash says it uses its own HEVC decoder, and the scan finds the flashing.',
    query: QA,
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.pick('label.filebtn.primary', 'flash_hevc.mp4');
      await d.opened('flash_hevc.mp4');
      await d.point('#bannerText', { dx: 0.3 });
      await d.wait(2400);
      await d.click('#btnScan');
      await d.idle();
    },
  },
  {
    name: 'exact-chunks',
    alt: "The same minute of video scanned twice, in one go above and in pieces below: the pieces are taken in the film's order, and both end with exactly the same sections.",
    ...RACE,
    race: [
      { label: 'in one go', query: QA + '&chunked=0' },
      { label: 'in pieces', query: QA + '&chunk=5&order=file' },
    ],
    setup: (d) => d.open('flash-minute.mp4'),
  },
  {
    name: 'likeliest-first',
    alt: "A scan of a film whose flashing is near its end: before the scan gets there, the places the file's index suggests are looked at first, and what they hold is marked on the timeline, dashed.",
    query: QA + '&chunk=5',
    setup: (d) => d.open('late-flash.mp4'),
    view: (d) => d.around(['#btnScan', '#timelineWrap'], 4),
    async play(d) {
      await d.click('#btnScan');
      await d.idle();
    },
  },
  {
    name: 'shrink-faster',
    browser: 'firefox',
    alt: 'In Firefox, a 1080p film scanned; the debug report shows the pictures made small in the workers, a few milliseconds each.',
    query: FQ + '&auto=0',
    setup: (d) => d.open('flash-1080p.mp4'),
    view: (d) => d.around('#btnScan'),
    async play(d) {
      await d.click('#btnScan');
      await d.faster(() => d.idle(), 4);
      await report(d, /^\s*worker\.shrink.*/);
      await d.wait(1200);
    },
  },
  {
    name: 'h264-exact',
    browser: 'firefox',
    alt: "The same H.264 film scanned in Firefox twice: above with Firefox's decoder, below with Unflash's own; both find exactly the same.",
    ...RACE,
    race: [
      { label: "Firefox's decoder", query: FQ + '&auto=0&hybrid=0' },
      { label: "Unflash's decoder", query: FQ + '&auto=0&hybrid=0&builtin=1' },
    ],
    setup: (d) => d.open('flash_h264.mp4'),
  },
  {
    name: 'two-decoders',
    browser: 'firefox',
    alt: "In Firefox, a two-minute H.264 film is scanned by two decoders at once, Firefox's and Unflash's; the debug report shows what each did.",
    query: FQ + '&auto=0&hybrid=2,2',
    setup: (d) => d.open('flash-long-h264.mp4'),
    view: (d) => d.around('#btnScan'),
    async play(d) {
      await d.click('#btnScan');
      await d.faster(() => d.idle(), 6);
      await report(d, /built-in.*chunks.*/);
      await d.wait(1200);
    },
  },
  {
    name: 'gpu-batches',
    browser: 'firefox',
    alt: 'In Firefox, a scan on the GPU; the debug report shows the batches of frames sent to the GPU and how long it took to answer.',
    query: FQ + '&auto=0',
    setup: (d) => d.open('flash-minute.mp4'),
    view: (d) => d.around('#btnScan'),
    async play(d) {
      await d.click('#btnScan');
      await d.idle();
      await report(d, /^\s*gpu\.latency.*/);
      await d.wait(1200);
    },
  },
  {
    name: 'shrink-in-workers',
    browser: 'firefox',
    alt: 'The same 1080p scan in Firefox twice: above, every picture reaches the page at full size; below, the workers make it small first, and the scan ends sooner.',
    ...RACE,
    race: [
      { label: 'before: full-size pictures', query: FQ + '&auto=0&shrink=0' },
      { label: 'now: made small in the workers', query: FQ + '&auto=0' },
    ],
    setup: (d) => d.open('flash-1080p.mp4'),
  },
  {
    name: 'no-freeze',
    alt: 'Suggest: fewest removals on a long stretch of flashing: some frames are let back in at a rate that cannot flash, so the picture does not freeze, and the section passes.',
    query: Q,
    async setup(d) {
      await inSection(d, 'extended.mp4');
      await toTop(d, '.toolbar');
    },
    view: (d) => d.from('.toolbar'),
    async play(d) {
      await d.job(() => d.click('#btnSuggestFewest'));
      await d.verdict(/^passes/);
      await d.eval(() => document.querySelector('#frameGrid .frame[data-i="36"]').scrollIntoView({ block: 'start' }));
      await d.camera('#frameGrid .frame[data-i="36"]', { anchor: true, pad: 20, ms: 700 });
      await d.wait(2600);
    },
  },
  {
    name: 'debug-info',
    alt: 'Debug info, bottom right, opens a plain-text report of the browser, its GPU, the video and how long each job took, ready to copy into a message.',
    query: Q,
    setup: (d) => d.open('flash.mp4'),
    view: { x: 560, y: 350, width: 720, height: 450 },
    async play(d) {
      await d.click('#btnDebug', { after: 500 });
      await d.camera('#debugModal .modal-box', { pad: 4, ms: 900 });
      await d.wait(1200);
      await d.scroll('#debugText', 220, 1200);
      await d.wait(900);
      await d.click('#btnDebugCopy', { after: 300 });
      await d.camera(['#debugModal .modal-box', '#toast'], { pad: 4, ms: 700 });
      await d.wait(1800);
    },
  },
  {
    name: 'blend-marks',
    alt: 'Frames selected and marked B: blended with the frames either side instead of removed. The blend slider appears and sets how far; the check follows it.',
    query: Q,
    async setup(d) {
      await inSection(d);
      await toTop(d, '#frameGrid .frame[data-i="36"]');
    },
    view: (d) => d.from('#frameGrid .frame[data-i="36"]', 20),
    async play(d) {
      await d.click('#frameGrid .frame[data-i="43"]', { after: 300 });
      await d.click('#frameGrid .frame[data-i="53"]', { modifiers: ['Shift'], after: 500 });
      await d.press('B', { after: 1400 });
      await toTop(d, '.ws-head');
      await d.camera('.ws-head', { anchor: true, ms: 700 });
      await d.wait(900);
      const s = await d.centre('#blendStrength', 0.75, 0.5);
      await d.moveTo(s.x, s.y, 500);
      await d.page.mouse.down();
      for (let k = 1; k <= 10; k++) await d.moveTo(s.x - k * 6, s.y, 60);
      await d.page.mouse.up();
      await d.wait(1600);
      await d.moveTo(s.x - 60, s.y, 200);
      await d.page.mouse.down();
      for (let k = 1; k <= 14; k++) await d.moveTo(s.x - 60 + k * 7, s.y, 60);
      await d.page.mouse.up();
      await d.verdict();
      await d.wait(1200);
    },
  },

  // ======== 2026-09-22 ==========================================================
  {
    name: 'timeline-redraw',
    alt: 'A ten-minute film scanned: the timeline shows what the scan has found so far all the way through, without holding it up.',
    query: QA,
    setup: (d) => d.open('flash-10min.mp4'),
    view: (d) => d.around(['#btnScan', '#timelineWrap'], 4),
    async play(d) {
      await d.click('#btnScan');
      await d.faster(() => d.idle(), 8);
    },
  },
  {
    name: 'thumbs-zoom',
    alt: 'The thumbnails in four sizes, S to XL; Z shows the selected frame at full size, and the arrow keys step through the frames, a new picture at most every 0.4 s.',
    query: Q,
    async setup(d) {
      await inSection(d);
      await toTop(d, '.grid-head');
    },
    view: (d) => d.from('.grid-head'),
    async play(d) {
      for (const size of ['l', 'xl', 'm']) await d.click(`.thumb-size button[data-thumb="${size}"]`, { after: 900 });
      await d.click('#frameGrid .frame[data-i="2"]', { after: 400 });
      await d.press('Z', { after: 300 });
      await d.until(() => !document.querySelector('#frameViewer').classList.contains('hidden'));
      await d.camera('#frameViewer .viewer-box', { pad: 4, ms: 700 });
      await d.wait(900);
      for (let k = 0; k < 4; k++) await d.press('ArrowRight', { after: 600 });
      await d.wait(600);
      await d.press('Escape');
    },
  },
  {
    name: 'fewest',
    alt: 'Suggest: keep dark removes the dark frames of a section; undone, Suggest: fewest removals fixes it by removing fewer.',
    query: Q,
    async setup(d) {
      await inSection(d);
      await toTop(d, '.ws-head');
    },
    view: (d) => d.from('.ws-head'),
    async play(d) {
      await d.job(() => d.click('#btnSuggestDark'));
      await d.verdict();
      await d.wait(1800);
      await d.press('Control+Z', { after: 900 });
      await d.verdict();
      await d.job(() => d.click('#btnSuggestFewest'));
      await d.verdict();
      await d.wait(1200);
    },
  },
  {
    name: 'decode-workers',
    browser: 'firefox',
    alt: 'The same 1080p scan in Firefox twice: above, decoded on the page; below, decoded in the background, several pictures at once, and done sooner.',
    ...RACE,
    // (scans in one go: a chunked scan's lanes decode in workers whatever the setting)
    race: [
      { label: 'before: decoded on the page', query: FQ + '&auto=0&chunked=0&decodeworkers=0' },
      { label: 'now: decoded in the background', query: FQ + '&auto=0&chunked=0' },
    ],
    setup: (d) => d.open('flash-1080p.mp4'),
  },
  {
    name: 'mkv',
    alt: 'An MKV file opens and is scanned like an MP4; an AVI is recognised, and Unflash says how to convert it.',
    query: QA,
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.pick('label.filebtn.primary', 'flash.mkv');
      await d.opened('flash.mkv');
      await d.click('#btnScan');
      await d.idle();
      await d.wait(900);
      await d.pick('label.filebtn.primary', 'clip.avi');
      await d.until(() => !document.querySelector('#banner').classList.contains('hidden'));
      await d.camera('#banner', { pad: 8, ms: 700 });
      await d.wait(2600);
    },
  },
  {
    name: 'alerts',
    alt: 'Alerts, in the header: a beep when a job that ran over a minute finishes, a system notification too if you like, and a button to try the beep.',
    query: Q,
    view: { x: 560, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.click('#btnAlerts', { after: 900 });
      await d.click('#alertBeep', { after: 700 });
      await d.click('#alertBeep', { after: 700 });
      await d.click('#btnAlertTest', { after: 1400 });
    },
  },
  {
    name: 'background-tab',
    alt: "A scan starts, and another tab comes to the front for a while; back on Unflash's tab, the scan has gone on without it.",
    query: QA,
    setup: (d) => d.open('flash-10min.mp4'),
    view: (d) => d.around(['#btnScan', '#timelineWrap'], 4),
    async play(d) {
      await d.click('#btnScan', { after: 1500 });
      await d.caption('another tab in front');
      const other = await d.page.context().newPage();
      await other.bringToFront();
      await d.faster(() => d.wait(12000), 6);
      await d.page.bringToFront();
      await other.close();
      await d.caption('back');
      await d.wait(1500);
      await d.caption('');
      await d.faster(() => d.idle(), 6);
    },
  },
  {
    name: 'prepare-faster',
    alt: 'A section opens and prepares: its frames are decoded in several pieces at once, and it is checked.',
    query: Q,
    setup: (d) => d.open('flash-minute.mp4'),
    view: { x: 0, y: 0, width: 1024, height: 640 },
    async play(d) {
      await d.click('#sectionList .sec-item', { after: 0 });
      await d.verdict();
      await d.wait(900);
    },
  },
  {
    name: 'export-formats',
    alt: 'The export dialog offers one choice per format, and says where each plays and whether the parts not edited are copied as they are.',
    query: Q,
    async setup(d) {
      await fixedSection(d);
      await click(d, '#btnExport');
      await d.until(() => !document.querySelector('#btnDoExport').disabled);
    },
    view: (d) => d.around('#exportModal .modal-box', 6),
    async play(d) {
      for (let k = 0; k < 3; k++) {
        await d.point('#exportCodec', { ms: 500 });
        await d.wait(300);
        await d.eval(() => {
          const s = document.querySelector('#exportCodec');
          s.selectedIndex = (s.selectedIndex + 1) % s.options.length;
          s.dispatchEvent(new Event('change', { bubbles: true }));
        });
        await d.point('#exportFormatNote', { dx: 0.4, ms: 400 });
        await d.wait(1800);
      }
    },
  },
  {
    name: 'guide-beside',
    alt: 'With a video open, Guide opens the guide beside the work instead of over it; Esc puts it away.',
    query: Q,
    viewport: { width: 1152, height: 720 },
    setup: (d) => d.open('flash.mp4'),
    view: { x: 0, y: 0, width: 1152, height: 720 },
    async play(d) {
      await d.click('#btnHome', { after: 2200 });
      await d.press('Escape', { after: 1200 });
    },
  },
  {
    name: 'section-player',
    alt: 'The section player plays a fixed section as the export will write it, looping, at half speed; the line above it says what is on screen and whether it passes.',
    query: Q,
    setup: (d) => playable(d),
    view: (d) => d.around('#playerBox', 8),
    async play(d) {
      await d.click('#previewLoop', { after: 500 });
      await d.point('#previewSpeed', { ms: 400 });
      await d.eval(() => {
        const s = document.querySelector('#previewSpeed');
        s.value = '0.5';
        s.dispatchEvent(new Event('change', { bubbles: true }));
      });
      await d.wait(500);
      await d.click('#btnPreviewPlay', { after: 4200 });
      await d.click('#btnPreviewStop', { after: 300 });
    },
  },
  {
    name: 'keys',
    alt: 'Marking frames with the keys, as in the original tool: R removes, R again takes it off, F, E and K mark, U clears; Ctrl+Z undoes and Ctrl+Shift+Z redoes.',
    query: Q,
    async setup(d) {
      await inSection(d);
      await d.eval(() => document.querySelector('#frameGrid .frame[data-i="36"]').scrollIntoView({ block: 'start' }));
    },
    view: (d) => d.from('#frameGrid .frame[data-i="36"]', 20),
    async play(d) {
      await d.click('#frameGrid .frame[data-i="44"]', { after: 200 });
      await d.click('#frameGrid .frame[data-i="46"]', { modifiers: ['Shift'], after: 500 });
      for (const k of ['R', 'R', 'F', 'E', 'K', 'U']) await d.press(k, { after: 900 });
      await d.press('R', { after: 700 });
      await d.press('Control+Z', { after: 900 });
      await d.press('Control+Shift+Z', { after: 900 });
    },
  },
  {
    name: 'auto-fix-off',
    alt: 'Auto-fix, in the header, is off. A video is opened: its scan starts by itself; a section clicked prepares itself; nothing is fixed until asked.',
    query: Q,
    view: { x: 0, y: 0, width: 1024, height: 640 },
    async play(d) {
      await d.point('#autoToggle', { ms: 600 });
      await d.wait(1200);
      await d.pick('label.filebtn.primary', 'flash.mp4');
      await d.opened('flash.mp4');
      await d.click('#sectionList .sec-item');
      await d.verdict();
    },
  },
  {
    name: 'smart-cut',
    alt: 'The export dialog says only the stretches around the sections are decoded and re-encoded, the rest copied as it is; the export runs and is checked.',
    query: Q,
    async setup(d) {
      await fixedSection(d, 'flash-minute.mp4');
      await click(d, '#btnExport');
      await d.until(() => !document.querySelector('#btnDoExport').disabled);
    },
    view: (d) => d.around('#exportModal .modal-box', 6),
    async play(d) {
      await d.point('#exportPlan', { dx: 0.3, ms: 600 });
      await d.wait(2400);
      await d.job(() => d.click('#btnDoExport'));
      await d.job(() => d.click('#btnVerifyExport'));
      await d.camera('#exportModal .modal-box', { pad: 6, ms: 500 });
    },
  },
  {
    name: 'scan-parts',
    browser: 'firefox',
    alt: 'In Firefox, a one-minute film scanned on the GPU; the debug report shows it cut into chunks and decoded by two lanes at once, with what each lane did.',
    query: FQ + '&auto=0',
    setup: (d) => d.open('flash-minute.mp4'),
    view: (d) => d.around('#btnScan'),
    async play(d) {
      await d.click('#btnScan');
      await d.idle();
      await report(d, /^\s*chunked: .*\n.*\n.*/);
      await d.wait(1600);
    },
  },

  // ======== 2026-09-21 ==========================================================
  {
    name: 'live-monitor',
    alt: 'The live monitor, over the player, reads the finished scan: clicking the timeline at the flashing shows how much of the picture flashes there.',
    query: Q,
    async setup(d) {
      await d.open('flash.mp4');
      await click(d, '#liveToggle');
    },
    view: { x: 0, y: 60, width: 1024, height: 640 },
    async play(d) {
      for (const at of [0.2, 0.45, 0.8]) {
        const c = await d.centre('#timeline', at, 0.4);
        await d.moveTo(c.x, c.y, 500);
        await d.wait(150);
        await d.page.mouse.click(c.x, c.y);
        await d.wait(1800);
      }
    },
  },
  {
    name: 'read-once',
    alt: 'A video opened and scanned straight away; the debug report shows how little time opening it took.',
    query: Q,
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.pick('label.filebtn.primary', 'flash-minute.mp4');
      await d.opened('flash-minute.mp4');
      await report(d, /^Jobs .*/);
      await d.wait(1200);
    },
  },

  // ======== 2026-09-20 ==========================================================
  {
    name: 'drop',
    alt: 'A video file dragged onto the page and dropped: it opens, and its scan starts.',
    query: Q,
    view: { x: 0, y: 0, width: 1024, height: 640 },
    async play(d) {
      await d.eval(async () => {
        const blob = await (await fetch('clips/flash.mp4')).blob();
        window.__dropped = new File([blob], 'flash.mp4', { type: 'video/mp4' });
      });
      await d.moveTo(1010, 600, 10);
      await d.carry('flash.mp4');
      await d.moveTo(560, 420, 1200);
      await d.eval(([x, y]) => {
        const dt = new DataTransfer();
        dt.items.add(window.__dropped);
        const t = document.elementFromPoint(x, y);
        for (const type of ['dragenter', 'dragover', 'drop']) t.dispatchEvent(new DragEvent(type, { bubbles: true, cancelable: true, clientX: x, clientY: y, dataTransfer: dt }));
      }, [560, 420]);
      await d.carry('');
      await d.opened('flash.mp4');
      await d.wait(600);
    },
  },
  {
    name: 'auto-fix',
    alt: 'Auto-fix ticked, a video opened: it is scanned, every section fixed, the result exported and checked, with no clicks; then it can be downloaded.',
    query: Q,
    view: { x: 0, y: 0, width: 1024, height: 640 },
    async play(d) {
      await d.click('#autoToggle', { after: 700 });
      await d.pick('label.filebtn.primary', 'flash.mp4');
      await d.until(() => !document.querySelector('#auto').classList.contains('hidden'));
      await d.faster(() => d.until(() => !document.querySelector('#autoDownload').classList.contains('hidden'), null, 300000), 3);
      await d.point('#autoDownload', { ms: 600 });
      await d.wait(1200);
    },
  },
  {
    name: 'red-same-brightness',
    alt: 'Saturated red swapped with a grey just as bright, five times a second: no brightness flash at all, but the scan finds a red flash, and its frames are marked in magenta.',
    query: Q,
    async setup(d) {
      await inSection(d, 'redflash.mp4');
      await toTop(d, '.grid-head');
    },
    view: (d) => d.from('.grid-head'),
    async play(d) {
      await d.point('.legend .chip.flagged-red', { ms: 700 });
      await d.wait(1400);
      await d.scroll('#stage', 240, 1400);
      await d.wait(1800);
    },
  },
  {
    name: 'yuv-planes',
    alt: "A scan on the GPU whose pictures go to it as the video's own colour planes: the debug report says \"yuv (I420…)\". Filmed in Chromium, made to take that route, as the Firefox here decodes to BGRX and has no planes to send.",
    // (on the GPU: SwiftShader's, slow, so a short clip)
    query: 'tour=0&auto=0&chunked=0&route=yuv',
    setup: (d) => d.open('flash.mp4'),
    view: (d) => d.around('#btnScan'),
    async play(d) {
      await d.click('#btnScan');
      await d.faster(() => d.idle(), 4);
      await report(d, /^\s*pictures reach it as: yuv.*/);
      await d.wait(1600);
    },
  },
  {
    name: 'firefox-works',
    browser: 'firefox',
    alt: 'In Firefox: Scan is clicked, and the scan runs to the end on the GPU, the flashing marked on the timeline as it is found.',
    query: FQ + '&auto=0',
    setup: (d) => d.open('flash-minute.mp4'),
    view: (d) => d.around('#btnScan'),
    async play(d) {
      await d.click('#btnScan');
      await d.idle();
    },
  },
  {
    name: 'h264-builtin',
    alt: 'An H.264 film opens in a browser without an H.264 decoder of its own: Unflash says it decodes it itself, and the scan finds the flashing; an interlaced one opens too.',
    query: QA,
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.pick('label.filebtn.primary', 'flash_h264.mp4');
      await d.opened('flash_h264.mp4');
      await d.point('#bannerText', { dx: 0.3 });
      await d.wait(2000);
      await d.click('#btnScan');
      await d.idle();
      await d.wait(700);
      await d.pick('label.filebtn.primary', 'flash_h264i.mp4');
      await d.opened('flash_h264i.mp4');
      await d.click('#btnScan');
      await d.idle();
    },
  },
  {
    name: 'stripes',
    alt: 'The stripes test clip opened from the start page: the scan finds a stripe pattern; in its section, soften stripes blurs them just enough, and the section passes.',
    query: Q,
    view: { x: 0, y: 330, width: 720, height: 450 },
    async play(d) {
      await d.click('button[data-clip="stripes.mp4"]');
      await d.opened('stripes.mp4');
      await d.camera({ x: 0, y: 0, width: 1024, height: 640 }, { ms: 800 });
      await d.click('#sectionList .sec-item');
      await d.verdict();
      await hideToast(d);
      await toTop(d, '.ws-head');
      await d.camera('.ws-head', { anchor: true, ms: 800 });
      await d.click('#softenToggle');
      await d.verdict(/^passes/);
      await d.wait(600);
      await d.wait(1200);
    },
  },

  // ======== 2026-09-19 ==========================================================
  {
    name: 'web-version',
    alt: 'Unflash in the browser, start to end: a video opened and scanned, its section fixed with a Suggest button, the result exported and checked.',
    query: Q,
    view: { x: 0, y: 0, width: 1024, height: 640 },
    async play(d) {
      await d.pick('label.filebtn.primary', 'flash.mp4');
      await d.opened('flash.mp4');
      await d.click('#sectionList .sec-item');
      await d.verdict();
      await hideToast(d);
      await d.job(() => d.click('#btnSuggestFewest'));
      await d.verdict(/^passes/);
      await d.click('#btnExport', { after: 400 });
      await d.camera('#exportModal .modal-box', { pad: 6, ms: 700 });
      await d.job(() => d.click('#btnDoExport'), 3);
      await d.job(() => d.click('#btnVerifyExport'), 3);
      await d.camera('#exportModal .modal-box', { pad: 6, ms: 400 });
    },
  },

  // ======== 2026-09-24 (later) ==================================================
  {
    name: 'job-steps',
    browser: 'firefox',
    alt: 'In Firefox, a six-hour MKV opens (it keeps no index, so the file is read through); the debug report says how long the opening took and where the time went.',
    query: FQ + '&auto=0',
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.pick('label.filebtn.primary', 'six-hours.mkv');
      await d.opened('six-hours.mkv');
      await report(d, /^\s+reading through the file.*/);
      await d.wait(1800);
    },
  },

  {
    name: 'rebalance',
    browser: 'firefox',
    alt: "The same 1080p H.264 scan in Firefox twice, Firefox's lanes as slow as on a Windows PC: above, the chunks handed out in turn, and taken over when a slow lane holds one up; below, each chunk to the lane that would finish it first, the slow lanes set aside and Unflash's decoder grown into their cores, and the scan ends sooner.",
    ...RACE,
    race: [
      { label: 'before: in turn, then taken over', query: FQ + '&auto=0&hybrid=2,2&chunk=1&order=file&hold=13&slowlanes=150&rebalance=0' },
      { label: 'now: to whichever finishes first', query: FQ + '&auto=0&hybrid=2,2&chunk=1&order=file&hold=13&slowlanes=150' },
    ],
    // (1080p H.264: decoding is the slow part here, as on the PC; at 640×360 both wait on the detector)
    setup: (d) => d.open('flash-1080p.mp4'),
  },

  // ======== 2026-09-29 ==========================================================
  {
    name: 'sound-says',
    alt: "The section player's sound is turned on: at ½× its button says sound at 1× only; back at 1×, ▶ plays the section, and in this browser, which can't decode AAC, the button says no sound, and a note says which sound it couldn't decode.",
    query: Q,
    // (an MP4 with AAC sound, which the test browser can't decode)
    setup: (d) => playable(d),
    view: (d) => d.around('#playerBox', 8),
    async play(d) {
      await d.click('#btnPreviewSound', { after: 900 });
      await d.point('#previewSpeed', { ms: 500 });
      await d.eval(() => {
        const s = document.querySelector('#previewSpeed');
        s.value = '0.5';
        s.dispatchEvent(new Event('change'));
      });
      await d.wait(1600);
      await d.eval(() => {
        const s = document.querySelector('#previewSpeed');
        s.value = '1';
        s.dispatchEvent(new Event('change'));
      });
      await d.wait(900);
      await d.click('#btnPreviewPlay');
      await d.until(() => document.querySelector('#btnPreviewSound').textContent === 'no sound' && !document.querySelector('#toast').classList.contains('hidden'), null, 30000);
      // the note that says which sound it is, beside the button
      await d.camera(await d.around(['#btnPreviewSound', '#toast'], 12), { ms: 800 });
      await d.wait(3200);
    },
  },

  {
    name: 'save-as',
    browser: 'firefox',
    alt: "In Firefox, the export dialog: Save as has the video's name with .unflashed, and a new name is typed in its place; under it, how to have Firefox ask which folder to save in (its setting Always ask you where to save files). Export writes the video, and Download offers it under the new name.",
    query: FQ,
    async setup(d) {
      await fixedSection(d);
      await click(d, '#btnExport');
      await d.until(() => !document.querySelector('#btnDoExport').disabled && document.querySelector('#exportName').value);
    },
    view: (d) => d.around('#exportModal .modal-box', 4),
    async play(d) {
      await d.point('#exportName', { ms: 700 });
      await d.wait(900);
      await d.type('#exportName', 'my video, safe to watch');
      await d.press('Tab', { after: 1800 });
      await d.job(() => d.click('#btnDoExport'));
      await d.until(() => !document.querySelector('#exportDownload').classList.contains('hidden') && document.querySelector('#exportDownload').download === 'my video, safe to watch.mp4');
      await d.point('#exportDownload', { ms: 700 });
      await d.wait(2000);
    },
  },

  {
    name: 'dolby-sound',
    sound: true,
    alt: "A video whose sound is E-AC-3 (Dolby Digital Plus), which this browser can't decode: sound off is clicked, and the section plays with its sound (a steady tone), decoded by Unflash itself, as the debug report then says. The film has its sound: turn it on with the film's own sound button.",
    query: Q,
    setup: (d) => playable(d, 'flash_eac3.mp4'),
    view: (d) => d.around('#playerBox', 8),
    async play(d) {
      await d.click('#btnPreviewSound', { after: 900 });
      await d.recordSound();
      await d.click('#btnPreviewPlay', { after: 3600 });
      await d.click('#btnPreviewStop', { after: 600 });
      await report(d, /^.*section player's sound: on, decoded by the built-in AC-3 decoder.*/);
      await d.wait(2000);
    },
  },

  // ======== 2026-09-30 ==========================================================
  {
    name: 'frame-times',
    alt: 'A section 50 seconds into a video: the verdict and the list under it say when the flash is in the whole video and how far into the section; then each frame in the grid shows its time in the whole video at the top right, and how far into the section it is at the bottom right.',
    query: Q,
    viewport: { width: 960, height: 700 },
    async setup(d) {
      await inSection(d, 'late-flash.mp4');
      await toTop(d, '#wsTitle');
    },
    view: (d) => d.from('#wsVerdict', 16),
    async play(d) {
      await d.point('#wsVerdict', { dx: 0.2, ms: 600 });
      await d.wait(2400);
      await d.point('#wsFindings .finding b', { ms: 600 });
      await d.wait(2600);
      await d.scrollTo('#frameGrid', { ms: 900 });
      await d.camera('#frameGrid .frame:nth-child(1)', { anchor: true, pad: 12, ms: 700 });
      await d.point('#frameGrid .frame:nth-child(2) .fv', { ms: 700 });
      await d.wait(2000);
      await d.point('#frameGrid .frame:nth-child(2) .ft', { ms: 700 });
      await d.wait(2000);
    },
  },

  {
    name: 'verify-sections',
    alt: 'An export whose check still finds flashing: Verify says which section each problem is in, and a click on section #1 opens it with the frames that were on screen during the flash selected.',
    query: Q,
    async setup(d) {
      await inSection(d);
      await click(d, '#btnExport');
      await d.until(() => !document.querySelector('#btnDoExport').disabled);
      await d.job(() => click(d, '#btnDoExport'));
      await d.until(() => !document.querySelector('#btnVerifyExport').disabled);
    },
    view: (d) => d.around('#exportModal .modal-box', 4),
    async play(d) {
      await d.job(() => d.click('#btnVerifyExport'), 4);
      await d.until(() => /re-scanned/.test(document.querySelector('#exportResult').textContent));
      await d.camera('#exportModal .modal-box', { pad: 6, ms: 500 });
      await d.point('#exportResult button[data-open-at]', { ms: 700 });
      await d.wait(2400);
      await d.click('#exportResult button[data-open-at]', { after: 1200 });
      await d.camera('#frameGrid .frame.selected', { anchor: true, pad: 40, ms: 900 });
      await d.wait(3000);
    },
  },

  {
    name: 'fewest-verdict',
    alt: 'A section of bursts of quick flashes, each of which fails by itself: Suggest: fewest removals takes every burst out, and says the section passes, which the verdict now says too (it showed the check of the last burst tried back in, which fails).',
    query: Q,
    setup: (d) => inSection(d, 'bursts.mp4'),
    view: (d) => d.around(['#wsVerdict', '#btnSuggestFewest'], 10),
    async play(d) {
      await d.point('#wsVerdict', { ms: 600 });
      await d.wait(1200);
      await d.job(() => d.click('#btnSuggestFewest'));
      await d.verdict(/^passes/);
      await d.camera(await d.around(['#wsVerdict', '#btnSuggestFewest', '#toast'], 10), { ms: 800 });
      await d.wait(3600);
    },
  },

  {
    name: 'no-adapter',
    alt: "A browser whose WebGPU gives the page no graphics adapter (hardware acceleration off, say): a video is opened, and the message says in words that the scans run on the CPU, with the same results, and what to look at in Chrome: Settings, System, Use graphics acceleration when available, and chrome://gpu. The debug report keeps the browser's own error.",
    // (the browser as it is with graphics acceleration off: WebGPU there, no adapter)
    init: `if (navigator.gpu) navigator.gpu.requestAdapter = async () => null;`,
    query: 'tour=0&auto=0',
    viewport: { width: 960, height: 700 },
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.pick('label.filebtn.primary', 'flash.mp4');
      await d.until(() => /graphics adapter/.test(document.querySelector('#bannerText').textContent));
      await d.camera('#banner', { pad: 8, ms: 900 });
      await d.wait(4200);
      await report(d, /^.*CPU \(WebAssembly\) at.*/);
      await d.wait(2400);
    },
  },

  {
    name: 'quality-scale',
    browser: 'firefox',
    alt: "In Firefox, the export dialog's quality says 7 of 10; it is moved to 10 of 10 and the page reloaded; with the video open again, the dialog says 7 of 10, with the slider at 7 (Firefox used to put the slider back at 10 and leave the number at 7).",
    query: FQ,
    async setup(d) {
      await d.open('flash.mp4');
      await hideToast(d);
      await click(d, '#btnExport');
      await d.until(() => !document.querySelector('#btnDoExport').disabled);
    },
    view: (d) => d.around('#exportModal .modal-box', 4),
    async play(d) {
      await d.point('#exportQuality', { dx: 0.67, ms: 700 });
      await d.wait(1400);
      // (dragged to the top)
      await d.point('#exportQuality', { dx: 0.98, ms: 700 });
      await d.eval(() => {
        const q = document.querySelector('#exportQuality');
        q.value = '10';
        q.dispatchEvent(new Event('input', { bubbles: true }));
        q.dispatchEvent(new Event('change', { bubbles: true }));
      });
      await d.wait(1800);
      await d.reload();
      await d.caption('the page, reloaded');
      await d.wait(900);
      await d.pick('label.filebtn.primary', 'flash.mp4');
      await d.caption('');
      await d.opened('flash.mp4');
      await hideToast(d);
      await d.click('#btnExport', { after: 400 });
      await d.until(() => !document.querySelector('#btnDoExport').disabled);
      await d.camera('#exportModal .modal-box', { pad: 6, ms: 700 });
      await d.point('#exportQuality', { dx: 0.67, ms: 700 });
      await d.wait(2600);
    },
  },

  {
    name: 'new-version',
    alt: "A page open while a newer version of Unflash is published: it says so, once, and new version: reload stays in the header until the page is reloaded; until then the page carries on as it is, on its own version's files.",
    // (acting as an earlier build; versions.json, as the site has it once a newer one is out, is given in the film)
    query: Q + '&build=earlier',
    viewport: { width: 960, height: 700 },
    setup: (d) => inSection(d),
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.wait(1200);
      await d.caption('meanwhile, a newer version is published');
      await d.page.route('**/versions.json', (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify({ current: 'later', builds: [] }) }));
      await d.wait(1600);
      await d.eval(() => window.__unflash.checkForUpdate(true));
      await d.until(() => !document.querySelector('#btnUpdate').classList.contains('hidden'));
      await d.caption('');
      await d.camera(await d.around(['#banner', '#btnUpdate'], 10), { ms: 800 });
      await d.wait(3000);
      await d.point('#btnUpdate', { ms: 700 });
      await d.wait(2400);
    },
  },

  {
    name: 'transport-streams',
    alt: "A Blu-ray .m2ts file opens: Unflash reads it through (a transport stream keeps no index), and the line under the player names its H.264 and AC-3; the scan finds its two flashes, as in the MP4 it was made from.",
    query: QA,
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.pick('label.filebtn.primary', 'flash_ac3.m2ts');
      await d.opened('flash_ac3.m2ts');
      await d.point('#videoInfo', { dx: 0.7, ms: 700 });
      await d.wait(2200);
      await d.job(() => d.click('#btnScan'));
      await d.wait(600);
      await d.camera(await d.around(['#timelineWrap', '#sectionList'], 8), { ms: 800 });
      await d.wait(2600);
    },
  },
  {
    name: 'dts-sound',
    sound: true,
    alt: "A video whose sound is DTS, which no browser decodes for a web page: sound off is clicked, and the section plays with its sound (a steady tone), decoded by Unflash itself, as the debug report then says. The film has its sound: turn it on with the film's own sound button.",
    query: Q,
    setup: (d) => playable(d, 'flash_dts.mkv'),
    view: (d) => d.around('#playerBox', 8),
    async play(d) {
      await d.click('#btnPreviewSound', { after: 900 });
      await d.recordSound();
      await d.click('#btnPreviewPlay', { after: 3600 });
      await d.click('#btnPreviewStop', { after: 600 });
      await report(d, /^.*section player's sound: on, decoded by the built-in DTS decoder.*/);
      await d.wait(2000);
    },
  },
  {
    name: 'pcm-sound',
    sound: true,
    alt: "A video whose sound is uncompressed (24-bit PCM in a MOV, as cameras record it): sound off is clicked, and the section plays with its sound (a steady tone), read by Unflash itself, as the debug report then says. The film has its sound: turn it on with the film's own sound button.",
    query: Q,
    setup: (d) => playable(d, 'flash_pcm.mov'),
    view: (d) => d.around('#playerBox', 8),
    async play(d) {
      await d.click('#btnPreviewSound', { after: 900 });
      await d.recordSound();
      await d.click('#btnPreviewPlay', { after: 3600 });
      await d.click('#btnPreviewStop', { after: 600 });
      await report(d, /^.*section player's sound: on, decoded by the built-in PCM decoder.*/);
      await d.wait(2000);
    },
  },
  {
    name: 'sound-track',
    alt: 'A video with two sound tracks, the first in a form no browser plays (ALAC): Unflash takes the second, and the line under the player says so (sound 2 of 2); the export keeps that track, and says why.',
    query: QA,
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.pick('label.filebtn.primary', 'flash_2audio.mkv');
      await d.opened('flash_2audio.mkv');
      await d.point('#videoInfo', { dx: 0.85, ms: 700 });
      await d.wait(2600);
      await d.click('#btnExport');
      await d.until(() => !document.querySelector('#btnDoExport').disabled);
      await d.job(() => d.click('#btnDoExport'), 4);
      await d.until(() => !document.querySelector('#btnVerifyExport').disabled);
      await d.camera('#exportModal .modal-box', { pad: 6, ms: 600 });
      await d.wait(3200);
    },
  },

  // ======== What's new itself, films and all: filmed last ======================
  {
    name: 'films',
    alt: "What's new, a film beside each change: the one in view plays by itself, muted; a click pauses it, and another plays it again.",
    query: 'tour=0',
    view: { x: 0, y: 0, width: 720, height: 450 },
    async play(d) {
      await d.click('#btnChanges', { after: 500 });
      await d.camera('#changesModal .modal-box', { pad: 4, ms: 800 });
      await d.wait(2600);
      await d.click('#changesList .shot-film', { dx: 0.5, dy: 0.45, after: 1600 });
      await d.click('#changesList .shot-film', { dx: 0.5, dy: 0.45, after: 1800 });
      await d.scroll('#changesList', 620, 1600);
      await d.wait(2400);
    },
  },
  {
    name: 'whats-new',
    alt: "Someone back after an update: the changes since their visit wait on the start page, and What's new, in the header, has them all, the new ones marked, each with its film.",
    init: `try { localStorage.setItem('unflash:changesSeen', String(Date.UTC(2026, 8, 23, 18, 0))); } catch (e) {}`,
    query: 'tour=0',
    view: { x: 0, y: 40, width: 720, height: 450 },
    async play(d) {
      await d.wait(1800);
      await d.click('#btnChanges', { after: 600 });
      await d.camera('#changesModal .modal-box', { pad: 4, ms: 900 });
      await d.wait(1200);
      await d.scroll('#changesList', 500, 2400);
      await d.wait(2500);
    },
  },
];
