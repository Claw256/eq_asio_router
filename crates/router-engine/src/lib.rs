//! Standalone router engine: VBMatrix bridge (shared-memory ring A) -> rate matching -> EQ ->
//! master ASIO driver (the Yamaha), supervised by a control thread.
//!
//! * [`Engine`]: `start` / `status` / `apply` / `stop` / `publish_caps`.
//! * [`logic`]: pure decision logic (config diff, back-off, stall tracking, latency maths).
//! * [`pipeline`]: the real-time ring -> matcher -> EQ -> ASIO conversion path.
//! * [`bridge`]: the shared-memory source (attach, fill statistics, loopback feeder).
//!
//! The engine never opens WASAPI: the source is the bridge, the synthetic measurement source
//! feeds the pipeline in-process, and the WASAPI tooling lives in `wasapi-capture` (`capture-probe`).

pub mod baseline;
pub mod bridge;
pub mod engine;
pub mod logic;
pub mod pipeline;
pub mod probe;
pub mod priority;
pub mod status;
pub mod synth;
pub mod tap;

#[cfg(test)]
mod pipeline_tests;

pub use engine::{probe_driver, probe_master, Engine, EngineOptions};
pub use baseline::{ChannelReport, Character, Thresholds};
pub use probe::{LatencyProbe, LoopbackProbe, ProbeInfo, ProbeResult};
pub use synth::SyntheticSource;
pub use tap::TapConfig;
pub use priority::HardeningReport;
pub use status::{
    busy_message, ApplyOutcome, EngineError, EngineState, EngineStatus, HolderInfo, LearnStatus,
};
