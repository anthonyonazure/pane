# Pane

Use your Android phone or iPhone from your Mac.

- **Android:** the phone's screen appears live in a window. Click to tap, drag to swipe,
  type to send text. Toolbar: Home, App Switcher, Search, Quick Settings, zoom and rotation.
  Works over a USB cable or over Wi-Fi (pair once by QR code or six-digit code).
- **iPhone:** opens Apple's own iPhone Mirroring with one click (Apple keeps the iPhone
  connection private). The button is pinned to the top of the window, and also in the phone
  window's ••• menu.

## What you need

- A Mac with [Rust](https://rustup.rs) installed.
- Android's tools: `brew install android-platform-tools`.
- For smooth live video from Android: `brew install ffmpeg`. Without it the app still works, but slower
  (a screenshot per frame, about 2 to 9 pictures a second instead of 30 to 60).
- On the phone: Developer options turned on, then **USB debugging** (cable) or
  **Wireless debugging** (Wi-Fi).

## Build and run

```sh
cargo build --release
./target/release/pane
```

## Keyboard shortcuts (while a phone is shown)

| Keys | Action |
|---|---|
| ⌘1 / ⌘2 / ⌘3 / ⌘4 | Home / App Switcher / Search / Quick Settings |
| ⌘+ / ⌘− / ⌘0 | Zoom in / out / actual size |
| ⌘9 | Phone shape or free-size window |
| ⌘8 | Rotate |
| Esc or right-click | Back |

## Options for testing

- `--screenshot <file.png>`: draw about 120 frames, save a picture of the window, quit.
- `--demo` (or the environment variable `uiTesting=on`): show a pretend phone (no phone needed).
- `--connect <serial>`: open that phone straight away (with `--screenshot`, prints pictures per second).

`MECHANICS.md` lists the timings and rules the app uses and where each came from, and ends with a
reference of the app's state machines (which are explicit, which are not, and who moves each).

## For developers and AI helpers

A short map of the four source files, and the things that are easy to break, is in `AGENTS.md`.
