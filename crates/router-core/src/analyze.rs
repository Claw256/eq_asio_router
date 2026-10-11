//! Offline signal-integrity analysis of recorded streams (test-tone and music-safe stats).
//!
//! * [`analyze_sine`]: for a recorded sine, a per-segment least-squares fit (amplitude,
//!   phase, DC and *frequency*, because the rate-matcher's drift trim legitimately
//!   changes the tone frequency by a few ppm over time) is subtracted. The residual gives
//!   THD+N and "glitches" (residual spikes above `spike_mads` x the residual median
//!   absolute deviation). A separate demodulating phase tracker finds phase steps
//!   (dropped / duplicated samples), which a short sine fit cannot show as spikes.
//! * [`generic_stats`]: signal-agnostic checks that also work on music: sample-to-sample
//!   jump outliers, clipping, duplicated-sample runs and zero runs.

use std::f64::consts::PI;

/// Parameters of [`analyze_sine`].
#[derive(Clone, Debug)]
pub struct SineParams {
    /// Expected tone frequency (Hz); the search covers +-1 % around it. 0 = estimate
    /// from the zero crossings of the analysed region.
    pub freq_hint: f64,
    /// Seconds ignored at the start (start-up fades) ...
    pub skip_s: f64,
    /// ... and at the end.
    pub tail_s: f64,
    /// Fit segment length in samples.
    pub seg: usize,
    /// Spike threshold in residual MADs.
    pub spike_mads: f64,
    /// Phase-step threshold in radians.
    pub phase_thr: f64,
    /// Spikes below this level relative to the fundamental are ignored (default -66 dB; the
    /// per-segment fit cannot model a continuously chirping tone better than about -70 dB).
    pub min_rel: f64,
}

impl Default for SineParams {
    fn default() -> Self {
        SineParams { freq_hint: 0.0, skip_s: 1.0, tail_s: 0.25, seg: 4096, spike_mads: 8.0, phase_thr: 0.03, min_rel: 5e-4 }
    }
}

/// One residual spike (clustered).
#[derive(Clone, Copy, Debug)]
pub struct Glitch {
    /// Sample index in the original stream.
    pub index: usize,
    pub time_s: f64,
    /// Peak residual relative to the fundamental amplitude.
    pub rel_amp: f64,
}

/// One phase step.
#[derive(Clone, Copy, Debug)]
pub struct PhaseStep {
    pub index: usize,
    pub time_s: f64,
    pub radians: f64,
}

/// Signal-agnostic statistics.
#[derive(Clone, Debug, Default)]
pub struct GenericStats {
    pub samples: usize,
    pub peak: f32,
    pub max_abs_diff: f32,
    /// Number of |x[n]-x[n-1]| above 8x MAD-scaled threshold of the difference signal
    /// (music has a heavy tail, so only large multiples are counted; see `jump_threshold`).
    pub jump_outliers: usize,
    pub jump_threshold: f32,
    /// |x| >= 0.999.
    pub clipped: usize,
    /// Runs (length >= 3) of identical non-zero samples, and the longest.
    pub flat_runs: usize,
    pub longest_flat: usize,
    /// Runs (length >= 8) of exact zeros, and the longest.
    pub zero_runs: usize,
    pub longest_zero: usize,
}

/// Result of [`analyze_sine`].
#[derive(Clone, Debug, Default)]
pub struct SineReport {
    pub rate: u32,
    pub analyzed_samples: usize,
    pub freq_mean: f64,
    pub freq_min: f64,
    pub freq_max: f64,
    /// Mean fundamental amplitude (peak).
    pub amp: f64,
    pub residual_rms_db: f64,
    /// Median-absolute-deviation based noise floor, dB rel. fundamental RMS.
    pub noise_mad_db: f64,
    pub spike_threshold_rel_db: f64,
    pub glitches: Vec<Glitch>,
    pub glitches_per_s: f64,
    pub phase_steps: Vec<PhaseStep>,
    pub max_jump: f64,
    pub expected_max_jump: f64,
    pub generic: GenericStats,
}

impl SineReport {
    /// True when neither spikes nor phase steps were found.
    pub fn clean(&self) -> bool {
        self.glitches.is_empty() && self.phase_steps.is_empty()
    }
}

fn db(x: f64) -> f64 {
    if x <= 1e-30 {
        -300.0
    } else {
        20.0 * x.log10()
    }
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

/// Sums needed for the 3-parameter fit at a fixed frequency.
struct Sums {
    n: f64,
    cc: f64,
    ss: f64,
    cs: f64,
    c: f64,
    s: f64,
    xc: f64,
    xs: f64,
    x: f64,
    xx: f64,
}

fn sums(x: &[f64], w: f64) -> Sums {
    let (sw, cw) = w.sin_cos();
    let (mut sn, mut cn) = (0.0f64, 1.0f64);
    let mut a = Sums { n: x.len() as f64, cc: 0.0, ss: 0.0, cs: 0.0, c: 0.0, s: 0.0, xc: 0.0, xs: 0.0, x: 0.0, xx: 0.0 };
    for (i, &v) in x.iter().enumerate() {
        a.cc += cn * cn;
        a.ss += sn * sn;
        a.cs += cn * sn;
        a.c += cn;
        a.s += sn;
        a.xc += v * cn;
        a.xs += v * sn;
        a.x += v;
        a.xx += v * v;
        // phasor rotation (renormalised periodically)
        let (nc, ns) = (cn * cw - sn * sw, sn * cw + cn * sw);
        cn = nc;
        sn = ns;
        if i & 1023 == 1023 {
            let m = (cn * cn + sn * sn).sqrt();
            cn /= m;
            sn /= m;
        }
    }
    a
}

/// Solves the 3x3 normal equations; returns (coef_cos, coef_sin, dc, residual energy).
fn solve(a: &Sums) -> (f64, f64, f64, f64) {
    let m = [[a.cc, a.cs, a.c], [a.cs, a.ss, a.s], [a.c, a.s, a.n]];
    let rhs = [a.xc, a.xs, a.x];
    let det = |m: &[[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let d = det(&m);
    if d.abs() < 1e-30 {
        return (0.0, 0.0, a.x / a.n.max(1.0), a.xx);
    }
    let mut out = [0.0; 3];
    for k in 0..3 {
        let mut mk = m;
        for r in 0..3 {
            mk[r][k] = rhs[r];
        }
        out[k] = det(&mk) / d;
    }
    let explained = out[0] * rhs[0] + out[1] * rhs[1] + out[2] * rhs[2];
    (out[0], out[1], out[2], (a.xx - explained).max(0.0))
}

/// Golden-section search for the frequency (Hz) minimising the fit residual in `[lo, hi]`.
fn best_freq(x: &[f64], rate: f64, lo: f64, hi: f64) -> f64 {
    let g = 0.618_033_988_749_894_9;
    let cost = |f: f64| solve(&sums(x, 2.0 * PI * f / rate)).3;
    let (mut a, mut b) = (lo, hi);
    let mut c = b - g * (b - a);
    let mut d = a + g * (b - a);
    let (mut fc, mut fd) = (cost(c), cost(d));
    for _ in 0..60 {
        if fc < fd {
            b = d;
            d = c;
            fd = fc;
            c = b - g * (b - a);
            fc = cost(c);
        } else {
            a = c;
            c = d;
            fc = fd;
            d = a + g * (b - a);
            fd = cost(d);
        }
        if (b - a) < 1e-9 * lo {
            break;
        }
    }
    0.5 * (a + b)
}

/// Analyses channel samples `x` (rate `rate`) holding a sine of about `p.freq_hint` Hz.
pub fn analyze_sine(x: &[f32], rate: u32, p: &SineParams) -> SineReport {
    let r = f64::from(rate);
    let start = ((p.skip_s * r) as usize).min(x.len());
    let end = x.len().saturating_sub((p.tail_s * r) as usize).max(start);
    let xs: Vec<f64> = x[start..end].iter().map(|&v| f64::from(v)).collect();
    let mut rep = SineReport { rate, analyzed_samples: xs.len(), ..SineReport::default() };
    let mut p = p.clone();
    if p.freq_hint <= 0.0 {
        p.freq_hint = crossing_freq(&xs, r).unwrap_or(1000.0);
    }
    let p = &p;
    rep.generic = generic_stats(&x[start..end]);
    if xs.len() < p.seg {
        return rep;
    }

    // Per-segment fit.
    let mut resid = vec![0.0f64; xs.len()];
    let mut freqs = Vec::new();
    let (mut amp_sum, mut amp_n) = (0.0f64, 0usize);
    let mut pos = 0usize;
    while pos < xs.len() {
        let mut stop = (pos + p.seg).min(xs.len());
        if xs.len() - stop < p.seg / 2 {
            stop = xs.len();
        }
        let seg = &xs[pos..stop];
        // Search +-1 % but never wider than about one frequency bin of the segment (the fit cost
        // has one lobe per bin; a wider window at high tone frequencies would hit side lobes).
        let hw = (0.01 * p.freq_hint).min(r / seg.len() as f64);
        // Robust fit: samples that are clear outliers (clicks) are replaced by the current fit
        // and the fit is repeated, so one click does not smear into the whole segment.
        let mut work = seg.to_vec();
        #[allow(unused_assignments)]
        let (mut f, mut cc, mut ss, mut dc) = (0.0, 0.0, 0.0, 0.0);
        let mut fitv = vec![0.0f64; seg.len()];
        for _ in 0..3 {
            f = best_freq(&work, r, p.freq_hint - hw, p.freq_hint + hw);
            let w = 2.0 * PI * f / r;
            let sol = solve(&sums(&work, w));
            cc = sol.0;
            ss = sol.1;
            dc = sol.2;
            let (sw, cw) = w.sin_cos();
            let (mut sn, mut cn) = (0.0f64, 1.0f64);
            for (i, fv) in fitv.iter_mut().enumerate() {
                *fv = cc * cn + ss * sn + dc;
                let (nc, ns) = (cn * cw - sn * sw, sn * cw + cn * sw);
                cn = nc;
                sn = ns;
                if i & 1023 == 1023 {
                    let m = (cn * cn + sn * sn).sqrt();
                    cn /= m;
                    sn /= m;
                }
            }
            let mut dev: Vec<f64> = seg.iter().zip(&fitv).map(|(a, b)| (a - b).abs()).collect();
            let mad = median(&mut dev);
            let thr = (8.0 * mad).max(p.min_rel * (cc * cc + ss * ss).sqrt());
            let mut changed = false;
            for i in 0..seg.len() {
                let out = (seg[i] - fitv[i]).abs() > thr;
                let new = if out { fitv[i] } else { seg[i] };
                if new != work[i] {
                    work[i] = new;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for (i, &v) in seg.iter().enumerate() {
            resid[pos + i] = v - fitv[i];
        }
        freqs.push(f);
        amp_sum += (cc * cc + ss * ss).sqrt();
        amp_n += 1;
        pos = stop;
    }
    let amp = amp_sum / amp_n.max(1) as f64;
    rep.amp = amp;
    rep.freq_mean = freqs.iter().sum::<f64>() / freqs.len() as f64;
    rep.freq_min = freqs.iter().cloned().fold(f64::INFINITY, f64::min);
    rep.freq_max = freqs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    let fund_rms = amp / 2f64.sqrt();
    let rms = (resid.iter().map(|v| v * v).sum::<f64>() / resid.len() as f64).sqrt();
    rep.residual_rms_db = db(rms / fund_rms.max(1e-30));

    let mut tmp = resid.clone();
    let med = median(&mut tmp);
    let mut dev: Vec<f64> = resid.iter().map(|v| (v - med).abs()).collect();
    let mad = median(&mut dev);
    rep.noise_mad_db = db(mad * 1.4826 / fund_rms.max(1e-30));
    let thr = (p.spike_mads * mad).max(p.min_rel * amp);
    rep.spike_threshold_rel_db = db(thr / amp.max(1e-30));

    // Cluster spikes closer than 16 samples.
    let mut last: Option<usize> = None;
    for (i, &v) in resid.iter().enumerate() {
        if (v - med).abs() > thr {
            match (last, rep.glitches.last_mut()) {
                (Some(l), Some(g)) if i - l <= 16 => {
                    if v.abs() / amp > g.rel_amp {
                        g.rel_amp = v.abs() / amp;
                        g.index = start + i;
                        g.time_s = (start + i) as f64 / r;
                    }
                }
                _ => rep.glitches.push(Glitch { index: start + i, time_s: (start + i) as f64 / r, rel_amp: v.abs() / amp }),
            }
            last = Some(i);
        }
    }
    rep.glitches_per_s = rep.glitches.len() as f64 / (xs.len() as f64 / r);

    rep.max_jump = f64::from(rep.generic.max_abs_diff);
    rep.expected_max_jump = amp * 2.0 * (PI * rep.freq_mean / r).sin();
    rep.phase_steps = phase_steps(&xs, r, rep.freq_mean, p.phase_thr, start);
    rep
}

/// Median-interval frequency estimate from interpolated rising zero crossings.
pub fn crossing_freq(x: &[f64], rate: f64) -> Option<f64> {
    let mean = x.iter().sum::<f64>() / x.len().max(1) as f64;
    let mut times = Vec::new();
    for i in 1..x.len() {
        let (a, b) = (x[i - 1] - mean, x[i] - mean);
        if a < 0.0 && b >= 0.0 {
            times.push((i - 1) as f64 + (-a) / (b - a));
        }
    }
    if times.len() < 3 {
        return None;
    }
    // Mean period from first to last crossing (interval medians would snap to whole samples
    // for tones with few samples per cycle).
    let m = (times[times.len() - 1] - times[0]) / (times.len() - 1) as f64;
    if m > 0.0 {
        Some(rate / m)
    } else {
        None
    }
}

/// Demodulating phase-step detector (see module docs).
fn phase_steps(x: &[f64], rate: f64, f0: f64, thr: f64, offset: usize) -> Vec<PhaseStep> {
    let n = x.len();
    // Window = whole number of cycles that is closest to an integer sample count.
    let cyc = rate / f0;
    let mut best = (f64::INFINITY, (2.0 * cyc).round() as usize);
    for m in 2..=12usize {
        let len = cyc * m as f64;
        let err = (len - len.round()).abs() / len;
        if err < best.0 * 0.999 {
            best = (err, len.round() as usize);
        }
        if err < 0.002 {
            best = (err, len.round() as usize);
            break;
        }
    }
    let l = best.1.max(4);
    if n < 8 * l + 2 {
        return Vec::new();
    }
    let w0 = 2.0 * PI * f0 / rate;
    // Prefix sums of the mixed-down signal.
    let mut pre_re = vec![0.0f64; n + 1];
    let mut pre_im = vec![0.0f64; n + 1];
    for i in 0..n {
        let ph = w0 * i as f64;
        let (s, c) = ph.sin_cos();
        pre_re[i + 1] = pre_re[i] + x[i] * c;
        pre_im[i + 1] = pre_im[i] - x[i] * s;
    }
    let m = n - l + 1;
    let mut phi = vec![0.0f64; m];
    let mut prev = 0.0f64;
    for i in 0..m {
        let re = pre_re[i + l] - pre_re[i];
        let im = pre_im[i + l] - pre_im[i];
        let mut a = im.atan2(re);
        while a - prev > PI {
            a -= 2.0 * PI;
        }
        while a - prev < -PI {
            a += 2.0 * PI;
        }
        phi[i] = a;
        prev = a;
    }
    let mut pp = vec![0.0f64; m + 1];
    for i in 0..m {
        pp[i + 1] = pp[i] + phi[i];
    }
    // M2[i] = mean(phi[i..i+l]) - mean(phi[i-l..i]).
    let mut t = vec![0.0f64; m];
    for i in l..m.saturating_sub(l) {
        t[i] = (pp[i + l] - pp[i]) / l as f64 - (pp[i] - pp[i - l]) / l as f64;
    }
    // Remove the slow trend (frequency offset gives a constant) with a long moving average.
    let wide = 3000usize.min(m / 4).max(l);
    let mut pt = vec![0.0f64; m + 1];
    for i in 0..m {
        pt[i + 1] = pt[i] + t[i];
    }
    let mut out: Vec<PhaseStep> = Vec::new();
    let mut i = 2 * l;
    while i + 2 * l < m {
        let lo = i.saturating_sub(wide);
        let hi = (i + wide).min(m);
        let trend = (pt[hi] - pt[lo]) / (hi - lo) as f64;
        let v = t[i] - trend;
        if v.abs() > thr {
            // local maximum of |v| within +-l
            let mut best_i = i;
            let mut best_v = v;
            let mut j = i;
            while j < (i + 2 * l).min(m - 1) {
                let lo2 = j.saturating_sub(wide);
                let hi2 = (j + wide).min(m);
                let tr = (pt[hi2] - pt[lo2]) / (hi2 - lo2) as f64;
                let vv = t[j] - tr;
                if vv.abs() > best_v.abs() {
                    best_v = vv;
                    best_i = j;
                }
                j += 1;
            }
            let idx = best_i + l / 2 + offset;
            out.push(PhaseStep { index: idx, time_s: idx as f64 / rate, radians: best_v });
            i = best_i + 2 * l;
        } else {
            i += 1;
        }
    }
    out
}

/// Signal-agnostic checks (work on music too).
pub fn generic_stats(x: &[f32]) -> GenericStats {
    let mut g = GenericStats { samples: x.len(), ..GenericStats::default() };
    if x.is_empty() {
        return g;
    }
    g.peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    g.clipped = x.iter().filter(|v| v.abs() >= 0.999).count();
    let diffs: Vec<f64> = x.windows(2).map(|w| f64::from((w[1] - w[0]).abs())).collect();
    g.max_abs_diff = diffs.iter().cloned().fold(0.0, f64::max) as f32;
    if !diffs.is_empty() {
        let mut d = diffs.clone();
        let mad = median(&mut d); // median of |diff| (centre ~0)
        // Heavy-tailed for music: use a generous multiple of the *median* |diff|.
        let th = (mad * 8.0 / 0.6745).max(1e-9);
        g.jump_threshold = th as f32;
        // Only count real outliers: many times the typical jump AND a sizeable absolute step.
        g.jump_outliers = diffs.iter().filter(|&&v| v > th * 4.0 && v > 0.01).count();
    }
    let mut run = 1usize;
    let mut zrun = 0usize;
    for i in 0..x.len() {
        if x[i] == 0.0 {
            zrun += 1;
        } else {
            if zrun >= 8 {
                g.zero_runs += 1;
            }
            g.longest_zero = g.longest_zero.max(zrun);
            zrun = 0;
        }
        if i > 0 && x[i] == x[i - 1] && x[i] != 0.0 {
            run += 1;
        } else {
            if run >= 3 {
                g.flat_runs += 1;
            }
            g.longest_flat = g.longest_flat.max(run.max(1));
            run = 1;
        }
    }
    if zrun >= 8 {
        g.zero_runs += 1;
    }
    g.longest_zero = g.longest_zero.max(zrun);
    if run >= 3 {
        g.flat_runs += 1;
    }
    g.longest_flat = g.longest_flat.max(run);
    g
}

/// Human-readable report.
pub fn format_report(label: &str, r: &SineReport) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{label}: {} samples @ {} Hz  tone {:.3} Hz (min {:.3} max {:.3})  amp {:.2} dBFS",
        r.analyzed_samples,
        r.rate,
        r.freq_mean,
        r.freq_min,
        r.freq_max,
        db(r.amp)
    );
    let _ = writeln!(
        s,
        "  THD+N {:.1} dB (noise floor by MAD {:.1} dB), spike threshold {:.1} dB rel. fundamental",
        r.residual_rms_db, r.noise_mad_db, r.spike_threshold_rel_db
    );
    let _ = writeln!(s, "  glitches: {} ({:.3}/s)  phase steps: {}", r.glitches.len(), r.glitches_per_s, r.phase_steps.len());
    for g in r.glitches.iter().take(12) {
        let _ = writeln!(s, "    spike @ {:.4} s (sample {}), {:.1} dB rel. fundamental", g.time_s, g.index, db(g.rel_amp));
    }
    if r.glitches.len() > 12 {
        let _ = writeln!(s, "    ... {} more", r.glitches.len() - 12);
    }
    for p in r.phase_steps.iter().take(12) {
        let _ = writeln!(s, "    phase step @ {:.4} s (sample {}), {:+.3} rad", p.time_s, p.index, p.radians);
    }
    if r.phase_steps.len() > 12 {
        let _ = writeln!(s, "    ... {} more", r.phase_steps.len() - 12);
    }
    let g = &r.generic;
    let _ = writeln!(
        s,
        "  max jump {:.5} (sine would be {:.5}, ratio {:.2}); jump outliers {} (thr {:.5}); clipped {}; flat runs {} (longest {}); zero runs {} (longest {})",
        r.max_jump,
        r.expected_max_jump,
        if r.expected_max_jump > 0.0 { r.max_jump / r.expected_max_jump } else { 0.0 },
        g.jump_outliers,
        g.jump_threshold,
        g.clipped,
        g.flat_runs,
        g.longest_flat,
        g.zero_runs,
        g.longest_zero
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(n: usize, rate: f64, f: f64, a: f64) -> Vec<f32> {
        (0..n).map(|i| (a * (2.0 * PI * f * i as f64 / rate).sin()) as f32).collect()
    }

    fn params() -> SineParams {
        SineParams { skip_s: 0.1, tail_s: 0.1, ..SineParams::default() }
    }

    #[test]
    fn clean_sine_is_clean() {
        let x = sine(48000 * 3, 48000.0, 1000.0, 0.03);
        let r = analyze_sine(&x, 48000, &params());
        assert!(r.clean(), "{}", format_report("clean", &r));
        assert!(r.residual_rms_db < -110.0, "thd+n {}", r.residual_rms_db);
        assert!((r.freq_mean - 1000.0).abs() < 0.01);
        assert!((r.amp - 0.03).abs() < 1e-5);
        assert_eq!(r.generic.clipped, 0);
    }

    #[test]
    fn clean_sine_44k1() {
        let x = sine(44100 * 3, 44100.0, 1000.0, 0.5);
        let r = analyze_sine(&x, 44100, &params());
        assert!(r.clean(), "{}", format_report("clean44", &r));
        assert!(r.residual_rms_db < -110.0);
    }

    #[test]
    fn slow_frequency_drift_is_not_a_glitch() {
        // 1000 Hz sweeping +200 ppm over 3 s (what the PI trim does).
        let rate = 48000.0;
        let mut ph = 0.0f64;
        let x: Vec<f32> = (0..(rate as usize * 3))
            .map(|i| {
                let f = 1000.0 * (1.0 + 0.0002 * i as f64 / (rate * 3.0));
                ph += 2.0 * PI * f / rate;
                (0.05 * ph.sin()) as f32
            })
            .collect();
        let r = analyze_sine(&x, 48000, &params());
        assert!(r.clean(), "{}", format_report("drift", &r));
        assert!(r.residual_rms_db < -70.0, "thd+n {}", r.residual_rms_db);
    }

    #[test]
    fn ten_khz_tone_is_analysed() {
        let x = sine(48000 * 3, 48000.0, 10_000.0, 0.03);
        let r = analyze_sine(&x, 48000, &params());
        assert!(r.clean(), "{}", format_report("10k", &r));
        assert!(r.residual_rms_db < -100.0, "thd+n {}", r.residual_rms_db);
    }

    #[test]
    fn auto_frequency_finds_offset_tone() {
        let x = sine(44100 * 3, 44100.0, 918.75, 0.03);
        let r = analyze_sine(&x, 44100, &SineParams { freq_hint: 0.0, ..params() });
        assert!(r.clean(), "{}", format_report("auto", &r));
        assert!((r.freq_mean - 918.75).abs() < 0.05, "{}", r.freq_mean);
    }

    #[test]
    fn injected_clicks_are_found() {
        let mut x = sine(48000 * 3, 48000.0, 1000.0, 0.03);
        let at = [30000usize, 71234, 120000];
        for &i in &at {
            x[i] += 0.01;
        }
        let r = analyze_sine(&x, 48000, &params());
        assert_eq!(r.glitches.len(), 3, "{}", format_report("clicks", &r));
        for (g, &i) in r.glitches.iter().zip(&at) {
            assert!(g.index.abs_diff(i) <= 2, "{} vs {}", g.index, i);
        }
        assert!(r.glitches_per_s > 0.9);
    }

    #[test]
    fn dropped_sample_gives_phase_step() {
        let mut x = sine(48000 * 3, 48000.0, 1000.0, 0.03);
        x.remove(70000);
        let r = analyze_sine(&x, 48000, &params());
        assert_eq!(r.phase_steps.len(), 1, "{}", format_report("drop", &r));
        let s = r.phase_steps[0];
        assert!(s.index.abs_diff(70000) < 200, "at {}", s.index);
        assert!((s.radians.abs() - 2.0 * PI * 1000.0 / 48000.0).abs() < 0.04, "{}", s.radians);
    }

    #[test]
    fn duplicated_sample_gives_opposite_phase_step() {
        let mut x = sine(48000 * 3, 48000.0, 1000.0, 0.03);
        let dup = x[70000];
        x.insert(70000, dup);
        let r = analyze_sine(&x, 48000, &params());
        assert_eq!(r.phase_steps.len(), 1, "{}", format_report("dup", &r));
        let drop = {
            let mut y = sine(48000 * 3, 48000.0, 1000.0, 0.03);
            y.remove(70000);
            analyze_sine(&y, 48000, &params()).phase_steps[0].radians
        };
        assert!(r.phase_steps[0].radians * drop < 0.0, "signs must differ");
        assert!(r.generic.flat_runs <= 1);
    }

    #[test]
    fn clipped_sine_is_reported() {
        let x: Vec<f32> = sine(48000 * 3, 48000.0, 1000.0, 1.3).into_iter().map(|v| v.clamp(-1.0, 1.0)).collect();
        let r = analyze_sine(&x, 48000, &params());
        assert!(r.generic.clipped > 1000, "clipped {}", r.generic.clipped);
        assert!(r.residual_rms_db > -40.0, "thd+n {}", r.residual_rms_db);
        assert!(r.generic.longest_flat >= 3);
    }

    #[test]
    fn zero_runs_and_flat_runs() {
        let mut x = sine(4800, 48000.0, 440.0, 0.3);
        for v in &mut x[1000..1100] {
            *v = 0.0;
        }
        for v in &mut x[2000..2010] {
            *v = 0.25;
        }
        let g = generic_stats(&x);
        assert_eq!(g.zero_runs, 1);
        assert_eq!(g.longest_zero, 100);
        assert_eq!(g.flat_runs, 1);
        assert_eq!(g.longest_flat, 10);
    }
}
