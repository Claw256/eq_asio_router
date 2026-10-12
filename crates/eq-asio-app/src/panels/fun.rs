//! "Fun" section: JBL Bass Mode (car-subwoofer bass boost, distortion and pumping).

use std::ops::RangeInclusive;

use eq_core::{Config, JblLfoTarget, JblParams};

use crate::app::App;

/// Slider with a unit suffix and a plain-language hover text. Returns true when changed.
fn slider(
    ui: &mut egui::Ui,
    value: &mut f32,
    range: RangeInclusive<f32>,
    label: &str,
    suffix: &str,
    log: bool,
    tip: &str,
) -> bool {
    let mut s = egui::Slider::new(value, range).text(label).suffix(suffix);
    if log {
        s = s.logarithmic(true);
    }
    ui.add(s).on_hover_text(tip).changed()
}

/// Frequency slider (logarithmic, Hz).
fn hz(ui: &mut egui::Ui, value: &mut f32, range: RangeInclusive<f32>, label: &str, tip: &str) -> bool {
    slider(ui, value, range, label, " Hz", true, tip)
}

fn toggle(ui: &mut egui::Ui, value: &mut bool, label: &str, tip: &str) -> bool {
    ui.checkbox(value, label).on_hover_text(tip).changed()
}

fn group(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui) -> bool) -> bool {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(title).strong());
    body(ui)
}

fn advanced(ui: &mut egui::Ui, p: &mut JblParams, intensity: &mut u8) -> bool {
    let mut c = false;

    c |= group(ui, "Pump", |ui| {
        let mut c = toggle(
            ui,
            &mut p.duck_on,
            "Pumping (ducking)",
            "Lowers the whole signal on every bass hit, then lets it swell back, like a straining amplifier.",
        );
        c |= slider(ui, &mut p.duck_depth, 0.0..=6.0, "Depth", "", false, "How hard the level is pulled down on a bass hit. 0 = none.");
        c |= ui
            .add(egui::Slider::new(&mut p.duck_floor_db, -24.0..=0.0).text("Floor").custom_formatter(|v, _| {
                if v <= -23.95 { "none".to_owned() } else { format!("{v:.1} dB") }
            }))
            .on_hover_text(
                "How far the level is allowed to drop on a bass hit. Higher = louder during the pumping (shallower dips,                  same timing and rhythm). 0 dB = no dip at all, none = the full, uncapped dip.",
            )
            .changed();
        c |= slider(ui, &mut p.duck_attack_ms, 0.5..=50.0, "Attack", " ms", true, "How fast the level drops when bass hits. Short = punchy.");
        c |= slider(ui, &mut p.duck_release_ms, 10.0..=800.0, "Release", " ms", true, "How slowly the level recovers after a hit. Long = more audible pumping.");
        c |= hz(ui, &mut p.detect_hz, 40.0..=250.0, "Detect", "Frequency below which sound counts as bass and triggers the pumping.");
        c
    });

    c |= group(ui, "Mids/highs", |ui| {
        let mut c = ui
            .add(egui::Slider::new(&mut p.mid_pass, 0.0..=1.0).text("Pass-through").custom_formatter(|v, _| format!("{:.0} %", v * 100.0)).custom_parser(|t| {
                t.trim().trim_end_matches('%').trim().parse::<f64>().ok().map(|v| v / 100.0)
            }))
            .on_hover_text(
                "How much of the original sound above the crossover is let through unprocessed. Low = the distortion, lo-fi, dulling and pumping hit the whole sound and the bass takes over (the default is low, 15 %). High = vocals and highs stay clean while only the bass distorts. 0 % = everything is processed.",
            )
            .changed();
        c |= hz(ui, &mut p.mid_pass_hz, 100.0..=1000.0, "Crossover", "Frequency above which the original sound is kept. Lower = more of the low-mids stay clean; higher = more of the bass harmonics stay distorted.");
        c
    });

    c |= group(ui, "Low end", |ui| {
        let mut c = hz(ui, &mut p.shelf_hz, 30.0..=200.0, "Shelf", "Corner frequency of the bass boost. Everything below it is lifted.");
        c |= slider(ui, &mut p.shelf_db, 0.0..=18.0, "Shelf gain", " dB", false, "How much the bass is lifted below the shelf frequency.");
        c |= hz(ui, &mut p.bass_boost_hz, 30.0..=200.0, "Boost freq", "Corner frequency of the Bass boost slider. Everything below it gets the extra clean gain.");
        c |= toggle(ui, &mut p.res_on, "Resonance", "Adds a narrow boom at one low frequency, like a ported enclosure.");
        c |= hz(ui, &mut p.res_hz, 25.0..=120.0, "Resonance freq", "Centre frequency of the boom.");
        c |= slider(ui, &mut p.res_db, 0.0..=18.0, "Resonance gain", " dB", false, "How loud the boom is.");
        c |= slider(ui, &mut p.res_q, 0.5..=10.0, "Resonance Q", "", true, "Sharpness of the boom. Higher = narrower and ringier.");
        c
    });

    c |= group(ui, "Distortion", |ui| {
        let mut c = slider(ui, &mut p.drive_max, 1.0..=12.0, "Drive", "x", true, "How hard the signal is pushed into clipping. Higher = more crunch.");
        c |= slider(ui, &mut p.bias_max, 0.0..=0.8, "Bias", "", false, "Asymmetry of the clipping. More bias adds even harmonics and a grittier, buzzier tone.");
        c |= toggle(ui, &mut p.kick_on, "Kick-band saturation", "Extra distortion applied only to the kick/bass band.");
        c |= hz(ui, &mut p.kick_hz, 60.0..=300.0, "Kick freq", "Upper edge of the band that gets the extra distortion.");
        c |= slider(ui, &mut p.kick_drive_max, 1.0..=6.0, "Kick drive", "x", true, "How hard the kick band is distorted.");
        c
    });

    c |= group(ui, "Lo-fi", |ui| {
        let mut c = toggle(ui, &mut p.crush_on, "Bit crush", "Reduces bit depth for a gritty, digital sound.");
        c |= slider(ui, &mut p.crush_bits_min, 6.0..=16.0, "Min bits", " bit", false, "Bit depth reached at 100% intensity. Lower = rougher.");
        c |= toggle(ui, &mut p.hold_on, "Sample hold", "Repeats samples to lower the effective sample rate (aliasing grit).");
        c |= slider(ui, &mut p.hold_max, 1.0..=8.0, "Hold max", "x", false, "Longest hold at 100% intensity. Higher = more aliasing.");
        c |= toggle(ui, &mut p.lpf_on, "Low-pass", "Rolls off the treble, like a cheap speaker.");
        c |= hz(ui, &mut p.lpf_hz_start, 2000.0..=20000.0, "LPF start", "Low-pass cutoff at low intensity.");
        c |= hz(ui, &mut p.lpf_hz_end, 2000.0..=20000.0, "LPF end", "Low-pass cutoff at 100% intensity.");
        c
    });

    c |= group(ui, "Rattle LFO", |ui| {
        let mut c = toggle(ui, &mut p.lfo_on, "Rattle", "Slow wobble in level, like a loose trunk panel rattling.");
        c |= hz(ui, &mut p.lfo_rate_hz, 0.5..=20.0, "Rate", "Speed of the wobble.");
        c |= slider(ui, &mut p.lfo_depth, 0.0..=1.0, "Depth", "", false, "How strong the wobble is. 0 = none.");
        c |= toggle(ui, &mut p.lfo_follow_bass, "Follow bass", "Only wobble while bass is playing, so quiet parts stay clean.");
        let before = p.lfo_target;
        ui.horizontal(|ui| {
            ui.label("Target");
            egui::ComboBox::from_id_salt("jbl_lfo_target")
                .selected_text(match p.lfo_target {
                    JblLfoTarget::Gain => "Gain",
                    JblLfoTarget::BassBand => "Bass band",
                    JblLfoTarget::Both => "Both",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut p.lfo_target, JblLfoTarget::Gain, "Gain");
                    ui.selectable_value(&mut p.lfo_target, JblLfoTarget::BassBand, "Bass band");
                    ui.selectable_value(&mut p.lfo_target, JblLfoTarget::Both, "Both");
                })
                .response
                .on_hover_text("What the wobble modulates: overall level, only the distorted bass band, or both.");
        });
        c |= p.lfo_target != before;
        c
    });

    c |= group(ui, "Output", |ui| {
        let mut c = slider(
            ui,
            &mut p.out_trim_db,
            -12.0..=12.0,
            "Trim",
            " dB",
            false,
            "Final volume adjustment on top of the automatic loudness compensation. The default sound lands about 1 dB below the original volume at any input level; 0 dB here keeps that.",
        );
        c |= toggle(
            ui,
            &mut p.auto_level,
            "Auto level",
            "Makes Bass Mode sound the same at any volume: it follows how loud the music is, so the distortion, pump and bass boom stay the same and the output stays about as loud as the original (just under it). Turn it off for the older behaviour, where loud music got quieter and flatter and quiet music got louder and grittier.",
        );
        c |= toggle(
            ui,
            &mut p.loudness_guard,
            "Level guard",
            "Optional safety net, off by default: keeps Bass Mode from ever being louder than the original sound (plus any Bass boost you add) by turning it down smoothly when it gets too loud. Auto level already matches the volume, so you rarely need this.",
        );
        c
    });

    ui.add_space(6.0);
    if ui
        .button("Reset to defaults")
        .on_hover_text("Restore Intensity and every Advanced setting to the default JBL sound.")
        .clicked()
    {
        *p = JblParams::default();
        *intensity = Config::default().jbl_intensity;
        c = true;
    }
    c
}

pub fn ui(ui: &mut egui::Ui, app: &mut App) {
    egui::CollapsingHeader::new(egui::RichText::new("Fun").heading()).default_open(true).show(ui, |ui| {
        let mut changed = false;
        changed |= ui
            .checkbox(&mut app.cfg.jbl_bass_mode, "JBL Bass Mode")
            .on_hover_text(
                "Bass boost, clipping distortion and bass-keyed pumping on top of the active AutoEQ preset, \
                 like an overdriven car subwoofer.",
            )
            .changed();
        let mut pct = i32::from(app.cfg.jbl_intensity);
        let on = app.cfg.jbl_bass_mode;
        if ui
            .add_enabled(on, egui::Slider::new(&mut pct, 0..=100).suffix("%").text("Intensity"))
            .changed()
        {
            app.cfg.jbl_intensity = pct.clamp(0, 100) as u8;
            changed = true;
        }
        if ui
            .add_enabled(
                on,
                egui::Slider::new(&mut app.cfg.jbl.bass_boost_db, 0.0..=12.0).suffix(" dB").text("Bass boost"),
            )
            .on_hover_text("Extra clean low-end gain on top of the effect, after the distortion. 0 = off.")
            .changed()
        {
            changed = true;
        }
        // Preset picker: the selection is derived from the values every frame (never stored).
        let presets = eq_core::jbl_presets();
        let current = presets.iter().find(|p| p.matches(app.cfg.jbl_intensity, &app.cfg.jbl));
        let mut picked = None;
        ui.add_enabled_ui(on, |ui| {
            ui.horizontal(|ui| {
                ui.label("Preset");
                egui::ComboBox::from_id_salt("jbl_preset")
                    .selected_text(current.map_or("Custom", |p| p.name))
                    .show_ui(ui, |ui| {
                        for p in presets {
                            let sel = current.is_some_and(|c| c.name == p.name);
                            if ui.selectable_label(sel, p.name).on_hover_text(p.description).clicked() {
                                picked = Some(p);
                            }
                        }
                    })
                    .response
                    .on_hover_text("Load a ready-made JBL sound. Editing any value afterwards makes it Custom.");
            });
            ui.label(egui::RichText::new(current.map_or("Custom: hand-edited values.", |p| p.description)).weak());
        });
        if let Some(p) = picked {
            app.cfg.jbl = p.params;
            app.cfg.jbl_intensity = p.intensity;
            changed = true;
        }
        egui::CollapsingHeader::new("Advanced").default_open(false).show(ui, |ui| {
            ui.add_enabled_ui(on, |ui| {
                ui.label("Values are the settings at 100% Intensity; lower Intensity blends toward neutral.");
                changed |= advanced(ui, &mut app.cfg.jbl, &mut app.cfg.jbl_intensity);
            });
        });
        if changed {
            app.on_edited(false);
        }
    });
}
