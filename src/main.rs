//! Pane: use an Android phone or an iPhone from your Mac.
//! Android is shown and controlled live in this window (through adb);
//! iPhone opens Apple's own mirroring app, since its connection is private.

mod devices;
mod stream;
mod wifi;

use devices::{AndroidDevice, AndroidSession, AndroidState, Frame, Scan};
use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

// Values from MECHANICS.md / mechanics.json.
// Function addresses are in ScreenContinuityUI (arm64e slice).
const CHROME_HIDE_DELAY: f32 = 2.0; // s, timer in 0xa7554
const CHROME_SPRING_RESPONSE: f32 = 0.28; // s, spring(0.28, damping 1.0) in 0xa7330
const ZOOM_STEP: f32 = 0.1; // 0xaff30
const MIN_SCALE: f32 = 0.5; // 0xaff30
const MAX_SCALE: f32 = 1.5; // 0xaff30
const DEFAULT_SCALE: f32 = 0.77; // Actual Size, 0xaff30
const SCREEN_MARGIN_W: f32 = 16.0; // visibleFrame.width - 16, 0xaff30
const SCREEN_MARGIN_H: f32 = 8.0; // visibleFrame.height - (toolbar + 8), 0xaff30
const DP_DENSITY: f32 = 160.0; // Android: points = pixels * 160 / density (Android's own rule)
const CORNER_RADIUS: f32 = 44.0; // pt at scale 1, estimated (the original asks the phone)
const MENU_BAR_H: f32 = 25.0; // macOS menu bar, estimated stand-in for visibleFrame

/// Finger movements go out at most this often while dragging. Measured on a Pixel 6: one
/// movement command takes about 0.04 s through the kept-open connection.
const TOUCH_MOVE_INTERVAL_MS: u64 = 40;

const BAR_H: f32 = 40.0; // toolbar height, estimated (the original measures it at run time)
const MIN_WINDOW_W: f32 = 360.0;
const PAD: f32 = SCREEN_MARGIN_W / 2.0;
const WELCOME_SIZE: [f32; 2] = [480.0, 820.0];

const ACCENT: Color32 = Color32::from_rgb(10, 132, 255);
const ANDROID_TINT: Color32 = Color32::from_rgb(61, 220, 132);
const BG: Color32 = Color32::from_rgb(30, 30, 32);
const CARD: Color32 = Color32::from_rgb(44, 44, 47);
const SUBTLE: Color32 = Color32::from_rgb(160, 160, 166);

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let screenshot = args.iter().position(|a| a == "--screenshot").and_then(|i| args.get(i + 1)).map(PathBuf::from);
    // The original shows its pretend phone when the environment has uiTesting=on
    // (main program 0x100001b0c); this app does the same, as well as with --demo.
    let demo = args.iter().any(|a| a == "--demo") || std::env::var("uiTesting").map(|v| v == "on").unwrap_or(false);
    let show_pairing = args.iter().any(|a| a == "--show-pairing");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Pane")
            .with_inner_size(WELCOME_SIZE)
            .with_min_inner_size([200.0, 300.0])
            // The original turns full screen off (NSFullScreenMenuItemEverywhere = false, main program 0x100004750).
            .with_maximize_button(false)
            // Open in front of other windows.
            .with_active(true),
        ..Default::default()
    };
    eframe::run_native(
        "Pane",
        options,
        Box::new(move |cc| Ok(Box::new(MirrorApp::new(cc, screenshot, demo, show_pairing)))),
    )
}

#[derive(PartialEq, Clone, Copy)]
enum PairTab {
    Qr,
    Code,
}

#[derive(PartialEq, Clone, Copy)]
enum Rotation {
    Portrait,
    LandscapeLeft,
    LandscapeRight,
}

struct Mirror {
    session: Option<AndroidSession>, // None in demo mode
    texture: Option<egui::TextureHandle>,
    frame_size: [usize; 2],
    seen_frames: u32,
    scale: f32,
    rotation: Rotation,
    last_activity: Instant,
    chrome_alpha: f32,
    chrome_target: f32,
    chrome_from: f32,
    chrome_since: Instant,
    density: f32,
    phone_shaped: bool,
    monitor: Option<Vec2>,
    press: Option<(Pos2, Instant)>,
    /// Where and when the last finger movement was sent (to space them out).
    last_move: (Pos2, Instant),
    /// How many commands were meant for the phone (also counted in demo mode; used by tests).
    commands_sent: u32,
    sized_for: Option<([usize; 2], f32)>,
    name: String,
}

struct MirrorApp {
    adb: Option<PathBuf>,
    scan: Arc<Mutex<Scan>>,
    /// Ask the scanner to look again right away.
    rescan: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the window had focus last frame (to notice the app becoming active).
    was_focused: bool,
    mirror: Option<Mirror>,
    status: Option<String>,
    wifi_address: String,
    pair_address: String,
    pair_code: String,
    wifi_job: wifi::Job,
    show_pairing: bool,
    pair_tab: PairTab,
    /// The person picked a pairing tab themselves (don't switch it for them).
    pair_tab_chosen: bool,
    show_manual_pair: bool,
    show_manual_connect: bool,
    qr: Option<(String, String)>,
    qr_tried: Option<String>,
    screenshot: Option<PathBuf>,
    frame_count: u32,
    /// Frames drawn so far; used to bring the window forward right after it appears.
    frames_drawn: u32,
    shot_requested: bool,
}

impl MirrorApp {
    fn new(cc: &eframe::CreationContext<'_>, screenshot: Option<PathBuf>, demo: bool, show_pairing: bool) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BG;
        visuals.window_fill = BG;
        visuals.selection.bg_fill = ACCENT;
        visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(70, 70, 75);
        cc.egui_ctx.set_visuals(visuals);
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        cc.egui_ctx.set_style(style);

        let adb = devices::find_adb();
        let rescan = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let scan = devices::spawn_scanner(adb.clone(), cc.egui_ctx.clone(), rescan.clone());
        let mut app = Self {
            adb,
            scan,
            rescan,
            was_focused: true,
            mirror: None,
            status: None,
            wifi_address: String::new(),
            pair_address: String::new(),
            pair_code: String::new(),
            wifi_job: Default::default(),
            show_pairing: false,
            pair_tab: PairTab::Qr,
            pair_tab_chosen: false,
            show_manual_pair: false,
            show_manual_connect: false,
            qr: None,
            qr_tried: None,
            screenshot,
            frame_count: 0,
            frames_drawn: 0,
            shot_requested: false,
        };
        if std::env::args().any(|a| a == "--pairing-code-tab") {
            app.pair_tab = PairTab::Code;
            app.pair_tab_chosen = true;
        }
        if show_pairing {
            app.show_pairing = true;
            app.qr = Some(wifi::new_qr_identity());
        }
        // --connect <serial>: open that phone straight away (for testing speed).
        let argv: Vec<String> = std::env::args().collect();
        if let Some(serial) = argv.iter().position(|a| a == "--connect").and_then(|i| argv.get(i + 1)) {
            if let Some(adb) = app.adb.clone() {
                let session = AndroidSession::start(adb, serial.clone(), cc.egui_ctx.clone());
                app.mirror = Some(Mirror::new(Some(session), serial.clone()));
            }
        }
        if demo {
            let mut m = Mirror::new(None, "Demo phone".into());
            m.density = DEMO_DENSITY;
            app.mirror = Some(m);
        }
        app
    }

    fn connect(&mut self, dev: &AndroidDevice, ctx: &egui::Context) {
        let Some(adb) = self.adb.clone() else { return };
        let session = AndroidSession::start(adb, dev.serial.clone(), ctx.clone());
        self.mirror = Some(Mirror::new(Some(session), dev.model.clone()));
        self.status = None;
        bring_to_front(ctx);
    }

    fn disconnect(&mut self, ctx: &egui::Context) {
        self.mirror = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(WELCOME_SIZE.into()));
        ctx.send_viewport_cmd(egui::ViewportCommand::Title("Pane".into()));
    }

    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.screenshot.clone() else { return };
        self.frame_count += 1;
        ctx.request_repaint();
        // Testing aid: PANE_MEASURE_SECS=N keeps the window up N seconds of connection
        // before the picture is taken, so pictures per second is measured over a fair stretch.
        let measure: f32 = std::env::var("PANE_MEASURE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let waited = self.mirror.as_ref().and_then(|m| m.session.as_ref()).map_or(measure, |s| s.started.elapsed().as_secs_f32());
        if self.frame_count >= 120 && waited >= measure && !self.shot_requested {
            self.shot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = shot {
            let [w, h] = image.size;
            let mut bytes = Vec::with_capacity(w * h * 4);
            for p in &image.pixels {
                bytes.extend_from_slice(&[p.r(), p.g(), p.b(), 255]);
            }
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = image::save_buffer(&path, &bytes, w as u32, h as u32, image::ColorType::Rgba8) {
                eprintln!("could not save screenshot: {e}");
            }
            if let Some(sess) = self.mirror.as_ref().and_then(|m| m.session.as_ref()) {
                let n = sess.frames.load(Ordering::Relaxed);
                let secs = sess.started.elapsed().as_secs_f32();
                eprintln!("pictures: {n} in {secs:.1}s = {:.1} per second", n as f32 / secs);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if self.frame_count > 600 + (measure * 240.0) as u32 {
            eprintln!("screenshot never arrived");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for MirrorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_screenshot(ctx);
        // Bring the window to the front once it is on screen (a launched program on macOS
        // otherwise stays behind whatever was in front), and again a moment later in case
        // the first request came before the window was ready.
        self.frames_drawn += 1;
        if self.frames_drawn == 2 || self.frames_drawn == 20 {
            bring_to_front(ctx);
        }
        if self.frames_drawn < 20 {
            ctx.request_repaint();
        }
        // Each time the app becomes active again the original starts a fresh check
        // (applicationWillBecomeActive, main program 0x100007ad8); here: look for phones at once.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if focused && !self.was_focused {
            self.rescan.store(true, Ordering::Relaxed);
        }
        self.was_focused = focused;

        let mut leave = false;
        if let Some(m) = self.mirror.as_mut() {
            leave = m.ui(ctx);
            // A phone that stopped answering for a while: back to the list.
            if let Some(s) = &m.session {
                if s.failures.load(Ordering::Relaxed) >= 6 {
                    self.status = Some(format!("Connection interrupted: {} stopped answering.", m.name));
                    leave = true;
                }
            }
        } else {
            self.welcome(ctx);
        }
        if leave {
            self.disconnect(ctx);
        }
    }
}

// ---------- Welcome / device list ----------

impl MirrorApp {
    fn welcome(&mut self, ctx: &egui::Context) {
        let scan = self.scan.lock().unwrap().clone();
        let mut connect_to: Option<AndroidDevice> = None;

        // A bar pinned to the top that never scrolls away, so opening iPhone Mirroring is
        // always one click, however long the Android list below gets.
        egui::TopBottomPanel::top("open-bar")
            .frame(egui::Frame::new().fill(CARD).inner_margin(egui::Margin::symmetric(16, 8)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    phone_glyph(ui, ACCENT);
                    ui.label(egui::RichText::new("iPhone").strong().color(Color32::WHITE));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(accent_button("Open iPhone Mirroring")).clicked() {
                            self.status = Some(open_iphone_message());
                        }
                    });
                });
            });

        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(24.0)).show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                // While pairing, the picture steps aside so the whole pairing panel fits.
                if !self.show_pairing {
                    ui.add_space(8.0);
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(220.0, 150.0), Sense::hover());
                    draw_illustration(ui.painter(), rect);
                }
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Pane").size(26.0).strong().color(Color32::WHITE));
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Use your Android phone or iPhone right from your Mac.")
                        .size(14.0)
                        .color(SUBTLE),
                );
                ui.add_space(if self.show_pairing { 8.0 } else { 18.0 });
            });

            if let Some(msg) = &self.status {
                notice(ui, Color32::from_rgb(255, 159, 10), msg);
                ui.add_space(8.0);
            }

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                section_title(ui, "Android");
                if !scan.adb_found {
                    card(ui, |ui| {
                        ui.label(egui::RichText::new("Android tools not installed").strong());
                        ui.label(
                            egui::RichText::new(
                                "Install them once in Terminal with: brew install android-platform-tools",
                            )
                            .color(SUBTLE),
                        );
                    });
                } else if !scan.scanned_once {
                    card(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Looking for phones…");
                        });
                    });
                } else if scan.android.is_empty() && !self.show_pairing {
                    card(ui, |ui| {
                        ui.label(egui::RichText::new("No Android phone found").strong());
                        ui.label(
                            egui::RichText::new(
                                "Plug in your phone with a USB cable and turn on USB debugging, or use Wi-Fi below.",
                            )
                            .color(SUBTLE),
                        );
                    });
                }
                for dev in &scan.android {
                    card(ui, |ui| {
                        ui.horizontal(|ui| {
                            phone_glyph(ui, ANDROID_TINT);
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(&dev.model).strong().color(Color32::WHITE));
                                let (txt, col) = match &dev.state {
                                    AndroidState::Ready => (
                                        if dev.wireless { "Ready over Wi-Fi" } else { "Ready over USB" }.to_string(),
                                        SUBTLE,
                                    ),
                                    AndroidState::Unauthorized => (
                                        "Unlock your phone and tap Allow on the USB debugging prompt.".to_string(),
                                        Color32::from_rgb(255, 159, 10),
                                    ),
                                    AndroidState::Offline => ("Offline. Unplug and plug in again.".to_string(), SUBTLE),
                                    AndroidState::Other(s) => (s.clone(), SUBTLE),
                                };
                                ui.label(egui::RichText::new(txt).size(12.0).color(col));
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let ok = dev.state == AndroidState::Ready;
                                if ui.add_enabled(ok, accent_button("Connect")).clicked() {
                                    connect_to = Some(dev.clone());
                                }
                            });
                        });
                    });
                }
                if scan.adb_found {
                    self.wifi_ui(ui, &scan, ctx);
                }

                ui.add_space(14.0);
                section_title(ui, "iPhone");
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        phone_glyph(ui, ACCENT);
                        ui.vertical(|ui| {
                            let title = scan.iphones_usb.first().cloned().unwrap_or_else(|| "Your iPhone".into());
                            ui.label(egui::RichText::new(title).strong().color(Color32::WHITE));
                            let sub = if scan.iphones_usb.is_empty() {
                                "Nearby, on the same Apple Account"
                            } else {
                                "Found on USB"
                            };
                            ui.label(egui::RichText::new(sub).size(12.0).color(SUBTLE));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add(accent_button("Open")).clicked() {
                                self.status = Some(open_iphone_message());
                            }
                        });
                    });
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Apple keeps the iPhone connection private, so iPhones open in Apple's own mirroring window.",
                        )
                        .size(11.5)
                        .color(SUBTLE),
                    );
                });
            });
        });

        // A phone that just connected over Wi-Fi opens by itself once it shows up in the list.
        if connect_to.is_none() {
            let pending = self.wifi_job.lock().unwrap().open_when_listed.clone();
            if let Some((addr, name)) = pending {
                let ip = addr.rsplit_once(':').map(|(ip, _)| ip.to_string()).unwrap_or(addr.clone());
                if let Some(dev) = scan.android.iter().find(|d| {
                    d.state == AndroidState::Ready
                        && (d.serial == addr
                            || d.serial.starts_with(&format!("{ip}:"))
                            || name.as_ref().is_some_and(|n| d.serial.starts_with(n.as_str()))
                            || (name.is_none() && addr.ends_with(":0") && d.serial.contains("_adb-tls-connect")))
                }) {
                    connect_to = Some(dev.clone());
                    self.wifi_job.lock().unwrap().open_when_listed = None;
                }
            }
        }
        if let Some(dev) = connect_to {
            self.connect(&dev, ctx);
        }
    }
}

// ---------- Android over Wi-Fi ----------

impl MirrorApp {
    fn wifi_ui(&mut self, ui: &mut egui::Ui, scan: &Scan, ctx: &egui::Context) {
        let Some(adb) = self.adb.clone() else { return };
        let job = self.wifi_job.lock().unwrap().clone();
        // No network: everything Wi-Fi waits (greyed out) until it comes back, as in 0xcfd98.
        let offline = scan.scanned_once && !scan.network_ok;
        let busy = job.step.is_busy() || offline;
        if offline {
            card(ui, |ui| {
                ui.label(egui::RichText::new("Wi-Fi Off").strong().color(Color32::WHITE));
                ui.label(egui::RichText::new("Turn on Mac Wi-Fi to connect.").color(SUBTLE));
                if ui.add(accent_button("Settings")).clicked() {
                    wifi::open_wifi_settings();
                }
            });
        }

        // Phones announcing Wireless debugging that aren't connected yet. This card always has
        // the same shape (a row per phone, or one "looking" row), so nothing below it jumps.
        let listed = |svc: &wifi::Service| {
            scan.android.iter().any(|d| d.serial.starts_with(&svc.name) || d.serial == svc.addr || d.serial.starts_with(svc.ip()))
        };
        let ready: Vec<&wifi::Service> =
            scan.wifi.iter().filter(|s| s.kind == wifi::ServiceKind::Connect && !listed(s)).collect();
        let pairing_box: Option<&wifi::Service> = scan
            .wifi
            .iter()
            .find(|s| s.kind == wifi::ServiceKind::Pairing && !s.name.starts_with("pane-") && !s.name.starts_with("phonemirror-"));

        card(ui, |ui| {
            ui.label(egui::RichText::new("Android over Wi-Fi").strong().color(Color32::WHITE));
            ui.label(
                egui::RichText::new("On the phone: Settings › System › Developer options › Wireless debugging (same Wi-Fi as this Mac).")
                    .size(12.0)
                    .color(SUBTLE),
            );
            ui.add_space(6.0);
            if ready.is_empty() {
                fixed_row(ui, |ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new("Looking for a phone with Wireless debugging on…").color(SUBTLE));
                });
            }
            for svc in &ready {
                fixed_row(ui, |ui| {
                    phone_glyph(ui, ANDROID_TINT);
                    ui.label(egui::RichText::new(format!("Android phone ({})", svc.ip())).color(Color32::WHITE));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(!busy, egui::Button::new("Pair")).clicked() {
                            self.open_pairing();
                        }
                        if ui.add_enabled(!busy, accent_button("Connect")).clicked() {
                            wifi::start_connect(adb.clone(), svc.addr.clone(), Some(svc.name.clone()), self.wifi_job.clone(), ctx.clone());
                        }
                    });
                });
            }
            ui.label(
                egui::RichText::new("First time? Pair once, then Connect works every time after.")
                    .size(12.0)
                    .color(SUBTLE),
            );

            // Open pairing by itself when the phone turned out to be unpaired, or when the
            // phone is showing its pairing box right now (then the code tab is the one to use).
            if (job.step.needs_pairing() || pairing_box.is_some()) && !self.show_pairing {
                self.open_pairing();
            }
            if pairing_box.is_some() && !self.pair_tab_chosen {
                self.pair_tab = PairTab::Code;
            }
            ui.add_space(4.0);
            let label = if self.show_pairing { "Hide pairing" } else { "Pair a phone over Wi-Fi…" };
            let pair_button = if self.show_pairing { egui::Button::new(label) } else { accent_button(label) };
            if ui.add(pair_button).clicked() {
                if self.show_pairing {
                    self.show_pairing = false;
                } else {
                    self.open_pairing();
                }
            }
            if self.show_pairing {
                ui.add_space(6.0);
                self.pairing_ui(ui, scan, pairing_box, &adb, busy, ctx);
            }

            // Typing an address is rarely needed: phones are found by themselves.
            ui.add_space(6.0);
            ui.checkbox(&mut self.show_manual_connect, "Connect by typing an address");
            if self.show_manual_connect {
                ui.label(
                    egui::RichText::new(
                        "Use the \"IP address & Port\" on the main Wireless debugging screen (not the one in the pairing box). The port number changes each time Wireless debugging is turned on.",
                    )
                    .size(12.0)
                    .color(SUBTLE),
                );
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.wifi_address)
                            .hint_text("e.g. 192.168.1.20:41234")
                            .desired_width(220.0),
                    );
                    if ui.add_enabled(!busy, egui::Button::new("Connect")).clicked() {
                        match wifi::normalize_addr(&self.wifi_address) {
                            Some(addr) => wifi::start_connect(adb.clone(), addr, None, self.wifi_job.clone(), ctx.clone()),
                            None => {
                                *self.wifi_job.lock().unwrap() =
                                    wifi::JobState::failed("Type the IP address and port shown on the Wireless debugging screen.")
                            }
                        }
                    }
                });
            }

            // Shown last, so a message appearing never pushes the boxes you are typing in.
            ui.add_space(6.0);
            // Result of the last step, with the original's one-button-per-error rule (0x49260).
            {
                // One match over the step: every state says exactly what it draws.
                match &job.step {
                    wifi::Step::Idle => {}
                    wifi::Step::Busy(text) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(text);
                        });
                    }
                    wifi::Step::Done(text) => {
                        ui.label(egui::RichText::new(text).color(ANDROID_TINT));
                    }
                    wifi::Step::Failed { text, next } => {
                        ui.label(egui::RichText::new(text).color(Color32::from_rgb(255, 159, 10)));
                        match next {
                            wifi::Next::Nothing => {}
                            wifi::Next::Pair => {
                                if ui.add_enabled(!busy, accent_button("Enable Phone")).clicked() {
                                    self.open_pairing();
                                }
                            }
                            wifi::Next::Retry(addr) => {
                                if ui.add_enabled(!busy, accent_button("Try Again")).clicked() {
                                    wifi::start_retry(adb.clone(), addr.clone(), self.wifi_job.clone(), ctx.clone());
                                }
                            }
                        }
                    }
                }
            }

        });
    }

    fn open_pairing(&mut self) {
        self.show_pairing = true;
        if self.qr.is_none() {
            self.qr = Some(wifi::new_qr_identity());
            self.qr_tried = None;
        }
    }

    fn pairing_ui(
        &mut self,
        ui: &mut egui::Ui,
        scan: &Scan,
        pairing_box: Option<&wifi::Service>,
        adb: &PathBuf,
        busy: bool,
        ctx: &egui::Context,
    ) {
        // Two ways to pair, one at a time (a shorter panel that doesn't move around).
        ui.horizontal(|ui| {
            if ui.selectable_label(self.pair_tab == PairTab::Qr, "  Scan a QR code  ").clicked() {
                self.pair_tab = PairTab::Qr;
                self.pair_tab_chosen = true;
            }
            if ui.selectable_label(self.pair_tab == PairTab::Code, "  Type a 6-digit code  ").clicked() {
                self.pair_tab = PairTab::Code;
                self.pair_tab_chosen = true;
            }
        });
        ui.add_space(6.0);

        let steps = |ui: &mut egui::Ui, lines: &[&str]| {
            egui::Frame::new()
                .fill(ACCENT.gamma_multiply(0.15))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for l in lines {
                        ui.label(*l);
                    }
                });
        };

        match self.pair_tab {
            PairTab::Qr => {
                steps(
                    ui,
                    &[
                        "1. On the phone open Wireless debugging and tap \"Pair device with QR code\".",
                        "2. Point that screen's scanner at the code below.",
                        "Don't use the Camera app: it only shows a link and can't pair.",
                    ],
                );
                ui.add_space(6.0);
                if let Some((name, password)) = self.qr.clone() {
                    ui.horizontal(|ui| {
                        draw_qr(ui, &wifi::qr_payload(&name, &password), 150.0);
                        ui.vertical(|ui| {
                            // The phone, once it has scanned, announces itself under our name.
                            let scanned = scan.wifi.iter().find(|s| s.kind == wifi::ServiceKind::Pairing && s.name == name);
                            status_slot(ui, |ui| {
                                if scanned.is_some() {
                                    ui.label(egui::RichText::new("Your phone scanned it. Pairing…").color(ANDROID_TINT));
                                } else {
                                    ui.horizontal(|ui| {
                                        ui.spinner();
                                        ui.label("Waiting for your phone to scan…");
                                    });
                                }
                            });
                            if ui.small_button("Make a new code").clicked() {
                                self.qr = Some(wifi::new_qr_identity());
                                self.qr_tried = None;
                            }
                            if let Some(svc) = scanned {
                                if !busy && self.qr_tried.as_deref() != Some(svc.addr.as_str()) {
                                    self.qr_tried = Some(svc.addr.clone());
                                    wifi::start_pair(adb.clone(), svc.addr.clone(), password.clone(), self.wifi_job.clone(), ctx.clone());
                                }
                            }
                        });
                    });
                }
            }
            PairTab::Code => {
                steps(
                    ui,
                    &[
                        "1. On the phone open Wireless debugging and tap \"Pair device with pairing code\".",
                        "2. A box shows a 6-digit code. Keep it open and type the code here.",
                        "You don't need the port: this Mac finds the pairing box by itself.",
                    ],
                );
                ui.add_space(6.0);
                // Follow the pairing box the phone announces (its port changes every time it opens).
                if let Some(svc) = pairing_box {
                    if !self.show_manual_pair {
                        self.pair_address = svc.addr.clone();
                    }
                }
                status_slot(ui, |ui| match pairing_box {
                    Some(svc) => {
                        ui.label(egui::RichText::new(format!("Found your phone's pairing box ({}).", svc.addr)).color(ANDROID_TINT));
                    }
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Looking for the pairing box on your phone…");
                        });
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.pair_code)
                            .hint_text("6-digit code")
                            .char_limit(6)
                            .desired_width(110.0),
                    );
                    let have_addr = !self.pair_address.trim().is_empty();
                    let ok = !busy && have_addr && self.pair_code.trim().len() == 6;
                    if ui.add_enabled(ok, accent_button("Pair")).clicked() {
                        if let Some(addr) = wifi::normalize_addr(&self.pair_address) {
                            wifi::start_pair(adb.clone(), addr, self.pair_code.trim().to_string(), self.wifi_job.clone(), ctx.clone());
                            self.pair_code.clear();
                        }
                    }
                });
                ui.checkbox(&mut self.show_manual_pair, "My phone isn't found: type the pairing box's address");
                if self.show_manual_pair {
                    ui.label(
                        egui::RichText::new(
                            "Use the \"IP address & Port\" printed inside the pairing box, under the code (not the one on the main Wireless debugging screen).",
                        )
                        .size(12.0)
                        .color(SUBTLE),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.pair_address)
                            .hint_text("e.g. 192.168.1.20:37123")
                            .desired_width(220.0),
                    );
                }
            }
        }
    }
}

/// A row of fixed height, so lists that change don't move what's below them.
fn fixed_row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(egui::vec2(width, 34.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(egui::vec2(width, 34.0));
        add(ui);
    });
}

/// A space of fixed height for a status message, so messages coming and going don't move things.
fn status_slot(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(egui::vec2(width, 26.0), egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.set_min_size(egui::vec2(width, 26.0));
        add(ui);
    });
}

fn draw_qr(ui: &mut egui::Ui, text: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(6), Color32::WHITE);
    let Ok(code) = qrcode::QrCode::new(text.as_bytes()) else { return };
    let n = code.width();
    let quiet = 2.0;
    let cell = size / (n as f32 + quiet * 2.0);
    for y in 0..n {
        for x in 0..n {
            if code[(x, y)] == qrcode::Color::Dark {
                let min = rect.min + egui::vec2((x as f32 + quiet) * cell, (y as f32 + quiet) * cell);
                p.rect_filled(Rect::from_min_size(min, egui::vec2(cell + 0.3, cell + 0.3)), CornerRadius::ZERO, Color32::BLACK);
            }
        }
    }
}

// ---------- Mirroring window ----------

impl Mirror {
    fn new(session: Option<AndroidSession>, name: String) -> Self {
        Self {
            session,
            texture: None,
            frame_size: [1080, 2400],
            seen_frames: 0,
            scale: DEFAULT_SCALE,
            rotation: Rotation::Portrait,
            last_activity: Instant::now(),
            chrome_alpha: 1.0,
            chrome_target: 1.0,
            chrome_from: 1.0,
            chrome_since: Instant::now(),
            density: 420.0,
            phone_shaped: true,
            monitor: None,
            press: None,
            last_move: (Pos2::ZERO, Instant::now()),
            commands_sent: 0,
            sized_for: None,
            name,
        }
    }

    /// The phone's own size in points (like the iPhone's "unscaled device size").
    fn unscaled_size(&self) -> Vec2 {
        egui::vec2(self.frame_size[0] as f32, self.frame_size[1] as f32) * DP_DENSITY / self.density
    }

    fn phone_size(&self) -> Vec2 {
        self.unscaled_size() * self.scale
    }

    /// The window must fit the screen: width - 16, height - (toolbar + 8); never below 0.5.
    fn fit_to_screen(&self, scale: f32) -> f32 {
        let Some(m) = self.monitor else { return scale };
        let (avail_w, avail_h) = (m.x - SCREEN_MARGIN_W, m.y - MENU_BAR_H - (BAR_H + SCREEN_MARGIN_H));
        let u = self.unscaled_size();
        if u.x * scale > avail_w || u.y * scale > avail_h {
            (avail_w / u.x).min(avail_h / u.y).max(MIN_SCALE)
        } else {
            scale
        }
    }

    fn can_zoom_in(&self) -> bool {
        self.scale < MAX_SCALE - 1e-3 && self.fit_to_screen(self.scale + ZOOM_STEP) > self.scale + 1e-3
    }
    fn can_zoom_out(&self) -> bool {
        self.scale > MIN_SCALE + 1e-3
    }

    fn pull_frame(&mut self, ctx: &egui::Context) {
        match &self.session {
            Some(s) => {
                let n = s.frames.load(Ordering::Relaxed);
                if n == self.seen_frames {
                    return;
                }
                self.seen_frames = n;
                let frame = s.latest.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(f) = frame {
                    self.upload(ctx, f);
                }
            }
            None => {
                if self.texture.is_none() {
                    let f = demo_frame(self.rotation != Rotation::Portrait);
                    self.upload(ctx, f);
                }
            }
        }
    }

    fn upload(&mut self, ctx: &egui::Context, f: Frame) {
        self.frame_size = [f.width, f.height];
        let img = egui::ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
        match &mut self.texture {
            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("phone-screen", img, egui::TextureOptions::LINEAR)),
        }
    }

    /// Zoom In: +0.1 up to 1.5, rounded up. Zoom Out: -0.1 down to 0.5, rounded down. (0xaff30)
    fn zoom(&mut self, delta: f32) {
        let raw = self.scale + delta;
        let stepped = if delta > 0.0 {
            ((raw * 10.0 - 1e-3).ceil() / 10.0).min(MAX_SCALE)
        } else {
            ((raw * 10.0 + 1e-3).floor() / 10.0).max(MIN_SCALE)
        };
        self.scale = self.fit_to_screen(stepped);
        self.phone_shaped = true;
    }

    /// Actual Size: back to 0.77 of the phone's own size. (0xaff30)
    fn actual_size(&mut self) {
        self.scale = self.fit_to_screen(DEFAULT_SCALE);
        self.phone_shaped = true;
    }

    fn rotate(&mut self, r: Rotation) {
        self.rotation = r;
        match &mut self.session {
            Some(s) => s.set_rotation(match r {
                Rotation::Portrait => 0,
                Rotation::LandscapeLeft => 1,
                Rotation::LandscapeRight => 3,
            }),
            None => self.texture = None,
        }
    }

    fn toggle_rotation(&mut self) {
        let next = match self.rotation {
            Rotation::Portrait => Rotation::LandscapeLeft,
            _ => Rotation::Portrait,
        };
        self.rotate(next);
    }

    /// "iPhone Size" (Cmd+9): switch between the phone's shape and a freely sized window.
    fn toggle_phone_size(&mut self) {
        self.phone_shaped = !self.phone_shaped;
        self.sized_for = None;
    }

    /// The mouse button came up: lift the finger (live touches), or on older phones send the
    /// whole tap, long press or swipe now.
    fn finish_touch(&mut self, start: Pos2, t0: Instant, end: Pos2, live: bool, to_phone: &dyn Fn(Pos2) -> (i32, i32)) {
        let (x2, y2) = to_phone(end);
        if live {
            if end != self.last_move.0 {
                self.act(|s| s.touch_move(x2, y2));
            }
            self.act(|s| s.touch_up(x2, y2));
            self.last_move = (end, Instant::now());
            return;
        }
        let held = t0.elapsed().as_millis() as u32;
        let (x1, y1) = to_phone(start);
        if (end - start).length() < 6.0 {
            if held > 500 {
                self.act(|s| s.swipe(x1, y1, x1, y1, held.min(2000)));
            } else {
                self.act(|s| s.tap(x1, y1));
            }
        } else {
            self.act(|s| s.swipe(x1, y1, x2, y2, held.clamp(80, 1500)));
        }
    }

    fn act(&mut self, f: impl FnOnce(&mut AndroidSession)) {
        self.commands_sent += 1;
        if let Some(s) = &mut self.session {
            f(s);
        }
    }

    /// Draw one frame of the mirror. Returns true when the person wants to leave.
    fn ui(&mut self, ctx: &egui::Context) -> bool {
        self.pull_frame(ctx);
        let mut leave = false;
        if let Some(d) = self.session.as_ref().and_then(|s| s.density()) {
            self.density = d;
        }
        self.monitor = ctx.input(|i| i.viewport().monitor_size);

        // Keep the window the size of the phone (plus the control bar).
        let want = (self.frame_size, (self.scale * 1000.0).round() / 1000.0);
        if self.phone_shaped && self.sized_for != Some(want) && self.texture.is_some() {
            self.scale = self.fit_to_screen(self.scale);
            self.sized_for = Some(want);
            let mut size = self.phone_size() + egui::vec2(PAD * 2.0, BAR_H + PAD);
            size.x = size.x.max(MIN_WINDOW_W);
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{} – Pane", self.name)));
        }

        // Keyboard: menu shortcuts, then typing goes to the phone.
        let events = ctx.input(|i| i.events.clone());
        for e in &events {
            match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } if modifiers.command => match key {
                    // Shortcuts from 0x4a020: Cmd+1/2/3/4, Cmd+= or +, Cmd+-, Cmd+0, Cmd+9, Cmd+8.
                    egui::Key::Num1 => self.act(|s| s.home()),
                    egui::Key::Num2 => self.act(|s| s.app_switcher()),
                    egui::Key::Num3 => self.act(|s| s.search()),
                    egui::Key::Num4 => self.act(|s| s.quick_settings()),
                    egui::Key::Equals | egui::Key::Plus => self.zoom(ZOOM_STEP),
                    egui::Key::Minus => self.zoom(-ZOOM_STEP),
                    egui::Key::Num0 => self.actual_size(),
                    egui::Key::Num9 => self.toggle_phone_size(),
                    egui::Key::Num8 => self.toggle_rotation(),
                    _ => {}
                },
                egui::Event::Key { key, pressed: true, modifiers, .. } if !modifiers.command => match key {
                    egui::Key::Enter => self.act(|s| s.key(66)),
                    egui::Key::Backspace => self.act(|s| s.key(67)),
                    egui::Key::Escape => self.act(|s| s.back()),
                    egui::Key::ArrowUp => self.act(|s| s.key(19)),
                    egui::Key::ArrowDown => self.act(|s| s.key(20)),
                    egui::Key::ArrowLeft => self.act(|s| s.key(21)),
                    egui::Key::ArrowRight => self.act(|s| s.key(22)),
                    _ => {}
                },
                egui::Event::Text(t) => {
                    let t = t.clone();
                    self.act(|s| s.text(&t));
                }
                _ => {}
            }
        }

        // Controls fade out 2 s after the pointer goes still (0.28 s fade).
        let moved = ctx.input(|i| i.pointer.delta() != Vec2::ZERO || i.pointer.any_down());
        if moved {
            self.last_activity = Instant::now();
        }
        let hovered_bar = ctx.input(|i| i.pointer.hover_pos().map(|p| p.y < BAR_H).unwrap_or(false));
        let show = CHROME_HIDE_DELAY == 0.0
            || hovered_bar
            || self.last_activity.elapsed().as_secs_f32() < CHROME_HIDE_DELAY;
        let target = if show { 1.0 } else { 0.0 };
        if target != self.chrome_target {
            self.chrome_from = self.chrome_alpha;
            self.chrome_target = target;
            self.chrome_since = Instant::now();
        }
        // Critically damped spring (damping 1.0) with a 0.28 s response, as in 0xa7330.
        let omega = std::f32::consts::TAU / CHROME_SPRING_RESPONSE;
        let t = self.chrome_since.elapsed().as_secs_f32();
        let progress = 1.0 - (1.0 + omega * t) * (-omega * t).exp();
        self.chrome_alpha = self.chrome_from + (self.chrome_target - self.chrome_from) * progress;
        if (self.chrome_alpha - target).abs() > 0.002 {
            ctx.request_repaint();
        } else if show {
            let left = CHROME_HIDE_DELAY - self.last_activity.elapsed().as_secs_f32();
            ctx.request_repaint_after(std::time::Duration::from_secs_f32(left.max(0.05)));
        }

        egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::from_rgb(18, 18, 20))).show(ctx, |ui| {
            let full = ui.max_rect();
            let bar = Rect::from_min_size(full.min, egui::vec2(full.width(), BAR_H));
            let area = Rect::from_min_max(egui::pos2(full.min.x + PAD, bar.max.y), egui::pos2(full.max.x - PAD, full.max.y - PAD));

            // Fit the phone inside the area, keeping its shape.
            let fs = egui::vec2(self.frame_size[0] as f32, self.frame_size[1] as f32);
            let fit = (area.width() / fs.x).min(area.height() / fs.y);
            let screen = Rect::from_center_size(area.center(), fs * fit);
            let radius = (CORNER_RADIUS * self.scale).min(screen.width() * 0.2);

            // Phone body.
            ui.painter().rect(
                screen.expand(3.0),
                CornerRadius::same((radius + 3.0) as u8),
                Color32::BLACK,
                Stroke::new(1.0, Color32::from_gray(70)),
                StrokeKind::Outside,
            );
            match &self.texture {
                Some(tex) => {
                    egui::Image::new((tex.id(), screen.size()))
                        .corner_radius(CornerRadius::same(radius as u8))
                        .paint_at(ui, screen);
                }
                None => {
                    ui.painter().rect_filled(screen, CornerRadius::same(radius as u8), Color32::from_gray(12));
                    ui.put(
                        Rect::from_center_size(screen.center(), egui::vec2(screen.width(), 60.0)),
                        egui::Label::new(egui::RichText::new(format!("Connecting to {}", self.name)).color(SUBTLE)),
                    );
                }
            }

            // Pointer on the screen becomes touch on the phone.
            let resp = ui.interact(screen, ui.id().with("screen"), Sense::click_and_drag());
            let to_phone = |p: Pos2| -> (i32, i32) {
                let rel = (p - screen.min) / screen.size();
                ((rel.x * fs.x).round() as i32, (rel.y * fs.y).round() as i32)
            };
            // Like the original (gesture states 1/2/3 in ScreenContinuityUI 0x4c118, 0x4c3c0,
            // 0x4bb00): the finger goes down the moment the mouse is pressed, follows every drag,
            // and lifts on release; nothing waits for the click to finish. Older phones without
            // live touches get a tap or swipe on release instead.
            //
            // Every press, movement and release is handled as its own event, in order, the way
            // the original receives each mouseDown / mouseDragged / mouseUp. A light trackpad
            // tap ("tap to click") sends press and release almost together, often inside one
            // redraw; checking "is the button held right now?" once per redraw missed those
            // taps, so only firm, longer clicks used to register.
            let live = self.session.as_ref().map(|s| s.live_touch()).unwrap_or(false);
            let menu_open = ctx.memory(|m| m.any_popup_open());
            let events = ctx.input(|i| i.events.clone());
            let mut right_ended_touch = false;
            let mut previous: Option<&egui::Event> = None;
            for event in &events {
                // The original queues each mouse event but skips an exact repeat of the last one
                // (ScreenContinuityUI 0x4b99c); so does this.
                if previous == Some(event) {
                    continue;
                }
                previous = Some(event);
                match event {
                    egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, .. }
                        if self.press.is_none() && screen.contains(*pos) && !menu_open =>
                    {
                        self.press = Some((*pos, Instant::now()));
                        self.last_move = (*pos, Instant::now());
                        if live {
                            let (x, y) = to_phone(*pos);
                            self.act(|s| s.touch_down(x, y));
                        }
                    }
                    egui::Event::PointerMoved(pos) if self.press.is_some() => {
                        // Follow the drag, at most one update per TOUCH_MOVE_INTERVAL so they don't pile up.
                        let p = pos.clamp(screen.min, screen.max);
                        if live
                            && p != self.last_move.0
                            && self.last_move.1.elapsed() >= std::time::Duration::from_millis(TOUCH_MOVE_INTERVAL_MS)
                        {
                            let (x, y) = to_phone(p);
                            self.act(|s| s.touch_move(x, y));
                            self.last_move = (p, Instant::now());
                        }
                    }
                    egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, .. } => {
                        if let Some((start, t0)) = self.press.take() {
                            self.finish_touch(start, t0, pos.clamp(screen.min, screen.max), live, &to_phone);
                        }
                    }
                    egui::Event::PointerButton { button: egui::PointerButton::Secondary, pressed: false, .. }
                        if self.press.is_some() =>
                    {
                        // A right-button release ends a touch in progress at the last known
                        // position (rightMouseUp → 0x4bb00 with "keep position", 0x4c2f0).
                        if let Some((start, t0)) = self.press.take() {
                            let last = self.last_move.0;
                            self.finish_touch(start, t0, last, live, &to_phone);
                            right_ended_touch = true;
                        }
                    }
                    egui::Event::PointerGone | egui::Event::WindowFocused(false) => {
                        // The pointer left the window or the app lost focus mid-press: the gesture
                        // failed. The original then drops what was queued and cancels the touch
                        // stream instead of finishing it (0xdba0c → 0xdf274), so nothing gets
                        // tapped by accident. Older phones had nothing sent yet: just forget it.
                        if self.press.take().is_some() && live {
                            let (x, y) = to_phone(self.last_move.0);
                            self.act(|s| s.touch_cancel(x, y));
                        }
                    }
                    _ => {}
                }
            }
            // Right-click on its own is Back (my Android mapping; the original ignores a lone
            // right press, rightMouseDown is empty). Not when it just ended a touch.
            if resp.secondary_clicked() && !right_ended_touch {
                self.act(|s| s.back());
            }
            if resp.hovered() {
                let scroll = ctx.input(|i| i.raw_scroll_delta);
                if scroll.length() > 30.0 {
                    if let Some(p) = resp.hover_pos() {
                        let (x, y) = to_phone(p);
                        let d = (scroll * fs.y / screen.height() * 0.6).clamp(Vec2::splat(-fs.y / 3.0), Vec2::splat(fs.y / 3.0));
                        let (dx, dy) = (d.x as i32, d.y as i32);
                        self.act(|s| s.swipe(x, y, x + dx, y + dy, 120));
                    }
                }
            }

            // Control bar (fades).
            if self.chrome_alpha > 0.01 {
                leave = self.control_bar(ui, bar);
            }
        });
        leave
    }

    fn control_bar(&mut self, ui: &mut egui::Ui, bar: Rect) -> bool {
        let mut leave = false;
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(bar.shrink2(egui::vec2(8.0, 6.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        child.set_opacity(self.chrome_alpha);
        let ui = &mut child;
        // A toolbar button; greyed out when it can't do anything (as the original's zoom buttons, 0x1d134).
        let btn = |ui: &mut egui::Ui, icon: Icon, tip: &str, enabled: bool| -> bool {
            let sense = if enabled { Sense::click() } else { Sense::hover() };
            let (r, resp) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), sense);
            let col = if !enabled {
                Color32::from_gray(90)
            } else if resp.hovered() {
                Color32::WHITE
            } else {
                Color32::from_gray(200)
            };
            if enabled && resp.hovered() {
                ui.painter().rect_filled(r, CornerRadius::same(7), Color32::from_white_alpha(28));
            }
            draw_icon(ui.painter(), r.shrink(6.0), icon, col);
            enabled && resp.on_hover_text(tip).clicked()
        };
        if btn(ui, Icon::Home, "Home Screen (⌘1)", true) {
            self.act(|s| s.home());
        }
        if btn(ui, Icon::Switcher, "App Switcher (⌘2)", true) {
            self.act(|s| s.app_switcher());
        }
        if btn(ui, Icon::Search, "Search (⌘3)", true) {
            self.act(|s| s.search());
        }
        if btn(ui, Icon::Settings, "Quick Settings (⌘4)", true) {
            self.act(|s| s.quick_settings());
        }
        let (zoom_in_ok, zoom_out_ok) = (self.can_zoom_in(), self.can_zoom_out());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if btn(ui, Icon::Rotate, "Toggle Rotation (⌘8)", true) {
                self.toggle_rotation();
            }
            if btn(ui, Icon::Plus, "Zoom In (⌘+)", zoom_in_ok) {
                self.zoom(ZOOM_STEP);
            }
            if btn(ui, Icon::Minus, "Zoom Out (⌘−)", zoom_out_ok) {
                self.zoom(-ZOOM_STEP);
            }
            if btn(ui, Icon::Resize, "Actual Size (⌘0)", true) {
                self.actual_size();
            }
            ui.menu_button(egui::RichText::new("•••").size(13.0), |ui| {
                if ui.checkbox(&mut self.phone_shaped.clone(), "Phone Size (⌘9)").clicked() {
                    self.toggle_phone_size();
                    ui.close_menu();
                }
                if ui.button("Notifications").clicked() {
                    self.act(|s| s.notifications());
                    ui.close_menu();
                }
                ui.separator();
                for (label, r) in [
                    ("Portrait", Rotation::Portrait),
                    ("Landscape Left", Rotation::LandscapeLeft),
                    ("Landscape Right", Rotation::LandscapeRight),
                ] {
                    if ui.radio(self.rotation == r, label).clicked() {
                        self.rotate(r);
                        ui.close_menu();
                    }
                }
                ui.separator();
                // Always reachable, even while an Android phone is showing. If it cannot open,
                // say so right here in the menu: the start screen's message area is not on
                // screen while a phone is showing, so a silent failure would look like a dead button.
                let note_id = egui::Id::new("iphone-open-note");
                if ui.button("Open iPhone Mirroring").clicked() {
                    if devices::open_iphone_mirroring() {
                        ui.memory_mut(|m| m.data.remove::<Instant>(note_id));
                        ui.close_menu();
                    } else {
                        ui.memory_mut(|m| m.data.insert_temp(note_id, Instant::now()));
                    }
                }
                if iphone_note_visible(ui.memory(|m| m.data.get_temp::<Instant>(note_id))) {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(IPHONE_UNAVAILABLE).size(11.0).color(Color32::from_rgb(255, 159, 10)),
                        )
                        .wrap(),
                    );
                }
                if ui.button("Back (Esc / right-click)").clicked() {
                    self.act(|s| s.back());
                    ui.close_menu();
                }
                if ui.button("Disconnect (back to phone list)").clicked() {
                    leave = true;
                    ui.close_menu();
                }
            });
        });
        leave
    }
}

/// Open Apple's iPhone Mirroring and say what happened.
const IPHONE_UNAVAILABLE: &str = "iPhone Mirroring needs macOS 15 or later on this Mac.";

fn open_iphone_message() -> String {
    if devices::open_iphone_mirroring() {
        "Opened Apple's iPhone Mirroring for your iPhone.".into()
    } else {
        IPHONE_UNAVAILABLE.into()
    }
}

/// The menu's "could not open" note stays up for a few seconds after a failed click.
fn iphone_note_visible(failed_at: Option<Instant>) -> bool {
    failed_at.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(8))
}

#[cfg(test)]
mod iphone_note_tests {
    use super::*;

    #[test]
    fn note_shows_only_for_a_recent_failure() {
        assert!(!iphone_note_visible(None));
        assert!(iphone_note_visible(Some(Instant::now())));
        if let Some(old) = Instant::now().checked_sub(std::time::Duration::from_secs(9)) {
            assert!(!iphone_note_visible(Some(old)));
        }
    }
}

/// Show the window (un-minimise it if needed) and make it the active window.
fn bring_to_front(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

// ---------- Drawing helpers ----------

fn accent_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(text).color(Color32::WHITE).strong())
        .fill(ACCENT)
        .corner_radius(CornerRadius::same(8))
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text.to_uppercase()).size(11.5).strong().color(SUBTLE));
    ui.add_space(4.0);
}

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(CornerRadius::same(12))
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(8.0);
}

fn notice(ui: &mut egui::Ui, col: Color32, text: &str) {
    egui::Frame::new()
        .fill(col.gamma_multiply(0.18))
        .stroke(Stroke::new(1.0, col.gamma_multiply(0.6)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(text).color(Color32::WHITE));
        });
}

fn phone_glyph(ui: &mut egui::Ui, tint: Color32) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(26.0, 40.0), Sense::hover());
    let p = ui.painter();
    p.rect(r.shrink2(egui::vec2(2.0, 0.0)), CornerRadius::same(6), Color32::from_gray(20), Stroke::new(2.0, tint), StrokeKind::Inside);
    p.circle_filled(egui::pos2(r.center().x, r.min.y + 5.0), 1.6, tint);
}

fn gradient_rect(p: &egui::Painter, r: Rect, top: Color32, bottom: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(r.left_top(), top);
    mesh.colored_vertex(r.right_top(), top);
    mesh.colored_vertex(r.right_bottom(), bottom);
    mesh.colored_vertex(r.left_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    p.add(mesh);
}

/// Laptop with two phones beside it: our own simple artwork.
fn draw_illustration(p: &egui::Painter, r: Rect) {
    let c = r.center();
    // Laptop
    let screen = Rect::from_center_size(c + egui::vec2(0.0, -8.0), egui::vec2(150.0, 96.0));
    p.rect_filled(screen.expand(5.0), CornerRadius::same(10), Color32::from_gray(90));
    gradient_rect(p, screen, Color32::from_rgb(40, 80, 160), Color32::from_rgb(120, 60, 170));
    let base = Rect::from_center_size(egui::pos2(c.x, screen.max.y + 10.0), egui::vec2(190.0, 8.0));
    p.rect_filled(base, CornerRadius::same(4), Color32::from_gray(120));
    // Mirrored phones on the laptop screen
    for (dx, col) in [(-28.0, ANDROID_TINT), (28.0, ACCENT)] {
        let ph = Rect::from_center_size(egui::pos2(screen.center().x + dx, screen.center().y), egui::vec2(34.0, 70.0));
        p.rect_filled(ph, CornerRadius::same(7), Color32::from_gray(15));
        gradient_rect(p, ph.shrink(3.0), col.gamma_multiply(0.9), Color32::from_gray(25));
        for i in 0..3 {
            for j in 0..2 {
                let dot = egui::pos2(ph.min.x + 10.0 + j as f32 * 14.0, ph.min.y + 14.0 + i as f32 * 14.0);
                p.rect_filled(Rect::from_center_size(dot, egui::vec2(8.0, 8.0)), CornerRadius::same(2), Color32::from_white_alpha(170));
            }
        }
    }
    // Real phones at the sides
    for (x, col) in [(r.min.x + 6.0, ANDROID_TINT), (r.max.x - 32.0, ACCENT)] {
        let ph = Rect::from_min_size(egui::pos2(x, c.y - 4.0), egui::vec2(26.0, 52.0));
        p.rect(ph, CornerRadius::same(6), Color32::from_gray(25), Stroke::new(2.0, col), StrokeKind::Inside);
    }
}

#[derive(Clone, Copy)]
enum Icon {
    Home,
    Switcher,
    Search,
    Settings,
    Rotate,
    Resize,
    Plus,
    Minus,
}

fn draw_icon(p: &egui::Painter, r: Rect, icon: Icon, col: Color32) {
    let s = Stroke::new(1.8, col);
    let c = r.center();
    let w = r.width() / 2.0;
    match icon {
        Icon::Home => {
            // A 3x3 grid of app tiles.
            for i in 0..3 {
                for j in 0..3 {
                    let at = egui::pos2(r.min.x + r.width() * (0.17 + 0.33 * j as f32), r.min.y + r.height() * (0.17 + 0.33 * i as f32));
                    p.rect_filled(Rect::from_center_size(at, egui::vec2(w * 0.42, w * 0.42)), CornerRadius::same(1), col);
                }
            }
        }
        Icon::Switcher => {
            p.rect_stroke(Rect::from_center_size(c + egui::vec2(-w * 0.35, 0.0), egui::vec2(w * 0.8, w * 1.5)), CornerRadius::same(2), s, StrokeKind::Middle);
            p.rect_stroke(Rect::from_center_size(c + egui::vec2(w * 0.55, 0.0), egui::vec2(w * 0.5, w * 1.2)), CornerRadius::same(2), s, StrokeKind::Middle);
        }
        Icon::Search => {
            p.circle_stroke(c + egui::vec2(-w * 0.15, -w * 0.15), w * 0.6, s);
            p.line_segment([c + egui::vec2(w * 0.3, w * 0.3), c + egui::vec2(w * 0.9, w * 0.9)], s);
        }
        Icon::Settings => {
            for (i, y) in [-0.55f32, 0.0, 0.55].iter().enumerate() {
                let y = c.y + y * w;
                p.line_segment([egui::pos2(r.min.x, y), egui::pos2(r.max.x, y)], s);
                let x = r.min.x + r.width() * [0.7, 0.3, 0.6][i];
                p.circle_filled(egui::pos2(x, y), 2.6, col);
            }
        }
        Icon::Rotate => {
            p.rect_stroke(Rect::from_center_size(c, egui::vec2(w * 0.9, w * 1.6)), CornerRadius::same(2), s, StrokeKind::Middle);
            p.line_segment([c + egui::vec2(w * 0.7, -w * 0.9), c + egui::vec2(w, -w * 0.4)], s);
            p.line_segment([c + egui::vec2(-w * 0.7, w * 0.9), c + egui::vec2(-w, w * 0.4)], s);
        }
        Icon::Resize => {
            p.rect_stroke(Rect::from_center_size(c, egui::vec2(w * 1.6, w * 1.6)), CornerRadius::same(3), s, StrokeKind::Middle);
            p.line_segment([c + egui::vec2(-w * 0.4, w * 0.4), c + egui::vec2(w * 0.4, -w * 0.4)], s);
        }
        Icon::Plus => {
            p.line_segment([c - egui::vec2(w * 0.8, 0.0), c + egui::vec2(w * 0.8, 0.0)], s);
            p.line_segment([c - egui::vec2(0.0, w * 0.8), c + egui::vec2(0.0, w * 0.8)], s);
        }
        Icon::Minus => {
            p.line_segment([c - egui::vec2(w * 0.8, 0.0), c + egui::vec2(w * 0.8, 0.0)], s);
        }
    }
}

/// The original's pretend test phone has a 430 x 932 point screen at scale 1.0
/// (main program 0x100001efc). The demo phone uses the same size, drawn at 2 pixels per point.
const DEMO_POINTS: (usize, usize) = (430, 932);
const DEMO_PIXELS_PER_POINT: usize = 2;
/// Density that makes the demo frame come out at DEMO_POINTS (points = pixels * 160 / density).
const DEMO_DENSITY: f32 = 160.0 * DEMO_PIXELS_PER_POINT as f32;

/// A made-up phone screen for --demo or uiTesting=on (no phone needed): wallpaper and app tiles.
fn demo_frame(landscape: bool) -> Frame {
    let (pw, ph) = (DEMO_POINTS.0 * DEMO_PIXELS_PER_POINT, DEMO_POINTS.1 * DEMO_PIXELS_PER_POINT);
    let (w, h) = if landscape { (ph, pw) } else { (pw, ph) };
    let unit = w.min(h) as f32 / 1080.0; // tile layout drawn for a 1080-pixel-wide screen
    let (cols, size, gap, left, top) = (
        4usize,
        (156.0 * unit) as usize,
        (90.0 * unit) as usize,
        (90.0 * unit) as usize,
        (270.0 * unit) as usize,
    );
    let mut rgba = vec![255u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let t = y as f32 / h as f32;
            let i = (y * w + x) * 4;
            rgba[i] = (20.0 + 60.0 * t) as u8;
            rgba[i + 1] = (90.0 - 40.0 * t) as u8;
            rgba[i + 2] = (170.0 - 30.0 * t) as u8;
            if x >= left && y >= top {
                let (gx, gy) = ((x - left) % (size + gap), (y - top) % (size + gap));
                if gx < size && gy < size && (x - left) / (size + gap) < cols && (y - top) / (size + gap) < 5 {
                    rgba[i] = 235;
                    rgba[i + 1] = 235;
                    rgba[i + 2] = 240;
                }
            }
        }
    }
    Frame { width: w, height: h, rgba }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run one redraw of the phone view with the given input events.
    fn redraw(ctx: &egui::Context, m: &mut Mirror, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(400.0, 900.0))),
            events,
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            m.ui(ctx);
        });
    }

    fn button(pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() }
    }

    fn demo_mirror(ctx: &egui::Context) -> Mirror {
        let mut m = Mirror::new(None, "test".into());
        m.density = DEMO_DENSITY;
        redraw(ctx, &mut m, vec![]); // first redraws load the pretend screen and lay it out
        redraw(ctx, &mut m, vec![]);
        m
    }

    #[test]
    fn light_tap_inside_one_redraw_registers() {
        // A light trackpad tap: press and release arrive together, inside one redraw.
        let ctx = egui::Context::default();
        let mut m = demo_mirror(&ctx);
        let p = Pos2::new(200.0, 450.0);
        redraw(&ctx, &mut m, vec![egui::Event::PointerMoved(p), button(p, true), button(p, false)]);
        assert_eq!(m.commands_sent, 1, "the tap must reach the phone");
        assert!(m.press.is_none(), "the finger must be lifted again");
    }

    #[test]
    fn firm_click_over_two_redraws_registers_once() {
        let ctx = egui::Context::default();
        let mut m = demo_mirror(&ctx);
        let p = Pos2::new(200.0, 450.0);
        redraw(&ctx, &mut m, vec![egui::Event::PointerMoved(p), button(p, true)]);
        redraw(&ctx, &mut m, vec![button(p, false)]);
        assert_eq!(m.commands_sent, 1);
    }

    fn right(pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton { pos, button: egui::PointerButton::Secondary, pressed, modifiers: Default::default() }
    }

    #[test]
    fn right_release_ends_a_touch_without_going_back() {
        // rightMouseUp ends the touch in progress (0x4c2f0); it must not also press Back.
        let ctx = egui::Context::default();
        let mut m = demo_mirror(&ctx);
        let p = Pos2::new(200.0, 450.0);
        redraw(&ctx, &mut m, vec![egui::Event::PointerMoved(p), button(p, true)]);
        redraw(&ctx, &mut m, vec![right(p, true), right(p, false)]);
        assert!(m.press.is_none(), "the touch must have ended");
        assert_eq!(m.commands_sent, 1, "one tap, and no Back");
    }

    #[test]
    fn leaving_the_window_mid_press_does_not_tap() {
        // A failed gesture is dropped, not finished (0xdba0c).
        let ctx = egui::Context::default();
        let mut m = demo_mirror(&ctx);
        let p = Pos2::new(200.0, 450.0);
        redraw(&ctx, &mut m, vec![egui::Event::PointerMoved(p), button(p, true)]);
        redraw(&ctx, &mut m, vec![egui::Event::PointerGone]);
        assert!(m.press.is_none());
        assert_eq!(m.commands_sent, 0, "nothing gets tapped by accident");
    }

    #[test]
    fn an_exact_repeat_event_is_ignored() {
        let ctx = egui::Context::default();
        let mut m = demo_mirror(&ctx);
        let p = Pos2::new(200.0, 450.0);
        redraw(&ctx, &mut m, vec![egui::Event::PointerMoved(p), button(p, true), button(p, true), button(p, false), button(p, false)]);
        assert_eq!(m.commands_sent, 1);
    }

    #[test]
    fn clicks_on_the_toolbar_are_not_touches() {
        let ctx = egui::Context::default();
        let mut m = demo_mirror(&ctx);
        let p = Pos2::new(200.0, 10.0); // in the control bar above the phone
        redraw(&ctx, &mut m, vec![egui::Event::PointerMoved(p), button(p, true), button(p, false)]);
        assert_eq!(m.commands_sent, 0);
    }
}
