//! End-to-end latency probe: detects an injected click at two points of the router and turns the
//! QPC timestamps into latency figures (`router-cli latency`).
//!
//! Two detection points, both threshold detectors armed by the control thread:
//!
//! * **capture** (the source tap in `Pipeline::render`, i.e. every block popped from ring A
//!   while armed — the bridge path `router-cli latency` measures): first sample above the
//!   threshold -> QPC at the pop (callback entry) + frame offset within the block. This is the
//!   instant the router first sees the click (render side + VBMatrix + ring A delivery).
//! * **loopback** (the ASIO audio callback, the Yamaha's own inputs "UR22C Streaming L/R", which carry
//!   the internal loopback of the output mix): first sample above the threshold in the input buffer
//!   half `index` -> QPC at callback entry + sample offset.
//!
//! Real-time rules: the callback side is allocation-free (preallocated scratch), uses atomics only, and
//! does nothing at all unless armed (`Detector::is_active`). Without `EngineOptions::loopback_probe` no
//! input buffers are created and no probe object exists.
//!
//! ASIO buffer-half semantics (relied on by [`measure`]): in `bufferSwitch(index)` the input buffer half
//! `index` holds the block that has just been captured; its LAST sample is the newest. The driver-reported
//! input latency `in_lat` (samples) is the time from a sample reaching the device's input boundary to
//! the callback in which the host gets it, so for the sample at offset `j` of an `N`-sample block
//! `t_device(j) = t_callback - (in_lat - j) / rate`.

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU8, Ordering::{Acquire, Relaxed, Release}};
use std::sync::{Arc, Mutex};

use asio_abi::AsioSampleType;
use router_core::asio_convert::read_asio_to_f32;
use router_core::click::{click_samples, onset_index, ImpulseShape};

use crate::baseline::{ChannelReport, HighPass, Monitor};
use crate::pipeline::PtrTable;

const IDLE: u8 = 0;
const ARMED: u8 = 1;
const PEAK: u8 = 2;
const DONE: u8 = 3;

/// Blocks scanned after the first crossing to find the click's peak (for the onset-bias correction).
pub const PEAK_BLOCKS: u32 = 2;

/// One detection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Detection {
    /// QPC ticks at callback entry.
    pub qpc: i64,
    /// Sample/frame offset of the first sample at or above the threshold within the block.
    pub offset: u32,
    /// Frames in that block.
    pub block_len: u32,
    /// Peak magnitude seen from the crossing on (over the next [`PEAK_BLOCKS`] blocks too), linear.
    pub peak: f32,
    /// Channel (0 = L, 1 = R) that crossed first.
    pub channel: u8,
    /// Threshold that was armed (linear).
    pub threshold: f32,
}

/// Lock-free threshold detector: one writer (the audio / capture thread), one reader (control thread).
#[derive(Default)]
pub struct Detector {
    state: AtomicU8,
    thr: AtomicU32,
    qpc: AtomicI64,
    offset: AtomicU32,
    block_len: AtomicU32,
    peak: AtomicU32,
    channel: AtomicU8,
    blocks_left: AtomicU32,
    /// Measured baseline mean (DC) of the left / right input, subtracted before the threshold test (f32 bits).
    dc: [AtomicU32; 2],
}

#[inline]
fn first_at_or_above(x: &[f32], thr: f32, dc: f32) -> Option<usize> {
    x.iter().position(|v| (v - dc).abs() >= thr)
}

#[inline]
fn max_abs(x: &[f32], dc: f32) -> f32 {
    x.iter().fold(0.0f32, |m, v| m.max((v - dc).abs()))
}

impl Detector {
    /// Arms the detector (control thread). Any previous result is dropped.
    pub fn arm(&self, threshold_lin: f32) {
        self.arm_with_dc(threshold_lin, [0.0, 0.0]);
    }

    /// Arms the detector; the measured baseline means `dc` (left, right; linear) are subtracted from the
    /// samples first, so a DC offset cannot trigger it and the reported peak is the deviation from the baseline.
    pub fn arm_with_dc(&self, threshold_lin: f32, dc: [f32; 2]) {
        self.dc[0].store(dc[0].to_bits(), Relaxed);
        self.dc[1].store(dc[1].to_bits(), Relaxed);
        self.thr.store(threshold_lin.max(1e-7).to_bits(), Relaxed);
        self.peak.store(0, Relaxed);
        self.state.store(ARMED, Release);
    }

    pub fn disarm(&self) {
        self.state.store(IDLE, Release);
    }

    /// True while a block must be scanned (armed or collecting the peak).
    #[inline]
    pub fn is_active(&self) -> bool {
        matches!(self.state.load(Relaxed), ARMED | PEAK)
    }

    /// True once a detection (including its peak window) is complete.
    pub fn is_done(&self) -> bool {
        self.state.load(Acquire) == DONE
    }

    /// True once the first crossing was seen (peak window may still be running).
    pub fn has_crossing(&self) -> bool {
        matches!(self.state.load(Acquire), PEAK | DONE)
    }

    /// Scans one stereo block (audio / capture thread; allocation-free). `now` = QPC at callback entry.
    #[inline]
    pub fn on_block(&self, l: &[f32], r: &[f32], now: i64) {
        match self.state.load(Relaxed) {
            ARMED => {
                let thr = f32::from_bits(self.thr.load(Relaxed));
                let (dcl, dcr) = (f32::from_bits(self.dc[0].load(Relaxed)), f32::from_bits(self.dc[1].load(Relaxed)));
                let (il, ir) = (first_at_or_above(l, thr, dcl), first_at_or_above(r, thr, dcr));
                let (idx, ch) = match (il, ir) {
                    (Some(a), Some(b)) if b < a => (b, 1u8),
                    (Some(a), _) => (a, 0u8),
                    (None, Some(b)) => (b, 1u8),
                    (None, None) => return,
                };
                let peak = max_abs(&l[idx.min(l.len())..], dcl).max(max_abs(&r[idx.min(r.len())..], dcr));
                self.qpc.store(now, Relaxed);
                self.offset.store(idx as u32, Relaxed);
                self.block_len.store(l.len().min(r.len()) as u32, Relaxed);
                self.channel.store(ch, Relaxed);
                self.peak.store(peak.to_bits(), Relaxed);
                self.blocks_left.store(PEAK_BLOCKS, Relaxed);
                self.state.store(PEAK, Release);
            }
            PEAK => {
                let (dcl, dcr) = (f32::from_bits(self.dc[0].load(Relaxed)), f32::from_bits(self.dc[1].load(Relaxed)));
                let p = max_abs(l, dcl).max(max_abs(r, dcr));
                if p > f32::from_bits(self.peak.load(Relaxed)) {
                    self.peak.store(p.to_bits(), Relaxed);
                }
                let left = self.blocks_left.load(Relaxed).saturating_sub(1);
                self.blocks_left.store(left, Relaxed);
                if left == 0 {
                    self.state.store(DONE, Release);
                }
            }
            _ => {}
        }
    }

    /// The finished detection, if any; returns the detector to idle.
    pub fn take(&self) -> Option<Detection> {
        if self.state.load(Acquire) != DONE {
            return None;
        }
        let d = Detection {
            qpc: self.qpc.load(Relaxed),
            offset: self.offset.load(Relaxed),
            block_len: self.block_len.load(Relaxed),
            peak: f32::from_bits(self.peak.load(Relaxed)),
            channel: self.channel.load(Relaxed),
            threshold: f32::from_bits(self.thr.load(Relaxed)),
        };
        self.state.store(IDLE, Release);
        Some(d)
    }
}

/// Static facts about the running probe session (set when the session opens).
#[derive(Clone, Debug, Default)]
pub struct ProbeInfo {
    pub asio_rate: u32,
    pub cable_rate: u32,
    pub buffer_frames: u32,
    /// Driver latencies in samples at `asio_rate` (valid after createBuffers).
    pub input_latency: i32,
    pub output_latency: i32,
    /// ASIO input channel indices used as the loopback pair and their names.
    pub input_channels: [usize; 2],
    pub input_names: [String; 2],
    pub capture_period_frames: u32,
}

/// State shared by the engine, the audio callback, the source tap and the [`LatencyProbe`] handle.
#[derive(Default)]
pub struct ProbeShared {
    pub capture: Detector,
    pub loopback: Detector,
    /// Baseline level statistics (diagnostic and threshold derivation).
    pub monitor: Monitor,
    /// Corner (Hz, f32 bits) of the high-pass the loopback detector looks through; 0 = none.
    hp_fc: AtomicU32,
    info: Mutex<Option<ProbeInfo>>,
}

impl ProbeShared {
    /// Sets the corner of the high-pass the loopback detector looks through (`None` = unfiltered).
    pub fn set_highpass(&self, fc_hz: Option<f32>) {
        self.hp_fc.store(fc_hz.unwrap_or(0.0).max(0.0).to_bits(), Relaxed);
    }
    pub fn set_info(&self, i: ProbeInfo) {
        *self.info.lock().unwrap_or_else(|e| e.into_inner()) = Some(i);
    }
    pub fn clear_info(&self) {
        *self.info.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Result of one armed cycle.
#[derive(Clone, Copy, Debug)]
pub struct ProbeResult {
    /// Capture-side detection (None if the click was not seen there, e.g. the sink was the wrong cable).
    pub capture: Option<Detection>,
    pub loopback: Detection,
}

/// Cloneable handle on the probe of a running [`Engine`](crate::Engine).
#[derive(Clone)]
pub struct LatencyProbe(pub(crate) Arc<ProbeShared>);

impl LatencyProbe {
    pub(crate) fn new(shared: Arc<ProbeShared>) -> Self {
        LatencyProbe(shared)
    }

    /// Facts about the current streaming session (`None` until the session is up).
    pub fn info(&self) -> Option<ProbeInfo> {
        self.0.info.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Arms both detectors with thresholds in dBFS (capture side: the cable level, loopback side: the
    /// level at the Yamaha's loopback input). Drops any previous result.
    pub fn arm(&self, capture_threshold_dbfs: f32, loopback_threshold_dbfs: f32) {
        self.arm_with_dc(capture_threshold_dbfs, loopback_threshold_dbfs, [0.0; 4]);
    }

    /// As [`arm`](Self::arm), removing the measured baseline means `dc` = [capture L, R, loopback L, R] first.
    pub fn arm_with_dc(&self, capture_threshold_dbfs: f32, loopback_threshold_dbfs: f32, dc: [f32; 4]) {
        self.0.capture.arm_with_dc(10f32.powf(capture_threshold_dbfs / 20.0), [dc[0], dc[1]]);
        self.0.loopback.arm_with_dc(10f32.powf(loopback_threshold_dbfs / 20.0), [dc[2], dc[3]]);
    }

    /// Re-arms only the loopback detector (after a stray crossing was rejected); the capture result is untouched.
    pub fn rearm_loopback(&self, loopback_threshold_dbfs: f32, dc: [f32; 4]) {
        self.0.loopback.arm_with_dc(10f32.powf(loopback_threshold_dbfs / 20.0), [dc[2], dc[3]]);
    }

    /// Starts accumulating baseline statistics on both streams (nothing is armed).
    pub fn set_highpass(&self, fc_hz: Option<f32>) {
        self.0.set_highpass(fc_hz);
    }

    /// Loopback statistics after the high-pass (valid when [`set_highpass`](Self::set_highpass) was set before the monitor).
    pub fn monitor_hp(&self) -> [ChannelReport; 2] {
        self.0.monitor.report_hp()
    }

    pub fn monitor_start(&self) {
        self.0.monitor.start();
    }

    /// As [`monitor_start`](Self::monitor_start), also keeping the first samples of every channel.
    pub fn monitor_start_raw(&self) {
        self.0.monitor.start_with(true);
    }

    /// Raw samples kept by [`monitor_start_raw`](Self::monitor_start_raw): [capture L, R, loopback L, R].
    pub fn monitor_raw(&self) -> [Vec<f32>; 4] {
        self.0.monitor.raw()
    }

    /// Stops the baseline statistics: [capture L, capture R, loopback L, loopback R].
    pub fn monitor_stop(&self) -> [ChannelReport; 4] {
        self.0.monitor.stop()
    }

    pub fn disarm(&self) {
        self.0.capture.disarm();
        self.0.loopback.disarm();
    }

    /// True if the capture side has already seen the click.
    pub fn capture_seen(&self) -> bool {
        self.0.capture.has_crossing()
    }

    /// True if either detector crossed its threshold since arming (used for the quiet baseline check).
    pub fn any_crossing(&self) -> bool {
        self.0.capture.has_crossing() || self.0.loopback.has_crossing()
    }

    /// The finished result once the loopback detection is complete (the capture detection is
    /// included if it happened). Returns the probe to idle. `None` while waiting.
    pub fn take_result(&self) -> Option<ProbeResult> {
        if !self.0.loopback.is_done() {
            return None;
        }
        // The capture side normally finished long before (it sees the click one ring + chain delay earlier);
        // if it is still collecting its peak, wait for it so the result is complete.
        if self.0.capture.has_crossing() && !self.0.capture.is_done() {
            return None;
        }
        let lb = self.0.loopback.take()?;
        let cap = self.0.capture.take();
        self.0.capture.disarm();
        Some(ProbeResult { capture: cap, loopback: lb })
    }
}

/// Which ASIO input channels carry the Yamaha loopback (the latency probe is on whenever this is set).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoopbackProbe {
    /// Input channel pair (indices). `None` = pick the first two inputs whose name contains "Streaming"
    /// (the Yamaha UR22C's "UR22C Streaming L/R"), else inputs 2 and 3.
    pub input_channels: Option<[usize; 2]>,
}

/// Picks the loopback input pair from the driver's input channel names.
pub fn select_input_channels(names: &[String], wanted: Option<[usize; 2]>) -> Result<[usize; 2], String> {
    if let Some(w) = wanted {
        if w[0] != w[1] && w.iter().all(|&c| c < names.len()) {
            return Ok(w);
        }
        return Err(format!("input channels {w:?} are not valid for a driver with {} inputs", names.len()));
    }
    let idx: Vec<usize> = names.iter().enumerate().filter(|(_, n)| n.to_lowercase().contains("streaming")).map(|(i, _)| i).collect();
    if idx.len() >= 2 {
        return Ok([idx[0], idx[1]]);
    }
    if names.len() >= 4 {
        return Ok([2, 3]);
    }
    Err(format!("no 'Streaming' loopback inputs found (inputs: {})", names.join(", ")))
}

/// Reads the loopback input halves in the audio callback and feeds the loopback [`Detector`].
///
/// With a high-pass configured ([`LatencyProbe::set_highpass`]) the detector looks at the high-passed signal (a
/// high-frequency click then stands out of band-limited programme audio such as a microphone) and the monitor
/// also collects the filtered statistics.
pub struct LoopbackTap {
    ptrs: Arc<PtrTable>,
    types: [AsioSampleType; 2],
    l: Vec<f32>,
    r: Vec<f32>,
    fl: Vec<f32>,
    fr: Vec<f32>,
    shared: Arc<ProbeShared>,
    rate: u32,
    hp: HighPass,
    hp_fc: u32,
    /// Blocks to skip after the tap became active (filter start-up).
    warm: u32,
    was_active: bool,
}

/// Blocks the filtered path ignores after it (re)starts from zero state.
const HP_WARM_BLOCKS: u32 = 1;

impl LoopbackTap {
    /// `ptrs` slots 0/1 must hold the two input channels' buffer halves.
    pub fn new(ptrs: Arc<PtrTable>, types: [AsioSampleType; 2], frames: usize, rate: u32, shared: Arc<ProbeShared>) -> Self {
        LoopbackTap {
            ptrs,
            types,
            l: vec![0.0; frames],
            r: vec![0.0; frames],
            fl: vec![0.0; frames],
            fr: vec![0.0; frames],
            shared,
            rate,
            hp: HighPass::new(1000.0, rate),
            hp_fc: 0,
            warm: 0,
            was_active: false,
        }
    }

    /// Scans input half `half` (`n` samples) at callback entry time `now`. Allocation-free; a single
    /// relaxed load when idle.
    ///
    /// # Safety
    /// The pointer table must hold valid driver input buffers of at least `n` samples of the configured types.
    #[inline]
    pub unsafe fn scan(&mut self, half: usize, n: usize, now: i64) {
        let det = self.shared.loopback.is_active();
        let mon = self.shared.monitor.is_active();
        if !det && !mon {
            self.was_active = false;
            return;
        }
        let n = n.min(self.l.len());
        for (ch, dst) in [&mut self.l, &mut self.r].into_iter().enumerate() {
            let p = self.ptrs.get(ch, half & 1);
            if p.is_null() {
                dst[..n].fill(0.0);
            } else {
                read_asio_to_f32(p as *const u8, self.types[ch], n, &mut dst[..n]);
            }
        }
        let fc = self.shared.hp_fc.load(Relaxed);
        if fc != self.hp_fc {
            self.hp_fc = fc;
            if fc != 0 {
                self.hp = HighPass::new(f32::from_bits(fc), self.rate);
            }
            self.was_active = false;
        }
        let use_hp = fc != 0;
        if use_hp {
            if !self.was_active {
                self.hp.reset();
                self.warm = HP_WARM_BLOCKS;
            }
            self.hp.process_env(0, &self.l[..n], &mut self.fl[..n]);
            self.hp.process_env(1, &self.r[..n], &mut self.fr[..n]);
        }
        self.was_active = true;
        let warm = use_hp && self.warm > 0;
        if warm {
            self.warm -= 1;
        }
        if mon {
            self.shared.monitor.feed(1, &self.l[..n], &self.r[..n]);
            if use_hp && !warm {
                self.shared.monitor.feed(2, &self.fl[..n], &self.fr[..n]);
            }
        }
        if det && !warm {
            if use_hp {
                self.shared.loopback.on_block(&self.fl[..n], &self.fr[..n], now);
            } else {
                self.shared.loopback.on_block(&self.l[..n], &self.r[..n], now);
            }
        }
    }
}

// ------------------------------------------------------------------------------------
// Measurement maths (pure)
// ------------------------------------------------------------------------------------

/// Onset bias (frames at `rate`) of a threshold detector that fired at `threshold` on a click whose
/// received peak was `peak`: the first sample of the nominal click at `threshold / peak` of its peak.
/// Subtract it from the detected offset to reference the detection to the click's first frame.
pub fn onset_bias_frames(shape: ImpulseShape, rate: u32, threshold: f32, peak: f32) -> f64 {
    if peak <= 0.0 {
        return 0.0;
    }
    let ratio = (threshold / peak).clamp(0.0, 1.0);
    onset_index(&click_samples(shape, rate, -40.0), ratio) as f64
}

pub use router_core::band::{band_gain_db, band_onset_bias_frames};

/// All figures of one impulse, milliseconds unless noted.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Measurement {
    /// QPC at injection (submission) to the source block that carried the click (render side +
    /// VBMatrix + ring A delivery; stage 1 via the engine's ring-A source tap).
    /// `None` if the source side did not see it.
    pub stage1_ms: Option<f64>,
    /// (a) Injection to the estimated instant the click leaves the render client (queue delay in the WASAPI
    /// render stream). `None` if unknown.
    pub render_queue_ms: Option<f64>,
    /// Injection to the callback that delivered the loopback block (callback-entry, quantised by the block).
    pub raw_callback_ms: f64,
    /// (a) Injection to the click's arrival on the loopback input stream, anchored so that the block ends
    /// at the callback instant (sample `j` of an `N`-sample block is `N - j` samples before it): no block
    /// quantisation, but it still contains the driver's whole input latency.
    pub total_ms: f64,
    /// (b) Injection to the click at the device's input boundary: `total_ms` minus the driver-reported
    /// input latency beyond the block (`in_lat - N`). Estimate (depends on the driver's `getLatencies`).
    pub corrected_ms: f64,
    /// Capture packet -> loopback tap (corrected): ring + resampler + EQ + ASIO block + Yamaha path to the tap.
    pub stage2_ms: Option<f64>,
    /// Estimated injection -> DAC-equivalent time: `corrected_ms` plus 0 ..= `(out_lat - N)` samples (the part
    /// of the output path after the loopback tap), as [low, high].
    pub to_dac_range_ms: [f64; 2],
}

impl Measurement {
    /// Midpoint of [`to_dac_range_ms`](Self::to_dac_range_ms).
    pub fn to_dac_mid_ms(&self) -> f64 {
        0.5 * (self.to_dac_range_ms[0] + self.to_dac_range_ms[1])
    }
}

/// Inputs that do not change between impulses.
#[derive(Clone, Copy, Debug)]
pub struct Timebase {
    pub tick_hz: i64,
    pub asio_rate: u32,
    pub buffer_frames: u32,
    pub input_latency: i32,
    pub output_latency: i32,
}

/// Turns the timestamps of one impulse into a [`Measurement`].
///
/// * `inject_ticks`: QPC of the click submission; `audible_ticks`: estimated render-client exit (optional).
/// * `lb_bias_frames`: onset bias of the loopback detection (frames at the ASIO rate), see [`onset_bias_frames`].
pub fn measure(
    tb: &Timebase,
    inject_ticks: i64,
    audible_ticks: Option<i64>,
    capture: Option<&Detection>,
    loopback: &Detection,
    lb_bias_frames: f64,
) -> Measurement {
    let hz = tb.tick_hz.max(1) as f64;
    let rate = f64::from(tb.asio_rate.max(1));
    let to_ms = |ticks: i64| ticks as f64 * 1000.0 / hz;
    let n = f64::from(tb.buffer_frames.max(1));
    let j = f64::from(loopback.offset) - lb_bias_frames;
    let raw = to_ms(loopback.qpc - inject_ticks);
    let total = raw - (n - j) * 1000.0 / rate;
    let extra = (f64::from(tb.input_latency) - n).max(0.0);
    let corrected = total - extra * 1000.0 / rate;
    let stage1 = capture.map(|c| to_ms(c.qpc - inject_ticks));
    let after_tap = (f64::from(tb.output_latency) - n).max(0.0) * 1000.0 / rate;
    Measurement {
        stage1_ms: stage1,
        render_queue_ms: audible_ticks.map(|a| to_ms(a - inject_ticks)),
        raw_callback_ms: raw,
        total_ms: total,
        corrected_ms: corrected,
        stage2_ms: stage1.map(|s| corrected - s),
        to_dac_range_ms: [corrected, corrected + after_tap],
    }
}

/// min / median / mean / max / sample standard deviation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    pub n: usize,
    pub min: f64,
    pub median: f64,
    pub mean: f64,
    pub max: f64,
    pub sigma: f64,
}

pub fn median_of(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let m = s.len() / 2;
    if s.len() % 2 == 1 {
        s[m]
    } else {
        0.5 * (s[m - 1] + s[m])
    }
}

pub fn stats(v: &[f64]) -> Option<Stats> {
    if v.is_empty() {
        return None;
    }
    let n = v.len();
    let mean = v.iter().sum::<f64>() / n as f64;
    let var = if n > 1 { v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0) } else { 0.0 };
    Some(Stats {
        n,
        min: v.iter().cloned().fold(f64::INFINITY, f64::min),
        median: median_of(v),
        mean,
        max: v.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        sigma: var.sqrt(),
    })
}

/// Flags values that are far from the median: more than `max(floor_ms, 3 * 1.4826 * MAD)`.
pub fn outliers(v: &[f64], floor_ms: f64) -> Vec<bool> {
    if v.len() < 4 {
        return vec![false; v.len()];
    }
    let med = median_of(v);
    let dev: Vec<f64> = v.iter().map(|x| (x - med).abs()).collect();
    let limit = floor_ms.max(3.0 * 1.4826 * median_of(&dev));
    v.iter().map(|x| (x - med).abs() > limit).collect()
}

// ------------------------------------------------------------------------------------
// Synthetic loopback (selftest and unit tests, no hardware)
// ------------------------------------------------------------------------------------

/// Parameters of the synthetic path.
#[derive(Clone, Debug)]
pub struct SyntheticPath {
    pub rate: u32,
    pub block: u32,
    pub in_lat: i32,
    pub out_lat: i32,
    /// Cable packet size (frames) and delivery lag after the packet's last frame (ms).
    pub packet: u32,
    pub packet_lag_ms: f64,
    /// Render side (inject -> cable has the first frame), ms.
    pub render_ms: f64,
    /// Everything between the cable and the loopback tap (ms): ring + resample + EQ + Yamaha to the tap.
    pub chain_ms: f64,
    pub shape: ImpulseShape,
    pub peak_dbfs: f32,
    pub capture_thr_dbfs: f32,
    pub loopback_thr_dbfs: f32,
    /// Gain of the chain (linear) applied to the click before it reaches the loopback tap.
    pub chain_gain: f32,
}

impl Default for SyntheticPath {
    fn default() -> Self {
        SyntheticPath {
            rate: 48000,
            block: 256,
            in_lat: 280,
            out_lat: 375,
            packet: 480,
            packet_lag_ms: 0.3,
            render_ms: 9.0,
            chain_ms: 33.0,
            shape: ImpulseShape::default(),
            peak_dbfs: -40.0,
            capture_thr_dbfs: -60.0,
            loopback_thr_dbfs: -78.0,
            chain_gain: 0.1,
        }
    }
}

/// Ground truth and measurement of one synthetic impulse.
#[derive(Clone, Debug)]
pub struct SyntheticOutcome {
    /// True injection -> device-input-boundary time of the click's first frame (ms).
    pub truth_corrected_ms: f64,
    /// True injection -> delivery of the capture packet containing the click's threshold crossing (ms).
    pub truth_stage1_ms: f64,
    pub measurement: Measurement,
}

/// Runs the detectors against a synthetic click path with known timing. `phase_us` shifts the injection time
/// relative to the packet/block grids (different phases exercise the in-block offset maths).
/// Timestamps use a 1 MHz tick so that ticks are microseconds.
pub fn synthetic_run(p: &SyntheticPath, phase_us: i64) -> Option<SyntheticOutcome> {
    const HZ: i64 = 1_000_000;
    let rate = f64::from(p.rate);
    let us_per_frame = 1e6 / rate;
    let inject = 2_000_000 + phase_us; // us
    let click_cap = click_samples(p.shape, p.rate, p.peak_dbfs);
    let click_lb: Vec<f32> = click_cap.iter().map(|x| x * p.chain_gain).collect();

    // Cable stream: frame f at time t_start + f / rate; the click's frame 0 is there at inject + render_ms.
    let t_start = 0i64;
    let f0 = (((inject as f64 + p.render_ms * 1000.0) - t_start as f64) / us_per_frame).round() as usize;
    let total_cap = f0 + click_cap.len() + 3 * p.packet as usize;
    let mut cap = vec![0.0f32; total_cap];
    cap[f0..f0 + click_cap.len()].copy_from_slice(&click_cap);

    let shared = ProbeShared::default();
    shared.capture.arm(10f32.powf(p.capture_thr_dbfs / 20.0));
    shared.loopback.arm(10f32.powf(p.loopback_thr_dbfs / 20.0));
    // Capture packets.
    let mut pkt_deliver_of_crossing = None;
    let mut k = 0usize;
    while (k + 1) * p.packet as usize <= cap.len() {
        let blk = &cap[k * p.packet as usize..(k + 1) * p.packet as usize];
        let deliver = t_start + (((k + 1) * p.packet as usize) as f64 * us_per_frame + p.packet_lag_ms * 1000.0) as i64;
        let was = shared.capture.has_crossing();
        shared.capture.on_block(blk, blk, deliver);
        if !was && shared.capture.has_crossing() {
            pkt_deliver_of_crossing = Some(deliver);
        }
        k += 1;
    }

    // Loopback stream: sample index s is at the device boundary at t_in_ref + s / rate; the click's first
    // frame arrives chain_ms after it reached the cable.
    let t_in_ref = 777i64; // arbitrary phase of the input clock
    let s0 = (((inject as f64 + (p.render_ms + p.chain_ms) * 1000.0) - t_in_ref as f64) / us_per_frame).round() as usize;
    let n = p.block as usize;
    let total_in = ((s0 + click_lb.len()) / n + 6) * n;
    let mut lb = vec![0.0f32; total_in];
    lb[s0..s0 + click_lb.len()].copy_from_slice(&click_lb);
    for m in 0..total_in / n {
        let blk = &lb[m * n..(m + 1) * n];
        // Callback entry: the block's last sample reached the device boundary, plus the non-buffer latency.
        let last = (m + 1) * n - 1;
        let extra = f64::from(p.in_lat) - n as f64;
        let t_cb = t_in_ref + ((last as f64 + 1.0 + extra) * us_per_frame) as i64;
        shared.loopback.on_block(blk, blk, t_cb);
    }
    let lbd = shared.loopback.take()?;
    let cap_d = shared.capture.take()?;
    let tb = Timebase {
        tick_hz: HZ,
        asio_rate: p.rate,
        buffer_frames: p.block,
        input_latency: p.in_lat,
        output_latency: p.out_lat,
    };
    let lb_bias = onset_bias_frames(p.shape, p.rate, lbd.threshold, lbd.peak);
    let m = measure(&tb, inject, None, Some(&cap_d), &lbd, lb_bias);
    let t_tap0 = t_in_ref as f64 + s0 as f64 * us_per_frame;
    Some(SyntheticOutcome {
        truth_corrected_ms: (t_tap0 - inject as f64) / 1000.0,
        truth_stage1_ms: (pkt_deliver_of_crossing? - inject) as f64 / 1000.0,
        measurement: m,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_finds_first_sample_and_peak_over_following_blocks() {
        let d = Detector::default();
        let mut l = vec![0.0f32; 64];
        let r = vec![0.0f32; 64];
        d.on_block(&l, &r, 5); // idle: ignored
        assert!(!d.is_active() && d.take().is_none());
        d.arm(0.01);
        d.on_block(&l, &r, 10);
        assert!(!d.has_crossing());
        l[20] = 0.02;
        l[30] = -0.2;
        d.on_block(&l, &r, 100);
        assert!(d.has_crossing() && !d.is_done() && d.is_active());
        let mut l2 = vec![0.0f32; 64];
        l2[3] = 0.5;
        d.on_block(&l2, &r, 200);
        assert!(!d.is_done());
        d.on_block(&r, &r, 300);
        assert!(d.is_done());
        let det = d.take().unwrap();
        assert_eq!((det.qpc, det.offset, det.block_len, det.channel), (100, 20, 64, 0));
        assert!((det.peak - 0.5).abs() < 1e-6);
        assert!(!d.is_active() && d.take().is_none());
    }

    #[test]
    fn detector_picks_the_earlier_channel() {
        let d = Detector::default();
        d.arm(0.1);
        let mut l = vec![0.0f32; 16];
        let mut r = vec![0.0f32; 16];
        l[9] = 0.3;
        r[4] = -0.3;
        d.on_block(&l, &r, 1);
        d.on_block(&l, &r, 2);
        d.on_block(&l, &r, 3);
        let det = d.take().unwrap();
        assert_eq!((det.offset, det.channel), (4, 1));
    }

    #[test]
    fn channel_selection() {
        let names: Vec<String> = ["Mic/Line 1", "Mic/Line 2", "UR22C Streaming L", "UR22C Streaming R"].iter().map(|s| s.to_string()).collect();
        assert_eq!(select_input_channels(&names, None), Ok([2, 3]));
        assert_eq!(select_input_channels(&names, Some([0, 1])), Ok([0, 1]));
        assert!(select_input_channels(&names, Some([0, 9])).is_err());
        assert!(select_input_channels(&names, Some([1, 1])).is_err());
        let odd: Vec<String> = ["A", "B", "Loop L", "Loop R"].iter().map(|s| s.to_string()).collect();
        assert_eq!(select_input_channels(&odd, None), Ok([2, 3]));
        let two: Vec<String> = ["A", "B"].iter().map(|s| s.to_string()).collect();
        assert!(select_input_channels(&two, None).is_err());
        let rev: Vec<String> = ["x", "Streaming 1", "y", "Streaming 2"].iter().map(|s| s.to_string()).collect();
        assert_eq!(select_input_channels(&rev, None), Ok([1, 3]));
    }

    #[test]
    fn measure_maths_by_hand() {
        let tb = Timebase { tick_hz: 1_000_000, asio_rate: 48000, buffer_frames: 256, input_latency: 280, output_latency: 375 };
        let lb = Detection { qpc: 100_000, offset: 100, block_len: 256, peak: 0.01, channel: 0, threshold: 0.0001 };
        let cap = Detection { qpc: 60_000, offset: 5, block_len: 480, peak: 0.01, channel: 0, threshold: 0.001 };
        let m = measure(&tb, 10_000, Some(10_000 + 8_000), Some(&cap), &lb, 4.0);
        assert!((m.raw_callback_ms - 90.0).abs() < 1e-9);
        // j = 96; total = 90 - (256 - 96)/48 ms
        assert!((m.total_ms - (90.0 - 160.0 / 48.0)).abs() < 1e-9);
        // corrected = total - (280-256)/48 ms
        assert!((m.corrected_ms - (m.total_ms - 24.0 / 48.0)).abs() < 1e-9);
        assert_eq!(m.stage1_ms, Some(50.0));
        assert_eq!(m.render_queue_ms, Some(8.0));
        assert!((m.stage2_ms.unwrap() - (m.corrected_ms - 50.0)).abs() < 1e-9);
        assert!((m.to_dac_range_ms[1] - m.to_dac_range_ms[0] - 119.0 / 48.0).abs() < 1e-9);
        assert!(m.to_dac_range_ms[0] < m.to_dac_mid_ms() && m.to_dac_mid_ms() < m.to_dac_range_ms[1]);
    }

    #[test]
    fn synthetic_loopback_recovers_known_latency_at_all_phases() {
        let p = SyntheticPath::default();
        let mut worst = 0.0f64;
        for phase in (0..21_333).step_by(997) {
            let o = synthetic_run(&p, phase).expect("detected");
            let err = (o.measurement.corrected_ms - o.truth_corrected_ms).abs();
            worst = worst.max(err);
            assert!(err < 0.06, "phase {phase}: measured {:.3} truth {:.3}", o.measurement.corrected_ms, o.truth_corrected_ms);
            let s1 = o.measurement.stage1_ms.unwrap();
            assert!((s1 - o.truth_stage1_ms).abs() < 1e-9);
            // render 9 ms + up to one 10 ms packet + lag
            assert!(s1 > 9.0 && s1 < 9.0 + 10.0 + 0.7 + 0.1, "stage1 {s1}");
            // the true total (render + chain) is what we put in
            assert!((o.truth_corrected_ms - 42.0).abs() < 0.05);
        }
        assert!(worst < 0.06);
    }

    #[test]
    fn synthetic_loopback_other_geometries() {
        for (block, in_lat, out_lat, shape) in [
            (128u32, 200i32, 290i32, ImpulseShape::default()),
            (512, 728, 871, ImpulseShape::Chirp { f0_hz: 800.0, f1_hz: 4000.0, dur_ms: 4.0 }),
            (256, 256, 256, ImpulseShape::Burst { freq_hz: 2000.0, dur_ms: 1.5 }),
        ] {
            let p = SyntheticPath { block, in_lat, out_lat, shape, chain_ms: 27.4, ..SyntheticPath::default() };
            for phase in [0i64, 3333, 10_000, 17_777] {
                let o = synthetic_run(&p, phase).unwrap();
                let err = (o.measurement.corrected_ms - o.truth_corrected_ms).abs();
                assert!(err < 0.06, "block {block} phase {phase}: err {err}");
            }
        }
    }

    #[test]
    fn stats_and_outliers() {
        let s = stats(&[40.0, 41.0, 42.0, 43.0, 44.0]).unwrap();
        assert_eq!((s.n, s.min, s.median, s.mean, s.max), (5, 40.0, 42.0, 42.0, 44.0));
        assert!((s.sigma - 1.5811388).abs() < 1e-6);
        assert!(stats(&[]).is_none());
        assert_eq!(stats(&[7.0]).unwrap().sigma, 0.0);
        assert_eq!(median_of(&[1.0, 2.0, 3.0, 10.0]), 2.5);
        let v = [40.0, 40.5, 41.0, 40.2, 40.8, 70.0];
        assert_eq!(outliers(&v, 2.5), vec![false, false, false, false, false, true]);
        assert_eq!(outliers(&[1.0, 50.0], 2.5), vec![false, false]); // too few
        assert!(outliers(&[40.0, 40.1, 40.2, 40.3, 40.1], 2.5).iter().all(|&o| !o));
    }

    #[test]
    fn band_detector_reference_is_consistent() {
        let shape = ImpulseShape::Burst { freq_hz: 18500.0, dur_ms: 2.0 };
        let g = band_gain_db(shape, 48000, 16000.0);
        // 2 ms burst, 32-sample RMS window: the envelope peak is a few dB under the click peak
        assert!(g < -2.0 && g > -9.0, "{g}");
        let lo = band_onset_bias_frames(shape, 48000, 16000.0, 0.0001, 0.01);
        let hi = band_onset_bias_frames(shape, 48000, 16000.0, 0.004, 0.01);
        assert!(lo <= hi && hi > 0.0, "{lo} {hi}");
        assert_eq!(band_onset_bias_frames(shape, 48000, 16000.0, 0.1, 0.0), 0.0);
    }

    #[test]
    fn onset_bias_follows_the_threshold_ratio() {
        let s = ImpulseShape::default();
        let lo = onset_bias_frames(s, 48000, 0.0001, 0.01);
        let hi = onset_bias_frames(s, 48000, 0.005, 0.01);
        assert!(lo <= hi && hi > 0.0);
        assert_eq!(onset_bias_frames(s, 48000, 0.1, 0.0), 0.0);
    }
}
