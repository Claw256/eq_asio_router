//! Capture-corruption monitor (pure, allocation-free, runs on the capture thread).
//!
//! Measured on the VAC lite "Line 1" capture endpoint, a shared stream with a period that is too
//! small does not fail loudly, it silently delivers damaged data:
//!
//! * (a) frames arrive **faster than real time** (e.g. 3.06 ms per 240-frame packet) with slips,
//! * (b) **repeated blocks** (a 48-frame / 1 ms period repeats about every second block),
//! * (c) sustained **packet-size / gap anomalies** (odd packet sizes, or back-to-back packet bursts
//!   that also exceed real-time throughput). Bursts that stay at real-time throughput are valid:
//!   the VB-Audio Hi-Fi Cable in default Shared mode delivers 480-frame packets in pairs every
//!   20 ms (gaps ~0 / ~20 ms) and must never trip.
//!
//! `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY` is deliberately NOT used: it is spurious on the
//! VB-Audio Hi-Fi Cable.
//!
//! Valid VAC behaviour that must not trip: at 240 frames the gaps alternate ~3 ms / ~6-7 ms with a
//! mean of exactly 5.0 ms (the ahead-of-real-time error stays below one packet).
//!
//! Caveat for synthetic test signals: a pure digital tone whose period divides the packet length
//! (1 kHz at 48 kHz repeats every 48 frames, so every 240-frame packet is identical) is
//! indistinguishable from a repeated block. Use a non-commensurate test frequency (e.g. 997 Hz)
//! or noise when exercising a monitored stream. Flat (constant) blocks and digital silence are
//! never counted as repeats.

use std::fmt;

/// Tunables; [`MonitorConfig::new`] gives the defaults described in the work package.
#[derive(Clone, Copy, Debug)]
pub struct MonitorConfig {
    pub sample_rate: u32,
    /// Granted / nominal packet size in frames.
    pub nominal_frames: u32,
    /// (a) trip when delivered frames exceed `rate * wall time` by more than this many packets ...
    pub ahead_limit_packets: f64,
    /// ... continuously for this long (µs).
    pub sustain_us: u64,
    /// (a) the real-time reference is re-based every epoch so clock drift cannot accumulate (µs).
    pub epoch_us: u64,
    /// (b) minimum packet size taken into account for repeat detection.
    pub dup_min_frames: u32,
    /// (b) this many repeated packets (max 8) ...
    pub dup_count: usize,
    /// ... within this window (µs) trip.
    pub dup_window_us: u64,
    /// (b) slow rule: this many repeated packets (max 8) within `slow_dup_window_us`. Measured on
    /// VAC lite at 240 frames: a lone duplicated 5 ms block roughly every 30 s (an audible click).
    pub slow_dup_count: usize,
    pub slow_dup_window_us: u64,
    /// (c) a gap shorter than this fraction of the nominal period is anomalous.
    pub burst_gap_frac: f64,
    /// (c) short gaps only count as an anomaly when the window's throughput exceeds real time by
    /// more than this factor.
    pub burst_throughput_factor: f64,
    /// (c) trip when more than this fraction of the packets of a 1 s window are anomalous ...
    pub anomaly_fraction: f64,
    /// ... and the window held at least this many packets.
    pub anomaly_min_packets: u32,
}

impl MonitorConfig {
    pub fn new(sample_rate: u32, nominal_frames: u32) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            nominal_frames: nominal_frames.max(1),
            ahead_limit_packets: 1.5,
            sustain_us: 1_000_000,
            epoch_us: 4_000_000,
            dup_min_frames: 48,
            dup_count: 4,
            dup_window_us: 1_000_000,
            slow_dup_count: 3,
            slow_dup_window_us: 60_000_000,
            burst_gap_frac: 0.4,
            burst_throughput_factor: 1.15,
            anomaly_fraction: 0.3,
            anomaly_min_packets: 20,
        }
    }
}

/// Why the monitor tripped.
#[derive(Clone, Debug, PartialEq)]
pub enum CorruptionReason {
    /// Frames arrive faster than real time by `ahead_frames` for `sustained_ms`.
    FasterThanRealTime { ahead_frames: u32, sustained_ms: u32 },
    /// `count` identical consecutive packets within `window_ms`.
    RepeatedBlocks { count: u32, window_ms: u32 },
    /// `anomalous` of `total` packets of a 1 s window had an abnormal size or gap.
    AnomalousPackets { anomalous: u32, total: u32 },
}

impl fmt::Display for CorruptionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CorruptionReason::FasterThanRealTime { ahead_frames, sustained_ms } => write!(
                f,
                "frames delivered faster than real time ({ahead_frames} frames ahead for {sustained_ms} ms)"
            ),
            CorruptionReason::RepeatedBlocks { count, window_ms } => {
                write!(f, "repeated identical blocks ({count} within {window_ms} ms)")
            }
            CorruptionReason::AnomalousPackets { anomalous, total } => {
                write!(f, "anomalous packet sizes/gaps ({anomalous} of {total} packets in 1 s)")
            }
        }
    }
}

/// Cheap fingerprint of one packet's raw bytes. `None` for empty, digital-silence and flat
/// (every frame identical) packets, which are never treated as repeats.
pub fn packet_fingerprint(data: &[u8], block_align: usize) -> Option<u64> {
    if data.len() < 16 || block_align == 0 || data.len() < block_align * 2 {
        return None;
    }
    // flat test: every frame equal to the previous one (vectorises)
    let mut diff = 0u8;
    for i in block_align..data.len() {
        diff |= data[i] ^ data[i - block_align];
    }
    if diff == 0 {
        return None;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let (chunks, rest) = data.as_chunks::<8>();
    for c in chunks {
        let w = u64::from_le_bytes(*c);
        h = (h ^ w).wrapping_mul(0x0000_0100_0000_01b3);
        h ^= h >> 29;
    }
    for &b in rest {
        h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(h)
}

const RING: usize = 8;

/// The monitor. Feed it every packet in arrival order; it never allocates.
#[derive(Clone, Debug)]
pub struct CorruptionMonitor {
    cfg: MonitorConfig,
    nominal_gap_us: f64,
    started: bool,
    last_t: u64,
    // (a)
    epoch_t: u64,
    epoch_frames: u64,
    ahead_since: Option<u64>,
    max_ahead: i64,
    // (b)
    prev_hash: Option<u64>,
    dup_ring: [u64; RING],
    dup_n: usize,
    dups_total: u64,
    // (c)
    win_start: u64,
    win_n: u32,
    win_anom: u32,
    win_short: u32,
    win_frames: u64,
    trips: u32,
}

impl CorruptionMonitor {
    pub fn new(cfg: MonitorConfig) -> Self {
        let nominal_gap_us = cfg.nominal_frames as f64 * 1e6 / cfg.sample_rate as f64;
        Self {
            cfg,
            nominal_gap_us,
            started: false,
            last_t: 0,
            epoch_t: 0,
            epoch_frames: 0,
            ahead_since: None,
            max_ahead: 0,
            prev_hash: None,
            dup_ring: [0; RING],
            dup_n: 0,
            dups_total: 0,
            win_start: 0,
            win_n: 0,
            win_anom: 0,
            win_short: 0,
            win_frames: 0,
            trips: 0,
        }
    }

    pub fn config(&self) -> &MonitorConfig {
        &self.cfg
    }
    /// Largest "ahead of real time" excess seen so far (frames, may be negative at start).
    pub fn max_ahead_frames(&self) -> i64 {
        self.max_ahead
    }
    /// Identical-consecutive-packet events seen (below the trip threshold they are harmless).
    pub fn repeated_blocks_total(&self) -> u64 {
        self.dups_total
    }
    pub fn trips(&self) -> u32 {
        self.trips
    }

    fn rebase(&mut self, t: u64) {
        self.epoch_t = t;
        self.epoch_frames = 0;
        self.ahead_since = None;
        self.dup_n = 0;
        self.prev_hash = None;
        self.win_start = t;
        self.win_n = 0;
        self.win_anom = 0;
        self.win_short = 0;
        self.win_frames = 0;
    }

    fn trip(&mut self, t: u64, r: CorruptionReason) -> Option<CorruptionReason> {
        self.trips += 1;
        self.rebase(t); // re-arm so a report-only session keeps detecting
        Some(r)
    }

    /// One packet: arrival time `t_us` (monotonic µs), size in frames and its
    /// [`packet_fingerprint`] (`None` = silent/flat/unknown). Returns a reason when tripped.
    pub fn on_packet(&mut self, t_us: u64, frames: u32, hash: Option<u64>) -> Option<CorruptionReason> {
        let t = t_us;
        if !self.started {
            self.started = true;
            self.last_t = t;
            self.rebase(t);
            self.prev_hash = hash;
            return None;
        }
        let gap = t.saturating_sub(self.last_t);
        self.last_t = t;

        // (a) ahead of real time, relative to the epoch's first packet.
        if t - self.epoch_t >= self.cfg.epoch_us {
            self.epoch_t = t;
            self.epoch_frames = 0;
            self.ahead_since = None;
        } else {
            self.epoch_frames += frames as u64;
            let expected = ((t - self.epoch_t) as u128 * self.cfg.sample_rate as u128 / 1_000_000) as i64;
            let ahead = self.epoch_frames as i64 - expected;
            self.max_ahead = self.max_ahead.max(ahead);
            let limit = (self.cfg.ahead_limit_packets * self.cfg.nominal_frames.max(frames) as f64) as i64;
            if ahead > limit {
                let since = *self.ahead_since.get_or_insert(t);
                if t - since >= self.cfg.sustain_us {
                    let r = CorruptionReason::FasterThanRealTime {
                        ahead_frames: ahead.max(0) as u32,
                        sustained_ms: ((t - since) / 1000) as u32,
                    };
                    return self.trip(t, r);
                }
            } else {
                self.ahead_since = None;
            }
        }

        // (b) repeated blocks.
        if let (Some(h), true) = (hash, frames >= self.cfg.dup_min_frames) {
            if self.prev_hash == Some(h) {
                self.dups_total += 1;
                let n = self.cfg.dup_count.clamp(1, RING);
                self.dup_ring[self.dup_n % RING] = t;
                self.dup_n += 1;
                if self.dup_n >= n {
                    let oldest = self.dup_ring[(self.dup_n - n) % RING];
                    if t - oldest <= self.cfg.dup_window_us {
                        let r = CorruptionReason::RepeatedBlocks {
                            count: n as u32,
                            window_ms: ((t - oldest) / 1000).max(1) as u32,
                        };
                        return self.trip(t, r);
                    }
                }
                let sn = self.cfg.slow_dup_count.clamp(1, RING);
                if self.dup_n >= sn {
                    let oldest = self.dup_ring[(self.dup_n - sn) % RING];
                    if t - oldest <= self.cfg.slow_dup_window_us {
                        let r = CorruptionReason::RepeatedBlocks {
                            count: sn as u32,
                            window_ms: ((t - oldest) / 1000).max(1).min(u32::MAX as u64) as u32,
                        };
                        return self.trip(t, r);
                    }
                }
            }
        }
        self.prev_hash = hash;

        // (c) packet size / gap anomalies over 1 s windows.
        self.win_n += 1;
        self.win_frames += frames as u64;
        let nominal = self.cfg.nominal_frames;
        if (gap as f64) < self.cfg.burst_gap_frac * self.nominal_gap_us {
            self.win_short += 1;
        }
        if frames > nominal.saturating_mul(2) || frames.saturating_mul(4) < nominal {
            self.win_anom += 1;
        }
        if t - self.win_start >= 1_000_000 {
            let dur = (t - self.win_start) as f64;
            let throughput = self.win_frames as f64 * 1e6 / (dur * self.cfg.sample_rate as f64);
            let (n, mut a) = (self.win_n, self.win_anom);
            if throughput > self.cfg.burst_throughput_factor {
                a += self.win_short;
            }
            self.win_start = t;
            self.win_n = 0;
            self.win_anom = 0;
            self.win_short = 0;
            self.win_frames = 0;
            if n >= self.cfg.anomaly_min_packets && f64::from(a) > self.cfg.anomaly_fraction * f64::from(n) {
                return self.trip(t, CorruptionReason::AnomalousPackets { anomalous: a, total: n });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a trace of `(gap_us, frames, repeat_prev_hash)` for `secs` and returns the first trip.
    fn run(
        cfg: MonitorConfig,
        secs: u64,
        mut next: impl FnMut(u64) -> (u64, u32, bool),
    ) -> (Option<(u64, CorruptionReason)>, CorruptionMonitor) {
        let mut m = CorruptionMonitor::new(cfg);
        let (mut t, mut i, mut h) = (1_000u64, 0u64, 1u64);
        m.on_packet(t, cfg.nominal_frames, Some(h));
        while t < secs * 1_000_000 {
            let (gap, frames, rep) = next(i);
            i += 1;
            t += gap;
            if !rep {
                h = h.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            }
            if let Some(r) = m.on_packet(t, frames, Some(h)) {
                return (Some((t, r)), m);
            }
        }
        (None, m)
    }

    #[test]
    fn regular_5ms_does_not_trip() {
        let (r, m) = run(MonitorConfig::new(48000, 240), 30, |_| (5000, 240, false));
        assert!(r.is_none(), "{r:?}");
        assert!(m.max_ahead_frames() <= 240);
    }

    #[test]
    fn alternating_vac_gaps_do_not_trip() {
        // valid VAC behaviour: 3 ms / 7 ms (and 3.2 / 6.8), mean 5.0 ms
        for (a, b) in [(3000u64, 7000u64), (3200, 6800), (2900, 7100)] {
            let (r, m) = run(MonitorConfig::new(48000, 240), 60, |i| (if i % 2 == 0 { a } else { b }, 240, false));
            assert!(r.is_none(), "{a}/{b}: {r:?}");
            assert!(m.max_ahead_frames() < 240, "ahead {}", m.max_ahead_frames());
        }
    }

    #[test]
    fn catch_up_burst_after_stall_does_not_trip() {
        let (r, _) = run(MonitorConfig::new(48000, 240), 20, |i| match i {
            400 => (100_000, 240, false),                // 100 ms scheduling stall ...
            401..=419 => (100, 240, false),              // ... then a back-to-back catch-up burst
            _ => (5000, 240, false),
        });
        assert!(r.is_none(), "{r:?}");
    }

    #[test]
    fn small_clock_drift_does_not_trip() {
        // 0.06 % fast for two minutes
        let (r, _) = run(MonitorConfig::new(48000, 240), 120, |_| (4997, 240, false));
        assert!(r.is_none(), "{r:?}");
    }

    #[test]
    fn fast_3_06ms_with_slips_trips() {
        // 240 frames every 3.06 ms = 63 % faster than real time
        let (r, _) = run(MonitorConfig::new(48000, 240), 10, |i| {
            // an occasional damaged (short) packet as slips
            (3060, if i % 50 == 49 { 192 } else { 240 }, false)
        });
        let (t, reason) = r.expect("must trip");
        assert!(matches!(reason, CorruptionReason::FasterThanRealTime { .. }), "{reason:?}");
        assert!(t < 3_000_000, "tripped late at {t} us");
    }

    #[test]
    fn slowly_fast_stream_trips_within_epoch() {
        // 5 % fast (4750 us per 240 frames)
        let (r, _) = run(MonitorConfig::new(48000, 240), 20, |_| (4750, 240, false));
        assert!(matches!(r, Some((_, CorruptionReason::FasterThanRealTime { .. }))), "{r:?}");
    }

    #[test]
    fn one_ms_repeated_blocks_trip() {
        // 48-frame / 1 ms period where every second block is a duplicate
        let (r, _) = run(MonitorConfig::new(48000, 48), 10, |i| (1000, 48, i % 2 == 1));
        let (t, reason) = r.expect("must trip");
        assert!(matches!(reason, CorruptionReason::RepeatedBlocks { .. }), "{reason:?}");
        assert!(t < 1_000_000, "tripped late at {t} us");
    }

    #[test]
    fn rare_repeats_do_not_trip() {
        // a duplicate only every 40 s (<3 per minute) is not a trip
        let (r, m) = run(MonitorConfig::new(48000, 240), 200, |i| (5000, 240, i % 8000 == 7999));
        assert!(r.is_none(), "{r:?}");
        assert!(m.repeated_blocks_total() >= 4);
    }

    #[test]
    fn slow_repeats_trip() {
        // the measured VAC-240 failure: a lone duplicated block every ~10 s
        let (r, _) = run(MonitorConfig::new(48000, 240), 120, |i| (5000, 240, i % 2000 == 1999));
        let (t, reason) = r.expect("must trip");
        assert!(matches!(reason, CorruptionReason::RepeatedBlocks { count: 3, .. }), "{reason:?}");
        assert!(t < 40_000_000, "{t}");
    }

    #[test]
    fn silent_or_flat_blocks_are_not_repeats() {
        let mut m = CorruptionMonitor::new(MonitorConfig::new(48000, 240));
        let mut t = 0;
        for _ in 0..2000 {
            t += 5000;
            assert!(m.on_packet(t, 240, None).is_none());
        }
        assert_eq!(m.repeated_blocks_total(), 0);
        assert_eq!(packet_fingerprint(&[0u8; 960], 4), None);
        assert_eq!(packet_fingerprint(&[7u8; 960], 4), None);
        let mut v = vec![0u8; 960];
        v[500] = 1;
        assert!(packet_fingerprint(&v, 4).is_some());
        let mut w = v.clone();
        assert_eq!(packet_fingerprint(&v, 4), packet_fingerprint(&w, 4));
        w[501] = 9;
        assert_ne!(packet_fingerprint(&v, 4), packet_fingerprint(&w, 4));
    }

    #[test]
    fn hifi_cable_paired_realtime_bursts_do_not_trip() {
        // measured on the Hi-Fi Cable in Shared: 480-frame packets in back-to-back pairs every 20 ms
        let (r, m) = run(MonitorConfig::new(48000, 480), 120, |i| (if i % 2 == 0 { 50 } else { 19_950 }, 480, false));
        assert!(r.is_none(), "{r:?}");
        assert!(m.max_ahead_frames() <= 480, "{}", m.max_ahead_frames());
    }

    #[test]
    fn odd_packet_sizes_trip() {
        // sustained size anomalies: 480-frame nominal, packets alternate 480 / 40 frames
        let (r, _) = run(MonitorConfig::new(48000, 480), 10, |i| (if i % 2 == 0 { 10_000 } else { 833 }, if i % 2 == 0 { 480 } else { 40 }, false));
        assert!(matches!(r, Some((_, CorruptionReason::AnomalousPackets { .. }))), "{r:?}");
    }

    #[test]
    fn fast_bursts_trip() {
        // back-to-back pairs delivering 70 % more than real time
        let (r, _) = run(MonitorConfig::new(48000, 480), 10, |i| (if i % 2 == 0 { 100 } else { 11_500 }, 480, false));
        assert!(r.is_some(), "{r:?}");
    }

    #[test]
    fn rearms_after_trip() {
        let cfg = MonitorConfig::new(48000, 240);
        let mut m = CorruptionMonitor::new(cfg);
        let mut t = 0u64;
        let mut trips = 0;
        for i in 0..2000u64 {
            t += 3060;
            if m.on_packet(t, 240, Some(i + 1)).is_some() {
                trips += 1;
            }
        }
        assert!(trips >= 3, "{trips}");
        assert_eq!(m.trips(), trips);
    }
}
