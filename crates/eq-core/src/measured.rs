//! Measured end-to-end latency (`router-cli latency`) and the render-side figure the latency readout uses.
//!
//! The router cannot observe the render side of a cable (playing application -> Windows audio engine ->
//! cable driver -> capture event), so the GUI used to show a fixed guess. `router-cli latency` measures it
//! end to end with an injected click and stores the result in `%APPDATA%\eq_asio_router\latency_measured.toml`.
//! This module holds the pure part: the file format, load / save, the freshness and similarity rules, the
//! default estimates and the date text. No I/O besides the file helpers; everything else is unit-tested.

use crate::config::ConfigError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A measurement older than this is not used for the readout (the Windows / driver state may have changed).
pub const MEASURED_MAX_AGE_DAYS: u64 = 30;
/// Fewest clicks a run needs before it may be stored.
pub const MIN_CLICKS_TO_SAVE: u32 = 3;
/// Capture period of a stored measurement and of the running session may differ by this fraction.
pub const PERIOD_TOLERANCE: f64 = 0.25;
/// Render profile stored for a measurement taken with the tool's default render stream.
pub const DEFAULT_RENDER_PROFILE: &str = "default";

/// Default render-side estimate (ms) used until a measurement exists: a typical shared-mode
/// 10 ms event-driven client rendering into the VBMatrix In endpoint waits about one period for
/// the Windows engine and one more for its own queue (10 + 10 = 20 ms). Basis: measured
/// 2026-10-10 with `capture-probe impulses` (typical-client profile: `IAudioClient::Initialize`,
/// default buffer, one period written per event, one period queued): render queue 19 to 22 ms
/// (an IAudioClient3 client at a 1 ms minimum period about 6 to 7 ms, a client that tops its whole
/// buffer up considerably more). "Typical on this client model, measure to confirm".
pub const RENDER_SIDE_ESTIMATE_MS: f64 = 20.0;

/// Default render-side estimate (ms).
pub fn default_render_estimate_ms() -> f64 {
    RENDER_SIDE_ESTIMATE_MS
}

/// Where a render-side figure comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum RenderBasis {
    /// Default estimate (not measured).
    #[default]
    Estimate,
    /// Measured by `router-cli latency` at this Unix time (seconds).
    Measured { unix_s: u64 },
}

impl RenderBasis {
    pub fn is_measured(&self) -> bool {
        matches!(self, RenderBasis::Measured { .. })
    }

    /// Short label for the UIs. `now_unix_s` is used for the age; 0 skips the age.
    pub fn label(&self, now_unix_s: u64) -> String {
        match *self {
            RenderBasis::Estimate => "estimate, not measured; run router-cli latency".to_string(),
            RenderBasis::Measured { unix_s } => {
                let age = if now_unix_s > unix_s { format!(", {} d old", (now_unix_s - unix_s) / 86_400) } else { String::new() };
                format!("measured {}{age}", date_from_unix(unix_s))
            }
        }
    }
}

/// The render-side term of the latency model with its provenance.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderSide {
    /// Milliseconds used in the model total.
    pub ms: f64,
    pub basis: RenderBasis,
    /// What the default estimate would have said (for the comparison).
    pub estimate_ms: f64,
    /// End-to-end total (to-DAC equivalent) at the time of the measurement, when measured.
    pub measured_total_ms: Option<f64>,
}

impl RenderSide {
    /// The default estimate (no measurement exists yet).
    pub fn estimate() -> Self {
        let ms = default_render_estimate_ms();
        RenderSide { ms, basis: RenderBasis::Estimate, estimate_ms: ms, measured_total_ms: None }
    }

    /// The measured figure of a stored entry.
    pub fn from_entry(e: &MeasuredEntry) -> Self {
        RenderSide {
            ms: e.render_side_ms.max(0.0),
            basis: RenderBasis::Measured { unix_s: e.measured_unix_s },
            estimate_ms: default_render_estimate_ms(),
            measured_total_ms: Some(e.to_dac_ms),
        }
    }
}

/// Engine parameters the similarity rule looks at.
#[derive(Clone, Debug, PartialEq)]
pub struct EngineParams {
    /// Fixed source key the session reads from (the bridge, [`crate::config::BRIDGE_SOURCE_KEY`]).
    pub source: String,
    pub asio_buffer: u32,
    pub capture_period_frames: u32,
}

/// One stored measurement (one source, one engine configuration).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeasuredEntry {
    /// Source the router listened to (the entry key): the fixed [`crate::config::BRIDGE_SOURCE_KEY`].
    /// (Older files used `capture_device`; such entries never match a bridge session.)
    pub source: String,
    /// Render side as the latency model defines it: measured to-DAC equivalent minus (Yamaha out + ring +
    /// capture staleness). Median over the clicks, ms.
    pub render_side_ms: f64,
    /// Queue inside the injecting WASAPI stream (clock estimate), median, ms (0 = unknown).
    pub render_queue_ms: f64,
    /// Injection -> the frame's arrival on the bridge source side (ring-A tap), median, ms.
    pub inj_to_capture_ms: f64,
    /// Bridge source arrival -> loopback tap, median, ms.
    pub capture_to_tap_ms: f64,
    /// (b) corrected total, injection -> device input boundary, median, ms.
    pub total_ms: f64,
    /// To-DAC equivalent total (midpoint estimate), median, ms.
    pub to_dac_ms: f64,
    /// Clicks that went into the medians.
    pub clicks: u32,
    /// `YYYY-MM-DD` (UTC) of the run and the same instant as Unix seconds.
    pub date: String,
    pub measured_unix_s: u64,
    // Engine parameters during the run.
    pub asio_buffer: u32,
    pub capture_period_frames: u32,
    pub safety_ms: f32,
    pub target_frames: u32,
    /// `default` for the tool's standard render stream, else a description of the options that were used.
    pub render_profile: String,
}

impl MeasuredEntry {
    /// Same source (case-insensitive), ASIO buffer, source period and render profile: replaced by a new run.
    pub fn same_key(&self, o: &MeasuredEntry) -> bool {
        self.source.eq_ignore_ascii_case(&o.source)
            && self.asio_buffer == o.asio_buffer
            && self.capture_period_frames == o.capture_period_frames
            && self.render_profile == o.render_profile
    }

    /// True if this entry describes a session with `p` closely enough to use its render side.
    pub fn matches(&self, p: &EngineParams) -> bool {
        let (a, b) = (f64::from(self.capture_period_frames), f64::from(p.capture_period_frames));
        let period_ok = if a == 0.0 || b == 0.0 { a == b } else { (a - b).abs() <= PERIOD_TOLERANCE * a.max(b) };
        self.source.eq_ignore_ascii_case(&p.source) && self.asio_buffer == p.asio_buffer && period_ok
    }
}

/// The contents of `latency_measured.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeasuredLatency {
    pub entries: Vec<MeasuredEntry>,
}

impl MeasuredLatency {
    /// Default location: `%APPDATA%\eq_asio_router\latency_measured.toml`.
    pub fn default_path() -> Option<PathBuf> {
        let appdata = std::env::var_os("APPDATA")?;
        Some(PathBuf::from(appdata).join("eq_asio_router").join("latency_measured.toml"))
    }

    pub fn load(path: impl AsRef<Path>) -> Result<MeasuredLatency, ConfigError> {
        Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
    }

    /// Loads the default file; missing or unreadable means an empty set.
    pub fn load_default() -> MeasuredLatency {
        Self::default_path().and_then(|p| Self::load(p).ok()).unwrap_or_default()
    }

    /// Atomic save (temp file + rename), creating parent directories.
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

    /// Adds `e`, replacing an entry with the same key ([`MeasuredEntry::same_key`]); other entries stay.
    pub fn upsert(&mut self, e: MeasuredEntry) {
        match self.entries.iter_mut().find(|x| x.same_key(&e)) {
            Some(x) => *x = e,
            None => self.entries.push(e),
        }
    }

    /// Read-modify-write of the default file (additive). Returns the path written.
    pub fn record_default(e: MeasuredEntry) -> Result<PathBuf, ConfigError> {
        let path = Self::default_path().ok_or_else(|| ConfigError::Io(std::io::Error::other("%APPDATA% is not set")))?;
        let mut all = Self::load(&path).unwrap_or_default();
        all.upsert(e);
        all.save(&path)?;
        Ok(path)
    }

    /// The newest entry for the default render profile that matches `p` and is at most
    /// [`MEASURED_MAX_AGE_DAYS`] old at `now_unix_s` (a clock that is behind the entry counts as fresh).
    pub fn lookup(&self, p: &EngineParams, now_unix_s: u64) -> Option<&MeasuredEntry> {
        self.entries
            .iter()
            .filter(|e| e.render_profile == DEFAULT_RENDER_PROFILE || e.render_profile.is_empty())
            .filter(|e| e.matches(p))
            .filter(|e| now_unix_s.saturating_sub(e.measured_unix_s) <= MEASURED_MAX_AGE_DAYS * 86_400)
            .max_by_key(|e| e.measured_unix_s)
    }

    /// The render side to use for the session `p`: measured when a fresh matching entry exists, else the estimate.
    pub fn render_side(&self, p: &EngineParams, now_unix_s: u64) -> RenderSide {
        match self.lookup(p, now_unix_s) {
            Some(e) => RenderSide::from_entry(e),
            None => RenderSide::estimate(),
        }
    }
}

/// Unix seconds now (0 if the clock is before 1970).
pub fn now_unix_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `YYYY-MM-DD` (UTC) of a Unix time.
pub fn date_from_unix(unix_s: u64) -> String {
    // Days-to-civil conversion (proleptic Gregorian), after H. Hinnant.
    let z = (unix_s / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BRIDGE_SOURCE_KEY;

    const DAY: u64 = 86_400;

    fn entry(src: &str, period: u32, unix: u64, render: f64) -> MeasuredEntry {
        MeasuredEntry {
            source: src.into(),
            render_side_ms: render,
            render_queue_ms: 12.0,
            inj_to_capture_ms: 40.0,
            capture_to_tap_ms: 23.5,
            total_ms: 63.0,
            to_dac_ms: 64.5,
            clicks: 5,
            date: date_from_unix(unix),
            measured_unix_s: unix,
            asio_buffer: 256,
            capture_period_frames: period,
            safety_ms: 2.0,
            target_frames: 700,
            render_profile: DEFAULT_RENDER_PROFILE.into(),
        }
    }

    fn params(src: &str, period: u32) -> EngineParams {
        EngineParams { source: src.into(), asio_buffer: 256, capture_period_frames: period }
    }

    #[test]
    fn dates() {
        assert_eq!(date_from_unix(0), "1970-01-01");
        assert_eq!(date_from_unix(951_782_400), "2000-02-29");
        assert_eq!(date_from_unix(1_791_590_400 + 3600), "2026-10-10");
        assert_eq!(date_from_unix(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn default_estimate_is_realistic() {
        assert_eq!(default_render_estimate_ms(), 20.0);
        let v = RenderSide::estimate();
        assert!(!v.basis.is_measured() && v.basis == RenderBasis::Estimate);
        assert!(v.basis.label(0).contains("estimate, not measured") && v.basis.label(0).contains("router-cli latency"));
    }

    #[test]
    fn labels_show_date_and_age() {
        let b = RenderBasis::Measured { unix_s: 1_791_590_400 };
        assert_eq!(b.label(0), "measured 2026-10-10");
        assert_eq!(b.label(1_791_590_400 + 3 * DAY + 5), "measured 2026-10-10, 3 d old");
    }

    #[test]
    fn upsert_is_additive_and_replaces_same_key() {
        let mut m = MeasuredLatency::default();
        m.upsert(entry(BRIDGE_SOURCE_KEY, 288, 1000, 30.0));
        m.upsert(entry("other", 480, 1000, 100.0));
        m.upsert(entry("vbmatrix", 288, 2000, 31.0));
        assert_eq!(m.entries.len(), 2);
        assert_eq!(m.entries[0].render_side_ms, 31.0);
        m.upsert(entry(BRIDGE_SOURCE_KEY, 300, 3000, 40.0));
        assert_eq!(m.entries.len(), 3, "the source period is part of the key");
        let mut other = entry(BRIDGE_SOURCE_KEY, 288, 4000, 9.0);
        other.render_profile = "shared-lowlatency/period".into();
        m.upsert(other);
        assert_eq!(m.entries.len(), 4, "render profile is part of the key");
    }

    #[test]
    fn round_trip_and_missing_file() {
        let dir = std::env::temp_dir().join(format!("eq_core_measured_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub").join("latency_measured.toml");
        let mut m = MeasuredLatency::default();
        m.upsert(entry(BRIDGE_SOURCE_KEY, 288, 1_791_590_400, 30.5));
        m.save(&path).unwrap();
        assert!(!dir.join("sub").join("latency_measured.toml.tmp").exists());
        assert_eq!(MeasuredLatency::load(&path).unwrap(), m);
        assert!(MeasuredLatency::load(dir.join("none.toml")).is_err());
        // old / partial files still load (serde defaults); the legacy capture_device key is
        // ignored and the entry never matches a bridge session.
        std::fs::write(&path, "[[entries]]\ncapture_device = \"X\"\n").unwrap();
        let old = MeasuredLatency::load(&path).unwrap();
        assert_eq!(old.entries.len(), 1);
        assert!(old.entries[0].source.is_empty());
        assert!(old.lookup(&params(BRIDGE_SOURCE_KEY, 288), 1_791_590_400).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lookup_uses_fresh_similar_entries_only() {
        let now = 100 * DAY;
        let mut m = MeasuredLatency::default();
        m.upsert(entry(BRIDGE_SOURCE_KEY, 288, now - 2 * DAY, 30.0));
        let p = params(BRIDGE_SOURCE_KEY, 288);
        assert_eq!(m.lookup(&p, now).unwrap().render_side_ms, 30.0);
        let rs = m.render_side(&p, now);
        assert_eq!(rs.ms, 30.0);
        assert!(rs.basis.is_measured());
        assert_eq!(rs.estimate_ms, 20.0);
        assert_eq!(rs.measured_total_ms, Some(64.5));
        // period within 25 %, but not 480 vs 288
        assert!(m.lookup(&params(BRIDGE_SOURCE_KEY, 300), now).is_some());
        assert!(m.lookup(&params(BRIDGE_SOURCE_KEY, 480), now).is_none());
        // a different source never matches
        assert!(m.lookup(&params("legacy-cable", 288), now).is_none());
        let mut p2 = p.clone();
        p2.asio_buffer = 128;
        assert!(m.lookup(&p2, now).is_none());
        // stale
        assert!(m.lookup(&p, now + 40 * DAY).is_none());
        let rs = m.render_side(&p, now + 40 * DAY);
        assert!(!rs.basis.is_measured() && rs.ms == RENDER_SIDE_ESTIMATE_MS);
        // a clock that is behind the measurement does not hide it
        assert!(m.lookup(&p, now - 10 * DAY).is_some());
    }

    #[test]
    fn newest_wins_and_non_default_profiles_are_ignored() {
        let now = 100 * DAY;
        let mut m = MeasuredLatency::default();
        let mut e1 = entry(BRIDGE_SOURCE_KEY, 288, now - 5 * DAY, 30.0);
        e1.capture_period_frames = 288;
        m.entries.push(e1);
        let mut e2 = entry(BRIDGE_SOURCE_KEY, 300, now - DAY, 28.0);
        e2.asio_buffer = 256;
        m.entries.push(e2);
        let mut exp = entry(BRIDGE_SOURCE_KEY, 288, now, 5.0);
        exp.render_profile = "shared-min-buffer".into();
        m.entries.push(exp);
        assert_eq!(m.lookup(&params(BRIDGE_SOURCE_KEY, 288), now).unwrap().render_side_ms, 28.0);
    }
}
