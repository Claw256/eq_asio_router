//! Render keep-alive for the render counterpart of a virtual cable's capture endpoint.
//!
//! A silent, event-driven shared render stream at the endpoint's minimum IAudioClient3 period
//! lowers the shared engine period of that render endpoint for every app rendering into it (VAC
//! "Line 1" supports 1 ms), which shortens the render half of the cable latency.
//!
//! Safety: a keep-alive is only ever started on an endpoint whose name matches a known
//! virtual-cable pattern ([`is_virtual_cable_name`]) or an explicit caller allow-list, never on a
//! real output device. It renders digital silence only.

use crate::device::{list_endpoints, EndpointInfo, Flow};
use crate::loopback::{current_engine_period, KeepAlive, KeepAliveInfo, KeepAliveOptions};
use crate::{CaptureError, CaptureEvent, Result};
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// Substrings (lower case) that identify a virtual-cable endpoint.
const CABLE_PATTERNS: &[&str] = &[
    "virtual audio cable",
    "vb-audio",
    "vb-cable",
    "hi-fi cable",
    "cable input",
    "cable output",
    "virtual cable",
];

/// True when `name` looks like a virtual-cable endpoint (never a real output device).
pub fn is_virtual_cable_name(name: &str) -> bool {
    let n = name.to_lowercase();
    CABLE_PATTERNS.iter().any(|p| n.contains(p))
}

/// Name of the render endpoint that pairs with a cable's capture endpoint name.
///
/// * `Line 1 (Virtual Audio Cable)` -> `Line 1 (Virtual Audio Cable)` (same name)
/// * `Hi-Fi Cable Output (VB-Audio Hi-Fi Cable)` -> `Hi-Fi Cable Input (VB-Audio Hi-Fi Cable)`
/// * `CABLE Output` -> `CABLE Input`
///
/// Names that already end in "Input" and names without an Output/Input word are returned
/// unchanged. `None` for an empty name. This is pure string logic; whether the result is safe to
/// use is decided by [`is_virtual_cable_name`].
pub fn pair_render_name(capture_name: &str) -> Option<String> {
    let t = capture_name.trim();
    if t.is_empty() {
        return None;
    }
    let (base, tail) = match t.find(" (") {
        Some(i) => (&t[..i], &t[i..]),
        None => (t, ""),
    };
    let lower = base.to_lowercase();
    if lower.ends_with(" output") {
        let stem = &base[..base.len() - " output".len()];
        Some(format!("{stem} Input{tail}"))
    } else {
        Some(t.to_string())
    }
}

/// Options for [`RenderKeepAlive::start_for_capture_endpoint`].
#[derive(Clone, Debug)]
pub struct RenderKeepAliveOptions {
    /// Extra endpoint-name substrings (case-insensitive) that may be kept alive although they do
    /// not match a known virtual-cable pattern. Empty = built-in patterns only.
    pub allow_list: Vec<String>,
    /// Request the minimum IAudioClient3 period (the point of the keep-alive).
    pub low_latency: bool,
    /// Settle time before the engine period is read back.
    pub settle: Duration,
}

impl Default for RenderKeepAliveOptions {
    fn default() -> Self {
        Self { allow_list: Vec::new(), low_latency: true, settle: Duration::from_millis(400) }
    }
}

/// What the keep-alive did.
#[derive(Clone, Debug)]
pub struct RenderKeepAliveInfo {
    pub render_name: String,
    pub render_id: String,
    pub stream: KeepAliveInfo,
    /// `(frames, rate)` engine period of the render endpoint before the keep-alive started.
    pub engine_period_before: Option<(u32, u32)>,
    /// `(frames, rate)` engine period after it started (`current_engine_period`).
    pub engine_period_after: Option<(u32, u32)>,
}

/// A running keep-alive on a cable's render endpoint. Dropping it stops the stream.
pub struct RenderKeepAlive {
    inner: KeepAlive,
    info: RenderKeepAliveInfo,
}

fn pick_render(renders: &[EndpointInfo], wanted: &str) -> Option<EndpointInfo> {
    let w = wanted.to_lowercase();
    renders
        .iter()
        .find(|e| e.name.to_lowercase() == w)
        .or_else(|| renders.iter().find(|e| e.name.to_lowercase().contains(&w)))
        .cloned()
}

fn allowed(name: &str, allow_list: &[String]) -> bool {
    let n = name.to_lowercase();
    is_virtual_cable_name(name) || allow_list.iter().any(|a| !a.is_empty() && n.contains(&a.to_lowercase()))
}

impl RenderKeepAlive {
    /// Start a silent minimum-period render stream on the render endpoint paired with the capture
    /// endpoint `capture_id_or_name` (an endpoint id, or a name / name substring; the capture
    /// endpoint itself may be inactive, only its name is used).
    pub fn start_for_capture_endpoint(capture_id_or_name: &str, options: RenderKeepAliveOptions) -> Result<Self> {
        let caps = list_endpoints(Flow::Capture).unwrap_or_default();
        let cap_name = caps
            .iter()
            .find(|e| e.id == capture_id_or_name)
            .or_else(|| find_cap_by_name(&caps, capture_id_or_name))
            .map(|e| e.name.clone())
            .unwrap_or_else(|| capture_id_or_name.to_string());
        let wanted = pair_render_name(&cap_name)
            .ok_or_else(|| CaptureError::NotFound(capture_id_or_name.to_string()))?;
        let renders = list_endpoints(Flow::Render)?;
        let render = pick_render(&renders, &wanted).ok_or_else(|| CaptureError::NotFound(wanted.clone()))?;
        if !allowed(&render.name, &options.allow_list) {
            return Err(CaptureError::Refused(format!(
                "render endpoint \"{}\" is not a known virtual cable; refusing to run a keep-alive on it",
                render.name
            )));
        }
        let before = current_engine_period(&render.id);
        let inner = KeepAlive::start(&render.id, KeepAliveOptions { low_latency: options.low_latency, signal: None })?;
        std::thread::sleep(options.settle);
        let after = current_engine_period(&render.id);
        let info = RenderKeepAliveInfo {
            render_name: render.name,
            render_id: render.id,
            stream: inner.info().clone(),
            engine_period_before: before,
            engine_period_after: after,
        };
        Ok(Self { inner, info })
    }

    pub fn info(&self) -> &RenderKeepAliveInfo {
        &self.info
    }
    pub fn events(&self) -> &Receiver<CaptureEvent> {
        self.inner.events()
    }
    pub fn is_running(&self) -> bool {
        self.inner.is_running()
    }
    pub fn is_lost(&self) -> bool {
        self.inner.is_lost()
    }
    pub fn underruns(&self) -> u32 {
        self.inner.underruns()
    }
    pub fn frames_rendered(&self) -> u64 {
        self.inner.frames_rendered()
    }
    /// Current engine period `(frames, rate)` of the render endpoint (re-read).
    pub fn current_engine_period(&self) -> Option<(u32, u32)> {
        current_engine_period(&self.info.render_id)
    }
    pub fn stop(&mut self) {
        self.inner.stop();
    }
}

fn find_cap_by_name<'a>(caps: &'a [EndpointInfo], name: &str) -> Option<&'a EndpointInfo> {
    let n = name.to_lowercase();
    caps.iter().find(|e| e.name.to_lowercase() == n).or_else(|| caps.iter().find(|e| e.name.to_lowercase().contains(&n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing() {
        assert_eq!(pair_render_name("Line 1 (Virtual Audio Cable)").as_deref(), Some("Line 1 (Virtual Audio Cable)"));
        assert_eq!(
            pair_render_name("Hi-Fi Cable Output (VB-Audio Hi-Fi Cable)").as_deref(),
            Some("Hi-Fi Cable Input (VB-Audio Hi-Fi Cable)")
        );
        assert_eq!(pair_render_name("CABLE Output").as_deref(), Some("CABLE Input"));
        assert_eq!(
            pair_render_name("CABLE Output (VB-Audio Virtual Cable)").as_deref(),
            Some("CABLE Input (VB-Audio Virtual Cable)")
        );
        assert_eq!(pair_render_name("cable output (x)").as_deref(), Some("cable Input (x)"));
        assert_eq!(pair_render_name("Hi-Fi Cable Input").as_deref(), Some("Hi-Fi Cable Input"));
        assert_eq!(pair_render_name("  Hi-Fi Cable Output  ").as_deref(), Some("Hi-Fi Cable Input"));
        assert_eq!(pair_render_name(""), None);
        assert_eq!(pair_render_name("   "), None);
    }

    #[test]
    fn virtual_cable_patterns() {
        for n in [
            "Line 1 (Virtual Audio Cable)",
            "Hi-Fi Cable Input (VB-Audio Hi-Fi Cable)",
            "CABLE Input (VB-Audio Virtual Cable)",
            "CABLE-A Input (VB-Audio Cable A)",
            "Line 2 (virtual audio cable)",
        ] {
            assert!(is_virtual_cable_name(n), "{n}");
        }
        for n in [
            "Speakers (Realtek(R) Audio)",
            "Headphones (Yamaha UR22C)",
            "Digital Audio (S/PDIF) (NVIDIA High Definition Audio)",
            "Line Out (Focusrite USB Audio)",
            "Line 1",
        ] {
            assert!(!is_virtual_cable_name(n), "{n}");
        }
    }

    #[test]
    fn allow_list_extends_but_never_implicitly() {
        assert!(!allowed("Speakers (Realtek)", &[]));
        assert!(!allowed("Speakers (Realtek)", &[String::new()]));
        assert!(allowed("Line 1", &["line 1".to_string()]));
        assert!(allowed("Line 1 (Virtual Audio Cable)", &[]));
    }

    #[test]
    fn pick_prefers_exact_name() {
        let eps = vec![
            EndpointInfo { id: "a".into(), name: "Line 1 (Virtual Audio Cable) 2".into(), is_default: false },
            EndpointInfo { id: "b".into(), name: "Line 1 (Virtual Audio Cable)".into(), is_default: false },
        ];
        assert_eq!(pick_render(&eps, "Line 1 (Virtual Audio Cable)").unwrap().id, "b");
        assert_eq!(pick_render(&eps, "line 1").unwrap().id, "a");
        assert!(pick_render(&eps, "nothing").is_none());
    }
}
