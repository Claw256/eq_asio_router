//! Pure click/impulse signal generation shared by `router-cli latency`, `router-engine`'s
//! probe maths and `capture-probe` (WASAPI side). Moved here from `wasapi-capture::impulse`
//! so `router-engine` needs no WASAPI dependency for its detection maths; `wasapi-capture`
//! re-exports everything from this module, so existing paths keep working.

use std::f64::consts::PI;

/// Shape of one injected impulse.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ImpulseShape {
    /// Raised-cosine windowed burst of a sine (default 2 ms of 1.5 kHz: survives EQ and limiter).
    Burst { freq_hz: f32, dur_ms: f32 },
    /// Raised-cosine windowed linear chirp.
    Chirp { f0_hz: f32, f1_hz: f32, dur_ms: f32 },
}

impl Default for ImpulseShape {
    fn default() -> Self {
        ImpulseShape::Burst { freq_hz: 1500.0, dur_ms: 2.0 }
    }
}

impl ImpulseShape {
    /// Parses "burst", "burst:FREQ[:MS]", "chirp", "chirp:F0:F1[:MS]".
    pub fn parse(s: &str) -> Option<Self> {
        let mut it = s.split(':');
        match it.next()? {
            "burst" | "click" => {
                let f: f32 = it.next().map(str::parse).transpose().ok()?.unwrap_or(1500.0);
                let d: f32 = it.next().map(str::parse).transpose().ok()?.unwrap_or(2.0);
                (f > 50.0 && f < 20000.0 && d > 0.2 && d <= 20.0).then_some(ImpulseShape::Burst { freq_hz: f, dur_ms: d })
            }
            "chirp" => {
                let f0: f32 = it.next().map(str::parse).transpose().ok()?.unwrap_or(800.0);
                let f1: f32 = it.next().map(str::parse).transpose().ok()?.unwrap_or(4000.0);
                let d: f32 = it.next().map(str::parse).transpose().ok()?.unwrap_or(4.0);
                (f0 > 50.0 && f1 > 50.0 && f0 < 20000.0 && f1 < 20000.0 && d > 0.5 && d <= 40.0)
                    .then_some(ImpulseShape::Chirp { f0_hz: f0, f1_hz: f1, dur_ms: d })
            }
            _ => None,
        }
    }

    /// Duration in milliseconds.
    pub fn dur_ms(&self) -> f32 {
        match *self {
            ImpulseShape::Burst { dur_ms, .. } | ImpulseShape::Chirp { dur_ms, .. } => dur_ms,
        }
    }
}

/// One click, mono, peak-normalised to exactly `peak_dbfs`. The first sample is 0 (the window starts at 0).
pub fn click_samples(shape: ImpulseShape, rate: u32, peak_dbfs: f32) -> Vec<f32> {
    let rate_f = f64::from(rate.max(8000));
    let dur_s = f64::from(shape.dur_ms()) / 1000.0;
    let n = ((dur_s * rate_f).round() as usize).max(8);
    let mut v: Vec<f64> = (0..n)
        .map(|i| {
            let window = 0.5 * (1.0 - (2.0 * PI * i as f64 / n as f64).cos());
            let t = i as f64 / rate_f;
            let carrier = match shape {
                ImpulseShape::Burst { freq_hz, .. } => (2.0 * PI * f64::from(freq_hz) * t).sin(),
                ImpulseShape::Chirp { f0_hz, f1_hz, .. } => {
                    let (f0, f1) = (f64::from(f0_hz), f64::from(f1_hz));
                    (2.0 * PI * (f0 * t + (f1 - f0) * t * t / (2.0 * dur_s))).sin()
                }
            };
            window * carrier
        })
        .collect();
    let peak = v.iter().fold(0.0f64, |m, x| m.max(x.abs())).max(1e-12);
    let amp = 10f64.powf(f64::from(peak_dbfs) / 20.0);
    for x in v.iter_mut() {
        *x *= amp / peak;
    }
    v.into_iter().map(|x| x as f32).collect()
}

/// Index of the first sample whose magnitude reaches `ratio` times the click's peak.
///
/// A threshold detector on the received signal fires this many samples after the click's first frame;
/// subtracting it from the detected position references the measurement to the click's frame 0.
pub fn onset_index(click: &[f32], ratio: f32) -> usize {
    let peak = click.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if peak <= 0.0 {
        return 0;
    }
    let thr = ratio.clamp(0.0, 1.0) * peak;
    click.iter().position(|x| x.abs() >= thr).unwrap_or(0)
}
