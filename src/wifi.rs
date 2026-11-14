//! Android over Wi-Fi ("Wireless debugging", Android 11 and later).
//!
//! A phone must be paired with this Mac once (QR code or six-digit code), then it can connect.
//! Phones announce themselves on the network; `adb mdns services` lists them:
//!   _adb-tls-pairing._tcp  while the phone's pairing screen is open
//!   _adb-tls-connect._tcp  when Wireless debugging is on (ready to connect once paired)
//! Every network step runs on a background thread with a time limit, so the window never freezes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// Time limits from the original. ScreenSharingKit TimeoutPrimitives.defaultForBuildConfiguration
// (0x127A44) sets short 5 s, long 10 s, very long 60 s; ScreenContinuityUI 0x5f2ac guards each
// connection step with the long limit and a few special steps with the very long one.
pub const SHORT_TIMEOUT: Duration = Duration::from_secs(5);
pub const LONG_TIMEOUT: Duration = Duration::from_secs(10);
pub const VERY_LONG_TIMEOUT: Duration = Duration::from_secs(60);

/// Message timings from the original's start-up (main program 0x1000089dc): an error stays up at
/// least 0.8 s before another error replaces it, a finished step pauses 0.2 s before moving on,
/// and Try Again waits 0.75 s to let things settle. With the environment variable uiTesting=on
/// the original uses 0.1 s for all three (0x100001b0c); so does this app.
pub struct MessageDelays {
    pub error_to_error: Duration,
    pub loading_complete: Duration,
    pub retry_settling: Duration,
}

pub fn message_delays() -> MessageDelays {
    if std::env::var("uiTesting").map(|v| v == "on").unwrap_or(false) {
        let d = Duration::from_millis(100);
        MessageDelays { error_to_error: d, loading_complete: d, retry_settling: d }
    } else {
        MessageDelays {
            error_to_error: Duration::from_millis(800),
            loading_complete: Duration::from_millis(200),
            retry_settling: Duration::from_millis(750),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ServiceKind {
    Pairing,
    Connect,
}

#[derive(Clone, Debug)]
pub struct Service {
    pub name: String,
    pub kind: ServiceKind,
    pub addr: String, // ip:port
}

impl Service {
    pub fn ip(&self) -> &str {
        self.addr.rsplit_once(':').map(|(ip, _)| ip).unwrap_or(&self.addr)
    }
}

pub fn parse_mdns(text: &str) -> Vec<Service> {
    let mut out = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let kind = if parts[1].starts_with("_adb-tls-pairing") {
            ServiceKind::Pairing
        } else if parts[1].starts_with("_adb-tls-connect") || parts[1].starts_with("_adb._tcp") {
            ServiceKind::Connect
        } else {
            continue;
        };
        out.push(Service { name: parts[0].to_string(), kind, addr: parts[2].to_string() });
    }
    out
}

pub fn scan_mdns(adb: &Path) -> Vec<Service> {
    match run_limited(adb, &["mdns", "services"], SHORT_TIMEOUT) {
        Ok(text) => parse_mdns(&text),
        Err(_) => Vec::new(),
    }
}

/// Run adb with a time limit; returns what it printed (stdout and stderr together).
pub fn run_limited(adb: &Path, args: &[&str], limit: Duration) -> Result<String, String> {
    let mut child = Command::new(adb)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let mut text = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut text);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut text);
    }
    Ok(text.trim().to_string())
}

/// Accept "192.168.1.20:41234", "192.168.1.20 41234" or a bare IP (port 5555, the older method).
pub fn normalize_addr(input: &str) -> Option<String> {
    let s = input.trim().replace(' ', ":").replace("::", ":");
    if s.is_empty() {
        return None;
    }
    if s.contains(':') {
        Some(s)
    } else {
        Some(format!("{s}:5555"))
    }
}

/// What the Wi-Fi controls should offer after a step failed: one button per error, like the
/// original (AppErrorAction, 0x49260).
#[derive(Clone, Debug, PartialEq)]
pub enum Next {
    /// Just the message, no button.
    Nothing,
    /// The phone answered but doesn't trust this Mac yet: the button opens pairing.
    Pair,
    /// The step timed out or was refused: the button tries this address again.
    Retry(String),
}

/// Where the Wi-Fi step is right now. Exactly one of these at a time, so a spinner and an error
/// can never show together and a "Try Again" button can never outlive its error.
///
/// Moves (all through `JobState::begin` / `succeed` / `fail`, under the job's lock):
///   any state ---begin---> Busy        (a new step starts; the old message is gone)
///   Busy ---succeed---> Done           (green message)
///   Busy ---fail---> Failed            (orange message plus at most one button)
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Step {
    #[default]
    Idle,
    Busy(String),
    Done(String),
    Failed { text: String, next: Next },
}

impl Step {
    pub fn is_busy(&self) -> bool {
        matches!(self, Step::Busy(_))
    }
    pub fn needs_pairing(&self) -> bool {
        matches!(self, Step::Failed { next: Next::Pair, .. })
    }
}

/// The state of the last Wi-Fi step, shown under the Wi-Fi controls.
#[derive(Clone, Default)]
pub struct JobState {
    pub step: Step,
    /// A phone just connected over Wi-Fi: open its screen as soon as it appears in the list.
    /// Holds its address and, when known, the name it announces on the network. This is a
    /// request waiting for the scanner, not a phase, so it lives outside `step`.
    pub open_when_listed: Option<(String, Option<String>)>,
    /// When the last error message appeared (for the 0.8 s error-to-error rule). A timer
    /// carried across steps, so also outside `step`.
    pub error_shown_at: Option<Instant>,
}

impl JobState {
    /// A new step starts.
    pub fn begin(&mut self, text: &str) {
        self.step = Step::Busy(text.to_string());
    }
    /// The step worked.
    pub fn succeed(&mut self, text: String) {
        self.step = Step::Done(text);
        self.error_shown_at = None;
    }
    /// The step failed; `next` is the one button to offer.
    pub fn fail(&mut self, text: String, next: Next) {
        self.step = Step::Failed { text, next };
        self.error_shown_at = Some(Instant::now());
    }
    /// A failure with no step behind it (for example, a blank address was typed).
    pub fn failed(text: &str) -> Self {
        let mut s = Self::default();
        s.fail(text.to_string(), Next::Nothing);
        s
    }
}

pub type Job = Arc<Mutex<JobState>>;

fn set_busy(job: &Job, text: &str, ctx: &eframe::egui::Context) {
    job.lock().unwrap().begin(text);
    ctx.request_repaint();
}

fn finish_ok(job: &Job, text: String, ctx: &eframe::egui::Context) {
    job.lock().unwrap().succeed(text);
    ctx.request_repaint();
}

fn finish_err(job: &Job, text: String, next: Next, ctx: &eframe::egui::Context) {
    // An error doesn't replace another error until the first has been up 0.8 s.
    let since = job.lock().unwrap().error_shown_at.map(|t| t.elapsed());
    if let Some(since) = since {
        let wait = message_delays().error_to_error;
        if since < wait {
            std::thread::sleep(wait - since);
        }
    }
    // The message and its button arrive together, never one frame apart.
    job.lock().unwrap().fail(text, next);
    ctx.request_repaint();
}

/// After a step finishes, pause briefly (0.2 s) before moving on, then open the phone.
fn open_after_success(job: &Job, addr: String, name: Option<String>) {
    std::thread::sleep(message_delays().loading_complete);
    job.lock().unwrap().open_when_listed = Some((addr, name));
}

fn connect_once(adb: &Path, addr: &str) -> Result<(), String> {
    // One connection step: the original's long limit (10 s).
    let out = run_limited(adb, &["connect", addr], LONG_TIMEOUT).map_err(|_| "timed out".to_string())?;
    let low = out.to_lowercase();
    if low.contains("connected to") && !low.contains("failed") && !low.contains("cannot") {
        Ok(())
    } else {
        Err(out)
    }
}

/// How long a phone stays listed after it was last seen on the network. A connect address is
/// kept one long step (10 s); a pairing box only one short step (5 s), since it closes once paired.
pub fn sticky_for(kind: &ServiceKind) -> Duration {
    match kind {
        ServiceKind::Connect => LONG_TIMEOUT,
        ServiceKind::Pairing => SHORT_TIMEOUT,
    }
}

/// Is this Mac on a network? (Asks the system which local address it would use; sends nothing.)
pub fn mac_has_network() -> bool {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("192.0.2.1:9")?;
            s.local_addr()
        })
        .map(|a| !a.ip().is_unspecified() && !a.ip().is_loopback())
        .unwrap_or(false)
}

/// Open macOS Wi-Fi settings (the original's "Settings" action, AppErrorAction 5-8).
pub fn open_wifi_settings() {
    let _ = Command::new("/usr/bin/open").arg("x-apple.systempreferences:com.apple.wifi-settings-extension").status();
}

/// Does anything answer at this address? A phone that answers but then refuses adb's
/// secure handshake (adb logs "CERTIFICATE_UNKNOWN") simply hasn't been paired with this Mac.
fn port_answers(addr: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    addr.to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .map(|sa| TcpStream::connect_timeout(&sa, SHORT_TIMEOUT).is_ok())
        .unwrap_or(false)
}

/// Explain a failed connect in plain words.
fn connect_advice(addr: &str, raw: &str) -> String {
    let low = raw.to_lowercase();
    if raw == "timed out" {
        // The original's wording for a step that ran past its limit.
        return format!("Timed out connecting to {addr}. Make sure the phone is unlocked, on the same Wi-Fi, and Wireless debugging is still on.");
    }
    let why = if addr.ends_with(":5555") {
        "Port 5555 only works after the phone was switched to Wi-Fi from a USB cable. With Wireless debugging, use the address and port shown on that screen."
    } else if low.contains("refused") {
        "The phone refused it: the port changes every time Wireless debugging is turned on, so check the number on the phone."
    } else {
        "This Mac isn't paired with the phone yet (or the pairing was reset). Pair it below first."
    };
    format!("Couldn't connect to {addr}. {why}")
}

/// Connect to a phone that is already paired.
pub fn start_connect(adb: PathBuf, addr: String, name: Option<String>, job: Job, ctx: eframe::egui::Context) {
    connect_job(adb, addr, name, job, ctx, false);
}

/// Try Again: wait 0.75 s for things to settle first, as the original does.
pub fn start_retry(adb: PathBuf, addr: String, job: Job, ctx: eframe::egui::Context) {
    connect_job(adb, addr, None, job, ctx, true);
}

fn connect_job(adb: PathBuf, addr: String, name: Option<String>, job: Job, ctx: eframe::egui::Context, settle: bool) {
    set_busy(&job, &if settle { "Trying again…".to_string() } else { format!("Connecting to {addr}…") }, &ctx);
    std::thread::spawn(move || match {
        if settle {
            std::thread::sleep(message_delays().retry_settling);
            set_busy(&job, &format!("Connecting to {addr}…"), &ctx);
        }
        connect_once(&adb, &addr)
    } {
        Ok(()) => {
            finish_ok(&job, format!("Connected to {addr}. Opening its screen…"), &ctx);
            open_after_success(&job, addr, name);
        }
        Err(raw) if raw != "timed out" && !addr.ends_with(":5555") && port_answers(&addr) => {
            // Reached the phone, but it doesn't know this Mac: go straight to pairing.
            finish_err(
                &job,
                "Your phone is there, but it hasn't been paired with this Mac yet. Pair it once (below), and it will connect.".into(),
                Next::Pair,
                &ctx,
            );
        }
        Err(raw) => {
            let advice = connect_advice(&addr, &raw);
            finish_err(&job, advice, Next::Retry(addr), &ctx);
        }
    });
}

/// Pair with a phone (six-digit code or the QR password), then connect to it.
pub fn start_pair(adb: PathBuf, pair_addr: String, code: String, job: Job, ctx: eframe::egui::Context) {
    set_busy(&job, &format!("Pairing with {pair_addr}…"), &ctx);
    std::thread::spawn(move || {
        let out = match run_limited(&adb, &["pair", &pair_addr, code.trim()], VERY_LONG_TIMEOUT) {
            Ok(o) => o,
            Err(_) => {
                // The original reports a timed-out pairing with the device's name and lets the
                // person try again (timeoutReached, 0xcfd98).
                return finish_err(&job, format!("Timed out pairing with {pair_addr}. Make sure the phone is unlocked, on the same Wi-Fi, and its pairing screen is still open, then pair again."), Next::Nothing, &ctx)
            }
        };
        if !out.to_lowercase().contains("successfully paired") {
            let why = if out.to_lowercase().contains("wrong password") || out.to_lowercase().contains("protocol fault") {
                "The code didn't match. Codes change each time the pairing screen opens."
            } else {
                "Check the address and code on the phone's pairing screen."
            };
            return finish_err(&job, format!("Pairing failed. {why}"), Next::Nothing, &ctx);
        }
        // Paired. The phone now announces a connect address on the same IP; find it and connect.
        set_busy(&job, "Paired. Connecting…", &ctx);
        let ip = pair_addr.rsplit_once(':').map(|(ip, _)| ip.to_string()).unwrap_or_default();
        // Waiting for the phone to come back: one long step (10 s), as the original.
        let waited = Instant::now();
        while waited.elapsed() < LONG_TIMEOUT {
            let services = scan_mdns(&adb);
            if let Some(s) = services.iter().find(|s| s.kind == ServiceKind::Connect && s.ip() == ip) {
                if connect_once(&adb, &s.addr).is_ok() {
                    finish_ok(&job, format!("Paired and connected to {}. Opening its screen…", s.addr), &ctx);
                    open_after_success(&job, s.addr.clone(), Some(s.name.clone()));
                    return;
                }
            }
            // adb also connects paired phones by itself; stop when it shows up.
            if let Ok(list) = run_limited(&adb, &["devices"], SHORT_TIMEOUT) {
                if list.lines().skip(1).any(|l| l.contains(&ip) || l.contains("_adb-tls-connect")) && list.contains("\tdevice") {
                    finish_ok(&job, "Paired and connected. Opening its screen…".into(), &ctx);
                    open_after_success(&job, format!("{ip}:0"), None);
                    return;
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        finish_ok(
            &job,
            "Paired. If the phone doesn't appear in a few seconds, type the address from the Wireless debugging screen and press Connect.".into(),
            &ctx,
        )
    });
}

/// Name and password for the "Pair device with QR code" screen, made fresh each time.
pub fn new_qr_identity() -> (String, String) {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ (std::process::id() as u128) << 64;
    let mut x = seed as u64 ^ (seed >> 64) as u64 | 1;
    let mut next = |n: usize| {
        let alphabet = b"abcdefghijkmnpqrstuvwxyz23456789";
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                alphabet[(x % alphabet.len() as u64) as usize] as char
            })
            .collect::<String>()
    };
    (format!("pane-{}", next(6)), next(10))
}

/// The text the phone's camera reads (Android's own format for ADB pairing).
pub fn qr_payload(name: &str, password: &str) -> String {
    format!("WIFI:T:ADB;S:{name};P:{password};;")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_services() {
        let t = "List of discovered mdns services\nadb-1A10-tWX\t_adb-tls-connect._tcp\t192.168.1.77:34897\nstudio-abc\t_adb-tls-pairing._tcp\t192.168.1.77:40001\n";
        let s = parse_mdns(t);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].kind, ServiceKind::Connect);
        assert_eq!(s[1].kind, ServiceKind::Pairing);
        assert_eq!(s[1].ip(), "192.168.1.77");
    }
    #[test]
    fn message_timings_match_original() {
        // main program 0x1000089dc: 0.8 s, 0.2 s, 0.75 s outside UI testing.
        if std::env::var("uiTesting").is_err() {
            let d = message_delays();
            assert_eq!(d.error_to_error, Duration::from_millis(800));
            assert_eq!(d.loading_complete, Duration::from_millis(200));
            assert_eq!(d.retry_settling, Duration::from_millis(750));
        }
    }
    #[test]
    fn normalizes() {
        assert_eq!(normalize_addr("10.0.0.5").unwrap(), "10.0.0.5:5555");
        assert_eq!(normalize_addr(" 10.0.0.5 4123 ").unwrap(), "10.0.0.5:4123");
    }
}

#[cfg(test)]
mod step_tests {
    use super::*;

    #[test]
    fn starts_idle_with_nothing_to_show() {
        let j = JobState::default();
        assert_eq!(j.step, Step::Idle);
        assert!(!j.step.is_busy() && !j.step.needs_pairing());
    }

    #[test]
    fn begin_wipes_the_old_message_and_its_button() {
        let mut j = JobState::default();
        j.fail("no".into(), Next::Retry("1.2.3.4:5".into()));
        j.begin("Connecting…");
        assert_eq!(j.step, Step::Busy("Connecting…".into()));
        assert!(j.step.is_busy());
        // The old retry address is gone with it: nothing can offer "Try Again" while busy.
        assert!(!matches!(j.step, Step::Failed { .. }));
    }

    #[test]
    fn failure_carries_exactly_one_button() {
        let mut j = JobState::default();
        j.begin("x");
        j.fail("pair first".into(), Next::Pair);
        assert!(j.step.needs_pairing());
        j.begin("x");
        j.fail("timed out".into(), Next::Retry("a:1".into()));
        assert!(!j.step.needs_pairing());
        assert_eq!(j.step, Step::Failed { text: "timed out".into(), next: Next::Retry("a:1".into()) });
    }

    #[test]
    fn success_has_no_button_and_clears_the_error_timer() {
        let mut j = JobState::default();
        j.fail("bad".into(), Next::Pair);
        assert!(j.error_shown_at.is_some());
        j.begin("again");
        j.succeed("ok".into());
        assert_eq!(j.step, Step::Done("ok".into()));
        assert!(j.error_shown_at.is_none());
        assert!(!j.step.needs_pairing());
    }

    #[test]
    fn typed_blank_address_is_a_plain_failure() {
        let j = JobState::failed("Type the IP address");
        assert_eq!(j.step, Step::Failed { text: "Type the IP address".into(), next: Next::Nothing });
        assert!(j.error_shown_at.is_some());
    }

    #[test]
    fn a_pending_open_survives_a_new_step_as_before() {
        // open_when_listed is a request waiting for the scanner, not a phase: starting another
        // step must not drop it (this matches the behaviour before the change).
        let mut j = JobState::default();
        j.open_when_listed = Some(("1.2.3.4:5".into(), None));
        j.begin("x");
        assert!(j.open_when_listed.is_some());
    }
}
