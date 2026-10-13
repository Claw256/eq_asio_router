//! Hardware-free seam fakes and the `--selftest` harness (Phase C, deviation D4).
//!
//! The fakes used to live in `src/pipeline_tests.rs` behind `cfg(test)`; they are
//! promoted here (pub, std-only) so the tests **and** `portable-cli --selftest`
//! share one rig. Everything here is deterministic and touches no OS audio API —
//! `run_selftest` runs the fake-driven pipeline on every platform (used by CI on
//! Windows / Linux / macOS).

use std::f64::consts::PI;
use std::sync::atomic::{AtomicI64, Ordering::Relaxed};
use std::sync::Arc;

use eq_core::{Config, EqProfile, FilterKind, FilterSpec, ResamplerQuality};
use router_core::arrival::{ArrivalSnapshot, ArrivalStats, ArrivalTracker};
use router_core::processor::{EqProcessor, Handoff, Snapshot};
use router_core::resample::{RateMatcher, RateMatcherConfig};
use router_core::ring::{self, Producer};
use router_core::watchdog::Heartbeat;

use crate::pipeline::{Live, Pipeline, PipelineCallback, PipelineInit};
use crate::session::{burst_estimate, plan_capacity, plan_target, Session};
use crate::seams::{
    Clock, OutputCallback, OutputSink, SinkError, SourceError, StreamRequest, StreamSpec, SystemSource,
};
use crate::status::InProcTransport;

/// Tick domain of [`TestClock`] (microseconds).
pub const TICK_HZ: i64 = 1_000_000;

/// Sentinel bits of an unwritten output sample (`0x5555_5555`, as in the template).
pub const SENTINEL_BITS: u32 = 0x5555_5555;

pub fn sentinel() -> f32 {
    f32::from_bits(SENTINEL_BITS)
}

pub fn has_sentinel(buf: &[f32]) -> bool {
    buf.iter().any(|&v| v.to_bits() == SENTINEL_BITS)
}

/// Identity mapping request -> granted spec (the fake grants exactly what is asked).
pub fn spec_of(want: &StreamRequest) -> StreamSpec {
    StreamSpec { rate_hz: want.rate_hz, buffer_frames: want.buffer_frames, channels: want.channels }
}

// ------------------------------------------------------------------------------------
// Seam fakes
// ------------------------------------------------------------------------------------

/// Deterministic clock: `now` is set by the rig before each pump so the callback's
/// self-timing is synthetic (300 us of pretended work per block).
#[derive(Default)]
pub struct TestClock {
    pub now: AtomicI64,
}

impl Clock for TestClock {
    fn now_ticks(&self) -> i64 {
        self.now.load(Relaxed)
    }
    fn ticks_per_second(&self) -> i64 {
        TICK_HZ
    }
    fn sleep_until(&self, _ticks: i64) {}
}

/// Fake [`SystemSource`]: pushes synthetic tone packets into the attached ring and
/// feeds the arrival statistics, like a system tap would.
pub struct FakeSource {
    pub rate: u32,
    pub ring: Option<Producer>,
    pub arrival: ArrivalTracker,
    pub hb: Arc<Heartbeat>,
}

impl FakeSource {
    pub fn new(rate: u32, stats: Arc<ArrivalStats>, hb: Arc<Heartbeat>) -> Self {
        FakeSource { rate, ring: None, arrival: ArrivalTracker::new(stats), hb }
    }

    /// Delivers one packet (producer side; not on any audio callback).
    pub fn push(&mut self, t_s: f64, l: &[f32], r: &[f32]) {
        self.hb.beat_capture();
        self.arrival.on_frames(t_s, l.len() as u32);
        if let Some(p) = self.ring.as_mut() {
            p.push_frames(l, r);
        }
    }
}

impl SystemSource for FakeSource {
    fn negotiated_rate(&self) -> u32 {
        self.rate
    }
    fn attach(&mut self, ring: Producer) -> Result<(), SourceError> {
        self.ring = Some(ring);
        Ok(())
    }
    fn arrival(&mut self) -> &mut ArrivalTracker {
        &mut self.arrival
    }
    fn fill_snapshot(&mut self) -> ArrivalSnapshot {
        self.arrival.stats().snapshot()
    }
    fn heartbeat(&self) -> &Heartbeat {
        &self.hb
    }
}

/// Fake [`OutputSink`]: keeps one `f32` buffer per channel (pre-filled with the
/// sentinel) and drives its [`FakeCallback`] per period like an ASIO `buffer_switch`.
pub struct FakeSink {
    pub spec: StreamSpec,
    pub buffers: Vec<Vec<f32>>,
    pub cb: Option<FakeCallback>,
    pub started: bool,
}

impl FakeSink {
    pub fn new() -> Self {
        FakeSink { spec: StreamSpec::default(), buffers: Vec::new(), cb: None, started: false }
    }

    /// One output period: splits the buffers into channel slices and calls the callback.
    pub fn pump(&mut self, now_ticks: i64) {
        let cb = self.cb.as_mut().expect("callback started");
        let mut bufs: Vec<&mut [f32]> = self.buffers.iter_mut().map(|b| &mut b[..]).collect();
        cb.render(&mut bufs, now_ticks);
    }

    pub fn reset_sentinel(&mut self) {
        for b in &mut self.buffers {
            for v in b.iter_mut() {
                *v = sentinel();
            }
        }
    }

    pub fn channel(&self, ch: usize) -> &[f32] {
        &self.buffers[ch]
    }

    pub fn pipeline(&self) -> &Pipeline {
        self.cb.as_ref().expect("callback started").inner.pipeline()
    }
}

impl Default for FakeSink {
    fn default() -> Self {
        Self::new()
    }
}

/// The callback installed into [`FakeSink`]: wraps the portable `PipelineCallback`
/// (`write = false` proves the sentinel catches unwritten output).
pub struct FakeCallback {
    pub inner: PipelineCallback,
    pub write: bool,
}

impl OutputCallback for FakeCallback {
    fn render(&mut self, out: &mut [&mut [f32]], now_ticks: i64) {
        if self.write {
            self.inner.render(out, now_ticks);
        }
    }
}

impl OutputSink for FakeSink {
    type Callback = FakeCallback;

    fn negotiate(&mut self, want: &StreamRequest) -> Result<StreamSpec, SinkError> {
        self.spec = spec_of(want);
        self.buffers = (0..want.channels as usize).map(|_| vec![sentinel(); want.buffer_frames as usize]).collect();
        Ok(self.spec)
    }

    fn start(&mut self, cb: Self::Callback) -> Result<(), SinkError> {
        self.cb = Some(cb);
        self.started = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        self.started = false;
        self.cb = None;
        Ok(())
    }

    fn spec(&self) -> &StreamSpec {
        &self.spec
    }
}

// ------------------------------------------------------------------------------------
// Selftest harness
// ------------------------------------------------------------------------------------

/// Outcome of [`run_selftest`] (the `portable-cli --selftest` report).
#[derive(Clone, Debug)]
pub struct SelftestReport {
    pub periods: u64,
    pub callbacks: u64,
    pub underruns: u64,
    pub overruns: u64,
    pub fault: u32,
    pub restarts: u64,
    /// Measured output gain of the +6 dB peaking stage over the input tone (dB).
    pub gain_db: f64,
    pub ok: bool,
}

/// Drives the full fake-based rig — tone -> ring -> rate matcher -> EQ -> fake sink —
/// through a [`Session`] (supervision included), for a fixed number of periods, with
/// no hardware and no OS calls. `ok` iff every callback ran, no underruns/overruns,
/// no fault, no supervisor restart and the EQ stage applied its configured +6 dB.
pub fn run_selftest() -> SelftestReport {
    const FRAMES: usize = 512;
    const OUT_RATE: u32 = 48_000;
    const CABLE_RATE: u32 = 44_100;
    const PERIODS: usize = 320; // ~6.8 s of output audio
    const WARMUP: usize = 40;
    const GAIN_DB: f64 = 6.0;

    let cfg = Config {
        profile: EqProfile {
            name: "selftest".into(),
            preamp_db: 0.0,
            filters: vec![FilterSpec { kind: FilterKind::Peaking, fc_hz: 1000.0, gain_db: GAIN_DB, q: 1.0, enabled: true }],
        },
        limiter: false,
        auto_headroom: false,
        ..Config::default()
    };

    // Rig (same geometry as `pipeline_tests::rig`).
    let period = (CABLE_RATE / 100) as usize; // 10 ms packets
    let target = plan_target(2.0, FRAMES as u32, CABLE_RATE, OUT_RATE, burst_estimate(period as u32, 0, 0));
    let (prod, cons) = ring::channel(plan_capacity(FRAMES as u32, CABLE_RATE, OUT_RATE, 4096));
    let live = Arc::new(Live::default());
    let hb = Arc::new(Heartbeat::new());
    let handoff = Arc::new(Handoff::<Snapshot>::new());
    let eq = EqProcessor::new(f64::from(OUT_RATE), &cfg, 2, handoff.clone());
    let matcher = RateMatcher::new(RateMatcherConfig::new(CABLE_RATE, OUT_RATE, target, ResamplerQuality::Cubic));
    let pipe = Pipeline::new(PipelineInit {
        source: cons,
        matcher,
        eq,
        frames: FRAMES,
        out_rate: OUT_RATE,
        tick_hz: TICK_HZ,
        live: live.clone(),
        status: Some(Arc::new(InProcTransport::new())),
        heartbeat: hb.clone(),
        target_frames: target as u32,
        quality: ResamplerQuality::Cubic,
    });
    let clock = Arc::new(TestClock::default());
    let cb = PipelineCallback::new(pipe, clock.clone());
    let mut sink = FakeSink::new();
    let want = StreamRequest { rate_hz: OUT_RATE, buffer_frames: FRAMES as u32, channels: 2 };
    sink.negotiate(&want).expect("fake negotiation");
    sink.start(FakeCallback { inner: cb, write: true }).expect("fake start");
    let stats = ArrivalStats::new(f64::from(CABLE_RATE));
    let mut source = FakeSource::new(CABLE_RATE, stats, hb.clone());
    source.attach(prod).expect("attach");

    // Supervision over the same Live (the selftest runs Session + pipeline).
    let mut session = Session::new(cfg.clone(), live.clone(), Arc::new(InProcTransport::new()), TICK_HZ);
    session.set_running(true);

    // Deterministic drive: 10 ms tone pairs every 20 ms of stream time, one pump per
    // 512-frame period, 300 us of pretended callback work.
    let mut pushed = 0u64;
    let mut acc = 0.0f64;
    let mut cnt = 0usize;
    for block in 0..PERIODS {
        let t_s = block as f64 * FRAMES as f64 / f64::from(OUT_RATE);
        let pairs = (t_s / 0.020).floor() as u64 + 1;
        while pushed < pairs * 2 * period as u64 {
            let n = period * 2;
            let l: Vec<f32> = (0..n)
                .map(|i| (0.25 * (2.0 * PI * 1000.0 * (pushed + i as u64) as f64 / f64::from(CABLE_RATE)).sin()) as f32)
                .collect();
            source.push(t_s, &l, &l);
            pushed += n as u64;
        }
        let t0 = (t_s * TICK_HZ as f64) as i64;
        clock.now.store(t0 + 300, Relaxed);
        sink.pump(t0);
        session.tick(clock.now_ticks());
        if block >= WARMUP {
            for v in sink.channel(0) {
                acc += f64::from(*v) * f64::from(*v);
                cnt += 1;
            }
        }
    }
    session.set_running(false);

    let rms = (acc / cnt.max(1) as f64).sqrt();
    let gain_db = 20.0 * (rms / (0.25 / 2f64.sqrt())).log10();
    let callbacks = live.callbacks.load(Relaxed);
    let underruns = live.underruns.load(Relaxed);
    let overruns = live.overruns.load(Relaxed);
    let fault = live.fault.load(Relaxed);
    let restarts = session.restarts();
    let ok = callbacks == PERIODS as u64
        && underruns == 0
        && overruns == 0
        && fault == 0
        && restarts == 0
        && (gain_db - GAIN_DB).abs() < 0.5;
    SelftestReport { periods: PERIODS as u64, callbacks, underruns, overruns, fault, restarts, gain_db, ok }
}
