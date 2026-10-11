//! Capture-source selection logic (pure, unit-tested): how a config maps to WASAPI capture options,
//! the "corruption -> restart once in plain Shared" state machine, and the read-only readiness check
//! of a virtual-cable source (is the endpoint there, is it the default playback device, formats).

use eq_core::{frames_for_ms, Config, RECOMMENDED_CAPTURE_PERIOD_MS};
use wasapi_capture::{is_virtual_cable_name, pair_render_name, CaptureMode, CaptureOptions, EndpointInfo, Flow};

/// Result of [`plan_capture`].
#[derive(Clone, Debug)]
pub struct CapturePlan {
    pub options: CaptureOptions,
    /// Period the config asks for in frames at the endpoint's rate (0 = the driver's default period).
    pub requested_period_frames: u32,
    /// Non-error remark for the status panel (Exclusive mapped to Shared, period ignored, ...).
    pub note: Option<String>,
}

/// Maps the config to capture options.
///
/// * `FixedPeriod` when `capture_period_ms > 0`, the mode is Shared and the endpoint is a known
///   virtual cable; the 6 ms default is passed as `None` so the planner rounds it to the engine's
///   fundamental period.
/// * Exclusive stays mapped to Shared, LowLatency stays LowLatency (WP1 refuses it below 2.5 ms).
/// * `forced_shared` (after a corruption trip) always yields plain Shared.
pub fn plan_capture(cfg: &Config, endpoint_name: &str, endpoint_rate: u32, forced_shared: bool) -> CapturePlan {
    let (mode, mode_note) = crate::engine::effective_capture_mode(cfg.capture_mode);
    let mut notes: Vec<String> = mode_note.map(str::to_string).into_iter().collect();
    let mut options = CaptureOptions::new(mode);
    let mut requested = 0;
    if cfg.capture_period_ms > 0.0 {
        if mode != CaptureMode::Shared {
            notes.push("capture_period_ms is ignored in LowLatency mode".to_string());
        } else if !is_virtual_cable_name(endpoint_name) {
            notes.push(format!(
                "capture period {:.1} ms is ignored: \"{endpoint_name}\" is not a known virtual cable",
                cfg.capture_period_ms
            ));
        } else {
            requested = frames_for_ms(f64::from(cfg.capture_period_ms), endpoint_rate);
            if !forced_shared {
                options.mode = CaptureMode::FixedPeriod;
                options.period_frames = if (cfg.capture_period_ms - RECOMMENDED_CAPTURE_PERIOD_MS).abs() < 0.01 {
                    None
                } else {
                    Some(requested)
                };
            }
        }
    }
    CapturePlan { options, requested_period_frames: requested, note: (!notes.is_empty()).then(|| notes.join(" ")) }
}

/// What to do when the corruption monitor trips.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CorruptionAction {
    /// Restart the capture once in plain Shared; the text is the status note / restart reason.
    RestartInShared(String),
    /// Nothing to do (not a fixed-period session, or the fallback already happened): no restart loops.
    Ignore,
}

/// Remembers that fixed-period capture was abandoned for the current source settings.
///
/// The flag (`forced_shared_until_config_change`) is reset when the capture device or the period
/// changes, never by a restart, so a source that corrupts at its fixed period is reopened in Shared
/// exactly once and then left alone.
#[derive(Clone, Debug, Default)]
pub struct FallbackState {
    forced: Option<String>,
    key: Option<(String, u32)>,
}

impl FallbackState {
    /// Call before every session start with the config in force; returns true if the flag was reset.
    pub fn note_config(&mut self, cfg: &Config) -> bool {
        let key = (cfg.capture_device.trim().to_lowercase(), cfg.capture_period_ms.to_bits());
        let changed = self.key.as_ref().is_some_and(|k| *k != key);
        self.key = Some(key);
        if changed && self.forced.is_some() {
            self.forced = None;
            return true;
        }
        false
    }

    /// Forgets the fallback so fixed-period capture may be tried again (explicit user retry).
    pub fn reset(&mut self) {
        self.forced = None;
    }

    /// True while fixed-period capture must not be used.
    pub fn forced_shared(&self) -> bool {
        self.forced.is_some()
    }

    /// The user-visible text of the fallback (`EngineStatus::capture_fallback`).
    pub fn text(&self) -> Option<&str> {
        self.forced.as_deref()
    }

    /// Feeds a `CaptureEvent::Corruption` of a session running in `session_mode`.
    pub fn on_corruption(&mut self, session_mode: CaptureMode, reason: &str) -> CorruptionAction {
        if session_mode != CaptureMode::FixedPeriod || self.forced.is_some() {
            return CorruptionAction::Ignore;
        }
        let note = fallback_text(reason);
        self.forced = Some(note.clone());
        CorruptionAction::RestartInShared(note)
    }
}

/// "Capture fell back to Shared (10 ms): <reason>".
pub fn fallback_text(reason: &str) -> String {
    format!("Capture fell back to Shared (10 ms): {reason}")
}

// ------------------------------------------------------------------------------------
// Readiness
// ------------------------------------------------------------------------------------

/// Read-only state of a configured capture source.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceReadiness {
    /// The configured capture endpoint is active.
    pub capture_present: bool,
    /// The paired render endpoint is active.
    pub render_present: bool,
    /// The render endpoint is the Windows default playback device (`None` = unknown / no render endpoint).
    pub is_default_playback: Option<bool>,
    /// Both endpoints are stereo at the same rate (`None` = formats could not be read).
    pub formats_ok: Option<bool>,
    /// What the user should do, one sentence each.
    pub notes: Vec<String>,
}

/// Raw facts gathered from Windows (see [`source_readiness`]).
#[derive(Clone, Debug, Default)]
pub struct ReadinessInputs {
    pub captures: Vec<EndpointInfo>,
    pub renders: Vec<EndpointInfo>,
    /// `(sample rate, channels)` of the capture / render endpoint's device format, when readable.
    pub capture_format: Option<(u32, u16)>,
    pub render_format: Option<(u32, u16)>,
}

fn pick<'a>(list: &'a [EndpointInfo], wanted: &str) -> Option<&'a EndpointInfo> {
    let w = wanted.trim().to_lowercase();
    if w.is_empty() {
        return None;
    }
    list.iter().find(|e| e.name.to_lowercase() == w).or_else(|| list.iter().find(|e| e.name.to_lowercase().contains(&w)))
}

/// Pure evaluation of the gathered facts.
pub fn evaluate_readiness(cfg: &Config, i: &ReadinessInputs) -> SourceReadiness {
    let mut r = SourceReadiness::default();
    let cap = pick(&i.captures, &cfg.capture_device);
    r.capture_present = cap.is_some();
    let cap_name = cap.map(|c| c.name.clone()).unwrap_or_else(|| cfg.capture_device.clone());
    if cap.is_none() {
        r.notes.push(format!(
            "Waiting for {}: the capture endpoint is not active (Windows Sound > Recording; show disabled devices and enable it).",
            cfg.capture_device
        ));
    }
    let render = pair_render_name(&cap_name).and_then(|n| pick(&i.renders, &n));
    r.render_present = render.is_some();
    if let Some(rd) = render {
        r.is_default_playback = Some(rd.is_default);
        if !rd.is_default && is_virtual_cable_name(&rd.name) {
            r.notes.push(format!(
                "Set \"{}\" as the Windows default playback device (or choose it in the app) so audio reaches the router.",
                rd.name
            ));
        }
    } else if cap.is_some() {
        r.notes.push("The paired playback endpoint of the cable is not active.".to_string());
    }
    if let (Some((cr, cc)), Some((rr, rc))) = (i.capture_format, i.render_format) {
        let ok = cr == rr && cc == 2 && rc == 2;
        r.formats_ok = Some(ok);
        if !ok {
            r.notes.push(format!(
                "Cable formats differ or are not stereo (capture {cr} Hz/{cc} ch, playback {rr} Hz/{rc} ch); set both to the same rate in Windows Sound > Properties > Advanced."
            ));
        }
    }
    r
}

/// Runs a closure on a fresh thread (its own COM apartment) so callers on STA/GUI threads are safe.
fn on_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    std::thread::Builder::new().name("source-readiness".into()).spawn(f).ok()?.join().ok()
}

/// Gathers the endpoint list and device formats (read-only: nothing is opened for streaming or
/// changed) and evaluates them for `cfg`. Safe to call from a GUI thread; takes a few ms.
pub fn source_readiness(cfg: &Config) -> SourceReadiness {
    let cfg2 = cfg.clone();
    on_thread(move || {
        let captures = wasapi_capture::list_endpoints(Flow::Capture).unwrap_or_default();
        let renders = wasapi_capture::list_endpoints(Flow::Render).unwrap_or_default();
        let cap = pick(&captures, &cfg2.capture_device).cloned();
        let rd = cap.as_ref().and_then(|c| pair_render_name(&c.name)).and_then(|n| pick(&renders, &n).cloned());
        let fmt = |e: &Option<EndpointInfo>| {
            e.as_ref().and_then(|e| wasapi_capture::get_endpoint_device_format(&e.id).ok()).map(|f| (f.sample_rate, f.channels))
        };
        let inputs = ReadinessInputs { capture_format: fmt(&cap), render_format: fmt(&rd), captures, renders };
        evaluate_readiness(&cfg2, &inputs)
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eq_core::{CaptureMode as C, SourcePreset};

    fn vac_cfg() -> Config {
        let mut c = Config::default();
        c.apply_source_preset(SourcePreset::Vac);
        c
    }
    const VAC: &str = "Line 1 (Virtual Audio Cable)";

    #[test]
    fn vac_preset_plans_fixed_period() {
        let p = plan_capture(&vac_cfg(), VAC, 48000, false);
        assert_eq!(p.options.mode, CaptureMode::FixedPeriod);
        assert_eq!(p.options.period_frames, None, "6 ms default is left to the planner");
        assert_eq!(p.requested_period_frames, 288);
        assert!(p.note.is_none());
        let mut c = vac_cfg();
        c.capture_period_ms = 7.5;
        let p = plan_capture(&c, VAC, 48000, false);
        assert_eq!((p.options.mode, p.options.period_frames, p.requested_period_frames), (CaptureMode::FixedPeriod, Some(360), 360));
    }

    #[test]
    fn cable_default_and_non_cable_stay_shared() {
        let c = Config::default();
        let p = plan_capture(&c, "Hi-Fi Cable Output (VB-Audio Hi-Fi Cable)", 48000, false);
        assert_eq!((p.options.mode, p.requested_period_frames), (CaptureMode::Shared, 0));
        // a period on a real (non-cable) endpoint is ignored with a note
        let mut c = vac_cfg();
        c.capture_device = "Microphone (USB)".into();
        let p = plan_capture(&c, "Microphone (USB)", 48000, false);
        assert_eq!(p.options.mode, CaptureMode::Shared);
        assert!(p.note.as_deref().unwrap_or("").contains("not a known virtual cable"));
    }

    #[test]
    fn exclusive_and_lowlatency_mapping_is_unchanged() {
        let mut c = vac_cfg();
        c.capture_mode = C::Exclusive;
        let p = plan_capture(&c, VAC, 48000, false);
        assert_eq!(p.options.mode, CaptureMode::FixedPeriod, "Exclusive is mapped to Shared, which then gets the period");
        assert!(p.note.as_deref().unwrap_or("").contains("Exclusive"));
        let mut c = Config { capture_mode: C::LowLatency, ..Config::default() };
        assert_eq!(plan_capture(&c, VAC, 48000, false).options.mode, CaptureMode::LowLatency);
        c.capture_period_ms = 6.0;
        let p = plan_capture(&c, VAC, 48000, false);
        assert_eq!(p.options.mode, CaptureMode::LowLatency, "LowLatency is never silently upgraded");
        assert!(p.note.is_some());
    }

    #[test]
    fn forced_shared_overrides_but_keeps_the_request() {
        let p = plan_capture(&vac_cfg(), VAC, 48000, true);
        assert_eq!(p.options.mode, CaptureMode::Shared);
        assert_eq!(p.requested_period_frames, 288, "the GUI still shows requested 288 vs granted 480");
    }

    #[test]
    fn corruption_restarts_once_in_shared_then_never_again() {
        let cfg = vac_cfg();
        let mut st = FallbackState::default();
        assert!(!st.note_config(&cfg));
        assert!(!st.forced_shared() && st.text().is_none());
        let a = st.on_corruption(CaptureMode::FixedPeriod, "repeated identical blocks (3 within 500 ms)");
        assert_eq!(
            a,
            CorruptionAction::RestartInShared(
                "Capture fell back to Shared (10 ms): repeated identical blocks (3 within 500 ms)".into()
            )
        );
        assert!(st.forced_shared());
        // the restarted session runs in Shared: a second event (or the same event again) does nothing
        assert_eq!(st.on_corruption(CaptureMode::Shared, "x"), CorruptionAction::Ignore);
        assert_eq!(st.on_corruption(CaptureMode::FixedPeriod, "x"), CorruptionAction::Ignore);
        // plain restarts / unrelated config edits keep the fallback
        let mut other = cfg.clone();
        other.safety_buffer_ms = 5.0;
        assert!(!st.note_config(&other));
        assert!(!st.note_config(&cfg));
        assert!(st.forced_shared());
        assert!(st.text().unwrap().starts_with("Capture fell back to Shared"));
        // changing the period or the device re-arms the fixed-period attempt
        let mut p = cfg.clone();
        p.capture_period_ms = 7.0;
        assert!(st.note_config(&p));
        assert!(!st.forced_shared());
        st.on_corruption(CaptureMode::FixedPeriod, "y");
        let mut d = p.clone();
        d.capture_device = "Hi-Fi Cable Output".into();
        assert!(st.note_config(&d));
        assert!(!st.forced_shared());
        // a non-fixed session never triggers a fallback
        let mut fresh = FallbackState::default();
        assert_eq!(fresh.on_corruption(CaptureMode::Shared, "z"), CorruptionAction::Ignore);
        assert!(!fresh.forced_shared());
        // an explicit user retry forgets the fallback without a config change
        let mut r = FallbackState::default();
        r.on_corruption(CaptureMode::FixedPeriod, "q");
        assert!(r.forced_shared());
        r.reset();
        assert!(!r.forced_shared() && r.text().is_none());
    }

    fn ep(name: &str, def: bool) -> EndpointInfo {
        EndpointInfo { id: name.to_string(), name: name.to_string(), is_default: def }
    }

    #[test]
    fn readiness_all_good() {
        let i = ReadinessInputs {
            captures: vec![ep(VAC, false), ep("Mic", true)],
            renders: vec![ep(VAC, true), ep("Speakers", false)],
            capture_format: Some((48000, 2)),
            render_format: Some((48000, 2)),
        };
        let r = evaluate_readiness(&vac_cfg(), &i);
        assert_eq!(
            r,
            SourceReadiness {
                capture_present: true,
                render_present: true,
                is_default_playback: Some(true),
                formats_ok: Some(true),
                notes: vec![]
            }
        );
    }

    #[test]
    fn readiness_missing_capture_and_not_default() {
        let none = evaluate_readiness(&vac_cfg(), &ReadinessInputs::default());
        assert!(!none.capture_present && !none.render_present);
        assert_eq!((none.is_default_playback, none.formats_ok), (None, None));
        assert!(none.notes[0].starts_with("Waiting for Line 1 (Virtual Audio Cable)"));
        let i = ReadinessInputs {
            captures: vec![ep(VAC, false)],
            renders: vec![ep(VAC, false)],
            capture_format: Some((48000, 2)),
            render_format: Some((44100, 2)),
        };
        let r = evaluate_readiness(&vac_cfg(), &i);
        assert!(r.capture_present && r.render_present);
        assert_eq!((r.is_default_playback, r.formats_ok), (Some(false), Some(false)));
        assert_eq!(r.notes.len(), 2);
        assert!(r.notes[0].contains("default playback") && r.notes[1].contains("44100"));
    }

    #[test]
    fn readiness_hifi_cable_pairs_output_to_input() {
        let cfg = Config::default(); // "Hi-Fi Cable Output"
        let i = ReadinessInputs {
            captures: vec![ep("Hi-Fi Cable Output (VB-Audio Hi-Fi Cable)", false)],
            renders: vec![ep("Hi-Fi Cable Input (VB-Audio Hi-Fi Cable)", true)],
            ..Default::default()
        };
        let r = evaluate_readiness(&cfg, &i);
        assert!(r.capture_present && r.render_present);
        assert_eq!((r.is_default_playback, r.formats_ok), (Some(true), None));
    }

    #[test]
    fn waiting_text_for_missing_line_1() {
        let e = crate::status::EngineError::CaptureDeviceNotFound(VAC.into());
        assert!(e.is_unavailable() && !e.is_fatal());
        assert_eq!(e.waiting_text(), "Waiting for Line 1 (Virtual Audio Cable)\u{2026}");
    }
}
