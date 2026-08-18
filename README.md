# Unflash

Detect and repair photosensitive-hazard flashing in videos, while keeping the
visual information that naive "flash removal" filters destroy. Removed frames
are replaced by the last kept frame (timing is untouched), and important
flash frames can be *extended* — held for 1 second with silent audio — so
nothing informational is lost.

## Setup

```
pip install -r requirements.txt
```

Requires `ffmpeg` and `ffprobe` on PATH.

## Run

```
run_unflash.bat
```

A browser page opens at http://127.0.0.1:8765/. Everything runs locally.

## Workflow

1. **Open video** — a work folder `<name>.unflash/` is created next to it;
   all state (sections, edits, proxies, renders) persists there, so you can
   close and resume anytime.
2. **Scan for flashes** — a WCAG 2.x / PEAT-style detector (general flash,
   red flash, extended-flash warning) runs over the whole video and produces
   numbered work *sections* around each problem, snapped to keyframes. The
   timeline shows a flash-intensity heatmap. You can also add sections by
   dragging on the timeline or typing exact timestamps, and change a
   section's range from its workspace (this resets its preparation).
3. **Prepare a section** (or **prepare all** in the sidebar) — analyzes every
   frame, caches analysis frames, builds a 540p proxy and per-frame
   thumbnails.
4. **Edit** — mark frames *removed* (red, replaced by last kept frame) or
   *extended* (blue, held 1 s with silence). Click to select; shift-click
   selects the run between clicks; with **Caps Lock on**, shift-click selects
   a geometric rectangle in the grid instead (no key-holding needed);
   ctrl-click adds/removes. Keys **R** / **E** / **U** apply to the
   selection. **Suggest: keep light / keep dark** proposes a removal set,
   verifies it through the detector, and escalates until the section passes —
   tick *selection only* to confine its removals to the frames you selected.
   If the very first frame is removed, it backfills from the first kept
   frame.
5. **Check safety** — instant verdict: your current edits are simulated
   through the detector without rendering anything. If it fails, **select
   unsafe frames** highlights exactly the frames inside the failing
   window(s). Frames in flash events carry a corner dot: yellow for general
   flashes, magenta for red flashes.
6. **Preview render** — applies edits at proxy resolution (decoded from the
   original, so frames always line up) and re-verifies the rendered file.
7. **Render full-res** — **required for every section before export** (or
   use **render all** in the sidebar, which skips sections already rendered
   with their current edits). If you edit a section after rendering it, it
   gets a *render stale ⚠* badge — re-render it, or export will warn.
8. **Export** — stitches rendered sections with the untouched spans.
   Both *re-encode* options rebuild every untouched span and differ only in
   how the parts are joined: the **stream-copy join** stitches them without
   re-encoding (fastest, no quality loss), while the **filter join** decodes
   and concatenates everything in a single ffmpeg pass — one extra encode
   generation (~45 dB PSNR, visually invisible) in exchange for rebuilding
   every timestamp, so no mismatch between parts can throw it off.
   *Smart-cut* stream-copies untouched spans (fast, h264 sources only).
   The dialog reports what the chosen join will do before you start,
   including the part count; past roughly 275 sections the filter join needs
   more inputs than one command line can name, so the export is assembled in
   batches — those are joined by stream copy, so batching costs no extra
   quality. The export self-checks that the output's timing matches the sum
   of its parts. Then **Verify exported file** re-scans the final output
   with your selected detector profile.

The 🔔 field in the header sets a threshold (minutes): any operation that
takes longer triggers a beep + desktop notification when it finishes.

## CLI

```
python -m unflash.cli analyze VIDEO [--start S --duration D] [--wcag]
python -m unflash.cli scan VIDEO [--wcag]
```

## Detection details

Implements the WCAG 2.x / PEAT definitions: relative luminance with sRGB
linearization; a transition qualifies when a pixel's accumulated monotonic
luminance change is >= 10% of max luminance with the darker state < 0.80
(red: |Δ(R−G−B)×320| > 20 **and** the pixel enters or leaves the saturated
state R/(R+G+B) >= 0.8 — brightness wobble inside a continuously-red scene is
not a red flash; red↔dark flashing is caught by the general luminance
criterion). A pixel *flashes* when it completes a pair of opposing
qualifying transitions within a second. Content fails only when three
things hold at once in some 341×256 window (content viewed at 1024×768):

1. pixels flashing more than 3 times per second (strict profile: 2) cover
   at least a quarter (strict: a fifth) of the window,
2. those pixels flashed *just now* (concurrency — a band sweeping across
   the screen during a pan is not simultaneous flashing), and
3. the window's **mean** luminance is itself flashing at that rate
   (coherence — a dark limb swinging over a bright background flicks
   individual pixels while the region's overall brightness barely moves).

Calibrated against synthetic patterns (tests/): exactly 3 flashes/s passes
WCAG, 4 fails; 15% window area passes, 35% fails; bright-only flicker
passes; jittered multi-frame ramps fail; moving boxes, slow pans over
high-contrast edges and swinging occluders (walking characters) pass; fast
dense scrolling gratings (strobe-equivalent) fail. Validated on real anime:
a 23-minute episode yields a handful of short, plausible flash windows
(lightning strikes, a flash-cut OP montage) instead of blanket coverage.

Extended-flash warnings (sustained sub-threshold flashing; not part of
WCAG) are **advisory**: they appear as gray marks along the top of the
timeline but never create sections or block anything.

The default profile is **exact WCAG**; a stricter profile (tighter
thresholds, 2 flashes/s — extra margin for photosensitive migraine) is
selectable in the header and is used by all checks and verifications.

## Messy real-world files

Stream VODs and clipped videos often have broken timestamps: negative start
times, variable frame rate, audio offset from video, or multi-second pts
jumps that make a 3-second clip claim to be 26 seconds long. Unflash reads
the *real* timeline from the packet index and bridges timestamp anomalies in
both the edited sections **and** the untouched spans between them (reported
as warnings, since bridging a jump makes the export shorter than the
source's nominal duration). Audio and video durations are forced to match
exactly in every part, and every part is written on one shared mp4 timescale
— sections are constant-rate on the fine grid while untouched spans inherit
the source's rate, so their timebases disagree by construction, and the
concat demuxer does not reconcile that: it mis-stamps whatever disagrees
with the first part, collapsing it to a few milliseconds and leaving a hole
where it should have been. Parts rendered before this was pinned are
remuxed (a stream copy) rather than re-rendered. The final concatenation is
sanity-checked for span and gaps. Untouched spans keep the source's exact
frame-to-frame timing throughout, VFR included. Sections work on the
repaired timeline; frame identity is the ordinal within a section, with
per-frame timestamps from ffmpeg's `showinfo` as ground truth.

## Limitations — please read

- **This is risk reduction, not a guarantee.** Passing the detector means
  passing WCAG-style thresholds, not that content is safe for every person.
- Static spatial patterns (fine stripes, gratings) can also trigger
  photosensitive responses and are **not** detected.
- Review flagged sections yourself before sharing; the player dims unedited
  content by default as a courtesy, not a safeguard.
