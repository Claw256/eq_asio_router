//! ALSA playback backend for Linux: [`AlsaSink`] implements [`OutputSink`].
//!
//! All bindings are hand-written `#[link]` extern FFI over `libasound` + libc
//! (docs/plans/cross-platform.md A4/A8: no `-sys` crates — their `build.rs` probes
//! pkg-config/system SDKs and would break the A7 cross-`cargo check` gate).
//!
//! Policy (A6 left the period policy OPEN; these are the documented defaults):
//! * **Device**: requested name -> `"default"` -> `"plughw:0,0"` (see
//!   [`device_candidates`]); all-fail maps to [`SinkError::Unavailable`].
//! * **Format**: `FLOAT_LE` if the device accepts it, else `S32_LE` (the required
//!   minimum); access `RW_INTERLEAVED`; the render callback stays per-channel
//!   planar `f32` and conversion happens at the write boundary (see
//!   [`planar_to_interleaved_f32`] / [`planar_to_interleaved_i32`]).
//! * **Period**: as close as ALSA grants to the requested `buffer_frames`
//!   (default [`DEFAULT_PERIOD_FRAMES`] = 256, clamped to
//!   [`MIN_PERIOD_FRAMES`]..=[`MAX_PERIOD_FRAMES`]), buffered over
//!   [`PERIOD_COUNT`] = **3 periods** (~8 ms at 48 kHz with a 256-frame period).
//!   3 rather than 2 costs one extra period of latency (~2.7 ms at 48 kHz) and in
//!   exchange gives the DSP thread a full period of slack before an xrun becomes
//!   possible. The shape ALSA actually granted is returned in [`StreamSpec`] so
//!   ring sizing stays exact.
//! * **RT loop**: one render callback per period, event-driven — `poll(2)` on the
//!   PCM poll descriptors, then `snd_pcm_writei` of one period. Xruns
//!   (`-EPIPE`/`-ESTRPIPE`) are recovered with `snd_pcm_recover`, counted
//!   ([`AlsaSink::xrun_count`]) and reported through
//!   [`OutputCallback::on_xrun`]; the [`XrunState`] machine tracks
//!   running/recovering/dead.
//! * **Ticks**: `clock_gettime(CLOCK_MONOTONIC)` in nanoseconds
//!   ([`TICKS_PER_SECOND`]), the Linux embodiment of the [`crate::seams::Clock`]
//!   tick domain.
//! * **Thread priority**: best-effort `sched_setscheduler(SCHED_FIFO, prio 50)`
//!   on the render thread; silently falls back to normal scheduling when the
//!   policy is not permitted (`EPERM`) — never a start failure.

use std::ffi::{c_char, c_int, c_uint, c_void, CString};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::seams::{OutputCallback, OutputSink, SinkError, StreamRequest, StreamSpec};

// ------------------------------------------------------------------------------------
// Pure policy helpers (hardware-free; unit-tested in `tests/linux_negotiation.rs`)
// ------------------------------------------------------------------------------------

/// Default requested period size in frames when the request carries none.
pub const DEFAULT_PERIOD_FRAMES: u32 = 256;
/// Smallest period the policy will negotiate down to.
pub const MIN_PERIOD_FRAMES: u32 = 32;
/// Largest period the policy will negotiate up to.
pub const MAX_PERIOD_FRAMES: u32 = 4096;
/// Periods per buffer (A6 policy choice: 3; see module doc).
pub const PERIOD_COUNT: u32 = 3;
/// Tick domain of [`monotonic_ns`] (nanoseconds).
pub const TICKS_PER_SECOND: i64 = 1_000_000_000;

/// Negotiated hardware sample format (interleaved).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    /// `SND_PCM_FORMAT_FLOAT_LE` (preferred).
    FloatLe,
    /// `SND_PCM_FORMAT_S32_LE` (required minimum).
    S32Le,
}

impl SampleFormat {
    /// ALSA `snd_pcm_format_t` discriminant.
    pub fn alsa_code(self) -> c_int {
        match self {
            SampleFormat::FloatLe => 14,
            SampleFormat::S32Le => 10,
        }
    }

    /// Bytes per sample in the interleaved write buffer.
    pub fn sample_bytes(self) -> usize {
        4
    }
}

/// Format preference: `FLOAT_LE` when available, else `S32_LE`.
pub fn pick_format(supports_float: bool) -> SampleFormat {
    if supports_float {
        SampleFormat::FloatLe
    } else {
        SampleFormat::S32Le
    }
}

/// Period policy: clamp the request (0 -> [`DEFAULT_PERIOD_FRAMES`]) and return
/// `(period_frames, periods)` with the fixed [`PERIOD_COUNT`].
pub fn negotiate_period(requested: u32) -> (u32, u32) {
    let frames = if requested == 0 {
        DEFAULT_PERIOD_FRAMES
    } else {
        requested.clamp(MIN_PERIOD_FRAMES, MAX_PERIOD_FRAMES)
    };
    (frames, PERIOD_COUNT)
}

/// Buffer size in frames for `period_frames * periods` (saturating).
pub fn plan_buffer(period_frames: u32, periods: u32) -> u32 {
    period_frames.saturating_mul(periods)
}

/// Device fallback chain: requested (if non-empty) -> `"default"` -> `"plughw:0,0"`,
/// deduplicated, order preserved.
pub fn device_candidates(configured: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(3);
    if !configured.is_empty() {
        out.push(configured.to_string());
    }
    for name in ["default", "plughw:0,0"] {
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// Xrun recovery state machine: `Running -> Recovering -> Running | Dead`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XrunState {
    /// Stream is delivering normally.
    Running,
    /// An xrun happened; `snd_pcm_recover` is in progress.
    Recovering,
    /// Recovery failed; the RT loop must stop.
    Dead,
}

impl XrunState {
    /// An xrun was observed.
    pub fn on_xrun(self) -> Self {
        XrunState::Recovering
    }

    /// `snd_pcm_recover` succeeded.
    pub fn on_recovered(self) -> Self {
        match self {
            XrunState::Dead => XrunState::Dead,
            _ => XrunState::Running,
        }
    }

    /// Recovery failed (terminal).
    pub fn on_fatal(self) -> Self {
        XrunState::Dead
    }

    /// Whether the state machine reached its terminal state.
    pub fn is_dead(self) -> bool {
        self == XrunState::Dead
    }
}

/// Convert one planar `f32` sample to `S32_LE` (clamped; -1.0 -> -2147483647,
/// +1.0 -> 2147483647). The scaling is done in `f64` so the multiplier is exact.
pub fn f32_to_s32(x: f32) -> i32 {
    let s = if x.is_finite() { x.clamp(-1.0, 1.0) } else { 0.0 };
    (f64::from(s) * 2_147_483_647.0) as i32
}

/// Interleave planar `f32` channels into an `S32_LE` buffer
/// (`dst[frame * channels + channel]`).
pub fn planar_to_interleaved_i32(chans: &[&[f32]], dst: &mut [i32]) {
    let nch = chans.len();
    if nch == 0 {
        return;
    }
    let frames = dst.len() / nch;
    for f in 0..frames {
        for (c, row) in chans.iter().enumerate() {
            let s = row.get(f).copied().unwrap_or(0.0);
            dst[f * nch + c] = f32_to_s32(s);
        }
    }
}

/// Interleave planar `f32` channels into a `FLOAT_LE` buffer
/// (`dst[frame * channels + channel]`).
pub fn planar_to_interleaved_f32(chans: &[&[f32]], dst: &mut [f32]) {
    let nch = chans.len();
    if nch == 0 {
        return;
    }
    let frames = dst.len() / nch;
    for f in 0..frames {
        for (c, row) in chans.iter().enumerate() {
            let s = row.get(f).copied().unwrap_or(0.0);
            dst[f * nch + c] = s;
        }
    }
}

/// `clock_gettime(CLOCK_MONOTONIC)` in nanoseconds (tick domain of the render path).
pub fn monotonic_ns() -> i64 {
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    let rc = unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    ts.tv_sec.wrapping_mul(1_000_000_000).wrapping_add(ts.tv_nsec)
}

// ------------------------------------------------------------------------------------
// Hand-written FFI (A8: no -sys crates; cargo check does not link, so no SDK needed)
// ------------------------------------------------------------------------------------

/// Opaque `snd_pcm_t`.
pub type SndPcmHandle = c_void;
/// Opaque `snd_pcm_hw_params_t` (real size queried via `snd_pcm_hw_params_sizeof`).
pub type SndPcmHwParams = c_void;
/// Opaque `snd_pcm_sw_params_t` (real size queried via `snd_pcm_sw_params_sizeof`).
pub type SndPcmSwParams = c_void;

/// `struct pollfd` (Linux x86_64 layout).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PollFd {
    /// File descriptor.
    pub fd: c_int,
    /// Requested events (bitmask).
    pub events: i16,
    /// Returned events (bitmask).
    pub revents: i16,
}

/// `struct timespec` (Linux x86_64: `time_t`/`long` are 64-bit).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Timespec {
    /// Seconds.
    pub tv_sec: i64,
    /// Nanoseconds.
    pub tv_nsec: i64,
}

/// `struct sched_param`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SchedParam {
    /// RT priority.
    pub sched_priority: c_int,
}

/// `snd_pcm_stream_t` playback direction.
pub const SND_PCM_STREAM_PLAYBACK: c_int = 0;
/// `snd_pcm_access_t` RW interleaved.
pub const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;
/// `EPIPE` (xrun).
pub const EPIPE: c_int = 32;
/// `ESTRPIPE` (stream suspended).
pub const ESTRPIPE: c_int = 86;
/// `EAGAIN`.
pub const EAGAIN: c_int = 11;
/// `POLLIN`.
pub const POLLIN: i16 = 0x001;
/// `POLLOUT`.
pub const POLLOUT: i16 = 0x004;
/// `POLLERR`.
pub const POLLERR: i16 = 0x008;
/// `POLLHUP`.
pub const POLLHUP: i16 = 0x010;
/// `POLLNVAL`.
pub const POLLNVAL: i16 = 0x020;
/// `CLOCK_MONOTONIC`.
pub const CLOCK_MONOTONIC: c_int = 1;
/// `SCHED_FIFO`.
pub const SCHED_FIFO: c_int = 1;
/// Poll timeout guarding the stop flag (ms).
pub const POLL_TIMEOUT_MS: c_int = 100;
/// Best-effort RT priority for the render thread.
pub const RT_PRIORITY: c_int = 50;

#[link(name = "asound")]
extern "C" {
    /// `snd_pcm_open`.
    pub fn snd_pcm_open(pcm: *mut *mut SndPcmHandle, name: *const c_char, stream: c_int, mode: c_int) -> c_int;
    /// `snd_pcm_close`.
    pub fn snd_pcm_close(pcm: *mut SndPcmHandle) -> c_int;
    /// `snd_pcm_prepare`.
    pub fn snd_pcm_prepare(pcm: *mut SndPcmHandle) -> c_int;
    /// `snd_pcm_drop`.
    pub fn snd_pcm_drop(pcm: *mut SndPcmHandle) -> c_int;
    /// `snd_pcm_writei` (frames).
    pub fn snd_pcm_writei(pcm: *mut SndPcmHandle, buffer: *const c_void, size: usize) -> isize;
    /// `snd_pcm_recover`.
    pub fn snd_pcm_recover(pcm: *mut SndPcmHandle, err: c_int, silent: c_int) -> c_int;
    /// `snd_pcm_poll_descriptors`.
    pub fn snd_pcm_poll_descriptors(pcm: *mut SndPcmHandle, pfds: *mut PollFd, space: c_uint) -> c_int;
    /// `snd_pcm_poll_descriptors_count`.
    pub fn snd_pcm_poll_descriptors_count(pcm: *mut SndPcmHandle) -> c_int;
    /// `snd_pcm_hw_params_sizeof`.
    pub fn snd_pcm_hw_params_sizeof() -> usize;
    /// `snd_pcm_sw_params_sizeof`.
    pub fn snd_pcm_sw_params_sizeof() -> usize;
    /// `snd_pcm_hw_params_any`.
    pub fn snd_pcm_hw_params_any(pcm: *mut SndPcmHandle, params: *mut SndPcmHwParams) -> c_int;
    /// `snd_pcm_hw_params`.
    pub fn snd_pcm_hw_params(pcm: *mut SndPcmHandle, params: *mut SndPcmHwParams) -> c_int;
    /// `snd_pcm_hw_params_set_access`.
    pub fn snd_pcm_hw_params_set_access(pcm: *mut SndPcmHandle, params: *mut SndPcmHwParams, access: c_int) -> c_int;
    /// `snd_pcm_hw_params_set_format`.
    pub fn snd_pcm_hw_params_set_format(pcm: *mut SndPcmHandle, params: *mut SndPcmHwParams, format: c_int) -> c_int;
    /// `snd_pcm_hw_params_set_channels`.
    pub fn snd_pcm_hw_params_set_channels(pcm: *mut SndPcmHandle, params: *mut SndPcmHwParams, channels: c_uint) -> c_int;
    /// `snd_pcm_hw_params_set_rate_near`.
    pub fn snd_pcm_hw_params_set_rate_near(
        pcm: *mut SndPcmHandle,
        params: *mut SndPcmHwParams,
        rate: *mut c_uint,
        dir: *mut c_int,
    ) -> c_int;
    /// `snd_pcm_hw_params_set_period_size_near`.
    pub fn snd_pcm_hw_params_set_period_size_near(
        pcm: *mut SndPcmHandle,
        params: *mut SndPcmHwParams,
        period: *mut usize,
        dir: *mut c_int,
    ) -> c_int;
    /// `snd_pcm_hw_params_set_periods_near`.
    pub fn snd_pcm_hw_params_set_periods_near(
        pcm: *mut SndPcmHandle,
        params: *mut SndPcmHwParams,
        periods: *mut c_uint,
        dir: *mut c_int,
    ) -> c_int;
    /// `snd_pcm_hw_params_get_rate`.
    pub fn snd_pcm_hw_params_get_rate(params: *const SndPcmHwParams, rate: *mut c_uint, dir: *mut c_int) -> c_int;
    /// `snd_pcm_hw_params_get_period_size`.
    pub fn snd_pcm_hw_params_get_period_size(params: *const SndPcmHwParams, period: *mut usize, dir: *mut c_int) -> c_int;
    /// `snd_pcm_hw_params_get_periods`.
    pub fn snd_pcm_hw_params_get_periods(params: *const SndPcmHwParams, periods: *mut c_uint, dir: *mut c_int) -> c_int;
    /// `snd_pcm_hw_params_get_channels`.
    pub fn snd_pcm_hw_params_get_channels(params: *const SndPcmHwParams, channels: *mut c_uint) -> c_int;
    /// `snd_pcm_sw_params_current`.
    pub fn snd_pcm_sw_params_current(pcm: *mut SndPcmHandle, params: *mut SndPcmSwParams) -> c_int;
    /// `snd_pcm_sw_params_set_start_threshold`.
    pub fn snd_pcm_sw_params_set_start_threshold(pcm: *mut SndPcmHandle, params: *mut SndPcmSwParams, val: usize) -> c_int;
    /// `snd_pcm_sw_params_set_avail_min`.
    pub fn snd_pcm_sw_params_set_avail_min(pcm: *mut SndPcmHandle, params: *mut SndPcmSwParams, val: usize) -> c_int;
    /// `snd_pcm_sw_params`.
    pub fn snd_pcm_sw_params(pcm: *mut SndPcmHandle, params: *mut SndPcmSwParams) -> c_int;
}

#[link(name = "c")]
extern "C" {
    /// `poll(2)`.
    pub fn poll(fds: *mut PollFd, nfds: usize, timeout: c_int) -> c_int;
    /// `clock_gettime(2)`.
    pub fn clock_gettime(clock_id: c_int, tp: *mut Timespec) -> c_int;
    /// `sched_setscheduler(2)`.
    pub fn sched_setscheduler(pid: c_int, policy: c_int, param: *const SchedParam) -> c_int;
}

/// Aligned byte storage for the opaque ALSA `hw_params`/`sw_params` structs: their
/// concrete shape is not part of the ABI, so we allocate `snd_pcm_*_sizeof()` bytes
/// of 64-aligned storage instead of guessing a Rust mirror.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct AlignedBuf<const N: usize>(pub [u8; N]);

/// Storage capacity for `snd_pcm_hw_params_t` (alsa-lib uses ~600 bytes).
pub const HW_PARAMS_BYTES: usize = 4096;
/// Storage capacity for `snd_pcm_sw_params_t`.
pub const SW_PARAMS_BYTES: usize = 1024;

// ------------------------------------------------------------------------------------
// OutputSink implementation
// ------------------------------------------------------------------------------------

/// Delegate wrapping any [`OutputCallback`] into one concrete associated type for
/// [`AlsaSink`] (heap allocation happens at [`OutputSink::start`], never per period).
pub struct AlsaCallback(Box<dyn OutputCallback>);

impl AlsaCallback {
    /// Wrap a callback.
    pub fn new(inner: impl OutputCallback) -> Self {
        AlsaCallback(Box::new(inner))
    }

    /// Wrap an already-boxed callback.
    pub fn from_boxed(inner: Box<dyn OutputCallback>) -> Self {
        AlsaCallback(inner)
    }
}

impl OutputCallback for AlsaCallback {
    fn render(&mut self, out: &mut [&mut [f32]], now_ticks: i64) {
        self.0.render(out, now_ticks)
    }

    fn on_xrun(&mut self) {
        self.0.on_xrun()
    }
}

/// Raw PCM handle moved onto the render thread (single-threaded access, see
/// `AlsaSink::stop` — the handle is closed only after the thread is joined).
struct PcmThreadHandle(*mut SndPcmHandle);

// SAFETY: the handle is only dereferenced on the render thread; `AlsaSink::stop`
// joins that thread before closing the handle, so there is a single live user.
unsafe impl Send for PcmThreadHandle {}

/// ALSA playback [`OutputSink`] (see module doc for the negotiated policies).
pub struct AlsaSink {
    name: String,
    spec: StreamSpec,
    format: SampleFormat,
    handle: *mut SndPcmHandle,
    device_opened: String,
    period_frames: u32,
    periods: u32,
    xruns: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

// SAFETY: the PCM handle is used either from the render thread (while running) or
// from `&mut self` methods (while stopped); `stop()` joins the thread before the
// handle is touched, and all shared state is atomic.
unsafe impl Send for AlsaSink {}

impl AlsaSink {
    /// A sink that will try `device` first, then `"default"`, then `"plughw:0,0"`
    /// (see [`device_candidates`]). No hardware is touched until
    /// [`OutputSink::negotiate`].
    pub fn new(device: impl Into<String>) -> Self {
        AlsaSink {
            name: device.into(),
            spec: StreamSpec::default(),
            format: SampleFormat::S32Le,
            handle: std::ptr::null_mut(),
            device_opened: String::new(),
            period_frames: DEFAULT_PERIOD_FRAMES,
            periods: PERIOD_COUNT,
            xruns: Arc::new(AtomicU64::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    /// Device name the stream was actually opened on (empty before negotiation).
    pub fn device_opened(&self) -> &str {
        &self.device_opened
    }

    /// Format negotiated by [`OutputSink::negotiate`].
    pub fn format(&self) -> SampleFormat {
        self.format
    }

    /// `(period_frames, periods)` of the running configuration.
    pub fn period(&self) -> (u32, u32) {
        (self.period_frames, self.periods)
    }

    /// Xruns recovered since the last [`OutputSink::negotiate`].
    pub fn xrun_count(&self) -> u64 {
        self.xruns.load(Ordering::Relaxed)
    }

    /// Whether the render thread is currently running.
    pub fn is_running(&self) -> bool {
        self.thread.is_some()
    }
}

impl OutputSink for AlsaSink {
    type Callback = AlsaCallback;

    fn negotiate(&mut self, want: &StreamRequest) -> Result<StreamSpec, SinkError> {
        // A renegotiation tears the running stream down first (graceful).
        self.stop().map_err(|e| SinkError::Negotiation(e.to_string()))?;
        self.xruns.store(0, Ordering::Relaxed);

        let want_rate = if want.rate_hz == 0 { 48_000 } else { want.rate_hz };
        let want_ch = if want.channels == 0 { 2 } else { want.channels };
        let (period_target, periods) = negotiate_period(want.buffer_frames);

        let mut opened_any = false;
        let mut last_err = String::new();
        for name in device_candidates(&self.name) {
            let Ok(c_name) = CString::new(name.clone()) else {
                last_err = format!("{name}: interior NUL in device name");
                continue;
            };
            let mut h: *mut SndPcmHandle = std::ptr::null_mut();
            let rc = unsafe { snd_pcm_open(&mut h, c_name.as_ptr(), SND_PCM_STREAM_PLAYBACK, 0) };
            if rc < 0 || h.is_null() {
                last_err = format!("{name}: snd_pcm_open({rc})");
                continue;
            }
            opened_any = true;
            // SAFETY: `h` is a freshly opened, exclusively owned handle.
            match unsafe { configure_pcm(h, want_rate, want_ch, period_target, periods) } {
                Ok((format, spec, granted_period, granted_periods)) => {
                    self.handle = h;
                    self.format = format;
                    self.spec = spec;
                    self.period_frames = granted_period;
                    self.periods = granted_periods;
                    self.device_opened = name;
                    return Ok(spec);
                }
                Err(msg) => {
                    // SAFETY: closing the just-opened handle we still own.
                    unsafe { snd_pcm_close(h) };
                    last_err = format!("{name}: {msg}");
                }
            }
        }
        Err(if opened_any {
            SinkError::Negotiation(last_err)
        } else {
            SinkError::Unavailable(last_err)
        })
    }

    fn start(&mut self, cb: Self::Callback) -> Result<(), SinkError> {
        if self.handle.is_null() {
            return Err(SinkError::Start("no negotiated stream; call negotiate first".into()));
        }
        if self.thread.is_some() {
            return Err(SinkError::Start("stream already running".into()));
        }
        self.stop.store(false, Ordering::Relaxed);
        let handle = PcmThreadHandle(self.handle);
        let stop = self.stop.clone();
        let xruns = self.xruns.clone();
        let format = self.format;
        let channels = self.spec.channels;
        let period_frames = self.period_frames;
        let thread = std::thread::Builder::new()
            .name("alsa-out".into())
            .spawn(move || rt_loop(handle, stop, cb, xruns, format, channels, period_frames))
            .map_err(|e| SinkError::Start(e.to_string()))?;
        self.thread = Some(thread);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            if t.join().is_err() {
                return Err(SinkError::Stop("render thread panicked".into()));
            }
        }
        if !self.handle.is_null() {
            // SAFETY: the render thread has been joined (or never started), so the
            // handle has no other user.
            unsafe {
                snd_pcm_drop(self.handle);
                snd_pcm_close(self.handle);
            }
            self.handle = std::ptr::null_mut();
            self.device_opened.clear();
        }
        Ok(())
    }

    fn spec(&self) -> &StreamSpec {
        &self.spec
    }
}

impl Drop for AlsaSink {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// ------------------------------------------------------------------------------------
// Negotiation + RT loop internals
// ------------------------------------------------------------------------------------

/// Full hw/sw negotiation on an open handle. Returns the granted
/// `(format, spec, period_frames, periods)`.
unsafe fn configure_pcm(
    h: *mut SndPcmHandle,
    want_rate: u32,
    want_channels: u16,
    period_target: u32,
    periods: u32,
) -> Result<(SampleFormat, StreamSpec, u32, u32), String> {
    let mut hw = AlignedBuf::<HW_PARAMS_BYTES>([0u8; HW_PARAMS_BYTES]);
    if snd_pcm_hw_params_sizeof() > HW_PARAMS_BYTES {
        return Err("snd_pcm_hw_params_t exceeds HW_PARAMS_BYTES".into());
    }
    let hw_ptr = hw.0.as_mut_ptr() as *mut SndPcmHwParams;
    let check = |rc: c_int, what: &str| {
        if rc < 0 {
            Err(format!("{what} failed ({rc})"))
        } else {
            Ok(())
        }
    };

    check(snd_pcm_hw_params_any(h, hw_ptr), "snd_pcm_hw_params_any")?;
    check(
        snd_pcm_hw_params_set_access(h, hw_ptr, SND_PCM_ACCESS_RW_INTERLEAVED),
        "set_access(RW_INTERLEAVED)",
    )?;

    // FLOAT_LE preferred, S32_LE is the required minimum.
    let format = if snd_pcm_hw_params_set_format(h, hw_ptr, SampleFormat::FloatLe.alsa_code()) >= 0 {
        SampleFormat::FloatLe
    } else {
        check(
            snd_pcm_hw_params_set_format(h, hw_ptr, SampleFormat::S32Le.alsa_code()),
            "set_format(S32_LE)",
        )?;
        SampleFormat::S32Le
    };

    check(
        snd_pcm_hw_params_set_channels(h, hw_ptr, c_uint::from(want_channels)),
        "set_channels",
    )?;

    let mut rate: c_uint = want_rate;
    let mut dir: c_int = 0;
    check(
        snd_pcm_hw_params_set_rate_near(h, hw_ptr, &mut rate, &mut dir),
        "set_rate_near",
    )?;

    let mut period: usize = period_target as usize;
    dir = 0;
    check(
        snd_pcm_hw_params_set_period_size_near(h, hw_ptr, &mut period, &mut dir),
        "set_period_size_near",
    )?;

    let mut periods_granted: c_uint = periods;
    dir = 0;
    check(
        snd_pcm_hw_params_set_periods_near(h, hw_ptr, &mut periods_granted, &mut dir),
        "set_periods_near",
    )?;

    check(snd_pcm_hw_params(h, hw_ptr), "snd_pcm_hw_params")?;

    // Read back what ALSA actually granted.
    let mut got_rate: c_uint = 0;
    let mut got_period: usize = 0;
    let mut got_periods: c_uint = 0;
    let mut got_channels: c_uint = 0;
    dir = 0;
    check(snd_pcm_hw_params_get_rate(hw_ptr, &mut got_rate, &mut dir), "get_rate")?;
    dir = 0;
    check(
        snd_pcm_hw_params_get_period_size(hw_ptr, &mut got_period, &mut dir),
        "get_period_size",
    )?;
    dir = 0;
    check(snd_pcm_hw_params_get_periods(hw_ptr, &mut got_periods, &mut dir), "get_periods")?;
    check(snd_pcm_hw_params_get_channels(hw_ptr, &mut got_channels), "get_channels")?;

    // sw params: wake the loop when one period of space is available; start the
    // stream after the first full period has been written.
    let mut sw = AlignedBuf::<SW_PARAMS_BYTES>([0u8; SW_PARAMS_BYTES]);
    if snd_pcm_sw_params_sizeof() > SW_PARAMS_BYTES {
        return Err("snd_pcm_sw_params_t exceeds SW_PARAMS_BYTES".into());
    }
    let sw_ptr = sw.0.as_mut_ptr() as *mut SndPcmSwParams;
    check(snd_pcm_sw_params_current(h, sw_ptr), "sw_params_current")?;
    check(
        snd_pcm_sw_params_set_start_threshold(h, sw_ptr, got_period),
        "set_start_threshold",
    )?;
    check(snd_pcm_sw_params_set_avail_min(h, sw_ptr, got_period), "set_avail_min")?;
    check(snd_pcm_sw_params(h, sw_ptr), "snd_pcm_sw_params")?;

    let spec = StreamSpec {
        rate_hz: got_rate,
        buffer_frames: got_period.min(u32::MAX as usize) as u32,
        channels: got_channels.min(u16::MAX as c_uint) as u16,
    };
    Ok((format, spec, spec.buffer_frames, got_periods))
}

/// Best-effort `SCHED_FIFO` for the calling thread; returns whether the policy
/// was applied (false = graceful fallback to normal scheduling).
pub fn set_rt_priority() -> bool {
    let param = SchedParam { sched_priority: RT_PRIORITY };
    // SAFETY: `param` is a valid `sched_param`; pid 0 = calling thread. Failure
    // (e.g. EPERM without CAP_SYS_NICE) is fine — fallback to normal scheduling.
    unsafe { sched_setscheduler(0, SCHED_FIFO, &param) == 0 }
}

/// The render thread: poll -> render one period -> interleave -> `snd_pcm_writei`,
/// with xrun recovery and counting (see module doc).
fn rt_loop(
    handle: PcmThreadHandle,
    stop: Arc<AtomicBool>,
    mut cb: AlsaCallback,
    xruns: Arc<AtomicU64>,
    format: SampleFormat,
    channels: u16,
    period_frames: u32,
) {
    let h = handle.0;
    let nch = channels.max(1) as usize;
    let pf = period_frames.max(1) as usize;

    let _rt = set_rt_priority();

    // All buffers are allocated before the first render call (no RT allocation).
    let mut scratch: Vec<f32> = vec![0.0; nch * pf];
    let mut i32_buf: Vec<i32> = vec![0; nch * pf];
    let mut f32_buf: Vec<f32> = vec![0.0; nch * pf];
    // Fat-pointer table used to build `&mut [&mut [f32]]` per period without
    // allocating: `&mut [f32]` has the layout of `*mut [f32]`.
    let mut rows: Vec<*mut [f32]> = Vec::with_capacity(nch);
    for ch in 0..nch {
        // SAFETY: `ch * pf .. ch * pf + pf` is in bounds of `scratch`; the
        // resulting slices are pairwise disjoint.
        rows.push(unsafe {
            core::ptr::slice_from_raw_parts_mut(scratch.as_mut_ptr().add(ch * pf), pf)
        });
    }

    let mut pollfds = [PollFd { fd: 0, events: 0, revents: 0 }; 4];
    let mut xstate = XrunState::Running;

    // SAFETY: fresh handle owned by this thread.
    unsafe { snd_pcm_prepare(h) };

    'outer: while !stop.load(Ordering::Relaxed) {
        // SAFETY: `pollfds` has space for at least one descriptor; ALSA fills
        // `min(count, 4)` of them.
        let nfds = unsafe { snd_pcm_poll_descriptors(h, pollfds.as_mut_ptr(), 4) };
        if nfds < 0 {
            break;
        }
        let nfds = (nfds as usize).min(pollfds.len());
        for p in pollfds[..nfds].iter_mut() {
            p.revents = 0;
        }
        // SAFETY: `nfds` <= 4 = pollfds.len().
        let pr = unsafe { poll(pollfds.as_mut_ptr(), nfds, POLL_TIMEOUT_MS) };
        if pr < 0 {
            continue; // EINTR / transient: re-check the stop flag
        }
        let mut writable = false;
        for p in &pollfds[..nfds] {
            if p.revents & (POLLERR | POLLHUP | POLLNVAL) != 0 {
                writable = true; // force a write attempt; ALSA reports the real error
            }
            if p.revents & POLLOUT != 0 {
                writable = true;
            }
        }
        if !writable {
            continue; // timeout: loop back to the stop flag
        }

        // Render one period into the per-channel planar scratch.
        let now = monotonic_ns();
        // SAFETY: `rows` holds `nch` disjoint `pf`-frame slices of `scratch`;
        // `&mut [f32]` is layout-identical to `*mut [f32]`. The view lives only
        // for the render call and cannot outlive the next table rebuild.
        let out: &mut [&mut [f32]] =
            unsafe { std::slice::from_raw_parts_mut(rows.as_mut_ptr() as *mut &mut [f32], nch) };
        cb.render(out, now);
        // `out` is dead here: the planar scratch is now only read for interleaving.
        // SAFETY: shared reborrow of the same disjoint slices after `out` ended.
        let planar: &[&[f32]] =
            unsafe { std::slice::from_raw_parts(rows.as_ptr() as *const &[f32], nch) };

        let (ptr, frame_bytes) = match format {
            SampleFormat::FloatLe => {
                planar_to_interleaved_f32(planar, &mut f32_buf);
                (f32_buf.as_ptr() as *const u8, nch * format.sample_bytes())
            }
            SampleFormat::S32Le => {
                planar_to_interleaved_i32(planar, &mut i32_buf);
                (i32_buf.as_ptr() as *const u8, nch * format.sample_bytes())
            }
        };

        // Write exactly one period (looping over partial writes).
        let mut written = 0usize;
        while written < pf {
            // SAFETY: `ptr + written * frame_bytes` is in bounds of the interleave
            // buffer for `pf - written` frames.
            let ret = unsafe {
                snd_pcm_writei(
                    h,
                    ptr.add(written * frame_bytes) as *const c_void,
                    pf - written,
                )
            };
            if ret > 0 {
                written += ret as usize;
                continue;
            }
            if ret == 0 {
                continue;
            }
            let err = ret as c_int; // negative errno, as snd_pcm_writei returns it
            if err == -EAGAIN {
                continue;
            }
            // -EPIPE / -ESTRPIPE / other: recover, count and notify.
            xstate = xstate.on_xrun();
            cb.on_xrun();
            xruns.fetch_add(1, Ordering::Relaxed);
            // SAFETY: handle is live; recovery runs on its owning thread.
            let rec = unsafe { snd_pcm_recover(h, err, 1) };
            if rec == 0 {
                xstate = xstate.on_recovered();
            } else {
                xstate = xstate.on_fatal();
            }
            if xstate.is_dead() {
                break 'outer;
            }
        }
    }
}
