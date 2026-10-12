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

/// Static loudness target of JBL Bass Mode relative to the dry signal, in dB: a touch below dry.
/// With auto level the effect lands here at every input level; with it off, only at the reference
/// level (`AUTO_REF_DB`, about -18 dBFS RMS).
const BASS_FX_TRIM_DB: f32 = -1.0;

/// Auto level: the input RMS (dBFS) at which the clipper/saturator calibration (`LEVEL_DB_*` and the
/// presets' `out_trim_db`) was made. The wet path always sees its input scaled to this level.
#[cfg(test)]
const AUTO_REF_DB: f32 = -18.0;
/// Auto level: mean-square of the reference level (`10^(AUTO_REF_DB / 10)`; checked by a test).
const AUTO_REF_MS: f32 = 0.015_848_932;
/// Auto level: largest pre-gain (dB). Input quieter than `AUTO_REF_DB - AUTO_MAX_BOOST_DB` (-42 dBFS)
/// gets this fixed gain, so silence and noise are never boosted without bound (and never NaN).
#[cfg(test)]
const AUTO_MAX_BOOST_DB: f32 = 24.0;
/// Auto level: largest pre-attenuation (dB) for very hot input.
#[cfg(test)]
const AUTO_MAX_CUT_DB: f32 = 30.0;
/// Auto level: floor of the mean-square ratio and the clamps of the make-up factor, derived from
/// `AUTO_MAX_BOOST_DB` / `AUTO_MAX_CUT_DB` (10^(-24/10), 10^(-24/20), 10^(30/20); checked by a test).
const AUTO_MIN_RATIO: f32 = 0.003_981_072;
const AUTO_MIN_INV: f32 = 0.063_095_73;
const AUTO_MAX_INV: f32 = 31.622_776;
/// Auto level: attack / release (ms) of the input mean-square follower. Slow enough (>> one 40 Hz
/// cycle) that the bass does not ripple the gain, fast enough to follow the programme.
const AUTO_ATTACK_MS: f32 = 40.0;
const AUTO_RELEASE_MS: f32 = 300.0;
/// Auto level: when the level really drops (the fast follower, `AUTO_FAST_MS`, is more than
/// `AUTO_DROP_RATIO` = 4 dB below the slow one), the slow follower catches up with time constant
/// `AUTO_CATCHUP_MS`, so a sudden volume cut is not followed by a long burst of under-driven (too
/// loud) output. Ordinary programme dips (< 4 dB) keep the slow release.
const AUTO_FAST_MS: f32 = 40.0;
const AUTO_CATCHUP_MS: f32 = 25.0;
const AUTO_DROP_RATIO: f32 = 2.5;

/// Level guard: window (ms) of the mean-square followers on the input and the wet signal. Long
/// enough (>> one 40 Hz cycle) that the bass does not ripple the gain, short enough to follow the
/// programme.
const GUARD_WINDOW_MS: f32 = 150.0;
/// Level guard: time (ms) for the gain to come down when the wet signal is too loud.
const GUARD_ATTACK_MS: f32 = 40.0;
/// Level guard: time (ms) for the gain to recover once the wet signal is no longer too loud.
const GUARD_RELEASE_MS: f32 = 300.0;
/// Level guard: mean-square floor (about -100 dBFS) below which the ratio is not trusted and the
/// guard stays at unity, so silence and dither-level noise never produce gain swings or NaN.
const GUARD_FLOOR: f32 = 1e-10;
/// Level guard: ceiling of the wet RMS relative to the input RMS (dB), before `bass_boost_db`.
const GUARD_CEILING_DB: f32 = 0.0;

/// Two cascaded identical sections (4th order, Linkwitz-Riley when Butterworth Q).
#[inline]
fn lr4(b: &mut [Biquad; 2], x: f32) -> f32 {
    let y = b[0].process(x);
    b[1].process(y)
}

/// "JBL Bass Mode": car-subwoofer style bass boost, saturation and bass-keyed pumping.
///
/// Low shelf boost, `tanh` clipping, then a gain dip keyed by the bass envelope so the
/// mids and highs duck with each bass hit (an amp running out of headroom). The wet amount
/// ramps over ~10 ms so toggling is click-free. Auto level (`JblParams::auto_level`) pre-gains the
/// wet path to a fixed reference level from a slow input level follower and undoes that gain on the
/// wet output, so the distortion amount and the loudness do not depend on the input volume. An
/// optional level guard (`JblParams::loudness_guard`) keeps the wet RMS from exceeding the input
/// RMS. Every stage is driven by [`JblParams`]
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
    /// Ducking floor (linear gain the duck gain is lifted towards); `duck_floor_on` false = no floor.
    duck_floor: f32,
    duck_floor_on: bool,
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
    /// Clean low shelf on the wet output (after the lo-fi stages); `boost_on` false = bypassed.
    boost_on: bool,
    boost_l: Biquad,
    boost_r: Biquad,
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
    /// Auto level (see `AUTO_*`): on/off, mean-square follower of the (dry) input and its attack /
    /// release coefficients. The wet path is pre-gained to the reference level and the same factor
    /// is undone on the wet output, so the clipper sees a level-independent drive.
    auto_on: bool,
    lvl_ms: f32,
    lvl_fast: f32,
    lvl_att: f32,
    lvl_rel: f32,
    lvl_fc: f32,
    lvl_cc: f32,
    /// Level guard (see `GUARD_*`): on/off, ceiling (linear, vs input RMS), mean-square followers
    /// of the input and of the wet signal, the smoothed gain applied to the wet signal, and the
    /// one-pole coefficients for the followers / gain attack / gain release.
    guard_on: bool,
    guard_ceiling: f32,
    guard_in: f32,
    guard_out: f32,
    guard_gain: f32,
    guard_win: f32,
    guard_att: f32,
    guard_rel: f32,
    /// Mids/highs pass-through (`JblParams::mid_pass`): amount (0 = off), crossover high passes
    /// (two cascaded 2nd-order sections = 4th order, Linkwitz-Riley): a high pass on the dry input
    /// and a low pass on the wet signal, and the guard's followers of the pass-through energy and its correlation with
    /// the guarded wet signal.
    mid_pass: f32,
    mid_on: bool,
    hp_dry_l: [Biquad; 2],
    hp_dry_r: [Biquad; 2],
    lp_wet_l: [Biquad; 2],
    lp_wet_r: [Biquad; 2],
    guard_pass: f32,
    guard_cross: f32,
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
            duck_floor: 0.0,
            duck_floor_on: false,
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
            boost_on: false,
            boost_l: Biquad::identity(),
            boost_r: Biquad::identity(),
            res_l: Biquad::identity(),
            res_r: Biquad::identity(),
            bias: 0.0,
            kick_on: true,
            kick_l: Biquad::identity(),
            kick_r: Biquad::identity(),
            kick_drive: 1.0,
            out_gain: 1.0,
            auto_on: true,
            lvl_ms: 0.0,
            lvl_fast: 0.0,
            lvl_att: 0.0,
            lvl_rel: 0.0,
            lvl_fc: 0.0,
            lvl_cc: 0.0,
            guard_on: true,
            guard_ceiling: 1.0,
            guard_in: 0.0,
            guard_out: 0.0,
            guard_gain: 1.0,
            guard_win: 0.0,
            guard_att: 0.0,
            guard_rel: 0.0,
            mid_pass: 0.0,
            mid_on: false,
            hp_dry_l: [Biquad::identity(); 2],
            hp_dry_r: [Biquad::identity(); 2],
            lp_wet_l: [Biquad::identity(); 2],
            lp_wet_r: [Biquad::identity(); 2],
            guard_pass: 0.0,
            guard_cross: 0.0,
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
        self.duck_floor_on = p.duck_floor_db > crate::config::DUCK_FLOOR_OFF_DB;
        self.duck_floor = if self.duck_floor_on { 10f32.powf(p.duck_floor_db / 20.0) } else { 0.0 };
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
        // Clean wet-path bass boost: fully bypassed (and state cleared) at 0 dB.
        if p.bass_boost_db > 0.0 {
            let boost = FilterSpec {
                kind: FilterKind::LowShelf,
                fc_hz: f64::from(p.bass_boost_hz).min(nyq_cap),
                gain_db: f64::from(p.bass_boost_db),
                q: 0.7,
                enabled: true,
            };
            if !self.boost_on {
                self.boost_l.reset();
                self.boost_r.reset();
            }
            self.boost_l.set_spec(&boost, self.fs);
            self.boost_r.set_spec(&boost, self.fs);
            self.boost_on = true;
        } else {
            self.boost_on = false;
            self.boost_l = Biquad::identity();
            self.boost_r = Biquad::identity();
        }
        // Mids/highs pass-through: crossover high passes (LR4) on the dry input and the wet signal.
        if p.mid_pass > 0.0 {
            let hp = FilterSpec {
                kind: FilterKind::HighPass,
                fc_hz: f64::from(p.mid_pass_hz).min(nyq_cap),
                gain_db: 0.0,
                q: 0.707,
                enabled: true,
            };
            let lp = FilterSpec { kind: FilterKind::LowPass, ..hp };
            if !self.mid_on {
                self.reset_crossover();
                self.guard_pass = 0.0;
                self.guard_cross = 0.0;
            }
            let fs = self.fs;
            for b in self.hp_dry_l.iter_mut().chain(self.hp_dry_r.iter_mut()) {
                b.set_spec(&hp, fs);
            }
            for b in self.lp_wet_l.iter_mut().chain(self.lp_wet_r.iter_mut()) {
                b.set_spec(&lp, fs);
            }
            self.mid_on = true;
            self.mid_pass = p.mid_pass;
        } else {
            self.mid_on = false;
            self.mid_pass = 0.0;
        }
        self.bias = p.bias_max * i;
        // Measured loudness of the effect (dB vs dry, with the 0.45 base gain) at 0/30/60/80/90/100 %
        // for the default parameters; interpolated and cancelled so the mode lands at `BASS_FX_TRIM_DB`
        // (about 1 dB below dry). Auto level on: music-like test signals at -30..-12 dBFS RMS (the
        // result is the same at every level). Auto level off (legacy): about -18 dBFS RMS only.
        // See `bass_fx_level_report`. `out_trim_db` lets the user correct drift for other settings.
        const LEVEL_DB_AUTO: [(f32, f32); 6] =
            [(0.0, -1.0), (0.3, 4.64), (0.6, 8.03), (0.8, 7.97), (0.9, 7.72), (1.0, 7.27)];
        const LEVEL_DB_FIXED: [(f32, f32); 6] =
            [(0.0, -1.0), (0.3, 2.88), (0.6, 5.27), (0.8, 5.07), (0.9, 4.74), (1.0, 4.32)];
        let table = if p.auto_level { &LEVEL_DB_AUTO } else { &LEVEL_DB_FIXED };
        let mut meas = table[5].1;
        for w in table.windows(2) {
            if i <= w[1].0 {
                meas = w[0].1 + (w[1].1 - w[0].1) * (i - w[0].0) / (w[1].0 - w[0].0);
                break;
            }
        }
        self.out_gain = 0.45 * 10f32.powf((BASS_FX_TRIM_DB - meas) / 20.0) * 10f32.powf(p.out_trim_db / 20.0);
        let was_idle = self.is_idle();
        self.guard_on = p.loudness_guard;
        self.auto_on = p.auto_level;
        self.guard_ceiling = 10f32.powf((GUARD_CEILING_DB + p.bass_boost_db) / 20.0);
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
        self.guard_win = ms_to_coef(GUARD_WINDOW_MS);
        self.guard_att = ms_to_coef(GUARD_ATTACK_MS);
        self.guard_rel = ms_to_coef(GUARD_RELEASE_MS);
        self.lvl_att = ms_to_coef(AUTO_ATTACK_MS);
        self.lvl_rel = ms_to_coef(AUTO_RELEASE_MS);
        self.lvl_fc = ms_to_coef(AUTO_FAST_MS);
        self.lvl_cc = ms_to_coef(AUTO_CATCHUP_MS);
        if was_idle && enabled {
            self.reset_guard();
        }
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
            // Keep the input level follower running so switching the effect on starts at the right gain.
            if self.auto_on {
                for (xl, xr) in l.iter().zip(r.iter()) {
                    self.track_level(0.5 * (*xl * *xl + *xr * *xr));
                }
            }
            return;
        }
        let step = (1.0 / (BYPASS_RAMP_SECS * self.fs).max(1.0)) as f32;
        let norm = 1.0 / self.drive.tanh().max(1e-3);
        let kn = 0.8 / self.kick_drive.tanh().max(1e-3);
        let lfo_active = self.lfo_depth > 0.0 && (self.lfo_gain || (self.lfo_band && self.kick_on));
        for (xl, xr) in l.iter_mut().zip(r.iter_mut()) {
            // Input loudness for the level guard (dry input, before the mix is applied).
            let guard_x = 0.5 * (*xl * *xl + *xr * *xr);
            // Auto level: one scalar gain for both channels (stereo image kept), from the input
            // mean-square: pre-gain `a` takes the input to the reference level, `inv` = 1/a undoes it.
            let (a, inv) = if self.auto_on {
                self.track_level(guard_x);
                let inv = (self.lvl_ms.max(AUTO_REF_MS * AUTO_MIN_RATIO) / AUTO_REF_MS)
                    .sqrt()
                    .clamp(AUTO_MIN_INV, AUTO_MAX_INV);
                (1.0 / inv, inv)
            } else {
                (1.0, 1.0)
            };
            let (inl, inr) = if self.auto_on { (*xl * a, *xr * a) } else { (*xl, *xr) };
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
            let sl = self.res_l.process(self.shelf_l.process(inl));
            let sr = self.res_r.process(self.shelf_r.process(inr));
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
            if self.duck_floor_on {
                // Floor: same curve, but the dip stops at `duck_floor` (1 stays 1).
                g = self.duck_floor + (1.0 - self.duck_floor) * g;
            }
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
            if self.auto_on {
                // Back to the true level (wet part only; the dry pass-through below was never pre-gained).
                yl *= inv;
                yr *= inv;
            }
            // Mids/highs pass-through (Linkwitz-Riley split): `mid_pass` of the wet signal's band
            // above the crossover is swapped for the same amount of the DRY band there (the dry part
            // is added after the guard). With wet == dry the two bands sum to an all-pass.
            let (mut pl, mut pr) = (0.0f32, 0.0f32);
            if self.mid_on {
                let (wl, wr) = (lr4(&mut self.lp_wet_l, yl), lr4(&mut self.lp_wet_r, yr));
                // The pass-through pumps with the same duck gain (and rattle) as the processed signal.
                pl = self.mid_pass * g * lr4(&mut self.hp_dry_l, *xl);
                pr = self.mid_pass * g * lr4(&mut self.hp_dry_r, *xr);
                yl += self.mid_pass * (wl - yl);
                yr += self.mid_pass * (wr - yr);
            }
            if self.boost_on {
                // Clean bass boost after all distortion; soft limit keeps the wet signal bounded.
                yl = soft_limit(self.boost_l.process(yl));
                yr = soft_limit(self.boost_r.process(yr));
            }
            if self.guard_on {
                // Level guard: follow the mean-square of the input and of the wet signal with the same
                // window, and turn the wet signal down so its RMS never exceeds ceiling * input RMS.
                // Fast attack, slow release; the gain only ever attenuates. With the mids/highs
                // pass-through, only the processed part is turned down: the gain g solves
                // g^2 E_wet + 2 g C + E_pass = ceiling^2 E_in (C = wet/pass correlation), so the
                // pass-through is counted but never attenuated.
                let w = self.guard_win;
                self.guard_in += (guard_x - self.guard_in) * w;
                self.guard_out += (0.5 * (yl * yl + yr * yr) - self.guard_out) * w;
                let target = if self.guard_in > GUARD_FLOOR && self.guard_out > GUARD_FLOOR {
                    if self.mid_on {
                        self.guard_pass += (0.5 * (pl * pl + pr * pr) - self.guard_pass) * w;
                        self.guard_cross += (0.5 * (yl * pl + yr * pr) - self.guard_cross) * w;
                        let c = self.guard_pass - self.guard_ceiling * self.guard_ceiling * self.guard_in;
                        let (a, b) = (self.guard_out, self.guard_cross);
                        ((-b + (b * b - a * c).max(0.0).sqrt()) / a).clamp(0.0, 1.0)
                    } else {
                        (self.guard_ceiling * (self.guard_in / self.guard_out).sqrt()).min(1.0)
                    }
                } else {
                    1.0
                };
                let c = if target < self.guard_gain { self.guard_att } else { self.guard_rel };
                self.guard_gain += (target - self.guard_gain) * c;
                yl *= self.guard_gain;
                yr *= self.guard_gain;
            }
            if self.mid_on {
                // The dry band is added on top of the (bounded) wet signal: keep the sum bounded too.
                yl = soft_limit(yl + pl);
                yr = soft_limit(yr + pr);
            }
            *xl += (yl - *xl) * self.mix;
            *xr += (yr - *xr) * self.mix;
        }
    }

    fn crossover_sections(&mut self) -> impl Iterator<Item = &mut Biquad> {
        self.hp_dry_l.iter_mut().chain(self.hp_dry_r.iter_mut()).chain(self.lp_wet_l.iter_mut()).chain(self.lp_wet_r.iter_mut())
    }

    fn reset_crossover(&mut self) {
        self.crossover_sections().for_each(Biquad::reset);
    }

    /// Auto level: update the input mean-square followers with the instantaneous power `x`.
    #[inline]
    fn track_level(&mut self, x: f32) {
        self.lvl_fast += (x - self.lvl_fast) * self.lvl_fc;
        let c = if x > self.lvl_ms {
            self.lvl_att
        } else if self.lvl_ms > AUTO_DROP_RATIO * self.lvl_fast {
            self.lvl_cc
        } else {
            self.lvl_rel
        };
        self.lvl_ms += (x - self.lvl_ms) * c;
    }

    fn reset_level(&mut self) {
        self.lvl_ms = 0.0;
        self.lvl_fast = 0.0;
    }

    fn reset_guard(&mut self) {
        self.guard_in = 0.0;
        self.guard_out = 0.0;
        self.guard_gain = 1.0;
        self.guard_pass = 0.0;
        self.guard_cross = 0.0;
    }

    /// Clear filter, envelope, level-guard and LFO state.
    pub fn reset(&mut self) {
        self.reset_guard();
        self.reset_level();
        self.shelf_l.reset();
        self.shelf_r.reset();
        self.detector.reset();
        self.kick_l.reset();
        self.kick_r.reset();
        self.res_l.reset();
        self.res_r.reset();
        self.lp_l.reset();
        self.lp_r.reset();
        self.boost_l.reset();
        self.boost_r.reset();
        self.reset_crossover();
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

    /// Calibration tone: 55 Hz kick-ish + mid tones + highs, about -18 dBFS RMS.
    fn calibration_tone(n: usize, fs: f64) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let env = (-((t * 2.0) % 1.0) * 6.0).exp();
                (0.35 * env * (2.0 * PI * 55.0 * t).sin()
                    + 0.12 * (2.0 * PI * 440.0 * t).sin()
                    + 0.08 * (2.0 * PI * 2500.0 * t).sin()
                    + 0.04 * (2.0 * PI * 7000.0 * t).sin()) as f32
            })
            .collect()
    }

    /// "Mastered" version of `x` at `rms_db` dBFS RMS: gain, then a gentle `tanh` ceiling at 0.98
    /// (found by bisection), so hot levels have a realistic crest factor and peaks stay below 1.0.
    fn master_to(x: &[f32], rms_db: f64) -> Vec<f32> {
        let clip = |v: f32, g: f64| (0.98 * (f64::from(v) * g / 0.98).tanh()) as f32;
        let target = 10f64.powf(rms_db / 20.0);
        let (mut lo, mut hi) = (1e-3f64, 1e3f64);
        for _ in 0..60 {
            let g = (lo * hi).sqrt();
            let r = (x.iter().map(|v| f64::from(clip(*v, g)).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
            if r < target {
                lo = g;
            } else {
                hi = g;
            }
        }
        let g = (lo * hi).sqrt();
        x.iter().map(|v| clip(*v, g)).collect()
    }

    /// The music-like signals plus the calibration tone, "mastered" to `rms_db` dBFS RMS.
    fn all_level_signals(n: usize, fs: f64, rms_db: f64) -> Vec<(&'static str, Vec<f32>)> {
        let mut sigs = level_signals(n, fs, -18.0);
        sigs.push(("tone", calibration_tone(n, fs)));
        sigs.into_iter().map(|(name, x)| (name, master_to(&x, rms_db))).collect()
    }

    /// Mean (over the signals) JBL-vs-dry loudness in dB through a full `StereoEq`, second half of the
    /// render only (the level followers have settled).
    fn jbl_gain_db(fs: f64, pct: u8, p: &JblParams, sigs: &[(&'static str, Vec<f32>)]) -> f64 {
        let mut sum = 0.0;
        for (_, x) in sigs {
            let n = x.len();
            let (mut l, mut r) = (x.clone(), x.clone());
            let mut eq = StereoEq::new(fs);
            eq.set_limiter(true);
            eq.set_bass_fx(true, pct, p);
            eq.process_stereo(&mut l, &mut r);
            sum += 20.0 * (rms_of(&l[n / 2..]) / rms_of(&x[n / 2..])).log10();
        }
        sum / sigs.len() as f64
    }

    const TEST_LEVELS_DB: [f64; 6] = [-30.0, -24.0, -18.0, -12.0, -6.0, -3.0];

    /// Loudness report: JBL (auto level on, guard off) vs dry at 30/60/80/90/100 % for the calibration
    /// tone and three music-like signals at -30..-3 dBFS RMS. At every level the mean must sit near
    /// `BASS_FX_TRIM_DB` (re-calibrate `LEVEL_DB` from this printout when defaults change).
    #[test]
    fn bass_fx_level_report() {
        let fs = 48000.0;
        let n = 96000;
        for pct in [30u8, 60, 80, 90, 100] {
            let mut all = 0.0;
            for lv in TEST_LEVELS_DB {
                let sigs = all_level_signals(n, fs, lv);
                let p = JblParams { loudness_guard: false, ..JblParams::default() };
                let mean = jbl_gain_db(fs, pct, &p, &sigs);
                println!("LEVEL pct={pct} in={lv:.0} MEAN gain_db={mean:.2}");
                if std::env::var_os("LEVEL_CAPTURE").is_none() {
                    // At -3 dBFS RMS (a crushed master, peaks at 0.98) the soft limiters take a bit more.
                    let tol = if lv > -6.0 { 2.0 } else { 1.5 };
                    assert!((mean - f64::from(BASS_FX_TRIM_DB)).abs() < tol, "level {lv} at {pct}%: {mean:.2} dB vs dry");
                }
                all += mean;
            }
            println!("LEVEL pct={pct} ALL gain_db={:.2}", all / TEST_LEVELS_DB.len() as f64);
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

    /// Parameters the two golden tables (`GOLDEN_VALUES*`) were captured with: the plain pre-pass-through,
    /// pre-floor, pre-auto-level path (this guards the original signal path bit-for-bit), not the
    /// current Default.
    fn golden_params() -> JblParams {
        JblParams {
            duck_depth: 0.9,
            duck_floor_db: crate::config::DUCK_FLOOR_OFF_DB,
            drive_max: 6.5,
            kick_drive_max: 2.0,
            mid_pass: 0.0,
            out_trim_db: 0.0,
            auto_level: false,
            ..JblParams::default()
        }
    }

    /// Golden regression for the default BassFx sound (`GOLDEN_VALUES`, level guard on) and for the same
    /// sound with the guard switched off (`GOLDEN_VALUES_OFF`; the guard-off path is the pre-guard
    /// signal path, so it must stay bit-for-bit what it was). Re-captured when the defaults were
    /// retuned: Default duck depth 4.0 -> 0.9 (mids/highs ducked less), `LEVEL_DB` re-calibrated to land
    /// about 1 dB below dry (`BASS_FX_TRIM_DB` = -1), and the new level guard (on by default), which
    /// trims the wet signal wherever the effect would be louder than its input. They must not change
    /// from DSP refactors that keep the defaults.
    #[test]
    fn bass_fx_golden_default_output() {
        let fs = 48000.0;
        let n = 48000;
        let (x_l, x_r) = golden_signal(n, fs);
        // (pct, rms_l, rms_r, [l[1000], l[12000], l[30001], r[20000], r[47999]])
        for (guard, golden) in [(true, GOLDEN_VALUES), (false, GOLDEN_VALUES_OFF)] {
            // The guard only acts when the effect would be louder than its input, so the guard-on
            // capture feeds a quiet (-20 dB) version of the signal, where it is clearly engaged.
            let gain = if guard { 0.1 } else { 1.0 };
            for (pct, g_rms_l, g_rms_r, g_samples) in golden {
                let (mut l, mut r): (Vec<f32>, Vec<f32>) =
                    (x_l.iter().map(|v| v * gain).collect(), x_r.iter().map(|v| v * gain).collect());
                let mut eq = StereoEq::new(fs);
                eq.set_limiter(true);
                eq.set_bass_fx(true, pct, &JblParams { loudness_guard: guard, ..golden_params() });
                eq.process_stereo(&mut l, &mut r);
                let rms = |v: &[f32]| (v.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
                let got = [l[1000], l[12000], l[30001], r[20000], r[47999]];
                println!("GOLDEN guard={guard} pct={pct} rms_l={:.9} rms_r={:.9} samples={:?}", rms(&l), rms(&r), got);
                if std::env::var_os("GOLDEN_CAPTURE").is_some() {
                    continue; // print-only mode for re-capturing the tables below
                }
                assert!((rms(&l) - g_rms_l).abs() < 1e-6, "rms_l changed at {pct}% guard={guard}: {} vs {g_rms_l}", rms(&l));
                assert!((rms(&r) - g_rms_r).abs() < 1e-6, "rms_r changed at {pct}% guard={guard}: {} vs {g_rms_r}", rms(&r));
                for (a, b) in got.iter().zip(g_samples.iter()) {
                    assert!((a - b).abs() < 1e-5, "sample changed at {pct}% guard={guard}: {a} vs {b}");
                }
            }
        }
    }

    const GOLDEN_VALUES: [(u8, f64, f64, [f32; 5]); 4] = [
        (30, 0.025029625, 0.024738744, [0.0295918, -0.022795824, 0.029564697, -0.032025337, -0.015578992]),
        (60, 0.027380994, 0.027291761, [0.033338424, -0.021420917, 0.029934185, -0.037474297, -0.026813412]),
        (80, 0.030882675, 0.030860605, [0.03095889, -0.018173989, 0.025837451, -0.03944438, -0.03383317]),
        (100, 0.033654449, 0.033659080, [0.015548063, -0.013222201, 0.021554193, -0.035479497, -0.035110615]),
    ];
    const GOLDEN_VALUES_OFF: [(u8, f64, f64, [f32; 5]); 4] = [
        (30, 0.179586074, 0.179944428, [0.1739729, -0.17117979, 0.19096248, -0.23139955, -0.14926437]),
        (60, 0.126787082, 0.126777496, [0.11095706, -0.12877673, 0.115828216, -0.13728784, -0.14431871]),
        (80, 0.109297161, 0.109295564, [0.089102, -0.10711855, 0.08926545, -0.11647016, -0.12412149]),
        (100, 0.097723692, 0.097570002, [0.07368797, -0.09375, 0.072265625, -0.10546875, -0.11328125]),
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
        // The LFO is on by default, so the reference is the default set with the LFO switched off.
        let base = run_fx(48000.0, 80, &JblParams { lfo_on: false, ..JblParams::default() }, 24000);
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
        // One LFO period = fs/rate samples = 1/6 s = 33.3 windows (rounded to 33, so a steeper default drive needs slack): envelope repeats.
        let period = (fs / f64::from(rate) / win as f64).round() as usize;
        for k in 0..env.len() - period {
            assert!((env[k] - env[k + period]).abs() < 0.04, "not periodic at {k}: {} vs {}", env[k], env[k + period]);
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

    /// Every built-in JBL preset renders finite and within +-1 at all rates and intensities.
    #[test]
    fn jbl_presets_finite_and_bounded() {
        for pr in crate::config::jbl_presets() {
            for fs in [44100.0, 48000.0, 96000.0, 192000.0] {
                for pct in [0u8, 50, 100] {
                    let (l, r) = run_fx(fs, pct, &pr.params, 19200);
                    assert!(
                        l.iter().chain(r.iter()).all(|s| s.is_finite() && s.abs() <= 1.0),
                        "{}: non-finite/unbounded at fs={fs} pct={pct}",
                        pr.name
                    );
                }
            }
        }
    }

    fn rms_of(v: &[f32]) -> f64 {
        (v.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt()
    }

    fn render_mono(fs: f64, x: &[f32], pct: u8, p: &JblParams) -> Vec<f32> {
        let (mut l, mut r) = (x.to_vec(), x.to_vec());
        let mut eq = StereoEq::new(fs);
        eq.set_limiter(true);
        eq.set_bass_fx(true, pct, p);
        for (cl, cr) in l.chunks_mut(480).zip(r.chunks_mut(480)) {
            eq.process_stereo(cl, cr);
        }
        l
    }

    /// With the (opt-in) level guard on, JBL Bass Mode is never louder than its input, with or without auto level.
    /// Every preset and the default, at -30..-6 dBFS RMS programme level and 44.1/48/96 kHz, over the
    /// three music-like signals: RMS(out)/RMS(in) <= +0.5 dB (0 dB ceiling plus measurement slack).
    #[test]
    fn bass_fx_guard_never_louder_than_input() {
        let mut worst = f64::NEG_INFINITY;
        for fs in [44100.0, 48000.0, 96000.0] {
            let n = (fs * 3.0) as usize;
            let skip = (fs * 1.0) as usize;
            let base = level_signals(n, fs, -18.0);
            for pr in crate::config::jbl_presets() {
                for lv in [-30.0f64, -24.0, -18.0, -12.0, -6.0] {
                    let g = 10f64.powf((lv + 18.0) / 20.0) as f32;
                    for (name, x) in &base {
                        let x: Vec<f32> = x.iter().map(|v| v * g).collect();
                        for auto in [false, true] {
                            let p = JblParams { loudness_guard: true, auto_level: auto, ..pr.params };
                            let y = render_mono(fs, &x, pr.intensity, &p);
                            let db = 20.0 * (rms_of(&y[skip..]) / rms_of(&x[skip..])).log10();
                            worst = worst.max(db);
                            assert!(
                                db <= 0.5,
                                "{} {name} at {lv} dBFS, {fs} Hz, auto {auto}: out is {db:+.2} dB vs in",
                                pr.name
                            );
                        }
                    }
                }
            }
        }
        println!("GUARD worst out-in over all presets/levels/rates: {worst:+.2} dB");
    }

    /// Switching the guard off (with auto level off) gives the old (louder at low levels) behaviour, and the guard only
    /// ever attenuates: guard-on output RMS never exceeds guard-off output RMS.
    #[test]
    fn bass_fx_guard_off_is_louder_at_low_level_and_on_only_attenuates() {
        let fs = 48000.0;
        let n = 96000;
        let base = level_signals(n, fs, -30.0);
        let (x, skip) = (&base[0].1, 48000);
        let fixed = JblParams { auto_level: false, ..JblParams::default() };
        let on = render_mono(fs, x, 90, &JblParams { loudness_guard: true, ..fixed });
        let off = render_mono(fs, x, 90, &JblParams { loudness_guard: false, ..fixed });
        let (ron, roff) = (rms_of(&on[skip..]), rms_of(&off[skip..]));
        assert!(roff > 1.5 * rms_of(&x[skip..]), "unguarded effect should be well above dry at -30 dBFS");
        assert!(ron < 1.05 * rms_of(&x[skip..]) && ron < roff);
    }

    /// An explicit Bass boost raises the guard's ceiling by its dB amount (and no more).
    #[test]
    fn bass_fx_guard_ceiling_follows_bass_boost() {
        let fs = 48000.0;
        let n = 144000;
        let base = level_signals(n, fs, -30.0);
        let skip = 48000;
        for (name, x) in &base {
            let g = JblParams { loudness_guard: true, auto_level: false, ..JblParams::default() };
            let plain = render_mono(fs, x, 90, &g);
            let boosted = render_mono(fs, x, 90, &JblParams { bass_boost_db: 6.0, ..g });
            let (dp, db) = (
                20.0 * (rms_of(&plain[skip..]) / rms_of(&x[skip..])).log10(),
                20.0 * (rms_of(&boosted[skip..]) / rms_of(&x[skip..])).log10(),
            );
            assert!(dp <= 0.5, "{name}: {dp}");
            assert!(db > dp + 1.0, "{name}: boost should be audible past the plain ceiling ({db} vs {dp})");
            assert!(db <= 6.5, "{name}: boosted ceiling is 6 dB, got {db}");
        }
    }

    /// A level step (-36 -> -18 dBFS) is not left sticking out: from 50 ms after it the short-term (100 ms)
    /// out/in level never exceeds +3 dB, and the guard does not pump: its gain moves smoothly.
    #[test]
    fn bass_fx_guard_step_response_and_smooth_gain() {
        let fs = 48000.0;
        let seg = 96000;
        let quiet = level_signals(seg, fs, -36.0);
        let loud = level_signals(seg, fs, -18.0);
        for k in 0..3 {
            let x: Vec<f32> = quiet[k].1.iter().chain(loud[k].1.iter()).copied().collect();
            let mut fx = BassFx::new(fs);
            fx.set(true, 90, &JblParams { loudness_guard: true, auto_level: false, ..JblParams::default() });
            let (mut l, mut r) = (x.clone(), x.clone());
            let mut gains = Vec::with_capacity(l.len());
            for (a, b) in l.chunks_mut(1).zip(r.chunks_mut(1)) {
                fx.process(a, b);
                gains.push(fx.guard_gain);
            }
            let win = (fs * 0.1) as usize;
            let mut t = seg + win / 2;
            while t + win <= l.len() {
                let db = 20.0 * (rms_of(&l[t..t + win]) / rms_of(&x[t..t + win])).log10();
                assert!(db < 3.0, "{}: {db:+.2} dB over input {} ms after the step", quiet[k].0, (t - seg) * 1000 / 48000);
                t += win;
            }
            assert!(gains.iter().all(|g| g.is_finite() && *g > 0.0 && *g <= 1.0));
            let step = gains.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
            assert!(step < 2e-3, "{}: guard gain jumps by {step} in one sample", quiet[k].0);
        }
    }

    #[test]
    fn bass_fx_guard_silence_and_noise_floor() {
        for fs in [44100.0, 48000.0, 96000.0] {
            for amp in [0.0f32, 1e-9, 1e-6] {
                let n = (fs * 0.5) as usize;
                let x: Vec<f32> = (0..n).map(|i| amp * ((i as f32) * 0.37).sin()).collect();
                let y = render_mono(fs, &x, 90, &JblParams { loudness_guard: true, ..JblParams::default() });
                assert!(y.iter().all(|v| v.is_finite()), "non-finite at amp {amp}");
                if amp == 0.0 {
                    assert!(y.iter().all(|v| *v == 0.0), "silence in must be silence out");
                } else {
                    assert!(y.iter().all(|v| v.abs() < 1e-3), "tiny input must not blow up (amp {amp})");
                }
            }
        }
        // Loud material then silence: the tail decays to zero, no NaN from 0/0 in the guard.
        let fs = 48000.0;
        let mut x = level_signals(48000, fs, -18.0).remove(1).1;
        x.extend(std::iter::repeat_n(0.0f32, 96000));
        let y = render_mono(fs, &x, 90, &JblParams { loudness_guard: true, ..JblParams::default() });
        assert!(y.iter().all(|v| v.is_finite()));
        assert!(y[y.len() - 4800..].iter().all(|v| v.abs() < 1e-4));
    }

    #[test]
    fn bass_fx_guard_reset_and_sample_rate_change() {
        let mut fx = BassFx::new(48000.0);
        let guarded = JblParams { loudness_guard: true, auto_level: false, ..JblParams::default() };
        fx.set(true, 90, &guarded);
        let x = level_signals(48000, 48000.0, -30.0).remove(0).1;
        let (mut l, mut r) = (x.clone(), x);
        fx.process(&mut l, &mut r);
        assert!(fx.guard_in > 0.0 && fx.guard_out > 0.0 && fx.guard_gain < 1.0, "guard should be engaged at -30 dBFS");
        let gain = fx.guard_gain;
        fx.set_sample_rate(96000.0);
        assert_eq!(fx.guard_gain, gain, "state is kept across a rate change");
        let mut fresh = BassFx::new(96000.0);
        fresh.set(true, 90, &JblParams::default());
        assert_eq!((fx.guard_win, fx.guard_att, fx.guard_rel), (fresh.guard_win, fresh.guard_att, fresh.guard_rel));
        fx.reset();
        assert_eq!((fx.guard_in, fx.guard_out, fx.guard_gain), (0.0, 0.0, 1.0));
        // Parameter changes while running keep working and stay bounded.
        fx.set(true, 50, &JblParams { loudness_guard: false, ..guarded });
        fx.set(true, 50, &guarded);
        let (mut l, mut r) = (vec![0.05f32; 4800], vec![0.05f32; 4800]);
        fx.process(&mut l, &mut r);
        assert!(l.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
    }

    /// Each preset is loudness-matched to the default sound (guard off, auto level on): mean over the
    /// music-like signals at -30..-12 dBFS RMS (48 kHz, at the preset's own intensity) within 1 dB of
    /// the Default, and the Default itself near `BASS_FX_TRIM_DB` (just below dry) at every level
    /// including the hot ones (-6/-3 dBFS, where the soft limiters may take up to ~1.5 dB more).
    /// A DSP retune that breaks the matching must re-trim the presets' `out_trim_db`.
    #[test]
    fn jbl_presets_loudness_matched() {
        let fs = 48000.0;
        let n = 96000;
        let d_int = crate::config::Config::default().jbl_intensity;
        let mut sets = Vec::new();
        for lv in TEST_LEVELS_DB {
            sets.push((lv, all_level_signals(n, fs, lv)));
        }
        let reference: Vec<f64> = sets.iter().map(|(_, s)| jbl_gain_db(fs, d_int, &JblParams::default(), s)).collect();
        for ((lv, _), r) in sets.iter().zip(&reference) {
            println!("PRESET LEVEL Default in={lv:.0} {r:+.2} dB");
            let tol = if *lv > -6.0 { 2.0 } else { 1.5 };
            assert!((r - f64::from(BASS_FX_TRIM_DB)).abs() < tol, "default is {r:.2} dB vs dry at {lv} dBFS");
        }
        for pr in crate::config::jbl_presets() {
            let mut line = format!("PRESET LEVEL {:14}", pr.name);
            for ((lv, sigs), r) in sets.iter().zip(&reference) {
                let db = jbl_gain_db(fs, pr.intensity, &pr.params, sigs);
                line += &format!(" {lv:.0}:{db:+.2}");
                let tol = if *lv > -6.0 { 2.0 } else if *lv > -12.0 { 1.5 } else { 1.0 };
                if std::env::var_os("LEVEL_CAPTURE").is_none() {
                    assert!((db - r).abs() < tol, "{} is {:.2} dB off the default loudness at {lv} dBFS", pr.name, db - r);
                }
            }
            println!("{line}");
        }
    }

    // ---- Auto level (`JblParams::auto_level`): the effect follows the input loudness ----

    #[test]
    fn auto_level_constants_are_consistent() {
        let db = |x: f32| f64::from(x);
        assert!((db(AUTO_REF_MS) - 10f64.powf(db(AUTO_REF_DB) / 10.0)).abs() < 1e-8);
        assert!((db(AUTO_MIN_RATIO) - 10f64.powf(-db(AUTO_MAX_BOOST_DB) / 10.0)).abs() < 1e-8);
        assert!((db(AUTO_MIN_INV) - 10f64.powf(-db(AUTO_MAX_BOOST_DB) / 20.0)).abs() < 1e-6);
        assert!((db(AUTO_MAX_INV) - 10f64.powf(db(AUTO_MAX_CUT_DB) / 20.0)).abs() < 1e-4);
    }

    /// The headline requirement: with auto level on (guard off), every preset and the Default land
    /// about 1 dB below dry (+-1.5 dB; +-2 dB at the extreme -3 dBFS RMS, where the soft limiters act)
    /// for music-like input at -30..-3 dBFS RMS at 44.1/48/96 kHz. The old fixed-drive path fails this
    /// by up to 20 dB at the loud end.
    #[test]
    fn bass_fx_auto_level_loudness_matches_input_for_all_presets_levels_rates() {
        let mut sets: Vec<(&str, JblParams, u8)> = vec![("Default", JblParams::default(), 90)];
        for pr in crate::config::jbl_presets() {
            sets.push((pr.name, pr.params, pr.intensity));
        }
        for fs in [44100.0, 48000.0, 96000.0] {
            let n = (fs * 2.0) as usize;
            for lv in TEST_LEVELS_DB {
                let sigs: Vec<_> = all_level_signals(n, fs, lv).into_iter().take(3).collect();
                for (name, p, pct) in &sets {
                    let db = jbl_gain_db(fs, *pct, &JblParams { loudness_guard: false, auto_level: true, ..*p }, &sigs);
                    let tol = if lv > -6.0 { 2.0 } else { 1.5 };
                    assert!(
                        (db - f64::from(BASS_FX_TRIM_DB)).abs() < tol,
                        "{name} at {lv} dBFS, {fs} Hz: {db:+.2} dB vs dry"
                    );
                }
            }
        }
        // And the legacy path really does lose level when hot (what this test guards against).
        let fs = 48000.0;
        let sigs = all_level_signals(96000, fs, -12.0);
        let legacy = jbl_gain_db(fs, 90, &JblParams { auto_level: false, ..JblParams::default() }, &sigs);
        assert!(legacy < -4.0, "legacy fixed-drive path at -12 dBFS: {legacy:+.2} dB");
    }

    /// Amplitude-scale invariance: with auto level the whole wet path (clipper, kick saturation, duck,
    /// quantiser) sees the same drive at any input volume, so a quieter copy of the input comes out as
    /// the same waveform, scaled by the same factor.
    #[test]
    fn bass_fx_auto_level_output_scales_with_input() {
        let fs = 48000.0;
        let n = 96000;
        let loud = level_signals(n, fs, -15.0);
        for (name, x) in &loud {
            let y_loud = render_mono(fs, x, 90, &JblParams::default());
            for g_db in [-6.0f64, -12.0, -15.0] {
                let g = 10f64.powf(g_db / 20.0) as f32;
                let xq: Vec<f32> = x.iter().map(|v| v * g).collect();
                let y_q = render_mono(fs, &xq, 90, &JblParams::default());
                let err: Vec<f32> = y_q.iter().zip(&y_loud).map(|(a, b)| a / g - b).collect();
                let rel = rms_of(&err[n / 4..]) / rms_of(&y_loud[n / 4..]);
                assert!(rel < 0.02, "{name}: {g_db} dB copy differs from the scaled loud output by {rel:.4}");
            }
        }
        // The legacy path is clearly not scale invariant.
        let fixed = JblParams { auto_level: false, ..JblParams::default() };
        let y1 = render_mono(fs, &loud[1].1, 90, &fixed);
        let xq: Vec<f32> = loud[1].1.iter().map(|v| v * 0.25).collect();
        let y2 = render_mono(fs, &xq, 90, &fixed);
        let err: Vec<f32> = y2.iter().zip(&y1).map(|(a, b)| a / 0.25 - b).collect();
        assert!(rms_of(&err[n / 4..]) / rms_of(&y1[n / 4..]) > 0.3);
    }

    /// THD of a steady sine (RMS `rms_db`) through the effect: harmonics 2..=8 against the fundamental,
    /// over the last second (an integer number of cycles of `f`).
    fn tone_thd_db(fs: f64, f: f64, rms_db: f64, p: &JblParams) -> f64 {
        let amp = 10f64.powf(rms_db / 20.0) * 2f64.sqrt();
        let x = sine(fs, f, amp, (fs * 2.0) as usize);
        let y = render_mono(fs, &x, 90, p);
        let seg = &y[y.len() - fs as usize..];
        let bin = |k: f64| {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (i, v) in seg.iter().enumerate() {
                let ph = 2.0 * PI * k * f * i as f64 / fs;
                re += f64::from(*v) * ph.cos();
                im += f64::from(*v) * ph.sin();
            }
            re * re + im * im
        };
        let harm: f64 = (2..=8).map(|k| bin(f64::from(k))).sum();
        10.0 * (harm / bin(1.0)).log10()
    }

    /// The amount of distortion (THD of 60 Hz and 1 kHz tones) is the same at every input level.
    #[test]
    fn bass_fx_auto_level_distortion_is_level_independent() {
        let fs = 48000.0;
        for f in [60.0, 1000.0] {
            let thd = |auto: bool, lv: f64| tone_thd_db(fs, f, lv, &JblParams { auto_level: auto, ..JblParams::default() });
            let on: Vec<f64> = [-30.0, -24.0, -18.0, -12.0, -6.0].iter().map(|lv| thd(true, *lv)).collect();
            let off: Vec<f64> = [-30.0, -18.0, -6.0].iter().map(|lv| thd(false, *lv)).collect();
            let spread = |v: &[f64]| v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min);
            println!("THD {f} Hz auto on {on:.1?} off {off:.1?}");
            assert!(spread(&on) < 2.0, "THD at {f} Hz varies with level: {on:.1?}");
            if f == 60.0 {
                assert!(spread(&off) > 3.0, "legacy THD should vary with level: {off:.1?}");
            }
        }
    }

    /// Worst dip (dB) of a 2 kHz tone under 60 Hz bass bursts, against the same render without ducking,
    /// for input scaled by `g_db`.
    fn mid_dip_db(fs: f64, g_db: f64, p: &JblParams) -> f64 {
        let n = 48000;
        let g = 10f64.powf(g_db / 20.0);
        let x: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let burst = if (t % 0.5) < 0.1 { 0.5 } else { 0.0 };
                (g * (burst * (2.0 * PI * 60.0 * t).sin() + 0.1 * (2.0 * PI * 2000.0 * t).sin())) as f32
            })
            .collect();
        let hf = |p: &JblParams| {
            let y = render_mono(fs, &x, 90, p);
            let sp = FilterSpec { kind: FilterKind::HighPass, fc_hz: 1000.0, gain_db: 0.0, q: 0.707, enabled: true };
            let mut b = Biquad::from_spec(&sp, fs);
            y.iter().map(|v| b.process(*v)).collect::<Vec<f32>>()
        };
        let (y, reference) = (hf(p), hf(&JblParams { duck_on: false, ..*p }));
        let win = 240;
        (20..(n / win - 1))
            .map(|k| {
                let s = k * win;
                20.0 * (rms_of(&y[s..s + win]) / rms_of(&reference[s..s + win])).log10()
            })
            .fold(0.0, f64::min)
    }

    /// The Default's pump (mid dip on each bass hit) is the same at every input level.
    #[test]
    fn bass_fx_auto_level_pump_is_level_independent() {
        let fs = 48000.0;
        let dips: Vec<f64> =
            [-24.0, -18.0, -12.0, -6.0, 0.0].iter().map(|g| mid_dip_db(fs, *g, &JblParams::default())).collect();
        println!("DUCK mid dips by level: {dips:.1?}");
        let spread = dips.iter().cloned().fold(f64::MIN, f64::max) - dips.iter().cloned().fold(f64::MAX, f64::min);
        assert!(spread < 2.0, "pump depth varies with level: {dips:.1?}");
        assert!(dips.iter().all(|d| *d < -3.0), "the pump must be there at every level: {dips:.1?}");
    }

    /// A level step (either way, 18 dB) is smooth: no click (largest sample step stays within 1.7x of
    /// the same material at constant level), the level has settled to the constant-level result by
    /// 500 ms after the step, and the gain never overshoots (<= +3 dB) while it adapts.
    #[test]
    fn bass_fx_auto_level_step_response_is_smooth() {
        let fs = 48000.0;
        let seg = 96000;
        let (quiet, loud) = (level_signals(seg, fs, -36.0), level_signals(seg, fs, -18.0));
        let win = (fs * 0.05) as usize;
        for k in 0..3 {
            for up in [true, false] {
                let (a, b) = if up { (&quiet[k].1, &loud[k].1) } else { (&loud[k].1, &quiet[k].1) };
                let x: Vec<f32> = a.iter().chain(b.iter()).copied().collect();
                let y = render_mono(fs, &x, 90, &JblParams::default());
                // Constant-level reference of the second segment (the same material, same phase).
                let steady: Vec<f32> = b.iter().chain(b.iter()).copied().collect();
                let ys = render_mono(fs, &steady, 90, &JblParams::default());
                let ys = &ys[seg..];
                let max_step = |v: &[f32]| v.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
                // (Skip 5 ms: the concatenated test input itself jumps at the join.)
                let (m_step, m_ref) = (max_step(&y[seg + 240..]), max_step(&ys[240..]));
                assert!(m_step <= 1.7 * m_ref, "{} up={up}: click, step {m_step} vs {m_ref}", quiet[k].0);
                let mut t = 0;
                while t + win <= seg {
                    let d = 20.0 * (rms_of(&y[seg + t..seg + t + win]) / rms_of(&ys[t..t + win])).log10();
                    // A big volume cut leaves the wet path briefly under-driven (louder); bounded and short.
                    let limit = if t < (fs * 0.1) as usize { 16.0 } else if t < (fs * 0.25) as usize { 8.5 } else { 3.0 };
                    assert!(d < limit, "{} up={up}: overshoot {d:+.2} dB at {} ms", quiet[k].0, t * 1000 / 48000);
                    if t >= (fs * 0.5) as usize {
                        assert!(d.abs() < 1.0, "{} up={up}: not settled at {} ms ({d:+.2} dB)", quiet[k].0, t * 1000 / 48000);
                    }
                    t += win;
                }
            }
        }
    }

    /// The pre-gain / make-up factors themselves move smoothly (no zipper) through a level step, stay
    /// within their clamps, and silence stays silence with no NaN (also after loud material).
    #[test]
    fn bass_fx_auto_level_gain_is_smooth_bounded_and_silence_is_silent() {
        let fs = 48000.0;
        let mut fx = BassFx::new(fs);
        fx.set(true, 90, &JblParams::default());
        let seg = 48000;
        let loud = level_signals(seg, fs, -12.0).remove(2).1;
        let quiet: Vec<f32> = loud.iter().map(|v| v * 0.03).collect();
        let x: Vec<f32> = quiet.iter().chain(loud.iter()).chain(quiet.iter()).copied().collect();
        let mut invs = Vec::with_capacity(x.len());
        for v in &x {
            let (mut l, mut r) = ([*v], [*v]);
            fx.process(&mut l, &mut r);
            assert!(l[0].is_finite() && r[0].is_finite());
            invs.push((fx.lvl_ms.max(AUTO_REF_MS * AUTO_MIN_RATIO) / AUTO_REF_MS).sqrt().clamp(AUTO_MIN_INV, AUTO_MAX_INV));
        }
        let rel_step = invs.windows(2).map(|w| ((w[1] - w[0]) / w[0]).abs()).fold(0f32, f32::max);
        assert!(rel_step < 0.1, "level gain jumps by {rel_step} in one sample");
        assert!(invs.iter().all(|g| *g >= AUTO_MIN_INV && *g <= AUTO_MAX_INV));
        // Silence, and a loud passage followed by silence.
        for fs in [44100.0, 48000.0, 96000.0] {
            let n = (fs * 0.5) as usize;
            let y = render_mono(fs, &vec![0.0; n], 90, &JblParams::default());
            assert!(y.iter().all(|v| *v == 0.0), "silence in must be silence out");
            let mut x = level_signals(n, fs, -12.0).remove(1).1;
            x.extend(std::iter::repeat_n(0.0f32, (fs * 2.0) as usize));
            let y = render_mono(fs, &x, 90, &JblParams::default());
            assert!(y.iter().all(|v| v.is_finite()));
            assert!(y[y.len() - 4800..].iter().all(|v| v.abs() < 1e-4));
        }
    }

    /// Very quiet input is never blown up: the boost is capped at +24 dB and undone on the output.
    #[test]
    fn bass_fx_auto_level_very_quiet_input_is_bounded() {
        for fs in [44100.0, 48000.0, 96000.0] {
            for amp in [1e-9f32, 1e-7, 1e-5, 1e-3] {
                let n = (fs * 0.5) as usize;
                let x: Vec<f32> = (0..n).map(|i| amp * ((i as f32) * 0.37).sin()).collect();
                let y = render_mono(fs, &x, 90, &JblParams::default());
                assert!(y.iter().all(|v| v.is_finite() && v.abs() <= 10.0 * amp + 1e-9), "fs {fs} amp {amp}");
            }
        }
    }

    /// Switching the effect on mid-programme starts at the right gain (the level follower keeps
    /// running while the effect is idle): no dip in the first 100 ms after the mix ramp.
    #[test]
    fn bass_fx_auto_level_toggle_on_is_not_a_level_dip() {
        let fs = 48000.0;
        let x = level_signals(144000, fs, -12.0).remove(2).1;
        let mut fx = BassFx::new(fs);
        fx.set(false, 90, &JblParams::default());
        let (mut l, mut r) = (x.clone(), x.clone());
        let seg = 48000;
        fx.process(&mut l[..seg], &mut r[..seg]);
        fx.set(true, 90, &JblParams::default());
        fx.process(&mut l[seg..2 * seg], &mut r[seg..2 * seg]);
        let win = 12000;
        for k in 0..2 {
            let s = seg + 960 + k * win;
            let d = 20.0 * (rms_of(&l[s..s + win]) / rms_of(&x[s..s + win])).log10();
            assert!((-4.0..3.5).contains(&d), "{d:+.2} dB vs dry in window {k} after switching on");
        }
    }

    /// With auto level the output stays bounded and finite for every preset at every rate and level
    /// (including a hot, crushed master), and does not engage the soft limiters at normal levels.
    #[test]
    fn jbl_presets_finite_bounded_and_unlimited_across_levels() {
        for fs in [44100.0, 48000.0, 96000.0, 192000.0] {
            let n = (fs * 0.6) as usize;
            for lv in [-60.0, -30.0, -18.0, -6.0, -3.0, 0.0] {
                let sigs = all_level_signals(n, fs, lv);
                for pr in crate::config::jbl_presets() {
                    for (name, x) in sigs.iter().take(2) {
                        let y = render_mono(fs, x, pr.intensity, &pr.params);
                        assert!(y.iter().all(|s| s.is_finite() && s.abs() <= 1.0), "{} {name} {lv} dBFS {fs} Hz", pr.name);
                        if lv <= -18.0 {
                            let over = y.iter().filter(|s| s.abs() > LIMITER_KNEE).count();
                            assert_eq!(over, 0, "{} {name} at {lv} dBFS {fs} Hz: soft limiter engaged", pr.name);
                        }
                    }
                }
            }
        }
    }

    /// Golden for the auto-level Default (guard off) at two input levels. Changes only with deliberate
    /// DSP/default changes. Re-capture with GOLDEN_CAPTURE=1. (Auto level off is covered by the older goldens.)
    #[test]
    fn bass_fx_golden_default_auto_level() {
        let fs = 48000.0;
        let (x_l, x_r) = golden_signal(48000, fs);
        for (pct, gain, g_rms_l, g_rms_r, g_samples) in GOLDEN_AUTO {
            let (mut l, mut r): (Vec<f32>, Vec<f32>) =
                (x_l.iter().map(|v| v * gain).collect(), x_r.iter().map(|v| v * gain).collect());
            let mut eq = StereoEq::new(fs);
            eq.set_limiter(true);
            eq.set_bass_fx(true, pct, &JblParams::default());
            eq.process_stereo(&mut l, &mut r);
            let got = [l[1000], l[12000], l[30001], r[20000], r[47999]];
            println!("GOLDEN_AUTO ({pct}, {gain:?}, {:.9}, {:.9}, {got:?}),", rms_of(&l), rms_of(&r));
            if std::env::var_os("GOLDEN_CAPTURE").is_some() {
                continue;
            }
            assert!((rms_of(&l) - g_rms_l).abs() < 1e-6 && (rms_of(&r) - g_rms_r).abs() < 1e-6, "rms changed at {pct}% x{gain}");
            for (a, b) in got.iter().zip(g_samples.iter()) {
                assert!((a - b).abs() < 1e-5, "sample changed at {pct}% x{gain}: {a} vs {b}");
            }
        }
    }

    const GOLDEN_AUTO: [(u8, f32, f64, f64, [f32; 5]); 2] = [
        (90, 0.1, 0.020659782, 0.020653142, [0.0051437076, -0.013983352, 0.019596968, -0.02482602, -0.024389725]),
        (90, 1.0, 0.206288228, 0.206217674, [0.075743094, -0.13983417, 0.19596952, -0.2482607, -0.24389726]),
    ];

    // ---- Mids/highs pass-through (`JblParams::mid_pass`) and ducking floor (`duck_floor_db`) ----

    fn sine(fs: f64, f: f64, amp: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (amp * (2.0 * PI * f * i as f64 / fs).sin()) as f32).collect()
    }

    /// RMS (second half) of `x` through two cascaded 2nd-order Butterworth sections of the given kind.
    fn band_rms(fs: f64, x: &[f32], kind: FilterKind, fc: f64) -> f64 {
        let sp = FilterSpec { kind, fc_hz: fc, gain_db: 0.0, q: 0.707, enabled: true };
        let mut b = [Biquad::from_spec(&sp, fs); 2];
        let y: Vec<f32> = x.iter().map(|v| lr4(&mut b, *v)).collect();
        rms_of(&y[y.len() / 2..])
    }

    /// With mid_pass 0 the crossover is not in the signal path: any crossover frequency, and having
    /// had the pass-through on and off again, gives the identical output.
    #[test]
    fn bass_fx_mid_pass_zero_is_bit_identical() {
        let base = JblParams { mid_pass: 0.0, ..JblParams::default() };
        let a = run_fx(48000.0, 90, &base, 24000);
        assert_eq!(run_fx(48000.0, 90, &JblParams { mid_pass_hz: 150.0, ..base }, 24000), a);
        let mut fx = BassFx::new(48000.0);
        fx.set(true, 90, &JblParams { mid_pass: 0.8, ..base });
        fx.set(true, 90, &base);
        fx.reset();
        let (mut l, mut r) = golden_signal(24000, 48000.0);
        fx.process(&mut l, &mut r);
        assert_eq!((l, r), a);
        assert!(!fx.mid_on);
    }

    /// The Linkwitz-Riley pair (low pass on one branch, high pass on the other) sums flat: with
    /// wet == dry the pass-through is a pure all-pass, so no frequency is boosted or notched.
    #[test]
    fn mid_pass_crossover_sums_flat() {
        for fs in [44100.0, 48000.0, 96000.0] {
            for fc in [100.0, 400.0, 1000.0] {
                let (hp, lp) = (
                    FilterSpec { kind: FilterKind::HighPass, fc_hz: fc, gain_db: 0.0, q: 0.707, enabled: true },
                    FilterSpec { kind: FilterKind::LowPass, fc_hz: fc, gain_db: 0.0, q: 0.707, enabled: true },
                );
                for f in [50.0, 100.0, 200.0, 400.0, 800.0, 1600.0, 3200.0, 6400.0, 12800.0, 16000.0] {
                    if f > fs * 0.4 {
                        continue;
                    }
                    let n = (fs * 0.5) as usize;
                    let x = sine(fs, f, 0.5, n);
                    let (mut h, mut l) = ([Biquad::from_spec(&hp, fs); 2], [Biquad::from_spec(&lp, fs); 2]);
                    let y: Vec<f32> = x.iter().map(|v| lr4(&mut h, *v) + lr4(&mut l, *v)).collect();
                    let db = 20.0 * (rms_of(&y[n / 2..]) / rms_of(&x[n / 2..])).log10();
                    assert!(db.abs() < 1.5, "fs {fs} fc {fc} f {f}: sum is {db:+.2} dB");
                }
            }
        }
    }

    /// A tone above the crossover comes through (nearly) dry at mid_pass 1, a tone below it is unchanged
    /// by the pass-through, and on music the band above 1 kHz comes up clearly.
    #[test]
    fn bass_fx_mid_pass_lifts_mids_and_highs_not_bass() {
        let fs = 48000.0;
        let n = 96000;
        let off = JblParams { mid_pass: 0.0, loudness_guard: false, lfo_on: false, ..JblParams::default() };
        for mp in [0.5f32, 1.0] {
            let on = JblParams { mid_pass: mp, ..off };
            let level = |p: &JblParams, f: f64, amp: f64| {
                let x = sine(fs, f, amp, n);
                let y = render_mono(fs, &x, 90, p);
                20.0 * (rms_of(&y[n / 2..]) / rms_of(&x[n / 2..])).log10()
            };
            for f in [1500.0, 4000.0] {
                let (_d0, d1) = (level(&off, f, 0.05), level(&on, f, 0.05));
                if mp == 1.0 {
                    assert!(d1.abs() < 1.0, "{f} Hz should be about dry at mid_pass 1, got {d1:+.2} dB");
                }
            }
            let (b0, b1) = (level(&off, 60.0, 0.3), level(&on, 60.0, 0.3));
            assert!((b1 - b0).abs() < 1.0, "mp {mp}: 60 Hz moved from {b0:+.2} to {b1:+.2} dB");
        }
        // Music-like signal (ducker off so only the clipper/lo-fi squash is compared): the mid and high
        // band levels rise, the bass band stays.
        let off = JblParams { duck_on: false, ..off };
        let x = level_signals(144000, fs, -18.0).remove(1).1;
        let (a, b) = (render_mono(fs, &x, 90, &off), render_mono(fs, &x, 90, &JblParams { mid_pass: 0.9, ..off }));
        let band = |v: &[f32], k: FilterKind, f: f64| band_rms(fs, &v[48000..], k, f);
        let (hi_a, hi_b) = (band(&a, FilterKind::HighPass, 1000.0), band(&b, FilterKind::HighPass, 1000.0));
        let (lo_a, lo_b) = (band(&a, FilterKind::LowPass, 100.0), band(&b, FilterKind::LowPass, 100.0));
        let hi_up = 20.0 * (hi_b / hi_a).log10();
        let lo_move = 20.0 * (lo_b / lo_a).log10();
        println!("MID_PASS music: >1 kHz {hi_up:+.2} dB, <100 Hz {lo_move:+.2} dB");
        assert!(hi_up > 2.0, "mids/highs should come up clearly, got {hi_up:+.2} dB");
        assert!(lo_move.abs() < 1.5, "bass band should stay, moved {lo_move:+.2} dB");
    }

    /// The guard still holds out <= in (+0.5 dB slack) for every pass-through amount, crossover and
    /// ducking floor, and the guard-on result never exceeds the guard-off one.
    #[test]
    fn bass_fx_mid_pass_guard_never_louder_than_input() {
        for fs in [44100.0, 48000.0] {
            let n = (fs * 3.0) as usize;
            let skip = (fs * 1.0) as usize;
            let base = level_signals(n, fs, -18.0);
            for (mp, hz, floor) in [(0.3f32, 150.0f32, -9.0f32), (0.9, 400.0, -9.0), (1.0, 1000.0, -3.0), (1.0, 400.0, 0.0)] {
                for lv in [-30.0f64, -18.0, -6.0] {
                    let g = 10f64.powf((lv + 18.0) / 20.0) as f32;
                    for (name, x) in &base {
                        let x: Vec<f32> = x.iter().map(|v| v * g).collect();
                        let p = JblParams {
                            mid_pass: mp,
                            mid_pass_hz: hz,
                            duck_floor_db: floor,
                            loudness_guard: true,
                            auto_level: false,
                            ..JblParams::default()
                        };
                        let on = render_mono(fs, &x, 90, &p);
                        let off = render_mono(fs, &x, 90, &JblParams { loudness_guard: false, ..p });
                        let db = 20.0 * (rms_of(&on[skip..]) / rms_of(&x[skip..])).log10();
                        assert!(db <= 0.5, "{name} mp {mp} hz {hz} floor {floor} at {lv} dBFS, {fs} Hz: {db:+.2} dB vs in");
                        assert!(rms_of(&on[skip..]) <= rms_of(&off[skip..]) * 1.02, "{name}: guard must only attenuate");
                    }
                }
            }
        }
    }

    /// Toggling the effect on and off with the pass-through active is not a level jump: the output
    /// level shortly after switching on is within 3 dB of the dry level, there is no sample-to-sample
    /// step bigger than the signal's own, and off returns to the dry signal.
    #[test]
    fn bass_fx_mid_pass_toggle_is_not_a_level_jump() {
        let fs = 48000.0;
        let x = level_signals(144000, fs, -18.0).remove(2).1;
        let mut fx = BassFx::new(fs);
        fx.set(false, 90, &JblParams::default());
        let (mut l, mut r) = (x.clone(), x.clone());
        let seg = 48000;
        fx.process(&mut l[..seg], &mut r[..seg]);
        fx.set(true, 90, &JblParams::default());
        fx.process(&mut l[seg..2 * seg], &mut r[seg..2 * seg]);
        fx.set(false, 90, &JblParams::default());
        fx.process(&mut l[2 * seg..], &mut r[2 * seg..]);
        assert_eq!(&l[..seg], &x[..seg]);
        let win = 4800;
        let db = |a: &[f32], b: &[f32]| 20.0 * (rms_of(a) / rms_of(b)).log10();
        let on = db(&l[seg + 24000..seg + 24000 + win], &x[seg + 24000..seg + 24000 + win]);
        assert!(on.abs() < 4.0, "effect on is {on:+.2} dB vs dry");
        let step = |v: &[f32]| v.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
        assert!(step(&l) < 3.0 * step(&x) + 0.02, "click: {} vs dry {}", step(&l), step(&x));
        assert_eq!(&l[2 * seg + 4800..], &x[2 * seg + 4800..]);
    }

    /// Reset clears the crossover state and the guard's pass-through followers; a sample rate change
    /// retunes the crossover like a fresh effect (and keeps the state).
    #[test]
    fn bass_fx_mid_pass_reset_and_sample_rate_change() {
        let mut fx = BassFx::new(48000.0);
        fx.set(true, 90, &JblParams { loudness_guard: true, ..JblParams::default() });
        let x = level_signals(24000, 48000.0, -18.0).remove(2).1;
        let (mut l, mut r) = (x.clone(), x.clone());
        fx.process(&mut l, &mut r);
        assert!(fx.mid_on && fx.guard_pass > 0.0);
        fx.set_sample_rate(96000.0);
        assert!(fx.guard_pass > 0.0, "state kept across a rate change");
        let mut fresh = BassFx::new(96000.0);
        fresh.set(true, 90, &JblParams { loudness_guard: true, ..JblParams::default() });
        assert_eq!(fx.lp_wet_l[0].c, fresh.lp_wet_l[0].c);
        assert_eq!(fx.hp_dry_r[1].c, fresh.hp_dry_r[1].c);
        fx.reset();
        assert_eq!((fx.guard_pass, fx.guard_cross), (0.0, 0.0));
        assert_eq!((fx.hp_dry_l[0].z1, fx.lp_wet_r[1].z2), (0.0, 0.0));
        let (mut a, mut a2) = (vec![0.1f32; 960], vec![0.1f32; 960]);
        let (mut b, mut b2) = (a.clone(), a2.clone());
        fx.mix = 0.0;
        fx.process(&mut a, &mut a2);
        fresh.process(&mut b, &mut b2);
        assert_eq!(a, b);
    }

    #[test]
    fn bass_fx_mid_pass_extremes_finite_and_bounded() {
        for fs in [44100.0, 96000.0, 192000.0] {
            for (mp, hz) in [(1.0f32, 100.0f32), (1.0, 1000.0), (0.5, 1000.0), (f32::NAN, f32::NAN), (9.0, -5.0)] {
                for p in extreme_sets() {
                    for pct in [0u8, 50, 100] {
                        let (l, r) = run_fx(fs, pct, &JblParams { mid_pass: mp, mid_pass_hz: hz, ..p }, 9600);
                        assert!(
                            l.iter().chain(r.iter()).all(|s| s.is_finite() && s.abs() <= 1.0),
                            "fs {fs} mp {mp} hz {hz} pct {pct}"
                        );
                    }
                }
            }
        }
        for floor in [-100.0f32, -24.0, -9.0, 0.0, 5.0, f32::NAN] {
            for fs in [44100.0, 192000.0] {
                let (l, r) = run_fx(fs, 100, &JblParams { duck_floor_db: floor, ..JblParams::default() }, 9600);
                assert!(l.iter().chain(r.iter()).all(|s| s.is_finite() && s.abs() <= 1.0), "floor {floor} fs {fs}");
            }
        }
    }

    /// Ducking floor: -24 dB is the plain original curve (exactly), 0 dB is no ducking at all (exactly
    /// the same as the ducker switched off), and in between the dip is shallower the higher the floor
    /// while the timing (position of the deepest point) is unchanged.
    #[test]
    fn bass_fx_duck_floor_off_zero_and_depth() {
        let fs = 48000.0;
        let n = 96000;
        let base = JblParams {
            drive_max: 6.5,
            kick_drive_max: 2.0,
            out_trim_db: 1.5,
            mid_pass: 0.0,
            loudness_guard: false,
            auto_level: false,
            lfo_on: false,
            ..JblParams::default()
        };
        let plain = run_fx(fs, 90, &JblParams { duck_floor_db: crate::config::DUCK_FLOOR_OFF_DB, ..base }, 24000);
        let sub = run_fx(fs, 90, &JblParams { duck_floor_db: -23.99, ..base }, 24000);
        assert!(plain != sub, "any floor above the minimum is active");
        let no_duck = run_fx(fs, 90, &JblParams { duck_on: false, ..base }, 24000);
        assert_eq!(run_fx(fs, 90, &JblParams { duck_floor_db: 0.0, ..base }, 24000), no_duck);
        // Mid tone under 60 Hz bass bursts: ratio against the no-duck render per 5 ms window.
        let x: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let burst = if (t % 0.5) < 0.1 { 0.5 } else { 0.0 };
                (burst * (2.0 * PI * 60.0 * t).sin() + 0.1 * (2.0 * PI * 2000.0 * t).sin()) as f32
            })
            .collect();
        let hf = |p: &JblParams| {
            let y = render_mono(fs, &x, 90, p);
            let sp = FilterSpec { kind: FilterKind::HighPass, fc_hz: 1000.0, gain_db: 0.0, q: 0.707, enabled: true };
            let mut b = Biquad::from_spec(&sp, fs);
            y.iter().map(|v| b.process(*v)).collect::<Vec<f32>>()
        };
        let reference = hf(&JblParams { duck_on: false, ..base });
        let win = 240;
        let dip = |p: &JblParams| -> (f64, usize) {
            let y = hf(p);
            let mut best = (0.0f64, 0usize);
            for k in 20..(n / win - 1) {
                let s = k * win;
                let r = 20.0 * (rms_of(&y[s..s + win]) / rms_of(&reference[s..s + win])).log10();
                if r < best.0 {
                    best = (r, k % (24000 / win));
                }
            }
            best
        };
        let (d_off, t_off) = dip(&JblParams { duck_floor_db: crate::config::DUCK_FLOOR_OFF_DB, ..base });
        let (d_9, t_9) = dip(&JblParams { duck_floor_db: -9.0, ..base });
        let (d_4, _) = dip(&JblParams { duck_floor_db: -4.0, ..base });
        println!("DUCK floor dips: off {d_off:.1} dB, -9 {d_9:.1} dB, -4 {d_4:.1} dB (min at window {t_off}/{t_9})");
        assert!(d_off < d_9 - 1.0 && d_9 < d_4 - 1.0, "dip should shrink with a higher floor: {d_off} {d_9} {d_4}");
        assert!(d_9 > -9.0 - 1.5 && d_4 > -4.0 - 1.5, "dip should stay near the floor");
        assert!((t_off as i64 - t_9 as i64).abs() <= 3, "dip timing moved: {t_off} vs {t_9}");
    }

    /// Golden for the current Default (pass-through, floor, level guard on; quiet input where the guard
    /// engages). Changes only with deliberate DSP/default changes. Re-capture with GOLDEN_CAPTURE=1.
    #[test]
    fn bass_fx_golden_default_with_mid_pass() {
        let fs = 48000.0;
        let (x_l, x_r) = golden_signal(48000, fs);
        for (pct, g_rms_l, g_rms_r, g_samples) in GOLDEN_MID_PASS {
            let (mut l, mut r): (Vec<f32>, Vec<f32>) =
                (x_l.iter().map(|v| v * 0.1).collect(), x_r.iter().map(|v| v * 0.1).collect());
            let mut eq = StereoEq::new(fs);
            eq.set_limiter(true);
            eq.set_bass_fx(true, pct, &JblParams { loudness_guard: true, auto_level: false, ..JblParams::default() });
            eq.process_stereo(&mut l, &mut r);
            let got = [l[1000], l[12000], l[30001], r[20000], r[47999]];
            println!("GOLDEN_MID pct={pct} rms_l={:.9} rms_r={:.9} samples={got:?}", rms_of(&l), rms_of(&r));
            if std::env::var_os("GOLDEN_CAPTURE").is_some() {
                continue;
            }
            assert!((rms_of(&l) - g_rms_l).abs() < 1e-6 && (rms_of(&r) - g_rms_r).abs() < 1e-6, "rms changed at {pct}%");
            for (a, b) in got.iter().zip(g_samples.iter()) {
                assert!((a - b).abs() < 1e-5, "sample changed at {pct}%: {a} vs {b}");
            }
        }
    }

    const GOLDEN_MID_PASS: [(u8, f64, f64, [f32; 5]); 2] = [
        (30, 0.026002256, 0.025796345, [0.034590274, -0.016313348, 0.033009145, -0.034697354, -0.01750641]),
        (100, 0.032903435, 0.032894785, [-0.02811724, -0.0043451586, 0.022759866, -0.03262191, -0.03004234]),
    ];
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

#[cfg(test)]
mod bass_boost_tests {
    use super::*;

    /// Linear wet path (no distortion, pumping, lo-fi or rattle) so the boost shelf can be measured.
    fn clean(db: f32, hz: f32) -> JblParams {
        JblParams {
            duck_on: false,
            shelf_db: 0.0,
            res_on: false,
            drive_max: 1.0,
            bias_max: 0.0,
            kick_on: false,
            crush_on: false,
            hold_on: false,
            lpf_on: false,
            lfo_on: false,
            bass_boost_db: db,
            bass_boost_hz: hz,
            ..JblParams::default()
        }
    }

    fn run(fs: f64, p: &JblParams, intensity: u8, freq: f64, amp: f64, n: usize) -> Vec<f32> {
        let x: Vec<f32> = (0..n).map(|i| (amp * (2.0 * PI * freq * i as f64 / fs).sin()) as f32).collect();
        let (mut l, mut r) = (x.clone(), x);
        let mut fx = BassFx::new(fs);
        fx.set(true, intensity, p);
        fx.process(&mut l, &mut r);
        l
    }

    fn rms(v: &[f32]) -> f64 {
        (v.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
    }

    fn gain_db(freq: f64, boost: f32) -> f64 {
        let fs = 48000.0;
        let n = 48000;
        let a = run(fs, &clean(0.0, 80.0), 50, freq, 0.05, n);
        let b = run(fs, &clean(boost, 80.0), 50, freq, 0.05, n);
        20.0 * (rms(&b[n / 2..]) / rms(&a[n / 2..])).log10()
    }

    #[test]
    fn boost_zero_is_bit_identical() {
        let mut p = JblParams::default();
        let x: Vec<f32> = (0..20000).map(|i| (0.4 * (i as f32 * 0.013).sin() + 0.3 * (i as f32 * 0.31).sin()).clamp(-1.0, 1.0)).collect();
        let (mut a, mut a2) = (x.clone(), x.clone());
        let mut fx = BassFx::new(48000.0);
        fx.set(true, 90, &p);
        fx.process(&mut a, &mut a2);
        // Turn boost on and back off: must return to the same output as never having it.
        p.bass_boost_db = 6.0;
        let mut fx2 = BassFx::new(48000.0);
        fx2.set(true, 90, &p);
        p.bass_boost_db = 0.0;
        fx2.set(true, 90, &p);
        fx2.reset();
        let (mut b, mut b2) = (x.clone(), x);
        fx2.process(&mut b, &mut b2);
        assert_eq!(a, b);
    }

    #[test]
    fn boost_raises_lows_not_highs() {
        for db in [3.0f32, 6.0, 12.0] {
            let low = gain_db(60.0, db);
            let high = gain_db(2000.0, db);
            eprintln!("boost {db} dB: 60 Hz {low:.2} dB, 2 kHz {high:.2} dB");
            // The 80 Hz shelf is about 3/4 of full gain at 60 Hz and reaches the full gain well below.
            let deep = gain_db(30.0, db);
            eprintln!("  30 Hz {deep:.2} dB");
            assert!(low > 0.6 * f64::from(db) && low < f64::from(db) + 0.5, "60 Hz {low} vs {db}");
            assert!((deep - f64::from(db)).abs() < 1.5, "30 Hz {deep} vs {db}");
            assert!(high.abs() < 0.3, "2 kHz changed by {high}");
        }
    }

    #[test]
    fn boost_extreme_finite_and_bounded() {
        for fs in [44100.0, 48000.0, 96000.0, 192000.0] {
            for hz in [30.0f32, 80.0, 200.0] {
                for p in [clean(12.0, hz), JblParams { bass_boost_db: 12.0, bass_boost_hz: hz, ..JblParams::default() }] {
                    for f in [30.0, 60.0, 200.0, 1000.0] {
                        let y = run(fs, &p, 100, f, 1.0, fs as usize / 2);
                        assert!(y.iter().all(|s| s.is_finite() && s.abs() <= 1.0), "fs {fs} hz {hz} f {f}");
                    }
                }
            }
        }
    }

    #[test]
    fn boost_survives_sample_rate_change() {
        let mut fx = BassFx::new(44100.0);
        fx.set(true, 90, &JblParams { bass_boost_db: 12.0, ..JblParams::default() });
        for fs in [48000.0, 192000.0, 44100.0] {
            fx.set_sample_rate(fs);
            let (mut l, mut r) = (vec![0.9f32; 4000], vec![-0.9f32; 4000]);
            fx.process(&mut l, &mut r);
            assert!(l.iter().chain(r.iter()).all(|s| s.is_finite() && s.abs() <= 1.0));
        }
        assert!(fx.boost_on);
    }
}
