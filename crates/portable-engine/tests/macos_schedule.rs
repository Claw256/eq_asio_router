//! IO scheduling math tests (hardware-free) for the macOS backend: frames ↔ ticks
//! conversions in the `mach_absolute_time` tick domain ([`MachClock`]).
//!
//! Compiles only for macOS + `backend-macos` (A6 feature gating): on the Windows
//! workspace gates this file is cfg'd out entirely, keeping suite counts green.
//! Pure logic only — no live Core Audio calls (A7: hardware is a Phase C check).

#![cfg(all(target_os = "macos", feature = "backend-macos"))]

use portable_engine::backend::macos::{
    frames_to_ticks, period_seconds, ticks_per_second_from_timebase, ticks_to_seconds, MachClock,
};
use portable_engine::seams::Clock;

#[test]
fn period_duration_matches_frames_over_rate() {
    assert_eq!(period_seconds(480, 48_000), 0.01);
    assert_eq!(period_seconds(512, 48_000), 512.0 / 48_000.0);
    assert_eq!(period_seconds(256, 44_100), 256.0 / 44_100.0);
    assert_eq!(period_seconds(480, 0), 0.0); // degenerate rate fails closed
}

#[test]
fn frames_to_ticks_is_exact_for_clean_divisors() {
    // 480 frames @ 48 kHz = 10 ms; at 24 MHz ticks (Apple Silicon timebase) = 240_000.
    assert_eq!(frames_to_ticks(480, 48_000, 24_000_000), 240_000);
    // 441 frames @ 44.1 kHz = 10 ms as well.
    assert_eq!(frames_to_ticks(441, 44_100, 24_000_000), 240_000);
    // One frame @ 48 kHz at 1 GHz ticks.
    assert_eq!(frames_to_ticks(1, 48_000, 1_000_000_000), 20_833); // truncating division
    assert_eq!(frames_to_ticks(480, 0, 1_000_000_000), 0); // degenerate rate
}

#[test]
fn ticks_to_seconds_inverts_frames_to_ticks() {
    let tps = 24_000_000i64;
    let ticks = frames_to_ticks(512, 48_000, tps);
    let secs = ticks_to_seconds(ticks, tps);
    assert!((secs - 512.0 / 48_000.0).abs() < 1e-9);
    assert_eq!(ticks_to_seconds(0, tps), 0.0);
    assert_eq!(ticks_to_seconds(100, 0), 0.0); // degenerate tps
}

#[test]
fn timebase_ratio_gives_ticks_per_second() {
    // Apple Silicon: numer=125, denom=3 → 24 MHz.
    assert_eq!(ticks_per_second_from_timebase(125, 3), 24_000_000);
    // Intel: numer=1, denom=1 → 1 GHz.
    assert_eq!(ticks_per_second_from_timebase(1, 1), 1_000_000_000);
    assert_eq!(ticks_per_second_from_timebase(0, 3), 0); // degenerate timebase
}

#[test]
fn mach_clock_reads_the_mach_tick_domain() {
    let clock = MachClock::new();
    assert!(clock.ticks_per_second() > 0);
    let t0 = clock.now_ticks();
    let t1 = clock.now_ticks();
    assert!(t1 >= t0, "mach_absolute_time must be monotonic ({t1} < {t0})");
}
