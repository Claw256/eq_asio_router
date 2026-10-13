//! The portability seams (docs/plans/cross-platform.md §A3, "signatures only" made
//! code). FROZEN: changing anything here is a leader-mediated request (A6).
//!
//! Type mapping (A3): `FrameSource` = `router_core::resample::FrameSource` ·
//! `ArrivalStats`/`ArrivalTracker` = `router_core::arrival` · `Handoff` =
//! `router_core::processor::Handoff` · `StatusWriter` = `router_core::status_writer` ·
//! `RateMatcher` = `router_core::resample::RateMatcher` · `EqProcessor` =
//! `router_core::processor::EqProcessor` · QPC = [`Clock`] ·
//! `Handler::buffer_switch` = [`OutputCallback::render`] · `Live` =
//! [`crate::pipeline::Live`] (port of `router-engine/src/pipeline.rs:48`) ·
//! `ConfigChange` is the port of `router-engine/src/logic.rs:32` (defined here so the
//! frozen trait signatures are self-contained; [`crate::session`] ports
//! `classify_change` and the other decision helpers that produce it).

use eq_core::Config;
use router_core::arrival::{ArrivalSnapshot, ArrivalTracker};
use router_core::ring::Producer;
use router_core::status_writer::{BlockReport, BridgeReport, StaticInfo};
use router_core::watchdog::Heartbeat;

use crate::pipeline::Live;

/// Negotiated stream shape (echoes bridge `negotiated()` at
/// `router-engine/src/bridge.rs:99` and `logic::choose_buffer_size` at
/// `router-engine/src/logic.rs:202`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamRequest {
    pub rate_hz: u32,
    pub buffer_frames: u32,
    pub channels: u16,
}

/// What the output backend actually granted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamSpec {
    pub rate_hz: u32,
    pub buffer_frames: u32,
    pub channels: u16,
}

/// Failure of an [`OutputSink`] operation.
#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("output device unavailable: {0}")]
    Unavailable(String),
    #[error("stream negotiation failed: {0}")]
    Negotiation(String),
    #[error("output start failed: {0}")]
    Start(String),
    #[error("output stop failed: {0}")]
    Stop(String),
}

/// Failure of a [`SystemSource`] operation.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("source unavailable: {0}")]
    Unavailable(String),
    #[error("ring attach failed: {0}")]
    Attach(String),
}

/// How a new config can be taken over (port of `router-engine/src/logic.rs:32`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigChange {
    /// Nothing relevant changed.
    Unchanged,
    /// EQ / bypass / limiter / headroom / safety buffer / resampler quality: live.
    HotApply,
    /// Master driver, sample rate, output channels or the bridge loopback toggle: restart.
    NeedsRestart(&'static str),
}

/// Outcome of one supervision tick (new type; expresses the `engine.rs` control-loop
/// outcomes start / stop / retry / hot-apply, see [`crate::session::Session`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlAction {
    /// Nothing to do.
    None,
    /// Re-publish the live settings into the running stream.
    HotApply,
    /// Tear the stream down and open it again.
    Restart(String),
    /// Retry opening a stream that previously failed (at [`ControlLoop::next_retry`]).
    Retry,
    /// Shut down for good.
    Stop,
}

/// Master output device. RT side = [`OutputCallback::render`], called per period,
/// allocation-free (same rules as `Pipeline::render`, `router-engine/src/pipeline.rs:3-5`).
///
/// Windows impl wraps `asio_host::AsioDriver` + `Handler::buffer_switch`
/// (`asio-host/src/lib.rs:141-146`); the portable RT core IS the callback body
/// (port of `router-engine/src/pipeline.rs` `Pipeline::render:441`, see
/// [`crate::pipeline::PipelineCallback`]).
pub trait OutputSink {
    type Callback: OutputCallback;
    fn negotiate(&mut self, want: &StreamRequest) -> Result<StreamSpec, SinkError>;
    fn start(&mut self, cb: Self::Callback) -> Result<(), SinkError>;
    fn stop(&mut self) -> Result<(), SinkError>;
    fn spec(&self) -> &StreamSpec;
}

/// The render callback installed into an [`OutputSink`]: `out` holds one `f32` slice
/// per negotiated channel, `now_ticks` is the backend clock reading at entry
/// (same tick domain as [`Clock`]).
pub trait OutputCallback: Send + 'static {
    fn render(&mut self, out: &mut [&mut [f32]], now_ticks: i64);
    fn on_xrun(&mut self) {}
}

/// System-audio source: producer side fills the ring; consumer side is any
/// `FrameSource` (`router-core/src/resample.rs:19`) — today `BridgeSource`
/// (`router-engine/src/bridge.rs:136`) or `PipelineSource` (`pipeline.rs:241`);
/// here the ring [`Consumer`](router_core::ring::Consumer) feeding the portable pipeline.
pub trait SystemSource {
    fn negotiated_rate(&self) -> u32;
    fn attach(&mut self, ring: Producer) -> Result<(), SourceError>;
    fn arrival(&mut self) -> &mut ArrivalTracker;
    /// Aggregated arrival statistics of the sliding window.
    ///
    /// A3 sketched the return type as `ArrivalStats` (`router-core/src/arrival.rs:111`),
    /// but `ArrivalStats` is the Arc-shared atomics blob with no by-value constructor;
    /// the by-value snapshot type is `ArrivalSnapshot` (`arrival.rs:123`, produced by
    /// `ArrivalStats::snapshot`). Deviation reported in the Phase A2 handoff.
    fn fill_snapshot(&mut self) -> ArrivalSnapshot;
    fn heartbeat(&self) -> &Heartbeat;
}

/// QPC on Windows (`router-engine/src/pipeline.rs:143/154`);
/// `clock_gettime(CLOCK_MONOTONIC)` on Linux; `mach_absolute_time` on macOS.
/// Everything that consumes it is clock-free today (ticks are passed in).
pub trait Clock: Send + Sync {
    fn now_ticks(&self) -> i64;
    fn ticks_per_second(&self) -> i64;
    fn sleep_until(&self, ticks: i64);
}

/// Status publication. Windows impl = `router-core/src/status_writer.rs:80` (shm
/// `StatusWriter`, `eq-core/src/status.rs` `StatusBlock`); portable impls =
/// [`crate::status::NullTransport`] / [`crate::status::InProcTransport`] (and a
/// Unix-socket or file transport later).
pub trait StatusTransport: Send + Sync {
    fn init(&self, info: &StaticInfo); // status_writer.rs:29,133
    fn set_running(&self, on: bool); // :176
    fn set_xruns(&self, n: u64); // :179
    fn publish_block(&self, r: &BlockReport); // :43,213
    fn set_bridge(&self, r: &BridgeReport); // :60,197
}

/// Supervision: config-diff classification, restart back-off, stall tracking, auto
/// safety. Direct port of `router-engine/src/logic.rs` (`classify_change:32`,
/// `Backoff:71`, `StallTracker:147`, `AutoSafety:274`, `plan_target:190`).
pub trait ControlLoop {
    fn on_config(&mut self, old: &Config, new: &Config, effective_rate: u32) -> ConfigChange;
    fn on_tick(&mut self, now_ticks: i64, live: &Live) -> ControlAction;
    fn next_retry(&self) -> Option<i64>;
}
