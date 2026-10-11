//! Public status types and the error enum of the engine.

use asio_host::Holder;
use eq_core::{FaultState, LatencyBreakdown};
use wasapi_capture::CaptureMode;

use crate::priority::HardeningReport;

/// A process holding the master driver (exe name and PID).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HolderInfo {
    pub pid: u32,
    pub exe_name: String,
}

impl From<&Holder> for HolderInfo {
    fn from(h: &Holder) -> Self {
        HolderInfo { pid: h.pid, exe_name: h.exe_name.clone() }
    }
}

/// Friendly text for "the master driver is held by another program".
pub fn busy_message(name: &str, holders: &[Holder]) -> String {
    format!(
        "{name} is {}. Close that program (or stop its ASIO engine / pick another device in it) and the router will start automatically.",
        asio_host::describe_holders(holders)
    )
}

/// Lifecycle of the engine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum EngineState {
    #[default]
    Stopped,
    /// Opening the master driver / capture device.
    Starting,
    /// Streaming.
    Running,
    /// Torn down, waiting to retry (exponential back-off).
    Restarting { reason: String },
    /// The master driver is in use by another program; retrying every 2 s until it is free.
    WaitingForDevice { holders: Vec<HolderInfo> },
    /// Fatal problem (bad config, device unusable); waits for a new config or `stop`.
    Failed,
}

impl EngineState {
    pub fn is_running(&self) -> bool {
        matches!(self, EngineState::Running)
    }

    pub fn is_waiting(&self) -> bool {
        matches!(self, EngineState::WaitingForDevice { .. })
    }
}

/// Progress / outcome of the "Learn (30 s)" jitter measurement.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LearnStatus {
    /// A measurement window is running.
    pub active: bool,
    /// Seconds measured so far / total.
    pub progress_s: f32,
    pub total_s: f32,
    /// Number of finished learn runs since the engine started (a changed value means a new result).
    pub finished: u32,
    /// Result of the last finished run: minimal safe jitter headroom in ms (None = failed).
    pub result_ms: Option<f32>,
    /// Why the last run failed (not enough capture data, ...).
    pub error: Option<String>,
}

/// The silent render stream kept alive on a virtual cable's render endpoint.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeepAliveStatus {
    /// Render endpoint it runs on (e.g. "Line 1 (Virtual Audio Cable)").
    pub render_name: String,
    /// Shared-engine period of that endpoint before / after the keep-alive started, in ms
    /// (`None` = could not be read). VAC Line 1: 10.0 -> 1.0.
    pub engine_period_before_ms: Option<f32>,
    pub engine_period_after_ms: Option<f32>,
}

/// Snapshot of everything a GUI wants to show. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct EngineStatus {
    pub state: EngineState,
    pub master_name: String,
    /// Rate of the WASAPI capture stream (Hz).
    pub cable_rate: u32,
    /// Rate of the ASIO master (Hz).
    pub asio_rate: u32,
    /// ASIO buffer size in frames.
    pub buffer_size: u32,
    /// Driver latencies in samples at `asio_rate`.
    pub input_latency: u32,
    pub output_latency: u32,
    /// Master output channels (indexes) receiving the stereo pair.
    pub output_channels: [usize; 2],
    pub supported_rates: Vec<u32>,
    /// Ring fill now / target, in cable frames.
    pub ring_fill_frames: u32,
    pub ring_target_frames: u32,
    pub ring_capacity_frames: u32,
    /// Underrun blocks, producer overruns, ASIO overload reports.
    pub underruns: u64,
    pub overruns: u64,
    pub xruns: u64,
    pub discarded_frames: u64,
    pub gate_rearms: u64,
    pub callbacks: u64,
    /// Drift trim of the resampler in ppm.
    pub drift_ppm: f32,
    pub fault: FaultState,
    pub non_finite: u64,
    pub capture_mode_requested: Option<CaptureMode>,
    pub capture_mode_in_use: Option<CaptureMode>,
    pub capture_device: String,
    pub capture_period_frames: u32,
    pub capture_period_us: u32,
    /// Largest number of frames delivered by one capture callback.
    pub capture_burst_max_frames: u32,
    /// Callback load: average / worst duration in microseconds and as percent of the period.
    pub cpu_avg_us: f32,
    pub cpu_max_us: f32,
    pub cpu_avg_pct: f32,
    pub cpu_max_pct: f32,
    /// Peak of the last ~100 ms, dBFS (L, R), before and after the EQ.
    pub peak_in_db: [f32; 2],
    pub peak_out_db: [f32; 2],
    /// Effective safety buffer in ms (config value plus the auto component).
    pub safety_buffer_ms: f32,
    /// Live latency breakdown (see `eq_core::Caps::latency_breakdown`).
    pub latency: LatencyBreakdown,
    /// Mean actual ring fill (cable frames, ~2 s average; 0 until measured).
    pub ring_mean_fill_frames: u32,
    /// Jitter headroom now in the target (cable frames) and what the arrival model currently requires.
    pub jitter_headroom_frames: u32,
    pub required_headroom_frames: u32,
    /// Measured p99.9 of the gap between capture arrival events (ms) and of the fill dip (frames).
    pub arrival_gap_p999_ms: f32,
    pub arrival_dip_p999_frames: u32,
    /// Mean staleness of a captured frame at delivery (ms).
    pub capture_staleness_ms: f32,
    /// Seconds of arrival statistics behind the numbers above (max 30).
    pub arrival_observed_s: f32,
    /// "Learn (30 s)" state.
    pub learn: LearnStatus,
    /// Which process / thread hardening steps succeeded.
    pub hardening: HardeningReport,
    /// Number of restarts since `Engine::start`.
    pub restarts: u32,
    /// Last error strings (cleared when the next start succeeds).
    pub last_error: Option<String>,
    /// Processes currently holding the master driver (empty unless waiting for it).
    pub busy_holders: Vec<HolderInfo>,
    pub capture_error: Option<String>,
    /// Non-error remark about the capture mode actually used (e.g. Exclusive mapped to Shared).
    pub capture_note: Option<String>,
    /// Fixed capture period the config asks for, in frames at the capture rate (0 = driver default),
    /// and the period actually granted by WASAPI (same value as `capture_period_frames`).
    pub capture_period_frames_requested: u32,
    pub capture_period_frames_granted: u32,
    /// Set (and kept until the capture device / period changes) when a fixed-period capture was
    /// abandoned because the corruption monitor tripped:
    /// "Capture fell back to Shared (10 ms): <reason>".
    pub capture_fallback: Option<String>,
    /// The render keep-alive, while it runs.
    pub keepalive: Option<KeepAliveStatus>,
    /// Why a requested keep-alive is not running (non-fatal), or a remark about it.
    pub keepalive_note: Option<String>,
    /// Source preset of the config in force.
    pub source_preset: eq_core::SourcePreset,
    /// Rate the Hi-Fi Cable had before the router set it to the master's rate.
    pub cable_rate_changed_from: Option<u32>,
    /// Human-readable result of the cable rate matching (success or how to fix it manually).
    pub cable_rate_note: Option<String>,
    /// Capture-side counters of the running session: packets, DATA_DISCONTINUITY flags
    /// (after the first packet), silent-flagged packets.
    pub capture_packets: u64,
    pub capture_discontinuities: u64,
    pub capture_silent_packets: u64,
    /// Last config reload failure from the watched file.
    pub config_error: Option<String>,
    pub dropped_logs: u64,
    /// Milliseconds since the stream last started.
    pub uptime_ms: u64,
}

/// Engine errors.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum EngineError {
    #[error("another router engine instance is already running")]
    AlreadyRunning,
    #[error("master driver \"{0}\" is not allowed (it is the frozen v1 virtual driver)")]
    ForbiddenMaster(String),
    #[error("ASIO driver \"{0}\" is not registered")]
    MasterNotFound(String),
    #[error("cannot open ASIO driver \"{name}\": {detail} (is another program using it?)")]
    MasterOpen { name: String, detail: String },
    #[error("{}", busy_message(name, holders))]
    MasterBusy { name: String, holders: Vec<Holder> },
    #[error("master has {found} output channel(s); at least 2 are required")]
    TooFewOutputs { found: i32 },
    #[error("master output sample type {0} is not supported")]
    UnsupportedSampleType(i32),
    #[error("ASIO error: {0}")]
    Asio(String),
    #[error("capture device \"{0}\" not found")]
    CaptureDeviceNotFound(String),
    #[error("capture error: {0}")]
    Capture(String),
    #[error("thread error: {0}")]
    Thread(String),
    #[error("the engine is stopped")]
    Stopped,
    #[error("timed out waiting for the engine to start")]
    Timeout,
}

impl EngineError {
    /// True if retrying cannot help without a config change.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            EngineError::ForbiddenMaster(_)
                | EngineError::MasterNotFound(_)
                | EngineError::TooFewOutputs { .. }
                | EngineError::UnsupportedSampleType(_)
                | EngineError::AlreadyRunning
        )
    }
}

impl EngineError {
    /// True if a device (or the audio service) may simply not be there *yet* (e.g. right after
    /// login), so retrying with back-off can succeed without any config change.
    pub fn is_unavailable(&self) -> bool {
        matches!(
            self,
            EngineError::MasterNotFound(_)
                | EngineError::MasterOpen { .. }
                | EngineError::MasterBusy { .. }
                | EngineError::CaptureDeviceNotFound(_)
                | EngineError::Capture(_)
                | EngineError::Asio(_)
        )
    }

    /// Short status text for a retryable error, e.g. "Waiting for Hi-Fi Cable Output...".
    pub fn waiting_text(&self) -> String {
        match self {
            EngineError::MasterNotFound(n) => format!("Waiting for ASIO driver \"{n}\" to appear\u{2026}"),
            EngineError::CaptureDeviceNotFound(n) => format!("Waiting for {n}\u{2026}"),
            EngineError::MasterOpen { name, .. } => format!("Waiting for {name} to open\u{2026}"),
            EngineError::Capture(d) => format!("Waiting for the capture device ({d})\u{2026}"),
            EngineError::Asio(d) => format!("Waiting for the ASIO driver ({d})\u{2026}"),
            other => other.to_string(),
        }
    }
}

/// Maps a driver-open failure of `name` to the engine error.
pub fn map_open_error(name: &str, e: asio_host::HostError) -> EngineError {
    match e {
        asio_host::HostError::DriverBusy { holders } => EngineError::MasterBusy { name: name.to_string(), holders },
        other => EngineError::MasterOpen { name: name.to_string(), detail: other.to_string() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder() -> Holder {
        Holder { pid: 12088, exe_name: "VBAudioMatrixCoconut_x64.exe".into(), exe_path: None }
    }

    #[test]
    fn busy_text() {
        let e = EngineError::MasterBusy { name: "Yamaha Steinberg USB ASIO".into(), holders: vec![holder()] };
        assert_eq!(
            e.to_string(),
            "Yamaha Steinberg USB ASIO is in use by VBAudioMatrixCoconut_x64.exe (PID 12088). Close that program (or stop its ASIO engine / pick another device in it) and the router will start automatically."
        );
        assert!(!e.is_fatal());
    }

    #[test]
    fn unavailable_classification_and_text() {
        let nf = EngineError::CaptureDeviceNotFound("Hi-Fi Cable Output".into());
        assert!(nf.is_unavailable() && !nf.is_fatal());
        assert_eq!(nf.waiting_text(), "Waiting for Hi-Fi Cable Output\u{2026}");
        let m = EngineError::MasterNotFound("Yamaha".into());
        assert!(m.is_unavailable() && m.is_fatal());
        assert!(m.waiting_text().starts_with("Waiting for ASIO driver"));
        assert!(!EngineError::ForbiddenMaster("x".into()).is_unavailable());
        assert!(!EngineError::TooFewOutputs { found: 1 }.is_unavailable());
        assert!(!EngineError::AlreadyRunning.is_unavailable());
    }

    #[test]
    fn open_error_mapping() {
        let e = map_open_error("Y", asio_host::HostError::DriverBusy { holders: vec![holder()] });
        assert!(matches!(e, EngineError::MasterBusy { ref holders, .. } if holders.len() == 1));
        let e = map_open_error("Y", asio_host::HostError::InitFailed);
        assert_eq!(e, EngineError::MasterOpen { name: "Y".into(), detail: "driver init failed".into() });
    }

    #[test]
    fn holder_info() {
        let i = HolderInfo::from(&holder());
        assert_eq!((i.pid, i.exe_name.as_str()), (12088, "VBAudioMatrixCoconut_x64.exe"));
        assert!(EngineState::WaitingForDevice { holders: vec![i] }.is_waiting());
    }
}

/// What `Engine::apply` did with a new config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Unchanged,
    /// Taken over live (EQ, bypass, limiter, headroom, safety buffer, resampler).
    HotApplied,
    /// A controlled restart was scheduled (reason).
    RestartScheduled(&'static str),
}
