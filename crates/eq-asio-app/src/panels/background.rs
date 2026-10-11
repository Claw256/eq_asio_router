//! "Background" section: tray, close behaviour and run-at-login.

use crate::app::{App, AMBER, RED};
use crate::autostart::Status;

pub fn ui(ui: &mut egui::Ui, app: &mut App) {
    egui::CollapsingHeader::new(egui::RichText::new("Background").heading()).default_open(true).show(ui, |ui| {
        let tray = app.tray_available();
        if let Some(e) = &app.tray_error {
            ui.colored_label(RED, e);
        }

        let mut close = app.settings.close_to_tray;
        if ui
            .add_enabled(tray, egui::Checkbox::new(&mut close, "Close button hides to tray"))
            .on_hover_text("When off, the window close button quits the app (and stops the engine)")
            .changed()
        {
            app.set_close_to_tray(close);
        }

        let mut min = app.settings.start_minimized;
        if ui
            .add_enabled(tray, egui::Checkbox::new(&mut min, "Start hidden in tray"))
            .on_hover_text("Launch with the window hidden. The login entry always starts hidden.")
            .changed()
        {
            app.set_start_minimized(min);
        }

        let mut login = app.autostart_status.is_on();
        if ui
            .checkbox(&mut login, "Run at login")
            .on_hover_text(
                "Adds a per-user startup entry (HKCU Run, no admin) that starts this app hidden in the tray \
                 with the engine running. Not a Windows service: services cannot reach your audio devices or tray.",
            )
            .changed()
        {
            app.set_run_at_login(login);
        }
        if let Status::PointsElsewhere(cmd) = app.autostart_status.clone() {
            ui.colored_label(AMBER, format!("Login entry points elsewhere: {cmd}"));
            if ui.button("Fix (point to this exe)").clicked() {
                app.set_run_at_login(true);
            }
        }
        if let Some(e) = &app.autostart_error {
            ui.colored_label(RED, e);
        }

        if ui.add_enabled(tray, egui::Button::new("Hide to tray now")).clicked() {
            let ctx = ui.ctx().clone();
            app.hide_to_tray(&ctx);
        }
        ui.weak("Quit (header, or tray menu) stops the engine and exits.");
    });
}
