//! Heartbeat counters and a watchdog thread that detects stalled audio or capture threads.
//!
//! The audio callback and the capture thread call [`Heartbeat::beat_audio`] /
//! [`Heartbeat::beat_capture`] (one relaxed atomic increment). The watchdog polls every
//! 200 ms (configurable) and, while the engine is marked running, reports
//! [`WatchdogEvent::Stalled`] when a counter did not advance for `stall_polls` consecutive
//! polls, and [`WatchdogEvent::Recovered`] when it moves again. Events are edge-triggered.
//!
//! The decision logic lives in the clock-free [`WatchdogCore::poll`], so tests (and any
//! caller with its own clock) simply call it once per simulated interval.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Default polling interval.
pub const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Monitored component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Component {
    /// The ASIO callback.
    Audio,
    /// The WASAPI capture thread.
    Capture,
}

/// Watchdog notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchdogEvent {
    Stalled(Component),
    Recovered(Component),
}

/// Counters shared between the engine threads and the watchdog.
#[derive(Debug, Default)]
pub struct Heartbeat {
    audio: AtomicU64,
    capture: AtomicU64,
    running: AtomicBool,
    capture_expected: AtomicBool,
}

impl Heartbeat {
    /// New heartbeat (not running; capture monitoring enabled).
    pub fn new() -> Self {
        let h = Heartbeat::default();
        h.capture_expected.store(true, Ordering::Relaxed);
        h
    }
    /// Call once per ASIO callback.
    #[inline]
    pub fn beat_audio(&self) {
        self.audio.fetch_add(1, Ordering::Relaxed);
    }
    /// Call once per captured packet / wake-up.
    #[inline]
    pub fn beat_capture(&self) {
        self.capture.fetch_add(1, Ordering::Relaxed);
    }
    pub fn audio_count(&self) -> u64 {
        self.audio.load(Ordering::Relaxed)
    }
    pub fn capture_count(&self) -> u64 {
        self.capture.load(Ordering::Relaxed)
    }
    /// Marks the engine as streaming (stalls are only reported while true).
    pub fn set_running(&self, on: bool) {
        self.running.store(on, Ordering::Relaxed);
    }
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
    /// Enable/disable monitoring of the capture counter (e.g. while re-opening the device).
    pub fn set_capture_expected(&self, on: bool) {
        self.capture_expected.store(on, Ordering::Relaxed);
    }
}

/// Clock-free stall detection state machine.
#[derive(Debug)]
pub struct WatchdogCore {
    stall_polls: u32,
    was_running: bool,
    last: [u64; 2],
    misses: [u32; 2],
    stalled: [bool; 2],
}

fn idx(c: Component) -> usize {
    match c {
        Component::Audio => 0,
        Component::Capture => 1,
    }
}

impl WatchdogCore {
    /// `stall_polls`: consecutive polls without progress before reporting (minimum 1).
    pub fn new(stall_polls: u32) -> Self {
        WatchdogCore { stall_polls: stall_polls.max(1), was_running: false, last: [0; 2], misses: [0; 2], stalled: [false; 2] }
    }

    /// True if `c` is currently considered stalled.
    pub fn is_stalled(&self, c: Component) -> bool {
        self.stalled[idx(c)]
    }

    /// One poll. Returns up to two events (audio first). While the engine is not running
    /// nothing is reported and state is cleared; the first poll after it starts only takes
    /// a baseline.
    pub fn poll(&mut self, hb: &Heartbeat) -> [Option<WatchdogEvent>; 2] {
        let mut out = [None, None];
        let running = hb.is_running();
        let now = [hb.audio_count(), hb.capture_count()];
        if !running || !self.was_running {
            self.was_running = running;
            self.last = now;
            self.misses = [0; 2];
            self.stalled = [false; 2];
            return out;
        }
        let expected = [true, hb.capture_expected.load(Ordering::Relaxed)];
        let comps = [Component::Audio, Component::Capture];
        for i in 0..2 {
            if !expected[i] {
                self.misses[i] = 0;
                self.stalled[i] = false;
                self.last[i] = now[i];
                continue;
            }
            if now[i] != self.last[i] {
                self.misses[i] = 0;
                if self.stalled[i] {
                    self.stalled[i] = false;
                    out[i] = Some(WatchdogEvent::Recovered(comps[i]));
                }
            } else {
                self.misses[i] += 1;
                if self.misses[i] >= self.stall_polls && !self.stalled[i] {
                    self.stalled[i] = true;
                    out[i] = Some(WatchdogEvent::Stalled(comps[i]));
                }
            }
            self.last[i] = now[i];
        }
        out
    }
}

/// Watchdog thread wrapper.
pub struct Watchdog {
    stop: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl Watchdog {
    /// Spawns the thread with the default 200 ms interval and a 2-poll stall threshold.
    pub fn spawn(hb: Arc<Heartbeat>, on_event: impl FnMut(WatchdogEvent) + Send + 'static) -> std::io::Result<Self> {
        Self::spawn_with(hb, POLL_INTERVAL, 2, on_event)
    }

    /// Spawns the thread with a custom interval and stall threshold.
    pub fn spawn_with(
        hb: Arc<Heartbeat>,
        interval: Duration,
        stall_polls: u32,
        mut on_event: impl FnMut(WatchdogEvent) + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = channel::<()>();
        let handle = std::thread::Builder::new().name("router-watchdog".into()).spawn(move || {
            let mut core = WatchdogCore::new(stall_polls);
            while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(interval) {
                for ev in core.poll(&hb).into_iter().flatten() {
                    on_event(ev);
                }
            }
        })?;
        Ok(Watchdog { stop: Some(tx), handle: Some(handle) })
    }

    /// Stops and joins the thread.
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn idle_engine_reports_nothing() {
        let hb = Heartbeat::new();
        let mut w = WatchdogCore::new(2);
        for _ in 0..10 {
            assert_eq!(w.poll(&hb), [None, None]);
        }
    }

    #[test]
    fn first_poll_after_start_is_baseline() {
        let hb = Heartbeat::new();
        let mut w = WatchdogCore::new(1);
        hb.set_running(true);
        assert_eq!(w.poll(&hb), [None, None]); // baseline
        hb.beat_audio();
        hb.beat_capture();
        assert_eq!(w.poll(&hb), [None, None]);
    }

    #[test]
    fn audio_stall_detected_after_threshold_and_recovers() {
        let hb = Heartbeat::new();
        let mut w = WatchdogCore::new(2);
        hb.set_running(true);
        w.poll(&hb);
        hb.beat_audio();
        hb.beat_capture();
        assert_eq!(w.poll(&hb), [None, None]);
        hb.beat_capture();
        assert_eq!(w.poll(&hb), [None, None], "one miss is tolerated");
        hb.beat_capture();
        assert_eq!(w.poll(&hb), [Some(WatchdogEvent::Stalled(Component::Audio)), None]);
        assert!(w.is_stalled(Component::Audio));
        hb.beat_capture();
        assert_eq!(w.poll(&hb), [None, None], "edge-triggered: no repeat");
        hb.beat_audio();
        hb.beat_capture();
        assert_eq!(w.poll(&hb), [Some(WatchdogEvent::Recovered(Component::Audio)), None]);
    }

    #[test]
    fn capture_stall_and_expected_flag() {
        let hb = Heartbeat::new();
        let mut w = WatchdogCore::new(1);
        hb.set_running(true);
        w.poll(&hb);
        hb.beat_audio();
        assert_eq!(w.poll(&hb), [None, Some(WatchdogEvent::Stalled(Component::Capture))]);
        // device being re-opened: stop expecting beats; stall state clears silently
        hb.set_capture_expected(false);
        hb.beat_audio();
        assert_eq!(w.poll(&hb), [None, None]);
        assert!(!w.is_stalled(Component::Capture));
        hb.set_capture_expected(true);
        hb.beat_audio();
        assert_eq!(w.poll(&hb), [None, Some(WatchdogEvent::Stalled(Component::Capture))]);
    }

    #[test]
    fn both_stall_and_restart_clears() {
        let hb = Heartbeat::new();
        let mut w = WatchdogCore::new(1);
        hb.set_running(true);
        w.poll(&hb);
        assert_eq!(
            w.poll(&hb),
            [Some(WatchdogEvent::Stalled(Component::Audio)), Some(WatchdogEvent::Stalled(Component::Capture))]
        );
        hb.set_running(false);
        assert_eq!(w.poll(&hb), [None, None]);
        assert!(!w.is_stalled(Component::Audio));
        hb.set_running(true);
        assert_eq!(w.poll(&hb), [None, None], "fresh baseline after restart");
    }

    #[test]
    fn thread_reports_stall_through_callback() {
        let hb = Arc::new(Heartbeat::new());
        let events = Arc::new(Mutex::new(Vec::new()));
        let ev2 = events.clone();
        let mut wd = Watchdog::spawn_with(hb.clone(), Duration::from_millis(30), 3, move |e| ev2.lock().unwrap().push(e)).unwrap();
        hb.set_running(true);
        // healthy for a while
        for _ in 0..60 {
            hb.beat_audio();
            hb.beat_capture();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(events.lock().unwrap().is_empty(), "{:?}", events.lock().unwrap());
        // audio stops, capture continues
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_millis(3000) && events.lock().unwrap().is_empty() {
            hb.beat_capture();
            std::thread::sleep(Duration::from_millis(5));
        }
        wd.shutdown();
        let ev = events.lock().unwrap().clone();
        assert_eq!(ev, vec![WatchdogEvent::Stalled(Component::Audio)]);
    }
}
