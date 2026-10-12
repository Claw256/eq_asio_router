//! Master device and output channel selection (the source is the VBMatrix bridge and
//! has no device picker — see the bridge readiness panel).

use crate::app::{App, AMBER, RED};

fn combo_text(s: &str) -> String {
    if s.is_empty() {
        "(none selected)".to_string()
    } else {
        s.to_string()
    }
}

pub fn ui(ui: &mut egui::Ui, app: &mut App) {
    ui.heading("Devices");
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
                .selected_text(crate::logic::channel_label(&names, app.cfg.output_channels[i]))
                .width(130.0)
                .show_ui(ui, |ui| {
                    for ch in 0..limit {
                        changed |= ui
                            .selectable_value(&mut app.cfg.output_channels[i], ch, crate::logic::channel_label(&names, ch))
                            .changed();
                    }
                });
        }
    });
    if out_channels == 0 {
        ui.weak("Channel count unknown until the engine has started once.");
    }

    ui.weak("Changes to the master, channels or rate apply after a ~1-2 s restart.");

    if changed {
        if app.cfg.master_driver != app.cfg.master_driver.trim() {
            app.cfg.master_driver = app.cfg.master_driver.trim().to_string();
        }
        app.refresh_panel_path();
        app.on_edited(false);
    }
}
