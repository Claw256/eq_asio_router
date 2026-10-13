//! Buffer/rate negotiation state-machine tests + seam conformance (hardware-free)
//! for the macOS backend.
//!
//! Compiles only for macOS + `backend-macos` (A6 feature gating): on the Windows
//! workspace gates this file is cfg'd out entirely, keeping suite counts green.
//! Pure logic only — no live Core Audio calls (A7: hardware is a Phase C check).

#![cfg(all(target_os = "macos", feature = "backend-macos"))]

use portable_engine::backend::macos::{
    clamp_buffer_frames, negotiation_step, rate_supported, NegotiationEvent, NegotiationState,
};
use portable_engine::backend::macos::{MacCallback, MacSink};
use portable_engine::backend::macos_tap::MacTap;
use portable_engine::seams::{OutputCallback, OutputSink, StreamRequest, StreamSpec, SystemSource};

// ---- seam conformance (compile-time) ----------------------------------------------

fn _assert_output_sink<T: OutputSink>() {}
fn _assert_output_callback<T: OutputCallback>() {}
fn _assert_system_source<T: SystemSource>() {}

#[test]
fn macos_types_implement_the_seams() {
    _assert_output_sink::<MacSink>();
    _assert_output_callback::<MacCallback>();
    _assert_system_source::<MacTap>();
    // The associated callback type of the sink is exactly MacCallback.
    fn callback_type_is_mac_callback<T: OutputSink<Callback = MacCallback>>() {}
    callback_type_is_mac_callback::<MacSink>();
}

// ---- buffer-size negotiation ------------------------------------------------------

#[test]
fn buffer_frames_clamp_to_device_range() {
    assert_eq!(clamp_buffer_frames(512, 64, 4096), 512);
    assert_eq!(clamp_buffer_frames(16, 64, 4096), 64); // below range → floor
    assert_eq!(clamp_buffer_frames(8192, 64, 4096), 4096); // above range → cap
    assert_eq!(clamp_buffer_frames(64, 64, 64), 64); // exact bounds ok
    assert_eq!(clamp_buffer_frames(100, 256, 256), 256); // degenerate range
    assert_eq!(clamp_buffer_frames(100, 4096, 64), 0); // unusable range → 0 (fail closed)
}

#[test]
fn rate_supported_checks_advertised_ranges() {
    let ranges = [(44_100.0, 44_100.0), (48_000.0, 48_000.0), (88_200.0, 192_000.0)];
    assert!(rate_supported(44_100, &ranges));
    assert!(rate_supported(48_000, &ranges));
    assert!(rate_supported(96_000, &ranges));
    assert!(rate_supported(192_000, &ranges));
    assert!(!rate_supported(22_050, &ranges));
    assert!(!rate_supported(96_001, &ranges));
    // No published restriction → accept.
    assert!(rate_supported(48_000, &[]));
}

// ---- negotiation state machine ----------------------------------------------------

#[test]
fn negotiation_happy_path_confirms() {
    use NegotiationEvent as E;
    use NegotiationState as S;
    let mut s = S::Init;
    s = negotiation_step(s, E::RateOk);
    assert_eq!(s, S::RateApplied);
    s = negotiation_step(s, E::BufferOk);
    assert_eq!(s, S::BufferApplied);
    s = negotiation_step(s, E::ReadBackMatches);
    assert_eq!(s, S::Confirmed);
}

#[test]
fn negotiation_rejections_fail_at_each_step() {
    use NegotiationEvent as E;
    use NegotiationState as S;
    assert_eq!(negotiation_step(S::Init, E::RateRejected), S::Failed);
    assert_eq!(negotiation_step(S::RateApplied, E::BufferRejected), S::Failed);
    assert_eq!(negotiation_step(S::BufferApplied, E::ReadBackMismatch), S::Failed);
}

#[test]
fn negotiation_fails_closed_on_out_of_order_events() {
    use NegotiationEvent as E;
    use NegotiationState as S;
    // Skipping steps (e.g. buffer before rate) must fail, not proceed.
    assert_eq!(negotiation_step(S::Init, E::BufferOk), S::Failed);
    assert_eq!(negotiation_step(S::Init, E::ReadBackMatches), S::Failed);
    assert_eq!(negotiation_step(S::RateApplied, E::RateOk), S::Failed);
    assert_eq!(negotiation_step(S::RateApplied, E::ReadBackMatches), S::Failed);
    assert_eq!(negotiation_step(S::BufferApplied, E::BufferOk), S::Failed);
    // Late events after terminal states stay failed.
    assert_eq!(negotiation_step(S::Confirmed, E::RateOk), S::Failed);
    assert_eq!(negotiation_step(S::Failed, E::ReadBackMatches), S::Failed);
}

// ---- stream shape plumbing --------------------------------------------------------

#[test]
fn stream_request_and_spec_round_trip() {
    let want = StreamRequest { rate_hz: 48_000, buffer_frames: 512, channels: 2 };
    let spec = StreamSpec { rate_hz: want.rate_hz, buffer_frames: want.buffer_frames, channels: want.channels };
    assert_eq!(spec.rate_hz, 48_000);
    assert_eq!(spec.buffer_frames, 512);
    assert_eq!(spec.channels, 2);
}
