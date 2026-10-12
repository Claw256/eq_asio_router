//! Live engine status, meters and errors.

use crate::app::{state_label, App, AMBER, GREEN, RED};
use crate::logic::{meter_fraction, ring_gauge};
use eq_core::FaultState;

fn count(ui: &mut egui::Ui, v: u64) {
    if v > 0 {
        ui.colored_label(AMBER, v.to_string());
    } else {
        ui.label("0");
    }
}

pub fn ui(ui: &mut egui::Ui, app: &App) {
    ui.heading("Live status");
    let (text, color) = state_label(&app.snap);
    ui.colored_label(color, text);
    let Some(s) = &app.snap.status else {
        ui.weak("Engine not running.");
        return;
    };

    let (ftext, fcolor) = match s.fault {
        FaultState::Ok => ("Ok", GREEN),
        FaultState::Warn => ("Warn", AMBER),
        FaultState::Muted => ("Muted", RED),
    };
    ui.horizontal(|ui| {
        ui.label("Fault");
        ui.label(egui::RichText::new(format!(" {ftext} ")).background_color(fcolor).color(egui::Color32::BLACK).strong());
    });

    ui.horizontal(|ui| {
        ui.label("Ring");
        ui.add(
            egui::ProgressBar::new(ring_gauge(s.ring_fill_frames, s.ring_target_frames, s.ring_capacity_frames))
                .desired_width(160.0)
                .text(format!("{} / target {}", s.ring_fill_frames, s.ring_target_frames)),
        );
    });

    egui::Grid::new("status_grid").num_columns(2).show(ui, |ui| {
        ui.label("Master");
        ui.label(&s.master_name);
        ui.end_row();
        ui.label("Rates");
        ui.label(format!("source {} Hz / Yamaha {} Hz", s.cable_rate, s.asio_rate));
        ui.end_row();
        ui.label("Buffer");
        ui.label(format!("{} frames", s.buffer_size));
        ui.end_row();
        ui.label("Underruns");
        count(ui, s.underruns);
        ui.end_row();
        ui.label("Overruns");
        count(ui, s.overruns);
        ui.end_row();
        ui.label("Xruns");
        count(ui, s.xruns);
        ui.end_row();
        ui.label("Discarded frames");
        count(ui, s.discarded_frames);
        ui.end_row();
        ui.label("Gate re-arms");
        count(ui, s.gate_rearms);
        ui.end_row();
        ui.label("Drift");
        ui.label(format!("{:+.1} ppm", s.drift_ppm));
        ui.end_row();
        ui.label("CPU");
        ui.label(format!("avg {:.1}% / max {:.1}%", s.cpu_avg_pct, s.cpu_max_pct));
        ui.end_row();
        ui.label("Callback");
        ui.label(format!("avg {:.0} us / max {:.0} us", s.cpu_avg_us, s.cpu_max_us));
        ui.end_row();
        if s.bridge_attached {
            ui.label("Bridge");
            ui.label(format!(
                "{} {} Hz / {} fr, ring A {} / ring B {}",
                if s.vbmatrix_running { "running" } else { "idle" },
                s.bridge_sample_rate,
                s.bridge_buffer_frames,
                s.fill_a,
                s.fill_b
            ));
            ui.end_row();
        }
        ui.label("Source period");
        ui.label(crate::logic::frames_ms_text(s.capture_period_frames, s.cable_rate));
        ui.end_row();
        ui.label("Ring fill");
        ui.label(format!(
            "mean {} fr / target {} fr (headroom {} fr, required {} fr)",
            s.ring_mean_fill_frames, s.ring_target_frames, s.jitter_headroom_frames, s.required_headroom_frames
        ));
        ui.end_row();
        ui.label("Fill stats (p99.9)");
        ui.label(format!(
            "gap {:.1} ms, dip {} fr, staleness {:.1} ms ({:.0} s)",
            s.arrival_gap_p999_ms, s.arrival_dip_p999_frames, s.capture_staleness_ms, s.arrival_observed_s
        ));
        ui.end_row();
        ui.label("Hardening");
        ui.horizontal_wrapped(|ui| {
            for (name, state) in s.hardening.rows() {
                let (mark, color) = match state {
                    Some(true) => ("ok", GREEN),
                    Some(false) => ("failed", RED),
                    None => ("pending", AMBER),
                };
                ui.colored_label(color, format!("{name} {mark}"));
            }
        });
        ui.end_row();
        ui.label("Uptime");
        let secs = s.uptime_ms / 1000;
        ui.label(format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60));
        ui.end_row();
        if s.bridge_attached {
            ui.label("Ring counters");
            ui.label(format!(
                "A over {} / discard {}, B under {} / drop {}",
                s.a_overruns, s.a_discarded, s.b_underruns, s.b_dropped
            ));
            ui.end_row();
        }
        ui.label("Restarts");
        ui.label(s.restarts.to_string());
        ui.end_row();
    });

    ui.add_space(4.0);
    meter(ui, "In L", s.peak_in_db[0]);
    meter(ui, "In R", s.peak_in_db[1]);
    meter(ui, "Out L", s.peak_out_db[0]);
    meter(ui, "Out R", s.peak_out_db[1]);

    let errors = [("Error", &s.last_error), ("Config", &s.config_error)];
    if errors.iter().any(|(_, e)| e.is_some()) {
        ui.add_space(4.0);
        ui.strong("Errors");
        for (k, e) in errors {
            if let Some(e) = e {
                ui.colored_label(RED, format!("{k}: {e}"));
            }
        }
    }
}

fn meter(ui: &mut egui::Ui, label: &str, db: f32) {
    let color = if db > -1.0 {
        RED
    } else if db > -12.0 {
        AMBER
    } else {
        GREEN
    };
    let shown = if db.is_finite() && db > -119.0 { format!("{db:.1} dBFS") } else { "-inf".into() };
    ui.horizontal(|ui| {
        ui.add_sized([44.0, 16.0], egui::Label::new(label));
        ui.add(egui::ProgressBar::new(meter_fraction(db)).fill(color).desired_width(150.0).text(shown));
    });
}
