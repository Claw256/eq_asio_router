//! VBMatrix bridge readiness panel: is VBMatrix running, is the virtual ASIO driver
//! registered, is the shared-memory mapping attached, is the slot running, what format
//! did it negotiate and how full are the rings — plus the `[bridge]` settings.
//!
//! The header is read directly through `bridge-abi` (cheap, once per repaint), so the
//! panel also works while the engine is stopped; only the heartbeat-staleness verdict
//! comes from `EngineStatus` when the engine is attached.

use crate::app::{App, AMBER, GREEN, RED};
use crate::logic::Severity;
use bridge_abi::{RING_FRAMES, VERSION};

/// Row helper: mark + label + value.
fn row(ui: &mut egui::Ui, sev: Severity, label: &str, text: String) {
    let color = match sev {
        Severity::Ok => GREEN,
        Severity::Warn => AMBER,
        Severity::Fail => RED,
        Severity::Unknown => egui::Color32::GRAY,
    };
    ui.colored_label(color, sev.mark());
    ui.label(label);
    ui.colored_label(color, text);
    ui.end_row();
}

/// True if a VBMatrix process (any bitness) is running.
fn vbmatrix_process() -> bool {
    crate::sys::process_names().iter().any(|n| {
        let n = n.to_ascii_lowercase();
        n.contains("matrix") && n.contains("vb")
    })
}

pub fn ui(ui: &mut egui::Ui, app: &mut App) {
    ui.heading("VBMatrix bridge");
    let mut changed = false;

    // Direct header read (works with or without a running engine).
    let map = bridge_abi::open();
    let hdr = map.as_ref().map(|m| unsafe { m.header() });

    let vbmatrix_up = vbmatrix_process();
    let registered = asio_host::find_driver("EQ ASIO Router").is_ok();
    let attached = hdr.is_some();
    let rate = hdr.map(|h| h.get_sample_rate()).unwrap_or(0.0);
    let buffer = hdr.map(|h| h.get_buffer_frames()).unwrap_or(0);
    let running = hdr.is_some_and(|h| h.driver_running.load(std::sync::atomic::Ordering::Acquire) != 0);
    let beat = hdr.map(|h| h.driver_beat.load(std::sync::atomic::Ordering::Acquire)).unwrap_or(0);
    let fill_a = hdr.map(|h| h.fill_a()).unwrap_or(0);
    let fill_b = hdr.map(|h| h.fill_b()).unwrap_or(0);
    let a_overruns = hdr.map(|h| h.a_overruns()).unwrap_or(0);
    let a_discarded = hdr.map(|h| h.a_discarded()).unwrap_or(0);
    let b_underruns = hdr.map(|h| h.b_underruns()).unwrap_or(0);
    let b_dropped = hdr.map(|h| h.b_dropped()).unwrap_or(0);

    let engine = app.snap.status.as_ref().filter(|_| app.snap.engine_present);
    let engine_attached = engine.is_some_and(|s| s.bridge_attached);

    egui::Grid::new("bridge_grid").num_columns(3).show(ui, |ui| {
        row(
            ui,
            if vbmatrix_up { Severity::Ok } else { Severity::Warn },
            "VBMatrix process",
            if vbmatrix_up { "running".into() } else { "not running (start VBMatrix)".into() },
        );
        row(
            ui,
            if registered { Severity::Ok } else { Severity::Fail },
            "Virtual ASIO driver",
            if registered { "\"EQ ASIO Router\" registered".into() } else { "\"EQ ASIO Router\" not registered".into() },
        );
        row(
            ui,
            if attached { Severity::Ok } else { Severity::Warn },
            "Mapping",
            if attached {
                format!("attached (layout v{VERSION})")
            } else {
                "not found (open the slot in VBMatrix)".into()
            },
        );
        // Heartbeat verdict: the engine's monitor (driver_running + beat freshness) when
        // attached, else the raw flag.
        let slot_sev = if running { if engine_attached { Severity::Ok } else { Severity::Warn } } else { Severity::Warn };
        let slot_text = if !running {
            "stopped (VBMatrix slot not started)".into()
        } else if engine_attached {
            let ok = engine.is_some_and(|s| s.vbmatrix_running);
            if ok {
                format!("running (heartbeat beat #{beat})")
            } else {
                "running, heartbeat stale".into()
            }
        } else {
            format!("started (beat #{beat}; start the engine for a freshness check)")
        };
        row(ui, slot_sev, "Driver slot", slot_text);
        row(
            ui,
            if rate >= 8_000.0 && buffer > 0 { Severity::Ok } else { Severity::Unknown },
            "Negotiated format",
            if rate >= 8_000.0 && buffer > 0 {
                format!("{} Hz, {} frames ({:.2} ms)", rate.round() as u32, buffer, f64::from(buffer) * 1000.0 / rate)
            } else {
                "not negotiated yet".into()
            },
        );
        let fa_sev = if fill_a == 0 { Severity::Warn } else if fill_a >= RING_FRAMES as u64 { Severity::Warn } else { Severity::Ok };
        row(ui, fa_sev, "Ring A (source)", format!("{fill_a} / {RING_FRAMES} frames"));
        let fb_sev = if app.cfg.bridge.loopback_enabled && fill_b == 0 { Severity::Warn } else { Severity::Ok };
        row(
            ui,
            fb_sev,
            "Ring B (loopback)",
            if app.cfg.bridge.loopback_enabled { format!("{fill_b} / {RING_FRAMES} frames") } else { "disabled".into() },
        );
        row(
            ui,
            if a_overruns + b_dropped > 0 { Severity::Warn } else { Severity::Ok },
            "Ring drops",
            format!("A overrun {a_overruns}, A discarded {a_discarded}, B underrun {b_underruns}, B dropped {b_dropped}"),
        );
        row(
            ui,
            if engine_attached { Severity::Ok } else if attached { Severity::Unknown } else { Severity::Fail },
            "Engine attached",
            if engine_attached { "yes".into() } else { "no (engine not running)".into() },
        );
    });

    ui.add_space(4.0);
    if !vbmatrix_up {
        ui.colored_label(AMBER, "VBMatrix is not running: the source ring stays empty and the output stays silent.");
    }
    if ui
        .checkbox(
            &mut app.cfg.bridge.loopback_enabled,
            "Post-EQ loopback to VBMatrix (ring B)",
        )
        .on_hover_text(
            "Feeds the processed stereo back into the driver's ASIO inputs so VBMatrix can patch it onward. \
             Turning this on/off restarts the stream (~1-2 s).",
        )
        .changed()
    {
        changed = true;
    }
    ui.horizontal(|ui| {
        ui.label("Ring target floor");
        let r = ui.add(
            egui::DragValue::new(&mut app.cfg.bridge.ring_target_ms)
                .range(0.0..=200.0)
                .speed(0.5)
                .max_decimals(1)
                .suffix(" ms"),
        );
        if r.changed() {
            changed = true;
        }
        ui.weak("0 = auto (derive from buffers + safety)");
    });
    ui.weak(
        "Minimum fill target of the source ring. 0 = derive it from the negotiated buffer and the safety \
         buffer (recommended). Applied live.",
    );
    if changed {
        app.on_edited(false);
    }
}
