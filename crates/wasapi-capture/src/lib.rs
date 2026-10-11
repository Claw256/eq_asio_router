//! WASAPI capture of the VB-Audio Hi-Fi Cable output (low latency, robust).
//!
//! * [`convert`]: pure sample-format conversion / channel mapping / accounting (no COM).
//! * [`device`]: endpoint enumeration and format/period discovery.
//! * [`session`]: event-driven capture thread ([`CaptureSession`]).
//! * [`tone`]: optional quiet test tone renderer (used by `capture-probe tone`).
//! * [`impulse`]: quiet click/chirp injection into a cable's playback endpoint with QPC timestamps
//!   (used by `router-cli latency`; also re-exported from [`tone`]).

pub mod band;
pub mod convert;
pub mod device;
pub mod impulse;
pub mod keepalive;
pub mod loopback;
pub mod monitor;
pub mod ks;
pub mod policy;
pub mod process_loopback;
pub mod record;
pub mod session;
pub mod signal;
pub mod tone;
pub mod wdm_baseline;

pub use convert::{FormatInfo, PacketFlags, SampleFormat};
pub use device::{
    find_capture_device, find_device, get_capture_info, list_endpoints, mix_format_sample_rate,
    CaptureInfo, EndpointInfo, EnginePeriods, Flow, DEFAULT_CAPTURE_NAME,
};
pub use policy::{
    cable_base_name, get_endpoint_device_format, match_cable_rates, read_cable_rates,
    set_endpoint_device_format, CableRateResult, DeviceFormat, PolicyError,
};
pub use session::{
    exclusive_rate_candidates, frames_for_ms, plan_fixed_period, DEFAULT_FIXED_PERIOD_MS, CaptureEvent, CaptureMode, CaptureOptions,
    CaptureSession, CaptureStats, FrameSink, MonitorPolicy, StatsSnapshot, MIN_PERIOD_GUARD_MS,
    SAFE_PERIOD_FLOOR_MS,
};
pub use monitor::{packet_fingerprint, CorruptionMonitor, CorruptionReason, MonitorConfig};
pub use impulse::{
    click_samples, is_sink_allowed, onset_index, qpc_hz, qpc_ticks, render_impulses, validate_peak_dbfs, ImpulseRenderOptions,
    ImpulseRenderer, ImpulseShape, ImpulseTimes, ImpulseTrain, RenderFill, RenderInfo, RenderMode, RenderPrefill, DEFAULT_IMPULSE_DBFS,
    MAX_IMPULSE_DBFS,
};
pub use keepalive::{
    is_virtual_cable_name, pair_render_name, RenderKeepAlive, RenderKeepAliveInfo, RenderKeepAliveOptions,
};

/// Errors of this crate.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("{context}: {source}")]
    Com {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
    #[error("no active endpoint matching \"{0}\"")]
    NotFound(String),
    #[error("unsupported mix format: {0}")]
    UnsupportedFormat(String),
    #[error("capture thread failed to start: {0}")]
    Thread(String),
    /// A request was refused by a safety guard (unsafe period, real output device, ...).
    #[error("refused: {0}")]
    Refused(String),
}

impl CaptureError {
    pub(crate) fn com(context: &'static str) -> impl FnOnce(windows::core::Error) -> Self {
        move |source| CaptureError::Com { context, source }
    }
}

pub type Result<T> = std::result::Result<T, CaptureError>;
