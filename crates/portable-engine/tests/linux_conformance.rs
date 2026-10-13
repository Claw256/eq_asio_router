//! Compile-time seam conformance + hardware-free behavioural checks for the B1
//! Linux backends (`backend/linux.rs`, `backend/pipewire.rs`).
//!
//! Gated on `target_os = "linux"` + `feature = "backend-linux"` so the Windows
//! workspace test run compiles this file to nothing and stays green. Nothing here
//! opens a sound device.

#![cfg(all(target_os = "linux", feature = "backend-linux"))]

use portable_engine::backend::linux::{AlsaCallback, AlsaSink};
use portable_engine::backend::pipewire::{ArrivalQueue, PipeWireSource, ReconnectState};
use portable_engine::seams::{OutputCallback, OutputSink, StreamSpec, SystemSource};

use router_core::arrival::{ArrivalStats, ArrivalTracker};
use router_core::ring;

fn _assert_sink<T: OutputSink>() {}
fn _assert_callback<T: OutputCallback>() {}
fn _assert_source<T: SystemSource>() {}

struct NopCallback;

impl OutputCallback for NopCallback {
    fn render(&mut self, _out: &mut [&mut [f32]], _now_ticks: i64) {}
}

#[test]
fn seam_conformance_is_static() {
    // Compile-time: the concrete types satisfy the frozen seams exactly.
    _assert_sink::<AlsaSink>();
    _assert_callback::<AlsaCallback>();
    _assert_source::<PipeWireSource>();
}

#[test]
fn alsa_sink_constructs_without_hardware() {
    let sink = AlsaSink::new("default");
    // Not negotiated yet: default spec, not running, zero xruns.
    assert_eq!(sink.spec(), &StreamSpec::default());
    assert!(!sink.is_running());
    assert_eq!(sink.xrun_count(), 0);
    assert_eq!(sink.period(), (portable_engine::backend::linux::DEFAULT_PERIOD_FRAMES, 3));
}

#[test]
fn alsa_callback_delegates() {
    let mut cb = AlsaCallback::new(NopCallback);
    cb.render(&mut [], 0);
    cb.on_xrun(); // default hook is a no-op
}

#[test]
fn pipewire_source_constructs_without_hardware() {
    let mut src = PipeWireSource::new(48_000);
    assert_eq!(src.negotiated_rate(), 48_000);
    assert_eq!(src.reconnect_state(), ReconnectState::Disconnected);
    assert_eq!(src.heartbeat().capture_count(), 0);
    assert!(src.last_error().is_none());
    assert_eq!(src.captured_chunks(), 0);
    // Snapshot on a fresh source is well-formed zeros.
    let snap = src.fill_snapshot();
    assert_eq!(snap.rate, 48_000.0);
    assert_eq!(snap.events, 0);
}

#[test]
fn pipewire_source_zero_rate_defaults_to_48k() {
    let src = PipeWireSource::new(0);
    assert_eq!(src.negotiated_rate(), 48_000);
}

#[test]
fn arrival_queue_replays_events_in_order() {
    let q = ArrivalQueue::new(8);
    let stats = ArrivalStats::new(48_000.0);
    let mut tracker = ArrivalTracker::new(stats.clone());

    q.push(1.0, 256);
    q.push(1.005, 256);
    q.push(1.010, 256);
    assert_eq!(q.pending(), 3);
    assert_eq!(q.dropped(), 0);

    q.drain_into(&mut tracker);
    assert_eq!(q.pending(), 0);

    let snap = stats.snapshot();
    assert!(snap.events >= 1, "events: {}", snap.events);
    assert_eq!(snap.rate, 48_000.0);
}

#[test]
fn arrival_queue_drops_and_counts_on_overflow() {
    let q = ArrivalQueue::new(16);
    let stats = ArrivalStats::new(48_000.0);
    let mut tracker = ArrivalTracker::new(stats);

    for i in 0..32u32 {
        q.push(f64::from(i) * 0.01, 128);
    }
    assert_eq!(q.pending(), 16);
    assert_eq!(q.dropped(), 16);

    q.drain_into(&mut tracker);
    assert_eq!(q.pending(), 0);
}

#[test]
fn ring_attach_is_reported_and_forgotten_cleanly() {
    // `attach` with no Pulse server available must fail gracefully (Unavailable),
    // never panic or block: on a build host without libpulse's daemon this is the
    // expected path; with a daemon present attach succeeds and `Drop` cleans up.
    let mut src = PipeWireSource::new(48_000);
    let (prod, _cons) = ring::channel(4096);
    match src.attach(prod) {
        Ok(()) => {
            assert_eq!(src.reconnect_state(), ReconnectState::Connected);
        }
        Err(e) => {
            let msg = e.to_string();
            assert!(!msg.is_empty(), "error must carry context");
            assert!(src.last_error().is_some(), "failure must be recorded");
        }
    }
}
