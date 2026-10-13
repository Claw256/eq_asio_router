//! Integration-style tests of the real-time path with in-memory seam fakes (same
//! style as `router-engine/src/pipeline_tests.rs`): a synthetic tone leaves a fake
//! [`SystemSource`], travels ring -> matcher -> EQ and lands in a fake
//! [`OutputSink`]. No hardware, no OS calls — the seams are exercised through
//! `FakeSource` / `FakeSink` / `NullTransport` / `InProcTransport` / `TestClock`.

use std::f64::consts::PI;
use std::sync::atomic::{AtomicI64, Ordering::Relaxed};
use std::sync::Arc;

use eq_core::{Config, EqProfile, FilterKind, FilterSpec, ResamplerQuality};
use router_core::arrival::{ArrivalSnapshot, ArrivalStats, ArrivalTracker};
use router_core::processor::{EqProcessor, Handoff, Snapshot};
use router_core::resample::{RateMatcher, RateMatcherConfig};
use router_core::ring::{self, Producer};
use router_core::watchdog::Heartbeat;

use crate::pipeline::{write_out, write_silence_out, Live, Pipeline, PipelineCallback, PipelineInit};
use crate::session::{burst_estimate, plan_capacity, plan_target};
use crate::seams::{Clock, OutputCallback, OutputSink, SinkError, SourceError, StreamRequest, StreamSpec, SystemSource};
use crate::status::{InProcTransport, NullTransport};

const FRAMES: usize = 512;
const OUT_RATE: u32 = 48000;
const TICK_HZ: i64 = 1_000_000;

/// Sentinel bits of an unwritten output sample (`0x5555_5555`, as in the template).
const SENTINEL_BITS: u32 = 0x5555_5555;

fn sentinel() -> f32 {
    f32::from_bits(SENTINEL_BITS)
}

fn has_sentinel(buf: &[f32]) -> bool {
    buf.iter().any(|&v| v.to_bits() == SENTINEL_BITS)
}

/// Identity mapping request -> granted spec (the fake grants exactly what is asked).
fn spec_of(want: &StreamRequest) -> StreamSpec {
    StreamSpec { rate_hz: want.rate_hz, buffer_frames: want.buffer_frames, channels: want.channels }
}

// ------------------------------------------------------------------------------------
// Seam fakes
// ------------------------------------------------------------------------------------

/// Deterministic clock: `now` is set by the rig before each pump so the callback's
/// self-timing is synthetic (300 us of pretended work per block).
#[derive(Default)]
struct TestClock {
    now: AtomicI64,
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
struct FakeSource {
    rate: u32,
    ring: Option<Producer>,
    arrival: ArrivalTracker,
    hb: Arc<Heartbeat>,
}

impl FakeSource {
    fn new(rate: u32, stats: Arc<ArrivalStats>, hb: Arc<Heartbeat>) -> Self {
        FakeSource { rate, ring: None, arrival: ArrivalTracker::new(stats), hb }
    }

    /// Delivers one packet (producer side; not on any audio callback).
    fn push(&mut self, t_s: f64, l: &[f32], r: &[f32]) {
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
struct FakeSink {
    spec: StreamSpec,
    buffers: Vec<Vec<f32>>,
    cb: Option<FakeCallback>,
    started: bool,
}

impl FakeSink {
    fn new() -> Self {
        FakeSink { spec: StreamSpec::default(), buffers: Vec::new(), cb: None, started: false }
    }

    /// One output period: splits the buffers into channel slices and calls the callback.
    fn pump(&mut self, now_ticks: i64) {
        let cb = self.cb.as_mut().expect("callback started");
        let mut bufs: Vec<&mut [f32]> = self.buffers.iter_mut().map(|b| &mut b[..]).collect();
        cb.render(&mut bufs, now_ticks);
    }

    fn reset_sentinel(&mut self) {
        for b in &mut self.buffers {
            for v in b.iter_mut() {
                *v = sentinel();
            }
        }
    }

    fn channel(&self, ch: usize) -> &[f32] {
        &self.buffers[ch]
    }

    fn pipeline(&self) -> &Pipeline {
        self.cb.as_ref().expect("callback started").inner.pipeline()
    }
}

/// The callback installed into [`FakeSink`]: wraps the portable `PipelineCallback`
/// (`write = false` proves the sentinel catches unwritten output).
struct FakeCallback {
    inner: PipelineCallback,
    write: bool,
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
// Rig
// ------------------------------------------------------------------------------------

struct Rig {
    sink: FakeSink,
    source: FakeSource,
    live: Arc<Live>,
    hb: Arc<Heartbeat>,
    handoff: Arc<Handoff<Snapshot>>,
    clock: Arc<TestClock>,
    cable_rate: u32,
    period: usize,
    pushed: u64,
    block: u64,
}

fn sine_cfg(gain_db: f64) -> Config {
    Config {
        profile: EqProfile {
            name: "t".into(),
            preamp_db: 0.0,
            filters: vec![FilterSpec { kind: FilterKind::Peaking, fc_hz: 1000.0, gain_db, q: 1.0, enabled: true }],
        },
        limiter: false,
        auto_headroom: false,
        ..Config::default()
    }
}

fn rig(cfg: &Config, cable_rate: u32, status: Option<Arc<dyn crate::seams::StatusTransport>>) -> Rig {
    let period = (cable_rate / 100) as usize; // 10 ms packets
    let target = plan_target(2.0, FRAMES as u32, cable_rate, OUT_RATE, burst_estimate(period as u32, 0, 0));
    let (prod, cons) = ring::channel(plan_capacity(FRAMES as u32, cable_rate, OUT_RATE, 4096));
    let live = Arc::new(Live::default());
    let hb = Arc::new(Heartbeat::new());
    let handoff = Arc::new(Handoff::new());
    let eq = EqProcessor::new(f64::from(OUT_RATE), cfg, 2, handoff.clone());
    let matcher = RateMatcher::new(RateMatcherConfig::new(cable_rate, OUT_RATE, target, ResamplerQuality::Cubic));
    let pipe = Pipeline::new(PipelineInit {
        source: cons,
        matcher,
        eq,
        frames: FRAMES,
        out_rate: OUT_RATE,
        tick_hz: TICK_HZ,
        live: live.clone(),
        status,
        heartbeat: hb.clone(),
        target_frames: target as u32,
        quality: ResamplerQuality::Cubic,
    });
    let clock = Arc::new(TestClock::default());
    let cb = PipelineCallback::new(pipe, clock.clone());
    let mut sink = FakeSink::new();
    let want = StreamRequest { rate_hz: OUT_RATE, buffer_frames: FRAMES as u32, channels: 2 };
    let spec = sink.negotiate(&want).expect("negotiation");
    assert_eq!(spec, spec_of(&want));
    sink.start(FakeCallback { inner: cb, write: true }).expect("start");
    let stats = ArrivalStats::new(f64::from(cable_rate));
    let mut source = FakeSource::new(cable_rate, stats, hb.clone());
    source.attach(prod).expect("attach");
    Rig { sink, source, live, hb, handoff, clock, cable_rate, period, pushed: 0, block: 0 }
}

impl Rig {
    /// Delivers capture data the way WASAPI does: pairs of 10 ms packets every 20 ms of
    /// stream time (one pair is already there at t = 0).
    fn feed(&mut self, t_s: f64) {
        let pairs = (t_s / 0.020).floor() as u64 + 1;
        while self.pushed < pairs * 2 * self.period as u64 {
            let n = self.period * 2;
            let l: Vec<f32> = (0..n)
                .map(|i| (0.25 * (2.0 * PI * 1000.0 * (self.pushed + i as u64) as f64 / f64::from(self.cable_rate)).sin()) as f32)
                .collect();
            self.source.push(t_s, &l, &l);
            self.pushed += n as u64;
        }
    }

    /// One output callback as the sink runs it, with a synthetic clock.
    fn callback(&mut self) {
        let t_s = self.block as f64 * FRAMES as f64 / f64::from(OUT_RATE);
        self.feed(t_s);
        let t0 = (t_s * TICK_HZ as f64) as i64;
        self.clock.now.store(t0 + 300, Relaxed); // pretend 300 us of work
        self.sink.pump(t0);
        self.block += 1;
    }

    /// RMS of channel 0 over the next `blocks` callbacks.
    fn run_rms(&mut self, blocks: usize) -> f64 {
        let mut acc = 0.0f64;
        let mut cnt = 0usize;
        for _ in 0..blocks {
            self.callback();
            for v in self.sink.channel(0) {
                acc += f64::from(*v) * f64::from(*v);
                cnt += 1;
            }
        }
        (acc / cnt as f64).sqrt()
    }
}

fn in_rms() -> f64 {
    0.25 / 2f64.sqrt()
}

fn gain_db(rms: f64) -> f64 {
    20.0 * (rms / in_rms()).log10()
}

// ------------------------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------------------------

#[test]
fn end_to_end_sine_plus_6db_no_underruns() {
    for cable in [44100u32, 48000] {
        let mut rig = rig(&sine_cfg(6.0), cable, None);
        for _ in 0..20 {
            rig.callback(); // priming, fade-in and filter settling
        }
        let g = gain_db(rig.run_rms(120));
        assert!((g - 6.0).abs() < 0.25, "cable {cable}: gain {g:.3} dB");
        assert_eq!(rig.live.underruns.load(Relaxed), 0, "cable {cable}");
        assert_eq!(rig.live.overruns.load(Relaxed), 0);
        assert_eq!(rig.live.rearms.load(Relaxed), 0);
        assert_eq!(rig.sink.pipeline().fault(), 0);
        assert!(!has_sentinel(rig.sink.channel(0)) && !has_sentinel(rig.sink.channel(1)), "cable {cable}");
        assert_eq!(rig.hb.audio_count(), rig.block);
    }
}

#[test]
fn steady_state_no_underrun() {
    let mut rig = rig(&sine_cfg(0.0), 44100, None);
    for _ in 0..40 {
        rig.callback(); // settle
    }
    for _ in 0..400 {
        rig.callback();
    }
    let last = rig.sink.pipeline().last_render();
    assert!(!last.underrun, "steady state must not underrun");
    assert!(!last.gated, "steady state must be out of the startup gate");
    assert!(last.fill > 0);
    assert_eq!(rig.live.underruns.load(Relaxed), 0);
    assert_eq!(rig.live.rearms.load(Relaxed), 0);
}

#[test]
fn startup_gate_silences_priming_block() {
    let mut rig = rig(&sine_cfg(0.0), 44100, None);
    rig.callback(); // only one pair (882 frames) < target: gated
    assert!(rig.sink.channel(0).iter().all(|&v| v == 0.0), "gated block must be silent");
    assert_eq!(rig.live.underruns.load(Relaxed), 0);
    assert!(rig.sink.pipeline().last_render().gated);
}

#[test]
fn sentinel_catches_unwritten_output() {
    let mut rig = rig(&sine_cfg(0.0), 44100, None);
    // Positive control: a callback that writes nothing leaves the sentinel behind.
    rig.sink.cb.as_mut().unwrap().write = false;
    rig.sink.reset_sentinel();
    rig.callback();
    assert!(has_sentinel(rig.sink.channel(0)), "sentinel must catch unwritten output");
    // The real callback overwrites every sample.
    rig.sink.cb.as_mut().unwrap().write = true;
    rig.sink.reset_sentinel();
    rig.callback();
    assert!(!has_sentinel(rig.sink.channel(0)) && !has_sentinel(rig.sink.channel(1)));
}

#[test]
fn null_transport_smoke() {
    // NullTransport against the pipeline and the session: everything runs, nothing is published.
    let null: Arc<dyn crate::seams::StatusTransport> = Arc::new(NullTransport);
    let mut rig = rig(&sine_cfg(0.0), 44100, Some(null.clone()));
    for _ in 0..30 {
        rig.callback();
    }
    assert_eq!(rig.live.callbacks.load(Relaxed), 30);
    assert_eq!(rig.live.underruns.load(Relaxed), 0);

    let mut session = crate::session::Session::new(
        Config::default(),
        Arc::new(Live::default()),
        Arc::new(NullTransport),
        TICK_HZ,
    );
    session.set_running(true);
    assert_eq!(session.tick(0), crate::seams::ControlAction::None);
    session.set_running(false);
}

#[test]
fn hot_apply_bypass_through_handoff() {
    let cfg = sine_cfg(6.0);
    let mut rig = rig(&cfg, 44100, None);
    for _ in 0..20 {
        rig.callback();
    }
    assert!((gain_db(rig.run_rms(40)) - 6.0).abs() < 0.3);
    let mut bypassed = cfg.clone();
    bypassed.bypass = true;
    rig.handoff.publish(Snapshot::from_config(&bypassed, 2, OUT_RATE));
    for _ in 0..10 {
        rig.callback(); // click-free ramp
    }
    let flat = gain_db(rig.run_rms(40));
    assert!(flat.abs() < 0.3, "bypassed gain {flat}");
    assert_eq!(rig.live.underruns.load(Relaxed), 0);
}

#[test]
fn hot_target_change_is_picked_up() {
    let mut rig = rig(&sine_cfg(0.0), 44100, None);
    let t = rig.sink.pipeline().target_fill();
    rig.live.target_frames.store(t as u32 + 100, Relaxed);
    rig.live.quality.store(0, Relaxed);
    rig.callback();
    assert_eq!(rig.sink.pipeline().target_fill(), t + 100);
}

#[test]
fn status_block_and_timing_stats() {
    let t = Arc::new(InProcTransport::new());
    let mut rig = rig(&sine_cfg(6.0), 44100, Some(t.clone()));
    for _ in 0..40 {
        rig.callback();
    }
    assert_eq!(rig.live.callbacks.load(Relaxed), 40);
    let avg = f32::from_bits(rig.live.cb_avg_us.load(Relaxed));
    assert!((avg - 300.0).abs() < 1.0, "avg {avg}");
    let max = f32::from_bits(rig.live.cb_max_us.load(Relaxed));
    assert!((max - 300.0).abs() < 1.0);
    assert!(f32::from_bits(rig.live.peak_in[0].load(Relaxed)) > 0.2);
    assert!(f32::from_bits(rig.live.peak_out[0].load(Relaxed)) > 0.2);
    assert_eq!(t.blocks(), 40);
    let b = t.block_report();
    assert_eq!(b.underruns, 0);
    assert!(b.peak_out[0] > 0.2);
}

#[test]
fn system_source_seam_reports_arrival_stats() {
    let hb = Arc::new(Heartbeat::new());
    let stats = ArrivalStats::new(44100.0);
    let mut src = FakeSource::new(44100, stats, hb.clone());
    assert_eq!(src.negotiated_rate(), 44100);
    let (prod, _cons) = ring::channel(4096);
    src.attach(prod).unwrap();
    let l = vec![0.1f32; 441];
    for k in 0..10 {
        src.push(k as f64 * 0.02, &l, &l);
    }
    let snap = src.fill_snapshot();
    assert!(snap.events > 0, "arrival events: {}", snap.events);
    assert_eq!(snap.rate, 44100.0);
    assert_eq!(src.heartbeat().capture_count(), 10);
    assert_eq!(src.arrival().stats().snapshot().events, snap.events);
    // re-attach replaces the ring
    let (prod2, _cons2) = ring::channel(4096);
    src.attach(prod2).unwrap();
    assert!(src.fill_snapshot().events >= snap.events);
}

#[test]
fn output_write_handles_short_and_extra_channels() {
    let l = [0.5f32; 8];
    let mut ch2 = [0.0f32; 8];
    let mut ch3 = [0.25f32; 8];
    let mut short = [0.0f32; 4];
    {
        let mut out: [&mut [f32]; 3] = [&mut short, &mut ch2, &mut ch3];
        write_out(&mut out, &l, &l, 8); // short channel: truncated, no panic
    }
    assert_eq!(short, [0.5; 4]);
    assert_eq!(ch2, [0.5; 8]);
    assert_eq!(ch3, [0.0; 8], "extra channels are silenced");
    write_silence_out(&mut [&mut ch2[..], &mut ch3[..]]);
    assert!(ch2.iter().chain(ch3.iter()).all(|&v| v == 0.0));
}

#[test]
fn sink_negotiation_roundtrip() {
    let mut sink = FakeSink::new();
    let want = StreamRequest { rate_hz: 44100, buffer_frames: 256, channels: 2 };
    let spec = sink.negotiate(&want).unwrap();
    let expect = spec_of(&want);
    assert_eq!(spec, expect);
    assert_eq!(sink.spec(), &expect);
    assert!(!sink.started);
    sink.stop().unwrap(); // stopping an unstarted sink is a no-op
    assert!(!sink.started);
}
