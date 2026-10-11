//! Integration-style tests of the real-time path with a fake ASIO host: synthetic
//! capture data goes through ring -> matcher -> EQ -> converted Int32 output buffers.

use std::f64::consts::PI;
use std::ffi::c_void;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use asio_abi::{ASIOST_INT16_LSB, ASIOST_INT32_LSB};
use eq_core::{Config, EqProfile, FilterKind, FilterSpec, ResamplerQuality};
use router_core::processor::{EqProcessor, Handoff, Snapshot};
use router_core::resample::{RateMatcher, RateMatcherConfig};
use router_core::ring::{self, Producer};
use router_core::status_writer::StatusWriter;
use router_core::watchdog::Heartbeat;

use crate::logic::{burst_estimate, plan_capacity, plan_target};
use crate::pipeline::{Live, OutputSet, Pipeline, PipelineInit, PtrTable};

const FRAMES: usize = 512;
const ASIO_RATE: u32 = 48000;
const TICK_HZ: i64 = 1_000_000;

struct Rig {
    pipe: Pipeline,
    prod: Producer,
    live: Arc<Live>,
    hb: Arc<Heartbeat>,
    handoff: Arc<Handoff<Snapshot>>,
    out: OutputSet,
    bufs: Vec<Vec<i32>>, // [ch][half * FRAMES + i]
    cable_rate: u32,
    period: usize,
    pushed: u64,
    block: u64,
    /// Some(frame): silence with one click at that cable frame instead of the sine.
    click_at: Option<u64>,
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

fn rig(cfg: &Config, cable_rate: u32, status: Option<Arc<StatusWriter>>) -> Rig {
    let period = (cable_rate / 100) as usize; // 10 ms packets
    let target = plan_target(2.0, FRAMES as u32, cable_rate, ASIO_RATE, burst_estimate(period as u32, 0, 0));
    let (prod, cons) = ring::channel(plan_capacity(FRAMES as u32, cable_rate, ASIO_RATE, 4096));
    let live = Arc::new(Live::default());
    let hb = Arc::new(Heartbeat::new());
    let handoff = Arc::new(Handoff::new());
    let eq = EqProcessor::new(f64::from(ASIO_RATE), cfg, 2, handoff.clone());
    let matcher = RateMatcher::new(RateMatcherConfig::new(cable_rate, ASIO_RATE, target, ResamplerQuality::Cubic));
    let pipe = Pipeline::new(PipelineInit {
        consumer: cons,
        matcher,
        eq,
        frames: FRAMES,
        asio_rate: ASIO_RATE,
        tick_hz: TICK_HZ,
        live: live.clone(),
        status,
        heartbeat: hb.clone(),
        target_frames: target as u32,
        quality: ResamplerQuality::Cubic,
        tap_resample: None,
        tap_eq: None,
    });
    let mut bufs = vec![vec![0x5555_5555i32; FRAMES * 2]; 2];
    let ptrs = Arc::new(PtrTable::default());
    for (ch, b) in bufs.iter_mut().enumerate() {
        let p = b.as_mut_ptr();
        // SAFETY: both halves lie inside the Vec.
        ptrs.set(ch, [p as *mut c_void, unsafe { p.add(FRAMES) } as *mut c_void]);
    }
    let out = OutputSet { ptrs, types: [ASIOST_INT32_LSB; 2] };
    Rig { pipe, prod, live, hb, handoff, out, bufs, cable_rate, period, pushed: 0, block: 0, click_at: None }
}

impl Rig {
    /// Delivers capture data the way WASAPI does: pairs of 10 ms packets every 20 ms of
    /// stream time (one pair is already there at t = 0).
    fn feed(&mut self, t_s: f64) {
        let pairs = (t_s / 0.020).floor() as u64 + 1;
        while self.pushed < pairs * 2 * self.period as u64 {
            let n = self.period * 2;
            let click = self.click_at.map(|_| wasapi_capture::click_samples(wasapi_capture::ImpulseShape::default(), self.cable_rate, -40.0));
            let l: Vec<f32> = (0..n)
                .map(|i| {
                    let j = self.pushed + i as u64;
                    if let (Some(at), Some(c)) = (self.click_at, click.as_ref()) {
                        return if j >= at && ((j - at) as usize) < c.len() { c[(j - at) as usize] } else { 0.0 };
                    }
                    (0.25 * (2.0 * PI * 1000.0 * j as f64 / f64::from(self.cable_rate)).sin()) as f32
                })
                .collect();
            self.prod.push_frames(&l, &l);
            self.pushed += n as u64;
        }
    }

    /// One ASIO callback as `AudioCallback::run` does it, with a synthetic clock.
    fn callback(&mut self) {
        let t_s = self.block as f64 * FRAMES as f64 / f64::from(ASIO_RATE);
        self.feed(t_s);
        let t0 = (t_s * TICK_HZ as f64) as i64;
        let half = (self.block & 1) as usize;
        self.pipe.begin(t0);
        let n = self.pipe.render();
        let (l, r) = self.pipe.out();
        // SAFETY: the table points into `self.bufs`, which hold FRAMES samples per half.
        unsafe { self.out.write(half, l, r, n) };
        self.pipe.end(t0, t0 + 300); // pretend 300 us of work
        self.block += 1;
    }

    fn half_as_f32(&self, ch: usize, half: usize) -> Vec<f32> {
        self.bufs[ch][half * FRAMES..(half + 1) * FRAMES].iter().map(|&v| v as f32 / 2147483648.0).collect()
    }

    /// RMS of channel 0 over the next `blocks` callbacks.
    fn run_rms(&mut self, blocks: usize) -> f64 {
        let mut acc = 0.0f64;
        let mut cnt = 0usize;
        for _ in 0..blocks {
            self.callback();
            let half = ((self.block - 1) & 1) as usize;
            for v in self.half_as_f32(0, half) {
                acc += f64::from(v) * f64::from(v);
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
        assert_eq!(rig.pipe.fault(), 0);
        assert!(rig.bufs.iter().all(|b| b.iter().all(|&v| v != 0x5555_5555)));
        assert_eq!(rig.hb.audio_count(), rig.block);
    }
}

#[test]
fn priming_is_silent_and_not_counted_as_underrun() {
    let mut rig = rig(&sine_cfg(0.0), 44100, None);
    rig.callback(); // only one pair (882 frames) < target: gated
    let first = rig.half_as_f32(0, 0);
    assert!(first.iter().all(|&v| v == 0.0), "gated block must be silent");
    assert_eq!(rig.live.underruns.load(Relaxed), 0);
    assert!(rig.pipe.last_render().gated);
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
    rig.handoff.publish(Snapshot::from_config(&bypassed, 2, ASIO_RATE));
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
    let t = rig.pipe.target_fill();
    rig.live.target_frames.store(t as u32 + 100, Relaxed);
    rig.live.quality.store(0, Relaxed);
    rig.callback();
    assert_eq!(rig.pipe.target_fill(), t + 100);
}

#[test]
fn status_block_and_timing_stats() {
    let mut block = Box::new(eq_core::StatusBlock::new());
    // SAFETY: the box outlives the writer within this test and is 8-byte aligned.
    let sw = Arc::new(unsafe { StatusWriter::from_raw(&mut *block as *mut _) });
    let mut rig = rig(&sine_cfg(6.0), 44100, Some(sw));
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
    drop(rig); // drops the pipeline and with it the writer
    assert_eq!(block.callbacks, 40);
}

#[test]
fn output_conversion_types_and_null_pointers() {
    let mut i16buf = vec![0x7777i16; 16];
    let ptrs = Arc::new(PtrTable::default());
    ptrs.set(0, [i16buf.as_mut_ptr() as *mut c_void, std::ptr::null_mut()]);
    let set = OutputSet { ptrs, types: [ASIOST_INT16_LSB, ASIOST_INT32_LSB] };
    let l = [0.5f32; 8];
    // SAFETY: 8 samples fit into the 16-sample buffer; all other pointers are null.
    unsafe { set.write(0, &l, &l, 8) };
    assert!(i16buf[..8].iter().all(|&v| v == 16384));
    assert!(i16buf[8..].iter().all(|&v| v == 0x7777));
    // SAFETY: as above.
    unsafe {
        set.write(1, &l, &l, 8); // null half: must be a no-op
        set.silence(0, 8);
    }
    assert!(i16buf[..8].iter().all(|&v| v == 0));
}


// ------------------------------------------------------------------------------------
// Latency probe: simulated Yamaha loopback (input = output delayed by D samples)
// ------------------------------------------------------------------------------------

use crate::probe::{LoopbackTap, ProbeShared};

/// Drives `rig` for `blocks` callbacks with a simulated loopback input that mirrors the output delayed by
/// `delay` samples, scanning it like `AudioCallback::run` does. Returns the output history (channel 0) and
/// the per-block callback ticks.
fn run_with_loopback(rig: &mut Rig, shared: &Arc<ProbeShared>, delay: usize, blocks: usize, arm_at: usize) -> Vec<f32> {
    let mut inbufs: Vec<Vec<i32>> = vec![vec![0; FRAMES * 2]; 2];
    let in_ptrs = Arc::new(PtrTable::default());
    for (ch, b) in inbufs.iter_mut().enumerate() {
        let p = b.as_mut_ptr();
        // SAFETY: both halves lie inside the Vec.
        in_ptrs.set(ch, [p as *mut c_void, unsafe { p.add(FRAMES) } as *mut c_void]);
    }
    let mut tap = LoopbackTap::new(in_ptrs, [ASIO_INT32; 2], FRAMES, 48000, shared.clone());
    let mut hist: Vec<f32> = Vec::new();
    for k in 0..blocks {
        if k == arm_at {
            shared.loopback.arm(10f32.powf(-60.0 / 20.0));
        }
        let half = k & 1;
        // Input block k holds stream samples [k*N, (k+1)*N) = output samples delayed by `delay`.
        for i in 0..FRAMES {
            let s = (k * FRAMES + i) as i64 - delay as i64;
            let v = if s >= 0 && (s as usize) < hist.len() { hist[s as usize] } else { 0.0 };
            for b in inbufs.iter_mut() {
                b[half * FRAMES + i] = (v * 2147483648.0) as i32;
            }
        }
        let t0 = ((k as f64) * FRAMES as f64 / f64::from(ASIO_RATE) * TICK_HZ as f64) as i64;
        rig.callback();
        let out_half = ((rig.block - 1) & 1) as usize;
        hist.extend(rig.half_as_f32(0, out_half));
        // SAFETY: the table points into `inbufs`, FRAMES samples per half.
        unsafe { tap.scan(half, FRAMES, t0) };
    }
    hist
}

const ASIO_INT32: i32 = ASIOST_INT32_LSB;

#[test]
fn loopback_probe_detects_click_with_simulated_output_delay() {
    for delay in [700usize, 1100, 1531] {
        let mut rig = rig(&sine_cfg(0.0), 48000, None);
        rig.click_at = Some(30_000);
        let shared = Arc::new(ProbeShared::default());
        let hist = run_with_loopback(&mut rig, &shared, delay, 160, 20);
        assert!(shared.loopback.is_done(), "delay {delay}: no detection");
        let det = shared.loopback.take().unwrap();
        // Ground truth: the first output sample at or above the threshold.
        let thr = 10f32.powf(-60.0 / 20.0);
        let first_out = hist.iter().position(|v| v.abs() >= thr).expect("click reached the output");
        // Detected position in the input stream = block start + offset; the input stream is the output delayed.
        let block_idx = (det.qpc as f64 / 1e6 * f64::from(ASIO_RATE) / FRAMES as f64).round() as usize;
        let s_det = block_idx * FRAMES + det.offset as usize;
        assert_eq!(s_det, first_out + delay, "delay {delay}: detected input sample vs output sample + delay");
        assert_eq!(det.block_len as usize, FRAMES);
        assert!(det.peak > 0.009 && det.peak < 0.0101, "peak {}", det.peak); // -40 dBFS = 0.01
        assert_eq!(rig.live.underruns.load(Relaxed), 0);
        // After take() the probe is idle again and scanning is a no-op.
        assert!(!shared.loopback.is_active());
    }
}

#[test]
fn loopback_probe_is_inert_until_armed() {
    let mut rig = rig(&sine_cfg(0.0), 48000, None);
    let shared = Arc::new(ProbeShared::default());
    // Never armed: a null pointer table must not even be touched.
    let mut tap = LoopbackTap::new(Arc::new(PtrTable::default()), [ASIO_INT32; 2], FRAMES, 48000, shared.clone());
    for k in 0..30 {
        rig.callback();
        // SAFETY: idle scan returns before reading anything.
        unsafe { tap.scan(k & 1, FRAMES, k as i64) };
    }
    assert!(!shared.loopback.is_done() && !shared.loopback.is_active());
    assert!(shared.loopback.take().is_none());
}

/// Feeds `x` (stereo-identical) through a [`LoopbackTap`] as Int32 ASIO blocks of `FRAMES`; `per_block(k)` runs
/// before block `k` is scanned (arming, monitor control).
fn feed_tap(shared: &Arc<ProbeShared>, x: &[f32], mut per_block: impl FnMut(usize)) {
    let blocks = x.len() / FRAMES;
    let mut inbufs: Vec<Vec<i32>> = vec![vec![0; FRAMES * 2]; 2];
    let in_ptrs = Arc::new(PtrTable::default());
    for (ch, b) in inbufs.iter_mut().enumerate() {
        let p = b.as_mut_ptr();
        // SAFETY: both halves lie inside the Vec.
        in_ptrs.set(ch, [p as *mut c_void, unsafe { p.add(FRAMES) } as *mut c_void]);
    }
    let mut tap = LoopbackTap::new(in_ptrs, [ASIO_INT32; 2], FRAMES, 48000, shared.clone());
    for k in 0..blocks {
        per_block(k);
        let half = k & 1;
        for i in 0..FRAMES {
            let v = (x[k * FRAMES + i] * 2147483648.0) as i32;
            for b in inbufs.iter_mut() {
                b[half * FRAMES + i] = v;
            }
        }
        // SAFETY: the table points into `inbufs`, FRAMES samples per half.
        unsafe { tap.scan(half, FRAMES, k as i64 * 1000) };
    }
}

/// Synthetic microphone-like loopback content: dense tones below 12 kHz at about -46 dBFS rms plus a DC offset.
fn mic_like(len: usize, dc_db: f32) -> Vec<f32> {
    let amp = 10f32.powf(-46.0 / 20.0) * (2.0f32 / 60.0).sqrt();
    let dc = 10f32.powf(dc_db / 20.0);
    (0..len)
        .map(|i| dc + (0..60).map(|k| amp * (2.0 * std::f32::consts::PI * (150.0 + k as f32 * 190.0) * i as f32 / 48000.0 + k as f32 * 1.7).sin()).sum::<f32>())
        .collect()
}

#[test]
fn loopback_dc_offset_alone_does_not_trigger_a_dc_removed_detector() {
    let mut x = vec![10f32.powf(-50.0 / 20.0); FRAMES * 40]; // DC at -50 dBFS, nothing else
    let shared = Arc::new(ProbeShared::default());
    // The old fixed-threshold detector (-66 dBFS) fires on the DC at once ...
    feed_tap(&shared, &x, |k| {
        if k == 5 {
            shared.loopback.arm(10f32.powf(-66.0 / 20.0));
        }
    });
    assert!(shared.loopback.has_crossing());
    // ... the DC-removed one stays quiet and still sees a -55 dBFS click on top of the offset.
    let shared = Arc::new(ProbeShared::default());
    let click_at = FRAMES * 20 + 100;
    for i in 0..96 {
        x[click_at + i] += 10f32.powf(-55.0 / 20.0) * (2.0 * std::f32::consts::PI * 1500.0 * i as f32 / 48000.0).sin() * (0.5 - 0.5 * (2.0 * std::f32::consts::PI * (i as f32 + 0.5) / 96.0).cos());
    }
    feed_tap(&shared, &x, |k| {
        if k == 5 {
            shared.loopback.arm_with_dc(10f32.powf(-66.0 / 20.0), [10f32.powf(-50.0 / 20.0); 2]);
        }
    });
    assert!(shared.loopback.is_done());
    let d = shared.loopback.take().unwrap();
    let pos = (d.qpc / 1000) as usize * FRAMES + d.offset as usize;
    assert!(pos >= click_at && pos < click_at + 40, "detected at {pos}, click at {click_at}");
}

#[test]
fn loopback_high_frequency_click_is_found_through_microphone_noise() {
    let mut x = mic_like(FRAMES * 60, -60.0);
    let click_at = FRAMES * 40 + 37;
    let shared = Arc::new(ProbeShared::default());
    // Baseline over 20 blocks with the high-pass on.
    shared.set_highpass(Some(16000.0));
    feed_tap(&shared, &x[..FRAMES * 30], |k| {
        if k == 0 {
            shared.monitor.start();
        }
    });
    let (raw, hp) = (shared.monitor.stop(), shared.monitor.report_hp());
    assert!(raw[2].dev_peak_db > -45.0, "raw floor {}", raw[2].dev_peak_db);
    assert!(hp[0].peak_db < -85.0, "hp floor {}", hp[0].peak_db);
    // A -55 dBFS 18.5 kHz click (the loopback level of a -35 dBFS click at -20 dB preamp).
    for i in 0..96 {
        let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * (i as f32 + 0.5) / 96.0).cos();
        x[click_at + i] += 10f32.powf(-55.0 / 20.0) * w * (2.0 * std::f32::consts::PI * 18500.0 * i as f32 / 48000.0).sin();
    }
    feed_tap(&shared, &x, |k| {
        if k == 33 {
            shared.loopback.arm_with_dc(10f32.powf(-68.0 / 20.0), [0.0, 0.0]);
        }
    });
    assert!(shared.loopback.is_done(), "no detection");
    let d = shared.loopback.take().unwrap();
    let pos = (d.qpc / 1000) as usize * FRAMES + d.offset as usize;
    // The detector's own delay (filter + RMS window) is removed with the reference bias.
    let shape = wasapi_capture::ImpulseShape::Burst { freq_hz: 18500.0, dur_ms: 2.0 };
    let bias = crate::probe::band_onset_bias_frames(shape, 48000, 16000.0, d.threshold, d.peak);
    let first = pos as f64 - bias;
    assert!((first - click_at as f64).abs() <= 3.0, "detected at {pos} (bias {bias}) -> {first}, click at {click_at}");
    let g = crate::probe::band_gain_db(shape, 48000, 16000.0);
    let expect_env = 10f32.powf((-55.0 + g as f32) / 20.0);
    assert!(d.peak > expect_env * 0.8 && d.peak < expect_env * 1.25, "env peak {} expected {}", d.peak, expect_env);
    // The same broadband content without the high-pass would have crossed even a -66 dBFS threshold at once.
    let shared = Arc::new(ProbeShared::default());
    feed_tap(&shared, &x[..FRAMES * 30], |k| {
        if k == 5 {
            shared.loopback.arm(10f32.powf(-66.0 / 20.0));
        }
    });
    assert!(shared.loopback.has_crossing());
}
