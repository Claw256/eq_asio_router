//! Biquad filters (RBJ cookbook) and the stereo EQ chain.

use crate::autoeq::EqProfile;
use crate::config::{JblLfoTarget, JblParams};
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
/// ramps over ~10 ms so toggling is click-free. Every stage is driven by [`JblParams`]
/// (values at 100 % intensity; the intensity scales them from idle). An optional LFO
/// "trunk rattle" modulates the output gain and/or the kick-band level. Allocation-free per sample.
#[derive(Clone, Copy, Debug)]
pub struct BassFx {
    fs: f64,
    target: f32,
    mix: f32,
    intensity: f32,
    params: JblParams,
    drive: f32,
    duck: f32,
    shelf_l: Biquad,
    shelf_r: Biquad,
    detector: Biquad,
    env: f32,
    attack: f32,
    release: f32,
    /// Low-quality mids/highs: quantiser step (0 = off), sample-hold factor and a dulling low pass.
    quant_step: f32,
    hold: u32,
    hold_pos: u32,
    held: [f32; 2],
    lp_l: Biquad,
    lp_r: Biquad,
    /// Sub resonance peak ahead of the clipper, and clip asymmetry bias.
    res_l: Biquad,
    res_r: Biquad,
    bias: f32,
    /// Extra kick/bass-only saturation: bass-band splitter and its drive (`kick_on`).
    kick_on: bool,
    kick_l: Biquad,
    kick_r: Biquad,
    kick_drive: f32,
    /// Output trim that keeps the loudness near 0 dB versus the dry signal.
    out_gain: f32,
    /// Rattle LFO: phase (0..1, kept across reconfiguration), per-sample increment, effective
    /// depth (0 = inactive), and which paths it modulates.
    lfo_phase: f32,
    lfo_inc: f32,
    lfo_depth: f32,
    lfo_follow: bool,
    lfo_gain: bool,
    lfo_band: bool,
}

impl BassFx {
    /// Create an inactive effect at `fs` Hz.
    pub fn new(fs: f64) -> Self {
        let mut fx = BassFx {
            fs,
            target: 0.0,
            mix: 0.0,
            intensity: 0.0,
            params: JblParams::default(),
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
            kick_on: true,
            kick_l: Biquad::identity(),
            kick_r: Biquad::identity(),
            kick_drive: 1.0,
            out_gain: 1.0,
            lfo_phase: 0.0,
            lfo_inc: 0.0,
            lfo_depth: 0.0,
            lfo_follow: false,
            lfo_gain: false,
            lfo_band: false,
        };
        fx.configure(false, 0, &JblParams::default());
        fx
    }

    /// Enable or disable, set intensity (0-100) and the stage parameters. Control path (uses `powf`).
    pub fn set(&mut self, enabled: bool, intensity_pct: u8, params: &JblParams) {
        self.configure(enabled, intensity_pct, params);
    }

    /// Change the sample rate, keeping state. Invalid rates are ignored.
    pub fn set_sample_rate(&mut self, fs: f64) {
        if fs.is_finite() && fs > 0.0 {
            self.fs = fs;
            let params = self.params;
            self.configure(self.target > 0.0, (self.intensity * 100.0).round() as u8, &params);
        }
    }

    fn configure(&mut self, enabled: bool, intensity_pct: u8, params: &JblParams) {
        let p = params.sanitized();
        self.params = p;
        let i = f32::from(intensity_pct.min(100)) / 100.0;
        let i64_ = f64::from(i);
        let nyq_cap = self.fs * 0.45;
        self.target = if enabled { 1.0 } else { 0.0 };
        self.intensity = i;
        self.drive = 1.0 + (p.drive_max - 1.0).max(0.0) * i;
        self.duck = if p.duck_on { p.duck_depth.max(0.0) * i } else { 0.0 };
        // Bit crush: step of the quantiser (0 = off), then sample-hold and a sliding low pass.
        self.quant_step =
            if p.crush_on { 2.0 / 2f32.powf(16.0 - (16.0 - p.crush_bits_min).max(0.0) * i) } else { 0.0 };
        self.hold = if p.hold_on { 1 + ((p.hold_max - 1.0).max(0.0) * i).round() as u32 } else { 1 };
        if p.lpf_on {
            let (a, b) = (f64::from(p.lpf_hz_start), f64::from(p.lpf_hz_end));
            let dull = FilterSpec {
                kind: FilterKind::LowPass,
                fc_hz: (a - (a - b) * i64_).min(nyq_cap).max(1.0),
                gain_db: 0.0,
                q: 0.707,
                enabled: true,
            };
            self.lp_l.set_spec(&dull, self.fs);
            self.lp_r.set_spec(&dull, self.fs);
        } else {
            self.lp_l = Biquad::identity();
            self.lp_r = Biquad::identity();
        }
        let shelf = FilterSpec {
            kind: FilterKind::LowShelf,
            fc_hz: f64::from(p.shelf_hz).min(nyq_cap),
            gain_db: f64::from(p.shelf_db) * i64_,
            q: 0.7,
            enabled: true,
        };
        self.shelf_l.set_spec(&shelf, self.fs);
        self.shelf_r.set_spec(&shelf, self.fs);
        if p.res_on {
            let res = FilterSpec {
                kind: FilterKind::Peaking,
                fc_hz: f64::from(p.res_hz).min(nyq_cap),
                gain_db: f64::from(p.res_db) * i64_,
                q: f64::from(p.res_q).max(0.1),
                enabled: true,
            };
            self.res_l.set_spec(&res, self.fs);
            self.res_r.set_spec(&res, self.fs);
        } else {
            self.res_l = Biquad::identity();
            self.res_r = Biquad::identity();
        }
        self.bias = p.bias_max * i;
        // Measured loudness of the effect (dB vs dry, music-like test signal) at 0/30/60/80/100 %
        // for the default parameters; interpolated and cancelled so the mode stays around 0 dB.
        // See `bass_fx_level_report`. `out_trim_db` lets the user correct drift for other settings.
        const LEVEL_DB: [(f32, f32); 5] = [(0.0, 0.0), (0.3, 0.86), (0.6, 2.79), (0.8, 2.32), (1.0, 1.23)];
        let mut meas = LEVEL_DB[4].1;
        for w in LEVEL_DB.windows(2) {
            if i <= w[1].0 {
                meas = w[0].1 + (w[1].1 - w[0].1) * (i - w[0].0) / (w[1].0 - w[0].0);
                break;
            }
        }
        self.out_gain = 0.45 * 10f32.powf((BASS_FX_TRIM_DB - meas) / 20.0) * 10f32.powf(p.out_trim_db / 20.0);
        self.kick_on = p.kick_on;
        self.kick_drive = 1.0 + (p.kick_drive_max - 1.0).max(0.0) * i;
        let kick = FilterSpec {
            kind: FilterKind::LowPass,
            fc_hz: f64::from(p.kick_hz).min(nyq_cap),
            gain_db: 0.0,
            q: 0.707,
            enabled: true,
        };
        self.kick_l.set_spec(&kick, self.fs);
        self.kick_r.set_spec(&kick, self.fs);
        let lp = FilterSpec {
            kind: FilterKind::LowPass,
            fc_hz: f64::from(p.detect_hz).min(nyq_cap),
            gain_db: 0.0,
            q: 0.707,
            enabled: true,
        };
        self.detector.set_spec(&lp, self.fs);
        let fs = self.fs;
        let ms_to_coef = |ms: f32| (1.0 - (-1.0 / (f64::from(ms) / 1000.0 * fs).max(1.0)).exp()) as f32;
        self.attack = ms_to_coef(p.duck_attack_ms);
        self.release = ms_to_coef(p.duck_release_ms);
        // LFO: the phase is deliberately left alone so reconfiguring does not glitch the rattle.
        let depth = if p.lfo_on && p.lfo_depth.is_finite() { p.lfo_depth.clamp(0.0, 1.0) } else { 0.0 };
        self.lfo_depth = depth;
        self.lfo_inc = (f64::from(p.lfo_rate_hz.max(0.0)) / self.fs) as f32;
        self.lfo_follow = p.lfo_follow_bass;
        self.lfo_gain = matches!(p.lfo_target, JblLfoTarget::Gain | JblLfoTarget::Both);
        self.lfo_band = matches!(p.lfo_target, JblLfoTarget::BassBand | JblLfoTarget::Both);
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
        let kn = 0.8 / self.kick_drive.tanh().max(1e-3);
        let lfo_active = self.lfo_depth > 0.0 && (self.lfo_gain || (self.lfo_band && self.kick_on));
        for (xl, xr) in l.iter_mut().zip(r.iter_mut()) {
            if self.mix < self.target {
                self.mix = (self.mix + step).min(self.target);
            } else if self.mix > self.target {
                self.mix = (self.mix - step).max(self.target);
            }
            // Rattle LFO: 1 - depth * 0.5 * (1 + sin), optionally only while the bass is hitting.
            let lfo = if lfo_active {
                let s = (2.0 * std::f32::consts::PI * self.lfo_phase).sin();
                self.lfo_phase += self.lfo_inc;
                if self.lfo_phase >= 1.0 {
                    self.lfo_phase -= self.lfo_phase.floor();
                }
                let d = if self.lfo_follow { self.lfo_depth * self.env.min(1.0) } else { self.lfo_depth };
                1.0 - d * 0.5 * (1.0 + s)
            } else {
                1.0
            };
            let sl = self.res_l.process(self.shelf_l.process(*xl));
            let sr = self.res_r.process(self.shelf_r.process(*xr));
            // Kick/bass band gets its own heavy saturation before the full-band clipper.
            let (sl, sr) = if self.kick_on {
                let (kl, kr) = (self.kick_l.process(sl), self.kick_r.process(sr));
                let m = if self.lfo_band { lfo } else { 1.0 };
                (sl - kl + (self.kick_drive * kl * m).tanh() * kn, sr - kr + (self.kick_drive * kr * m).tanh() * kn)
            } else {
                (sl, sr)
            };
            let bass = self.detector.process(0.5 * (sl + sr)).abs();
            let coef = if bass > self.env { self.attack } else { self.release };
            self.env += (bass - self.env) * coef;
            let mut g = 1.0 / (1.0 + self.duck * self.env.min(2.0));
            if self.lfo_gain {
                g *= lfo;
            }
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
            self.hold_pos = (self.hold_pos + 1) % self.hold.max(1);
            let q = self.quant_step;
            if q > 0.0 {
                yl = (self.held[0] / q).round() * q;
                yr = (self.held[1] / q).round() * q;
            } else {
                yl = self.held[0];
                yr = self.held[1];
            }
            yl = self.lp_l.process(yl);
            yr = self.lp_r.process(yr);
            *xl += (yl - *xl) * self.mix;
            *xr += (yr - *xr) * self.mix;
        }
    }

    /// Clear filter, envelope and LFO state.
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
        self.lfo_phase = 0.0;
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
    /// AutoEQ stage (preamp + bands) switch and its wet amount (1 = fully applied). Disabling
    /// ramps `eq_mix` to 0 over ~10 ms while the filters keep running, so re-enabling is click-free.
    eq_enabled: bool,
    eq_mix: f32,
    eq_dry_l: Box<[f32]>,
    eq_dry_r: Box<[f32]>,
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
            eq_enabled: true,
            eq_mix: 1.0,
            eq_dry_l: vec![0.0; BYPASS_SCRATCH_FRAMES].into_boxed_slice(),
            eq_dry_r: vec![0.0; BYPASS_SCRATCH_FRAMES].into_boxed_slice(),
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
        self.eq_enabled = other.eq_enabled;
        self.eq_mix = other.eq_mix;
    }

    /// Switch the AutoEQ stage (preamp and bands) on or off. Off crossfades just that stage to
    /// dry over ~10 ms; JBL Bass Mode and the limiter are unaffected.
    pub fn set_eq_enabled(&mut self, on: bool) {
        self.eq_enabled = on;
    }

    /// Enable or disable "JBL Bass Mode", set its intensity (0-100) and stage parameters.
    pub fn set_bass_fx(&mut self, enabled: bool, intensity_pct: u8, params: &JblParams) {
        self.bass_fx.set(enabled, intensity_pct, params);
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
        if self.eq_enabled && self.eq_mix == 1.0 {
            self.eq_stage(l, r);
        } else {
            self.eq_stage_ramped(l, r);
        }
        self.bass_fx.process(l, r);
        if self.limiter {
            l.iter_mut().for_each(|s| *s = soft_limit(*s));
            r.iter_mut().for_each(|s| *s = soft_limit(*s));
        }
    }

    /// Preamp and band filters, in place.
    fn eq_stage(&mut self, l: &mut [f32], r: &mut [f32]) {
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
    }

    /// The EQ stage while `eq_mix` is ramping to, or sitting at, 0: the filters keep running and
    /// the output crossfades (linear, per sample) between the stage input and output.
    fn eq_stage_ramped(&mut self, l: &mut [f32], r: &mut [f32]) {
        let target = if self.eq_enabled { 1.0f32 } else { 0.0 };
        let step = (1.0 / (BYPASS_RAMP_SECS * self.fs).max(1.0)) as f32;
        let n = l.len().min(r.len());
        let mut pos = 0;
        while pos < n {
            let end = (pos + BYPASS_SCRATCH_FRAMES).min(n);
            let (cl, cr) = (&mut l[pos..end], &mut r[pos..end]);
            pos = end;
            let len = cl.len();
            self.eq_dry_l[..len].copy_from_slice(cl);
            self.eq_dry_r[..len].copy_from_slice(cr);
            self.eq_stage(cl, cr);
            let mut w = self.eq_mix;
            for i in 0..len {
                if w < target {
                    w = (w + step).min(target);
                } else if w > target {
                    w = (w - step).max(target);
                }
                cl[i] = self.eq_dry_l[i] + (cl[i] - self.eq_dry_l[i]) * w;
                cr[i] = self.eq_dry_r[i] + (cr[i] - self.eq_dry_r[i]) * w;
            }
            self.eq_mix = w;
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

    /// Deterministic music-like test signals at `rms_db` dBFS RMS (128 BPM kick, sub-bass line,
    /// pink noise + chord, vocal-like mids). Mirrors the `jbl_audit` harness in `tests/`.
    fn level_signals(n: usize, fs: f64, rms_db: f64) -> Vec<(&'static str, Vec<f32>)> {
        let beat = 60.0 / 128.0;
        let kick = |t: f64| {
            let tk = t % beat;
            let ph = 2.0 * PI * (48.0 * tk + 90.0 * (1.0 - (-tk * 40.0).exp()) / 40.0);
            (-tk * 9.0).exp() * ph.sin()
        };
        let vocal = |t: f64| {
            let f0 = 200.0 * (1.0 + 0.01 * (2.0 * PI * 5.0 * t).sin());
            (2..=18)
                .map(|h| {
                    let f = f0 * h as f64;
                    let form = |c: f64, w: f64| (-((f - c) / w).powi(2)).exp();
                    let a = form(700.0, 250.0) + 0.6 * form(1200.0, 300.0) + 0.3 * form(2600.0, 500.0);
                    a / (h as f64).sqrt() * (2.0 * PI * f * t + h as f64).sin()
                })
                .sum::<f64>()
        };
        let mut seed = 7u64;
        let mut noise = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        };
        let notes = [55.0, 55.0, 65.41, 49.0];
        let mut pink = [0.0f64; 7];
        let (mut a, mut b, mut c) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
        for i in 0..n {
            let t = i as f64 / fs;
            let w = noise();
            pink[0] = 0.99886 * pink[0] + w * 0.0555179;
            pink[1] = 0.99332 * pink[1] + w * 0.0750759;
            pink[2] = 0.96900 * pink[2] + w * 0.1538520;
            pink[3] = 0.86650 * pink[3] + w * 0.3104856;
            pink[4] = 0.55000 * pink[4] + w * 0.5329522;
            pink[5] = -0.7616 * pink[5] - w * 0.0168980;
            let p = pink[..6].iter().sum::<f64>() + pink[6] + w * 0.5362;
            pink[6] = w * 0.115926;
            let note = notes[((t / (beat * 4.0)) as usize) % 4];
            let sub = 0.5 * (2.0 * PI * note * t).sin() * (0.6 + 0.4 * (2.0 * PI * t / beat).sin().abs());
            let hat = 0.1 * noise() * (-((t + beat / 2.0) % beat) * 60.0).exp();
            let chord: f64 = [220.0, 261.63, 329.63]
                .iter()
                .map(|f| (1..=8).map(|h| (2.0 * PI * f * h as f64 * t).sin() / h as f64).sum::<f64>())
                .sum();
            a.push(0.9 * kick(t) + sub + hat);
            b.push(0.35 * p + 0.12 * chord + 0.4 * (2.0 * PI * 55.0 * t).sin());
            c.push(0.9 * kick(t) + 0.35 * vocal(t));
        }
        let scale = |v: Vec<f64>| -> Vec<f32> {
            let r = (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt();
            let g = 10f64.powf(rms_db / 20.0) / r;
            v.iter().map(|x| (x * g) as f32).collect()
        };
        vec![("kick+sub", scale(a)), ("fullband", scale(b)), ("kick+vox", scale(c))]
    }

    /// Loudness report: JBL output vs dry at 30/60/80/100 % for the old calibration tone and three
    /// music-like signals at -18 dBFS RMS (typical programme level after EQ headroom). The mean must
    /// sit near `BASS_FX_TRIM_DB` (re-calibrate `LEVEL_DB` from this printout when defaults change).
    #[test]
    fn bass_fx_level_report() {
        let fs = 48000.0;
        let n = 96000;
        // Calibration tone: 55 Hz kick-ish + mid tones + highs, ~-18 dBFS RMS.
        let tone: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let env = (-((t * 2.0) % 1.0) * 6.0).exp();
                (0.35 * env * (2.0 * PI * 55.0 * t).sin()
                    + 0.12 * (2.0 * PI * 440.0 * t).sin()
                    + 0.08 * (2.0 * PI * 2500.0 * t).sin()
                    + 0.04 * (2.0 * PI * 7000.0 * t).sin()) as f32
            })
            .collect();
        let mut sigs = level_signals(n, fs, -18.0);
        sigs.push(("tone", tone));
        let rms = |v: &[f32]| (v.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        for pct in [30u8, 60, 80, 100] {
            let mut sum = 0.0;
            for (name, x) in &sigs {
                let (mut l, mut r) = (x.clone(), x.clone());
                let mut eq = StereoEq::new(fs);
                eq.set_limiter(true);
                eq.set_bass_fx(true, pct, &JblParams::default());
                eq.process_stereo(&mut l, &mut r);
                let db = 20.0 * (rms(&l[n / 2..]) / rms(&x[n / 2..])).log10();
                println!("LEVEL pct={pct} {name} gain_db={db:.2}");
                assert!((db - f64::from(BASS_FX_TRIM_DB)).abs() < 2.0, "{name} level off at {pct}%: {db:.2} dB");
                sum += db;
            }
            let mean = sum / sigs.len() as f64;
            println!("LEVEL pct={pct} MEAN gain_db={mean:.2}");
            assert!((mean - f64::from(BASS_FX_TRIM_DB)).abs() < 0.75, "mean level drifted at {pct}%: {mean:.2} dB");
        }
    }

    /// Deterministic stereo test signal: 55 Hz + 1 kHz sines plus kick-like decaying bursts (R slightly different).
    fn golden_signal(n: usize, fs: f64) -> (Vec<f32>, Vec<f32>) {
        let mk = |k: f64| -> Vec<f32> {
            (0..n)
                .map(|i| {
                    let t = i as f64 / fs;
                    let kick = (-((t * 4.0) % 1.0) * 9.0).exp() * (2.0 * PI * (45.0 + 60.0 * (-((t * 4.0) % 1.0) * 20.0).exp()) * t).sin();
                    (0.30 * (2.0 * PI * 55.0 * t).sin() + 0.15 * k * (2.0 * PI * 1000.0 * t).sin() + 0.35 * kick) as f32
                })
                .collect()
        };
        (mk(1.0), mk(0.8))
    }

    /// Golden regression for the default BassFx sound. Re-captured when the defaults were retuned
    /// (res_db 9 -> 6, res_q 3 -> 2, drive_max 5 -> 4, plus the `LEVEL_DB` recalibration); it must
    /// not change from DSP refactors that keep the defaults.
    #[test]
    fn bass_fx_golden_default_output() {
        let fs = 48000.0;
        let n = 48000;
        let (x_l, x_r) = golden_signal(n, fs);
        // (pct, rms_l, rms_r, [l[1000], l[12000], l[30001], r[20000], r[47999]])
        let golden: [(u8, f64, f64, [f32; 5]); 4] = GOLDEN_VALUES;
        for (pct, g_rms_l, g_rms_r, g_samples) in golden {
            let (mut l, mut r) = (x_l.clone(), x_r.clone());
            let mut eq = StereoEq::new(fs);
            eq.set_limiter(true);
            eq.set_bass_fx(true, pct, &JblParams::default());
            eq.process_stereo(&mut l, &mut r);
            let rms = |v: &[f32]| (v.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
            let got = [l[1000], l[12000], l[30001], r[20000], r[47999]];
            println!("GOLDEN pct={pct} rms_l={:.9} rms_r={:.9} samples={:?}", rms(&l), rms(&r), got);
            assert!((rms(&l) - g_rms_l).abs() < 1e-6, "rms_l changed at {pct}%: {} vs {g_rms_l}", rms(&l));
            assert!((rms(&r) - g_rms_r).abs() < 1e-6, "rms_r changed at {pct}%: {} vs {g_rms_r}", rms(&r));
            for (a, b) in got.iter().zip(g_samples.iter()) {
                assert!((a - b).abs() < 1e-5, "sample changed at {pct}%: {a} vs {b}");
            }
        }
    }

    const GOLDEN_VALUES: [(u8, f64, f64, [f32; 5]); 4] = [
        (30, 0.180613230, 0.180478605, [0.17582655, -0.20840953, 0.20334709, -0.24601033, -0.14398156]),
        (60, 0.123357297, 0.123356423, [0.10713634, -0.14554128, 0.12687653, -0.14801922, -0.14690912]),
        (80, 0.102350772, 0.102268223, [0.074729785, -0.12072089, 0.09691678, -0.121571034, -0.12072089]),
        (100, 0.089456323, 0.089219136, [0.06836742, -0.10359563, 0.078125, -0.109375, -0.107421875]),
    ];

    #[test]
    fn bass_fx_off_is_bit_identical() {
        let x = two_tone(48000, 48000.0);
        let (mut a_l, mut a_r) = (x.clone(), x.clone());
        let (mut b_l, mut b_r) = (x.clone(), x);
        let mut plain = StereoEq::new(48000.0);
        let mut fx = StereoEq::new(48000.0);
        fx.set_bass_fx(false, 80, &JblParams::default());
        plain.process_stereo(&mut a_l, &mut a_r);
        fx.process_stereo(&mut b_l, &mut b_r);
        assert_eq!(a_l, b_l);
        assert_eq!(a_r, b_r);
    }

    fn run_fx(fs: f64, pct: u8, p: &JblParams, n: usize) -> (Vec<f32>, Vec<f32>) {
        let (mut l, mut r) = golden_signal(n, fs);
        let mut eq = StereoEq::new(fs);
        eq.set_limiter(true);
        eq.set_bass_fx(true, pct, p);
        eq.process_stereo(&mut l, &mut r);
        (l, r)
    }

    #[test]
    fn bass_fx_each_toggle_off_removes_its_stage() {
        let base = run_fx(48000.0, 80, &JblParams::default(), 24000).0;
        let toggles: [(&str, fn(&mut JblParams)); 6] = [
            ("duck", |p| p.duck_on = false),
            ("res", |p| p.res_on = false),
            ("kick", |p| p.kick_on = false),
            ("crush", |p| p.crush_on = false),
            ("hold", |p| p.hold_on = false),
            ("lpf", |p| p.lpf_on = false),
        ];
        for (name, f) in toggles {
            let mut p = JblParams::default();
            f(&mut p);
            let out = run_fx(48000.0, 80, &p, 24000).0;
            assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0), "{name} not finite/bounded");
            assert!(out != base, "{name} off should change the output");
        }
    }

    #[test]
    fn bass_fx_lfo_depth_zero_or_off_is_identical() {
        let base = run_fx(48000.0, 80, &JblParams::default(), 24000);
        for target in [JblLfoTarget::Gain, JblLfoTarget::BassBand, JblLfoTarget::Both] {
            let off = JblParams { lfo_on: false, lfo_depth: 0.8, lfo_target: target, ..JblParams::default() };
            let zero = JblParams { lfo_on: true, lfo_depth: 0.0, lfo_target: target, ..JblParams::default() };
            assert_eq!(run_fx(48000.0, 80, &off, 24000), base);
            assert_eq!(run_fx(48000.0, 80, &zero, 24000), base);
        }
        let on = JblParams { lfo_on: true, lfo_depth: 0.8, lfo_follow_bass: false, ..JblParams::default() };
        assert!(run_fx(48000.0, 80, &on, 24000) != base);
    }

    #[test]
    fn bass_fx_lfo_modulates_at_set_rate() {
        let fs = 48000.0;
        let rate = 6.0;
        let p = JblParams {
            lfo_on: true,
            lfo_depth: 0.9,
            lfo_rate_hz: rate,
            lfo_follow_bass: false,
            lfo_target: JblLfoTarget::Gain,
            duck_on: false,
            ..JblParams::default()
        };
        // Steady tone, so the output envelope is dominated by the LFO.
        let n = 96000;
        let tone: Vec<f32> = (0..n).map(|i| (0.3 * (2.0 * PI * 200.0 * i as f64 / fs).sin()) as f32).collect();
        let (mut l, mut r) = (tone.clone(), tone);
        let mut eq = StereoEq::new(fs);
        eq.set_bass_fx(true, 100, &p);
        eq.process_stereo(&mut l, &mut r);
        // Peak envelope per 5 ms window after the wet ramp.
        let win = 240;
        let env: Vec<f32> = l[4800..].chunks(win).map(|c| c.iter().fold(0f32, |a, s| a.max(s.abs()))).collect();
        let mean = env.iter().sum::<f32>() / env.len() as f32;
        let var = env.iter().map(|e| (e - mean).powi(2)).sum::<f32>();
        assert!(var > 1e-3, "envelope should wobble, var={var}");
        // One LFO period = fs/rate samples = 1/6 s = 40 windows: envelope repeats.
        let period = (fs / f64::from(rate) / win as f64).round() as usize;
        for k in 0..env.len() - period {
            assert!((env[k] - env[k + period]).abs() < 0.02, "not periodic at {k}: {} vs {}", env[k], env[k + period]);
        }
    }

    #[test]
    fn bass_fx_lfo_phase_kept_on_configure_cleared_on_reset() {
        let mut fx = BassFx::new(48000.0);
        let p = JblParams { lfo_on: true, lfo_depth: 0.5, ..JblParams::default() };
        fx.set(true, 80, &p);
        let (mut l, mut r) = (vec![0.1f32; 1000], vec![0.1f32; 1000]);
        fx.process(&mut l, &mut r);
        let ph = fx.lfo_phase;
        assert!(ph > 0.0);
        fx.set(true, 60, &p);
        fx.set_sample_rate(96000.0);
        assert_eq!(fx.lfo_phase, ph);
        fx.reset();
        assert_eq!(fx.lfo_phase, 0.0);
    }

    fn extreme_sets() -> Vec<JblParams> {
        let lo = JblParams {
            duck_depth: 0.0,
            duck_attack_ms: 0.5,
            duck_release_ms: 10.0,
            detect_hz: 40.0,
            shelf_hz: 30.0,
            shelf_db: 0.0,
            res_hz: 25.0,
            res_db: 0.0,
            res_q: 0.5,
            drive_max: 1.0,
            bias_max: 0.0,
            kick_hz: 60.0,
            kick_drive_max: 1.0,
            crush_bits_min: 16.0,
            hold_max: 1.0,
            lpf_hz_start: 2000.0,
            lpf_hz_end: 2000.0,
            lfo_on: true,
            lfo_rate_hz: 0.5,
            lfo_depth: 0.0,
            out_trim_db: -12.0,
            ..JblParams::default()
        };
        let hi = JblParams {
            duck_depth: 6.0,
            duck_attack_ms: 50.0,
            duck_release_ms: 800.0,
            detect_hz: 250.0,
            shelf_hz: 200.0,
            shelf_db: 18.0,
            res_hz: 120.0,
            res_db: 18.0,
            res_q: 10.0,
            drive_max: 12.0,
            bias_max: 0.8,
            kick_hz: 300.0,
            kick_drive_max: 6.0,
            crush_bits_min: 6.0,
            hold_max: 8.0,
            lpf_hz_start: 20000.0,
            lpf_hz_end: 2000.0,
            lfo_on: true,
            lfo_rate_hz: 20.0,
            lfo_depth: 1.0,
            lfo_follow_bass: false,
            lfo_target: JblLfoTarget::Both,
            out_trim_db: 6.0,
            ..JblParams::default()
        };
        let swapped = JblParams { lpf_hz_start: 2000.0, lpf_hz_end: 20000.0, ..hi };
        let junk = JblParams {
            duck_depth: f32::NAN,
            res_q: f32::INFINITY,
            shelf_hz: -5.0,
            lfo_rate_hz: f32::NAN,
            lfo_depth: 1e9,
            out_trim_db: f32::NEG_INFINITY,
            hold_max: -3.0,
            ..hi
        };
        vec![lo, hi, swapped, junk]
    }

    #[test]
    fn bass_fx_extreme_params_finite_and_bounded_at_all_rates() {
        for fs in [44100.0, 96000.0, 192000.0] {
            for (k, p) in extreme_sets().iter().enumerate() {
                for pct in [0u8, 1, 50, 100] {
                    let (l, r) = run_fx(fs, pct, p, 19200);
                    assert!(
                        l.iter().chain(r.iter()).all(|s| s.is_finite() && s.abs() <= 1.0),
                        "non-finite/unbounded: fs={fs} set={k} pct={pct}"
                    );
                }
            }
        }
    }

    #[test]
    fn bass_fx_sample_rate_change_with_effect_on() {
        let p = JblParams { lfo_on: true, lfo_follow_bass: false, ..JblParams::default() };
        let mut eq = StereoEq::new(48000.0);
        eq.set_limiter(true);
        eq.set_bass_fx(true, 100, &p);
        for fs in [48000.0, 192000.0, 44100.0, 96000.0] {
            eq.set_sample_rate(fs);
            let (mut l, mut r) = golden_signal(9600, fs);
            eq.process_stereo(&mut l, &mut r);
            assert!(l.iter().chain(r.iter()).all(|s| s.is_finite() && s.abs() <= 1.0), "fs={fs}");
        }
        // Params survive the rate change: result at 44.1k equals a fresh effect configured there.
        let mut a = StereoEq::new(48000.0);
        a.set_bass_fx(true, 70, &p);
        a.set_sample_rate(44100.0);
        let mut b = StereoEq::new(44100.0);
        b.set_bass_fx(true, 70, &p);
        let (mut al, mut ar) = golden_signal(4410, 44100.0);
        let (mut bl, mut br) = (al.clone(), ar.clone());
        a.process_stereo(&mut al, &mut ar);
        b.process_stereo(&mut bl, &mut br);
        assert_eq!(al, bl);
    }

    #[test]
    fn bass_fx_on_changes_signal_and_stays_finite_and_bounded() {
        let x = two_tone(48000, 48000.0);
        let (mut l, mut r) = (x.clone(), x.clone());
        let mut eq = StereoEq::new(48000.0);
        eq.set_limiter(true);
        eq.set_bass_fx(true, 100, &JblParams::default());
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
    fn eq_enabled_off_ramps_to_dry_and_back_click_free() {
        let fs = 48000.0;
        let wet = 0.5 * 10f32.powf(-6.0 / 20.0);
        let mut eq = StereoEq::new(fs);
        eq.set_profile(&EqProfile {
            name: String::new(),
            preamp_db: -6.0,
            filters: vec![spec(FilterKind::Peaking, 1000.0, 0.0, 1.0)],
        });
        let n = 4800;
        let run = |eq: &mut StereoEq| {
            let mut l = vec![0.5f32; n];
            let mut r = l.clone();
            eq.process_stereo(&mut l, &mut r);
            l
        };
        assert!((run(&mut eq)[n - 1] - wet).abs() < 1e-4, "enabled by default");
        eq.set_eq_enabled(false);
        let l = run(&mut eq);
        assert!(l.windows(2).all(|w| w[1] >= w[0] - 1e-6), "monotonic to dry");
        assert!(l.windows(2).all(|w| (w[1] - w[0]).abs() < 1e-3), "no step");
        assert!((l[0] - wet).abs() < 0.01);
        assert!((l[n - 1] - 0.5).abs() < 1e-6, "fully dry once ramped");
        assert!((run(&mut eq)[0] - 0.5).abs() < 1e-6, "stays dry");
        eq.set_eq_enabled(true);
        let l = run(&mut eq);
        assert!(l.windows(2).all(|w| (w[1] - w[0]).abs() < 1e-3), "no step back");
        assert!((l[n - 1] - wet).abs() < 1e-4, "EQ applied again");
        // A block larger than the scratch buffer works too.
        eq.set_eq_enabled(false);
        let big = BYPASS_SCRATCH_FRAMES * 2 + 10;
        let mut l = vec![0.5f32; big];
        let mut r = l.clone();
        eq.process_stereo(&mut l, &mut r);
        assert!((l[big - 1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn eq_disabled_leaves_jbl_bass_mode_and_limiter_working() {
        let fs = 48000.0;
        let params = JblParams::default();
        let sig = |n: usize| -> Vec<f32> {
            (0..n).map(|i| 0.4 * (std::f32::consts::TAU * 55.0 * i as f32 / fs as f32).sin()).collect()
        };
        let mut eq = StereoEq::new(fs);
        eq.set_profile(&EqProfile {
            name: String::new(),
            preamp_db: -6.0,
            filters: vec![spec(FilterKind::LowShelf, 100.0, 6.0, 0.7)],
        });
        eq.set_eq_enabled(false);
        eq.set_limiter(true);
        // Let the EQ ramp finish, then compare with Bass Mode off and on.
        let warm = 4800;
        let (mut wl, mut wr) = (sig(warm), sig(warm));
        eq.process_stereo(&mut wl, &mut wr);
        let n = 9600;
        let (mut off_l, mut off_r) = (sig(n), sig(n));
        eq.process_stereo(&mut off_l, &mut off_r);
        let dry = sig(n);
        assert!(off_l.iter().zip(&dry).all(|(a, b)| (a - b).abs() < 1e-6), "EQ off and Bass Mode off is dry");
        eq.set_bass_fx(true, 80, &params);
        let (mut on_l, mut on_r) = (sig(n), sig(n));
        eq.process_stereo(&mut on_l, &mut on_r);
        let diff = on_l.iter().zip(&dry).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(diff > 0.05, "Bass Mode still shapes the signal with the EQ disabled: {diff}");
        assert!(on_l.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
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
