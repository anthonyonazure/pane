# Implementation notes: live video stream (branch video-stream)

## What changed
Android pictures now come from a live H.264 stream (`src/stream.rs`) instead of one screenshot per
frame. The phone's built-in `screenrecord` encodes, `ffmpeg` decodes to JPEG, and the pictures drop
into the same "latest picture" slot the window already reads. The four screenshot workers pause while
the stream is live and resume by themselves if it cannot start (no ffmpeg, phone refuses).

## Measured (Pixel 6, Wi-Fi, same swipe motion, 10 to 12 s)
| Method | Pictures per second |
|---|---|
| Screenshot per frame (old) | 2.1 to 2.8 under motion (9 best case, from earlier notes) |
| Live stream (new) | 33 to 46 average over 10 s runs, 55 to 85 while the screen moves |

## Deviations from the plan (and why)
- **screenrecord instead of scrcpy's helper.** Plan said push scrcpy-server. screenrecord needs no
  file pushed, no extra download, and proved the idea in one step. Costs: it sends nothing while the
  screen is still (fine, a still screen needs no update), and it stops after 180 s (the stream
  restarts straight away and keeps the last picture on screen). Taps are still adb commands (about
  0.11 s each): input over a live connection is the next stage and needs scrcpy-server.
- **ffmpeg as a separate program** (found in the usual Homebrew spots). A built-in decoder crate
  would remove that need; not done yet. Without ffmpeg the app quietly uses the old method.

## Gotchas found
- `ffmpeg -fflags nobuffer` threw away 425 of 432 frames. Removed. Test any ffmpeg flag against a
  saved clip and count frames before trusting it.
- Test mode used to quit after about 1.6 s, mostly startup. `PANE_MEASURE_SECS=N` now holds it
  N seconds so pictures per second is measured fairly. `PANE_DEBUG=1` prints stream rates.
  `PANE_NO_STREAM=1` forces the old method.

## Review fix
The stream's "live" flag is now cleared on every exit path and during the restart gap, so the screenshot
fallback backfills instead of the window freezing. Screenshot workers also re-check it before writing.

## Not done yet
- Not tried interactively yet: only the automated screenshot mode was run, nobody has watched the live window or tapped during a stream.
- Taps and typing over a live connection (needs scrcpy-server).
- Native decoder so ffmpeg is not required.
- Delay (time from phone change to pixel change) not measured; only frame rate.
- Not tested: phone rotation during a stream, phone screen off/locked, USB cable, two phones.

## Review loop (4 passes)
- Pass 1 found and fixed: adb/ffmpeg left running when the window closed or the phone changed (the reader sat blocked on a still screen, so nothing could stop it; now a watchdog and `stop()` kill both); a frozen window when the link died silently (after 3 s without pictures one screenshot worker watches; the rest rest); a picture start marker split across two reads was dropped; no cap on the read buffer (32 MB cap); rotation left the stream in the old shape (restart 0.9 s after a rotate); a hung run past 195 s is killed; poisoned locks no longer panic.
- Pass 2 (adversarial) found: after two quick failures (phone screen off) the stream gave up for good, so a phone asleep at launch never streamed. Now it retries every 10 s. Verified live: asleep, then woken, streams.
- Passes 3 and 4 found nothing real.
- Still open: a screenshot worker blocked on a hung adb has no timeout (existed before the stream); `wifi::run_limited` waits for exit before reading output (fine for small outputs); rotation by the phone itself (auto-rotate) is not detected; interactive window not watched by a person.
- Speed after fixes, phone awake and swiping: 36.9 and 46.5 pictures per second (two runs). Screen off: 6.8 per second via screenshots, as designed.
