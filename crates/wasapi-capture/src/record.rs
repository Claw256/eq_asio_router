//! Capture-side recorder used by `capture-probe record`: writes what the capture session
//! delivers to a 32-bit float stereo WAV plus a packet log, to separate "the cable /
//! capture mode delivers crackle" from "our DSP creates it".

use crate::convert::PacketFlags;
use crate::loopback::{arrival_stats, ArrivalStats};
use crate::monitor::CorruptionReason;
use crate::session::{CaptureEvent, CaptureMode, CaptureOptions, CaptureSession, FrameSink};
use crate::{find_capture_device, Result};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct RecData {
    l: Vec<f32>,
    r: Vec<f32>,
    /// (first sample index, frames, flags, seconds since start)
    packets: Vec<(usize, u32, PacketFlags, f64)>,
    t0: Option<Instant>,
}

struct RecSink(Arc<Mutex<RecData>>);

impl FrameSink for RecSink {
    fn on_frames(&mut self, left: &[f32], right: &[f32]) {
        let mut d = self.0.lock().unwrap_or_else(|e| e.into_inner());
        d.l.extend_from_slice(left);
        d.r.extend_from_slice(right);
    }
    fn on_packet(&mut self, frames: u32, flags: PacketFlags) {
        let mut d = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let t0 = *d.t0.get_or_insert_with(Instant::now);
        let at = d.l.len();
        d.packets.push((at, frames, flags, t0.elapsed().as_secs_f64()));
    }
}

fn write_wav(path: &Path, rate: u32, l: &[f32], r: &[f32]) -> std::io::Result<()> {
    let n = l.len().min(r.len());
    let bytes = (n * 8) as u32;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + bytes).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&3u16.to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * 8).to_le_bytes())?;
    f.write_all(&8u16.to_le_bytes())?;
    f.write_all(&32u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&bytes.to_le_bytes())?;
    for i in 0..n {
        f.write_all(&l[i].to_le_bytes())?;
        f.write_all(&r[i].to_le_bytes())?;
    }
    f.flush()
}

/// Records `secs` seconds from the capture endpoint matching `name` into `path`
/// (+ `path.packets.txt`). Returns a human-readable summary.
pub fn record_to_wav(
    path: &Path,
    name: Option<&str>,
    secs: f32,
    mode: CaptureMode,
    preferred_rate: Option<u32>,
) -> Result<String> {
    let opts = CaptureOptions { preferred_rate, ..CaptureOptions::new(mode) };
    Ok(record_with_options(path, name, secs, opts)?.summary)
}

/// Structured outcome of [`record_with_options`].
#[derive(Clone, Debug)]
pub struct RecordReport {
    /// Human-readable summary (as printed by `capture-probe record`).
    pub summary: String,
    pub mode_requested: CaptureMode,
    pub mode_in_use: CaptureMode,
    pub sample_rate: u32,
    /// Granted stream period in frames.
    pub period_frames: u32,
    /// Endpoint engine period right after init (0 = unknown).
    pub engine_period_frames: u32,
    /// Packet inter-arrival statistics (min/mean/p99/p99.9/max in microseconds).
    pub arrival: Option<ArrivalStats>,
    /// Monitor trips (0 also when the monitor is off) and the reasons that were reported.
    pub corruption_trips: u64,
    pub corruption_reasons: Vec<CorruptionReason>,
    /// 0 = off, 1 = report only, 2 = stop on trip.
    pub monitor_policy: u8,
    pub max_ahead_frames: u32,
    pub repeated_blocks: u64,
    /// Fallback notes (`requested -> using: reason`).
    pub fallbacks: Vec<String>,
    /// True when the session ended before `secs` (stopped by the monitor, device lost, error).
    pub ended_early: bool,
    pub frames: usize,
}

/// Like [`record_to_wav`] with full [`CaptureOptions`] and a structured report. A session that is
/// stopped by the corruption monitor ends the recording early.
pub fn record_with_options(path: &Path, name: Option<&str>, secs: f32, options: CaptureOptions) -> Result<RecordReport> {
    let mode = options.mode;
    let ep = find_capture_device(name)?;
    let data = Arc::new(Mutex::new(RecData::default()));
    let mut s = CaptureSession::start_with_options(&ep.id, options, Box::new(RecSink(data.clone())))?;
    let fmt = s.format();
    let head = format!(
        "{}: requested {:?}, in use {:?}; {} Hz {} ch {}; period {} frames (engine period {})",
        ep.name,
        mode,
        s.mode_in_use(),
        fmt.sample_rate,
        fmt.channels,
        fmt.format.name(),
        s.period_frames(),
        s.stats().engine_period_frames.load(std::sync::atomic::Ordering::Relaxed)
    );
    let end = Instant::now() + Duration::from_secs_f32(secs);
    let (mut reasons, mut fallbacks) = (Vec::new(), Vec::new());
    let mut ended_early = false;
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50).min(end.saturating_duration_since(Instant::now())));
        while let Ok(ev) = s.events().try_recv() {
            match ev {
                CaptureEvent::Corruption(r) => reasons.push(r),
                CaptureEvent::Fallback { requested, using, reason } => {
                    fallbacks.push(format!("{requested:?} -> {using:?}: {reason}"))
                }
                CaptureEvent::DeviceLost | CaptureEvent::Error(_) => ended_early = true,
                CaptureEvent::Started { .. } => {}
            }
        }
        if !s.is_running() {
            ended_early = true;
            break;
        }
    }
    s.stop();
    while let Ok(ev) = s.events().try_recv() {
        match ev {
            CaptureEvent::Corruption(r) => reasons.push(r),
            CaptureEvent::Fallback { requested, using, reason } => {
                fallbacks.push(format!("{requested:?} -> {using:?}: {reason}"))
            }
            _ => {}
        }
    }
    let st = s.stats().snapshot();
    let d = data.lock().unwrap_or_else(|e| e.into_inner());
    let io = |e: std::io::Error| crate::CaptureError::Thread(format!("{}: {e}", path.display()));
    write_wav(path, fmt.sample_rate, &d.l, &d.r).map_err(io)?;
    let mut log = String::new();
    let mut hist: std::collections::BTreeMap<u32, usize> = Default::default();
    let mut prev_t = None;
    let (mut max_gap, mut sum_gap, mut n_gap) = (0.0f64, 0.0f64, 0usize);
    for (at, frames, fl, t) in &d.packets {
        *hist.entry(*frames).or_default() += 1;
        if let Some(p) = prev_t {
            let g: f64 = t - p;
            max_gap = max_gap.max(g);
            sum_gap += g;
            n_gap += 1;
        }
        prev_t = Some(*t);
        if fl.data_discontinuity || fl.silent || fl.timestamp_error {
            log.push_str(&format!(
                "t={:.4}s sample={} frames={} discontinuity={} silent={} ts_error={}\n",
                at_secs(*at, fmt.sample_rate),
                at,
                frames,
                fl.data_discontinuity,
                fl.silent,
                fl.timestamp_error
            ));
        }
    }
    let _ = std::fs::write(path.with_extension("packets.txt"), &log);
    let hist_s: Vec<String> = hist.iter().map(|(k, v)| format!("{k}fr x{v}")).collect();
    let times_us: Vec<f64> = d.packets.iter().map(|p| p.3 * 1e6).collect();
    let nominal_us = s.period_frames() as f64 * 1e6 / f64::from(fmt.sample_rate.max(1));
    let arrival = arrival_stats(&times_us, nominal_us);
    let stats = s.stats();
    use std::sync::atomic::Ordering::Relaxed;
    let mut summary = format!(
        "{head}\nrecorded {} frames ({:.2} s) -> {}\npackets {} ({}), discontinuity flags {}, silent {}, ts errors {}, mean packet gap {:.2} ms (max {:.2} ms); flagged packets logged in {}",
        d.l.len(),
        d.l.len() as f64 / f64::from(fmt.sample_rate),
        path.display(),
        st.packets,
        hist_s.join(", "),
        st.discontinuities,
        st.silent_packets,
        st.timestamp_errors,
        if n_gap > 0 { sum_gap / n_gap as f64 * 1000.0 } else { 0.0 },
        max_gap * 1000.0,
        path.with_extension("packets.txt").display()
    );
    if let Some(a) = &arrival {
        summary.push_str(&format!(
            "
gaps us min {:.0} mean {:.0} p99 {:.0} p99.9 {:.0} max {:.0} (n={})",
            a.min_us, a.mean_us, a.p99_us, a.p999_us, a.max_us, a.gaps
        ));
    }
    let trips = stats.corruption_trips.load(Relaxed);
    let policy = stats.monitor_policy.load(Relaxed);
    summary.push_str(&format!(
        "
monitor: {} trips {} max-ahead {} frames repeated-blocks {}{}",
        ["off", "report-only", "stop-on-trip"][policy.min(2) as usize],
        trips,
        stats.max_ahead_frames.load(Relaxed),
        stats.repeated_blocks.load(Relaxed),
        reasons.first().map(|r| format!("; first: {r}")).unwrap_or_default()
    ));
    Ok(RecordReport {
        summary,
        mode_requested: mode,
        mode_in_use: s.mode_in_use(),
        sample_rate: fmt.sample_rate,
        period_frames: s.period_frames(),
        engine_period_frames: stats.engine_period_frames.load(Relaxed),
        arrival,
        corruption_trips: trips,
        corruption_reasons: reasons,
        monitor_policy: policy,
        max_ahead_frames: stats.max_ahead_frames.load(Relaxed),
        repeated_blocks: stats.repeated_blocks.load(Relaxed),
        fallbacks,
        ended_early,
        frames: d.l.len(),
    })
}

fn at_secs(sample: usize, rate: u32) -> f64 {
    sample as f64 / f64::from(rate.max(1))
}
