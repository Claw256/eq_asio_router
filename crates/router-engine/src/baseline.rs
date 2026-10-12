//! Baseline measurement of the latency probe: level statistics of the source (ring-A tap) and loopback streams while
//! nothing is injected (diagnostics, `router-cli latency --baseline`) and the detection thresholds derived
//! from them (`router-cli latency`).

use std::sync::atomic::{AtomicU8, Ordering::{Relaxed, Release}};
use std::sync::Mutex;

/// Blocks of per-block history kept per channel (about 11 s at 256-frame blocks).
const MONITOR_HISTORY: usize = 2048;
/// Frames of raw samples kept per loopback channel when raw capture is requested (about 1.4 s at 48 kHz).
pub const RAW_FRAMES: usize = 65536;
/// Block-peak thresholds (dBFS) whose exceedance fractions are reported.
pub const MONITOR_THRESHOLDS_DB: [f32; 4] = [-90.0, -80.0, -70.0, -60.0];

/// Index of a monitored channel: capture L, capture R, loopback L, loopback R.
pub const CH_CAP_L: usize = 0;
pub const CH_CAP_R: usize = 1;
pub const CH_LB_L: usize = 2;
pub const CH_LB_R: usize = 3;

/// 20 log10 with a floor of -200 dBFS for (near) zero.
#[inline]
pub fn lin_to_db(x: f64) -> f64 {
    if x <= 1e-10 {
        -200.0
    } else {
        20.0 * x.log10()
    }
}

struct ChanAcc {
    n: u64,
    sum: f64,
    sumsq: f64,
    min: f32,
    max: f32,
    blocks: u32,
    above: [u32; 4],
    /// Per-block RMS (dBFS incl. DC; -200 for digital zero).
    block_rms_db: Vec<f32>,
    /// Per-block maximum and minimum sample (for the robust noise floor).
    block_max: Vec<f32>,
    block_min: Vec<f32>,
    /// First `RAW_FRAMES` samples (only if requested at `start`).
    raw: Vec<f32>,
    keep_raw: bool,
}

impl ChanAcc {
    fn new() -> Self {
        ChanAcc {
            n: 0,
            sum: 0.0,
            sumsq: 0.0,
            min: f32::INFINITY,
            max: f32::NEG_INFINITY,
            blocks: 0,
            above: [0; 4],
            block_rms_db: Vec::with_capacity(MONITOR_HISTORY),
            block_max: Vec::with_capacity(MONITOR_HISTORY),
            block_min: Vec::with_capacity(MONITOR_HISTORY),
            raw: Vec::with_capacity(RAW_FRAMES),
            keep_raw: false,
        }
    }

    fn reset(&mut self) {
        self.n = 0;
        self.sum = 0.0;
        self.sumsq = 0.0;
        self.min = f32::INFINITY;
        self.max = f32::NEG_INFINITY;
        self.blocks = 0;
        self.above = [0; 4];
        self.block_rms_db.clear();
        self.block_max.clear();
        self.block_min.clear();
        self.raw.clear();
    }

    /// Allocation-free: the history vector is preallocated and never grows past its capacity.
    fn feed(&mut self, x: &[f32]) {
        if x.is_empty() {
            return;
        }
        let (mut sum, mut sumsq, mut pk) = (0.0f64, 0.0f64, 0.0f32);
        let (mut bmax, mut bmin) = (f32::NEG_INFINITY, f32::INFINITY);
        for &v in x {
            bmax = bmax.max(v);
            bmin = bmin.min(v);
            sum += f64::from(v);
            sumsq += f64::from(v) * f64::from(v);
            pk = pk.max(v.abs());
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
        if self.keep_raw {
            let room = RAW_FRAMES - self.raw.len();
            self.raw.extend_from_slice(&x[..x.len().min(room)]);
        }
        self.n += x.len() as u64;
        self.sum += sum;
        self.sumsq += sumsq;
        self.blocks += 1;
        for (k, t) in MONITOR_THRESHOLDS_DB.iter().enumerate() {
            if f64::from(pk) >= 10f64.powf(f64::from(*t) / 20.0) {
                self.above[k] += 1;
            }
        }
        if self.block_rms_db.len() < MONITOR_HISTORY {
            self.block_rms_db.push(lin_to_db((sumsq / x.len() as f64).sqrt()) as f32);
            self.block_max.push(bmax);
            self.block_min.push(bmin);
        }
    }

    fn report(&self) -> ChannelReport {
        if self.n == 0 {
            return ChannelReport::default();
        }
        let n = self.n as f64;
        let dc = self.sum / n;
        let ms = self.sumsq / n;
        let ac = (ms - dc * dc).max(0.0).sqrt();
        let peak = f64::from(self.max.abs().max(self.min.abs()));
        let dev = (f64::from(self.max) - dc).abs().max((dc - f64::from(self.min)).abs());
        let (m, sd, character) = classify(&self.block_rms_db, dev);
        let dev_blocks: Vec<f64> = self.block_max.iter().zip(&self.block_min).map(|(&hi, &lo)| (f64::from(hi) - dc).abs().max((dc - f64::from(lo)).abs())).collect();
        let abs_blocks: Vec<f64> = self.block_max.iter().zip(&self.block_min).map(|(&hi, &lo)| f64::from(hi.abs().max(lo.abs()))).collect();
        let mut af = [0.0; 4];
        for (a, c) in af.iter_mut().zip(self.above) {
            *a = f64::from(c) / f64::from(self.blocks.max(1));
        }
        ChannelReport {
            blocks: self.blocks,
            samples: self.n,
            peak_db: lin_to_db(peak),
            rms_db: lin_to_db(ms.sqrt()),
            ac_rms_db: lin_to_db(ac),
            dc,
            dc_db: lin_to_db(dc.abs()),
            min: self.min,
            max: self.max,
            dev_peak: dev,
            dev_peak_db: lin_to_db(dev),
            floor_dev_db: lin_to_db(percentile(&dev_blocks, FLOOR_PERCENTILE)),
            floor_peak_db: lin_to_db(percentile(&abs_blocks, FLOOR_PERCENTILE)),
            above_fraction: af,
            block_rms_mean_db: m,
            block_rms_sd_db: sd,
            character,
        }
    }
}

/// What the level over the window looks like.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Character {
    /// No data.
    #[default]
    NoData,
    /// Every sample exactly zero.
    DigitalSilence,
    /// Constant offset only (no variation around it).
    DcOnly,
    /// Level constant from block to block: noise floor, dither, hum, or a steady tone.
    Steady,
    /// Level varies from block to block: programme audio or intermittent sounds.
    TimeVarying,
}

impl Character {
    pub fn describe(self) -> &'static str {
        match self {
            Character::NoData => "no data",
            Character::DigitalSilence => "digital silence",
            Character::DcOnly => "constant DC offset only",
            Character::Steady => "steady (noise / dither / hum)",
            Character::TimeVarying => "time-varying (programme audio or intermittent)",
        }
    }
}

/// Percentile of the per-block peaks used as the noise floor.
pub const FLOOR_PERCENTILE: f64 = 0.99;

/// `p` (0..1) percentile of `v` (nearest rank); 0 for an empty slice.
pub fn percentile(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let k = ((s.len() as f64 * p).ceil() as usize).clamp(1, s.len()) - 1;
    s[k]
}

/// Standard deviation (dB) of the per-block RMS above which a signal counts as time-varying.
pub const STEADY_SD_DB: f64 = 3.0;

/// Classifies a per-block RMS history (dBFS): (mean, standard deviation, character). Pure.
pub fn classify(block_rms_db: &[f32], dev_peak: f64) -> (f64, f64, Character) {
    if block_rms_db.is_empty() {
        return (-200.0, 0.0, Character::NoData);
    }
    let v: Vec<f64> = block_rms_db.iter().map(|&x| f64::from(x).max(-200.0)).collect();
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    let sd = if v.len() > 1 { (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (v.len() as f64 - 1.0)).sqrt() } else { 0.0 };
    let ch = if v.iter().all(|&x| x <= -199.0) {
        Character::DigitalSilence
    } else if dev_peak < 1e-9 {
        Character::DcOnly
    } else if sd < STEADY_SD_DB {
        Character::Steady
    } else {
        Character::TimeVarying
    };
    (mean, sd, ch)
}

/// Summary of one monitored channel over the window.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChannelReport {
    pub blocks: u32,
    pub samples: u64,
    pub peak_db: f64,
    pub rms_db: f64,
    /// RMS after removing the mean (noise, hum and programme part), dBFS.
    pub ac_rms_db: f64,
    /// Mean (DC offset), linear and dBFS of its magnitude.
    pub dc: f64,
    pub dc_db: f64,
    pub min: f32,
    pub max: f32,
    /// Largest deviation from the mean (linear and dBFS): the noise floor a DC-removing detector sees.
    pub dev_peak: f64,
    pub dev_peak_db: f64,
    /// Robust noise floor: the 99th percentile over the blocks of the per-block deviation from the mean (dBFS) and of
    /// the per-block absolute peak (dBFS). Rare outliers (a few blocks of a window) do not move it, unlike `dev_peak_db`.
    pub floor_dev_db: f64,
    pub floor_peak_db: f64,
    /// Fraction of blocks whose peak was at or above each of [`MONITOR_THRESHOLDS_DB`].
    pub above_fraction: [f64; 4],
    pub block_rms_mean_db: f64,
    pub block_rms_sd_db: f64,
    pub character: Character,
}

/// Accumulates level statistics of the capture and loopback streams while active.
/// The audio / capture threads only ever `try_lock` (a missed block is harmless) and never allocate.
pub struct Monitor {
    active: AtomicU8,
    /// capture L/R, loopback L/R, loopback L/R after the detection high-pass.
    acc: Mutex<[ChanAcc; 6]>,
}

impl Default for Monitor {
    fn default() -> Self {
        Monitor { active: AtomicU8::new(0), acc: Mutex::new(std::array::from_fn(|_| ChanAcc::new())) }
    }
}

impl Monitor {
    /// Clears the statistics and starts accumulating (control thread).
    pub fn start(&self) {
        self.start_with(false);
    }

    /// As [`start`](Self::start); with `keep_raw` the first [`RAW_FRAMES`] samples of every channel are kept.
    pub fn start_with(&self, keep_raw: bool) {
        {
            let mut a = self.acc.lock().unwrap_or_else(|e| e.into_inner());
            for c in a.iter_mut() {
                c.reset();
                c.keep_raw = keep_raw;
            }
        }
        self.active.store(1, Release);
    }

    /// The raw samples kept since the last `start_with(true)`: [capture L, capture R, loopback L, loopback R].
    pub fn raw(&self) -> [Vec<f32>; 4] {
        let a = self.acc.lock().unwrap_or_else(|e| e.into_inner());
        [a[0].raw.clone(), a[1].raw.clone(), a[2].raw.clone(), a[3].raw.clone()]
    }

    /// Stops accumulating and returns the four channel reports.
    pub fn stop(&self) -> [ChannelReport; 4] {
        self.active.store(0, Release);
        self.report()
    }

    /// Statistics of the loopback pair after the detection high-pass (empty reports if none was configured).
    pub fn report_hp(&self) -> [ChannelReport; 2] {
        let a = self.acc.lock().unwrap_or_else(|e| e.into_inner());
        [a[4].report(), a[5].report()]
    }

    pub fn report(&self) -> [ChannelReport; 4] {
        let a = self.acc.lock().unwrap_or_else(|e| e.into_inner());
        [a[0].report(), a[1].report(), a[2].report(), a[3].report()]
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        self.active.load(Relaxed) != 0
    }

    /// `which`: 0 = capture side, 1 = loopback inputs, 2 = loopback inputs after the high-pass. Audio / capture thread.
    #[inline]
    pub fn feed(&self, which: usize, l: &[f32], r: &[f32]) {
        if let Ok(mut a) = self.acc.try_lock() {
            let base = (which % 3) * 2;
            a[base].feed(l);
            a[base + 1].feed(r);
        }
    }
}

// ------------------------------------------------------------------------------------
// Detection high-pass
// ------------------------------------------------------------------------------------

pub use router_core::band::{auto_highpass_hz, click_envelope, HighPass, ENV_WINDOW};

// ------------------------------------------------------------------------------------
// Thresholds derived from the measured baseline
// ------------------------------------------------------------------------------------

/// Margin above the measured noise floor for the detection thresholds (dB).
pub const FLOOR_MARGIN_DB: f64 = 12.0;
/// The threshold must stay at least this far below the expected click level (dB).
pub const CLICK_HEADROOM_DB: f64 = 6.0;
/// Capture-side signal above this in the baseline is programme audio (dBFS).
pub const CAP_CONTAMINATION_DBFS: f64 = -60.0;
/// Loopback deviation above this in the baseline is programme audio (dBFS).
pub const LB_CONTAMINATION_DBFS: f64 = -40.0;
/// With the band-limited (high-pass) detector the unfiltered loopback may carry quiet programme audio below the
/// detector's band (a microphone monitored into the Streaming mix); only louder content than this aborts (dBFS).
/// **Only** for a burst of at least [`HP_LOOSENING_MIN_HZ`] with the high-pass active (the caller decides, see
/// [`plan_thresholds_checked`]); every other run keeps [`LB_CONTAMINATION_DBFS`].
pub const LB_CONTAMINATION_HP_DBFS: f64 = -20.0;
/// Lowest burst frequency for which the raw loopback limit may be relaxed to [`LB_CONTAMINATION_HP_DBFS`] (Hz).
pub const HP_LOOSENING_MIN_HZ: f32 = 15000.0;
/// The loopback threshold is never more sensitive than this far below the expected click level (dB).
pub const LB_MIN_BELOW_CLICK_DB: f64 = 25.0;
/// The capture threshold is never more sensitive than this far below the click level (dB).
pub const CAP_MIN_BELOW_CLICK_DB: f64 = 20.0;

/// Detection thresholds and the DC offsets to remove.
#[derive(Clone, Debug, PartialEq)]
pub struct Thresholds {
    pub cap_db: f32,
    pub lb_db: f32,
    /// Baseline means (linear) of capture L/R and loopback L/R.
    pub dc: [f32; 4],
    pub cap_floor_db: f64,
    pub lb_floor_db: f64,
    /// Expected click level at the loopback (dBFS).
    pub lb_expected_db: f64,
    pub notes: Vec<String>,
}

const NAMES: [&str; 4] = ["source L", "source R", "loopback L (Yamaha input)", "loopback R (Yamaha input)"];

/// Derives the detection thresholds from the baseline reports (`rep` = [src L, src R, lb L, lb R]).
///
/// * source: `max(click - 20, floor + 12)`, never above `click - 6`;
/// * loopback: `max(click + preamp - 25, floor + 12)`, never above `click + preamp - 6`; `floor` is the largest
///   deviation from the baseline mean (the detector removes that mean);
/// * programme audio (capture peak above -60 dBFS or loopback deviation above -40 dBFS) aborts.
///
/// Explicit overrides are honoured but checked against the floor.
///
/// This form relaxes the raw loopback limit to -20 dBFS whenever `hp` is given; production code must call
/// [`plan_thresholds_checked`] and say whether that relaxation is allowed (a >= 15 kHz burst with the high-pass).
pub fn plan_thresholds(
    rep: &[ChannelReport; 4],
    hp: Option<&[ChannelReport; 2]>,
    env_gain_db: f64,
    click_db: f32,
    preamp_db: f32,
    cap_override: Option<f32>,
    lb_override: Option<f32>,
) -> Result<Thresholds, String> {
    plan_thresholds_checked(rep, hp, env_gain_db, click_db, preamp_db, cap_override, lb_override, hp.is_some())
}

/// [`plan_thresholds`] with an explicit permission for the relaxed raw loopback limit (-20 dBFS instead of
/// -40 dBFS): `relax_raw_limit` must be true only for a `burst:<hz >= 15000>` click detected through the
/// high-pass; when false the -40 dBFS limit applies even if `hp` is given.
#[allow(clippy::too_many_arguments)]
pub fn plan_thresholds_checked(
    rep: &[ChannelReport; 4],
    hp: Option<&[ChannelReport; 2]>,
    env_gain_db: f64,
    click_db: f32,
    preamp_db: f32,
    cap_override: Option<f32>,
    lb_override: Option<f32>,
    relax_raw_limit: bool,
) -> Result<Thresholds, String> {
    let click = f64::from(click_db);
    // With the band-limited detector the click shows up as the peak of its RMS envelope (`env_gain_db` below its peak).
    let expect = click + f64::from(preamp_db) + if hp.is_some() { env_gain_db } else { 0.0 };
    for (i, c) in rep.iter().enumerate() {
        if c.blocks == 0 {
            return Err(format!("no data from the {} during the baseline (the stream delivered no blocks)", NAMES[i]));
        }
    }
    let cap_k = if rep[1].peak_db > rep[0].peak_db { 1 } else { 0 };
    let cap_peak = rep[cap_k].peak_db;
    if cap_peak > CAP_CONTAMINATION_DBFS {
        return Err(format!(
            "baseline is not quiet at the CAPTURE detector: {} peak {cap_peak:.1} dBFS (limit {CAP_CONTAMINATION_DBFS:.0}), {}: another program is playing into the cable; aborting",
            NAMES[cap_k],
            rep[cap_k].character.describe()
        ));
    }
    let lb_k = if rep[CH_LB_R].dev_peak_db > rep[CH_LB_L].dev_peak_db { CH_LB_R } else { CH_LB_L };
    let raw_floor = rep[lb_k].dev_peak_db;
    let raw_floor_robust = rep[CH_LB_L].floor_dev_db.max(rep[CH_LB_R].floor_dev_db);
    // Programme audio is judged on the unfiltered loopback, whatever the detector uses.
    let raw_limit = if hp.is_some() && relax_raw_limit { LB_CONTAMINATION_HP_DBFS } else { LB_CONTAMINATION_DBFS };
    if raw_floor > raw_limit {
        return Err(format!(
            "baseline is not quiet at the LOOPBACK detector: {} deviation {raw_floor:.1} dBFS (limit {raw_limit:.0}), {}: the Yamaha loopback carries other audio (another Windows endpoint, or a mixer input such as a microphone); aborting",
            NAMES[lb_k],
            rep[lb_k].character.describe()
        ));
    }
    // The floor the detector actually sees: after the high-pass when one is used.
    let (lb_floor, band_note) = match hp {
        Some(h) => {
            if h.iter().any(|c| c.blocks == 0) {
                return Err("no data from the high-passed loopback during the baseline".into());
            }
            (h[0].floor_peak_db.max(h[1].floor_peak_db), " after the high-pass and RMS envelope")
        }
        None => (raw_floor_robust, ""),
    };
    let cap_floor = rep[CH_CAP_L].floor_dev_db.max(rep[CH_CAP_R].floor_dev_db);
    let mut notes = Vec::new();
    if hp.is_some() && relax_raw_limit && raw_floor > LB_CONTAMINATION_DBFS {
        notes.push(format!(
            "SAFETY NOTE: the Yamaha loopback carries audio (raw peak {:.1} dBFS, deviation {raw_floor:.1} dBFS; {}; probably a microphone or other input monitored into the Streaming mix), above the normal {LB_CONTAMINATION_DBFS:.0} dBFS limit. The limit is relaxed to {LB_CONTAMINATION_HP_DBFS:.0} dBFS only because the click is a burst of at least {HP_LOOSENING_MIN_HZ:.0} Hz detected through the high-pass; the router output stays limited to -40 dBFS",
            rep[lb_k].peak_db,
            rep[lb_k].character.describe()
        ));
    }
    let cap_thr = match cap_override {
        Some(t) => {
            if f64::from(t) < cap_floor + 6.0 {
                notes.push(format!("note: --cap-threshold-db {t:.1} is within 6 dB of the source noise floor {cap_floor:.1} dBFS"));
            }
            f64::from(t)
        }
        None => (click - CAP_MIN_BELOW_CLICK_DB).max(cap_floor + FLOOR_MARGIN_DB),
    };
    if cap_thr > click - CLICK_HEADROOM_DB {
        return Err(format!(
            "source noise floor {cap_floor:.1} dBFS is too high to detect a {click:.0} dBFS click: threshold {cap_thr:.1} dBFS would exceed click - {CLICK_HEADROOM_DB:.0} dB ({:.1} dBFS)",
            click - CLICK_HEADROOM_DB
        ));
    }
    let lb_thr = match lb_override {
        Some(t) => {
            if f64::from(t) < lb_floor + 6.0 {
                notes.push(format!("note: --lb-threshold-db {t:.1} is within 6 dB of the loopback noise floor {lb_floor:.1} dBFS"));
            }
            f64::from(t)
        }
        None => (expect - LB_MIN_BELOW_CLICK_DB).max(lb_floor + FLOOR_MARGIN_DB),
    };
    if lb_thr > expect - CLICK_HEADROOM_DB {
        let max_floor = expect - CLICK_HEADROOM_DB - FLOOR_MARGIN_DB;
        return Err(format!(
            "the loopback noise floor ({lb_floor:.1} dBFS deviation on {}{band_note}, {}) is too high to detect a {click:.0} dBFS click: the click reaches the loopback at about {click:.0} + ({preamp_db:.0}) = {expect:.0} dBFS; the threshold must be {FLOOR_MARGIN_DB:.0} dB above the floor ({lb_thr:.1} dBFS) and at most {CLICK_HEADROOM_DB:.0} dB below the click ({:.1} dBFS), so the floor would have to be below {max_floor:.1} dBFS. A louder click is not allowed (max -35 dBFS); remove what feeds the loopback instead (see `router-cli latency --baseline`)",
            NAMES[lb_k],
            rep[lb_k].character.describe(),
            expect - CLICK_HEADROOM_DB
        ));
    }
    Ok(Thresholds {
        cap_db: cap_thr as f32,
        lb_db: lb_thr as f32,
        dc: match hp {
            Some(_) => [rep[0].dc as f32, rep[1].dc as f32, 0.0, 0.0],
            None => [rep[0].dc as f32, rep[1].dc as f32, rep[2].dc as f32, rep[3].dc as f32],
        },
        cap_floor_db: cap_floor,
        lb_floor_db: lb_floor,
        lb_expected_db: expect,
        notes,
    })
}

// ------------------------------------------------------------------------------------
// Spectrum of the raw baseline samples (diagnostics)
// ------------------------------------------------------------------------------------

fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0usize;
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
        let ang = -2.0 * std::f64::consts::PI / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f64, 0.0f64);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let nr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = nr;
            }
        }
        len <<= 1;
    }
}

/// The strongest spectral peaks of `x` (Hann window, up to the largest power of two of samples, at most
/// 32768): (frequency Hz, level dBFS of a full-scale-equivalent sine amplitude). DC bin excluded.
pub fn spectrum_peaks(x: &[f32], rate: u32, count: usize) -> Vec<(f64, f64)> {
    let mut n = 1usize;
    while n * 2 <= x.len().min(32768) {
        n *= 2;
    }
    if n < 256 {
        return Vec::new();
    }
    let mean = x[..n].iter().map(|&v| f64::from(v)).sum::<f64>() / n as f64;
    let mut re: Vec<f64> = (0..n)
        .map(|i| (f64::from(x[i]) - mean) * (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos()))
        .collect();
    let mut im = vec![0.0f64; n];
    fft(&mut re, &mut im);
    // Coherent gain of the Hann window is 0.5: amplitude = 2 |X| / (n * 0.5).
    let mag: Vec<f64> = (0..n / 2).map(|k| 4.0 * (re[k] * re[k] + im[k] * im[k]).sqrt() / n as f64).collect();
    let mut peaks: Vec<(usize, f64)> = (2..mag.len() - 1).filter(|&k| mag[k] > mag[k - 1] && mag[k] >= mag[k + 1]).map(|k| (k, mag[k])).collect();
    peaks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    peaks.into_iter().take(count).map(|(k, m)| (k as f64 * f64::from(rate) / n as f64, lin_to_db(m))).collect()
}

/// Correlation coefficient of two channels and the largest absolute difference (linear).
pub fn channel_similarity(l: &[f32], r: &[f32]) -> (f64, f64) {
    let n = l.len().min(r.len());
    if n == 0 {
        return (0.0, 0.0);
    }
    let (ml, mr) = (l[..n].iter().map(|&v| f64::from(v)).sum::<f64>() / n as f64, r[..n].iter().map(|&v| f64::from(v)).sum::<f64>() / n as f64);
    let (mut sll, mut srr, mut slr, mut maxd) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for i in 0..n {
        let (a, b) = (f64::from(l[i]) - ml, f64::from(r[i]) - mr);
        sll += a * a;
        srr += b * b;
        slr += a * b;
        maxd = maxd.max((f64::from(l[i]) - f64::from(r[i])).abs());
    }
    let c = if sll > 0.0 && srr > 0.0 { slr / (sll * srr).sqrt() } else { 0.0 };
    (c, maxd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_block(m: &Monitor, which: usize, f: impl Fn(usize) -> f32, n: usize, blocks: usize) {
        for b in 0..blocks {
            let x: Vec<f32> = (0..n).map(|i| f(b * n + i)).collect();
            m.feed(which, &x, &x);
        }
    }

    fn reports(cap: impl Fn(usize) -> f32, lb: impl Fn(usize) -> f32) -> [ChannelReport; 4] {
        let m = Monitor::default();
        m.start();
        feed_block(&m, 0, cap, 480, 20);
        feed_block(&m, 1, lb, 256, 40);
        m.stop()
    }

    #[test]
    fn silence_and_dc_and_noise_are_classified() {
        let r = reports(|_| 0.0, |_| 0.0);
        assert_eq!(r[CH_LB_L].character, Character::DigitalSilence);
        assert_eq!(r[CH_LB_L].peak_db, -200.0);
        let dc = 0.001f32; // -60 dBFS
        let r = reports(|_| 0.0, |_| dc);
        assert_eq!(r[CH_LB_L].character, Character::DcOnly);
        assert!((r[CH_LB_L].dc_db + 60.0).abs() < 0.1);
        assert!(r[CH_LB_L].dev_peak_db < -150.0, "{}", r[CH_LB_L].dev_peak_db);
        // steady noise-like alternating signal around the DC
        let r = reports(|_| 0.0, |i| dc + if i % 2 == 0 { 1e-4 } else { -1e-4 });
        assert_eq!(r[CH_LB_L].character, Character::Steady);
        assert!((r[CH_LB_L].dev_peak_db + 80.0).abs() < 0.1, "{}", r[CH_LB_L].dev_peak_db);
        assert!((r[CH_LB_L].dc_db + 60.0).abs() < 0.1);
        // programme-like: bursts of 5 blocks every 10
        let r = reports(|_| 0.0, |i| if (i / 256) % 10 < 5 { 0.05 * ((i as f32) * 0.3).sin() } else { 1e-5 });
        assert_eq!(r[CH_LB_L].character, Character::TimeVarying);
        assert!(r[CH_LB_L].above_fraction[3] > 0.3 && r[CH_LB_L].above_fraction[3] < 0.7);
    }

    #[test]
    fn exceedance_fractions_count_blocks() {
        let m = Monitor::default();
        m.start();
        for b in 0..10 {
            let mut x = vec![0.0f32; 64];
            if b < 3 {
                x[5] = 10f32.powf(-75.0 / 20.0); // -75 dBFS: above -80 and -90 only
            }
            m.feed(1, &x, &x);
        }
        let r = m.stop();
        let a = r[CH_LB_L].above_fraction;
        assert!((a[0] - 0.3).abs() < 1e-9 && (a[1] - 0.3).abs() < 1e-9 && a[2] == 0.0 && a[3] == 0.0, "{a:?}");
        assert_eq!(r[CH_CAP_L].blocks, 0);
    }

    #[test]
    fn inactive_monitor_after_stop_keeps_data_until_restart() {
        let m = Monitor::default();
        m.start();
        m.feed(0, &[0.5; 8], &[0.25; 8]);
        let a = m.stop();
        assert!((a[CH_CAP_L].peak_db + 6.02).abs() < 0.01 && (a[CH_CAP_R].peak_db + 12.04).abs() < 0.01);
        assert!(!m.is_active());
        m.start();
        assert_eq!(m.report()[CH_CAP_L].blocks, 0);
    }

    fn quiet() -> [ChannelReport; 4] {
        reports(|_| 0.0, |_| 0.0)
    }

    #[test]
    fn thresholds_on_a_silent_baseline_are_the_fixed_offsets() {
        let t = plan_thresholds(&quiet(), None, 0.0, -40.0, -20.0, None, None).unwrap();
        assert!((t.cap_db + 60.0).abs() < 1e-3);
        assert!((t.lb_db + 85.0).abs() < 1e-3); // -60 - 25
        assert_eq!(t.lb_expected_db, -60.0);
    }

    #[test]
    fn thresholds_follow_the_floor_and_remove_dc() {
        // DC at -50 dBFS (would have crossed any fixed -85 threshold) plus +-1e-5 (-100 dBFS) noise.
        let dc = 10f32.powf(-50.0 / 20.0);
        let r = reports(|_| 0.0, |i| dc + if i % 2 == 0 { 1e-5 } else { -1e-5 });
        let t = plan_thresholds(&r, None, 0.0, -40.0, -20.0, None, None).unwrap();
        assert!((t.lb_db + 85.0).abs() < 0.2, "{}", t.lb_db); // floor -100 + 12 = -88 < -85: the lower bound wins
        assert!((t.dc[CH_LB_L] - dc).abs() < 1e-7);
        // noise floor of -75 dBFS: threshold -63 (<= -66?) -> too high for a -40 dBFS click
        let n = 10f32.powf(-75.0 / 20.0);
        let r = reports(|_| 0.0, |i| if i % 2 == 0 { n } else { -n });
        let e = plan_thresholds(&r, None, 0.0, -40.0, -20.0, None, None).unwrap_err();
        assert!(e.contains("too high") && e.contains("loopback"), "{e}");
        // ... but fine for a -35 dBFS click (expected -55, threshold -63 <= -61)
        let t = plan_thresholds(&r, None, 0.0, -35.0, -20.0, None, None).unwrap();
        assert!((t.lb_db + 63.0).abs() < 0.2, "{}", t.lb_db);
    }

    #[test]
    fn programme_audio_aborts_with_the_detector_named() {
        let loud = |i: usize| 0.05 * ((i as f32) * 0.2).sin(); // -26 dBFS
        let e = plan_thresholds(&reports(|_| 0.0, loud), None, 0.0, -40.0, -20.0, None, None).unwrap_err();
        assert!(e.contains("LOOPBACK") && e.contains("not quiet"), "{e}");
        let e = plan_thresholds(&reports(|i| 0.002 * ((i as f32) * 0.2).sin(), |_| 0.0), None, 0.0, -40.0, -20.0, None, None).unwrap_err();
        assert!(e.contains("CAPTURE") && e.contains("not quiet"), "{e}");
        let e = plan_thresholds(&[ChannelReport::default(), ChannelReport::default(), ChannelReport::default(), ChannelReport::default()], None, 0.0, -40.0, -20.0, None, None).unwrap_err();
        assert!(e.contains("no data"));
    }

    #[test]
    fn band_limited_detector_tolerates_quiet_programme_audio_but_not_loud() {
        let hp_quiet = reports(|_| 0.0, |i| if i % 2 == 0 { 1e-5 } else { -1e-5 }); // -100 dBFS "after the high-pass"
        let hp = [hp_quiet[CH_LB_L].clone(), hp_quiet[CH_LB_R].clone()];
        let mic = |i: usize| 0.02 * ((i as f32) * 0.05).sin() * (1.0 + 0.5 * ((i as f32) * 0.0005).sin()); // about -34 dBFS peak
        let raw = reports(|_| 0.0, mic);
        assert!(plan_thresholds(&raw, None, 0.0, -35.0, -20.0, None, None).is_err());
        let t = plan_thresholds(&raw, Some(&hp), -4.0, -35.0, -20.0, None, None).unwrap();
        assert!(t.notes.iter().any(|n| n.contains("microphone")), "{:?}", t.notes);
        let loud = reports(|_| 0.0, |i| 0.3 * ((i as f32) * 0.05).sin());
        assert!(plan_thresholds(&loud, Some(&hp), -4.0, -35.0, -20.0, None, None).is_err());
    }

    #[test]
    fn relaxed_loopback_limit_needs_explicit_permission() {
        let hp_quiet = reports(|_| 0.0, |i| if i % 2 == 0 { 1e-5 } else { -1e-5 });
        let hp = [hp_quiet[CH_LB_L].clone(), hp_quiet[CH_LB_R].clone()];
        let mic = |i: usize| 0.02 * ((i as f32) * 0.05).sin() * (1.0 + 0.5 * ((i as f32) * 0.0005).sin());
        let raw = reports(|_| 0.0, mic);
        let ok = plan_thresholds_checked(&raw, Some(&hp), -4.0, -35.0, -20.0, None, None, true).unwrap();
        assert!(ok.notes.iter().any(|n| n.contains("SAFETY NOTE") && n.contains("dBFS") && n.contains("raw peak")), "{:?}", ok.notes);
        // same data, relaxation not allowed (low burst / chirp with a manual --hp-hz): -40 dBFS applies
        assert!(plan_thresholds_checked(&raw, Some(&hp), -4.0, -35.0, -20.0, None, None, false).is_err());
    }

    #[test]
    fn overrides_are_honoured_and_checked() {
        let t = plan_thresholds(&quiet(), None, 0.0, -35.0, -20.0, Some(-55.0), Some(-62.0)).unwrap();
        assert_eq!((t.cap_db, t.lb_db), (-55.0, -62.0));
        assert!(plan_thresholds(&quiet(), None, 0.0, -40.0, -20.0, None, Some(-50.0)).is_err()); // above click - 6
    }

    #[test]
    fn spectrum_finds_a_tone_and_similarity_a_copy() {
        let rate = 48000u32;
        let x: Vec<f32> = (0..32768).map(|i| 0.01 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / rate as f32).sin() + 1e-5).collect();
        let p = spectrum_peaks(&x, rate, 3);
        assert!((p[0].0 - 1000.0).abs() < 5.0, "{p:?}");
        assert!((p[0].1 + 40.0).abs() < 1.5, "{p:?}");
        let (c, d) = channel_similarity(&x, &x);
        assert!((c - 1.0).abs() < 1e-9 && d == 0.0);
        let neg: Vec<f32> = x.iter().map(|v| -v).collect();
        assert!(channel_similarity(&x, &neg).0 < -0.99);
        assert!(spectrum_peaks(&x[..100], rate, 3).is_empty());
    }

    /// A mic-like broadband loopback at about -46 dBFS rms with a hard cut at 12 kHz: a -35 dBFS click
    /// at 18.5 kHz is detectable after the high-pass, while the same floor blocks a 1.5 kHz click.
    #[test]
    fn highpass_sees_through_band_limited_programme_audio() {
        let rate = 48000u32;
        // broadband noise below 12 kHz (sum of many sines), about -46 dBFS rms
        let tones: Vec<(f32, f32)> = (0..60).map(|k| (150.0 + k as f32 * 190.0, k as f32 * 1.7)).collect();
        let amp = 10f32.powf(-46.0 / 20.0) * (2.0f32 / 60.0).sqrt();
        let noise = |i: usize| -> f32 { tones.iter().map(|(f, ph)| amp * (2.0 * std::f32::consts::PI * f * i as f32 / rate as f32 + ph).sin()).sum() };
        let m = Monitor::default();
        m.start();
        let mut hp = HighPass::new(16000.0, rate);
        let mut fl = vec![0.0f32; 256];
        for b in 0..80 {
            let x: Vec<f32> = (0..256).map(|i| noise(b * 256 + i)).collect();
            hp.process_env(0, &x, &mut fl);
            hp.process_env(1, &x, &mut fl);
            m.feed(0, &fl, &fl);
            m.feed(1, &x, &x);
            if b >= 2 {
                m.feed(2, &fl, &fl);
            }
        }
        let (raw, f) = (m.report(), m.report_hp());
        assert!(raw[CH_LB_L].dev_peak_db > -40.0, "{}", raw[CH_LB_L].dev_peak_db);
        // (the raw floor of this synthetic noise exceeds the programme limit, so test the planner on a calmer copy)
        let mut calm = raw.clone();
        for c in calm.iter_mut().skip(2) {
            c.dev_peak_db = -45.0;
            c.peak_db = -45.0;
        }
        for c in calm.iter_mut().take(2) {
            c.peak_db = -200.0;
            c.dev_peak_db = -200.0;
        }
        assert!(f[0].peak_db < -85.0, "high-passed floor {}", f[0].peak_db);
        assert!(plan_thresholds(&calm, None, 0.0, -35.0, -20.0, None, None).is_err());
        let t = plan_thresholds(&calm, Some(&f), -3.0, -35.0, -20.0, None, None).unwrap();
        assert!(t.lb_db <= -64.0 && t.lb_floor_db < -85.0, "{t:?}");
    }

    #[test]
    fn highpass_passes_the_click_band_and_blocks_the_rest() {
        let rate = 48000u32;
        let gain = |f: f32| -> f64 {
            let mut hp = HighPass::new(16000.0, rate);
            let x: Vec<f32> = (0..9600).map(|i| (2.0 * std::f32::consts::PI * f * i as f32 / rate as f32).sin()).collect();
            let mut y = vec![0.0f32; x.len()];
            hp.process(0, &x, &mut y);
            f64::from(y[4800..].iter().fold(0.0f32, |m, v| m.max(v.abs())))
        };
        assert!(gain(18500.0) > 0.95, "{}", gain(18500.0));
        assert!(gain(10000.0) < 0.01, "{}", gain(10000.0));
        assert!(gain(1500.0) < 1e-4, "{}", gain(1500.0));
        assert_eq!(auto_highpass_hz(14000.0), None);
        assert_eq!(auto_highpass_hz(17500.0), Some(15750.0));
    }

    #[test]
    fn rare_outliers_do_not_set_the_floor() {
        let m = Monitor::default();
        m.start();
        for b in 0..400 {
            let mut x: Vec<f32> = (0..256).map(|i| if i % 2 == 0 { 1e-5 } else { -1e-5 }).collect(); // -100 dBFS
            if b == 200 {
                x[10] = 10f32.powf(-60.0 / 20.0); // one -60 dBFS spike in 400 blocks
            }
            m.feed(1, &x, &x);
            m.feed(0, &[0.0; 16], &[0.0; 16]);
        }
        let r = m.stop();
        assert!((r[CH_LB_L].dev_peak_db + 60.0).abs() < 0.1);
        assert!(r[CH_LB_L].floor_dev_db < -95.0, "{}", r[CH_LB_L].floor_dev_db);
        assert!(r[CH_LB_L].floor_peak_db < -95.0);
        let t = plan_thresholds(&r, None, 0.0, -40.0, -20.0, None, None).unwrap();
        assert!((t.lb_db + 85.0).abs() < 0.1, "{}", t.lb_db); // lower bound (-60 - 25); the spike is above it and will be rejected by its amplitude
        assert_eq!(percentile(&[], 0.99), 0.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 0.5), 2.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 1.0), 4.0);
    }
}
