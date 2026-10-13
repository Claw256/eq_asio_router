//! Supervision: direct port of `router-engine/src/logic.rs` (pure decision logic) plus
//! the control-loop essentials of `router-engine/src/engine.rs`, minus the Win32
//! STA pump / mutex / COM lifecycle (those are per-OS backend concerns).
//!
//! Everything here is clock-free like the template: callers pass tick readings
//! ([`Supervisor`] converts them with its ticks-per-second) and durations.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use eq_core::Config;
use router_core::status_writer::StaticInfo;

use crate::pipeline::{reset_reason_text, Live};
use crate::seams::{ConfigChange, ControlAction, ControlLoop};

/// Name of the frozen v1 virtual ASIO driver that must never be used as master
/// (Windows concept; kept for config compatibility — `Config::master_driver` is
/// Windows-shaped).
pub const FORBIDDEN_MASTER: &str = "EQ ASIO Router";
/// Master used when `Config::master_driver` is empty.
pub const DEFAULT_MASTER: &str = "Yamaha Steinberg USB ASIO";

/// Master driver name to use for a config (empty = default).
pub fn effective_master(cfg: &Config) -> String {
    let n = cfg.master_driver.trim();
    if n.is_empty() {
        DEFAULT_MASTER.to_string()
    } else {
        n.to_string()
    }
}

/// True if `name` is the frozen v1 driver (case-insensitive).
pub fn is_forbidden_master(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case(FORBIDDEN_MASTER)
}

/// Classifies `old -> new`. `effective_rate` is the output rate currently in use (0 =
/// unknown): asking for exactly that rate, or "keep current" (0) while a rate is set,
/// is not a restart-worthy change. (Port of `router-engine/src/logic.rs::classify_change`.)
pub fn classify_change(old: &Config, new: &Config, effective_rate: u32) -> ConfigChange {
    if !effective_master(old).eq_ignore_ascii_case(&effective_master(new)) {
        return ConfigChange::NeedsRestart("master driver changed");
    }
    if old.sample_rate != new.sample_rate {
        let already = new.sample_rate == 0 || (effective_rate != 0 && new.sample_rate == effective_rate);
        if !already {
            return ConfigChange::NeedsRestart("sample rate changed");
        }
    }
    if old.output_channels != new.output_channels {
        return ConfigChange::NeedsRestart("output channels changed");
    }
    if old.bridge.loopback_enabled != new.bridge.loopback_enabled {
        return ConfigChange::NeedsRestart("bridge loopback changed");
    }
    // `bridge.ring_target_ms` is applied live (the status tick re-derives the target).
    if old == new {
        ConfigChange::Unchanged
    } else {
        ConfigChange::HotApply
    }
}

/// Exponential restart back-off (200 ms -> 5 s by default), clock-free: callers pass
/// how long the previous run lasted. (Port of `router-engine/src/logic.rs:71`.)
#[derive(Clone, Debug)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
    attempts: u32,
    /// A run at least this long counts as stable and resets the delay.
    stable_after: Duration,
}

impl Backoff {
    pub fn new(min: Duration, max: Duration) -> Self {
        Backoff { min, max, next: min, attempts: 0, stable_after: Duration::from_secs(10) }
    }

    /// Standard engine back-off: 200 ms doubling to 5 s.
    pub fn standard() -> Self {
        Self::new(Duration::from_millis(200), Duration::from_secs(5))
    }

    /// Delay before the next attempt; doubles the following one.
    pub fn next_delay(&mut self) -> Duration {
        let d = self.next;
        self.next = (self.next * 2).min(self.max);
        self.attempts += 1;
        d
    }

    /// Records that a run ended after `uptime`; a stable run resets the back-off.
    pub fn note_run_ended(&mut self, uptime: Duration) {
        if uptime >= self.stable_after {
            self.reset();
        }
    }

    pub fn reset(&mut self) {
        self.next = self.min;
        self.attempts = 0;
    }

    /// Attempts since the last reset.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

/// Delay between attempts while the master device is held by another program.
pub const WAIT_RETRY: Duration = Duration::from_secs(2);

/// Remembers which processes held the device at the last attempt so that a changed
/// holder set is logged once and an unchanged one stays quiet. (Port of `logic.rs`.)
#[derive(Clone, Debug, Default)]
pub struct HolderTracker {
    last: Option<Vec<u32>>,
}

impl HolderTracker {
    /// True if `pids` differs from the previous attempt (order-insensitive) or this is the first.
    pub fn changed(&mut self, pids: &[u32]) -> bool {
        let mut v = pids.to_vec();
        v.sort_unstable();
        if self.last.as_ref() == Some(&v) {
            return false;
        }
        self.last = Some(v);
        true
    }

    /// Forget the history (next busy attempt is logged again).
    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// Tracks how long the audio / capture heartbeat has been stalled so that a short
/// hiccup (or an idle source) does not cause a restart.
/// (Port of `router-engine/src/logic.rs:147`.)
#[derive(Clone, Debug, Default)]
pub struct StallTracker {
    since: [Option<Duration>; 2],
}

/// Stall durations that trigger a restart: audio callback, capture.
pub const STALL_RESTART_AFTER: [Duration; 2] = [Duration::from_millis(1000), Duration::from_millis(3000)];

impl StallTracker {
    /// `which`: 0 audio, 1 capture. Marks a stall as started at `now` (keeps the earlier time).
    pub fn stalled(&mut self, which: usize, now: Duration) {
        if let Some(s) = self.since.get_mut(which) {
            if s.is_none() {
                *s = Some(now);
            }
        }
    }
    pub fn recovered(&mut self, which: usize) {
        if let Some(s) = self.since.get_mut(which) {
            *s = None;
        }
    }
    pub fn clear(&mut self) {
        self.since = [None, None];
    }
    /// Returns the component index (0/1) whose stall has lasted long enough, if any.
    pub fn due(&self, now: Duration) -> Option<usize> {
        (0..2).find(|&i| self.since[i].is_some_and(|t| now.saturating_sub(t) >= STALL_RESTART_AFTER[i]))
    }
}

/// Target fill and ring capacity for a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetPlan {
    pub target_frames: usize,
    pub capacity_frames: usize,
}

/// Maximum safety buffer (ms) the ring is dimensioned for.
pub const MAX_SAFETY_MS: f32 = 20.0;

/// Target fill for `safety_ms` (see `router_core::resample::target_fill_frames`).
pub fn plan_target(safety_ms: f32, master_buffer: u32, cable_rate: u32, asio_rate: u32, period_frames: u32) -> usize {
    router_core::resample::target_fill_frames(safety_ms, master_buffer, cable_rate, asio_rate, period_frames)
}

/// Ring capacity that stays valid for any safety setting up to [`MAX_SAFETY_MS`] and
/// capture periods up to `max_period_frames`, so the safety slider can move live.
pub fn plan_capacity(master_buffer: u32, cable_rate: u32, asio_rate: u32, max_period_frames: u32) -> usize {
    router_core::resample::ring_capacity_for(plan_target(MAX_SAFETY_MS, master_buffer, cable_rate, asio_rate, max_period_frames))
}

/// Buffer size to use: the driver's preferred size, clamped into `[min, max]`
/// (falls back to `min` when the preferred value is unusable).
pub fn choose_buffer_size(min: i32, max: i32, preferred: i32) -> i32 {
    let (lo, hi) = (min.max(1), max.max(min.max(1)));
    if preferred >= lo && preferred <= hi {
        preferred
    } else {
        preferred.clamp(lo, hi)
    }
}

/// Callbacks of observation before the measured burst size replaces the assumption.
pub const BURST_OBSERVE_CALLBACKS: u64 = 200;

/// Size (frames) of the capture bursts the ring must absorb (port of `logic.rs`).
pub fn burst_estimate(period_frames: u32, observed_burst: u32, callbacks: u64) -> u32 {
    if callbacks < BURST_OBSERVE_CALLBACKS {
        period_frames.saturating_mul(2).max(observed_burst)
    } else {
        period_frames.max(observed_burst)
    }
}

/// Milliseconds represented by `frames` at `rate` (0 for an unknown rate).
pub fn frames_to_ms(frames: f64, rate: u32) -> f64 {
    if rate == 0 {
        0.0
    } else {
        frames * 1000.0 / f64::from(rate)
    }
}

/// Gap ratio fed to the fault detector: callback spacing beyond three nominal periods
/// (i.e. a skipped callback). 0 below 3 periods, 0.25 (= mute threshold) at 4 periods.
pub fn gap_ratio(gap_ticks: i64, nominal_ticks: f64) -> f32 {
    if nominal_ticks <= 0.0 || gap_ticks <= 0 {
        return 0.0;
    }
    let periods = gap_ticks as f64 / nominal_ticks;
    ((periods - 3.0).max(0.0) * 0.25).min(1.0) as f32
}

/// Fraction of a block that was not real audio.
pub fn underrun_ratio(produced: usize, n: usize, gated: bool) -> f32 {
    if gated || n == 0 {
        0.0
    } else {
        (1.0 - produced.min(n) as f32 / n as f32).clamp(0.0, 1.0)
    }
}

/// Percentage of the callback period used by `us` microseconds.
pub fn cpu_percent(us: f64, period_us: f64) -> f64 {
    if period_us <= 0.0 {
        0.0
    } else {
        us / period_us * 100.0
    }
}

/// dBFS of a linear peak (-200 for silence).
pub fn to_db(x: f32) -> f32 {
    if x > 1e-10 {
        20.0 * x.log10()
    } else {
        -200.0
    }
}

/// "Auto" safety buffer: raise after underruns, lower slowly when quiet.
/// (Port of `router-engine/src/logic.rs:274`.)
#[derive(Clone, Debug)]
pub struct AutoSafety {
    extra_ms: f32,
    last_underruns: u64,
    last_change: Duration,
    step_up_ms: f32,
    step_down_ms: f32,
    quiet_for: Duration,
}

impl Default for AutoSafety {
    fn default() -> Self {
        AutoSafety {
            extra_ms: 0.0,
            last_underruns: 0,
            last_change: Duration::ZERO,
            step_up_ms: 1.0,
            step_down_ms: 0.5,
            quiet_for: Duration::from_secs(30),
        }
    }
}

impl AutoSafety {
    pub fn extra_ms(&self) -> f32 {
        self.extra_ms
    }
    pub fn reset(&mut self, underruns: u64, now: Duration) {
        self.extra_ms = 0.0;
        self.last_underruns = underruns;
        self.last_change = now;
    }
    /// A new stream starts (counters restart at 0); the learned extra is kept.
    pub fn new_stream(&mut self, now: Duration) {
        self.last_underruns = 0;
        self.last_change = now;
    }
    /// Feeds the running underrun count; returns the effective safety (ms) for
    /// `base_ms`. `now` is a monotonic time since engine start.
    pub fn update(&mut self, base_ms: f32, underruns: u64, now: Duration) -> f32 {
        if underruns > self.last_underruns {
            self.last_underruns = underruns;
            self.last_change = now;
            self.extra_ms = (self.extra_ms + self.step_up_ms).min(MAX_SAFETY_MS - base_ms.min(MAX_SAFETY_MS));
        } else if self.extra_ms > 0.0 && now.saturating_sub(self.last_change) >= self.quiet_for {
            self.extra_ms = (self.extra_ms - self.step_down_ms).max(0.0);
            self.last_change = now;
        }
        (base_ms + self.extra_ms).clamp(0.0, MAX_SAFETY_MS)
    }
}

// ------------------------------------------------------------------------------------
// Control loop
// ------------------------------------------------------------------------------------

/// [`ControlLoop`] implementation: the decision half of `engine.rs`'s `control_loop`
/// (`router-engine/src/engine.rs:1021`) — reset requests, stall tracking and the
/// restart back-off schedule — with the Win32 message pump replaced by caller-driven
/// ticks.
pub struct Supervisor {
    tps: f64,
    start_ticks: i64,
    backoff: Backoff,
    stalls: StallTracker,
    auto: AutoSafety,
    retry_at: Option<i64>,
}

impl Supervisor {
    pub fn new(ticks_per_second: i64) -> Self {
        Supervisor {
            tps: f64::from(ticks_per_second.max(1) as u32).max(1.0),
            start_ticks: 0,
            backoff: Backoff::standard(),
            stalls: StallTracker::default(),
            auto: AutoSafety::default(),
            retry_at: None,
        }
    }

    /// Rebases the tick origin (a new stream / session starts).
    pub fn started(&mut self, now_ticks: i64) {
        self.start_ticks = now_ticks;
    }

    /// Elapsed supervision time at `now_ticks`.
    pub fn elapsed(&self, now_ticks: i64) -> Duration {
        Duration::from_secs_f64(((now_ticks - self.start_ticks).max(0) as f64) / self.tps)
    }

    /// Records that a run ended after `uptime`; stable runs reset the back-off.
    pub fn note_run_ended(&mut self, uptime: Duration) {
        self.backoff.note_run_ended(uptime);
        self.stalls.clear();
    }

    /// Schedules the next retry using the back-off; returns the retry tick.
    pub fn schedule_retry(&mut self, now_ticks: i64) -> i64 {
        let d = self.backoff.next_delay();
        let t = now_ticks + (d.as_secs_f64() * self.tps) as i64;
        self.retry_at = Some(t);
        t
    }

    /// Watchdog event: a component stalled / recovered (`which`: 0 audio, 1 capture).
    pub fn on_stall(&mut self, which: usize, now_ticks: i64) {
        self.stalls.stalled(which, self.elapsed(now_ticks));
    }
    pub fn on_recover(&mut self, which: usize) {
        self.stalls.recovered(which);
    }

    /// Auto-safety state (control thread only).
    pub fn auto_safety(&mut self) -> &mut AutoSafety {
        &mut self.auto
    }

    /// Back-off state (control thread only).
    pub fn backoff(&mut self) -> &mut Backoff {
        &mut self.backoff
    }
}

impl ControlLoop for Supervisor {
    fn on_config(&mut self, old: &Config, new: &Config, effective_rate: u32) -> ConfigChange {
        classify_change(old, new, effective_rate)
    }

    fn on_tick(&mut self, now_ticks: i64, live: &Live) -> ControlAction {
        if live.reset_requested.load(Relaxed) {
            return ControlAction::Restart(reset_reason_text(live.reset_reason.load(Relaxed)).to_string());
        }
        if let Some(i) = self.stalls.due(self.elapsed(now_ticks)) {
            return ControlAction::Restart(if i == 0 { "audio callback stalled" } else { "source stalled" }.to_string());
        }
        ControlAction::None
    }

    fn next_retry(&self) -> Option<i64> {
        self.retry_at
    }
}

// ------------------------------------------------------------------------------------
// Session
// ------------------------------------------------------------------------------------

/// Lifecycle state of a [`Session`] (subset of `engine.rs`'s `EngineState` that the
/// headless supervision loop needs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    Starting,
    Running,
    Restarting { reason: String },
    WaitingForDevice,
    Failed,
    Stopped,
}

/// Supervises one engine session: owns the effective config and rate, the shared
/// [`Live`] counters, the [`crate::seams::StatusTransport`] and the
/// [`Supervisor`] decision state (port of the control-loop essentials of
/// `router-engine/src/engine.rs`).
pub struct Session {
    config: Config,
    effective_rate: u32,
    live: Arc<Live>,
    status: Arc<dyn crate::seams::StatusTransport>,
    supervisor: Supervisor,
    state: SessionState,
    restarts: u64,
}

impl Session {
    pub fn new(
        config: Config,
        live: Arc<Live>,
        status: Arc<dyn crate::seams::StatusTransport>,
        ticks_per_second: i64,
    ) -> Self {
        Session {
            config,
            effective_rate: 0,
            live,
            status,
            supervisor: Supervisor::new(ticks_per_second),
            state: SessionState::Starting,
            restarts: 0,
        }
    }

    /// Takes over the bookkeeping shared by `apply` and the config file poller
    /// (port of `engine.rs` `Shared::adopt`): classifies the diff, pushes the
    /// live-applicable knobs into [`Live`] and stores the new config either way
    /// (the caller acts on the returned [`ConfigChange`]).
    pub fn adopt(&mut self, new: Config, effective_rate: u32) -> ConfigChange {
        let change = classify_change(&self.config, &new, effective_rate);
        self.live.quality.store(crate::pipeline::quality_to_u8(new.resampler_quality), Relaxed);
        self.live.out_rate.store(effective_rate, Relaxed);
        self.config = new;
        change
    }

    /// One supervision tick (port of the `run_session` body of `engine.rs`):
    /// returns what the caller's control loop must do.
    pub fn tick(&mut self, now_ticks: i64) -> ControlAction {
        let action = self.supervisor.on_tick(now_ticks, &self.live);
        match &action {
            ControlAction::Restart(reason) => {
                self.state = SessionState::Restarting { reason: reason.clone() };
                self.restarts += 1;
            }
            ControlAction::Stop => self.state = SessionState::Stopped,
            _ => {}
        }
        action
    }

    /// Marks the stream as running (after a successful open).
    pub fn set_running(&mut self, on: bool) {
        self.state = if on { SessionState::Running } else { SessionState::Starting };
        self.status.set_running(on);
    }

    /// Records that a run ended after `uptime` and schedules the back-off retry.
    pub fn note_run_ended(&mut self, now_ticks: i64, uptime: Duration) -> i64 {
        self.supervisor.note_run_ended(uptime);
        self.live.reset_counters();
        self.supervisor.schedule_retry(now_ticks)
    }

    /// Effective safety buffer (ms) including the learned auto extra.
    pub fn safety_ms(&mut self, now_ticks: i64) -> f32 {
        let base = self.config.safety_buffer_ms;
        let underruns = self.live.underruns.load(Relaxed);
        let now = self.supervisor.elapsed(now_ticks);
        if self.config.safety_buffer_auto {
            self.supervisor.auto_safety().update(base, underruns, now)
        } else {
            base.clamp(0.0, MAX_SAFETY_MS)
        }
    }

    /// Static status fields derived from the current session (caller publishes them
    /// through [`crate::seams::StatusTransport::init`]).
    pub fn static_info(&self) -> StaticInfo {
        StaticInfo {
            sample_rate: self.effective_rate.max(self.config.sample_rate),
            buffer_size: 0, // negotiated by the backend (Phase B/C)
            in_latency: 0,
            out_latency: 0,
            pid: std::process::id(),
            effective_rate: self.effective_rate,
            capture_period_us: 0,
            safety_buffer_us: (self.config.safety_buffer_ms.max(0.0) * 1000.0) as u32,
        }
    }

    /// Records the negotiated output rate (0 = unknown).
    pub fn set_effective_rate(&mut self, rate: u32) {
        self.effective_rate = rate;
        self.live.out_rate.store(rate, Relaxed);
    }

    pub fn config(&self) -> &Config {
        &self.config
    }
    pub fn live(&self) -> &Arc<Live> {
        &self.live
    }
    pub fn status(&self) -> &Arc<dyn crate::seams::StatusTransport> {
        &self.status
    }
    pub fn state(&self) -> &SessionState {
        &self.state
    }
    pub fn restarts(&self) -> u64 {
        self.restarts
    }
    pub fn supervisor(&mut self) -> &mut Supervisor {
        &mut self.supervisor
    }
    pub fn next_retry(&self) -> Option<i64> {
        self.supervisor.next_retry()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn classification() {
        let base = Config::default();
        assert_eq!(classify_change(&base, &base, 48000), ConfigChange::Unchanged);

        let mut c = base.clone();
        c.bypass = true;
        assert_eq!(classify_change(&base, &c, 48000), ConfigChange::HotApply);
        let mut c = base.clone();
        c.profile.preamp_db = -3.0;
        assert_eq!(classify_change(&base, &c, 48000), ConfigChange::HotApply);
        for f in [
            |c: &mut Config| c.safety_buffer_ms = 7.0,
            |c: &mut Config| c.limiter = false,
            |c: &mut Config| c.auto_headroom = false,
            |c: &mut Config| c.mute_on_fault = false,
            |c: &mut Config| c.safety_buffer_auto = false,
            |c: &mut Config| c.resampler_quality = eq_core::ResamplerQuality::Linear,
        ] {
            let mut c = base.clone();
            f(&mut c);
            assert_eq!(classify_change(&base, &c, 48000), ConfigChange::HotApply);
        }
        let mut c = base.clone();
        c.bridge.loopback_enabled = !c.bridge.loopback_enabled;
        assert_eq!(classify_change(&base, &c, 48000), ConfigChange::NeedsRestart("bridge loopback changed"));
        let mut c = base.clone();
        c.bridge.ring_target_ms = 12.0;
        assert_eq!(classify_change(&base, &c, 48000), ConfigChange::HotApply);

        let restarts: [fn(&mut Config); 3] = [
            |c| c.master_driver = "Other ASIO".into(),
            |c| c.sample_rate = 96000,
            |c| c.output_channels = [2, 3],
        ];
        for f in restarts {
            let mut c = base.clone();
            f(&mut c);
            assert!(matches!(classify_change(&base, &c, 48000), ConfigChange::NeedsRestart(_)));
        }
    }

    #[test]
    fn backoff_sequence() {
        let mut b = Backoff::standard();
        let ms: Vec<u128> = (0..8).map(|_| b.next_delay().as_millis()).collect();
        assert_eq!(ms, vec![200, 400, 800, 1600, 3200, 5000, 5000, 5000]);
        assert_eq!(b.attempts(), 8);
        b.note_run_ended(secs(2)); // unstable: keeps growing
        assert_eq!(b.next_delay().as_millis(), 5000);
        b.note_run_ended(secs(11)); // stable: reset
        assert_eq!(b.next_delay().as_millis(), 200);
        assert_eq!(b.attempts(), 1);
    }

    #[test]
    fn stall_tracker() {
        let mut t = StallTracker::default();
        assert_eq!(t.due(secs(100)), None);
        t.stalled(0, secs(10));
        t.stalled(0, secs(11)); // keeps the first time
        assert_eq!(t.due(Duration::from_millis(10_900)), None);
        assert_eq!(t.due(secs(11)), Some(0));
        t.recovered(0);
        assert_eq!(t.due(secs(20)), None);
        t.stalled(1, secs(20));
        assert_eq!(t.due(secs(22)), None);
        assert_eq!(t.due(secs(23)), Some(1));
        t.clear();
        assert_eq!(t.due(secs(99)), None);
    }

    #[test]
    fn target_numbers() {
        // 2 ms safety, 512-frame buffer, 44.1k cable -> 48k master, 441-frame period
        let t = plan_target(2.0, 512, 44100, 48000, 441);
        let expect = (2.0e-3 * 44100.0 + 512.0 * 44100.0 / 48000.0f64).ceil() as usize + 441;
        assert_eq!(t, expect);
        assert_eq!(t, 89 + 470 + 441);
        let cap = plan_capacity(512, 44100, 48000, 2048);
        assert!(cap.is_power_of_two());
        assert!(cap >= plan_target(MAX_SAFETY_MS, 512, 44100, 48000, 2048) * 4);
    }

    #[test]
    fn auto_safety_raises_and_decays() {
        let mut a = AutoSafety::default();
        assert_eq!(a.update(2.0, 0, secs(1)), 2.0);
        assert_eq!(a.update(2.0, 1, secs(2)), 3.0);
        assert_eq!(a.update(2.0, 3, secs(3)), 4.0); // new underruns -> +1 step per update
        assert_eq!(a.update(2.0, 3, secs(20)), 4.0); // quiet but not long enough
        assert_eq!(a.update(2.0, 3, secs(34)), 3.5);
        a.reset(3, secs(100));
        assert_eq!(a.update(2.0, 3, secs(101)), 2.0);
    }

    #[test]
    fn supervisor_tick_restart_and_retry() {
        let live = Live::default();
        let mut sup = Supervisor::new(1_000_000); // 1 tick = 1 us
        sup.started(0);
        assert_eq!(sup.on_tick(1000, &live), ControlAction::None);
        live.request_reset(crate::pipeline::RESET_RATE);
        assert_eq!(sup.on_tick(2000, &live), ControlAction::Restart("driver sample rate changed".to_string()));
        live.reset_counters();
        assert_eq!(sup.on_tick(3000, &live), ControlAction::None);
        // stall: audio not heard for 1 s
        sup.on_stall(0, 10_000);
        assert_eq!(sup.on_tick(500_000, &live), ControlAction::None);
        assert_eq!(
            sup.on_tick(1_010_000, &live),
            ControlAction::Restart("audio callback stalled".to_string())
        );
        let retry = sup.schedule_retry(2_000_000);
        assert_eq!(retry, 2_200_000); // first back-off step: 200 ms
        assert_eq!(sup.next_retry(), Some(2_200_000));
    }
}
