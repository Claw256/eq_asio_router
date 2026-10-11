//! Making the Hi-Fi Cable follow the ASIO master's sample rate.
//!
//! The decision logic is pure and works against the [`CableFormats`] trait so it can be tested
//! with a fake; [`WindowsCableFormats`] is the real implementation (IPolicyConfig).

use std::sync::Arc;
use std::time::{Duration, Instant};
use wasapi_capture::Flow;

/// One cable endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CableEndpoint {
    pub flow: Flow,
    pub name: String,
    pub id: String,
}

/// Read / change the Windows device formats of the cable endpoints.
pub trait CableFormats: Send + Sync {
    /// All active endpoints (render + capture) whose name contains `base`.
    fn endpoints(&self, base: &str) -> Vec<CableEndpoint>;
    /// Current device-format rate of one endpoint.
    fn get_rate(&self, endpoint_id: &str) -> Result<u32, String>;
    /// Set every endpoint matching `base` to `rate`; one result per endpoint (name, outcome).
    fn set_rate_all(&self, base: &str, rate: u32) -> Vec<(String, Result<(), String>)>;
}

/// Injectable handle (test seam in `EngineOptions`).
#[derive(Clone)]
pub struct CableFormatsHandle(pub Arc<dyn CableFormats>);

impl std::fmt::Debug for CableFormatsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CableFormatsHandle")
    }
}

/// Real implementation on top of `wasapi_capture::policy`.
pub struct WindowsCableFormats;

/// Runs `f` on a fresh thread (COM MTA), so the STA control thread is never involved.
fn on_mta<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    std::thread::Builder::new().name("cable-format".into()).spawn(f).ok()?.join().ok()
}

/// Active VB-cable endpoints (both flows) whose name contains `base`. Virtual Audio Cable and every
/// other endpoint are excluded here, so no caller can read or change them through this provider.
fn vb_endpoints(base: &str) -> Vec<(Flow, wasapi_capture::EndpointInfo)> {
    let needle = base.trim().to_lowercase();
    let mut out = Vec::new();
    for flow in [Flow::Render, Flow::Capture] {
        for e in wasapi_capture::list_endpoints(flow).unwrap_or_default() {
            if is_cable_name(&e.name) && e.name.to_lowercase().contains(&needle) {
                out.push((flow, e));
            }
        }
    }
    out
}

impl CableFormats for WindowsCableFormats {
    fn endpoints(&self, base: &str) -> Vec<CableEndpoint> {
        let base = base.to_string();
        on_mta(move || {
            vb_endpoints(&base).into_iter().map(|(flow, e)| CableEndpoint { flow, name: e.name, id: e.id }).collect()
        })
        .unwrap_or_default()
    }
    fn get_rate(&self, endpoint_id: &str) -> Result<u32, String> {
        let id = endpoint_id.to_string();
        on_mta(move || {
            wasapi_capture::get_endpoint_device_format(&id).map(|f| f.sample_rate).map_err(|e| e.to_string())
        })
        .unwrap_or_else(|| Err("thread failed".into()))
    }
    fn set_rate_all(&self, base: &str, rate: u32) -> Vec<(String, Result<(), String>)> {
        let base = base.to_string();
        on_mta(move || {
            vb_endpoints(&base)
                .into_iter()
                .map(|(_, e)| {
                    let r = wasapi_capture::set_endpoint_device_format(&e.id, rate).map_err(|er| er.to_string());
                    (e.name, r)
                })
                .collect()
        })
        .unwrap_or_default()
    }
}

/// True if `name` is a VB-Audio cable endpoint ("Hi-Fi Cable", "VB-Audio ...", "CABLE Output").
/// Anything else is never touched: not a microphone, and in particular not a Virtual Audio Cable
/// ("Line 1 (Virtual Audio Cable)" is fixed at 48 kHz and must keep its Windows format).
pub fn is_cable_name(name: &str) -> bool {
    eq_core::is_vb_cable_name(name)
}

/// Pure decision: change anything? `rates` are the readable endpoint rates.
pub fn needs_change(rates: &[u32], target: u32) -> bool {
    target != 0 && rates.iter().any(|&r| r != target)
}

/// Whether to attempt matching again, given the last attempt `(target, time, succeeded)`.
/// A failed attempt for the same target is not repeated for 60 s (never loop on a driver
/// that reverts the format).
pub fn should_attempt(last: Option<(u32, Instant, bool)>, target: u32, now: Instant) -> bool {
    match last {
        Some((t, at, false)) if t == target => now.duration_since(at) >= Duration::from_secs(60),
        _ => true,
    }
}

pub fn success_note(from: u32, to: u32) -> String {
    format!("Cable set from {from} Hz to {to} Hz to match the Yamaha")
}

pub fn failure_note(reason: &str, output_name: &str, to: u32) -> String {
    format!(
        "Could not change the cable rate ({reason}): set {output_name} to {to} Hz in Windows Sound > Recording > Properties > Advanced"
    )
}

/// Result of one matching attempt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MatchOutcome {
    /// Rate the cable had before we changed it (None = nothing changed).
    pub changed_from: Option<u32>,
    /// Text for the GUI / CLI (None = nothing to say: already matching or no cable found).
    pub note: Option<String>,
    /// True if an attempt to change was made and it did not end with every endpoint at `target`.
    pub failed: bool,
}

/// Reads the cable formats and, if any endpoint differs from `target`, sets all of them and
/// polls (`poll_total`, every `poll_step`) until Windows reports the new rate. Called once per
/// engine start; it never retries.
pub fn match_cable(
    p: &dyn CableFormats,
    base: &str,
    target: u32,
    poll_total: Duration,
    poll_step: Duration,
) -> MatchOutcome {
    if eq_core::is_vac_name(base) {
        return MatchOutcome::default();
    }
    // Defence in depth: whatever the provider returns, only VB cable endpoints are ever read or changed.
    let eps: Vec<CableEndpoint> = p.endpoints(base).into_iter().filter(|e| is_cable_name(&e.name)).collect();
    if eps.is_empty() || target == 0 {
        return MatchOutcome::default();
    }
    let read = |p: &dyn CableFormats| -> Vec<Option<u32>> { eps.iter().map(|e| p.get_rate(&e.id).ok()).collect() };
    let before = read(p);
    let known: Vec<u32> = before.iter().flatten().copied().collect();
    if !needs_change(&known, target) {
        return MatchOutcome::default();
    }
    // "from" = the capture endpoint's rate if it differs, else the first differing rate.
    let from = eps
        .iter()
        .zip(&before)
        .find(|(e, r)| e.flow == Flow::Capture && r.is_some_and(|r| r != target))
        .and_then(|(_, r)| *r)
        .or_else(|| known.iter().copied().find(|&r| r != target))
        .unwrap_or(0);
    let output_name = eps
        .iter()
        .find(|e| e.flow == Flow::Capture)
        .map(|e| e.name.clone())
        .unwrap_or_else(|| format!("{base} Output"));

    let results = p.set_rate_all(base, target);
    let first_err = results.iter().find_map(|(n, r)| r.as_ref().err().map(|e| format!("{n}: {e}")));

    let start = Instant::now();
    let after = loop {
        let now = read(p);
        let known: Vec<u32> = now.iter().flatten().copied().collect();
        if !needs_change(&known, target) || start.elapsed() >= poll_total {
            break known;
        }
        std::thread::sleep(poll_step);
    };
    if !needs_change(&after, target) && first_err.is_none() {
        return MatchOutcome { changed_from: Some(from), note: Some(success_note(from, target)), failed: false };
    }
    if !needs_change(&after, target) {
        // Windows now reports the target even though a call reported an error: good enough.
        return MatchOutcome { changed_from: Some(from), note: Some(success_note(from, target)), failed: false };
    }
    let reason = first_err.unwrap_or_else(|| {
        let other = after.iter().copied().find(|&r| r != target).unwrap_or(0);
        format!("Windows still reports {other} Hz")
    });
    MatchOutcome { changed_from: None, note: Some(failure_note(&reason, &output_name, target)), failed: true }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Fake: render + capture endpoints with mutable rates; `stuck` makes sets have no effect.
    struct Fake {
        rates: Mutex<Vec<(CableEndpoint, u32)>>,
        stuck: bool,
        fail: Option<String>,
        sets: Mutex<u32>,
    }

    impl Fake {
        fn new(render: u32, capture: u32) -> Fake {
            Fake {
                rates: Mutex::new(vec![
                    (CableEndpoint { flow: Flow::Render, name: "Hi-Fi Cable Input".into(), id: "r".into() }, render),
                    (CableEndpoint { flow: Flow::Capture, name: "Hi-Fi Cable Output".into(), id: "c".into() }, capture),
                ]),
                stuck: false,
                fail: None,
                sets: Mutex::new(0),
            }
        }
    }

    impl CableFormats for Fake {
        fn endpoints(&self, _base: &str) -> Vec<CableEndpoint> {
            self.rates.lock().unwrap().iter().map(|(e, _)| e.clone()).collect()
        }
        fn get_rate(&self, id: &str) -> Result<u32, String> {
            self.rates.lock().unwrap().iter().find(|(e, _)| e.id == id).map(|(_, r)| *r).ok_or("none".into())
        }
        fn set_rate_all(&self, _base: &str, rate: u32) -> Vec<(String, Result<(), String>)> {
            *self.sets.lock().unwrap() += 1;
            let mut v = self.rates.lock().unwrap();
            v.iter_mut()
                .map(|(e, r)| {
                    if let Some(f) = &self.fail {
                        return (e.name.clone(), Err(f.clone()));
                    }
                    if !self.stuck {
                        *r = rate;
                    }
                    (e.name.clone(), Ok(()))
                })
                .collect()
        }
    }

    const Z: Duration = Duration::ZERO;

    #[test]
    fn decision() {
        assert!(!needs_change(&[48000, 48000], 48000));
        assert!(needs_change(&[48000, 44100], 48000));
        assert!(!needs_change(&[], 48000));
        assert!(!needs_change(&[44100], 0));
    }

    #[test]
    fn already_matching_does_nothing() {
        let f = Fake::new(48000, 48000);
        let o = match_cable(&f, "Hi-Fi Cable", 48000, Z, Z);
        assert_eq!(o, MatchOutcome::default());
        assert_eq!(*f.sets.lock().unwrap(), 0);
    }

    #[test]
    fn capture_differs_is_changed_and_noted() {
        let f = Fake::new(48000, 44100);
        let o = match_cable(&f, "Hi-Fi Cable", 48000, Z, Z);
        assert_eq!(o.changed_from, Some(44100));
        assert_eq!(o.note.as_deref(), Some("Cable set from 44100 Hz to 48000 Hz to match the Yamaha"));
        assert!(!o.failed);
        assert_eq!(f.get_rate("c"), Ok(48000));
    }

    #[test]
    fn render_differs_only() {
        let f = Fake::new(44100, 48000);
        let o = match_cable(&f, "Hi-Fi Cable", 48000, Z, Z);
        assert_eq!(o.changed_from, Some(44100));
        assert!(!o.failed);
        assert_eq!(f.get_rate("r"), Ok(48000));
    }

    #[test]
    fn stuck_driver_reports_failure_once() {
        let mut f = Fake::new(48000, 44100);
        f.stuck = true;
        let o = match_cable(&f, "Hi-Fi Cable", 48000, Z, Z);
        assert!(o.failed);
        assert_eq!(o.changed_from, None);
        let n = o.note.unwrap();
        assert!(n.starts_with("Could not change the cable rate (Windows still reports 44100 Hz)"), "{n}");
        assert!(n.contains("set Hi-Fi Cable Output to 48000 Hz in Windows Sound > Recording > Properties > Advanced"));
        assert_eq!(*f.sets.lock().unwrap(), 1, "exactly one set call");
    }

    #[test]
    fn set_error_is_reported() {
        let mut f = Fake::new(48000, 44100);
        f.fail = Some("access denied".into());
        let o = match_cable(&f, "Hi-Fi Cable", 48000, Z, Z);
        assert!(o.failed);
        assert!(o.note.unwrap().contains("access denied"));
    }

    #[test]
    fn no_cable_no_note() {
        struct Empty;
        impl CableFormats for Empty {
            fn endpoints(&self, _: &str) -> Vec<CableEndpoint> {
                vec![]
            }
            fn get_rate(&self, _: &str) -> Result<u32, String> {
                Err("x".into())
            }
            fn set_rate_all(&self, _: &str, _: u32) -> Vec<(String, Result<(), String>)> {
                vec![]
            }
        }
        assert_eq!(match_cable(&Empty, "x", 48000, Z, Z), MatchOutcome::default());
    }

    #[test]
    fn retry_guard() {
        let t0 = Instant::now();
        assert!(should_attempt(None, 48000, t0));
        assert!(!should_attempt(Some((48000, t0, false)), 48000, t0 + Duration::from_secs(5)));
        assert!(should_attempt(Some((48000, t0, false)), 48000, t0 + Duration::from_secs(61)));
        assert!(should_attempt(Some((48000, t0, false)), 96000, t0));
        assert!(should_attempt(Some((48000, t0, true)), 48000, t0));
    }

    #[test]
    fn cable_names() {
        assert!(is_cable_name("Hi-Fi Cable Output"));
        assert!(is_cable_name("Hi-Fi Cable Input (VB-Audio Hi-Fi Cable)"));
        assert!(is_cable_name("CABLE Output (VB-Audio Virtual Cable)"));
        assert!(!is_cable_name("Microphone (Yamaha)"));
        // VAC contains the word "cable" but is not a VB cable
        assert!(!is_cable_name("Line 1 (Virtual Audio Cable)"));
        assert!(!is_cable_name("Virtual Audio Cable"));
    }

    #[test]
    fn vac_is_never_touched() {
        let vac = |flow, id: &str| CableEndpoint { flow, name: "Line 1 (Virtual Audio Cable)".into(), id: id.into() };
        let f = Fake {
            rates: Mutex::new(vec![(vac(Flow::Render, "r"), 44100), (vac(Flow::Capture, "c"), 44100)]),
            stuck: false,
            fail: None,
            sets: Mutex::new(0),
        };
        // by base name ...
        assert_eq!(match_cable(&f, "Line 1 (Virtual Audio Cable)", 48000, Z, Z), MatchOutcome::default());
        assert_eq!(match_cable(&f, "Line 1", 48000, Z, Z), MatchOutcome::default());
        // ... and even if a provider hands VAC endpoints back for a VB base name
        assert_eq!(match_cable(&f, "Hi-Fi Cable", 48000, Z, Z), MatchOutcome::default());
        assert_eq!(*f.sets.lock().unwrap(), 0, "no set call ever");
        assert_eq!(f.get_rate("c"), Ok(44100));
    }

    #[test]
    fn mixed_endpoints_only_vb_ones_count() {
        let f = Fake::new(48000, 48000);
        f.rates.lock().unwrap().push((
            CableEndpoint { flow: Flow::Capture, name: "Line 1 (Virtual Audio Cable)".into(), id: "v".into() },
            44100,
        ));
        // the VAC endpoint at 44.1 kHz must not make the VB cable look mismatched
        assert_eq!(match_cable(&f, "Hi-Fi Cable", 48000, Z, Z), MatchOutcome::default());
        assert_eq!(*f.sets.lock().unwrap(), 0);
    }
}
