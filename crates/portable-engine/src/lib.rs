//! Portable engine core of the cross-platform effort (docs/plans/cross-platform.md,
//! Phase A): the shared skeleton every OS backend plugs into.
//!
//! * [`seams`]: the A3 portability traits (FROZEN) and their small companion types.
//! * [`pipeline`]: the real-time ring -> rate matcher -> EQ -> output path
//!   (port of `router-engine/src/pipeline.rs`, allocation-free/lock-free).
//! * [`session`]: supervision (port of `router-engine/src/logic.rs` + the
//!   `engine.rs` control-loop essentials).
//! * [`status`]: [`seams::StatusTransport`] implementations (Null + in-process).
//! * [`backend`]: per-OS dispatch (cfg only).
//!
//! Thin-crate decision (A2): DSP comes from `eq-core`, the ring / resampler /
//! drift / processor helpers from `router-core` — nothing is forked here.

pub mod backend;
pub mod pipeline;
pub mod seams;
pub mod session;
pub mod status;

#[cfg(test)]
mod pipeline_tests;

pub use seams::{
    Clock, ConfigChange, ControlAction, ControlLoop, OutputCallback, OutputSink, SinkError, SourceError, StatusTransport,
    StreamRequest, StreamSpec, SystemSource,
};
pub use status::{InProcTransport, NullTransport};
