//! Hardware-free unit tests for the pure policy/logic helpers of the B1 Linux
//! backends: period math, format negotiation, interleaving, and the xrun /
//! reconnect state machines. Gated to Linux + `backend-linux` so the Windows
//! workspace test run compiles to nothing.

#![cfg(all(target_os = "linux", feature = "backend-linux"))]

use portable_engine::backend::linux::{
    device_candidates as alsa_device_candidates, f32_to_s32, negotiate_period, pick_format, plan_buffer,
    planar_to_interleaved_f32, planar_to_interleaved_i32, SampleFormat, XrunState, DEFAULT_PERIOD_FRAMES,
    MAX_PERIOD_FRAMES, MIN_PERIOD_FRAMES, PERIOD_COUNT, TICKS_PER_SECOND,
};
use portable_engine::backend::linux::monotonic_ns;
use portable_engine::backend::pipewire::{
    chunk_frames, deinterleave_f32, device_candidates as pw_device_candidates, float32le_spec, fragsize_bytes,
    monotonic_seconds, PaSampleSpec, ReconnectState, PA_SAMPLE_FLOAT32LE, PA_STREAM_RECORD,
};

// ------------------------------------------------------------------------------------
// Period policy (linux.rs)
// ------------------------------------------------------------------------------------

#[test]
fn negotiate_period_defaults_and_clamps() {
    assert_eq!(negotiate_period(0), (DEFAULT_PERIOD_FRAMES, PERIOD_COUNT));
    assert_eq!(negotiate_period(128), (128, PERIOD_COUNT));
    assert_eq!(negotiate_period(256), (256, PERIOD_COUNT));
    assert_eq!(negotiate_period(3), (MIN_PERIOD_FRAMES, PERIOD_COUNT));
    assert_eq!(negotiate_period(1_000_000), (MAX_PERIOD_FRAMES, PERIOD_COUNT));
    assert_eq!(PERIOD_COUNT, 3, "documented default: 3 periods per buffer");
}

#[test]
fn plan_buffer_multiplies_periods() {
    assert_eq!(plan_buffer(256, 3), 768);
    assert_eq!(plan_buffer(128, 3), 384);
    assert_eq!(plan_buffer(0, 3), 0);
    assert_eq!(plan_buffer(u32::MAX, 3), u32::MAX); // saturates instead of overflowing
}

// ------------------------------------------------------------------------------------
// Format negotiation (linux.rs)
// ------------------------------------------------------------------------------------

#[test]
fn pick_format_prefers_float_le_then_s32_le() {
    assert_eq!(pick_format(true), SampleFormat::FloatLe);
    assert_eq!(pick_format(false), SampleFormat::S32Le);
}

#[test]
fn format_alsa_codes_and_sizes() {
    assert_eq!(SampleFormat::FloatLe.alsa_code(), 14); // SND_PCM_FORMAT_FLOAT_LE
    assert_eq!(SampleFormat::S32Le.alsa_code(), 10); // SND_PCM_FORMAT_S32_LE
    assert_eq!(SampleFormat::FloatLe.sample_bytes(), 4);
    assert_eq!(SampleFormat::S32Le.sample_bytes(), 4);
}

#[test]
fn alsa_device_chain_is_ordered_and_deduped() {
    assert_eq!(
        alsa_device_candidates("hw:1,0"),
        vec!["hw:1,0".to_string(), "default".to_string(), "plughw:0,0".to_string()]
    );
    assert_eq!(
        alsa_device_candidates(""),
        vec!["default".to_string(), "plughw:0,0".to_string()]
    );
    // Configured name that collides with a fallback stays first, no duplicate.
    assert_eq!(
        alsa_device_candidates("default"),
        vec!["default".to_string(), "plughw:0,0".to_string()]
    );
}

// ------------------------------------------------------------------------------------
// Interleaved conversion at the write boundary (linux.rs)
// ------------------------------------------------------------------------------------

#[test]
fn f32_to_s32_scales_and_clamps() {
    assert_eq!(f32_to_s32(0.0), 0);
    assert_eq!(f32_to_s32(1.0), 2_147_483_647);
    assert_eq!(f32_to_s32(-1.0), -2_147_483_647);
    assert_eq!(f32_to_s32(2.0), 2_147_483_647); // clamped
    assert_eq!(f32_to_s32(-2.0), -2_147_483_647); // clamped
    assert_eq!(f32_to_s32(f32::NAN), 0); // non-finite -> silence
    assert_eq!(f32_to_s32(0.5), 1_073_741_823); // 0.5 * 2147483647
}

#[test]
fn planar_to_interleaved_i32_frames_channels() {
    let l = [0.0f32, 1.0, -1.0];
    let r = [0.5f32, -0.5, 0.0];
    let mut out = [0i32; 6];
    planar_to_interleaved_i32(&[&l, &r], &mut out);
    assert_eq!(out[0], 0);
    assert_eq!(out[1], 1_073_741_823);
    assert_eq!(out[2], 2_147_483_647);
    assert_eq!(out[3], -1_073_741_823);
    assert_eq!(out[4], -2_147_483_647);
    assert_eq!(out[5], 0);
}

#[test]
fn planar_to_interleaved_f32_frames_channels() {
    let l = [0.25f32, -0.75];
    let r = [0.5f32, 1.0];
    let mut out = [0.0f32; 4];
    planar_to_interleaved_f32(&[&l, &r], &mut out);
    assert_eq!(out, [0.25, 0.5, -0.75, 1.0]);
}

#[test]
fn planar_to_interleaved_handles_short_rows_and_no_channels() {
    let l = [1.0f32]; // shorter than dst implies silence padding
    let mut out = [7i32; 4];
    planar_to_interleaved_i32(&[&l, &l], &mut out);
    assert_eq!(out, [2_147_483_647, 2_147_483_647, 0, 0]);

    let mut out2 = [9i32; 2];
    planar_to_interleaved_i32(&[], &mut out2); // no channels: no-op
    assert_eq!(out2, [9, 9]);
}

// ------------------------------------------------------------------------------------
// Xrun state machine (linux.rs)
// ------------------------------------------------------------------------------------

#[test]
fn xrun_state_machine_recovers_and_dies() {
    assert_eq!(XrunState::Running.on_xrun(), XrunState::Recovering);
    assert_eq!(XrunState::Recovering.on_recovered(), XrunState::Running);
    assert_eq!(XrunState::Running.on_recovered(), XrunState::Running);
    assert_eq!(XrunState::Recovering.on_fatal(), XrunState::Dead);
    assert!(XrunState::Dead.is_dead());
    assert!(XrunState::Dead.on_recovered().is_dead(), "Dead is terminal");
    assert!(XrunState::Dead.on_xrun().is_dead(), "Dead is terminal");
}

// ------------------------------------------------------------------------------------
// Reconnect state machine (pipewire.rs)
// ------------------------------------------------------------------------------------

#[test]
fn reconnect_state_machine_allows_one_reconnect() {
    use ReconnectState::*;
    assert_eq!(Disconnected.on_connected(), Connected);
    assert_eq!(Connected.on_read_error(), Reconnecting);
    assert_eq!(Reconnecting.on_reconnected(), ReconnectedOnce);
    // Budget spent: the next device change is terminal.
    assert_eq!(ReconnectedOnce.on_read_error(), Failed);
    // Or the reconnect attempt itself fails.
    assert_eq!(Reconnecting.on_reconnect_failed(), Failed);
    assert!(Failed.is_failed());
    assert!(Failed.on_connected().is_failed(), "Failed is terminal");
    assert!(!Connected.is_failed());
}

// ------------------------------------------------------------------------------------
// Monitor source helpers (pipewire.rs)
// ------------------------------------------------------------------------------------

#[test]
fn pw_device_chain_is_ordered_and_deduped() {
    assert_eq!(
        pw_device_candidates(None),
        vec!["@DEFAULT_SINK@.monitor".to_string(), "default".to_string()]
    );
    assert_eq!(
        pw_device_candidates(Some("alsa_output.pci-0000.monitor")),
        vec![
            "alsa_output.pci-0000.monitor".to_string(),
            "@DEFAULT_SINK@.monitor".to_string(),
            "default".to_string()
        ]
    );
    assert_eq!(
        pw_device_candidates(Some("default")),
        vec!["default".to_string(), "@DEFAULT_SINK@.monitor".to_string()]
    );
    assert_eq!(pw_device_candidates(Some("")), pw_device_candidates(None));
}

#[test]
fn capture_chunk_and_frag_sizes_follow_the_10ms_policy() {
    assert_eq!(chunk_frames(48_000), 480);
    assert_eq!(chunk_frames(44_100), 441);
    assert_eq!(chunk_frames(0), 480);
    assert_eq!(chunk_frames(1), 64, "clamped to a sane minimum");
    assert_eq!(fragsize_bytes(48_000, 2), 480 * 2 * 4);
}

#[test]
fn float32le_spec_is_pulse_shaped() {
    let s: PaSampleSpec = float32le_spec(48_000, 2);
    assert_eq!(s.format, PA_SAMPLE_FLOAT32LE);
    assert_eq!(s.rate, 48_000);
    assert_eq!(s.channels, 2);
    assert_eq!(PA_STREAM_RECORD, 1);
    // Zero rate falls back to 48 kHz.
    assert_eq!(float32le_spec(0, 2).rate, 48_000);
}

#[test]
fn deinterleave_splits_stereo_pairs() {
    let interleaved = [0.1f32, -0.1, 0.2, -0.2, 0.3, -0.3];
    let mut l = [0.0f32; 3];
    let mut r = [0.0f32; 3];
    deinterleave_f32(&interleaved, &mut l, &mut r);
    assert_eq!(l, [0.1, 0.2, 0.3]);
    assert_eq!(r, [-0.1, -0.2, -0.3]);

    // Short output is respected.
    let mut l2 = [0.0f32; 1];
    let mut r2 = [0.0f32; 1];
    deinterleave_f32(&interleaved, &mut l2, &mut r2);
    assert_eq!(l2, [0.1]);
    assert_eq!(r2, [-0.1]);
}

// ------------------------------------------------------------------------------------
// Clock helper
// ------------------------------------------------------------------------------------

#[test]
fn monotonic_clocks_are_ns_and_seconds() {
    let a = monotonic_ns();
    let b = monotonic_ns();
    assert!(b >= a, "CLOCK_MONOTONIC must not go backwards");
    assert!(a > 0);
    assert_eq!(TICKS_PER_SECOND, 1_000_000_000);

    let s1 = monotonic_seconds();
    let s2 = monotonic_seconds();
    assert!(s2 >= s1);
    assert!(s1 > 0.0);
}
