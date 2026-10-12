//! The `IASIO` COM object "EQ ASIO Router" **v2 — a pure bridge**.
//!
//! The driver hosts nothing and runs no DSP: it shuttles samples through
//! `bridge-abi` shared memory only.
//!
//! - 8 in / 8 out, `Int32LSB` on every channel; the engine pair is 0/1
//!   (outputs 2..8 are discarded after the shuttle copy, inputs 2..8 silence).
//! - `init` attaches or creates the mapping and **never fails hard** when the
//!   app is absent — the driver works standalone, audio simply drops.
//! - `start` spawns the [`crate::pacing`] thread that fires the host's
//!   `bufferSwitch`/`bufferSwitchTimeInfo` with `directProcess = TRUE`;
//!   `stop` joins it (v1 `ResetThread` pattern for the in-callback case).
//! - Per callback (real-time rules): Int32→f32 of the host-written outputs
//!   into ring A, ring B→Int32 into the inputs (silence + underrun count when
//!   empty), `driver_beat`/position update, then the host callback. No
//!   allocation, no locks, no syscalls.

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use asio_abi::*;

use crate::com::{guard, E_NOINTERFACE, E_POINTER, E_UNEXPECTED, OBJECT_COUNT, S_OK};
use crate::ids::*;
use crate::pacing::{self, PacingThread, StopOutcome, Tick};
use crate::{convert, dlog};
use bridge_abi::Header;

/// Defines an `IASIO` method with the right ABI (`thiscall` on x86, `system`
/// elsewhere) whose body cannot unwind into the host.
macro_rules! asio_method {
    (fn $name:ident($($arg:ident : $t:ty),*) -> $ret:ty = $fail:expr, $body:block) => {
        #[cfg(target_arch = "x86")]
        unsafe extern "thiscall" fn $name($($arg: $t),*) -> $ret {
            guard($fail, || $body)
        }
        #[cfg(not(target_arch = "x86"))]
        unsafe extern "system" fn $name($($arg: $t),*) -> $ret {
            guard($fail, || $body)
        }
    };
}

// ---------------------------------------------------------------- format contract

/// Sample rates the driver accepts (negotiated with the host).
pub const SAMPLE_RATES: [f64; 6] = [44_100.0, 48_000.0, 88_200.0, 96_000.0, 176_400.0, 192_000.0];
/// Smallest buffer the host may request (frames).
pub const BUFFER_MIN: i32 = 64;
/// Largest buffer the host may request (frames).
pub const BUFFER_MAX: i32 = 2048;
/// Preferred buffer size (frames).
pub const BUFFER_PREFERRED: i32 = 256;
/// Buffer size granularity (frames): sizes are multiples of this.
pub const BUFFER_GRANULARITY: i32 = 64;

/// True if `rate` is one of [`SAMPLE_RATES`] (within 0.5 Hz).
pub fn rate_supported(rate: f64) -> bool {
    SAMPLE_RATES.iter().any(|&r| (r - rate).abs() < 0.5)
}

/// True if `size` is inside the advertised range and on the granularity grid.
pub fn buffer_size_valid(size: i32) -> bool {
    size >= BUFFER_MIN && size <= BUFFER_MAX && size % BUFFER_GRANULARITY == 0
}

// ---------------------------------------------------------------- runtime state

/// Raw pointer to [`Rt`] for the pacing closure (the closure is `Send`).
struct RtPtr(*mut Rt);
// The runtime is only mutated by the pacing thread while running; the control
// thread touches it only before spawn / after the thread stopped.
unsafe impl Send for RtPtr {}

impl RtPtr {
    /// One tick on the pacing thread. (A method, so the closure captures the
    /// `Send` wrapper as a whole rather than the raw pointer field.)
    ///
    /// # Safety
    /// The pointee must live until the pacing thread has exited.
    unsafe fn tick(&self) -> Tick {
        (*self.0).pacing_tick()
    }
}

/// Everything the pacing thread needs, allocated at `createBuffers` and freed
/// at `disposeBuffers` (only once the pacing thread has exited).
struct Rt {
    frames: usize,
    host: AsioCallbacks,
    use_time_info: bool,
    /// Bridge header (null when the mapping is unavailable — shuttle skipped).
    header: *const Header,
    /// Negotiated rate as `f64` bits (written by `setSampleRate`).
    rate_bits: AtomicU64,
    /// Pending rate change as `f64` bits, `0` = none (set while running so the
    /// pacing thread can notify the host and re-anchor).
    pending_rate: AtomicU64,
    /// Cleared by `stop()` so an in-flight tick skips its fire.
    running: AtomicBool,
    /// Samples since `start()` — block start of the current/next callback.
    pos: AtomicU64,
    /// `start_qpc_ns + pos × 1e9 / rate` (monotonic, derived — no QPC on the
    /// real-time path).
    ts_ns: AtomicU64,
    /// QPC ns captured at `start()` (written before the thread spawns).
    start_ns: u64,
    /// Frames since start; advanced by the pacing thread after each fire.
    frames_done: u64,
    /// Half `index` of this tick (0, then 1, ...). Pacing thread only.
    index: i32,
    /// Base of `[half0 | half1]` Int32 buffers for output channels 0/1
    /// (`null` = not requested by the host).
    out_pair: [*mut i32; 2],
    /// Same for input channels 0/1.
    in_pair: [*mut i32; 2],
    /// Requested input channels 2..8: zeroed every tick (returned silence).
    zero_ins: Vec<*mut i32>,
    /// Preallocated f32 scratch, `frames` each (never reallocated).
    scratch_l: Vec<f32>,
    scratch_r: Vec<f32>,
}

impl Rt {
    /// One pacing tick. Called under MMCSS on the pacing thread.
    fn pacing_tick(&mut self) -> Tick {
        if !self.running.load(Ordering::Acquire) {
            return Tick::Continue;
        }
        // Host notification for a rate change (control thread set the flag).
        let pend = self.pending_rate.swap(0, Ordering::Acquire);
        let changed = if pend != 0 {
            let r = f64::from_bits(pend);
            // SAFETY: callbacks valid while buffers exist; we are running.
            unsafe { (self.host.sample_rate_did_change)(r) };
            Some(r)
        } else {
            None
        };
        // SAFETY: all pointers in `Rt` are valid while buffers exist.
        unsafe { self.on_tick() };
        match changed {
            Some(r) => Tick::RateChanged(r.round() as u32),
            None => Tick::Continue,
        }
    }

    /// The shuttle. Real-time: allocation-free, lock-free, syscall-free.
    ///
    /// # Safety
    /// Every stored pointer must refer to live buffers of `frames` samples per
    /// half and `header` (when non-null) to a valid mapping image.
    unsafe fn on_tick(&mut self) {
        if !self.running.load(Ordering::Acquire) {
            return;
        }
        let n = self.frames;
        let idx = self.index as usize;
        let prev = idx ^ 1;
        let rate = f64::from_bits(self.rate_bits.load(Ordering::Relaxed));
        let hdr = self.header;

        // 1. ring A <- outputs the host wrote during the *previous* callback
        //    (channels 0/1; outputs 2..8 are discarded here by design).
        if !hdr.is_null() {
            self.scratch_l.fill(0.0);
            self.scratch_r.fill(0.0);
            for ch in 0..2 {
                let p = self.out_pair[ch];
                if p.is_null() {
                    continue;
                }
                let src = core::slice::from_raw_parts(p.add(prev * n), n);
                if ch == 0 {
                    convert::i32_slice_to_f32(src, &mut self.scratch_l);
                } else {
                    convert::i32_slice_to_f32(src, &mut self.scratch_r);
                }
            }
            (*hdr).ring_a_push(&self.scratch_l, &self.scratch_r);
        }

        // 2. ring B -> inputs of *this* half; silence on underrun / no mapping.
        if !hdr.is_null() {
            let k = (*hdr).ring_b_pop(&mut self.scratch_l, &mut self.scratch_r);
            if k < n {
                self.scratch_l[k..].fill(0.0);
                self.scratch_r[k..].fill(0.0);
            }
        } else {
            self.scratch_l.fill(0.0);
            self.scratch_r.fill(0.0);
        }
        for ch in 0..2 {
            let p = self.in_pair[ch];
            if p.is_null() {
                continue;
            }
            if ch == 0 {
                convert::write_f32(&self.scratch_l, p.add(idx * n) as *mut u8, ASIOST_INT32_LSB);
            } else {
                convert::write_f32(&self.scratch_r, p.add(idx * n) as *mut u8, ASIOST_INT32_LSB);
            }
        }
        for &p in &self.zero_ins {
            core::ptr::write_bytes(p.add(idx * n) as *mut u8, 0, n * 4);
        }

        // 3. heartbeat + monotonic position (block start), then fire the host.
        let pos = self.frames_done;
        let ts = self.start_ns + pacing::position_time_ns(pos, rate as u64);
        if !hdr.is_null() {
            (*hdr).driver_beat.fetch_add(1, Ordering::Release);
        }
        self.pos.store(pos, Ordering::Release);
        self.ts_ns.store(ts, Ordering::Release);
        if self.use_time_info {
            let mut ti = AsioTime::zeroed();
            ti.time_info.speed = 1.0;
            ti.time_info.system_time = AsioInt64::from_u64(ts);
            ti.time_info.sample_position = AsioInt64::from_u64(pos);
            ti.time_info.sample_rate = rate;
            ti.time_info.flags = K_SYSTEM_TIME_VALID | K_SAMPLE_POSITION_VALID | K_SAMPLE_RATE_VALID;
            (self.host.buffer_switch_time_info)(&mut ti, idx as i32, ASIO_TRUE);
        } else {
            (self.host.buffer_switch)(idx as i32, ASIO_TRUE);
        }

        // 4. advance for the next tick.
        self.frames_done = pos + n as u64;
        self.index = (idx ^ 1) as i32;
    }
}

// ---------------------------------------------------------------- driver object

struct Inner {
    error: String,
    initialized: bool,
    /// Negotiated rate (default 48 kHz until the host sets one).
    rate: f64,
    /// Buffer size negotiated at `createBuffers` (preferred until then).
    negotiated: i32,
    mapping: Option<bridge_abi::Mapping>,
    /// Mirror of `mapping.header()`; null when the bridge is unavailable.
    header: *const Header,
    rt: *mut Rt,
    our_bufs: Vec<Box<[i32]>>,
    active_in: [bool; 8],
    active_out: [bool; 8],
    buffers_created: bool,
    started: bool,
    pacing: Option<PacingThread>,
}

impl Inner {
    fn new() -> Self {
        Inner {
            error: String::new(),
            initialized: false,
            rate: 48_000.0,
            negotiated: BUFFER_PREFERRED,
            mapping: None,
            header: core::ptr::null(),
            rt: core::ptr::null_mut(),
            our_bufs: Vec::new(),
            active_in: [false; 8],
            active_out: [false; 8],
            buffers_created: false,
            started: false,
            pacing: None,
        }
    }

    /// Attaches to (or creates) the bridge mapping. Never fails hard: on any
    /// problem the driver runs standalone and audio drops.
    fn attach_bridge(&mut self) {
        match bridge_abi::open_or_create() {
            Some(m) => {
                let created = m.created();
                {
                    let h = unsafe { m.header() };
                    h.set_sample_rate(self.rate);
                    h.set_buffer_frames(self.negotiated as u32);
                    // A fresh driver instance is not running (a previous crash
                    // may have left the flag set); ring indices are untouched.
                    h.driver_running.store(0, Ordering::Release);
                }
                self.header = unsafe { m.header() };
                self.mapping = Some(m);
                dlog!("bridge mapping {}", if created { "created" } else { "opened (existing)" });
            }
            None => {
                self.header = core::ptr::null();
                dlog!("bridge mapping unavailable — running standalone (audio drops)");
            }
        }
    }

    fn init(&mut self, _sys: *mut c_void) -> AsioBool {
        if self.initialized {
            return ASIO_TRUE;
        }
        self.attach_bridge();
        self.initialized = true;
        ASIO_TRUE
    }

    fn create_buffers(
        &mut self,
        rt_slot: &AtomicPtr<Rt>,
        infos: *mut AsioBufferInfo,
        num: i32,
        size: i32,
        callbacks: *const AsioCallbacks,
    ) -> AsioError {
        if !self.initialized {
            return ASE_NOT_PRESENT;
        }
        if self.buffers_created {
            return ASE_INVALID_MODE;
        }
        if infos.is_null() || callbacks.is_null() || num <= 0 {
            return ASE_INVALID_PARAMETER;
        }
        if !buffer_size_valid(size) {
            self.error = format!(
                "unsupported buffer size {size} (allowed {BUFFER_MIN}..={BUFFER_MAX}, granularity {BUFFER_GRANULARITY})"
            );
            dlog!("{}", self.error);
            return ASE_INVALID_MODE;
        }
        // SAFETY: the host passes a live `AsioCallbacks` struct.
        let host = unsafe { *callbacks };

        // Plan M0 gate: we only support direct processing (we fire
        // `bufferSwitch(..., directProcess = TRUE)` from our pacing thread).
        if (host.buffer_switch as usize == 0) && (host.buffer_switch_time_info as usize == 0) {
            self.error = "host registered no bufferSwitch callback (directProcess = FALSE not supported)".into();
            dlog!("{}", self.error);
            return ASE_INVALID_MODE;
        }
        if host.asio_message as usize != 0 {
            // SAFETY: host callback, plain query with null pointers.
            let wants_deferred = unsafe {
                (host.asio_message)(K_ASIO_SELECTOR_SUPPORTED, K_ASIO_DEFER_PROCESS, core::ptr::null_mut(), core::ptr::null_mut())
            };
            if wants_deferred == 1 {
                self.error = "host requires deferred processing (directProcess = FALSE)".into();
                dlog!("{}", self.error);
                return ASE_INVALID_MODE;
            }
        }

        // SAFETY: the host passes `num` entries.
        let reqs = unsafe { core::slice::from_raw_parts_mut(infos, num as usize) };
        for r in reqs.iter() {
            let (is_in, ch) = ({ r.is_input } != 0, { r.channel_num });
            let limit = if is_in { NUM_INPUTS } else { NUM_OUTPUTS };
            if ch < 0 || ch >= limit {
                return ASE_INVALID_PARAMETER;
            }
        }

        // The mapping may have been unavailable at init (e.g. a transient
        // failure): retry now, never abort.
        if self.mapping.is_none() {
            self.attach_bridge();
        }

        let frames = size as usize;
        let mut bufs: Vec<Box<[i32]>> = Vec::with_capacity(reqs.len());
        let mut out_pair = [core::ptr::null_mut(); 2];
        let mut in_pair = [core::ptr::null_mut(); 2];
        let mut zero_ins: Vec<*mut i32> = Vec::new();
        for r in reqs.iter_mut() {
            let (is_in, ch) = ({ r.is_input } != 0, { r.channel_num } as usize);
            let mut b = vec![0i32; frames * 2].into_boxed_slice();
            let p = b.as_mut_ptr();
            // SAFETY: p..p+2*frames is the allocation.
            r.buffers = [p as *mut c_void, unsafe { p.add(frames) } as *mut c_void];
            if is_in {
                self.active_in[ch] = true;
                if ch < 2 {
                    in_pair[ch] = p;
                } else {
                    zero_ins.push(p);
                }
            } else {
                self.active_out[ch] = true;
                if ch < 2 {
                    out_pair[ch] = p;
                }
            }
            bufs.push(b);
        }

        // Does the host want time info? Prefer `bufferSwitchTimeInfo` only
        // when it registered that callback; otherwise fall back to
        // `bufferSwitch` (plan §Component 2).
        let use_time_info = (host.buffer_switch_time_info as usize != 0)
            && (host.asio_message as usize != 0)
            && unsafe {
                (host.asio_message)(K_ASIO_SELECTOR_SUPPORTED, K_ASIO_SUPPORTS_TIME_INFO, core::ptr::null_mut(), core::ptr::null_mut()) == 1
                    && (host.asio_message)(K_ASIO_SUPPORTS_TIME_INFO, 0, core::ptr::null_mut(), core::ptr::null_mut()) == 1
            };

        self.negotiated = size;
        if !self.header.is_null() {
            // SAFETY: mapping alive (owned by self.mapping).
            unsafe { (*self.header).set_buffer_frames(size as u32) };
        }

        let rt = Box::new(Rt {
            frames,
            host,
            use_time_info,
            header: self.header,
            rate_bits: AtomicU64::new(self.rate.to_bits()),
            pending_rate: AtomicU64::new(0),
            running: AtomicBool::new(false),
            pos: AtomicU64::new(0),
            ts_ns: AtomicU64::new(0),
            start_ns: 0,
            frames_done: 0,
            index: 0,
            out_pair,
            in_pair,
            zero_ins,
            scratch_l: vec![0.0; frames],
            scratch_r: vec![0.0; frames],
        });
        self.rt = Box::into_raw(rt);
        rt_slot.store(self.rt, Ordering::Release);
        self.our_bufs = bufs;
        self.buffers_created = true;
        dlog!(
            "createBuffers ok: {num} requested, size {size}, time info {use_time_info}, bridge {}",
            if self.header.is_null() { "absent" } else { "attached" }
        );
        ASE_OK
    }

    fn start(&mut self) -> AsioError {
        if !self.initialized || !self.buffers_created {
            return ASE_NOT_PRESENT;
        }
        if self.started {
            return ASE_OK;
        }
        // A previously detached thread must be gone before a new one starts.
        if let Some(p) = self.pacing.as_mut() {
            if !p.finish(1000) {
                self.error = "previous pacing thread still running".into();
                dlog!("{}", self.error);
                return ASE_HW_MALFUNCTION;
            }
            self.pacing = None;
        }
        if self.rt.is_null() {
            return ASE_NOT_PRESENT;
        }
        // SAFETY: buffers exist and no pacing thread runs right now.
        let rt = unsafe { &mut *self.rt };
        rt.frames_done = 0;
        rt.index = 0;
        rt.pos.store(0, Ordering::Release);
        rt.ts_ns.store(0, Ordering::Release);
        rt.start_ns = pacing::qpc_now_ns(pacing::qpc_freq());
        rt.rate_bits.store(self.rate.to_bits(), Ordering::Release);
        rt.pending_rate.store(0, Ordering::Relaxed);
        let frames = rt.frames as u32;
        let rate = self.rate;
        rt.running.store(true, Ordering::Release);
        if !self.header.is_null() {
            // SAFETY: mapping alive.
            unsafe { (*self.header).driver_running.store(1, Ordering::Release) };
        }
        let raw = RtPtr(self.rt);
        let spawned = pacing::PacingThread::spawn(frames, rate, move || unsafe { raw.tick() });
        match spawned {
            Some(p) => {
                self.pacing = Some(p);
                self.started = true;
                dlog!("started: {frames} frames @ {rate} Hz");
                ASE_OK
            }
            None => {
                unsafe { (*self.rt).running.store(false, Ordering::Release) };
                if !self.header.is_null() {
                    unsafe { (*self.header).driver_running.store(0, Ordering::Release) };
                }
                self.error = "failed to spawn pacing thread".into();
                dlog!("{}", self.error);
                ASE_HW_MALFUNCTION
            }
        }
    }

    fn stop(&mut self) -> AsioError {
        if !self.started {
            return ASE_OK;
        }
        self.started = false;
        if !self.rt.is_null() {
            // SAFETY: rt alive while buffers exist.
            unsafe { (*self.rt).running.store(false, Ordering::Release) };
        }
        if !self.header.is_null() {
            // SAFETY: mapping alive.
            unsafe { (*self.header).driver_running.store(0, Ordering::Release) };
        }
        if let Some(p) = self.pacing.as_mut() {
            // v1 ResetThread pattern: join unless we are *inside* the host's
            // callback (then detach; `dispose` waits for `exited` first).
            let outcome = p.shutdown();
            dlog!(
                "stopped (pacing {})",
                match outcome {
                    StopOutcome::Joined => "joined",
                    StopOutcome::Detached => "detached — will be reaped at dispose/start",
                }
            );
        }
        ASE_OK
    }

    /// Stop, make sure the pacing thread is gone, then free the runtime and
    /// our buffers — in that order (a callback in flight completes first).
    fn dispose(&mut self) -> AsioError {
        if !self.buffers_created {
            return ASE_OK;
        }
        self.stop();
        let exited = match self.pacing.as_mut() {
            Some(p) => p.finish(2000),
            None => true,
        };
        self.pacing = None;
        if !exited {
            // Pathological (host callback wedged > 2 s): rather than free
            // memory a stray tick could touch, leak it and log loudly.
            dlog!("dispose: pacing thread did not exit; leaking runtime state");
            self.rt = core::ptr::null_mut();
            std::mem::forget(std::mem::take(&mut self.our_bufs));
            self.buffers_created = false;
            self.active_in = [false; 8];
            self.active_out = [false; 8];
            return ASE_OK;
        }
        if !self.rt.is_null() {
            // SAFETY: allocated by Box::into_raw in create_buffers; the pacing
            // thread has exited (checked above).
            drop(unsafe { Box::from_raw(self.rt) });
            self.rt = core::ptr::null_mut();
        }
        self.our_bufs.clear();
        self.active_in = [false; 8];
        self.active_out = [false; 8];
        self.buffers_created = false;
        dlog!("buffers disposed");
        ASE_OK
    }

    fn set_sample_rate(&mut self, rate: f64) -> AsioError {
        if !rate_supported(rate) {
            return ASE_NO_CLOCK;
        }
        if (rate - self.rate).abs() < 0.5 {
            return ASE_OK;
        }
        self.rate = rate;
        if !self.header.is_null() {
            // SAFETY: mapping alive; single writer for this field.
            unsafe { (*self.header).set_sample_rate(rate) };
        }
        if !self.rt.is_null() {
            // SAFETY: rt alive while buffers exist; atomics only.
            unsafe {
                (*self.rt).rate_bits.store(rate.to_bits(), Ordering::Release);
                if self.started {
                    (*self.rt).pending_rate.store(rate.to_bits(), Ordering::Release);
                }
            }
        }
        dlog!("setSampleRate({rate})");
        ASE_OK
    }
}

#[repr(C)]
pub struct Driver {
    vtbl: *const IAsioVtbl,
    refs: AtomicU32,
    /// Mirror of `inner.rt` for `getSamplePosition` (atomics only).
    rt_ptr: AtomicPtr<Rt>,
    inner: UnsafeCell<Inner>,
}

static VTBL: IAsioVtbl = IAsioVtbl {
    query_interface: m_query_interface,
    add_ref: m_add_ref,
    release: m_release,
    init: m_init,
    get_driver_name: m_get_driver_name,
    get_driver_version: m_get_driver_version,
    get_error_message: m_get_error_message,
    start: m_start,
    stop: m_stop,
    get_channels: m_get_channels,
    get_latencies: m_get_latencies,
    get_buffer_size: m_get_buffer_size,
    can_sample_rate: m_can_sample_rate,
    get_sample_rate: m_get_sample_rate,
    set_sample_rate: m_set_sample_rate,
    get_clock_sources: m_get_clock_sources,
    set_clock_source: m_set_clock_source,
    get_sample_position: m_get_sample_position,
    get_channel_info: m_get_channel_info,
    create_buffers: m_create_buffers,
    dispose_buffers: m_dispose_buffers,
    control_panel: m_control_panel,
    future: m_future,
    output_ready: m_output_ready,
};

pub fn create_object() -> *mut IAsio {
    let d = Box::new(Driver {
        vtbl: &VTBL,
        refs: AtomicU32::new(1),
        rt_ptr: AtomicPtr::new(core::ptr::null_mut()),
        inner: UnsafeCell::new(Inner::new()),
    });
    OBJECT_COUNT.fetch_add(1, Ordering::AcqRel);
    Box::into_raw(d) as *mut IAsio
}

/// # Safety
/// `this` must be a live driver object; pointers per COM contract.
pub unsafe fn query_interface(this: *mut IAsio, riid: *const Guid, ppv: *mut *mut c_void) -> i32 {
    m_query_interface(this, riid, ppv)
}

/// # Safety
/// `this` must be a live driver object.
pub unsafe fn release(this: *mut IAsio) -> u32 {
    m_release(this)
}

unsafe fn drv<'a>(this: *mut IAsio) -> &'a Driver {
    &*(this as *const Driver)
}

#[allow(clippy::mut_from_ref)]
unsafe fn me<'a>(this: *mut IAsio) -> &'a mut Inner {
    &mut *drv(this).inner.get()
}

// ---------------------------------------------------------------- IUnknown

unsafe extern "system" fn m_query_interface(this: *mut IAsio, riid: *const Guid, ppv: *mut *mut c_void) -> i32 {
    guard(E_UNEXPECTED, || {
        if ppv.is_null() || riid.is_null() {
            return E_POINTER;
        }
        let iid = *riid;
        // ASIO quirk: hosts ask for IASIO using the driver's own CLSID as IID.
        if iid == IID_IUNKNOWN || iid == CLSID_EQ_ASIO {
            m_add_ref(this);
            *ppv = this as *mut c_void;
            S_OK
        } else {
            *ppv = core::ptr::null_mut();
            E_NOINTERFACE
        }
    })
}

unsafe extern "system" fn m_add_ref(this: *mut IAsio) -> u32 {
    drv(this).refs.fetch_add(1, Ordering::AcqRel) + 1
}

unsafe extern "system" fn m_release(this: *mut IAsio) -> u32 {
    let n = drv(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if n == 0 {
        guard((), || {
            dlog!("driver object released, tearing down");
            // Drop runs teardown: dispose buffers (joins the pacing thread),
            // free the runtime, unmap the bridge — all before the object dies.
            drop(Box::from_raw(this as *mut Driver));
        });
        OBJECT_COUNT.fetch_sub(1, Ordering::AcqRel);
    }
    n
}

impl Drop for Driver {
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        inner.dispose();
        inner.mapping = None;
    }
}

// ---------------------------------------------------------------- helpers

fn copy_cstr(dst: *mut c_char, s: &str, cap: usize) {
    if dst.is_null() || cap == 0 {
        return;
    }
    let b = s.as_bytes();
    let n = b.len().min(cap - 1);
    // SAFETY: caller guarantees `cap` writable bytes.
    unsafe {
        for (i, &c) in b[..n].iter().enumerate() {
            *dst.add(i) = c as c_char;
        }
        *dst.add(n) = 0;
    }
}

fn name_array(s: &str) -> [c_char; 32] {
    let mut a = [0 as c_char; 32];
    for (i, &c) in s.as_bytes().iter().take(31).enumerate() {
        a[i] = c as c_char;
    }
    a
}

// ---------------------------------------------------------------- IASIO methods

asio_method!(fn m_init(this: *mut IAsio, sys: *mut c_void) -> AsioBool = ASIO_FALSE, {
    me(this).init(sys)
});

asio_method!(fn m_get_driver_name(_this: *mut IAsio, name: *mut c_char) -> () = (), {
    copy_cstr(name, DRIVER_NAME, 32);
});

asio_method!(fn m_get_driver_version(_this: *mut IAsio) -> i32 = 0, { DRIVER_VERSION });

asio_method!(fn m_get_error_message(this: *mut IAsio, msg: *mut c_char) -> () = (), {
    let e = &me(this).error;
    copy_cstr(msg, if e.is_empty() { "No error" } else { e }, 124);
});

asio_method!(fn m_start(this: *mut IAsio) -> AsioError = ASE_HW_MALFUNCTION, { me(this).start() });

asio_method!(fn m_stop(this: *mut IAsio) -> AsioError = ASE_HW_MALFUNCTION, { me(this).stop() });

asio_method!(fn m_get_channels(_this: *mut IAsio, n_in: *mut i32, n_out: *mut i32) -> AsioError = ASE_HW_MALFUNCTION, {
    // Fixed topology — answerable at any time (v2 has no master to probe).
    if n_in.is_null() || n_out.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    *n_in = NUM_INPUTS;
    *n_out = NUM_OUTPUTS;
    ASE_OK
});

asio_method!(fn m_get_latencies(this: *mut IAsio, l_in: *mut i32, l_out: *mut i32) -> AsioError = ASE_HW_MALFUNCTION, {
    if l_in.is_null() || l_out.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    // One buffer of shuttle latency in each direction.
    let frames = me(this).negotiated;
    *l_in = frames;
    *l_out = frames;
    ASE_OK
});

asio_method!(fn m_get_buffer_size(_this: *mut IAsio, mn: *mut i32, mx: *mut i32, pf: *mut i32, gr: *mut i32) -> AsioError = ASE_HW_MALFUNCTION, {
    if mn.is_null() || mx.is_null() || pf.is_null() || gr.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    *mn = BUFFER_MIN;
    *mx = BUFFER_MAX;
    *pf = BUFFER_PREFERRED;
    *gr = BUFFER_GRANULARITY;
    ASE_OK
});

asio_method!(fn m_can_sample_rate(_this: *mut IAsio, rate: AsioSampleRate) -> AsioError = ASE_HW_MALFUNCTION, {
    if rate_supported(rate) { ASE_OK } else { ASE_NO_CLOCK }
});

asio_method!(fn m_get_sample_rate(this: *mut IAsio, rate: *mut AsioSampleRate) -> AsioError = ASE_HW_MALFUNCTION, {
    if rate.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    *rate = me(this).rate;
    ASE_OK
});

asio_method!(fn m_set_sample_rate(this: *mut IAsio, rate: AsioSampleRate) -> AsioError = ASE_HW_MALFUNCTION, {
    me(this).set_sample_rate(rate)
});

asio_method!(fn m_get_clock_sources(_this: *mut IAsio, clocks: *mut AsioClockSource, n: *mut i32) -> AsioError = ASE_HW_MALFUNCTION, {
    if clocks.is_null() || n.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    if *n < 1 {
        return ASE_INVALID_PARAMETER;
    }
    (*clocks).index = 0;
    (*clocks).associated_channel = -1;
    (*clocks).associated_group = -1;
    (*clocks).is_current_source = ASIO_TRUE;
    (*clocks).name = name_array("Internal");
    *n = 1;
    ASE_OK
});

asio_method!(fn m_set_clock_source(_this: *mut IAsio, reference: i32) -> AsioError = ASE_HW_MALFUNCTION, {
    if reference == 0 { ASE_OK } else { ASE_INVALID_PARAMETER }
});

asio_method!(fn m_get_sample_position(this: *mut IAsio, pos: *mut AsioSamples, ts: *mut AsioTimeStamp) -> AsioError = ASE_HW_MALFUNCTION, {
    if pos.is_null() || ts.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    let rt = drv(this).rt_ptr.load(Ordering::Acquire);
    // Not created, or created but not streaming: position is not advancing.
    if rt.is_null() || !unsafe { (*rt).running.load(Ordering::Acquire) } {
        return ASE_SP_NOT_ADVANCING;
    }
    *pos = AsioInt64::from_u64(unsafe { (*rt).pos.load(Ordering::Acquire) });
    *ts = AsioInt64::from_u64(unsafe { (*rt).ts_ns.load(Ordering::Acquire) });
    ASE_OK
});

asio_method!(fn m_get_channel_info(this: *mut IAsio, info: *mut AsioChannelInfo) -> AsioError = ASE_HW_MALFUNCTION, {
    let s = me(this);
    if info.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    let (ch, is_in) = ({ (*info).channel }, { (*info).is_input } != 0);
    if ch < 0 {
        return ASE_INVALID_PARAMETER;
    }
    if is_in {
        if ch >= NUM_INPUTS {
            return ASE_INVALID_PARAMETER;
        }
        (*info).is_active = s.active_in[ch as usize] as AsioBool;
        (*info).name = name_array(&format!("EQ In {}", ch + 1));
    } else {
        if ch >= NUM_OUTPUTS {
            return ASE_INVALID_PARAMETER;
        }
        (*info).is_active = s.active_out[ch as usize] as AsioBool;
        (*info).name = name_array(&format!("EQ Out {}", ch + 1));
    }
    (*info).channel_group = 0;
    (*info).sample_type = ASIOST_INT32_LSB;
    ASE_OK
});

asio_method!(fn m_create_buffers(this: *mut IAsio, infos: *mut AsioBufferInfo, num: i32, size: i32, cb: *const AsioCallbacks) -> AsioError = ASE_HW_MALFUNCTION, {
    me(this).create_buffers(&drv(this).rt_ptr, infos, num, size, cb)
});

asio_method!(fn m_dispose_buffers(this: *mut IAsio) -> AsioError = ASE_HW_MALFUNCTION, {
    let s = me(this);
    if !s.buffers_created {
        return ASE_INVALID_MODE;
    }
    // Hide the runtime from getSamplePosition before it is freed.
    drv(this).rt_ptr.store(core::ptr::null_mut(), Ordering::Release);
    s.dispose()
});

asio_method!(fn m_control_panel(_this: *mut IAsio) -> AsioError = ASE_OK, {
    // Virtual bridge: nothing to configure here (the app GUI is the panel).
    ASE_OK
});

asio_method!(fn m_future(_this: *mut IAsio, selector: i32, _opt: *mut c_void) -> AsioError = ASE_NOT_PRESENT, {
    match selector {
        K_ASIO_CAN_TIME_INFO => ASE_SUCCESS,
        _ => ASE_NOT_PRESENT,
    }
});

asio_method!(fn m_output_ready(_this: *mut IAsio) -> AsioError = ASE_OK, { ASE_OK });

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_table_matches_the_plan() {
        for r in [44_100.0, 48_000.0, 88_200.0, 96_000.0, 176_400.0, 192_000.0] {
            assert!(rate_supported(r), "{r}");
        }
        for r in [22_050.0, 32_000.0, 12_000.0, 384_000.0, f64::NAN] {
            assert!(!rate_supported(r), "{r}");
        }
        // within 0.5 Hz counts as the same rate
        assert!(rate_supported(48_000.25));
        assert!(!rate_supported(48_001.0));
    }

    #[test]
    fn buffer_sizes_64_to_2048_granularity_64() {
        assert!(buffer_size_valid(64));
        assert!(buffer_size_valid(256));
        assert!(buffer_size_valid(2048));
        assert!(buffer_size_valid(1280));
        assert!(!buffer_size_valid(0));
        assert!(!buffer_size_valid(32));
        assert!(!buffer_size_valid(2049));
        assert!(!buffer_size_valid(100));
        assert!(!buffer_size_valid(96)); // not on the 64-grid
        assert_eq!(BUFFER_PREFERRED, 256);
    }
}
