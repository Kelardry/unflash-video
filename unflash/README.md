# Unflash (Python reference implementation)

This is the original Python/ffmpeg tool. The browser version built on it
lives at the repository root; see [../README.md](../README.md). The two share
one detector definition, and this one is the reference the Rust port is
tested against.

Unflash finds the flashing in a video that can trigger photosensitive
seizures, and helps you take it out without wrecking the footage.

Most "flash removal" just dims or blurs the whole video. Unflash removes
individual frames instead and holds the neighboring frame in their place,
so the picture stays sharp, the audio stays in sync and the running time
doesn't change. If a frame you need to remove has something important on it,
you can hold it on screen for a second instead, with the sound muted.

Everything happens on your own computer. Nothing is uploaded anywhere.

**This reduces risk. It is not a guarantee.** Please read
[Limitations](#limitations) at the bottom before you rely on it.

If you want to know exactly how the flash detection works, that's in
[DETECTION.md](../DETECTION.md).

## Installing

You need Python, and ffmpeg with `ffmpeg` and `ffprobe` on your PATH.

```
pip install -r requirements.txt
```

## Running

```
run_unflash.bat
```

A browser tab opens at http://127.0.0.1:8765/.

When you're finished, press **Quit** in the header rather than just closing
the tab. Closing the tab leaves Unflash running in the background, and the
next launch will find that copy and reopen the tab on it. This matters after
an update: a running copy keeps the code it started with, so a new version
does nothing until the old one has really stopped. The header shows the date
of the code it's running, and a red banner appears if the files on disk are
newer than that.

## The short version

1. **Open video.** A folder called `<name>.unflash` appears next to it, and
   everything you do is saved there as you go. You can close the tab and
   come back later.
2. **Scan for flashes.** Unflash checks the whole video and puts a numbered
   *section* around each problem. The timeline shows where the flashing is.
3. **Prepare** a section (or **prepare all** in the sidebar). This pulls out
   the frames so you can work on them.
4. **Edit.** Mark the frames you want gone, or let **Suggest** do it.
5. **Check safety.** An instant verdict with nothing to render. Green means
   this section passes now.
6. **Render full-res.** Every section needs this before you can export.
7. **Export**, then **Verify exported file** to re-scan the finished video.

You can also make your own sections: drag on the timeline, or type a start
and end time next to it.

## Editing a section

Click a frame in the grid to select it. Shift-click selects everything
between two clicks, ctrl-click adds or removes one, and Esc clears the
selection. With **Caps Lock on**, shift-click selects a rectangle in the
grid instead, which is handy for taking out a whole run of rows.

Then press a key, or use the buttons under the grid:

| key | what it does |
|---|---|
| **R** | remove, and show the frame *before* it instead |
| **F** | remove, and show the frame *after* it instead |
| **E** | hold this frame on screen for 1 second, muted |
| **U** | unmark |

Removed frames go red either way. The little badge on each one tells you
which frame will be showing in its place, so you can see at a glance what
you're actually going to get.

R and F usually look identical, but not always: on a cut, filling from the
wrong side drags a frame of the old shot across the join. If something looks
smeared in the preview, try the other one.

### Letting it pick for you

**Suggest: keep light** and **Suggest: keep dark** work out a set of
removals, run them past the detector, and keep going until the section
passes. Keep-light holds the brighter frames, keep-dark the darker ones.
Try both and see which looks better; whichever you run last replaces the
one before it, so there's nothing to undo in between.

**Suggest: reduce FPS** is the fallback for flashing the other two can't
budge, like a strobe with no steady bright or dark phase to hold on to. It
thins the section down to a frame rate that simply can't flash fast enough
to fail, working from the frame timings alone. It never looks at the
pictures, so it works on anything. The result is choppier, and the button
tells you what rate it's about to use.

That rate is a worst case, and most footage passes at a lot more frames than
it allows. The **▾** next to the button lets you type your own rate, with
*safe rate* to put it back. A good way to use it: run it at the safe rate to
see the section go green, then raise the rate until it goes red again and
step back one.

Check **selection only** on any of the three to confine it to the frames
you've selected.

## When a check fails

**Check safety** tells you what's still wrong and roughly where. Press
**select unsafe frames** and it highlights the exact frames inside the
failing moment, so you can remove more of them.

Two things it might tell you that aren't about this section:

- **"just over the line"** means the flashing is sitting almost exactly on
  the threshold. Re-encoding a video moves the measurement by a couple of
  percent all by itself, so content this close can pass here and fail in the
  finished file. Trim a bit more than looks necessary and it settles down.
- **"past the end of this section"** means your edits left flashing just
  after the last frame you can reach. Drag the section's end out past it, or
  edit the next section.

Previews are slightly less sensitive than full renders, because they're
analyzed at a lower resolution. Where the two disagree, believe the
full-resolution one.

## Exporting

Every section has to be rendered at full resolution first. **render all** in
the sidebar does them all and skips any that are already up to date. If you
edit a section after rendering it, it gets a *render stale* badge and you'll
need to render it again.

The export dialog offers three ways of putting the video back together:

- **Re-encode spans, stream-copy join** (the default). Rebuilds the parts,
  then joins them without re-encoding. Fastest, and no quality loss at the
  joins.
- **Re-encode spans, filter join.** Decodes and re-joins everything in one
  pass, rebuilding every timestamp along the way. Costs one more encode
  (invisible in practice) and is the one to reach for if a join ever comes
  out wrong.
- **Smart-cut.** Copies the untouched parts as they are instead of
  re-encoding them. Much faster, h264 sources only.

Then press **Verify exported file** to re-scan the finished video. Verifying
reads the file that's already there; it doesn't export again.

## Picking a profile

The **Profile** dropdown in the header decides what counts as a problem.
Every check, render and verification uses whichever one is selected.

| Profile | What it flags |
|---|---|
| **Exact WCAG + flag extended flashes** (default) | WCAG failures, plus sustained flashing that sits right at the legal limit |
| **Exact WCAG only** | WCAG failures and nothing else |
| **Stricter than WCAG** | A tighter threshold, for extra margin |

**Extended flashes** are the middle one's specialty: flashing that meets
every WCAG failure condition except the rate, running *at* the permitted
speed rather than above it, for 5 seconds or more. WCAG lets that through.
UK broadcast guidance doesn't, and it does affect some viewers, so the
default profile treats them as work sections you can edit like any other.
They're labeled *extended flash* so you can tell them apart.

**Stricter than WCAG** is for photosensitive migraine and similar, where the
WCAG line is drawn in the wrong place. It doesn't list extended flashes
separately because it already fails outright at that speed.

If you change profile partway through a project, use the sidebar's **all
sections ▾** menu to re-prepare, re-check and refresh labels under the new
one. Your frame marks are kept.

## Resuming, moving and recovering

Everything lives in the `<video name>.unflash` folder next to the video, so
reopening that video picks the work up again, even if you've since moved or
renamed both.

If it isn't picked up automatically, or you moved the folder away from its
video, use **Open project folder…** and point it at the `.unflash` folder
itself. The paths saved inside get repaired, and anything genuinely missing
is listed at the top of the window.

If the folder still has section folders in it that the project file doesn't
know about, a **recover sections** button appears. It rebuilds them from
what's on disk and keeps the full-res renders, so a project whose project
file got lost can still be exported. The frame marks are gone for good
though, so recovered sections show as *unprepared*: re-rendering one would
give you an unedited version. Verify the export when you're done.

## Sharing a PC

Each Windows account gets its own copy, on its own port (8765, then 8766,
and so on), and a copy only answers its own account. Anyone else's browser
gets a short "this server is not yours" page. The access token is kept in
your own profile folder (`%LOCALAPPDATA%\Unflash`) and printed when Unflash
starts, so if you ever land on that page you can paste the printed address
to get back in.

Two accounts opening the *same* video still share the one `.unflash` folder
beside it, and would overwrite each other's edits.

Command-line flags, if you need them:

| flag | effect |
|---|---|
| `--port N` | use this exact port |
| `--new` | start another copy even if this account has one |
| `--no-token` | turn the access check off, so every account on the PC can use this copy, its projects and its file dialogs |
| `--no-browser` | don't open a browser |
| `--video FILE` | open this video on startup |

## Other bits

The 🔔 box in the header takes a number of minutes. Any job that runs longer
than that beeps and posts a desktop notification when it finishes, so you
can go do something else during a long render.

The video player is dimmed by default, and says whether what you're about to
watch has passed the detector. The dimming is a courtesy, not a safeguard.

## Limitations

- **This reduces risk. It does not guarantee safety.** Passing the detector
  means passing a published set of thresholds, not that the video is safe
  for every person.
- Static patterns like fine stripes and gratings can also trigger
  photosensitive responses, and Unflash does **not** detect those.
- Review the flagged sections yourself before you share anything.

If you want the detail: [DETECTION.md](../DETECTION.md) covers what counts as a
flash, how the thresholds are applied, why the safe frame rate is what it
is, and why a section that passes its own check also passes in the finished
file.
