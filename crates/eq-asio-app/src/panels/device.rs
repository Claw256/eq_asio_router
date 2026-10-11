//! Master device, capture endpoint, capture mode and output channel selection.

use crate::app::{App, AMBER, RED};
use crate::logic::channel_label;
use eq_core::CaptureMode;

fn combo_text(s: &str) -> String {
    if s.is_empty() {
        "(none selected)".to_string()
    } else {
        s.to_string()
    }
}

const EXCLUSIVE_REASON: &str = "Disabled: measured on the Hi-Fi Cable, exclusive capture duplicates/drops 30-70 % of the 3-5 ms packets (crackle) and is only clean at 10 ms periods, which is no faster than Shared.";

pub fn ui(ui: &mut egui::Ui, app: &mut App) {
    ui.heading("Devices");
    crate::panels::source::ui(ui, app);
    ui.separator();
    let mut changed = false;

    ui.label("Master ASIO device");
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("master_driver")
            .selected_text(combo_text(&app.cfg.master_driver))
            .width(220.0)
            .show_ui(ui, |ui| {
                for name in &app.devices {
                    if ui.selectable_label(app.cfg.master_driver == *name, name).clicked() {
                        app.cfg.master_driver = name.clone();
                        changed = true;
                    }
                }
            });
        if ui.button("Refresh").clicked() {
            app.refresh_devices();
        }
    });
    if let Some(e) = &app.device_error {
        ui.colored_label(RED, e);
    } else if app.devices.is_empty() {
        ui.weak("No ASIO drivers found.");
    }
    if !app.cfg.master_driver.is_empty() && !app.devices.is_empty() && !app.devices.contains(&app.cfg.master_driver) {
        ui.colored_label(AMBER, "Configured master is not currently registered.");
    }

    ui.add_space(6.0);
    ui.label("Capture endpoint");
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("capture_device")
            .selected_text(combo_text(&app.cfg.capture_device))
            .width(220.0)
            .show_ui(ui, |ui| {
                for name in &app.captures {
                    if ui.selectable_label(app.cfg.capture_device == *name, name).clicked()
                        && app.cfg.capture_device != *name
                    {
                        app.cfg.capture_device = name.clone();
                        app.cfg.source_preset = eq_core::SourcePreset::Custom;
                        changed = true;
                    }
                }
            });
        if ui.button("Refresh").clicked() {
            app.refresh_captures();
        }
    });
    if let Some(e) = &app.capture_list_error {
        ui.colored_label(RED, e);
    } else if !app.captures.is_empty() && !app.captures.iter().any(|n| n.contains(&app.cfg.capture_device)) {
        ui.colored_label(AMBER, "Configured capture endpoint is not currently present.");
    }

    ui.add_space(6.0);
    ui.label("Capture mode");
    ui.horizontal(|ui| {
        let mode = app.cfg.capture_mode;
        if ui.selectable_label(mode == CaptureMode::Shared, "Shared").clicked() && mode != CaptureMode::Shared {
            app.cfg.capture_mode = CaptureMode::Shared;
            changed = true;
        }
        if ui
            .selectable_label(mode == CaptureMode::LowLatency, "Low-latency")
            .on_hover_text("Shared mode with the smallest engine period (falls back to Shared if unsupported)")
            .clicked()
            && mode != CaptureMode::LowLatency
        {
            app.cfg.capture_mode = CaptureMode::LowLatency;
            changed = true;
        }
        ui.add_enabled_ui(false, |ui| {
            ui.selectable_label(mode == CaptureMode::Exclusive, "Exclusive").on_disabled_hover_text(EXCLUSIVE_REASON)
        });
    });
    if app.cfg.capture_mode == CaptureMode::Exclusive {
        ui.colored_label(AMBER, "Exclusive is disabled (Shared is used instead).");
        if ui.button("Switch to Shared").clicked() {
            app.cfg.capture_mode = CaptureMode::Shared;
            changed = true;
        }
    }
    ui.weak("Exclusive is unavailable: the Hi-Fi Cable driver drops/duplicates packets in exclusive mode.");

    ui.add_space(6.0);
    ui.label("Output channel pair");
    let caps = app
        .snap
        .caps
        .as_ref()
        .filter(|c| c.master_name == app.cfg.master_driver || app.cfg.master_driver.is_empty());
    let out_channels = caps.map_or(0, |c| c.out_channels);
    let names: Vec<String> = caps.map(|c| c.out_names.clone()).unwrap_or_default();
    let limit = if out_channels >= 2 { out_channels } else { 16 };
    ui.horizontal(|ui| {
        for (i, label) in ["L", "R"].iter().enumerate() {
            ui.label(*label);
            egui::ComboBox::from_id_salt(("out_ch", i))
                .selected_text(channel_label(&names, app.cfg.output_channels[i]))
                .width(130.0)
                .show_ui(ui, |ui| {
                    for ch in 0..limit {
                        changed |= ui
                            .selectable_value(&mut app.cfg.output_channels[i], ch, channel_label(&names, ch))
                            .changed();
                    }
                });
        }
    });
    if out_channels == 0 {
        ui.weak("Channel count unknown until the engine has started once.");
    }

    ui.weak("Changes to device, capture, mode, channels or rate apply after a ~1-2 s restart.");

    if changed {
        if app.cfg.master_driver != app.cfg.master_driver.trim() {
            app.cfg.master_driver = app.cfg.master_driver.trim().to_string();
        }
        app.refresh_panel_path();
        app.on_edited(false);
    }
}
