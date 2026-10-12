//! `bridge-abi` — the shared-memory contract between the EQ ASIO Router v2
//! bridge driver (`eq-asio-driver`, loaded by an ASIO host such as VBMatrix)
//! and the router app process (`router-engine`, M2+).
//!
//! # Layout
//!
//! One named, session-local file mapping:
//! `Local\eq_asio_router_bridge_v2` ([`MAPPING_NAME`]), fixed size
//! [`TOTAL_SIZE`] bytes, identical for 32-bit and 64-bit processes (all
//! fixed-size types, no `usize`). The image is:
//!
//! ```text
//! [0, 4096)              Header   (#[repr(C, align(64))], POD with atomics)
//! [4096, 135168)         ring A   interleaved stereo f32, RING_FRAMES frames
//! [135168, 266240)       ring B   interleaved stereo f32, RING_FRAMES frames
//! ```
//!
//! * **Ring A (source):** driver → app. The driver pushes the host's ASIO
//!   output pair (channels 0/1) each buffer-switch; the app pops.
//! * **Ring B (loopback):** app → driver. The app pushes the post-EQ stereo
//!   feed; the driver pops it into the ASIO input pair (channels 0/1).
//!
//! Both rings are cross-process SPSC with `u64` monotonic (wrapping) indices.
//! Ordering is `Release` stores of the indices and `Acquire` loads —
//! lock-prefixed operations are cache-coherent across processes on the same
//! machine, so **push/pop take no locks and make no OS calls** (atomics only).
//! Semantics mirror `router-core::ring`: the producer never overwrites unread
//! data (frames that do not fit are dropped new and counted); the consumer may
//! discard the oldest frames ([`Header::ring_a_discard`]) when the fill
//! exceeds its target.
//!
//! Counter semantics (all four count **frames**, single writer each):
//!
//! | Counter | Written by | Meaning |
//! |---|---|---|
//! | `a_overruns`  | driver | frames dropped by [`Header::ring_a_push`] (ring A full) |
//! | `a_discarded` | app    | frames dropped by [`Header::ring_a_discard`] (consumer latency trim) |
//! | `b_dropped`   | app    | frames dropped by [`Header::ring_b_push`] (ring B full) |
//! | `b_underruns` | driver | input frames that [`Header::ring_b_pop`] could not provide (fed as silence) |
//!
//! # Ownership
//!
//! The mapping is created by the first comer (driver *or* app) — page-file
//! backed, zero-filled. `magic` is stored **last** by the creator, so a reader
//! that catches a half-initialised header fails [`Header::validate`] and can
//! retry. The mapping object stays alive while any process holds a handle;
//! indices and counters persist across driver/app attach and detach.
//!
//! # Example (driver side)
//!
//! ```no_run
//! use bridge_abi::{open_or_create, Header};
//!
//! let mapping = open_or_create().expect("mapping");
//! let header = unsafe { mapping.header() };
//! header.validate().expect("magic/version");
//! let l = [0.5f32; 256];
//! let r = [0.5f32; 256];
//! let pushed = unsafe { header.ring_a_push(&l, &r) };
//! assert_eq!(pushed, 256);
//! ```

mod layout;
mod mapping;

pub use layout::{
    ValidateError, CHANNELS, HEADER_SIZE, MAGIC, MAPPING_NAME, RING_BYTES, RING_FRAMES, RING_MASK, TOTAL_SIZE,
    VERSION,
};
pub use layout::{Header, Layout};
pub use mapping::Mapping;

/// Opens the fixed-name mapping if it already exists (app side).
///
/// Returns `None` if no mapping exists yet, it cannot be mapped, or it fails
/// [`Header::validate`] (foreign/corrupt image).
pub fn open() -> Option<Mapping> {
    mapping::open_named(MAPPING_NAME)
}

/// Creates the fixed-name mapping or opens the existing one (either side).
///
/// Header initialisation happens only when *this* call actually created the
/// object (detected via `ERROR_ALREADY_EXISTS`); an existing image is
/// validated but never re-initialised, so ring indices persist.
pub fn open_or_create() -> Option<Mapping> {
    mapping::open_or_create_named(MAPPING_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// Set in the child process only: which role it plays.
    const ROLE_ENV: &str = "EQ_BRIDGE_ABI_CHILD_ROLE";
    /// Set in the child process only: the mapping name the parent created.
    const NAME_ENV: &str = "EQ_BRIDGE_ABI_CHILD_NAME";

    fn unique_name(tag: &str) -> String {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!(
            "Local\\eq_asio_router_bridge_v2_xproc_{}_{}_{}_{}",
            tag,
            std::process::id(),
            t,
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn role() -> Option<String> {
        std::env::var(ROLE_ENV).ok()
    }

    /// Spawns this test binary (`current_exe`) running exactly one child test.
    fn run_child(child_test: &str, role: &str, name: &str) -> bool {
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(_) => return false,
        };
        let mut child = match Command::new(exe)
            .args([child_test, "--exact", "--nocapture"])
            .env(ROLE_ENV, role)
            .env(NAME_ENV, name)
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return false,
        };
        // Bounded wait so a wedged child fails the test instead of hanging it.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) => {
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => return false,
            }
        }
    }

    fn pattern(n: usize) -> (Vec<f32>, Vec<f32>) {
        let l = (0..n).map(|i| 0.001 * i as f32 + 0.25).collect();
        let r = (0..n).map(|i| -(0.002 * i as f32) - 0.5).collect();
        (l, r)
    }

    /// Child role: push `RING_FRAMES/2` patterned frames into ring A.
    #[test]
    fn child_push() {
        if role().as_deref() != Some("push") {
            return;
        }
        let name = std::env::var(NAME_ENV).expect("name env");
        let m = mapping::open_named(&name).expect("child open");
        let h = unsafe { m.header() };
        assert_eq!(h.validate(), Ok(()));
        let n = RING_FRAMES / 2;
        let (l, r) = pattern(n);
        let k = unsafe { h.ring_a_push(&l, &r) };
        assert_eq!(k, n, "child push must store every frame");
        assert_eq!(h.fill_a() as usize, n);
    }

    /// Child role: pop what the parent pushed into ring B and verify it.
    #[test]
    fn child_pop() {
        if role().as_deref() != Some("pop") {
            return;
        }
        let name = std::env::var(NAME_ENV).expect("name env");
        let m = mapping::open_named(&name).expect("child open");
        let h = unsafe { m.header() };
        assert_eq!(h.validate(), Ok(()));
        let n = RING_FRAMES / 3;
        assert_eq!(h.fill_b() as usize, n, "parent must have pushed first");
        let (want_l, want_r) = pattern(n);
        let mut ol = vec![0.0f32; n];
        let mut orr = vec![0.0f32; n];
        let k = unsafe { h.ring_b_pop(&mut ol, &mut orr) };
        assert_eq!(k, n);
        assert_eq!(ol, want_l);
        assert_eq!(orr, want_r);
        assert_eq!(h.fill_b(), 0);
        assert_eq!(h.b_underruns(), 0);
    }

    /// Parent side: a *different process* pushes ring A, this process pops it.
    #[test]
    fn cross_process_child_pushes_parent_pops() {
        if role().is_some() {
            return;
        }
        let name = unique_name("a");
        let m = mapping::open_or_create_named(&name).expect("parent create");
        assert!(m.created());
        let n = RING_FRAMES / 2;
        assert!(run_child("tests::child_push", "push", &name), "child push process failed");
        let h = unsafe { m.header() };
        assert_eq!(h.fill_a() as usize, n, "frames pushed by the child must be visible");
        let (want_l, want_r) = pattern(n);
        let mut ol = vec![0.0f32; n];
        let mut orr = vec![0.0f32; n];
        let k = unsafe { h.ring_a_pop(&mut ol, &mut orr) };
        assert_eq!(k, n);
        assert_eq!(ol, want_l);
        assert_eq!(orr, want_r);
        assert_eq!(h.fill_a(), 0);
        assert_eq!(h.a_overruns(), 0);
    }

    /// Parent side: this process pushes ring B, a *different process* pops it.
    #[test]
    fn cross_process_parent_pushes_child_pops() {
        if role().is_some() {
            return;
        }
        let name = unique_name("b");
        let m = mapping::open_or_create_named(&name).expect("parent create");
        let n = RING_FRAMES / 3;
        let (l, r) = pattern(n);
        let h = unsafe { m.header() };
        assert_eq!(unsafe { h.ring_b_push(&l, &r) }, n);
        assert!(run_child("tests::child_pop", "pop", &name), "child pop process failed");
        assert_eq!(h.fill_b(), 0, "the child must have consumed everything");
        assert_eq!(h.b_dropped(), 0);
    }
}
