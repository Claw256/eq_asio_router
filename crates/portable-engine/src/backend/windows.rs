//! Windows output backend: the master ASIO output over the existing `asio-host` crate
//! (Phase C wiring of the Phase A stub).
//!
//! Same semantics as `router-engine`'s usage of `asio-host`, adapted to the frozen
//! [`crate::seams`] shape:
//!
//! * [`WindowsSink::open`] = registry lookup + `AsioDriver::load_diagnosed`
//!   (`router-engine/src/engine.rs:86-89`): device-busy / holder diagnosis is
//!   **delegated to `asio-host`** (`asio-host/src/busy.rs`), whose `DriverBusy`
//!   error is mapped into [`SinkError::Unavailable`] by [`map_open_error`].
//! * [`OutputSink::negotiate`] = `engine.rs` `Session::open` rate/buffer policy:
//!   requested rate 0 = keep the driver's current rate (an unsupported rate logs and
//!   keeps the current one, it never fails), buffer = the driver's preferred size
//!   clamped into `[min, max]` ([`crate::session::choose_buffer_size`]), output
//!   channel sample types must be supported by `router_core::asio_convert`.
//! * [`OutputSink::start`] = `create_buffers` + `start` with an
//!   [`asio_host::Handler`] whose `buffer_switch` half-buffer loop drives the frozen
//!   [`OutputCallback`](crate::seams::OutputCallback) (the portable `PipelineCallback`)
//!   and converts its f32 output to the driver's native sample type
//!   (`router-engine/src/pipeline.rs:174-231` geometry, `engine.rs:880` ordering).
//!
//! Constraints recorded for callers:
//!
//! * `asio-host` supports **one hosted driver per process** (its `HANDLER` callback
//!   slot is a process-wide static, `asio-host/src/lib.rs:157`) — like the router,
//!   the portable CLI opens a single master per run.
//! * All driver calls stay on the thread that opened the driver (STA COM);
//!   `buffer_switch` runs on the driver's audio thread.
//! * Tick domain: [`QpcClock`] / QPC ticks — `now_ticks` passed to
//!   [`OutputCallback::render`] reads the same domain (A3 contract).

use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use asio_host::{AsioDriver, BufferSizes, Handler, HostError, OutputReady};
use windows::Win32::Foundation::HWND;

use router_core::asio_convert::{is_supported, write_f32_to_asio, write_silence};
use router_core::processor::validate_output_channels;

use crate::pipeline::{Live, RESET_PANIC, RESET_RATE, RESET_REQUEST};
use crate::seams::{Clock, OutputCallback, OutputSink, SinkError, StreamRequest, StreamSpec};
use crate::session::choose_buffer_size;

// ------------------------------------------------------------------------------------
// QPC clock (the tick domain of this backend)
// ------------------------------------------------------------------------------------

#[link(name = "kernel32")]
extern "system" {
    fn QueryPerformanceCounter(out: *mut i64) -> i32;
    fn QueryPerformanceFrequency(out: *mut i64) -> i32;
}

/// `QueryPerformanceCounter` in ticks.
#[inline]
pub fn qpc() -> i64 {
    let mut t = 0i64;
    // SAFETY: valid out pointer; QPC cannot fail on supported Windows versions.
    unsafe {
        let _ = QueryPerformanceCounter(&mut t);
    }
    t
}

/// `QueryPerformanceFrequency` in Hz (never 0).
fn qpc_freq() -> i64 {
    let mut f = 0i64;
    // SAFETY: valid out pointer.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut f);
    }
    f.max(1)
}

/// [`Clock`] over QPC: the tick domain [`OutputCallback::render`] is called in, so a
/// `PipelineCallback` built with this clock self-times in the same domain the sink
/// reports (`router-engine/src/pipeline.rs:143/154` semantics).
pub struct QpcClock {
    tps: i64,
}

impl QpcClock {
    pub fn new() -> Self {
        QpcClock { tps: qpc_freq() }
    }
}

impl Default for QpcClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for QpcClock {
    fn now_ticks(&self) -> i64 {
        qpc()
    }
    fn ticks_per_second(&self) -> i64 {
        self.tps
    }
    fn sleep_until(&self, ticks: i64) {
        let now = self.now_ticks();
        if ticks > now {
            let us = (ticks - now) as f64 / self.tps as f64 * 1e6;
            std::thread::sleep(Duration::from_micros(us.max(0.0) as u64));
        }
    }
}

// ------------------------------------------------------------------------------------
// Driver loading (test seam)
// ------------------------------------------------------------------------------------

/// Opens (finds + loads + inits) the master ASIO driver by name. Test seam: the real
/// loader is [`default_master_loader`]; [`WindowsSink::open_with`] accepts fakes so
/// the error mapping runs without hardware (mirrors `router-engine/src/engine.rs:74`).
pub type LoaderFn = dyn Fn(&str) -> Result<AsioDriver, HostError> + Send + Sync;

/// Real loader: registry lookup (`asio_host::find_driver`), then
/// `AsioDriver::load_diagnosed(&info, HWND::default())` — the init-failure path
/// diagnoses busy/holder processes and fails with `HostError::DriverBusy`
/// (`engine.rs:86-89` semantics; nothing is ever closed on the user's behalf).
pub fn default_master_loader(name: &str) -> Result<AsioDriver, HostError> {
    let info = asio_host::find_driver(name)?;
    AsioDriver::load_diagnosed(&info, HWND::default())
}

/// Maps `asio-host` open failures to [`SinkError`] (device busy/holders delegated to
/// `asio-host`, whose `DriverBusy` already carries the holder list).
fn map_open_error(name: &str, e: HostError) -> SinkError {
    match e {
        HostError::NotFound(_) => SinkError::Unavailable(format!("ASIO driver \"{name}\" not found in HKLM\\SOFTWARE\\ASIO")),
        HostError::DriverBusy { holders } => {
            SinkError::Unavailable(format!("ASIO driver \"{name}\" is {}", asio_host::describe_holders(&holders)))
        }
        other => SinkError::Unavailable(format!("ASIO driver \"{name}\": {other}")),
    }
}

fn asio_msg(call: &str, e: HostError) -> String {
    format!("ASIO {call}: {e}")
}

/// Buffer size for the negotiated stream: the driver's preferred size clamped into
/// `[min, max]` — `logic::choose_buffer_size` / `engine.rs` policy
/// ([`crate::session::choose_buffer_size`]).
fn buffer_from_sizes(sizes: &BufferSizes) -> u32 {
    choose_buffer_size(sizes.min, sizes.max, sizes.preferred).max(1) as u32
}

// ------------------------------------------------------------------------------------
// RT side: buffer pointers + ASIO handler
// ------------------------------------------------------------------------------------

/// Buffer pointers of the two created output channels, filled by the control thread
/// after `create_buffers` (the driver allocates them) and before `start`
/// (`router-engine/src/pipeline.rs` `PtrTable` geometry).
#[derive(Default)]
struct PtrTable {
    slots: [[AtomicPtr<c_void>; 2]; 2],
}

impl PtrTable {
    fn set(&self, ch: usize, ptrs: [*mut c_void; 2]) {
        if let Some(s) = self.slots.get(ch) {
            s[0].store(ptrs[0], Relaxed);
            s[1].store(ptrs[1], Relaxed);
        }
    }
    #[inline]
    fn get(&self, ch: usize, half: usize) -> *mut c_void {
        match self.slots.get(ch).and_then(|s| s.get(half)) {
            Some(p) => p.load(Relaxed),
            None => std::ptr::null_mut(),
        }
    }
}

/// The `asio_host::Handler` installed into the driver: renders one block per
/// `buffer_switch` through the wrapped [`OutputCallback`] (f32 slices), then converts
/// into the driver's native sample type for the given half.
struct AsioHandler {
    cb: Box<dyn OutputCallback>,
    l: Vec<f32>,
    r: Vec<f32>,
    ptrs: Arc<PtrTable>,
    types: [i32; 2],
    frames: usize,
    out_ready: OutputReady,
    live: Option<Arc<Live>>,
}

impl AsioHandler {
    /// Writes the rendered block into the native buffers of `half`.
    ///
    /// # Safety
    /// The pointers in the table must be valid driver buffers of at least `frames`
    /// samples of the recorded sample types (guaranteed while the stream is created).
    #[inline]
    unsafe fn write_half(&self, half: usize) {
        let half = half & 1;
        for (ch, src) in [&self.l[..], &self.r[..]].into_iter().enumerate() {
            let p = self.ptrs.get(ch, half);
            if !p.is_null() {
                write_f32_to_asio(p as *mut u8, self.types[ch], self.frames, src);
            }
        }
    }

    /// Silences both halves (dead callback / panic path).
    ///
    /// # Safety
    /// As [`write_half`](Self::write_half).
    #[inline]
    unsafe fn silence_half(&self, half: usize) {
        let half = half & 1;
        for ch in 0..2 {
            let p = self.ptrs.get(ch, half);
            if !p.is_null() {
                write_silence(p as *mut u8, self.types[ch], self.frames);
            }
        }
    }
}

impl Handler for AsioHandler {
    fn buffer_switch(&mut self, index: i32) {
        let half = (index & 1) as usize;
        let now = qpc();
        // Disjoint field borrows so the callback and its scratch buffers are both
        // reachable without aliasing `self`.
        let Self { cb, l, r, .. } = self;
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cb.render(&mut [&mut l[..], &mut r[..]], now);
        }))
        .is_err()
        {
            if let Some(live) = &self.live {
                live.panicked.store(true, Relaxed);
                live.request_reset(RESET_PANIC);
            }
            // SAFETY: driver buffers of `frames` samples, stream is live.
            unsafe {
                self.silence_half(half);
            }
            return;
        }
        // SAFETY: as above; the callback filled the scratch buffers.
        unsafe {
            self.write_half(half);
        }
        self.out_ready.call();
    }

    fn reset_request(&mut self) {
        if let Some(live) = &self.live {
            live.request_reset(RESET_REQUEST);
        }
    }

    fn sample_rate_changed(&mut self, _rate: f64) {
        if let Some(live) = &self.live {
            live.request_reset(RESET_RATE);
        }
    }

    fn overload(&mut self) {
        if let Some(live) = &self.live {
            live.xruns.fetch_add(1, Relaxed);
        }
    }
}

// ------------------------------------------------------------------------------------
// Callback delegate
// ------------------------------------------------------------------------------------

/// The [`OutputCallback`] type driven by [`WindowsSink`] (`AlsaCallback`-style
/// delegate): wraps the portable `PipelineCallback` and optionally the shared
/// [`Live`] so driver reset requests / rate changes / overloads reach the supervision
/// loop (`router-engine` `AudioCallback` `Handler` semantics).
pub struct WindowsCallback {
    inner: Box<dyn OutputCallback>,
    live: Option<Arc<Live>>,
}

impl WindowsCallback {
    /// Wrap a callback.
    pub fn new(inner: impl OutputCallback) -> Self {
        WindowsCallback { inner: Box::new(inner), live: None }
    }

    /// Wrap an already-boxed callback.
    pub fn from_boxed(inner: Box<dyn OutputCallback>) -> Self {
        WindowsCallback { inner, live: None }
    }

    /// Share the engine [`Live`] counters with the driver event handlers
    /// (`reset_request` -> `Live::request_reset`, `overload` -> `xruns`).
    pub fn with_live(mut self, live: Arc<Live>) -> Self {
        self.live = Some(live);
        self
    }
}

impl OutputCallback for WindowsCallback {
    fn render(&mut self, out: &mut [&mut [f32]], now_ticks: i64) {
        self.inner.render(out, now_ticks)
    }

    fn on_xrun(&mut self) {
        self.inner.on_xrun()
    }
}

// ------------------------------------------------------------------------------------
// Sink
// ------------------------------------------------------------------------------------

/// Master output over the Windows ASIO host (`OutputSink` over `asio_host::AsioDriver`).
///
/// Lifecycle: [`WindowsSink::open`] (driver) -> [`OutputSink::negotiate`] (rate,
/// buffer, sample types) -> [`OutputSink::start`] (buffers + stream) ->
/// [`OutputSink::stop`]. Dropping the sink releases the driver
/// (`AsioDriver::drop` stops/disposes anything left).
pub struct WindowsSink {
    name: String,
    outputs: [u32; 2],
    /// Output channel pair actually negotiated (`validate_output_channels`).
    out_pair: [usize; 2],
    driver: Option<AsioDriver>,
    spec: StreamSpec,
    types: [i32; 2],
    frames: u32,
    buffers_created: bool,
    started: bool,
}

impl WindowsSink {
    /// Opens the master ASIO driver `name` with the real loader and the default
    /// output channel pair (config `output_channels` semantics go through
    /// [`open_with`](Self::open_with)). Call on the thread that will own the driver
    /// (COM STA is initialised here).
    pub fn open(name: &str) -> Result<WindowsSink, SinkError> {
        Self::open_with(name, [0, 1], Arc::new(default_master_loader))
    }

    /// As [`open`](Self::open), with explicit output channels and an injectable
    /// driver loader (tests: error mapping without hardware).
    pub fn open_with(name: &str, outputs: [u32; 2], loader: Arc<LoaderFn>) -> Result<WindowsSink, SinkError> {
        // ASIO drivers register as `ThreadingModel=Apartment`.
        asio_host::init_com_sta();
        let driver = (loader)(name).map_err(|e| map_open_error(name, e))?;
        Ok(WindowsSink {
            name: name.to_string(),
            outputs,
            out_pair: [0, 1],
            driver: Some(driver),
            spec: StreamSpec::default(),
            types: [0; 2],
            frames: 0,
            buffers_created: false,
            started: false,
        })
    }

    /// Name this sink was opened with.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the ASIO stream is currently running.
    pub fn is_running(&self) -> bool {
        self.started
    }
}

impl OutputSink for WindowsSink {
    type Callback = WindowsCallback;

    fn negotiate(&mut self, want: &StreamRequest) -> Result<StreamSpec, SinkError> {
        if self.started {
            return Err(SinkError::Negotiation("stream is running; stop before renegotiating".into()));
        }
        let drv = self.driver.as_ref().ok_or_else(|| SinkError::Negotiation("driver not open".into()))?;

        // Output capacity (engine.rs: `n_out < 2` is fatal).
        let (_n_in, n_out) = drv.channels().map_err(|e| SinkError::Negotiation(asio_msg("getChannels", e)))?;
        if n_out < 2 {
            return Err(SinkError::Negotiation(format!("master \"{}\" has {} output channels, 2 required", self.name, n_out)));
        }

        // Rate policy (engine.rs `Session::open`): 0 = keep current; an unsupported
        // rate keeps the current one instead of failing.
        if want.rate_hz != 0 {
            let want_rate = f64::from(want.rate_hz);
            let cur = drv.sample_rate().unwrap_or(0.0);
            if (cur - want_rate).abs() > 0.5 {
                if drv.can_sample_rate(want_rate) {
                    if let Err(e) = drv.set_sample_rate(want_rate) {
                        eprintln!("windows sink: setSampleRate({}) failed: {e}; keeping {cur} Hz", want.rate_hz);
                    }
                } else {
                    eprintln!("windows sink: master does not support {} Hz; keeping {cur} Hz", want.rate_hz);
                }
            }
        }
        let rate = drv.sample_rate().map_err(|e| SinkError::Negotiation(asio_msg("getSampleRate", e)))?.round() as u32;
        if rate == 0 {
            return Err(SinkError::Negotiation("master reports a zero sample rate".into()));
        }

        // Buffer: driver preferred clamped into [min, max] (choose_buffer_size).
        let sizes = drv.buffer_sizes().map_err(|e| SinkError::Negotiation(asio_msg("getBufferSize", e)))?;
        let buffer = buffer_from_sizes(&sizes);

        // Output channel pair + native sample types (engine.rs:649).
        let outputs = validate_output_channels(self.outputs, n_out as usize);
        let mut types = [0i32; 2];
        for (i, &o) in outputs.iter().enumerate() {
            let ci = drv
                .channel_info(o as i32, false)
                .map_err(|e| SinkError::Negotiation(asio_msg("getChannelInfo", e)))?;
            if !is_supported(ci.sample_type) {
                return Err(SinkError::Negotiation(format!(
                    "output channel {o} has unsupported ASIO sample type {}",
                    ci.sample_type
                )));
            }
            types[i] = ci.sample_type;
        }

        self.out_pair = outputs;
        self.types = types;
        self.frames = buffer;
        self.spec = StreamSpec { rate_hz: rate, buffer_frames: buffer, channels: crate::pipeline::OUT_CH as u16 };
        Ok(self.spec)
    }

    fn start(&mut self, cb: Self::Callback) -> Result<(), SinkError> {
        if self.started {
            return Err(SinkError::Start("stream already running".into()));
        }
        if self.frames == 0 {
            return Err(SinkError::Start("no negotiated stream; call negotiate first".into()));
        }
        let frames = self.frames as usize;
        let ptrs = Arc::new(PtrTable::default());
        let drv = self.driver.as_mut().ok_or_else(|| SinkError::Start("driver not open".into()))?;
        let out_ready = drv.output_ready_handle();
        let handler = AsioHandler {
            cb: cb.inner,
            l: vec![0.0; frames],
            r: vec![0.0; frames],
            ptrs: ptrs.clone(),
            types: self.types,
            frames,
            out_ready,
            live: cb.live,
        };
        // The validated pair from `negotiate` (falling back to 0/1) is what the
        // handler writes; `buffer_ptrs` indexes match this order.
        let bufs = [(false, self.out_pair[0] as i32), (false, self.out_pair[1] as i32)];
        drv.create_buffers(&bufs, self.frames as i32, Box::new(handler))
            .map_err(|e| SinkError::Start(asio_msg("createBuffers", e)))?;
        self.buffers_created = true;
        for ch in 0..2 {
            ptrs.set(ch, drv.buffer_ptrs(ch));
        }
        // Start from silence in both halves (engine.rs semantics).
        // SAFETY: pointers come from the driver's `create_buffers` for `frames` samples.
        unsafe {
            for half in 0..2 {
                for ch in 0..2 {
                    let p = ptrs.get(ch, half);
                    if !p.is_null() {
                        write_silence(p as *mut u8, self.types[ch], frames);
                    }
                }
            }
        }
        drv.start().map_err(|e| SinkError::Start(asio_msg("start", e)))?;
        self.started = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        let mut result = Ok(());
        if let Some(drv) = self.driver.as_mut() {
            if self.started {
                if let Err(e) = drv.stop() {
                    result = Err(SinkError::Stop(asio_msg("stop", e)));
                }
            }
            if self.buffers_created {
                // Disposes the buffers and releases the handler (the RT callback can
                // no longer run); `AsioDriver::drop` covers the abort paths.
                if let Err(e) = drv.dispose_buffers() {
                    if result.is_ok() {
                        result = Err(SinkError::Stop(asio_msg("disposeBuffers", e)));
                    }
                }
                self.buffers_created = false;
            }
        }
        self.started = false;
        result
    }

    fn spec(&self) -> &StreamSpec {
        &self.spec
    }
}

// ------------------------------------------------------------------------------------
// Tests (no hardware: injected loader + pure mapping helpers)
// ------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_choice_follows_choose_buffer_size() {
        // preferred inside the range is granted verbatim.
        let s = BufferSizes { min: 32, max: 2048, preferred: 512, granularity: 32 };
        assert_eq!(buffer_from_sizes(&s), 512);
        // preferred above max clamps to max; below min clamps to min (0 -> min).
        let s = BufferSizes { min: 32, max: 2048, preferred: 99_999, granularity: 32 };
        assert_eq!(buffer_from_sizes(&s), 2048);
        let s = BufferSizes { min: 32, max: 2048, preferred: 0, granularity: 32 };
        assert_eq!(buffer_from_sizes(&s), 32);
        // never zero even for a degenerate range.
        let s = BufferSizes { min: 0, max: 0, preferred: 0, granularity: -1 };
        assert_eq!(buffer_from_sizes(&s), 1);
    }

    #[test]
    fn open_errors_map_to_unavailable() {
        // not found
        let e = map_open_error("Ghost ASIO", HostError::NotFound("Ghost ASIO".into()));
        assert!(matches!(e, SinkError::Unavailable(_)), "{e}");
        assert!(e.to_string().contains("Ghost ASIO"));
        // busy with holders (delegated diagnosis, message names the holders)
        let holders = vec![asio_host::Holder { pid: 42, exe_name: "daw.exe".into(), exe_path: None }];
        let e = map_open_error("Yamaha Steinberg USB ASIO", HostError::DriverBusy { holders });
        assert!(matches!(e, SinkError::Unavailable(_)), "{e}");
        let msg = e.to_string();
        assert!(msg.contains("in use by daw.exe (PID 42)"), "{msg}");
        // other host failures keep the call context
        let e = map_open_error("X", HostError::InitFailed);
        assert!(e.to_string().contains("X"), "{e}");
    }

    #[test]
    fn open_with_propagates_loader_failure_without_hardware() {
        let loader: Arc<LoaderFn> = Arc::new(|name| Err(HostError::NotFound(name.to_string())));
        let r = WindowsSink::open_with("Missing ASIO", [0, 1], loader);
        match r {
            Err(SinkError::Unavailable(msg)) => assert!(msg.contains("Missing ASIO"), "{msg}"),
            Err(e) => panic!("expected Unavailable, got {e}"),
            Ok(_) => panic!("expected the loader error to propagate"),
        }
        // A successful open is not covered here: it would need a real COM driver
        // (hardware validation checklist instead).
    }

    #[test]
    fn qpc_clock_reads_monotonic_ticks() {
        let c = QpcClock::new();
        assert!(c.ticks_per_second() > 0);
        let a = c.now_ticks();
        let b = c.now_ticks();
        assert!(b >= a, "QPC must not go backwards");
    }
}
