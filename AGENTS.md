# Pane: notes for AI helpers

A Rust Mac app. It shows an Android phone's screen in a window (through `adb`) and opens Apple's own iPhone Mirroring for iPhones. Full description: `README.md`.

## Where things live

All code is in `src`. Four files:

| File | What it does |
|---|---|
| `src/main.rs` | The window and everything drawn in it. `MirrorApp` is the start screen (phone list, Wi-Fi card, pairing screen, the "Open iPhone Mirroring" button). `Mirror` is the live phone window (zoom, rotation, taps and swipes, toolbar, the ••• menu). Also holds the UI tests and the demo picture used by `--demo`. |
| `src/devices.rs` | Finding and talking to phones. Finds `adb`, scans for phones every few seconds, opens iPhone Mirroring, and runs `AndroidSession` (one connected phone: the video thread, 4 screenshot helpers, taps, keys, rotation, clean shutdown). |
| `src/stream.rs` | The fast live video. Asks the phone to record its screen, pipes it through `ffmpeg` into pictures, and watches over the helper programs so none are left running. Holds the shared `Control` switches. |
| `src/wifi.rs` | Connecting over Wi-Fi: finding phones on the network, pairing by QR code or six-digit code, and the one-step-at-a-time `Step` state that drives the Wi-Fi card. Every network step runs on a background thread with a time limit. |

Other files: `MECHANICS.md` (the numbers copied from Apple's app, plus a table for each state machine), `mechanics.json` (same numbers, machine-readable), `implementation-notes.md` (decisions, gotchas, what is not done yet).

## Things that bite

- **Run reviews only from inside the project folder.** A review started from another folder once edited an unrelated project.
- **Keep the video switches in step.** `Control` in `src/stream.rs` has `live`, `up` and `restart`. `live` must be cleared on every way out of the stream thread, or the screenshot helpers sleep forever and the picture freezes. The three legal combinations are in `MECHANICS.md`.
- **Do not add `-fflags nobuffer` to ffmpeg.** It drops frames (speed fell from about 40 to about 5 pictures a second). The current flags are in `src/stream.rs`.
- **The Wi-Fi card is one state.** Change it only through the `JobState` methods in `src/wifi.rs` (`begin`, `succeed`, `fail`), never by setting pieces by hand.
- **Never block the window thread.** Anything that waits on `adb`, `ffmpeg` or the network goes on a background thread.
- **Check the build with `cargo test`.** All 23 tests should pass. `cargo run -- --demo` shows the window without a phone.

## Test switches

`PANE_NO_STREAM` (skip fast video, use screenshots), `PANE_DEBUG` (print stream details), `PANE_MEASURE_SECS=N` (keep the window up N seconds and report speed), `--demo`, `--screenshot <file.png>`.

## Not verified by hand yet

The orange note in the ••• menu, rotation while streaming, a phone leaving the Wi-Fi, a USB cable, two phones at once. See `implementation-notes.md`.
