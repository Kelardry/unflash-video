# Unflash

Unflash finds the flashing in a video that can trigger photosensitive
seizures, and helps you take it out without wrecking the footage. It removes
individual frames and holds a neighbouring frame in their place, so the
picture stays sharp, the audio stays in sync and the running time doesn't
change. It also finds hazardous **stripe patterns** (fine gratings, the
other photosensitive trigger broadcast guidance names) and can soften just
the frames that carry them.

This is the **WebAssembly + WebGPU** implementation: the detector is written
in Rust, the per-pixel work runs as WebGPU compute shaders (or an 8-lane SIMD
kernel where there is no WebGPU), frames come straight out of WebCodecs, and
everything happens in the browser tab with nothing uploaded anywhere.

There is no render–analyze cycle any more. A scan runs as fast as the GPU can
take frames (hundreds to thousands of frames a second at the default analysis
size), the **live monitor** runs the same detector on whatever the player is
showing and meters the flashing as it happens, and once a section is
prepared **every mark you make is re-checked the instant you make it**.

The original Python/ffmpeg tool this was rebuilt from is still in
[`unflash/`](unflash/README.md); its detector is the reference the Rust
port is tested against, bit for bit where the arithmetic allows.

**This reduces risk. It is not a guarantee.** See
[Limitations](#limitations).

## What it looks like

![A scanned test clip: the timeline with what the scan found and a section around it, the sections list, the player, and the flashing charted over the whole video with the numbers under the pointer](web/screenshots/scan.png)

A scanned test clip: what the scan found along the timeline (orange for
general flashes, magenta for red ones), a numbered section around it, and
the flashing charted over the whole video, with the numbers under the
pointer.

| A section: what still fails, and how to fix it | The same section after *fewest removals*: it passes |
|---|---|
| ![A section of the red-flash clip, failing: an extended flash and a red flash listed with their frames and times, the suggestion buttons, and the frame grid](web/screenshots/section.png) | ![The same section passing, 45 frames marked](web/screenshots/fixed.png) |
| **The export, verified** | **The guided tour, on a first visit** |
| ![The export dialog after exporting and verifying: 240 frames re-encoded, passes WCAG](web/screenshots/export.png) | ![The start page dimmed, Open video lit up, and the tour's card beside it: Getting started, 2 of 6](web/screenshots/tour.png) |

`node tests/e2e/screenshots.mjs` makes these again from the test clips.

## Using it

**Hosted:** https://l1n.github.io/unflash-video/ — built and published from
`main` by the [Pages workflow](.github/workflows/pages.yml). It is a static
site: the video never leaves your machine.

**What's new:** [CHANGELOG.md](CHANGELOG.md), in plain words, newest first.
`build.sh` puts a copy beside the page, and the app shows someone coming
back the changes made since their last visit (on the start page, and
behind *What's new* in the header). Each line starts with the time it goes
live, in a comment; add one with every change people will notice. A change
with something on screen to show names its tour in the same comment
(`<!-- 16:00 tour:findings -->`; the tours are in
[`web/tours.js`](web/tours.js)). Every change has a film too, a few
seconds of the app doing what it says, filmed in the app itself (in
Firefox, for the changes about Firefox; a race, the old way above the new,
for the speed-ups this 4-core test machine can show: one it cannot, with
every core already busy either way, gets a film of what changed instead)
and named the same way
(`<!-- 16:00 tour:findings shot:findings -->`): What's new plays it, muted
and looped, while it is in view, and shows its last picture instead to
whoever asks their system for less motion. A film of the section player's
sound has that sound in it, recorded as the player played it, and a
button of its own to hear it. The scenes are in
[`tests/e2e/whatsnew-scenes.mjs`](tests/e2e/whatsnew-scenes.mjs);
`node tests/e2e/whatsnew.mjs NAME` films one into `web/whatsnew/` (VP9,
and its last picture), and scans the film with Unflash under its
strictest profile: one that flashes is not kept. CI checks that every
change has its film and that the files are the ones that were scanned.

**The guided tour:** the first visit gets a tour of the page, a part at a
time, each where it belongs: the start page, then the first video once
its scan is done, then the first section once it is checked. One part of
the page is lit at a time, with a card beside it saying what it is for
(**→**/**←** or Enter to step, Esc to end); the page's own keys wait until
it is over. A tour starts only when nothing else is going on (no job
running, no dialog open, no click or key in the last moment), and each
comes once. After an update, each change with something to see gets a
short tour of its own, the first time you are where it is, and a *show
me* beside it in What's new. **Take the tour**, in the guide, runs the
part for where you are again. With a video open, the guide also lists
every part of the screen (the header's controls, the timeline, the
sections, the chart, the player, and each part of a section's page) with a
line on what it does and a *show me* that lights it up, and the keys.
`?tour=0` turns the tours off; a browser driven by a test gets none unless
it asks with `?tour=1`.

**Locally:** open `web/` from any static web server over `http://localhost`
or `https://` (WebGPU and WebCodecs need a secure context):

```
./build.sh                     # needs Rust + the wasm32 target + wasm-bindgen (see Building)
npx http-server web -p 8765    # or python3 -m http.server -d web 8765
```

then open http://127.0.0.1:8765/.

Browser support:

| | scan / prepare / export | live monitor | detector |
|---|---|---|---|
| Chrome, Edge, Opera 113+ | WebCodecs (H.264, HEVC*, VP9, AV1) | any file the `<video>` element plays | WebGPU |
| Safari 26+ | WebCodecs | yes | WebGPU |
| Firefox 141+ (Windows), 142+ (macOS), other Firefox | WebCodecs where available | yes | WebGPU where enabled (its WebGPU takes no `VideoFrame` or `<video>` as a copy source, so pictures reach it through a canvas), otherwise the SIMD CPU kernel |
| any of these without a decoder for the file's codec (H.264 in Chromium builds without proprietary codecs and some Linux browsers, HEVC in most browsers, VP9, VP8 or AV1 in some) | the **built-in decoders**: H.264 (Constrained Baseline, Main and High, progressive or interlaced), HEVC (Main, Main 10), VP9 (profiles 0 and 2), VP8, AV1 (8 and 10-bit) | no: the player cannot play the file | as above |

\* platform dependent. A file in any of those five codecs is decoded by
Unflash itself when the browser cannot decode it; a file whose codec
neither can decode can still be watched with the live monitor where the
player plays it. So is sound in AC-3, E-AC-3 or DTS (Dolby Digital,
Dolby Digital Plus, DTS: TV recordings, films, Blu-ray rips), which no
browser's WebCodecs decodes: the section player plays it, mixed down to
stereo, and an export whose held frames need silence put into the sound
re-encodes it (in stereo); an export without held frames copies it as it
is (DTS only from an MP4: from an MKV or a transport stream it is
re-encoded). DTS is read as its core, which most DTS-HD streams carry
too: their extensions are passed over, so a 6.1, 7.1, 96 kHz or lossless
track plays as its core (5.1 at 48 kHz at most), and DTS Express or
DTS-HD without a core can't be played. (The whole-video player is the
browser's own, and plays such a video without its sound.) Uncompressed
sound (PCM) is read by Unflash itself, in every form (big and little
endian, 8 to 32 bits, float, G.711): WebCodecs has no name for some, and
Chromium's decoder of 24-bit PCM crashes the page when its sound is
copied out. Of several sound tracks, the first that can be played here
is used (the file's default track first); the video's line and the debug
report say which, and the export keeps that one.

Where a browser has WebGPU but gives the page no graphics adapter
(graphics acceleration turned off in its settings, or WebGPU turned off
for the graphics card or its driver: chrome://gpu says which), Unflash
scans on the CPU instead: the same results, more slowly, and a note says
what to look at.

Files it reads:

- **MP4, MOV, M4V, 3GP** (ISO base media), fragmented files and edit lists
  included. Only the index is read when the file opens. Uncompressed sound
  in QuickTime's forms (`sowt`, `twos`, `in24`, `lpcm`, …) and ISO's
  (`ipcm`, `fpcm`) is read as packets of 2048 frames, and re-encoded for
  an export.
- **MKV and WebM** (Matroska). Matroska keeps no index of its frames, so
  opening one reads through the whole file once (the job bar shows how far;
  the frames' contents are skipped over, not decoded). Video: H.264, HEVC,
  VP9, AV1 and VP8, as the browser decodes them. Audio goes into the MP4
  export as it is when an MP4 can carry it (AAC, MP3, Opus, FLAC, AC-3,
  E-AC-3) and is re-encoded with the browser's own encoder (AAC, else Opus)
  when it can't (Vorbis, PCM, DTS). Subtitle tracks are left out of the
  export, as the original tool leaves them out, and of several audio tracks
  the one in use is kept (see above); the export says so. Live recordings
  (clusters of unknown size), laced audio and header stripping are handled,
  and a damaged stretch is skipped to the next cluster. A browser whose
  `<video>` won't play the MKV (Chrome and Firefox play WebM, and often MKV
  offered as WebM, which Unflash tries) still scans, edits, plays sections
  and exports; only the whole-video view says it can't.
- **MPEG transport streams**: .ts (TV recordings, OBS and ffmpeg
  recordings), .m2ts (Blu-ray) and .mts (AVCHD camcorders). A transport
  stream keeps no index either, so it is read through once when it opens,
  following each stream's packets. Video: H.264 (field pairs as one
  picture, as an MP4 holds them) and HEVC, their decoder setup built from
  the parameter sets in the stream. Sound: AAC (ADTS), AC-3, E-AC-3, MPEG
  audio, DTS and Blu-ray's LPCM. A recording that starts mid-GOP starts at
  its first keyframe, the 33-bit clock is followed across its wrap, and a
  jump of the clock (a recording across a discontinuity) is closed up.
  MPEG-2 video (most TV recordings in standard definition, and DVDs), VC-1,
  AAC in LATM and Dolby TrueHD are not read: the file opens with a note
  saying so. The `<video>` element plays no transport stream, so the
  whole-video view says so; scanning, sections and the export work as
  usual.
- Anything else (AVI, FLV, WMV, MPEG program streams, Ogg) is named when
  it is opened, with how to convert or remux it.

### The short version

1. **Open video** (MP4, MOV, MKV, WebM or a transport stream), or drop one
   anywhere on the page. The file's index is read (an MP4's headers only;
   an MKV or a transport stream is read through once, as it keeps no
   index; nothing is uploaded), the detector starts on
   WebGPU or, failing that, on the CPU, the project is restored from the
   browser's storage if you have opened this file before, and the scan
   starts: every frame is decoded and pushed through the detector, and a
   numbered *section* goes around each problem.
2. **Open a section.** It prepares itself (its frames, plus a run-up and
   run-out, are decoded into memory at analysis resolution) and is checked.
   Under the verdict, whatever still fails is listed with its frames and
   times, in the whole video and into the section (each frame's thumbnail
   has both too: the video's time at the top right, the section's at the
   bottom right); flashing that goes on from before the section, or past its end,
   says so and names the section it is in. Flashing from before the
   section that reaches it only as its first picture comes up is the
   section before's to fix, and is listed as such, outside this verdict.
3. **Edit it.** Mark frames: **R** removes a frame and shows the previous
   one in its place, **F** the next one, **E** holds a frame for a second,
   **B** blends it with the frames either side of it (the flash is toned
   down rather than taken out, see below);
   pressing the same key again takes the mark off, as in the original tool,
   and the keys work with the focus anywhere but a text field. **Ctrl+Z**
   undoes (and **Ctrl+Shift+Z** redoes) any change to the section's marks,
   suggestions included. The section is re-checked after every change, in
   well under a second. Thumbnails come in four sizes (S to XL); **Z**, a
   double-click or *view frame* shows the selected frame at full size,
   decoded from the file (the thumbnails are the detector's small copies,
   too small to read a subtitle on), **←/→** step through the frames and
   the mark keys work on the frame in view. While you step, a new picture
   shows at most every 0.4 s, so stepping through flashing doesn't flash.
4. **Suggest** gives you a first pass: *keep dark* or *keep light* removes
   the frames that make the flashing; *fewest removals* takes out
   whichever of the light or dark frames are fewer and then puts back as
   many flashes as the rules allow (no more than three a second, fewer
   where the profile still objects, each try checked), so as little as
   possible goes; where that still leaves a picture frozen for half a
   second or more (a long run of removed frames), frames come back into it
   at the safe picture rate, spaced from the pictures either side too, so
   it moves at a few pictures a second instead (checked; a stretch inside a
   window that still fails is removed again); *blend frames* removes nothing: it blends those frames
   with the frames around them instead, as little as passes; *reduce FPS*
   thins the section the way
   an editor does it by hand, trying twice the rate that can never fail
   first and stepping down a tenth at a time until the check passes (the ▾
   menu thins to a rate you type). They choose by brightness alone, so
   they can take out a line of burnt-in subtitles or a frame that matters:
   mark such frames **K** (keep) first and every suggestion works around
   them, leaving their own marks (a hold, say) in place.
5. **Watch it.** With a section open, the player plays that section with
   your marks applied, rendered from the source file exactly as the export
   will render it (removed frames showing their stand-in, holds held,
   blended frames blended, softened frames blurred); switch it to
   *original* to compare, or to the
   *whole video*. It plays from the selected frame, loops if asked, runs at
   ½× or ¼×, marks the frame on screen in the grid (a dim bar, moved at most
   every 0.4 s: nothing on the page itself should flicker along with the
   video), and with **live monitor** on shows the check's meter for that
   frame, which falls back slowly rather than following every flash. It starts small
   and dimmed (S, M, L and *dim* above it), and the line above it says what
   is on screen and whether that passes. Its sound is off to start with
   (*sound off* under it): on, it plays at 1×, with held frames silent as
   in the export, and the button says *sound on* only while there is sound
   to be had: *sound at 1× only* while the player is slowed, *no sound*
   where the browser can't decode the video's sound (a note says which
   sound it is), *sound held back* where the browser hasn't started its
   audio (▶ tries again).
6. A stripe pattern can't be removed a frame at a time; tick **soften
   stripes** and the frames that carry it are blurred just enough to take
   it under the threshold, in the check, the player and the export alike.

**Blend frames** (Kel's "get rid of flashing by reducing contrast rather
than removing frames"). What it lowers is the contrast of the flash, from
one frame to the next, not the picture's: turned down towards grey, a
picture would lose nearly all of it before a full-screen flash passed (a
black and white one passes only once its swing in brightness is under a
tenth of the full range). Blending takes out only what changes from frame
to frame, and what stays still stays sharp: the same idea as the old trick
of laying a copy of the footage over itself, a flash later, at half
opacity. A frame marked **B** is mixed with what the frames
either side of it show: the nearest unmarked frame before it and the
nearest after, weighted by where it sits between them. At 100% a run of
blended frames becomes a crossfade between its neighbours, so the flash
is gone but every frame and the timing stay; at less, some of the flash
stays, and so does whatever it shows (a line of subtitles). One **blend**
strength per section (shown once it has B marks; 80% for marks made by
hand) sets how far. *Suggest: blend frames* marks the frames on the
flashing's minority side (the light frames among dark ones, or the other
way round), adds the frames a check still flags if that is not enough at
100%, finds the least strength that passes in 5% steps and sets a little
more (what is left of the flash is at most 80% of what just passes);
removals within its reach make way for it, and frames marked **K** are
never blended. The mix is made on 8-bit sRGB values, the space in which
the detector averages pixels down to its analysis size, so blending the
small copies predicts what blending the full-size frames in the export
shows; the section player and the export blend the decoded frames on a
canvas the same way. Where something moves between the frames, a blended
frame shows it twice, faintly (a ghost), which is the price of keeping the
frame.
7. **Export**: the spans around the marked sections are re-encoded in the
   browser with the edits applied, several at a time; every GOP no marked
   section touches is copied from the source as it is, and so is the audio, unless frames
   are held (**E**): then the sound is re-encoded with a second of silence
   under each held frame (see below). *Save as* names the file (the
   video's name with *.unflashed* to start with); Chrome and Edge then ask
   which folder, and in Firefox, which has no save dialog for pages,
   turning on *Always ask you where to save files* in its settings has
   *Download* ask for the folder (the dialog says so). **Verify** re-scans
   the exported file with the same detector; **verify a saved file…** does
   the same for a file you saved earlier. Anything still failing is listed
   with the section it is in, and a click on the section opens it with the
   frames on screen during it selected (after a held frame an export runs
   later than the video, so the list gives the video's times too).

**Auto-fix** (tick it in the header; off unless you do) does steps 2 to 7
unattended: it softens stripes, tries the fewest removals (which end by
letting frames back into long removed stretches, see below), then keep
dark, then keep light, then reduce FPS on every section, exports,
verifies, and offers **Download fixed video**. A section it can't fix stops the run and is opened for editing.
Treat what it makes as a starting point: it can't tell which frames carry
something that matters, so editing by hand, with the player, gives better
results. Its changes to each section can be undone like any other.

The **Guide** button opens the guide beside your work and closes it again
(so does Esc).

**Alerts** (in the header) sets the finish alert: a beep when a job that ran
for over a minute ends (the time is yours to change), and if you allow it
a system notification while the tab is in the background. Jobs that
follow one another, such as opening a file and its scan, or every stage of
auto-fix, count as one wait and alert once. While a job runs the tab's
title shows how far it has got, and one that ends while you are in another
tab leaves a ✓ (or ✗) there. Work goes on at full speed in a background
tab: nothing in a scan, a prepare or an export waits on a timer, which
browsers slow to once a second in hidden tabs; the decoder, the encoder
and the GPU's readbacks wake it instead.

Profiles (**WCAG + extended flashes + stripe patterns**, **Exact WCAG
only**, **Stricter than WCAG**), the suggesters, the safe frame-rate bound
and the run-up/run-out logic are the reference's; see
[DETECTION.md](DETECTION.md) for what counts as a flash or a pattern and
why the check of a section agrees with a scan of the export.

### Test clips

The hosted site publishes short synthetic videos with known problems, so
there is something to try it on: open one with its **open** button on the
start page, or download it and open it from your disk (or feed it to any
other checker).

| clip | contents | VP9 | H.264 |
|---|---|---|---|
| flash | a slow pan, 4 flashes/s over the whole picture at 3.0–5.5 s, a red flash at 7.0–8.5 s | [flash.mp4](https://l1n.github.io/unflash-video/clips/flash.mp4) | [flash_h264.mp4](https://l1n.github.io/unflash-video/clips/flash_h264.mp4) |
| stripes | the pan, fine vertical stripes at 2–6 s, diagonal stripes at 6–9 s, no flashing | [stripes.mp4](https://l1n.github.io/unflash-video/clips/stripes.mp4) | [stripes_h264.mp4](https://l1n.github.io/unflash-video/clips/stripes_h264.mp4) |
| extended | 3 flashes/s for 8 s: passes WCAG, an extended flash under the default profile | [extended.mp4](https://l1n.github.io/unflash-video/clips/extended.mp4) | [extended_h264.mp4](https://l1n.github.io/unflash-video/clips/extended_h264.mp4) |
| redflash | the pan, then saturated red swapped for a grey of the same luminance 5 times a second from 2 s: no luminance flash, one red-flash failure | [redflash.mp4](https://l1n.github.io/unflash-video/clips/redflash.mp4) | [redflash_h264.mp4](https://l1n.github.io/unflash-video/clips/redflash_h264.mp4) |
| steady | the pan alone | [steady.mp4](https://l1n.github.io/unflash-video/clips/steady.mp4) | [steady_h264.mp4](https://l1n.github.io/unflash-video/clips/steady_h264.mp4) |

All are 640×360, 30 fps, with a tone on the audio track, made by
`tests/media/gen_e2e.py` (the same files the browser test runs on). The
Pages build regenerates them; for a local copy run
`python3 tests/media/gen_e2e.py web/clips`.

`?cpu=1` in the URL forces the CPU detector (for comparison);
`web/bench.html` measures both on your machine.

### Diagnostics

**Debug info**, at the bottom right, puts together what someone helping
with a slow or failing run needs, as text to paste into a message: the
browser, the WebGPU adapter, the detector and how pictures reach it, the
open video (container, codec, size, frame rate, length), each job of the
visit with how long it took (how much of that the tab spent out of sight,
and, for one of more than two seconds, the steps that took most of it:
opening a video reads its index, asks the browser about its decoder,
loads the project kept for it, looks for its export and starts the
detector), the last scan's time per operation and any errors. It copies it to
the clipboard where the browser allows, and saves it as a file. It names
no files.

The console reports, at **debug level** (enable "Debug" / "Verbose" messages
in the devtools console), how long each operation takes: decoder waits, file
reads, `copyTo`, canvas blits, uploads to the detector, the GPU's
submit-to-result latency, polling, the built-in decoder's time per picture.
A summary is printed every 5 s during a scan and at the end of every scan,
section prepare, export and verify; `window.__unflash.profile.summary()`
gives the same text at any moment. The first line of each report names the
route pictures take to the detector.

Pictures reach the GPU detector by the first route that works in the
browser: the `VideoFrame` itself (Chrome, Safari); its own YUV planes
(`copyTo` of I420 / NV12, converted to RGB on the GPU: what Firefox needs,
since its WebGPU takes no `VideoFrame` or `<video>` as a copy source, and
what the built-in decoder hands over directly); WebCodecs' RGBA conversion;
a canvas blit; or canvas pixels. `?route=videoframe|yuv|rgba|canvas|pixels`
forces one, `?extsrc=canvas` (or `none`) pretends WebGPU accepts only those
copy sources, `?workers=N` sets the number of built-in decoder workers,
`?auto=0` keeps anything from starting when a file is opened (not even the
scan), `?auto=1` ticks auto-fix, and
`?monitor=detect` makes the live monitor run the detector on the player
even when a finished scan of the file could be read instead.

## How it works

```
   WebCodecs VideoFrame ─┐            ┌── GridStats (a few KB / frame) ──┐
   <video> element ──────┼─► GPU ─────┤                                  ├─► temporal stage ─► violations, sections
   cached RGBA frames ───┘   stage    └── per-pixel state stays on the GPU ┘   (Rust, on the CPU)
```

Flashes are judged by WCAG 2.2's definitions, the red flash included (a
change of more than 0.2 in CIE 1976 u′v′ to or from a saturated red); see
[DETECTION.md](DETECTION.md#which-wcag) for how that differs from WCAG 2.0
and from the original tool. The detector is split in two.

The **pixel stage** owns the per-pixel state machine of the reference
(`_ExtremaTracker`, `_FlashCounter`, `_Pool`): a monotonic-run tracker for
luminance and one for the colour's distance from red (carrying the colour's
chromaticity at the run's two ends), flash pairing, a ring of the last K
flash times and their opening times, and the pooling timers, about 130 bytes
per pixel. It is written once, as a plain per-pixel function in Rust
(`crates/unflash-core/src/pixel.rs`), and restated twice: as a WGSL compute
shader and as an 8-lane SIMD kernel. All three are held bit-for-bit
identical by tests. Per frame the stage reads and partially writes that
record for every analysis pixel and reduces the frame to one 48-byte cell
per sliding-window position: window sums of luminance and distance from red, the
count of pixels in each of eight mask classes (strobing at the failure rate,
strobing at the permitted rate, pooled transitions), and the age of the
oldest transition still feeding a failure window. That is the whole
bandwidth story: **one pass over the pixel state per frame**, no
intermediate images, nothing per pixel read back.

The GPU version runs five dispatches per frame — an ingest pass that area-
averages the source (any size, straight from a `VideoFrame`) into the
analysis model and linearises it through the same sRGB table the CPU uses,
a pass counting the pixels that moved since the last new picture, the
update pass, a row pass and a gather pass, with one more converting a
picture handed over as YUV planes to RGB first — and copies the
few-kilobyte result into one of a ring of staging buffers, so several
frames are in flight while the CPU handles the rest. The reduction passes
use no workgroup barriers: on a real GPU they are latency-bound and take
tens of microseconds; on a software implementation (SwiftShader, lavapipe)
they are merely slow rather than pathological.

The **pattern stage** (`crates/unflash-core/src/pattern.rs`, and a sixth
dispatch on the GPU) looks for stationary hazards the flash detector cannot
see: regular stripes and gratings. It walks the luminance plane along
parallel lines in eight orientations with the same monotonic-run tracker,
and marks a pixel when it lies in a stretch of more than five regularly
spaced light–dark pairs of flash-strength contrast that is coherent across
neighbouring lines. The frame's pattern area is the number of marked
pixels; a quarter of the screen for half a second is a violation of kind
*pattern*, with its own sections. The GPU and CPU versions produce the
identical mask (it is integer and fixed-point throughout), and the mean
stripe spacing they measure sizes the blur that **soften stripes** applies.

### The built-in H.264 decoder

WebCodecs is only as good as the codecs the browser ships, and H.264, the
codec of nearly every camera and phone, is missing from Chromium builds
without proprietary codecs and from some Linux browsers. So
`crates/unflash-h264` is a complete H.264 decoder in plain Rust, used
whenever `VideoDecoder.isConfigSupported` says no to an `avc1`/`avc3`
track: the Constrained Baseline, Baseline (without FMO/ASO), Main and High
profiles for 4:2:0 8-bit video, progressive or interlaced (field pictures
and MBAFF frames), with CAVLC and CABAC, I/P/B slices and every partition
size, multiple and long-term references, memory management control
operations, explicit and implicit weighted prediction, spatial and temporal
direct prediction, the 8x8 transform, scaling matrices, I_PCM and the
deblocking filter. 4:2:2/4:4:4, high bit depths, slice groups, SP/SI
slices and data partitioning are reported as unsupported rather than
decoded wrongly. Besides the x264 streams below it is checked against the
JVT conformance suite (`cargo run --release -p unflash-h264 --example
conformance -- <dir>` over the streams from
https://fate-suite.ffmpeg.org/h264-conformance/ with ffmpeg's per-frame
MD5s): every stream within those limits decodes bit-exact, 169 of them.

It is written to the standard and tested bit-exact against ffmpeg's
decoder on x264 streams that exercise those tools
(`crates/unflash-h264/tests`, media in `tests/media/h264`). Pictures come
back in decode order with the container's timestamps; `web/media.js`
re-orders them for presentation, so the rest of the app (scan, sections,
export, verify) does not know which decoder it is on. Scans and section
prepares take the pictures as I420 planes straight into the detector (no
`VideoFrame` in between); the export, which re-encodes them, gets real
`VideoFrame`s.

### The built-in HEVC, VP9, VP8 and AV1 decoders

HEVC is the codec browsers most often lack (Firefox has none, Chrome only
where the operating system lends one), and VP9, VP8 and AV1 are missing
here and there too. So the app has a decoder of its own for each, in a
WebAssembly module of their own (`crates/unflash-decoders`, built into
`web/pkg-dec`, about 2 MB) that is loaded only when a file needs one; H.264's
stays in the main module. Each runs in the decode workers the way H.264's
does (`web/softworker.js`, a group of pictures per worker, pictures made
the detector's size there), so everything above holds for them: the
fallback when the browser cannot decode a file, hybrid scans next to the
browser's own decoder, the export. `?builtin=1` makes the app use the
built-in decoder even where the browser has one.

- **HEVC** (`crates/unflash-hevc`): Main, Main 10 and Main Still Picture,
  4:2:0 and 4:0:0 at 8 to 12 bits, every tool of those profiles (tiles and
  wavefronts decoded serially, dependent slices, AMP, TMVP, weighted
  prediction, scaling lists, lossless and PCM, SAO). Bit-exact with ffmpeg
  on the 177 JCT-VC conformance streams within those profiles and on the
  x265 streams in `tests/media/hevc`; about 53 frames a second at 1080p on
  one core, natively. Pictures come out in decoding order; the workers put
  them in presentation order with the reordering the sequence declares.
- **VP9** (`crates/unflash-vp9`): profiles 0 and 2 (8, 10 and 12-bit
  4:2:0), every tool including superframes, `show_existing_frame` and
  frame size changes from scaled references. Bit-exact on all 306 libvpx
  test vectors of those profiles, natively and in WebAssembly; 1080p at
  about 90 frames a second in WebAssembly.
- **VP8** (`crates/unflash-vp8`): all of RFC 6386. Bit-exact on the 62
  libvpx test vectors; about 90% of ffmpeg's single-thread speed.
- **AV1** (`crates/unflash-av1`): rav1d, the Rust port of dav1d, vendored
  in `third_party/rav1d` without its assembly and patched to build for
  WebAssembly, behind a small wrapper: 8 and 10-bit 4:2:0 (film grain
  applied, as the browsers apply it), bit-exact with ffmpeg's libdav1d.

Deeper pictures are rounded to 8 bits, which is what the detector reads.
Every decoder conceals what it cannot decode and marks the picture
damaged; none panics on the damaged, truncated and shuffled streams the
fuzzing fed them. `tests/e2e/decoders.mjs` scans the flash clip in each
codec with its built-in decoder and requires what the browser's own
decoder finds, and a hybrid scan of the VP9 clip, the built-in decoder
beside the browser's, exactly what the browser's decoder gives alone.

The decoding runs in parallel Web Workers (`web/h264pool.js`, one group of
pictures per worker, split at sync samples), with eight-lane SIMD row
kernels (wasm simd128, SSE2 or NEON through `wide`) for the interpolation,
averaging and weighting of blocks at least eight samples wide, and for the
deblocking filter: eight lines of an edge at a time in 16-bit lanes, every
decision a lane mask, a vertical edge's lines transposed into lanes and
back (the line-at-a-time filter stays as the reference a randomised test
checks it against). A full
reconstruction is needed (H.264 predicts every macroblock from its
neighbours and from earlier pictures, so there is no DC-only or
low-resolution shortcut as for MPEG-2). The decoder has a **fast mode**
that leaves out the in-loop deblocking filter (about a fifth of the
decoding time), but nothing that gives a verdict uses it any more: the filter only
touches block edges, yet later pictures are predicted from the unfiltered
ones, so the difference grows through each GOP. On a 1080p clip at CRF 26
with 10 s GOPs, the means of 8×8 luma blocks (about the detector's cells at
1080p) were off by 0.4 codes on average, but by up to 11.6 codes by the end
of a GOP (99th percentile 4.0): about 4.5 % of full luminance, against a
flash threshold of 10 %. Natively the
decoder does about 65 fps at 1080p (80 fast); in WebAssembly about 50 fps
single-threaded (60 fast) and 125 fps with four workers, and 400–550 fps
at 640×360: well above real time for the analysis but slower than a
hardware decoder. (Those were measured before the work in the next
paragraph, which made it about 15 % faster natively and 20 % in
WebAssembly, the two builds timed side by side.) The player itself still
cannot play such a file, so the live monitor is off for it.

Where the rest of the time goes, measured with cachegrind (instruction,
branch and cache simulation) on 48 frames of 1080p at CRF 20: about a
fifth is CABAC's coefficient loop, one arithmetic-decoded bin after
another. Its engine keeps codIRange, the offset and the bit count in
machine registers through a block (through `&mut self`, each bin stored
them and the next loaded them back), and a bin takes no branch on its own
value; what remains are the significance map's branches on whether a
coefficient is there, which are the data itself (ffmpeg's decoder has
them too). The other syntax elements share one out-of-line bin decoder
instead of a copy inlined at each of the ~55 places a bin is read, the
luma interpolation filters are out-of-line primitives with one copy per
block width (inlined into every case they made 41 KB of code), and a
macroblock's four neighbours are found once instead of at every lookup:
the macroblock layer's hot code was just over a 32 KB instruction cache
(over the first 16 frames, 16.4 M simulated misses at 32 KB but 3.7 M at
48 KB; now 10.4 M and 1.7 M). Together with the
deblocking kernels (and the boundary strengths' coefficient test done on
all of an edge's segments at once) this took the decoder from 9.53 G to
8.17 G instructions, from 51.5 M to 38.5 M mispredicted branches and from
60.4 M to 39.5 M instruction-cache misses on that clip.

The **temporal stage** (`crates/unflash-core/src/temporal.rs`) is the rest
of the reference `FlashDetector`, unchanged in logic: the window-mean
coherence gate, the concurrent-area test, events, per-frame statistics,
violations, extended flashes. It runs on the CPU in float64 on a few hundred
numbers per frame, whichever pixel stage produced them.

### Time on the GPU

The reference keeps every per-pixel time in float64 because a float32 loses
the millisecond precision the 0.125 s and 1 s windows need after an hour of
video. GPUs have no float64, so the kernels keep time as **unsigned 32-bit
microseconds**: integer subtraction is exact, an age measured at second 4259
is the same number it would be at second 4, and wrap-around is handled by
computing ages with wrapping arithmetic and periodically saturating every
stored time at 2^30 µs (about 18 minutes), which is the reference's "never"
sentinel in a different coat. See [DETECTION.md](DETECTION.md#the-webgpu-implementation).

### Frames in batches, files in segments

At 256×144 the detector's work on a frame is a few tens of microseconds of
GPU time; a scan's speed is set by round trips. The stage therefore runs
**sixteen frames per command buffer**: each picture is converted into its
own slice of the input planes as it arrives, and the passes that depend on
the previous frame's state (the moved-pixel count, the pattern mask, the
update, the row sums and the gather) run for the whole batch in one
submission with one readback, so the submit-to-result latency is paid once
per sixteen frames rather than once per frame. Up to **32 batches are in
flight** at a time, so that the GPU always has the next one: a scan's pace
is at most the frames in flight divided by the round trip, and Firefox's
GPU runs in another process, about 300 ms from submission to result, which
two batches held to about a hundred frames a second; 32 allow 1700, what
a scan's one detector needs to keep up with all its decoders (a batch in
flight costs only its readback buffer, about 50 KB). The live monitor asks
for a batch of one, since it wants a result after every frame.

A long file is scanned **in chunks** (see *Both decoders at once* below):
several decoders at once, one detector taking their pictures in file
order. It used to be cut into up to four **segments scanned at the same
time**, each with its own decoder and detector, every segment after the
first starting a run-up early (the same run-up a section check gets) so
that its detector's state at the seam would be the state a run from the
start of the file reaches, and the per-frame statistics joined with the
run-ups dropped. That holds for most frames, not all: the detector's state
remembers more than any run-up. A pixel's flashes pair opposite changes
less than a second apart, so a chain of them decides which changes pair
from where it began, however long ago, and a pixel still since some change
long before keeps where that change left its monotonic run (the phase of
the two-second run cap, and which way it last moved), which a fresh
detector does not know. At a seam in the middle of flashing that moved a
violation's edge by four frames in the browser test. The segments remain
behind `?chunked=0` (with `?segments=N` to force a count); a file shorter
than four run-ups per segment is scanned in one.

Preparing a section works the same way without the run-ups: the range is
cut at keyframes into as many spans as a scan would use, each decoded by
its own decoder into its own detector, and the cached pictures joined in
order. A picture's capture and its pattern figures depend on that picture
alone, so the join is exact (a test holds a three-span prepare identical,
byte for byte, to one pass), and every span after the first starts at its
keyframe with nothing to decode before its first frame.

Pictures reach the GPU by the cheapest route the browser allows: the
decoded frame itself where WebGPU takes one (Chrome), else its own YUV
planes, else, for a decoder that hands out RGB (Firefox on a Mac gives
BGRX), its pixels copied as they are, the GPU swapping the channels while
it reads them. Where WebGPU takes no frame (Firefox), that copy is most
of what the page does per frame, so scans, prepares and verifications
decode in **Web Workers** instead, one per segment or span: each worker
decodes with WebCodecs, copies each picture out (its planes, or its RGB
pixels) straight into the WebAssembly module's memory and makes it the
detector's size there (`resample::Shrink`: the boxes of the GPU's ingest
pass, summed exactly in integers, rows first so the sums vectorise, YUV
converted a row at a time with the GPU's coefficients in 16.16 fixed
point, which agree with its floating point but for rounding at exact
halves), and the page gets 128 KB a frame instead of the whole picture.
(The CPU detector makes a picture it is handed whole small the same way,
so what it finds does not depend on where pictures were made small.) The
YUV conversion is written for the vector unit: each chroma sample's terms
are worked out once for the two rows that share them, each row goes to
three planes (R, G, B) sixteen samples at a time in WebAssembly SIMD (a
shuffle spreads each chroma term over its two samples, and the saturating
narrowing from 32 to 8 bits is the clamp), and each average is divided by
a multiply and a shift instead of a division (`Divider`). A 1920×960
picture takes 3.7 ms in WebAssembly, 10 before; `tests/e2e/shrink.mjs`
holds the WebAssembly build to a plain JavaScript statement of the
arithmetic, value for value. That upload was most of a
scan in Firefox, where it also crosses to a separate GPU process: an hour
of 1920×960 took 172 s of a 269 s scan uploading 7 MB pictures. The
browser test prepares a section both ways and requires the same cached
pictures (they are identical there). A worker keeps up to 32 small
pictures waiting (four whole ones), so it decodes on while the page is
busy. The built-in decoder's workers do the same for scans, straight from
the decoder's own picture buffers (the full-size picture never leaves the
worker's memory), and may each hold a long GOP's worth of small pictures:
the page takes the groups of pictures in order, and a worker made to stop
a few pictures into a later group left the pool little faster than one
worker. The export and the players, which need real frames, decode on
the page. `?decodeworkers=0` / `=1` overrides the choice, and `?shrink=0`
hands the pictures over whole.

### Both decoders at once, one detector in order

A file of two chunks or more is scanned **in chunks**: cut at keyframes
into chunks of about 10 seconds, decoded by several **lanes** at once, as
many of the browser's decoder (each in a decode worker) as the segments
would have used, and each picture made the detector's size where it is
decoded. The browser's decoder (usually hardware) has a speed of its own
and leaves the processor's cores mostly idle, so for H.264, where the app
has a decoder of its own, a scan of a file of two minutes or more on a
machine with six cores or more is also **hybrid**: two to four lanes of the
browser's decoder and one of the built-in decoder, in a worker for each
core left over (two stay for the page and the browser). Where the
browser's pictures have to be copied out of its decoder (Firefox), the
balance tips the other way: each picture comes out whole (8 MB at 1080p,
some 25 ms), through the process that draws every tab, so a lane of the
browser's decoder makes some 17 fps of 1080p on a core and slows the
other tabs, where a built-in worker makes some 40. There H.264 gets one
lane of the browser's decoder, and the built-in decoder a worker for each
core left over, less three to seven (by the number of cores) for the
page, the browser and the rest of the computer: up to twelve workers at
1080p, some 32 MB each, and eight above it.

However many lanes decode, **one detector takes the pictures in file
order**, so a scan in chunks gives exactly what a scan in one piece gives,
whichever lane decoded which chunk and in whatever order, and no chunk
needs a run-up. Each chunk's pictures wait in a slot of their own until
the detector gets to them (about 150 KB each at 256×144). A `ChunkPicker`
decides what each lane decodes next: the chunk the detector is on if
nobody has it, else the next chunk nobody has, as long as the pictures
held stay within a budget (96 MB for each GB of memory the browser reports,
384 to 768 MB, as browsers report 8 GB at most; 512 MB where it reports
none; `?hold=MB` sets it). A lane
with no room waits for the detector to free some, and the detector's own
chunk never waits, so the lanes cannot all stop. On a real GPU the
detector is far faster than the decoders (a few tens of microseconds of
GPU time a frame), so it keeps up with all of them. The built-in decoder is asked for its
next chunk only when a worker is free for it, so its workers never wait at
a seam. It decodes fully here (deblocking filter and all), so its pictures
are the browser's, and each picture it sends says whether it is damaged:
at the first damaged one it stops, and its chunk goes to the browser's
decoder, which goes on from the last picture it gave.

**Rebalancing and take-overs.** How fast each lane goes differs from one
computer to the next (in Firefox on one Windows PC a lane of Firefox's
decoder made 20 fps of 1080p where a built-in worker made 47; a hardware
decoder with cheap copies can tip it the other way), so a scan does not
go by its plan alone: it times each lane as it goes (the pictures it decodes per
second of decoding). Until every lane has been timed the chunks go out in
turn. After that, each free chunk goes, in order, to the lane that would
finish it first, counting what each lane has yet to finish: a lane asking
for work takes the first chunk that falls to it within the budget's
reach, and one that would finish none of them sooner than the others is
**set aside** instead of holding the detector up (and asked again
whenever something changes). A lane of the browser's decoder set aside
gives its core to the built-in decoder, which starts another worker (up to
one for each lane of the plan; the worker joins the decode under way).
A chunk handed out before the lanes were timed can still hold the
detector up: when it waits on a chunk a slower lane is decoding and a
faster lane is idle, the faster one **takes it over** from the keyframe
at or before the last picture in, and the slow one stops. In a model of
Firefox on such a PC here (four lanes of the browser's decoder slowed to
a few pictures a second beside two built-in workers, two minutes of
1080p60 H.264), take-overs alone scanned at 61 fps, and rebalancing, which
set the four aside and grew the built-in decoder to six workers, at 103;
in stock Firefox on this machine's four cores, its two lanes slowed the
same way, 20 s of 1080p H.264 took 9.4 s with take-overs alone and 7.1 s
rebalanced (the built-in decoder grown from two workers to four).
`?rebalance=0` hands the chunks out in turn, `?steal=0` turns take-overs
off.

**Early looks.** Before any of it is decoded, `triage.js` scores each chunk
from the file's index alone for how likely it is to flash (bytes per
frame against the film's median, keyframes a second, frames that cost
three times the chunk's upper quartile, and near-empty keyframes: a
white, black or faded picture), and the likeliest quarter are hot. A lane
not needed for the chunk the detector is on, or a lone lane at the start,
takes an **early look** at the likeliest hot chunk still far enough ahead:
it decodes the chunks before it as a run-up (6.5 s), the hot chunk and
the hot chunks straight after it, and runs them through a detector of
its own. What that finds is shown on the timeline, dashed, long before the
scan gets there; the pictures wait for the scan's detector like any
others, so nothing is decoded twice and the result is the same. Early
looks use at most 60% of the budget. As the scan goes, the timeline
shows what it has found so far, exactly up to where it has got and the
early looks' findings after that.

The debug report shows how many chunks and frames each decoder decoded,
how fast, how many early looks it took, which lanes were set aside and
for how long, how far the built-in decoder grew, and the most pictures
held, for
the last scan and for the last check of an export (a scan of the exported
file), and how far apart the video's keyframes are.
`?hybrid=0` turns the built-in decoder off, `?hybrid=1` on for any file,
`?hybrid=H,S` sets H browser lanes and S built-in workers, `?chunk=S` the
chunks' length, `?order=file` leaves out the early looks and `?chunked=0`
scans in segments as before. The browser test, whose Chromium has no
H.264, runs hybrid scans with the built-in decoder standing in for the
browser's (`?hybrid=sim:H,S`) and requires exactly the violations of the
scan in one piece: with early looks, when the built-in decoder fails at
its tenth picture (`?hybridfail=1`), with room for one chunk held
(`?hold=5`), on one lane with early looks, and with four lanes slowed
down (`?slowlanes=MS`), taken over and rebalanced; the Firefox test does
the last two with Firefox's own lanes.

### Spans re-encoded, the rest copied

An export used to decode and re-encode every frame of the file. Almost all
of them are frames no section touches, and those are now **copied from the
source as they are**, whole GOPs at a time, with neither a decoder nor an
encoder in the way: a smart cut. Only the spans around the sections with
marks are decoded, edited and re-encoded (a section left as it was is
copied like the rest), from the last IDR picture before a section
to the first one after it (for H.264 the sync samples are read to make sure
they are IDR pictures, since an open-GOP I picture is marked as a sync
sample too but the B pictures after it lean on what came before). Each span
starts with a keyframe the decoder can pick up cold, so the spans are
independent: the export runs several at once (one per two logical cores,
up to four; `?parallel=N` sets the count), each with its own decoder and
encoder, and long spans are cut at keyframes between sections so the
workers stay busy. The writer takes the pieces in file order.

Copied and re-encoded samples share one track. For VP9 that is nothing
special: the frames carry their own headers. For H.264 the track's `avcC`
record has to hold the parameter sets of both streams, and both number
theirs from zero, so the encoder's are **renumbered**: the ids in its SPS
and PPS, and the `pic_parameter_set_id` in every slice header, which moves
the bits after it. CABAC slice data is aligned to its byte boundary again;
CAVLC data is shifted, and for it the new id is chosen so that the shift is
a whole byte, because I_PCM samples in CAVLC slices are aligned to the NAL
unit's bytes. Tests decode every conformance stream after renumbering and
splice GOPs of one encoding into another with B-frames on both sides, in
this decoder and in ffmpeg's. The audio is copied as before.

When the encoder's codec cannot share a track with the source's (an HEVC
or AV1 source, or an H.264 file exported as VP9 because the browser has no
H.264 encoder), or with `?smartcut=0`, the whole file is re-encoded, still
in parallel pieces cut at keyframes. The export dialog says which it will
be, and how much is copied; it offers one choice per format, each with a
line on where it plays and whether the parts you didn't edit are copied.

Firefox's H.264 encoder on Windows writes a damaged record: its avcC writer
puts a NAL header byte in front of parameter sets that already start with
one, so every SPS and PPS reads one byte off (`67 67 64 00 1e ...`). Such
records are repaired when they are read (the byte after an SPS header is
profile_idc, and 0x67 is no profile), and parameter sets an encoder repeats
inside its samples are taken in too, under the ids the track gave them. An
encoder that gives no record at all (WebCodecs' way of saying its stream is
Annex B) has one made from its first keyframe's parameter sets, and its
start codes are turned into the lengths MP4 wants.
Should an encoder's stream still be impossible to join, the export is not
lost: it is redone as a plain re-encode through one encoder, whose stream
needs no joining, and says so; the console then holds the record's bytes
for a bug report.

### What it costs

Per frame at the default analysis size (a 16:9 source becomes 256×144, the
model of a 1024×768 screen at scale 0.25; `analysis_scale = 1.0` gives the
full 1024×576):

| stage | per frame | on this machine (4-core VM, software GPU) |
|---|---|---|
| CPU scalar kernel | 27 ns/px | 1.0 ms, ≈1000 fps |
| CPU SIMD kernel (AVX2 natively, simd128 in WASM) | 16 ns/px | 0.6 ms native, 1.0 ms in the browser |
| GPU stage | ≈130 B/px of state traffic | limited by the GPU's memory bandwidth on real hardware |

At full analysis scale the state is 71 MB and the CPU kernel is memory-bound
at ≈5 GB/s; that is where the GPU pays for itself, at a few hundred GB/s
on a discrete card. The whole-video scan is then bounded by the decoder.

`cargo run --release -p unflash-gpu --example bench` prints these numbers
for your hardware; `web/bench.html` does the same in the browser.

## Building

```
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.128   # must match the crate version in Cargo.lock
./build.sh            # -> web/pkg/
```

`wasm-opt` (binaryen) is used if present. The workspace:

| crate | what |
|---|---|
| `crates/unflash-core` | the detector: config and profiles, the per-pixel kernel (scalar and SIMD), grid reduction, temporal stage, violations, sections, editing helpers, the blend (blend frames). No I/O. |
| `crates/unflash-gpu` | the WGSL pipeline on `wgpu` (native backends and the browser's WebGPU) |
| `crates/unflash-mp4` | byte-range demuxers for WebCodecs, MP4 (fragmented files, edit lists, PCM) and Matroska / WebM (lacing, unknown sizes, header stripping) and MPEG transport streams (samples as an MP4 holds them, found again in the stream's packets by `web/ts.js`), giving codec strings, decoder descriptions and sample tables; MP4 sample entries for Matroska and transport stream audio; a muxer for the export |
| `crates/unflash-h264` | the built-in H.264 decoder, for browsers whose WebCodecs has none |
| `crates/unflash-hevc` | the built-in HEVC decoder (Main, Main 10; 4:2:0, 4:0:0) |
| `crates/unflash-vp9` | the built-in VP9 decoder (profiles 0 and 2) |
| `crates/unflash-vp8` | the built-in VP8 decoder |
| `crates/unflash-sound` | the built-in sound decoders: AC-3 and E-AC-3 (Dolby Digital, Dolby Digital Plus), written to ATSC A/52, and DTS (the core of DTS Coherent Acoustics, which DTS-HD streams carry too), written to ETSI TS 102 114 |
| `crates/unflash-decoders` | the `wasm-bindgen` API of the built-in HEVC, VP9, VP8 and AV1 decoders and of the AC-3 / E-AC-3 and DTS sound decoders: a module of its own, loaded when a file needs one |
| `crates/unflash-av1` | AV1 decoding (8- and 10-bit, film grain applied) into 8-bit 4:2:0 pictures: a small wrapper over rav1d, bit-exact with ffmpeg's libdav1d |
| `third_party/rav1d` | rav1d 1.1.0, the Rust port of dav1d (BSD-2-Clause), without its assembly and patched to build for wasm32 (see its `UNFLASH.md`) |
| `crates/unflash-wasm` | the `wasm-bindgen` API |
| `web/` | the app (plain ES modules, no build step beyond the WASM); `site.mjs` makes the published site from it |
| `unflash/` | the Python reference implementation |

## Deploying

`.github/workflows/pages.yml` builds the WASM on every push to `main` and
publishes the site made from `web/` to GitHub Pages; it can also be run by
hand from any branch (Actions → Pages → Run workflow). The first run enables
Pages with the "GitHub Actions" source; if the repository refuses that,
enable it once under Settings → Pages → Build and deployment → Source:
GitHub Actions.

[`site.mjs`](site.mjs) makes the site: each build's code (the modules, the
style sheet, the WebAssembly) goes in a folder of its own, `v/<build>/`,
which `index.html` loads, and the data (test clips, What's new's films, the
changelog) stay beside `index.html`. So a page only ever loads the files of
the build it started with: nothing of one build in the browser's cache can
meet another build's files, and a page left open over an update goes on
loading its own build's files when it next needs one (a decoder, a worker).
The builds replaced in the last three days stay published beside the new
one, for the pages still open on them: the workflow copies them from the
site as it stands, which lists its builds and their files' hashes in
`versions.json`. That file also names the current build, and a page on an
older one says so and offers *new version: reload* in the header; the debug
report names the build. `node site.mjs OUT --build ID [--keep-from URL|DIR]`
makes one by hand (after `./build.sh`); `tests/e2e/site.mjs` tests it.
`.github/workflows/ci.yml` runs the Rust tests (on lavapipe), builds the
WASM once and runs the browser tests on it in parts side by side, on every
push.

## Testing

```
cargo test --workspace                    # unit tests, the reference cross-check, GPU-vs-CPU (needs any Vulkan/Metal/DX12 adapter; lavapipe is enough)
python3 tests/gen_fixtures.py             # regenerate the reference fixtures from unflash/analysis.py (needs numpy)
bash tests/media/gen.sh                   # demuxer/muxer test files: MP4, MKV, PCM, transport streams and ffmpeg's MP4 of each (needs ffmpeg)
bash tests/media/h264/gen.sh              # H.264 decoder test streams and ffmpeg's per-frame MD5s (needs ffmpeg with libx264)
cargo run --release -p unflash-h264 --example compare -- file.mp4   # decode any MP4 and diff every frame against ffmpeg
cargo run --release -p unflash-h264 --example conformance -- dir [filter]   # the JVT conformance streams (Annex B) against ffmpeg's framemd5 (dir/NAME.framemd5)
bash tests/media/hevc/gen.sh              # HEVC decoder test streams and ffmpeg's per-frame MD5s (needs ffmpeg with libx265)
cargo run --release -p unflash-hevc --example conformance -- dir   # the JCT-VC conformance streams against ffmpeg's framemd5
bash tests/media/vp9/gen.sh               # VP9 decoder test streams and ffmpeg's per-frame MD5s (needs ffmpeg with libvpx)
cargo run --release -p unflash-vp9 --example conformance -- dir    # the libvpx VP9 test vectors (with their .md5 files)
bash tests/media/vp8/gen.sh               # VP8 decoder test streams and ffmpeg's per-frame MD5s (needs ffmpeg with libvpx)
cargo run --release -p unflash-vp8 --example conformance -- dir    # the libvpx VP8 test vectors
bash tests/media/av1/gen.sh               # AV1 decoder test streams and ffmpeg's per-frame MD5s (needs ffmpeg with libaom, libsvtav1, librav1e, libdav1d)
cargo run --release -p unflash-av1 --example compare -- file.mkv   # decode an AV1 track, diff every picture against ffmpeg's libdav1d, time it
bash tests/media/ac3/gen.sh               # AC-3 / E-AC-3 decoder test streams, from ffmpeg's encoders (the tests compare with ffmpeg's decoder when it is installed)
cargo run --release -p unflash-sound --example fate -- ac3 dir   # AC-3 and E-AC-3 files (ffmpeg's FATE samples) against ffmpeg's decode
bash tests/media/dts/gen.sh               # DTS decoder test streams, from ffmpeg's encoder (the tests compare with ffmpeg's decoder when it is installed)
cargo run --release -p unflash-sound --example fate -- dts dir   # DTS and DTS-HD files (ffmpeg's FATE samples) against ffmpeg's decode of their core
python3 tests/media/gen_e2e.py            # synthetic flashing / striped videos for the browser test (and the site's test clips)
node tests/e2e/run.mjs                    # the whole app in headless Chromium with WebGPU (needs playwright)
node tests/e2e/tour.mjs                   # the guided tour, on a first visit and after an update
node tests/e2e/busy.mjs                   # the page while a job runs: a second start turned away, never a long freeze
node tests/e2e/streams.mjs                # transport streams read as ffmpeg reads them, scanned and exported; PCM and DTS sound; the sound track that plays
FIREFOX=/path/to/firefox node tests/e2e/firefox.mjs   # the app in headless Firefox, WebGPU on lavapipe (needs puppeteer-core)
node tests/e2e/screenshots.mjs            # the screenshots above, made again from the test clips (not a test)
FIREFOX=/path/to/firefox node tests/e2e/whatsnew.mjs [NAME...]   # What's new's films, made again and scanned for flashing (not a test)
node tests/e2e/whatsnew-check.mjs         # every change has its film, and each is the file that was scanned
node tests/e2e/site.mjs                   # the site as published: each build's code in its own folder, old builds kept for open pages
```

`crates/unflash-core/tests/reference_fixtures.rs` regenerates the frames the
Python detector was run on (CRC-checked) and asserts identical per-frame
hazard areas, events, violations and verdicts on all fixtures.
`crates/unflash-gpu/tests/gpu_vs_cpu.rs` compares the entire per-pixel state
of the GPU stage with the CPU kernel after every frame, and the pattern mask
and statistics on striped frames. `crates/unflash-h264/tests/streams.rs`
decodes the x264 test streams and requires ffmpeg's MD5 of every frame.
`crates/unflash-h264/tests/rewrite.rs` renumbers the parameter sets of
every conformance stream and decodes it again, and splices GOPs of one
encoding into another and checks the result in this decoder and in ffmpeg.
`tests/e2e/run.mjs` scans, edits, blends, softens, exports and verifies the
synthetic clips in headless Chromium, including the H.264 clip through the
built-in decoder (the test browser has no H.264); the VP9 exports copy
their untouched GOPs. `tests/e2e/splice.mjs` runs the H.264 smart cut with
a stand-in encoder that hands back a second encoding's samples, and
requires every frame of the exported file to decode as its source did.
`tests/e2e/tour.mjs` walks the guided tour as a first visitor and as one
back after an update: every step lights something on screen with its card
in view beside it, each part waits until its place is on screen and
nothing is running, comes once, and holds back the page's own keys; and
with a section open, every part of the screen the guide lists lights up
from its *show me* (but the switches a section shows only when it needs
them).
`tests/e2e/busy.mjs` clicks Scan and opens another video while a scan
runs (both turned away, the scan finishes whole), and watches the page's
long tasks during a chunked scan whose detector is slowed down, so that
three decode lanes get ahead of it: none may reach 250 ms (the old
detector loop kept the page for over a second). `tests/e2e/firefox.mjs`
runs the app in stock Firefox (headless, WebGPU on lavapipe, driven over
WebDriver BiDi), where the pictures reach WebGPU through the decode
workers: a scan there must find exactly what the CPU detector finds, the
page must answer through it (the longest gap between 10 ms timer ticks,
Firefox having no long-task API) and turn a second Scan click away, a
section must prepare, check and pass after *fewest removals*, and in a
scan in chunks with Firefox's lanes slowed down, the built-in decoder must
take their chunks over and change nothing in the result. CI runs it on a
pinned Firefox release.

To compare the two detectors on a real file rather than on synthetic
frames, run both over it and line up the violations:

```
python3 -m unflash.cli analyze file.mp4 --profile wcag_ext --json   # the Python reference (needs ffmpeg, numpy)
node tests/e2e/scanfile.mjs file.mp4 --profile wcag_ext             # this detector, in headless Chromium (--segments N to force a segment count)
```

Onsets, starts and ends agree to the hundredth of a second on the test
clips; the frames come from different decoders and scalers (ffmpeg's
against the browser's), so a frame's difference at a boundary is possible
on other material. The reference has no pattern test, so stripes are
reported by this detector alone.

### Memory and long files

Nothing scales with the length of the file except what a scan records per
frame (about 24 bytes) and what a prepared section holds. Sections are
cached at analysis resolution (256×144 for a 16:9 picture, whatever the
source), three bytes a pixel, so a second of a 30 fps section costs about
3.3 MB plus a fixed run-up and run-out of 6.5 s each side under the default
profile. Cached sections are kept up to a budget scaled to the device's
memory (64 MB for each GB the browser reports, 192 to 512 MB; 256 MB where
it reports none); older ones are dropped and
prepared again when opened, and an export applies their marks all the same,
from the frame times. WebAssembly memory never shrinks, so that budget is
also the high-water mark a long session settles at.

An export is streamed to disk wherever the browser allows it: to a file of
your choosing (Chrome, Edge, Opera) or to the browser's private storage
(Chrome, Firefox), from which it is offered for download. Only where neither
exists (Safari) is it assembled in memory, and then only up to a size the
device can hold; a larger one is left for you to export by hand. An export
in private storage is kept until another video is opened (or the next
export replaces it): open the same video again after a reload and it is
offered again, to download or verify. The
detector's clock is 32-bit microseconds with wrapping ages, so a film longer
than the 71 minutes at which it wraps is analysed like any other. With a
scan in hand, the live monitor reads the scan's per-frame numbers at the
player's position rather than detecting again, so it never misses a frame.

## Limitations

- **This reduces risk. It does not guarantee safety.** Passing the detector
  means passing a published set of thresholds, not that the video is safe
  for every person.
- The pattern test covers regular stripes and gratings, the case the
  broadcast guidance quantifies (more than five light–dark pairs, flash-
  strength contrast, a quarter of the screen). It measures contrast and
  area, not how many *cycles per degree* a viewer sees, and it does not
  claim to catch every texture that could affect someone. Softening blurs
  the frames that carry the pattern; the result is verified by the same
  detector, and it is still a blur.
- The built-in H.264 decoder does not do 4:2:2/4:4:4, 10-bit, slice groups
  or SP/SI slices; the built-in HEVC decoder does not do 4:2:2/4:4:4 or the
  range extensions, VP9 profiles 1 and 3 (4:2:2, 4:4:4) and AV1 4:2:2,
  4:4:4 and 12-bit are not decoded either. Such files need a browser with
  its own decoder for them.
- The export copies the audio (or re-encodes it, from an MKV whose audio an
  MP4 can't carry). Where frames are held (**E**), it re-encodes the sound
  (AAC where the browser has an AAC encoder, else Opus) with silence under
  each held frame; a browser that can't re-encode audio copies it as it
  is, and the sound then runs ahead of the picture after each hold (the
  export says so). Removals (R/F) do not change timing and need no audio
  work. Subtitle tracks and all but one audio track (the one in use) are
  left out.
- AVI, FLV, WMV, MPEG program streams (.mpg, .vob) and Ogg are not read,
  nor MPEG-2 or VC-1 video in a transport stream; remux or convert them
  first.
- The export copies the untouched GOPs only when the encoder's codec is the
  source's (H.264 into H.264, VP9 into VP9); an HEVC or AV1 source, or a
  browser without an H.264 encoder, gets a full re-encode.
- Sections, marks and the scan are stored in the browser's IndexedDB per
  file (found again for a copy of the file with the same name and size);
  **Project…** in the header saves them to a file (JSON) and loads one back,
  for another browser or computer. Frame caches live in memory and are
  rebuilt when a section is prepared again.
- Review the flagged sections yourself before you share anything.
