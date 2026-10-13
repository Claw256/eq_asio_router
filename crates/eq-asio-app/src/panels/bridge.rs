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
use std::collections::VecDeque;
use std::time::Instant;

/// Labels of the four lifetime bridge counters (same order as [`counter_snapshot`]).
pub const COUNTER_LABELS: [&str; 4] = ["A overrun", "A discarded", "B underrun", "B dropped"];

/// The four lifetime counters from the shm header (`None` while no mapping exists).
///
/// They are written by the driver/app even when the engine is stopped, so the
/// readiness panel reads them directly (cheap, once per status tick).
pub fn counter_snapshot() -> Option<[u64; 4]> {
    let m = bridge_abi::open()?;
    let h = unsafe { m.header() };
    Some([h.a_overruns(), h.a_discarded(), h.b_underruns(), h.b_dropped()])
}

/// Display-side view of the lifetime bridge counters: current total, delta since
/// app start (re-baselined by "Reset view") and a live rate over the last 60 s.
///
/// Sampled from the app's status tick ([`App::tick`]), so the rates keep updating
/// while the panel is hidden; the panel only formats. Purely presentational: the
/// raw counters in the mapping are never touched.
#[derive(Clone, Debug)]
pub struct CounterView {
    /// Delta baseline (first sample = app start; "Reset view" re-baselines here).
    base: Option<[u64; 4]>,
    /// The baseline was re-set by "Reset view" (affects the delta label only).
    rebased: bool,
    /// Samples over the last 60 s: (seconds since app start, counts).
    window: VecDeque<(f64, [u64; 4])>,
    /// Last sampled totals.
    current: [u64; 4],
    /// Clock origin (app start).
    start: Instant,
}

impl Default for CounterView {
    fn default() -> Self {
        Self::new()
    }
}

impl CounterView {
    pub fn new() -> Self {
        CounterView {
            base: None,
            rebased: false,
            window: VecDeque::new(),
            current: [0; 4],
            start: Instant::now(),
        }
    }

    /// One status-tick sample; `None` (mapping absent) keeps the last state.
    pub fn sample(&mut self, counts: Option<[u64; 4]>) {
        let Some(c) = counts else { return };
        if self.base.is_none() {
            self.base = Some(c);
        }
        self.current = c;
        let t = self.start.elapsed().as_secs_f64();
        self.window.push_back((t, c));
        // Retain ~60 s of history plus one older anchor for the rate slope.
        while self.window.len() > 2 && t - self.window[1].0 > 60.0 {
            self.window.pop_front();
        }
    }

    /// Total as last sampled.
    pub fn total(&self, i: usize) -> u64 {
        self.current[i]
    }

    /// Delta since the baseline ("Reset view" zeroes it, display-side only).
    pub fn delta(&self, i: usize) -> u64 {
        self.current[i].saturating_sub(self.base.map_or(0, |b| b[i]))
    }

    /// What the delta is measured from ("start" or "reset").
    pub fn delta_label(&self) -> &'static str {
        if self.rebased { "reset" } else { "start" }
    }

    /// Live rate in frames/s over the retained window (0 until two samples exist).
    pub fn rate(&self, i: usize) -> f64 {
        let (Some(&(t0, c0)), Some(&(t1, c1))) = (self.window.front(), self.window.back()) else { return 0.0 };
        if t1 <= t0 {
            return 0.0;
        }
        c1[i].saturating_sub(c0[i]) as f64 / (t1 - t0)
    }

    /// Seconds covered by the rate window.
    pub fn window_s(&self) -> f64 {
        match (self.window.front(), self.window.back()) {
            (Some(&(t0, _)), Some(&(t1, _))) if t1 > t0 => t1 - t0,
            _ => 0.0,
        }
    }

    /// Re-base the delta at the current totals (display-side only).
    pub fn reset(&mut self) {
        self.base = Some(self.current);
        self.rebased = true;
    }
}

/// Rate as "fr/s" (or "fr/min" when very slow).
fn rate_text(r: f64) -> String {
    if r >= 0.1 {
        format!("{r:.2} fr/s")
    } else {
        format!("{:.1} fr/min", r * 60.0)
    }
}

/// Severity color.
fn color_of(sev: Severity) -> egui::Color32 {
    match sev {
        Severity::Ok => GREEN,
        Severity::Warn => AMBER,
        Severity::Fail => RED,
        Severity::Unknown => egui::Color32::GRAY,
    }
}

/// Row helper: mark + label + value.
fn row(ui: &mut egui::Ui, sev: Severity, label: &str, text: String) {
    let color = color_of(sev);
    ui.colored_label(color, sev.mark());
    ui.label(label);
    ui.colored_label(color, text);
    ui.end_row();
}

/// Row with a hover detail line on the value cell.
fn row_detail(ui: &mut egui::Ui, sev: Severity, label: &str, text: String, detail: String) {
    let color = color_of(sev);
    ui.colored_label(color, sev.mark());
    ui.label(label);
    ui.colored_label(color, text).on_hover_text(detail);
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
        // Lifetime counters: current total + delta since app start ("Reset view" re-baselines)
        // + live rate over the last 60 s (sampled from the status tick, see `CounterView`).
        for (i, label) in COUNTER_LABELS.iter().enumerate() {
            let total = app.bridge_view.total(i);
            let delta = app.bridge_view.delta(i);
            let sev = if (i == 0 || i == 3) && total > 0 { Severity::Warn } else { Severity::Ok };
            let text = format!(
                "{total} total, +{delta} since {}, {}",
                app.bridge_view.delta_label(),
                rate_text(app.bridge_view.rate(i))
            );
            let detail = format!(
                "{label}: raw total {total} frames\n\
                 delta since {}: {delta} frames\n\
                 rate over the last {:.0} s: {:.3} fr/s",
                app.bridge_view.delta_label(),
                app.bridge_view.window_s(),
                app.bridge_view.rate(i)
            );
            row_detail(ui, sev, label, text, detail);
        }
        row(
            ui,
            if engine_attached { Severity::Ok } else if attached { Severity::Unknown } else { Severity::Fail },
            "Engine attached",
            if engine_attached { "yes".into() } else { "no (engine not running)".into() },
        );
    });

    ui.add_space(2.0);
    ui.horizontal(|ui| {
        if ui
            .button("Reset view")
            .on_hover_text(
                "Zeroes the delta baselines of the counter rows above (display-side only; the raw lifetime totals are untouched).",
            )
            .clicked()
        {
            app.bridge_view.reset();
        }
        ui.weak("deltas and rates are computed by this view only");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Deltas since app start, rates over the window, "Reset view" re-baselines
    /// (display-side only), and a missing mapping leaves the view untouched.
    #[test]
    fn counter_view_delta_rate_and_reset() {
        let mut v = CounterView::new();
        v.sample(None); // mapping absent: nothing changes
        assert_eq!(v.total(0), 0);
        v.sample(Some([100, 200, 300, 400]));
        assert_eq!([v.delta(0), v.delta(1), v.delta(2), v.delta(3)], [0, 0, 0, 0]);
        assert_eq!(v.rate(0), 0.0, "no slope with a single sample");
        std::thread::sleep(Duration::from_millis(50));
        v.sample(Some([110, 220, 300, 405]));
        assert_eq!(v.delta(0), 10);
        assert_eq!(v.delta(1), 20);
        assert_eq!(v.delta(2), 0);
        assert_eq!(v.delta(3), 5);
        let r = v.rate(0);
        assert!(r > 0.0 && r <= 250.0, "10 frames over >=50 ms -> (0, 200] fr/s, got {r}");
        assert_eq!(v.rate(2), 0.0, "counter that did not move has rate 0");
        assert!(v.window_s() >= 0.05);
        assert_eq!(v.delta_label(), "start");
        // "Reset view": zero the baselines, display-side only (totals untouched).
        v.reset();
        assert_eq!(v.delta_label(), "reset");
        assert_eq!([v.delta(0), v.delta(1)], [0, 0]);
        assert_eq!(v.total(0), 110);
        std::thread::sleep(Duration::from_millis(20));
        v.sample(Some([113, 220, 300, 405]));
        assert_eq!(v.delta(0), 3, "delta grows from the new baseline");
    }
}
