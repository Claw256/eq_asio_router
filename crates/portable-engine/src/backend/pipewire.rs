//! System-audio capture backend for Linux: [`PipeWireSource`] implements
//! [`SystemSource`] by recording the **monitor source of the current sink**
//! (i.e. what the user hears).
//!
//! # API-level choice (A8 left this OPEN): PulseAudio `pa_simple`, not native
//! PipeWire
//!
//! A8 DECIDED the bindings must be hand-written `#[link]` FFI (no `-sys`/wrapper
//! crates — their `build.rs` probes pkg-config and breaks the A7 cross-`cargo
//! check` gate). Under that constraint this module targets the **PulseAudio C ABI**
//! (`libpulse-simple` + `libpulse`) rather than the native PipeWire stream API:
//!
//! * **Availability**: every modern PipeWire distro ships the `pipewire-pulse`
//!   compatibility layer, so the Pulse ABI is present and speaks to PipeWire; the
//!   same code also works on plain PulseAudio systems.
//! * **ABI surface**: `pa_simple` is ~6 functions + 2 tiny `#[repr(C)]` structs —
//!   stable since PulseAudio 0.9 and unchanged through the PipeWire
//!   compatibility layer. Native PipeWire needs `pw_main_loop`/`pw_stream` plus
//!   SPA pod construction (`spa_pod_builder`) driven through FFI: a large,
//!   evolving surface where hand-writing POD serialization is fragile and
//!   unverifiable from a non-Linux host.
//! * **Threading**: `pa_simple`'s blocking `pa_simple_read` maps directly onto a
//!   capture thread that stamps arrival times for [`ArrivalTracker`]; the native
//!   API is callback/main-loop driven and would additionally require hand-written
//!   `pw_main_loop` glue.
//!
//! Default source name: `"@DEFAULT_SINK@.monitor"` (the monitor of the default
//! sink), with the candidate chain in [`device_candidates`].
//!
//! # Threading / statistics contract
//!
//! [`SystemSource::arrival`] hands out `&mut ArrivalTracker`, so the tracker lives
//! on the owner. The capture thread records each read as a `(t, n_frames)` arrival
//! event into the lock-free [`ArrivalQueue`] (fixed capacity, no allocation) and
//! touches [`Heartbeat::beat_capture`] directly; the owner replays the queue into
//! the tracker, in order, inside `arrival()`/`fill_snapshot()`. Replay is
//! statistics-identical to calling `ArrivalTracker::on_frames` on the spot — that
//! method is a pure function of the ordered `(t, n)` sequence.
//!
//! Device change: on a read error the stream is torn down and reconnected **once**
//! per [`attach`](SystemSource::attach) session ([`ReconnectState`]); a second
//! failure is reported via [`SourceError::Unavailable`] at attach time and via
//! [`PipeWireSource::last_error`] / [`PipeWireSource::reconnect_state`] for later
//! failures, and the capture thread stops.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use router_core::arrival::{ArrivalSnapshot, ArrivalStats, ArrivalTracker};
use router_core::ring::Producer;
use router_core::watchdog::Heartbeat;

use crate::seams::{SourceError, SystemSource};

// ------------------------------------------------------------------------------------
// Pure policy helpers (hardware-free; unit-tested in `tests/linux_negotiation.rs`)
// ------------------------------------------------------------------------------------

/// Capture chunk length target in milliseconds (arrival event granularity).
pub const CHUNK_MS: u32 = 10;
/// Stereo capture (the system mix is stereo on essentially every setup).
pub const CAPTURE_CHANNELS: u16 = 2;
/// `pa_stream_direction_t`: record.
pub const PA_STREAM_RECORD: c_int = 1;
/// `pa_sample_format_t`: `PA_SAMPLE_FLOAT32LE`.
pub const PA_SAMPLE_FLOAT32LE: c_int = 5;

/// Frames per capture chunk (~[`CHUNK_MS`] at `rate`, clamped).
pub fn chunk_frames(rate: u32) -> u32 {
    let rate = if rate == 0 { 48_000 } else { rate };
    ((rate / 1000).saturating_mul(CHUNK_MS)).clamp(64, 8192)
}

/// `pa_buffer_attr.fragsize` in bytes for `rate`/`channels` float32 capture.
pub fn fragsize_bytes(rate: u32, channels: u16) -> u32 {
    chunk_frames(rate)
        .saturating_mul(u32::from(channels))
        .saturating_mul(4)
}

/// Source-name candidate chain: configured (if any) -> `"@DEFAULT_SINK@.monitor"`
/// -> `"default"`, deduplicated, order preserved.
pub fn device_candidates(configured: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(3);
    if let Some(c) = configured {
        if !c.is_empty() {
            out.push(c.to_string());
        }
    }
    for name in ["@DEFAULT_SINK@.monitor", "default"] {
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// `pa_sample_spec` for float32-LE capture at `rate`/`channels`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaSampleSpec {
    /// `pa_sample_format_t` discriminant.
    pub format: c_int,
    /// Frames per second.
    pub rate: c_uint,
    /// Channel count.
    pub channels: u8,
}

/// `pa_buffer_attr` (only `fragsize` matters for record streams).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaBufferAttr {
    /// Playback: maxlength.
    pub maxlength: u32,
    /// Playback: tlength.
    pub tlength: u32,
    /// Playback: prebuf.
    pub prebuf: u32,
    /// Playback: minreq.
    pub minreq: u32,
    /// Record: requested fragment size in bytes.
    pub fragsize: u32,
}

/// Build the float32-LE capture sample spec.
pub fn float32le_spec(rate: u32, channels: u8) -> PaSampleSpec {
    PaSampleSpec {
        format: PA_SAMPLE_FLOAT32LE,
        rate: if rate == 0 { 48_000 } else { rate },
        channels,
    }
}

/// Deinterleave stereo `FLOAT_LE` input into planar `l`/`r`
/// (`n = min(l.len(), r.len(), interleaved.len() / 2)` frames).
pub fn deinterleave_f32(interleaved: &[f32], l: &mut [f32], r: &mut [f32]) {
    let frames = l.len().min(r.len()).min(interleaved.len() / 2);
    for i in 0..frames {
        l[i] = interleaved[2 * i];
        r[i] = interleaved[2 * i + 1];
    }
}

/// `clock_gettime(CLOCK_MONOTONIC)` in seconds (arrival timestamps).
pub fn monotonic_seconds() -> f64 {
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    let rc = unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0.0;
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

/// Device-change / reconnect state machine: one reconnect per attach session,
/// then [`ReconnectState::Failed`] (see module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconnectState {
    /// No stream yet.
    Disconnected,
    /// Stream is capturing.
    Connected,
    /// A read failed; one reconnect attempt is in progress.
    Reconnecting,
    /// The single allowed reconnect succeeded.
    ReconnectedOnce,
    /// Terminal: reconnect budget spent or reconnect failed.
    Failed,
}

impl ReconnectState {
    /// A stream connection succeeded.
    pub fn on_connected(self) -> Self {
        match self {
            ReconnectState::Disconnected | ReconnectState::Reconnecting => ReconnectState::Connected,
            other => other,
        }
    }

    /// A `pa_simple_read` failed.
    pub fn on_read_error(self) -> Self {
        match self {
            ReconnectState::Connected => ReconnectState::Reconnecting,
            ReconnectState::Disconnected => ReconnectState::Disconnected,
            _ => ReconnectState::Failed,
        }
    }

    /// The reconnect attempt succeeded (budget: once).
    pub fn on_reconnected(self) -> Self {
        match self {
            ReconnectState::Reconnecting => ReconnectState::ReconnectedOnce,
            other => other,
        }
    }

    /// The reconnect attempt failed (terminal).
    pub fn on_reconnect_failed(self) -> Self {
        ReconnectState::Failed
    }

    /// Whether the source gave up.
    pub fn is_failed(self) -> bool {
        self == ReconnectState::Failed
    }
}

// ------------------------------------------------------------------------------------
// Lock-free arrival event queue (capture thread -> owner)
// ------------------------------------------------------------------------------------

/// Fixed-capacity SPSC queue of `(t_seconds, n_frames)` arrival events. The
/// capture thread is the single producer; the [`SystemSource`] owner is the single
/// consumer ([`ArrivalQueue::drain_into`]). No allocation after construction.
pub struct ArrivalQueue {
    t_bits: Box<[AtomicU64]>,
    n: Box<[AtomicU64]>,
    head: AtomicUsize,
    tail: AtomicUsize,
    dropped: AtomicU64,
    mask: usize,
}

impl ArrivalQueue {
    /// A queue of at least `capacity` events (rounded up to a power of two, min 16).
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(16).next_power_of_two();
        let mk = || (0..cap).map(|_| AtomicU64::new(0)).collect::<Vec<_>>().into_boxed_slice();
        ArrivalQueue {
            t_bits: mk(),
            n: mk(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            mask: cap - 1,
        }
    }

    /// Producer side: record that `n` frames arrived at `t` (seconds, monotonic).
    /// On overflow the event is dropped and counted ([`ArrivalQueue::dropped`]).
    pub fn push(&self, t: f64, n: u32) {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) > self.mask {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let idx = tail & self.mask;
        self.t_bits[idx].store(t.to_bits(), Ordering::Relaxed);
        self.n[idx].store(u64::from(n), Ordering::Release);
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
    }

    /// Consumer side: replay queued events into `tracker` in arrival order.
    pub fn drain_into(&self, tracker: &mut ArrivalTracker) {
        let mut head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        while head != tail {
            let idx = head & self.mask;
            let n = self.n[idx].load(Ordering::Acquire) as u32;
            let t = f64::from_bits(self.t_bits[idx].load(Ordering::Relaxed));
            tracker.on_frames(t, n);
            head = head.wrapping_add(1);
        }
        self.head.store(head, Ordering::Release);
    }

    /// Events waiting to be drained.
    pub fn pending(&self) -> usize {
        self.tail.load(Ordering::Acquire).wrapping_sub(self.head.load(Ordering::Acquire))
    }

    /// Events dropped on overflow.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

// ------------------------------------------------------------------------------------
// Hand-written FFI (A8: no -sys crates; module doc explains Pulse-vs-PipeWire)
// ------------------------------------------------------------------------------------

/// Opaque `pa_simple`.
pub type PaSimple = c_void;

/// `struct timespec` (Linux x86_64: `time_t`/`long` are 64-bit).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Timespec {
    /// Seconds.
    pub tv_sec: i64,
    /// Nanoseconds.
    pub tv_nsec: i64,
}

/// `CLOCK_MONOTONIC`.
pub const CLOCK_MONOTONIC: c_int = 1;

#[link(name = "c")]
extern "C" {
    /// `clock_gettime(2)` (duplicated here on purpose: no cross-file coupling
    /// beyond the frozen seams).
    pub fn clock_gettime(clock_id: c_int, tp: *mut Timespec) -> c_int;
}

#[link(name = "pulse-simple")]
extern "C" {
    /// `pa_simple_new`.
    pub fn pa_simple_new(
        server: *const c_char,
        name: *const c_char,
        direction: c_int,
        device: *const c_char,
        stream_name: *const c_char,
        spec: *const PaSampleSpec,
        map: *const c_void,
        attr: *const PaBufferAttr,
        error: *mut c_int,
    ) -> *mut PaSimple;
    /// `pa_simple_free`.
    pub fn pa_simple_free(s: *mut PaSimple);
    /// `pa_simple_read`.
    pub fn pa_simple_read(s: *mut PaSimple, data: *mut c_void, bytes: usize, error: *mut c_int) -> c_int;
    /// `pa_simple_flush`.
    pub fn pa_simple_flush(s: *mut PaSimple, error: *mut c_int) -> c_int;
    /// `pa_simple_latency`.
    pub fn pa_simple_latency(s: *mut PaSimple, error: *mut c_int) -> u64;
}

#[link(name = "pulse")]
extern "C" {
    /// `pa_strerror`.
    pub fn pa_strerror(error: c_int) -> *const c_char;
}

/// Human-readable Pulse error string (best-effort).
pub fn pa_error_text(error: c_int) -> String {
    // SAFETY: `pa_strerror` returns a NUL-terminated static string for any code.
    unsafe {
        let p = pa_strerror(error);
        if p.is_null() {
            format!("pulse error {error}")
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// `pa_simple` handle moved onto the capture thread (single live user).
struct PaThreadHandle(*mut PaSimple);

// SAFETY: the handle is only used on the capture thread; it is freed there too.
unsafe impl Send for PaThreadHandle {}

// ------------------------------------------------------------------------------------
// SystemSource implementation
// ------------------------------------------------------------------------------------

/// Cross-thread bookkeeping of the capture thread.
#[derive(Debug, Default)]
struct Shared {
    /// `ReconnectState` discriminant as usize.
    state: AtomicUsize,
    /// Chunks delivered to the ring.
    chunks: AtomicU64,
    /// Stop flag checked after every blocking read (~one chunk of latency).
    stop: AtomicBool,
    /// Last failure text (written once on the error path).
    last_error: Mutex<Option<String>>,
}

impl Shared {
    fn set_state(&self, s: ReconnectState) {
        self.state.store(s as usize, Ordering::Release);
    }

    fn state(&self) -> ReconnectState {
        match self.state.load(Ordering::Acquire) {
            0 => ReconnectState::Disconnected,
            1 => ReconnectState::Connected,
            2 => ReconnectState::Reconnecting,
            3 => ReconnectState::ReconnectedOnce,
            _ => ReconnectState::Failed,
        }
    }

    fn fail(&self, msg: String) {
        self.set_state(ReconnectState::Failed);
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(msg);
        }
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn clear_stop(&self) {
        self.stop.store(false, Ordering::Relaxed);
    }
}

/// Monitor-source [`SystemSource`] (see module doc for the API-level rationale and
/// the arrival-statistics ownership contract).
pub struct PipeWireSource {
    negotiated_rate: u32,
    channels: u16,
    name: Option<String>,
    stats: Arc<ArrivalStats>,
    tracker: ArrivalTracker,
    heartbeat: Arc<Heartbeat>,
    queue: Arc<ArrivalQueue>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl PipeWireSource {
    /// A source capturing at `rate` Hz from the default monitor
    /// (`"@DEFAULT_SINK@.monitor"`).
    pub fn new(rate: u32) -> Self {
        Self::with_heartbeat(rate, Arc::new(Heartbeat::new()))
    }

    /// Same as [`PipeWireSource::new`], with a shared engine heartbeat.
    pub fn with_heartbeat(rate: u32, heartbeat: Arc<Heartbeat>) -> Self {
        let rate = if rate == 0 { 48_000 } else { rate };
        let stats = ArrivalStats::new(f64::from(rate));
        PipeWireSource {
            negotiated_rate: rate,
            channels: CAPTURE_CHANNELS,
            name: None,
            tracker: ArrivalTracker::new(stats.clone()),
            stats,
            heartbeat,
            queue: Arc::new(ArrivalQueue::new(4096)),
            shared: Arc::new(Shared::default()),
            thread: None,
        }
    }

    /// Capture from a specific monitor/source name instead of the default chain
    /// (see [`device_candidates`]). Takes effect on the next
    /// [`SystemSource::attach`].
    pub fn with_source(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Last capture-thread failure, if any (cleared by [`SystemSource::attach`]).
    pub fn last_error(&self) -> Option<String> {
        self.shared.last_error.lock().ok().and_then(|s| s.clone())
    }

    /// Current reconnect state machine position.
    pub fn reconnect_state(&self) -> ReconnectState {
        self.shared.state()
    }

    /// Chunks delivered to the ring since [`SystemSource::attach`].
    pub fn captured_chunks(&self) -> u64 {
        self.shared.chunks.load(Ordering::Relaxed)
    }

    /// Frames per capture chunk (arrival-event granularity).
    pub fn chunk_frames(&self) -> u32 {
        chunk_frames(self.negotiated_rate)
    }

    /// Stop and join the capture thread (idempotent).
    fn detach(&mut self) {
        if let Some(t) = self.thread.take() {
            self.shared.request_stop();
            drop(t.join());
        }
    }
}

impl SystemSource for PipeWireSource {
    fn negotiated_rate(&self) -> u32 {
        self.negotiated_rate
    }

    fn attach(&mut self, ring: Producer) -> Result<(), SourceError> {
        // Re-attach replaces the ring (and the capture thread) entirely.
        self.detach();
        *self.shared.last_error.lock().map_err(|e| SourceError::Attach(e.to_string()))? = None;
        self.shared.chunks.store(0, Ordering::Relaxed);

        let spec = float32le_spec(self.negotiated_rate, self.channels as u8);
        let attr = PaBufferAttr {
            maxlength: u32::MAX,
            fragsize: fragsize_bytes(self.negotiated_rate, self.channels),
            ..PaBufferAttr::default()
        };
        let candidates = device_candidates(self.name.as_deref());
        // Connect eagerly so a missing device fails `attach`, not silently later.
        let handle = match connect_first(&candidates, &spec, &attr) {
            Ok(h) => h,
            Err(msg) => {
                self.shared.fail(msg.clone());
                return Err(SourceError::Unavailable(msg));
            }
        };
        self.shared.clear_stop();
        self.shared.set_state(ReconnectState::Connected);

        let thread_handle = PaThreadHandle(handle);
        let queue = self.queue.clone();
        let heartbeat = self.heartbeat.clone();
        let shared = self.shared.clone();
        let rate = self.negotiated_rate;
        let channels = self.channels;
        let thread = std::thread::Builder::new()
            .name("pw-capture".into())
            .spawn(move || {
                capture_loop(thread_handle, ring, queue, heartbeat, shared, candidates, rate, channels, spec, attr)
            })
            .map_err(|e| SourceError::Attach(e.to_string()))?;
        self.thread = Some(thread);
        Ok(())
    }

    fn arrival(&mut self) -> &mut ArrivalTracker {
        self.queue.drain_into(&mut self.tracker);
        &mut self.tracker
    }

    fn fill_snapshot(&mut self) -> ArrivalSnapshot {
        self.queue.drain_into(&mut self.tracker);
        self.stats.snapshot()
    }

    fn heartbeat(&self) -> &Heartbeat {
        &self.heartbeat
    }
}

impl Drop for PipeWireSource {
    fn drop(&mut self) {
        self.shared.request_stop();
        if let Some(t) = self.thread.take() {
            drop(t.join());
        }
    }
}

// ------------------------------------------------------------------------------------
// Connect + capture loop internals
// ------------------------------------------------------------------------------------

/// Try the candidate source names in order; return the first live handle.
fn connect_first(
    candidates: &[String],
    spec: &PaSampleSpec,
    attr: &PaBufferAttr,
) -> Result<*mut PaSimple, String> {
    let stream_name = CString::new("portable-engine capture").map_err(|e| e.to_string())?;
    let mut last = String::new();
    for name in candidates {
        let Ok(c_name) = CString::new(name.clone()) else {
            last = format!("{name}: interior NUL in source name");
            continue;
        };
        let mut err: c_int = 0;
        // SAFETY: all pointers are valid, `spec`/`attr` are repr(C) and by-value
        // copies are taken by libpulse before returning.
        let s = unsafe {
            pa_simple_new(
                std::ptr::null(), // default server
                stream_name.as_ptr(),
                PA_STREAM_RECORD,
                c_name.as_ptr(),
                stream_name.as_ptr(),
                spec,
                std::ptr::null(),
                attr,
                &mut err,
            )
        };
        if !s.is_null() {
            return Ok(s);
        }
        last = format!("{name}: pa_simple_new({err}) {}", pa_error_text(err));
    }
    Err(last)
}

#[allow(clippy::too_many_arguments)]
fn capture_loop(
    handle: PaThreadHandle,
    mut ring: Producer,
    queue: Arc<ArrivalQueue>,
    heartbeat: Arc<Heartbeat>,
    shared: Arc<Shared>,
    candidates: Vec<String>,
    rate: u32,
    channels: u16,
    spec: PaSampleSpec,
    attr: PaBufferAttr,
) {
    let frames = chunk_frames(rate) as usize;
    let ch = channels.max(1) as usize;
    // Allocated before the first read (the capture thread is not hard-RT, but no
    // reason to allocate per chunk either).
    let mut buf: Vec<f32> = vec![0.0; frames * ch];
    let mut l: Vec<f32> = vec![0.0; frames];
    let mut r: Vec<f32> = vec![0.0; frames];
    let mut handle = handle;
    let mut state = ReconnectState::Connected;

    loop {
        if shared.stop_requested() {
            break;
        }
        let mut err: c_int = 0;
        // SAFETY: `buf` holds `frames * ch` f32s = the byte count passed.
        let rc = unsafe {
            pa_simple_read(handle.0, buf.as_mut_ptr() as *mut c_void, buf.len() * 4, &mut err)
        };
        if rc < 0 {
            state = state.on_read_error();
            shared.set_state(state);
            heartbeat.beat_capture(); // liveness: the thread saw the device and reacts
            // SAFETY: handle is ours; free before reconnecting.
            unsafe { pa_simple_free(handle.0) };
            handle.0 = std::ptr::null_mut();
            if state.is_failed() {
                shared.fail(format!(
                    "capture lost after reconnect ({} {})",
                    err,
                    pa_error_text(err)
                ));
                break;
            }
            match connect_first(&candidates, &spec, &attr) {
                Ok(h2) => {
                    handle.0 = h2;
                    state = state.on_reconnected();
                    shared.set_state(state);
                }
                Err(msg) => {
                    state = state.on_reconnect_failed();
                    shared.set_state(state);
                    shared.fail(format!("reconnect failed: {msg}"));
                    break;
                }
            }
            continue;
        }

        let now = monotonic_seconds();
        queue.push(now, frames as u32);
        if ch == 2 {
            deinterleave_f32(&buf, &mut l, &mut r);
            ring.push_frames(&l, &r);
        } else {
            // Odd channel counts: feed channel 0 to both ring sides.
            for i in 0..frames {
                l[i] = buf[i * ch];
                r[i] = buf[i * ch];
            }
            ring.push_frames(&l, &r);
        }
        heartbeat.beat_capture();
        shared.chunks.fetch_add(1, Ordering::Relaxed);
    }

    // SAFETY: handle is exclusively ours; freeing here ends the session.
    if !handle.0.is_null() {
        unsafe { pa_simple_free(handle.0) };
    }
}
