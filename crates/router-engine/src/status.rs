//! Public status types and the error enum of the engine.

use asio_host::Holder;
use eq_core::{FaultState, LatencyBreakdown};

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

/// Snapshot of everything a GUI wants to show. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct EngineStatus {
    pub state: EngineState,
    pub master_name: String,
    /// Rate of the source (the negotiated bridge format; the synthetic rate in measurements) in Hz.
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
    /// Name of the source side: `bridge::BRIDGE_SOURCE_NAME` (or "synthetic source" in measurements).
    pub source_name: String,
    /// Source period in frames (the negotiated bridge buffer; the synthetic packet size in measurements).
    pub capture_period_frames: u32,
    pub capture_period_us: u32,
    /// Largest number of frames delivered by one source burst (synthetic measurements; 0 on the bridge).
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
    /// The engine is attached to the bridge mapping (`Local\eq_asio_router_bridge_v2`).
    pub bridge_attached: bool,
    /// The VBMatrix-driven driver slot is running with a fresh heartbeat
    /// (`driver_running` + no stall within `bridge::BEAT_STALL`).
    pub vbmatrix_running: bool,
    /// Sample rate the driver negotiated with VBMatrix (Hz); also the source rate.
    pub bridge_sample_rate: u32,
    /// ASIO buffer size the driver negotiated (frames); the producer's burst size.
    pub bridge_buffer_frames: u32,
    /// Frames currently in ring A (source) / ring B (loopback).
    pub fill_a: u32,
    pub fill_b: u32,
    /// Ring counters (frames): ring A dropped by the driver / discarded by the
    /// app's latency trim; ring B starved for the driver / dropped by the app.
    pub a_overruns: u64,
    pub a_discarded: u64,
    pub b_underruns: u64,
    pub b_dropped: u64,
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
    #[error("bridge {0}")]
    Bridge(String),
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
                | EngineError::Bridge(_)
                | EngineError::Asio(_)
        )
    }

    /// Short status text for a retryable error, e.g. "Waiting for the VBMatrix bridge (...)".
    pub fn waiting_text(&self) -> String {
        match self {
            EngineError::MasterNotFound(n) => format!("Waiting for ASIO driver \"{n}\" to appear\u{2026}"),
            EngineError::MasterOpen { name, .. } => format!("Waiting for {name} to open\u{2026}"),
            EngineError::Bridge(d) => format!("Waiting for the VBMatrix bridge ({d})\u{2026}"),
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
        let b = EngineError::Bridge("mapping not found".into());
        assert!(b.is_unavailable() && !b.is_fatal());
        assert_eq!(b.waiting_text(), "Waiting for the VBMatrix bridge (mapping not found)\u{2026}");
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
