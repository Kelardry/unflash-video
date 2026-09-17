# How Unflash decides what counts as a flash

This is the supplement to [README.md](README.md), for anyone who wants to
check the reasoning rather than take the verdict on trust. Nothing here is
needed to use the tool.

Unflash implements the WCAG 2.x / PEAT definitions of general flash and red
flash, adds an optional test for sustained flashing at the legal limit, and
tries hard to make sure that a section which passes its own check also
passes when you re-scan the exported file.

## Contents

- [What counts as a transition](#what-counts-as-a-transition)
- [What counts as a failure](#what-counts-as-a-failure)
- [Applying a 1024x768 rule to other shapes](#applying-a-1024x768-rule-to-other-shapes)
- [Calibration](#calibration)
- [Extended flashes](#extended-flashes)
- [The three profiles](#the-three-profiles)
- [The safe frame rate](#the-safe-frame-rate)
- [Why a section's check matches the export](#why-a-sections-check-matches-the-export)
- [Awkward source files](#awkward-source-files)
- [Things that are inherent, not bugs](#things-that-are-inherent-not-bugs)
- [Command line](#command-line)

## What counts as a transition

Pixels are converted to relative luminance with sRGB linearization. A pixel
makes a qualifying **luminance transition** when its accumulated monotonic
change in luminance reaches 10% of maximum luminance or more, and the darker
of the two states is below 0.80.

A qualifying **red transition** needs `|Δ(R−G−B) × 320| > 20` *and* the pixel
entering or leaving the saturated-red state `R/(R+G+B) >= 0.8`. Requiring the
saturation change as well as the amplitude is deliberate: brightness wobble
inside a scene that is continuously red is not a red flash. Red flashing
against dark is still caught, by the ordinary luminance criterion.

A pixel **flashes** when it completes a pair of opposing qualifying
transitions within one second.

Accumulating the change monotonically is what lets a flash that ramps over
three or four frames count as one transition rather than several small ones
that each fall short. It also means a pixel can be part-way through a run for
a long time, which matters later (see
[finite memory](#a-run-up-only-works-if-the-detectors-memory-is-finite)).

## What counts as a failure

Content fails when three things are true at once, somewhere in a 341×256
window (a tenth of a 1024×768 screen, standing in for the central ten degrees
of vision):

1. **Area.** Pixels flashing more than 3 times a second cover at least a
   quarter of the window. (Strict profile: more than 2 times a second, over a
   fifth of the window.)
2. **Concurrency.** Those pixels flashed *just now*, not at scattered moments
   across the last second. Without this, a bright band sweeping across the
   screen during a pan reads as widespread flashing.
3. **Coherence.** The window's *mean* luminance is itself flashing at that
   rate. Without this, a dark limb swinging across a bright background flicks
   individual pixels on and off while the region's overall brightness barely
   moves. Walking characters would fail everywhere.

Conditions 2 and 3 are the ones doing the real work. They're what separates
flashing from motion, and dropping either of them produces a detector that
flags ordinary dialogue scenes.

## Applying a 1024x768 rule to other shapes

WCAG defines its thresholds for content that fills a 1024×768 field of view.
It says nothing about other shapes, and real video is rarely 4:3, so this is
a judgment call and worth stating plainly.

Unflash fits the frame inside the 1024×768 box by whichever dimension runs
out first, and scales the window with it:

| source shape | fitted to | window covers |
|---|---|---|
| 4:3 | 1024×768 | 33% × 33% of the picture |
| 16:9 | 1024×576 | 33% × **44%** of the picture |

The reasoning is that a viewer sits so the picture's *width* fills their
field of view, and a widescreen frame is simply shorter.

The consequence: on widescreen content, flashing has to cover a taller share
of the frame before it's flagged than the same flashing would on 4:3. If
you'd rather err the other way, fitting by height instead (1365×768 for 16:9,
window 25% × 33%) flags more. That's `screen_w` / `screen_h` in the detector
profile in `config.py`, or ask and it can be made a setting.

## Calibration

Checked against synthetic patterns in `tests/`:

- exactly 3 flashes/s passes WCAG, 4 fails
- 15% window area passes, 35% fails
- bright-only flicker passes
- jittered multi-frame ramps fail
- moving boxes, slow pans over high-contrast edges, and swinging occluders
  (walking characters) all pass
- fast dense scrolling gratings, which are strobe-equivalent, fail

And against real footage: a 23-minute anime episode yields a handful of
short, plausible flash windows (lightning strikes, a flash-cut opening
montage) rather than blanket coverage.

## Extended flashes

An extended flash is the same hazard one step below the failure rate:
flashing that satisfies *every* criterion above (swing, dark state,
concurrency, area, mean coherence) at exactly the permitted rate of 3
flashes a second rather than above it, recurring with no gap longer than a
second, for at least 5 seconds.

WCAG passes that. ITC/Ofcom guidance treats sustained flashing at the limit
as a hazard, and it does still affect some viewers, so the default profile
reports it.

The rate test is what keeps this honest. Everything the failure test rejects
as motion rather than flashing (pans, cuts between light and dark shots,
scrolling credits, blinks, mouth-flaps) gets rejected here for the same
reason, because a pixel crossed once by a moving edge does not flash three
times a second. An earlier version asked only that pixels had flashed *once*
in the last second across a third of the area, and that flagged ordinary
dialogue scenes and scrolling end credits.

3 flashes/s is also the lowest rate at which that separation holds, which is
why the strict profile doesn't report extended flashes. Its own limit is 2
flashes/s, and at that rate scrolling credits and shot-cut dialogue are
indistinguishable from real flashing by per-pixel rate, area, swing or window
amplitude. Measured: end credits qualified on 73% of frames against 48% for a
genuinely flashing scene. Strict loses nothing by leaving them out, because
everything the default profile calls an extended flash, strict fails
outright.

Because extended flashes aren't WCAG failures, verdicts keep them separate. A
section or exported file whose only remaining problems are extended flashes
still passes WCAG, and Unflash says so while still marking it unsafe under
the active profile.

## The three profiles

The profile is chosen in the header and used by every check, render verdict
and verification.

| Profile | WCAG thresholds | Extended flashes |
|---|---|---|
| **Exact WCAG + flag extended flashes** (default) | exact | reported as violations: they get their own work sections labeled *extended flash*, count in the verdict, and Suggest tries to clear them |
| **Exact WCAG only** | exact | not detected or reported at all |
| **Stricter than WCAG** | tighter: 0.08 swing, 1/5 area, 2 flashes/s | not reported separately, because this profile already fails at 3 flashes/s |

## The safe frame rate

Every profile has a frame rate below which it cannot report flashing at all,
whatever the pictures contain, and **Suggest: reduce FPS** thins a section
down to it.

The bound is arithmetic. A pixel's brightness run reverses at most once per
frame, and a picture merely held on screen again isn't looked at, so every
qualifying transition needs a *new picture*. A flash is a pair of opposing
transitions, so it needs two. Reaching `k` flashes therefore takes at least
`2(k−1)` frame intervals between the first flash and the last. Fit fewer than
that many intervals into a second and the verdict is unreachable.

| Profile | flashes needed | frame intervals | safe rate |
|---|---|---|---|
| **Exact WCAG + flag extended flashes** | 3 (extended, at the limit) | 4 | 3.8 /s |
| **Exact WCAG only** | 4 (more than 3) | 6 | 5.71 /s |
| **Stricter than WCAG** | 3 (more than 2) | 4 | 3.8 /s |

The quoted rates sit about 5% under the whole numbers the arithmetic gives
(4/s and 6/s). A render can only place a picture on the nearest slot of its
100- or 120-per-second grid, so a gap read back off the file can come out a
slot shorter than the editor laid out. They're then rounded *down* to two
decimals, so the figure on the button is the figure the guarantee was worked
out for, and "is this rate still safe?" is a plain comparison rather than a
question about rounding.

### Choosing a different rate

The safe rate is a worst case. It assumes every picture is the exact opposite
of the one before it, over a quarter of the screen, for as long as you like.
Almost nothing looks like that, and most flashing footage passes at a good
many more frames than the bound allows, which is why the rate is editable.

Above the safe rate the result stops being a promise and becomes a proposal
like keep-light and keep-dark. It's still checked before it comes back, and
the message says which of the two you got. A rate at or under the safe one
holds however you edit around it. One above it was judged on the frames as
they were, so re-check the section if you change anything near it.

The thinning keeps the first frame, then the next frame at least that far
along *in time*, and so on. Keeping every nth frame instead would give a
different rate in every passage of a variable-rate source (a downloaded
livestream that runs at 60 fps through the action and stalls for a second
here and there). Going by time gives the same rate throughout, and takes
nothing out of a stall that was already slow enough.

With *selection only* ticked it thins just the selected frames. Frames
outside the selection are left alone and still set the pace, so the first
survivor inside the selection is spaced from whatever really precedes it. But
the guarantee then covers only the span you selected, and flashing carried by
full-rate frames either side of it will still be reported.

## Why a section's check matches the export

The point of a work section is that editing it until it passes should mean
the exported video passes. Four things have to be true for that, and each one
was a real bug before it was fixed.

### A section has to contain the frames responsible for its own violation

A general-flash failure is more than three flashes in a second, so the
detector can only announce one when the last of those flashes lands, up to a
second after the flashing began. Padding a section out from the announcement
left the run-in that caused it outside the section, where nothing could be
done about it.

Every violation now carries an `onset`, the exact moment of the earliest
transition still inside its failure window, and sections are padded from
there. It adds about a second to the head of a section and creates no new
ones.

### A section's check has to see what a pass over the whole video sees

The detector carries state. A flash is a pair of transitions up to a second
apart, and the failure test looks back over a second of flashes. Checking a
section on its own used to start the detector cold at the section's first
frame, which left it blind for roughly its first second. Flashing there
passed the check and then turned up when the finished export was verified,
"inside a section that was already safe".

Preparing a section now also caches a few seconds of footage from before and
after it, and the check, the suggester and the rendered section's verdict all
run over run-up + section + run-out. Where that footage falls inside a
*neighboring* section, the neighbor's edited frames are used instead of the
original ones, so a check never reports flashing you've already removed
somewhere else.

Flashing found in the run-out is reported separately. If it lands in the next
section, it's that section's to fix. Otherwise it's flagged as something your
edits pushed past the end. Which side of a boundary a failure falls on is
decided by where its *flashing* is, never by how far its onset reaches back.
The onset exists to widen a section; letting it decide ownership blames a
section for flashing that starts after its last frame and then offers its
final frames as the fix.

One side effect: because a section's check reads its neighbors' edits,
editing one section clears the recorded verdict of any section close enough
to have read it. They show as unchecked until you re-check.

Sections prepared by an older version have no cached run-up. They still
check, but cold. The check says so, and preparing them again fixes it.

### A rendered file has to be read by its pictures, not by its frames

A section is written onto a constant-rate grid, so a 24 fps section comes
back at 120 fps with every picture written five times over. The hazard tests
ask whether enough of the picture is flashing *at this instant*, so reading
that file frame by frame asks five times as often as the check did, and finds
instants the check stepped over. That was the whole of a run of "passes the
check, fails the preview" reports on 1080p footage: identical pictures at
identical times, different sampling, opposite verdicts.

Spotting the repeats by comparing frames doesn't undo it. They aren't
identical by the time they come back. x264 codes the first copy of a picture
roughly and refines it over the copies that follow, so on a 1080p render only
about one repeat in seven survives as an exact match.

So a rendered section is read at the times its own pictures go up, one frame
each, which is the sequence the check reasons about, with the pixels the
render actually produced. A verify pass over the finished export does the
same through each section and takes the untouched spans as they come, since
those keep the source's rate.

Stamping each frame with its picture's time rather than the file's matters
for a second reason. The grid can only place a picture to the nearest slot,
and 24000/1001 fps lands on 5.005 of them, so the file's timestamps come back
jittered by up to half a slot. The failure test counts flashes inside a hard
one-second window, and eight milliseconds decides whether the fourth flash
falls inside it.

There's a worked example of how small that difference is and how much it can
change. A 120-slot grid places 24000/1001 fps pictures five slots apart, so
the export presents them at 24.000 fps rather than 23.976, a tenth of a
percent fast. That lands on a discretization cliff: the pooling window is
0.125 s, which is 2.997 frames at the true spacing and *exactly* 3.000 at the
grid's, so the concurrency pool takes three frames on one side of it and four
on the other. Four frames pooled instead of three pushed one section 13% over
the area threshold. Set the window to 0.124 or 0.126 and both timelines
agree; only at 0.125 do they differ. The footage wasn't flashing any more.
The measurement was pooling one more frame.

### A run-up only works if the detector's memory is finite

It wasn't, originally. The per-pixel tracker accumulates one monotonic run so
that a flash ramping over a few frames still counts as a single transition,
and until the pixel turns, the swing it will eventually report is measured
from wherever that run began.

On a slow drift (a fade, a scene brightening, exposure adjusting) a pixel can
be mid-run for half a minute. Measured on real footage, a fifth of the
picture was mid-run from more than six seconds earlier, and some of it from
twenty-six seconds earlier. So a check starting cold a few seconds before a
section measured different swings than a pass over the whole video did. That
gives you a section that passes its own check, fails when the export is
verified, and passes again when you draw a fresh section over the very same
frames, with nothing you can edit to break the loop.

A run is now re-anchored once it reaches `MAX_RUN_SECONDS`, which bounds the
memory and makes the run-up length an honest promise. Nothing that slow was
ever a flash anyway: a flash is a pair of opposing changes inside a second,
so a swing that took longer than that can't be half of one.

A related point about arithmetic rather than logic. Every per-pixel time the
detector keeps is float64. A float32 holding 4259 s resolves to a quarter of
a millisecond, and that's measured against a 0.125 s pooling window which at
24 fps sits 2.997 frames back, so a quarter-millisecond nudge decides whether
a third frame still counts as "just now". The symptom when this breaks is
distinctive: every section passes its own check and its render, and the
export verify fails a handful of them, all in the *second half* of the video,
because that's where the clock is large enough to matter.

## Awkward source files

Stream VODs and clipped videos often have broken timestamps: negative start
times, variable frame rate, audio offset from video, or multi-second jumps
that make a 3-second clip claim to be 26 seconds long.

Unflash reads the real timeline from the packet index and bridges timestamp
anomalies in both the edited sections and the untouched spans between them.
That's reported as a warning, because bridging a jump makes the export
shorter than the source's nominal duration.

Audio and video durations are forced to match exactly in every part, and
every part is written on one shared mp4 timescale. Sections are constant-rate
on the fine grid while untouched spans inherit the source's rate, so their
timebases disagree by construction, and the concat demuxer doesn't reconcile
that: it mis-stamps whatever disagrees with the first part, collapsing it to
a few milliseconds and leaving a hole where it should have been. Parts
rendered before this was pinned down get remuxed (a stream copy) rather than
re-rendered. The final concatenation is sanity-checked for span and gaps.

Untouched spans keep the source's exact frame-to-frame timing throughout,
variable rate included. Sections work on the repaired timeline, and frame
identity is the ordinal within a section, with per-frame timestamps from
ffmpeg's `showinfo` as ground truth.

All of which means how densely a file samples its own footage varies a lot: a
rendered section carries three or four times the frames per second of the
untouched material either side of it, and a screen or game capture can change
rate from moment to moment. The detector is deliberately blind to this. It
measures sustained flashing in seconds rather than in frames, it searches
window positions continuously instead of only where frames happen to fall,
and a frame that merely repeats the one before it isn't counted as a fresh
observation of anything. So the same footage gets the same verdict whether
it's checked from the cache, rendered, or verified inside the finished
export. Before this, the extended-flash test counted frames, so a rendered
section outvoted its own neighbors four to one and could fail a check its
source footage passed.

The render grid is chosen to carry the source's timing exactly where it can:
100 slots a second divides 25 and 50, and 120 divides 24, 30 and 60. The
1000/1001 rates (23.976, 29.97, 59.94) divide neither and keep a residual of
half a slot, about 4 ms, as does any variable-rate source. Reading a render
back by its pictures is what stops that residual reaching a verdict.

Where the source crowds two frames onto one slot (a timestamp anomaly putting
a pair microseconds apart, which ordinary stream VODs do contain) both are
still written, one borrowing a slot that the next frame with room hands back.
A marked frame never silently fails to reach the video, and the run keeps its
length.

## Things that are inherent, not bugs

**Knife-edge content.** The detector counts pixels crossing a hard threshold
and compares that count to a hard area threshold. A re-encode moves the count
by 2–3%. That's irrelevant at 1649 against a bar of 1360, and decisive at
1408. Because one of the tests is a yes/no coherence gate, a marginal case can
drop from 1408 to 0 rather than shading down gradually. Unflash says "only N%
over the area threshold, just over the line" when it spots this, and the
answer is to trim further than looks necessary rather than to go looking for
a bug.

**Previews are less sensitive than full renders** on that borderline content,
because they analyze a 540p encode. Treat the full-resolution verdict as the
authoritative one.

**NTSC rates keep a grid residual.** 100 divides 25 and 50; 120 divides 24,
30 and 60; nothing divides 24000/1001 or 30000/1001.

Two known limitations, both real, neither urgent. The export genuinely
presents 1000/1001 content about 0.1% fast; presenting it at its true times
needs variable-rate output at a timescale that can express those rates
exactly (120000 works for every common one), which is a real change to the
render and concat path. And the 0.125 s pooling window sits exactly on 3
frames at 24 fps, so 24 fps material is balanced on that cliff; 0.12 would
put it unambiguously on the three-frame side, but that shifts detection
sensitivity for everything.

## Command line

```
python -m unflash.cli analyze VIDEO [--start S --duration D] [--wcag]
python -m unflash.cli scan VIDEO [--wcag]
```
