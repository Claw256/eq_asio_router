//! Per-OS backend dispatch (cfg only — no logic here; A6 "frozen").
//!
//! Module activation (A6 features `backend-linux` / `backend-macos` /
//! `backend-windows` "for CI slicing"): a backend module compiles only when its
//! feature is ON **and** the target OS matches. The features are off by default, so
//! the B1/B2-owned files below can be absent during Phase A and every gate still
//! passes; B1/B2 flip their feature when they land their files.
//!
//! File ownership (A6): `linux.rs` + `pipewire.rs` = B1 · `macos.rs` +
//! `macos_tap.rs` = B2 · `windows.rs` = Phase C (stub in Phase A).

#[cfg(all(target_os = "linux", feature = "backend-linux"))]
pub mod linux; // B1 OWNS (Phase B)

#[cfg(all(target_os = "linux", feature = "backend-linux"))]
pub mod pipewire; // B1 OWNS (Phase B)

#[cfg(all(target_os = "macos", feature = "backend-macos"))]
pub mod macos; // B2 OWNS (Phase B)

#[cfg(all(target_os = "macos", feature = "backend-macos"))]
pub mod macos_tap; // B2 OWNS (Phase B)

#[cfg(all(target_os = "windows", feature = "backend-windows"))]
pub mod windows;

/// Name of the backend compiled in for the current target/feature combination
/// (diagnostics only; "none" until a backend feature is enabled).
pub const BACKEND_NAME: &str = {
    #[cfg(all(target_os = "windows", feature = "backend-windows"))]
    {
        "windows (asio-host stub)"
    }
    #[cfg(all(target_os = "linux", feature = "backend-linux"))]
    {
        "linux (alsa)"
    }
    #[cfg(all(target_os = "macos", feature = "backend-macos"))]
    {
        "macos (core audio)"
    }
    #[cfg(not(any(
        all(target_os = "windows", feature = "backend-windows"),
        all(target_os = "linux", feature = "backend-linux"),
        all(target_os = "macos", feature = "backend-macos"),
    )))]
    {
        "none (backend feature off)"
    }
};
