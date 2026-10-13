//! The real-time part: ring -> rate matcher -> EQ -> output buffers.
//!
//! Portable port of `router-engine/src/pipeline.rs` geometry (same layout, same
//! bookkeeping), behind the [`crate::seams`] boundaries: the ASIO `PtrTable`/`OutputSet`
//! half-buffer writes become an f32 copy into the [`OutputCallback`](crate::seams::OutputCallback)
//! buffers, and QPC self-timing becomes the [`Clock`](crate::seams::Clock) seam with
//! ticks passed in.
//!
//! Everything reachable from [`PipelineCallback::render`] is allocation-free,
//! lock-free and syscall-light (docs/architecture.md §3: relaxed atomics and `rlog!`
//! only). [`Pipeline`] is clock-free (ticks are passed in) so it can be driven by
//! tests with synthetic ring data.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;

use eq_core::ResamplerQuality;
use router_core::logger::Level;
use router_core::processor::{peak, BlockStats, EqProcessor};
use router_core::resample::{RateMatcher, RenderStats};
use router_core::ring::Consumer;
use router_core::rlog;
use router_core::status_writer::BlockReport;
use router_core::watchdog::Heartbeat;

use crate::seams::{Clock, OutputCallback, StatusTransport};
use crate::session::{gap_ratio, underrun_ratio};

/// Native-format conversion for non-f32 backends (the "asio_convert equivalent" of
/// `router-engine`): the portable render path itself copies plain `f32` via
/// [`write_out`], backends with a native sample format use these helpers
/// (e.g. Phase C's Windows `asio-host` sink).
pub use router_core::asio_convert;

/// Callbacks ignored for the gap measurement and the worst-case figure (start-up priming).
pub const WARMUP_CALLBACKS: u64 = 8;

/// Reason codes stored in [`Live::reset_reason`].
pub const RESET_NONE: u8 = 0;
pub const RESET_REQUEST: u8 = 1;
pub const RESET_RATE: u8 = 2;
pub const RESET_PANIC: u8 = 3;

/// Human-readable reset reason (port of `router-engine/src/engine.rs::reset_reason_text`).
pub fn reset_reason_text(code: u8) -> &'static str {
    match code {
        RESET_REQUEST => "driver reset request (buffer size / configuration changed)",
        RESET_RATE => "driver sample rate changed",
        RESET_PANIC => "audio callback panicked",
        _ => "driver reset",
    }
}

/// Counters and control values shared between the audio callback, the control thread
/// and readers. All relaxed atomics (port of `router-engine/src/pipeline.rs:48`; the
/// MMCSS slot became the generic per-OS [`Live::rt_prio`], the shared ASIO rate became
/// [`Live::out_rate`]).
#[derive(Default)]
pub struct Live {
    pub callbacks: AtomicU64,
    pub xruns: AtomicU64,
    pub underruns: AtomicU64,
    pub overruns: AtomicU64,
    pub discarded: AtomicU64,
    pub rearms: AtomicU64,
    pub non_finite: AtomicU64,
    pub ring_fill: AtomicU32,
    /// f32 bits, microseconds.
    pub cb_avg_us: AtomicU32,
    pub cb_max_us: AtomicU32,
    /// Peak holds (f32 bits, non-negative so `fetch_max` orders correctly); the control
    /// thread swaps them to 0 every 100 ms.
    pub peak_in: [AtomicU32; 2],
    pub peak_out: [AtomicU32; 2],
    pub fault: AtomicU32,
    /// f32 bits, ppm.
    pub trim_ppm: AtomicU32,
    /// Desired target fill in source frames (0 = keep).
    pub target_frames: AtomicU32,
    /// 0 = linear, 1 = cubic.
    pub quality: AtomicU8,
    pub reset_requested: AtomicBool,
    pub reset_reason: AtomicU8,
    pub panicked: AtomicBool,
    pub burst_max: AtomicU32,
    /// Start-up gate bias in source frames (see `RateMatcher::set_open_bias`; 0 = keep).
    pub open_bias: AtomicU32,
    /// Mean ring fill (source frames, f32 bits), exponential average over ~2 s.
    pub fill_mean: AtomicU32,
    /// Smallest ungated ring fill since the control tick last swapped it out
    /// (`u32::MAX` = no sample yet).
    pub fill_min_tick: AtomicU32,
    /// Result of the per-OS RT priority registration of the callback thread (0 not yet,
    /// 1 ok, 2 failed): MMCSS "Pro Audio" on Windows, `SCHED_FIFO`/QoS on Linux/macOS.
    pub rt_prio: AtomicU8,
    /// Output rate, shared with the config watcher.
    pub out_rate: Arc<AtomicU32>,
}

impl Live {
    /// Zeroes the per-stream counters (not the control inputs `quality`/`out_rate`).
    pub fn reset_counters(&self) {
        for a in [&self.callbacks, &self.xruns, &self.underruns, &self.overruns, &self.discarded, &self.rearms, &self.non_finite] {
            a.store(0, Relaxed);
        }
        for a in [
            &self.ring_fill,
            &self.cb_avg_us,
            &self.cb_max_us,
            &self.fault,
            &self.trim_ppm,
            &self.target_frames,
            &self.burst_max,
            &self.open_bias,
            &self.fill_mean,
            &self.peak_in[0],
            &self.peak_in[1],
            &self.peak_out[0],
            &self.peak_out[1],
        ] {
            a.store(0, Relaxed);
        }
        self.rt_prio.store(0, Relaxed);
        self.fill_min_tick.store(u32::MAX, Relaxed);
        self.reset_requested.store(false, Relaxed);
        self.reset_reason.store(RESET_NONE, Relaxed);
        self.panicked.store(false, Relaxed);
    }

    /// Asks the control thread for a restart (callable from any thread).
    pub fn request_reset(&self, reason: u8) {
        self.reset_reason.store(reason, Relaxed);
        self.reset_requested.store(true, Relaxed);
    }
}

/// `ResamplerQuality` <-> the [`Live::quality`] byte.
pub fn quality_to_u8(q: ResamplerQuality) -> u8 {
    match q {
        ResamplerQuality::Linear => 0,
        ResamplerQuality::Cubic => 1,
    }
}

fn u8_to_quality(v: u8) -> ResamplerQuality {
    if v == 0 {
        ResamplerQuality::Linear
    } else {
        ResamplerQuality::Cubic
    }
}

/// Number of output channels the engine renders (the stereo pair).
pub const OUT_CH: usize = 2;

/// Copies the processed stereo block into the sink's `f32` channel slices: channel 0
/// gets `l`, channel 1 gets `r`, any further channel is silenced. Allocation-free.
/// This is the portable equivalent of `OutputSet::write` (`router-engine/src/pipeline.rs:174`);
/// native-format sinks use [`asio_convert`] instead.
#[inline]
pub fn write_out(out: &mut [&mut [f32]], l: &[f32], r: &[f32], n: usize) {
    let n = n.min(l.len()).min(r.len());
    for (ch, buf) in out.iter_mut().enumerate() {
        if ch < OUT_CH {
            let m = n.min(buf.len());
            let src: &[f32] = if ch == 0 { &l[..m] } else { &r[..m] };
            buf[..m].copy_from_slice(src);
        } else {
            for v in buf.iter_mut() {
                *v = 0.0;
            }
        }
    }
}

/// Silences every output channel (panic path / dead callback).
#[inline]
pub fn write_silence_out(out: &mut [&mut [f32]]) {
    for buf in out.iter_mut() {
        for v in buf.iter_mut() {
            *v = 0.0;
        }
    }
}

// ------------------------------------------------------------------------------------
// Pipeline
// ------------------------------------------------------------------------------------

/// Everything the pipeline needs, built on the control thread (port of
/// `router-engine/src/pipeline.rs:315`; the ASIO/bridge/measurement extras stay
/// Windows-side and are not part of the portable core).
pub struct PipelineInit {
    pub source: Consumer,
    pub matcher: RateMatcher,
    pub eq: EqProcessor,
    pub frames: usize,
    pub out_rate: u32,
    pub tick_hz: i64,
    pub live: Arc<Live>,
    pub status: Option<Arc<dyn StatusTransport>>,
    pub heartbeat: Arc<Heartbeat>,
    pub target_frames: u32,
    pub quality: ResamplerQuality,
}

/// [`Consumer`] -> [`RateMatcher`] -> [`EqProcessor`] with timing and status
/// bookkeeping (port of `router-engine/src/pipeline.rs:342`).
pub struct Pipeline {
    source: Consumer,
    matcher: RateMatcher,
    eq: EqProcessor,
    l: Vec<f32>,
    r: Vec<f32>,
    live: Arc<Live>,
    status: Option<Arc<dyn StatusTransport>>,
    heartbeat: Arc<Heartbeat>,
    tick_hz: f64,
    nominal_ticks: f64,
    period_us: f64,
    last_start: Option<i64>,
    gap: f32,
    n_cb: u64,
    cb_avg: f32,
    cb_max: f32,
    cur_target: u32,
    cur_bias: u32,
    cur_quality: u8,
    fill_mean: f32,
    last: RenderStats,
    peaks_in: [f32; 2],
    peaks_out: [f32; 2],
    fault: u32,
}

impl Pipeline {
    pub fn new(init: PipelineInit) -> Self {
        let period_s = init.frames as f64 / f64::from(init.out_rate.max(1));
        Pipeline {
            source: init.source,
            matcher: init.matcher,
            eq: init.eq,
            l: vec![0.0; init.frames],
            r: vec![0.0; init.frames],
            live: init.live,
            status: init.status,
            heartbeat: init.heartbeat,
            tick_hz: init.tick_hz as f64,
            nominal_ticks: period_s * init.tick_hz as f64,
            period_us: period_s * 1e6,
            last_start: None,
            gap: 0.0,
            n_cb: 0,
            cb_avg: 0.0,
            cb_max: 0.0,
            cur_target: init.target_frames,
            cur_bias: 0,
            fill_mean: 0.0,
            cur_quality: quality_to_u8(init.quality),
            last: RenderStats::default(),
            peaks_in: [0.0; 2],
            peaks_out: [0.0; 2],
            fault: 0,
        }
    }

    /// Frames per block.
    pub fn frames(&self) -> usize {
        self.l.len()
    }

    /// Callback period in microseconds.
    pub fn period_us(&self) -> f64 {
        self.period_us
    }

    /// Start of a callback at `now` ticks: measures the spacing to the previous one.
    #[inline]
    pub fn begin(&mut self, now: i64) {
        self.gap = match self.last_start {
            Some(prev) if self.n_cb >= WARMUP_CALLBACKS => gap_ratio(now - prev, self.nominal_ticks),
            _ => 0.0,
        };
        self.last_start = Some(now);
    }

    /// Renders one block (rate matching + EQ). Returns the frames produced; the post-EQ
    /// samples are then available from [`out`](Self::out).
    #[inline]
    pub fn render(&mut self) -> usize {
        let n = self.l.len().min(self.r.len());
        // Hot-changed settings (single relaxed loads).
        let b = self.live.open_bias.load(Relaxed);
        if b != self.cur_bias {
            self.matcher.set_open_bias(b as usize);
            self.cur_bias = b;
        }
        let t = self.live.target_frames.load(Relaxed);
        if t != 0 && t != self.cur_target {
            self.matcher.set_target_fill(t as usize);
            self.cur_target = t;
        }
        let q = self.live.quality.load(Relaxed);
        if q != self.cur_quality {
            self.matcher.set_quality(u8_to_quality(q));
            self.cur_quality = q;
        }
        let (l, r) = (&mut self.l[..n], &mut self.r[..n]);
        let st = self.matcher.render(&mut self.source, l, r);
        self.peaks_in = [peak(l), peak(r)];
        let stats = BlockStats { underrun_ratio: underrun_ratio(st.produced, n, st.gated), gap_ratio: self.gap };
        let fault = self.eq.process(l, r, stats);
        self.fault = fault.as_u32();
        self.peaks_out = [peak(l), peak(r)];
        self.last = st;
        if st.discarded > 0 {
            rlog!(Level::Info, "ring trimmed: discarded {} frames (fill now {}, target {}, callback #{})", st.discarded, st.fill, self.cur_target, self.n_cb);
        }
        if self.gap > 0.0 {
            rlog!(Level::Warn, "callback gap ratio {:.2} at callback #{}", self.gap, self.n_cb);
        }
        n
    }

    /// Post-EQ block of the last [`render`](Self::render).
    #[inline]
    pub fn out(&self) -> (&[f32], &[f32]) {
        (&self.l, &self.r)
    }

    /// Current target fill of the rate matcher (source frames).
    pub fn target_fill(&self) -> usize {
        self.matcher.target_fill()
    }

    /// Statistics of the last render call.
    pub fn last_render(&self) -> RenderStats {
        self.last
    }

    /// Fault state of the last block.
    pub fn fault(&self) -> u32 {
        self.fault
    }

    /// End of a callback: timing statistics, status publication and the heartbeat.
    #[inline]
    pub fn end(&mut self, start: i64, end: i64) {
        let us = ((end - start).max(0) as f64 / self.tick_hz * 1e6) as f32;
        self.n_cb += 1;
        self.cb_avg = if self.n_cb == 1 { us } else { self.cb_avg + (us - self.cb_avg) * (1.0 / 128.0) };
        if self.n_cb > WARMUP_CALLBACKS {
            self.cb_max = self.cb_max.max(us);
        }
        let live = &*self.live;
        let underruns = self.matcher.underruns();
        let overruns = self.source.overruns();
        live.callbacks.store(self.n_cb, Relaxed);
        live.underruns.store(underruns, Relaxed);
        live.overruns.store(overruns, Relaxed);
        live.discarded.store(self.matcher.discarded_frames(), Relaxed);
        live.rearms.store(self.matcher.rearms(), Relaxed);
        live.non_finite.store(self.eq.non_finite_total(), Relaxed);
        live.ring_fill.store(self.last.fill as u32, Relaxed);
        if !self.last.gated {
            // ~2 s average of the fill seen at the start of each block (the quantity the drift
            // controller regulates); the first open block seeds it.
            let f = self.last.fill as f32;
            self.fill_mean = if self.fill_mean == 0.0 { f } else { self.fill_mean + (f - self.fill_mean) * (1.0 / 188.0) };
            live.fill_mean.store(self.fill_mean.to_bits(), Relaxed);
            // Smallest fill of this control-tick window.
            live.fill_min_tick.fetch_min(self.last.fill as u32, Relaxed);
        }
        live.cb_avg_us.store(self.cb_avg.to_bits(), Relaxed);
        live.cb_max_us.store(self.cb_max.to_bits(), Relaxed);
        live.fault.store(self.fault, Relaxed);
        live.trim_ppm.store(((self.matcher.trim() * 1e6) as f32).to_bits(), Relaxed);
        for i in 0..2 {
            live.peak_in[i].fetch_max(self.peaks_in[i].to_bits(), Relaxed);
            live.peak_out[i].fetch_max(self.peaks_out[i].to_bits(), Relaxed);
        }
        if let Some(sw) = &self.status {
            sw.set_xruns(live.xruns.load(Relaxed));
            sw.publish_block(&BlockReport {
                peak_in: self.peaks_in,
                peak_out: self.peaks_out,
                fault_state: self.fault,
                non_finite: self.eq.non_finite_total().min(u64::from(u32::MAX)) as u32,
                ring_fill_frames: self.last.fill as u32,
                underruns: underruns.min(u64::from(u32::MAX)) as u32,
                overruns: overruns.min(u64::from(u32::MAX)) as u32,
                max_callback_us: self.cb_max as u32,
                avg_callback_us: self.cb_avg as u32,
            });
        }
        self.heartbeat.beat_audio();
    }
}

// ------------------------------------------------------------------------------------
// Sink callback
// ------------------------------------------------------------------------------------

/// The [`OutputCallback`] installed into an [`crate::seams::OutputSink`]: one render
/// call per period, mirroring the ASIO `Handler::buffer_switch` contract
/// (`asio-host/src/lib.rs:143`) with the portable [`Pipeline`] as the body.
///
/// `clock` must read the same tick domain as the `now_ticks` the backend passes in.
pub struct PipelineCallback {
    pipe: Pipeline,
    clock: Arc<dyn Clock>,
    live: Arc<Live>,
    dead: bool,
}

impl PipelineCallback {
    pub fn new(pipe: Pipeline, clock: Arc<dyn Clock>) -> Self {
        let live = pipe.live.clone();
        PipelineCallback { pipe, clock, live, dead: false }
    }

    /// Access to the wrapped pipeline (control thread / tests).
    pub fn pipeline(&self) -> &Pipeline {
        &self.pipe
    }
}

impl OutputCallback for PipelineCallback {
    #[inline]
    fn render(&mut self, out: &mut [&mut [f32]], now_ticks: i64) {
        if self.dead {
            write_silence_out(out);
            return;
        }
        if catch_unwind(AssertUnwindSafe(|| {
            self.pipe.begin(now_ticks);
            let n = self.pipe.render();
            let (l, r) = self.pipe.out();
            write_out(out, l, r, n);
            self.pipe.end(now_ticks, self.clock.now_ticks());
        }))
        .is_err()
        {
            self.dead = true;
            self.live.panicked.store(true, Relaxed);
            self.live.request_reset(RESET_PANIC);
            rlog!(Level::Error, "panic in audio callback; output muted, restart requested");
            write_silence_out(out);
        }
    }

    fn on_xrun(&mut self) {
        self.live.xruns.fetch_add(1, Relaxed);
    }
}
