//! Synthetic capture source for hardware measurements: stands in for the WASAPI capture of
//! the cable so the real pipeline + ASIO device can be exercised at any cable rate / packet
//! size / burst jitter without touching the Windows audio settings.
//!
//! The source produces a continuous multi-tone (or single sine) in real time on its own
//! thread and delivers it through the same [`FrameSink`] the real capture uses.

use std::f64::consts::PI;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wasapi_capture::FrameSink;

/// Description of the synthetic source.
#[derive(Clone, Debug)]
pub struct SyntheticSource {
    /// "Cable" sample rate (Hz).
    pub rate: u32,
    /// Frames per delivered packet (e.g. 132 for exclusive, 441/480 for shared).
    pub packet_frames: u32,
    /// Tone frequencies (Hz); the sum is scaled to `dbfs`.
    pub freqs: Vec<f32>,
    /// Level of the sum in dBFS (peak if all tones aligned).
    pub dbfs: f32,
    /// Random extra delivery delay up to this many ms (never reorders; late packets
    /// are followed by a catch-up burst like WASAPI does).
    pub jitter_ms: f32,
}

impl SyntheticSource {
    /// A single quiet sine.
    pub fn tone(rate: u32, packet_frames: u32, freq: f32, dbfs: f32, jitter_ms: f32) -> Self {
        SyntheticSource { rate, packet_frames, freqs: vec![freq], dbfs, jitter_ms }
    }
}

/// Running synthetic source; dropping it stops the thread.
pub struct SynthHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SynthHandle {
    pub fn stop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for SynthHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawns the source thread.
pub fn spawn(cfg: SyntheticSource, mut sink: Box<dyn FrameSink>) -> std::io::Result<SynthHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let thread = std::thread::Builder::new().name("router-synth".into()).spawn(move || {
        use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
        // SAFETY: balanced timer-resolution request, process wide but harmless.
        unsafe {
            timeBeginPeriod(1);
        }
        run(&cfg, &mut *sink, &s2);
        // SAFETY: matches the call above.
        unsafe {
            timeEndPeriod(1);
        }
    })?;
    Ok(SynthHandle { stop, thread: Some(thread) })
}

fn run(cfg: &SyntheticSource, sink: &mut dyn FrameSink, stop: &AtomicBool) {
    let n = cfg.packet_frames.max(1) as usize;
    let rate = f64::from(cfg.rate);
    let amp = 10f64.powf(f64::from(cfg.dbfs) / 20.0) / cfg.freqs.len().max(1) as f64;
    let mut phases = vec![0.0f64; cfg.freqs.len()];
    let steps: Vec<f64> = cfg.freqs.iter().map(|&f| 2.0 * PI * f64::from(f) / rate).collect();
    let (mut l, mut r) = (vec![0.0f32; n], vec![0.0f32; n]);
    let t0 = Instant::now();
    let mut k = 0u64;
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut last_deliver = Duration::ZERO;
    while !stop.load(Relaxed) {
        // The packet completes at (k+1)*n/rate on the source clock.
        let due = Duration::from_secs_f64((k + 1) as f64 * n as f64 / rate);
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let jit = Duration::from_secs_f64(f64::from(cfg.jitter_ms.max(0.0)) * 1e-3 * (rng >> 11) as f64 / (1u64 << 53) as f64);
        let deliver = (due + jit).max(last_deliver);
        last_deliver = deliver;
        loop {
            if stop.load(Relaxed) {
                return;
            }
            let now = t0.elapsed();
            if now >= deliver {
                break;
            }
            let left = deliver - now;
            if left > Duration::from_micros(1500) {
                std::thread::sleep(Duration::from_millis(1));
            } else {
                std::hint::spin_loop();
            }
        }
        for i in 0..n {
            let mut v = 0.0;
            for (p, s) in phases.iter_mut().zip(&steps) {
                v += amp * p.sin();
                *p += s;
                if *p > 2.0 * PI {
                    *p -= 2.0 * PI;
                }
            }
            l[i] = v as f32;
            r[i] = v as f32;
        }
        sink.on_frames(&l, &r);
        k += 1;
    }
}
