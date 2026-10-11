//! Standalone router engine: Hi-Fi Cable (WASAPI capture) -> rate matching -> EQ ->
//! master ASIO driver (the Yamaha), supervised by a control thread.
//!
//! * [`Engine`]: `start` / `status` / `apply` / `stop` / `publish_caps`.
//! * [`logic`]: pure decision logic (config diff, back-off, stall tracking, latency maths).
//! * [`pipeline`]: the real-time ring -> matcher -> EQ -> ASIO conversion path.

pub mod baseline;
pub mod cable;
pub mod engine;
pub mod logic;
pub mod pipeline;
pub mod probe;
pub mod source;
pub mod priority;
pub mod status;
pub mod synth;
pub mod tap;

#[cfg(test)]
mod pipeline_tests;

pub use cable::{CableFormats, CableFormatsHandle};
pub use engine::{probe_driver, probe_master, Engine, EngineOptions};
pub use baseline::{ChannelReport, Character, Thresholds};
pub use probe::{LatencyProbe, LoopbackProbe, ProbeInfo, ProbeResult};
pub use synth::SyntheticSource;
pub use tap::TapConfig;
pub use priority::HardeningReport;
pub use source::{evaluate_readiness, plan_capture, source_readiness, CapturePlan, CorruptionAction, FallbackState, ReadinessInputs, SourceReadiness};
pub use status::{
    busy_message, ApplyOutcome, EngineError, EngineState, EngineStatus, HolderInfo, KeepAliveStatus, LearnStatus,
};
