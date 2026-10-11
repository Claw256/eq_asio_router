//! Driver registration section.

use crate::app::App;
use crate::install::{run_regsvr32, Bitness, Target};

pub fn ui(ui: &mut egui::Ui, app: &mut App) {
    ui.heading("Driver install");
    ui.weak("Registering needs administrator rights (a UAC prompt will appear).");

    let mut message = None;
    let mut refresh = false;
    for (i, label) in ["32-bit driver (VB ASIO Bridge)", "64-bit driver (optional)"].iter().enumerate() {
        let t = &app.targets[i];
        ui.add_space(4.0);
        ui.strong(*label);
        ui.label(egui::RichText::new(t.dll.display().to_string()).small().monospace());
        if !t.dll_exists {
            ui.colored_label(egui::Color32::YELLOW, "DLL not found");
        }
        ui.horizontal(|ui| {
            for (unreg, text) in [(false, "Register driver (admin)"), (true, "Unregister")] {
                if button(ui, t, text) {
                    message = Some(match run_regsvr32(t, unreg) {
                        Ok(()) => format!("Started regsvr32 ({:?}); check the dialog it shows.", t.bitness),
                        Err(e) => e,
                    });
                }
            }
        });
        if t.bitness == Bitness::X86 && !t.regsvr32.exists() {
            ui.colored_label(egui::Color32::LIGHT_RED, "SysWOW64 regsvr32.exe not found");
        }
    }
    if ui.small_button("Re-scan for DLLs").clicked() {
        refresh = true;
    }
    if refresh {
        app.targets = [
            crate::install::resolve_target(Bitness::X86),
            crate::install::resolve_target(Bitness::X64),
        ];
        app.refresh_devices();
    }
    if message.is_some() {
        app.install_msg = message;
    }
    if let Some(m) = &app.install_msg {
        ui.label(m);
    }
}

fn button(ui: &mut egui::Ui, t: &Target, text: &str) -> bool {
    let enabled = t.dll_exists && t.regsvr32.exists();
    let resp = ui.add_enabled(enabled, egui::Button::new(text));
    let resp = if enabled {
        resp
    } else {
        resp.on_disabled_hover_text(format!("Driver DLL not found at {}", t.dll.display()))
    };
    resp.clicked()
}
