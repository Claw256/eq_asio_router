//! Hi-Fi Cable rate matching controls (part of the Sample-rate panel).

use crate::app::{App, AMBER, GREEN};
use std::sync::Mutex;

/// Outcome of the manual "set cable now" job (runs off the UI thread).
#[derive(Default)]
struct Job {
    running: bool,
    result: Option<(bool, String)>,
}

static JOB: Mutex<Job> = Mutex::new(Job { running: false, result: None });

fn job() -> std::sync::MutexGuard<'static, Job> {
    JOB.lock().unwrap_or_else(|e| e.into_inner())
}

/// Text for the result of `match_cable_rates` (one line per endpoint).
pub fn result_text(rate: u32, rows: &[wasapi_capture::CableRateResult]) -> (bool, String) {
    let mut ok = true;
    let mut parts = Vec::new();
    for r in rows {
        match &r.result {
            Ok(()) => match (r.before, r.after) {
                (Some(b), Some(a)) if b != a => parts.push(format!("{}: {b} -> {a} Hz", r.endpoint_name)),
                (_, Some(a)) => parts.push(format!("{}: already {a} Hz", r.endpoint_name)),
                _ => parts.push(format!("{}: ok", r.endpoint_name)),
            },
            Err(e) => {
                ok = false;
                parts.push(format!("{}: {e}", r.endpoint_name));
            }
        }
    }
    let head = if ok { format!("Cable now at {rate} Hz") } else { format!("Could not set the cable to {rate} Hz") };
    (ok, format!("{head} ({})", parts.join("; ")))
}

fn spawn_job(base: String, rate: u32, ctx: egui::Context) {
    job().running = true;
    let spawned = std::thread::Builder::new().name("cable-match".into()).spawn(move || {
        let rows = wasapi_capture::match_cable_rates(&[base.as_str()], rate);
        let res = result_text(rate, &rows);
        let mut j = job();
        j.running = false;
        j.result = Some(res);
        ctx.request_repaint();
    });
    if spawned.is_err() {
        let mut j = job();
        j.running = false;
        j.result = Some((false, "Could not start the worker thread".into()));
    }
}

pub fn ui(ui: &mut egui::Ui, app: &mut App, yamaha: u32) {
    let mut changed = false;
    changed |= ui
        .checkbox(
            &mut app.cfg.match_cable_rate,
            "Match cable sample rate to the Yamaha (changes the Windows Hi-Fi Cable format)",
        )
        .changed();
    if let Some(n) = app.snap.status.as_ref().and_then(|s| s.cable_rate_note.clone()) {
        let col = if n.starts_with("Could not") { AMBER } else { GREEN };
        ui.colored_label(col, n);
    }
    let (running, result) = {
        let j = job();
        (j.running, j.result.clone())
    };
    ui.horizontal(|ui| {
        let can = yamaha > 0 && !running;
        let b = ui.add_enabled(can, egui::Button::new("Set cable to Yamaha rate now"));
        if b.clicked() {
            let base = wasapi_capture::cable_base_name(&app.cfg.capture_device);
            job().result = None;
            spawn_job(base, yamaha, ui.ctx().clone());
        }
        if yamaha == 0 {
            ui.weak("(Yamaha rate not known yet)");
        }
        if running {
            ui.spinner();
        }
    });
    if let Some((ok, text)) = result {
        ui.colored_label(if ok { GREEN } else { AMBER }, text);
    }
    ui.add_space(4.0);
    egui::CollapsingHeader::new("Compatibility: music app sounds sped up or crackles").show(ui, |ui| {
        for line in COMPAT_HINT {
            ui.label(*line);
        }
    });
    if changed {
        app.on_edited(false);
    }
}

/// Steps shown to the user when an app (e.g. Spotify) with its own "Exclusive mode" misbehaves.
pub const COMPAT_HINT: &[&str] = &[
    "The Hi-Fi Cable driver does not convert sample rates: a stream opened in exclusive mode at a rate      different from the cable format arrives unconverted (44.1 kHz data on a 48 kHz cable = 8.8 % too fast, with crackle).",
    "Windows also blocks exclusive playback on Hi-Fi Cable Input by default (\"Allow applications to take exclusive control\" is off).",
    "1. In Spotify (or any player) turn OFF \"Exclusive mode\" and leave the output device on Hi-Fi Cable Input.",
    "   With the router there is no benefit from it: the router already outputs bit-exact through the Yamaha ASIO.",
    "2. In shared mode Windows converts any app format correctly (measured: 44.1/16, 44.1/float and 96/24 into a 48 kHz cable all give the exact pitch).",
    "3. If you must use exclusive mode, set the app's rate to the cable format shown above and keep the cable at that rate.",
];

#[cfg(test)]
mod tests {
    use super::*;
    use wasapi_capture::{Flow, PolicyError};

    fn row(name: &str, b: u32, a: u32, ok: bool) -> wasapi_capture::CableRateResult {
        wasapi_capture::CableRateResult {
            flow: Flow::Capture,
            endpoint_name: name.into(),
            endpoint_id: String::new(),
            before: Some(b),
            after: Some(a),
            result: if ok { Ok(()) } else { Err(PolicyError::AccessDenied(name.into())) },
        }
    }

    #[test]
    fn texts() {
        let (ok, t) = result_text(48000, &[row("Out", 44100, 48000, true), row("In", 48000, 48000, true)]);
        assert!(ok && t.contains("Out: 44100 -> 48000 Hz") && t.contains("In: already 48000 Hz"));
        let (ok, t) = result_text(48000, &[row("Out", 44100, 44100, false)]);
        assert!(!ok && t.starts_with("Could not set the cable to 48000 Hz"));
    }
}
