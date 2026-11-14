//! Live video from an Android phone.
//!
//! The phone's own `screenrecord` encodes the screen as H.264 and sends it over the adb
//! connection. `ffmpeg` decodes it into one JPEG per frame, which the app already knows how to
//! show. Measured on a Pixel 6 over Wi-Fi: about 54 frames a second while the screen moves,
//! against 6 to 9 for taking a screenshot per frame.
//!
//! Two limits of `screenrecord`, both handled here:
//! - it sends nothing while the screen is still (so a still phone costs no bandwidth), and
//! - it stops after 180 s, so the stream is restarted straight away, keeping the last picture.
//!
//! If `ffmpeg` is missing the caller never starts this; if the phone refuses (screen off), the
//! screenshot workers cover and the stream is retried every 10 s.

use crate::devices::{decode_picture, Frame};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long one `screenrecord` run may last (Android's hard cap is 180 s).
const RUN_SECONDS: u32 = 180;
/// Video quality sent over the cable or Wi-Fi.
const BIT_RATE: &str = "8M";
/// No picture for this long: hand over to one screenshot worker (a still screen sends nothing,
/// and a dead connection looks the same, so something has to look).
const STALE_AFTER: Duration = Duration::from_secs(3);
/// A run that outlives the 180 s cap by this much is hung (phone vanished): kill it.
const HUNG_AFTER: Duration = Duration::from_secs(RUN_SECONDS as u64 + 15);
/// A picture bigger than this is garbage, not a picture.
const MAX_BUFFER: usize = 32 << 20;

/// Shared state between the stream thread, its watchdog, the screenshot workers and the session.
#[derive(Default)]
pub struct Control {
    /// True while fresh stream pictures are arriving (screenshot workers rest).
    pub live: AtomicBool,
    /// True while a stream run is active, even if the screen is still: only one screenshot
    /// worker keeps watch then.
    pub up: AtomicBool,
    /// Ask the stream to start over (the phone rotated, so the picture shape changes).
    pub restart: AtomicBool,
    /// Milliseconds (since `epoch`) of the newest stream picture.
    last_ms: AtomicU64,
    epoch: Option<Instant>,
    kids: Mutex<Vec<Child>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Control {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { epoch: Some(Instant::now()), ..Default::default() })
    }
    fn now_ms(&self) -> u64 {
        self.epoch.map_or(0, |e| e.elapsed().as_millis() as u64)
    }
    fn since_last(&self) -> Duration {
        Duration::from_millis(self.now_ms().saturating_sub(self.last_ms.load(Ordering::Relaxed)))
    }
    /// Kill adb and ffmpeg right now (does not wait for them).
    pub fn kill_all(&self) {
        for c in lock(&self.kids).iter_mut() {
            let _ = c.kill();
        }
    }
    fn reap(&self) {
        let mut kids = lock(&self.kids);
        for c in kids.iter_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        kids.clear();
    }
}

/// Find ffmpeg: the usual Homebrew spots first, then the search path.
pub fn find_ffmpeg() -> Option<PathBuf> {
    ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .or_else(|| {
            let ok = Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().ok()?;
            ok.success().then(|| PathBuf::from("ffmpeg"))
        })
}

/// Pull complete JPEG pictures out of a growing byte buffer.
/// A JPEG starts with FF D8 and ends with FF D9 (inside the data, FF is always followed by 00).
pub fn take_jpegs(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let Some(start) = buf.windows(2).position(|w| w == [0xFF, 0xD8]) else {
            // Keep a trailing FF: its D8 may arrive in the next read.
            let keep = buf.last() == Some(&0xFF);
            buf.clear();
            if keep {
                buf.push(0xFF);
            }
            break;
        };
        if start > 0 {
            buf.drain(..start);
        }
        let Some(end) = buf[2..].windows(2).position(|w| w == [0xFF, 0xD9]).map(|p| p + 4) else {
            break;
        };
        out.push(buf.drain(..end).collect());
    }
    if buf.len() > MAX_BUFFER {
        buf.clear();
    }
    out
}

/// Stream the phone's screen into `latest` until `running` goes false.
/// Returns whether it ever delivered a picture. While it is not delivering, screenshots cover.
#[allow(clippy::too_many_arguments)]
pub fn run(
    adb: &Path,
    ffmpeg: &Path,
    serial: &str,
    latest: &Arc<Mutex<Option<Frame>>>,
    frames: &Arc<AtomicU32>,
    running: &Arc<AtomicBool>,
    ctl: &Arc<Control>,
    ctx: &eframe::egui::Context,
) -> bool {
    let mut ever_worked = false;
    let mut quick_failures = 0;
    let debug = std::env::var_os("PANE_DEBUG").is_some();
    while running.load(Ordering::Relaxed) {
        let began = Instant::now();
        // A rotation request from the gap between runs is already satisfied by this fresh start.
        ctl.restart.store(false, Ordering::Relaxed);
        let spawned = (|| -> Option<std::process::ChildStdout> {
            let mut rec = Command::new(adb)
                .args(["-s", serial, "exec-out", "screenrecord", "--output-format=h264", "--bit-rate", BIT_RATE])
                .args(["--time-limit", &RUN_SECONDS.to_string(), "-"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let Some(rec_out) = rec.stdout.take() else {
                let _ = rec.kill();
                let _ = rec.wait();
                return None;
            };
            let dec = Command::new(ffmpeg)
                // Not "-fflags nobuffer": measured, it makes ffmpeg throw away most frames (7 of 432).
                .args(["-v", "error", "-flags", "low_delay", "-probesize", "65536", "-analyzeduration", "0"])
                .args(["-f", "h264", "-framerate", "60", "-i", "-", "-fps_mode", "passthrough"])
                .args(["-q:v", "3", "-f", "image2pipe", "-c:v", "mjpeg", "-"])
                .stdin(rec_out)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let mut dec = match dec {
                Ok(d) => d,
                Err(_) => {
                    let _ = rec.kill();
                    let _ = rec.wait();
                    return None;
                }
            };
            let out = dec.stdout.take();
            // Register both so the watchdog and the session can kill them from outside.
            {
                let mut kids = lock(&ctl.kids);
                kids.push(rec);
                kids.push(dec);
            }
            out
        })();
        let Some(mut out) = spawned else {
            ctl.live.store(false, Ordering::Relaxed);
            ctl.up.store(false, Ordering::Relaxed);
            return ever_worked;
        };
        ctl.up.store(true, Ordering::Relaxed);
        ctl.last_ms.store(ctl.now_ms(), Ordering::Relaxed);
        // Watchdog: a blocked read cannot notice the window closing, a rotation, a hung phone
        // or a still screen, so this thread does (and kills the children to unblock the read).
        let done = Arc::new(AtomicBool::new(false));
        let restarted = Arc::new(AtomicBool::new(false));
        let dog = {
            let (ctl, running, done, restarted) = (ctl.clone(), running.clone(), done.clone(), restarted.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    if !running.load(Ordering::Relaxed) || began.elapsed() > HUNG_AFTER {
                        ctl.kill_all();
                        break;
                    }
                    if ctl.restart.swap(false, Ordering::Relaxed) {
                        restarted.store(true, Ordering::Relaxed);
                        ctl.kill_all();
                        break;
                    }
                    if ctl.since_last() > STALE_AFTER {
                        ctl.live.store(false, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
        };
        // The session may have stopped between the loop check and registering the children.
        if !running.load(Ordering::Relaxed) {
            ctl.kill_all();
        }
        let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
        let mut chunk = vec![0u8; 1 << 16];
        let mut got_any = false;
        let (mut seen, mut shown, mut tick) = (0usize, 0usize, Instant::now());
        while running.load(Ordering::Relaxed) {
            match out.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
            // Only the newest complete picture matters: older ones are already stale.
            let ready = take_jpegs(&mut buf);
            if debug {
                seen += ready.len();
                if tick.elapsed() >= Duration::from_secs(2) {
                    eprintln!("stream: {} pictures arrived, {} shown in the last 2 s", seen, shown);
                    seen = 0;
                    shown = 0;
                    tick = Instant::now();
                }
            }
            if let Some(newest) = ready.last() {
                if let Some(f) = decode_picture(newest) {
                    shown += 1;
                    *lock(latest) = Some(f);
                    frames.fetch_add(1, Ordering::Relaxed);
                    got_any = true;
                    ever_worked = true;
                    ctl.last_ms.store(ctl.now_ms(), Ordering::Relaxed);
                    ctl.live.store(true, Ordering::Relaxed);
                    ctx.request_repaint();
                }
            }
        }
        // The stream ended (180 s cap, rotation, or a failure). Hand over to screenshots for the
        // short gap before the next run so the window never freezes mid-scroll.
        ctl.live.store(false, Ordering::Relaxed);
        ctl.up.store(false, Ordering::Relaxed);
        done.store(true, Ordering::Relaxed);
        let _ = dog.join();
        ctl.reap();
        let was_restart = restarted.load(Ordering::Relaxed);
        if !got_any && !was_restart && began.elapsed() < Duration::from_secs(20) {
            quick_failures += 1;
            // Screen off, phone locked, cable blip: screenshots cover meanwhile. Wait a little
            // (longer after repeated failures), then try the stream again; a phone that was
            // asleep when the app opened starts streaming once it wakes.
            let wait = if quick_failures >= 2 { 100 } else { 10 };
            for _ in 0..wait {
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        } else {
            quick_failures = 0;
        }
    }
    ctl.live.store(false, Ordering::Relaxed);
    ctl.up.store(false, Ordering::Relaxed);
    ever_worked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_two_pictures_and_keeps_partial() {
        let mut b = vec![1, 2, 0xFF, 0xD8, 9, 9, 0xFF, 0xD9, 0xFF, 0xD8, 7, 0xFF, 0xD9, 0xFF, 0xD8, 5];
        let got = take_jpegs(&mut b);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], vec![0xFF, 0xD8, 9, 9, 0xFF, 0xD9]);
        assert_eq!(b, vec![0xFF, 0xD8, 5]);
    }

    #[test]
    fn keeps_a_split_start_marker() {
        let mut b = vec![7, 7, 0xFF];
        assert!(take_jpegs(&mut b).is_empty());
        assert_eq!(b, vec![0xFF]);
        b.extend([0xD8, 1, 0xFF, 0xD9]);
        assert_eq!(take_jpegs(&mut b).len(), 1);
    }

    #[test]
    fn garbage_without_an_end_cannot_grow_forever() {
        let mut b = vec![0xFF, 0xD8];
        b.resize(MAX_BUFFER + 1, 1);
        assert!(take_jpegs(&mut b).is_empty());
        assert!(b.is_empty());
    }

    #[test]
    fn kill_all_stops_registered_children() {
        let ctl = Control::new();
        let c = Command::new("sleep").arg("30").spawn().unwrap();
        lock(&ctl.kids).push(c);
        let t = Instant::now();
        ctl.kill_all();
        ctl.reap();
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(lock(&ctl.kids).is_empty());
    }
}
