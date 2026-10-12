//! The pacing thread: the v2 bridge driver has no hardware, so this thread is
//! the clock that drives the host's `bufferSwitch` callbacks.
//!
//! Design (per plan §Component 2):
//! - `CreateWaitableTimerExW` + `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`, with
//!   plain waitable timer / `Sleep` fallbacks when unsupported;
//! - MMCSS `"Pro Audio"` on the thread;
//! - a QPC-anchored **absolute** schedule: `deadline(k) = anchor + k·frames·1e9/rate`
//!   computed in exact rational arithmetic (`u128`) — every tick advances from
//!   the previous deadline and is **never re-anchored to "now"**, so wake-up
//!   jitter cannot accumulate into long-term drift. Re-anchoring happens only
//!   on an explicit rate change (a discontinuity).
//!
//! The stop-vs-callback race follows v1's `ResetThread` pattern: `shutdown`
//! signals the loop and joins it, but if called from *inside* the host's
//! buffer-switch callback (same thread) it detaches instead — the loop notices
//! the stop flag right after the callback returns, and the owner later waits
//! for `exited` with [`PacingThread::finish`] before freeing anything.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{current, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateWaitableTimerExW,
    CreateWaitableTimerW, SetWaitableTimerEx, Sleep, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
};

use crate::dlog;

// ---------------------------------------------------------------- time helpers

/// QPC frequency (Hz); `1` when unavailable.
pub fn qpc_freq() -> i64 {
    let mut f = 0i64;
    // SAFETY: valid out pointer.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut f);
    }
    if f > 0 { f } else { 1 }
}

/// Converts a QPC reading to nanoseconds without 128-bit math.
pub fn qpc_to_ns(counts: i64, freq: i64) -> u64 {
    if freq <= 0 || counts < 0 {
        return 0;
    }
    let (c, f) = (counts as u64, freq as u64);
    (c / f) * 1_000_000_000 + (c % f) * 1_000_000_000 / f
}

/// Current QPC time in nanoseconds since an arbitrary origin (boot).
pub fn qpc_now_ns(freq: i64) -> u64 {
    let mut c = 0i64;
    // SAFETY: valid out pointer.
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    qpc_to_ns(c, freq)
}

/// Frames since start converted to nanoseconds: `frames × 1e9 / rate`
/// (exact rational, `u128` intermediate).
pub fn position_time_ns(frames: u64, rate: u64) -> u64 {
    if rate == 0 {
        return 0;
    }
    ((frames as u128 * 1_000_000_000u128) / rate as u128) as u64
}

/// Nominal period of one buffer in nanoseconds (rounded to nearest ns).
pub fn period_ns(frames: u32, rate: f64) -> u64 {
    if rate <= 0.0 {
        return 0;
    }
    ((frames as f64 / rate) * 1e9).round() as u64
}

// ---------------------------------------------------------------- schedule

/// Absolute, drift-free tick schedule.
///
/// Tick `k` (0-based, after `k` calls to [`Schedule::advance`]) is due at
/// `anchor + (k+1)·frames·1e9/rate`. [`Schedule::wait_ns`] only *reads* the
/// clock; nothing ever moves the anchor except [`Schedule::reanchor`].
#[derive(Debug, Clone, Copy)]
pub struct Schedule {
    anchor_ns: u64,
    frames: u64,
    rate: u64,
    ticks: u64,
}

impl Schedule {
    /// Schedule starting one period after `now_ns`. `rate` is rounded to a
    /// whole Hz (all ASIO rates we negotiate are integers).
    pub fn new(now_ns: u64, frames: u32, rate: u32) -> Self {
        Self { anchor_ns: now_ns, frames: frames as u64, rate: (rate as u64).max(1), ticks: 0 }
    }

    /// Absolute deadline of the next tick (ns, same origin as the anchor).
    pub fn deadline_ns(&self) -> u64 {
        let n = (self.ticks + 1) as u128 * self.frames as u128 * 1_000_000_000u128;
        self.anchor_ns + (n / self.rate as u128) as u64
    }

    /// Nanoseconds until the next tick; `0` when overdue (fire immediately —
    /// the deadline itself never moves, so lateness cannot accumulate).
    pub fn wait_ns(&self, now_ns: u64) -> u64 {
        self.deadline_ns().saturating_sub(now_ns)
    }

    /// One tick fired: advance to the next absolute deadline.
    pub fn advance(&mut self) {
        self.ticks += 1;
    }

    /// Period of the schedule in nanoseconds (integer part).
    pub fn period_ns(&self) -> u64 {
        (self.frames as u128 * 1_000_000_000u128 / self.rate as u128) as u64
    }

    /// Restart the schedule at `now_ns` — only for a sample-rate change, which
    /// is a genuine clock discontinuity.
    pub fn reanchor(&mut self, now_ns: u64, rate: u32) {
        self.anchor_ns = now_ns;
        self.rate = (rate as u64).max(1);
        self.ticks = 0;
    }
}

// ---------------------------------------------------------------- thread

/// What the driver's tick callback reports back to the pacing loop.
pub enum Tick {
    /// Keep firing on the current schedule.
    Continue,
    /// The host was notified of a new sample rate; re-anchor with this rate.
    RateChanged(u32),
    /// Stop the loop (no more work to do).
    Stop,
}

struct Shared {
    stop: AtomicBool,
    exited: AtomicBool,
}

/// Outcome of [`PacingThread::shutdown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The loop fully stopped and was joined — safe to free driver state.
    Joined,
    /// The loop was not joinable right now (called from the pacing/callback
    /// thread itself, or it failed to exit within the timeout). Caller must
    /// keep the object and poll [`PacingThread::finish`] before freeing state.
    Detached,
}

/// Handle to the pacing loop. Owns the thread; the thread owns its timer.
pub struct PacingThread {
    shared: Arc<Shared>,
    thread_id: ThreadId,
    handle: Option<JoinHandle<()>>,
}

impl PacingThread {
    /// Spawns the loop: MMCSS, timer setup, then `on_tick` once per period.
    /// `on_tick` runs on the pacing thread (real-time rules apply to it).
    pub fn spawn<F>(frames: u32, rate: f64, on_tick: F) -> Option<PacingThread>
    where
        F: FnMut() -> Tick + Send + 'static,
    {
        let shared = Arc::new(Shared { stop: AtomicBool::new(false), exited: AtomicBool::new(false) });
        let s2 = shared.clone();
        let handle = std::thread::Builder::new()
            .name("eq-asio-pacing".into())
            .spawn(move || run(s2, frames, rate, on_tick))
            .ok()?;
        let thread_id = handle.thread().id();
        Some(PacingThread { shared, thread_id, handle: Some(handle) })
    }

    /// Signals the loop to stop and waits up to ~2 s for it to exit.
    ///
    /// When called from the pacing thread itself (the host invoked `stop()`
    /// from inside its buffer-switch callback) this never blocks: the loop
    /// exits on its own after the callback returns, and the caller must later
    /// call [`Self::finish`] before freeing anything the loop still uses.
    pub fn shutdown(&mut self) -> StopOutcome {
        self.shared.stop.store(true, Ordering::Release);
        if current().id() == self.thread_id {
            return StopOutcome::Detached;
        }
        if self.finish(2000) {
            StopOutcome::Joined
        } else {
            StopOutcome::Detached
        }
    }

    /// Waits (bounded) for the loop to exit and joins it. `true` = exited.
    pub fn finish(&mut self, timeout_ms: u64) -> bool {
        let t0 = Instant::now();
        while !self.shared.exited.load(Ordering::Acquire) {
            if t0.elapsed() >= Duration::from_millis(timeout_ms) {
                return false;
            }
            // SAFETY: plain sleep.
            unsafe { Sleep(1) };
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        true
    }

    /// Non-blocking: has the loop exited?
    pub fn is_exited(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }
}

impl Drop for PacingThread {
    fn drop(&mut self) {
        // If we are dropped without a successful join, at least tell the loop
        // to stop; the detached thread closes its own timer on exit.
        self.shared.stop.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------- loop

fn run<F>(shared: Arc<Shared>, frames: u32, rate: f64, mut on_tick: F)
where
    F: FnMut() -> Tick,
{
    let freq = qpc_freq();
    let mut mode = "sleep";
    let timer = create_timer(&mut mode);

    // MMCSS "Pro Audio" (best effort; the loop is still deadline-driven without it).
    let task: Vec<u16> = "Pro Audio".encode_utf16().chain(core::iter::once(0)).collect();
    // SAFETY: valid NUL-terminated name + out pointer.
    let av = unsafe { AvSetMmThreadCharacteristicsW(PCWSTR(task.as_ptr()), &mut 0u32) };
    let mmcss = av.is_ok();

    let rate_u = rate.round().clamp(1.0, 768_000.0) as u32;
    let mut sched = Schedule::new(qpc_now_ns(freq), frames, rate_u);
    dlog!(
        "pacing: timer={mode} mmcss={mmcss} period={} ns, {frames} frames @ {rate_u} Hz",
        sched.period_ns()
    );

    while !shared.stop.load(Ordering::Acquire) {
        let now = qpc_now_ns(freq);
        let wait = sched.wait_ns(now);
        if !wait_for(timer, wait, freq, &shared) {
            break;
        }
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        sched.advance();
        match catch_unwind(AssertUnwindSafe(&mut on_tick)) {
            Ok(Tick::Continue) => {}
            Ok(Tick::RateChanged(r)) => {
                dlog!("pacing: rate change -> {r} Hz, re-anchoring");
                sched.reanchor(qpc_now_ns(freq), r);
            }
            Ok(Tick::Stop) => break,
            Err(_) => dlog!("pacing: panic in tick callback (recovered)"),
        }
    }

    if let Some(t) = timer {
        // SAFETY: created above, still ours (nobody else touches it).
        unsafe {
            let _ = CloseHandle(t);
        }
    }
    if let Ok(h) = av {
        // SAFETY: returned by AvSetMmThreadCharacteristicsW above.
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(h);
        }
    }
    shared.exited.store(true, Ordering::Release);
}

/// Waits `wait_ns` (deadline-relative). Returns `false` when the loop should
/// stop. The wait never fires *early*: relative timer units are rounded up.
fn wait_for(timer: Option<HANDLE>, wait_ns: u64, freq: i64, shared: &Shared) -> bool {
    match timer {
        Some(t) => {
            // Waitable timer due times are 100 ns units, negative = relative.
            let units: i64 = if wait_ns == 0 { -1 } else { -(((wait_ns + 99) / 100) as i64) };
            // SAFETY: valid handle; null completion routine / context.
            let armed = unsafe { SetWaitableTimerEx(t, &units, 0, None, None, None, 0) }.is_ok();
            if armed {
                // SAFETY: as above.
                let r = unsafe { WaitForSingleObject(t, INFINITE) };
                if r != WAIT_OBJECT_0 {
                    sleep_wait(wait_ns, freq, shared);
                }
            } else {
                sleep_wait(wait_ns, freq, shared);
            }
        }
        None => sleep_wait(wait_ns, freq, shared),
    }
    !shared.stop.load(Ordering::Acquire)
}

/// Fallback wait for systems without waitable timers (coarse by nature).
fn sleep_wait(wait_ns: u64, freq: i64, shared: &Shared) {
    let deadline = qpc_now_ns(freq).saturating_add(wait_ns);
    while !shared.stop.load(Ordering::Acquire) {
        let now = qpc_now_ns(freq);
        if now >= deadline {
            return;
        }
        let ms = ((deadline - now + 999_999) / 1_000_000).clamp(1, 5) as u32;
        // SAFETY: plain sleep.
        unsafe { Sleep(ms) };
    }
}

/// Timer acquisition: high-resolution → plain → legacy → `None` (sleep mode).
fn create_timer(mode: &mut &'static str) -> Option<HANDLE> {
    // SAFETY: null security attributes and name; handles closed by the loop.
    unsafe {
        if let Ok(h) =
            CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
        {
            *mode = "high-resolution";
            return Some(h);
        }
        if let Ok(h) = CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0) {
            *mode = "standard";
            return Some(h);
        }
        if let Ok(h) = CreateWaitableTimerW(None, true, PCWSTR::null()) {
            *mode = "legacy";
            return Some(h);
        }
        *mode = "sleep";
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qpc_conversion() {
        assert_eq!(qpc_to_ns(10_000_000, 10_000_000), 1_000_000_000);
        assert_eq!(qpc_to_ns(15_000_000, 10_000_000), 1_500_000_000);
        assert_eq!(qpc_to_ns(1, 10_000_000), 100);
        assert_eq!(qpc_to_ns(5, 0), 0);
        // large uptime: 30 days at 10 MHz
        let c = 30i64 * 86_400 * 10_000_000;
        assert_eq!(qpc_to_ns(c, 10_000_000), 30u64 * 86_400 * 1_000_000_000);
    }

    #[test]
    fn position_time_is_exact_frames_over_rate() {
        assert_eq!(position_time_ns(48_000, 48_000), 1_000_000_000);
        assert_eq!(position_time_ns(256, 48_000), 5_333_333);
        assert_eq!(position_time_ns(0, 48_000), 0);
        assert_eq!(position_time_ns(44_100, 44_100), 1_000_000_000);
        assert_eq!(position_time_ns(1, 44_100), 22_675);
        assert_eq!(position_time_ns(5, 0), 0);
        // exact rational: an integral number of seconds has no rounding error
        assert_eq!(position_time_ns(256 * 44_100, 44_100), 1_000_000_000 * 256);
    }

    #[test]
    fn period_of_common_formats() {
        assert_eq!(period_ns(256, 48_000.0), 5_333_333);
        assert_eq!(period_ns(64, 48_000.0), 1_333_333);
        assert_eq!(period_ns(2048, 44_100.0), 46_439_909);
        assert_eq!(period_ns(256, 44_100.0), 5_804_989);
        assert_eq!(period_ns(256, 0.0), 0);
        assert_eq!(period_ns(256, 192_000.0), 1_333_333);
    }

    #[test]
    fn schedule_is_absolute_and_never_reanchors() {
        let anchor = 1_000_000_000u64;
        // The exact grid the schedule must stay on: anchor + k·frames·1e9/rate.
        let grid = |k: u64| -> u64 { anchor + ((k as u128 * 256 * 1_000_000_000u128) / 48_000) as u64 };

        let mut s = Schedule::new(anchor, 256, 48_000);
        assert_eq!(s.wait_ns(anchor), grid(1) - anchor);
        assert_eq!(s.deadline_ns(), grid(1));

        // 1000 jittery, always-late wake-ups: waits collapse to 0, but the
        // deadline grid stays exactly on the anchor (never re-anchored to "now").
        let mut now = anchor;
        for k in 1..=1000u64 {
            now += 7_000_000; // 7 ms "wake-up" spacing vs 5.33 ms period
            assert_eq!(s.wait_ns(now), 0, "overdue at tick {k}");
            assert_eq!(s.deadline_ns(), grid(k), "deadline drifted at tick {k}");
            s.advance();
        }

        // precise wake-ups: the wait is exactly the remaining grid time
        let mut s = Schedule::new(anchor, 256, 48_000);
        let mut now = anchor;
        for k in 1..=100u64 {
            let deadline = grid(k);
            assert_eq!(s.deadline_ns(), deadline);
            assert_eq!(s.wait_ns(now), deadline.saturating_sub(now));
            s.advance();
            now = deadline; // fired exactly on time
        }
    }

    #[test]
    fn schedule_accumulates_no_drift_over_an_hour() {
        // 48 kHz, 256-frame buffers: 187.5 blocks/s. After one hour of exact
        // rational deadlines the position must be exactly 3600 s worth of samples.
        let anchor = 0u64;
        let mut s = Schedule::new(anchor, 256, 48_000);
        let blocks_per_hour: u64 = 675_000; // 187.5 blocks/s × 3600 s
        s.ticks = blocks_per_hour - 1; // deadline of the last block of the hour
        let last = s.deadline_ns();
        assert_eq!(last, position_time_ns(256 * blocks_per_hour, 48_000));
        assert_eq!(last, 3_600u64 * 1_000_000_000); // exactly one hour
    }

    #[test]
    fn schedule_reanchors_only_for_rate_changes() {
        let mut s = Schedule::new(5_000, 64, 48_000);
        s.advance();
        let before = s.deadline_ns();
        s.reanchor(99_999, 96_000);
        assert_eq!(s.deadline_ns(), 99_999 + 64 * 1_000_000_000 / 96_000);
        assert_ne!(before, s.deadline_ns());
        assert_eq!(s.period_ns(), 666_666);
    }

    #[test]
    fn spawn_and_stop_joins_cleanly() {
        let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let t2 = ticks.clone();
        let mut p = PacingThread::spawn(64, 48_000.0, move || {
            t2.fetch_add(1, Ordering::Relaxed);
            Tick::Continue
        })
        .expect("spawn");
        // Wait for a few real callbacks (64 @ 48k = 1.33 ms apart).
        let t0 = Instant::now();
        while ticks.load(Ordering::Relaxed) < 3 && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let seen = ticks.load(Ordering::Relaxed);
        assert!(seen >= 3, "pacing thread never ticked (saw {seen})");
        assert!(matches!(p.shutdown(), StopOutcome::Joined));
        assert!(p.is_exited());
        let after = ticks.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(ticks.load(Ordering::Relaxed), after, "no ticks after shutdown");
    }

    #[test]
    fn shutdown_from_the_pacing_thread_detaches() {
        type Slot = Arc<std::sync::Mutex<Option<PacingThread>>>;
        let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let t2 = ticks.clone();
        let slot: Slot = Arc::new(std::sync::Mutex::new(None));
        let slot_tick = slot.clone();
        let outcome = Arc::new(std::sync::Mutex::new(None::<StopOutcome>));
        let outcome_tick = outcome.clone();

        let p = PacingThread::spawn(64, 48_000.0, move || {
            let n = t2.fetch_add(1, Ordering::Relaxed);
            if n == 2 {
                // Shut down from *inside* the callback: must return Detached
                // immediately instead of joining ourselves.
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let taken = slot_tick.lock().unwrap().take();
                    if let Some(mut pt) = taken {
                        let o = pt.shutdown();
                        *outcome_tick.lock().unwrap() = Some(o);
                        *slot_tick.lock().unwrap() = Some(pt);
                        break;
                    }
                    assert!(Instant::now() < deadline, "test thread never published the handle");
                    std::thread::yield_now();
                }
                return Tick::Stop;
            }
            Tick::Continue
        })
        .expect("spawn");
        *slot.lock().unwrap() = Some(p);

        // The detached loop must still exit on its own after the callback.
        let t0 = Instant::now();
        loop {
            let done = slot.lock().unwrap().as_ref().map(|p| p.is_exited()).unwrap_or(false);
            if done {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "detached pacing thread never exited");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*outcome.lock().unwrap(), Some(StopOutcome::Detached));
        let taken = slot.lock().unwrap().take();
        if let Some(mut p) = taken {
            assert!(p.finish(1000), "join after exit must succeed");
        }
    }
}
