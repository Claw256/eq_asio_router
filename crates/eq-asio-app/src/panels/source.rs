//! Capture source: preset selector, fixed capture period, render keep-alive, readiness check and
//! the corruption-fallback banner.

use crate::app::{App, AMBER, GREEN, RED};
use crate::logic::{self, PeriodChoice, Severity};
use eq_core::{SourcePreset, MAX_CAPTURE_PERIOD_MS, MIN_CAPTURE_PERIOD_MS, RECOMMENDED_CAPTURE_PERIOD_MS};
use std::time::{Duration, Instant};

/// How often the readiness check is refreshed while its section is open.
const READINESS_POLL: Duration = Duration::from_secs(3);

const RESTART_HINT: &str = "Changes to the source apply after a ~1-2 s restart.";

pub fn ui(ui: &mut egui::Ui, app: &mut App) {
    ui.strong("Source");
    fallback_banner(ui, app);

    // Preset selector.
    let current = app.cfg.source_preset;
    let mut picked = None;
    ui.horizontal(|ui| {
        ui.label("Preset");
        egui::ComboBox::from_id_salt("source_preset").selected_text(logic::preset_label(current)).width(210.0).show_ui(
            ui,
            |ui| {
                for p in [SourcePreset::Cable, SourcePreset::Vac, SourcePreset::Custom] {
                    if ui.selectable_label(current == p, logic::preset_label(p)).clicked() && current != p {
                        picked = Some(p);
                    }
                }
            },
        );
    });
    ui.weak(logic::preset_description(current));
    if let Some(p) = picked {
        if p == SourcePreset::Custom || app.settings.preset_confirmed {
            apply_preset(app, p);
        } else {
            app.pending_preset = Some(p);
        }
    }

    let mut changed = false;
    if logic::is_known_virtual_cable(&app.cfg.capture_device) {
        ui.add_space(4.0);
        changed |= period_control(ui, app);
        ui.add_space(2.0);
        if ui.checkbox(&mut app.cfg.render_keepalive, "Render keep-alive").on_hover_text(logic::KEEPALIVE_EXPLANATION).changed() {
            changed = true;
        }
        ui.weak(logic::KEEPALIVE_EXPLANATION);
        if app.cfg.render_keepalive && eq_core::is_vac_name(&app.cfg.capture_device) {
            ui.weak("It lowers Line 1's engine period for every app that plays into Line 1.");
        }
    }
    ui.weak(RESTART_HINT);
    if changed {
        app.cfg.source_preset = SourcePreset::Custom;
        app.on_edited(false);
    }

    ui.add_space(4.0);
    readiness(ui, app);
}

fn apply_preset(app: &mut App, p: SourcePreset) {
    app.cfg.apply_source_preset(p);
    app.refresh_captures();
    app.on_edited(false);
    app.readiness_requested = None; // re-check against the new device right away
}

/// Auto / 6 ms / 5 ms / custom selector; returns true if the config changed.
fn period_control(ui: &mut egui::Ui, app: &mut App) -> bool {
    let mut changed = false;
    let choice = logic::period_choice(app.cfg.capture_period_ms);
    ui.horizontal(|ui| {
        ui.label("Capture period");
        egui::ComboBox::from_id_salt("capture_period")
            .selected_text(logic::period_choice_label(choice))
            .width(190.0)
            .show_ui(ui, |ui| {
                let options = [
                    (PeriodChoice::Auto, 0.0),
                    (PeriodChoice::Recommended, RECOMMENDED_CAPTURE_PERIOD_MS),
                    (PeriodChoice::Minimum, MIN_CAPTURE_PERIOD_MS),
                ];
                for (c, ms) in options {
                    let sel = std::mem::discriminant(&choice) == std::mem::discriminant(&c);
                    if ui.selectable_label(sel, logic::period_choice_label(c)).clicked() && !sel {
                        app.cfg.capture_period_ms = ms;
                        changed = true;
                    }
                }
                let is_custom = matches!(choice, PeriodChoice::Custom(_));
                if ui.selectable_label(is_custom, "Custom\u{2026}").clicked() && !is_custom {
                    app.cfg.capture_period_ms = 8.0;
                    changed = true;
                }
            });
    });
    if let PeriodChoice::Custom(_) = choice {
        ui.horizontal(|ui| {
            ui.label("Custom period");
            let mut ms = app.cfg.capture_period_ms;
            let r = ui.add(
                egui::DragValue::new(&mut ms)
                    .range(MIN_CAPTURE_PERIOD_MS..=MAX_CAPTURE_PERIOD_MS)
                    .speed(0.1)
                    .max_decimals(1)
                    .suffix(" ms"),
            );
            if r.changed() {
                app.cfg.capture_period_ms = logic::clamp_period_ms(ms);
                changed = true;
            }
            ui.weak(format!(
                "= {} frames at 48 kHz",
                logic::ms_to_frames(app.cfg.capture_period_ms, 48_000)
            ));
        });
    }
    if let Some(w) = logic::period_warning(logic::period_choice(app.cfg.capture_period_ms)) {
        ui.colored_label(AMBER, w);
    }
    ui.weak(logic::PERIOD_EXPLANATION);
    changed
}

/// Amber banner while capture runs in the plain-Shared fallback, with a retry button.
pub fn fallback_banner(ui: &mut egui::Ui, app: &App) {
    let Some(text) = app.snap.status.as_ref().and_then(|s| s.capture_fallback.as_deref()) else { return };
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(70, 52, 10))
        .stroke(egui::Stroke::new(1.0, AMBER))
        .inner_margin(6.0)
        .corner_radius(4.0)
        .show(ui, |ui| {
            ui.colored_label(AMBER, format!("\u{26A0} {}", logic::fallback_banner_text(text)));
            if ui
                .button("Retry fixed period")
                .on_hover_text("Forgets the fallback and restarts the stream (~1-2 s) so the fixed period is tried again")
                .clicked()
            {
                app.host.reset_fallback();
            }
        });
}

/// Readiness rows + buttons; the check runs on a worker thread (on demand and every ~3 s while open).
fn readiness(ui: &mut egui::Ui, app: &mut App) {
    let header = egui::CollapsingHeader::new("Source readiness").default_open(true).show(ui, |ui| {
        let now = Instant::now();
        let stale = app.readiness_requested.is_none_or(|t| now.duration_since(t) >= READINESS_POLL);
        if stale {
            app.readiness_requested = Some(now);
            app.host.request_readiness(app.cfg.clone());
        }
        ui.horizontal(|ui| {
            if ui.button("Refresh").clicked() {
                app.refresh_captures();
                app.readiness_requested = Some(now);
                app.host.request_readiness(app.cfg.clone());
            }
            if ui
                .button("Open Windows Sound settings")
                .on_hover_text("Opens the Windows Sound page (ms-settings:sound). Nothing is changed by the app.")
                .clicked()
            {
                app.sound_settings_error = crate::sys::open_sound_settings().err();
            }
            if app.snap.readiness_busy {
                ui.weak("checking\u{2026}");
            }
        });
        if let Some(e) = &app.sound_settings_error {
            ui.colored_label(RED, e);
        }
        let Some(r) = app.snap.readiness.clone() else {
            ui.weak("Checking the source\u{2026}");
            return;
        };
        if !r.capture_present && eq_core::is_vac_name(&app.cfg.capture_device) {
            ui.colored_label(AMBER, logic::waiting_for_line1_text());
        }
        egui::Grid::new("readiness_grid").num_columns(3).show(ui, |ui| {
            for row in logic::readiness_rows(&r, &app.cfg.capture_device) {
                let color = match row.severity {
                    Severity::Ok => GREEN,
                    Severity::Warn => AMBER,
                    Severity::Fail => RED,
                    Severity::Unknown => egui::Color32::GRAY,
                };
                ui.colored_label(color, row.severity.mark());
                ui.label(row.label);
                ui.colored_label(color, row.text);
                ui.end_row();
            }
        });
        for n in &r.notes {
            ui.weak(n);
        }
        ui.weak("Exclusive capture stays disabled (the Hi-Fi Cable driver corrupts it).");
    });
    if header.body_response.is_none() {
        app.readiness_requested = None;
    }
}

/// First-time confirmation before a preset changes the capture device, period and rate matching.
pub fn confirm_dialog(ctx: &egui::Context, app: &mut App) {
    let Some(p) = app.pending_preset else { return };
    egui::Window::new("Switch source preset?")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .max_width(420.0)
        .show(ctx, |ui| {
            ui.strong(logic::preset_label(p));
            ui.label(logic::preset_change_summary(p));
            ui.weak("This confirmation is only shown once.");
            ui.horizontal(|ui| {
                if ui.button("Switch").clicked() {
                    app.settings.preset_confirmed = true;
                    app.save_settings();
                    apply_preset(app, p);
                    app.pending_preset = None;
                }
                if ui.button("Cancel").clicked() {
                    app.pending_preset = None;
                }
            });
        });
}
