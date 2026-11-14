# Mechanics sheet (Pane rebuild)

Source: iPhone Mirroring 2.1 (com.apple.ScreenContinuity).
Addresses are in the arm64e slice of ScreenContinuityUI.framework (sha256 88359f8a…), read with Ghidra
via native_function_pseudocode, unless marked "main program".

## Window size and zoom (FUN_000aff30, windowScaleCommandReceived(adjustment:))
| Rule | Value | Source |
|---|---|---|
| Actual Size (adjustment 0) | scale = 0.77 × the phone's own size | 0xaff30 (`dVar23 = 0.77`) |
| Zoom In (adjustment 1) | scale + 0.1, at most 1.5, rounded up | 0xaff30 |
| Zoom Out (adjustment 2) | scale − 0.1, at least 0.5, rounded down | 0xaff30 |
| Fit to screen | if too big: scale = max(0.5, min(availW / w, availH / h)) | 0xaff30 |
| Available width | screen visible width − 16 pt | 0xaff30 (`dVar24 + -16.0`) |
| Available height | screen visible height − (toolbar height + 8) | 0xaff30 (`_toolbarHeight + 8.0`) |
| Landscape | width and height swap (orientation 2) | 0xaff30 |
| Zoom buttons grey out at their limit | rule | toolbar builder 0x1d134 (`FUN_0001e0e8(0/1/2)` sets disabled) |
| Rotation keeps the scale; the window is pre-shrunk if the new shape won't fit | rule | 0x96544 ("Pre-shrinking window … before rotation") |
| "iPhone Size" is a toggle, shown only when the phone allows a resizable window | rule | `isiPhoneSizeToggleVisible`, `_deviceIsResizable` fields |

The phone's "own size" for Android: pixels × 160 ÷ screen density (Android's density-independent
points, read from `wm density`). This mapping is mine; the original reads the iPhone's point size.

## Window controls ("chrome")
| Rule | Value | Source |
|---|---|---|
| Controls hide after the pointer goes still | 2.0 s (`Duration.seconds(2.0)`) | 0xa7554 |
| Only Apple-internal builds can change it; 0 = never hide | rule | 0xa7554 |
| Show/hide animation | spring, response 0.28 s, damping 1.0, speed 1.0, delay 0 | 0xa7330 |
| Controls held visible while dragging | rule | 0xa7330 ("Drag debounce elapsed") |

## Keyboard shortcuts (FUN_0004a020, MainSceneCommandsViewModel init; all with Command)
| Command | Key |
|---|---|
| Home Screen | ⌘1 |
| App Switcher | ⌘2 |
| Spotlight | ⌘3 |
| Control Center | ⌘4 |
| Zoom In | ⌘+ (also ⌘=) |
| iPhone Size | ⌘9 |
| Actual Size | ⌘0 |
| Zoom Out | ⌘− |
| Toggle Rotation | ⌘8 |
(Ghidra shows each key as a tiny address; the byte value is the character, e.g. 0x31 = "1".)

## Toolbar icons (symbol names in the code)
Home `app.grid.3x3`, App Switcher `iphone.app.switcher`, Actual Size `square.resize`,
Zoom In `square.resize.up`, Zoom Out `square.resize.down`, Rotation `rectangle.portrait.rotate`
(0x1c988, 0x1d134, 0x1e994). The rebuild draws its own look-alike shapes.

## Connecting (used for Android over Wi-Fi)
| Rule | Value | Source |
|---|---|---|
| Short time limit | 5 s | ScreenSharingKit `TimeoutPrimitives.defaultForBuildConfiguration` 0x127A44 (system library, read with `dyld_info -disassemble`: `mov w8,#0x5` at offset 0) |
| Long time limit | 10 s | same function (`mov w8,#0xa` at offset 0x28) |
| Very long time limit | 60 s | same function (`mov w8,#0x3c` at offset 0x50) |
| Each connection step waits the long limit; steps 24, 29, 36 wait the very long one | rule | ScreenContinuityUI 0x5f2ac (bitmask 0x1021000000) |
| A defaults switch can turn time limits off | rule | 0x5f2ac (`TaskTimeoutsDisabledKey`) |
| On a time-out: "timed out connecting" message with Try Again | rule | strings at 0x14c0d0 and "Try Again"; step timer 0x5fcbc |
| Failed pairing search: error shown on the welcome screen, its button enabled again | rule | 0xce6a8 |

How the rebuild applies them (my mapping): finding phones on the network = short (5 s);
`adb connect` = one step, long (10 s), with Try Again on failure; `adb pair` = very long (60 s),
since the person is acting on the phone; waiting for a newly paired phone to show up = long (10 s).

## Message timings (used for the Wi-Fi status messages)
| Rule | Value | Source |
|---|---|---|
| An error stays up before another error replaces it | 0.8 s (`errorToErrorAnimationDelay`) | main program 0x1000089dc (`Duration.seconds(0.8)`) |
| Pause after a step finishes before moving on | 0.2 s (`loadingCompleteAnimationDelay`) | main program 0x1000089dc (`Duration.seconds(0.2)`) |
| Wait before a retry | 0.75 s (`retrySettlingDelay`) | main program 0x1000089dc (`Duration.seconds(0.75)`); cancel path ScreenContinuityUI 0x6d500 |
| UI-testing switch | env `uiTesting=on` → all three 0.1 s | main program 0x100001b0c |

How the rebuild applies them: a new Wi-Fi error waits until the previous one has shown 0.8 s;
after "Connected"/"Paired" the phone's screen opens 0.2 s later; Try Again shows "Trying again…"
for 0.75 s before reconnecting. Phones that blink off the network stay listed 10 s (connect) or
5 s (pairing box), reusing the original's long and short time limits (my mapping).

## Setup and pairing (OnboardingViewModel, used for Android pairing)
| Rule | Value | Source |
|---|---|---|
| One action button per error, by error kind | 0 Enable iPhone, 1 Connect, 2 Try Again, 3 Resume, 4 Sign In, 5-8 Settings, else Find My | `AppErrorAction.actionTitle` 0x49260 |
| No network during pairing | watch the network; keep the button greyed out until it returns | 0xcfd98 (`networkNotConnected` → `startMonitoringNetwork`, button enabled = error ≠ networkNotConnected) |
| Any other pairing failure | show the error, cancel outstanding requests, enable the button again | 0xcfd98 |
| Pairing timed out | reported as `timeoutReached(deviceName)` | 0xcfd98 |
| Pairing cancelled because the app is quitting | ignored, no error shown | 0xcfd98 ("Cancelled pairing step…") |
| Final setup button | starts the connection | 0xd07f4 |
| "Wi-Fi Off" / "Turn on Mac Wi-Fi to connect." | the original's no-network wording | Localizable strings |

How the rebuild applies them (my mapping): phone not paired → "Enable Phone" (the "Enable iPhone"
action) opens pairing; time-out or refused → "Try Again"; Mac offline → "Wi-Fi Off" card with a
"Settings" button that opens Wi-Fi settings, and every Wi-Fi button stays greyed out until the
network is back; a timed-out pairing names the phone's address.

## Clicks become touches (CaptureGestureRecognizer)
| Rule | Value | Source |
|---|---|---|
| Mouse pressed → finger down at once (gesture began) | state 1, records position, modifiers, time | ScreenContinuityUI 0x4c118 (from mouseDown 0x4c1ac) |
| Every drag movement → finger moves (gesture changed) | state 2 | 0x4c3c0 (from mouseDragged 0x4c43c) |
| Mouse released with no buttons left → finger up (gesture ended) | state 3 (else stays 2) | 0x4bb00 (from mouseUp 0x4c288) |
| Each change is handled straight away | handleGestureAction → 0xdba0c | 0x1189d4 |

Each press, drag and release is a separate event in the original (one NSEvent per call above), so
the rebuild also handles them one by one, in order, from the frame's input events. A light
trackpad tap (press and release inside one redraw) therefore counts; checking "is the button held
now?" once per redraw used to miss it. If the pointer leaves the window or the app loses focus
mid-press, the finger is lifted.

More rules from the same code:
| Rule | Value | Source |
|---|---|---|
| Every mouse event is queued in order; an exact repeat of the last one is skipped | event queue | ScreenContinuityUI 0x4b99c |
| Each gesture change sends all queued events in order, then empties the queue | flush per change | 0xdba0c (loop over queue → 0xdc080) |
| Gesture failed → queue thrown away, touch stream cancelled (not finished) | cancel | 0xdba0c → 0xdf274 |
| Right-button release ends a touch in progress at the last position | "keep position" flag = 1 | rightMouseUp 0x4c2f0 → 0x4bb00 |
| A right-button press alone does nothing | empty handler | rightMouseDown (4-byte stub) |
| Reset clears queue, active touches, position, modifier keys | reset | 0x4c5bc |

How the rebuild applies these: repeat events are skipped; a right-button release during a drag lifts
the finger where it was; leaving the window or switching apps mid-press sends Android's
`input motionevent CANCEL` (live phones) or sends nothing (older phones), so no accidental tap.
A lone right-click stays Back, which is my Android mapping (the original does nothing there).

How the rebuild applies them: on phones with Android 10+ (SDK 29, read from `ro.build.version.sdk`)
it sends `input motionevent DOWN` on press, `MOVE` while dragging (at most every 40 ms, my
measurement: one command ≈ 0.04 s on a Pixel 6), and `UP` on release, so long presses and drags
behave like a real finger. Older phones get a tap or swipe on release.

## Android picture speed (measured on a Pixel 6 over Wi-Fi; not from the original)
| Way of fetching the screen | Size | Time per picture |
|---|---|---|
| raw (`screencap`) | 10 MB | about 2.1 s |
| PNG (`screencap -p`) | about 650 KB | about 1.3 s |
| JPEG (`screencap -j`, not listed in the phone's help but supported) | about 610 KB | about 0.4 s |

Fetches running at once (JPEG): 1 → 2.5, 2 → 4.5, 3 → 7.6, 4 → 9.0, 5 → 10.6 pictures a second.
The rebuild uses JPEG with four fetches, falls back to PNG then raw on phones without JPEG, drops a
picture older than the one on screen, and builds its picture decoders fully optimised.
Sending a tap or key: about 0.12 s per command (one kept-open connection is used).

## Test mode (used for the demo phone)
| Rule | Value | Source |
|---|---|---|
| Test mode switch | environment `uiTesting=on` | main program 0x100001b0c |
| Only allowed on Apple's internal test setup | checks for XCTI Angel.app / internal build, else stops | main program 0x100001930 (not copied: the rebuild's demo is for everyone) |
| Pretend phone | "Discovery's iPhone 15", 430 x 932 points, scale 1.0, new id each run | main program 0x100001efc |
| Fake eligibility / restriction answers | `isDeviceEligible=on`, `deviceRestricted=on` | main program 0x10000403c, 0x100002e08 |

How the rebuild applies them: `--demo` or `uiTesting=on` shows a pretend Android phone with the
same 430 x 932 point screen (drawn at 2 pixels per point), so the window sizing rules can be checked
without a phone. It is named "Demo phone" rather than copying Apple's test name.

## App
| Rule | Value | Source |
|---|---|---|
| Full screen turned off | `NSFullScreenMenuItemEverywhere = false` | main program 0x100004750 |
| AutoFill item removed from the Edit menu | `NSAutoFillSystemInsertMenuEnabled = false` | main program 0x100005ed0 (not applicable: the rebuild has no Edit menu) |
| Closing the window quits the app | `applicationShouldTerminateAfterLastWindowClosed` returns true | main program 0x100006808 |
| Clicking the Dock icon brings the window back | `applicationShouldHandleReopen` → 0x100008060 returns true (after re-checking the launch request) | main program 0x1000067ac |
| Becoming active starts a fresh check | `applicationWillBecomeActive` → 0x10000736c starts a main-actor task | main program 0x100007ad8 |
| Launch checks for a launch request | `applicationDidFinishLaunching` | main program 0x1000060bc |
| Opening iPhone Mirroring is always one click | the rebuild pins "Open iPhone Mirroring" to the top of the start screen and to the phone window's menu (my layout choice, asked for by the person) | rebuild |
| Ordinary window (never floats above others) | the code reads `windowLevel` but never sets it | ScreenContinuityUI strings |

How the rebuild applies them (my mapping): the window asks to come to the front when it opens
and when a phone connects (an unbundled program launched from a tool otherwise opens behind other
apps; an app opened from Finder is brought forward by macOS); becoming active again re-scans for
phones at once instead of waiting for the 2-second scan; closing the window quits.

## Android equivalents (my mapping, not from the original)
Home key 3, App Switcher key 187, Back key 4, Spotlight = assistant intent,
Control Center = expand quick settings, rotation = lock auto-rotate then user_rotation 0/1/3.

## Estimated (looked for, not found as a fixed number)
| Rule | Value | Why |
|---|---|---|
| Toolbar height | 40 pt | the original measures it at run time (`_toolbarHeight` starts at 0, 0x5962c) |
| Menu bar height (stand-in for the visible frame) | 25 pt | egui reports the whole monitor, not the visible frame |
| Phone corner radius | 44 pt × scale | the original asks the iPhone (`_unscaledDeviceCornerRadii`) |
| Remembered window scale between launches | not built | `_lastKnownWindowScale` exists in the original |

## State machines (found 2026-11-08)
A flag pile counts as a hidden state machine only if some flag combinations are nonsense. Each
cluster below was checked against that test. Tests named in the last column pin the moves.

| Machine | Where | Explicit? | States | Who moves it |
|---|---|---|---|---|
| Wi-Fi step | `wifi.rs` `Step` in `JobState` | **Yes** (made explicit 2026-11-08) | Idle, Busy(text), Done(text), Failed{text, next: Nothing / Pair / Retry(addr)} | Worker threads call `begin` / `succeed` / `fail` under the job lock; the window only reads. Tests: `step_tests` |
| Phone connection state | `devices.rs` `AndroidState` | Yes | Ready, Unauthorized, Offline, Other | The scanner thread, from `adb devices` |
| Live video control | `stream.rs` `Control` (`live`, `up`, `restart`) | **No, documented only** (see below) | Off, Up but still, Live | Stream thread, its watchdog thread, the session's `stop()` |
| Pairing card | `main.rs` `show_pairing`, `pair_tab`, `pair_tab_chosen` | Partly (`PairTab`) | Hidden, Shown(Qr or Code, auto or chosen by the person) | The window, per click |
| Screen choice | `main.rs` `mirror: Option<Mirror>`, `session: Option<AndroidSession>`, `Scan` flags | No (derived each frame) | Starting, No adb, Phone list, Demo mirror, Live mirror | The window |
| Rotation | `main.rs` `Rotation` | Yes | Portrait, LandscapeLeft, LandscapeRight | Toolbar, shortcut |

### The Wi-Fi step (explicit)
```
any ──begin──▶ Busy ──succeed──▶ Done
                 └──────fail───▶ Failed{ next }      next: Nothing | Pair | Retry(addr)
```
- A spinner and a message can never show together, and a button can never outlive its error.
- The message and its button arrive in the same instant (before, "Try Again" was added one step
  after the error, so a frame could show the error without it).
- Two things sit outside the machine on purpose: `open_when_listed` (a request waiting for the
  scanner to list the phone, not a phase) and `error_shown_at` (a timer for the 0.8 s
  error-to-error rule, carried across steps). A new step does not clear either, as before.

### Live video control (documented, not converted)
Three flags make five possible combinations, three of which are real:

| `up` | `live` | Meaning | Screenshot workers |
|---|---|---|---|
| false | false | Off: no ffmpeg, gave up, between runs, or waiting out a screen-off retry (10 s) | All four run |
| true | false | Up but still: a run is active, no fresh picture for a moment (a still screen sends nothing; a dead connection looks the same) | Worker 0 keeps watch (it can trigger "connection interrupted"); the other three rest |
| true | true | Live: pictures arriving | All four rest |
| false | true | **Impossible** (every code path clears `live` with or after `up`) | n/a |

Moves, and who makes them (all in `stream.rs` unless noted):
- up on: stream thread, when a run starts. up and live off: when the run ends or fails to start.
- live on: stream thread, on each decoded picture. live off: watchdog thread, after 3 s without a picture.
- restart request (event, not a state): `devices.rs` sets it 0.9 s after the app rotates the phone;
  the watchdog consumes it and kills the helpers so the run restarts with the new picture shape.
- Hung run (180 s cap plus 15 s) and window close both kill adb and ffmpeg (`kill_all`).
- Worker rule, in `devices.rs`: rest when `live || (worker > 0 && up)`.

**Why it was left as flags.** It was hardened across four review passes on 2026-11-08, nobody has
yet driven the live window by hand, and its flags are shared across four kinds of thread. Turning
them into one enum is the right next step once someone has hand-tested the window (a single
`Phase` with `Off`, `UpStill`, `Live` would make the impossible row unrepresentable).

### Looked at and deliberately left alone
These are not hidden state machines, or are already as small as they should be:
- `show_manual_pair`, `show_manual_connect`, `phone_shaped`: independent checkboxes tied straight to
  buttons. Every combination is valid.
- `Mirror.press` (finger down, moving, up): documented under "Clicks become touches" above.
- `chrome_*` (controls fading in and out): a short animation re-derived every frame.
- `Scan.adb_found`, `scanned_once`, `network_ok`: readiness facts; their combinations are valid.
- `frame_count`, `shot_requested`: the test-mode picture taker.
