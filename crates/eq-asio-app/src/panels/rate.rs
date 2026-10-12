//! Sample rate, buffer size and the Yamaha control panel button.

use crate::app::{App, RED};
use crate::logic::rate_hint;
use eq_core::COMMON_RATES;

pub fn ui(ui: &mut egui::Ui, app: &mut App) {
    ui.heading("Sample rate & buffer");
    let mut changed = false;

    let rates: Vec<u32> = match &app.snap.caps {
        Some(c) if !c.supported_rates.is_empty() => c.supported_rates.clone(),
        _ => COMMON_RATES.to_vec(),
    };
    let text = if app.cfg.sample_rate == 0 {
        "Keep Yamaha's current".to_string()
    } else {
        format!("{} Hz", app.cfg.sample_rate)
    };
    ui.horizontal(|ui| {
        ui.label("Engine rate");
        egui::ComboBox::from_id_salt("rate").selected_text(text).show_ui(ui, |ui| {
            changed |= ui.selectable_value(&mut app.cfg.sample_rate, 0, "Keep Yamaha's current").changed();
            for r in rates {
                changed |= ui.selectable_value(&mut app.cfg.sample_rate, r, format!("{r} Hz")).changed();
            }
        });
    });

    let st = app.snap.status.as_ref();
    let cable = st.map_or(0, |s| s.cable_rate);
    let yamaha = st.map(|s| s.asio_rate).filter(|r| *r > 0).or(app.snap.caps.as_ref().map(|c| c.sample_rate)).unwrap_or(0);
    egui::Grid::new("rates").num_columns(2).show(ui, |ui| {
        ui.label("Source (bridge)");
        ui.label(if cable > 0 { format!("{cable} Hz") } else { "-".into() });
        ui.end_row();
        ui.label("Yamaha rate");
        ui.label(if yamaha > 0 { format!("{yamaha} Hz") } else { "-".into() });
        ui.end_row();
        ui.label("Buffer size");
        let buf = st.map(|s| s.buffer_size).filter(|b| *b > 0).or(app
            .snap
            .caps
            .as_ref()
            .map(|c| c.buffer_preferred.max(0) as u32)
            .filter(|b| *b > 0));
        ui.label(match buf {
            Some(b) if yamaha > 0 => format!("{b} frames ({:.2} ms)", f64::from(b) * 1000.0 / f64::from(yamaha)),
            Some(b) => format!("{b} frames"),
            None => "-".into(),
        });
        ui.end_row();
    });
    if let Some(h) = rate_hint(cable, yamaha) {
        ui.colored_label(crate::app::AMBER, h);
    }

    ui.add_space(4.0);
    let enabled = app.panel_path.is_some();
    let resp = ui.add_enabled(enabled, egui::Button::new("Open Yamaha control panel"));
    let resp = if enabled {
        resp
    } else {
        resp.on_disabled_hover_text("ysusb_cp.exe was not found next to the master's ASIO DLL")
    };
    if resp.clicked() {
        app.open_yamaha_panel();
    }
    if let Some(e) = &app.panel_error {
        ui.colored_label(RED, e);
    }
    ui.weak("Buffer size is set in the Yamaha panel; restart the engine after changing it.");
    if ui.add_enabled(app.snap.engine_present, egui::Button::new("Restart engine")).clicked() {
        app.host.restart();
    }

    if changed {
        app.on_edited(false);
    }
}
