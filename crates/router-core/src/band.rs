//! Band-limited click detector DSP shared by `router-cli latency` (router-engine), `router-engine`'s probe
//! and `capture-probe wdm-baseline`.
//!
//! Pure (no COM): an 8th-order Butterworth high-pass followed by a 32-sample sliding RMS envelope, and the
//! onset-bias / envelope-gain references derived from a nominal click. Moved here from `wasapi-capture::band`
//! so `router-engine` needs no WASAPI dependency for its detection maths; `wasapi-capture` re-exports this
//! module, so both tools keep running exactly the same detector.

use crate::click::{click_samples, onset_index, ImpulseShape};

/// Q of the four sections of an 8th-order Butterworth filter.
const BUTTER8_Q: [f64; 4] = [0.509_795_6, 0.601_344_9, 0.899_976_2, 2.562_915_4];

/// Length (samples) of the sliding RMS window of the band-limited detector: a few-sample spike is averaged
/// down by about `10 log10(ENV_WINDOW)` dB while a click of a millisecond or two passes almost intact.
pub const ENV_WINDOW: usize = 32;

/// Stereo 8th-order Butterworth high-pass (four RBJ biquads, transposed direct form II, f64 state). No heap use,
/// so it lives in the audio callback. Used to look at a stream above the band where a microphone or other
/// programme material has energy, with a high-frequency click.
#[derive(Clone, Copy, Debug)]
pub struct HighPass {
    b: [[f64; 3]; 4],
    a: [[f64; 2]; 4],
    z: [[[f64; 2]; 4]; 2],
    ring: [[f64; ENV_WINDOW]; 2],
    rpos: [usize; 2],
    rsum: [f64; 2],
}

/// One sample through the four biquads (transposed direct form II).
#[inline]
fn sections(b: &[[f64; 3]; 4], a: &[[f64; 2]; 4], z: &mut [[f64; 2]; 4], x: f64) -> f64 {
    let mut s = x;
    for ((b, a), z) in b.iter().zip(a).zip(z.iter_mut()) {
        let y = b[0] * s + z[0];
        z[0] = b[1] * s - a[0] * y + z[1];
        z[1] = b[2] * s - a[1] * y;
        s = y;
    }
    s
}

impl HighPass {
    pub fn new(fc_hz: f32, rate: u32) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * f64::from(fc_hz).clamp(10.0, f64::from(rate) * 0.45) / f64::from(rate.max(1));
        let (cw, sw) = (w0.cos(), w0.sin());
        let mut hp = HighPass { b: [[0.0; 3]; 4], a: [[0.0; 2]; 4], z: [[[0.0; 2]; 4]; 2], ring: [[0.0; ENV_WINDOW]; 2], rpos: [0; 2], rsum: [0.0; 2] };
        for (k, q) in BUTTER8_Q.iter().enumerate() {
            let alpha = sw / (2.0 * q);
            let a0 = 1.0 + alpha;
            hp.b[k] = [(1.0 + cw) / 2.0 / a0, -(1.0 + cw) / a0, (1.0 + cw) / 2.0 / a0];
            hp.a[k] = [-2.0 * cw / a0, (1.0 - alpha) / a0];
        }
        hp
    }

    pub fn reset(&mut self) {
        self.z = [[[0.0; 2]; 4]; 2];
        self.ring = [[0.0; ENV_WINDOW]; 2];
        self.rpos = [0; 2];
        self.rsum = [0.0; 2];
    }

    /// High-pass followed by a sliding RMS over [`ENV_WINDOW`] samples (the band-limited detector's signal:
    /// non-negative, no DC to remove). `x` -> `out[..x.len()]`, channel `ch` 0 or 1.
    #[inline]
    pub fn process_env(&mut self, ch: usize, x: &[f32], out: &mut [f32]) {
        let c = ch & 1;
        let z = &mut self.z[c];
        for (o, &v) in out.iter_mut().zip(x) {
            let s = sections(&self.b, &self.a, z, f64::from(v));
            let sq = s * s;
            let pos = self.rpos[c];
            self.rsum[c] += sq - self.ring[c][pos];
            self.ring[c][pos] = sq;
            self.rpos[c] = (pos + 1) % ENV_WINDOW;
            *o = (self.rsum[c].max(0.0) / ENV_WINDOW as f64).sqrt() as f32;
        }
    }

    /// Filters `x` (channel `ch` 0 or 1) into `out[..x.len()]`.
    #[inline]
    pub fn process(&mut self, ch: usize, x: &[f32], out: &mut [f32]) {
        let z = &mut self.z[ch & 1];
        for (o, &v) in out.iter_mut().zip(x) {
            let s = sections(&self.b, &self.a, z, f64::from(v));
            *o = s as f32;
        }
    }
}

/// The detector's signal (high-pass then sliding RMS) for a nominal click, with a lead-in of silence and a tail:
/// the reference for the onset bias and the expected level of the band-limited detector.
pub fn click_envelope(click: &[f32], rate: u32, fc_hz: f32) -> Vec<f32> {
    let mut x = vec![0.0f32; 64];
    x.extend_from_slice(click);
    x.extend(std::iter::repeat_n(0.0, 256));
    let mut hp = HighPass::new(fc_hz, rate);
    let mut y = vec![0.0f32; x.len()];
    hp.process_env(0, &x, &mut y);
    y[64..].to_vec()
}

/// Corner of the detection high-pass for a click, if its band is high enough that a high-pass helps
/// (lowest significant frequency at or above 15 kHz: 0.9 times that), else `None`.
pub fn auto_highpass_hz(low_edge_hz: f32) -> Option<f32> {
    (low_edge_hz >= 15000.0).then(|| (low_edge_hz * 0.9).round())
}

/// Onset bias (frames at `rate`) of the band-limited detector (high-pass at `fc_hz`, then a sliding RMS): where its
/// envelope first reaches `threshold / peak` of its own peak for the nominal click, counted from the click's first
/// frame. Includes the filters' delay, so the detected offset minus this is the click's first frame.
pub fn band_onset_bias_frames(shape: ImpulseShape, rate: u32, fc_hz: f32, threshold: f32, peak: f32) -> f64 {
    if peak <= 0.0 {
        return 0.0;
    }
    let env = click_envelope(&click_samples(shape, rate, -40.0), rate, fc_hz);
    onset_index(&env, (threshold / peak).clamp(0.0, 1.0)) as f64
}

/// Peak of the band-limited detector's envelope relative to the click's peak (dB, negative).
pub fn band_gain_db(shape: ImpulseShape, rate: u32, fc_hz: f32) -> f64 {
    let click = click_samples(shape, rate, -40.0);
    let pk_in = click.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    let pk_env = click_envelope(&click, rate, fc_hz).iter().fold(0.0f32, |m, v| m.max(*v));
    if pk_in <= 0.0 || pk_env <= 0.0 {
        return -200.0;
    }
    20.0 * f64::from(pk_env / pk_in).log10()
}
