//! Biquad filters (RBJ cookbook) and the stereo EQ chain.

use crate::autoeq::EqProfile;
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// Maximum number of bands processed by [`StereoEq`].
pub const MAX_BANDS: usize = 32;

/// Soft limiter knee start, about -1 dBFS.
const LIMITER_KNEE: f32 = 0.891_250_9;
/// State values below this magnitude are flushed to zero (denormal guard).
const FLUSH: f64 = 1e-25;

/// Filter type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterKind {
    /// Peaking EQ.
    #[serde(rename = "PK")]
    Peaking,
    /// Low shelf (Q-based).
    #[serde(rename = "LSC")]
    LowShelf,
    /// High shelf (Q-based).
    #[serde(rename = "HSC")]
    HighShelf,
    /// Second-order low pass.
    #[serde(rename = "LP")]
    LowPass,
    /// Second-order high pass.
    #[serde(rename = "HP")]
    HighPass,
}

/// One parametric band.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FilterSpec {
    pub kind: FilterKind,
    pub fc_hz: f64,
    pub gain_db: f64,
    pub q: f64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Normalised biquad coefficients (a0 == 1).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Coefs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Coefs {
    const IDENTITY: Coefs = Coefs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    /// RBJ cookbook coefficients, computed in f64.
    fn from_spec(spec: &FilterSpec, fs: f64) -> Coefs {
        let fc = spec.fc_hz.clamp(1.0, fs * 0.499);
        let q = if spec.q > 1e-3 { spec.q } else { 1e-3 };
        let w0 = 2.0 * PI * fc / fs;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a = 10f64.powf(spec.gain_db / 40.0);
        let (b0, b1, b2, a0, a1, a2);
        match spec.kind {
            FilterKind::Peaking => {
                b0 = 1.0 + alpha * a;
                b1 = -2.0 * cos;
                b2 = 1.0 - alpha * a;
                a0 = 1.0 + alpha / a;
                a1 = -2.0 * cos;
                a2 = 1.0 - alpha / a;
            }
            FilterKind::LowShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                b0 = a * ((a + 1.0) - (a - 1.0) * cos + s);
                b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos);
                b2 = a * ((a + 1.0) - (a - 1.0) * cos - s);
                a0 = (a + 1.0) + (a - 1.0) * cos + s;
                a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos);
                a2 = (a + 1.0) + (a - 1.0) * cos - s;
            }
            FilterKind::HighShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                b0 = a * ((a + 1.0) + (a - 1.0) * cos + s);
                b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos);
                b2 = a * ((a + 1.0) + (a - 1.0) * cos - s);
                a0 = (a + 1.0) - (a - 1.0) * cos + s;
                a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos);
                a2 = (a + 1.0) - (a - 1.0) * cos - s;
            }
            FilterKind::LowPass => {
                b0 = (1.0 - cos) / 2.0;
                b1 = 1.0 - cos;
                b2 = b0;
                a0 = 1.0 + alpha;
                a1 = -2.0 * cos;
                a2 = 1.0 - alpha;
            }
            FilterKind::HighPass => {
                b0 = (1.0 + cos) / 2.0;
                b1 = -(1.0 + cos);
                b2 = b0;
                a0 = 1.0 + alpha;
                a1 = -2.0 * cos;
                a2 = 1.0 - alpha;
            }
        }
        Coefs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    /// Magnitude response in dB at `freq` Hz.
    fn magnitude_db(&self, freq: f64, fs: f64) -> f64 {
        let w = 2.0 * PI * freq / fs;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let nr = self.b0 + self.b1 * c1 + self.b2 * c2;
        let ni = -(self.b1 * s1 + self.b2 * s2);
        let dr = 1.0 + self.a1 * c1 + self.a2 * c2;
        let di = -(self.a1 * s1 + self.a2 * s2);
        let num = nr * nr + ni * ni;
        let den = dr * dr + di * di;
        10.0 * (num / den.max(1e-300)).max(1e-300).log10()
    }
}

/// Biquad section: transposed direct form II, f64 state, f32 I/O.
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    c: Coefs,
    z1: f64,
    z2: f64,
}

impl Default for Biquad {
    fn default() -> Self {
        Self::identity()
    }
}

impl Biquad {
    /// Pass-through section.
    pub const fn identity() -> Self {
        Biquad { c: Coefs::IDENTITY, z1: 0.0, z2: 0.0 }
    }

    /// Build a section from a spec at sample rate `fs`.
    pub fn from_spec(spec: &FilterSpec, fs: f64) -> Self {
        Biquad { c: Coefs::from_spec(spec, fs), z1: 0.0, z2: 0.0 }
    }

    /// Replace coefficients, keeping state (click-free retune).
    pub fn set_spec(&mut self, spec: &FilterSpec, fs: f64) {
        self.c = Coefs::from_spec(spec, fs);
    }

    /// Clear the delay line.
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    /// Magnitude response in dB at `freq` Hz.
    pub fn magnitude_db(&self, freq: f64, fs: f64) -> f64 {
        self.c.magnitude_db(freq, fs)
    }

    /// Process one sample.
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let x = f64::from(x);
        let c = &self.c;
        let y = c.b0 * x + self.z1;
        let mut z1 = c.b1 * x - c.a1 * y + self.z2;
        let mut z2 = c.b2 * x - c.a2 * y;
        if z1.abs() < FLUSH {
            z1 = 0.0;
        }
        if z2.abs() < FLUSH {
            z2 = 0.0;
        }
        self.z1 = z1;
        self.z2 = z2;
        y as f32
    }

    /// Process a block in place.
    #[inline]
    pub fn process_block(&mut self, buf: &mut [f32]) {
        for s in buf {
            *s = self.process(*s);
        }
    }
}

/// Smooth soft clip: identity below the knee, `tanh` roll-off above, bounded by 1.0.
#[inline]
fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= LIMITER_KNEE {
        x
    } else {
        let room = 1.0 - LIMITER_KNEE;
        let y = LIMITER_KNEE + room * ((a - LIMITER_KNEE) / room).tanh();
        y.copysign(x)
    }
}

/// Loudness of JBL Bass Mode relative to the dry signal, in dB (0 = matched).
const BASS_FX_TRIM_DB: f32 = -1.5;

/// "JBL Bass Mode": car-subwoofer style bass boost, saturation and bass-keyed pumping.
///
/// Low shelf boost, `tanh` clipping, then a gain dip keyed by the bass envelope so the
/// mids and highs duck with each bass hit (an amp running out of headroom). The wet amount
/// ramps over ~10 ms so toggling is click-free. Allocation-free per sample.
#[derive(Clone, Copy, Debug)]
pub struct BassFx {
    fs: f64,
    target: f32,
    mix: f32,
    intensity: f32,
    drive: f32,
    duck: f32,
    shelf_l: Biquad,
    shelf_r: Biquad,
    detector: Biquad,
    env: f32,
    attack: f32,
    release: f32,
    /// Low-quality mids/highs: quantiser step, sample-hold factor and a dulling low pass.
    quant_step: f32,
    hold: u32,
    hold_pos: u32,
    held: [f32; 2],
    lp_l: Biquad,
    lp_r: Biquad,
    /// Sub resonance (~50 Hz peak) ahead of the clipper, and clip asymmetry bias.
    res_l: Biquad,
    res_r: Biquad,
    bias: f32,
    /// Extra kick/bass-only saturation: bass-band splitter and its drive.
    kick_l: Biquad,
    kick_r: Biquad,
    kick_drive: f32,
    /// Output trim that keeps the loudness near 0 dB versus the dry signal.
    out_gain: f32,
}

impl BassFx {
    const SHELF_HZ: f64 = 70.0;
    const DETECT_HZ: f64 = 100.0;
    const RES_HZ: f64 = 50.0;

    /// Create an inactive effect at `fs` Hz.
    pub fn new(fs: f64) -> Self {
        let mut fx = BassFx {
            fs,
            target: 0.0,
            mix: 0.0,
            intensity: 0.0,
            drive: 1.0,
            duck: 0.0,
            shelf_l: Biquad::identity(),
            shelf_r: Biquad::identity(),
            detector: Biquad::identity(),
            env: 0.0,
            attack: 0.0,
            release: 0.0,
            quant_step: 0.0,
            hold: 1,
            hold_pos: 0,
            held: [0.0; 2],
            lp_l: Biquad::identity(),
            lp_r: Biquad::identity(),
            res_l: Biquad::identity(),
            res_r: Biquad::identity(),
            bias: 0.0,
            kick_l: Biquad::identity(),
            kick_r: Biquad::identity(),
            kick_drive: 1.0,
            out_gain: 1.0,
        };
        fx.configure(false, 0);
        fx
    }

    /// Enable or disable and set intensity (0-100). Control path (uses `powf`).
    pub fn set(&mut self, enabled: bool, intensity_pct: u8) {
        self.configure(enabled, intensity_pct);
    }

    /// Change the sample rate, keeping state. Invalid rates are ignored.
    pub fn set_sample_rate(&mut self, fs: f64) {
        if fs.is_finite() && fs > 0.0 {
            self.fs = fs;
            self.configure(self.target > 0.0, (self.intensity * 100.0).round() as u8);
        }
    }

    fn configure(&mut self, enabled: bool, intensity_pct: u8) {
        let i = f32::from(intensity_pct.min(100)) / 100.0;
        self.target = if enabled { 1.0 } else { 0.0 };
        self.intensity = i;
        self.drive = 1.0 + 4.0 * i;
        self.duck = 2.0 * i;
        // Up to ~7-bit steps and 4x sample-hold, then a low pass sliding 12 kHz -> 4 kHz.
        self.quant_step = 2.0 / 2f32.powf(16.0 - 6.0 * i);
        self.hold = 1 + (2.0 * i).round() as u32;
        let dull = FilterSpec {
            kind: FilterKind::LowPass,
            fc_hz: (14000.0 - 6500.0 * f64::from(i)).min(self.fs * 0.45),
            gain_db: 0.0,
            q: 0.707,
            enabled: true,
        };
        self.lp_l.set_spec(&dull, self.fs);
        self.lp_r.set_spec(&dull, self.fs);
        let shelf = FilterSpec {
            kind: FilterKind::LowShelf,
            fc_hz: Self::SHELF_HZ,
            gain_db: 12.0 * f64::from(i),
            q: 0.7,
            enabled: true,
        };
        self.shelf_l.set_spec(&shelf, self.fs);
        self.shelf_r.set_spec(&shelf, self.fs);
        let res = FilterSpec {
            kind: FilterKind::Peaking,
            fc_hz: Self::RES_HZ,
            gain_db: 9.0 * f64::from(i),
            q: 3.0,
            enabled: true,
        };
        self.res_l.set_spec(&res, self.fs);
        self.res_r.set_spec(&res, self.fs);
        self.bias = 0.35 * i;
        // Measured loudness of the effect (dB vs dry, music-like test signal) at 0/30/60/80/100 %;
        // interpolated and cancelled so the mode stays around 0 dB. See `bass_fx_level_report`.
        const LEVEL_DB: [(f32, f32); 5] = [(0.0, 0.0), (0.3, 0.85), (0.6, 2.2), (0.8, 1.92), (1.0, 1.3)];
        let mut meas = LEVEL_DB[4].1;
        for w in LEVEL_DB.windows(2) {
            if i <= w[1].0 {
                meas = w[0].1 + (w[1].1 - w[0].1) * (i - w[0].0) / (w[1].0 - w[0].0);
                break;
            }
        }
        self.out_gain = 0.45 * 10f32.powf((BASS_FX_TRIM_DB - meas) / 20.0);
        self.kick_drive = 1.0 + 1.2 * i;
        let kick = FilterSpec { kind: FilterKind::LowPass, fc_hz: 130.0, gain_db: 0.0, q: 0.707, enabled: true };
        self.kick_l.set_spec(&kick, self.fs);
        self.kick_r.set_spec(&kick, self.fs);
        let lp = FilterSpec { kind: FilterKind::LowPass, fc_hz: Self::DETECT_HZ, gain_db: 0.0, q: 0.707, enabled: true };
        self.detector.set_spec(&lp, self.fs);
        self.attack = (1.0 - (-1.0 / (0.005 * self.fs)).exp()) as f32;
        self.release = (1.0 - (-1.0 / (0.120 * self.fs)).exp()) as f32;
    }

    /// True when the effect has no audible contribution (off and fully ramped out).
    pub fn is_idle(&self) -> bool {
        self.target == 0.0 && self.mix == 0.0
    }

    /// Process a stereo block in place.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        if self.is_idle() {
            return;
        }
        let step = (1.0 / (BYPASS_RAMP_SECS * self.fs).max(1.0)) as f32;
        let norm = 1.0 / self.drive.tanh().max(1e-3);
        for (xl, xr) in l.iter_mut().zip(r.iter_mut()) {
            if self.mix < self.target {
                self.mix = (self.mix + step).min(self.target);
            } else if self.mix > self.target {
                self.mix = (self.mix - step).max(self.target);
            }
            let sl = self.res_l.process(self.shelf_l.process(*xl));
            let sr = self.res_r.process(self.shelf_r.process(*xr));
            // Kick/bass band gets its own heavy saturation before the full-band clipper.
            let (kl, kr) = (self.kick_l.process(sl), self.kick_r.process(sr));
            let kn = 0.8 / self.kick_drive.tanh().max(1e-3);
            let sl = sl - kl + (self.kick_drive * kl).tanh() * kn;
            let sr = sr - kr + (self.kick_drive * kr).tanh() * kn;
            let bass = self.detector.process(0.5 * (sl + sr)).abs();
            let coef = if bass > self.env { self.attack } else { self.release };
            self.env += (bass - self.env) * coef;
            let g = 1.0 / (1.0 + self.duck * self.env.min(2.0));
            // Asymmetric tanh (even + odd harmonics) followed by a hard clip (square-ish, odd-rich).
            let b = self.bias;
            let ob = b.tanh();
            let cl = (((self.drive * sl + b).tanh() - ob) * norm * 1.1).clamp(-1.0, 1.0);
            let cr = (((self.drive * sr + b).tanh() - ob) * norm * 1.1).clamp(-1.0, 1.0);
            let mut yl = cl * g * self.out_gain;
            let mut yr = cr * g * self.out_gain;
            // Low-quality mids/highs: sample-hold (aliasing), bit crush, then dull the top end.
            if self.hold_pos == 0 {
                self.held = [yl, yr];
            }
            self.hold_pos = (self.hold_pos + 1) % self.hold;
            let q = self.quant_step;
            yl = (self.held[0] / q).round() * q;
            yr = (self.held[1] / q).round() * q;
            yl = self.lp_l.process(yl);
            yr = self.lp_r.process(yr);
            *xl += (yl - *xl) * self.mix;
            *xr += (yr - *xr) * self.mix;
        }
    }

    /// Clear filter and envelope state.
    pub fn reset(&mut self) {
        self.shelf_l.reset();
        self.shelf_r.reset();
        self.detector.reset();
        self.kick_l.reset();
        self.kick_r.reset();
        self.res_l.reset();
        self.res_r.reset();
        self.lp_l.reset();
        self.lp_r.reset();
        self.held = [0.0; 2];
        self.env = 0.0;
    }
}

/// Maximum frames processed per chunk by [`StereoEq::process_stereo_with_bypass_ramp`].
pub const BYPASS_SCRATCH_FRAMES: usize = 16384;
/// Bypass crossfade duration in seconds.
const BYPASS_RAMP_SECS: f64 = 0.010;

/// Stereo EQ: preamp, up to [`MAX_BANDS`] biquads per channel, optional soft limiter.
///
/// Non-finite input samples are replaced by 0 and counted ([`StereoEq::non_finite_count`]).
#[derive(Clone, Debug)]
pub struct StereoEq {
    fs: f64,
    preamp_db: f64,
    extra_db: f64,
    preamp: f32,
    limiter: bool,
    active: usize,
    specs: [FilterSpec; MAX_BANDS],
    left: [Biquad; MAX_BANDS],
    right: [Biquad; MAX_BANDS],
    non_finite: u64,
    bass_fx: BassFx,
    /// Wet amount of the bypass crossfade (1 = fully processed).
    wet: f32,
    dry_l: Box<[f32]>,
    dry_r: Box<[f32]>,
}

impl StereoEq {
    /// Create a flat EQ at `sample_rate` Hz.
    pub fn new(sample_rate: f64) -> Self {
        let blank = FilterSpec { kind: FilterKind::Peaking, fc_hz: 1000.0, gain_db: 0.0, q: 1.0, enabled: true };
        StereoEq {
            fs: sample_rate,
            preamp_db: 0.0,
            extra_db: 0.0,
            preamp: 1.0,
            limiter: false,
            active: 0,
            specs: [blank; MAX_BANDS],
            left: [Biquad::identity(); MAX_BANDS],
            right: [Biquad::identity(); MAX_BANDS],
            non_finite: 0,
            bass_fx: BassFx::new(sample_rate),
            wet: 1.0,
            dry_l: vec![0.0; BYPASS_SCRATCH_FRAMES].into_boxed_slice(),
            dry_r: vec![0.0; BYPASS_SCRATCH_FRAMES].into_boxed_slice(),
        }
    }

    /// Copies the complete filter configuration and state of `other` (profile, gains, limiter,
    /// biquad coefficients and delay lines) without allocating. The bypass crossfade state and
    /// scratch buffers are left alone.
    pub fn copy_state_from(&mut self, other: &StereoEq) {
        self.fs = other.fs;
        self.preamp_db = other.preamp_db;
        self.extra_db = other.extra_db;
        self.preamp = other.preamp;
        self.limiter = other.limiter;
        self.active = other.active;
        self.specs = other.specs;
        self.left = other.left;
        self.right = other.right;
        self.bass_fx = other.bass_fx;
    }

    /// Enable or disable "JBL Bass Mode" and set its intensity (0-100).
    pub fn set_bass_fx(&mut self, enabled: bool, intensity_pct: u8) {
        self.bass_fx.set(enabled, intensity_pct);
    }

    /// Sample rate in Hz.
    pub fn sample_rate(&self) -> f64 {
        self.fs
    }

    /// Number of active (enabled) bands.
    pub fn active_bands(&self) -> usize {
        self.active
    }

    /// Enable or disable the soft limiter.
    pub fn set_limiter(&mut self, on: bool) {
        self.limiter = on;
    }

    /// Number of non-finite samples (NaN/inf) replaced by 0 since the last reset
    /// (saturates at `u32::MAX`).
    pub fn non_finite_count(&self) -> u32 {
        self.non_finite.min(u64::from(u32::MAX)) as u32
    }

    /// Reset the non-finite sample counter.
    pub fn reset_non_finite_count(&mut self) {
        self.non_finite = 0;
    }

    /// Extra gain in dB (<= 0 typically, e.g. [`auto_headroom_db`]) applied on top of the
    /// profile preamp. Uses `powf`; call from the control path, not per sample.
    pub fn set_extra_gain_db(&mut self, db: f64) {
        self.extra_db = if db.is_finite() { db } else { 0.0 };
        self.preamp = 10f64.powf((self.preamp_db + self.extra_db) / 20.0) as f32;
    }

    /// Load a profile (recomputes coefficients; not for the audio thread).
    ///
    /// Disabled filters are skipped; bands beyond [`MAX_BANDS`] are ignored.
    pub fn set_profile(&mut self, profile: &EqProfile) {
        self.preamp_db = profile.preamp_db;
        self.preamp = 10f64.powf((self.preamp_db + self.extra_db) / 20.0) as f32;
        let mut n = 0;
        for spec in profile.filters.iter().filter(|f| f.enabled).take(MAX_BANDS) {
            self.specs[n] = *spec;
            self.left[n].set_spec(spec, self.fs);
            self.right[n].set_spec(spec, self.fs);
            n += 1;
        }
        for i in n..MAX_BANDS {
            self.left[i] = Biquad::identity();
            self.right[i] = Biquad::identity();
        }
        self.active = n;
    }

    /// Change the sample rate, recomputing coefficients for the current profile while
    /// preserving filter state. Invalid rates (<= 0 / non-finite) are ignored.
    pub fn set_sample_rate(&mut self, fs: f64) {
        if !(fs.is_finite() && fs > 0.0) {
            return;
        }
        self.fs = fs;
        for i in 0..self.active {
            self.left[i].set_spec(&self.specs[i], fs);
            self.right[i].set_spec(&self.specs[i], fs);
        }
        self.bass_fx.set_sample_rate(fs);
    }

    /// Clear all filter state.
    pub fn reset(&mut self) {
        self.left.iter_mut().chain(self.right.iter_mut()).for_each(Biquad::reset);
        self.bass_fx.reset();
    }

    /// Process a stereo block in place (allocation-free).
    ///
    /// Only the first `min(l.len(), r.len())` samples are processed.
    /// NaN/inf input samples become 0 and are counted.
    pub fn process_stereo(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        let (l, r) = (&mut l[..n], &mut r[..n]);
        let mut bad = 0u64;
        for s in l.iter_mut().chain(r.iter_mut()) {
            if !s.is_finite() {
                *s = 0.0;
                bad += 1;
            }
        }
        self.non_finite += bad;
        let g = self.preamp;
        if g != 1.0 {
            l.iter_mut().for_each(|s| *s *= g);
            r.iter_mut().for_each(|s| *s *= g);
        }
        for b in &mut self.left[..self.active] {
            b.process_block(l);
        }
        for b in &mut self.right[..self.active] {
            b.process_block(r);
        }
        self.bass_fx.process(l, r);
        if self.limiter {
            l.iter_mut().for_each(|s| *s = soft_limit(*s));
            r.iter_mut().for_each(|s| *s = soft_limit(*s));
        }
    }

    /// Like [`process_stereo`](Self::process_stereo) but with click-free bypass: the
    /// filters keep running while `bypass` is true and the output crossfades between
    /// wet and dry over ~10 ms (linear, per sample). Allocation-free; blocks longer than
    /// [`BYPASS_SCRATCH_FRAMES`] are processed in chunks.
    pub fn process_stereo_with_bypass_ramp(&mut self, l: &mut [f32], r: &mut [f32], bypass: bool) {
        let n = l.len().min(r.len());
        let target = if bypass { 0.0f32 } else { 1.0 };
        let step = (1.0 / (BYPASS_RAMP_SECS * self.fs).max(1.0)) as f32;
        let mut pos = 0;
        while pos < n {
            let end = (pos + BYPASS_SCRATCH_FRAMES).min(n);
            let (cl, cr) = (&mut l[pos..end], &mut r[pos..end]);
            pos = end;
            if self.wet == target && !bypass {
                self.process_stereo(cl, cr);
                continue;
            }
            let len = cl.len();
            for i in 0..len {
                // Dry path gets the same non-finite guard as the wet path.
                self.dry_l[i] = if cl[i].is_finite() { cl[i] } else { 0.0 };
                self.dry_r[i] = if cr[i].is_finite() { cr[i] } else { 0.0 };
            }
            self.process_stereo(cl, cr);
            let mut w = self.wet;
            for i in 0..len {
                if w < target {
                    w = (w + step).min(target);
                } else if w > target {
                    w = (w - step).max(target);
                }
                cl[i] = self.dry_l[i] + (cl[i] - self.dry_l[i]) * w;
                cr[i] = self.dry_r[i] + (cr[i] - self.dry_r[i]) * w;
            }
            self.wet = w;
        }
    }
}

/// Extra attenuation in dB (always `<= 0`) needed so that the profile preamp plus its
/// maximum combined magnitude over ~2048 log-spaced points from 10 Hz to
/// `0.99 * fs / 2` does not exceed 0 dB. Not for the audio thread (allocates).
///
/// Apply with [`StereoEq::set_extra_gain_db`].
pub fn auto_headroom_db(profile: &EqProfile, fs: f64) -> f64 {
    const POINTS: usize = 2048;
    let hi = 0.99 * fs / 2.0;
    if !(fs.is_finite() && hi > 10.0) {
        return 0.0;
    }
    let ratio = (hi / 10.0).powf(1.0 / (POINTS - 1) as f64);
    let freqs: Vec<f64> = (0..POINTS).map(|i| 10.0 * ratio.powi(i as i32)).collect();
    let peak = magnitude_response_db(profile, fs, &freqs)
        .into_iter()
        .filter(|v| v.is_finite())
        .fold(f64::NEG_INFINITY, f64::max);
    if peak > 0.0 {
        -peak
    } else {
        0.0
    }
}

/// Per-sample linear gain ramp (allocation-free).
#[derive(Clone, Copy, Debug)]
pub struct GainRamp {
    gain: f32,
    target: f32,
    step: f32,
    remaining: u32,
}

impl GainRamp {
    /// Start at a fixed gain (no ramp in progress).
    pub const fn new(gain: f32) -> Self {
        GainRamp { gain, target: gain, step: 0.0, remaining: 0 }
    }

    /// Current gain.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// True while a ramp is in progress.
    pub fn is_ramping(&self) -> bool {
        self.remaining > 0
    }

    /// Ramp linearly from the current gain to `gain` over `ramp_samples` samples
    /// (0 = jump immediately). Non-finite targets are ignored.
    pub fn set_target(&mut self, gain: f32, ramp_samples: u32) {
        if !gain.is_finite() {
            return;
        }
        self.target = gain;
        if ramp_samples == 0 {
            self.gain = gain;
            self.step = 0.0;
            self.remaining = 0;
        } else {
            self.step = (gain - self.gain) / ramp_samples as f32;
            self.remaining = ramp_samples;
        }
    }

    /// Next gain value (advances the ramp by one sample).
    #[inline]
    pub fn next_gain(&mut self) -> f32 {
        if self.remaining > 0 {
            self.remaining -= 1;
            self.gain = if self.remaining == 0 { self.target } else { self.gain + self.step };
        }
        self.gain
    }

    /// Multiply `buf` by the ramping gain, one gain value per sample.
    pub fn apply(&mut self, buf: &mut [f32]) {
        if self.remaining == 0 {
            if self.gain != 1.0 {
                let g = self.gain;
                buf.iter_mut().for_each(|s| *s *= g);
            }
            return;
        }
        for s in buf {
            *s *= self.next_gain();
        }
    }

    /// Apply the same ramp to two channels (one gain value per frame).
    pub fn apply_stereo(&mut self, l: &mut [f32], r: &mut [f32]) {
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let g = self.next_gain();
            *a *= g;
            *b *= g;
        }
    }
}

/// Health of the audio path as judged by [`FaultDetector`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum FaultState {
    /// Healthy.
    #[default]
    Ok = 0,
    /// Degraded; keep playing.
    Warn = 1,
    /// Output should be faded to silence.
    Muted = 2,
}

impl FaultState {
    /// Numeric value as stored in `StatusBlock::fault_state`.
    pub fn as_u32(self) -> u32 {
        self as u32
    }
}

/// Thresholds for [`FaultDetector`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FaultConfig {
    /// Peak (linear) at or above which a block is `Warn`.
    pub peak_warn: f32,
    /// Peak (linear) at or above which a block is `Muted` (~+30 dBFS).
    pub peak_mute: f32,
    /// Non-finite sample count per block that triggers `Warn`.
    pub non_finite_warn: u32,
    /// Non-finite sample count per block that triggers `Muted`.
    pub non_finite_mute: u32,
    /// Underrun ratio (0..1) for `Warn`.
    pub underrun_warn: f32,
    /// Underrun ratio for `Muted`.
    pub underrun_mute: f32,
    /// Capture gap ratio for `Warn`.
    pub gap_warn: f32,
    /// Capture gap ratio for `Muted`.
    pub gap_mute: f32,
    /// A block is "clean" only if every metric is below `warn * hysteresis`.
    pub hysteresis: f32,
    /// Consecutive clean blocks to go `Muted` -> `Warn`.
    pub recover_blocks: u32,
    /// Consecutive clean blocks to go `Warn` -> `Ok`.
    pub clear_blocks: u32,
}

impl Default for FaultConfig {
    fn default() -> Self {
        FaultConfig {
            peak_warn: 4.0,
            peak_mute: 32.0,
            non_finite_warn: 1,
            non_finite_mute: 16,
            underrun_warn: 0.02,
            underrun_mute: 0.25,
            gap_warn: 0.02,
            gap_mute: 0.25,
            hysteresis: 0.5,
            recover_blocks: 100,
            clear_blocks: 50,
        }
    }
}

/// Fault state machine with hysteresis and auto-recovery. Pure, allocation-free.
///
/// Escalates immediately; de-escalates one level at a time after a run of clean blocks
/// (`Muted` -> `Warn` -> `Ok`). Blocks between the clean and warn thresholds hold the
/// state and restart the clean counter.
#[derive(Clone, Copy, Debug)]
pub struct FaultDetector {
    cfg: FaultConfig,
    state: FaultState,
    clean: u32,
}

impl Default for FaultDetector {
    fn default() -> Self {
        Self::new(FaultConfig::default())
    }
}

impl FaultDetector {
    /// Create a detector in the `Ok` state.
    pub const fn new(cfg: FaultConfig) -> Self {
        FaultDetector { cfg, state: FaultState::Ok, clean: 0 }
    }

    /// Current state.
    pub fn state(&self) -> FaultState {
        self.state
    }

    /// Back to `Ok`.
    pub fn reset(&mut self) {
        self.state = FaultState::Ok;
        self.clean = 0;
    }

    /// Feed one block's metrics and return the new state. A non-finite `peak` or ratio
    /// counts as a mute-level fault.
    pub fn update(&mut self, peak: f32, non_finite: u32, underrun_ratio: f32, gap_ratio: f32) -> FaultState {
        let c = &self.cfg;
        let bad = |v: f32, lim: f32| !v.is_finite() || v >= lim;
        let mute = bad(peak, c.peak_mute)
            || non_finite >= c.non_finite_mute
            || bad(underrun_ratio, c.underrun_mute)
            || bad(gap_ratio, c.gap_mute);
        let warn = bad(peak, c.peak_warn)
            || non_finite >= c.non_finite_warn
            || bad(underrun_ratio, c.underrun_warn)
            || bad(gap_ratio, c.gap_warn);
        let h = c.hysteresis;
        let clean = peak < c.peak_warn * h
            && non_finite == 0
            && underrun_ratio < c.underrun_warn * h
            && gap_ratio < c.gap_warn * h;
        if mute {
            self.state = FaultState::Muted;
            self.clean = 0;
        } else if warn {
            if self.state == FaultState::Ok {
                self.state = FaultState::Warn;
            }
            self.clean = 0;
        } else if clean {
            self.clean = self.clean.saturating_add(1);
            match self.state {
                FaultState::Muted if self.clean >= c.recover_blocks => {
                    self.state = FaultState::Warn;
                    self.clean = 0;
                }
                FaultState::Warn if self.clean >= c.clear_blocks => {
                    self.state = FaultState::Ok;
                    self.clean = 0;
                }
                _ => {}
            }
        } else {
            self.clean = 0;
        }
        self.state
    }
}


/// Combined magnitude response (dB, including preamp) of a profile at each frequency.
pub fn magnitude_response_db(profile: &EqProfile, sample_rate: f64, freqs: &[f64]) -> Vec<f64> {
    let coefs: Vec<Coefs> = profile
        .filters
        .iter()
        .filter(|f| f.enabled)
        .take(MAX_BANDS)
        .map(|f| Coefs::from_spec(f, sample_rate))
        .collect();
    freqs
        .iter()
        .map(|&f| {
            profile.preamp_db + coefs.iter().map(|c| c.magnitude_db(f, sample_rate)).sum::<f64>()
        })
        .collect()
}

#[cfg(test)]
mod bass_fx_tests {
    use super::*;

    fn two_tone(n: usize, fs: f64) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                (0.4 * (2.0 * PI * 60.0 * t).sin() + 0.2 * (2.0 * PI * 1000.0 * t).sin()) as f32
            })
            .collect()
    }

    #[test]
    fn bass_fx_level_report() {
        let fs = 48000.0;
        let n = 96000;
        // Music-like: 50 Hz kick-ish tone + mid tones + noise-ish highs, ~-12 dBFS RMS.
        let x: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let env = (-((t * 2.0) % 1.0) * 6.0).exp();
                (0.35 * env * (2.0 * PI * 55.0 * t).sin()
                    + 0.12 * (2.0 * PI * 440.0 * t).sin()
                    + 0.08 * (2.0 * PI * 2500.0 * t).sin()
                    + 0.04 * (2.0 * PI * 7000.0 * t).sin()) as f32
            })
            .collect();
        let rms = |v: &[f32]| (v.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        for pct in [30u8, 60, 80, 100] {
            let (mut l, mut r) = (x.clone(), x.clone());
            let mut eq = StereoEq::new(fs);
            eq.set_limiter(true);
            eq.set_bass_fx(true, pct);
            eq.process_stereo(&mut l, &mut r);
            let db = 20.0 * (rms(&l[n / 2..]) / rms(&x[n / 2..])).log10();
            println!("LEVEL pct={pct} gain_db={db:.2}");
            assert!((db - f64::from(BASS_FX_TRIM_DB)).abs() < 1.0, "level drifted at {pct}%: {db:.2} dB");
        }
    }

    #[test]
    fn bass_fx_off_is_bit_identical() {
        let x = two_tone(48000, 48000.0);
        let (mut a_l, mut a_r) = (x.clone(), x.clone());
        let (mut b_l, mut b_r) = (x.clone(), x);
        let mut plain = StereoEq::new(48000.0);
        let mut fx = StereoEq::new(48000.0);
        fx.set_bass_fx(false, 80);
        plain.process_stereo(&mut a_l, &mut a_r);
        fx.process_stereo(&mut b_l, &mut b_r);
        assert_eq!(a_l, b_l);
        assert_eq!(a_r, b_r);
    }

    #[test]
    fn bass_fx_on_changes_signal_and_stays_finite_and_bounded() {
        let x = two_tone(48000, 48000.0);
        let (mut l, mut r) = (x.clone(), x.clone());
        let mut eq = StereoEq::new(48000.0);
        eq.set_limiter(true);
        eq.set_bass_fx(true, 100);
        eq.process_stereo(&mut l, &mut r);
        assert!(l.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        let diff: f32 = l.iter().zip(&x).map(|(a, b)| (a - b).abs()).sum();
        assert!(diff > 100.0, "effect should audibly alter the signal, diff={diff}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: FilterKind, fc: f64, gain: f64, q: f64) -> FilterSpec {
        FilterSpec { kind, fc_hz: fc, gain_db: gain, q, enabled: true }
    }

    fn db_at(s: &FilterSpec, fs: f64, f: f64) -> f64 {
        Biquad::from_spec(s, fs).magnitude_db(f, fs)
    }

    #[test]
    fn peaking_gain_at_fc() {
        for &(fc, g, q) in &[(31.0, 1.5, 1.41), (1000.0, 6.0, 1.0), (8000.0, -9.5, 3.0), (100.0, 12.0, 0.5)] {
            for &fs in &[44100.0, 48000.0, 96000.0] {
                let d = db_at(&spec(FilterKind::Peaking, fc, g, q), fs, fc);
                assert!((d - g).abs() < 0.01, "fc={fc} g={g} fs={fs} got {d}");
            }
        }
    }

    #[test]
    fn peaking_far_from_fc_is_flat() {
        let s = spec(FilterKind::Peaking, 1000.0, 6.0, 4.0);
        assert!(db_at(&s, 48000.0, 20.0).abs() < 0.05);
        assert!(db_at(&s, 48000.0, 20000.0).abs() < 0.5);
    }

    #[test]
    fn shelf_asymptotes() {
        let ls = spec(FilterKind::LowShelf, 1000.0, 6.0, 0.707);
        assert!((db_at(&ls, 48000.0, 1.0) - 6.0).abs() < 0.01);
        assert!(db_at(&ls, 48000.0, 23990.0).abs() < 0.05);
        // Shelf midpoint is half the gain.
        assert!((db_at(&ls, 48000.0, 1000.0) - 3.0).abs() < 0.01);
        let hs = spec(FilterKind::HighShelf, 4000.0, -5.0, 0.707);
        assert!(db_at(&hs, 48000.0, 1.0).abs() < 0.01);
        assert!((db_at(&hs, 48000.0, 23999.0) + 5.0).abs() < 0.05);
        assert!((db_at(&hs, 48000.0, 4000.0) + 2.5).abs() < 0.01);
    }

    #[test]
    fn lowpass_highpass() {
        let lp = spec(FilterKind::LowPass, 1000.0, 0.0, std::f64::consts::FRAC_1_SQRT_2);
        assert!(db_at(&lp, 48000.0, 10.0).abs() < 0.01);
        assert!((db_at(&lp, 48000.0, 1000.0) + 3.0103).abs() < 0.01);
        let hp = spec(FilterKind::HighPass, 1000.0, 0.0, std::f64::consts::FRAC_1_SQRT_2);
        assert!((db_at(&hp, 48000.0, 1000.0) + 3.0103).abs() < 0.01);
        assert!(db_at(&hp, 48000.0, 20000.0).abs() < 0.5);
    }

    #[test]
    fn zero_gain_is_identity() {
        let mut b = Biquad::from_spec(&spec(FilterKind::Peaking, 500.0, 0.0, 1.0), 48000.0);
        for i in 0..100 {
            let x = (i as f32 * 0.37).sin();
            assert!((b.process(x) - x).abs() < 1e-6);
        }
    }

    #[test]
    fn extreme_fc_is_clamped_and_finite() {
        let mut b = Biquad::from_spec(&spec(FilterKind::Peaking, 30000.0, 6.0, 1.0), 48000.0);
        for _ in 0..1000 {
            assert!(b.process(0.5).is_finite());
        }
    }

    fn test_profile() -> EqProfile {
        EqProfile {
            name: "t".into(),
            preamp_db: -3.0,
            filters: vec![
                spec(FilterKind::LowShelf, 100.0, 4.0, 0.7),
                spec(FilterKind::Peaking, 1000.0, 6.0, 1.0),
                spec(FilterKind::Peaking, 3000.0, -4.0, 2.0),
                spec(FilterKind::HighShelf, 8000.0, -3.0, 0.7),
                FilterSpec { enabled: false, ..spec(FilterKind::Peaking, 500.0, 12.0, 1.0) },
            ],
        }
    }

    #[test]
    fn stereo_eq_matches_magnitude_response() {
        let fs = 48000.0;
        let p = test_profile();
        let freqs = [50.0, 100.0, 300.0, 1000.0, 3000.0, 8000.0, 12000.0];
        let expect = magnitude_response_db(&p, fs, &freqs);
        for (f, e) in freqs.iter().zip(expect) {
            let mut eq = StereoEq::new(fs);
            eq.set_profile(&p);
            let n = 96000;
            let mut l: Vec<f32> =
                (0..n).map(|i| (0.1 * (2.0 * PI * f * i as f64 / fs).sin()) as f32).collect();
            let mut r = l.clone();
            eq.process_stereo(&mut l, &mut r);
            let rms = |x: &[f32]| {
                (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
            };
            let reference = 0.1 / 2f64.sqrt();
            let ml = 20.0 * (rms(&l[48000..]) / reference).log10();
            let mr = 20.0 * (rms(&r[48000..]) / reference).log10();
            assert!((ml - e).abs() < 0.05, "f={f}: measured {ml} expected {e}");
            assert!((mr - e).abs() < 0.05, "f={f}: measured {mr} expected {e}");
        }
    }

    #[test]
    fn disabled_filters_ignored_and_max_bands() {
        let p = test_profile();
        let mut eq = StereoEq::new(48000.0);
        eq.set_profile(&p);
        assert_eq!(eq.active_bands(), 4);
        let big = EqProfile {
            name: String::new(),
            preamp_db: 0.0,
            filters: vec![spec(FilterKind::Peaking, 1000.0, 1.0, 1.0); 40],
        };
        eq.set_profile(&big);
        assert_eq!(eq.active_bands(), MAX_BANDS);
        eq.set_profile(&EqProfile::default());
        assert_eq!(eq.active_bands(), 0);
    }

    #[test]
    fn limiter_bounds_output() {
        let mut eq = StereoEq::new(48000.0);
        eq.set_limiter(true);
        let mut l = vec![0.5, 0.8, 1.0, 2.0, -3.0, 100.0, -0.2];
        let mut r = l.clone();
        eq.process_stereo(&mut l, &mut r);
        assert_eq!(l[0], 0.5);
        assert_eq!(l[6], -0.2);
        for v in &l {
            assert!(v.abs() <= 1.0);
        }
        assert!(l[4] < 0.0 && l[3] > l[2]);
        assert!(soft_limit(LIMITER_KNEE + 1e-4) - LIMITER_KNEE < 2e-4);
    }

    #[test]
    fn handles_mismatched_lengths_and_denormals() {
        let mut eq = StereoEq::new(48000.0);
        eq.set_profile(&test_profile());
        let mut l = vec![1.0; 10];
        let mut r = vec![1.0; 4];
        eq.process_stereo(&mut l, &mut r);
        assert_eq!(l[9], 1.0);
        let mut z = vec![0.0f32; 20000];
        let mut z2 = z.clone();
        eq.process_stereo(&mut z, &mut z2);
        assert!(z.iter().all(|v| *v == 0.0 || v.abs() > 1e-30));
        assert!(z.last().unwrap().abs() < 1e-20);
    }

    #[test]
    fn non_finite_sanitized_and_counted() {
        let mut eq = StereoEq::new(48000.0);
        eq.set_profile(&test_profile());
        let mut l = vec![0.1, f32::NAN, f32::INFINITY, 0.1];
        let mut r = vec![f32::NEG_INFINITY, 0.1, 0.1, 0.1];
        eq.process_stereo(&mut l, &mut r);
        assert_eq!(eq.non_finite_count(), 3);
        assert!(l.iter().chain(r.iter()).all(|v| v.is_finite()));
        eq.reset_non_finite_count();
        assert_eq!(eq.non_finite_count(), 0);
    }

    #[test]
    fn auto_headroom_peak_profile() {
        let p = EqProfile {
            name: String::new(),
            preamp_db: 0.0,
            filters: vec![spec(FilterKind::Peaking, 1000.0, 8.0, 1.0)],
        };
        let h = auto_headroom_db(&p, 48000.0);
        assert!((h + 8.0).abs() < 0.1, "got {h}");
        // Preamp already compensates: no extra attenuation, never positive.
        let q = EqProfile { preamp_db: -8.0, ..p.clone() };
        assert!(auto_headroom_db(&q, 48000.0).abs() < 0.1);
        let cut = EqProfile { preamp_db: -3.0, filters: vec![], name: String::new() };
        assert_eq!(auto_headroom_db(&cut, 48000.0), 0.0);
        // Applying it keeps a sine at the peak below 0 dBFS.
        let mut eq = StereoEq::new(48000.0);
        eq.set_profile(&p);
        eq.set_extra_gain_db(h);
        let mut l: Vec<f32> =
            (0..48000).map(|i| (2.0 * PI * 1000.0 * i as f64 / 48000.0).sin() as f32).collect();
        let mut r = l.clone();
        eq.process_stereo(&mut l, &mut r);
        let peak = l[24000..].iter().fold(0f32, |a, v| a.max(v.abs()));
        assert!(peak < 1.01, "peak {peak}");
    }

    #[test]
    fn gain_ramp_monotonic() {
        let mut g = GainRamp::new(1.0);
        g.set_target(0.0, 100);
        let mut buf = vec![1.0f32; 150];
        g.apply(&mut buf);
        assert!(buf.windows(2).all(|w| w[1] <= w[0]));
        assert!(buf[0] < 1.0 && buf[0] > 0.9);
        assert_eq!(buf[99], 0.0);
        assert_eq!(buf[149], 0.0);
        assert!(!g.is_ramping());
        g.set_target(1.0, 50);
        let mut up = vec![1.0f32; 60];
        g.apply(&mut up);
        assert!(up.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(up[59], 1.0);
        g.set_target(0.5, 0);
        assert_eq!(g.gain(), 0.5);
    }

    #[test]
    fn bypass_ramp_is_click_free_and_reaches_dry() {
        let fs = 48000.0;
        let wet = 0.5 * 10f32.powf(-6.0 / 20.0);
        let mut eq = StereoEq::new(fs);
        eq.set_profile(&EqProfile {
            name: String::new(),
            preamp_db: -6.0,
            filters: vec![spec(FilterKind::Peaking, 1000.0, 0.0, 1.0)],
        });
        let n = 4800;
        let mut l = vec![0.5f32; n];
        let mut r = l.clone();
        eq.process_stereo_with_bypass_ramp(&mut l, &mut r, false);
        assert!((l[n - 1] - wet).abs() < 1e-4);
        let mut l = vec![0.5f32; n];
        let mut r = l.clone();
        eq.process_stereo_with_bypass_ramp(&mut l, &mut r, true);
        assert!(l.windows(2).all(|w| w[1] >= w[0] - 1e-6), "monotonic to dry");
        assert!(l.windows(2).all(|w| (w[1] - w[0]).abs() < 1e-3), "no step");
        assert!((l[0] - wet).abs() < 0.01);
        assert!((l[n - 1] - 0.5).abs() < 1e-6);
        // Larger than the scratch buffer works too.
        let big = BYPASS_SCRATCH_FRAMES * 2 + 10;
        let mut l = vec![0.5f32; big];
        let mut r = l.clone();
        eq.process_stereo_with_bypass_ramp(&mut l, &mut r, false);
        assert!((l[big - 1] - wet).abs() < 1e-4);
    }

    #[test]
    fn set_sample_rate_retunes_and_keeps_state() {
        let p = test_profile();
        let mut a = StereoEq::new(48000.0);
        a.set_profile(&p);
        let mut warm = vec![0.3f32; 200];
        let mut warm_r = warm.clone();
        a.process_stereo(&mut warm, &mut warm_r);
        let mut b = a.clone();
        b.set_sample_rate(96000.0);
        assert_eq!(b.sample_rate(), 96000.0);
        assert_eq!(b.active_bands(), 4);
        let mut fresh = StereoEq::new(96000.0);
        fresh.set_profile(&p);
        let f = 3000.0;
        let freqs = [f];
        let expect = magnitude_response_db(&p, 96000.0, &freqs)[0];
        let n = 96000;
        let mut l: Vec<f32> =
            (0..n).map(|i| (0.1 * (2.0 * PI * f * i as f64 / 96000.0).sin()) as f32).collect();
        let mut r = l.clone();
        b.process_stereo(&mut l, &mut r);
        let rms = (l[48000..].iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / 48000.0).sqrt();
        let m = 20.0 * (rms / (0.1 / 2f64.sqrt())).log10();
        assert!((m - expect).abs() < 0.05, "{m} vs {expect}");
        // State preserved (not equal to a freshly reset filter right after the switch).
        let mut c = a.clone();
        c.set_sample_rate(48000.0);
        let (mut x, mut y) = (vec![0.0f32; 4], vec![0.0f32; 4]);
        let (mut x2, mut y2) = (vec![0.0f32; 4], vec![0.0f32; 4]);
        c.process_stereo(&mut x, &mut y);
        let mut d = StereoEq::new(48000.0);
        d.set_profile(&p);
        d.process_stereo(&mut x2, &mut y2);
        assert!(x[0].abs() > 0.0 && x2[0] == 0.0);
    }

    #[test]
    fn fault_state_machine() {
        let cfg = FaultConfig { recover_blocks: 5, clear_blocks: 3, ..FaultConfig::default() };
        let mut f = FaultDetector::new(cfg);
        assert_eq!(f.update(0.5, 0, 0.0, 0.0), FaultState::Ok);
        assert_eq!(f.update(5.0, 0, 0.0, 0.0), FaultState::Warn);
        // Warn holds in the hysteresis band (peak 3.0 is between 2.0 and 4.0).
        for _ in 0..10 {
            assert_eq!(f.update(3.0, 0, 0.0, 0.0), FaultState::Warn);
        }
        for i in 0..3 {
            let s = f.update(0.1, 0, 0.0, 0.0);
            assert_eq!(s, if i < 2 { FaultState::Warn } else { FaultState::Ok });
        }
        // NaN peak or non-finite storm mutes immediately.
        assert_eq!(f.update(f32::NAN, 0, 0.0, 0.0), FaultState::Muted);
        assert_eq!(f.update(0.1, 100, 0.0, 0.0), FaultState::Muted);
        // Underrun storm keeps it muted and resets recovery.
        for _ in 0..4 {
            f.update(0.1, 0, 0.0, 0.0);
        }
        assert_eq!(f.update(0.1, 0, 0.5, 0.0), FaultState::Muted);
        for i in 0..5 {
            let s = f.update(0.1, 0, 0.0, 0.0);
            assert_eq!(s, if i < 4 { FaultState::Muted } else { FaultState::Warn });
        }
        for _ in 0..3 {
            f.update(0.1, 0, 0.0, 0.0);
        }
        assert_eq!(f.state(), FaultState::Ok);
        assert_eq!(f.update(0.1, 0, 0.0, 0.3), FaultState::Muted);
        f.reset();
        assert_eq!(f.state().as_u32(), 0);
    }
}
