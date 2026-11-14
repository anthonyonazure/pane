//! Talking to phones: Android through the `adb` tool, iPhone through macOS.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Find the Android `adb` tool: PATH first, then the usual install places.
pub fn find_adb() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join("adb");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        "/opt/homebrew/bin/adb".to_string(),
        "/usr/local/bin/adb".to_string(),
        format!("{home}/Library/Android/sdk/platform-tools/adb"),
    ];
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

#[derive(Clone, Debug, PartialEq)]
pub enum AndroidState {
    Ready,
    Unauthorized,
    Offline,
    Other(String),
}

#[derive(Clone, Debug)]
pub struct AndroidDevice {
    pub serial: String,
    pub model: String,
    pub state: AndroidState,
    pub wireless: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Scan {
    pub adb_found: bool,
    pub android: Vec<AndroidDevice>,
    /// Names of iPhones seen on USB (wireless ones are found by Apple's app itself).
    pub iphones_usb: Vec<String>,
    /// Phones announcing Wireless debugging on the network.
    pub wifi: Vec<crate::wifi::Service>,
    /// Whether this Mac is on a network at all (the original keeps its pairing button
    /// greyed out until the network comes back, ScreenContinuityUI 0xcfd98).
    pub network_ok: bool,
    pub scanned_once: bool,
}

fn parse_adb_devices(text: &str) -> Vec<AndroidDevice> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let mut parts = line.split_whitespace();
        let Some(serial) = parts.next() else { continue };
        let Some(state) = parts.next() else { continue };
        let mut model = String::new();
        for kv in parts {
            if let Some(m) = kv.strip_prefix("model:") {
                model = m.replace('_', " ");
            }
        }
        let state = match state {
            "device" => AndroidState::Ready,
            "unauthorized" => AndroidState::Unauthorized,
            "offline" => AndroidState::Offline,
            s => AndroidState::Other(s.to_string()),
        };
        if model.is_empty() {
            model = "Android phone".into();
        }
        out.push(AndroidDevice {
            wireless: serial.contains(':') || serial.contains("._adb-tls"),
            serial: serial.to_string(),
            model,
            state,
        });
    }
    out
}

fn scan_iphones_usb() -> Vec<String> {
    let Ok(out) = Command::new("/usr/sbin/ioreg").args(["-p", "IOUSB", "-l", "-w0"]).output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut names = Vec::new();
    for line in text.lines() {
        if let Some(idx) = line.find("\"USB Product Name\" = \"") {
            let rest = &line[idx + 22..];
            if let Some(end) = rest.find('"') {
                let name = &rest[..end];
                if name.starts_with("iPhone") && !names.contains(&name.to_string()) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// Is this a phone listed by its announced name ("adb-<serial>-…") that is already listed by address?
fn is_duplicate_named(serial: &str, serials_by_address: &[String]) -> bool {
    serial
        .strip_prefix("adb-")
        .and_then(|rest| rest.split('-').next())
        .map(|sn| serials_by_address.iter().any(|a| a == sn))
        .unwrap_or(false)
}

/// Background watcher that keeps the device list fresh (every 2 seconds, or at once when
/// `wake` is set, e.g. when the app becomes active again).
pub fn spawn_scanner(adb: Option<PathBuf>, ctx: eframe::egui::Context, wake: Arc<AtomicBool>) -> Arc<Mutex<Scan>> {
    let shared = Arc::new(Mutex::new(Scan::default()));
    let s2 = shared.clone();
    // Phones blink in and out of the network listing (we saw it go empty between scans), so a
    // phone stays listed for a while after it was last seen; otherwise the screen jumps.
    let mut seen: Vec<(crate::wifi::Service, std::time::Instant)> = Vec::new();
    // Serial numbers of phones connected by address, asked once each (to spot duplicates).
    let mut serials: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    std::thread::spawn(move || loop {
        let mut scan = Scan { adb_found: adb.is_some(), scanned_once: true, ..Default::default() };
        if let Some(adb) = &adb {
            if let Ok(out) = Command::new(adb).args(["devices", "-l"]).stderr(Stdio::null()).output() {
                scan.android = parse_adb_devices(&String::from_utf8_lossy(&out.stdout));
                // One phone can be connected twice: by address (192.168.1.77:34897) and by the
                // name it announces (adb-<serial>-xxxx._adb-tls-connect._tcp). Show it once.
                for d in scan.android.iter().filter(|d| d.state == AndroidState::Ready && d.serial.contains(':')) {
                    if !serials.contains_key(&d.serial) {
                        if let Ok(sn) = crate::wifi::run_limited(adb, &["-s", &d.serial, "shell", "getprop", "ro.serialno"], crate::wifi::SHORT_TIMEOUT) {
                            serials.insert(d.serial.clone(), sn.trim().to_string());
                        }
                    }
                }
                let by_address: Vec<String> = scan
                    .android
                    .iter()
                    .filter_map(|d| serials.get(&d.serial))
                    .filter(|sn| !sn.is_empty())
                    .cloned()
                    .collect();
                scan.android.retain(|d| !is_duplicate_named(&d.serial, &by_address));
            }
            let now = std::time::Instant::now();
            for svc in crate::wifi::scan_mdns(adb) {
                seen.retain(|(old, _)| !(old.name == svc.name && old.kind == svc.kind));
                seen.push((svc, now));
            }
            seen.retain(|(svc, t)| now.duration_since(*t) < crate::wifi::sticky_for(&svc.kind));
            scan.wifi = seen.iter().map(|(svc, _)| svc.clone()).collect();
        }
        scan.network_ok = crate::wifi::mac_has_network();
        scan.iphones_usb = scan_iphones_usb();
        *s2.lock().unwrap() = scan;
        ctx.request_repaint();
        for _ in 0..20 {
            if wake.swap(false, Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    shared
}

/// Open Apple's own iPhone Mirroring app.
pub fn open_iphone_mirroring() -> bool {
    Command::new("/usr/bin/open")
        .args(["-b", "com.apple.ScreenContinuity"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// One picture of the phone's screen, as RGBA bytes.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

const FORMAT_JPEG: u8 = 0;
const FORMAT_PNG: u8 = 1;
const FORMAT_RAW: u8 = 2;
/// How many picture fetches run at once. Measured on a Pixel 6 over Wi-Fi with JPEG: 1 → 2.5,
/// 2 → 4.5, 3 → 7.6, 4 → 9.0, 5 → 10.6 pictures a second. Four balances speed against the
/// phone's battery and heat.
const CAPTURE_WORKERS: usize = 4;

/// Decode whatever `screencap` sent: JPEG, PNG, or its raw format.
pub fn decode_picture(data: &[u8]) -> Option<Frame> {
    let compressed = data.starts_with(&[0xFF, 0xD8]) || data.starts_with(&[0x89, b'P', b'N', b'G']);
    if compressed {
        let img = image::load_from_memory(data).ok()?.to_rgba8();
        let (w, h) = img.dimensions();
        return Some(Frame { width: w as usize, height: h as usize, rgba: img.into_raw() });
    }
    decode_screencap(data)
}

/// Decode the raw output of `screencap` (no PNG): a small header, then pixels.
pub fn decode_screencap(data: &[u8]) -> Option<Frame> {
    if data.len() < 12 {
        return None;
    }
    let rd = |i: usize| u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
    let (w, h, fmt) = (rd(0), rd(4), rd(8));
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return None;
    }
    let bpp = match fmt {
        1 | 2 | 5 => 4,
        3 => 3,
        4 => 2,
        _ => 4,
    };
    let need = w * h * bpp;
    let header = if data.len() >= 16 + need { 16 } else if data.len() >= 12 + need { 12 } else { return None };
    let px = &data[header..header + need];
    let mut rgba = vec![255u8; w * h * 4];
    match fmt {
        5 => {
            for (o, i) in rgba.chunks_exact_mut(4).zip(px.chunks_exact(4)) {
                o[0] = i[2];
                o[1] = i[1];
                o[2] = i[0];
            }
        }
        3 => {
            for (o, i) in rgba.chunks_exact_mut(4).zip(px.chunks_exact(3)) {
                o[..3].copy_from_slice(i);
            }
        }
        4 => {
            for (o, i) in rgba.chunks_exact_mut(4).zip(px.chunks_exact(2)) {
                let v = u16::from_le_bytes([i[0], i[1]]);
                o[0] = (((v >> 11) & 0x1f) << 3) as u8;
                o[1] = (((v >> 5) & 0x3f) << 2) as u8;
                o[2] = ((v & 0x1f) << 3) as u8;
            }
        }
        _ => {
            for (o, i) in rgba.chunks_exact_mut(4).zip(px.chunks_exact(4)) {
                o[..3].copy_from_slice(&i[..3]);
            }
        }
    }
    Some(Frame { width: w, height: h, rgba })
}

/// A live connection to one Android phone: pictures come in, taps and keys go out.
pub struct AndroidSession {
    pub serial: String,
    pub latest: Arc<Mutex<Option<Frame>>>,
    pub frames: Arc<AtomicU32>,
    pub failures: Arc<AtomicU32>,
    pub running: Arc<AtomicBool>,
    /// When the connection started (for measuring pictures per second).
    pub started: std::time::Instant,
    /// The phone's Android SDK level (0 until known).
    sdk: Arc<AtomicU32>,
    /// Screen density (dots per inch, Android's "wm density"); 0 until known.
    density: Arc<AtomicU32>,
    shell: Option<(Child, ChildStdin)>,
    adb: PathBuf,
    /// Live-stream controls (kills its adb and ffmpeg when the session ends).
    stream: Arc<crate::stream::Control>,
}

impl AndroidSession {
    pub fn start(adb: PathBuf, serial: String, ctx: eframe::egui::Context) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let frames = Arc::new(AtomicU32::new(0));
        let failures = Arc::new(AtomicU32::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let density = Arc::new(AtomicU32::new(0));
        let sdk = Arc::new(AtomicU32::new(0));
        {
            let (adb, serial, sdk) = (adb.clone(), serial.clone(), sdk.clone());
            std::thread::spawn(move || {
                if let Ok(o) = Command::new(&adb).args(["-s", &serial, "shell", "getprop", "ro.build.version.sdk"]).output() {
                    if let Ok(v) = String::from_utf8_lossy(&o.stdout).trim().parse::<u32>() {
                        sdk.store(v, Ordering::Relaxed);
                    }
                }
            });
        }
        {
            // Ask the phone for its density once: points = pixels * 160 / density.
            let (adb, serial, density) = (adb.clone(), serial.clone(), density.clone());
            std::thread::spawn(move || {
                if let Ok(o) = Command::new(&adb).args(["-s", &serial, "shell", "wm", "density"]).output() {
                    let text = String::from_utf8_lossy(&o.stdout);
                    let pick = |key: &str| {
                        text.lines()
                            .find(|l| l.contains(key))
                            .and_then(|l| l.rsplit(':').next())
                            .and_then(|v| v.trim().parse::<u32>().ok())
                    };
                    if let Some(d) = pick("Override density").or_else(|| pick("Physical density")) {
                        density.store(d, Ordering::Relaxed);
                    }
                }
            });
        }
        let stream = crate::stream::Control::new();
        {
            // Pictures arrive faster as JPEG: on a Pixel 6 over Wi-Fi a raw picture is 10 MB and
            // takes about 2 s, a PNG about 1.3 s, a JPEG about 0.4 s. Older phones without JPEG
            // fall back to PNG, then raw. Several fetches run side by side so a new picture lands
            // about four times as often; a picture older than the one on screen is dropped.
            let format = Arc::new(AtomicU8::new(FORMAT_JPEG));
            let started = Arc::new(AtomicU64::new(0));
            let shown = Arc::new(AtomicU64::new(0));
            // Live video is far faster (about 54 pictures a second against 6 to 9). While it is
            // delivering, the screenshot workers below rest; if it cannot start, they carry on.
            if std::env::var_os("PANE_NO_STREAM").is_none() {
                if let Some(ffmpeg) = crate::stream::find_ffmpeg() {
                    let (adb, serial, ctx) = (adb.clone(), serial.clone(), ctx.clone());
                    let (latest, frames, running, ctl) = (latest.clone(), frames.clone(), running.clone(), stream.clone());
                    std::thread::spawn(move || {
                        crate::stream::run(&adb, &ffmpeg, &serial, &latest, &frames, &running, &ctl, &ctx);
                    });
                }
            }
            for worker in 0..CAPTURE_WORKERS {
                let ctl = stream.clone();
                let (adb, serial, ctx) = (adb.clone(), serial.clone(), ctx.clone());
                let (latest, frames, failures, running) =
                    (latest.clone(), frames.clone(), failures.clone(), running.clone());
                let (format, started, shown) = (format.clone(), started.clone(), shown.clone());
                std::thread::spawn(move || {
                    // Spread the fetches evenly over one picture's time (about 0.4 s).
                    std::thread::sleep(Duration::from_millis(100 * worker as u64));
                    while running.load(Ordering::Relaxed) {
                        // Resting while the stream delivers; while it is up but quiet (still
                        // screen, or a dead link) only worker 0 keeps watch.
                        if ctl.live.load(Ordering::Relaxed) || (worker > 0 && ctl.up.load(Ordering::Relaxed)) {
                            std::thread::sleep(Duration::from_millis(250));
                            continue;
                        }
                        let fmt = format.load(Ordering::Relaxed);
                        let seq = started.fetch_add(1, Ordering::Relaxed) + 1;
                        let mut args = vec!["-s", serial.as_str(), "exec-out", "screencap"];
                        match fmt {
                            FORMAT_JPEG => args.push("-j"),
                            FORMAT_PNG => args.push("-p"),
                            _ => {}
                        }
                        let out = Command::new(&adb)
                            .args(&args)
                            .stderr(Stdio::null())
                            .stdout(Stdio::piped())
                            .spawn()
                            .and_then(|mut c| {
                                let mut buf = Vec::with_capacity(1 << 20);
                                c.stdout.take().unwrap().read_to_end(&mut buf)?;
                                c.wait()?;
                                Ok(buf)
                            });
                        match out.ok().and_then(|b| decode_picture(&b)) {
                            Some(f) => {
                                // Keep only pictures newer than the one already on screen, and never
                                // let a slow screenshot land on top of a live stream picture.
                                if !ctl.live.load(Ordering::Relaxed) && shown.fetch_max(seq, Ordering::Relaxed) < seq {
                                    *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
                                    frames.fetch_add(1, Ordering::Relaxed);
                                }
                                failures.store(0, Ordering::Relaxed);
                                ctx.request_repaint();
                            }
                            None => {
                                // This phone can't make that kind of picture: try the next kind.
                                if fmt < FORMAT_RAW {
                                    let _ = format.compare_exchange(fmt, fmt + 1, Ordering::Relaxed, Ordering::Relaxed);
                                } else {
                                    failures.fetch_add(1, Ordering::Relaxed);
                                }
                                ctx.request_repaint();
                                std::thread::sleep(Duration::from_millis(500));
                            }
                        }
                    }
                });
            }
        }
        let shell = Command::new(&adb)
            .args(["-s", &serial, "shell"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
            .and_then(|mut c| c.stdin.take().map(|s| (c, s)));
        Self { serial, latest, frames, failures, running, started: std::time::Instant::now(), sdk, density, shell, adb, stream }
    }

    pub fn density(&self) -> Option<f32> {
        match self.density.load(Ordering::Relaxed) {
            0 => None,
            d => Some(d as f32),
        }
    }

    /// Send one shell line to the phone (kept open, so it's quick).
    pub fn run(&mut self, cmd: &str) {
        let ok = self
            .shell
            .as_mut()
            .map(|(_, stdin)| stdin.write_all(format!("{cmd}\n").as_bytes()).and_then(|_| stdin.flush()).is_ok())
            .unwrap_or(false);
        if !ok {
            // The open shell broke: fall back to a one-off command.
            let _ = Command::new(&self.adb)
                .args(["-s", &self.serial, "shell", cmd])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }

    /// Live touches ("input motionevent") exist from Android 10 (SDK 29).
    pub fn live_touch(&self) -> bool {
        self.sdk.load(Ordering::Relaxed) >= 29
    }
    pub fn touch_down(&mut self, x: i32, y: i32) {
        self.run(&format!("input motionevent DOWN {x} {y}"));
    }
    pub fn touch_move(&mut self, x: i32, y: i32) {
        self.run(&format!("input motionevent MOVE {x} {y}"));
    }
    /// The touch was interrupted: Android drops it without treating it as a tap.
    pub fn touch_cancel(&mut self, x: i32, y: i32) {
        self.run(&format!("input motionevent CANCEL {x} {y}"));
    }
    pub fn touch_up(&mut self, x: i32, y: i32) {
        self.run(&format!("input motionevent UP {x} {y}"));
    }

    pub fn tap(&mut self, x: i32, y: i32) {
        self.run(&format!("input tap {x} {y}"));
    }
    pub fn swipe(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, ms: u32) {
        self.run(&format!("input swipe {x1} {y1} {x2} {y2} {ms}"));
    }
    pub fn key(&mut self, code: u32) {
        self.run(&format!("input keyevent {code}"));
    }
    pub fn text(&mut self, s: &str) {
        let mut esc = String::new();
        for c in s.chars() {
            match c {
                ' ' => esc.push_str("%s"),
                '\'' => esc.push_str("'\\''"),
                c => esc.push(c),
            }
        }
        self.run(&format!("input text '{esc}'"));
    }
    pub fn home(&mut self) {
        self.key(3);
    }
    pub fn app_switcher(&mut self) {
        self.key(187);
    }
    pub fn back(&mut self) {
        self.key(4);
    }
    pub fn search(&mut self) {
        self.run("am start -a android.intent.action.ASSIST");
    }
    pub fn quick_settings(&mut self) {
        self.run("cmd statusbar expand-settings");
    }
    pub fn notifications(&mut self) {
        self.run("cmd statusbar expand-notifications");
    }
    /// 0 portrait, 1 landscape left, 3 landscape right.
    pub fn set_rotation(&mut self, r: u32) {
        self.run("settings put system accelerometer_rotation 0");
        self.run(&format!("settings put system user_rotation {r}"));
        // screenrecord keeps the picture shape it started with: start it over once the phone
        // has turned.
        let stream = self.stream.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(900));
            stream.restart.store(true, Ordering::Relaxed);
        });
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.stream.kill_all();
        if let Some((mut child, stdin)) = self.shell.take() {
            drop(stdin);
            let _ = child.kill();
        }
    }
}

impl Drop for AndroidSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spots_duplicate_phone() {
        let by_addr = vec!["1A101FDF6005UW".to_string()];
        assert!(is_duplicate_named("adb-1A101FDF6005UW-tWXIX2._adb-tls-connect._tcp", &by_addr));
        assert!(!is_duplicate_named("192.168.1.77:34897", &by_addr));
        assert!(!is_duplicate_named("adb-OTHER-xx._adb-tls-connect._tcp", &by_addr));
    }
    #[test]
    fn parses_device_list() {
        let t = "List of devices attached\nR58M123 device usb:1-1 product:x model:Pixel_8 device:y\nabc unauthorized usb:2\n";
        let d = parse_adb_devices(t);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].model, "Pixel 8");
        assert_eq!(d[1].state, AndroidState::Unauthorized);
    }
    #[test]
    fn decodes_new_header() {
        let mut v = vec![];
        v.extend(2u32.to_le_bytes());
        v.extend(1u32.to_le_bytes());
        v.extend(1u32.to_le_bytes());
        v.extend(0u32.to_le_bytes());
        v.extend([1, 2, 3, 4, 5, 6, 7, 8]);
        let f = decode_screencap(&v).unwrap();
        assert_eq!((f.width, f.height), (2, 1));
        assert_eq!(&f.rgba[..4], &[1, 2, 3, 255]);
    }
}
