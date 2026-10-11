//! The real-time part: ring -> rate matcher -> EQ -> ASIO output buffers.
//!
//! Everything reachable from [`AudioCallback::buffer_switch`] is allocation-free,
//! lock-free and syscall-light (QueryPerformanceCounter, relaxed atomics, `rlog!`).
//! [`Pipeline`] is clock-free (ticks are passed in) so it can be driven by tests with
//! synthetic ring data.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;

use asio_abi::AsioSampleType;
use asio_host::{Handler, OutputReady};
use eq_core::ResamplerQuality;
use router_core::arrival::{ArrivalStats, ArrivalTracker};
use router_core::asio_convert::{write_f32_to_asio, write_silence};
use router_core::logger::Level;
use router_core::processor::{peak, BlockStats, EqProcessor};
use router_core::resample::{RateMatcher, RenderStats};
use router_core::ring::{Consumer, Producer};
use router_core::rlog;
use router_core::status_writer::{BlockReport, StatusWriter};
use router_core::watchdog::Heartbeat;
use wasapi_capture::FrameSink;

use crate::logic::{gap_ratio, underrun_ratio};
use crate::priority::CallbackMmcss;
use crate::probe::{LoopbackTap, ProbeShared};
use crate::tap::TapPoint;

/// Callbacks ignored for the gap measurement and the worst-case figure (start-up priming).
pub const WARMUP_CALLBACKS: u64 = 8;

/// ASIO callbacks that must have run before the capture sink starts feeding the ring
/// (two priming callbacks plus two paced ones).
pub const SINK_ARM_AFTER_CALLBACKS: u64 = 4;

/// Reason codes stored in [`Live::reset_reason`].
pub const RESET_NONE: u8 = 0;
pub const RESET_REQUEST: u8 = 1;
pub const RESET_RATE: u8 = 2;
pub const RESET_PANIC: u8 = 3;

/// Counters and control values shared between the audio callback, the control thread
/// and readers. All relaxed atomics.
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
    /// Desired target fill in cable frames (0 = keep).
    pub target_frames: AtomicU32,
    /// 0 = linear, 1 = cubic.
    pub quality: AtomicU8,
    pub reset_requested: AtomicBool,
    pub reset_reason: AtomicU8,
    pub panicked: AtomicBool,
    pub burst_max: AtomicU32,
    /// Start-up gate bias in cable frames (see `RateMatcher::set_open_bias`; 0 = keep).
    pub open_bias: AtomicU32,
    /// Mean ring fill (cable frames, f32 bits), exponential average over ~2 s.
    pub fill_mean: AtomicU32,
    /// Result of the MMCSS registration of the ASIO callback thread (0 not yet, 1 ok, 2 failed).
    pub mmcss_asio: AtomicU8,
    /// ASIO rate, shared with the config watcher.
    pub asio_rate: Arc<AtomicU32>,
}

impl Live {
    /// Zeroes the per-stream counters (not the control inputs `quality`/`asio_rate`).
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
        self.mmcss_asio.store(0, Relaxed);
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

/// `QueryPerformanceCounter` in ticks.
#[inline]
pub fn qpc() -> i64 {
    use windows::Win32::System::Performance::QueryPerformanceCounter;
    let mut t = 0i64;
    // SAFETY: valid out pointer. Cannot fail on supported Windows versions.
    unsafe {
        let _ = QueryPerformanceCounter(&mut t);
    }
    t
}

/// `QueryPerformanceFrequency` in Hz (never 0).
pub fn qpc_freq() -> i64 {
    use windows::Win32::System::Performance::QueryPerformanceFrequency;
    let mut f = 0i64;
    // SAFETY: valid out pointer.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut f);
    }
    f.max(1)
}

// ------------------------------------------------------------------------------------
// Output buffers
// ------------------------------------------------------------------------------------

/// Number of output channels the engine creates buffers for (the stereo pair).
pub const OUT_CH: usize = 2;

/// Buffer pointers of the created channels. Filled by the control thread after
/// `create_buffers` (the driver allocates them) and before `start`.
#[derive(Default)]
pub struct PtrTable {
    slots: [[AtomicPtr<c_void>; 2]; OUT_CH],
}

impl PtrTable {
    pub fn set(&self, ch: usize, ptrs: [*mut c_void; 2]) {
        if let Some(s) = self.slots.get(ch) {
            s[0].store(ptrs[0], Relaxed);
            s[1].store(ptrs[1], Relaxed);
        }
    }
    #[inline]
    pub fn get(&self, ch: usize, half: usize) -> *mut c_void {
        match self.slots.get(ch).and_then(|s| s.get(half)) {
            Some(p) => p.load(Relaxed),
            None => std::ptr::null_mut(),
        }
    }
}

/// Converts the processed block into the driver's native sample type.
pub struct OutputSet {
    pub ptrs: Arc<PtrTable>,
    pub types: [AsioSampleType; OUT_CH],
}

impl OutputSet {
    /// Writes `l`/`r` (first `n` samples) to the two output buffers of `half`.
    ///
    /// # Safety
    /// The pointers in the table must be valid driver buffers of at least `n` samples
    /// of the configured sample types (guaranteed while the stream is created).
    #[inline]
    pub unsafe fn write(&self, half: usize, l: &[f32], r: &[f32], n: usize) {
        let half = half & 1;
        for (ch, src) in [l, r].into_iter().enumerate() {
            let p = self.ptrs.get(ch, half);
            if !p.is_null() {
                write_f32_to_asio(p as *mut u8, self.types[ch], n, src);
            }
        }
    }

    /// Silence for the whole set.
    ///
    /// # Safety
    /// As [`write`](Self::write).
    #[inline]
    pub unsafe fn silence(&self, half: usize, n: usize) {
        let half = half & 1;
        for ch in 0..OUT_CH {
            let p = self.ptrs.get(ch, half);
            if !p.is_null() {
                write_silence(p as *mut u8, self.types[ch], n);
            }
        }
    }
}

// ------------------------------------------------------------------------------------
// Pipeline
// ------------------------------------------------------------------------------------

/// Everything the pipeline needs, built on the control thread.
pub struct PipelineInit {
    pub consumer: Consumer,
    pub matcher: RateMatcher,
    pub eq: EqProcessor,
    pub frames: usize,
    pub asio_rate: u32,
    pub tick_hz: i64,
    pub live: Arc<Live>,
    pub status: Option<Arc<StatusWriter>>,
    pub heartbeat: Arc<Heartbeat>,
    pub target_frames: u32,
    pub quality: ResamplerQuality,
    /// Measurement taps (None = off, zero cost).
    pub tap_resample: Option<TapPoint>,
    pub tap_eq: Option<TapPoint>,
}

/// ring -> [`RateMatcher`] -> [`EqProcessor`] with timing and status bookkeeping.
pub struct Pipeline {
    consumer: Consumer,
    matcher: RateMatcher,
    eq: EqProcessor,
    l: Vec<f32>,
    r: Vec<f32>,
    live: Arc<Live>,
    status: Option<Arc<StatusWriter>>,
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
    mmcss: Option<CallbackMmcss>,
    last: RenderStats,
    peaks_in: [f32; 2],
    peaks_out: [f32; 2],
    fault: u32,
    tap_resample: Option<TapPoint>,
    tap_eq: Option<TapPoint>,
}

impl Pipeline {
    pub fn new(init: PipelineInit) -> Self {
        let period_s = init.frames as f64 / f64::from(init.asio_rate.max(1));
        Pipeline {
            consumer: init.consumer,
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
            mmcss: None,
            cur_quality: quality_to_u8(init.quality),
            last: RenderStats::default(),
            peaks_in: [0.0; 2],
            peaks_out: [0.0; 2],
            fault: 0,
            tap_resample: init.tap_resample,
            tap_eq: init.tap_eq,
        }
    }

    /// Enables the one-time MMCSS "Pro Audio" registration of the callback thread (done from the
    /// first [`render`](Self::render); off in offline tests so they do not touch the test thread).
    pub fn enable_mmcss(&mut self) {
        self.mmcss = Some(CallbackMmcss::default());
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
        // MMCSS registration of the driver-owned callback thread: once (see `priority::CallbackMmcss`);
        // afterwards a thread-id compare. Never reverted from here.
        if let Some(m) = self.mmcss.as_mut() {
            let r = m.ensure();
            if r != 0 {
                self.live.mmcss_asio.store(r, Relaxed);
            }
        }
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
        let st = self.matcher.render(&mut self.consumer, l, r);
        if let Some(t) = self.tap_resample.as_mut() {
            t.push(l, r);
        }
        self.peaks_in = [peak(l), peak(r)];
        let stats = BlockStats { underrun_ratio: underrun_ratio(st.produced, n, st.gated), gap_ratio: self.gap };
        let fault = self.eq.process(l, r, stats);
        self.fault = fault.as_u32();
        self.peaks_out = [peak(l), peak(r)];
        if let Some(t) = self.tap_eq.as_mut() {
            t.push(l, r);
        }
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

    /// Current target fill of the rate matcher (cable frames).
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
        let overruns = self.consumer.overruns();
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
// ASIO handler
// ------------------------------------------------------------------------------------

/// The `Handler` installed into the ASIO driver.
pub struct AudioCallback {
    pipe: Pipeline,
    out: OutputSet,
    out_ready: Option<OutputReady>,
    live: Arc<Live>,
    dead: bool,
    /// Loopback-input scanner of the latency probe (None = off: no extra work at all).
    loopback: Option<LoopbackTap>,
}

impl AudioCallback {
    pub fn new(pipe: Pipeline, out: OutputSet, out_ready: Option<OutputReady>, live: Arc<Live>) -> Self {
        let mut pipe = pipe;
        pipe.enable_mmcss();
        AudioCallback { pipe, out, out_ready, live, dead: false, loopback: None }
    }

    /// Attaches the loopback-input scanner (latency probe).
    pub fn with_loopback(mut self, tap: Option<LoopbackTap>) -> Self {
        self.loopback = tap;
        self
    }

    #[inline]
    fn run(&mut self, index: i32) {
        let t0 = qpc();
        let half = (index & 1) as usize;
        self.pipe.begin(t0);
        let n = self.pipe.render();
        {
            let (l, r) = self.pipe.out();
            // SAFETY: the pointer table was filled from the driver's own buffers of
            // `frames` samples per half; `n <= frames`.
            unsafe { self.out.write(half, l, r, n) };
        }
        if let Some(or) = self.out_ready {
            if !or.call() {
                self.out_ready = None; // not supported: stop asking
            }
        }
        let t1 = qpc();
        self.pipe.end(t0, t1);
        // Latency probe: the input half `index` holds the block that was just captured. Only does work
        // while armed (one relaxed load otherwise).
        if let Some(lb) = self.loopback.as_mut() {
            // SAFETY: the tap's pointer table was filled from the driver's own input buffers of `frames` samples.
            unsafe { lb.scan(half, n, t0) };
        }
    }
}

impl Handler for AudioCallback {
    fn buffer_switch(&mut self, index: i32) {
        if self.dead {
            // SAFETY: as in `run`.
            unsafe { self.out.silence((index & 1) as usize, self.pipe.frames()) };
            return;
        }
        if catch_unwind(AssertUnwindSafe(|| self.run(index))).is_err() {
            self.dead = true;
            self.live.panicked.store(true, Relaxed);
            self.live.request_reset(RESET_PANIC);
            rlog!(Level::Error, "panic in audio callback; output muted, restart requested");
            // SAFETY: as in `run`.
            unsafe { self.out.silence((index & 1) as usize, self.pipe.frames()) };
        }
    }

    fn reset_request(&mut self) {
        self.live.request_reset(RESET_REQUEST);
        rlog!(Level::Warn, "driver reset request");
    }

    fn sample_rate_changed(&mut self, rate: f64) {
        self.live.request_reset(RESET_RATE);
        rlog!(Level::Warn, "driver reports sample rate change to {rate} Hz");
    }

    fn overload(&mut self) {
        self.live.xruns.fetch_add(1, Relaxed);
    }
}

// ------------------------------------------------------------------------------------
// Capture side
// ------------------------------------------------------------------------------------

/// `FrameSink` that feeds the SPSC ring. WASAPI delivers in bursts (typically two
/// 10 ms packets back to back every ~20 ms); the ring's target fill includes the measured
/// arrival headroom (`router_core::arrival`, fed from here) and the drift controller low-pass
/// filters the resulting fill sawtooth, so the sink simply pushes. Until `armed` it discards (the ASIO side is
/// not streaming yet and a stale ring would only add latency).
pub struct CaptureSink {
    producer: Producer,
    heartbeat: Arc<Heartbeat>,
    armed: Arc<AtomicBool>,
    live: Arc<Live>,
    /// Calls closer than this belong to one burst.
    window_ticks: i64,
    last_ticks: i64,
    burst_acc: u32,
    tap: Option<TapPoint>,
    arrival: Option<ArrivalTracker>,
    tick_hz: f64,
    /// Latency probe (capture-side click detector); None = off.
    probe: Option<Arc<ProbeShared>>,
}

impl CaptureSink {
    pub fn new(producer: Producer, heartbeat: Arc<Heartbeat>, armed: Arc<AtomicBool>, live: Arc<Live>) -> Self {
        CaptureSink {
            producer,
            heartbeat,
            armed,
            live,
            window_ticks: qpc_freq() / 2000, // 0.5 ms
            last_ticks: i64::MIN / 2,
            burst_acc: 0,
            tap: None,
            arrival: None,
            tick_hz: qpc_freq() as f64,
            probe: None,
        }
    }

    /// Attaches the capture-side detector of the latency probe.
    pub fn with_probe(mut self, probe: Option<Arc<ProbeShared>>) -> Self {
        self.probe = probe;
        self
    }

    /// Feeds the arrival-jitter statistics (atomics only, no allocation in `on_frames`).
    pub fn with_arrival(mut self, stats: Option<Arc<ArrivalStats>>) -> Self {
        self.arrival = stats.map(ArrivalTracker::new);
        self
    }

    /// Attaches the `capture_in` measurement tap.
    pub fn with_tap(mut self, tap: Option<TapPoint>) -> Self {
        self.tap = tap;
        self
    }
}

impl FrameSink for CaptureSink {
    fn on_frames(&mut self, left: &[f32], right: &[f32]) {
        self.heartbeat.beat_capture();
        // The Yamaha runs its two priming callbacks immediately and then needs ~200 ms
        // before it really streams; data accepted in between is stale and would only be
        // thrown away by the gate, so wait until the callbacks run at their real pace.
        if !self.armed.load(Relaxed) || self.live.callbacks.load(Relaxed) < SINK_ARM_AFTER_CALLBACKS {
            return;
        }
        // Burst size: frames delivered by calls less than 0.5 ms apart (WASAPI shared
        // mode hands over two 10 ms packets back to back every ~20 ms).
        let now = qpc();
        if let Some(pr) = &self.probe {
            if pr.capture.is_active() {
                pr.capture.on_block(left, right, now);
            }
            if pr.monitor.is_active() {
                pr.monitor.feed(0, left, right);
            }
        }
        let n = left.len() as u32;
        self.burst_acc = if now - self.last_ticks <= self.window_ticks { self.burst_acc.saturating_add(n) } else { n };
        self.last_ticks = now;
        self.live.burst_max.fetch_max(self.burst_acc, Relaxed);
        if let Some(a) = self.arrival.as_mut() {
            a.on_frames(now as f64 / self.tick_hz, n);
        }
        if let Some(t) = self.tap.as_mut() {
            t.push(left, right);
        }
        self.producer.push_frames(left, right);
    }
}
