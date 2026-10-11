//! Latency and safety settings with the live latency breakdown.

use crate::app::App;
use crate::logic::{capture_segment_label, latency_fractions, learned_margin_text, render_segment_label, render_side_text};
use eq_core::ResamplerQuality;

const SEG: [(&str, egui::Color32); 4] = [
    ("Yamaha out", egui::Color32::from_rgb(90, 160, 235)),
    ("Ring (mean fill)", egui::Color32::from_rgb(235, 185, 60)),
    ("Capture (staleness)", egui::Color32::from_rgb(150, 210, 120)),
    ("Render side", egui::Color32::from_rgb(170, 140, 200)),
];

pub fn ui(ui: &mut egui::Ui, app: &mut App) {
    ui.heading("Latency & safety");
    let mut changed = false;

    ui.horizontal(|ui| {
        ui.label("Safety buffer");
        changed |= ui
            .add(egui::Slider::new(&mut app.cfg.safety_buffer_ms, 0.0..=20.0).suffix(" ms").fixed_decimals(1))
            .changed();
    });
    changed |= ui
        .checkbox(&mut app.cfg.safety_buffer_auto, "Auto")
        .on_hover_text("Raise the safety buffer after underruns and slowly lower it again")
        .changed();

    // Learn (30 s): measure the capture arrival jitter and lock in the lowest safe headroom.
    let learn = app.snap.status.as_ref().map(|s| s.learn.clone());
    let mut start_learn = false;
    let mut clear_learn = false;
    ui.horizontal(|ui| {
        let active = learn.as_ref().is_some_and(|l| l.active);
        let can = learn.is_some() && !active;
        if ui
            .add_enabled(can, egui::Button::new("Learn (30 s)"))
            .on_hover_text(
                "Measures how the capture source delivers audio for 30 s (play something into the cable), then \
                 stores the lowest safe jitter margin. The router starts at that margin from now on and never goes \
                 below it.",
            )
            .clicked()
        {
            start_learn = true;
        }
        if let Some(l) = &learn {
            if l.active {
                ui.add(egui::ProgressBar::new(l.progress_s / l.total_s.max(1.0)).desired_width(140.0).text(format!("{:.0} / {:.0} s", l.progress_s, l.total_s)));
            } else if let Some(e) = &l.error {
                ui.colored_label(crate::app::AMBER, e);
            }
        }
        if app.cfg.jitter_margin_for(&app.cfg.capture_device) > 0.0 {
            ui.label(learned_margin_text(&app.cfg));
            if ui.small_button("Forget").on_hover_text("Go back to the automatic start-up margin").clicked() {
                clear_learn = true;
            }
        } else {
            ui.weak(learned_margin_text(&app.cfg));
        }
    });
    if start_learn {
        app.host.learn();
    }
    if clear_learn {
        app.cfg.set_jitter_margin(0.0);
        changed = true;
    }

    if let Some(s) = &app.snap.status {
        let b = s.latency;
        let f = latency_fractions(&b);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width().min(340.0), 16.0), egui::Sense::hover());
        let mut x = rect.left();
        for (i, (_, color)) in SEG.iter().enumerate() {
            let w = rect.width() * f[i];
            ui.painter().rect_filled(
                egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(w, rect.height())),
                0.0,
                *color,
            );
            x += w;
        }
        let fixed = s.capture_mode_in_use == Some(wasapi_capture::CaptureMode::FixedPeriod);
        let render_label = render_segment_label(&b);
        let names = [SEG[0].0, SEG[1].0, capture_segment_label(fixed), render_label.as_str()];
        let vals = [b.master_out_ms, b.ring_ms, b.capture_ms, b.render_side_ms];
        ui.horizontal_wrapped(|ui| {
            for (i, (_, color)) in SEG.iter().enumerate() {
                ui.colored_label(*color, format!("{} {:.1} ms", names[i], vals[i]));
            }
        });
        ui.strong(format!("Total {:.1} ms{}", b.total_ms, if b.render_basis.is_measured() { "" } else { " (model, render side estimated)" }));
        let now = eq_core::now_unix_s();
        let text = render_side_text(&b, now);
        if b.render_basis.is_measured() {
            ui.weak(text);
        } else {
            ui.colored_label(crate::app::AMBER, text);
        }
        ui.weak(format!(
            "Capture period: {}",
            crate::logic::period_status_text(s.capture_period_frames_requested, s.capture_period_frames_granted, s.cable_rate)
        ));
        if let Some(k) = &s.keepalive {
            ui.weak(format!("Render keep-alive: {}", crate::logic::keepalive_text(k)));
        }
        if let Some(n) = &s.keepalive_note {
            ui.colored_label(crate::app::AMBER, n);
        }
        if let Some(n) = &s.capture_note {
            ui.colored_label(crate::app::AMBER, n);
        }
        ui.weak(format!(
            "Ring target {} fr, mean fill {} fr. Jitter headroom {} fr (model requires {} fr). Capture arrival p99.9: gap {:.1} ms, dip {} fr ({:.0} s measured).",
            s.ring_target_frames,
            s.ring_mean_fill_frames,
            s.jitter_headroom_frames,
            s.required_headroom_frames,
            s.arrival_gap_p999_ms,
            s.arrival_dip_p999_frames,
            s.arrival_observed_s
        ));
        ui.weak(format!(
            "Effective safety buffer {:.1} ms. Underruns {} / overruns {}: raise the buffer if they climb.",
            s.safety_buffer_ms, s.underruns, s.overruns
        ));
    } else {
        ui.weak("Latency breakdown is shown while the engine runs.");
    }

    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        changed |= ui.checkbox(&mut app.cfg.bypass, "Bypass").changed();
        changed |= ui.checkbox(&mut app.cfg.limiter, "Limiter").changed();
        changed |= ui.checkbox(&mut app.cfg.auto_headroom, "Auto headroom").changed();
        changed |= ui.checkbox(&mut app.cfg.mute_on_fault, "Mute on fault").changed();
    });
    ui.horizontal(|ui| {
        ui.label("Resampler");
        let text = match app.cfg.resampler_quality {
            ResamplerQuality::Linear => "Linear",
            ResamplerQuality::Cubic => "Cubic (Hermite)",
        };
        egui::ComboBox::from_id_salt("resampler").selected_text(text).show_ui(ui, |ui| {
            changed |= ui.selectable_value(&mut app.cfg.resampler_quality, ResamplerQuality::Linear, "Linear").changed();
            changed |= ui
                .selectable_value(&mut app.cfg.resampler_quality, ResamplerQuality::Cubic, "Cubic (Hermite)")
                .changed();
        });
    });
    if let Some(a) = &app.snap.last_apply {
        ui.weak(format!("Last apply: {a}"));
    }

    if changed {
        app.on_edited(false);
    }
}
