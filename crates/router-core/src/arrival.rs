//! Measured capture-arrival model: how much ring headroom does the arrival pattern really need?
//!
//! # The maths
//!
//! The producer (WASAPI capture) delivers audio in *arrival events*: calls that are closer than
//! [`MERGE_S`] belong to one event (the Hi-Fi Cable hands over two 10 ms packets back to back every
//! 20 ms, i.e. one 960-frame event per 20 ms). The consumer (ASIO callback) pulls a block of
//! `B` input frames every `B / rate` seconds.
//!
//! Let `A(t)` be the cumulative frames delivered and `r` the stream's frame rate. The *deviation*
//! `D(t) = A(t) - r t` is a sawtooth: it falls linearly at `r` between events and jumps up by the
//! event size at an event. The consumption staircase equals `r t` at every pull instant, so the ring
//! fill seen at the start of pull `k` is exactly
//!
//! ```text
//!     fill_k = D(t_k) + c            (c is a constant fixed by the drift controller)
//! ```
//!
//! The drift controller keeps the *mean* of the fill at its target, so `c = target - mean(D)`. The
//! pull needs `B + L` frames in the ring (`L` = resampler/rounding look-ahead, a couple of frames),
//! hence the stream never underruns iff, for every pull instant,
//!
//! ```text
//!     target >= B + L + ( mean(D) - D(t_k) )
//! ```
//!
//! The pull instants beat against the event train, so over time they hit every phase and the
//! worst case is the *lowest point of the sawtooth*, just before an event. Define the **dip** of an
//! event as `dip_i = mean(D) - D(t_i^-)`. Then
//!
//! ```text
//!     required target = B + L + max_i dip_i  (+ safety for controller wander and clock steps)
//! ```
//!
//! Closed forms (they follow from the sawtooth): for a regular train of events every `G` seconds
//! `dip = r G / 2` (half the gap in frames); for the cable's pairs of 480-frame packets
//! `G = 20 ms`, `dip = 480` frames (10 ms) instead of the former assumption of `2 x 480 = 960` frames.
//! Jitter adds to the dip of late events: a one-off late event of lateness `d` seconds adds `r d`.
//!
//! The dip is measured directly by [`ArrivalTracker`] in the capture sink (it follows `D` with the
//! known frame rate and an exponential time-weighted mean with a 2 s time constant) and aggregated
//! into fixed histograms over a sliding 30 s window ([`ArrivalStats`]). [`required_headroom_frames`]
//! turns the p99.9 dip into frames of headroom with a +25 % margin.
//!
//! The sink side is allocation-free (atomics only); everything here is clock-free (times are
//! passed in), so it can be simulated exactly in tests.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

/// Calls closer than this belong to one arrival event (seconds).
pub const MERGE_S: f64 = 0.0005;
/// Gaps longer than this are stalls / restarts, not jitter: they are counted but not modelled.
pub const STALL_S: f64 = 0.100;
/// Sliding window = `SUB_WINDOWS` x `SUB_SECS`.
pub const SUB_WINDOWS: usize = 6;
pub const SUB_SECS: f64 = 5.0;
/// Window length in seconds (30).
pub const WINDOW_S: f64 = SUB_WINDOWS as f64 * SUB_SECS;
/// Dip histogram: bins of `DIP_BIN` frames; the last bin collects everything above.
pub const DIP_BIN: u32 = 16;
pub const DIP_BINS: usize = 321;
/// Gap histogram (frames-equivalent): bins of `GAP_BIN` frames.
pub const GAP_BIN: u32 = 32;
pub const GAP_BINS: usize = 161;
/// Time constant (s) of the mean-deviation estimate.
const MEAN_TAU_S: f64 = 2.0;
/// Events younger than this after (re)start are not recorded (the mean has not converged).
const WARMUP_S: f64 = 1.0;
/// Resampler look-ahead / rounding frames added to the block requirement (`L`).
pub const LOOKAHEAD_FRAMES: usize = 2;
/// Margin on the measured dip (+25 %).
pub const DIP_MARGIN: f64 = 1.25;
/// Percentile used for the dip and gap statistics.
pub const PERCENTILE: f64 = 0.999;

struct Slot {
    dip: Vec<AtomicU32>,
    gap: Vec<AtomicU32>,
    events: AtomicU32,
    stalls: AtomicU32,
    frames: AtomicU64,
    frames_sq: AtomicU64,
}

impl Slot {
    fn new() -> Slot {
        let mk = |n: usize| (0..n).map(|_| AtomicU32::new(0)).collect::<Vec<_>>();
        Slot {
            dip: mk(DIP_BINS),
            gap: mk(GAP_BINS),
            events: AtomicU32::new(0),
            stalls: AtomicU32::new(0),
            frames: AtomicU64::new(0),
            frames_sq: AtomicU64::new(0),
        }
    }
    fn clear(&self) {
        for a in self.dip.iter().chain(self.gap.iter()) {
            a.store(0, Relaxed);
        }
        self.events.store(0, Relaxed);
        self.stalls.store(0, Relaxed);
        self.frames.store(0, Relaxed);
        self.frames_sq.store(0, Relaxed);
    }
}

/// Shared, lock-free arrival statistics over a sliding 30 s window. Written by the capture
/// sink's [`ArrivalTracker`] (single writer), read by the control thread.
pub struct ArrivalStats {
    slots: Vec<Slot>,
    /// Frame rate of the stream (Hz, f64 bits; set once the capture rate is known).
    rate: AtomicU64,
    /// Seconds of data recorded since the last reset (f64 bits).
    observed_s: AtomicU64,
    /// Bumped by [`request_reset`](Self::request_reset); the tracker clears everything when it notices.
    reset_gen: AtomicU32,
}

/// Aggregated view of the window (see [`ArrivalStats::snapshot`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ArrivalSnapshot {
    /// Seconds of statistics (<= 30).
    pub observed_s: f64,
    /// Arrival events in the window.
    pub events: u32,
    /// Stalls (gaps > 100 ms) in the window.
    pub stalls: u32,
    /// p99.9 of the per-event dip (frames).
    pub dip_p999: u32,
    /// Largest recorded dip bin (frames).
    pub dip_max: u32,
    /// p99.9 of the gap between events, in frames-equivalent (gap x rate).
    pub gap_p999_frames: u32,
    /// Mean event size in frames.
    pub mean_event_frames: f64,
    /// Mean staleness of a delivered frame (s): frame-weighted mean of `b / (2 r)` over events,
    /// i.e. `E[b^2] / (2 r E[b])`. A regular 10 ms packet stream gives 5 ms, the cable's pairs 10 ms.
    pub staleness_s: f64,
    /// Frame rate the statistics were taken at.
    pub rate: f64,
}

impl ArrivalSnapshot {
    /// p99.9 gap in milliseconds.
    pub fn gap_p999_ms(&self) -> f64 {
        if self.rate > 0.0 {
            f64::from(self.gap_p999_frames) * 1000.0 / self.rate
        } else {
            0.0
        }
    }
}

fn percentile(bins: &[u32], width: u32, p: f64) -> u32 {
    let total: u64 = bins.iter().map(|&b| u64::from(b)).sum();
    if total == 0 {
        return 0;
    }
    let rank = ((total as f64 * p).ceil() as u64).clamp(1, total);
    let mut acc = 0u64;
    for (i, &b) in bins.iter().enumerate() {
        acc += u64::from(b);
        if acc >= rank {
            return (i as u32 + 1) * width;
        }
    }
    bins.len() as u32 * width
}

fn max_bin(bins: &[u32], width: u32) -> u32 {
    bins.iter().rposition(|&b| b > 0).map(|i| (i as u32 + 1) * width).unwrap_or(0)
}

impl ArrivalStats {
    /// Statistics for a stream of `rate` frames per second.
    pub fn new(rate: f64) -> Arc<ArrivalStats> {
        Arc::new(ArrivalStats {
            slots: (0..SUB_WINDOWS).map(|_| Slot::new()).collect(),
            rate: AtomicU64::new(rate.max(1.0).to_bits()),
            observed_s: AtomicU64::new(0f64.to_bits()),
            reset_gen: AtomicU32::new(0),
        })
    }

    /// Frame rate (Hz).
    pub fn rate(&self) -> f64 {
        f64::from_bits(self.rate.load(Relaxed))
    }

    /// Sets the frame rate (when the real capture rate becomes known) and restarts the measurement.
    pub fn set_rate(&self, rate: f64) {
        self.rate.store(rate.max(1.0).to_bits(), Relaxed);
        self.request_reset();
    }

    /// Asks the writer to throw away the window and start measuring afresh ("Learn").
    pub fn request_reset(&self) {
        self.reset_gen.fetch_add(1, Relaxed);
    }

    /// Aggregates the window. Allocates (control thread only).
    pub fn snapshot(&self) -> ArrivalSnapshot {
        let mut dip = vec![0u32; DIP_BINS];
        let mut gap = vec![0u32; GAP_BINS];
        let (mut events, mut stalls, mut frames, mut sq) = (0u64, 0u64, 0u64, 0u64);
        for s in &self.slots {
            for (d, a) in dip.iter_mut().zip(&s.dip) {
                *d = d.saturating_add(a.load(Relaxed));
            }
            for (d, a) in gap.iter_mut().zip(&s.gap) {
                *d = d.saturating_add(a.load(Relaxed));
            }
            events += u64::from(s.events.load(Relaxed));
            stalls += u64::from(s.stalls.load(Relaxed));
            frames += s.frames.load(Relaxed);
            sq += s.frames_sq.load(Relaxed);
        }
        let observed = f64::from_bits(self.observed_s.load(Relaxed)).min(WINDOW_S);
        ArrivalSnapshot {
            observed_s: observed,
            events: events.min(u64::from(u32::MAX)) as u32,
            stalls: stalls.min(u64::from(u32::MAX)) as u32,
            dip_p999: percentile(&dip, DIP_BIN, PERCENTILE),
            dip_max: max_bin(&dip, DIP_BIN),
            gap_p999_frames: percentile(&gap, GAP_BIN, PERCENTILE),
            mean_event_frames: if events > 0 { frames as f64 / events as f64 } else { 0.0 },
            staleness_s: if frames > 0 { sq as f64 / (2.0 * self.rate() * frames as f64) } else { 0.0 },
            rate: self.rate(),
        }
    }
}

/// Writer side, owned by the capture sink. `on_frames` is allocation-free.
pub struct ArrivalTracker {
    stats: Arc<ArrivalStats>,
    gen_seen: u32,
    started: bool,
    t_start: f64,
    t_last_call: f64,
    t_last_event: f64,
    /// Frames of the event in progress.
    event_frames: u64,
    d: f64,
    mean: f64,
    slot: usize,
    slot_t0: f64,
}

impl ArrivalTracker {
    pub fn new(stats: Arc<ArrivalStats>) -> ArrivalTracker {
        let gen_seen = stats.reset_gen.load(Relaxed);
        ArrivalTracker {
            stats,
            gen_seen,
            started: false,
            t_start: 0.0,
            t_last_call: 0.0,
            t_last_event: 0.0,
            event_frames: 0,
            d: 0.0,
            mean: 0.0,
            slot: 0,
            slot_t0: 0.0,
        }
    }

    /// The shared statistics.
    pub fn stats(&self) -> &Arc<ArrivalStats> {
        &self.stats
    }

    fn restart(&mut self, t: f64) {
        self.started = true;
        self.t_start = t;
        self.t_last_event = t;
        self.event_frames = 0;
        self.d = 0.0;
        self.mean = 0.0;
        self.slot = 0;
        self.slot_t0 = t;
        for s in &self.stats.slots {
            s.clear();
        }
        self.stats.observed_s.store(0f64.to_bits(), Relaxed);
    }

    fn close_event(&mut self) {
        if self.event_frames > 0 {
            let s = &self.stats.slots[self.slot];
            let b = self.event_frames;
            s.events.fetch_add(1, Relaxed);
            s.frames.fetch_add(b, Relaxed);
            s.frames_sq.fetch_add(b.saturating_mul(b), Relaxed);
            self.event_frames = 0;
        }
    }

    /// A capture callback delivered `n` frames at time `t` (seconds, monotonic).
    pub fn on_frames(&mut self, t: f64, n: u32) {
        let gen = self.stats.reset_gen.load(Relaxed);
        if gen != self.gen_seen {
            self.gen_seen = gen;
            self.started = false;
        }
        if !self.started {
            self.restart(t);
            self.t_last_call = t;
            self.event_frames = u64::from(n);
            self.d = f64::from(n);
            return;
        }
        let new_event = t - self.t_last_call > MERGE_S;
        self.t_last_call = t;
        if !new_event {
            self.event_frames += u64::from(n);
            self.d += f64::from(n);
            return;
        }
        self.close_event();
        let g = t - self.t_last_event;
        let rate = self.stats.rate();
        // Rotate the sub-window.
        let age = t - self.t_start;
        let idx = ((t - self.slot_t0) / SUB_SECS) as i64;
        if idx > 0 {
            let steps = (idx as usize).min(SUB_WINDOWS);
            for _ in 0..steps {
                self.slot = (self.slot + 1) % SUB_WINDOWS;
                self.stats.slots[self.slot].clear();
            }
            self.slot_t0 += idx as f64 * SUB_SECS;
        }
        self.stats.observed_s.store(age.max(0.0).to_bits(), Relaxed);
        if g > STALL_S {
            // A stall (or the stream was restarted): do not model it, resynchronise.
            self.stats.slots[self.slot].stalls.fetch_add(1, Relaxed);
            self.d = f64::from(n);
            self.mean = self.d - rate * 0.5 * MERGE_S;
            self.t_start = t;
            self.t_last_event = t;
            self.event_frames = u64::from(n);
            return;
        }
        let d_before = self.d - rate * g;
        let tau = MEAN_TAU_S.min(t - self.t_start + g).max(g);
        let a = 1.0 - (-g / tau).exp();
        self.mean += (0.5 * (self.d + d_before) - self.mean) * a;
        if t - self.t_start >= WARMUP_S {
            let dip = (self.mean - d_before).max(0.0);
            let s = &self.stats.slots[self.slot];
            let bin = ((dip as u32) / DIP_BIN).min(DIP_BINS as u32 - 1) as usize;
            s.dip[bin].fetch_add(1, Relaxed);
            let gf = (g * rate) as u32;
            let gbin = (gf / GAP_BIN).min(GAP_BINS as u32 - 1) as usize;
            s.gap[gbin].fetch_add(1, Relaxed);
        }
        self.d = d_before + f64::from(n);
        self.t_last_event = t;
        self.event_frames = u64::from(n);
    }
}

/// Parameters of [`required_headroom_frames`].
#[derive(Clone, Copy, Debug)]
pub struct HeadroomParams {
    /// Capture packet period in frames (floor for the headroom).
    pub period_frames: u32,
    /// Largest headroom ever requested (ring capacity guard).
    pub cap_frames: u32,
}

/// Jitter headroom (cable frames) the ring needs on top of `block + safety`: the p99.9 dip with
/// a +25 % margin plus the look-ahead, never below [`min_headroom_frames`] and never above
/// `cap_frames`.
pub fn required_headroom_frames(snap: &ArrivalSnapshot, p: &HeadroomParams) -> usize {
    let measured = (f64::from(snap.dip_p999) * DIP_MARGIN).ceil() as usize + LOOKAHEAD_FRAMES;
    measured.max(min_headroom_frames(p.period_frames)).min(p.cap_frames.max(1) as usize)
}

/// Floor of the headroom: regular arrivals need `period / 2` (see the module docs); the floor of
/// `0.6 x period` keeps a little more than that when the measurement is still unrepresentative.
pub fn min_headroom_frames(period_frames: u32) -> usize {
    (f64::from(period_frames) * 0.6).ceil() as usize + LOOKAHEAD_FRAMES
}

/// Conservative start-up headroom (cable frames) used until the measurement is warm.
///
/// `regular_small_packets`: the source is a fixed-period capture of a known virtual cable (small,
/// regular packets, possibly alternating short/long gaps; mean gap = period). Its worst dip is
/// about `0.5 x period` plus jitter, so an initial burst of `1.5 x period` is enough. Everything
/// else keeps the old assumption of two packets back to back (`2 x period`, e.g. the Hi-Fi Cable's
/// pairs of 10 ms packets). Learned margins and the planner take over after >= 10 s of statistics.
pub fn initial_headroom_frames(period_frames: u32, regular_small_packets: bool) -> usize {
    if regular_small_packets {
        (f64::from(period_frames) * 1.5).ceil() as usize
    } else {
        2 * period_frames as usize
    }
}

/// Lowest safe target fill (cable frames) = `block + safety + headroom`.
pub fn required_target_frames(
    snap: &ArrivalSnapshot,
    block_frames: u32,
    cable_rate: u32,
    asio_rate: u32,
    safety_ms: f32,
    p: &HeadroomParams,
) -> usize {
    crate::resample::target_fill_frames(safety_ms, block_frames, cable_rate, asio_rate, required_headroom_frames(snap, p) as u32)
}

/// Decides when the live headroom follows the measurement: raises at once, lowers slowly with
/// hysteresis, holds while the statistics are too young.
#[derive(Clone, Debug)]
pub struct HeadroomPlanner {
    cur: usize,
    below_since: Option<f64>,
    last_step: f64,
    /// Learned (persisted) headroom floor in frames; 0 = none.
    floor: usize,
}

/// Seconds of statistics before the headroom may be lowered below its conservative start.
pub const WARM_S: f64 = 10.0;
/// Events needed before the measurement is used at all.
pub const MIN_EVENTS: u32 = 100;
/// The measurement must stay this long below the current value before a step down.
pub const LOWER_HOLD_S: f64 = 5.0;
/// Minimum time between two steps down.
pub const LOWER_EVERY_S: f64 = 2.0;
/// Largest step down (frames).
pub const LOWER_STEP: usize = 48;

impl HeadroomPlanner {
    /// `start`: conservative initial headroom (e.g. `2 x period`, or the learned value);
    /// `floor`: learned lower bound (0 = none).
    pub fn new(start: usize, floor: usize) -> HeadroomPlanner {
        HeadroomPlanner { cur: start.max(floor), below_since: None, last_step: f64::NEG_INFINITY, floor }
    }

    /// Current headroom (frames).
    pub fn current(&self) -> usize {
        self.cur
    }

    /// Changes the learned floor (config edit).
    pub fn set_floor(&mut self, floor: usize) {
        self.floor = floor;
        self.cur = self.cur.max(floor);
    }

    /// Restarts the hold timers (after a statistics reset).
    pub fn note_reset(&mut self) {
        self.below_since = None;
    }

    /// One planning step at time `now` (s); returns the headroom to use.
    ///
    /// `may_lower`: the caller's go-ahead for stepping down; the engine withholds it while the ring's
    /// mean fill is still below the current target (the drift controller needs tens of seconds to pull the
    /// fill up after a raise or a start, and lowering the target further would eat the safety margin).
    pub fn update(&mut self, now: f64, snap: &ArrivalSnapshot, p: &HeadroomParams, may_lower: bool) -> usize {
        if snap.events < MIN_EVENTS || snap.observed_s < 3.0 {
            return self.cur;
        }
        let goal = required_headroom_frames(snap, p).max(self.floor).min(p.cap_frames.max(1) as usize);
        let up_hyst = (self.cur / 32).max(8);
        if goal > self.cur + up_hyst {
            self.cur = goal;
            self.below_since = None;
            return self.cur;
        }
        let down_hyst = (self.cur / 12).max(32);
        if may_lower && snap.observed_s >= WARM_S && goal + down_hyst < self.cur {
            let since = *self.below_since.get_or_insert(now);
            if now - since >= LOWER_HOLD_S && now - self.last_step >= LOWER_EVERY_S {
                self.cur = goal.max(self.cur.saturating_sub(LOWER_STEP));
                self.last_step = now;
            }
        } else {
            self.below_since = None;
        }
        self.cur
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `secs` of regular events (`size` frames every `size / rate` s) plus `extra` per event.
    fn feed(tr: &mut ArrivalTracker, rate: f64, size: u32, secs: f64, t0: f64) -> f64 {
        let dt = f64::from(size) / rate;
        let mut t = t0;
        while t < t0 + secs {
            tr.on_frames(t, size);
            t += dt;
        }
        t
    }

    #[test]
    fn regular_events_dip_is_half_the_gap() {
        // 480-frame events every 10 ms at 48 kHz: dip = r G / 2 = 240 frames.
        let st = ArrivalStats::new(48000.0);
        let mut tr = ArrivalTracker::new(st.clone());
        feed(&mut tr, 48000.0, 480, 20.0, 5.0);
        let s = st.snapshot();
        assert!(s.events > 1500, "{s:?}");
        assert!((s.dip_p999 as i32 - 240).abs() <= 2 * DIP_BIN as i32, "{s:?}");
        assert!((s.gap_p999_frames as i32 - 480).abs() <= 2 * GAP_BIN as i32, "{s:?}");
        assert!((s.mean_event_frames - 480.0).abs() < 1e-9);
        // regular packets: staleness = half a period = 5 ms
        assert!((s.staleness_s - 0.005).abs() < 1e-6, "{s:?}");
    }

    #[test]
    fn paired_bursts_dip_is_one_packet() {
        // Two 480-frame packets back to back (50 us apart) every 20 ms.
        let st = ArrivalStats::new(48000.0);
        let mut tr = ArrivalTracker::new(st.clone());
        let mut t = 5.0;
        while t < 30.0 {
            tr.on_frames(t, 480);
            tr.on_frames(t + 0.00005, 480);
            t += 0.020;
        }
        let s = st.snapshot();
        assert!((s.mean_event_frames - 960.0).abs() < 1e-9, "{s:?}");
        assert!((s.dip_p999 as i32 - 480).abs() <= 2 * DIP_BIN as i32, "{s:?}");
        assert!((s.gap_p999_frames as i32 - 960).abs() <= 2 * GAP_BIN as i32, "{s:?}");
        assert!((s.staleness_s - 0.010).abs() < 1e-6, "{s:?}");
        assert!((s.observed_s - 25.0).abs() < 0.1);
        let p = HeadroomParams { period_frames: 480, cap_frames: 4096 };
        let h = required_headroom_frames(&s, &p);
        assert!((590..=640).contains(&h), "{h}");
    }

    /// Alternating short/long gaps (VAC fixed period): events of `size` frames, gaps `g1`, `g2` s.
    fn feed_alternating(tr: &mut ArrivalTracker, size: u32, g1: f64, g2: f64, secs: f64, t0: f64) {
        let mut t = t0;
        let mut k = 0;
        while t < t0 + secs {
            tr.on_frames(t, size);
            t += if k % 2 == 0 { g1 } else { g2 };
            k += 1;
        }
    }

    #[test]
    fn initial_headroom_rule() {
        assert_eq!(initial_headroom_frames(288, true), 432);
        assert_eq!(initial_headroom_frames(288, false), 576);
        assert_eq!(initial_headroom_frames(480, false), 960);
        assert_eq!(initial_headroom_frames(441, true), 662);
    }

    #[test]
    fn alternating_gaps_are_modelled_and_initial_burst_covers_them() {
        // 288 frames (6 ms mean) with 4.5 / 7.5 ms gaps; 240 frames (5 ms) with 3 / 7 ms gaps.
        for (size, g1, g2) in [(288u32, 0.0045, 0.0075), (240, 0.003, 0.007)] {
            let st = ArrivalStats::new(48000.0);
            let mut tr = ArrivalTracker::new(st.clone());
            feed_alternating(&mut tr, size, g1, g2, 20.0, 2.0);
            let s = st.snapshot();
            let p = HeadroomParams { period_frames: size, cap_frames: 4096 };
            let req = required_headroom_frames(&s, &p);
            let init = initial_headroom_frames(size, true);
            println!("alt {size}: dip p99.9 {} fr, required {req} fr, initial {init} fr", s.dip_p999);
            assert!(s.events > 2000, "{s:?}");
            assert!(req <= init, "initial burst {init} must cover the measured requirement {req}");
            assert!(req < 2 * size as usize, "far below the old 2 x period rule");
        }
    }

    #[test]
    fn window_slides_and_reset_clears() {
        let st = ArrivalStats::new(48000.0);
        let mut tr = ArrivalTracker::new(st.clone());
        // 20 s of paired bursts, then 40 s of regular 480 events: the pairs fall out of the window.
        let mut t = 1.0;
        while t < 21.0 {
            tr.on_frames(t, 480);
            tr.on_frames(t + 0.00005, 480);
            t += 0.020;
        }
        assert!(st.snapshot().dip_p999 >= 400);
        let end = feed(&mut tr, 48000.0, 480, 40.0, t);
        let s = st.snapshot();
        assert!(s.dip_p999 <= 280, "{s:?}");
        assert!(s.observed_s <= WINDOW_S + 1e-9);
        st.request_reset();
        tr.on_frames(end + 0.01, 480);
        assert_eq!(st.snapshot().events, 0);
    }

    #[test]
    fn stalls_are_counted_not_modelled() {
        let st = ArrivalStats::new(48000.0);
        let mut tr = ArrivalTracker::new(st.clone());
        let t = feed(&mut tr, 48000.0, 480, 10.0, 1.0);
        tr.on_frames(t + 0.5, 480); // half a second of nothing
        let end = feed(&mut tr, 48000.0, 480, 5.0, t + 0.5 + 0.01);
        let s = st.snapshot();
        assert_eq!(s.stalls, 1);
        assert!(s.dip_p999 < 400, "{s:?}");
        assert!(end > 0.0);
    }

    #[test]
    fn percentile_and_headroom_bounds() {
        let mut bins = vec![0u32; 10];
        bins[2] = 999;
        bins[9] = 1;
        // p99.9 of 1000 samples: rank 999 -> the bin below the outlier
        assert_eq!(percentile(&bins, 16, 0.999), 48);
        assert_eq!(max_bin(&bins, 16), 160);
        assert_eq!(percentile(&[0; 4], 16, 0.999), 0);
        let p = HeadroomParams { period_frames: 480, cap_frames: 2000 };
        let none = ArrivalSnapshot::default();
        assert_eq!(required_headroom_frames(&none, &p), min_headroom_frames(480));
        let huge = ArrivalSnapshot { dip_p999: 100_000, ..Default::default() };
        assert_eq!(required_headroom_frames(&huge, &p), 2000);
        let mid = ArrivalSnapshot { dip_p999: 480, ..Default::default() };
        assert_eq!(required_headroom_frames(&mid, &p), 602);
    }

    fn snap(dip: u32, secs: f64) -> ArrivalSnapshot {
        ArrivalSnapshot { observed_s: secs, events: 5000, dip_p999: dip, rate: 48000.0, ..Default::default() }
    }

    #[test]
    fn planner_holds_raises_fast_and_lowers_slowly() {
        let p = HeadroomParams { period_frames: 480, cap_frames: 4096 };
        let mut pl = HeadroomPlanner::new(960, 0);
        // too little data: hold
        assert_eq!(pl.update(1.0, &ArrivalSnapshot { events: 10, ..snap(100, 1.0) }, &p, true), 960);
        // data but still warming up (< 10 s): no lowering
        assert_eq!(pl.update(5.0, &snap(480, 5.0), &p, true), 960);
        assert_eq!(pl.update(9.0, &snap(480, 9.0), &p, true), 960);
        // warm: needs LOWER_HOLD_S below, then 48-frame steps every 2 s
        let mut now = 12.0;
        assert_eq!(pl.update(now, &snap(480, 12.0), &p, true), 960); // timer starts
        now += 5.0;
        let a = pl.update(now, &snap(480, 17.0), &p, true);
        assert_eq!(a, 960 - LOWER_STEP);
        assert_eq!(pl.update(now + 1.0, &snap(480, 18.0), &p, true), a); // rate limited
        let mut v = a;
        let mut steps = 0;
        let mut t = now + 2.0;
        while v > 602 && steps < 50 {
            v = pl.update(t, &snap(480, 20.0), &p, true);
            t += 2.0;
            steps += 1;
        }
        assert!((602..=602 + 52).contains(&v), "converges to within the hysteresis of the requirement, never below: {v}");
        // small fluctuations do not move it (hysteresis) ...
        assert_eq!(pl.update(t + 10.0, &snap(470, 30.0), &p, true), v);
        assert_eq!(pl.update(t + 20.0, &snap(490, 30.0), &p, true), v);
        // ... a real increase is taken at once
        assert_eq!(pl.update(t + 30.0, &snap(800, 30.0), &p, true), 1002);
        // the caller can veto lowering (ring fill not yet at the target)
        let mut pl = HeadroomPlanner::new(960, 0);
        for i in 0..30 {
            assert_eq!(pl.update(12.0 + 3.0 * f64::from(i), &snap(480, 30.0), &p, false), 960);
        }
        // learned floor
        let mut pl = HeadroomPlanner::new(960, 700);
        for i in 0..40 {
            pl.update(20.0 + 3.0 * f64::from(i), &snap(100, 30.0), &p, true);
        }
        assert!((700..=760).contains(&pl.current()), "{}", pl.current());
    }
}
