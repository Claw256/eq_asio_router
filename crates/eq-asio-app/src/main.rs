//! Headroom GUI (binary `eq-asio-app`).
//!
//! Background operation: the app lives in the system tray (`tray`), can start with the
//! user's login (`autostart`, a per-user Run entry launching `--background`) and allows only
//! one GUI per session (`single_instance`). It is intentionally NOT a Windows service:
//! services run in session 0 and cannot show a tray icon or reliably use the user's WASAPI
//! endpoints and the Yamaha ASIO driver.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod cli;
mod debounce;
mod host;
mod install;
mod logic;
mod panels;
mod plot;
mod settings;
mod single_instance;
mod sys;
mod tray;

use std::io::Write;

/// Debug log line appended to the file named by `EQ_ASIO_APP_LOG` (no-op when unset).
pub fn dlog(msg: &str) {
    let Some(path) = std::env::var_os("EQ_ASIO_APP_LOG") else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[{:?}] {msg}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default());
    }
}

/// Prints `text` to the parent console (the exe is a GUI-subsystem program in release builds).
fn print_to_console(text: &str) {
    // SAFETY: attaching to the parent's console is harmless if there is none.
    unsafe {
        let _ = windows::Win32::System::Console::AttachConsole(windows::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
    print!("{text}");
    let _ = std::io::stdout().flush();
}

fn main() -> eframe::Result<()> {
    // Opt out of EcoQoS / efficiency mode before anything else: the app lives hidden in the tray
    // and must keep full-speed scheduling and 1 ms timers for the audio path.
    let hardening = router_engine::priority::harden_process();
    dlog(&format!("hardening: {}", hardening.summary()));
    let args = cli::Args::parse(std::env::args().skip(1));
    if args.help {
        print_to_console(cli::HELP);
        return Ok(());
    }
    if !args.unknown.is_empty() {
        dlog(&format!("ignoring unknown arguments: {:?}", args.unknown));
    }
    let instance = match single_instance::acquire(&single_instance::Names::real()) {
        single_instance::Outcome::First(g) => g,
        single_instance::Outcome::Second => {
            dlog("another GUI is running; asked it to show its window");
            return Ok(());
        }
    };
    let hidden = args.start_hidden(settings::AppSettings::load().start_minimized);
    dlog(&format!("starting (hidden={hidden}, background={})", args.background));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Headroom")
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([800.0, 500.0])
            .with_visible(!hidden),
        glow_options: eframe::egui_glow::GlowConfiguration {
            hardware_acceleration: eframe::egui_glow::HardwareAcceleration::Preferred,
            ..Default::default()
        },
        #[cfg(feature = "wgpu")]
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "Headroom",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            Ok(Box::new(app::App::new(&cc.egui_ctx, args, Some(instance), hidden)))
        }),
    )
}
