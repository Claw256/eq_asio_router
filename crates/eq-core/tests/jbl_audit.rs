//! JBL Bass Mode default-value audit harness (measurement only, no assertions).
//!
//! Run: `cargo test -p eq-core --release --test jbl_audit -- --ignored --nocapture`
//! Optional env: `JBL_SET="duck_depth=1.5,res_db=6"` overrides `JblParams` fields (bool as 0/1),
//! `JBL_INT="30,50,80,100"` intensities, `JBL_FS="44100,48000,96000"` rates.
use eq_core::{Biquad, FilterKind, FilterSpec, JblLfoTarget, JblParams, StereoEq};
use std::f64::consts::PI;

fn apply(p: &mut JblParams, kv: &str) {
    for item in kv.split(',').filter(|s| !s.is_empty()) {
        let (k, v) = item.split_once('=').expect("k=v");
        let f: f32 = v.parse().expect("number");
        let b = f != 0.0;
        match k {
            "duck_on" => p.duck_on = b,
            "duck_depth" => p.duck_depth = f,
            "duck_attack_ms" => p.duck_attack_ms = f,
            "duck_release_ms" => p.duck_release_ms = f,
            "detect_hz" => p.detect_hz = f,
            "shelf_hz" => p.shelf_hz = f,
            "shelf_db" => p.shelf_db = f,
            "res_on" => p.res_on = b,
            "res_hz" => p.res_hz = f,
            "res_db" => p.res_db = f,
            "res_q" => p.res_q = f,
            "drive_max" => p.drive_max = f,
            "bias_max" => p.bias_max = f,
            "kick_on" => p.kick_on = b,
            "kick_hz" => p.kick_hz = f,
            "kick_drive_max" => p.kick_drive_max = f,
            "crush_on" => p.crush_on = b,
            "crush_bits_min" => p.crush_bits_min = f,
            "hold_on" => p.hold_on = b,
            "hold_max" => p.hold_max = f,
            "lpf_on" => p.lpf_on = b,
            "lpf_hz_start" => p.lpf_hz_start = f,
            "lpf_hz_end" => p.lpf_hz_end = f,
            "lfo_on" => p.lfo_on = b,
            "lfo_rate_hz" => p.lfo_rate_hz = f,
            "lfo_depth" => p.lfo_depth = f,
            "lfo_follow_bass" => p.lfo_follow_bass = b,
            "lfo_target" => {
                p.lfo_target = match f as i32 {
                    0 => JblLfoTarget::Gain,
                    1 => JblLfoTarget::BassBand,
                    _ => JblLfoTarget::Both,
                }
            }
            "out_trim_db" => p.out_trim_db = f,
            _ => panic!("unknown key {k}"),
        }
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
}

const BPM: f64 = 128.0;

fn kick_env(t: f64) -> (f64, f64) {
    // returns (time since last kick, kick sample)
    let beat = 60.0 / BPM;
    let tk = t % beat;
    let f = 48.0 + 90.0 * (-tk * 40.0).exp();
    // phase of sweep: integral of f
    let ph = 2.0 * PI * (48.0 * tk + 90.0 * (1.0 - (-tk * 40.0).exp()) / 40.0);
    let _ = f;
    (tk, (-tk * 9.0).exp() * ph.sin())
}

fn scale_rms(mut v: Vec<f64>, rms_db: f64) -> Vec<f64> {
    let rms_db = std::env::var("JBL_RMS").ok().and_then(|s| s.parse().ok()).unwrap_or(rms_db);
    let r = (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt();
    let g = 10f64.powf(rms_db / 20.0) / r;
    v.iter_mut().for_each(|x| *x *= g);
    v
}

fn sig_kick_sub(n: usize, fs: f64) -> Vec<f64> {
    let mut rng = Rng(1);
    let notes = [55.0, 55.0, 65.41, 49.0];
    let beat = 60.0 / BPM;
    let v: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / fs;
            let (_, k) = kick_env(t);
            let note = notes[((t / (beat * 4.0)) as usize) % 4];
            let sub = 0.5 * (2.0 * PI * note * t).sin() * (0.6 + 0.4 * (2.0 * PI * t / beat).sin().abs());
            let off = (t + beat / 2.0) % beat;
            let hat = 0.1 * rng.next() * (-off * 60.0).exp();
            0.9 * k + sub + hat
        })
        .collect();
    scale_rms(v, -14.0)
}

fn sig_fullband(n: usize, fs: f64) -> Vec<f64> {
    let mut rng = Rng(7);
    let (mut b0, mut b1, mut b2, mut b3, mut b4, mut b5, mut b6) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    let chord = [220.0, 261.63, 329.63];
    let v: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / fs;
            let w = rng.next();
            b0 = 0.99886 * b0 + w * 0.0555179;
            b1 = 0.99332 * b1 + w * 0.0750759;
            b2 = 0.96900 * b2 + w * 0.1538520;
            b3 = 0.86650 * b3 + w * 0.3104856;
            b4 = 0.55000 * b4 + w * 0.5329522;
            b5 = -0.7616 * b5 - w * 0.0168980;
            let pink = b0 + b1 + b2 + b3 + b4 + b5 + b6 + w * 0.5362;
            b6 = w * 0.115926;
            let mut c = 0.0;
            for f in chord {
                for h in 1..=8 {
                    c += (2.0 * PI * f * h as f64 * t).sin() / h as f64;
                }
            }
            0.35 * pink + 0.12 * c + 0.4 * (2.0 * PI * 55.0 * t).sin()
        })
        .collect();
    scale_rms(v, -14.0)
}

/// Vocal-like harmonics (f0 200 Hz, formants ~700/1200/2600), no content below 300 Hz.
fn vocal(t: f64) -> f64 {
    let f0 = 200.0 * (1.0 + 0.01 * (2.0 * PI * 5.0 * t).sin());
    let mut s = 0.0;
    for h in 2..=18 {
        let f = f0 * h as f64;
        let form = |c: f64, w: f64| (-((f - c) / w).powi(2)).exp();
        let a = 1.0 * form(700.0, 250.0) + 0.6 * form(1200.0, 300.0) + 0.3 * form(2600.0, 500.0);
        s += a / (h as f64).sqrt() * (2.0 * PI * f * t + h as f64).sin();
    }
    s
}

fn sig_kick_vocal(n: usize, fs: f64) -> Vec<f64> {
    let v: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / fs;
            0.9 * kick_env(t).1 + 0.35 * vocal(t)
        })
        .collect();
    scale_rms(v, -14.0)
}

fn render(fs: f64, x: &[f64], pct: u8, p: &JblParams, on: bool) -> Vec<f32> {
    let mut l: Vec<f32> = x.iter().map(|v| *v as f32).collect();
    let mut r: Vec<f32> = l.clone();
    let mut eq = StereoEq::new(fs);
    eq.set_limiter(true);
    eq.set_bass_fx(on, pct, p);
    // process in 480-sample blocks like the engine would
    for (cl, cr) in l.chunks_mut(480).zip(r.chunks_mut(480)) {
        eq.process_stereo(cl, cr);
    }
    l
}

fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * PI / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        for s in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (s + k, s + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        len <<= 1;
    }
}

const NF: usize = 16384;

/// Averaged power spectrum (Hann), bin power.
fn spectrum(x: &[f32]) -> Vec<f64> {
    let mut acc = vec![0.0; NF / 2];
    let mut cnt = 0.0;
    let mut s = 0;
    while s + NF <= x.len() {
        let mut re: Vec<f64> =
            (0..NF).map(|i| f64::from(x[s + i]) * (0.5 - 0.5 * (2.0 * PI * i as f64 / NF as f64).cos())).collect();
        let mut im = vec![0.0; NF];
        fft(&mut re, &mut im);
        for k in 0..NF / 2 {
            acc[k] += re[k] * re[k] + im[k] * im[k];
        }
        cnt += 1.0;
        s += NF / 2;
    }
    acc.iter_mut().for_each(|a| *a /= cnt);
    acc
}

fn band_db(sp: &[f64], fs: f64, lo: f64, hi: f64) -> f64 {
    let df = fs / NF as f64;
    let e: f64 = sp.iter().enumerate().filter(|(k, _)| (*k as f64 * df) >= lo && (*k as f64 * df) < hi).map(|(_, v)| v).sum();
    10.0 * (e + 1e-30).log10()
}

fn rms_db(x: &[f32]) -> f64 {
    10.0 * ((x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len() as f64) + 1e-30).log10()
}

fn kweight(x: &[f32], fs: f64) -> Vec<f32> {
    let hp = FilterSpec { kind: FilterKind::HighPass, fc_hz: 38.0, gain_db: 0.0, q: 0.5, enabled: true };
    let hs = FilterSpec { kind: FilterKind::HighShelf, fc_hz: 1681.0, gain_db: 4.0, q: 0.7, enabled: true };
    let mut a = Biquad::from_spec(&hp, fs);
    let mut b = Biquad::from_spec(&hs, fs);
    x.iter().map(|v| b.process(a.process(*v))).collect()
}

fn bandpass(x: &[f32], fs: f64, lo: f64, hi: f64) -> Vec<f32> {
    let hp = FilterSpec { kind: FilterKind::HighPass, fc_hz: lo, gain_db: 0.0, q: 0.707, enabled: true };
    let lp = FilterSpec { kind: FilterKind::LowPass, fc_hz: hi, gain_db: 0.0, q: 0.707, enabled: true };
    let (mut a, mut b) = (Biquad::from_spec(&hp, fs), Biquad::from_spec(&lp, fs));
    let (mut c, mut d) = (Biquad::from_spec(&hp, fs), Biquad::from_spec(&lp, fs));
    x.iter().map(|v| d.process(c.process(b.process(a.process(*v))))).collect()
}

fn thd(fs: f64, f_target: f64, amp_db: f64, pct: u8, p: &JblParams) -> (f64, f64) {
    let k = (f_target * NF as f64 / fs).round();
    let f = k * fs / NF as f64;
    let amp = 10f64.powf(amp_db / 20.0);
    let n = NF * 4;
    let x: Vec<f64> = (0..n).map(|i| amp * (2.0 * PI * f * i as f64 / fs).sin()).collect();
    let y = render(fs, &x, pct, p, true);
    let seg = &y[NF * 2..NF * 3];
    let mut re: Vec<f64> = seg.iter().map(|v| f64::from(*v)).collect();
    let mut im = vec![0.0; NF];
    fft(&mut re, &mut im);
    let pw: Vec<f64> = (0..NF / 2).map(|k| re[k] * re[k] + im[k] * im[k]).collect();
    let kk = k as usize;
    let fund = pw[kk - 1] + pw[kk] + pw[kk + 1];
    let total: f64 = pw.iter().skip(1).sum();
    let mut harm = 0.0;
    for h in 2..=10 {
        let hk = kk * h;
        if hk + 1 < NF / 2 {
            harm += pw[hk - 1] + pw[hk] + pw[hk + 1];
        }
    }
    (10.0 * (harm / fund).log10(), 10.0 * ((total - fund) / fund).log10())
}

fn pump(fs: f64, pct: u8, p: &JblParams) -> (f64, f64, f64, f64) {
    let n = (fs * 12.0) as usize;
    let x = sig_kick_vocal(n, fs);
    let on = render(fs, &x, pct, p, true);
    let mut q = *p;
    q.duck_on = false;
    let off = render(fs, &x, pct, &q, true);
    let (mo, mf) = (bandpass(&on, fs, 300.0, 3000.0), bandpass(&off, fs, 300.0, 3000.0));
    let win = (fs * 0.005) as usize;
    let beat = 60.0 / BPM;
    let ratio_at = |t0: f64| -> f64 {
        let s = (t0 * fs) as usize;
        let e = |v: &[f32]| (v[s..s + win].iter().map(|a| f64::from(*a).powi(2)).sum::<f64>() / win as f64).sqrt();
        20.0 * ((e(&mo) + 1e-12) / (e(&mf) + 1e-12)).log10()
    };
    let (mut dips, mut tmins, mut recs, mut means) = (vec![], vec![], vec![], vec![]);
    for kk in 3..20 {
        let t0 = kk as f64 * beat;
        let steps = (beat / 0.005) as usize - 2;
        let rs: Vec<f64> = (0..steps).map(|i| ratio_at(t0 + i as f64 * 0.005)).collect();
        let (imin, vmin) = rs.iter().enumerate().fold((0, 1e9), |a, (i, v)| if *v < a.1 { (i, *v) } else { a });
        // recovery: first window after the min with ratio > -1 dB below the beat-average of last 100ms
        let tail = rs[steps - 20..].iter().sum::<f64>() / 20.0;
        let rec = rs.iter().enumerate().skip(imin).find(|(_, v)| **v > tail - 1.0).map(|(i, _)| i).unwrap_or(steps);
        dips.push(vmin - tail);
        tmins.push(imin as f64 * 5.0);
        recs.push((rec - imin) as f64 * 5.0);
        means.push(rs.iter().sum::<f64>() / steps as f64);
    }
    let m = |v: &Vec<f64>| v.iter().sum::<f64>() / v.len() as f64;
    (m(&dips), m(&tmins), m(&recs), m(&means))
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |a, v| a.max(v.abs()))
}

#[test]
#[ignore]
fn jbl_audit() {
    let mut p = JblParams::default();
    if let Ok(s) = std::env::var("JBL_SET") {
        apply(&mut p, &s);
    }
    let pcts: Vec<u8> =
        std::env::var("JBL_INT").unwrap_or("30,50,80,100".into()).split(',').map(|s| s.parse().unwrap()).collect();
    let rates: Vec<f64> =
        std::env::var("JBL_FS").unwrap_or("48000".into()).split(',').map(|s| s.parse().unwrap()).collect();
    println!("PARAMS {p:?}");
    for &fs in &rates {
        let n = (fs * 12.0) as usize;
        let sigs: [(&str, Vec<f64>); 3] =
            [("kick+sub", sig_kick_sub(n, fs)), ("fullband", sig_fullband(n, fs)), ("kick+vox", sig_kick_vocal(n, fs))];
        for (name, x) in &sigs {
            let dry = render(fs, x, 0, &p, false);
            let dry = &dry[(fs * 1.0) as usize..];
            let sd = spectrum(dry);
            let kd = rms_db(&kweight(dry, fs)[2000..]);
            for &pct in &pcts {
                let wet = render(fs, x, pct, &p, true);
                let wet = &wet[(fs * 1.0) as usize..];
                let sw = spectrum(wet);
                let kw = rms_db(&kweight(wet, fs)[2000..]);
                let knee = 0.891_250_9;
                let lim = wet.iter().filter(|v| v.abs() > knee).count() as f64 / wet.len() as f64 * 100.0;
                let b = |lo, hi| band_db(&sw, fs, lo, hi) - band_db(&sd, fs, lo, hi);
                println!(
                    "FS{fs:.0} {name:9} pct={pct:3} dRMS={:+.2} dK={:+.2} peak={:.3} lim%={:.2} | sub30-60={:+.1} bass60-150={:+.1} lowmid150-500={:+.1} mid.5-4k={:+.1} hi4-8k={:+.1} air8-16k={:+.1}",
                    rms_db(wet) - rms_db(dry), kw - kd, peak(wet), lim,
                    b(30.0, 60.0), b(60.0, 150.0), b(150.0, 500.0), b(500.0, 4000.0), b(4000.0, 8000.0), b(8000.0, 16000.0)
                );
            }
        }
        for &pct in &pcts {
            let (h60, n60) = thd(fs, 60.0, -12.0, pct, &p);
            let (h60q, _) = thd(fs, 60.0, -24.0, pct, &p);
            let (h1k, n1k) = thd(fs, 1000.0, -20.0, pct, &p);
            let (dip, tmin, rec, mean) = pump(fs, pct, &p);
            println!(
                "FS{fs:.0} pct={pct:3} THD60Hz(-12dBFS)={h60:.1}dB THD+N={n60:.1} THD60(-24)={h60q:.1} | THD1k(-20)={h1k:.1}dB THD+N={n1k:.1} | PUMP dip={dip:.1}dB at {tmin:.0}ms recov(-1dB)={rec:.0}ms meanMidGain={mean:.1}dB"
            );
        }
    }
}
