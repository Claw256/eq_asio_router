//! Optional measurement tap: copies the stereo stream at three points of the path into
//! 32-bit float WAV files, for offline analysis (`router-cli analyze`).
//!
//! * `capture_in.wav`: what the source delivers to the pipeline (bridge ring-A pops or the
//!   synthetic ring; source rate)
//! * `after_resample.wav`: rate matcher output (ASIO rate)
//! * `after_eq.wav`: the final block, exactly what is converted into the ASIO buffer
//!
//! The audio callback only pushes into lock-free SPSC rings ([`TapPoint::push`]); a
//! writer thread drains them to disk. If a ring is full the excess frames are dropped and
//! counted; the counts are written to `tap_report.txt` so the analysis can discount
//! those runs. With no tap configured nothing here runs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use router_core::ring::{self, Consumer, Producer};
use router_core::wav::WavWriter;

/// Tap request.
#[derive(Clone, Debug)]
pub struct TapConfig {
    /// Output directory (created if missing).
    pub dir: PathBuf,
}

/// Names of the three tap points / files.
pub const TAP_NAMES: [&str; 3] = ["capture_in", "after_resample", "after_eq"];

/// Ring capacity per tap in frames (~3 s).
const TAP_RING: usize = 1 << 17;

/// Producer end of one tap point (lives in the pipeline's source tap or the audio callback).
pub struct TapPoint {
    prod: Producer,
}

impl TapPoint {
    /// Copies a block; wait-free and allocation-free.
    #[inline]
    pub fn push(&mut self, l: &[f32], r: &[f32]) {
        self.prod.push_frames(l, r);
    }
}

/// The three producer ends.
pub struct TapPoints {
    pub capture_in: TapPoint,
    pub after_resample: TapPoint,
    pub after_eq: TapPoint,
}

/// Owner of the writer thread. [`finish`](Self::finish) drains, patches the headers and
/// writes `tap_report.txt`.
pub struct TapSession {
    stop: Arc<AtomicBool>,
    cable_rate: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl TapSession {
    /// Sets the capture sample rate once known (opens `capture_in.wav`).
    pub fn set_cable_rate(&self, rate: u32) {
        self.cable_rate.store(rate, Relaxed);
    }

    /// Stops the writer, flushing everything that is still queued.
    pub fn finish(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TapSession {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Starts a tap: returns the producer ends and the session handle.
pub fn start(cfg: &TapConfig, asio_rate: u32) -> std::io::Result<(TapPoints, TapSession)> {
    std::fs::create_dir_all(&cfg.dir)?;
    let mut prods = Vec::new();
    let mut cons = Vec::new();
    for _ in 0..3 {
        let (p, c) = ring::channel(TAP_RING);
        prods.push(TapPoint { prod: p });
        cons.push(c);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let cable_rate = Arc::new(AtomicU32::new(0));
    let (s2, c2, dir) = (stop.clone(), cable_rate.clone(), cfg.dir.clone());
    let thread = std::thread::Builder::new()
        .name("router-tap".into())
        .spawn(move || writer_main(dir, asio_rate, cons, s2, c2))?;
    let after_eq = prods.pop().expect("3 taps");
    let after_resample = prods.pop().expect("3 taps");
    let capture_in = prods.pop().expect("3 taps");
    Ok((TapPoints { capture_in, after_resample, after_eq }, TapSession { stop, cable_rate, thread: Some(thread) }))
}

fn writer_main(dir: PathBuf, asio_rate: u32, cons: Vec<Consumer>, stop: Arc<AtomicBool>, cable_rate: Arc<AtomicU32>) {
    let mut cons = cons;
    let mut files: Vec<Option<WavWriter>> = (0..3).map(|_| None).collect();
    let mut rates = [0u32; 3];
    let mut l = vec![0.0f32; 4096];
    let mut r = vec![0.0f32; 4096];
    loop {
        let stopping = stop.load(Relaxed);
        let mut moved = 0usize;
        for i in 0..3 {
            if files[i].is_none() {
                let rate = if i == 0 { cable_rate.load(Relaxed) } else { asio_rate };
                if rate == 0 {
                    continue;
                }
                rates[i] = rate;
                files[i] = WavWriter::create(&dir.join(format!("{}.wav", TAP_NAMES[i])), rate, 2).ok();
            }
            loop {
                let n = cons[i].pop_frames(&mut l, &mut r);
                if n == 0 {
                    break;
                }
                if let Some(f) = files[i].as_mut() {
                    let _ = f.write_stereo(&l[..n], &r[..n]);
                }
                moved += n;
            }
        }
        if stopping && moved == 0 {
            break;
        }
        if moved == 0 {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let mut report = String::new();
    for i in 0..3 {
        let frames = files[i].as_ref().map(WavWriter::frames).unwrap_or(0);
        report.push_str(&format!(
            "{} rate={} frames={} dropped_frames={} drop_events={}\n",
            TAP_NAMES[i],
            rates[i],
            frames,
            cons[i].dropped_frames(),
            cons[i].overruns()
        ));
    }
    for f in files.into_iter().flatten() {
        let _ = f.finish();
    }
    let _ = std::fs::write(dir.join("tap_report.txt"), report);
}

/// Reads `tap_report.txt` from `dir` (for the CLI summary).
pub fn read_report(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("tap_report.txt")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_writes_three_files_and_report() {
        let dir = std::env::temp_dir().join("router_tap_test");
        let _ = std::fs::remove_dir_all(&dir);
        let (mut pts, mut ses) = start(&TapConfig { dir: dir.clone() }, 48000).unwrap();
        ses.set_cable_rate(44100);
        let v: Vec<f32> = (0..1000).map(|i| i as f32 / 1000.0).collect();
        pts.capture_in.push(&v, &v);
        pts.after_resample.push(&v[..500], &v[..500]);
        pts.after_eq.push(&v[..100], &v[..100]);
        ses.finish();
        let a = router_core::wav::read(&dir.join("capture_in.wav")).unwrap();
        assert_eq!((a.rate, a.channels[0].len()), (44100, 1000));
        let b = router_core::wav::read(&dir.join("after_eq.wav")).unwrap();
        assert_eq!((b.rate, b.channels[1].len()), (48000, 100));
        let rep = read_report(&dir).unwrap();
        assert!(rep.contains("after_resample rate=48000 frames=500 dropped_frames=0"), "{rep}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
