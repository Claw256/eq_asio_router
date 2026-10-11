//! Deterministic test signals (tone, multitone, log sweep, white noise) and a
//! sample-accurate reference-alignment checker.
//!
//! The checker answers "did the path between the renderer and the recording drop or
//! duplicate samples?" even for broadband signals, where a sine-fit glitch detector is weak:
//! the recording is aligned to a locally regenerated reference and walked sample by sample.
//! Pure code, no COM.

use std::f64::consts::PI;

/// Which signal to generate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SignalKind {
    /// Sine at the given frequency (Hz).
    Tone(f64),
    /// Eight inharmonic sines, peak-normalised.
    Multitone,
    /// Repeating 4 s logarithmic sweep 50 Hz .. 16 kHz.
    Sweep,
    /// Deterministic white noise (xorshift), uniform in +-1.
    Noise,
}

impl SignalKind {
    pub fn parse(name: &str, freq: f64) -> Option<Self> {
        match name {
            "tone" | "sine" => Some(SignalKind::Tone(freq)),
            "multitone" | "multi" => Some(SignalKind::Multitone),
            "sweep" => Some(SignalKind::Sweep),
            "noise" => Some(SignalKind::Noise),
            _ => None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            SignalKind::Tone(_) => "tone",
            SignalKind::Multitone => "multitone",
            SignalKind::Sweep => "sweep",
            SignalKind::Noise => "noise",
        }
    }
}

const MULTI_FREQS: [f64; 8] = [220.37, 484.11, 997.73, 1733.29, 3100.61, 5300.17, 8100.43, 11300.89];
const SWEEP_SECS: f64 = 4.0;
const SWEEP_F0: f64 = 50.0;
const SWEEP_F1: f64 = 16000.0;

/// Sample-indexed generator (so a reference can be regenerated bit-identically).
#[derive(Clone, Debug)]
pub struct SignalGen {
    kind: SignalKind,
    rate: f64,
    amp: f64,
    index: u64,
    rng: u64,
}

impl SignalGen {
    pub fn new(kind: SignalKind, rate: u32, dbfs: f32) -> Self {
        SignalGen {
            kind,
            rate: f64::from(rate),
            amp: 10f64.powf(f64::from(dbfs) / 20.0),
            index: 0,
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }

    pub fn amp(&self) -> f64 {
        self.amp
    }

    /// Next sample.
    pub fn next_sample(&mut self) -> f32 {
        let i = self.index;
        self.index += 1;
        let t = i as f64 / self.rate;
        let v = match self.kind {
            SignalKind::Tone(f) => (2.0 * PI * f * t).sin(),
            SignalKind::Multitone => {
                MULTI_FREQS.iter().enumerate().map(|(k, f)| (2.0 * PI * f * t + k as f64 * 0.7).sin()).sum::<f64>()
                    / MULTI_FREQS.len() as f64
            }
            SignalKind::Sweep => {
                let period = (SWEEP_SECS * self.rate).round() as u64;
                let tt = (i % period) as f64 / self.rate;
                let k = (SWEEP_F1 / SWEEP_F0).ln();
                let ph = 2.0 * PI * SWEEP_F0 * SWEEP_SECS / k * ((tt / SWEEP_SECS * k).exp() - 1.0);
                ph.sin()
            }
            SignalKind::Noise => {
                let mut x = self.rng;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                self.rng = x;
                ((x >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
            }
        };
        (v * self.amp) as f32
    }

    pub fn take(&mut self, n: usize) -> Vec<f32> {
        (0..n).map(|_| self.next_sample()).collect()
    }
}

/// Result of [`check_alignment`].
#[derive(Clone, Debug, Default)]
pub struct AlignReport {
    /// Reference was found in the recording.
    pub aligned: bool,
    /// Reference index of the first compared recording sample.
    pub start_offset: i64,
    /// Estimated gain recording/reference.
    pub gain: f64,
    pub compared: usize,
    pub mismatched: usize,
    /// (recording index, change of the reference offset): +n = n reference samples skipped
    /// (recording lost n samples), -n = n samples repeated.
    pub slips: Vec<(usize, i64)>,
    /// Runs of mismatching samples that did not resync to a different offset.
    pub corrupt_runs: usize,
    pub dropped_total: i64,
}

impl AlignReport {
    pub fn clean(&self) -> bool {
        self.aligned && self.slips.is_empty() && self.corrupt_runs == 0
    }
}

/// Aligns `rec` (mono) with `reference` (same rate, same generator) and walks both.
/// `skip` leading samples of `rec` are ignored (start-up); `tol` is absolute.
pub fn check_alignment(rec: &[f32], reference: &[f32], skip: usize, tol: f64) -> AlignReport {
    let mut rep = AlignReport::default();
    let w = 16usize;
    if rec.len() < skip + 4096 || reference.len() < 4096 {
        return rep;
    }
    // Anchor: window in the middle of the useful recording.
    let a = skip + (rec.len() - skip) / 4;
    let win = &rec[a..a + 2048];
    let mut best: Option<(f64, i64)> = None;
    for off in 0..reference.len().saturating_sub(2048) {
        // cheap prefilter on the first w samples (gain assumed within 0.6..1.5)
        let mut ok = true;
        for k in 0..w {
            let r = f64::from(reference[off + k]);
            if (f64::from(win[k]) - r).abs() > tol * 6.0 + 0.4 * r.abs() {
                ok = false;
                break;
            }
        }
        if !ok {
            continue;
        }
        let (mut sxy, mut sxx, mut syy) = (0.0f64, 0.0f64, 0.0f64);
        for k in 0..2048 {
            let (x, r) = (f64::from(win[k]), f64::from(reference[off + k]));
            sxy += r * x;
            sxx += r * r;
            syy += x * x;
        }
        let g = if sxx > 0.0 { sxy / sxx } else { 1.0 };
        let e = ((syy - g * sxy).max(0.0) / 2048.0).sqrt();
        if best.is_none_or(|b| e < b.0) {
            best = Some((e, off as i64 - a as i64));
        }
    }
    let Some((e, d0)) = best else { return rep };
    if e > tol * 3.0 {
        return rep;
    }
    rep.aligned = true;
    rep.start_offset = d0 + skip as i64;
    // gain over the anchor window
    let (mut sxy, mut sxx) = (0.0f64, 0.0f64);
    for k in 0..2048 {
        let r = f64::from(reference[(a as i64 + d0) as usize + k]);
        sxy += r * f64::from(win[k]);
        sxx += r * r;
    }
    rep.gain = if sxx > 0.0 { sxy / sxx } else { 1.0 };
    let g = rep.gain;
    let expect = |k: usize, d: i64| -> Option<f64> {
        let j = k as i64 + d;
        (j >= 0 && (j as usize) < reference.len()).then(|| f64::from(reference[j as usize]) * g)
    };
    let matches = |k: usize, d: i64, n: usize| -> bool {
        (0..n).all(|q| k + q < rec.len() && expect(k + q, d).is_some_and(|x| (f64::from(rec[k + q]) - x).abs() <= tol))
    };
    // Walk forward from the anchor, and backward is skipped (start-up region is not interesting).
    let mut d = d0;
    let mut k = a;
    let mut in_bad = false;
    let mut good = 0usize;
    while k < rec.len() {
        let Some(x) = expect(k, d) else { break };
        rep.compared += 1;
        if (f64::from(rec[k]) - x).abs() <= tol {
            good += 1;
            if good >= 16 {
                in_bad = false;
            }
            k += 1;
            continue;
        }
        good = 0;
        rep.mismatched += 1;
        // try to resync at a nearby offset: next w samples must match
        let mut found = None;
        for delta in 1..=256i64 {
            for dd in [delta, -delta] {
                if matches(k, d + dd, w) {
                    found = Some(dd);
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        match found {
            Some(dd) => {
                rep.slips.push((k, dd));
                rep.dropped_total += dd;
                d += dd;
                in_bad = false;
                good = 0;
            }
            None => {
                if !in_bad {
                    rep.corrupt_runs += 1;
                    in_bad = true;
                }
            }
        }
        k += 1;
    }
    rep
}

/// Mean-period frequency estimate (Hz) from rising zero crossings of `x`.
pub fn crossing_freq(x: &[f32], rate: f64) -> Option<f64> {
    let mean = x.iter().map(|&v| f64::from(v)).sum::<f64>() / x.len().max(1) as f64;
    let mut first = None;
    let (mut last, mut n) = (0.0f64, 0usize);
    for i in 1..x.len() {
        let (a, b) = (f64::from(x[i - 1]) - mean, f64::from(x[i]) - mean);
        if a < 0.0 && b >= 0.0 {
            let t = (i - 1) as f64 + (-a) / (b - a);
            first.get_or_insert(t);
            last = t;
            n += 1;
        }
    }
    let first = first?;
    (n >= 3 && last > first).then(|| rate * (n - 1) as f64 / (last - first))
}

/// Reads a 32-bit float WAV (as written by `capture-probe record`); returns (rate, channels).
pub fn read_f32_wav(bytes: &[u8]) -> Option<(u32, Vec<Vec<f32>>)> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12;
    let (mut rate, mut ch, mut bits, mut tag) = (0u32, 0usize, 0u16, 0u16);
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().ok()?) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            tag = u16::from_le_bytes(bytes[body..body + 2].try_into().ok()?);
            ch = u16::from_le_bytes(bytes[body + 2..body + 4].try_into().ok()?) as usize;
            rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().ok()?);
            bits = u16::from_le_bytes(bytes[body + 14..body + 16].try_into().ok()?);
        } else if id == b"data" {
            if tag != 3 || bits != 32 || ch == 0 {
                return None;
            }
            let end = (body + len).min(bytes.len());
            let n = (end - body) / (4 * ch);
            let mut out = vec![Vec::with_capacity(n); ch];
            for i in 0..n {
                for (c, o) in out.iter_mut().enumerate() {
                    let p = body + (i * ch + c) * 4;
                    o.push(f32::from_le_bytes(bytes[p..p + 4].try_into().ok()?));
                }
            }
            return Some((rate, out));
        }
        pos = body + len + (len & 1);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen(kind: SignalKind, n: usize) -> Vec<f32> {
        SignalGen::new(kind, 48000, -30.0).take(n)
    }

    #[test]
    fn generators_are_deterministic_and_quiet() {
        for k in [SignalKind::Tone(1000.0), SignalKind::Multitone, SignalKind::Sweep, SignalKind::Noise] {
            let a = gen(k, 20000);
            let b = gen(k, 20000);
            assert_eq!(a, b);
            let pk = a.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(pk <= 0.0317 * 1.01 && pk > 0.005, "{k:?} peak {pk}");
        }
    }

    #[test]
    fn clean_copy_is_clean_with_offset_and_gain() {
        for k in [SignalKind::Tone(1000.0), SignalKind::Multitone, SignalKind::Sweep, SignalKind::Noise] {
            let r = gen(k, 96000);
            let rec: Vec<f32> = r[777..].iter().map(|v| v * 0.5).collect();
            let rep = check_alignment(&rec, &r, 100, 0.002);
            assert!(rep.aligned && rep.clean(), "{k:?} {rep:?}");
            assert!((rep.gain - 0.5).abs() < 0.01);
            if !matches!(k, SignalKind::Tone(_)) {
                assert_eq!(rep.start_offset, 777 + 100);
            }
        }
    }

    #[test]
    fn dropped_and_duplicated_samples_are_found() {
        for k in [SignalKind::Multitone, SignalKind::Noise, SignalKind::Sweep] {
            let r = gen(k, 96000);
            let mut rec: Vec<f32> = r[300..].to_vec();
            rec.remove(30000);
            rec.remove(30000); // drop 2
            let dup = rec[60000];
            rec.insert(60000, dup); // repeat 1
            let rep = check_alignment(&rec, &r, 100, 0.0002);
            assert!(rep.aligned, "{k:?}");
            let total: i64 = rep.slips.iter().map(|s| s.1).sum();
            assert_eq!(total, 1, "{k:?} {rep:?}"); // +2 -1
            assert_eq!(rep.slips.len(), 2, "{k:?} {rep:?}");
            assert!(rep.slips.iter().any(|s| s.1 == 2 && s.0.abs_diff(30000) < 4));
        }
    }

    #[test]
    fn zero_gap_is_corrupt_run() {
        let r = gen(SignalKind::Noise, 96000);
        let mut rec = r.clone();
        for v in &mut rec[40000..40100] {
            *v = 0.0;
        }
        let rep = check_alignment(&rec, &r, 100, 0.002);
        assert!(rep.aligned && rep.corrupt_runs == 1 && rep.slips.is_empty(), "{rep:?}");
    }

    #[test]
    fn unrelated_signal_is_not_aligned() {
        let r = gen(SignalKind::Noise, 50000);
        let rec = gen(SignalKind::Multitone, 50000);
        assert!(!check_alignment(&rec, &r, 100, 0.002).aligned);
    }

    #[test]
    fn crossing_freq_measures_tone() {
        let x = gen(SignalKind::Tone(1000.0), 48000);
        assert!((crossing_freq(&x, 48000.0).unwrap() - 1000.0).abs() < 0.5);
        // same samples interpreted at 52 kHz play 8.3 % faster
        assert!((crossing_freq(&x, 52000.0).unwrap() / 1000.0 - 52.0 / 48.0).abs() < 1e-3);
    }

    #[test]
    fn wav_roundtrip() {
        let l = [0.1f32, 0.2, 0.3];
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36u32 + 24).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&3u16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&48000u32.to_le_bytes());
        b.extend_from_slice(&(48000u32 * 8).to_le_bytes());
        b.extend_from_slice(&8u16.to_le_bytes());
        b.extend_from_slice(&32u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&24u32.to_le_bytes());
        for v in l {
            b.extend_from_slice(&v.to_le_bytes());
            b.extend_from_slice(&(-v).to_le_bytes());
        }
        let (rate, ch) = read_f32_wav(&b).unwrap();
        assert_eq!(rate, 48000);
        assert_eq!(ch[0], l);
        assert_eq!(ch[1], [-0.1, -0.2, -0.3]);
    }
}
