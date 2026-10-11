//! Streaming fractional-ratio stereo resampler, PI drift controller, startup gate
//! and the [`RateMatcher`] that glues them to the ring for the ASIO callback.
//!
//! All types here are pure (no I/O, no OS calls) and allocation-free after
//! construction, so they can run in the ASIO callback and be simulated in tests.
//!
//! Conventions: `ratio` is *input frames consumed per output frame*
//! (`cable_rate / asio_rate`, times the drift trim). The interpolation kernel is a
//! Kaiser-windowed sinc whose cutoff follows the nominal ratio, so both upsampling
//! (images) and downsampling (aliases) are suppressed.

use crate::ring::Consumer;
use eq_core::{GainRamp, ResamplerQuality};

/// Size of the internal input scratch (frames).
const SCRATCH: usize = 512;

/// Source of stereo frames (implemented for the ring [`Consumer`]).
pub trait FrameSource {
    /// Frames currently available.
    fn fill(&self) -> usize;
    /// Pops up to `min(l.len(), r.len())` frames, returns how many.
    fn pop(&mut self, l: &mut [f32], r: &mut [f32]) -> usize;
    /// Drops the oldest `n` frames, returns how many were dropped.
    fn discard(&mut self, n: usize) -> usize;
}

impl FrameSource for Consumer {
    fn fill(&self) -> usize {
        Consumer::fill(self)
    }
    fn pop(&mut self, l: &mut [f32], r: &mut [f32]) -> usize {
        self.pop_frames(l, r)
    }
    fn discard(&mut self, n: usize) -> usize {
        Consumer::discard(self, n)
    }
}

/// Interpolation taps of the high-quality (`Cubic`) mode.
const TAPS: usize = 32;
/// The interpolation point lies between `h[HALF - 1]` and `h[HALF]`.
const HALF: usize = TAPS / 2;
/// Fractional-phase resolution of the coefficient table (rows are linearly interpolated).
const PHASES: usize = 256;
/// Passband edge of the interpolation filter, cycles per *input* sample (0.45 = 19.8 kHz at 44.1 kHz).
const CUTOFF: f64 = 0.45;
/// Kaiser window beta.
const KAISER_BETA: f64 = 9.0;

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0f64, 1.0f64);
    let q = x * x / 4.0;
    for k in 1..60 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < 1e-18 * sum {
            break;
        }
    }
    sum
}

/// Polyphase windowed-sinc coefficient table: `(PHASES + 1)` rows of `TAPS`, row `p` is the
/// interpolation kernel for fractional position `p / PHASES` (each row normalised to unity DC gain).
/// `cutoff` is the lowpass edge in cycles per input sample (reduced for downsampling).
fn build_sinc_table(cutoff: f64) -> Box<[f32]> {
    let fc2 = 2.0 * cutoff;
    let i0b = bessel_i0(KAISER_BETA);
    let mut t = vec![0.0f32; (PHASES + 1) * TAPS];
    for p in 0..=PHASES {
        let mu = p as f64 / PHASES as f64;
        let mut row = [0.0f64; TAPS];
        let mut sum = 0.0;
        for (k, c) in row.iter_mut().enumerate() {
            let d = (HALF - 1) as f64 + mu - k as f64;
            let x = fc2 * d;
            let sinc = if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
            let r = d / HALF as f64;
            let w = if r.abs() >= 1.0 { 0.0 } else { bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / i0b };
            *c = fc2 * sinc * w;
            sum += *c;
        }
        for (k, c) in row.iter().enumerate() {
            t[p * TAPS + k] = (c / sum) as f32;
        }
    }
    t.into_boxed_slice()
}

/// Streaming stereo resampler: linear, or (`Cubic`, the default) a 32-tap Kaiser-windowed-sinc
/// polyphase interpolator.
///
/// The old 4-point Catmull-Rom "cubic" had no image rejection: a 10 kHz tone resampled 44.1 -> 48 kHz
/// came out with a -32 dB spurious tone at 13.9 kHz (measured on hardware), i.e. audible
/// harshness on cymbals / sibilants. The sinc kernel keeps images below -80 dB for content up to
/// ~19 kHz, costs ~0.1 % CPU and adds 15 input frames (0.3 ms) of latency.
///
/// Carries the fractional phase and `TAPS` history frames across calls. Input is
/// pulled on demand through a closure, so a call that must deliver exactly `N`
/// output frames pulls only the input frames it needs (plus at most one rounding
/// frame, kept in an internal scratch for the next call).
pub struct Resampler {
    quality: ResamplerQuality,
    hl: [f32; TAPS],
    hr: [f32; TAPS],
    /// Phase between `h[HALF-1]` and `h[HALF]`; may be >= 1 when a shift is pending.
    mu: f64,
    table: Box<[f32]>,
    in_l: Box<[f32]>,
    in_r: Box<[f32]>,
    in_pos: usize,
    in_len: usize,
}

#[inline]
fn interp(q: ResamplerQuality, h: &[f32; TAPS], mu: f32, table: &[f32]) -> f32 {
    match q {
        ResamplerQuality::Linear => h[HALF - 1] + mu * (h[HALF] - h[HALF - 1]),
        ResamplerQuality::Cubic => {
            let x = mu * PHASES as f32;
            let p = (x as usize).min(PHASES - 1);
            let frac = x - p as f32;
            let r0 = &table[p * TAPS..(p + 1) * TAPS];
            let r1 = &table[(p + 1) * TAPS..(p + 2) * TAPS];
            let mut acc = 0.0f32;
            for k in 0..TAPS {
                acc += h[k] * (r0[k] + frac * (r1[k] - r0[k]));
            }
            acc
        }
    }
}

impl Resampler {
    /// Creates a resampler for a nominal ratio of 1 (allocates the scratch and the table).
    pub fn new(quality: ResamplerQuality) -> Self {
        Self::with_nominal_ratio(quality, 1.0)
    }

    /// Creates a resampler whose interpolation filter is designed for `nominal` input frames per
    /// output frame (the cutoff drops below the output Nyquist when downsampling).
    pub fn with_nominal_ratio(quality: ResamplerQuality, nominal: f64) -> Self {
        let nominal = if nominal.is_finite() && nominal > 0.0 { nominal.clamp(0.05, 16.0) } else { 1.0 };
        Resampler {
            quality,
            hl: [0.0; TAPS],
            hr: [0.0; TAPS],
            mu: 0.0,
            table: build_sinc_table(CUTOFF / nominal.max(1.0)),
            in_l: vec![0.0; SCRATCH].into_boxed_slice(),
            in_r: vec![0.0; SCRATCH].into_boxed_slice(),
            in_pos: 0,
            in_len: 0,
        }
    }

    /// Changes the interpolation mode (state is kept).
    pub fn set_quality(&mut self, q: ResamplerQuality) {
        self.quality = q;
    }

    /// Clears history, phase and buffered input.
    pub fn reset(&mut self) {
        self.hl = [0.0; TAPS];
        self.hr = [0.0; TAPS];
        self.mu = 0.0;
        self.in_pos = 0;
        self.in_len = 0;
    }

    /// Produces up to `min(out_l.len(), out_r.len())` output frames, pulling input
    /// with `src(l, r) -> frames_written` (returning 0 means "no data now").
    /// Returns the number of frames produced; fewer than requested means the
    /// source ran dry (state stays consistent for the next call).
    pub fn process<F>(&mut self, ratio: f64, out_l: &mut [f32], out_r: &mut [f32], src: &mut F) -> usize
    where
        F: FnMut(&mut [f32], &mut [f32]) -> usize,
    {
        let n = out_l.len().min(out_r.len());
        if n == 0 {
            return 0;
        }
        let ratio = if ratio.is_finite() && ratio > 0.0 { ratio.clamp(0.05, 16.0) } else { 1.0 };
        let total_shifts = (self.mu + (n - 1) as f64 * ratio).floor() as usize;
        let mut want = total_shifts.saturating_sub(self.in_len - self.in_pos);
        for k in 0..n {
            while self.mu >= 1.0 {
                if self.in_pos == self.in_len {
                    let req = want.clamp(1, SCRATCH);
                    let got = src(&mut self.in_l[..req], &mut self.in_r[..req]).min(req);
                    if got == 0 {
                        return k;
                    }
                    self.in_pos = 0;
                    self.in_len = got;
                    want = want.saturating_sub(got);
                }
                self.hl.copy_within(1.., 0);
                self.hr.copy_within(1.., 0);
                self.hl[TAPS - 1] = self.in_l[self.in_pos];
                self.hr[TAPS - 1] = self.in_r[self.in_pos];
                self.in_pos += 1;
                self.mu -= 1.0;
            }
            let mu = self.mu as f32;
            out_l[k] = interp(self.quality, &self.hl, mu, &self.table);
            out_r[k] = interp(self.quality, &self.hr, mu, &self.table);
            self.mu += ratio;
        }
        n
    }
}

/// Drift controller: trims the resampling ratio so the ring fill tracks a target
/// **without modulating the ratio** at audio-relevant rates.
///
/// The raw ring fill is a sawtooth: the capture side delivers a packet every `P_c` ms while the
/// ASIO callback consumes every `P_a` ms, so the fill sampled at callback instants beats with a
/// period `1/|1/P_c - 1/P_a|` (about 160 ms for 10 ms packets vs 10.67 ms blocks) and an amplitude
/// of a whole packet. A plain PI controller on a low-passed fill passes a few percent of that into
/// the ratio, i.e. the pitch is frequency-modulated by 50..500 ppm at ~6 Hz. Measured on a 1 kHz tone
/// that is -50 dB sidebands (and proportionally worse at high frequencies): audible as roughness.
///
/// This controller is model based instead. State: a smooth fill estimate `est` and the clock
/// offset `delta` (capture rate / nominal - 1). Each block it *predicts* the fill from the ratio it
/// actually applied (`est += fs * dt * (delta - trim)`) and corrects with the sampled fill through a
/// critically damped alpha-beta filter whose time constant is seconds, so the sawtooth is averaged
/// out. The applied trim is `delta + (est - target) / (TAU_C * fs)`, slew-limited, so any residual
/// ripple in the estimates is bounded to well under 1 ppm at the beat rate.
#[derive(Clone, Debug)]
pub struct DriftController {
    target: f64,
    fs: f64,
    /// Smooth fill estimate (frames).
    est: f64,
    /// Estimated clock offset (fraction; +1e-4 = capture runs 100 ppm fast).
    delta: f64,
    /// Seconds since creation / `reset` (drives the gain schedule).
    age: f64,
    /// Running sum / count used for the initial mean-fill measurement.
    init_sum: f64,
    init_n: u32,
    max_trim: f64,
    trim: f64,
    /// Fill residuals beyond this many frames are treated as a real step (discard, stall).
    step_thresh: f64,
}

/// Maximum ratio trim (+-0.5 %).
pub const MAX_TRIM: f64 = 0.005;
/// Seconds spent measuring the mean fill before the estimator starts (> several beat periods).
const MEASURE_SECS: f64 = 1.0;
/// Estimator time constant (s): starts at `TAU_MIN` and grows with the age of the stream (like a
/// running average) up to `TAU_MAX`.
const TAU_MIN: f64 = 1.0;
const TAU_MAX: f64 = 25.0;
/// Time constant (s) with which the smooth fill error is pulled to zero: `max(TAU_C_MIN, age)` up to `TAU_C_MAX`.
const TAU_C_MIN: f64 = 8.0;
const TAU_C_MAX: f64 = 30.0;
/// Largest rate of change of the applied trim (fraction per second = 40 ppm/s).
const MAX_SLEW: f64 = 40e-6;

impl DriftController {
    /// `target_frames`: desired ring fill (input frames); `input_rate_hz`: cable rate.
    pub fn new(target_frames: usize, input_rate_hz: f64) -> Self {
        DriftController {
            target: target_frames as f64,
            fs: input_rate_hz.max(1.0),
            est: target_frames as f64,
            delta: 0.0,
            age: 0.0,
            init_sum: 0.0,
            init_n: 0,
            max_trim: MAX_TRIM,
            trim: 0.0,
            step_thresh: (target_frames as f64).max(1024.0),
        }
    }

    /// Target fill in frames.
    pub fn target(&self) -> usize {
        self.target as usize
    }

    /// Changes the target fill (keeps the learned clock offset and the fill estimate).
    pub fn set_target(&mut self, target_frames: usize) {
        self.target = target_frames as f64;
        self.step_thresh = self.target.max(1024.0);
    }

    /// Last computed trim (fraction, e.g. 2e-4 = +200 ppm).
    pub fn trim(&self) -> f64 {
        self.trim
    }

    /// Smooth fill estimate in frames.
    pub fn filtered_fill(&self) -> f64 {
        self.est
    }

    /// Learned clock offset (fraction).
    pub fn offset(&self) -> f64 {
        self.delta
    }

    /// Restart the fill estimate at the target and re-enter acquisition mode (keeps the
    /// learned clock offset). Called when the stream (re)starts at exactly the target fill.
    pub fn restart_filter(&mut self) {
        self.est = self.target;
    }

    /// The fill changed by a known amount (frames discarded): follow it instead of treating it as noise.
    pub fn resync(&mut self, fill_now: usize) {
        self.est = fill_now as f64;
    }

    /// Forget everything including the learned offset.
    pub fn reset(&mut self) {
        self.delta = 0.0;
        self.trim = 0.0;
        self.age = 0.0;
        self.init_sum = 0.0;
        self.init_n = 0;
        self.restart_filter();
    }

    /// Feed a fill measurement taken `dt` seconds after the previous one; returns the
    /// ratio trim to apply as `ratio = nominal * (1 + trim)`.
    pub fn update(&mut self, fill: usize, dt: f64) -> f64 {
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        self.age += dt;
        if self.age < MEASURE_SECS {
            // The fill is a sawtooth around its mean; average it over several beats first.
            self.init_sum += fill as f64;
            self.init_n += 1;
            self.est = self.init_sum / f64::from(self.init_n);
            return self.trim;
        }
        let tau = self.age.clamp(TAU_MIN, TAU_MAX);
        // Predict from the ratio that was actually applied.
        self.est += self.fs * dt * (self.delta - self.trim);
        let mut r = fill as f64 - self.est;
        if r.abs() > self.step_thresh {
            self.est += r; // real step, not beat noise
            r = 0.0;
        }
        if dt > 0.0 {
            let g = 1.0 - (-dt / tau).exp();
            let h = g * g / (2.0 - g);
            self.est += g * r;
            self.delta = (self.delta + h * r / (self.fs * dt)).clamp(-self.max_trim, self.max_trim);
        }
        let tau_c = self.age.clamp(TAU_C_MIN, TAU_C_MAX);
        let want = (self.delta + (self.est - self.target) / (tau_c * self.fs)).clamp(-self.max_trim, self.max_trim);
        let step = MAX_SLEW * dt;
        self.trim = if dt > 0.0 { self.trim + (want - self.trim).clamp(-step, step) } else { self.trim };
        self.trim
    }
}

/// Holds the output silent until the ring has pre-filled to the target; re-arms
/// after a run of consecutive underruns.
#[derive(Clone, Debug)]
pub struct StartupGate {
    target: usize,
    open: bool,
    consecutive: u32,
    rearm_after: u32,
    rearms: u64,
}

impl StartupGate {
    /// New (closed) gate.
    pub fn new(target_frames: usize, rearm_after: u32) -> Self {
        StartupGate { target: target_frames, open: false, consecutive: 0, rearm_after: rearm_after.max(1), rearms: 0 }
    }
    /// True once streaming.
    pub fn is_open(&self) -> bool {
        self.open
    }
    /// Changes the pre-fill target used when (re)opening.
    pub fn set_target(&mut self, target_frames: usize) {
        self.target = target_frames;
    }
    /// Number of re-arms since creation.
    pub fn rearms(&self) -> u64 {
        self.rearms
    }
    /// Opens the gate when `fill >= target`; returns the (new) open state.
    pub fn update(&mut self, fill: usize) -> bool {
        if !self.open && fill >= self.target {
            self.open = true;
            self.consecutive = 0;
        }
        self.open
    }
    /// Reports one block's outcome; returns true if the gate just re-armed (closed).
    pub fn report(&mut self, underrun: bool) -> bool {
        if !self.open {
            return false;
        }
        if !underrun {
            self.consecutive = 0;
            return false;
        }
        self.consecutive += 1;
        if self.consecutive >= self.rearm_after {
            self.open = false;
            self.consecutive = 0;
            self.rearms += 1;
            return true;
        }
        false
    }
    /// Closes the gate (e.g. after a device restart).
    pub fn close(&mut self) {
        self.open = false;
        self.consecutive = 0;
    }
}

/// Target ring fill in input (cable) frames: safety buffer + one master buffer
/// (converted to cable frames) + one capture period (the producer delivers in
/// bursts of that size).
pub fn target_fill_frames(
    safety_ms: f32,
    master_buffer_frames: u32,
    cable_rate: u32,
    asio_rate: u32,
    capture_period_frames: u32,
) -> usize {
    let safety = f64::from(safety_ms.max(0.0)) * 1e-3 * f64::from(cable_rate);
    let buf = f64::from(master_buffer_frames) * f64::from(cable_rate) / f64::from(asio_rate.max(1));
    (safety + buf).ceil() as usize + capture_period_frames as usize
}

/// Ring capacity to request for a given target (4x the target, at least 4096).
pub fn ring_capacity_for(target_frames: usize) -> usize {
    (target_frames * 4).max(4096).next_power_of_two()
}

/// Configuration for [`RateMatcher`].
#[derive(Clone, Debug)]
pub struct RateMatcherConfig {
    pub cable_rate: u32,
    pub asio_rate: u32,
    pub target_fill: usize,
    pub quality: ResamplerQuality,
    /// Consecutive underrun blocks before the startup gate re-arms.
    pub rearm_after: u32,
    /// Fade length in frames for underrun fade-out / restart fade-in.
    pub fade_frames: u32,
}

impl RateMatcherConfig {
    /// Defaults: re-arm after 8 underrun blocks, 256-frame fades.
    pub fn new(cable_rate: u32, asio_rate: u32, target_fill: usize, quality: ResamplerQuality) -> Self {
        RateMatcherConfig { cable_rate, asio_rate, target_fill, quality, rearm_after: 8, fade_frames: 256 }
    }
}

/// What happened during one [`RateMatcher::render`] call.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderStats {
    /// Real (resampled) frames written; the rest is silence/hold.
    pub produced: usize,
    /// The source ran dry mid-block.
    pub underrun: bool,
    /// The startup gate was closed (block is silence, not counted as underrun).
    pub gated: bool,
    /// Frames the consumer discarded because the fill exceeded twice the target.
    pub discarded: usize,
    /// Ratio used (input frames per output frame).
    pub ratio: f64,
    /// Ring fill (frames) measured at the start of the block, after trimming.
    pub fill: usize,
}

/// Pull-based rate matcher for the ASIO callback: startup gate, drift control,
/// resampling, underrun hold/fade and overrun trim.
pub struct RateMatcher {
    cfg: RateMatcherConfig,
    nominal: f64,
    resampler: Resampler,
    drift: DriftController,
    gate: StartupGate,
    fade: GainRamp,
    faded_out: bool,
    last: (f32, f32),
    underruns: u64,
    discarded_total: u64,
    /// Extra fill (frames) the gate waits for before opening, see [`RateMatcher::set_open_bias`].
    open_bias: usize,
    /// Crossfaded input frames pending (mid-stream overrun trim), consumed before the ring.
    xf_l: Box<[f32]>,
    xf_r: Box<[f32]>,
    xf_pos: usize,
    xf_len: usize,
}

/// Length (input frames) of the crossfade that hides a mid-stream overrun trim.
const XFADE: usize = 64;

impl RateMatcher {
    /// Creates a matcher (allocates once). The gate starts closed.
    pub fn new(cfg: RateMatcherConfig) -> Self {
        let nominal = f64::from(cfg.cable_rate) / f64::from(cfg.asio_rate.max(1));
        RateMatcher {
            nominal,
            resampler: Resampler::with_nominal_ratio(cfg.quality, nominal),
            drift: DriftController::new(cfg.target_fill, f64::from(cfg.cable_rate)),
            gate: StartupGate::new(cfg.target_fill, cfg.rearm_after),
            fade: GainRamp::new(0.0),
            faded_out: true,
            last: (0.0, 0.0),
            underruns: 0,
            discarded_total: 0,
            open_bias: 0,
            xf_l: vec![0.0; XFADE].into_boxed_slice(),
            xf_r: vec![0.0; XFADE].into_boxed_slice(),
            xf_pos: 0,
            xf_len: 0,
            cfg,
        }
    }

    /// Total underrun blocks.
    pub fn underruns(&self) -> u64 {
        self.underruns
    }
    /// Total frames discarded by overrun trimming.
    pub fn discarded_frames(&self) -> u64 {
        self.discarded_total
    }
    /// Number of times the gate re-armed after repeated underruns.
    pub fn rearms(&self) -> u64 {
        self.gate.rearms()
    }
    /// True while streaming (gate open).
    pub fn is_running(&self) -> bool {
        self.gate.is_open()
    }
    /// Current drift trim.
    pub fn trim(&self) -> f64 {
        self.drift.trim()
    }
    /// Target fill in cable frames.
    pub fn target_fill(&self) -> usize {
        self.cfg.target_fill
    }
    /// Hot-changes the target fill (safety-buffer slider); allocation-free.
    pub fn set_target_fill(&mut self, target_frames: usize) {
        self.cfg.target_fill = target_frames;
        self.drift.set_target(target_frames);
        self.gate.set_target(target_frames + self.open_bias);
    }
    /// Start-up bias: the gate opens when the fill reaches `target + bias` and trims to that, instead
    /// of `target`. The gate opens right after an arrival, i.e. at the *top* of the fill sawtooth, and
    /// the drift controller needs tens of seconds to pull the *mean* fill up to the target; opening
    /// at `target + (peak - mean)` (the arrival dip, see `arrival.rs`) starts with the mean already at
    /// the target. Only affects (re)opening; 0 = off (default). Allocation-free.
    pub fn set_open_bias(&mut self, bias_frames: usize) {
        self.open_bias = bias_frames;
        self.gate.set_target(self.cfg.target_fill + bias_frames);
    }
    /// Closes the gate and silences output until the ring pre-fills again.
    pub fn rearm(&mut self) {
        self.gate.close();
        self.fade = GainRamp::new(0.0);
        self.faded_out = true;
        self.xf_pos = self.xf_len;
    }
    /// Changes interpolation quality.
    pub fn set_quality(&mut self, q: ResamplerQuality) {
        self.cfg.quality = q;
        self.resampler.set_quality(q);
    }

    /// Fills `out_l`/`out_r` completely (silence, hold/fade or resampled audio).
    pub fn render<S: FrameSource>(&mut self, src: &mut S, out_l: &mut [f32], out_r: &mut [f32]) -> RenderStats {
        let n = out_l.len().min(out_r.len());
        let (out_l, out_r) = (&mut out_l[..n], &mut out_r[..n]);
        let mut st = RenderStats { ratio: self.nominal, ..RenderStats::default() };
        let mut fill = src.fill() + (self.xf_len - self.xf_pos);
        if !self.gate.is_open() {
            if !self.gate.update(fill) {
                out_l.fill(0.0);
                out_r.fill(0.0);
                st.gated = true;
                st.fill = fill;
                return st;
            }
            // Just opened: start from a clean state and fade in.
            self.resampler.reset();
            self.xf_pos = self.xf_len;
            self.drift.restart_filter();
            self.fade = GainRamp::new(0.0);
            self.fade.set_target(1.0, self.cfg.fade_frames);
            self.faded_out = false;
            // The ring may have overshot the target while the gate was closed (the producer
            // delivers in bursts); start exactly at the target.
            let open_at = self.cfg.target_fill + self.open_bias;
            if fill > open_at {
                let d = src.discard(fill - open_at);
                st.discarded += d;
                self.discarded_total += d as u64;
                fill = src.fill();
            }
        }
        if fill > self.cfg.target_fill.saturating_mul(2) {
            let excess = fill - self.cfg.target_fill;
            let d = if self.xf_pos == self.xf_len && excess >= 3 * XFADE && src.fill() >= excess + XFADE {
                // Mid-stream: dropping input is a waveform discontinuity, so crossfade from the old
                // continuation (A) into the stream after the dropped span (B) over XFADE frames.
                let mut al = [0.0f32; XFADE];
                let mut ar = [0.0f32; XFADE];
                src.pop(&mut al, &mut ar);
                let dropped = src.discard(excess - XFADE);
                src.pop(&mut self.xf_l, &mut self.xf_r);
                for i in 0..XFADE {
                    let w = 0.5 - 0.5 * (std::f32::consts::PI * (i as f32 + 0.5) / XFADE as f32).cos();
                    self.xf_l[i] = al[i] + w * (self.xf_l[i] - al[i]);
                    self.xf_r[i] = ar[i] + w * (self.xf_r[i] - ar[i]);
                }
                self.xf_pos = 0;
                self.xf_len = XFADE;
                dropped + XFADE
            } else {
                src.discard(excess)
            };
            st.discarded += d;
            self.discarded_total += d as u64;
            fill = src.fill() + (self.xf_len - self.xf_pos);
            self.drift.resync(fill);
        }
        st.fill = fill;
        let dt = n as f64 / f64::from(self.cfg.asio_rate.max(1));
        let trim = self.drift.update(fill, dt);
        let ratio = self.nominal * (1.0 + trim);
        st.ratio = ratio;
        let (xf_l, xf_r, xf_pos, xf_len) = (&self.xf_l, &self.xf_r, &mut self.xf_pos, self.xf_len);
        let produced = self.resampler.process(ratio, out_l, out_r, &mut |l: &mut [f32], r: &mut [f32]| {
            if *xf_pos < xf_len {
                let k = l.len().min(r.len()).min(xf_len - *xf_pos);
                l[..k].copy_from_slice(&xf_l[*xf_pos..*xf_pos + k]);
                r[..k].copy_from_slice(&xf_r[*xf_pos..*xf_pos + k]);
                *xf_pos += k;
                k
            } else {
                src.pop(l, r)
            }
        });
        st.produced = produced;
        if produced > 0 {
            self.last = (out_l[produced - 1], out_r[produced - 1]);
            if produced == n && self.faded_out {
                self.fade.set_target(1.0, self.cfg.fade_frames);
                self.faded_out = false;
            }
            if self.fade.is_ramping() || self.fade.gain() != 1.0 {
                self.fade.apply_stereo(&mut out_l[..produced], &mut out_r[..produced]);
            }
        }
        if produced < n {
            st.underrun = true;
            self.underruns += 1;
            let tail = n - produced;
            out_l[produced..].fill(self.last.0);
            out_r[produced..].fill(self.last.1);
            if !self.faded_out {
                self.fade.set_target(0.0, self.cfg.fade_frames.min(tail as u32).max(1));
                self.faded_out = true;
            }
            self.fade.apply_stereo(&mut out_l[produced..], &mut out_r[produced..]);
        }
        if self.gate.report(st.underrun) {
            self.fade = GainRamp::new(0.0);
            self.faded_out = true;
        }
        st
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring;
    use std::f64::consts::PI;

    /// Feeds a sine of `f` Hz sampled at `rin` through the resampler to `rout`;
    /// returns `secs` seconds of output.
    fn resample_sine(q: ResamplerQuality, f: f64, rin: f64, rout: f64, secs: f64, ratio_trim: f64) -> Vec<f32> {
        let mut rs = Resampler::new(q);
        let ratio = rin / rout * (1.0 + ratio_trim);
        let n_out = (secs * rout) as usize;
        let mut idx = 0usize;
        let mut out_l = vec![0.0f32; n_out];
        let mut out_r = vec![0.0f32; n_out];
        let mut src = |l: &mut [f32], r: &mut [f32]| {
            for i in 0..l.len() {
                let v = (2.0 * PI * f * (idx + i) as f64 / rin).sin() as f32;
                l[i] = v;
                r[i] = v;
            }
            idx += l.len();
            l.len()
        };
        // Call in awkward block sizes to exercise state carry-over.
        let mut pos = 0;
        let sizes = [1usize, 7, 64, 333, 512, 100];
        let mut s = 0;
        while pos < n_out {
            let k = sizes[s % sizes.len()].min(n_out - pos);
            s += 1;
            let got = rs.process(ratio, &mut out_l[pos..pos + k], &mut out_r[pos..pos + k], &mut src);
            assert_eq!(got, k);
            pos += k;
        }
        assert_eq!(out_l, out_r);
        out_l
    }

    /// Frequency from interpolated rising zero crossings.
    fn freq_from_crossings(x: &[f32], rate: f64) -> f64 {
        let mut first = None;
        let mut last = 0.0;
        let mut count = 0usize;
        for i in 1..x.len() {
            if x[i - 1] < 0.0 && x[i] >= 0.0 {
                let frac = -f64::from(x[i - 1]) / (f64::from(x[i]) - f64::from(x[i - 1]));
                let t = (i - 1) as f64 + frac;
                if first.is_none() {
                    first = Some(t);
                } else {
                    count += 1;
                }
                last = t;
            }
        }
        count as f64 * rate / (last - first.unwrap())
    }

    /// Residual (dB re signal) after least-squares fitting a sinusoid of
    /// `cycles_per_sample`, plus the fitted amplitude.
    fn fit_residual_db(x: &[f32], cycles_per_sample: f64) -> (f64, f64) {
        let (mut sc, mut ss, mut cc, mut ssn, mut csn, mut xx) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        for (i, &v) in x.iter().enumerate() {
            let w = 2.0 * PI * cycles_per_sample * i as f64;
            let (s, c) = w.sin_cos();
            let v = f64::from(v);
            sc += s * c;
            ss += s * s;
            cc += c * c;
            ssn += s * v;
            csn += c * v;
            xx += v * v;
        }
        let det = ss * cc - sc * sc;
        let a = (ssn * cc - csn * sc) / det;
        let b = (csn * ss - ssn * sc) / det;
        let fit_energy = a * ssn + b * csn;
        let resid = (xx - fit_energy).max(1e-30);
        ((resid / fit_energy).log10() * 10.0, (a * a + b * b).sqrt())
    }

    #[test]
    fn output_length_is_exact_and_pull_matches_ratio() {
        for &(rin, rout) in &[(44100.0, 48000.0), (48000.0, 44100.0), (96000.0, 48000.0), (48000.0, 48000.0)] {
            let mut rs = Resampler::new(ResamplerQuality::Cubic);
            let mut pulled = 0usize;
            let mut src = |l: &mut [f32], r: &mut [f32]| {
                pulled += l.len();
                l.fill(0.1);
                r.fill(0.1);
                l.len()
            };
            let (mut l, mut r) = (vec![0.0; 512], vec![0.0; 512]);
            let blocks = 200;
            for _ in 0..blocks {
                assert_eq!(rs.process(rin / rout, &mut l, &mut r, &mut src), 512);
            }
            let expect = blocks as f64 * 512.0 * rin / rout;
            assert!((pulled as f64 - expect).abs() <= 2.0, "{rin}->{rout}: pulled {pulled}, expected {expect}");
        }
    }

    #[test]
    fn pitch_is_preserved() {
        for &(rin, rout) in &[(44100.0, 48000.0), (48000.0, 44100.0), (96000.0, 48000.0), (48000.0, 96000.0)] {
            for q in [ResamplerQuality::Linear, ResamplerQuality::Cubic] {
                let y = resample_sine(q, 1000.0, rin, rout, 1.0, 0.0);
                let f = freq_from_crossings(&y[1000..], rout);
                assert!((f - 1000.0).abs() < 0.05, "{rin}->{rout} {q:?}: {f} Hz");
            }
        }
    }

    #[test]
    fn identity_ratio_is_a_pure_delay() {
        let y = resample_sine(ResamplerQuality::Cubic, 1000.0, 48000.0, 48000.0, 0.1, 0.0);
        // Output k equals input k-17 (HALF + 1 frames of interpolation latency at ratio 1).
        #[allow(clippy::needless_range_loop)]
        for k in 40..2000 {
            let expect = (2.0 * PI * 1000.0 * (k as f64 - 17.0) / 48000.0).sin() as f32;
            assert!((y[k] - expect).abs() < 1e-4, "k={k} {} vs {expect}", y[k]);
        }
    }

    #[test]
    fn cubic_beats_linear_on_10khz() {
        let mut res = Vec::new();
        for q in [ResamplerQuality::Linear, ResamplerQuality::Cubic] {
            let y = resample_sine(q, 10_000.0, 44100.0, 48000.0, 0.5, 0.0);
            let (db, amp) = fit_residual_db(&y[2000..], 10_000.0 / 48000.0);
            println!("10 kHz 44.1k->48k {q:?}: residual {db:.1} dB, amplitude {amp:.4}");
            res.push((db, amp));
        }
        let (lin, cub) = (res[0], res[1]);
        assert!(cub.0 < lin.0 - 5.0, "cubic {} dB vs linear {} dB", cub.0, lin.0);
        assert!(cub.0 < -80.0, "cubic (sinc) image/alias residual {} dB", cub.0);
        assert!(cub.1 > lin.1, "cubic keeps more HF amplitude");
    }

    /// High-quality mode: flat and clean across the audio band for 44.1 -> 48 kHz (the old
    /// 4-point Catmull-Rom left a -32 dB image at 13.9 kHz for a 10 kHz tone).
    #[test]
    fn sinc_mode_is_flat_and_clean_up_to_17khz() {
        for &f in &[100.0, 1000.0, 5000.0, 10_000.0, 15_000.0, 17_000.0] {
            let mut rs = Resampler::with_nominal_ratio(ResamplerQuality::Cubic, 44100.0 / 48000.0);
            let ratio = 44100.0 / 48000.0;
            let mut idx = 0usize;
            let mut src = |l: &mut [f32], r: &mut [f32]| {
                for i in 0..l.len() {
                    let v = (2.0 * PI * f * (idx + i) as f64 / 44100.0).sin() as f32;
                    l[i] = v;
                    r[i] = v;
                }
                idx += l.len();
                l.len()
            };
            let n = 48000;
            let (mut l, mut r) = (vec![0.0f32; n], vec![0.0f32; n]);
            assert_eq!(rs.process(ratio, &mut l, &mut r, &mut src), n);
            let (db, amp) = fit_residual_db(&l[4000..], f / 48000.0);
            println!("sinc 44.1->48 {f} Hz: residual {db:.1} dB amplitude {amp:.5}");
            assert!(db < -75.0, "{f} Hz: residual {db} dB");
            let tol = if f > 16_000.0 { 0.015 } else { 0.002 };
            assert!((amp - 1.0).abs() < tol, "{f} Hz: amplitude {amp}");
        }
    }

    /// Downsampling 96 -> 48 kHz must not fold content above 24 kHz back into the band.
    #[test]
    fn sinc_mode_rejects_aliases_when_downsampling() {
        let ratio = 2.0;
        let f = 30_000.0; // would alias to 18 kHz at 48 kHz
        let mut rs = Resampler::with_nominal_ratio(ResamplerQuality::Cubic, ratio);
        let mut idx = 0usize;
        let mut src = |l: &mut [f32], r: &mut [f32]| {
            for i in 0..l.len() {
                let v = (2.0 * PI * f * (idx + i) as f64 / 96000.0).sin() as f32;
                l[i] = v;
                r[i] = v;
            }
            idx += l.len();
            l.len()
        };
        let n = 24000;
        let (mut l, mut r) = (vec![0.0f32; n], vec![0.0f32; n]);
        assert_eq!(rs.process(ratio, &mut l, &mut r, &mut src), n);
        let peak = l[2000..].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak < 0.003, "alias amplitude {peak} (> -50 dB)");
    }

    #[test]
    fn cubic_low_frequency_error_is_tiny() {
        let y = resample_sine(ResamplerQuality::Cubic, 1000.0, 44100.0, 48000.0, 0.5, 0.0);
        let (db, amp) = fit_residual_db(&y[2000..], 1000.0 / 48000.0);
        println!("1 kHz 44.1k->48k Cubic: residual {db:.1} dB, amplitude {amp:.5}");
        assert!(db < -80.0);
        assert!((amp - 1.0).abs() < 1e-3);
    }

    #[test]
    fn starvation_returns_partial_and_resumes() {
        let mut rs = Resampler::new(ResamplerQuality::Linear);
        let budget = std::cell::Cell::new(10usize);
        let mut src = |l: &mut [f32], r: &mut [f32]| {
            let k = l.len().min(budget.get());
            budget.set(budget.get() - k);
            for i in 0..k {
                l[i] = 1.0;
                r[i] = 1.0;
            }
            k
        };
        let (mut l, mut r) = (vec![0.0; 100], vec![0.0; 100]);
        let got = rs.process(1.0, &mut l, &mut r, &mut src);
        assert!(got > 0 && got < 100, "{got}");
        budget.set(1000);
        assert_eq!(rs.process(1.0, &mut l, &mut r, &mut src), 100);
    }

    #[test]
    fn drift_controller_signs_and_limits() {
        // Past the initial mean-fill measurement window.
        let settled = |target: usize| {
            let mut d = DriftController::new(target, 48000.0);
            for _ in 0..120 {
                d.update(target, 0.01);
            }
            d
        };
        let mut d = settled(1000);
        assert!(d.update(1000, 0.01).abs() < 1e-12);
        for _ in 0..400 {
            d.update(1600, 0.01);
        }
        assert!(d.trim() > 0.0, "too full -> consume faster");
        let mut d = settled(1000);
        for _ in 0..400 {
            d.update(400, 0.01);
        }
        assert!(d.trim() < 0.0, "too empty -> consume slower");
        let mut d = settled(1000);
        for _ in 0..100_000 {
            d.update(1_000_000, 0.01);
        }
        assert!((d.trim() - MAX_TRIM).abs() < 1e-12);
    }

    /// The applied trim never moves faster than the slew limit, whatever the input does.
    #[test]
    fn drift_trim_is_slew_limited() {
        let mut d = DriftController::new(1000, 48000.0);
        for _ in 0..120 {
            d.update(1000, 0.01);
        }
        let mut prev = d.trim();
        for i in 0..5000 {
            let fill = if i % 2 == 0 { 1900 } else { 100 };
            let t = d.update(fill, 0.0107);
            assert!((t - prev).abs() <= MAX_SLEW * 0.0107 + 1e-12);
            prev = t;
        }
    }

    #[test]
    fn gate_opens_at_target_and_rearms() {
        let mut g = StartupGate::new(100, 3);
        assert!(!g.update(99));
        assert!(g.update(100));
        assert!(!g.report(true));
        assert!(!g.report(false)); // clean block resets the run
        assert!(!g.report(true));
        assert!(!g.report(true));
        assert!(g.report(true));
        assert!(!g.is_open());
        assert_eq!(g.rearms(), 1);
        assert!(g.update(500));
    }

    #[test]
    fn target_and_capacity_helpers() {
        // 2 ms safety + 512 @48k + 480 capture period at 48k/48k
        assert_eq!(target_fill_frames(2.0, 512, 48000, 48000, 480), 96 + 512 + 480);
        assert_eq!(target_fill_frames(0.0, 512, 44100, 48000, 0), 471);
        assert_eq!(ring_capacity_for(1088), 4096 * 2);
        assert_eq!(ring_capacity_for(10), 4096);
    }

    struct SimResult {
        min_fill: usize,
        max_fill: usize,
        underruns: u64,
        rearms: u64,
        discarded: u64,
        final_trim: f64,
        max_trim_after_settle: f64,
    mean_trim_late: f64,
        /// Peak-to-peak of the trim over the last 20 s.
        trim_ptp_late: f64,
        trim_range_late: f64,
        out: Vec<f32>,
    }

    /// Event-driven simulation: capture pushes `period` frames at `cap_rate`; ASIO
    /// calls `render` with `block` frames at `asio_rate`. 1 kHz sine in capture time.
    #[allow(clippy::too_many_arguments)]
    fn simulate(
        cable_nominal: f64,
        ppm: f64,
        asio_rate: f64,
        block: usize,
        period: usize,
        safety_ms: f32,
        secs: f64,
        keep_out: bool,
    ) -> SimResult {
        let cap_rate = cable_nominal * (1.0 + ppm * 1e-6);
        let target = target_fill_frames(safety_ms, block as u32, cable_nominal as u32, asio_rate as u32, period as u32);
        let (mut p, mut c) = ring::channel(ring_capacity_for(target));
        let mut rm = RateMatcher::new(RateMatcherConfig::new(
            cable_nominal as u32,
            asio_rate as u32,
            target,
            ResamplerQuality::Cubic,
        ));
        let cap_dt = period as f64 / cap_rate;
        let asio_dt = block as f64 / asio_rate;
        let (mut t_cap, mut t_asio) = (0.0f64, 0.37 * cap_dt); // arbitrary phase offset
        let mut cap_idx = 0usize;
        let (mut l, mut r) = (vec![0.0f32; block], vec![0.0f32; block]);
        let (mut cl, mut cr) = (vec![0.0f32; period], vec![0.0f32; period]);
        let mut min_fill = usize::MAX;
        let mut max_fill = 0usize;
        let mut opened_at: Option<f64> = None;
        let mut underruns_at_open = 0;
        let mut max_trim = 0.0f64;
        let (mut late_sum, mut late_n) = (0.0f64, 0usize);
        let (mut tmin, mut tmax) = (f64::INFINITY, f64::NEG_INFINITY);
        let mut tmax_ptp_1s = 0.0f64;
        let mut trims_late: Vec<f64> = Vec::new();
        let mut out = Vec::new();
        while t_asio < secs {
            if t_cap <= t_asio {
                for i in 0..period {
                    let v = (0.5 * (2.0 * PI * 1000.0 * (cap_idx + i) as f64 / cap_rate).sin()) as f32;
                    cl[i] = v;
                    cr[i] = v;
                }
                cap_idx += period;
                p.push_frames(&cl, &cr);
                t_cap += cap_dt;
            } else {
                let st = rm.render(&mut c, &mut l, &mut r);
                if !st.gated {
                    if opened_at.is_none() {
                        opened_at = Some(t_asio);
                        underruns_at_open = rm.underruns();
                    }
                    if t_asio > opened_at.unwrap() + 0.5 {
                        min_fill = min_fill.min(st.fill);
                        max_fill = max_fill.max(st.fill);
                    }
                    if t_asio > secs - 10.0 {
                        late_sum += rm.trim();
                        late_n += 1;
                    }
                    if t_asio > secs - 20.0 {
                        tmin = tmin.min(rm.trim());
                        tmax = tmax.max(rm.trim());
                        trims_late.push(rm.trim());
                    }
                    if t_asio > 20.0 {
                        max_trim = max_trim.max(rm.trim().abs());
                    }
                }
                if keep_out {
                    out.extend_from_slice(&l);
                }
                t_asio += asio_dt;
            }
        }
        // Worst peak-to-peak within any 1 s window (ripple, as opposed to slow convergence).
        let win = (asio_rate / block as f64) as usize;
        for w in trims_late.chunks(win.max(2)) {
            let (lo, hi) = w.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |a, &v| (a.0.min(v), a.1.max(v)));
            tmax_ptp_1s = tmax_ptp_1s.max(hi - lo);
        }
        SimResult {
            min_fill,
            max_fill,
            underruns: rm.underruns() - underruns_at_open,
            rearms: rm.rearms(),
            discarded: rm.discarded_frames(),
            final_trim: rm.trim(),
            max_trim_after_settle: max_trim,
            mean_trim_late: late_sum / late_n.max(1) as f64,
            trim_ptp_late: tmax_ptp_1s,
            trim_range_late: tmax - tmin,
            out,
        }
    }

    #[test]
    fn drift_sim_60s_plus_minus_200ppm() {
        for &ppm in &[-200.0, 0.0, 200.0] {
            let r = simulate(48000.0, ppm, 48000.0, 512, 480, 2.0, 60.0, false);
            let target = target_fill_frames(2.0, 512, 48000, 48000, 480) as i64;
            println!(
                "ppm {ppm:+}: fill {}..{} (target {target}), underruns {}, start-up discard {}, mean trim last 10 s {:+.1} ppm",
                r.min_fill,
                r.max_fill,
                r.underruns,
                r.discarded,
                r.mean_trim_late * 1e6
            );
            assert_eq!(r.underruns, 0, "ppm {ppm}");
            assert_eq!(r.rearms, 0);
            assert!(r.discarded <= 480, "only the start-up trim may discard: {}", r.discarded);
            assert!(r.min_fill as i64 >= target - 512, "ppm {ppm}: min {}", r.min_fill);
            assert!(r.max_fill as i64 <= target + 512, "ppm {ppm}: max {}", r.max_fill);
            // Trim converges to the clock offset (capture faster -> consume faster).
            assert!((r.mean_trim_late * 1e6 - ppm).abs() < 40.0, "ppm {ppm}: mean trim {}", r.mean_trim_late * 1e6);
        }
    }

    /// The ratio must not be modulated by the capture/ASIO packet beat: peak-to-peak trim
    /// ripple after settling stays below a few ppm (a plain PI on the low-passed fill gave
    /// 50..500 ppm, i.e. -50 dB FM sidebands on a 1 kHz tone, measured on hardware).
    #[test]
    fn drift_trim_ripple_is_tiny() {
        // (cable rate, asio rate, block, capture packet)
        let cases = [
            (48000.0, 48000.0, 512usize, 480usize),
            (48000.0, 48000.0, 512, 960),
            (44100.0, 48000.0, 512, 441),
            (44100.0, 48000.0, 512, 132),
            (44100.0, 48000.0, 512, 882),
            (48000.0, 48000.0, 128, 480),
        ];
        for &(cr, ar, block, packet) in &cases {
            for &ppm in &[-150.0, 0.0, 120.0] {
                let r = simulate(cr, ppm, ar, block, packet, 2.0, 100.0, false);
                println!(
                    "{cr}->{ar} blk {block} pkt {packet} {ppm:+} ppm: trim ptp {:.2} ppm (range {:.1}), mean {:+.1}, fill {}..{}, underruns {}",
                    r.trim_ptp_late * 1e6,
                    r.trim_range_late * 1e6,
                    r.mean_trim_late * 1e6,
                    r.min_fill,
                    r.max_fill,
                    r.underruns
                );
                assert!(r.trim_ptp_late * 1e6 < 4.0, "{cr}->{ar} pkt {packet} {ppm} ppm: ripple {} ppm", r.trim_ptp_late * 1e6);
                assert_eq!(r.underruns, 0);
                assert!((r.mean_trim_late * 1e6 - ppm).abs() < 45.0, "{cr}->{ar} {ppm}: mean {}", r.mean_trim_late * 1e6);
            }
        }
    }

    #[test]
    #[ignore = "long-running convergence study"]
    fn drift_long_convergence() {
        for &ppm in &[-150.0, 120.0] {
            for &secs in &[200.0, 400.0] {
                let r = simulate(44100.0, ppm, 48000.0, 512, 441, 2.0, secs, false);
                println!("{ppm:+} ppm after {secs} s: mean last 10 s {:+.1}, ptp {:.2}, fill {}..{}", r.mean_trim_late * 1e6, r.trim_ptp_late * 1e6, r.min_fill, r.max_fill);
            }
        }
    }

    #[test]
    fn drift_sim_44k1_cable_to_48k_asio() {
        let r = simulate(44100.0, 150.0, 48000.0, 512, 441, 2.0, 40.0, false);
        println!(
            "44.1k->48k +150ppm: fill {}..{}, underruns {}, trim {:+.1} ppm",
            r.min_fill,
            r.max_fill,
            r.underruns,
            r.final_trim * 1e6
        );
        assert_eq!(r.underruns, 0);
        let target = target_fill_frames(2.0, 512, 44100, 48000, 441) as i64;
        assert!((r.max_fill as i64 - target).abs() <= 512 && (r.min_fill as i64 - target).abs() <= 512);
    }

    #[test]
    fn drift_sim_small_buffer_and_output_is_clean() {
        // 128-frame ASIO blocks, 10 ms capture period, -200 ppm; output stays a clean 1 kHz tone.
        let r = simulate(48000.0, -200.0, 48000.0, 128, 480, 2.0, 30.0, true);
        assert_eq!(r.underruns, 0);
        println!("128-frame blocks -200ppm: fill {}..{}, max |trim| after 20 s {:.1} ppm", r.min_fill, r.max_fill, r.max_trim_after_settle * 1e6);
        let skip = 48000 * 22;
        let y = &r.out[skip..];
        let f = freq_from_crossings(y, 48000.0);
        assert!((f - 1000.0).abs() < 0.3, "{f}");
        // No glitches: second difference of a 1 kHz/0.5 amplitude tone is bounded.
        let bound = 0.5 * (2.0 * PI * 1000.0 / 48000.0).powi(2) * 1.2 + 1e-4;
        let worst = y.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).abs()).fold(0.0f32, f32::max);
        assert!(f64::from(worst) < bound, "worst second difference {worst} > {bound}");
    }

    #[test]
    fn startup_gating_silence_then_fadein() {
        let target = 1000;
        let (mut p, mut c) = ring::channel(8192);
        let mut rm = RateMatcher::new(RateMatcherConfig::new(48000, 48000, target, ResamplerQuality::Linear));
        let (mut l, mut r) = (vec![9.0f32; 256], vec![9.0f32; 256]);
        let ones = vec![0.5f32; 600];
        p.push_frames(&ones, &ones);
        let st = rm.render(&mut c, &mut l, &mut r);
        assert!(st.gated && !st.underrun);
        assert!(l.iter().chain(r.iter()).all(|&x| x == 0.0));
        assert_eq!(c.fill(), 600, "gated render must not consume");
        p.push_frames(&ones, &ones);
        let st = rm.render(&mut c, &mut l, &mut r);
        assert!(!st.gated && !st.underrun);
        assert!(rm.is_running());
        // fade-in: starts near zero, reaches the full level
        assert!(l[0].abs() < 0.01);
        assert!((l[255] - 0.5).abs() < 0.01);
        for w in l.windows(2) {
            assert!(w[1] >= w[0] - 1e-6, "monotonic fade-in");
        }
    }

    #[test]
    fn underrun_fades_counts_and_recovers() {
        let target = 600;
        let (mut p, mut c) = ring::channel(8192);
        let mut cfg = RateMatcherConfig::new(48000, 48000, target, ResamplerQuality::Linear);
        cfg.rearm_after = 3;
        let mut rm = RateMatcher::new(cfg);
        let dc = vec![0.5f32; 2000];
        p.push_frames(&dc, &dc);
        let (mut l, mut r) = (vec![0.0f32; 512], vec![0.0f32; 512]);
        // opens and streams
        let st = rm.render(&mut c, &mut l, &mut r);
        assert!(!st.gated && !st.underrun);
        // ring now has ~1500 left; drain it
        loop {
            let st = rm.render(&mut c, &mut l, &mut r);
            if st.underrun {
                // hold-last then fade to zero within the block, no abrupt step
                assert!(st.produced < 512);
                let last_real = l[st.produced - 1];
                assert!((last_real - 0.5).abs() < 0.01);
                assert!(l[511].abs() < 0.01, "fades to silence");
                for w in l[st.produced..].windows(2) {
                    assert!(w[1] <= w[0] + 1e-6);
                }
                break;
            }
        }
        assert_eq!(rm.underruns(), 1);
        // more underruns -> gate re-arms after 3 consecutive
        rm.render(&mut c, &mut l, &mut r);
        rm.render(&mut c, &mut l, &mut r);
        assert_eq!(rm.rearms(), 1);
        assert!(!rm.is_running());
        assert!(l.iter().all(|&x| x == 0.0));
        // refill beyond the target -> restarts
        p.push_frames(&dc, &dc);
        let st = rm.render(&mut c, &mut l, &mut r);
        assert!(!st.gated && rm.is_running());
        assert!(l[0].abs() < 0.01 && (l[511] - 0.5).abs() < 0.01);
    }

    #[test]
    fn single_underrun_then_data_resumes_with_fade_in() {
        let (mut p, mut c) = ring::channel(8192);
        let mut rm = RateMatcher::new(RateMatcherConfig::new(48000, 48000, 300, ResamplerQuality::Linear));
        let dc = vec![0.5f32; 400];
        p.push_frames(&dc, &dc);
        let (mut l, mut r) = (vec![0.0f32; 256], vec![0.0f32; 256]);
        rm.render(&mut c, &mut l, &mut r); // opens, plays 256
        let st = rm.render(&mut c, &mut l, &mut r); // 144 left -> underrun
        assert!(st.underrun);
        p.push_frames(&dc, &dc);
        let st = rm.render(&mut c, &mut l, &mut r);
        assert!(!st.underrun && !st.gated);
        assert!(l[0].abs() < 0.05, "fade-in after underrun, got {}", l[0]);
        assert!((l[255] - 0.5).abs() < 0.02);
    }

    /// A mid-stream overrun trim drops input but must not create a step in the waveform.
    #[test]
    fn midstream_overrun_trim_is_crossfaded_without_jump() {
        let (mut p, mut c) = ring::channel(65536);
        let mut rm = RateMatcher::new(RateMatcherConfig::new(48000, 48000, 1000, ResamplerQuality::Cubic));
        let tone = |i: usize| (0.5 * (2.0 * PI * 1000.0 * i as f64 / 48000.0).sin()) as f32;
        let mut idx = 0usize;
        let mut push = |p: &mut ring::Producer, n: usize| {
            let v: Vec<f32> = (idx..idx + n).map(tone).collect();
            idx += n;
            p.push_frames(&v, &v);
        };
        push(&mut p, 1100);
        let (mut l, mut r) = (vec![0.0f32; 256], vec![0.0f32; 256]);
        let mut out = Vec::new();
        for _ in 0..4 {
            rm.render(&mut c, &mut l, &mut r);
            out.extend_from_slice(&l);
            push(&mut p, 256);
        }
        push(&mut p, 5000); // burst: fill far above 2x target
        let mut total_discard = 0;
        for _ in 0..8 {
            let st = rm.render(&mut c, &mut l, &mut r);
            total_discard += st.discarded;
            out.extend_from_slice(&l);
            push(&mut p, 256);
        }
        assert!(total_discard > 3000, "discarded {total_discard}");
        let max_step = 0.5 * 2.0 * (PI * 1000.0 / 48000.0).sin();
        let worst = out[600..].windows(2).map(|w| f64::from((w[1] - w[0]).abs())).fold(0.0, f64::max);
        assert!(worst < max_step * 1.1, "step {worst} vs tone max {max_step}");
    }

    #[test]
    fn overrun_trim_discards_to_target() {
        let (mut p, mut c) = ring::channel(8192);
        let mut rm = RateMatcher::new(RateMatcherConfig::new(48000, 48000, 1000, ResamplerQuality::Linear));
        let dc = vec![0.1f32; 1000];
        p.push_frames(&dc, &dc);
        let (mut l, mut r) = (vec![0.0f32; 256], vec![0.0f32; 256]);
        rm.render(&mut c, &mut l, &mut r); // opens
        let big = vec![0.1f32; 3000];
        p.push_frames(&big, &big);
        let before = c.fill();
        let st = rm.render(&mut c, &mut l, &mut r);
        assert!(st.discarded > 0, "fill {before} should have been trimmed");
        assert!(st.fill <= 1000 + 1);
        assert_eq!(rm.discarded_frames(), st.discarded as u64);
    }
}
