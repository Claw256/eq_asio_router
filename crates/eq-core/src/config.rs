//! TOML configuration shared by the driver and the GUI.
//!
//! Callers detect changes by polling the file modification time ([`modified`]).

use crate::autoeq::EqProfile;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Mutex;
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

/// Fixed source key of the bridge era (`"vbmatrix"`): the key of the learned jitter margin in
/// `jitter_margin_by_device` and of the `source` field of `latency_measured.toml` entries. The
/// bridge is the only source since the M3a cutover; older files keyed the margin map by capture
/// device and are migrated by [`Config::sanitized`].
pub const BRIDGE_SOURCE_KEY: &str = "vbmatrix";

/// Config keys removed by the M3a bridge cutover. Old files still load (serde ignores unknown
/// keys); the keys found in a loaded file are reported once per process through
/// [`take_deprecation_notice`].
pub const REMOVED_CONFIG_KEYS: &[&str] = &[
    "capture_device",
    "capture_mode",
    "capture_period_ms",
    "render_keepalive",
    "keepalive",
    "match_cable_rate",
    "cable_rate_match",
    "source_preset",
    "source",
];

/// Keys seen by the last `Config::load` / `Config::from_toml_str` calls (union; first load wins
/// nothing — all loads before the first [`take_deprecation_notice`] contribute).
static DEPRECATION_KEYS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
/// Set as soon as a notice was taken: only one notice is ever reported per process.
static DEPRECATION_TAKEN: AtomicBool = AtomicBool::new(false);

/// Scans raw TOML for [`REMOVED_CONFIG_KEYS`] and stashes the found keys for
/// [`take_deprecation_notice`]. Called by [`Config::load`] / [`Config::from_toml_str`].
///
/// Only *top-level* keys are considered: every removed key lived at the document
/// root, and table-local keys (e.g. `[portable] source`, `[bridge] loopback_enabled`)
/// would otherwise be false positives (Phase C).
fn note_removed_keys(text: &str) {
    if DEPRECATION_TAKEN.load(Relaxed) {
        return;
    }
    let mut found: Vec<&'static str> = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if l.starts_with('[') {
            // Inside a table: its keys are not document-root keys.
            continue;
        }
        let Some((k, _)) = l.split_once('=') else { continue };
        let k = k.trim().trim_matches('"');
        if let Some(removed) = REMOVED_CONFIG_KEYS.iter().find(|r| r.eq_ignore_ascii_case(k)) {
            if !found.contains(removed) {
                found.push(removed);
            }
        }
    }
    if found.is_empty() {
        return;
    }
    let mut g = DEPRECATION_KEYS.lock().unwrap_or_else(|e| e.into_inner());
    for k in found {
        if !g.contains(&k) {
            g.push(k);
        }
    }
}

/// Takes the pending deprecation notice of removed config keys, at most once per process
/// (callers log/println it; later loads of the same file stay silent).
pub fn take_deprecation_notice() -> Option<String> {
    if DEPRECATION_TAKEN.swap(true, Relaxed) {
        return None;
    }
    let keys = std::mem::take(&mut *DEPRECATION_KEYS.lock().unwrap_or_else(|e| e.into_inner()));
    if keys.is_empty() {
        return None;
    }
    Some(format!(
        "config: removed setting(s) ignored: {} — since M3a the audio source is the VBMatrix bridge; delete these keys from the file",
        keys.join(", ")
    ))
}

/// Frames of `ms` milliseconds at `rate` Hz (rounded to nearest; shared by engine, GUI and CLI).
pub fn frames_for_ms(ms: f64, rate: u32) -> u32 {
    (ms * f64::from(rate) / 1000.0).round().max(0.0) as u32
}

/// Sample rates accepted by [`Config::sanitized`] (besides 0 = keep current).
const VALID_RATES: [u32; 6] = [44100, 48000, 88200, 96000, 176400, 192000];

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

fn default_safety_ms() -> f32 {
    2.0
}
fn default_true() -> bool {
    true
}

/// `[bridge]`: settings of the shared-memory bridge that carries audio between VBMatrix
/// (which loads the virtual ASIO driver) and the router. Added in M2; old config files
/// load unchanged (missing table = defaults).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgeConfig {
    /// Minimum fill target of the source ring (ring A) in milliseconds. The engine always
    /// derives a target from the negotiated driver buffer plus the safety buffer; this value
    /// only acts as a *floor* on top of that (0 = no floor, derive purely from the buffers).
    /// Clamped to 0..=200 ms.
    pub ring_target_ms: f32,
    /// Feed the post-EQ stereo audio back to VBMatrix through ring B (the driver's ASIO
    /// input side). Off = the driver's inputs stay silent. Restart-class change.
    pub loopback_enabled: bool,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        BridgeConfig { ring_target_ms: 0.0, loopback_enabled: true }
    }
}

impl BridgeConfig {
    /// Clamps `ring_target_ms` into 0..=200 ms (NaN -> 0); `loopback_enabled` is a flag.
    pub fn sanitized(&self) -> Self {
        BridgeConfig {
            ring_target_ms: if self.ring_target_ms.is_finite() { self.ring_target_ms.clamp(0.0, 200.0) } else { 0.0 },
            loopback_enabled: self.loopback_enabled,
        }
    }
}

/// `[portable]`: headless portable-engine settings (docs/cross-platform.md). A
/// deliberate **new** section — the Windows-only `[bridge]` semantics
/// ([`BridgeConfig`], VBMatrix shm ring) are NOT reused here (A4/Phase C scope):
/// every OS selects its source/device by name. Missing table = defaults.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PortableConfig {
    /// Source selector for this OS: `"auto"` (per-OS default), `"none"` (output only),
    /// or a backend-specific source kind (`"pipewire"` monitor capture on Linux,
    /// `"blackhole"` loopback tap on macOS). Unknown/unsupported selectors are
    /// reported and treated as `"none"`.
    pub source: String,
    /// Output device: ASIO driver name (Windows; empty = `master_driver` /
    /// "Yamaha Steinberg USB ASIO"), ALSA device (Linux; empty = "default"),
    /// Core Audio device name (macOS; empty = system default output).
    pub output_device: String,
    /// Input/monitor/tap device name: PipeWire monitor source (Linux), BlackHole
    /// loopback device (macOS). Empty = per-OS default.
    pub input_device: String,
    /// AutoEQ `ParametricEQ.txt` profile path; when set and readable it replaces
    /// `profile` at startup (the path lands in `profile_source`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_path: Option<PathBuf>,
}

impl Default for PortableConfig {
    fn default() -> Self {
        PortableConfig {
            source: "auto".to_string(),
            output_device: String::new(),
            input_device: String::new(),
            profile_path: None,
        }
    }
}

impl PortableConfig {
    /// Trimmed copy: selector/device strings trimmed, empty `source` becomes
    /// `"auto"`, empty `profile_path` collapses to `None`.
    pub fn sanitized(&self) -> Self {
        PortableConfig {
            source: {
                let s = self.source.trim();
                if s.is_empty() { "auto".to_string() } else { s.to_string() }
            },
            output_device: self.output_device.trim().to_string(),
            input_device: self.input_device.trim().to_string(),
            profile_path: self.profile_path.clone().filter(|p| !p.as_os_str().is_empty()),
        }
    }
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
    /// Apply the AutoEQ stage (preamp and bands). Off crossfades just that stage to dry; unlike
    /// `bypass`, JBL Bass Mode and the limiter keep working.
    #[serde(default = "default_true")]
    pub eq_enabled: bool,
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
    /// Resampler quality.
    #[serde(default)]
    pub resampler_quality: ResamplerQuality,
    /// Lower the effective preamp so the profile's max boost never exceeds 0 dBFS.
    #[serde(default = "default_true")]
    pub auto_headroom: bool,
    /// Fade-mute on detected faults.
    #[serde(default = "default_true")]
    pub mute_on_fault: bool,
    /// Arrival-jitter headroom (ms, 0..40) found by the GUI's "Learn (30 s)" measurement: the lowest
    /// ring headroom that survived the measured source fill-dip pattern, with margin. 0 = not
    /// learned (the engine starts conservatively and measures live). When set, the engine starts
    /// at this headroom and never goes below it. Old files store it under per-device keys and are
    /// migrated to [`BRIDGE_SOURCE_KEY`] by [`Config::sanitized`].
    #[serde(default)]
    pub jitter_margin_ms: f32,
    /// Learned jitter headroom per source (ms, 0..40). The bridge is the only source since M3a, so
    /// the only meaningful key is [`BRIDGE_SOURCE_KEY`] (`"vbmatrix"`); legacy capture-device
    /// keys from old files are migrated there by [`Config::sanitized`]. `jitter_margin_ms` mirrors
    /// the value for older readers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub jitter_margin_by_device: BTreeMap<String, f32>,
    /// "JBL Bass Mode": car-subwoofer bass boost, distortion and pumping on top of the EQ.
    #[serde(default)]
    pub jbl_bass_mode: bool,
    /// JBL Bass Mode intensity, 0..=100.
    #[serde(default = "default_jbl_intensity")]
    pub jbl_intensity: u8,
    /// Advanced JBL Bass Mode parameters (at-100 % endpoints; Intensity stays the master scaler).
    #[serde(default)]
    pub jbl: JblParams,
    /// VBMatrix bridge settings (see [`BridgeConfig`]).
    #[serde(default)]
    pub bridge: BridgeConfig,
    /// Headless portable-engine settings (see [`PortableConfig`]; not the bridge).
    #[serde(default)]
    pub portable: PortableConfig,
}

/// What the rattle LFO modulates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum JblLfoTarget {
    /// Final output gain.
    #[default]
    Gain,
    /// Level of the kick band feeding the saturator.
    BassBand,
    /// Both of the above.
    Both,
}

/// Advanced JBL Bass Mode settings. Values are the at-100 % endpoints; the existing
/// `jbl_intensity / 100` scaling interpolates from idle. `Default` is the measured, tuned set
/// (see `tests/jbl_audit.rs`); a saved `[jbl]` table keeps whatever values it holds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JblParams {
    // Pump / duck
    pub duck_on: bool,
    /// Duck depth (0..=6, default 4.0).
    pub duck_depth: f32,
    /// Ducking floor dB (-24..=0, default -9): the most the ducker may turn the level down on a bass
    /// hit (the curve shape, attack and release are unchanged; the dip just stops at this level).
    /// -24 = no floor (the plain curve, the original behaviour); 0 = no ducking. A saved `[jbl]`
    /// table without the key loads as -24, so an older sound is unchanged.
    #[serde(default = "no_duck_floor")]
    pub duck_floor_db: f32,
    /// Duck attack ms (0.5..=50, default 5).
    pub duck_attack_ms: f32,
    /// Duck release ms (10..=800, default 120).
    pub duck_release_ms: f32,
    /// Bass detector frequency Hz (40..=250, default 100).
    pub detect_hz: f32,
    // Low end
    /// Low-shelf frequency Hz (30..=200, default 70).
    pub shelf_hz: f32,
    /// Low-shelf gain dB (0..=18, default 10).
    pub shelf_db: f32,
    pub res_on: bool,
    /// Resonance frequency Hz (25..=120, default 50).
    pub res_hz: f32,
    /// Resonance gain dB (0..=18, default 6.5).
    pub res_db: f32,
    /// Resonance Q (0.5..=10, default 2).
    pub res_q: f32,
    // Distortion
    /// Max drive (1..=12, default 8).
    pub drive_max: f32,
    /// Max asymmetric bias (0..=0.8, default 0.35).
    pub bias_max: f32,
    pub kick_on: bool,
    /// Kick band frequency Hz (60..=300, default 130).
    pub kick_hz: f32,
    /// Max kick drive (1..=6, default 3).
    pub kick_drive_max: f32,
    // Lo-fi
    pub crush_on: bool,
    /// Bit depth at 100 % (6..=16, default 10).
    pub crush_bits_min: f32,
    pub hold_on: bool,
    /// Max sample-hold factor (1..=8, default 3).
    pub hold_max: f32,
    pub lpf_on: bool,
    /// LPF cutoff at 0 % Hz (2000..=20000, default 16000).
    pub lpf_hz_start: f32,
    /// LPF cutoff at 100 % Hz (2000..=20000, default 10000).
    pub lpf_hz_end: f32,
    // Rattle LFO
    pub lfo_on: bool,
    /// LFO rate Hz (0.5..=20, default 6).
    pub lfo_rate_hz: f32,
    /// LFO depth (0..=1, default 0.3).
    pub lfo_depth: f32,
    /// Scale LFO depth by the bass envelope so it only wobbles on hits.
    pub lfo_follow_bass: bool,
    pub lfo_target: JblLfoTarget,
    // Output
    /// Extra output trim dB (-12..=12, default 0; the Default sound uses 1.1).
    pub out_trim_db: f32,
    /// Clean low-shelf boost dB on the effect's wet output, after the distortion (0..=12, default 0 = off).
    pub bass_boost_db: f32,
    /// Corner frequency Hz of the clean bass boost (30..=200, default 80).
    pub bass_boost_hz: f32,
    /// Level guard (default off): the effect's output is never allowed to be louder than its input
    /// (plus `bass_boost_db`); it only ever turns the wet signal down. Off = unguarded. Auto level
    /// already matches the loudness, so the guard is an optional extra safety net.
    pub loudness_guard: bool,
    /// Auto level (default on): the effect follows the input loudness, so the distortion amount and
    /// the output loudness stay the same at any volume (see `docs/configuration.md`). A saved `[jbl]`
    /// table without the key loads as on. Off = the legacy fixed-drive behaviour (calibrated for
    /// about -18 dBFS RMS music only).
    #[serde(default = "default_true")]
    pub auto_level: bool,
    // Mids/highs
    /// Mids/highs pass-through (0..=1, default 0.15): how much of the DRY signal above `mid_pass_hz`
    /// replaces the processed signal there. Small by default so the bass takes over and the clipper,
    /// ducker and lo-fi act on the whole signal (0.9 kept the mids/highs clean: 1 kHz THD about
    /// -38 dB instead of -20 dB). 0 = off (the whole signal is processed). A saved `[jbl]` table
    /// that has no `mid_pass` key loads as 0, so an older customised sound is not changed behind the
    /// user's back; the Default preset and `JblParams::default()` use 0.15.
    #[serde(default = "zero_f32")]
    pub mid_pass: f32,
    /// Crossover frequency Hz of the mids/highs pass-through (100..=1000, default 400).
    pub mid_pass_hz: f32,
}

impl Default for JblParams {
    fn default() -> Self {
        JblParams {
            duck_on: true,
            duck_depth: 4.0,
            duck_floor_db: -9.0,
            duck_attack_ms: 5.0,
            duck_release_ms: 120.0,
            detect_hz: 100.0,
            shelf_hz: 70.0,
            shelf_db: 10.0,
            res_on: true,
            res_hz: 50.0,
            res_db: 6.5,
            res_q: 2.5,
            drive_max: 8.0,
            bias_max: 0.35,
            kick_on: true,
            kick_hz: 130.0,
            kick_drive_max: 3.0,
            crush_on: true,
            crush_bits_min: 10.0,
            hold_on: true,
            hold_max: 3.0,
            lpf_on: true,
            lpf_hz_start: 16000.0,
            lpf_hz_end: 10000.0,
            lfo_on: true,
            lfo_rate_hz: 5.0,
            lfo_depth: 0.3,
            lfo_follow_bass: true,
            lfo_target: JblLfoTarget::Gain,
            out_trim_db: 1.1,
            bass_boost_db: 0.0,
            bass_boost_hz: 80.0,
            loudness_guard: false,
            auto_level: true,
            mid_pass: 0.15,
            mid_pass_hz: 400.0,
        }
    }
}

impl JblParams {
    /// Clamps every field to its range; NaN/inf become the default. `lpf_hz_start`/`lpf_hz_end`
    /// are each clamped independently (either ordering is safe).
    pub fn sanitized(self) -> Self {
        let d = JblParams::default();
        let c = |v: f32, dv: f32, lo: f32, hi: f32| if v.is_finite() { v.clamp(lo, hi) } else { dv };
        JblParams {
            duck_depth: c(self.duck_depth, d.duck_depth, 0.0, 6.0),
            duck_floor_db: c(self.duck_floor_db, d.duck_floor_db, DUCK_FLOOR_OFF_DB, 0.0),
            duck_attack_ms: c(self.duck_attack_ms, d.duck_attack_ms, 0.5, 50.0),
            duck_release_ms: c(self.duck_release_ms, d.duck_release_ms, 10.0, 800.0),
            detect_hz: c(self.detect_hz, d.detect_hz, 40.0, 250.0),
            shelf_hz: c(self.shelf_hz, d.shelf_hz, 30.0, 200.0),
            shelf_db: c(self.shelf_db, d.shelf_db, 0.0, 18.0),
            res_hz: c(self.res_hz, d.res_hz, 25.0, 120.0),
            res_db: c(self.res_db, d.res_db, 0.0, 18.0),
            res_q: c(self.res_q, d.res_q, 0.5, 10.0),
            drive_max: c(self.drive_max, d.drive_max, 1.0, 12.0),
            bias_max: c(self.bias_max, d.bias_max, 0.0, 0.8),
            kick_hz: c(self.kick_hz, d.kick_hz, 60.0, 300.0),
            kick_drive_max: c(self.kick_drive_max, d.kick_drive_max, 1.0, 6.0),
            crush_bits_min: c(self.crush_bits_min, d.crush_bits_min, 6.0, 16.0),
            hold_max: c(self.hold_max, d.hold_max, 1.0, 8.0),
            lpf_hz_start: c(self.lpf_hz_start, d.lpf_hz_start, 2000.0, 20000.0),
            lpf_hz_end: c(self.lpf_hz_end, d.lpf_hz_end, 2000.0, 20000.0),
            lfo_rate_hz: c(self.lfo_rate_hz, d.lfo_rate_hz, 0.5, 20.0),
            lfo_depth: c(self.lfo_depth, d.lfo_depth, 0.0, 1.0),
            out_trim_db: c(self.out_trim_db, d.out_trim_db, -12.0, 12.0),
            bass_boost_db: c(self.bass_boost_db, d.bass_boost_db, 0.0, 12.0),
            bass_boost_hz: c(self.bass_boost_hz, d.bass_boost_hz, 30.0, 200.0),
            mid_pass: c(self.mid_pass, d.mid_pass, 0.0, 1.0),
            mid_pass_hz: c(self.mid_pass_hz, d.mid_pass_hz, 100.0, 1000.0),
            ..self
        }
    }
}

/// `duck_floor_db` value that disables the floor.
pub const DUCK_FLOOR_OFF_DB: f32 = -24.0;

fn no_duck_floor() -> f32 {
    DUCK_FLOOR_OFF_DB
}

fn zero_f32() -> f32 {
    0.0
}

fn default_jbl_intensity() -> u8 {
    90
}

/// A named JBL Bass Mode sound: an Intensity plus a full [`JblParams`] set. Presets are only
/// applied from the GUI (values are copied into the config); no preset name is ever stored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JblPreset {
    pub name: &'static str,
    pub description: &'static str,
    pub intensity: u8,
    pub params: JblParams,
}

impl JblPreset {
    /// True if `intensity` and `params` equal this preset (floats within a tiny epsilon).
    pub fn matches(&self, intensity: u8, params: &JblParams) -> bool {
        const EPS: f32 = 1e-4;
        let (a, b) = (&self.params, params);
        let f = |x: f32, y: f32| (x - y).abs() <= EPS;
        self.intensity == intensity
            && a.duck_on == b.duck_on
            && f(a.duck_depth, b.duck_depth)
            && f(a.duck_floor_db, b.duck_floor_db)
            && f(a.duck_attack_ms, b.duck_attack_ms)
            && f(a.duck_release_ms, b.duck_release_ms)
            && f(a.detect_hz, b.detect_hz)
            && f(a.shelf_hz, b.shelf_hz)
            && f(a.shelf_db, b.shelf_db)
            && a.res_on == b.res_on
            && f(a.res_hz, b.res_hz)
            && f(a.res_db, b.res_db)
            && f(a.res_q, b.res_q)
            && f(a.drive_max, b.drive_max)
            && f(a.bias_max, b.bias_max)
            && a.kick_on == b.kick_on
            && f(a.kick_hz, b.kick_hz)
            && f(a.kick_drive_max, b.kick_drive_max)
            && a.crush_on == b.crush_on
            && f(a.crush_bits_min, b.crush_bits_min)
            && a.hold_on == b.hold_on
            && f(a.hold_max, b.hold_max)
            && a.lpf_on == b.lpf_on
            && f(a.lpf_hz_start, b.lpf_hz_start)
            && f(a.lpf_hz_end, b.lpf_hz_end)
            && a.lfo_on == b.lfo_on
            && f(a.lfo_rate_hz, b.lfo_rate_hz)
            && f(a.lfo_depth, b.lfo_depth)
            && a.lfo_follow_bass == b.lfo_follow_bass
            && a.lfo_target == b.lfo_target
            && f(a.out_trim_db, b.out_trim_db)
            && f(a.bass_boost_db, b.bass_boost_db)
            && f(a.bass_boost_hz, b.bass_boost_hz)
            && a.loudness_guard == b.loudness_guard
            && a.auto_level == b.auto_level
            && f(a.mid_pass, b.mid_pass)
            && f(a.mid_pass_hz, b.mid_pass_hz)
    }
}

/// The built-in JBL Bass Mode presets, "Default" first. Each `out_trim_db` loudness-matches the
/// preset to the default sound (see `jbl_presets_loudness_matched` in `dsp.rs`).
pub fn jbl_presets() -> &'static [JblPreset] {
    static PRESETS: std::sync::OnceLock<Vec<JblPreset>> = std::sync::OnceLock::new();
    PRESETS.get_or_init(|| {
        let d = JblParams::default();
        // The other presets keep the pre-"lift mids/highs" values of the fields the Default retune changed,
        // so only the Default sound moves (their loudness is re-trimmed via `out_trim_db`).
        let legacy = JblParams {
            duck_depth: 4.5,
            duck_release_ms: 150.0,
            shelf_db: 14.0,
            res_db: 9.0,
            drive_max: 5.5,
            kick_drive_max: 3.0,
            lpf_hz_start: 14000.0,
            lpf_hz_end: 7500.0,
            mid_pass: 0.0,
            duck_floor_db: DUCK_FLOOR_OFF_DB,
            ..d
        };
        vec![
            JblPreset {
                name: "Default",
                description: "The tuned \"fun\" sound: big bass, pumping, grit and a light rattle.",
                intensity: default_jbl_intensity(),
                params: d,
            },
            JblPreset {
                name: "Mild",
                description: "Subtle warmth: a gentle bass lift and a light pump, no grit.",
                intensity: 45,
                params: JblParams {
                    duck_depth: 1.0,
                    duck_attack_ms: 8.0,
                    duck_release_ms: 140.0,
                    shelf_hz: 80.0,
                    shelf_db: 6.0,
                    res_hz: 55.0,
                    res_db: 3.0,
                    res_q: 1.5,
                    drive_max: 1.5,
                    bias_max: 0.1,
                    kick_on: false,
                    crush_on: false,
                    hold_on: false,
                    lpf_on: false,
                    lfo_on: false,
                    out_trim_db: 8.1,
                    ..legacy
                },
            },
            JblPreset {
                name: "Clean Boost",
                description: "Bass lift only: shelf and boom, no distortion, pumping or lo-fi.",
                intensity: 80,
                params: JblParams {
                    duck_on: false,
                    shelf_hz: 70.0,
                    shelf_db: 10.0,
                    res_hz: 50.0,
                    res_db: 6.0,
                    res_q: 2.0,
                    drive_max: 1.0,
                    bias_max: 0.0,
                    kick_on: false,
                    crush_on: false,
                    hold_on: false,
                    lpf_on: false,
                    lfo_on: false,
                    out_trim_db: 5.5,
                    ..legacy
                },
            },
            JblPreset {
                name: "Club Sub",
                description: "Heavy clean sub around 47 Hz with a slow, light pump and an open top end.",
                intensity: 85,
                params: JblParams {
                    duck_depth: 1.5,
                    duck_attack_ms: 8.0,
                    duck_release_ms: 220.0,
                    shelf_hz: 60.0,
                    shelf_db: 15.0,
                    res_hz: 47.0,
                    res_db: 12.0,
                    res_q: 2.0,
                    drive_max: 2.5,
                    bias_max: 0.1,
                    kick_hz: 110.0,
                    kick_drive_max: 1.8,
                    crush_on: false,
                    hold_on: false,
                    lpf_on: false,
                    lfo_on: false,
                    out_trim_db: 2.8,
                    ..legacy
                },
            },
            JblPreset {
                name: "Trunk Rattle",
                description: "Loose-panel rattle: a fast wobble on every bass hit, with some grit.",
                intensity: 85,
                params: JblParams {
                    duck_depth: 3.0,
                    duck_release_ms: 130.0,
                    drive_max: 4.0,
                    bias_max: 0.3,
                    kick_drive_max: 2.5,
                    crush_bits_min: 11.0,
                    hold_max: 2.0,
                    lpf_hz_end: 9000.0,
                    lfo_on: true,
                    lfo_rate_hz: 8.0,
                    lfo_depth: 0.5,
                    lfo_follow_bass: true,
                    lfo_target: JblLfoTarget::Both,
                    out_trim_db: 5.7,
                    ..legacy
                },
            },
            JblPreset {
                name: "Pumper",
                description: "Extreme sidechain wobble: the whole mix ducks hard on every kick.",
                intensity: 90,
                params: JblParams {
                    duck_depth: 6.0,
                    duck_attack_ms: 1.5,
                    duck_release_ms: 200.0,
                    shelf_db: 14.0,
                    res_db: 9.0,
                    res_q: 2.5,
                    drive_max: 3.0,
                    bias_max: 0.2,
                    kick_drive_max: 2.0,
                    crush_on: false,
                    hold_on: false,
                    lpf_hz_start: 16000.0,
                    lpf_hz_end: 12000.0,
                    lfo_on: false,
                    out_trim_db: 10.2,
                    ..legacy
                },
            },
            JblPreset {
                name: "Blown Speaker",
                description: "Torn-cone destruction: heavy clipping, bit-crush, sample-hold and a dull top.",
                intensity: 95,
                params: JblParams {
                    duck_depth: 2.5,
                    duck_release_ms: 120.0,
                    shelf_db: 12.0,
                    res_db: 8.0,
                    res_q: 2.0,
                    drive_max: 10.0,
                    bias_max: 0.6,
                    kick_drive_max: 5.0,
                    crush_bits_min: 6.5,
                    hold_max: 5.0,
                    lpf_hz_start: 8000.0,
                    lpf_hz_end: 4500.0,
                    lfo_on: false,
                    out_trim_db: 4.3,
                    ..legacy
                },
            },
        ]
    })
}

impl Default for Config {
    fn default() -> Self {
        Config {
            master_driver: String::new(),
            output_channels: [0, 1],
            passthrough_inputs: true,
            bypass: false,
            eq_enabled: true,
            limiter: true,
            profile_source: None,
            profile: EqProfile::default(),
            sample_rate: 0,
            safety_buffer_ms: default_safety_ms(),
            safety_buffer_auto: true,
            resampler_quality: ResamplerQuality::default(),
            auto_headroom: true,
            mute_on_fault: true,
            jitter_margin_ms: 0.0,
            jitter_margin_by_device: BTreeMap::new(),
            jbl_bass_mode: false,
            jbl_intensity: default_jbl_intensity(),
            jbl: JblParams::default(),
            bridge: BridgeConfig::default(),
            portable: PortableConfig::default(),
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
    /// Load from a TOML file. Unknown keys are ignored, so files written before the M3a bridge
    /// cutover still load; removed keys found in the file are reported once per process through
    /// [`take_deprecation_notice`].
    pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_toml_str(&text)
    }

    /// Parses TOML text; also reports removed keys (see [`take_deprecation_notice`]).
    pub fn from_toml_str(text: &str) -> Result<Config, ConfigError> {
        note_removed_keys(text);
        toml::from_str(text).map_err(Into::into)
    }

    /// Learned jitter margin (ms) of the bridge source (the only source since M3a): the
    /// [`BRIDGE_SOURCE_KEY`] entry of `jitter_margin_by_device`, else the legacy
    /// `jitter_margin_ms` while no keyed entry exists, else 0.
    pub fn jitter_margin_for(&self) -> f32 {
        if let Some((_, v)) =
            self.jitter_margin_by_device.iter().find(|(k, _)| k.trim().eq_ignore_ascii_case(BRIDGE_SOURCE_KEY))
        {
            return *v;
        }
        if self.jitter_margin_by_device.is_empty() {
            self.jitter_margin_ms
        } else {
            0.0
        }
    }

    /// Stores the learned margin for the bridge source (0 forgets it). Any legacy per-device keys
    /// are dropped (single global margin) and the value is mirrored into the legacy
    /// `jitter_margin_ms` so older readers keep working.
    pub fn set_jitter_margin(&mut self, ms: f32) {
        self.jitter_margin_by_device.retain(|k, _| k.trim().eq_ignore_ascii_case(BRIDGE_SOURCE_KEY));
        let ms = if ms.is_finite() { ms.clamp(0.0, 40.0) } else { 0.0 };
        if ms > 0.0 {
            self.jitter_margin_by_device.insert(BRIDGE_SOURCE_KEY.to_string(), ms);
        } else {
            self.jitter_margin_by_device.remove(BRIDGE_SOURCE_KEY);
        }
        self.jitter_margin_ms = ms;
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
    /// the learned jitter margin collapses to the single [`BRIDGE_SOURCE_KEY`] entry
    /// (legacy per-device keys from old files are migrated there).
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
        c.jbl = c.jbl.sanitized();
        c.bridge = c.bridge.sanitized();
        c.portable = c.portable.sanitized();
        c.jitter_margin_ms = if c.jitter_margin_ms.is_finite() { c.jitter_margin_ms.clamp(0.0, 40.0) } else { 0.0 };
        // Jitter margin: one global value stored under BRIDGE_SOURCE_KEY. Legacy per-device
        // keys (old files) collapse into it; the legacy scalar mirrors it when set.
        let keyed: Option<f32> = c
            .jitter_margin_by_device
            .iter()
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(BRIDGE_SOURCE_KEY))
            .map(|(_, v)| *v)
            .filter(|v| v.is_finite());
        let legacy_max = c.jitter_margin_by_device.values().copied().filter(|v| v.is_finite()).fold(0.0f32, f32::max);
        let migrated = keyed
            .or_else(|| (c.jitter_margin_ms > 0.0).then_some(c.jitter_margin_ms))
            .or_else(|| (legacy_max > 0.0).then_some(legacy_max));
        let mut by: BTreeMap<String, f32> = BTreeMap::new();
        if let Some(v) = migrated {
            by.insert(BRIDGE_SOURCE_KEY.to_string(), v.clamp(0.0, 40.0));
        }
        c.jitter_margin_by_device = by;
        if c.sample_rate != 0 {
            c.sample_rate = VALID_RATES
                .iter()
                .copied()
                .min_by_key(|r| r.abs_diff(c.sample_rate))
                .unwrap_or(48000);
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
    fn jbl_toml_round_trip() {
        let mut c = Config::default();
        c.jbl.duck_depth = 3.5;
        c.jbl.lfo_on = true;
        c.jbl.lfo_target = JblLfoTarget::Both;
        c.jbl.out_trim_db = -3.0;
        let text = toml::to_string_pretty(&c).unwrap();
        assert!(text.contains("[jbl]"));
        let back = Config::from_toml_str(&text).unwrap();
        assert_eq!(back.jbl, c.jbl);
    }

    #[test]
    fn jbl_presets_are_valid_and_unique() {
        let presets = jbl_presets();
        assert!(presets.len() >= 7);
        let mut names = std::collections::BTreeSet::new();
        for p in presets {
            assert!(!p.name.trim().is_empty() && !p.description.trim().is_empty());
            assert!(names.insert(p.name), "duplicate preset name {}", p.name);
            assert!(p.intensity <= 100, "{}", p.name);
            assert_eq!(p.params, p.params.sanitized(), "{} has out-of-range values", p.name);
        }
        assert_eq!(presets[0].name, "Default");
    }

    #[test]
    fn jbl_default_preset_matches_config_default_and_matches_is_strict() {
        let c = Config::default();
        let presets = jbl_presets();
        assert!(presets[0].matches(c.jbl_intensity, &c.jbl));
        // only the default config matches "Default"; every other preset differs from it
        for p in &presets[1..] {
            assert!(!p.matches(c.jbl_intensity, &c.jbl), "{} equals the default", p.name);
            assert!(p.matches(p.intensity, &p.params));
        }
        let mut q = c.jbl;
        q.duck_depth += 0.01;
        assert!(!presets[0].matches(c.jbl_intensity, &q));
        let mut q = c.jbl;
        q.lfo_target = JblLfoTarget::Both;
        assert!(!presets[0].matches(c.jbl_intensity, &q));
        let mut q = c.jbl;
        q.crush_on = !q.crush_on;
        assert!(!presets[0].matches(c.jbl_intensity, &q));
        assert!(!presets[0].matches(c.jbl_intensity - 1, &c.jbl));
        // a TOML round trip keeps the match
        let back = Config::from_toml_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert!(presets[0].matches(back.jbl_intensity, &back.jbl));
    }

    #[test]
    fn jbl_loudness_guard_default_roundtrip_and_matching() {
        // Configs without the key get the guard off (it is now opt-in); an explicit true is kept.
        let c = Config::from_toml_str("[jbl]
duck_depth = 4.0
").unwrap();
        assert!(!c.jbl.loudness_guard);
        let c = Config::from_toml_str("[jbl]
loudness_guard = true
").unwrap();
        assert!(c.jbl.loudness_guard);
        assert!(!JblParams::default().loudness_guard);
        let mut c = Config::default();
        c.jbl.loudness_guard = true;
        let back = Config::from_toml_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert!(back.jbl.loudness_guard);
        assert!(back.sanitized().jbl.loudness_guard);
        // Every preset has the guard off, and flipping it makes the preset stop matching.
        for pr in jbl_presets() {
            assert!(!pr.params.loudness_guard, "{}", pr.name);
            let on = JblParams { loudness_guard: true, ..pr.params };
            assert!(!pr.matches(pr.intensity, &on), "{}", pr.name);
        }
    }

    #[test]
    fn jbl_auto_level_default_roundtrip_and_matching() {
        // Missing key = on; an explicit false is kept through a round trip and sanitizing.
        let c = Config::from_toml_str("[jbl]
duck_depth = 4.0
").unwrap();
        assert!(c.jbl.auto_level);
        let c = Config::from_toml_str("[jbl]
auto_level = false
").unwrap();
        assert!(!c.jbl.auto_level);
        assert!(JblParams::default().auto_level);
        let mut c = Config::default();
        c.jbl.auto_level = false;
        let back = Config::from_toml_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert!(!back.jbl.auto_level);
        assert!(!back.sanitized().jbl.auto_level);
        for pr in jbl_presets() {
            assert!(pr.params.auto_level, "{}", pr.name);
            let off = JblParams { auto_level: false, ..pr.params };
            assert!(!pr.matches(pr.intensity, &off), "{}", pr.name);
        }
    }

    #[test]
    fn jbl_bass_boost_defaults_roundtrip_and_clamps() {
        let c = Config::from_toml_str("[jbl]
duck_depth = 4.0
").unwrap();
        assert_eq!(c.jbl.bass_boost_db, 0.0);
        assert_eq!(c.jbl.bass_boost_hz, 80.0);
        let mut c = Config::default();
        c.jbl.bass_boost_db = 7.5;
        c.jbl.bass_boost_hz = 120.0;
        let back = Config::from_toml_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(back.jbl.bass_boost_db, 7.5);
        assert_eq!(back.jbl.bass_boost_hz, 120.0);
        let hi = JblParams { bass_boost_db: 99.0, bass_boost_hz: 1.0, ..JblParams::default() }.sanitized();
        assert_eq!((hi.bass_boost_db, hi.bass_boost_hz), (12.0, 30.0));
        let lo = JblParams { bass_boost_db: -3.0, bass_boost_hz: 1e6, ..JblParams::default() }.sanitized();
        assert_eq!((lo.bass_boost_db, lo.bass_boost_hz), (0.0, 200.0));
        let nan = JblParams { bass_boost_db: f32::NAN, bass_boost_hz: f32::INFINITY, ..JblParams::default() }.sanitized();
        assert_eq!((nan.bass_boost_db, nan.bass_boost_hz), (0.0, 80.0));
        let p = JblParams { bass_boost_db: 3.0, ..JblParams::default() };
        assert!(jbl_presets().iter().all(|pr| !pr.matches(pr.intensity, &p)));
        assert!(jbl_presets().iter().all(|pr| pr.params.bass_boost_db == 0.0));
    }

    #[test]
    fn jbl_mid_pass_defaults_roundtrip_clamps_and_matching() {
        let d = JblParams::default();
        assert_eq!((d.mid_pass, d.mid_pass_hz), (0.15, 400.0));
        // Only the Default preset uses the pass-through; the others keep their old sound.
        let presets = jbl_presets();
        assert_eq!(presets[0].params.mid_pass, 0.15);
        assert!(presets[1..].iter().all(|p| p.params.mid_pass == 0.0), "other presets stay without pass-through");
        // A saved [jbl] table without the key keeps its old sound (mid_pass 0); the hz key defaults.
        let c = Config::from_toml_str("[jbl]
duck_depth = 4.0
").unwrap();
        assert_eq!((c.jbl.mid_pass, c.jbl.mid_pass_hz), (0.0, 400.0));
        let c = Config::from_toml_str("[jbl]
mid_pass = 0.5
mid_pass_hz = 250.0
").unwrap();
        assert_eq!((c.jbl.mid_pass, c.jbl.mid_pass_hz), (0.5, 250.0));
        // No [jbl] table at all: the full default.
        assert_eq!(Config::from_toml_str("jbl_intensity = 50
").unwrap().jbl.mid_pass, 0.15);
        let mut c = Config::default();
        c.jbl.mid_pass = 0.35;
        c.jbl.mid_pass_hz = 640.0;
        let back = Config::from_toml_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!((back.jbl.mid_pass, back.jbl.mid_pass_hz), (0.35, 640.0));
        // Clamps and NaN.
        let hi = JblParams { mid_pass: 9.0, mid_pass_hz: 1.0, ..d }.sanitized();
        assert_eq!((hi.mid_pass, hi.mid_pass_hz), (1.0, 100.0));
        let lo = JblParams { mid_pass: -1.0, mid_pass_hz: 1e9, ..d }.sanitized();
        assert_eq!((lo.mid_pass, lo.mid_pass_hz), (0.0, 1000.0));
        let nan = JblParams { mid_pass: f32::NAN, mid_pass_hz: f32::INFINITY, ..d }.sanitized();
        assert_eq!((nan.mid_pass, nan.mid_pass_hz), (d.mid_pass, d.mid_pass_hz));
        // matches() notices both fields.
        let c = Config::default();
        assert!(!presets[0].matches(c.jbl_intensity, &JblParams { mid_pass: 0.5, ..c.jbl }));
        assert!(!presets[0].matches(c.jbl_intensity, &JblParams { mid_pass_hz: 500.0, ..c.jbl }));
        assert!(!presets[1].matches(presets[1].intensity, &JblParams { mid_pass: 0.1, ..presets[1].params }));
    }

    #[test]
    fn jbl_duck_floor_defaults_roundtrip_clamps_and_matching() {
        let d = JblParams::default();
        assert_eq!((d.duck_depth, d.duck_attack_ms, d.duck_release_ms, d.duck_floor_db), (4.0, 5.0, 120.0, -9.0));
        let presets = jbl_presets();
        assert_eq!(presets[0].params.duck_floor_db, -9.0);
        assert!(presets[1..].iter().all(|p| p.params.duck_floor_db == DUCK_FLOOR_OFF_DB), "others keep the plain curve");
        // Old [jbl] tables keep their plain ducking curve.
        let c = Config::from_toml_str("[jbl]
duck_depth = 4.0
").unwrap();
        assert_eq!(c.jbl.duck_floor_db, DUCK_FLOOR_OFF_DB);
        assert_eq!(Config::from_toml_str("jbl_intensity = 50
").unwrap().jbl.duck_floor_db, -9.0);
        let mut c = Config::default();
        c.jbl.duck_floor_db = -4.5;
        let back = Config::from_toml_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(back.jbl.duck_floor_db, -4.5);
        assert_eq!(JblParams { duck_floor_db: -99.0, ..d }.sanitized().duck_floor_db, DUCK_FLOOR_OFF_DB);
        assert_eq!(JblParams { duck_floor_db: 3.0, ..d }.sanitized().duck_floor_db, 0.0);
        assert_eq!(JblParams { duck_floor_db: f32::NAN, ..d }.sanitized().duck_floor_db, -9.0);
        assert!(!presets[0].matches(Config::default().jbl_intensity, &JblParams { duck_floor_db: -8.0, ..d }));
        assert!(!presets[1].matches(presets[1].intensity, &JblParams { duck_floor_db: -20.0, ..presets[1].params }));
    }

    #[test]
    fn jbl_old_toml_loads_defaults() {
        let c = Config::from_toml_str("jbl_bass_mode = true\njbl_intensity = 55\n").unwrap();
        assert_eq!(c.jbl, JblParams::default());
        assert_eq!(c.jbl_intensity, 55);
        let p = Config::from_toml_str("[jbl]\nduck_depth = 4.0\n").unwrap();
        assert_eq!(p.jbl.duck_depth, 4.0);
        assert_eq!(p.jbl.shelf_hz, 70.0);
    }

    #[test]
    fn eq_enabled_defaults_on_and_round_trips() {
        assert!(Config::default().eq_enabled);
        assert!(Config::from_toml_str("bypass = true\n").unwrap().eq_enabled, "old files keep the EQ on");
        let c = Config::from_toml_str("eq_enabled = false\n").unwrap();
        assert!(!c.eq_enabled);
        let back = Config::from_toml_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert!(!back.eq_enabled);
    }

    #[test]
    fn jbl_sanitized_clamps_and_replaces_nan() {
        let mut c = Config::default();
        c.jbl.duck_depth = 99.0;
        c.jbl.res_q = f32::NAN;
        c.jbl.shelf_hz = f32::INFINITY;
        c.jbl.out_trim_db = -100.0;
        c.jbl.lpf_hz_start = 1.0;
        c.jbl.lpf_hz_end = 1e9;
        c.jbl.lfo_depth = -1.0;
        let s = c.sanitized().jbl;
        let d = JblParams::default();
        assert_eq!(s.duck_depth, 6.0);
        assert_eq!(s.res_q, d.res_q);
        assert_eq!(s.shelf_hz, d.shelf_hz);
        assert_eq!(s.out_trim_db, -12.0);
        assert_eq!(s.lpf_hz_start, 2000.0);
        assert_eq!(s.lpf_hz_end, 20000.0);
        assert_eq!(s.lfo_depth, 0.0);
        assert_eq!(JblParams::default().sanitized(), JblParams::default());
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
        assert_eq!(c.resampler_quality, ResamplerQuality::Cubic);
        assert!(c.jitter_margin_by_device.is_empty() && c.jitter_margin_ms == 0.0);
        let t = toml::to_string_pretty(&c).unwrap();
        assert!(t.contains("resampler_quality = \"cubic\""));
        assert!(!t.contains("capture_") && !t.contains("source_preset") && !t.contains("match_cable_rate"), "{t}");
        let p: Config = toml::from_str("resampler_quality = \"linear\"").unwrap();
        assert_eq!(p.resampler_quality, ResamplerQuality::Linear);
        assert!(p.mute_on_fault);
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
    fn removed_keys_load_and_report_once() {
        // Old files keep loading: the removed keys are unknown to serde and ignored.
        let old = Config::from_toml_str(
            "master_driver = \"X\"\ncapture_device = \"CABLE Output\"\ncapture_mode = \"shared\"\ncapture_period_ms = 6.0\nrender_keepalive = true\nmatch_cable_rate = false\nsource_preset = \"vac\"\n",
        )
        .unwrap();
        assert_eq!(old.master_driver, "X");
        assert_eq!(Config::default().master_driver, ""); // defaults for everything removed
        // Exactly one notice per process, naming the keys found in the raw TOML.
        let n = take_deprecation_notice().expect("first take reports the removed keys");
        for k in ["capture_device", "capture_mode", "capture_period_ms", "render_keepalive", "match_cable_rate", "source_preset"] {
            assert!(n.contains(k), "{n} must mention {k}");
        }
        assert!(n.contains("VBMatrix"), "{n}");
        assert!(take_deprecation_notice().is_none(), "the notice is reported only once");
    }

    #[test]
    fn old_files_load_unchanged() {
        let old = Config::from_toml_str("master_driver = \"X\"\njitter_margin_ms = 4.5\n").unwrap();
        assert_eq!(old.master_driver, "X");
        assert_eq!(old.jitter_margin_ms, 4.5);
        assert!(old.jitter_margin_by_device.is_empty(), "the map is only created by sanitized()");
        // removed keys do not error
        let hifi = Config::from_toml_str("capture_device = \"Hi-Fi Cable Output (VB-Audio Hi-Fi Cable)\"").unwrap();
        assert_eq!(hifi, Config::default());
    }

    #[test]
    fn jitter_margin_rekeys_to_the_bridge_source() {
        // legacy scalar migrates under the fixed "vbmatrix" key
        let c = Config { jitter_margin_ms: 7.5, ..Config::default() }.sanitized();
        assert_eq!(c.jitter_margin_by_device.get(BRIDGE_SOURCE_KEY), Some(&7.5));
        assert_eq!(c.jitter_margin_for(), 7.5);
        // legacy per-device keys from old files collapse into the single source key
        let mut old = Config::default();
        old.jitter_margin_by_device.insert("Hi-Fi Cable Output".into(), 7.5);
        let old = old.sanitized();
        assert_eq!(old.jitter_margin_by_device.get(BRIDGE_SOURCE_KEY), Some(&7.5));
        assert_eq!(old.jitter_margin_by_device.len(), 1);
        assert_eq!(old.jitter_margin_for(), 7.5);
        // a keyed value wins over the stale scalar
        let mut both = Config { jitter_margin_ms: 3.0, ..Config::default() };
        both.jitter_margin_by_device.insert(BRIDGE_SOURCE_KEY.into(), 5.0);
        assert_eq!(both.sanitized().jitter_margin_for(), 5.0);
        // legacy fallback only while no map exists
        let raw = Config { jitter_margin_ms: 3.0, ..Config::default() };
        assert_eq!(raw.jitter_margin_for(), 3.0);
        // set / clear
        let mut c = c;
        c.set_jitter_margin(4.2);
        assert_eq!(c.jitter_margin_for(), 4.2);
        assert_eq!(c.jitter_margin_by_device.len(), 1);
        let back = Config::from_toml_str(&toml::to_string_pretty(&c).unwrap()).unwrap().sanitized();
        assert_eq!(back.jitter_margin_by_device, c.jitter_margin_by_device);
        c.set_jitter_margin(0.0);
        assert_eq!(c.jitter_margin_for(), 0.0);
        assert!(c.jitter_margin_by_device.is_empty());
        // sanitising clamps / drops junk
        let mut m = Config::default();
        m.jitter_margin_by_device.insert("a".into(), 99.0);
        m.jitter_margin_by_device.insert(BRIDGE_SOURCE_KEY.into(), f32::NAN);
        let m = m.sanitized();
        assert_eq!(m.jitter_margin_by_device.len(), 1);
        assert_eq!(m.jitter_margin_by_device[BRIDGE_SOURCE_KEY], 40.0);
    }

    #[test]
    fn frames_helper() {
        assert_eq!(frames_for_ms(6.0, 48000), 288);
        assert_eq!(frames_for_ms(10.0, 44100), 441);
        assert_eq!(frames_for_ms(-1.0, 48000), 0);
    }

    #[test]
    fn bridge_config_parsing_defaults_and_clamps() {
        // Defaults: no ring-target floor, loopback on.
        let d = BridgeConfig::default();
        assert_eq!((d.ring_target_ms, d.loopback_enabled), (0.0, true));
        // A file without a [bridge] table loads the defaults (old configs keep working).
        let c = Config::from_toml_str("master_driver = \"X\"\n").unwrap();
        assert_eq!(c.bridge, d);
        // A present table parses.
        let c = Config::from_toml_str("[bridge]\nring_target_ms = 12.5\nloopback_enabled = false\n").unwrap();
        assert_eq!(c.bridge.ring_target_ms, 12.5);
        assert!(!c.bridge.loopback_enabled);
        // Round trip.
        let mut c = Config::default();
        c.bridge.ring_target_ms = 15.0;
        c.bridge.loopback_enabled = false;
        let text = toml::to_string_pretty(&c).unwrap();
        assert!(text.contains("[bridge]"));
        assert_eq!(Config::from_toml_str(&text).unwrap().bridge, c.bridge);
        // sanitized() clamps (NaN -> 0, range 0..=200 ms).
        let mut c = Config::default();
        c.bridge.ring_target_ms = f32::NAN;
        assert_eq!(c.sanitized().bridge.ring_target_ms, 0.0);
        c.bridge.ring_target_ms = 500.0;
        assert_eq!(c.sanitized().bridge.ring_target_ms, 200.0);
    }

    #[test]
    fn portable_section_defaults_round_trip_and_sanitizes() {
        // Defaults: per-OS auto selection, no devices, no profile override.
        let d = PortableConfig::default();
        assert_eq!(d.source, "auto");
        assert!(d.profile_path.is_none());
        // A file without a [portable] table loads the defaults (old configs keep working).
        let c = Config::from_toml_str("master_driver = \"X\"\n").unwrap();
        assert_eq!(c.portable, d);
        // A present table parses (its `source` key must not trip the removed-key scan).
        let text = "[portable]\nsource = \"pipewire\"\noutput_device = \"hw:UR22C\"\ninput_device = \"UR22C Monitor\"\nprofile_path = \"eq/parametric.txt\"\n";
        let c = Config::from_toml_str(text).unwrap();
        assert_eq!(c.portable.source, "pipewire");
        assert_eq!(c.portable.output_device, "hw:UR22C");
        assert_eq!(c.portable.input_device, "UR22C Monitor");
        assert_eq!(c.portable.profile_path, Some(PathBuf::from("eq/parametric.txt")));
        // Round trip through the pretty serializer.
        let mut c = Config::default();
        c.portable.source = "blackhole".into();
        c.portable.profile_path = Some(PathBuf::from("eq/parametric.txt"));
        let out = toml::to_string_pretty(&c).unwrap();
        assert!(out.contains("[portable]"), "{out}");
        assert_eq!(Config::from_toml_str(&out).unwrap().portable, c.portable);
        // sanitized(): trimmed selectors, empty source -> "auto", empty path -> None.
        let mut c = Config::default();
        c.portable.source = "  ".into();
        c.portable.output_device = " default ".into();
        c.portable.profile_path = Some(PathBuf::new());
        let s = c.sanitized().portable;
        assert_eq!(s.source, "auto");
        assert_eq!(s.output_device, "default");
        assert!(s.profile_path.is_none());
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
