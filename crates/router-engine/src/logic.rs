//! Pure, clock-free decision logic of the engine (unit-tested): config change
//! classification, restart back-off, stall tracking, auto safety buffer and the
//! small numeric helpers used by the audio callback.

use std::time::Duration;

use eq_core::Config;
use router_core::resample::{ring_capacity_for, target_fill_frames};

/// Name of the frozen v1 virtual ASIO driver that must never be used as master.
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

/// How a new config can be taken over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigChange {
    /// Nothing relevant changed.
    Unchanged,
    /// EQ / bypass / limiter / headroom / safety buffer / resampler quality: live.
    HotApply,
    /// Master driver, sample rate, capture device/mode or output channels: restart.
    NeedsRestart(&'static str),
}

/// Classifies `old -> new`. `effective_rate` is the ASIO rate currently in use (0 =
/// unknown): asking for exactly that rate, or "keep current" (0) while a rate is set,
/// is not a restart-worthy change.
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
    if !old.capture_device.eq_ignore_ascii_case(&new.capture_device) {
        return ConfigChange::NeedsRestart("capture device changed");
    }
    if old.capture_mode != new.capture_mode {
        return ConfigChange::NeedsRestart("capture mode changed");
    }
    if (old.capture_period_ms - new.capture_period_ms).abs() > 1e-4 {
        return ConfigChange::NeedsRestart("capture period changed");
    }
    if old.render_keepalive != new.render_keepalive {
        return ConfigChange::NeedsRestart("render keep-alive changed");
    }
    if old.output_channels != new.output_channels {
        return ConfigChange::NeedsRestart("output channels changed");
    }
    if !old.match_cable_rate && new.match_cable_rate {
        return ConfigChange::NeedsRestart("cable rate matching enabled");
    }
    if old == new {
        ConfigChange::Unchanged
    } else {
        ConfigChange::HotApply
    }
}

/// Exponential restart back-off (200 ms -> 5 s by default), clock-free: callers pass
/// how long the previous run lasted.
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

/// Delay between attempts while the master driver is held by another program.
pub const WAIT_RETRY: Duration = Duration::from_secs(2);

/// Remembers which processes held the driver at the last attempt so that a changed
/// holder set is logged once and an unchanged one stays quiet.
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
/// hiccup (or an idle cable) does not cause a restart.
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
        (0..2).find(|&i| {
            self.since[i].is_some_and(|t| now.saturating_sub(t) >= STALL_RESTART_AFTER[i])
        })
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
    target_fill_frames(safety_ms, master_buffer, cable_rate, asio_rate, period_frames)
}

/// Ring capacity that stays valid for any safety setting up to [`MAX_SAFETY_MS`] and
/// capture periods up to `max_period_frames`, so the safety slider can move live.
pub fn plan_capacity(master_buffer: u32, cable_rate: u32, asio_rate: u32, max_period_frames: u32) -> usize {
    ring_capacity_for(target_fill_frames(MAX_SAFETY_MS, master_buffer, cable_rate, asio_rate, max_period_frames))
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

/// Size (frames) of the capture bursts the ring must absorb. WASAPI shared capture of the
/// cable hands over two periods back to back, so until `callbacks` shows a measurement
/// (about two seconds) twice the period is assumed; afterwards the observed burst
/// (never less than one period) is used.
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

#[cfg(test)]
mod tests {
    #[test]
    fn holder_tracker_logs_once_per_change() {
        let mut t = super::HolderTracker::default();
        assert!(t.changed(&[5, 3]));
        assert!(!t.changed(&[3, 5]));
        assert!(!t.changed(&[5, 3]));
        assert!(t.changed(&[3]));
        assert!(t.changed(&[]));
        assert!(!t.changed(&[]));
        t.reset();
        assert!(t.changed(&[]));
    }

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

        // source settings: the preset label and learned margins are live, period / keep-alive restart
        for f in [
            |c: &mut Config| c.source_preset = eq_core::SourcePreset::Custom,
            |c: &mut Config| c.set_jitter_margin(5.0),
        ] {
            let mut c = base.clone();
            f(&mut c);
            assert_eq!(classify_change(&base, &c, 48000), ConfigChange::HotApply);
        }
        let mut c = base.clone();
        c.capture_period_ms = 6.0;
        assert_eq!(classify_change(&base, &c, 48000), ConfigChange::NeedsRestart("capture period changed"));
        let mut c = base.clone();
        c.render_keepalive = true;
        assert_eq!(classify_change(&base, &c, 48000), ConfigChange::NeedsRestart("render keep-alive changed"));
        let mut vac = base.clone();
        vac.apply_source_preset(eq_core::SourcePreset::Vac);
        assert!(matches!(classify_change(&base, &vac, 48000), ConfigChange::NeedsRestart(_)));

        let restarts: [fn(&mut Config); 5] = [
            |c| c.master_driver = "Other ASIO".into(),
            |c| c.sample_rate = 96000,
            |c| c.capture_device = "CABLE Output".into(),
            |c| c.capture_mode = eq_core::CaptureMode::Exclusive,
            |c| c.output_channels = [2, 3],
        ];
        for f in restarts {
            let mut c = base.clone();
            f(&mut c);
            assert!(matches!(classify_change(&base, &c, 48000), ConfigChange::NeedsRestart(_)));
        }
    }

    #[test]
    fn master_name_and_rate_edge_cases() {
        let mut a = Config::default();
        let mut b = Config::default();
        a.master_driver = String::new();
        b.master_driver = "yamaha steinberg usb asio".into();
        // same driver (default name, case-insensitive): the difference is cosmetic, no restart
        assert!(!matches!(classify_change(&a, &b, 48000), ConfigChange::NeedsRestart(_)));
        assert!(is_forbidden_master("eq asio router"));
        assert!(!is_forbidden_master(DEFAULT_MASTER));
        // asking for the rate already in use is not a restart
        let c = Config { sample_rate: 48000, ..Config::default() };
        assert_eq!(classify_change(&Config::default(), &c, 48000), ConfigChange::HotApply);
        assert!(matches!(classify_change(&Config::default(), &c, 44100), ConfigChange::NeedsRestart(_)));
        // 48000 -> 0 (keep current) is fine too
        assert_eq!(classify_change(&c, &Config::default(), 48000), ConfigChange::HotApply);
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
        assert!(cap > plan_target(20.0, 512, 44100, 48000, 441));
    }

    #[test]
    fn burst_assumption_then_measurement() {
        assert_eq!(burst_estimate(441, 0, 0), 882);
        assert_eq!(burst_estimate(441, 441, 10), 882);
        assert_eq!(burst_estimate(441, 1200, 10), 1200);
        assert_eq!(burst_estimate(441, 441, 1000), 441);
        assert_eq!(burst_estimate(441, 882, 1000), 882);
        assert_eq!(burst_estimate(441, 0, 1000), 441);
    }

    #[test]
    fn buffer_choice() {
        assert_eq!(choose_buffer_size(512, 512, 512), 512);
        assert_eq!(choose_buffer_size(64, 2048, 256), 256);
        assert_eq!(choose_buffer_size(64, 2048, 8192), 2048);
        assert_eq!(choose_buffer_size(64, 2048, 0), 64);
        assert_eq!(choose_buffer_size(0, 0, 0), 1);
    }

    #[test]
    fn latency_numbers() {
        assert!((frames_to_ms(871.0, 48000) - 18.1458).abs() < 1e-3);
        assert_eq!(frames_to_ms(10.0, 0), 0.0);
        assert!((cpu_percent(500.0, 10_666.7) - 4.6875).abs() < 1e-2);
        assert_eq!(cpu_percent(1.0, 0.0), 0.0);
        assert_eq!(to_db(0.0), -200.0);
        assert!((to_db(0.5) + 6.0206).abs() < 1e-3);
    }

    #[test]
    fn ratios() {
        let nominal = 10_000.0;
        assert_eq!(gap_ratio(10_000, nominal), 0.0);
        assert_eq!(gap_ratio(30_000, nominal), 0.0);
        assert!((gap_ratio(40_000, nominal) - 0.25).abs() < 1e-6);
        assert_eq!(gap_ratio(10_000_000, nominal), 1.0);
        assert_eq!(gap_ratio(-5, nominal), 0.0);
        assert_eq!(underrun_ratio(512, 512, false), 0.0);
        assert!((underrun_ratio(256, 512, false) - 0.5).abs() < 1e-6);
        assert_eq!(underrun_ratio(0, 512, true), 0.0);
        assert_eq!(underrun_ratio(0, 0, false), 0.0);
    }

    #[test]
    fn auto_safety_raises_and_decays() {
        let mut a = AutoSafety::default();
        assert_eq!(a.update(2.0, 0, secs(1)), 2.0);
        assert_eq!(a.update(2.0, 1, secs(2)), 3.0);
        assert_eq!(a.update(2.0, 3, secs(3)), 4.0); // new underruns -> +1 step per update
        assert_eq!(a.update(2.0, 3, secs(20)), 4.0); // quiet but not long enough
        assert_eq!(a.update(2.0, 3, secs(34)), 3.5);
        assert_eq!(a.update(2.0, 3, secs(50)), 3.5);
        assert_eq!(a.update(2.0, 3, secs(65)), 3.0);
        // cap at the maximum
        let mut a = AutoSafety::default();
        let mut u = 0;
        let mut v = 0.0;
        for i in 0..40 {
            u += 1;
            v = a.update(15.0, u, secs(i));
        }
        assert_eq!(v, MAX_SAFETY_MS);
        a.reset(u, secs(100));
        assert_eq!(a.update(2.0, u, secs(101)), 2.0);
    }
}
