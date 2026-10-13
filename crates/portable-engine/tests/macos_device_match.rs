//! Device UID/name matching tests (hardware-free) for the macOS backend.
//!
//! Compiles only for macOS + `backend-macos` (A6 feature gating): on the Windows
//! workspace gates this file is cfg'd out entirely, keeping suite counts green.
//! Pure logic only — no live Core Audio calls (A7: hardware is a Phase C check).

#![cfg(all(target_os = "macos", feature = "backend-macos"))]

use portable_engine::backend::macos::{
    match_rank, pick_output_index, pick_tap_index, DeviceInfo, MatchRank,
};

fn dev(id: u32, uid: &str, name: &str, input: u32, output: u32) -> DeviceInfo {
    DeviceInfo {
        id,
        uid: uid.to_string(),
        name: name.to_string(),
        input_channels: input,
        output_channels: output,
    }
}

fn fixture() -> Vec<DeviceInfo> {
    vec![
        dev(10, "BuiltInSpeakerDevice", "MacBook Pro Speakers", 0, 2),
        dev(20, "AppleUSBAudioEngine:UR22C", "Steinberg UR22C", 2, 2),
        dev(30, "BlackHole2ch_UID", "BlackHole 2ch", 2, 2),
        dev(40, "BlackHole16ch_UID", "BlackHole 16ch", 16, 16),
    ]
}

#[test]
fn uid_exact_beats_name_exact_beats_substring() {
    assert_eq!(
        match_rank("BlackHole2ch_UID", "BlackHole 2ch", Some("BlackHole2ch_UID"), Some("BlackHole")),
        MatchRank::UidExact
    );
    assert_eq!(
        match_rank("BlackHole2ch_UID", "BlackHole 2ch", Some("other"), Some("blackhole 2ch")),
        MatchRank::NameExact
    );
    assert_eq!(
        match_rank("BlackHole2ch_UID", "BlackHole 2ch", Some("other"), Some("BlackHole")),
        MatchRank::NameContains
    );
    assert_eq!(match_rank("x", "y", None, None), MatchRank::None);
    assert_eq!(match_rank("x", "y", Some("z"), Some("q")), MatchRank::None);
}

#[test]
fn name_matching_is_case_insensitive_uid_is_not() {
    assert_eq!(match_rank("UID_A", "BlackHole 2ch", None, Some("BLACKHOLE 2CH")), MatchRank::NameExact);
    assert_eq!(match_rank("UID_A", "BlackHole 2ch", None, Some("blackhole")), MatchRank::NameContains);
    assert_eq!(match_rank("uid_a", "x", Some("UID_A"), None), MatchRank::None);
}

#[test]
fn empty_wanted_name_is_not_a_substring_match() {
    assert_eq!(match_rank("a", "BlackHole 2ch", None, Some("")), MatchRank::None);
}

#[test]
fn output_pick_prefers_uid_then_name_then_default() {
    let devs = fixture();
    // UID wins even when the name also matches another device.
    assert_eq!(
        pick_output_index(&devs, Some("AppleUSBAudioEngine:UR22C"), Some("BlackHole"), None),
        Some(1)
    );
    // Exact name beats substring ("BlackHole 2ch" exact vs "BlackHole 16ch" contains).
    assert_eq!(pick_output_index(&devs, None, Some("BlackHole 2ch"), None), Some(2));
    // Substring match over the first enumerated device containing it.
    assert_eq!(pick_output_index(&devs, None, Some("BlackHole"), None), Some(2));
    // Default output fallback when nothing matches.
    assert_eq!(
        pick_output_index(&devs, Some("missing"), None, Some("BuiltInSpeakerDevice")),
        Some(0)
    );
    // Unknown selector and unknown default → no device.
    assert_eq!(pick_output_index(&devs, Some("missing"), None, Some("nope")), None);
}

#[test]
fn output_pick_requires_output_channels() {
    let devs = vec![dev(1, "in_only", "Input Only", 2, 0)];
    assert_eq!(pick_output_index(&devs, None, Some("Input Only"), None), None);
}

#[test]
fn tap_pick_finds_blackhole_variants() {
    let devs = fixture();
    // Empty selector → BlackHole substring rule finds the 2ch unit first.
    assert_eq!(pick_tap_index(&devs, None, None, None, None), Some(2));
    // Explicit UID is highest priority.
    assert_eq!(
        pick_tap_index(&devs, Some("BlackHole16ch_UID"), None, None, None),
        Some(3)
    );
    // Explicit exact name.
    assert_eq!(pick_tap_index(&devs, None, Some("BlackHole 16ch"), None, None), Some(3));
}

#[test]
fn tap_pick_falls_back_to_default_input_unless_it_is_the_default_output() {
    // No BlackHole at all: fall back to the default input (UR22C mic).
    let no_loopback = vec![
        dev(10, "BuiltInSpeakerDevice", "MacBook Pro Speakers", 0, 2),
        dev(20, "AppleUSBAudioEngine:UR22C", "Steinberg UR22C", 2, 2),
    ];
    assert_eq!(
        pick_tap_index(&no_loopback, None, None, Some("AppleUSBAudioEngine:UR22C"), Some("BuiltInSpeakerDevice")),
        Some(1)
    );
    // ...but never when the default input IS the default output (self-capture).
    assert_eq!(
        pick_tap_index(&no_loopback, None, None, Some("AppleUSBAudioEngine:UR22C"), Some("AppleUSBAudioEngine:UR22C")),
        None
    );
    // Fallback must be an input-capable device.
    assert_eq!(
        pick_tap_index(&no_loopback, None, None, Some("BuiltInSpeakerDevice"), Some("AppleUSBAudioEngine:UR22C")),
        None
    );
    // Explicit selector miss must NOT fall through to a wrong device when no
    // BlackHole exists and the default input is the default output.
    assert_eq!(
        pick_tap_index(&no_loopback, Some("missing"), None, Some("AppleUSBAudioEngine:UR22C"), Some("AppleUSBAudioEngine:UR22C")),
        None
    );
}

#[test]
fn tap_pick_skips_output_only_devices() {
    let devs = vec![dev(1, "spk", "BlackHole Speakers", 0, 2)];
    assert_eq!(pick_tap_index(&devs, None, None, None, None), None);
}
