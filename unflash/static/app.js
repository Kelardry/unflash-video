/* Unflash UI */
"use strict";

const $ = (id) => document.getElementById(id);

const state = {
  project: null,
  sectionId: null,
  section: null,        // full detail of the open section
  edits: {},            // ordinal(str) -> {removed, extended}
  selection: new Set(), // ordinals (int)
  anchor: null,
  flagged: new Set(),
  flaggedRed: new Set(),
  saveTimer: null,
  activeJob: null,
  audioCtx: null,
};

// ---------- api ----------
async function api(path, method = "GET", body = null) {
  const opt = { method, headers: {} };
  if (body !== null) {
    opt.headers["Content-Type"] = "application/json";
    opt.body = JSON.stringify(body);
  }
  const r = await fetch(path, opt);
  const data = await r.json().catch(() => ({}));
  if (!r.ok) {
    const err = new Error(data.error || r.statusText);
    err.status = r.status;
    err.needVideo = !!data.need_video;   // project folder without its video
    throw err;
  }
  return data;
}

function toast(msg, isError = false) {
  const t = $("toast");
  t.textContent = msg;
  t.className = "toast" + (isError ? " error" : "");
  clearTimeout(t._h);
  t._h = setTimeout(() => t.classList.add("hidden"), isError ? 9000 : 4500);
}

// ---------- long-operation notification ----------
$("notifyMin").value = localStorage.getItem("unflash.notifyMin") ?? 5;
$("notifyMin").onchange = () => {
  localStorage.setItem("unflash.notifyMin", $("notifyMin").value);
};
document.addEventListener("click", () => {
  // create the audio context inside a user gesture so beeps are allowed later
  if (!state.audioCtx) {
    try { state.audioCtx = new (window.AudioContext || window.webkitAudioContext)(); }
    catch (e) { /* no audio available */ }
  }
}, { once: true });

function beep() {
  const ctx = state.audioCtx;
  if (!ctx) return;
  try {
    if (ctx.state === "suspended") ctx.resume();
    const t0 = ctx.currentTime;
    for (const [f, start] of [[660, 0], [880, 0.18]]) {
      const o = ctx.createOscillator();
      const g = ctx.createGain();
      o.frequency.value = f;
      g.gain.setValueAtTime(0.001, t0 + start);
      g.gain.exponentialRampToValueAtTime(0.2, t0 + start + 0.02);
      g.gain.exponentialRampToValueAtTime(0.001, t0 + start + 0.35);
      o.connect(g).connect(ctx.destination);
      o.start(t0 + start);
      o.stop(t0 + start + 0.4);
    }
  } catch (e) { /* ignore */ }
}

function notifyLongOp(label, elapsedMs) {
  const minMin = parseFloat($("notifyMin").value);
  if (!(minMin >= 0) || elapsedMs < minMin * 60000) return;
  beep();
  const msg = `${label} finished (${(elapsedMs / 60000).toFixed(1)} min)`;
  if ("Notification" in window) {
    if (Notification.permission === "granted") {
      new Notification("Unflash", { body: msg });
    } else if (Notification.permission !== "denied") {
      Notification.requestPermission();
    }
  }
  document.title = "✅ " + msg;
  setTimeout(() => { document.title = "Unflash"; }, 15000);
}

// ---------- jobs ----------
function pollJob(jobId, label, onDone, startedAt = Date.now()) {
  state.activeJob = jobId;
  $("jobbar").classList.remove("hidden");
  $("jobName").textContent = label;
  if ("Notification" in window && Notification.permission === "default") {
    Notification.requestPermission();
  }
  const tick = async () => {
    let j;
    try { j = await api(`/api/job/${jobId}`); }
    catch (e) { toast(`${label}: ${e.message}`, true); $("jobbar").classList.add("hidden"); return; }
    $("jobBar").style.width = `${(j.progress * 100).toFixed(1)}%`;
    $("jobMsg").textContent = j.message || j.status;
    if (j.status === "running" || j.status === "queued") {
      setTimeout(tick, 500);
      return;
    }
    $("jobbar").classList.add("hidden");
    state.activeJob = null;
    notifyLongOp(label, Date.now() - startedAt);
    if (j.status === "error") { toast(`${label} failed: ${j.error}`, true); return; }
    if (j.status === "cancelled") { toast(`${label} cancelled`); return; }
    if (onDone) onDone(j.result);
  };
  tick();
}

$("btnCancelJob").onclick = () => {
  if (state.activeJob) api(`/api/job/${state.activeJob}/cancel`, "POST").catch(() => {});
};

async function resumeActiveJobs() {
  try {
    const d = await api("/api/jobs");
    if (d.jobs && d.jobs.length) {
      const j = d.jobs[d.jobs.length - 1];
      pollJob(j.id, j.name, () => { toast(`${j.name} finished`); refreshProject(); });
    }
  } catch (e) { /* server may have no jobs */ }
}

// ---------- time helpers ----------
function fmtTime(s) {
  const sign = s < 0 ? "−" : "";
  const a = Math.abs(s);
  const h = Math.floor(a / 3600), m = Math.floor((a % 3600) / 60);
  const sec = (a % 60).toFixed(2).padStart(5, "0");
  return sign + (h ? `${h}:${String(m).padStart(2, "0")}` : `${m}`) + `:${sec}`;
}

function parseTime(str) {
  str = (str || "").trim().replace("−", "-");
  if (!str) return NaN;
  const neg = str.startsWith("-");
  if (neg) str = str.slice(1);
  const parts = str.split(":").map(parseFloat);
  if (parts.some(isNaN) || parts.length > 3) return NaN;
  let s = 0;
  for (const p of parts) s = s * 60 + p;
  return neg ? -s : s;
}

function bounds() {
  const p = state.project;
  if (p && p.bounds && p.bounds[1] > p.bounds[0]) return p.bounds;
  return [0, (p && p.info && p.info.duration) || 1];
}

// ---------- project ----------
async function refreshProject(openSid = null) {
  const d = await api("/api/project");
  state.project = d.project;
  const p = state.project;
  $("btnScan").disabled = !p;
  $("btnExport").disabled = !p;
  if (!p) return;
  const info = p.info;
  $("videoInfo").textContent =
    `${p.video_path.split(/[\\/]/).pop()} — ${info.width}×${info.height}, ` +
    `${info.fps.toFixed(2)} fps, ${fmtTime(bounds()[1] - bounds()[0])}`;
  if (p.profile) $("profileSel").value = p.profile;
  $("timelineWrap").classList.remove("hidden");
  // the list first: it is what gets clicked, so it must not be left stale by
  // anything that goes wrong while drawing the timeline canvas
  renderSectionList();
  drawTimeline();
  if (openSid) openSection(openSid);
  else if (state.sectionId && p.sections[state.sectionId]) openSection(state.sectionId);
}

// notes from opening a project (relinked paths, missing files, …)
const esc = (s) => String(s).replace(/[&<>]/g,
  (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));

function showNotes(notes, recoverable) {
  const el = $("banner");
  const lines = (notes || []).map((n) => `<div>⚠ ${esc(n)}</div>`);
  const n = (recoverable || []).length;
  if (n) {
    lines.push(`<div>⚠ This folder holds ${plural(n, "section folder", "section folders")} `
      + `(#${recoverable.map(esc).join(", #")}) that the project file does not list — `
      + `renders and proxies that are not part of the project. They can be rebuilt from `
      + `what is on disk; frame marks cannot be brought back.</div>`);
  }
  $("bannerText").innerHTML = lines.join("");
  $("btnRecover").classList.toggle("hidden", !n);
  el.classList.toggle("hidden", !lines.length);
}
$("btnCloseBanner").onclick = () => $("banner").classList.add("hidden");

$("btnRecover").onclick = async () => {
  if (!confirm("Rebuild the unlisted section folders as sections?\n\n"
    + "Time ranges are taken from the last export's part list (or the last scan). "
    + "Existing full-res renders are kept and can be exported as they are, but the "
    + "frame marks that produced them are gone.")) return;
  try {
    const r = await api("/api/recover_sections", "POST", {});
    await refreshProject();
    showNotes(r.notes, r.recoverable);
    toast(r.recovered.length
      ? `Recovered ${plural(r.recovered.length, "section", "sections")}.`
      : "Nothing could be recovered — see the message at the top.",
      !r.recovered.length);
  } catch (e) { toast(e.message, true); }
};

async function pickVideoPath() {
  const r = await api("/api/pick", "POST", {});
  if (r.path) return r.path;
  if (r.error) toast(r.error, true);
  // picker failed or was cancelled — offer a paste-a-path fallback
  return prompt("Or paste the full path of the video file:");
}

async function afterOpen(res, msg) {
  state.sectionId = null;
  $("workspace").classList.add("hidden");
  $("welcome").classList.remove("hidden");
  await refreshProject();
  showNotes(res.notes, res.recoverable);
  toast(msg);
}

$("btnOpen").onclick = async () => {
  try {
    const path = await pickVideoPath();
    if (!path) return;
    toast("Opening video… (large or unusual files can take a moment)");
    const res = await api("/api/open", "POST", { path });
    await afterOpen(res, "Video opened. Run a scan to find problem sections.");
  } catch (e) { toast(e.message, true); }
};

$("btnOpenProject").onclick = async () => {
  try {
    const r = await api("/api/pick_dir", "POST", {});
    let path = r.path;
    if (!path) {
      if (r.error) toast(r.error, true);
      path = prompt("Or paste the full path of the .unflash project folder:");
      if (!path) return;
    }
    toast("Opening project…");
    let res;
    try {
      res = await api("/api/open_project", "POST", { path });
    } catch (e) {
      if (!e.needVideo) throw e;
      // the folder was moved away from its video: ask which video it is now
      toast(e.message, true);
      const video = await pickVideoPath();
      if (!video) return;
      res = await api("/api/open_project", "POST", { path, video });
    }
    await afterOpen(res, "Project opened from " + path);
  } catch (e) { toast(e.message, true); }
};

$("profileSel").onchange = async () => {
  try {
    const prof = $("profileSel").value;
    const r = await api("/api/settings", "POST", { profile: prof });
    let msg = "Detection profile changed — re-scan, then use the sidebar's "
      + "'all sections' menu to re-prepare, re-check and refresh labels "
      + "under it without losing your edits.";
    if (r.detector && r.detector.extended_mode === "off") {
      msg += " This profile does not report extended flashes; sections already created for them stay until you delete them.";
    }
    toast(msg);
  } catch (e) { toast(e.message, true); }
};

$("btnScan").onclick = async () => {
  try {
    const r = await api("/api/scan", "POST", {});
    pollJob(r.job, "Scanning", (res) => {
      let msg;
      if (res.safe) {
        msg = "Scan complete: no violations found 🎉";
      } else {
        const bits = [];
        if (res.violations) bits.push(plural(res.violations,
          "WCAG violation window", "WCAG violation windows"));
        if (res.extended) bits.push(plural(res.extended,
          "extended flash", "extended flashes"));
        msg = `Scan complete: ${bits.join(" + ")}, `
          + `${plural(res.sections_created, "new section", "new sections")}.`;
        if (res.extended && !res.flag_extended) {
          msg += " Extended flashes are informational under this profile (gray timeline marks, no sections).";
        } else if (res.extended) {
          msg += " Extended flashes pass WCAG but are hazardous for some viewers — their sections are marked 'extended flash'.";
        }
      }
      toast(msg);
      refreshProject();
    });
  } catch (e) { toast(e.message, true); }
};

// ---------- violation wording ----------
// "extended" is an extended flash (sustained sub-threshold flashing), not the
// frame-extension edit of the same name.
const KIND_LABELS = { flash: "general flash", red: "red flash",
                      extended: "extended flash" };
function kindLabel(k) { return KIND_LABELS[k] || k; }

function plural(n, one, many) { return `${n} ${n === 1 ? one : many}`; }

// compact form for the sidebar badges ("manual" and "flash" read fine as-is;
// "extended" alone would be mistaken for the frame-extension edit)
function kindBadge(k) { return k === "extended" ? "extended flash" : k; }

// Split a verdict's violations into WCAG failures and extended flashes.
function splitViolations(res) {
  const all = res.violations || [];
  return {
    wcag: all.filter((x) => x.kind !== "extended"),
    ext: all.filter((x) => x.kind === "extended"),
  };
}

// Where to look for a violation. The reported span runs from the first flash
// that feeds the failure to the last frame still over the rate, and a long
// passage of flashing merges into one span — so the span alone can be a
// minute of video with no hint where inside it to look. `peak` is the worst
// moment in it; quoting a couple of seconds around that is what you can
// actually go and watch.
function violationWhere(v) {
  const from = Math.min(v.onset ?? v.start, v.start);
  const span = `${fmtTime(from)}–${fmtTime(v.end)}`;
  const peak = v.peak ?? v.start;
  if (v.end - from <= 2.5) return span;
  return `${span}, worst around ${fmtTime(Math.max(from, peak - 0.5))}`
    + `–${fmtTime(Math.min(v.end, peak + 1.0))}`;
}

// The section whose range covers a moment, if any — a verify failure is much
// easier to act on when it says which section to reopen.
function sectionAt(t) {
  const secs = Object.values((state.project || {}).sections || {});
  return secs.find((s) => t >= s.start - 0.001 && t <= s.end + 0.001);
}

// "2 WCAG violation window(s) + 1 extended flash" — extended flashes are only
// mentioned when the active profile flags them (otherwise none are reported).
function violationPhrase(res) {
  const { wcag, ext } = splitViolations(res);
  const bits = [];
  if (wcag.length) bits.push(plural(wcag.length, "WCAG violation window",
                                    "WCAG violation windows"));
  if (ext.length && res.flag_extended)
    bits.push(plural(ext.length, "extended flash", "extended flashes"));
  return bits.join(" + ") || "no violations";
}

// ---------- timeline ----------
function drawTimeline() {
  const p = state.project;
  const cv = $("timeline");
  const W = cv.width = cv.clientWidth * (window.devicePixelRatio || 1);
  const H = cv.height;
  const ctx = cv.getContext("2d");
  ctx.clearRect(0, 0, W, H);
  const [t0, t1] = bounds();
  const span = Math.max(1e-6, t1 - t0);
  const X = (t) => (t - t0) / span * W;
  const scan = p.scan;
  if (scan && scan.timeline) {
    const tl = scan.timeline;
    const tlt0 = tl.t0 ?? 0;
    const n = tl.general.length;
    for (let i = 0; i < n; i++) {
      const g = tl.general[i], r = tl.red[i];
      if (!g && !r) continue;
      const inten = Math.min(1, Math.max(g, r) / 6);
      ctx.fillStyle = r > 0 ? `rgba(255,80,160,${0.35 + 0.65 * inten})`
                            : `rgba(255,${Math.round(160 - 100 * inten)},40,${0.35 + 0.65 * inten})`;
      const h = 10 + inten * (H - 22);
      const x0 = X(tlt0 + i * tl.bin);
      const x1 = X(tlt0 + (i + 1) * tl.bin);
      ctx.fillRect(x0, H - 6 - h, Math.max(1, x1 - x0), h);
    }
  }
  // extended flashes: dim gray strips along the top (they also get their own
  // sections when the profile flags them)
  if (scan && scan.violations) {
    ctx.fillStyle = "rgba(150,155,170,.35)";
    for (const v of scan.violations) {
      if (v.kind !== "extended") continue;
      const x0 = X(v.start), x1 = X(v.end);
      ctx.fillRect(x0, 0, Math.max(2, x1 - x0), 4);
    }
  }
  ctx.fillStyle = "#3a4050";
  ctx.fillRect(0, H - 5, W, 2);
  ctx.font = `${11 * (window.devicePixelRatio || 1)}px sans-serif`;
  for (const s of Object.values(p.sections)) {
    const x0 = X(s.start), x1 = X(s.end);
    const active = s.id === state.sectionId;
    ctx.strokeStyle = active ? "#4da3ff" : "#8891a5";
    ctx.lineWidth = 2;
    ctx.strokeRect(x0, 2, Math.max(3, x1 - x0), H - 8);
    ctx.fillStyle = active ? "#4da3ff" : "#aab2c5";
    ctx.fillText(`#${s.id}`, x0 + 3, 14 * (window.devicePixelRatio || 1));
  }
}

(() => {
  const cv = $("timeline");
  let dragStart = null;
  const toT = (ev) => {
    const rect = cv.getBoundingClientRect();
    const [t0, t1] = bounds();
    return t0 + (ev.clientX - rect.left) / rect.width * (t1 - t0);
  };
  cv.onmousedown = (ev) => { dragStart = toT(ev); };
  cv.onmouseup = async (ev) => {
    const t = toT(ev);
    const p = state.project;
    if (!p) return;
    if (dragStart !== null && Math.abs(t - dragStart) > 0.5) {
      const a = Math.min(dragStart, t), b = Math.max(dragStart, t);
      try {
        const r = await api("/api/sections", "POST", { start: a, end: b });
        toast(`Section #${r.section.id} added (${fmtTime(r.section.start)}–${fmtTime(r.section.end)})`);
        await refreshProject(r.section.id);
      } catch (e) { toast(e.message, true); }
    } else {
      const hit = Object.values(p.sections).find((s) => t >= s.start && t <= s.end);
      if (hit) openSection(hit.id);
    }
    dragStart = null;
  };
})();

$("btnAddSection").onclick = async () => {
  const a = parseTime($("addStart").value);
  const b = parseTime($("addEnd").value);
  if (isNaN(a) || isNaN(b) || b <= a) { toast("Enter valid start and end times (e.g. 1:23.5)", true); return; }
  try {
    // typed timestamps are exact (no keyframe snap); smart-cut export will
    // require re-encode mode for unaligned sections
    const r = await api("/api/sections", "POST", { start: a, end: b, snap: false });
    toast(`Section #${r.section.id} added (${fmtTime(r.section.start)}–${fmtTime(r.section.end)})`);
    $("addStart").value = ""; $("addEnd").value = "";
    await refreshProject(r.section.id);
  } catch (e) { toast(e.message, true); }
};

// ---------- section list ----------
function renderSectionList() {
  const list = $("sectionList");
  list.innerHTML = "";
  const secs = Object.values(state.project.sections)
    .sort((a, b) => a.start - b.start);
  if (!secs.length) {
    list.innerHTML = '<div style="color:var(--fg-dim);font-size:12px">No sections yet — run a scan, drag on the timeline, or type timestamps.</div>';
    return;
  }
  for (const s of secs) {
    const el = document.createElement("div");
    el.className = "section-item" + (s.id === state.sectionId ? " active" : "");
    const badges = [];
    for (const k of s.kinds || [])
      badges.push(`<span class="badge kind ${k === "extended" ? "ext" : k}">${kindBadge(k)}</span>`);
    if (!s.prepared) badges.push('<span class="badge neutral">unprepared</span>');
    if (s.n_edits) badges.push(`<span class="badge neutral">${s.n_edits} edits</span>`);
    if (s.check_safe === true) badges.push('<span class="badge ok">check ✓</span>');
    if (s.check_safe === false) badges.push('<span class="badge bad">check ✗</span>');
    if (s.render_stale) badges.push('<span class="badge warn">render stale ⚠</span>');
    else if (s.render_safe === true) badges.push('<span class="badge ok">rendered ✓</span>');
    else if (s.render_safe === false) badges.push('<span class="badge bad">rendered ✗</span>');
    // a recovered render carries no verdict: rendered, but never checked
    else if (s.has_render) badges.push('<span class="badge neutral">rendered, unchecked</span>');
    // flashing the section's edits leave just past its own end: real in the
    // export, but no frame of this section can remove it
    if (s.check_after || s.check_spills)
      badges.push(`<span class="badge warn" title="A failure reaches past this section's last frame — extend it, or edit the section after it">runs past end ⚠</span>`);
    if ((s.check_context_notes || []).length)
      badges.push('<span class="badge warn" title="Its check ran without the run-up frames, so it cannot see flashing in its opening second">prepare again ⚠</span>');
    if ((s.warnings || []).length) badges.push('<span class="badge warn">⚠ notes</span>');
    el.innerHTML = `<div class="times">#${s.id} · ${fmtTime(s.start)} – ${fmtTime(s.end)}</div>
      <div class="meta">${badges.join("")}</div>`;
    el.onclick = () => openSection(s.id);
    list.appendChild(el);
  }
}

$("btnPrepareAll").onclick = async () => {
  try {
    const r = await api("/api/prepare_all", "POST", {});
    pollJob(r.job, "Preparing all sections", (res) => {
      toast(`Prepared ${res.prepared}/${res.total} sections.`);
      refreshProject();
    });
  } catch (e) { toast(e.message, true); }
};

$("btnRenderAll").onclick = async () => {
  try {
    const r = await api("/api/render_all", "POST", {});
    pollJob(r.job, "Rendering all sections", (res) => {
      let msg = `Rendered ${res.rendered}/${res.total} sections` +
        (res.skipped ? ` (${res.skipped} already up to date)` : "") + ".";
      if ((res.unprepared || []).length) {
        msg += ` Skipped unprepared: #${res.unprepared.join(", #")}.`;
      }
      toast(msg, (res.unprepared || []).length > 0);
      refreshProject();
    });
  } catch (e) { toast(e.message, true); }
};

// ---------- "all sections" menu ----------
// Re-runs of a processing step over every section: changing the detection
// profile, or moving to a new version of the program, makes the whole project
// worth putting through a step again without starting over and losing edits.
function sectionCount() {
  return Object.keys((state.project || {}).sections || {}).length;
}

function closeAllMenu() {
  $("allMenu").classList.add("hidden");
  $("btnAllMenu").setAttribute("aria-expanded", "false");
}

$("btnAllMenu").onclick = () => {
  const opened = $("allMenu").classList.toggle("hidden") === false;
  $("btnAllMenu").setAttribute("aria-expanded", String(opened));
};
document.addEventListener("click", (ev) => {
  if (!ev.target.closest("#allMenu, #btnAllMenu")) closeAllMenu();
});
document.addEventListener("keydown", (ev) => {
  if (ev.key === "Escape") closeAllMenu();
});

$("allMenu").onclick = (ev) => {
  const btn = ev.target.closest("button[data-act]");
  if (!btn) return;
  closeAllMenu();
  const act = ALL_ACTIONS[btn.dataset.act];
  if (act) act();
};

// a bulk run can change any section, including the open one
function afterBulk() {
  refreshProject();
}

// stop showing a section: used when it has been deleted, and when opening one
// fails because it is no longer there
function forgetOpenSection() {
  state.sectionId = null;
  state.section = null;
  $("workspace").classList.add("hidden");
  $("welcome").classList.remove("hidden");
}

async function reprepareAll() {
  const n = sectionCount();
  if (!n) { toast("No sections yet"); return; }
  if (!confirm(`Re-prepare all ${n} sections?\n\n`
    + "Each one is analyzed again with the current detection profile and its "
    + "proxy and thumbnails are rebuilt. Your frame marks are kept. This "
    + "takes about as long as preparing them the first time.")) return;
  try {
    const r = await api("/api/prepare_all", "POST", { force: true });
    pollJob(r.job, "Re-preparing all sections", (res) => {
      toast(`Re-prepared ${res.prepared}/${res.total} sections.`);
      afterBulk();
    });
  } catch (e) { toast(e.message, true); }
}

async function rerenderAll() {
  const n = sectionCount();
  if (!n) { toast("No sections yet"); return; }
  if (!confirm("Re-render every prepared section at full resolution?\n\n"
    + "Sections already rendered with their current edits are rendered again "
    + "too, so this can take a long time.")) return;
  try {
    const r = await api("/api/render_all", "POST", { force: true });
    pollJob(r.job, "Re-rendering all sections", (res) => {
      let msg = `Re-rendered ${res.rendered}/${res.total} sections.`;
      if ((res.unprepared || []).length) {
        msg += ` Skipped unprepared: #${res.unprepared.join(", #")}.`;
      }
      toast(msg, (res.unprepared || []).length > 0);
      afterBulk();
    });
  } catch (e) { toast(e.message, true); }
}

async function recheckAll() {
  try {
    const r = await api("/api/check_all", "POST", {});
    pollJob(r.job, "Re-checking all sections", (res) => {
      let msg = `Checked ${res.checked}/${res.total} sections: `
        + `${res.safe} pass, ${res.unsafe} fail.`;
      if ((res.unprepared || []).length) {
        msg += ` Skipped unprepared: #${res.unprepared.join(", #")}.`;
      }
      const bad = (res.failed || []).length;
      if (bad) msg += ` ${plural(bad, "section", "sections")} could not be checked.`;
      toast(msg, bad > 0 || res.unsafe > 0);
      if (bad) showNotes(res.failed.map((f) => `Could not check ${f}`));
      afterBulk();
    });
  } catch (e) { toast(e.message, true); }
}

async function refreshAllLabels() {
  try {
    const r = await api("/api/refresh_labels", "POST", {});
    await refreshProject();
    const notes = (r.notes || []).slice();
    if (r.cleared.length) {
      notes.push("Cleared labels that no longer describe the project: "
        + r.cleared.join(", ") + ".");
    }
    if (r.unverified.length) {
      notes.push("These verdicts do not record which detection settings "
        + "produced them, so they were left as they are — re-render to be "
        + "sure of them: " + r.unverified.join(", ") + ".");
    }
    showNotes(notes);
    toast(r.cleared.length
      ? `Cleared ${plural(r.cleared.length, "stale label", "stale labels")} — see the message at the top.`
      : "Every label is up to date.");
  } catch (e) { toast(e.message, true); }
}

async function deleteAllSections() {
  const n = sectionCount();
  if (!n) { toast("No sections to delete"); return; }
  if (!confirm(`Delete ALL ${n} sections, including their edits and renders? This cannot be undone.`)) return;
  try {
    const r = await api("/api/sections", "DELETE");
    toast(`Deleted ${r.deleted} sections.`);
    forgetOpenSection();
    // the server has no sections now, so say so locally too: the sidebar is
    // then right even if the refresh below fails or is overtaken by an
    // in-flight refresh from some other operation
    if (state.project) state.project.sections = {};
    renderSectionList();
    drawTimeline();
    await refreshProject();
  } catch (e) { toast(e.message, true); }
}

const ALL_ACTIONS = {
  reprepare: reprepareAll,
  rerender: rerenderAll,
  recheck: recheckAll,
  labels: refreshAllLabels,
  delete: deleteAllSections,
};

$("btnHome").onclick = () => {
  state.sectionId = null;
  state.section = null;
  $("workspace").classList.add("hidden");
  $("welcome").classList.remove("hidden");
  if (state.project) {
    renderSectionList();
    drawTimeline();
  }
};

// ---------- section workspace ----------
async function openSection(sid) {
  try {
    const d = await api(`/api/section/${sid}`);
    state.sectionId = sid;
    state.section = d.section;
    state.edits = JSON.parse(JSON.stringify(d.section.edits || {}));
    state.selection.clear();
    state.anchor = null;
    $("welcome").classList.add("hidden");
    $("workspace").classList.remove("hidden");
    $("wsTitle").textContent =
      `Section #${sid}: ${fmtTime(d.section.start)} – ${fmtTime(d.section.end)}`;
    $("secStart").value = fmtTime(d.section.start);
    $("secEnd").value = fmtTime(d.section.end);
    const warns = d.section.warnings || [];
    const wsW = $("wsWarnings");
    wsW.classList.toggle("hidden", !warns.length);
    wsW.innerHTML = warns.map((w) => `⚠ ${w}`).join("<br>");
    renderSectionList();
    drawTimeline();
    if (!d.section.prepared) {
      $("wsUnprepared").classList.remove("hidden");
      $("wsBody").classList.add("hidden");
      setVerdict(null);
    } else {
      $("wsUnprepared").classList.add("hidden");
      $("wsBody").classList.remove("hidden");
      computeFlagged();
      setVerdictFromSection();
      updateUnsafeBtn();
      setPlayerSource(bestPlayerSource());
      renderGrid();
      drawChart();
    }
  } catch (e) {
    toast(e.message, true);
    if (e.status === 404) {
      // the section is gone from the project but still listed here: whatever
      // left the sidebar behind, clicking a stale entry now clears it
      forgetOpenSection();
      if (state.project) delete state.project.sections[String(sid)];
      renderSectionList();
      drawTimeline();
      refreshProject().catch(() => {});
    }
  }
}

$("btnApplyRange").onclick = async () => {
  const a = parseTime($("secStart").value);
  const b = parseTime($("secEnd").value);
  if (isNaN(a) || isNaN(b) || b <= a) { toast("Enter valid start/end times", true); return; }
  if (!confirm("Changing the range resets this section's preparation, edits and renders. Continue?")) return;
  try {
    await api(`/api/section/${state.sectionId}`, "PATCH", { start: a, end: b, snap: false });
    toast("Section range updated — re-prepare it.");
    await refreshProject(state.sectionId);
  } catch (e) { toast(e.message, true); }
};

function bestPlayerSource() {
  const s = state.section;
  if (s.render && s.render.path) return "render";
  if (s.preview && s.preview.path) return "preview";
  return "proxy";
}

function setVerdict(v) {
  const el = $("wsVerdict");
  if (v === null) { el.className = "verdict unknown"; el.textContent = "not checked"; return; }
  el.className = "verdict " + (v ? "safe" : "unsafe");
  el.textContent = v ? "SAFE (passes detector)" : "UNSAFE (fails detector)";
}

function setVerdictFromSection() {
  const s = state.section;
  if (s.check) setVerdict(!!s.check.safe);
  else if (s.analysis) setVerdict(s.analysis.safe && Object.keys(state.edits).length === 0 ? true : null);
  else setVerdict(null);
}

function computeFlagged() {
  state.flagged.clear();
  state.flaggedRed.clear();
  const s = state.section;
  if (!s.analysis || !s.pts) return;
  const pts = s.pts;
  const t0 = s.start;
  for (const ev of s.analysis.events || []) {
    const rel = ev.t - t0;
    let lo = 0, hi = pts.length - 1;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (pts[mid] < rel) lo = mid + 1; else hi = mid;
    }
    const set = ev.kind === "red" ? state.flaggedRed : state.flagged;
    set.add(lo);
    if (lo > 0) set.add(lo - 1);
  }
}

$("btnPrepare").onclick = async () => {
  const sid = state.sectionId;
  const r = await api(`/api/section/${sid}/prepare`, "POST", {});
  pollJob(r.job, `Preparing section #${sid}`, () => {
    toast("Section prepared.");
    refreshProject(sid);
  });
};

$("btnDeleteSection").onclick = async () => {
  if (!confirm("Delete this section (its edits and renders)?")) return;
  const sid = state.sectionId;
  try {
    await api(`/api/section/${sid}`, "DELETE");
    forgetOpenSection();
    if (state.project) delete state.project.sections[sid];
    renderSectionList();
    drawTimeline();
    await refreshProject();
  } catch (e) { toast(e.message, true); }
};

// ---------- player ----------
function playerWarningText(which) {
  const dimmed = $("dimToggle").checked;
  const dimNote = dimmed ? " Player is dimmed." : " Player is NOT dimmed — full brightness!";
  if (which === "proxy") {
    return "⚠ Unedited section — may contain flashing." + dimNote + " Play deliberately.";
  }
  const s = state.section;
  const v = which === "preview" ? s.preview : s.render;
  const safe = v && v.verdict && v.verdict.safe;
  return (safe ? "✓ This edited version passes the detector."
               : "⚠ Edited version — has NOT passed (or not been checked by) the detector.") + dimNote;
}

function setPlayerSource(which) {
  const s = state.section;
  $("playerSource").value = which;
  const player = $("player");
  const url = `/media/${s.id}/${which === "proxy" ? "proxy" : which}.mp4`;
  player.src = url + `?t=${Date.now() % 1e7}`;
  $("playerLabel").textContent =
    which === "proxy" ? "Proxy (original content)" :
    which === "preview" ? "Preview render (edited, low-res)" : "Full render (edited)";
  $("playerWarning").textContent = playerWarningText(which);
}
$("playerSource").onchange = () => {
  const which = $("playerSource").value;
  const s = state.section;
  if (which === "preview" && !(s.preview && s.preview.path)) { toast("No preview rendered yet"); setPlayerSource(bestPlayerSource()); return; }
  if (which === "render" && !(s.render && s.render.path)) { toast("No full render yet"); setPlayerSource(bestPlayerSource()); return; }
  setPlayerSource(which);
};
$("dimToggle").onchange = () => {
  $("player").classList.toggle("dimmed", $("dimToggle").checked);
  $("playerWarning").textContent = playerWarningText($("playerSource").value);
};
$("player").classList.add("dimmed");

// ---------- edits ----------
function editOf(i) { return state.edits[String(i)] || null; }

function setEdit(i, removed, extended) {
  const k = String(i);
  if (!removed && !extended) delete state.edits[k];
  else state.edits[k] = { removed, extended: extended && !removed };
  scheduleSave();
}

function scheduleSave() {
  setVerdict(null);
  if (state.section) state.section.check = null;   // edits changed: stale
  updateUnsafeBtn();
  clearTimeout(state.saveTimer);
  state.saveTimer = setTimeout(async () => {
    try {
      await api(`/api/section/${state.sectionId}/edits`, "POST", { edits: state.edits });
      refreshBadges();
    } catch (e) { toast("Saving edits failed: " + e.message, true); }
  }, 600);
}

async function refreshBadges() {
  // update sidebar badges/timeline without reloading the open section
  // (a full refresh would discard the user's selection)
  try {
    const d = await api("/api/project");
    state.project = d.project;
    renderSectionList();
    drawTimeline();
  } catch (e) { /* transient */ }
}

function toggleSelected(kind) {
  if (!state.selection.size) { toast("Select frames first (click, shift-click for ranges)"); return; }
  const items = [...state.selection];
  const first = editOf(items[0]);
  let target;
  if (kind === "removed") target = !(first && first.removed);
  if (kind === "extended") target = !(first && first.extended);
  for (const i of items) {
    const e = editOf(i) || { removed: false, extended: false };
    if (kind === "removed") setEdit(i, target, target ? false : e.extended);
    else if (kind === "extended") setEdit(i, target ? false : e.removed, target);
    else setEdit(i, false, false);
  }
  updateGridClasses();
  drawChart();
}

$("btnMarkRemoved").onclick = () => toggleSelected("removed");
$("btnMarkExtended").onclick = () => toggleSelected("extended");
$("btnUnmark").onclick = () => toggleSelected("unmark");
$("btnClearEdits").onclick = () => {
  if (!confirm("Clear ALL marks in this section?")) return;
  state.edits = {};
  scheduleSave();
  updateGridClasses();
  drawChart();
};
$("btnSelectFlagged").onclick = () => {
  state.selection = new Set([...state.flagged, ...state.flaggedRed]);
  updateGridClasses();
};

function updateUnsafeBtn() {
  const check = state.section && state.section.check;
  const frames = (check && !check.safe && check.flagged_frames) || [];
  $("btnSelectUnsafe").classList.toggle("hidden", frames.length === 0);
}

$("btnSelectUnsafe").onclick = () => {
  const check = state.section && state.section.check;
  const frames = (check && check.flagged_frames) || [];
  if (!frames.length) return;
  state.selection = new Set(frames);
  state.anchor = frames[0];
  updateGridClasses();
  const cell = $("frameGrid").children[frames[0]];
  if (cell) cell.scrollIntoView({ behavior: "smooth", block: "center" });
  toast(`Selected ${frames.length} frames inside the failing window(s).`);
};

document.addEventListener("keydown", (ev) => {
  if (ev.target.tagName === "INPUT" || ev.target.tagName === "SELECT" || ev.target.tagName === "TEXTAREA") return;
  if (!state.section || !state.section.prepared) return;
  const k = ev.key.toLowerCase();
  if (k === "r") toggleSelected("removed");
  else if (k === "e") toggleSelected("extended");
  else if (k === "u") toggleSelected("unmark");
  else if (k === "escape") { state.selection.clear(); state.anchor = null; updateGridClasses(); }
});

// ---------- frame grid ----------
function gridColumns() {
  const grid = $("frameGrid");
  const cols = getComputedStyle(grid).gridTemplateColumns.split(" ").length;
  return Math.max(1, cols);
}

function renderGrid() {
  const s = state.section;
  const grid = $("frameGrid");
  const info = state.project.info;
  // reserve thumbnail height before images load, so the layout never shifts
  grid.style.setProperty("--thumb-aspect", `${info.width}/${info.height}`);
  grid.innerHTML = "";
  const n = s.n_frames;
  $("frameCount").textContent = `(${n})`;
  const frag = document.createDocumentFragment();
  for (let i = 0; i < n; i++) {
    const cell = document.createElement("div");
    cell.className = "frame-cell";
    cell.dataset.i = i;
    const img = document.createElement("img");
    img.loading = "lazy";
    // the key makes this address specific to these thumbnails; without it a
    // browser reuses the ones it cached for section #<id> of another video
    img.src = `/thumb/${s.id}/${i}?v=${s.thumb_key || ""}`;
    img.draggable = false;
    cell.appendChild(img);
    const fn = document.createElement("span");
    fn.className = "fnum"; fn.textContent = i;
    cell.appendChild(fn);
    const ft = document.createElement("span");
    ft.className = "ftime"; ft.textContent = (s.pts[i] ?? 0).toFixed(3);
    cell.appendChild(ft);
    const rep = document.createElement("span");
    rep.className = "rep"; rep.style.display = "none";
    cell.appendChild(rep);
    cell.onclick = (ev) => onCellClick(i, ev);
    frag.appendChild(cell);
  }
  grid.appendChild(frag);
  updateGridClasses();
}

function onCellClick(i, ev) {
  if (ev.shiftKey && state.anchor !== null) {
    if (!ev.ctrlKey) state.selection.clear();
    if (ev.getModifierState && ev.getModifierState("CapsLock")) {
      // rectangle select: anchor and i are opposite corners in the grid
      const cols = gridColumns();
      const [r0, c0] = [Math.floor(state.anchor / cols), state.anchor % cols];
      const [r1, c1] = [Math.floor(i / cols), i % cols];
      const [rA, rB] = [Math.min(r0, r1), Math.max(r0, r1)];
      const [cA, cB] = [Math.min(c0, c1), Math.max(c0, c1)];
      const n = state.section.n_frames;
      for (let r = rA; r <= rB; r++) {
        for (let c = cA; c <= cB; c++) {
          const j = r * cols + c;
          if (j < n) state.selection.add(j);
        }
      }
    } else {
      const [a, b] = [Math.min(state.anchor, i), Math.max(state.anchor, i)];
      for (let j = a; j <= b; j++) state.selection.add(j);
    }
  } else if (ev.ctrlKey) {
    if (state.selection.has(i)) state.selection.delete(i);
    else state.selection.add(i);
    state.anchor = i;
  } else {
    state.selection.clear();
    state.selection.add(i);
    state.anchor = i;
  }
  updateGridClasses();
}

function updateGridClasses() {
  const grid = $("frameGrid");
  let lastKept = 0;
  const rep = {};
  const n = state.section.n_frames;
  // leading removed frames are backfilled from the first kept frame
  let firstKept = 0;
  while (firstKept < n && editOf(firstKept) && editOf(firstKept).removed) firstKept++;
  if (firstKept >= n) firstKept = 0;
  lastKept = firstKept;
  for (let i = 0; i < n; i++) {
    const e = editOf(i);
    if (e && e.removed) rep[i] = lastKept;
    else lastKept = i;
  }
  for (const cell of grid.children) {
    const i = +cell.dataset.i;
    const e = editOf(i);
    cell.classList.toggle("removed", !!(e && e.removed));
    cell.classList.toggle("extended", !!(e && e.extended));
    const inRed = state.flaggedRed.has(i);
    cell.classList.toggle("flagged-red", inRed);
    cell.classList.toggle("flagged", !inRed && state.flagged.has(i));
    cell.classList.toggle("selected", state.selection.has(i));
    const repEl = cell.querySelector(".rep");
    if (e && e.removed) { repEl.style.display = ""; repEl.textContent = "→ shows #" + rep[i]; }
    else repEl.style.display = "none";
  }
}

// ---------- chart ----------
function drawChart() {
  const s = state.section;
  const cv = $("chart");
  const stats = s.analysis && s.analysis.frame_stats;
  const W = cv.width = cv.clientWidth * (window.devicePixelRatio || 1);
  const H = cv.height;
  const ctx = cv.getContext("2d");
  ctx.clearRect(0, 0, W, H);
  if (!stats || !stats.t || !stats.t.length) return;
  const n = stats.t.length;
  const x = (i) => i / Math.max(1, n - 1) * W;
  const maxArea = Math.max(1, ...stats.up_area, ...stats.down_area, ...stats.red_area);

  for (let i = 0; i < n; i++) {
    const e = editOf(i);
    if (!e) continue;
    ctx.fillStyle = e.removed ? "rgba(204,51,51,.25)" : "rgba(51,119,204,.3)";
    ctx.fillRect(x(i) - W / n / 2, 0, Math.max(1, W / n), H);
  }
  for (let i = 0; i < n; i++) {
    const up = stats.up_area[i] / maxArea, dn = stats.down_area[i] / maxArea,
          rd = stats.red_area[i] / maxArea;
    if (up) { ctx.fillStyle = "rgba(245,185,66,.8)"; ctx.fillRect(x(i), H / 2 - up * H / 2, 1.5, up * H / 2); }
    if (dn) { ctx.fillStyle = "rgba(245,120,66,.8)"; ctx.fillRect(x(i), H / 2, 1.5, dn * H / 2); }
    if (rd) { ctx.fillStyle = "rgba(255,80,160,.9)"; ctx.fillRect(x(i), H / 2 - rd * H / 2, 1.5, rd * H); }
  }
  ctx.beginPath();
  ctx.strokeStyle = "#e8eaf0";
  ctx.lineWidth = 1.2;
  for (let i = 0; i < n; i++) {
    const y = H - stats.lum[i] * (H - 8) - 4;
    if (i === 0) ctx.moveTo(x(i), y); else ctx.lineTo(x(i), y);
  }
  ctx.stroke();
  cv._n = n;
}

$("chart").onclick = (ev) => {
  const cv = $("chart");
  if (!cv._n) return;
  const rect = cv.getBoundingClientRect();
  const i = Math.round((ev.clientX - rect.left) / rect.width * (cv._n - 1));
  const cell = $("frameGrid").children[i];
  if (cell) {
    cell.scrollIntoView({ behavior: "smooth", block: "center" });
    state.selection.clear();
    state.selection.add(i);
    state.anchor = i;
    updateGridClasses();
  }
};

// ---------- suggest / check / render ----------
function runSuggest(prefer) {
  const sid = state.sectionId;
  const selOnly = $("suggestSelOnly").checked;
  let only = null;
  if (selOnly) {
    if (!state.selection.size) { toast("'Selection only' is on but nothing is selected", true); return; }
    only = [...state.selection];
  }
  api(`/api/section/${sid}/suggest`, "POST", { prefer, only })
    .then((r) => pollJob(r.job, `Suggesting (keep ${prefer}${selOnly ? ", selection only" : ""})`, (res) => {
      const merged = {};
      for (const [k, v] of Object.entries(state.edits)) {
        const idx = parseInt(k, 10);
        const inScope = only === null || only.includes(idx);
        if (v.extended && !v.removed) merged[k] = { removed: false, extended: true };
        else if (!inScope) merged[k] = v;   // outside scope: untouched
      }
      for (const k of Object.keys(res.edits)) {
        merged[k] = { removed: true, extended: false };
      }
      state.edits = merged;
      scheduleSave();
      updateGridClasses();
      drawChart();
      setVerdict(res.safe);
      toast(res.note, !res.safe);
    }))
    .catch((e) => toast(e.message, true));
}
$("btnSuggestLight").onclick = () => runSuggest("light");
$("btnSuggestDark").onclick = () => runSuggest("dark");

$("btnCheck").onclick = () => {
  const sid = state.sectionId;
  api(`/api/section/${sid}/check`, "POST", { edits: state.edits })
    .then((r) => pollJob(r.job, "Checking safety", (res) => {
      setVerdict(res.safe);
      if (state.section) state.section.check = res;
      updateUnsafeBtn();
      let msg = res.safe
        ? "Passes the detector."
        : `Fails: ${violationPhrase(res)} — use "select unsafe frames" to see them.`;
      if (!res.safe && res.wcag_safe) {
        msg += " (No WCAG failure left — what remains is extended flashing.)";
      }
      // flashing the edits leave in the run-out: real in the export, but past
      // this section's last frame, so nothing here can remove it
      const after = res.after || [];
      if (after.length) {
        msg += ` Also flashing just past the end of this section (`
          + after.map(violationWhere).join(", ")
          + `) — extend this section past it, or add one there.`;
      }
      // a failure that starts inside the section but carries on past its last
      // frame: the frames on offer here may not be enough on their own
      const spills = res.spills || [];
      if (spills.length) {
        msg += ` One or more of these carry on past the section's last frame (`
          + spills.map(violationWhere).join(", ")
          + `); if removing the frames offered does not clear it, extend the `
          + `section end or edit the following one.`;
      }
      for (const note of (res.context_notes || [])) msg += " " + note;
      toast(msg, !res.safe || after.length > 0);
      refreshBadges();
    }))
    .catch((e) => toast(e.message, true));
};


$("btnPreview").onclick = () => {
  const sid = state.sectionId;
  api(`/api/section/${sid}/preview`, "POST", {})
    .then((r) => pollJob(r.job, "Rendering preview", (res) => {
      const safe = res.verdict && res.verdict.safe;
      toast(safe ? "Preview rendered — passes the detector ✓"
                 : `Preview rendered — still FAILS the detector (${violationPhrase(res.verdict || {})})`, !safe);
      api(`/api/section/${sid}`).then((d) => {
        state.section = d.section;
        setPlayerSource("preview");
      });
    }))
    .catch((e) => toast(e.message, true));
};

$("btnRender").onclick = () => {
  const sid = state.sectionId;
  api(`/api/section/${sid}/render`, "POST", {})
    .then((r) => pollJob(r.job, "Rendering full resolution", (res) => {
      const safe = res.verdict && res.verdict.safe;
      let msg = safe ? "Full render complete — passes the detector ✓"
                     : `Full render complete — still FAILS the detector (${violationPhrase(res.verdict || {})})`;
      if (res.warning) msg += ` (${res.warning})`;
      toast(msg, !safe);
      refreshProject(sid);
    }))
    .catch((e) => toast(e.message, true));
};

// ---------- export ----------
$("btnExport").onclick = () => {
  const p = state.project;
  const secs = Object.values(p.sections).sort((a, b) => a.start - b.start);
  const lines = secs.map((s) => {
    let st;
    if (!s.has_render) st = "❌ NOT RENDERED — use 'Render full-res' first";
    else if (s.render_stale) st = "⚠ rendered, but edits changed since (re-render)";
    else if (s.render_safe === true) st = "✓ rendered & safe";
    else if (s.render_safe === false) st = "⚠ rendered but NOT safe";
    else st = "• rendered, never checked by the detector (recovered)";
    return `#${s.id} · ${fmtTime(s.start)}–${fmtTime(s.end)}: ${st}`;
  });
  $("exportSummary").innerHTML =
    (secs.length ? lines.join("<br>") : "No sections.") +
    "<br><br>Every section must be rendered at full resolution (and ideally ✓ safe) before export.";
  $("exportResult").textContent = "";
  $("exportModal").classList.remove("hidden");
  refreshExportPlan();
};
$("btnCloseExport").onclick = () => $("exportModal").classList.add("hidden");

// how the selected assembly will run, and whether the part count forces
// the filter join to be split into batches
async function refreshExportPlan() {
  const el = $("exportPlan");
  const mode = $("exportMode").value;
  el.className = "hint";
  try {
    const r = await api("/api/export_plan?mode=" + encodeURIComponent(mode));
    if (mode.endsWith("-filter")) {
      let txt = `Filter join: decodes and re-encodes the whole video once ` +
        `(one extra generation, ~45 dB PSNR — visually invisible). ` +
        `${r.parts} parts, command ${r.chars} of ${r.limit} characters.`;
      if (r.batches > 1) {
        txt += ` ⚠ That is more than one ffmpeg command can name, so the ` +
          `export will be assembled in ${r.batches} batches and those joined ` +
          `by stream copy — still only one re-encode generation, but slower.`;
        el.className = "hint warn";
      }
      el.textContent = txt;
    } else if (mode === "smartcut") {
      el.textContent = `Untouched spans are copied, not re-encoded. ` +
        `${r.parts} parts. Needs keyframe-aligned sections.`;
    } else {
      el.textContent = `Stream-copy join: no re-encode at the join, ` +
        `so no quality loss. ${r.parts} parts.`;
    }
  } catch (e) { el.textContent = ""; }
}
$("exportMode").onchange = refreshExportPlan;

$("btnDoExport").onclick = async () => {
  try {
    const name = state.project.video_path.split(/[\\/]/).pop().replace(/\.[^.]+$/, "") + ".unflashed.mp4";
    const pick = await api("/api/pick_save", "POST", { initial: name });
    if (!pick.path) return;
    const r = await api("/api/export", "POST", { mode: $("exportMode").value, path: pick.path });
    pollJob(r.job, "Exporting", (res) => {
      let txt = `Exported to ${res.path}\n`;
      for (const w of res.warnings || []) txt += `⚠ ${w}\n`;
      txt += 'Run "Verify exported file" to re-scan it.';
      $("exportResult").textContent = txt;
      toast("Export complete: " + res.path, (res.warnings || []).length > 0);
    });
  } catch (e) { $("exportResult").textContent = e.message; toast(e.message, true); }
};

$("btnVerifyExport").onclick = async () => {
  try {
    const r = await api("/api/verify_export", "POST", {});
    pollJob(r.job, "Verifying export", (res) => {
      const { wcag, ext } = splitViolations(res);
      const shown = res.flag_extended ? wcag.concat(ext) : wcag;
      shown.sort((a, b) => a.start - b.start);
      let txt;
      if (res.safe) {
        txt = `✓ Exported file passes the detector (profile: ${res.profile}).`;
      } else {
        const covered = (x) => sectionAt(x.peak ?? x.start)
          || sectionAt(Math.min(x.onset ?? x.start, x.start));
        const rows = shown.map((x) => {
          const sec = covered(x);
          const where = sec ? `in section #${sec.id}`
                            : "in material no section covers";
          return `  • ${kindLabel(x.kind)} ${violationWhere(x)} — ${where}`;
        });
        txt = `✗ Exported file still fails (profile: ${res.profile}):\n`
          + rows.join("\n");
        if (shown.some((x) => !covered(x))) {
          txt += "\nTimes no section covers need one: add a section over "
            + "them, prepare it, edit it, then re-render and re-export.";
        }
        if (shown.some(covered)) {
          txt += "\nTimes inside a section: reopen it and check it again. "
            + "If its check disagrees with this, prepare it again first "
            + "— a section prepared by an older version has no run-up "
            + "frames cached, and without them its check cannot see flashing "
            + "in its opening second.";
        }
        if (res.wcag_safe) {
          txt += "\nThese pass WCAG but are extended flashes — "
            + "hazardous for some viewers under this profile.";
        }
      }
      $("exportResult").textContent = txt;
    });
  } catch (e) { toast(e.message, true); }
};

// ---------- boot ----------
window.addEventListener("resize", () => { if (state.project) drawTimeline(); if (state.section && state.section.prepared) drawChart(); });
refreshProject().catch(() => {});
resumeActiveJobs();
