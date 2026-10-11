//! End-to-end signal-integrity regression tests of ring + rate matcher (+ drift controller),
//! measured with the offline analyzer exactly like the hardware dumps.
//!
//! They reproduce the "crackly output" investigation: with the old PI drift controller a
//! bursty capture (shared mode: 10 ms packets in pairs; exclusive: 3 ms packets with several ms
//! of scheduling jitter) modulated the resampling ratio by up to the +-0.5 % clamp, which showed
//! up as THD+N of -23..-44 dB and hundreds of "glitches" on a clean 1 kHz tone, and the 4-point
//! Catmull-Rom interpolator left a -32 dB image for a 10 kHz tone at 44.1 -> 48 kHz.

use eq_core::ResamplerQuality;
use router_core::arrival::{required_target_frames, ArrivalSnapshot, ArrivalStats, ArrivalTracker, HeadroomParams};
use router_core::analyze::{analyze_sine, format_report, SineParams};
use router_core::resample::{target_fill_frames, RateMatcher, RateMatcherConfig, RenderStats};
use router_core::ring;
use std::f64::consts::PI;

/// Deterministic xorshift in [0, 1).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Scenario {
    cable_rate: f64,
    asio_rate: f64,
    block: usize,
    packet: usize,
    /// Max extra delivery delay in seconds (late packets are followed by a catch-up burst).
    jitter_s: f64,
    /// Capture clock offset in ppm.
    ppm: f64,
    tone_hz: f64,
    amp: f64,
    secs: f64,
    safety_ms: f32,
    /// (start s, length s) of capture stalls (nothing delivered, then everything at once).
    stalls: Vec<(f64, f64)>,
    /// Deliver the packets in back-to-back pairs every 2 packet periods (the Hi-Fi Cable's pattern).
    pair: bool,
    /// Explicit target fill (cable frames) instead of the old `2 x packet` burst rule.
    target: Option<usize>,
    /// Phase of the first ASIO callback in packet periods.
    phase: f64,
    /// (time s, target frames) changes applied live through `set_target_fill`.
    schedule: Vec<(f64, usize)>,
    /// Start-up gate bias (see `RateMatcher::set_open_bias`).
    bias: usize,
    /// Alternating short/long gaps of `period x (1 -/+ alt)` (VAC fixed-period capture); 0 = regular.
    alt: f64,
    /// Measured inter-callback gaps (seconds, mean-normalised to the packet period) replayed in a loop.
    gaps: Vec<f64>,
}

impl Scenario {
    fn new(cable_rate: f64, asio_rate: f64, packet: usize, jitter_ms: f64) -> Self {
        Scenario {
            cable_rate,
            asio_rate,
            block: 512,
            packet,
            jitter_s: jitter_ms * 1e-3,
            ppm: 60.0,
            tone_hz: 1000.0,
            amp: 0.5,
            secs: 30.0,
            safety_ms: 2.0,
            stalls: Vec::new(),
            pair: false,
            target: None,
            phase: 0.37,
            schedule: Vec::new(),
            bias: 0,
            alt: 0.0,
            gaps: Vec::new(),
        }
    }
}

struct Out {
    samples: Vec<f32>,
    stats: Vec<RenderStats>,
    underruns: u64,
    rearms: u64,
    discarded: u64,
    /// Arrival statistics measured on the delivered packets (as the capture sink does live).
    snap: ArrivalSnapshot,
    /// Lowest ring fill seen at the start of a block (after the gate opened).
    min_fill: usize,
}

/// Event-driven simulation of capture packets and ASIO callbacks (see `resample.rs` tests).
fn simulate(sc: &Scenario) -> Out {
    let cap_rate = sc.cable_rate * (1.0 + sc.ppm * 1e-6);
    // The engine plans for at least two packets back to back (3 ms exclusive packets also get
    // headroom for scheduling jitter).
    let burst = (2 * sc.packet).max((sc.jitter_s * sc.cable_rate) as usize);
    let target = sc.target.unwrap_or_else(|| {
        target_fill_frames(sc.safety_ms, sc.block as u32, sc.cable_rate as u32, sc.asio_rate as u32, burst as u32)
    });
    let arr = ArrivalStats::new(sc.cable_rate);
    let mut tracker = ArrivalTracker::new(arr.clone());
    let mut sched = sc.schedule.clone();
    sched.reverse();
    let mut min_fill = usize::MAX;
    let mut group_jitter = 0.0f64;
    let (mut p, mut c) = ring::channel(target * 8 + 8192);
    let mut rm = RateMatcher::new(RateMatcherConfig::new(sc.cable_rate as u32, sc.asio_rate as u32, target, ResamplerQuality::Cubic));
    rm.set_open_bias(sc.bias);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let pkt_dt = sc.packet as f64 / cap_rate;
    let asio_dt = sc.block as f64 / sc.asio_rate;
    // Delivery time of packet k (monotone), stalls hold everything until their end.
    let mut next_deliver = 0.0f64;
    let mut k = 0u64;
    // Cumulative event times of a replayed gap log (one loop length = `loop_t`).
    let mut cum = Vec::with_capacity(sc.gaps.len());
    let mut acc = 0.0;
    for g in &sc.gaps {
        acc += g;
        cum.push(acc);
    }
    let loop_t = acc;
    let deliver_time = |k: u64, prev: f64, rng: &mut Rng, gj: &mut f64| -> f64 {
        let mut t = if !cum.is_empty() {
            let n = cum.len() as u64;
            cum[(k % n) as usize] + (k / n) as f64 * loop_t
        } else if sc.alt > 0.0 {
            let base = (k / 2 + 1) as f64 * 2.0 * pkt_dt + if k % 2 == 1 { pkt_dt * (1.0 - sc.alt) } else { 0.0 };
            base + rng.next() * sc.jitter_s
        } else if sc.pair {
            if k.is_multiple_of(2) {
                *gj = rng.next() * sc.jitter_s;
            }
            (k / 2 + 1) as f64 * 2.0 * pkt_dt + *gj
        } else {
            (k + 1) as f64 * pkt_dt + rng.next() * sc.jitter_s
        };
        for &(s, len) in &sc.stalls {
            if t >= s && t < s + len {
                t = s + len;
            }
        }
        t.max(prev)
    };
    next_deliver = deliver_time(k, next_deliver, &mut rng, &mut group_jitter);
    let mut t_asio = sc.phase * pkt_dt;
    let (mut l, mut r) = (vec![0.0f32; sc.block], vec![0.0f32; sc.block]);
    let (mut pl, mut pr) = (vec![0.0f32; sc.packet], vec![0.0f32; sc.packet]);
    let mut out = Vec::new();
    let mut stats = Vec::new();
    let mut idx = 0u64;
    while t_asio < sc.secs {
        if next_deliver <= t_asio {
            for i in 0..sc.packet {
                let v = (sc.amp * (2.0 * PI * sc.tone_hz * (idx + i as u64) as f64 / cap_rate).sin()) as f32;
                pl[i] = v;
                pr[i] = v;
            }
            idx += sc.packet as u64;
            p.push_frames(&pl, &pr);
            // a pair's second packet arrives 50 us after the first (a separate capture callback)
            tracker.on_frames(next_deliver + if sc.pair && k % 2 == 1 { 5e-5 } else { 0.0 }, sc.packet as u32);
            k += 1;
            next_deliver = deliver_time(k, next_deliver, &mut rng, &mut group_jitter);
        } else {
            while sched.last().is_some_and(|&(ts, _)| ts <= t_asio) {
                if let Some((_, tf)) = sched.pop() {
                    rm.set_target_fill(tf);
                }
            }
            let st = rm.render(&mut c, &mut l, &mut r);
            if st.fill > 0 && rm.is_running() && t_asio > 3.0 {
                min_fill = min_fill.min(st.fill);
            }
            out.extend_from_slice(&l);
            stats.push(st);
            t_asio += asio_dt;
        }
    }
    Out {
        samples: out,
        stats,
        underruns: rm.underruns(),
        rearms: rm.rearms(),
        discarded: rm.discarded_frames(),
        snap: arr.snapshot(),
        min_fill,
    }
}

fn params(min_rel: f64) -> SineParams {
    SineParams { skip_s: 4.0, tail_s: 0.5, min_rel, ..SineParams::default() }
}

fn check_clean(name: &str, sc: &Scenario, max_thdn_db: f64) {
    let o = simulate(sc);
    assert_eq!(o.underruns, 0, "{name}: underruns");
    assert_eq!(o.rearms, 0, "{name}: rearms");
    let r = analyze_sine(&o.samples, sc.asio_rate as u32, &params(5e-4));
    println!("{name}: {}", format_report("out", &r));
    assert!(r.glitches.is_empty(), "{name}: glitches\n{}", format_report("out", &r));
    assert!(r.phase_steps.is_empty(), "{name}: phase steps\n{}", format_report("out", &r));
    assert!(r.residual_rms_db < max_thdn_db, "{name}: THD+N {} dB", r.residual_rms_db);
    // The tone is defined in capture time, so the output tone sits at tone * (1 + clock offset) when the
    // ratio trim equals the offset. While the controller is still learning the offset, the trim lags
    // it; that is a *slow* pitch drift (slew limited to MAX_SLEW), never a modulation.
    let expect = sc.tone_hz * (1.0 + sc.ppm * 1e-6);
    let dev_ppm = ((r.freq_min - expect).abs().max((r.freq_max - expect).abs()) / expect) * 1e6;
    let nominal = sc.cable_rate / sc.asio_rate;
    let dt = sc.block as f64 / sc.asio_rate;
    let first = (4.0 / dt) as usize;
    let trims: Vec<f64> = o.stats.iter().skip(first).map(|s| s.ratio / nominal - 1.0).collect();
    let max_err_ppm = trims.iter().map(|t| (t - sc.ppm * 1e-6).abs()).fold(0.0, f64::max) * 1e6;
    let max_slew_ppm_s = trims.windows(2).map(|w| (w[1] - w[0]).abs() / dt).fold(0.0, f64::max) * 1e6;
    let cents = 1200.0 * (1.0 + max_err_ppm * 1e-6).log2();
    println!("{name}: freq dev {dev_ppm:.0} ppm, trim error max {max_err_ppm:.0} ppm ({cents:.2} cent), slew max {max_slew_ppm_s:.1} ppm/s");
    assert!(dev_ppm < 600.0, "{name}: freq {}..{} (expected {expect})", r.freq_min, r.freq_max);
    assert!(max_err_ppm < 600.0, "{name}: trim error {max_err_ppm} ppm");
    assert!(max_slew_ppm_s <= 40.5, "{name}: ratio slew {max_slew_ppm_s} ppm/s");
}

#[test]
fn shared_mode_441_packets_44k1_to_48k() {
    check_clean("441x2 bursts", &Scenario::new(44100.0, 48000.0, 441, 9.0), -78.0);
}

#[test]
fn shared_mode_882_bursts_44k1_to_48k() {
    check_clean("882", &Scenario::new(44100.0, 48000.0, 882, 4.0), -78.0);
}

#[test]
fn exclusive_mode_132_packets_with_jitter_44k1_to_48k() {
    let mut sc = Scenario::new(44100.0, 48000.0, 132, 4.0);
    sc.safety_ms = 8.0; // headroom for 4 ms of scheduling jitter
    check_clean("132 jitter 4 ms", &sc, -78.0);
}

#[test]
fn same_rate_480_packets_48k() {
    check_clean("480 @48k", &Scenario::new(48000.0, 48000.0, 480, 3.0), -78.0);
}

#[test]
fn clock_offsets_both_directions() {
    for ppm in [-180.0, 180.0] {
        let mut sc = Scenario::new(44100.0, 48000.0, 441, 6.0);
        sc.ppm = ppm;
        sc.secs = 40.0;
        check_clean(&format!("{ppm:+} ppm"), &sc, -76.0);
    }
}

#[test]
fn ten_khz_tone_has_no_image_44k1_to_48k() {
    let mut sc = Scenario::new(44100.0, 48000.0, 441, 6.0);
    sc.tone_hz = 10_000.0;
    sc.amp = 0.3;
    let o = simulate(&sc);
    assert_eq!(o.underruns, 0);
    // The analysis fits a slowly chirping 10 kHz tone; judge the image level directly instead:
    // after the sine fit the residual stays below -60 dB (Catmull-Rom gave about -30 dB).
    let r = analyze_sine(&o.samples, 48000, &params(5e-3));
    println!("{}", format_report("10k", &r));
    assert!(r.residual_rms_db < -60.0, "10 kHz THD+N {} dB", r.residual_rms_db);
}

/// A capture stall long enough to starve the ring: the output fades, holds and re-arms; after the
/// recovery the stream is clean again and the fade itself contains no jump larger than the tone.
#[test]
fn stall_and_gate_rearm_recover_cleanly() {
    let mut sc = Scenario::new(44100.0, 48000.0, 441, 2.0);
    sc.stalls = vec![(12.0, 0.25)];
    sc.secs = 30.0;
    let o = simulate(&sc);
    assert!(o.underruns >= 1, "the stall must starve the ring");
    assert!(o.rearms >= 1, "and re-arm the gate");
    let rate = 48000.0;
    // Neighbourhood of the stall: no sample-to-sample jump beyond the tone's own maximum.
    let a = (11.9 * rate) as usize;
    let b = (13.5 * rate) as usize;
    let max_step = 0.5 * 2.0 * (PI * 1000.0 / rate).sin();
    let worst = o.samples[a..b].windows(2).map(|w| f64::from((w[1] - w[0]).abs())).fold(0.0, f64::max);
    assert!(worst <= max_step * 1.05, "jump {worst} > {max_step} during stall/recovery");
    assert!(o.samples[a..b].iter().all(|v| v.abs() <= 0.5 * 1.01), "no overshoot");
    // Well after the recovery (gate fade-in 256 frames + drift re-lock) the tone is clean.
    let tail: Vec<f32> = o.samples[(16.0 * rate) as usize..].to_vec();
    let r = analyze_sine(&tail, 48000, &SineParams { skip_s: 1.0, tail_s: 0.5, ..SineParams::default() });
    println!("{}", format_report("after stall", &r));
    assert!(r.clean(), "{}", format_report("after stall", &r));
    assert!(r.residual_rms_db < -70.0, "THD+N {}", r.residual_rms_db);
    // After the stall everything that piled up is delivered at once (a whole stall's worth of
    // audio, ~11k frames), so a large trim is inevitable. It must be inaudible: either done while
    // the gate is closed / output is silent (the re-arm path, followed by a fade-in), or crossfaded.
    // The jump check above already covers the whole neighbourhood; here make sure the trim happened
    // exactly once, at the recovery, and not repeatedly.
    let trims = o.stats.iter().filter(|s| s.discarded > 0).count();
    println!("stall: discarded {} frames in {trims} blocks", o.discarded);
    assert!(trims <= 2, "{trims} discarding blocks");
    assert!(o.discarded < 12_000, "discarded {}", o.discarded);
}

/// Steady state must not discard or re-arm: any discard is a waveform discontinuity.
#[test]
fn no_midstream_discards_in_normal_operation() {
    for (packet, jitter) in [(441usize, 9.0), (132, 4.0), (882, 3.0)] {
        let mut sc = Scenario::new(44100.0, 48000.0, packet, jitter);
        sc.safety_ms = 8.0;
        let o = simulate(&sc);
        let discards_after_start: usize = o.stats.iter().skip(200).map(|s| s.discarded).sum();
        assert_eq!(discards_after_start, 0, "packet {packet}");
        assert_eq!(o.rearms, 0);
    }
}

// ---------------------------------------------------------------------------------------------
// Measured-arrival target (see `router_core::arrival`): the Hi-Fi Cable delivers two packets back
// to back every 20 ms. The target is derived from the measured arrival pattern instead of the
// former fixed `2 x packet` burst term, then proven by simulation.
// ---------------------------------------------------------------------------------------------

/// Measures the arrival pattern of `sc` (a long enough pass, big target so nothing underruns) and
/// returns the lowest safe target for it plus the start-up bias (the measured dip).
fn measured_target(sc: &Scenario) -> (usize, usize, ArrivalSnapshot) {
    let mut m = Scenario::new(sc.cable_rate, sc.asio_rate, sc.packet, sc.jitter_s * 1e3);
    m.pair = sc.pair;
    m.alt = sc.alt;
    m.gaps = sc.gaps.clone();
    m.block = sc.block;
    m.ppm = sc.ppm;
    m.secs = 35.0;
    m.target = Some(6000);
    m.bias = sc.packet;
    let o = simulate(&m);
    let p = HeadroomParams { period_frames: sc.packet as u32, cap_frames: 4096 };
    let t = required_target_frames(&o.snap, sc.block as u32, sc.cable_rate as u32, sc.asio_rate as u32, sc.safety_ms, &p);
    (t, o.snap.dip_p999 as usize, o.snap)
}

fn paired(cable: f64, asio: f64, packet: usize, jitter_ms: f64) -> Scenario {
    let mut sc = Scenario::new(cable, asio, packet, jitter_ms);
    sc.pair = true;
    sc
}

fn assert_clean_output(name: &str, sc: &Scenario, o: &Out, max_thdn_db: f64) {
    assert_eq!(o.underruns, 0, "{name}: underruns");
    assert_eq!(o.rearms, 0, "{name}: rearms");
    let after_start: usize = o.stats.iter().skip(200).map(|s| s.discarded).sum();
    assert_eq!(after_start, 0, "{name}: discard jumps after start-up");
    let r = analyze_sine(&o.samples, sc.asio_rate as u32, &params(5e-4));
    println!("{name}: {}", format_report("out", &r));
    assert!(r.glitches.is_empty(), "{name}: glitches
{}", format_report("out", &r));
    assert!(r.phase_steps.is_empty(), "{name}: phase steps
{}", format_report("out", &r));
    assert!(r.residual_rms_db < max_thdn_db, "{name}: THD+N {} dB", r.residual_rms_db);
}

fn check_measured_pairs(cable: f64, asio: f64, packet: usize, jitter_ms: f64) {
    let base = paired(cable, asio, packet, jitter_ms);
    let (target, dip, snap) = measured_target(&base);
    let old = target_fill_frames(base.safety_ms, 512, cable as u32, asio as u32, 2 * packet as u32);
    println!(
        "pairs {packet}@{cable}->{asio} jitter {jitter_ms} ms: dip p99.9 {dip} fr, gap p99.9 {} fr, target {target} fr ({:.1} ms) vs old {old} fr ({:.1} ms)",
        snap.gap_p999_frames,
        target as f64 * 1e3 / cable,
        old as f64 * 1e3 / cable
    );
    assert!(target + 250 <= old, "the measured target must be well below the old fixed rule: {target} vs {old}");
    // The real requirement is block + dip: the target keeps the 25 % margin and the 2 ms safety on top.
    let block_c = (512.0 * cable / asio).ceil() as usize;
    assert!(target >= block_c + dip, "target {target} below block {block_c} + dip {dip}");
    for phase in [0.05, 0.4, 0.8] {
        let mut sc = paired(cable, asio, packet, jitter_ms);
        sc.target = Some(target);
        sc.bias = dip;
        sc.phase = phase;
        let o = simulate(&sc);
        println!("  phase {phase}: min fill {} (block {block_c})", o.min_fill);
        assert!(o.min_fill >= block_c, "fill dipped below one block");
        assert_clean_output(&format!("pairs {packet} phase {phase}"), &sc, &o, -76.0);
    }
}

#[test]
fn measured_target_paired_bursts_44k1_to_48k() {
    check_measured_pairs(44100.0, 48000.0, 441, 2.0);
}

#[test]
fn measured_target_paired_bursts_48k_to_48k() {
    check_measured_pairs(48000.0, 48000.0, 480, 2.0);
}

/// The derived bound is not vacuous: well below `block + dip` the very same stream underruns.
#[test]
fn target_below_the_derived_bound_underruns() {
    let mut sc = paired(48000.0, 48000.0, 480, 0.0);
    sc.secs = 20.0;
    sc.bias = 480;
    sc.target = Some(512 + 200); // block + 0.4 x dip
    let o = simulate(&sc);
    assert!(o.underruns > 0, "a target of block + 200 cannot survive 480-frame dips");
    // ... while block + dip (no margin at all) is just about enough in steady state:
    let mut sc2 = paired(48000.0, 48000.0, 480, 0.0);
    sc2.secs = 60.0;
    sc2.bias = 480;
    sc2.target = Some(512 + 480 + 2 + 48);
    let o2 = simulate(&sc2);
    println!("block+dip+48: underruns {} min fill {}", o2.underruns, o2.min_fill);
    assert!(o2.min_fill >= 512, "min fill {}", o2.min_fill);
}

/// Live lowering of the target (the engine's planner steps down 48 frames every 2 s once the
/// statistics are warm) must be absorbed by the drift controller: no discard, no glitch.
#[test]
fn live_target_ramp_down_is_inaudible() {
    let mut sc = paired(44100.0, 48000.0, 441, 2.0);
    sc.secs = 70.0;
    sc.bias = 441;
    let start = target_fill_frames(2.0, 512, 44100, 48000, 882);
    let (final_t, _, _) = measured_target(&sc);
    sc.target = Some(start);
    let mut t = start;
    let mut when = 12.0;
    while t > final_t {
        t = final_t.max(t.saturating_sub(48));
        sc.schedule.push((when, t));
        when += 2.0;
    }
    let o = simulate(&sc);
    let tail_fill: f64 = o.stats.iter().rev().take(2000).map(|s| s.fill as f64).sum::<f64>() / 2000.0;
    println!("ramp {start} -> {final_t}: final mean fill {tail_fill:.0}, {} schedule steps", sc.schedule.len());
    assert!(tail_fill < start as f64 - 100.0, "fill must have followed the target down");
    let a = analyze_sine(&o.samples, 48000, &params(5e-4));
    let jumps: usize = o.stats.iter().skip(200).map(|s| s.discarded).sum();
    assert_eq!((o.underruns, o.rearms, jumps), (0, 0, 0), "{}", format_report("ramp", &a));
    assert!(a.glitches.is_empty() && a.phase_steps.is_empty(), "{}", format_report("ramp", &a));
    assert!(a.residual_rms_db < -76.0, "THD+N {}", a.residual_rms_db);
}

// ---------------------------------------------------------------------------------------------
// VAC "Line 1" sources (WP2): a fixed-period capture delivers small packets with alternating
// short/long gaps (288 frames: mean 6 ms), the default Shared capture regular 480-frame packets
// with ~1 ms scheduling spread. The engine starts from `initial_headroom_frames` and the measured
// planner takes over; both must be clean on a 256-frame ASIO block (the Yamaha).
// ---------------------------------------------------------------------------------------------

const OLD_TARGET_256: usize = 96 + 256 + 960; // the 1312-frame target of the old burst assumption

fn vac_scenario(packet: usize, jitter_ms: f64, alt: f64) -> Scenario {
    let mut sc = Scenario::new(48000.0, 48000.0, packet, jitter_ms);
    sc.block = 256;
    sc.alt = alt;
    sc.ppm = 40.0;
    sc
}

/// Target the engine uses for a given start headroom.
fn vac_start_target(sc: &Scenario, headroom: usize) -> usize {
    target_fill_frames(sc.safety_ms, sc.block as u32, sc.cable_rate as u32, sc.asio_rate as u32, headroom as u32)
}

fn run_phases(name: &str, base: &Scenario, target: usize, bias: usize, thdn: f64) {
    for phase in [0.05, 0.4, 0.8] {
        let mut sc = vac_scenario(base.packet, base.jitter_s * 1e3, base.alt);
        sc.gaps = base.gaps.clone();
        sc.secs = base.secs;
        sc.ppm = base.ppm;
        sc.phase = phase;
        sc.target = Some(target);
        sc.bias = bias;
        let o = simulate(&sc);
        println!("  {name} phase {phase}: min fill {} (block {}), underruns {}", o.min_fill, sc.block, o.underruns);
        assert!(o.min_fill >= sc.block, "{name}: fill dipped below one block");
        assert_clean_output(&format!("{name} phase {phase}"), &sc, &o, thdn);
    }
}

#[test]
fn vac_fixed_288_alternating_gaps_start_target_is_clean() {
    let base = vac_scenario(288, 0.3, 0.25); // gaps 4.5 / 7.5 ms (+ up to 0.3 ms jitter)
    let headroom = router_core::arrival::initial_headroom_frames(288, true);
    let target = vac_start_target(&base, headroom);
    println!("288 alt: start target {target} fr vs old {OLD_TARGET_256} fr");
    assert!(target + 400 <= OLD_TARGET_256);
    run_phases("288 alt start", &base, target, (headroom as f64 / 1.25) as usize, -76.0);
}

#[test]
fn vac_fixed_288_alternating_gaps_measured_target_is_clean_and_lower() {
    let base = vac_scenario(288, 0.3, 0.25);
    let (target, dip, snap) = measured_target(&base);
    let start = vac_start_target(&base, router_core::arrival::initial_headroom_frames(288, true));
    println!(
        "288 alt: dip p99.9 {dip} fr, gap p99.9 {} fr, measured target {target} fr (start {start}, old {OLD_TARGET_256})",
        snap.gap_p999_frames
    );
    assert!(target <= start, "measured {target} must not exceed the start target {start}");
    assert!(target >= base.block + dip, "target below block + dip");
    run_phases("288 alt measured", &base, target, dip, -76.0);
}

#[test]
fn vac_fixed_240_alternating_gaps_3_7_ms() {
    let base = vac_scenario(240, 0.3, 0.4); // 3.0 / 7.0 ms
    let headroom = router_core::arrival::initial_headroom_frames(240, true);
    let target = vac_start_target(&base, headroom);
    run_phases("240 alt start", &base, target, (headroom as f64 / 1.25) as usize, -76.0);
}

#[test]
fn vac_shared_480_regular_packets_start_and_measured() {
    let base = vac_scenario(480, 2.5, 0.0); // regular 10 ms packets, up to 2.5 ms late
    // Shared (not fixed-period): the engine keeps the conservative 2 x period start headroom.
    let headroom = router_core::arrival::initial_headroom_frames(480, false);
    assert_eq!(headroom, 960);
    run_phases("480 start", &base, vac_start_target(&base, headroom), (headroom as f64 / 1.25) as usize, -76.0);
    let (target, dip, _) = measured_target(&base);
    println!("480 regular: dip {dip} fr, measured target {target} fr (old {OLD_TARGET_256})");
    assert!(target + 250 <= OLD_TARGET_256);
    run_phases("480 measured", &base, target, dip, -76.0);
}

/// Parses the `GAP <microseconds>` lines of a capture-probe gap log (stderr dump).
fn read_gap_log(path: &std::path::Path) -> Vec<f64> {
    let text = match std::fs::read(path) {
        Ok(b) if b.len() > 2 && b[0] == 0xFF && b[1] == 0xFE => {
            let u: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
            String::from_utf16_lossy(&u)
        }
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) => return Vec::new(),
    };
    text.lines()
        .filter_map(|l| l.split("GAP ").nth(1))
        .filter_map(|v| v.split_whitespace().next().and_then(|n| n.parse::<f64>().ok()))
        .map(|us| us * 1e-6)
        .collect()
}

/// Replays the gap sequences measured on VAC lite at a 240-frame (5 ms) capture period
/// (`target/vac_spike/gaps_low_240_*.txt`, bimodal ~3 / ~6-7 ms) when present; otherwise the same
/// bimodal pattern is synthesised. Either way the engine's start target must run clean.
#[test]
fn vac_measured_240_gap_log_replay() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/vac_spike");
    let mut logs: Vec<Vec<f64>> = ["gaps_low_240_tone.txt", "gaps_low_240_ka.txt"]
        .iter()
        .map(|f| read_gap_log(&dir.join(f)))
        .filter(|g| g.len() > 1000)
        .collect();
    let synthesised = logs.is_empty();
    if synthesised {
        println!("no measured gap logs found: synthesising alternating 3.0 / 7.0 ms gaps");
        logs.push((0..2400).map(|i| if i % 2 == 0 { 0.003 } else { 0.007 }).collect());
    }
    for (i, g) in logs.iter().enumerate() {
        let mean = g.iter().sum::<f64>() / g.len() as f64;
        let nominal = 240.0 / 48000.0;
        // Normalise the mean to the nominal period (the probe's wall clock differs from the stream
        // clock by some hundred ppm; the drift controller handles that in the real engine).
        let gaps: Vec<f64> = g.iter().map(|x| x * nominal / mean).collect();
        let max_gap = gaps.iter().cloned().fold(0.0, f64::max);
        let mut base = vac_scenario(240, 0.0, 0.0);
        base.gaps = gaps;
        base.ppm = 0.0;
        base.secs = 11.0;
        // The requirement measured on the log itself (big target so nothing underruns).
        let mut m = vac_scenario(240, 0.0, 0.0);
        m.gaps = base.gaps.clone();
        m.ppm = 0.0;
        m.secs = 11.0;
        m.target = Some(6000);
        m.bias = 240;
        let o = simulate(&m);
        let p = HeadroomParams { period_frames: 240, cap_frames: 4096 };
        let req_t = required_target_frames(&o.snap, 256, 48000, 48000, 2.0, &p);
        let headroom = router_core::arrival::initial_headroom_frames(240, true);
        let start = vac_start_target(&base, headroom);
        println!(
            "gap log {i} ({}): {} gaps, max {:.1} ms, dip p99.9 {} fr, required target {req_t} fr, start target {start} fr, old {OLD_TARGET_256} fr",
            if synthesised { "synthetic" } else { "measured" },
            base.gaps.len(),
            max_gap * 1e3,
            o.snap.dip_p999
        );
        // The start target (1.5 x period headroom) covers the typical pattern (p99.9 gap 7.6 ms). A
        // run that contains a one-off 11 ms scheduling hiccup needs more: that is the planner's job
        // (raises at once after 100 events); the target it converges to must be clean.
        if req_t > start {
            println!("  log {i}: start target {start} fr is below the measured requirement {req_t} fr: the planner raises it");
            assert!(max_gap > 0.009, "only a log with a long outlier gap may exceed the start target");
        }
        let target = start.max(req_t);
        if max_gap > 0.009 {
            // A single 11 ms gap in 12 s is far above the p99.9 the model dimensions for (it sees ~2
            // events of 2400): report the damage instead of pretending the target covers it.
            let mut sc = vac_scenario(240, 0.0, 0.0);
            sc.gaps = base.gaps.clone();
            sc.ppm = 0.0;
            sc.secs = 11.0;
            sc.target = Some(target);
            sc.bias = (headroom as f64 / 1.25) as usize;
            let o = simulate(&sc);
            println!("  log {i}: with a one-off {:.1} ms gap: {} underrun block(s) at target {target} fr", max_gap * 1e3, o.underruns);
            assert!(o.underruns <= 15, "a single outlier gap must not cause a storm of underruns");
            continue;
        }
        run_phases(&format!("240 log {i}"), &base, target, (headroom as f64 / 1.25) as usize, -70.0);
    }
}

/// Exploration (not part of the suite): minimal target per arrival pattern.
#[test]
#[ignore]
fn explore_minimal_targets() {
    for (cr, ar, pk) in [(44100.0, 48000.0, 441usize), (48000.0, 48000.0, 480)] {
        for jit in [0.0, 1.0, 3.0] {
            let mut sc = Scenario::new(cr, ar, pk, jit);
            sc.pair = true;
            sc.secs = 40.0;
            sc.target = Some(5000);
            let o = simulate(&sc);
            let p = HeadroomParams { period_frames: pk as u32, cap_frames: 4096 };
            let req = required_target_frames(&o.snap, 512, cr as u32, ar as u32, 2.0, &p);
            let nosafety = required_target_frames(&o.snap, 512, cr as u32, ar as u32, 0.0, &p);
            let old = target_fill_frames(2.0, 512, cr as u32, ar as u32, 2 * pk as u32);
            let ok = |t: usize, bias: usize| -> bool {
                [0.05, 0.3, 0.55, 0.8, 0.97].iter().all(|&ph| {
                    let mut s = Scenario::new(cr, ar, pk, jit);
                    s.pair = true;
                    s.secs = 40.0;
                    s.phase = ph;
                    s.target = Some(t);
                    s.bias = bias;
                    let o = simulate(&s);
                    o.underruns == 0 && o.rearms == 0
                })
            };
            let find = |bias: usize| {
                let (mut lo, mut hi) = (200usize, old + 400);
                while hi - lo > 8 {
                    let mid = (lo + hi) / 2;
                    if ok(mid, bias) { hi = mid } else { lo = mid }
                }
                hi
            };
            println!(
                "cable {cr} jit {jit} ms: dip_p999 {} gap {}
  old {old} fr, computed {req} fr (no safety {nosafety}), minimal 0-underrun: no bias {} / bias=packet {}",
                o.snap.dip_p999, o.snap.gap_p999_frames, find(0), find(pk)
            );
        }
    }
}

/// Exploration (not part of the suite): fill statistics of the VAC 288 start for several gate biases.
#[test]
#[ignore]
fn explore_open_bias_288() {
    let base = vac_scenario(288, 0.3, 0.25);
    let headroom = router_core::arrival::initial_headroom_frames(288, true);
    let target = vac_start_target(&base, headroom);
    let full = (headroom as f64 / 1.25) as usize;
    for frac in [0.0, 0.25, 0.5, 0.75, 1.0] {
        let bias = (full as f64 * frac) as usize;
        for phase in [0.05, 0.4, 0.8] {
            let mut sc = vac_scenario(288, 0.3, 0.25);
            sc.secs = 60.0;
            sc.phase = phase;
            sc.target = Some(target);
            sc.bias = bias;
            let o = simulate(&sc);
            let mean = |a: f64, b: f64| {
                let dt = 256.0 / 48000.0;
                let v: Vec<f64> = o.stats.iter().enumerate().filter(|(i, s)| { let t = *i as f64 * dt; t >= a && t < b && s.fill > 0 }).map(|(_, s)| s.fill as f64).collect();
                v.iter().sum::<f64>() / v.len().max(1) as f64
            };
            let minf = o.stats.iter().filter(|s| s.fill > 0 && !s.gated).map(|s| s.fill).min().unwrap_or(0);
            println!("bias {bias:4} phase {phase}: target {target} mean 1-5s {:.0} 5-10s {:.0} 10-20s {:.0} 20-40s {:.0} 40-60s {:.0} min {minf} (3s+ {}) under {} rearm {}", mean(1.0,5.0), mean(5.0,10.0), mean(10.0,20.0), mean(20.0,40.0), mean(40.0,60.0), o.min_fill, o.underruns, o.rearms);
        }
    }
}
