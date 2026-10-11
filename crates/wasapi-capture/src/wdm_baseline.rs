//! No-router latency baseline (`capture-probe wdm-baseline`): the same quiet 18.5 kHz burst that
//! `router-cli latency` uses, rendered WITHOUT the router straight into a Windows (WDM) render endpoint by an
//! ordinary shared-mode client and detected on a WDM capture endpoint that carries a device-side loopback of that
//! output (the Yamaha "Streaming" capture endpoint). The result is the latency of Windows' own path, to compare
//! with the router chain's figure.
//!
//! Everything here is pure (no COM) and unit-tested: argument validation and the safety gates, the recording sink,
//! the band-limited detector (the shared [`crate::band`] high-pass + RMS envelope, threshold = floor + 12 dB, onset
//! bias correction), the statistics, the router comparison and a synthetic `--selftest`.
//!
//! Timing model: the capture packet's QPC is taken at callback entry; the packet's LAST frame is anchored to that
//! instant (exactly as the router probe anchors an ASIO block), so frame `i` of a packet of `n` frames starting at
//! frame `s` is at `qpc - (s + n - i) / rate`. The time the packet waited between being captured by the device and
//! delivered to the callback (up to one capture period plus the stream latency) is NOT known: it is the
//! "capture-side buffering estimate" that widens the comparison range.

use crate::band::{band_gain_db, band_onset_bias_frames, HighPass};
use crate::impulse::{click_samples, is_sink_allowed, validate_peak_dbfs, ImpulseShape, DEFAULT_IMPULSE_DBFS};
use crate::session::FrameSink;
use std::sync::{Arc, Mutex};

/// Default render sink (the Yamaha WDM "Music" endpoint). It is never used unless named in `--allow-sink`.
pub const DEFAULT_SINK: &str = "Music (Steinberg UR22C)";
/// Default capture endpoint (the Yamaha WDM "Streaming" capture: device-side loopback of the output mix + live inputs).
pub const DEFAULT_CAPTURE: &str = "Streaming (Steinberg UR22C)";
/// The only burst shape accepted: the 18.5 kHz high-pass detector is what works through the microphone noise.
pub const SHAPE: ImpulseShape = ImpulseShape::Burst { freq_hz: 18500.0, dur_ms: 2.0 };
/// Detection high-pass corner for [`SHAPE`] (`router-cli latency` derives the same 15 750 Hz).
pub const HP_HZ: f32 = 15750.0;
pub const MAX_COUNT: u32 = 10;
pub const MIN_INTERVAL_MS: f32 = 1200.0;
pub const MAX_INTERVAL_MS: f32 = 10_000.0;
/// Hard limit of the whole live run (seconds).
pub const MAX_TOTAL_S: f64 = 20.0;
/// Windows endpoint master volume above which `--confirm-knob-down` is required.
pub const KNOB_SCALAR_LIMIT: f32 = 0.5;
/// Seconds of capture used for the baseline floor, taken after [`BASELINE_WARMUP_S`] of filter start-up.
pub const BASELINE_S: f64 = 0.7;
pub const BASELINE_WARMUP_S: f64 = 0.1;
/// Time the live run waits (capture running, nothing rendered) before the baseline is evaluated.
pub const PRE_BASELINE_WAIT_S: f64 = BASELINE_WARMUP_S + BASELINE_S + 0.1;
/// Render start-up and settling between the baseline and the first click.
pub const RENDER_SETTLE_S: f64 = 0.7;
/// Time after the last click before the streams are stopped.
pub const TAIL_S: f64 = 0.6;
/// Margin of the threshold above the measured floor (dB), as in the router tool.
pub const FLOOR_MARGIN_DB: f64 = 12.0;
/// The threshold must stay at least this far below the expected click envelope (dB).
pub const CLICK_HEADROOM_DB: f64 = 6.0;
/// The threshold is never more sensitive than this far below the expected click envelope (dB).
pub const MIN_BELOW_CLICK_DB: f64 = 25.0;
/// Raw (unfiltered) capture deviation above this in the baseline is programme audio: abort (dBFS).
pub const RAW_CONTAMINATION_DBFS: f64 = -20.0;
/// Frames after the first threshold crossing scanned for the envelope peak (the onset-bias reference), seconds.
pub const PEAK_SCAN_S: f64 = 0.02;
/// A click is looked for at most this long after its injection (seconds).
pub const MAX_LATENCY_S: f64 = 1.0;

/// Names of the flags that take a value / that are switches. Anything else is refused (no render-profile flags).
const VALUE_FLAGS: [&str; 8] = ["--target", "--allow-sink", "--device", "--count", "--interval-ms", "--db", "--shape", "--router-total"];
const SWITCH_FLAGS: [&str; 2] = ["--confirm-knob-down", "--selftest"];

/// Validated arguments of `wdm-baseline`.
#[derive(Clone, Debug, PartialEq)]
pub struct WdmArgs {
    pub sink: String,
    pub allow: Vec<String>,
    pub capture: String,
    pub count: u32,
    pub interval_ms: f32,
    pub db: f32,
    pub confirm_knob_down: bool,
    pub selftest: bool,
    pub router_total_ms: f64,
}

/// Parses and validates the arguments after the sub-command name. Pure; every hard limit is checked here.
pub fn parse_args(args: &[String]) -> Result<WdmArgs, String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if VALUE_FLAGS.contains(&a) {
            if i + 1 >= args.len() {
                return Err(format!("{a} needs a value"));
            }
            i += 2;
        } else if SWITCH_FLAGS.contains(&a) {
            i += 1;
        } else {
            return Err(format!(
                "unknown argument \"{a}\" (wdm-baseline accepts only {}, {})",
                VALUE_FLAGS.join(" "),
                SWITCH_FLAGS.join(" ")
            ));
        }
    }
    let val = |f: &str| args.iter().position(|a| a == f).and_then(|p| args.get(p + 1)).cloned();
    let has = |f: &str| args.iter().any(|a| a == f);
    let num = |f: &str, d: f64| -> Result<f64, String> {
        match val(f) {
            Some(s) => s.parse::<f64>().map_err(|_| format!("{f}: \"{s}\" is not a number")),
            None => Ok(d),
        }
    };
    if let Some(s) = val("--shape") {
        match ImpulseShape::parse(&s) {
            Some(sh) if sh == SHAPE => {}
            _ => return Err(format!("--shape \"{s}\" refused: wdm-baseline only runs burst:18500:2 (the 18.5 kHz high-pass detector is what works through the microphone noise)")),
        }
    }
    let count_f = num("--count", 5.0)?;
    if count_f.fract() != 0.0 || !(1.0..=f64::from(MAX_COUNT)).contains(&count_f) {
        return Err(format!("--count must be a whole number 1..={MAX_COUNT} (got {count_f})"));
    }
    let count = count_f as u32;
    let interval = num("--interval-ms", 1500.0)? as f32;
    if !interval.is_finite() || interval < MIN_INTERVAL_MS || interval > MAX_INTERVAL_MS {
        return Err(format!("--interval-ms must be {MIN_INTERVAL_MS:.0}..={MAX_INTERVAL_MS:.0} (got {interval})"));
    }
    let db = num("--db", f64::from(DEFAULT_IMPULSE_DBFS))? as f32;
    validate_peak_dbfs(db)?;
    let total = total_run_seconds(count, interval);
    if total > MAX_TOTAL_S {
        return Err(format!("the run would take {total:.1} s, above the {MAX_TOTAL_S:.0} s limit: use fewer clicks or a shorter interval"));
    }
    let router_total_ms = num("--router-total", 50.2)?;
    if !(router_total_ms.is_finite() && router_total_ms > 0.0 && router_total_ms < 1000.0) {
        return Err("--router-total must be a latency in ms (0..1000)".into());
    }
    let sink = val("--target").unwrap_or_else(|| DEFAULT_SINK.to_string());
    let allow: Vec<String> = val("--allow-sink").into_iter().collect();
    let selftest = has("--selftest");
    if !selftest {
        if allow.iter().all(|a| a.trim().is_empty()) {
            return Err(format!(
                "refusing to render into \"{sink}\": it is a real output device, not a virtual cable. Pass it explicitly with --allow-sink \"{DEFAULT_SINK}\" (and lower the Yamaha headphone knob first)"
            ));
        }
        if !is_sink_allowed(&sink, &allow) {
            return Err(format!("sink \"{sink}\" is not covered by --allow-sink {allow:?}; refusing"));
        }
    }
    Ok(WdmArgs {
        sink,
        allow,
        capture: val("--device").unwrap_or_else(|| DEFAULT_CAPTURE.to_string()),
        count,
        interval_ms: interval,
        db,
        confirm_knob_down: has("--confirm-knob-down"),
        selftest,
        router_total_ms,
    })
}

/// Wall-clock length of a live run: capture start-up and baseline, render settling, the clicks, the tail.
pub fn total_run_seconds(count: u32, interval_ms: f32) -> f64 {
    PRE_BASELINE_WAIT_S + RENDER_SETTLE_S + f64::from(count.saturating_sub(1)) * f64::from(interval_ms) / 1000.0 + TAIL_S
}

/// Result of the physical-volume gate.
#[derive(Clone, Debug, PartialEq)]
pub enum KnobGate {
    Proceed,
    /// Nothing may be rendered; the caller prints the text and exits with status 2.
    Refuse(String),
}

/// Decides whether rendering may start from the endpoint's Windows master volume / mute (read-only values;
/// `None` = could not be read, treated like a high volume). Windows volume and mute are never changed.
pub fn knob_gate(volume: Option<(f32, bool)>, confirm_knob_down: bool) -> KnobGate {
    match volume {
        Some((_, true)) => KnobGate::Refuse(
            "the Windows endpoint is MUTED: the click would not be audible and the loopback would carry nothing. Nothing was rendered. Unmute it yourself (this tool never changes volume or mute) and run again".into(),
        ),
        Some((v, false)) if v <= KNOB_SCALAR_LIMIT => KnobGate::Proceed,
        Some((_, false)) if confirm_knob_down => KnobGate::Proceed,
        Some((v, false)) => KnobGate::Refuse(format!(
            "the Windows endpoint volume is {v:.3} (above {KNOB_SCALAR_LIMIT}). Nothing was rendered. Lower the PHYSICAL Yamaha headphone knob first (this tool never changes the Windows volume), then run again with --confirm-knob-down"
        )),
        None if confirm_knob_down => KnobGate::Proceed,
        None => KnobGate::Refuse(
            "the Windows endpoint volume could not be read. Nothing was rendered. Lower the PHYSICAL Yamaha headphone knob first, then run again with --confirm-knob-down".into(),
        ),
    }
}

// ------------------------------------------------------------------------------------
// Recording
// ------------------------------------------------------------------------------------

/// One delivered capture packet: QPC at callback entry, index of its first frame in the recording, frame count.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PacketRec {
    pub qpc: i64,
    pub start: usize,
    pub frames: usize,
}

/// Captured audio plus the packet table.
#[derive(Clone, Debug, Default)]
pub struct Recording {
    pub rate: u32,
    pub l: Vec<f32>,
    pub r: Vec<f32>,
    pub packets: Vec<PacketRec>,
}

impl Recording {
    pub fn with_capacity(rate: u32, seconds: f64) -> Self {
        let n = (f64::from(rate) * seconds) as usize + 4096;
        Recording { rate, l: Vec::with_capacity(n), r: Vec::with_capacity(n), packets: Vec::with_capacity(4096) }
    }

    /// QPC of frame `i`, packet end anchored at the callback instant. `None` if `i` is outside the recording.
    pub fn frame_qpc(&self, i: usize, hz: i64) -> Option<i64> {
        let k = self.packets.partition_point(|p| p.start + p.frames <= i);
        let p = self.packets.get(k).filter(|p| p.start <= i)?;
        let behind = (p.start + p.frames - i) as f64 / f64::from(self.rate.max(1));
        Some(p.qpc - (behind * hz as f64).round() as i64)
    }
}

/// [`FrameSink`] that records everything with a QPC per packet (taken at callback entry).
pub struct RecordingSink {
    pub rec: Arc<Mutex<Recording>>,
    pub now: fn() -> i64,
}

impl RecordingSink {
    pub fn new(rec: Arc<Mutex<Recording>>) -> Self {
        RecordingSink { rec, now: crate::impulse::qpc_ticks }
    }
}

impl FrameSink for RecordingSink {
    fn on_packet(&mut self, frames: u32, _flags: crate::PacketFlags) {
        let qpc = (self.now)();
        let mut r = self.rec.lock().unwrap_or_else(|e| e.into_inner());
        let start = r.l.len();
        r.packets.push(PacketRec { qpc, start, frames: frames as usize });
    }
    fn on_frames(&mut self, l: &[f32], r: &[f32]) {
        let mut g = self.rec.lock().unwrap_or_else(|e| e.into_inner());
        g.l.extend_from_slice(l);
        g.r.extend_from_slice(r);
    }
}

// ------------------------------------------------------------------------------------
// Detector
// ------------------------------------------------------------------------------------

/// The band-limited detector's signal for the whole recording: per frame the larger of the two channels' envelopes
/// (8th-order Butterworth high-pass at `fc_hz`, then a 32-sample sliding RMS). Causal, so the envelope of a prefix
/// equals the prefix of the envelope.
pub fn envelope(rec: &Recording, fc_hz: f32) -> Vec<f32> {
    let n = rec.l.len().min(rec.r.len());
    let mut hp = HighPass::new(fc_hz, rec.rate);
    let (mut el, mut er) = (vec![0.0f32; n], vec![0.0f32; n]);
    hp.process_env(0, &rec.l[..n], &mut el);
    hp.process_env(1, &rec.r[..n], &mut er);
    el.iter().zip(&er).map(|(a, b)| a.max(*b)).collect()
}

fn lin_db(x: f64) -> f64 {
    if x <= 1e-10 {
        -200.0
    } else {
        20.0 * x.log10()
    }
}

/// Nearest-rank percentile (0 for an empty slice).
fn percentile(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    s[((s.len() as f64 * p).ceil() as usize).clamp(1, s.len()) - 1]
}

/// Detection plan derived from the quiet baseline.
#[derive(Clone, Debug, PartialEq)]
pub struct BaselinePlan {
    /// 99th percentile of the per-packet envelope peaks over the baseline window (dBFS).
    pub floor_db: f64,
    /// Largest deviation from the mean of the raw (unfiltered) capture over the window (dBFS).
    pub raw_dev_db: f64,
    /// Expected peak of the click's envelope in the capture (dBFS).
    pub expected_db: f64,
    pub thr_db: f64,
    pub thr_lin: f32,
    pub window_frames: usize,
}

/// Evaluates the first [`BASELINE_S`] seconds of the recording (after [`BASELINE_WARMUP_S`]): floor = p99 of the
/// per-packet envelope peaks, threshold = floor + 12 dB (never more sensitive than 25 dB below the expected click
/// envelope). Aborts with an explanation if the raw capture carries loud programme audio or the floor is too high to
/// detect the click (threshold above expected - 6 dB).
pub fn plan_baseline(rec: &Recording, env: &[f32], click_db: f32) -> Result<BaselinePlan, String> {
    let rate = f64::from(rec.rate.max(1));
    let a = (BASELINE_WARMUP_S * rate) as usize;
    let b = a + (BASELINE_S * rate) as usize;
    if env.len() < b || rec.l.len() < b || rec.r.len() < b {
        return Err(format!(
            "only {} frames were captured, {b} are needed for the baseline: the capture endpoint delivered no (or too little) data (is anything rendering on it? is the endpoint active?)",
            env.len()
        ));
    }
    let mut peaks = Vec::new();
    for p in &rec.packets {
        let (s, e) = (p.start.max(a), (p.start + p.frames).min(b));
        if s < e {
            peaks.push(f64::from(env[s..e].iter().fold(0.0f32, |m, v| m.max(*v))));
        }
    }
    if peaks.is_empty() {
        return Err("no capture packets inside the baseline window".into());
    }
    let floor_db = lin_db(percentile(&peaks, 0.99));
    let dev = |x: &[f32]| -> f64 {
        let m = x.iter().map(|&v| f64::from(v)).sum::<f64>() / x.len().max(1) as f64;
        x.iter().fold(0.0f64, |acc, &v| acc.max((f64::from(v) - m).abs()))
    };
    let raw_dev_db = lin_db(dev(&rec.l[a..b]).max(dev(&rec.r[a..b])));
    if raw_dev_db > RAW_CONTAMINATION_DBFS {
        return Err(format!(
            "baseline is not quiet: the raw capture deviates by {raw_dev_db:.1} dBFS (limit {RAW_CONTAMINATION_DBFS:.0} dBFS): the capture endpoint carries loud programme audio (another app playing to the Yamaha, or a loud microphone); aborting before anything is rendered"
        ));
    }
    let expected_db = f64::from(click_db) + band_gain_db(SHAPE, rec.rate, HP_HZ);
    let thr_db = (floor_db + FLOOR_MARGIN_DB).max(expected_db - MIN_BELOW_CLICK_DB);
    if thr_db > expected_db - CLICK_HEADROOM_DB {
        return Err(format!(
            "the capture noise floor ({floor_db:.1} dBFS after the high-pass) is too high to detect a {click_db:.0} dBFS click: the click envelope is about {expected_db:.1} dBFS, the threshold would be {thr_db:.1} dBFS (floor + {FLOOR_MARGIN_DB:.0} dB) but must be at most {:.1} dBFS (click - {CLICK_HEADROOM_DB:.0} dB). A louder click is not allowed (max -35 dBFS); aborting before anything is rendered",
            expected_db - CLICK_HEADROOM_DB
        ));
    }
    Ok(BaselinePlan { floor_db, raw_dev_db, expected_db, thr_db, thr_lin: 10f64.powf(thr_db / 20.0) as f32, window_frames: b })
}

/// One detected click.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClickDetection {
    pub index: u32,
    /// First frame at or above the threshold, and the corrected onset (frame, fractional) after removing the bias.
    pub crossing_frame: usize,
    pub bias_frames: f64,
    /// Peak of the envelope from the crossing on (dBFS).
    pub peak_db: f64,
    /// Injection -> detected click onset, ms (anchoring: packet end = callback instant).
    pub total_ms: f64,
}

/// Finds each click: the first envelope crossing of `plan.thr_lin` in a packet delivered after the injection
/// (a packet delivered earlier cannot hold the click) and before the next injection / [`MAX_LATENCY_S`]. The onset bias
/// of the threshold detector is removed ([`band_onset_bias_frames`]).
pub fn detect_clicks(rec: &Recording, env: &[f32], plan: &BaselinePlan, inject_qpc: &[i64], hz: i64) -> Vec<Option<ClickDetection>> {
    let rate = f64::from(rec.rate.max(1));
    let peak_frames = (PEAK_SCAN_S * rate) as usize;
    let max_ticks = (MAX_LATENCY_S * hz as f64) as i64;
    inject_qpc
        .iter()
        .enumerate()
        .map(|(k, &inj)| {
            let end = inject_qpc.get(k + 1).copied().unwrap_or(i64::MAX).min(inj + max_ticks);
            let first_pkt = rec.packets.partition_point(|p| p.qpc < inj);
            for p in &rec.packets[first_pkt..] {
                if p.qpc >= end {
                    break;
                }
                let (s, e) = (p.start.max(plan.window_frames), (p.start + p.frames).min(env.len()));
                if s >= e {
                    continue;
                }
                if let Some(off) = env[s..e].iter().position(|&v| v >= plan.thr_lin) {
                    let i = s + off;
                    let peak = env[i..(i + peak_frames).min(env.len())].iter().fold(0.0f32, |m, v| m.max(*v));
                    let bias = band_onset_bias_frames(SHAPE, rec.rate, HP_HZ, plan.thr_lin, peak);
                    let t = rec.frame_qpc(i, hz)? as f64 - bias * hz as f64 / rate;
                    return Some(ClickDetection {
                        index: k as u32,
                        crossing_frame: i,
                        bias_frames: bias,
                        peak_db: lin_db(f64::from(peak)),
                        total_ms: (t - inj as f64) * 1000.0 / hz as f64,
                    });
                }
            }
            None
        })
        .collect()
}

// ------------------------------------------------------------------------------------
// Statistics and the comparison with the router
// ------------------------------------------------------------------------------------

/// min / median / mean / max / sample standard deviation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    pub n: usize,
    pub min: f64,
    pub median: f64,
    pub mean: f64,
    pub max: f64,
    pub sigma: f64,
}

pub fn summarize(v: &[f64]) -> Option<Summary> {
    if v.is_empty() {
        return None;
    }
    let n = v.len();
    let mean = v.iter().sum::<f64>() / n as f64;
    let sigma = if n > 1 { (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0)).sqrt() } else { 0.0 };
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = if n % 2 == 1 { s[n / 2] } else { 0.5 * (s[n / 2 - 1] + s[n / 2]) };
    Some(Summary { n, min: s[0], median, mean, max: s[n - 1], sigma })
}

/// Router chain vs direct WDM.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Comparison {
    pub router_total_ms: f64,
    pub direct_total_ms: f64,
    /// Capture-side buffering estimate (ms): capture period + `GetStreamLatency`.
    pub capture_buffering_ms: f64,
    /// `router_total - direct_total`.
    pub added_ms: f64,
    pub added_low_ms: f64,
    pub added_high_ms: f64,
}

/// The measured direct figure includes between 0 and `capture_buffering_ms` of capture-side buffering (the router's
/// "(b) corrected" figure is referenced to the device's input boundary), so the true direct latency lies in
/// `[direct - buffering, direct]` and the router's added latency in `[router - direct, router - direct + buffering]`.
pub fn compare(router_total_ms: f64, direct_total_ms: f64, capture_buffering_ms: f64) -> Comparison {
    let b = capture_buffering_ms.max(0.0);
    let added = router_total_ms - direct_total_ms;
    Comparison { router_total_ms, direct_total_ms, capture_buffering_ms: b, added_ms: added, added_low_ms: added, added_high_ms: added + b }
}

/// Printable lines of the comparison, including the caveat text.
pub fn comparison_lines(c: &Comparison) -> Vec<String> {
    vec![
        format!(
            "router added latency = router_total - direct_total = {:.1} - {:.1} = {:.1} ms (range {:.1} .. {:.1} ms with the {:.1} ms capture-side buffering estimate)",
            c.router_total_ms, c.direct_total_ms, c.added_ms, c.added_low_ms, c.added_high_ms, c.capture_buffering_ms
        ),
        "caveat: router_total is the saved router-cli latency '(b) corrected' median (injection -> the Yamaha ASIO input boundary, ASIO input latency already subtracted); direct_total is injection -> the WASAPI capture callback of the Yamaha Streaming endpoint (packet end anchored at the callback). The direct figure still contains the capture-side buffering (up to one capture period + stream latency), so the true direct latency is between direct_total - buffering and direct_total and the router's added latency between the first and last values of the range. Both runs include the Windows engine pass and the Yamaha driver's own path; the Streaming endpoint also carries the live Input-1 mic (hence the 18.5 kHz burst and high-pass); the measurement is for the Windows WDM path only (router stopped), and VAC / Yamaha WDM-ASIO coexistence is not verified by it.".into(),
    ]
}

// ------------------------------------------------------------------------------------
// Synthetic recording and selftest (no audio, no hardware)
// ------------------------------------------------------------------------------------

/// Deterministic pseudo-random value in -1..1.
fn lcg(state: &mut u64) -> f32 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    ((*state >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
}

/// A synthetic capture: mic-like low-band noise (about -46 dBFS rms) plus a tiny white floor, `packet`-frame packets whose
/// callbacks occur at the packet's end (`phase_us` shifts the whole timeline), and one click per entry of `latencies_ms`
/// whose first frame reaches the device `latency_ms` after the injection. Ticks are microseconds (hz = 1 000 000).
/// Returns the recording and the injection times (ticks) of the clicks.
pub fn synthetic_recording(latencies_ms: &[f64], interval_ms: f64, phase_us: i64, click_db: f32, hf_noise_db: Option<f32>) -> (Recording, Vec<i64>) {
    let rate = 48000u32;
    let us_per_frame = 1e6 / f64::from(rate);
    let packet = 480usize;
    let lead_us = 1_600_000.0;
    let total_us = lead_us + latencies_ms.len() as f64 * interval_ms * 1000.0 + 600_000.0;
    let total_frames = ((total_us / us_per_frame) as usize / packet + 1) * packet;
    let t0 = phase_us;
    let mut seed = 12345u64;
    let amp = 10f32.powf(-46.0 / 20.0) * (2.0f32 / 20.0).sqrt();
    let tones: Vec<(f32, f32)> = (0..20).map(|k| (150.0 + k as f32 * 400.0, k as f32 * 1.3)).collect();
    let hf_amp = hf_noise_db.map(|d| 10f32.powf(d / 20.0));
    let mut l = vec![0.0f32; total_frames];
    for (i, x) in l.iter_mut().enumerate() {
        let t = i as f32 / rate as f32;
        let mic: f32 = tones.iter().map(|(f, ph)| amp * (2.0 * std::f32::consts::PI * f * t + ph).sin()).sum();
        *x = mic + 1e-5 * lcg(&mut seed) + hf_amp.map(|a| a * (2.0 * std::f32::consts::PI * 19000.0 * t).sin()).unwrap_or(0.0);
    }
    let mut r = l.clone();
    let click = click_samples(SHAPE, rate, click_db);
    let mut injects = Vec::new();
    for (k, lat) in latencies_ms.iter().enumerate() {
        let inject_us = t0 as f64 + lead_us + k as f64 * interval_ms * 1000.0;
        injects.push(inject_us as i64);
        let f0 = ((inject_us + lat * 1000.0 - t0 as f64) / us_per_frame).round() as usize;
        for (j, c) in click.iter().enumerate() {
            if f0 + j < total_frames {
                l[f0 + j] += c;
                r[f0 + j] += c;
            }
        }
    }
    let packets = (0..total_frames / packet)
        .map(|p| PacketRec { qpc: t0 + (((p + 1) * packet) as f64 * us_per_frame) as i64, start: p * packet, frames: packet })
        .collect();
    (Recording { rate, l, r, packets }, injects)
}

/// Offline self-test: the detector and the timing arithmetic against synthetic recordings of known latency, a floor too
/// high to detect (must abort), loud raw audio (must abort), the safety gates and the comparison. Returns report lines,
/// or the first failure.
pub fn selftest() -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let hz = 1_000_000i64;
    let truth = [31.0, 33.5, 35.25, 38.0];
    let mut worst = 0.0f64;
    for phase in [0i64, 3_333, 7_777, 10_001, 17_123] {
        let (rec, inj) = synthetic_recording(&truth, 1500.0, phase, -40.0, None);
        let env = envelope(&rec, HP_HZ);
        let plan = plan_baseline(&rec, &env, -40.0).map_err(|e| format!("phase {phase}: baseline failed: {e}"))?;
        let det = detect_clicks(&rec, &env, &plan, &inj, hz);
        for (k, d) in det.iter().enumerate() {
            let d = d.ok_or_else(|| format!("phase {phase}: click {k} not detected"))?;
            let err = d.total_ms - truth[k];
            worst = worst.max(err.abs());
            if err.abs() > 0.15 {
                return Err(format!("phase {phase} click {k}: measured {:.3} ms, truth {:.3} ms (error {err:+.3})", d.total_ms, truth[k]));
            }
        }
    }
    out.push(format!("detector + timing: 5 phases x {} clicks recovered within {worst:.3} ms of the known latency (limit 0.15 ms)", truth.len()));
    // the floor of the first 0.7 s decides; a click-band disturbance too strong for the click must abort
    let (rec, _) = synthetic_recording(&truth, 1500.0, 0, -40.0, Some(-48.0));
    let env = envelope(&rec, HP_HZ);
    match plan_baseline(&rec, &env, -40.0) {
        Err(e) if e.contains("too high") => out.push("floor too high for the click: aborts as required".into()),
        other => return Err(format!("a -48 dBFS 19 kHz floor must abort, got {other:?}")),
    }
    let mut loud = rec.clone();
    for x in loud.l.iter_mut().chain(loud.r.iter_mut()) {
        *x *= 40.0;
    }
    let env = envelope(&loud, HP_HZ);
    match plan_baseline(&loud, &env, -40.0) {
        Err(e) if e.contains("not quiet") => out.push("loud programme audio: aborts as required".into()),
        other => return Err(format!("loud audio must abort, got {other:?}")),
    }
    // safety gates
    if knob_gate(Some((1.0, false)), false) == KnobGate::Proceed || knob_gate(Some((1.0, false)), true) != KnobGate::Proceed {
        return Err("knob gate arithmetic is wrong".into());
    }
    if validate_peak_dbfs(-34.0).is_ok() || total_run_seconds(10, 1200.0) > MAX_TOTAL_S {
        return Err("level / duration limits are wrong".into());
    }
    out.push("safety gates: knob gate, -35 dBFS limit, 20 s limit OK".into());
    let c = compare(50.2, 40.0, 10.0);
    if (c.added_ms - 10.2).abs() > 1e-9 || (c.added_high_ms - 20.2).abs() > 1e-9 {
        return Err("comparison arithmetic is wrong".into());
    }
    out.push("comparison arithmetic OK".into());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::band::auto_highpass_hz;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn base() -> Vec<&'static str> {
        vec!["--allow-sink", DEFAULT_SINK]
    }

    #[test]
    fn defaults_and_allow_sink_required() {
        let p = parse_args(&a(&base())).unwrap();
        assert_eq!((p.sink.as_str(), p.capture.as_str(), p.count, p.interval_ms, p.db), (DEFAULT_SINK, DEFAULT_CAPTURE, 5, 1500.0, -40.0));
        assert_eq!(p.router_total_ms, 50.2);
        assert!(!p.confirm_knob_down);
        // no explicit allow-list entry: refused (a real output device is not a virtual cable)
        let e = parse_args(&a(&[])).unwrap_err();
        assert!(e.contains("--allow-sink"), "{e}");
        assert!(parse_args(&a(&["--allow-sink", "  "])).is_err());
        // an allow entry that does not cover the target is refused by the existing guard
        assert!(parse_args(&a(&["--allow-sink", "Realtek"])).unwrap_err().contains("not covered"));
        // selftest needs no sink
        assert!(parse_args(&a(&["--selftest"])).unwrap().selftest);
    }

    #[test]
    fn shape_must_be_the_18k5_burst() {
        let mut v = base();
        v.extend(["--shape", "burst:18500:2"]);
        assert!(parse_args(&a(&v)).is_ok());
        for bad in ["burst", "burst:1500:2", "chirp", "burst:18500:4", "noise"] {
            let mut v = base();
            v.extend(["--shape", bad]);
            assert!(parse_args(&a(&v)).unwrap_err().contains("burst:18500:2"), "{bad}");
        }
    }

    #[test]
    fn level_count_interval_and_duration_limits() {
        let with = |k: &str, val: &str| {
            let mut v = base();
            v.extend([k, val]);
            parse_args(&a(&v))
        };
        assert!(with("--db", "-35").is_ok());
        assert!(with("--db", "-34.9").is_err());
        assert!(with("--db", "0").is_err());
        assert!(with("--db", "nan").is_err());
        assert!(with("--count", "10").is_ok() && with("--count", "1").is_ok());
        assert!(with("--count", "11").is_err() && with("--count", "0").is_err() && with("--count", "2.5").is_err());
        assert!(with("--interval-ms", "1200").is_ok());
        assert!(with("--interval-ms", "1199").is_err() && with("--interval-ms", "200").is_err());
        assert!(with("--router-total", "0").is_err());
        // 10 clicks: 1.7 + 9 * i + 0.6 <= 20  =>  i <= 1966 ms
        let mut v = base();
        v.extend(["--count", "10", "--interval-ms", "1900"]);
        assert!(parse_args(&a(&v)).is_ok());
        let mut v = base();
        v.extend(["--count", "10", "--interval-ms", "2100"]);
        assert!(parse_args(&a(&v)).unwrap_err().contains("20 s"));
        assert!(total_run_seconds(10, 1200.0) < 20.0);
        // render-profile flags and anything unknown are refused (default legacy profile only)
        for bad in ["--render-mode", "--keepalive", "--render-fill"] {
            let mut v = base();
            v.extend([bad, "x"]);
            assert!(parse_args(&a(&v)).unwrap_err().contains("unknown argument"), "{bad}");
        }
    }

    #[test]
    fn knob_gate_refuses_without_confirmation() {
        assert!(matches!(knob_gate(Some((1.0, false)), false), KnobGate::Refuse(m) if m.contains("--confirm-knob-down") && m.contains("Nothing was rendered")));
        assert_eq!(knob_gate(Some((1.0, false)), true), KnobGate::Proceed);
        assert_eq!(knob_gate(Some((0.5, false)), false), KnobGate::Proceed);
        assert!(matches!(knob_gate(Some((0.501, false)), false), KnobGate::Refuse(_)));
        assert_eq!(knob_gate(Some((0.2, false)), false), KnobGate::Proceed);
        // unreadable volume is treated as high
        assert!(matches!(knob_gate(None, false), KnobGate::Refuse(_)));
        assert_eq!(knob_gate(None, true), KnobGate::Proceed);
        // muted: refused even with the flag (and never changed by us)
        assert!(matches!(knob_gate(Some((1.0, true)), true), KnobGate::Refuse(m) if m.contains("MUTED")));
    }

    #[test]
    fn hp_corner_matches_the_router_tool() {
        assert_eq!(auto_highpass_hz(18500.0 - 2000.0 / 2.0), Some(HP_HZ));
    }

    #[test]
    fn frame_time_anchors_the_packet_end_at_the_callback() {
        let rec = Recording {
            rate: 48000,
            l: vec![0.0; 960],
            r: vec![0.0; 960],
            packets: vec![PacketRec { qpc: 10_000, start: 0, frames: 480 }, PacketRec { qpc: 20_000, start: 480, frames: 480 }],
        };
        assert_eq!(rec.frame_qpc(479, 1_000_000), Some(10_000 - 21)); // one frame before the callback (20.8 us)
        assert_eq!(rec.frame_qpc(480, 1_000_000), Some(20_000 - 10_000));
        assert_eq!(rec.frame_qpc(960, 1_000_000), None);
    }

    #[test]
    fn recording_sink_tracks_packets() {
        let rec = Arc::new(Mutex::new(Recording::with_capacity(48000, 1.0)));
        let mut s = RecordingSink { rec: rec.clone(), now: || 42 };
        s.on_packet(4, crate::PacketFlags::default());
        s.on_frames(&[1.0, 2.0], &[1.0, 2.0]);
        s.on_frames(&[3.0, 4.0], &[3.0, 4.0]);
        s.on_packet(2, crate::PacketFlags::default());
        s.on_frames(&[5.0, 6.0], &[5.0, 6.0]);
        let g = rec.lock().unwrap();
        assert_eq!(g.packets, vec![PacketRec { qpc: 42, start: 0, frames: 4 }, PacketRec { qpc: 42, start: 4, frames: 2 }]);
        assert_eq!(g.l, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn detects_click_through_mic_noise_at_known_latency() {
        let truth = [31.0, 36.4];
        for phase in [0i64, 4_321, 9_999] {
            let (rec, inj) = synthetic_recording(&truth, 1500.0, phase, -40.0, None);
            let env = envelope(&rec, HP_HZ);
            let plan = plan_baseline(&rec, &env, -40.0).unwrap();
            assert!(plan.floor_db < -90.0, "{plan:?}");
            assert!(plan.thr_db >= plan.floor_db + 12.0 - 1e-9 && plan.thr_db <= plan.expected_db - 6.0, "{plan:?}");
            let det = detect_clicks(&rec, &env, &plan, &inj, 1_000_000);
            for (d, t) in det.iter().zip(truth) {
                let d = d.expect("detected");
                assert!((d.total_ms - t).abs() < 0.15, "phase {phase}: {} vs {t}", d.total_ms);
                assert!(d.bias_frames > 0.0);
            }
        }
    }

    #[test]
    fn click_before_injection_or_in_a_prior_packet_is_not_matched() {
        let (rec, inj) = synthetic_recording(&[31.0], 1500.0, 0, -40.0, None);
        let env = envelope(&rec, HP_HZ);
        let plan = plan_baseline(&rec, &env, -40.0).unwrap();
        // injection claimed 2 s later than the real one: the click lies before it, nothing to find
        let late = [inj[0] + 2_000_000];
        assert!(detect_clicks(&rec, &env, &plan, &late, 1_000_000)[0].is_none());
    }

    #[test]
    fn high_floor_and_loud_audio_abort() {
        let (rec, _) = synthetic_recording(&[31.0], 1500.0, 0, -40.0, Some(-48.0));
        let e = plan_baseline(&rec, &envelope(&rec, HP_HZ), -40.0).unwrap_err();
        assert!(e.contains("too high") && e.contains("before anything is rendered"), "{e}");
        let (mut rec, _) = synthetic_recording(&[31.0], 1500.0, 0, -40.0, None);
        for x in rec.l.iter_mut() {
            *x *= 50.0;
        }
        let e = plan_baseline(&rec, &envelope(&rec, HP_HZ), -40.0).unwrap_err();
        assert!(e.contains("not quiet"), "{e}");
        let empty = Recording { rate: 48000, ..Default::default() };
        assert!(plan_baseline(&empty, &[], -40.0).unwrap_err().contains("no (or too little) data"));
    }

    #[test]
    fn envelope_of_a_prefix_is_the_prefix_of_the_envelope() {
        let (rec, _) = synthetic_recording(&[31.0], 1500.0, 0, -40.0, None);
        let full = envelope(&rec, HP_HZ);
        let n = 40_000;
        let pre = Recording { rate: rec.rate, l: rec.l[..n].to_vec(), r: rec.r[..n].to_vec(), packets: vec![] };
        assert_eq!(envelope(&pre, HP_HZ), full[..n].to_vec());
    }

    #[test]
    fn statistics() {
        let s = summarize(&[40.0, 41.0, 42.0, 43.0, 44.0]).unwrap();
        assert_eq!((s.n, s.min, s.median, s.mean, s.max), (5, 40.0, 42.0, 42.0, 44.0));
        assert!((s.sigma - 1.5811388).abs() < 1e-6);
        assert_eq!(summarize(&[1.0, 2.0, 3.0, 10.0]).unwrap().median, 2.5);
        assert!(summarize(&[]).is_none());
        assert_eq!(summarize(&[7.0]).unwrap().sigma, 0.0);
    }

    #[test]
    fn comparison_range_and_caveat() {
        let c = compare(50.2, 38.0, 10.0);
        assert!((c.added_ms - 12.2).abs() < 1e-9 && (c.added_low_ms - 12.2).abs() < 1e-9 && (c.added_high_ms - 22.2).abs() < 1e-9);
        let t = comparison_lines(&c).join("\n");
        assert!(t.contains("router_total - direct_total") && t.contains("12.2") && t.contains("22.2"));
        assert!(t.contains("caveat") && t.contains("capture-side buffering") && t.contains("Input-1 mic"));
        assert_eq!(compare(50.0, 40.0, -3.0).added_high_ms, 10.0);
    }

    #[test]
    fn selftest_passes() {
        let lines = selftest().unwrap();
        assert!(lines.len() >= 5, "{lines:?}");
    }
}
