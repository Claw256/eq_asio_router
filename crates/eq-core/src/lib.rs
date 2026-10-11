//! DSP and configuration core for the ASIO EQ router.
//!
//! Pure Rust, no platform dependencies, no `unsafe`.

pub mod autoeq;
pub mod caps;
pub mod config;
pub mod dsp;
pub mod measured;
pub mod status;

pub use autoeq::{parse_parametric_eq, to_parametric_eq_text, AutoEqError, EqProfile};
pub use caps::{Caps, LatencyBreakdown, COMMON_RATES};
pub use measured::{
    date_from_unix, default_render_estimate_ms, now_unix_s, EngineParams, MeasuredEntry, MeasuredLatency, RenderBasis, RenderSide,
    DEFAULT_RENDER_PROFILE, MEASURED_MAX_AGE_DAYS, MIN_CLICKS_TO_SAVE, RENDER_SIDE_ESTIMATE_CABLE_MS, RENDER_SIDE_ESTIMATE_VAC_MS,
};
pub use config::{
    default_path, frames_for_ms, is_vac_name, is_vb_cable_name, modified, CaptureMode, Config, ConfigError,
    ResamplerQuality, SourcePreset, DEFAULT_CAPTURE_DEVICE, MAX_CAPTURE_PERIOD_MS, MIN_CAPTURE_PERIOD_MS,
    RECOMMENDED_CAPTURE_PERIOD_MS, VAC_CAPTURE_DEVICE,
};
pub use dsp::{
    auto_headroom_db, magnitude_response_db, BassFx, Biquad, FaultConfig, FaultDetector, FaultState, FilterKind,
    FilterSpec, GainRamp, StereoEq, BYPASS_SCRATCH_FRAMES, MAX_BANDS,
};
pub use status::{StatusBlock, STATUS_MAGIC, STATUS_MAPPING_NAME, STATUS_VERSION};
