//! Application state, config persistence and the top-level layout.

use crate::autostart::{self, Autostart};
use crate::cli::Args;
use crate::debounce::Debounce;
use crate::host::{Busy, EngineHost, Snapshot};
use crate::install::{self, Bitness, Target};
use crate::panels;
use crate::plot;
use crate::settings::AppSettings;
use crate::single_instance;
use crate::sys;
use crate::tray::{self, Tray, TrayCommand};
use std::sync::mpsc::{self, Receiver};
use eq_core::Config;
use router_engine::{EngineError, EngineState};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

const SAVE_DEBOUNCE: Duration = Duration::from_millis(250);
const REPAINT: Duration = Duration::from_millis(100);
const FILE_POLL: Duration = Duration::from_millis(500);
const BRIDGE_POLL: Duration = Duration::from_secs(2);
const AUTOSTART_POLL: Duration = Duration::from_secs(5);
const TRAY_GAP: Duration = Duration::from_secs(1);

pub const GREEN: egui::Color32 = egui::Color32::from_rgb(90, 200, 120);
pub const AMBER: egui::Color32 = egui::Color32::from_rgb(235, 185, 60);
pub const RED: egui::Color32 = egui::Color32::from_rgb(235, 80, 80);

pub struct App {
    pub cfg: Config,
    config_path: Option<PathBuf>,
    /// mtime of the config file as last read or written by us.
    seen_mtime: Option<SystemTime>,
    debounce: Debounce,
    pub settings: AppSettings,
    pub host: EngineHost,
    pub snap: Snapshot,
    /// Number of finished "Learn" runs already applied to the config.
    learn_seen: u32,
    pub devices: Vec<String>,
    pub device_error: Option<String>,
    pub captures: Vec<String>,
    pub capture_list_error: Option<String>,
    pub response: Vec<[f64; 2]>,
    /// Rate the response curve was computed for.
    plot_rate: u32,
    plot_dirty: bool,
    pub eq_error: Option<String>,
    pub config_error: Option<String>,
    pub settings_error: Option<String>,
    pub install_msg: Option<String>,
    pub targets: [Target; 2],
    pub panel_path: Option<PathBuf>,
    pub panel_error: Option<String>,
    pub bridge_running: bool,
    /// Holder the user is being asked to confirm a close request for.
    pub confirm_close: Option<router_engine::HolderInfo>,
    pub close_msg: Option<String>,
    /// Source preset the user picked and still has to confirm (first switch only).
    pub pending_preset: Option<eq_core::SourcePreset>,
    /// When the readiness check was last requested.
    pub readiness_requested: Option<Instant>,
    pub sound_settings_error: Option<String>,
    last_file_poll: Instant,
    last_bridge_poll: Instant,
    last_autostart_poll: Instant,
    args: Args,
    instance: Option<single_instance::Guard>,
    tray: Option<Tray>,
    tray_rx: Receiver<TrayCommand>,
    tray_throttle: tray::Throttle,
    pub tray_error: Option<String>,
    /// The main window is currently hidden (tray only).
    pub hidden: bool,
    /// A real quit is in progress (the close button must not hide to the tray any more).
    quitting: bool,
    autostart: Autostart,
    pub autostart_status: autostart::Status,
    pub autostart_error: Option<String>,
    exe: PathBuf,
    ticks_while_hidden: u64,
}

/// Display state of the engine.
pub fn state_label(snap: &Snapshot) -> (String, egui::Color32) {
    match snap.busy {
        Some(Busy::Starting) => return ("Starting".into(), AMBER),
        Some(Busy::Stopping) => return ("Stopping".into(), AMBER),
        None => {}
    }
    if snap.engine_present {
        match snap.status.as_ref().map(|s| &s.state) {
            Some(EngineState::Running) => ("Running".into(), GREEN),
            Some(EngineState::Starting) => ("Starting".into(), AMBER),
            Some(EngineState::Restarting { reason }) => (crate::logic::restarting_label(reason), AMBER),
            Some(EngineState::WaitingForDevice { holders }) => (crate::logic::waiting_label(holders), AMBER),
            Some(EngineState::Failed) => ("Failed".into(), RED),
            Some(EngineState::Stopped) | None => ("Stopped".into(), egui::Color32::GRAY),
        }
    } else if snap.start_error.is_some() {
        ("Failed".into(), RED)
    } else {
        ("Stopped".into(), egui::Color32::GRAY)
    }
}

impl App {
    /// `hidden`: the window was created invisible (tray-only start).
    pub fn new(ctx: &egui::Context, args: Args, instance: Option<single_instance::Guard>, hidden: bool) -> Self {
        let config_path = eq_core::default_path();
        let mut config_error = None;
        let cfg = match &config_path {
            Some(p) => match Config::load_or_default(p) {
                Ok(c) => c.sanitized(),
                Err(e) => {
                    config_error = Some(format!("Could not load config, using defaults: {e}"));
                    Config::default()
                }
            },
            None => {
                config_error = Some("%APPDATA% is not set; changes will not be saved".into());
                Config::default()
            }
        };
        let settings = AppSettings::load();
        let seen_mtime = config_path.as_ref().and_then(eq_core::modified);
        let now = Instant::now();
        let host = EngineHost::new();
        let exe = std::env::current_exe().unwrap_or_default();
        let autostart = Autostart::real();
        let autostart_status = autostart.status(&exe);
        let (tray_tx, tray_rx) = mpsc::channel();
        let (tray, tray_error) = match Tray::spawn(tray_tx, ctx.clone(), tray::TrayState::default()) {
            Ok(t) => (Some(t), None),
            Err(e) => {
                crate::dlog(&format!("tray failed: {e}"));
                (None, Some(format!("System tray unavailable: {e}")))
            }
        };
        let mut app = App {
            response: Vec::new(),
            plot_rate: 0,
            plot_dirty: true,
            cfg,
            config_path,
            seen_mtime,
            debounce: Debounce::new(SAVE_DEBOUNCE),
            settings,
            host,
            snap: Snapshot::default(),
            learn_seen: 0,
            devices: Vec::new(),
            device_error: None,
            captures: Vec::new(),
            capture_list_error: None,
            eq_error: None,
            config_error,
            settings_error: None,
            install_msg: None,
            targets: [install::resolve_target(Bitness::X86), install::resolve_target(Bitness::X64)],
            panel_path: None,
            panel_error: None,
            bridge_running: sys::bridge_running(),
            confirm_close: None,
            close_msg: None,
            pending_preset: None,
            readiness_requested: None,
            sound_settings_error: None,
            last_file_poll: now,
            last_bridge_poll: now,
            last_autostart_poll: now,
            args,
            instance,
            tray,
            tray_rx,
            tray_throttle: tray::Throttle::new(TRAY_GAP),
            tray_error,
            hidden,
            quitting: false,
            autostart,
            autostart_status,
            autostart_error: None,
            exe,
            ticks_while_hidden: 0,
        };
        if app.hidden && app.tray.is_none() {
            // Never leave the user with an invisible, unreachable app.
            app.hidden = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
        app.settings.run_at_login = app.autostart_status.is_on();
        app.refresh_devices();
        app.refresh_captures();
        app.refresh_panel_path();
        if app.args.start_engine(app.settings.auto_start) {
            app.start_engine();
        }
        app
    }

    pub fn refresh_devices(&mut self) {
        match asio_host::enumerate() {
            Ok(list) => {
                self.devices =
                    list.into_iter().map(|d| d.name).filter(|n| n != "EQ ASIO Router").collect();
                self.device_error = None;
            }
            Err(e) => {
                self.devices.clear();
                self.device_error = Some(format!("Could not list ASIO drivers: {e}"));
            }
        }
        self.refresh_panel_path();
    }

    pub fn refresh_captures(&mut self) {
        match wasapi_capture::list_endpoints(wasapi_capture::Flow::Capture) {
            Ok(list) => {
                self.captures = list.into_iter().map(|e| e.name).collect();
                self.capture_list_error = None;
            }
            Err(e) => {
                self.captures.clear();
                self.capture_list_error = Some(format!("Could not list capture devices: {e}"));
            }
        }
    }

    pub fn refresh_panel_path(&mut self) {
        self.panel_path =
            if self.cfg.master_driver.is_empty() { None } else { sys::find_control_panel(&self.cfg.master_driver) };
    }

    pub fn open_yamaha_panel(&mut self) {
        if let Some(p) = &self.panel_path {
            self.panel_error = std::process::Command::new(p)
                .current_dir(p.parent().unwrap_or(std::path::Path::new(".")))
                .spawn()
                .err()
                .map(|e| format!("Could not start {}: {e}", p.display()));
        }
    }

    /// Call after any user edit of `cfg`.
    pub fn on_edited(&mut self, profile_changed: bool) {
        if profile_changed {
            self.plot_dirty = true;
        }
        self.debounce.mark(Instant::now());
    }

    pub fn set_auto_start(&mut self, on: bool) {
        self.settings.auto_start = on;
        self.settings_error = self.settings.save().err().map(|e| format!("Could not save app settings: {e}"));
    }

    pub fn set_close_to_tray(&mut self, on: bool) {
        self.settings.close_to_tray = on;
        self.save_settings();
    }

    pub fn set_start_minimized(&mut self, on: bool) {
        self.settings.start_minimized = on;
        self.save_settings();
    }

    pub fn save_settings(&mut self) {
        self.settings_error = self.settings.save().err().map(|e| format!("Could not save app settings: {e}"));
    }

    /// Writes/removes/fixes the per-user Run entry for this exe and re-reads the real state.
    pub fn set_run_at_login(&mut self, on: bool) {
        let r = if on { self.autostart.enable(&self.exe) } else { self.autostart.disable() };
        self.autostart_error = r.err();
        self.refresh_autostart();
        self.settings.run_at_login = self.autostart_status.is_on();
        self.save_settings();
    }

    pub fn refresh_autostart(&mut self) {
        self.autostart_status = self.autostart.status(&self.exe);
    }

    pub fn tray_available(&self) -> bool {
        self.tray.is_some()
    }

    /// Hides the main window (tray only).
    pub fn hide_to_tray(&mut self, ctx: &egui::Context) {
        crate::dlog("hide to tray");
        if self.tray.is_none() {
            return;
        }
        self.hidden = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    pub fn show_window(&mut self, ctx: &egui::Context) {
        crate::dlog("show window");
        self.hidden = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Real quit: stop the engine (releases the Yamaha), remove the tray icon, close.
    pub fn quit(&mut self, ctx: &egui::Context) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        crate::dlog("quit requested");
        self.host.shutdown();
        if let Some(t) = self.tray.as_mut() {
            t.shutdown();
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        ctx.request_repaint();
    }

    fn handle_tray_command(&mut self, ctx: &egui::Context, cmd: TrayCommand) {
        match cmd {
            TrayCommand::Show => self.show_window(ctx),
            TrayCommand::ToggleEngine => {
                if self.snap.busy.is_none() {
                    if self.snap.engine_present {
                        self.stop_engine();
                    } else {
                        self.start_engine();
                    }
                }
            }
            TrayCommand::ToggleBypass => {
                self.cfg.bypass = !self.cfg.bypass;
                self.on_edited(false);
            }
            TrayCommand::ToggleRunAtLogin => {
                let on = !self.autostart_status.is_on();
                self.set_run_at_login(on);
            }
            TrayCommand::Quit => self.quit(ctx),
        }
    }

    /// Tray commands, second-launch "show" requests, close-to-tray, tray state push.
    fn background_tick(&mut self, ctx: &egui::Context, now: Instant) {
        while let Ok(cmd) = self.tray_rx.try_recv() {
            self.handle_tray_command(ctx, cmd);
        }
        if self.instance.as_ref().is_some_and(single_instance::Guard::take_show_request) {
            self.show_window(ctx);
        }
        if ctx.input(|i| i.viewport().close_requested())
            && !self.quitting
            && self.settings.close_to_tray
            && self.tray.is_some()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_to_tray(ctx);
        }
        if now.duration_since(self.last_autostart_poll) >= AUTOSTART_POLL {
            self.last_autostart_poll = now;
            self.refresh_autostart();
        }
        if let Some(t) = &self.tray {
            let label = state_label(&self.snap).0;
            let st = tray::tray_state(&self.snap, &label, self.cfg.bypass, self.autostart_status.is_on());
            if self.tray_throttle.should_send(now, &st) {
                t.update(&st);
            }
        }
        if self.hidden {
            self.ticks_while_hidden += 1;
            if self.ticks_while_hidden % 50 == 1 {
                crate::dlog(&format!("tick while hidden #{}", self.ticks_while_hidden));
            }
        }
    }

    /// Persist pending edits right now (used before Start).
    fn flush(&mut self) {
        if self.debounce.is_pending() {
            self.debounce.clear();
            self.save_now();
        }
    }

    pub fn start_engine(&mut self) {
        self.flush();
        self.host.start(self.cfg.sanitized());
    }

    pub fn stop_engine(&mut self) {
        self.host.stop();
    }

    fn save_now(&mut self) {
        let safe = self.cfg.sanitized();
        if let Some(path) = &self.config_path {
            match safe.save(path) {
                Ok(()) => {
                    self.seen_mtime = eq_core::modified(path);
                    self.config_error = None;
                }
                Err(e) => self.config_error = Some(format!("Could not save config: {e}")),
            }
        }
        self.host.apply(safe);
    }

    /// Reload when the file changed on disk and we have no unsaved edits.
    fn check_external_change(&mut self) {
        if self.debounce.is_pending() {
            return;
        }
        let Some(path) = &self.config_path else { return };
        let mtime = eq_core::modified(path);
        if mtime.is_none() || mtime == self.seen_mtime {
            return;
        }
        self.seen_mtime = mtime;
        match Config::load(path) {
            Ok(cfg) => {
                let cfg = cfg.sanitized();
                if cfg != self.cfg {
                    self.cfg = cfg;
                    self.plot_dirty = true;
                }
                self.config_error = None;
            }
            Err(e) => self.config_error = Some(format!("Config changed on disk but is invalid: {e}")),
        }
    }

    /// A finished "Learn (30 s)" run: lock the measured minimal safe headroom in as the persisted
    /// per-device jitter margin (`Config::set_jitter_margin`) (saved and applied through the normal debounced path).
    fn apply_learn_result(&mut self) {
        let Some(l) = self.snap.status.as_ref().map(|s| s.learn.clone()) else { return };
        if l.finished < self.learn_seen {
            self.learn_seen = l.finished; // engine restarted: its counter started over
        }
        if l.finished > self.learn_seen {
            self.learn_seen = l.finished;
            if let Some(ms) = l.result_ms {
                self.cfg.set_jitter_margin(ms);
                self.on_edited(false);
            }
        }
    }

    /// Rate used for the response plot: the master's actual rate.
    pub fn plot_fs(&self) -> u32 {
        let from_status = self.snap.status.as_ref().map_or(0, |s| s.asio_rate);
        let from_caps = self.snap.caps.as_ref().map_or(0, |c| c.sample_rate);
        [from_status, from_caps, self.cfg.sample_rate, 48_000].into_iter().find(|r| *r > 0).unwrap_or(48_000)
    }

    fn tick(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.snap = self.host.snapshot();
        self.apply_learn_result();
        self.background_tick(ctx, now);
        if now.duration_since(self.last_file_poll) >= FILE_POLL {
            self.last_file_poll = now;
            self.check_external_change();
        }
        if now.duration_since(self.last_bridge_poll) >= BRIDGE_POLL {
            self.last_bridge_poll = now;
            self.bridge_running = sys::bridge_running();
        }
        if self.debounce.poll(now) {
            self.save_now();
        }
        let fs = self.plot_fs();
        if self.plot_dirty || fs != self.plot_rate {
            self.response = plot::response_points(&self.cfg.profile, f64::from(fs));
            self.plot_rate = fs;
            self.plot_dirty = false;
        }
        ctx.request_repaint_after(REPAINT);
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let (text, color) = state_label(&self.snap);
        let busy = self.snap.busy.is_some();
        let engine_present = self.snap.engine_present;
        ui.horizontal_wrapped(|ui| {
            ui.heading("EQ ASIO Router");
            ui.add_space(8.0);
            ui.label(egui::RichText::new(format!("\u{25CF} {text}")).color(color).strong());
            ui.add_space(8.0);
            if ui.add_enabled(!busy && !engine_present, egui::Button::new("Start")).clicked() {
                self.start_engine();
            }
            if ui.add_enabled(!busy && engine_present, egui::Button::new("Stop")).clicked() {
                self.stop_engine();
            }
            let mut auto = self.settings.auto_start;
            if ui.checkbox(&mut auto, "Auto-start engine on launch").changed() {
                self.set_auto_start(auto);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Quit").on_hover_text("Stop the engine, release the Yamaha and exit the app").clicked() {
                    let ctx = ui.ctx().clone();
                    self.quit(&ctx);
                }
                if ui
                    .add_enabled(self.tray_available(), egui::Button::new("Hide to tray"))
                    .on_hover_text("Keep running in the background; use the tray icon to bring the window back")
                    .clicked()
                {
                    let ctx = ui.ctx().clone();
                    self.hide_to_tray(&ctx);
                }
                let btn = egui::Button::new(egui::RichText::new("PANIC MUTE (stop)").strong().color(egui::Color32::WHITE))
                    .fill(egui::Color32::from_rgb(190, 30, 30))
                    .min_size(egui::vec2(170.0, 30.0));
                if ui
                    .add_enabled(engine_present && !busy, btn)
                    .on_hover_text("Stops the engine immediately and releases the Yamaha")
                    .clicked()
                {
                    self.stop_engine();
                }
            });
        });
        match &self.snap.start_error {
            Some(EngineError::AlreadyRunning) => {
                ui.colored_label(
                    RED,
                    "Another router instance (or router-cli) is already running and owns the Yamaha. \
                     Close it, then press Start.",
                );
            }
            Some(e) => {
                ui.colored_label(RED, format!("Start failed: {e}"));
            }
            None => {}
        }
        self.waiting_ui(ui);
        if self.bridge_running {
            ui.colored_label(AMBER, "ASIO Bridge is running and may hold the Yamaha; close it (VBCABLE_AsioBridge.exe).");
        }
        if let Some(e) = &self.settings_error {
            ui.colored_label(RED, e);
        }
        if let Some(e) = &self.config_error {
            ui.colored_label(RED, e);
        }
    }

    /// Holder list, "Retry now" and per-holder "Ask <exe> to close" while waiting for the master.
    fn waiting_ui(&mut self, ui: &mut egui::Ui) {
        let holders = match self.snap.status.as_ref().map(|s| &s.state) {
            Some(EngineState::WaitingForDevice { holders }) if self.snap.engine_present => holders.clone(),
            _ => {
                self.close_msg = None;
                return;
            }
        };
        if let Some(e) = self.snap.status.as_ref().and_then(|s| s.last_error.as_ref()) {
            ui.colored_label(RED, e);
        }
        ui.colored_label(AMBER, format!("Held by: {}", crate::logic::holders_text(&holders)));
        ui.horizontal_wrapped(|ui| {
            if ui.button("Retry now").clicked() {
                self.host.retry_now();
            }
            for h in &holders {
                if ui
                    .button(format!("Ask {} to close", h.exe_name))
                    .on_hover_text(format!("{} (PID {})", h.exe_name, h.pid))
                    .clicked()
                {
                    self.confirm_close = Some(h.clone());
                }
            }
        });
        if let Some(m) = &self.close_msg {
            ui.colored_label(AMBER, m);
        }
    }

    fn close_dialog(&mut self, ctx: &egui::Context) {
        let Some(h) = self.confirm_close.clone() else { return };
        egui::Window::new(format!("Ask {} to close?", h.exe_name))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(crate::logic::close_confirm_text(&h.exe_name));
                ui.horizontal(|ui| {
                    if ui.button("Send close request").clicked() {
                        let r = asio_host::ask_holder_to_close(h.pid);
                        self.close_msg = Some(crate::logic::close_result_text(&h.exe_name, &r));
                        self.confirm_close = None;
                        if r.is_ok() {
                            self.host.retry_now();
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_close = None;
                    }
                });
            });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.tick(&ctx);

        egui::Panel::top("header").show(ui, |ui| {
            self.header(ui);
        });
        egui::Panel::left("side").resizable(true).default_size(380.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                panels::background::ui(ui, self);
                ui.separator();
                panels::device::ui(ui, self);
                ui.separator();
                panels::rate::ui(ui, self);
                ui.separator();
                panels::latency::ui(ui, self);
                ui.separator();
                panels::status::ui(ui, self);
                ui.separator();
                egui::CollapsingHeader::new("Advanced: legacy ASIO driver for the VB ASIO Bridge (not needed)")
                    .default_open(false)
                    .show(ui, |ui| panels::install::ui(ui, self));
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                panels::eq::ui(ui, self);
                ui.separator();
                panels::fun::ui(ui, self);
            });
        });
        self.close_dialog(&ctx);
        panels::source::confirm_dialog(&ctx, self);
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Stops the engine and releases the Yamaha before the process exits.
        self.host.shutdown();
        if let Some(t) = self.tray.as_mut() {
            t.shutdown();
        }
    }
}
