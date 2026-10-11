//! The EQ stage and the lock-free config handoff to the audio thread.
//!
//! Handoff design (no allocation, locks or frees on the audio thread):
//! * The watcher thread builds a [`Snapshot`] (parsed [`Config`] pieces: the
//!   profile plus [`Settings`]) and `publish`es it into a single `AtomicPtr`
//!   slot. An unconsumed older snapshot is replaced and freed by the watcher.
//! * The audio thread `take`s the slot with one atomic swap (wait-free), applies
//!   it, and `retire`s the snapshot it just consumed into a small fixed array of
//!   `AtomicPtr` slots (wait-free, bounded; leaks rather than frees if full).
//! * The watcher `collect`s (frees) retired snapshots on its next tick.
//!
//! State preservation: filter state lives in the audio thread's single
//! [`StereoEq`]. `StereoEq` has no state-copy API, so instead of swapping in a
//! prebuilt equaliser (state would restart from zero -> click), the audio
//! thread calls `StereoEq::set_profile` in place. That recomputes at most
//! `MAX_BANDS` x 2 sets of biquad coefficients (some trig, no allocation, a few
//! microseconds, once per config change) and keeps all filter memories.

use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

use eq_core::{Config, EqProfile, StereoEq};

use crate::engine::Processor;

/// Realtime-relevant settings derived from a [`Config`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub bypass: bool,
    pub limiter: bool,
    /// Master output channels that receive the EQ'd L/R pair (validated).
    pub outputs: [usize; 2],
    pub passthrough_inputs: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { bypass: false, limiter: true, outputs: [0, 1], passthrough_inputs: true }
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
            limiter: cfg.limiter,
            outputs: validate_output_channels(cfg.output_channels, n_out),
            passthrough_inputs: cfg.passthrough_inputs,
        }
    }
}

/// What the watcher hands to the audio thread.
pub struct Snapshot {
    pub profile: EqProfile,
    pub settings: Settings,
}

impl Snapshot {
    pub fn from_config(cfg: &Config, n_out: usize) -> Box<Snapshot> {
        Box::new(Snapshot { profile: cfg.profile.clone(), settings: Settings::from_config(cfg, n_out) })
    }
}

const RETIRE_SLOTS: usize = 8;

/// Single-slot publish + bounded retire list, generic over the payload.
pub struct Handoff<T> {
    pending: AtomicPtr<T>,
    retired: [AtomicPtr<T>; RETIRE_SLOTS],
}

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

/// The real [`Processor`]: a [`StereoEq`] plus hot-reloadable settings.
pub struct EqProcessor {
    eq: StereoEq,
    settings: Settings,
    handoff: Arc<Handoff<Snapshot>>,
}

impl EqProcessor {
    pub fn new(sample_rate: f64, cfg: &Config, n_out: usize, handoff: Arc<Handoff<Snapshot>>) -> Self {
        let settings = Settings::from_config(cfg, n_out);
        let mut eq = StereoEq::new(sample_rate);
        eq.set_profile(&cfg.profile);
        eq.set_limiter(settings.limiter);
        EqProcessor { eq, settings, handoff }
    }

    fn apply(&mut self, snap: &Snapshot) {
        let was_bypassed = self.settings.bypass;
        self.eq.set_profile(&snap.profile);
        self.eq.set_limiter(snap.settings.limiter);
        if was_bypassed && !snap.settings.bypass {
            self.eq.reset(); // state is stale after a bypassed stretch
        }
        self.settings = snap.settings;
    }
}

impl Processor for EqProcessor {
    fn update(&mut self) {
        if let Some(snap) = self.handoff.take() {
            self.apply(&snap);
            self.handoff.retire(snap);
        }
    }

    fn settings(&self) -> Settings {
        self.settings
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        if !self.settings.bypass {
            self.eq.process_stereo(l, r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eq_core::{FilterKind, FilterSpec};

    fn pk6(fc: f64) -> EqProfile {
        EqProfile {
            name: "t".into(),
            preamp_db: 0.0,
            filters: vec![FilterSpec { kind: FilterKind::Peaking, fc_hz: fc, gain_db: 6.0, q: 1.0, enabled: true }],
        }
    }

    fn cfg(p: EqProfile) -> Config {
        Config { profile: p, limiter: false, ..Config::default() }
    }

    fn sine(n: usize, f: f64, fs: f64, amp: f32) -> Vec<f32> {
        (0..n).map(|i| amp * (2.0 * std::f64::consts::PI * f * i as f64 / fs).sin() as f32).collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// Runs `secs` of sine through the processor in blocks, returns the RMS of the last half.
    fn run(p: &mut EqProcessor, f: f64, fs: f64, blocks: usize) -> f64 {
        let n = 256;
        let src = sine(n * blocks, f, fs, 0.25);
        let mut out = Vec::new();
        for ch in src.chunks(n) {
            let (mut l, mut r) = (ch.to_vec(), ch.to_vec());
            p.update();
            p.process(&mut l, &mut r);
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
        let c = Config { bypass: true, limiter: false, output_channels: [2, 9], passthrough_inputs: false, ..Config::default() };
        let s = Settings::from_config(&c, 4);
        assert!(s.bypass && !s.limiter && !s.passthrough_inputs);
        assert_eq!(s.outputs, [0, 1]);
        let s = Settings::from_config(&Config::default(), 4);
        assert!(!s.bypass && s.limiter && s.passthrough_inputs);
    }

    #[test]
    fn peak_computation() {
        assert_eq!(peak(&[]), 0.0);
        assert_eq!(peak(&[0.1, -0.7, 0.3]), 0.7);
        assert_eq!(peak(&[f32::NAN, 0.2]), 0.2);
    }

    #[test]
    fn offline_plus6db_peaking_at_1khz() {
        let fs = 48000.0;
        let h = Arc::new(Handoff::new());
        let mut p = EqProcessor::new(fs, &cfg(pk6(1000.0)), 2, h);
        let gain = run(&mut p, 1000.0, fs, 200);
        let expect = 10f64.powf(6.0 / 20.0);
        assert!((gain - expect).abs() < 0.02, "gain {gain}, expected {expect}");
        // far from the peak the response is flat
        let mut p2 = EqProcessor::new(fs, &cfg(pk6(1000.0)), 2, Arc::new(Handoff::new()));
        assert!((run(&mut p2, 100.0, fs, 200) - 1.0).abs() < 0.06);
    }

    #[test]
    fn bypass_is_identity() {
        let fs = 48000.0;
        let mut c = cfg(pk6(1000.0));
        c.bypass = true;
        let mut p = EqProcessor::new(fs, &c, 2, Arc::new(Handoff::new()));
        assert!((run(&mut p, 1000.0, fs, 100) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn hot_reload_via_handoff() {
        let fs = 48000.0;
        let h = Arc::new(Handoff::new());
        let mut p = EqProcessor::new(fs, &cfg(EqProfile::default()), 2, h.clone());
        assert!((run(&mut p, 1000.0, fs, 100) - 1.0).abs() < 1e-3);
        h.publish(Snapshot::from_config(&cfg(pk6(1000.0)), 2));
        let g = run(&mut p, 1000.0, fs, 200);
        assert!((g - 1.9953).abs() < 0.02, "{g}");
        // retired snapshot comes back to the producer side
        assert_eq!(h.collect(), 1);
        assert_eq!(h.collect(), 0);
        // bypass via reload
        let mut c = cfg(pk6(1000.0));
        c.bypass = true;
        h.publish(Snapshot::from_config(&c, 2));
        assert!((run(&mut p, 1000.0, fs, 100) - 1.0).abs() < 1e-6);
        assert!(p.settings().bypass);
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
        // Drop frees what is left.
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
}
