// The guided tour: one part of the page at a time lit up, the rest dimmed,
// and a card beside it saying what that part is for (in the manner of
// intro.js). Steps are `{ target, title, html, placement, label }`:
// `target` a CSS selector (or a function returning an element) of something
// on screen, or nothing for a card in the middle of the page; `placement`
// where the card goes if it fits ('bottom', 'top', 'right' or 'left'; by
// default whichever fits first); `label` the tour's name on the card. Steps
// whose target is not on screen are left out.
//
// Nothing here moves fast or blinks: the light moves from one part to the
// next in a fifth of a second (at once, for anyone who asks their system for
// less motion), and the page underneath does not change.

/** The element a step points at, if it is on screen. */
function resolve(target) {
  if (!target) return null;
  const el = typeof target === 'function' ? target() : document.querySelector(target);
  if (!el || !el.isConnected) return null;
  const r = el.getBoundingClientRect();
  if (r.width < 2 || r.height < 2 || !el.getClientRects().length) return null;
  // (an element inside a hidden one has no box)
  return el;
}

/** Whether a step can be shown now. */
export function stepShowable(step) {
  return !step.target || !!resolve(step.target);
}

const MARGIN = 12;
const GAP = 12;
const PAD = 6;

export class Tour {
  /**
   * `onEnd(how)` is called once, `how` 'done' (the last step's button),
   * 'skipped' (skip, Esc, a click outside the card) or 'empty' (none of the
   * steps was on screen: the tour never showed).
   */
  constructor(steps, { onEnd }) {
    this.steps = steps.filter(stepShowable);
    this.onEnd = onEnd;
    this.i = -1;
    this.el = null;
    this.target = null;
    this.last = null;
    this.raf = 0;
    this.placed = false;
  }

  get open() {
    return !!this.el;
  }

  start() {
    if (!this.steps.length) {
      this.onEnd('empty');
      return false;
    }
    this.returnFocus = document.activeElement;
    this.build();
    this.show(0);
    return true;
  }

  build() {
    const el = document.createElement('div');
    el.className = 'tour';
    el.innerHTML = `
      <div class="tour-shade"></div>
      <div class="tour-spot"></div>
      <div class="tour-card" role="dialog" aria-modal="true" aria-labelledby="tourTitle">
        <div class="tour-arrow"></div>
        <div class="tour-count"></div>
        <h3 id="tourTitle" class="tour-title"></h3>
        <div class="tour-text"></div>
        <div class="tour-actions">
          <button type="button" class="small tour-skip">skip the tour</button>
          <span class="spacer"></span>
          <button type="button" class="small tour-back">back</button>
          <button type="button" class="small primary tour-next">next</button>
        </div>
      </div>`;
    document.body.appendChild(el);
    this.el = el;
    this.shade = el.querySelector('.tour-shade');
    this.spot = el.querySelector('.tour-spot');
    this.card = el.querySelector('.tour-card');
    this.arrow = el.querySelector('.tour-arrow');
    // (a single card has nothing to skip: done closes it)
    el.querySelector('.tour-skip').classList.toggle('hidden', this.steps.length === 1);
    el.querySelector('.tour-skip').addEventListener('click', () => this.end('skipped'));
    el.querySelector('.tour-back').addEventListener('click', () => this.back());
    el.querySelector('.tour-next').addEventListener('click', () => this.next());
    this.shade.addEventListener('click', () => this.end('skipped'));
    this.onKey = (e) => this.key(e);
    this.onResize = () => this.place();
    // (on the window, while capturing: before any of the page's own keys)
    window.addEventListener('keydown', this.onKey, true);
    window.addEventListener('resize', this.onResize);
    const track = () => {
      if (!this.el) return;
      this.follow();
      this.raf = requestAnimationFrame(track);
    };
    this.raf = requestAnimationFrame(track);
  }

  show(i) {
    const step = this.steps[i];
    if (!step) return;
    this.i = i;
    // a target gone since the tour began (a menu closed): the card goes in the middle
    this.target = resolve(step.target);
    const label = step.label;
    this.el.querySelector('.tour-count').textContent = this.steps.length > 1 ? `${label} · ${i + 1} of ${this.steps.length}` : label;
    this.el.querySelector('.tour-title').textContent = step.title || '';
    this.el.querySelector('.tour-text').innerHTML = step.html || '';
    const back = this.el.querySelector('.tour-back');
    back.disabled = i === 0;
    back.classList.toggle('hidden', this.steps.length < 2);
    const next = this.el.querySelector('.tour-next');
    next.textContent = i === this.steps.length - 1 ? 'done' : 'next';
    this.el.classList.toggle('centred', !this.target);
    if (this.target) this.target.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    this.last = null;
    this.place();
    next.focus({ preventScroll: true });
  }

  next() {
    if (this.i >= this.steps.length - 1) return this.end('done');
    this.show(this.i + 1);
  }

  back() {
    if (this.i > 0) this.show(this.i - 1);
  }

  end(how) {
    if (!this.el) return;
    cancelAnimationFrame(this.raf);
    window.removeEventListener('keydown', this.onKey, true);
    window.removeEventListener('resize', this.onResize);
    this.el.remove();
    this.el = null;
    const back = this.returnFocus;
    if (back && back.isConnected && back.focus) back.focus({ preventScroll: true });
    this.onEnd(how);
  }

  key(e) {
    if (!this.el) return;
    const inCard = this.card.contains(e.target);
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      return this.end('skipped');
    }
    if (e.key === 'ArrowRight' || (e.key === 'Enter' && !inCard)) {
      e.preventDefault();
      e.stopPropagation();
      return this.next();
    }
    if (e.key === 'ArrowLeft') {
      e.preventDefault();
      e.stopPropagation();
      return this.back();
    }
    if (e.key === 'Tab') {
      // the focus stays on the card's buttons
      const buttons = [...this.card.querySelectorAll('button')].filter((b) => !b.disabled && !b.classList.contains('hidden'));
      const at = buttons.indexOf(document.activeElement);
      const to = e.shiftKey ? (at <= 0 ? buttons.length - 1 : at - 1) : at + 1 >= buttons.length ? 0 : at + 1;
      e.preventDefault();
      e.stopPropagation();
      if (buttons[to]) buttons[to].focus();
      return;
    }
    // the page's own keys (the marks, say) wait until the tour is over;
    // a button on the card still takes Enter and space, and the browser
    // its own shortcuts (zoom, find, reload)
    e.stopPropagation();
    if (e.ctrlKey || e.metaKey || e.altKey || /^F\d+$/.test(e.key)) return;
    if (!(inCard && (e.key === 'Enter' || e.key === ' '))) e.preventDefault();
  }

  /** Place again if the target moved (a scroll, a layout change). */
  follow() {
    if (!this.target) return;
    if (!this.target.isConnected) {
      this.target = null;
      this.el.classList.add('centred');
      this.place();
      return;
    }
    const r = this.target.getBoundingClientRect();
    const l = this.last;
    if (!l || Math.abs(l.left - r.left) > 0.5 || Math.abs(l.top - r.top) > 0.5 || Math.abs(l.width - r.width) > 0.5 || Math.abs(l.height - r.height) > 0.5) this.place();
  }

  place() {
    if (!this.el) return;
    const vw = document.documentElement.clientWidth;
    const vh = document.documentElement.clientHeight;
    const card = this.card;
    const cw = card.offsetWidth;
    const ch = card.offsetHeight;
    if (!this.target) {
      card.style.left = `${Math.max(MARGIN, (vw - cw) / 2)}px`;
      card.style.top = `${Math.max(MARGIN, (vh - ch) / 2.4)}px`;
      this.arrow.className = 'tour-arrow';
      this.finishPlacing();
      return;
    }
    const r = this.target.getBoundingClientRect();
    this.last = { left: r.left, top: r.top, width: r.width, height: r.height };
    // the light: the target and a little room, kept on screen
    const sx = Math.max(2, r.left - PAD);
    const sy = Math.max(2, r.top - PAD);
    const sw = Math.min(vw - 2, r.right + PAD) - sx;
    const sh = Math.min(vh - 2, r.bottom + PAD) - sy;
    Object.assign(this.spot.style, { left: `${sx}px`, top: `${sy}px`, width: `${Math.max(0, sw)}px`, height: `${Math.max(0, sh)}px` });
    // the card: beside the light where it fits, else over the bottom of the view
    const clampX = (x) => Math.min(vw - cw - MARGIN, Math.max(MARGIN, x));
    const clampY = (y) => Math.min(vh - ch - MARGIN, Math.max(MARGIN, y));
    const spots = {
      bottom: () => (sy + sh + GAP + ch <= vh - MARGIN ? { x: clampX(sx + sw / 2 - cw / 2), y: sy + sh + GAP } : null),
      top: () => (sy - GAP - ch >= MARGIN ? { x: clampX(sx + sw / 2 - cw / 2), y: sy - GAP - ch } : null),
      right: () => (sx + sw + GAP + cw <= vw - MARGIN ? { x: sx + sw + GAP, y: clampY(sy + sh / 2 - ch / 2) } : null),
      left: () => (sx - GAP - cw >= MARGIN ? { x: sx - GAP - cw, y: clampY(sy + sh / 2 - ch / 2) } : null),
    };
    const want = this.steps[this.i].placement;
    const order = [want, 'bottom', 'top', 'right', 'left'].filter((p, k, a) => p && spots[p] && a.indexOf(p) === k);
    let side = null;
    let at = null;
    for (const p of order) {
      at = spots[p]();
      if (at) {
        side = p;
        break;
      }
    }
    if (!at) at = { x: clampX(vw / 2 - cw / 2), y: vh - ch - MARGIN };
    card.style.left = `${at.x}px`;
    card.style.top = `${at.y}px`;
    // the arrow points from the card's edge at the middle of the light
    this.arrow.className = side ? `tour-arrow ${side}` : 'tour-arrow';
    if (side === 'bottom' || side === 'top') this.arrow.style.left = `${Math.min(cw - 18, Math.max(10, sx + sw / 2 - at.x - 7))}px`;
    else this.arrow.style.left = '';
    if (side === 'left' || side === 'right') this.arrow.style.top = `${Math.min(ch - 18, Math.max(10, sy + sh / 2 - at.y - 7))}px`;
    else this.arrow.style.top = '';
    this.finishPlacing();
  }

  finishPlacing() {
    // the first placing is where it starts; after that the light glides
    if (!this.placed) {
      this.placed = true;
      requestAnimationFrame(() => this.el && this.el.classList.add('moving'));
    }
  }
}
