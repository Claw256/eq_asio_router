//! The EQ stage and the lock-free config handoff to the audio thread.
//!
//! Handoff design (no allocation, locks or frees on the audio thread):
//! * The watcher thread builds a [`Snapshot`] (sanitised profile, [`Settings`] and the
//!   auto-headroom gain, all computed off the audio thread) and `publish`es it into a
//!   single `AtomicPtr` slot. An unconsumed older snapshot is replaced and freed by the
//!   watcher.
//! * The audio thread `take`s the slot with one atomic swap (wait-free), applies it, keeps
//!   it as the *current* snapshot and `retire`s the previous one into a small fixed array
//!   of `AtomicPtr` slots (wait-free, bounded; leaks rather than frees if full).
//! * The watcher `collect`s (frees) retired snapshots on its next tick.
//!
//! Filter state lives in the audio thread's single [`StereoEq`]; a new profile is applied
//! with `StereoEq::set_profile` in place (coefficients only, state kept -> no click).

use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

use eq_core::{
    auto_headroom_db, Config, EqProfile, FaultDetector, FaultState, GainRamp, JblParams, StereoEq, BYPASS_SCRATCH_FRAMES,
};

/// Realtime-relevant settings derived from a [`Config`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub bypass: bool,
    /// AutoEQ stage (preamp and bands) on; JBL Bass Mode and the limiter are unaffected.
    pub eq_enabled: bool,
    pub limiter: bool,
    /// Fade-mute the output while the fault detector reports `Muted`.
    pub mute_on_fault: bool,
    /// Informational here: the gain itself travels in [`Snapshot::extra_gain_db`].
    pub auto_headroom: bool,
    /// Master output channels that receive the EQ'd L/R pair (validated).
    pub outputs: [usize; 2],
    pub passthrough_inputs: bool,
    /// "JBL Bass Mode" on/off and intensity (0..=100).
    pub jbl_bass_mode: bool,
    pub jbl_intensity: u8,
    /// Detailed JBL Bass Mode stage parameters.
    pub jbl: JblParams,
}

impl Default for Settings {
    fn default() -> Self {
        let c = Config::default();
        Settings {
            bypass: false,
            eq_enabled: true,
            limiter: true,
            mute_on_fault: true,
            auto_headroom: true,
            outputs: [0, 1],
            passthrough_inputs: true,
            jbl_bass_mode: c.jbl_bass_mode,
            jbl_intensity: c.jbl_intensity,
            jbl: c.jbl,
        }
    }
}

/// Validates the configured output pair against the master's output count:
/// both must exist and differ, otherwise fall back to `[0, 1]`.
pub fn validate_output_channels(req: [u32; 2], n_out: usize) -> [usize; 2] {
    let (a, b) = (req[0] as usize, req[1] as usize);
    if a < n_out && b < n_out && a != b {
        [a, b]
    } else {
        [0, 1]
    }
}

impl Settings {
    pub fn from_config(cfg: &Config, n_out: usize) -> Self {
        Settings {
            bypass: cfg.bypass,
            eq_enabled: cfg.eq_enabled,
            limiter: cfg.limiter,
            mute_on_fault: cfg.mute_on_fault,
            auto_headroom: cfg.auto_headroom,
            outputs: validate_output_channels(cfg.output_channels, n_out),
            passthrough_inputs: cfg.passthrough_inputs,
            jbl_bass_mode: cfg.jbl_bass_mode,
            jbl_intensity: cfg.jbl_intensity,
            jbl: cfg.jbl,
        }
    }
}

/// What the watcher hands to the audio thread.
pub struct Snapshot {
    pub profile: EqProfile,
    pub settings: Settings,
    /// Extra gain in dB (<= 0) from [`auto_headroom_db`] at `sample_rate_hz`, or 0.
    pub extra_gain_db: f64,
    /// Sample rate the headroom was computed for.
    pub sample_rate_hz: u32,
}

impl Snapshot {
    /// Builds a snapshot (allocates and does FFT-free but non-trivial math: control thread only).
    /// The config should already be sanitised.
    pub fn from_config(cfg: &Config, n_out: usize, sample_rate_hz: u32) -> Box<Snapshot> {
        let extra = if cfg.auto_headroom && sample_rate_hz > 0 {
            auto_headroom_db(&cfg.profile, f64::from(sample_rate_hz))
        } else {
            0.0
        };
        Box::new(Snapshot {
            profile: cfg.profile.clone(),
            settings: Settings::from_config(cfg, n_out),
            extra_gain_db: extra,
            sample_rate_hz,
        })
    }
}

const RETIRE_SLOTS: usize = 8;

/// Single-slot publish + bounded retire list, generic over the payload.
pub struct Handoff<T> {
    pending: AtomicPtr<T>,
    retired: [AtomicPtr<T>; RETIRE_SLOTS],
}

// SAFETY: ownership of the boxed values is transferred through atomic swaps only.
unsafe impl<T: Send> Send for Handoff<T> {}
unsafe impl<T: Send> Sync for Handoff<T> {}

impl<T> Default for Handoff<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Handoff<T> {
    pub fn new() -> Self {
        Handoff {
            pending: AtomicPtr::new(core::ptr::null_mut()),
            retired: core::array::from_fn(|_| AtomicPtr::new(core::ptr::null_mut())),
        }
    }

    /// Producer (watcher thread). Replaces, and frees, any unconsumed snapshot.
    pub fn publish(&self, v: Box<T>) {
        let old = self.pending.swap(Box::into_raw(v), Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: the swap transferred sole ownership of `old` to us.
            drop(unsafe { Box::from_raw(old) });
        }
    }

    /// Consumer (audio thread). Wait-free; never frees.
    #[inline]
    pub fn take(&self) -> Option<Box<T>> {
        if self.pending.load(Ordering::Relaxed).is_null() {
            return None;
        }
        let p = self.pending.swap(core::ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: non-null pointers in `pending` come from Box::into_raw in `publish`.
        (!p.is_null()).then(|| unsafe { Box::from_raw(p) })
    }

    /// Consumer (audio thread): hand a used snapshot back for freeing off-thread.
    /// Wait-free (bounded scan). If every slot is occupied the value is leaked
    /// instead of being freed on the audio thread.
    pub fn retire(&self, v: Box<T>) {
        let p = Box::into_raw(v);
        for s in &self.retired {
            if s.compare_exchange(core::ptr::null_mut(), p, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                return;
            }
        }
        // Leak (documented fallback): a free here would be an audio-thread dealloc.
    }

    /// Producer: frees everything retired so far. Returns how many were freed.
    pub fn collect(&self) -> usize {
        let mut n = 0;
        for s in &self.retired {
            if s.load(Ordering::Relaxed).is_null() {
                continue;
            }
            let p = s.swap(core::ptr::null_mut(), Ordering::AcqRel);
            if !p.is_null() {
                // SAFETY: ownership transferred by the swap.
                drop(unsafe { Box::from_raw(p) });
                n += 1;
            }
        }
        n
    }
}

impl<T> Drop for Handoff<T> {
    fn drop(&mut self) {
        let p = *self.pending.get_mut();
        if !p.is_null() {
            // SAFETY: exclusive access in Drop.
            drop(unsafe { Box::from_raw(p) });
        }
        for s in &mut self.retired {
            let p = *s.get_mut();
            if !p.is_null() {
                // SAFETY: as above.
                drop(unsafe { Box::from_raw(p) });
            }
        }
    }
}

/// Max absolute sample of a block (0.0 for empty; NaN ignored).
#[inline]
pub fn peak(buf: &[f32]) -> f32 {
    buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()))
}

/// Per-block health inputs measured by the caller (the engine).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BlockStats {
    /// Fraction (0..1) of this block's output that came from an underrun (hold/fade/silence).
    pub underrun_ratio: f32,
    /// Capture-gap ratio: how late/early capture data arrives relative to expectation (0 = ideal).
    pub gap_ratio: f32,
}

/// Crossfade length (seconds) between the old and the new filters on a hot reload.
const XFADE_SECS: f64 = 0.010;

/// Fast mute fade when a fault is detected (frames).
const MUTE_FADE_FRAMES: u32 = 64;
/// Fade back in after recovery, seconds.
const UNMUTE_FADE_SECS: f64 = 0.02;

/// The audio-thread processor: [`StereoEq`] + hot-reloadable settings + fault muting.
pub struct EqProcessor {
    eq: StereoEq,
    /// Copy of the EQ as it was before the last hot reload, run in parallel during the crossfade.
    old_eq: StereoEq,
    xf_l: Box<[f32]>,
    xf_r: Box<[f32]>,
    xf_done: usize,
    xf_total: usize,
    fs: f64,
    settings: Settings,
    current: Box<Snapshot>,
    handoff: Arc<Handoff<Snapshot>>,
    detector: FaultDetector,
    mute: GainRamp,
    muting: bool,
    non_finite_total: u64,
}

impl EqProcessor {
    /// Control-thread constructor (allocates).
    pub fn new(sample_rate: f64, cfg: &Config, n_out: usize, handoff: Arc<Handoff<Snapshot>>) -> Self {
        let snap = Snapshot::from_config(cfg, n_out, sample_rate.round() as u32);
        let mut eq = StereoEq::new(sample_rate);
        eq.set_profile(&snap.profile);
        eq.set_extra_gain_db(snap.extra_gain_db);
        eq.set_limiter(snap.settings.limiter);
        eq.set_eq_enabled(snap.settings.eq_enabled);
        eq.set_bass_fx(snap.settings.jbl_bass_mode, snap.settings.jbl_intensity, &snap.settings.jbl);
        EqProcessor {
            eq,
            old_eq: StereoEq::new(sample_rate),
            xf_l: vec![0.0; BYPASS_SCRATCH_FRAMES].into_boxed_slice(),
            xf_r: vec![0.0; BYPASS_SCRATCH_FRAMES].into_boxed_slice(),
            xf_done: 0,
            xf_total: 0,
            fs: sample_rate,
            settings: snap.settings,
            current: snap,
            handoff,
            detector: FaultDetector::default(),
            mute: GainRamp::new(1.0),
            muting: false,
            non_finite_total: 0,
        }
    }

    /// Changes the sample rate: retunes the filters (state kept) and recomputes the
    /// auto-headroom gain. Allocates; call from the control thread while the audio
    /// callback is not running (stream stopped).
    pub fn set_sample_rate(&mut self, fs: f64) {
        if !(fs.is_finite() && fs > 0.0) {
            return;
        }
        self.fs = fs;
        self.xf_done = self.xf_total;
        self.eq.set_sample_rate(fs);
        self.old_eq.set_sample_rate(fs);
        let extra = if self.settings.auto_headroom { auto_headroom_db(&self.current.profile, fs) } else { 0.0 };
        self.eq.set_extra_gain_db(extra);
    }

    /// Current sample rate.
    pub fn sample_rate(&self) -> f64 {
        self.fs
    }

    /// Current settings.
    pub fn settings(&self) -> Settings {
        self.settings
    }

    /// Current fault state.
    pub fn fault_state(&self) -> FaultState {
        self.detector.state()
    }

    /// True while the output is faded to silence by the fault logic.
    pub fn is_muted(&self) -> bool {
        self.muting
    }

    /// Non-finite samples sanitised since creation.
    pub fn non_finite_total(&self) -> u64 {
        self.non_finite_total
    }

    /// Applies a pending snapshot, if any. Wait-free, allocation-free. Called by `process`.
    pub fn update(&mut self) {
        if let Some(snap) = self.handoff.take() {
            let was_bypassed = self.settings.bypass;
            // A coefficient switch (and a removed band, whose state is dropped) steps the waveform:
            // keep the old filters running and crossfade to the new ones over 10 ms.
            if self.xf_done >= self.xf_total && !was_bypassed && !snap.settings.bypass {
                self.old_eq.copy_state_from(&self.eq);
                self.xf_total = ((XFADE_SECS * self.fs) as usize).clamp(1, BYPASS_SCRATCH_FRAMES);
                self.xf_done = 0;
            }
            self.eq.set_profile(&snap.profile);
            self.eq.set_extra_gain_db(snap.extra_gain_db);
            self.eq.set_limiter(snap.settings.limiter);
            self.eq.set_eq_enabled(snap.settings.eq_enabled);
            self.eq.set_bass_fx(snap.settings.jbl_bass_mode, snap.settings.jbl_intensity, &snap.settings.jbl);
            if was_bypassed && !snap.settings.bypass {
                self.eq.reset(); // state is stale after a bypassed stretch
            }
            self.settings = snap.settings;
            let old = std::mem::replace(&mut self.current, snap);
            self.handoff.retire(old);
        }
    }

    /// Processes one stereo block in place and returns the fault state after it.
    ///
    /// Applies a pending config snapshot first, then the EQ (with click-free bypass),
    /// feeds the fault detector (block peak, non-finite count, `stats`), and fades the
    /// output to silence while `Muted` if `mute_on_fault` is set. Lock-free and
    /// allocation-free.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32], stats: BlockStats) -> FaultState {
        self.update();
        let n = l.len().min(r.len());
        if n == 0 {
            return self.detector.state();
        }
        let (l, r) = (&mut l[..n], &mut r[..n]);
        let peak_in = peak(l).max(peak(r));
        let xf = (self.xf_total - self.xf_done).min(n);
        if xf > 0 {
            self.xf_l[..xf].copy_from_slice(&l[..xf]);
            self.xf_r[..xf].copy_from_slice(&r[..xf]);
            self.old_eq.process_stereo(&mut self.xf_l[..xf], &mut self.xf_r[..xf]);
        }
        self.eq.process_stereo_with_bypass_ramp(l, r, self.settings.bypass);
        for i in 0..xf {
            let w = (self.xf_done + i + 1) as f32 / self.xf_total as f32;
            l[i] = self.xf_l[i] + w * (l[i] - self.xf_l[i]);
            r[i] = self.xf_r[i] + w * (r[i] - self.xf_r[i]);
        }
        self.xf_done += xf;
        let nf = self.eq.non_finite_count();
        self.eq.reset_non_finite_count();
        self.non_finite_total += u64::from(nf);
        let peak_out = peak(l).max(peak(r));
        let state = self.detector.update(peak_in.max(peak_out), nf, stats.underrun_ratio, stats.gap_ratio);

        let want_mute = self.settings.mute_on_fault && state == FaultState::Muted;
        if want_mute != self.muting {
            self.muting = want_mute;
            if want_mute {
                self.mute.set_target(0.0, MUTE_FADE_FRAMES);
            } else {
                self.mute.set_target(1.0, (UNMUTE_FADE_SECS * self.fs) as u32);
            }
        }
        if self.mute.is_ramping() || self.mute.gain() != 1.0 {
            self.mute.apply_stereo(l, r);
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eq_core::{FilterKind, FilterSpec};

    const FS: f64 = 48000.0;

    fn pk6(fc: f64) -> EqProfile {
        EqProfile {
            name: "t".into(),
            preamp_db: 0.0,
            filters: vec![FilterSpec { kind: FilterKind::Peaking, fc_hz: fc, gain_db: 6.0, q: 1.0, enabled: true }],
        }
    }

    fn cfg(p: EqProfile) -> Config {
        Config { profile: p, limiter: false, auto_headroom: false, ..Config::default() }
    }

    fn sine(n: usize, f: f64, fs: f64, amp: f32) -> Vec<f32> {
        (0..n).map(|i| amp * (2.0 * std::f64::consts::PI * f * i as f64 / fs).sin() as f32).collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// Runs a sine through the processor in 256-frame blocks; RMS gain of the last half.
    fn run(p: &mut EqProcessor, f: f64, fs: f64, blocks: usize) -> f64 {
        let n = 256;
        let src = sine(n * blocks, f, fs, 0.25);
        let mut out = Vec::new();
        for ch in src.chunks(n) {
            let (mut l, mut r) = (ch.to_vec(), ch.to_vec());
            p.process(&mut l, &mut r, BlockStats::default());
            out.extend_from_slice(&l);
        }
        rms(&out[out.len() / 2..]) / rms(&src[src.len() / 2..])
    }

    #[test]
    fn output_channel_validation() {
        assert_eq!(validate_output_channels([2, 3], 6), [2, 3]);
        assert_eq!(validate_output_channels([3, 2], 6), [3, 2]);
        assert_eq!(validate_output_channels([4, 6], 6), [0, 1]);
        assert_eq!(validate_output_channels([1, 1], 6), [0, 1]);
        assert_eq!(validate_output_channels([0, 1], 2), [0, 1]);
        assert_eq!(validate_output_channels([u32::MAX, 0], 8), [0, 1]);
    }

    #[test]
    fn settings_selection() {
        let c = Config {
            bypass: true,
            limiter: false,
            output_channels: [2, 9],
            passthrough_inputs: false,
            mute_on_fault: false,
            ..Config::default()
        };
        let s = Settings::from_config(&c, 4);
        assert!(s.bypass && !s.limiter && !s.passthrough_inputs && !s.mute_on_fault);
        assert_eq!(s.outputs, [0, 1]);
        let s = Settings::from_config(&Config::default(), 4);
        assert!(!s.bypass && s.limiter && s.passthrough_inputs && s.mute_on_fault && s.auto_headroom);
    }

    #[test]
    fn peak_computation() {
        assert_eq!(peak(&[]), 0.0);
        assert_eq!(peak(&[0.1, -0.7, 0.3]), 0.7);
        assert_eq!(peak(&[f32::NAN, 0.2]), 0.2);
    }

    #[test]
    fn offline_plus6db_peaking_at_1khz() {
        let h = Arc::new(Handoff::new());
        let mut p = EqProcessor::new(FS, &cfg(pk6(1000.0)), 2, h);
        let gain = run(&mut p, 1000.0, FS, 200);
        let expect = 10f64.powf(6.0 / 20.0);
        assert!((gain - expect).abs() < 0.02, "gain {gain}, expected {expect}");
        let mut p2 = EqProcessor::new(FS, &cfg(pk6(1000.0)), 2, Arc::new(Handoff::new()));
        assert!((run(&mut p2, 100.0, FS, 200) - 1.0).abs() < 0.06);
    }

    #[test]
    fn auto_headroom_cancels_boost() {
        let mut c = cfg(pk6(1000.0));
        c.auto_headroom = true;
        let mut p = EqProcessor::new(FS, &c, 2, Arc::new(Handoff::new()));
        let g = run(&mut p, 1000.0, FS, 200);
        assert!(g > 0.97 && g <= 1.01, "max boost brought to ~0 dB, got {g}");
        // Sample-rate change recomputes headroom and retunes the filter.
        p.set_sample_rate(96000.0);
        assert_eq!(p.sample_rate(), 96000.0);
        let g = run(&mut p, 1000.0, 96000.0, 400);
        assert!(g > 0.95 && g <= 1.02, "after 96 kHz switch got {g}");
    }

    #[test]
    fn bypass_ramps_to_identity() {
        let mut c = cfg(pk6(1000.0));
        c.bypass = true;
        let mut p = EqProcessor::new(FS, &c, 2, Arc::new(Handoff::new()));
        // starts bypassed: wet ramps from 1 to 0 within 10 ms, then identity
        assert!((run(&mut p, 1000.0, FS, 100) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn hot_reload_via_handoff() {
        let h = Arc::new(Handoff::new());
        let mut p = EqProcessor::new(FS, &cfg(EqProfile::default()), 2, h.clone());
        assert!((run(&mut p, 1000.0, FS, 100) - 1.0).abs() < 1e-3);
        h.publish(Snapshot::from_config(&cfg(pk6(1000.0)), 2, 48000));
        let g = run(&mut p, 1000.0, FS, 200);
        assert!((g - 1.9953).abs() < 0.02, "{g}");
        // The consumed snapshot is held as `current`; the initial one was retired.
        assert_eq!(h.collect(), 1);
        assert_eq!(h.collect(), 0);
        let mut c = cfg(pk6(1000.0));
        c.bypass = true;
        h.publish(Snapshot::from_config(&c, 2, 48000));
        assert!((run(&mut p, 1000.0, FS, 100) - 1.0).abs() < 1e-3);
        assert!(p.settings().bypass);
    }

    #[test]
    fn hot_reload_carries_headroom() {
        let h = Arc::new(Handoff::new());
        let mut p = EqProcessor::new(FS, &cfg(pk6(1000.0)), 2, h.clone());
        let mut c = cfg(pk6(1000.0));
        c.auto_headroom = true;
        h.publish(Snapshot::from_config(&c, 2, 48000));
        let g = run(&mut p, 1000.0, FS, 200);
        assert!(g > 0.97 && g <= 1.01, "{g}");
    }

    #[test]
    fn handoff_latest_wins_and_frees_unconsumed() {
        let h: Handoff<Arc<()>> = Handoff::new();
        let token = Arc::new(());
        h.publish(Box::new(token.clone()));
        h.publish(Box::new(token.clone())); // first one freed by publisher
        assert_eq!(Arc::strong_count(&token), 2);
        let got = h.take().unwrap();
        assert!(h.take().is_none());
        h.retire(got);
        assert_eq!(Arc::strong_count(&token), 2);
        assert_eq!(h.collect(), 1);
        assert_eq!(Arc::strong_count(&token), 1);
        h.publish(Box::new(token.clone()));
        drop(h);
        assert_eq!(Arc::strong_count(&token), 1);
    }

    #[test]
    fn retire_overflow_leaks_instead_of_blocking() {
        let h: Handoff<u32> = Handoff::new();
        for i in 0..(RETIRE_SLOTS as u32 + 3) {
            h.retire(Box::new(i));
        }
        assert_eq!(h.collect(), RETIRE_SLOTS);
    }

    #[test]
    fn handoff_across_threads() {
        let h = Arc::new(Handoff::<u64>::new());
        let h2 = h.clone();
        let producer = std::thread::spawn(move || {
            for i in 1..=2000u64 {
                h2.publish(Box::new(i));
                h2.collect();
            }
        });
        let mut last = 0;
        let mut seen = 0;
        while last < 2000 && seen < 50_000_000 {
            if let Some(v) = h.take() {
                assert!(*v > last, "values must be monotonically newer");
                last = *v;
                h.retire(v);
            }
            seen += 1;
        }
        producer.join().unwrap();
        if let Some(v) = h.take() {
            last = *v;
        }
        assert_eq!(last, 2000);
    }

    fn proc_plain() -> EqProcessor {
        EqProcessor::new(FS, &cfg(EqProfile::default()), 2, Arc::new(Handoff::new()))
    }

    #[test]
    fn huge_peak_mutes_and_recovers() {
        let mut p = proc_plain();
        let (mut l, mut r) = (vec![0.1f32; 256], vec![0.1f32; 256]);
        assert_eq!(p.process(&mut l, &mut r, BlockStats::default()), FaultState::Ok);
        assert!(l.iter().all(|&x| (x - 0.1).abs() < 1e-6));
        // +40 dBFS burst
        let (mut l, mut r) = (vec![100.0f32; 256], vec![100.0f32; 256]);
        assert_eq!(p.process(&mut l, &mut r, BlockStats::default()), FaultState::Muted);
        assert!(p.is_muted());
        assert!(l[255].abs() < 1.0, "fade reaches silence quickly, got {}", l[255]);
        // clean blocks while muted: silent output
        let (mut l, mut r) = (vec![0.1f32; 256], vec![0.1f32; 256]);
        p.process(&mut l, &mut r, BlockStats::default());
        assert!(l.iter().all(|&x| x == 0.0));
        // recover: 100 clean blocks -> Warn (unmute), 50 more -> Ok
        let mut state = FaultState::Muted;
        for _ in 0..100 {
            let (mut l, mut r) = (vec![0.1f32; 256], vec![0.1f32; 256]);
            state = p.process(&mut l, &mut r, BlockStats::default());
        }
        assert_eq!(state, FaultState::Warn);
        assert!(!p.is_muted());
        for _ in 0..60 {
            let (mut l, mut r) = (vec![0.1f32; 256], vec![0.1f32; 256]);
            state = p.process(&mut l, &mut r, BlockStats::default());
        }
        assert_eq!(state, FaultState::Ok);
        let (mut l, mut r) = (vec![0.1f32; 256], vec![0.1f32; 256]);
        p.process(&mut l, &mut r, BlockStats::default());
        assert!((l[255] - 0.1).abs() < 1e-6, "fully faded back in");
    }

    #[test]
    fn nan_injection_is_sanitised_counted_and_mutes_in_bulk() {
        let mut p = proc_plain();
        let (mut l, mut r) = (vec![0.1f32; 256], vec![0.1f32; 256]);
        l[10] = f32::NAN;
        let st = p.process(&mut l, &mut r, BlockStats::default());
        assert_eq!(st, FaultState::Warn);
        assert!(l.iter().all(|x| x.is_finite()));
        assert_eq!(p.non_finite_total(), 1);
        assert!(!p.is_muted());
        let (mut l, mut r) = (vec![f32::INFINITY; 256], vec![0.1f32; 256]);
        assert_eq!(p.process(&mut l, &mut r, BlockStats::default()), FaultState::Muted);
        assert!(l.iter().all(|x| x.is_finite()));
        assert_eq!(p.non_finite_total(), 257);
    }

    #[test]
    fn underrun_and_gap_ratios_drive_the_detector() {
        let mut p = proc_plain();
        let (mut l, mut r) = (vec![0.1f32; 256], vec![0.1f32; 256]);
        let st = p.process(&mut l, &mut r, BlockStats { underrun_ratio: 0.05, gap_ratio: 0.0 });
        assert_eq!(st, FaultState::Warn);
        let st = p.process(&mut l, &mut r, BlockStats { underrun_ratio: 0.0, gap_ratio: 0.5 });
        assert_eq!(st, FaultState::Muted);
        assert!(p.is_muted());
    }

    #[test]
    fn mute_on_fault_off_only_reports() {
        let mut c = cfg(EqProfile::default());
        c.mute_on_fault = false;
        let mut p = EqProcessor::new(FS, &c, 2, Arc::new(Handoff::new()));
        let (mut l, mut r) = (vec![100.0f32; 256], vec![100.0f32; 256]);
        assert_eq!(p.process(&mut l, &mut r, BlockStats::default()), FaultState::Muted);
        assert!(!p.is_muted());
        assert_eq!(l[255], 100.0);
    }

    /// Music-like test signal (128 BPM kick, sub-bass line, chord, noisy hats), mastered to `rms_db`
    /// dBFS RMS with a gentle `tanh` ceiling at 0.98 so hot levels stay below full scale.
    fn music(n: usize, fs: f64, rms_db: f64, seed: u64) -> Vec<f32> {
        use std::f64::consts::PI;
        let beat = 60.0 / 128.0;
        let mut s = seed;
        let mut noise = move || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        };
        let notes = [55.0, 55.0, 65.41, 49.0];
        let raw: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let tk = t % beat;
                let kick = (-tk * 9.0).exp() * (2.0 * PI * (48.0 * tk + 90.0 * (1.0 - (-tk * 40.0).exp()) / 40.0)).sin();
                let note = notes[((t / (beat * 4.0)) as usize) % 4];
                let sub = 0.5 * (2.0 * PI * note * t).sin() * (0.6 + 0.4 * (2.0 * PI * t / beat).sin().abs());
                let hat = 0.1 * noise() * (-((t + beat / 2.0) % beat) * 60.0).exp();
                let chord: f64 = [220.0, 261.63, 329.63]
                    .iter()
                    .map(|f| (1..=6).map(|h| (2.0 * PI * f * f64::from(h) * t).sin() / f64::from(h)).sum::<f64>())
                    .sum();
                0.9 * kick + sub + hat + 0.12 * chord
            })
            .collect();
        let clip = |v: f64, g: f64| 0.98 * (v * g / 0.98).tanh();
        let target = 10f64.powf(rms_db / 20.0);
        let (mut lo, mut hi) = (1e-3f64, 1e3f64);
        for _ in 0..50 {
            let g = (lo * hi).sqrt();
            let r = (raw.iter().map(|v| clip(*v, g).powi(2)).sum::<f64>() / n as f64).sqrt();
            if r < target {
                lo = g;
            } else {
                hi = g;
            }
        }
        let g = (lo * hi).sqrt();
        raw.iter().map(|v| clip(*v, g) as f32).collect()
    }

    /// Output RMS (dBFS, last 2/3) of the full processor on `x` in 256-frame blocks.
    fn processor_rms_db(c: &Config, fs: f64, x: &[f32]) -> f64 {
        let mut p = EqProcessor::new(fs, c, 2, Arc::new(Handoff::new()));
        let mut out = Vec::with_capacity(x.len());
        for ch in x.chunks(256) {
            let (mut l, mut r) = (ch.to_vec(), ch.to_vec());
            p.process(&mut l, &mut r, BlockStats::default());
            out.extend_from_slice(&l);
        }
        let seg = &out[out.len() / 3..];
        assert!(seg.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
        20.0 * rms(seg).log10()
    }

    /// Regression for "JBL Bass Mode gets quieter the louder the music is": through the full
    /// `EqProcessor` with the user's settings (Default JBL params, intensity 90), JBL on must stay
    /// within 3 dB of JBL off at every input level from -30 to -3 dBFS RMS. The legacy fixed-drive
    /// behaviour (auto level off) loses more than 6 dB at -6 dBFS.
    #[test]
    fn jbl_bass_mode_loudness_follows_input_level() {
        let fs = 48000.0;
        let n = (fs * 4.0) as usize;
        let base = Config { jbl_bass_mode: false, auto_headroom: false, ..Config::default() };
        let on = Config { jbl_bass_mode: true, ..base.clone() };
        let legacy = Config { jbl: JblParams { auto_level: false, ..on.jbl }, ..on.clone() };
        assert_eq!(on.jbl_intensity, 90);
        for lv in [-30.0, -24.0, -18.0, -12.0, -6.0, -3.0] {
            let x = music(n, fs, lv, 7);
            let (off_db, on_db) = (processor_rms_db(&base, fs, &x), processor_rms_db(&on, fs, &x));
            println!("JBL on-off at {lv} dBFS: {:+.2} dB", on_db - off_db);
            assert!((on_db - off_db).abs() <= 3.0, "at {lv} dBFS JBL is {:+.2} dB vs off", on_db - off_db);
        }
        let x = music(n, fs, -6.0, 7);
        let diff = processor_rms_db(&legacy, fs, &x) - processor_rms_db(&base, fs, &x);
        assert!(diff < -6.0, "legacy fixed-drive path at -6 dBFS: {diff:+.2} dB");
    }

    #[test]
    fn limiter_bounds_output() {
        let mut c = cfg(EqProfile::default());
        c.limiter = true;
        c.mute_on_fault = false;
        let mut p = EqProcessor::new(FS, &c, 2, Arc::new(Handoff::new()));
        let (mut l, mut r) = (vec![3.0f32; 128], vec![-3.0f32; 128]);
        p.process(&mut l, &mut r, BlockStats::default());
        assert!(peak(&l) <= 1.0 && peak(&r) <= 1.0);
    }
}

#[cfg(test)]
mod hot_reload_click_tests {
    use super::*;
    use eq_core::{FilterKind, FilterSpec};

    const FS: f64 = 48000.0;

    fn band(fc: f64, gain: f64, enabled: bool) -> FilterSpec {
        FilterSpec { kind: FilterKind::Peaking, fc_hz: fc, gain_db: gain, q: 1.0, enabled }
    }

    fn prof(preamp: f64, f: Vec<FilterSpec>) -> EqProfile {
        EqProfile { name: "t".into(), preamp_db: preamp, filters: f }
    }

    fn cfg(p: EqProfile) -> Config {
        Config { profile: p, limiter: false, auto_headroom: false, ..Config::default() }
    }

    /// Plays a 1 kHz tone through the processor, hot-swaps `to` after 0.3 s, and returns the worst
    /// second difference of the output divided by the largest value a clean tone of the output peak
    /// amplitude could have (1.0 = as smooth as the tone itself; a click is >> 1).
    fn click_ratio(from: EqProfile, to: EqProfile, f: f64, block: usize) -> f64 {
        let h = Arc::new(Handoff::new());
        let mut p = EqProcessor::new(FS, &cfg(from), 2, h.clone());
        let w = 2.0 * std::f64::consts::PI * f / FS;
        let total = (0.6 * FS) as usize;
        let swap_at = (0.3 * FS) as usize / block * block;
        let mut out = Vec::with_capacity(total);
        let mut i = 0usize;
        while i < total {
            if i == swap_at {
                h.publish(Snapshot::from_config(&cfg(to.clone()), 2, 48000));
            }
            let mut l: Vec<f32> = (0..block).map(|k| (0.25 * (w * (i + k) as f64).sin()) as f32).collect();
            let mut r = l.clone();
            p.process(&mut l, &mut r, BlockStats::default());
            out.extend_from_slice(&l);
            i += block;
        }
        let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs())) as f64;
        let tone_bound = peak * (2.0 - 2.0 * w.cos());
        let worst = out[swap_at.saturating_sub(64)..].windows(3).map(|v| f64::from((v[2] - 2.0 * v[1] + v[0]).abs())).fold(0.0, f64::max);
        worst / tone_bound
    }

    #[test]
    fn toggling_a_band_does_not_click() {
        let on = prof(0.0, vec![band(1000.0, 6.0, true), band(4000.0, -3.0, true)]);
        let off = prof(0.0, vec![band(1000.0, 6.0, false), band(4000.0, -3.0, true)]);
        for block in [64usize, 256, 512] {
            for (name, a, b, f) in [
                ("band off", on.clone(), off.clone(), 1000.0),
                ("band on", off.clone(), on.clone(), 1000.0),
                ("gain 6->2 dB", on.clone(), prof(0.0, vec![band(1000.0, 2.0, true), band(4000.0, -3.0, true)]), 1000.0),
                ("fc move", on.clone(), prof(0.0, vec![band(1100.0, 6.0, true), band(4000.0, -3.0, true)]), 1000.0),
                ("preamp -20 -> -10", prof(-20.0, vec![]), prof(-10.0, vec![]), 1000.0),
                ("all off", on.clone(), prof(0.0, vec![]), 1000.0),
            ] {
                let ratio = click_ratio(a, b, f, block);
                println!("{name} blk {block}: worst second difference = {ratio:.2} x tone bound");
                assert!(ratio < 1.5, "{name} (block {block}): click, {ratio:.2} x tone bound");
            }
        }
    }
}
