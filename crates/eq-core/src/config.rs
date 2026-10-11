//! TOML configuration shared by the driver and the GUI.
//!
//! Callers detect changes by polling the file modification time ([`modified`]).

use crate::autoeq::EqProfile;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Errors from loading or saving a [`Config`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("config parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("config serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
}

/// Default WASAPI capture endpoint name.
pub const DEFAULT_CAPTURE_DEVICE: &str = "Hi-Fi Cable Output";

/// Capture endpoint of the Virtual Audio Cable (lite) "Line 1": the low-latency source preset.
pub const VAC_CAPTURE_DEVICE: &str = "Line 1 (Virtual Audio Cable)";

/// Recommended fixed capture period (ms) for VAC: 288 frames at 48 kHz, measured sample-accurate.
pub const RECOMMENDED_CAPTURE_PERIOD_MS: f32 = 6.0;
/// Smallest accepted non-zero `capture_period_ms`: VAC lite corrupts audio below 5 ms.
pub const MIN_CAPTURE_PERIOD_MS: f32 = 5.0;
/// Largest accepted `capture_period_ms`.
pub const MAX_CAPTURE_PERIOD_MS: f32 = 20.0;

/// Frames of `ms` milliseconds at `rate` Hz (rounded to nearest; shared by engine, GUI and CLI).
pub fn frames_for_ms(ms: f64, rate: u32) -> u32 {
    (ms * f64::from(rate) / 1000.0).round().max(0.0) as u32
}

/// True if `name` is a Virtual Audio Cable (Eugene Muzychenko) endpoint, e.g. "Line 1 (Virtual Audio Cable)".
pub fn is_vac_name(name: &str) -> bool {
    name.to_lowercase().contains("virtual audio cable")
}

/// True if `name` is a VB-Audio cable endpoint ("Hi-Fi Cable ...", "CABLE Output (VB-Audio ...)").
/// A VAC name is never a VB cable. Only these endpoints may have their Windows format changed.
pub fn is_vb_cable_name(name: &str) -> bool {
    if is_vac_name(name) {
        return false;
    }
    let n = name.to_lowercase();
    ["hi-fi cable", "vb-audio", "vb-cable", "cable output", "cable input"].iter().any(|p| n.contains(p))
}

/// Sample rates accepted by [`Config::sanitized`] (besides 0 = keep current).
const VALID_RATES: [u32; 6] = [44100, 48000, 88200, 96000, 176400, 192000];

/// WASAPI capture mode. Serialised as lowercase strings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureMode {
    /// Plain shared-mode capture (default: the Hi-Fi Cable driver offers no
    /// smaller period, so low-latency mode brings no benefit).
    #[default]
    Shared,
    /// Shared mode with the smallest engine period (IAudioClient3); falls back to
    /// `Shared` at runtime if unsupported.
    LowLatency,
    /// Exclusive-mode capture attempt.
    Exclusive,
}

/// Resampler interpolation quality. Serialised as lowercase strings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResamplerQuality {
    /// Linear interpolation (cheapest).
    Linear,
    /// Cubic (Hermite) interpolation.
    #[default]
    Cubic,
}

/// Which low-latency source the settings describe. Serialised as lowercase strings.
///
/// A preset is only a convenience: [`Config::apply_source_preset`] copies its values into the
/// individual fields, which stay editable (the GUI then shows `Custom`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourcePreset {
    /// VB-Audio Hi-Fi Cable (default; plain shared capture).
    #[default]
    Cable,
    /// Virtual Audio Cable "Line 1": fixed 6 ms capture period + render keep-alive.
    Vac,
    /// Anything else / hand-edited values.
    Custom,
}

impl SourcePreset {
    pub fn label(self) -> &'static str {
        match self {
            SourcePreset::Cable => "cable",
            SourcePreset::Vac => "vac",
            SourcePreset::Custom => "custom",
        }
    }
    /// Parses "cable" / "vac" / "custom" (case-insensitive).
    pub fn parse(s: &str) -> Option<SourcePreset> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cable" | "hifi" | "hi-fi" => Some(SourcePreset::Cable),
            "vac" | "line1" => Some(SourcePreset::Vac),
            "custom" => Some(SourcePreset::Custom),
            _ => None,
        }
    }
}

fn default_safety_ms() -> f32 {
    2.0
}
fn default_true() -> bool {
    true
}
fn default_capture_device() -> String {
    DEFAULT_CAPTURE_DEVICE.to_string()
}

/// Router configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Name of the master ASIO driver to wrap.
    pub master_driver: String,
    /// Master output channel indexes receiving the EQ'd stereo pair.
    pub output_channels: [u32; 2],
    /// Pass the master's remaining inputs through untouched.
    pub passthrough_inputs: bool,
    /// Bypass the EQ entirely.
    pub bypass: bool,
    /// Enable the soft limiter after the EQ.
    pub limiter: bool,
    /// Where the profile came from (e.g. a file path), informational.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_source: Option<String>,
    /// Active EQ profile.
    pub profile: EqProfile,
    /// Engine sample rate in Hz; 0 = keep the master's current rate.
    #[serde(default)]
    pub sample_rate: u32,
    /// Extra ring prefill in ms (0..20): latency vs robustness trade.
    #[serde(default = "default_safety_ms")]
    pub safety_buffer_ms: f32,
    /// Let the engine raise/lower the safety buffer automatically.
    #[serde(default = "default_true")]
    pub safety_buffer_auto: bool,
    /// WASAPI capture endpoint name (empty = [`DEFAULT_CAPTURE_DEVICE`]).
    #[serde(default = "default_capture_device")]
    pub capture_device: String,
    /// Capture mode.
    #[serde(default)]
    pub capture_mode: CaptureMode,
    /// Resampler quality.
    #[serde(default)]
    pub resampler_quality: ResamplerQuality,
    /// Lower the effective preamp so the profile's max boost never exceeds 0 dBFS.
    #[serde(default = "default_true")]
    pub auto_headroom: bool,
    /// Fade-mute on detected faults.
    #[serde(default = "default_true")]
    pub mute_on_fault: bool,
    /// Ask Windows to set the Hi-Fi Cable endpoints to the ASIO master's sample rate.
    #[serde(default = "default_true")]
    pub match_cable_rate: bool,
    /// Arrival-jitter headroom (ms, 0..40) found by the GUI's "Learn (30 s)" measurement: the lowest
    /// ring headroom that survived the measured capture arrival pattern, with margin. 0 = not
    /// learned (the engine starts conservatively and measures live). When set, the engine starts
    /// at this headroom and never goes below it. Additive; old config files load unchanged.
    #[serde(default)]
    pub jitter_margin_ms: f32,
    /// Learned arrival-jitter headroom per capture device (ms, 0..40), keyed by `capture_device`
    /// (case-insensitive lookup). Replaces `jitter_margin_ms`, which [`Config::sanitized`] migrates
    /// into this map under the current capture device while the map is empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub jitter_margin_by_device: BTreeMap<String, f32>,
    /// Fixed WASAPI capture period in ms for a known virtual cable (IAudioClient3). 0 = the driver's
    /// default shared period. Allowed: 0 or 5.0..=20.0 (VAC lite corrupts audio below 5 ms);
    /// recommended 6.0 (288 frames at 48 kHz). Ignored for endpoints that are not virtual cables.
    #[serde(default)]
    pub capture_period_ms: f32,
    /// Run a silent minimum-period render stream on the cable's render endpoint so apps rendering into
    /// it see a 1 ms engine period (only on known virtual cables; never on real output devices).
    #[serde(default)]
    pub render_keepalive: bool,
    /// Which source preset these settings came from (informational; see [`SourcePreset`]).
    #[serde(default)]
    pub source_preset: SourcePreset,
    /// "JBL Bass Mode": car-subwoofer bass boost, distortion and pumping on top of the EQ.
    #[serde(default)]
    pub jbl_bass_mode: bool,
    /// JBL Bass Mode intensity, 0..=100.
    #[serde(default = "default_jbl_intensity")]
    pub jbl_intensity: u8,
}

fn default_jbl_intensity() -> u8 {
    80
}

impl Default for Config {
    fn default() -> Self {
        Config {
            master_driver: String::new(),
            output_channels: [0, 1],
            passthrough_inputs: true,
            bypass: false,
            limiter: true,
            profile_source: None,
            profile: EqProfile::default(),
            sample_rate: 0,
            safety_buffer_ms: default_safety_ms(),
            safety_buffer_auto: true,
            capture_device: default_capture_device(),
            capture_mode: CaptureMode::default(),
            resampler_quality: ResamplerQuality::default(),
            auto_headroom: true,
            mute_on_fault: true,
            match_cable_rate: true,
            jitter_margin_ms: 0.0,
            jitter_margin_by_device: BTreeMap::new(),
            capture_period_ms: 0.0,
            render_keepalive: false,
            source_preset: SourcePreset::Cable,
            jbl_bass_mode: false,
            jbl_intensity: default_jbl_intensity(),
        }
    }
}

fn finite_or(v: f64, d: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        d
    }
}

impl Config {
    /// Load from a TOML file. A file without `source_preset` (written by an older build) gets the
    /// preset inferred from its capture device: `Cable` if it contains "Hi-Fi", else `Custom`.
    pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_toml_str(&text)
    }

    /// Parses TOML text with the same migration as [`Config::load`].
    pub fn from_toml_str(text: &str) -> Result<Config, ConfigError> {
        let mut c: Config = toml::from_str(text)?;
        let has_preset = toml::from_str::<toml::Table>(text).map(|t| t.contains_key("source_preset")).unwrap_or(false);
        if !has_preset {
            c.source_preset = infer_preset(&c.capture_device);
        }
        Ok(c)
    }

    /// Copies the values of a source preset into the individual fields.
    ///
    /// * `Vac`: capture "Line 1 (Virtual Audio Cable)", shared mode, 6 ms fixed period, render
    ///   keep-alive on, Windows cable-rate matching off (VAC is fixed at 48 kHz and never touched).
    /// * `Cable`: the Hi-Fi Cable, driver-default period, no keep-alive, rate matching on.
    /// * `Custom`: only marks the config as hand-edited.
    ///
    /// Never called implicitly: loading an old file does not change its values.
    pub fn apply_source_preset(&mut self, preset: SourcePreset) {
        match preset {
            SourcePreset::Vac => {
                self.capture_device = VAC_CAPTURE_DEVICE.to_string();
                self.capture_mode = CaptureMode::Shared;
                self.capture_period_ms = RECOMMENDED_CAPTURE_PERIOD_MS;
                self.render_keepalive = true;
                self.match_cable_rate = false;
            }
            SourcePreset::Cable => {
                self.capture_device = DEFAULT_CAPTURE_DEVICE.to_string();
                self.capture_mode = CaptureMode::Shared;
                self.capture_period_ms = 0.0;
                self.render_keepalive = false;
                self.match_cable_rate = true;
            }
            SourcePreset::Custom => {}
        }
        self.source_preset = preset;
    }

    /// Learned jitter margin (ms) for `device`: its entry in `jitter_margin_by_device`
    /// (case-insensitive), else the legacy `jitter_margin_ms` only while the map is empty, else 0.
    pub fn jitter_margin_for(&self, device: &str) -> f32 {
        let d = device.trim().to_lowercase();
        if let Some((_, v)) = self.jitter_margin_by_device.iter().find(|(k, _)| k.trim().to_lowercase() == d) {
            return *v;
        }
        if self.jitter_margin_by_device.is_empty() {
            self.jitter_margin_ms
        } else {
            0.0
        }
    }

    /// Stores a learned margin for the current capture device (0 forgets it). Also mirrors it into
    /// the legacy `jitter_margin_ms` so older readers keep working.
    pub fn set_jitter_margin(&mut self, ms: f32) {
        let dev = self.capture_device.trim().to_string();
        let existing: Vec<String> =
            self.jitter_margin_by_device.keys().filter(|k| k.trim().eq_ignore_ascii_case(&dev)).cloned().collect();
        for k in existing {
            self.jitter_margin_by_device.remove(&k);
        }
        if ms > 0.0 && !dev.is_empty() {
            self.jitter_margin_by_device.insert(dev, ms);
        }
        self.jitter_margin_ms = ms.max(0.0);
    }

    /// Load, falling back to defaults if the file does not exist.
    /// The result is passed through [`Config::sanitized`].
    pub fn load_or_default(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        match Self::load(path) {
            Err(ConfigError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Config::default())
            }
            other => other.map(|c| c.sanitized()),
        }
    }

    /// Return a copy with every value clamped to a safe range.
    ///
    /// Gain +-24 dB, Q 0.1..20 (shelves 0.1..4), fc 10 Hz..24 kHz, preamp -40..+12 dB,
    /// `safety_buffer_ms` 0..20, at most 32 filters; NaN/inf are replaced by defaults;
    /// `sample_rate` becomes 0 or the nearest of 44100/48000/88200/96000/176400/192000;
    /// an empty `capture_device` becomes the default.
    pub fn sanitized(&self) -> Config {
        use crate::dsp::{FilterKind, MAX_BANDS};
        let mut c = self.clone();
        c.profile.preamp_db = finite_or(c.profile.preamp_db, 0.0).clamp(-40.0, 12.0);
        c.profile.filters.truncate(MAX_BANDS);
        for f in &mut c.profile.filters {
            f.fc_hz = finite_or(f.fc_hz, 1000.0).clamp(10.0, 24000.0);
            f.gain_db = finite_or(f.gain_db, 0.0).clamp(-24.0, 24.0);
            let qmax = match f.kind {
                FilterKind::LowShelf | FilterKind::HighShelf => 4.0,
                _ => 20.0,
            };
            f.q = finite_or(f.q, std::f64::consts::FRAC_1_SQRT_2).clamp(0.1, qmax);
        }
        c.safety_buffer_ms = if c.safety_buffer_ms.is_finite() {
            c.safety_buffer_ms.clamp(0.0, 20.0)
        } else {
            default_safety_ms()
        };
        c.jbl_intensity = c.jbl_intensity.min(100);
        c.jitter_margin_ms = if c.jitter_margin_ms.is_finite() { c.jitter_margin_ms.clamp(0.0, 40.0) } else { 0.0 };
        c.capture_period_ms = if !c.capture_period_ms.is_finite() || c.capture_period_ms <= 0.0 {
            0.0
        } else {
            c.capture_period_ms.clamp(MIN_CAPTURE_PERIOD_MS, MAX_CAPTURE_PERIOD_MS)
        };
        // Per-device margins: drop junk, clamp, bound the map; migrate the legacy single value.
        let mut by: BTreeMap<String, f32> = BTreeMap::new();
        for (k, v) in &c.jitter_margin_by_device {
            let key = k.trim();
            if !key.is_empty() && v.is_finite() && by.len() < 16 {
                by.insert(key.to_string(), v.clamp(0.0, 40.0));
            }
        }
        if by.is_empty() && c.jitter_margin_ms > 0.0 && !c.capture_device.trim().is_empty() {
            by.insert(c.capture_device.trim().to_string(), c.jitter_margin_ms);
        }
        c.jitter_margin_by_device = by;
        if c.sample_rate != 0 {
            c.sample_rate = VALID_RATES
                .iter()
                .copied()
                .min_by_key(|r| r.abs_diff(c.sample_rate))
                .unwrap_or(48000);
        }
        if c.capture_device.trim().is_empty() {
            c.capture_device = default_capture_device();
        }
        c
    }

    /// Save atomically (write a temp file next to the target, then rename).
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
}

fn infer_preset(capture_device: &str) -> SourcePreset {
    if capture_device.to_lowercase().contains("hi-fi") {
        SourcePreset::Cable
    } else {
        SourcePreset::Custom
    }
}

/// Default config location: `%APPDATA%\eq_asio_router\config.toml`.
pub fn default_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("eq_asio_router").join("config.toml"))
}

/// File modification time, or `None` if unavailable (for change polling).
pub fn modified(path: impl AsRef<Path>) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoeq::parse_parametric_eq;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("eq_core_test_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn defaults() {
        let c = Config::default();
        assert_eq!(c.output_channels, [0, 1]);
        assert!(!c.bypass);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn round_trip() {
        let dir = tmp_dir("rt");
        let path = dir.join("sub").join("config.toml");
        let mut c = Config::default();
        c.master_driver = "Focusrite USB ASIO".into();
        c.output_channels = [2, 3];
        c.bypass = true;
        c.profile_source = Some("C:\\eq\\hd650.txt".into());
        c.profile = parse_parametric_eq(
            "Preamp: -6.4 dB\nFilter 1: ON PK Fc 31 Hz Gain 1.5 dB Q 1.41\n\
             Filter 2: OFF LSC Fc 105 Hz Gain 5.5 dB Q 0.7\nFilter 3: ON HSC Fc 10000 Hz Gain -3.2 dB Q 0.7\n",
            "HD 650",
        )
        .unwrap();
        c.save(&path).unwrap();
        assert!(!dir.join("sub").join("config.toml.tmp").exists());
        assert!(modified(&path).is_some());
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, c);
        // Overwrite works.
        c.bypass = false;
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_and_missing_files() {
        let dir = tmp_dir("partial");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.toml");
        std::fs::write(&path, "master_driver = \"X\"\nbypass = true\n").unwrap();
        let c = Config::load(&path).unwrap();
        assert_eq!(c.master_driver, "X");
        assert!(c.bypass);
        assert_eq!(c.output_channels, [0, 1]);
        std::fs::write(&path, "bypass = [").unwrap();
        assert!(matches!(Config::load(&path), Err(ConfigError::Parse(_))));
        let missing = dir.join("nope.toml");
        assert!(matches!(Config::load(&missing), Err(ConfigError::Io(_))));
        assert_eq!(Config::load_or_default(&missing).unwrap(), Config::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_fields_defaults_and_strings() {
        let c = Config::default();
        assert_eq!(c.sample_rate, 0);
        assert_eq!(c.safety_buffer_ms, 2.0);
        assert!(c.safety_buffer_auto && c.auto_headroom && c.mute_on_fault);
        assert_eq!(c.capture_device, "Hi-Fi Cable Output");
        assert_eq!(c.capture_mode, CaptureMode::Shared);
        assert_eq!(c.resampler_quality, ResamplerQuality::Cubic);
        let t = toml::to_string_pretty(&c).unwrap();
        assert!(t.contains("capture_mode = \"shared\""));
        assert!(t.contains("resampler_quality = \"cubic\""));
        let p: Config =
            toml::from_str("capture_mode = \"exclusive\"\nresampler_quality = \"linear\"").unwrap();
        assert_eq!(p.capture_mode, CaptureMode::Exclusive);
        assert_eq!(p.resampler_quality, ResamplerQuality::Linear);
        assert_eq!(p.capture_device, "Hi-Fi Cable Output");
        assert!(p.mute_on_fault);
        assert!(p.match_cable_rate && c.match_cable_rate);
        let off: Config = toml::from_str("match_cable_rate = false").unwrap();
        assert!(!off.match_cable_rate);
        assert!(!off.sanitized().match_cable_rate);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn sanitize_clamps() {
        use crate::dsp::{FilterKind, FilterSpec};
        let mk = |kind, fc, gain, q| FilterSpec { kind, fc_hz: fc, gain_db: gain, q, enabled: true };
        let mut c = Config::default();
        c.profile.preamp_db = -100.0;
        c.sample_rate = 50000;
        c.safety_buffer_ms = 99.0;
        c.capture_device = String::new();
        c.profile.filters = vec![
            mk(FilterKind::Peaking, 1.0, 50.0, 100.0),
            mk(FilterKind::HighShelf, 99999.0, -50.0, 0.0),
            mk(FilterKind::Peaking, f64::NAN, f64::INFINITY, f64::NAN),
        ];
        c.profile.filters.extend(std::iter::repeat_n(mk(FilterKind::Peaking, 1000.0, 1.0, 1.0), 50));
        let s = c.sanitized();
        assert_eq!(s.profile.preamp_db, -40.0);
        assert_eq!(s.sample_rate, 48000);
        assert_eq!(s.safety_buffer_ms, 20.0);
        assert_eq!(s.capture_device, "Hi-Fi Cable Output");
        assert_eq!(s.profile.filters.len(), 32);
        let f = &s.profile.filters;
        assert_eq!((f[0].fc_hz, f[0].gain_db, f[0].q), (10.0, 24.0, 20.0));
        assert_eq!((f[1].fc_hz, f[1].gain_db, f[1].q), (24000.0, -24.0, 0.1));
        assert!(f[2].fc_hz.is_finite() && f[2].gain_db == 0.0 && f[2].q > 0.1);
        let mut n = Config::default();
        n.safety_buffer_ms = f32::NAN;
        n.profile.preamp_db = f64::NAN;
        n.sample_rate = 1_000_000;
        let n = n.sanitized();
        assert_eq!((n.safety_buffer_ms, n.profile.preamp_db, n.sample_rate), (2.0, 0.0, 192000));
        c.sample_rate = 0;
        assert_eq!(c.sanitized().sample_rate, 0);
    }

    #[test]
    fn jitter_margin_is_additive_and_sanitized() {
        // old files without the field load with 0 (= not learned)
        let c: Config = toml::from_str("safety_buffer_ms = 3.0
").unwrap();
        assert_eq!(c.jitter_margin_ms, 0.0);
        let mut c = Config { jitter_margin_ms: 99.0, ..Config::default() };
        assert_eq!(c.sanitized().jitter_margin_ms, 40.0);
        c.jitter_margin_ms = f32::NAN;
        assert_eq!(c.sanitized().jitter_margin_ms, 0.0);
        c.jitter_margin_ms = 12.5;
        let back: Config = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.jitter_margin_ms, 12.5);
    }

    #[test]
    fn vac_preset_and_cable_preset() {
        let mut c = Config::default();
        c.apply_source_preset(SourcePreset::Vac);
        assert_eq!(c.capture_device, "Line 1 (Virtual Audio Cable)");
        assert_eq!(c.capture_mode, CaptureMode::Shared);
        assert_eq!((c.capture_period_ms, c.render_keepalive, c.match_cable_rate), (6.0, true, false));
        assert_eq!(c.source_preset, SourcePreset::Vac);
        let t = toml::to_string_pretty(&c).unwrap();
        assert!(t.contains("source_preset = \"vac\""), "{t}");
        c.apply_source_preset(SourcePreset::Cable);
        assert_eq!(c.capture_device, "Hi-Fi Cable Output");
        assert_eq!((c.capture_period_ms, c.render_keepalive, c.match_cable_rate), (0.0, false, true));
        assert_eq!(c, Config::default());
        c.apply_source_preset(SourcePreset::Custom);
        assert_eq!(c.source_preset, SourcePreset::Custom);
        assert_eq!(c.capture_device, "Hi-Fi Cable Output");
        assert_eq!(SourcePreset::parse("VAC"), Some(SourcePreset::Vac));
        assert_eq!(SourcePreset::parse("x"), None);
    }

    #[test]
    fn old_files_load_unchanged_and_infer_preset() {
        let old = Config::from_toml_str("master_driver = \"X\"\n").unwrap();
        assert_eq!(old.source_preset, SourcePreset::Cable);
        assert_eq!((old.capture_period_ms, old.render_keepalive), (0.0, false));
        assert!(old.match_cable_rate && old.jitter_margin_by_device.is_empty());
        let hifi = Config::from_toml_str("capture_device = \"Hi-Fi Cable Output (VB-Audio Hi-Fi Cable)\"").unwrap();
        assert_eq!(hifi.source_preset, SourcePreset::Cable);
        let other = Config::from_toml_str("capture_device = \"CABLE Output\"\nmatch_cable_rate = true").unwrap();
        assert_eq!(other.source_preset, SourcePreset::Custom);
        assert!(other.match_cable_rate, "values are not rewritten by the inference");
        let vac = Config::from_toml_str("capture_device = \"Line 1 (Virtual Audio Cable)\"").unwrap();
        assert_eq!(vac.source_preset, SourcePreset::Custom);
        assert_eq!((vac.capture_period_ms, vac.render_keepalive), (0.0, false));
        // an explicit preset is kept
        let ex = Config::from_toml_str("source_preset = \"vac\"\ncapture_device = \"CABLE Output\"").unwrap();
        assert_eq!(ex.source_preset, SourcePreset::Vac);
    }

    #[test]
    fn capture_period_is_sanitized() {
        let s = |v: f32| Config { capture_period_ms: v, ..Config::default() }.sanitized().capture_period_ms;
        assert_eq!(s(0.0), 0.0);
        assert_eq!(s(-3.0), 0.0);
        assert_eq!(s(f32::NAN), 0.0);
        assert_eq!(s(f32::INFINITY), 0.0);
        assert_eq!(s(1.0), 5.0);
        assert_eq!(s(4.9), 5.0);
        assert_eq!(s(6.0), 6.0);
        assert_eq!(s(12.5), 12.5);
        assert_eq!(s(99.0), 20.0);
        let p: Config = toml::from_str("capture_period_ms = 6.0\nrender_keepalive = true").unwrap();
        assert!(p.render_keepalive && p.capture_period_ms == 6.0);
    }

    #[test]
    fn per_device_jitter_margin_and_migration() {
        // legacy value migrates under the current capture device
        let c = Config { jitter_margin_ms: 7.5, ..Config::default() }.sanitized();
        assert_eq!(c.jitter_margin_by_device.get("Hi-Fi Cable Output"), Some(&7.5));
        assert_eq!(c.jitter_margin_for("hi-fi cable output"), 7.5);
        assert_eq!(c.jitter_margin_for("Line 1 (Virtual Audio Cable)"), 0.0, "no entry for another device");
        // legacy fallback only while no map exists
        let raw = Config { jitter_margin_ms: 3.0, ..Config::default() };
        assert_eq!(raw.jitter_margin_for("anything"), 3.0);
        // set per device; unrelated devices keep their value
        let mut c = c;
        c.apply_source_preset(SourcePreset::Vac);
        c.set_jitter_margin(4.2);
        assert_eq!(c.jitter_margin_for("Line 1 (Virtual Audio Cable)"), 4.2);
        assert_eq!(c.jitter_margin_for("Hi-Fi Cable Output"), 7.5);
        let back = Config::from_toml_str(&toml::to_string_pretty(&c).unwrap()).unwrap().sanitized();
        assert_eq!(back.jitter_margin_by_device, c.jitter_margin_by_device);
        c.set_jitter_margin(0.0);
        assert_eq!(c.jitter_margin_for("Line 1 (Virtual Audio Cable)"), 0.0);
        // sanitising clamps / drops junk
        let mut m = Config::default();
        m.jitter_margin_by_device.insert("a".into(), 99.0);
        m.jitter_margin_by_device.insert("b".into(), f32::NAN);
        m.jitter_margin_by_device.insert("  ".into(), 1.0);
        let m = m.sanitized();
        assert_eq!(m.jitter_margin_by_device.len(), 1);
        assert_eq!(m.jitter_margin_by_device["a"], 40.0);
    }

    #[test]
    fn name_helpers_and_frames() {
        assert!(is_vac_name("Line 1 (Virtual Audio Cable)"));
        assert!(!is_vb_cable_name("Line 1 (Virtual Audio Cable)"));
        for n in ["Hi-Fi Cable Output (VB-Audio Hi-Fi Cable)", "CABLE Output (VB-Audio Virtual Cable)", "Hi-Fi Cable Input"] {
            assert!(is_vb_cable_name(n), "{n}");
        }
        for n in ["Microphone (Yamaha)", "Speakers (Realtek)", "Line 1"] {
            assert!(!is_vb_cable_name(n) && !is_vac_name(n), "{n}");
        }
        assert_eq!(frames_for_ms(6.0, 48000), 288);
        assert_eq!(frames_for_ms(10.0, 44100), 441);
        assert_eq!(frames_for_ms(-1.0, 48000), 0);
    }

    #[test]
    fn load_or_default_sanitizes() {
        let dir = tmp_dir("san");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.toml");
        std::fs::write(&path, "safety_buffer_ms = 500.0\nsample_rate = 44000\n").unwrap();
        let c = Config::load_or_default(&path).unwrap();
        assert_eq!((c.safety_buffer_ms, c.sample_rate), (20.0, 44100));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_path_shape() {
        if let Some(p) = default_path() {
            assert!(p.ends_with("eq_asio_router/config.toml"));
        }
    }
}
