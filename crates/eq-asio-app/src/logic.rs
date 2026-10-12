//! Pure helpers (no I/O, no egui): easy to unit-test.

use eq_core::{Config, LatencyBreakdown, RenderBasis};
use router_engine::HolderInfo;
use std::path::PathBuf;

/// Name of the process of the VB ASIO Bridge that may hold the Yamaha.
pub const BRIDGE_EXE: &str = "VBCABLE_AsioBridge.exe";
/// Yamaha Steinberg USB control panel executable.
pub const YAMAHA_PANEL_EXE: &str = "ysusb_cp.exe";

/// Map dBFS to a 0..=1 meter fill over a -60..0 dB range.
pub fn meter_fraction(db: f32) -> f32 {
    if db.is_nan() {
        return 0.0;
    }
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Segment widths (fractions of the bar, summing to 1 unless everything is 0)
/// for `[Yamaha out, ring, source staleness, render side (measured or estimate)]`.
pub fn latency_fractions(b: &LatencyBreakdown) -> [f32; 4] {
    let parts = [b.master_out_ms, b.ring_ms, b.capture_ms, b.render_side_ms]
        .map(|v| if v.is_finite() { v.max(0.0) } else { 0.0 });
    let sum: f64 = parts.iter().sum();
    if sum <= 0.0 {
        return [0.0; 4];
    }
    parts.map(|p| (p / sum) as f32)
}

/// Fill fraction of the ring gauge (fill relative to twice the target, so the target sits mid-gauge).
pub fn ring_gauge(fill: u32, target: u32, capacity: u32) -> f32 {
    let full = if target > 0 { target.saturating_mul(2) } else { capacity };
    if full == 0 {
        0.0
    } else {
        (fill as f32 / full as f32).clamp(0.0, 1.0)
    }
}

/// Hint shown when the source (VBMatrix bridge) and the master run at different rates.
pub fn rate_hint(source: u32, master: u32) -> Option<String> {
    if source == 0 || master == 0 || source == master {
        return None;
    }
    Some(format!(
        "Source (bridge) is {source} Hz, Yamaha {master} Hz: resampled in the router (extra conversion). Match the \
         rates by picking the same rate for the VBMatrix slot and the engine if you want to avoid it"
    ))
}

/// Registry sub-key (under HKLM) of the in-process server of a CLSID string like `{...}`.
pub fn inproc_subkey(clsid: &str) -> String {
    format!("SOFTWARE\\Classes\\CLSID\\{clsid}\\InprocServer32")
}

/// Registry-style CLSID string from GUID parts, e.g. `{CB7F9FFD-A33B-...}`.
pub fn format_clsid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> String {
    format!(
        "{{{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]
    )
}

/// Control panel path next to the ASIO DLL named by an `InprocServer32` value
/// (quotes and surrounding blanks tolerated). `None` if the value has no folder.
pub fn panel_path_from_inproc(value: &str) -> Option<PathBuf> {
    let v = value.trim().trim_matches('"').trim();
    let idx = v.rfind(['\\', '/'])?;
    let dir = &v[..idx];
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(format!("{dir}\\{YAMAHA_PANEL_EXE}")))
}

/// Convert a NUL-terminated UTF-16 buffer (e.g. `PROCESSENTRY32W::szExeFile`) to a string.
pub fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// True if any process name is the ASIO Bridge (case-insensitive).
pub fn bridge_in<I, S>(names: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    names.into_iter().any(|n| n.as_ref().eq_ignore_ascii_case(BRIDGE_EXE))
}

/// `"A.exe (PID 1), B.exe (PID 2)"`.
pub fn holders_text(holders: &[HolderInfo]) -> String {
    holders.iter().map(|h| format!("{} (PID {})", h.exe_name, h.pid)).collect::<Vec<_>>().join(", ")
}

/// Header state text while the master is held by other programs.
pub fn waiting_label(holders: &[HolderInfo]) -> String {
    if holders.is_empty() {
        "Waiting for Yamaha".to_string()
    } else {
        let names: Vec<&str> = holders.iter().map(|h| h.exe_name.as_str()).collect();
        format!("Waiting for Yamaha (in use by {})", names.join(", "))
    }
}

/// Explanation shown before sending a close request to a holder.
pub fn close_confirm_text(exe: &str) -> String {
    format!(
        "This sends a normal window-close request to {exe}, as if you clicked its X button. It may ask you to          save your work, or ignore the request. The router never force-kills a program."
    )
}

/// Result line after a close request.
pub fn close_result_text(exe: &str, r: &Result<(), String>) -> String {
    match r {
        Ok(()) => format!("Asked {exe} to close; the router will start as soon as the Yamaha is free."),
        Err(e) => format!("Could not ask {exe} to close: {e}. Close it yourself."),
    }
}

/// Label of an output channel: index plus the driver's name when known.
pub fn channel_label(names: &[String], idx: u32) -> String {
    match names.get(idx as usize) {
        Some(n) if !n.is_empty() => format!("{idx}: {n}"),
        _ => idx.to_string(),
    }
}

/// Status text for `Restarting`: waiting-for-device reasons read better without the wrapper.
pub fn restarting_label(reason: &str) -> String {
    if reason.starts_with("Waiting for") {
        reason.to_string()
    } else {
        format!("Restarting ({reason})")
    }
}

// ---------------------------------------------------------------------------------------------
// Shared texts (source / readiness panels were removed with the bridge cutover)
// ---------------------------------------------------------------------------------------------

/// Severity of a readiness row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warn,
    Fail,
    Unknown,
}

impl Severity {
    /// Short text indicator (colour is added by the UI).
    pub fn mark(self) -> &'static str {
        match self {
            Severity::Ok => "\u{2714}",
            Severity::Warn => "\u{26A0}",
            Severity::Fail => "\u{2716}",
            Severity::Unknown => "?",
        }
    }
}

/// Frames -> milliseconds at `rate` (0 for an unknown rate).
pub fn frames_to_ms(frames: u32, rate: u32) -> f64 {
    if rate == 0 {
        0.0
    } else {
        f64::from(frames) * 1000.0 / f64::from(rate)
    }
}

/// "288 frames (6.0 ms)".
pub fn frames_ms_text(frames: u32, rate: u32) -> String {
    if rate == 0 {
        format!("{frames} frames")
    } else {
        format!("{frames} frames ({:.1} ms)", frames_to_ms(frames, rate))
    }
}

/// Legend name of the render-side segment: says whether the figure is measured or an estimate.
pub fn render_segment_label(b: &LatencyBreakdown) -> String {
    match b.render_basis {
        RenderBasis::Measured { unix_s } => format!("Render side (measured {})", eq_core::date_from_unix(unix_s)),
        RenderBasis::Estimate => "Render side (estimate, not measured)".to_string(),
    }
}

/// One-paragraph explanation under the latency bar: where the render-side figure comes from and, when it was
/// measured, how the end-to-end measurement compares with the live model total. `now_unix_s` = 0 skips the age.
pub fn render_side_text(b: &LatencyBreakdown, now_unix_s: u64) -> String {
    let label = b.render_basis.label(now_unix_s);
    match b.render_basis {
        RenderBasis::Measured { .. } => {
            let cmp = b
                .measured_total_ms
                .map(|m| format!(" End-to-end measured {m:.1} ms at that time, model now {:.1} ms.", b.total_ms))
                .unwrap_or_default();
            format!("Render side {:.1} ms ({label}); the default estimate would be {:.0} ms.{cmp}", b.render_side_ms, b.render_estimate_ms)
        }
        _ => format!("Render side {:.0} ms ({label}). The total above is a model; it is not a measurement of this setup.", b.render_side_ms),
    }
}

/// What the learned margin is and its value.
pub fn learned_margin_text(cfg: &Config) -> String {
    let ms = cfg.jitter_margin_for();
    if ms > 0.0 {
        format!("learned margin {ms:.1} ms for the bridge source")
    } else {
        "not learned for the bridge source (automatic)".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_texts() {
        assert_eq!(frames_ms_text(288, 48000), "288 frames (6.0 ms)");
        assert_eq!(frames_ms_text(288, 0), "288 frames");
    }

    #[test]
    fn learned_margin_is_for_the_bridge_source() {
        let mut c = Config::default();
        assert!(learned_margin_text(&c).starts_with("not learned for the bridge source"));
        c.set_jitter_margin(3.2);
        assert_eq!(learned_margin_text(&c), "learned margin 3.2 ms for the bridge source");
        c.set_jitter_margin(0.0);
        assert!(learned_margin_text(&c).starts_with("not learned"));
    }

    #[test]
    fn restarting_text() {
        assert_eq!(restarting_label("Waiting for Hi-Fi Cable Output\u{2026}"), "Waiting for Hi-Fi Cable Output\u{2026}");
        assert_eq!(restarting_label("driver reset"), "Restarting (driver reset)");
    }

    #[test]
    fn meter_mapping() {
        assert_eq!(meter_fraction(-120.0), 0.0);
        assert_eq!(meter_fraction(0.0), 1.0);
        assert_eq!(meter_fraction(6.0), 1.0);
        assert!((meter_fraction(-30.0) - 0.5).abs() < 1e-6);
        assert_eq!(meter_fraction(f32::NAN), 0.0);
    }

    #[test]
    fn latency_bar() {
        let b = LatencyBreakdown { master_out_ms: 18.0, ring_ms: 2.0, capture_ms: 10.0, ..Default::default() };
        let f = latency_fractions(&b);
        assert!((f.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        assert!((f[0] - 0.6).abs() < 1e-6 && (f[1] - 2.0 / 30.0).abs() < 1e-6);
        assert_eq!(latency_fractions(&LatencyBreakdown::default()), [0.0; 4]);
        let nan = LatencyBreakdown { ring_ms: f64::NAN, master_out_ms: 1.0, ..Default::default() };
        assert_eq!(latency_fractions(&nan), [1.0, 0.0, 0.0, 0.0]);
        let with_render = LatencyBreakdown { master_out_ms: 10.0, ring_ms: 10.0, capture_ms: 10.0, render_side_ms: 10.0, ..Default::default() };
        assert_eq!(latency_fractions(&with_render), [0.25; 4]);
    }

    #[test]
    fn render_side_is_labelled_honestly() {
        use eq_core::RenderSide;
        let est = LatencyBreakdown { render_side_ms: 20.0, render_estimate_ms: 20.0, total_ms: 60.0, ..Default::default() };
        assert_eq!(render_segment_label(&est), "Render side (estimate, not measured)");
        let t = render_side_text(&est, 0);
        assert!(t.contains("estimate, not measured") && t.contains("router-cli latency") && t.contains("20 ms"), "{t}");
        let m = RenderSide { ms: 34.0, basis: RenderBasis::Measured { unix_s: 1_791_590_400 }, estimate_ms: 20.0, measured_total_ms: Some(64.5) };
        let b = LatencyBreakdown { render_side_ms: m.ms, render_basis: m.basis, render_estimate_ms: 20.0, measured_total_ms: m.measured_total_ms, total_ms: 63.0, ..Default::default() };
        assert_eq!(render_segment_label(&b), "Render side (measured 2026-10-10)");
        let t = render_side_text(&b, 1_791_590_400 + 2 * 86_400);
        assert!(t.contains("measured 2026-10-10, 2 d old") && t.contains("34.0 ms") && t.contains("estimate would be 20 ms") && t.contains("64.5") && t.contains("63.0"), "{t}");
    }

    #[test]
    fn gauge() {
        assert!((ring_gauge(50, 100, 1000) - 0.25).abs() < 1e-6);
        assert_eq!(ring_gauge(500, 100, 1000), 1.0);
        assert!((ring_gauge(500, 0, 1000) - 0.5).abs() < 1e-6);
        assert_eq!(ring_gauge(5, 0, 0), 0.0);
    }

    #[test]
    fn hints() {
        assert!(rate_hint(48000, 48000).is_none());
        assert!(rate_hint(0, 48000).is_none());
        let h = rate_hint(44100, 48000).unwrap();
        assert!(h.contains("44100 Hz") && h.contains("48000 Hz") && h.contains("resampled"));
    }

    #[test]
    fn clsid_and_panel() {
        let c = format_clsid(0xCB7F9FFD, 0xA33B, 0x48B2, [0x8B, 0xC0, 0x43, 0x7D, 0x94, 0xF3, 0x71, 0x42]);
        assert_eq!(c, "{CB7F9FFD-A33B-48B2-8BC0-437D94F37142}");
        assert_eq!(
            inproc_subkey(&c),
            "SOFTWARE\\Classes\\CLSID\\{CB7F9FFD-A33B-48B2-8BC0-437D94F37142}\\InprocServer32"
        );
        let p = panel_path_from_inproc("C:\\Program Files (x86)\\Yamaha\\Yamaha Steinberg USB Driver\\ysusb_asio64.dll")
            .unwrap();
        assert_eq!(
            p.to_str().unwrap(),
            "C:\\Program Files (x86)\\Yamaha\\Yamaha Steinberg USB Driver\\ysusb_cp.exe"
        );
        let q = panel_path_from_inproc("  \"C:\\Y\\d.dll\" ").unwrap();
        assert_eq!(q.to_str().unwrap(), "C:\\Y\\ysusb_cp.exe");
        assert!(panel_path_from_inproc("ysusb_asio64.dll").is_none());
        assert!(panel_path_from_inproc("").is_none());
    }

    #[test]
    fn bridge_detection() {
        assert!(bridge_in(["explorer.exe", "vbcable_asiobridge.EXE"]));
        assert!(!bridge_in(["explorer.exe"]));
        assert!(!bridge_in(Vec::<String>::new()));
        let mut w: Vec<u16> = "a.exe".encode_utf16().collect();
        w.extend([0, 0x41]);
        assert_eq!(wide_to_string(&w), "a.exe");
        assert_eq!(wide_to_string(&[0x62]), "b");
    }

    #[test]
    fn waiting_texts() {
        let h = |pid, n: &str| HolderInfo { pid, exe_name: n.into() };
        assert_eq!(waiting_label(&[]), "Waiting for Yamaha");
        assert_eq!(
            waiting_label(&[h(12088, "VBAudioMatrixCoconut_x64.exe")]),
            "Waiting for Yamaha (in use by VBAudioMatrixCoconut_x64.exe)"
        );
        assert_eq!(holders_text(&[h(1, "a.exe"), h(2, "b.exe")]), "a.exe (PID 1), b.exe (PID 2)");
        assert!(close_confirm_text("a.exe").contains("never force-kills"));
        assert!(close_result_text("a.exe", &Ok(())).starts_with("Asked a.exe"));
        assert!(close_result_text("a.exe", &Err("no window found".into())).contains("no window found"));
    }

    #[test]
    fn labels() {
        let n = vec!["Mix L".to_string(), String::new()];
        assert_eq!(channel_label(&n, 0), "0: Mix L");
        assert_eq!(channel_label(&n, 1), "1");
        assert_eq!(channel_label(&n, 5), "5");
    }
}
