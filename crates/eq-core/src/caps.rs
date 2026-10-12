//! Master-driver capabilities published by the engine for the GUI.
//!
//! The GUI may be unable to open the exclusive ASIO driver itself, so the engine
//! writes what it learned from the driver to `%APPDATA%\eq_asio_router\caps.toml`.

use crate::config::ConfigError;
use crate::measured::{RenderBasis, RenderSide};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Sample rates commonly offered by audio interfaces.
pub const COMMON_RATES: [u32; 6] = [44100, 48000, 88200, 96000, 176400, 192000];

/// What the engine learned from the master ASIO driver.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Caps {
    pub master_name: String,
    pub driver_version: i32,
    /// Current sample rate in Hz.
    pub sample_rate: u32,
    /// Rates the driver accepted via `canSampleRate`.
    pub supported_rates: Vec<u32>,
    /// Buffer sizes in frames.
    pub buffer_min: i32,
    pub buffer_preferred: i32,
    pub buffer_max: i32,
    pub buffer_granularity: i32,
    /// Reported latencies in samples.
    pub input_latency: i32,
    pub output_latency: i32,
    pub in_channels: u32,
    pub out_channels: u32,
    pub in_names: Vec<String>,
    pub out_names: Vec<String>,
    /// ASIO sample type code of the first channel.
    pub sample_type: i32,
    /// Time the file was written (Unix epoch, ms).
    pub updated_unix_ms: u64,
}

/// End-to-end latency components in milliseconds (source application -> Yamaha DAC input).
///
/// Honest model, no term counted twice:
///
/// * `render_side_ms`: the application's audio waits in the render queue, the Windows engine period and
///   the cable driver before it is available for capture. The router cannot observe it: it is the
///   figure measured by `router-cli latency` when a fresh measurement for this source exists
///   (`render_basis` = `Measured`), else a default estimate (`crate::measured`, 20 ms VAC, 100 ms Hi-Fi
///   Cable), and the UIs must say which.
/// * `capture_ms`: mean *staleness* of a frame when the capture callback delivers it. A frame in a
///   packet of `b` frames is on average `b / (2 rate)` old when the packet is delivered; for regular
///   10 ms packets that is 5 ms, for the Hi-Fi Cable's pairs of packets delivered back to back it is
///   10 ms (the first packet of a pair has waited an extra period). The engine measures it
///   (`ArrivalSnapshot::staleness_s`); this is **not** the ring's content, so it does not overlap
///   with `ring_ms`.
/// * `ring_ms`: how long frames wait in the router ring before the ASIO callback pulls them: the
///   mean actual fill (falls back to the target before a mean is available).
/// * `master_out_ms`: driver-reported output latency (includes the driver's buffer).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LatencyBreakdown {
    /// Driver-reported output latency (includes the driver buffer).
    pub master_out_ms: f64,
    /// Driver-reported input latency (informational; not on the playback path).
    pub master_in_ms: f64,
    /// One driver buffer (informational; already part of `master_out_ms`).
    pub buffer_ms: f64,
    /// Time frames wait in the router ring (mean actual fill, else the target).
    pub ring_ms: f64,
    /// Mean staleness of a frame at capture delivery (see the type docs).
    pub capture_ms: f64,
    /// Render side of the source application (measured by `router-cli latency`, or an estimate: see `render_basis`).
    pub render_side_ms: f64,
    /// Where `render_side_ms` comes from.
    pub render_basis: RenderBasis,
    /// What the default estimate would say for this source (for the measured-versus-estimate comparison).
    pub render_estimate_ms: f64,
    /// End-to-end total (to-DAC equivalent) at the time of the measurement, if the render side is measured.
    pub measured_total_ms: Option<f64>,
    /// Playback path total: `master_out_ms + ring_ms + capture_ms + render_side_ms`.
    pub total_ms: f64,
}

impl Caps {
    /// Default location: `%APPDATA%\eq_asio_router\caps.toml`.
    pub fn default_path() -> Option<PathBuf> {
        let appdata = std::env::var_os("APPDATA")?;
        Some(PathBuf::from(appdata).join("eq_asio_router").join("caps.toml"))
    }

    /// Load from a TOML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Caps, ConfigError> {
        Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
    }

    /// Save atomically (temp file + rename), creating parent directories.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let path = path.as_ref();
        let text = toml::to_string_pretty(self)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, text)?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// Convert a frame count at the driver's sample rate to milliseconds (0 if rate unknown).
    pub fn frames_to_ms(&self, frames: f64) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            frames * 1000.0 / f64::from(self.sample_rate)
        }
    }

    /// Latency maths. `ring_ms` is the ring wait in ms (mean actual fill; the target if no mean
    /// is known yet), `capture_staleness_ms` the mean staleness of captured frames at delivery
    /// (half a packet for regular packets; use half the packet period if nothing is measured),
    /// `render` the render-side term (measured or estimated, see [`RenderSide`]).
    /// Pure; allocation-free.
    pub fn latency_breakdown(&self, ring_ms: f64, capture_staleness_ms: f64, render: RenderSide) -> LatencyBreakdown {
        let master_out_ms = self.frames_to_ms(f64::from(self.output_latency.max(0)));
        let master_in_ms = self.frames_to_ms(f64::from(self.input_latency.max(0)));
        let buffer_ms = self.frames_to_ms(f64::from(self.buffer_preferred.max(0)));
        LatencyBreakdown {
            master_out_ms,
            master_in_ms,
            buffer_ms,
            ring_ms,
            capture_ms: capture_staleness_ms,
            render_side_ms: render.ms,
            render_basis: render.basis,
            render_estimate_ms: render.estimate_ms,
            measured_total_ms: render.measured_total_ms,
            total_ms: master_out_ms + ring_ms + capture_staleness_ms + render.ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yamaha() -> Caps {
        Caps {
            master_name: "Yamaha Steinberg USB ASIO".into(),
            driver_version: 1,
            sample_rate: 48000,
            supported_rates: vec![44100, 48000, 96000],
            buffer_min: 512,
            buffer_preferred: 512,
            buffer_max: 512,
            buffer_granularity: 0,
            input_latency: 728,
            output_latency: 871,
            in_channels: 4,
            out_channels: 2,
            in_names: vec!["In 1".into(), "In 2".into()],
            out_names: vec!["UR22C Mix L".into(), "UR22C Mix R".into()],
            sample_type: 18,
            updated_unix_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("eq_core_caps_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub").join("caps.toml");
        let c = yamaha();
        c.save(&path).unwrap();
        assert!(!dir.join("sub").join("caps.toml.tmp").exists());
        assert_eq!(Caps::load(&path).unwrap(), c);
        assert!(Caps::load(dir.join("none.toml")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latency_maths() {
        let est = RenderSide::estimate();
        let b = yamaha().latency_breakdown(2.0, 10.0, est);
        assert!((b.master_out_ms - 871.0 / 48.0).abs() < 1e-9);
        assert!((b.master_in_ms - 728.0 / 48.0).abs() < 1e-9);
        assert!((b.buffer_ms - 512.0 / 48.0).abs() < 1e-9);
        assert_eq!((b.ring_ms, b.capture_ms), (2.0, 10.0));
        assert_eq!(b.render_side_ms, 20.0);
        assert_eq!(b.render_basis, RenderBasis::Estimate);
        // out + ring + capture staleness + render estimate; the buffer and the input latency are informational
        assert!((b.total_ms - (871.0 / 48.0 + 2.0 + 10.0 + 20.0)).abs() < 1e-9);
        assert!((b.master_out_ms - 18.1458).abs() < 1e-3);
        assert_eq!(Caps::default().latency_breakdown(1.0, 1.0, est).total_ms, 2.0 + 20.0);
        // a measured render side replaces the estimate and is labelled as measured
        let m = RenderSide { ms: 34.0, basis: RenderBasis::Measured { unix_s: 1_791_590_400 }, estimate_ms: 20.0, measured_total_ms: Some(64.0) };
        let b = Caps::default().latency_breakdown(1.0, 1.0, m);
        assert_eq!((b.render_side_ms, b.render_estimate_ms, b.measured_total_ms), (34.0, 20.0, Some(64.0)));
        assert!(b.render_basis.is_measured());
        assert_eq!(b.total_ms, 1.0 + 1.0 + 34.0);
    }

    #[test]
    fn latency_has_no_double_count() {
        // Cable path: 1210-frame target at 48 kHz (25.2 ms), paired bursts (10 ms staleness):
        // the old maths added a whole capture period on top of the target (burst already inside).
        let b = yamaha().latency_breakdown(1210.0 / 48.0, 10.0, RenderSide::estimate());
        let parts = b.master_out_ms + b.ring_ms + b.capture_ms + b.render_side_ms;
        assert!((b.total_ms - parts).abs() < 1e-9);
        assert!(b.buffer_ms > 0.0, "the driver buffer is informational, already inside master_out");
    }

    #[test]
    fn rates_and_path() {
        assert_eq!(COMMON_RATES.len(), 6);
        if let Some(p) = Caps::default_path() {
            assert!(p.ends_with("eq_asio_router/caps.toml"));
        }
    }
}
