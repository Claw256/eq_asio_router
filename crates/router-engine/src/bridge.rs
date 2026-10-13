//! Component 3 of the VBMatrix bridge plan: the app side of the shared-memory bridge.
//!
//! The engine attaches to the mapping created by the virtual ASIO driver
//! (`bridge-abi`, `Local\eq_asio_router_bridge_v2`) and uses it as its only audio
//! source (in place of the WASAPI capture):
//!
//! * **Ring A (source):** the Yamaha ASIO callback consumes it directly through
//!   [`BridgeSource`] (a [`FrameSource`] for the existing `RateMatcher`: startup gate
//!   -> PI drift controller -> Kaiser resampler -> EQ). The producer lives in the
//!   VBMatrix process, so there is no in-process capture ring any more.
//! * **Ring B (loopback):** [`spawn_loopback`] runs a dedicated non-RT thread that
//!   drains a small in-process SPSC tapped inside the Yamaha callback after
//!   `EqProcessor`, resamples Yamaha rate -> negotiated rate (with a
//!   [`DriftController`] on `fill_b`, the mirror of the ring-A controller) and pushes
//!   into the shm ring. The tap push is allocation-free and lock-free (RT rules).
//! * **Control-tick bookkeeping:** [`BridgeMonitor`] derives `vbmatrix_running`
//!   (`driver_running` + heartbeat freshness) and [`FillWindow`] replaces the WASAPI
//!   arrival model with ring-fill statistics (see the module note below).
//!
//! # Deviation from the arrival model
//!
//! The WASAPI arrival timestamps cannot be reconstructed from the shm contract (the
//! header only carries a `driver_beat` counter, one increment per driver callback —
//! no producer-side timestamps). Feeding `ArrivalTracker` from the 100 ms control tick
//! would alias the delivery pattern badly (every gap looks like 100 ms, above the
//! model's stall threshold). The arrival model is therefore **simplified to fill
//! statistics**: [`FillWindow`] records, per control tick, the dip of the ring fill
//! (mean minus the minimum seen at the Yamaha callbacks inside that tick) plus the
//! `driver_beat` delta. The measured dip feeds the existing `HeadroomPlanner`
//! unchanged (it consumes an `ArrivalSnapshot`), so "Learn (30 s)" and the live
//! headroom planner keep working with ring-fill semantics.
//!
//! Everything here is either a plain state machine fed with values/clocks (unit
//! tested with a synthetic header and a fake clock) or a background thread.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bridge_abi::{Header, Mapping, RING_FRAMES};
use eq_core::ResamplerQuality;
use router_core::arrival::ArrivalSnapshot;
use router_core::resample::{DriftController, FrameSource, Resampler};
use router_core::ring::Consumer;

use crate::status::EngineError;

/// Source label used in the status/GUI for the bridge.
pub const BRIDGE_SOURCE_NAME: &str = "VBMatrix bridge";

/// How long the driver heartbeat may stay frozen (while `driver_running` is set)
/// before the slot counts as stalled. The driver beats once per ASIO callback
/// (typically every 2-20 ms), so one second is a very conservative limit.
pub const BEAT_STALL: Duration = Duration::from_millis(1000);

/// Control-tick period of the engine (`engine::TICK`); the fill window assumes it.
pub const TICK_S: f64 = 0.1;
/// Fill samples kept by [`FillWindow`] (30 s of 100 ms ticks).
pub const FILL_WINDOW_TICKS: usize = 300;
/// Ticks needed before a "Learn" run is considered representative (20 s of healthy
/// ticks at 10 Hz; the arrival model needed 500 producer events, which the bridge
/// cannot produce at tick rate).
pub const LEARN_MIN_TICKS: u32 = 200;

// ---------------------------------------------------------------------------------
// Attach
// ---------------------------------------------------------------------------------

/// The app's attachment to the driver's mapping (owns the Win32 view).
///
/// The mapping is created by the driver (first comer); the app only *opens* it, so
/// "mapping attached" also tells the readiness panel that VBMatrix has initialized
/// the EQ ASIO Router slot at least once. The header (and therefore ring indices and
/// counters) survives driver restarts while this handle is alive — which is why the
/// app keeps one attached in the process-wide [`BridgeHolder`] (see there) instead of
/// attaching per engine session.
pub struct BridgeAttach {
    mapping: Mapping,
}

impl BridgeAttach {
    /// Opens the fixed-name mapping and validates its header.
    pub fn attach() -> Result<BridgeAttach, EngineError> {
        Self::attach_named(bridge_abi::MAPPING_NAME)
    }

    /// As [`attach`](Self::attach) with an explicit mapping name (tests).
    /// Open-only: creation stays driver-side.
    pub fn attach_named(name: &str) -> Result<BridgeAttach, EngineError> {
        let mapping = Mapping::open_named(name).ok_or_else(|| {
            EngineError::Bridge("mapping not found (VBMatrix is not running the EQ ASIO Router driver)".into())
        })?;
        let h = unsafe { mapping.header() };
        h.validate().map_err(|e| EngineError::Bridge(format!("invalid mapping: {e}")))?;
        Ok(BridgeAttach { mapping })
    }

    /// The header (valid while `self` is alive).
    pub fn header(&self) -> &Header {
        // SAFETY: the view is mapped and validated in `attach`.
        unsafe { self.mapping.header() }
    }

    /// The negotiated `(sample_rate_hz, buffer_frames)` if the driver published a
    /// usable format (it publishes at `init()`, i.e. once VBMatrix opened the slot).
    pub fn negotiated(&self) -> Result<(u32, u32), EngineError> {
        negotiated(self.header().get_sample_rate(), self.header().get_buffer_frames())
    }

    /// A ring-A consumer for the Yamaha callback (raw header pointer; the `Mapping`
    /// must be dropped only after the audio callback has been disposed).
    pub fn source(&self) -> BridgeSource {
        BridgeSource { hdr: self.header() as *const Header }
    }

    /// A raw header pointer for the loopback feeder thread (same lifetime rule).
    pub fn raw(&self) -> *const Header {
        self.header() as *const Header
    }
}

/// Validates raw negotiated values (shared with the fake-header tests).
fn negotiated(rate: f64, buffer: u32) -> Result<(u32, u32), EngineError> {
    if !rate.is_finite() || rate < 8_000.0 || rate > 768_000.0 {
        return Err(EngineError::Bridge(format!("driver has not negotiated a sample rate yet ({rate} Hz)")));
    }
    if !(16..=8192).contains(&buffer) {
        return Err(EngineError::Bridge(format!("driver has not negotiated a buffer size yet ({buffer} frames)")));
    }
    Ok((rate.round() as u32, buffer))
}

// ---------------------------------------------------------------------------------
// Process-lifetime holder
// ---------------------------------------------------------------------------------

/// Holds the app's [`BridgeAttach`] for the lifetime of the process.
///
/// The mapping object lives exactly as long as any process holds a handle: the
/// driver (inside VBMatrix) opens/creates it at `init()` and releases it at
/// teardown, and VBMatrix re-creates/releases the driver object every few
/// seconds. Attaching per engine *session* therefore let the mapping die whenever
/// the engine was stopped — the next host re-created it fresh (ring indices and
/// counters reset) and session starts failed with "bridge mapping not found" until
/// it reappeared. The holder attaches **once** (open-only; creation stays
/// driver-side) and keeps the `Mapping` across engine sessions and engine stops;
/// sessions borrow [`Arc<BridgeAttach>`] handles from it. When the app starts
/// before any driver ever ran, [`ensure`](Self::ensure) fails and the engine's
/// back-off loop re-tries until the mapping appears.
pub struct BridgeHolder {
    attach: Mutex<Option<Arc<BridgeAttach>>>,
}

impl BridgeHolder {
    /// An empty holder.
    pub const fn new() -> Self {
        BridgeHolder { attach: Mutex::new(None) }
    }

    /// Attaches the fixed-name mapping unless it is already held, and returns the handle.
    pub fn ensure(&self) -> Result<Arc<BridgeAttach>, EngineError> {
        self.ensure_named(bridge_abi::MAPPING_NAME)
    }

    /// As [`ensure`](Self::ensure) with an explicit mapping name (tests).
    pub fn ensure_named(&self, name: &str) -> Result<Arc<BridgeAttach>, EngineError> {
        let mut g = self.attach.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(a) = g.as_ref() {
            return Ok(a.clone());
        }
        let a = Arc::new(BridgeAttach::attach_named(name)?);
        *g = Some(a.clone());
        Ok(a)
    }

    /// True while a mapping is held (its ring state survives for the process lifetime).
    pub fn is_attached(&self) -> bool {
        self.attach.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
}

impl Default for BridgeHolder {
    fn default() -> Self {
        Self::new()
    }
}

/// The app-wide mapping holder (one per process): attached at engine-manager
/// construction and kept until the process exits.
pub static BRIDGE: BridgeHolder = BridgeHolder::new();

// ---------------------------------------------------------------------------------
// Ring A consumer (audio callback side)
// ---------------------------------------------------------------------------------

/// Consumer half of ring A: implements [`FrameSource`] on top of the shm header so
/// the existing `RateMatcher` (gate -> drift controller -> resampler) runs unchanged.
///
/// Wait-free: atomics only (see `bridge-abi`). `Send` because the header lives in
/// mapped memory owned by the process-lifetime [`BridgeHolder`], not by the thread
/// that created this value.
#[derive(Clone, Copy)]
pub struct BridgeSource {
    hdr: *const Header,
}

// SAFETY: the pointed-to header outlives the pipeline (the [`BridgeHolder`] keeps the
// mapping alive for the whole process); access is atomic and single-consumer.
unsafe impl Send for BridgeSource {}

impl FrameSource for BridgeSource {
    fn fill(&self) -> usize {
        // SAFETY: as in the struct docs.
        unsafe { (*self.hdr).fill_a() as usize }
    }
    fn pop(&mut self, l: &mut [f32], r: &mut [f32]) -> usize {
        // SAFETY: single consumer, header valid for 'static of the session.
        unsafe { (*self.hdr).ring_a_pop(l, r) }
    }
    fn discard(&mut self, n: usize) -> usize {
        // SAFETY: as above.
        unsafe { (*self.hdr).ring_a_discard(n) }
    }
}

impl BridgeSource {
    /// Frames currently in ring B (what the driver will consume).
    pub fn fill_b(&self) -> u64 {
        // SAFETY: as above.
        unsafe { (*self.hdr).fill_b() }
    }
    /// Frames the driver's producer dropped because ring A was full.
    pub fn a_overruns(&self) -> u64 {
        // SAFETY: as above.
        unsafe { (*self.hdr).a_overruns() }
    }
    /// Pushes post-EQ frames into ring B (loopback feeder side).
    pub fn push_b(&self, l: &[f32], r: &[f32]) -> usize {
        // SAFETY: single producer (the feeder thread) while the session lives.
        unsafe { (*self.hdr).ring_b_push(l, r) }
    }
    /// The raw header (for the feeder thread).
    pub fn header_ptr(&self) -> *const Header {
        self.hdr
    }
}

// ---------------------------------------------------------------------------------
// Liveness (control tick, fake-clock testable)
// ---------------------------------------------------------------------------------

/// One control-tick sample of the driver side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BridgeSample {
    /// `driver_running != 0` (the driver is between `start()` and `stop()`).
    pub driver_running: bool,
    /// The heartbeat counter as read.
    pub beat: u64,
    /// Heartbeat advance since the previous tick (wrapping).
    pub beat_delta: u64,
    /// The heartbeat has not advanced for [`BEAT_STALL`].
    pub stalled: bool,
    /// What the status block reports: running *and* heartbeat fresh.
    pub vbmatrix_running: bool,
}

/// Tracks `driver_running` + `driver_beat` across control ticks. Pure state machine:
/// the caller passes the values and a monotonic clock, so tests fake both.
#[derive(Debug, Default)]
pub struct BridgeMonitor {
    last_beat: Option<u64>,
    last_advance: Option<Duration>,
}

impl BridgeMonitor {
    pub fn new() -> Self {
        BridgeMonitor::default()
    }

    /// Samples the heartbeat; call once per 100 ms control tick.
    pub fn sample(&mut self, driver_running: u32, beat: u64, now: Duration) -> BridgeSample {
        let delta = match self.last_beat {
            Some(prev) => beat.wrapping_sub(prev),
            None => 0, // first sample: only establishes the baseline
        };
        if self.last_beat.is_none() {
            self.last_advance = Some(now);
        }
        if delta > 0 || self.last_beat.is_none() {
            self.last_advance = Some(now);
        }
        self.last_beat = Some(beat);
        let stalled = self.last_advance.is_some_and(|t| now.saturating_sub(t) > BEAT_STALL);
        let running = driver_running != 0;
        BridgeSample {
            driver_running: running,
            beat,
            beat_delta: delta,
            stalled,
            vbmatrix_running: running && !stalled,
        }
    }

    /// Forgets the baseline (session start).
    pub fn reset(&mut self) {
        *self = BridgeMonitor::default();
    }
}

// ---------------------------------------------------------------------------------
// Fill statistics (replacement for the arrival model)
// ---------------------------------------------------------------------------------

/// Sliding window of ring-fill dips, one sample per healthy control tick.
///
/// `dip` = (smooth mean fill at the Yamaha callbacks) - (minimum fill seen during
/// the tick), i.e. how far the fill fell below its mean while the producer beats
/// against the consumer. The planner turns the p99.9 of that into headroom exactly
/// like the arrival model's dip histogram (same `ArrivalSnapshot` output type).
pub struct FillWindow {
    dips: Vec<u32>,
    beats: Vec<u64>,
    head: usize,
    count: usize,
}

impl FillWindow {
    pub fn new() -> Self {
        FillWindow { dips: vec![0; FILL_WINDOW_TICKS], beats: vec![0; FILL_WINDOW_TICKS], head: 0, count: 0 }
    }

    /// Records one tick. `dip` frames; `beats` driver callbacks since the previous tick.
    pub fn push(&mut self, dip: u32, beats: u64) {
        self.dips[self.head] = dip;
        self.beats[self.head] = beats;
        self.head = (self.head + 1) % FILL_WINDOW_TICKS;
        self.count = (self.count + 1).min(FILL_WINDOW_TICKS);
    }

    /// Throws the window away ("Learn").
    pub fn clear(&mut self) {
        self.head = 0;
        self.count = 0;
    }

    /// Samples recorded (<= [`FILL_WINDOW_TICKS`]).
    pub fn samples(&self) -> usize {
        self.count
    }

    /// Aggregates the window into the planner's input (control thread; allocates).
    ///
    /// `rate`: source (negotiated) rate; `buffer`: the driver's negotiated buffer size,
    /// which is both the producer's burst and the frame staleness (`buffer / 2 / rate`).
    pub fn snapshot(&self, rate: f64, buffer: u32) -> ArrivalSnapshot {
        if self.count == 0 {
            return ArrivalSnapshot { rate, ..ArrivalSnapshot::default() };
        }
        let idx = |i: usize| (self.head + FILL_WINDOW_TICKS - self.count + i) % FILL_WINDOW_TICKS;
        let mut dips: Vec<u32> = (0..self.count).map(|i| self.dips[idx(i)]).collect();
        dips.sort_unstable();
        let n = dips.len();
        // Nearest-rank p99.9 (for <= 300 samples this is the maximum, as intended:
        // the planner wants the worst observed dip).
        let rank = ((n as f64 * 0.999).ceil() as usize).clamp(1, n);
        let dip_p999 = dips[rank - 1];
        let dip_max = dips[n - 1];
        let beats: Vec<u64> = (0..self.count).map(|i| self.beats[idx(i)]).collect();
        let mean_beats = beats.iter().sum::<u64>() as f64 / n as f64;
        let per_tick_frames = |b: u64| (b.saturating_mul(u64::from(buffer))).min(u64::from(u32::MAX)) as u32;
        let mut gap: Vec<u32> = beats.iter().map(|&b| per_tick_frames(b)).collect();
        gap.sort_unstable();
        ArrivalSnapshot {
            observed_s: n as f64 * TICK_S,
            events: n as u32,
            stalls: 0,
            dip_p999,
            dip_max,
            gap_p999_frames: gap[n - 1],
            mean_event_frames: mean_beats * f64::from(buffer),
            staleness_s: f64::from(buffer) / (2.0 * rate.max(1.0)),
            rate,
        }
    }
}

impl Default for FillWindow {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------------

/// Ring-A fill target: the derived target (`plan_target` with the headroom term)
/// floored by `[bridge] ring_target_ms` at the source rate, capped so the shm ring
/// (16384 frames) always has room to trim latency (never above a quarter of it,
/// i.e. 4096 frames = ~85 ms @48k).
pub fn apply_ring_target_floor(target: usize, floor_ms: f32, source_rate: u32) -> usize {
    let floor = if floor_ms > 0.0 && source_rate > 0 {
        (f64::from(floor_ms) * f64::from(source_rate) / 1000.0).ceil() as usize
    } else {
        0
    };
    target.max(floor).min(RING_FRAMES / 4)
}

/// Ring-B (loopback) fill target in negotiated-rate frames: two driver buffers so
/// the driver never pops an empty ring under callback phase beat, plus the safety
/// buffer; capped at a quarter of the ring.
pub fn loopback_target_frames(safety_ms: f32, buffer_frames: u32, negotiated_rate: u32) -> usize {
    let safety = f64::from(safety_ms.max(0.0)) * 1e-3 * f64::from(negotiated_rate);
    (2 * buffer_frames as usize + safety.ceil() as usize).max(buffer_frames as usize).min(RING_FRAMES / 4)
}

// ---------------------------------------------------------------------------------
// Loopback feeder (Component 3)
// ---------------------------------------------------------------------------------

/// Capacity of the in-process tap ring (Yamaha-rate frames); ~85 ms at 48 kHz.
pub const TAP_RING_FRAMES: usize = 8192;
/// Chunk the resampler produces per push (frames).
const FEED_CHUNK: usize = 512;
/// Sleep when the tap ring is empty.
const FEED_IDLE: Duration = Duration::from_millis(1);

/// Running loopback feeder; dropping it stops and joins the thread.
pub struct LoopbackFeeder {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LoopbackFeeder {
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

impl Drop for LoopbackFeeder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawns the feeder: `input` (the in-process SPSC consumed by the Yamaha callback
/// after `EqProcessor`) is resampled `yamaha_rate -> negotiated_rate` with a PI drift
/// controller on the shm ring-B fill and pushed into ring B.
///
/// `header` must point into the session's mapping and stay valid until the returned
/// handle is dropped (the session joins the thread in its `Drop`).
///
/// The thread first pre-fills ring B with silence up to `target_frames` so the driver
/// never counts an underrun while the EQ chain starts up.
pub fn spawn_loopback(
    header: *const Header,
    mut input: Consumer,
    yamaha_rate: u32,
    negotiated_rate: u32,
    target_frames: usize,
    quality: ResamplerQuality,
) -> std::io::Result<LoopbackFeeder> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    // Wrap the raw pointer in `BridgeSource` (which is `Send` by contract) for the spawn.
    let src = BridgeSource { hdr: header };
    let thread = std::thread::Builder::new().name("router-loopback".into()).spawn(move || {
        // Method call on the whole `src` forces the closure to capture the `Send` wrapper.
        feed_loop(src.header_ptr(), &mut input, yamaha_rate, negotiated_rate, target_frames, quality, &stop2);
    })?;
    Ok(LoopbackFeeder { stop, thread: Some(thread) })
}

/// The feeder body (separated so the logic around it stays a plain function).
fn feed_loop(
    header: *const Header,
    input: &mut Consumer,
    yamaha_rate: u32,
    negotiated_rate: u32,
    target_frames: usize,
    quality: ResamplerQuality,
    stop: &AtomicBool,
) {
    // SAFETY: the caller (session) keeps the mapping alive until this thread is joined.
    let hdr = unsafe { &*header };
    let nominal = f64::from(yamaha_rate.max(1)) / f64::from(negotiated_rate.max(1));
    let mut rs = Resampler::with_nominal_ratio(quality, nominal);
    // Mirror of the ring-A controller: `fs` is the rate frames *enter* ring B
    // (post-resample, negotiated rate); `delta` learns the Yamaha-vs-driver clock offset.
    let mut drift = DriftController::new(target_frames, f64::from(negotiated_rate.max(1)));

    // One-shot silence pre-fill so `b_underruns` starts clean.
    {
        let mut need = target_frames.saturating_sub(hdr.fill_b() as usize);
        let zeros = [0.0f32; FEED_CHUNK];
        while need > 0 && !stop.load(Relaxed) {
            let k = need.min(zeros.len());
            // SAFETY: single producer while the session lives (see struct docs).
            unsafe { hdr.ring_b_push(&zeros[..k], &zeros[..k]) };
            need -= k;
        }
    }

    let (mut ol, mut or) = (vec![0.0f32; FEED_CHUNK], vec![0.0f32; FEED_CHUNK]);
    let mut last = Instant::now();
    while !stop.load(Relaxed) {
        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f64();
        last = now;
        let fill_b = hdr.fill_b() as usize;
        let trim = drift.update(fill_b, dt);
        let ratio = nominal * (1.0 + trim);
        let mut moved = 0usize;
        loop {
            let want = ol.len();
            let got = {
                let inp = &mut *input;
                rs.process(ratio, &mut ol, &mut or, &mut |l: &mut [f32], r: &mut [f32]| inp.pop_frames(l, r))
            };
            if got == 0 {
                break;
            }
            // SAFETY: as above.
            unsafe { hdr.ring_b_push(&ol[..got], &or[..got]) };
            moved += got;
            if got < want {
                break; // input dry
            }
        }
        if moved == 0 {
            std::thread::sleep(FEED_IDLE);
        }
    }
}

// ---------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use router_core::ring;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Allocates a private, zeroed bridge image (header page + both rings) without
    /// touching the Win32 mapping layer, seeded like `bridge-abi`'s creator would.
    fn synthetic_header() -> *mut Header {
        use bridge_abi::{CHANNELS, MAGIC, TOTAL_SIZE, VERSION};
        let raw = unsafe {
            let a = std::alloc::alloc(std::alloc::Layout::from_size_align(TOTAL_SIZE, 64).unwrap());
            core::ptr::write_bytes(a, 0, TOTAL_SIZE);
            a
        };
        let h = raw as *mut Header;
        unsafe {
            (*h).version = VERSION;
            (*h).channels = CHANNELS;
            (*h).qpc_freq = 10_000_000;
            AtomicU64::from_ptr(core::ptr::addr_of_mut!((*h).magic)).store(MAGIC, Ordering::Release);
        }
        h
    }

    unsafe fn free_header(h: *mut Header) {
        use bridge_abi::TOTAL_SIZE;
        std::alloc::dealloc(h as *mut u8, std::alloc::Layout::from_size_align(TOTAL_SIZE, 64).unwrap());
    }

    /// A header view that frees itself (the rings live right behind it).
    struct TestHeader(*mut Header);
    impl Drop for TestHeader {
        fn drop(&mut self) {
            unsafe { free_header(self.0) }
        }
    }
    impl TestHeader {
        fn h(&self) -> &Header {
            unsafe { &*self.0 }
        }
        fn source(&self) -> BridgeSource {
            BridgeSource { hdr: self.0 }
        }
    }

    #[test]
    fn bridge_source_pops_discards_and_fills() {
        let t = TestHeader(synthetic_header());
        let h = t.h();
        let mut src = t.source();
        assert_eq!(FrameSource::fill(&src), 0);
        let l: Vec<f32> = (0..300).map(|i| i as f32).collect();
        let r: Vec<f32> = (0..300).map(|i| -(i as f32)).collect();
        assert_eq!(unsafe { h.ring_a_push(&l, &r) }, 300);
        assert_eq!(FrameSource::fill(&src), 300);
        let mut ol = vec![0.0; 100];
        let mut or = vec![0.0; 100];
        assert_eq!(src.pop(&mut ol, &mut or), 100);
        assert_eq!(ol[99], 99.0);
        assert_eq!(src.discard(50), 50);
        assert_eq!(h.a_discarded(), 50);
        assert_eq!(FrameSource::fill(&src), 150);
        let mut ol = vec![0.0; 1000];
        let mut or = vec![0.0; 1000];
        assert_eq!(src.pop(&mut ol, &mut or), 150);
        assert_eq!(ol[149], 299.0); // frames 150..299 after the discard (150 + 149)
    }

    #[test]
    fn negotiated_requires_published_values() {
        assert!(negotiated(0.0, 256).is_err(), "no rate yet");
        assert!(negotiated(48_000.0, 0).is_err(), "no buffer yet");
        assert!(negotiated(48_000.0, 256).is_ok());
        assert_eq!(negotiated(44_100.0, 128).unwrap(), (44_100, 128));
        assert!(negotiated(f64::NAN, 256).is_err());
        assert!(negotiated(48_000.0, 4).is_err());
    }

    /// Unique mapping name for the holder test (never the fixed contract name).
    fn unique_map_name() -> String {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        format!(
            "Local\\eq_asio_router_bridge_v2_hold_test_{}_{}",
            std::process::id(),
            t + u128::from(SEQ.fetch_add(1, Ordering::Relaxed))
        )
    }

    /// A [`BridgeHolder`] keeps the mapping (and its ring contents) alive across
    /// simulated "session stop/start": sessions only *borrow* it, so the ring state
    /// survives even the driver releasing its own handle (VBMatrix driver churn).
    #[test]
    fn held_mapping_survives_session_stop_start() {
        let name = unique_map_name();
        // The driver (VBMatrix side) creates the mapping and fills ring A.
        let driver = Mapping::open_or_create_named(&name).expect("driver create");
        assert!(driver.created());
        let n = 1024usize;
        let l: Vec<f32> = (0..n).map(|i| i as f32).collect();
        let r: Vec<f32> = (0..n).map(|i| -(i as f32)).collect();
        assert_eq!(unsafe { driver.header().ring_a_push(&l, &r) }, n);
        assert_eq!(unsafe { driver.header().fill_a() }, n as u64);

        // The app attaches once and holds the mapping for the process lifetime.
        let holder = BridgeHolder::new();
        assert!(!holder.is_attached());
        let attach = holder.ensure_named(&name).expect("attach");
        assert!(holder.is_attached());

        // Session 1: borrows a source, consumes 300 frames, stops.
        {
            let mut src = attach.source();
            let (mut ol, mut or) = (vec![0.0f32; 300], vec![0.0f32; 300]);
            assert_eq!(src.pop(&mut ol, &mut or), 300);
            assert_eq!(ol[123], 123.0);
        }

        // VBMatrix releases the driver object: the holder keeps the image alive.
        drop(driver);
        assert!(Mapping::open_named(&name).is_some(), "the held mapping must keep the object alive");

        // Session 2 (after "session stop/start"): borrows again, contents intact.
        let attach2 = holder.ensure_named(&name).expect("still attached");
        assert!(Arc::ptr_eq(&attach, &attach2), "the holder must not re-attach");
        {
            let mut src = attach2.source();
            assert_eq!(FrameSource::fill(&src), n - 300, "no frames may be lost across the session restart");
            let (mut ol, mut or) = (vec![0.0f32; n - 300], vec![0.0f32; n - 300]);
            assert_eq!(src.pop(&mut ol, &mut or), n - 300);
            assert_eq!(ol[0], 300.0, "ring must continue exactly where session 1 stopped");
            assert_eq!(or[7], -307.0);
        }
        assert_eq!(attach2.header().a_overruns(), 0);
        assert_eq!(attach2.header().a_discarded(), 0, "counter state survives too");
    }

    #[test]
    fn monitor_tracks_running_and_stall_with_fake_clock() {
        let mut m = BridgeMonitor::new();
        let t0 = Duration::from_secs(10);
        // First sample only establishes the baseline: never stalled.
        let s = m.sample(1, 100, t0);
        assert!(s.driver_running && !s.stalled && s.vbmatrix_running);
        assert_eq!(s.beat_delta, 0);
        // Heartbeat advances every tick: healthy.
        let mut now = t0;
        let mut beat = 100;
        for _ in 0..5 {
            now += Duration::from_millis(100);
            beat += 10;
            let s = m.sample(1, beat, now);
            assert_eq!(s.beat_delta, 10);
            assert!(s.vbmatrix_running, "{s:?}");
        }
        // Heartbeat freezes (VBMatrix closed the slot): stalled after BEAT_STALL.
        let mut stalled_at = None;
        for i in 0..15 {
            now += Duration::from_millis(100);
            let s = m.sample(1, beat, now);
            assert_eq!(s.beat_delta, 0);
            assert!(s.driver_running);
            if s.stalled {
                stalled_at = Some(i);
                assert!(!s.vbmatrix_running);
                break;
            }
        }
        assert_eq!(stalled_at, Some(10), "stall exactly when 1 s of frozen beat passes (ticks 1..=10)");
        // Beat resumes: recovered on the next tick.
        now += Duration::from_millis(100);
        let s = m.sample(1, beat + 5, now);
        assert!(s.vbmatrix_running && !s.stalled);
        // driver_running = 0 (slot stopped) is never "running", stall or not.
        now += Duration::from_millis(100);
        let s = m.sample(0, beat + 10, now);
        assert!(s.driver_running == false && !s.vbmatrix_running);
    }

    #[test]
    fn fill_window_dip_statistics_feed_the_planner() {
        let mut w = FillWindow::new();
        assert_eq!(w.samples(), 0);
        assert_eq!(w.snapshot(48_000.0, 256).events, 0);
        // Steady state: mean fill 1000, dips of 100..300, 256-frame bursts.
        for i in 0..300u64 {
            w.push(if i % 3 == 0 { 300 } else { 100 }, 4);
        }
        let s = w.snapshot(48_000.0, 256);
        assert_eq!(s.events, 300);
        assert!((s.observed_s - 30.0).abs() < 1e-9);
        assert_eq!(s.dip_p999, 300, "p99.9 of 300 samples is the worst dip");
        assert_eq!(s.dip_max, 300);
        assert_eq!(s.gap_p999_frames, 4 * 256, "one tick's delivered frames");
        assert!((s.mean_event_frames - 4.0 * 256.0).abs() < 1e-9);
        assert!((s.staleness_s - 256.0 / 2.0 / 48_000.0).abs() < 1e-12);
        // The planner consumes it like the arrival model's dip.
        let p = router_core::arrival::HeadroomParams { period_frames: 256, cap_frames: 4096 };
        let h = router_core::arrival::required_headroom_frames(&s, &p);
        assert!(h >= 375, "headroom covers the measured dip (300 * 1.25 + look-ahead): {h}");
        // Window slides: after 300 more clean samples the old dips are gone.
        for _ in 0..300 {
            w.push(10, 4);
        }
        assert!(w.snapshot(48_000.0, 256).dip_p999 <= 10);
        w.clear();
        assert_eq!(w.samples(), 0);
    }

    #[test]
    fn target_floors_and_caps() {
        // Derived target below the floor -> floor wins.
        assert_eq!(apply_ring_target_floor(800, 50.0, 48_000), 2400);
        // Floor off (0 ms) -> derived target, capped at a quarter of the ring.
        assert_eq!(apply_ring_target_floor(800, 0.0, 48_000), 800);
        assert_eq!(apply_ring_target_floor(RING_FRAMES, 0.0, 48_000), RING_FRAMES / 4);
        // Loopback target: 2 buffers + safety.
        assert_eq!(loopback_target_frames(2.0, 256, 48_000), 512 + 96);
        assert_eq!(loopback_target_frames(0.0, 0, 48_000).max(1), 1);
        assert!(loopback_target_frames(20.0, 2048, 192_000) <= RING_FRAMES / 4);
    }

    #[test]
    fn loopback_feeder_drains_tap_into_ring_b_and_holds_fill() {
        // Private header, no driver: the feeder is the producer, we are the consumer.
        let h = TestHeader(synthetic_header());
        h.h().set_sample_rate(48_000.0);
        h.h().set_buffer_frames(256);
        let target = loopback_target_frames(2.0, 256, 48_000);
        let (mut prod, cons) = ring::channel(TAP_RING_FRAMES);
        let mut feeder = spawn_loopback(h.0, cons, 48_000, 48_000, target, ResamplerQuality::Linear).expect("spawn");
        // The pre-fill lands before any tap data (the resampler may emit one frame of
        // zero-history output on its first idle call, so allow a couple of frames).
        let deadline = Instant::now() + Duration::from_secs(2);
        while h.h().fill_b() < target as u64 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let prefill = h.h().fill_b();
        assert!(prefill >= target as u64 && prefill <= target as u64 + 4, "silence pre-fill to the target: {prefill}");
        // Real-time paced run: tap pushes and driver pops in the same 5.33 ms cadence,
        // as the Yamaha callback and the VBMatrix-driven driver would.
        let block = vec![0.25f32; 256];
        let (mut dl, mut dr) = (vec![0.0f32; 256], vec![0.0f32; 256]);
        let mut consumed = 0u64;
        let mut saw_audio = false;
        let steps = 94; // ~0.5 s
        for _ in 0..steps {
            prod.push_frames(&block, &block);
            std::thread::sleep(Duration::from_millis(5));
            // driver callback: pop one buffer from ring B
            let got = unsafe { h.h().ring_b_pop(&mut dl, &mut dr) };
            consumed += got as u64;
            if dl[..got].iter().any(|v| v.abs() > 0.1) {
                saw_audio = true;
            }
        }
        feeder.stop();
        let fill_now = h.h().fill_b();
        assert_eq!(consumed, (steps * 256) as u64, "the driver side must never see an underrun");
        assert!(saw_audio, "the loopback carries the tap audio, not just the pre-fill silence");
        assert!(fill_now <= bridge_abi::RING_FRAMES as u64);
        assert!(fill_now >= target as u64 / 4 && fill_now <= target as u64 * 2, "fill stays near the target: {fill_now} (target {target})");
        assert_eq!(h.h().b_dropped(), 0, "ring B never overflows at this rate");
        assert_eq!(h.h().b_underruns(), 0, "the pre-fill covers the start");
    }
}
