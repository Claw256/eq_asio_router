//! EQ profile editing, file import/export and the response plot.

use crate::app::App;
use crate::plot;
use eq_core::{FilterKind, FilterSpec, MAX_BANDS};

const KINDS: [(FilterKind, &str); 5] = [
    (FilterKind::Peaking, "PK"),
    (FilterKind::LowShelf, "LSC"),
    (FilterKind::HighShelf, "HSC"),
    (FilterKind::LowPass, "LP"),
    (FilterKind::HighPass, "HP"),
];

fn kind_label(k: FilterKind) -> &'static str {
    KINDS.iter().find(|(kk, _)| *kk == k).map_or("?", |(_, l)| l)
}

pub fn ui(ui: &mut egui::Ui, app: &mut App) {
    let mut profile_changed = false;
    let mut other_changed = false;

    ui.heading("Equalizer");
    ui.horizontal_wrapped(|ui| {
        if ui.button("Load AutoEQ ParametricEQ.txt…").clicked() {
            load_file(app, &mut profile_changed);
        }
        if ui.button("Save profile as text…").clicked() {
            save_file(app);
        }
        other_changed |= ui.checkbox(&mut app.cfg.bypass, "Bypass").changed();
    });
    if let Some(e) = &app.eq_error {
        ui.colored_label(egui::Color32::LIGHT_RED, e);
    }
    if let Some(src) = &app.cfg.profile_source {
        ui.weak(format!("Source: {src}"));
    }

    ui.horizontal(|ui| {
        ui.label("Name");
        other_changed |= ui.text_edit_singleline(&mut app.cfg.profile.name).changed();
        ui.label("Preamp");
        profile_changed |= ui
            .add(egui::Slider::new(&mut app.cfg.profile.preamp_db, -30.0..=10.0).suffix(" dB").fixed_decimals(1))
            .changed();
    });

    let fs = f64::from(app.plot_fs());
    let extra = eq_core::auto_headroom_db(&app.cfg.profile, fs);
    let eff = if app.cfg.auto_headroom { app.cfg.profile.preamp_db + extra } else { app.cfg.profile.preamp_db };
    ui.label(format!(
        "Effective preamp: {eff:+.1} dB (auto headroom {} {extra:+.1} dB) at {fs:.0} Hz",
        if app.cfg.auto_headroom { "on" } else { "off, would be" }
    ));

    ui.add_space(4.0);
    let plot_h = (ui.available_height() * 0.45).max(200.0);
    ui.allocate_ui(egui::vec2(ui.available_width(), plot_h), |ui| {
        plot::show(ui, &app.response);
    });

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.strong(format!("Bands ({}/{MAX_BANDS})", app.cfg.profile.filters.len()));
        let can_add = app.cfg.profile.filters.len() < MAX_BANDS;
        if ui.add_enabled(can_add, egui::Button::new("Add band")).clicked() {
            app.cfg.profile.filters.push(FilterSpec {
                kind: FilterKind::Peaking,
                fc_hz: 1000.0,
                gain_db: 0.0,
                q: 1.0,
                enabled: true,
            });
            profile_changed = true;
        }
    });

    let mut remove = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("bands").striped(true).num_columns(7).spacing([10.0, 4.0]).show(ui, |ui| {
            for h in ["On", "Type", "Fc (Hz)", "Gain (dB)", "Q", ""] {
                ui.strong(h);
            }
            ui.end_row();
            for (i, f) in app.cfg.profile.filters.iter_mut().enumerate() {
                profile_changed |= ui.checkbox(&mut f.enabled, "").changed();
                egui::ComboBox::from_id_salt(("kind", i)).width(60.0).selected_text(kind_label(f.kind)).show_ui(
                    ui,
                    |ui| {
                        for (k, l) in KINDS {
                            profile_changed |= ui.selectable_value(&mut f.kind, k, l).changed();
                        }
                    },
                );
                profile_changed |= ui
                    .add(egui::DragValue::new(&mut f.fc_hz).range(plot::F_MIN..=plot::F_MAX).speed(1.0).max_decimals(1))
                    .changed();
                profile_changed |= ui
                    .add(egui::DragValue::new(&mut f.gain_db).range(-30.0..=30.0).speed(0.05).max_decimals(2))
                    .changed();
                profile_changed |= ui
                    .add(egui::DragValue::new(&mut f.q).range(0.1..=20.0).speed(0.01).max_decimals(3))
                    .changed();
                if ui.small_button("Remove").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    });
    if let Some(i) = remove {
        app.cfg.profile.filters.remove(i);
        profile_changed = true;
    }

    if profile_changed || other_changed {
        app.on_edited(profile_changed);
    }
}

fn load_file(app: &mut App, profile_changed: &mut bool) {
    let Some(path) = rfd::FileDialog::new()
        .set_title("Load AutoEQ ParametricEQ.txt")
        .add_filter("Text", &["txt"])
        .add_filter("All files", &["*"])
        .pick_file()
    else {
        return;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            app.eq_error = Some(format!("Could not read {}: {e}", path.display()));
            return;
        }
    };
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "AutoEQ".into());
    match eq_core::parse_parametric_eq(&text, &name) {
        Ok(profile) => {
            app.cfg.profile = profile;
            app.cfg.profile_source = Some(path.display().to_string());
            app.eq_error = None;
            *profile_changed = true;
        }
        Err(e) => app.eq_error = Some(format!("Parse error in {}: {e}", path.display())),
    }
}

fn save_file(app: &mut App) {
    let default_name = format!("{}.txt", if app.cfg.profile.name.is_empty() { "profile" } else { &app.cfg.profile.name });
    let Some(path) = rfd::FileDialog::new()
        .set_title("Save profile as text")
        .set_file_name(default_name)
        .add_filter("Text", &["txt"])
        .save_file()
    else {
        return;
    };
    let text = eq_core::to_parametric_eq_text(&app.cfg.profile);
    app.eq_error = std::fs::write(&path, text)
        .err()
        .map(|e| format!("Could not write {}: {e}", path.display()));
}
