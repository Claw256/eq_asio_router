//! WASAPI **loopback** capture of a render endpoint (e.g. a monitor's HD Audio endpoint) plus a
//! keep-alive render stream, and the pure helpers used to judge the arrival cadence.
//!
//! * [`LoopbackSession`]: `AUDCLNT_STREAMFLAGS_LOOPBACK` on an `IAudioClient` activated on the
//!   *render* endpoint. Event-driven when the driver allows it, otherwise a timer-polled loop.
//!   Same [`FrameSink`] / [`CaptureStats`] / [`CaptureEvent`] conventions as `CaptureSession`.
//! * [`KeepAlive`]: a shared-mode event-driven render stream writing digital silence (or an
//!   optional, capped, inaudible test signal). Loopback delivers nothing while no stream renders,
//!   and a render client's period request can lower the endpoint's engine period.
//! * Read-only queries: [`endpoint_volume`], [`current_engine_period`].
//! * Pure helpers: [`arrival_stats`], [`classify_hresult`], [`polling_interval_ms`], ...

use crate::convert::{convert_to_stereo, write_sample, FormatInfo, PacketFlags};
use crate::device::{activate_client, open_device, ComGuard, MixFormat};
use crate::session::{CaptureEvent, CaptureMode, CaptureStats, FrameSink};
use crate::signal::{SignalGen, SignalKind};
use crate::{CaptureError, Result};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use windows::core::{w, Interface};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IAudioClient3, IAudioRenderClient, IMMDevice,
    AUDCLNT_E_DEVICE_IN_USE, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_ENDPOINT_CREATE_FAILED,
    AUDCLNT_E_RESOURCES_INVALIDATED, AUDCLNT_E_SERVICE_NOT_RUNNING, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    CreateWaitableTimerExW, SetEvent, SetWaitableTimer, WaitForMultipleObjects,
};

const EVENTCB: u32 = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
const LOOPBACK: u32 = AUDCLNT_STREAMFLAGS_LOOPBACK;
/// CREATE_WAITABLE_TIMER_HIGH_RESOLUTION
const TIMER_HIGH_RES: u32 = 0x2;
/// TIMER_ALL_ACCESS
const TIMER_ALL_ACCESS: u32 = 0x1F_0003;
/// Hard cap for any audible-in-theory test signal rendered by [`KeepAlive`].
pub const MAX_TEST_SIGNAL_DBFS: f32 = -40.0;

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// Coarse classification of a WASAPI failure code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HrClass {
    /// Endpoint removed / disabled / monitor link lost / audio service restarted: re-open later.
    DeviceGone,
    /// Another client holds the endpoint exclusively.
    Busy,
    /// Anything else.
    Other,
}

/// Classify an HRESULT (as `i32`) for device-lost handling.
pub fn classify_hresult(code: i32) -> HrClass {
    const E_NOTFOUND: i32 = 0x8007_0490_u32 as i32; // HRESULT_FROM_WIN32(ERROR_NOT_FOUND)
    const E_ELEMENT_NOT_FOUND: i32 = 0x8002_802B_u32 as i32;
    if code == AUDCLNT_E_DEVICE_INVALIDATED.0
        || code == AUDCLNT_E_RESOURCES_INVALIDATED.0
        || code == AUDCLNT_E_ENDPOINT_CREATE_FAILED.0
        || code == AUDCLNT_E_SERVICE_NOT_RUNNING.0
        || code == E_NOTFOUND
        || code == E_ELEMENT_NOT_FOUND
    {
        HrClass::DeviceGone
    } else if code == AUDCLNT_E_DEVICE_IN_USE.0 {
        HrClass::Busy
    } else {
        HrClass::Other
    }
}

/// True when `e` means the endpoint is missing / invalidated (monitor off, DP link lost).
pub fn is_endpoint_missing(e: &CaptureError) -> bool {
    match e {
        CaptureError::NotFound(_) => true,
        CaptureError::Com { source, .. } => classify_hresult(source.code().0) == HrClass::DeviceGone,
        _ => false,
    }
}

/// Polling interval (ms) for a period: half the period, at least 1 ms.
pub fn polling_interval_ms(period_frames: u32, rate: u32) -> u32 {
    if rate == 0 {
        return 1;
    }
    let ms = period_frames as f64 * 1000.0 / rate as f64 / 2.0;
    (ms.round() as u32).max(1)
}

/// Symbolic name of common AUDCLNT_E_* codes (for readable reports).
pub fn hr_name(code: i32) -> &'static str {
    match code as u32 {
        0x8889_0001 => "AUDCLNT_E_NOT_INITIALIZED",
        0x8889_0002 => "AUDCLNT_E_ALREADY_INITIALIZED",
        0x8889_0004 => "AUDCLNT_E_DEVICE_INVALIDATED",
        0x8889_0008 => "AUDCLNT_E_UNSUPPORTED_FORMAT",
        0x8889_000A => "AUDCLNT_E_DEVICE_IN_USE",
        0x8889_000E => "AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED",
        0x8889_000F => "AUDCLNT_E_ENDPOINT_CREATE_FAILED",
        0x8889_0010 => "AUDCLNT_E_SERVICE_NOT_RUNNING",
        0x8889_0011 => "AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED",
        0x8889_0014 => "AUDCLNT_E_EVENTHANDLE_NOT_SET",
        0x8889_0020 => "AUDCLNT_E_INVALID_DEVICE_PERIOD",
        0x8889_0021 => "AUDCLNT_E_INVALID_STREAM_FLAG",
        0x8889_0025 => "AUDCLNT_E_NONOFFLOAD_MODE_ONLY",
        0x8889_0026 => "AUDCLNT_E_RESOURCES_INVALIDATED",
        0x8007_0490 => "E_NOTFOUND",
        0x8007_0057 => "E_INVALIDARG",
        _ => "",
    }
}

/// Error text with the symbolic HRESULT name.
pub fn describe(e: &CaptureError) -> String {
    match e {
        CaptureError::Com { context, source } => format!("{context}: {}", hr_text(source)),
        other => other.to_string(),
    }
}

/// Format an HRESULT for reports.
pub fn hr_text(e: &windows::core::Error) -> String {
    let n = hr_name(e.code().0);
    format!("0x{:08X} {}", e.code().0 as u32, if n.is_empty() { e.message().trim().to_string() } else { n.to_string() })
}

/// Summary of packet arrival times.
#[derive(Clone, Debug, PartialEq)]
pub struct ArrivalStats {
    /// Number of inter-arrival gaps.
    pub gaps: usize,
    pub min_us: f64,
    pub mean_us: f64,
    pub p50_us: f64,
    pub p99_us: f64,
    pub p999_us: f64,
    pub max_us: f64,
    /// Gaps below a quarter of the nominal period (packets delivered back-to-back).
    pub back_to_back: usize,
    /// Gaps above 1.5x the nominal period.
    pub late: usize,
}

impl ArrivalStats {
    /// "regular" / "bursty (back-to-back packets)" / "irregular".
    pub fn pattern(&self) -> &'static str {
        if self.gaps == 0 {
            "n/a"
        } else if self.back_to_back as f64 > 0.3 * self.gaps as f64 {
            "bursty (back-to-back packets)"
        } else if self.late as f64 > 0.01 * self.gaps as f64 {
            "irregular (late arrivals)"
        } else {
            "regular"
        }
    }
}

/// Statistics of absolute arrival times (microseconds, ascending) against a nominal period.
pub fn arrival_stats(times_us: &[f64], nominal_period_us: f64) -> Option<ArrivalStats> {
    if times_us.len() < 2 {
        return None;
    }
    let mut gaps: Vec<f64> = times_us.windows(2).map(|w| w[1] - w[0]).collect();
    let n = gaps.len();
    let mean = gaps.iter().sum::<f64>() / n as f64;
    let back = gaps.iter().filter(|g| **g < nominal_period_us * 0.25).count();
    let late = gaps.iter().filter(|g| **g > nominal_period_us * 1.5).count();
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let q = |p: f64| gaps[(((n - 1) as f64) * p).round() as usize];
    Some(ArrivalStats {
        gaps: n,
        min_us: gaps[0],
        mean_us: mean,
        p50_us: q(0.5),
        p99_us: q(0.99),
        p999_us: q(0.999),
        max_us: gaps[n - 1],
        back_to_back: back,
        late,
    })
}

/// Fill `frames` frames of interleaved render data: silence, or the generator on channels 0/1
/// (other channels stay zero). `buf` must hold `frames * fmt.block_align()` bytes.
pub fn fill_render(buf: &mut [u8], frames: usize, fmt: &FormatInfo, gen: Option<&mut SignalGen>) {
    let block = fmt.block_align();
    let bps = fmt.format.bytes_per_sample();
    let n = frames.min(buf.len() / block.max(1));
    buf[..n * block].fill(0);
    if let Some(g) = gen {
        for i in 0..n {
            let v = g.next_sample();
            for c in 0..(fmt.channels as usize).min(2) {
                let o = i * block + c * bps;
                write_sample(fmt.format, v, &mut buf[o..o + bps]);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct SendHandle(isize);
impl SendHandle {
    fn new(h: HANDLE) -> Self {
        SendHandle(h.0 as isize)
    }
    fn get(self) -> HANDLE {
        HANDLE(self.0 as *mut _)
    }
}

// ---------------------------------------------------------------------------------------------
// Read-only endpoint queries
// ---------------------------------------------------------------------------------------------

/// Current endpoint master volume scalar (0..1) and mute state. READ-ONLY.
pub fn endpoint_volume(device_id: &str) -> Result<(f32, bool)> {
    let _com = ComGuard::init()?;
    let dev = open_device(device_id)?;
    // SAFETY: plain activation and getters on a valid interface.
    unsafe {
        let v: IAudioEndpointVolume = dev
            .Activate(CLSCTX_ALL, None)
            .map_err(CaptureError::com("Activate(IAudioEndpointVolume)"))?;
        let s = v.GetMasterVolumeLevelScalar().map_err(CaptureError::com("GetMasterVolumeLevelScalar"))?;
        let m = v.GetMute().map_err(CaptureError::com("GetMute"))?;
        Ok((s, m.as_bool()))
    }
}

/// `IAudioClient::GetStreamLatency` (ms) of a throw-away shared-mode client on `device_id` (initialised, never started:
/// nothing is captured or rendered). Best effort: `None` if the endpoint refuses or reports nothing. READ-ONLY.
pub fn probe_stream_latency_ms(device_id: &str) -> Option<f64> {
    let _com = ComGuard::init().ok()?;
    let dev = open_device(device_id).ok()?;
    let client = activate_client(&dev).ok()?;
    let mix = MixFormat::from_client(&client).ok()?;
    // SAFETY: valid client and format pointer; the client is dropped without Start.
    unsafe {
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, mix.ptr(), None).ok()?;
        client.GetStreamLatency().ok().map(|h| h as f64 / 10_000.0)
    }
}

/// Current shared-mode engine period of the endpoint in frames (IAudioClient3), if available.
pub fn current_engine_period(device_id: &str) -> Option<(u32, u32)> {
    let _com = ComGuard::init().ok()?;
    let dev = open_device(device_id).ok()?;
    current_period_of(&dev)
}

fn current_period_of(dev: &IMMDevice) -> Option<(u32, u32)> {
    // SAFETY: out params are locals; the returned format block is freed after reading.
    unsafe {
        let c3 = dev.Activate::<IAudioClient3>(CLSCTX_ALL, None).ok()?;
        let mut pf: *mut WAVEFORMATEX = std::ptr::null_mut();
        let mut period = 0u32;
        c3.GetCurrentSharedModeEnginePeriod(&mut pf, &mut period).ok()?;
        let rate = if pf.is_null() { 0 } else { std::ptr::read_unaligned(pf).nSamplesPerSec };
        if !pf.is_null() {
            CoTaskMemFree(Some(pf as *const _));
        }
        Some((period, rate))
    }
}

// ---------------------------------------------------------------------------------------------
// Loopback session
// ---------------------------------------------------------------------------------------------

/// How the loopback thread is woken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeMode {
    /// `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` (signalled by the engine).
    Event,
    /// Waitable-timer polling at half the period.
    Polling,
}

/// Loopback options.
#[derive(Clone, Copy, Debug)]
pub struct LoopbackOptions {
    /// `Shared` or `LowLatency` (IAudioClient3 min period). `Exclusive` is treated as `Shared`.
    pub mode: CaptureMode,
    /// Allow the timer-polled fallback when event-driven loopback cannot be initialised.
    pub allow_polling: bool,
    /// Skip the event-driven attempt and poll with a waitable timer. Useful when another stream
    /// (the keep-alive) lowered the endpoint's engine period below the loopback client's own
    /// event period: polling then sees the small engine packets as they appear.
    pub force_polling: bool,
    /// Polling interval override (ms); `None` = half of the current engine period (else of the
    /// stream period), at least 1 ms.
    pub poll_ms: Option<u32>,
    /// Experimental: explicit `IAudioClient::Initialize` buffer duration (100 ns units) for the
    /// plain shared attempt (0/`None` = engine default).
    pub buffer_hns: Option<i64>,
}

impl Default for LoopbackOptions {
    fn default() -> Self {
        Self { mode: CaptureMode::Shared, allow_polling: true, force_polling: false, poll_ms: None, buffer_hns: None }
    }
}

/// What `LoopbackSession::start` negotiated.
#[derive(Clone, Debug)]
pub struct LoopbackInfo {
    /// Engine mix format of the render endpoint (what the loopback delivers).
    pub format: FormatInfo,
    pub period_frames: u32,
    pub mode: CaptureMode,
    pub wake: WakeMode,
    /// One line per failed initialisation attempt, with the precise HRESULT.
    pub attempts: Vec<String>,
}

/// Running loopback capture. Dropping it stops and joins the thread.
pub struct LoopbackSession {
    stats: Arc<CaptureStats>,
    wakeups: Arc<AtomicU64>,
    events: Receiver<CaptureEvent>,
    stop: SendHandle,
    thread: Option<JoinHandle<()>>,
    info: LoopbackInfo,
}

struct Opened {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    fmt: FormatInfo,
    buffer_frames: u32,
    period_frames: u32,
    mode: CaptureMode,
    wake: WakeMode,
}

fn finish(
    client: IAudioClient,
    fmt: FormatInfo,
    period_frames: u32,
    mode: CaptureMode,
    wake: WakeMode,
    event: HANDLE,
) -> Result<Opened> {
    // SAFETY: client initialised; event handle valid.
    unsafe {
        if wake == WakeMode::Event {
            client.SetEventHandle(event).map_err(CaptureError::com("SetEventHandle"))?;
        }
        let buffer_frames = client.GetBufferSize().map_err(CaptureError::com("GetBufferSize"))?;
        let capture: IAudioCaptureClient =
            client.GetService().map_err(CaptureError::com("GetService(IAudioCaptureClient)"))?;
        Ok(Opened { client, capture, fmt, buffer_frames, period_frames, mode, wake })
    }
}

fn try_low_latency(dev: &IMMDevice, wake: WakeMode, event: HANDLE) -> Result<Opened> {
    let probe = activate_client(dev)?;
    let mix = MixFormat::from_client(&probe)?;
    let fmt = mix.info()?;
    // SAFETY: IAudioClient3 of the same endpoint; the mix pointer stays valid for the calls.
    unsafe {
        let c3 = dev
            .Activate::<IAudioClient3>(CLSCTX_ALL, None)
            .map_err(CaptureError::com("Activate(IAudioClient3)"))?;
        let (mut d, mut f, mut mn, mut mx) = (0u32, 0u32, 0u32, 0u32);
        c3.GetSharedModeEnginePeriod(mix.ptr(), &mut d, &mut f, &mut mn, &mut mx)
            .map_err(CaptureError::com("GetSharedModeEnginePeriod"))?;
        let flags = LOOPBACK | if wake == WakeMode::Event { EVENTCB } else { 0 };
        c3.InitializeSharedAudioStream(flags, mn, mix.ptr(), None)
            .map_err(CaptureError::com("InitializeSharedAudioStream(loopback)"))?;
        let c: IAudioClient = c3.cast().map_err(CaptureError::com("cast IAudioClient3"))?;
        finish(c, fmt, mn, CaptureMode::LowLatency, wake, event)
    }
}

fn try_shared(dev: &IMMDevice, wake: WakeMode, event: HANDLE, buffer_hns: Option<i64>) -> Result<Opened> {
    let client = activate_client(dev)?;
    let mix = MixFormat::from_client(&client)?;
    let fmt = mix.info()?;
    let mut def = 0i64;
    // SAFETY: valid pointers; shared-mode loopback init.
    unsafe {
        client.GetDevicePeriod(Some(&mut def), None).map_err(CaptureError::com("GetDevicePeriod"))?;
        let (flags, dur) = match wake {
            WakeMode::Event => (LOOPBACK | EVENTCB, buffer_hns.unwrap_or(0)),
            WakeMode::Polling => (LOOPBACK, buffer_hns.unwrap_or(def.max(100_000) * 2)),
        };
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, dur, 0, mix.ptr(), None)
            .map_err(CaptureError::com("IAudioClient::Initialize(shared, loopback)"))?;
    }
    let period = (def as f64 * fmt.sample_rate as f64 / 1e7).round() as u32;
    finish(client, fmt, period, CaptureMode::Shared, wake, event)
}

/// Try the attempts in preference order; returns the opened stream and a log of failures.
fn open_loopback(
    dev: &IMMDevice,
    opts: &LoopbackOptions,
    event: HANDLE,
) -> Result<(Opened, Vec<String>)> {
    let mut log = Vec::new();
    let mut order: Vec<(bool, WakeMode)> = Vec::new();
    if opts.mode == CaptureMode::LowLatency {
        order.push((true, WakeMode::Event));
        if opts.allow_polling {
            order.push((true, WakeMode::Polling));
        }
    }
    if opts.force_polling {
        order.retain(|(_, w)| *w == WakeMode::Polling);
        order.push((false, WakeMode::Polling));
    } else {
        order.push((false, WakeMode::Event));
        if opts.allow_polling {
            order.push((false, WakeMode::Polling));
        }
    }
    let mut last = None;
    for (ll, wake) in order {
        let r = if ll { try_low_latency(dev, wake, event) } else { try_shared(dev, wake, event, opts.buffer_hns) };
        match r {
            Ok(o) => return Ok((o, log)),
            Err(e) => {
                log.push(format!("{} + {:?}: {}", if ll { "IAudioClient3 min period" } else { "shared default period" }, wake, describe(&e)));
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| CaptureError::UnsupportedFormat("no loopback init attempted".into())))
}

impl LoopbackSession {
    /// Open the *render* endpoint `render_endpoint_id` in loopback and start capturing into
    /// `sink`. Blocks until running (or failed).
    pub fn start(render_endpoint_id: &str, options: LoopbackOptions, sink: Box<dyn FrameSink>) -> Result<Self> {
        // SAFETY: plain event creation; closed on stop/Drop.
        let stop = unsafe { CreateEventW(None, true, false, None) }
            .map_err(CaptureError::com("CreateEventW(stop)"))?;
        let stop_h = SendHandle::new(stop);
        let stats = Arc::new(CaptureStats::default());
        let wakeups = Arc::new(AtomicU64::new(0));
        let (ev_tx, ev_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<LoopbackInfo>>(1);
        let id = render_endpoint_id.to_string();
        let (st, wk) = (stats.clone(), wakeups.clone());
        let thread = std::thread::Builder::new()
            .name("wasapi-loopback".into())
            .spawn(move || loopback_thread(id, options, sink, st, wk, stop_h, ev_tx, ready_tx))
            .map_err(|e| CaptureError::Thread(e.to_string()))?;
        let fail = |thread: JoinHandle<()>| {
            let _ = thread.join();
            // SAFETY: handle created above and no longer used by the thread.
            unsafe { let _ = CloseHandle(stop); }
        };
        match ready_rx.recv() {
            Ok(Ok(info)) => Ok(Self { stats, wakeups, events: ev_rx, stop: stop_h, thread: Some(thread), info }),
            Ok(Err(e)) => {
                fail(thread);
                Err(e)
            }
            Err(_) => {
                fail(thread);
                Err(CaptureError::Thread("loopback thread exited during start".into()))
            }
        }
    }

    pub fn stats(&self) -> &Arc<CaptureStats> {
        &self.stats
    }
    /// Number of wake-ups that found the thread running (events or timer ticks).
    pub fn wakeups(&self) -> u64 {
        self.wakeups.load(Relaxed)
    }
    pub fn events(&self) -> &Receiver<CaptureEvent> {
        &self.events
    }
    pub fn info(&self) -> &LoopbackInfo {
        &self.info
    }
    pub fn format(&self) -> FormatInfo {
        self.info.format
    }
    pub fn sample_rate(&self) -> u32 {
        self.info.format.sample_rate
    }
    pub fn period_frames(&self) -> u32 {
        self.info.period_frames
    }
    pub fn mode_in_use(&self) -> CaptureMode {
        self.info.mode
    }
    pub fn wake_mode(&self) -> WakeMode {
        self.info.wake
    }
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
    /// Stop promptly and join the thread. Idempotent.
    pub fn stop(&mut self) {
        if let Some(t) = self.thread.take() {
            // SAFETY: the stop event stays valid until closed below.
            unsafe { let _ = SetEvent(self.stop.get()); }
            let _ = t.join();
            // SAFETY: thread joined; nobody else uses the handle.
            unsafe { let _ = CloseHandle(self.stop.get()); }
        }
    }
}

impl Drop for LoopbackSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn loopback_thread(
    device_id: String,
    options: LoopbackOptions,
    mut sink: Box<dyn FrameSink>,
    stats: Arc<CaptureStats>,
    wakeups: Arc<AtomicU64>,
    stop: SendHandle,
    events: Sender<CaptureEvent>,
    ready: mpsc::SyncSender<Result<LoopbackInfo>>,
) {
    let _com = match ComGuard::init() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut task_index = 0u32;
    // SAFETY: valid task name and out pointer.
    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }.ok();
    // SAFETY: auto-reset event signalled by WASAPI; closed at the end.
    let event = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(CaptureError::Com { context: "CreateEventW", source: e }));
            return;
        }
    };
    // SAFETY: high-resolution timer for the polling fallback; closed at the end.
    let timer = unsafe { CreateWaitableTimerExW(None, None, TIMER_HIGH_RES, TIMER_ALL_ACCESS) }
        .or_else(|_| unsafe { CreateWaitableTimerExW(None, None, 0, TIMER_ALL_ACCESS) })
        .ok();

    let result = (|| -> Result<()> {
        let dev = open_device(&device_id)?;
        let (o, log) = open_loopback(&dev, &options, event)?;
        stats.sample_rate.store(o.fmt.sample_rate, Relaxed);
        stats.channels.store(o.fmt.channels as u32, Relaxed);
        stats.period_frames.store(o.period_frames, Relaxed);
        if !log.is_empty() || o.mode != options.mode {
            let _ = events.send(CaptureEvent::Fallback {
                requested: options.mode,
                using: o.mode,
                reason: format!("{:?} wake; failed attempts: {}", o.wake, log.join(" | ")),
            });
        }
        let _ = events.send(CaptureEvent::Started { format: o.fmt, period_frames: o.period_frames, mode: o.mode });
        let info = LoopbackInfo { format: o.fmt, period_frames: o.period_frames, mode: o.mode, wake: o.wake, attempts: log };
        let _ = ready.send(Ok(info));
        let wake_h = match o.wake {
            WakeMode::Event => event,
            WakeMode::Polling => timer.ok_or_else(|| CaptureError::Thread("no waitable timer".into()))?,
        };
        if o.wake == WakeMode::Polling {
            let ms = options.poll_ms.unwrap_or_else(|| match current_period_of(&dev) {
                Some((p, r)) if r > 0 && p > 0 => polling_interval_ms(p, r),
                _ => polling_interval_ms(o.period_frames, o.fmt.sample_rate),
            });
            let due = -((ms as i64) * 10_000);
            // SAFETY: valid timer handle; periodic relative due time.
            unsafe { SetWaitableTimer(wake_h, &due, ms as i32, None, None, false) }
                .map_err(CaptureError::com("SetWaitableTimer"))?;
        }
        run_loop(&o, &mut *sink, &stats, &wakeups, wake_h, stop.get(), &events);
        // SAFETY: stopping a started/initialised client is always valid.
        unsafe { let _ = o.client.Stop(); }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = ready.send(Err(e));
    }
    // SAFETY: handles owned by this thread.
    unsafe {
        let _ = CloseHandle(event);
        if let Some(t) = timer {
            let _ = CloseHandle(t);
        }
        if let Some(h) = mmcss {
            let _ = AvRevertMmThreadCharacteristics(h);
        }
    }
}

fn lost_event(e: &windows::core::Error) -> CaptureEvent {
    if classify_hresult(e.code().0) == HrClass::DeviceGone {
        CaptureEvent::DeviceLost
    } else {
        CaptureEvent::Error(hr_text(e))
    }
}

fn run_loop(
    o: &Opened,
    sink: &mut dyn FrameSink,
    stats: &CaptureStats,
    wakeups: &AtomicU64,
    wake: HANDLE,
    stop: HANDLE,
    events: &Sender<CaptureEvent>,
) {
    let cap = (o.buffer_frames as usize).max(64);
    let mut left = vec![0.0f32; cap];
    let mut right = vec![0.0f32; cap];
    let handles = [wake, stop];
    // SAFETY: initialised client.
    if let Err(e) = unsafe { o.client.Start() } {
        let _ = events.send(lost_event(&e));
        return;
    }
    loop {
        // SAFETY: both handles valid for the loop's lifetime.
        let w = unsafe { WaitForMultipleObjects(&handles, false, 1000) };
        if w.0 == WAIT_OBJECT_0.0 + 1 {
            return;
        }
        if w == WAIT_TIMEOUT {
            stats.wait_timeouts.fetch_add(1, Relaxed);
            // Nothing signalled for 1 s (idle loopback, or a dead device): probe for invalidation.
            if let Err(e) = unsafe { o.capture.GetNextPacketSize() } {
                let _ = events.send(lost_event(&e));
                return;
            }
            continue;
        }
        if w != WAIT_OBJECT_0 {
            let _ = events.send(CaptureEvent::Error(format!("wait failed: {w:?}")));
            return;
        }
        wakeups.fetch_add(1, Relaxed);
        if let Err(e) = drain(o, sink, stats, &mut left, &mut right) {
            let _ = events.send(lost_event(&e));
            return;
        }
    }
}

fn drain(
    o: &Opened,
    sink: &mut dyn FrameSink,
    stats: &CaptureStats,
    left: &mut [f32],
    right: &mut [f32],
) -> windows::core::Result<()> {
    let block = o.fmt.block_align();
    loop {
        // SAFETY: valid capture client.
        let next = unsafe { o.capture.GetNextPacketSize()? };
        if next == 0 {
            return Ok(());
        }
        let mut data: *mut u8 = std::ptr::null_mut();
        let (mut frames, mut flags) = (0u32, 0u32);
        // SAFETY: out pointers are locals; buffer valid until ReleaseBuffer.
        unsafe { o.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };
        let pf = PacketFlags::from_raw(flags);
        stats.record_packet(frames, pf);
        sink.on_packet(frames, pf);
        let n = frames as usize;
        let mut off = 0usize;
        while off < n {
            let chunk = (n - off).min(left.len());
            if pf.silent || data.is_null() {
                left[..chunk].fill(0.0);
                right[..chunk].fill(0.0);
            } else {
                // SAFETY: WASAPI guarantees `frames * block_align` readable bytes at `data`.
                let bytes = unsafe { std::slice::from_raw_parts(data.add(off * block), chunk * block) };
                let c = convert_to_stereo(&o.fmt, bytes, chunk, left, right);
                if c < chunk {
                    left[c..chunk].fill(0.0);
                    right[c..chunk].fill(0.0);
                }
            }
            sink.on_frames(&left[..chunk], &right[..chunk]);
            off += chunk;
        }
        // SAFETY: releases exactly the frames obtained.
        unsafe { o.capture.ReleaseBuffer(frames)? };
    }
}

// ---------------------------------------------------------------------------------------------
// Keep-alive render stream
// ---------------------------------------------------------------------------------------------

/// Keep-alive options.
#[derive(Clone, Copy, Debug)]
pub struct KeepAliveOptions {
    /// Request the smallest IAudioClient3 shared period (falls back to the default period).
    pub low_latency: bool,
    /// Optional test signal `(kind, dBFS)`; `None` renders digital silence. dBFS must be
    /// `<= MAX_TEST_SIGNAL_DBFS` (-40).
    pub signal: Option<(SignalKind, f32)>,
}

impl Default for KeepAliveOptions {
    fn default() -> Self {
        Self { low_latency: true, signal: None }
    }
}

/// What the keep-alive stream negotiated.
#[derive(Clone, Debug)]
pub struct KeepAliveInfo {
    pub format: FormatInfo,
    pub period_frames: u32,
    pub buffer_frames: u32,
    pub mode: CaptureMode,
    pub attempts: Vec<String>,
}

/// A running keep-alive render stream. Dropping it stops and joins the thread.
pub struct KeepAlive {
    events: Receiver<CaptureEvent>,
    stop: SendHandle,
    thread: Option<JoinHandle<()>>,
    info: KeepAliveInfo,
    frames_rendered: Arc<AtomicU64>,
    underruns: Arc<AtomicU32>,
    lost: Arc<AtomicBool>,
}

impl KeepAlive {
    pub fn start(render_endpoint_id: &str, options: KeepAliveOptions) -> Result<Self> {
        if let Some((_, db)) = options.signal {
            if db > MAX_TEST_SIGNAL_DBFS {
                return Err(CaptureError::UnsupportedFormat(format!(
                    "keep-alive test signal {db} dBFS exceeds the {MAX_TEST_SIGNAL_DBFS} dBFS safety cap"
                )));
            }
        }
        // SAFETY: plain event creation; closed on stop/Drop.
        let stop = unsafe { CreateEventW(None, true, false, None) }
            .map_err(CaptureError::com("CreateEventW(stop)"))?;
        let stop_h = SendHandle::new(stop);
        let (ev_tx, ev_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<KeepAliveInfo>>(1);
        let frames = Arc::new(AtomicU64::new(0));
        let under = Arc::new(AtomicU32::new(0));
        let lost = Arc::new(AtomicBool::new(false));
        let (id, f2, u2, l2) = (render_endpoint_id.to_string(), frames.clone(), under.clone(), lost.clone());
        let thread = std::thread::Builder::new()
            .name("wasapi-keepalive".into())
            .spawn(move || keepalive_thread(id, options, f2, u2, l2, stop_h, ev_tx, ready_tx))
            .map_err(|e| CaptureError::Thread(e.to_string()))?;
        let fail = |thread: JoinHandle<()>| {
            let _ = thread.join();
            // SAFETY: handle created above and no longer used by the thread.
            unsafe { let _ = CloseHandle(stop); }
        };
        match ready_rx.recv() {
            Ok(Ok(info)) => Ok(Self {
                events: ev_rx,
                stop: stop_h,
                thread: Some(thread),
                info,
                frames_rendered: frames,
                underruns: under,
                lost,
            }),
            Ok(Err(e)) => {
                fail(thread);
                Err(e)
            }
            Err(_) => {
                fail(thread);
                Err(CaptureError::Thread("keep-alive thread exited during start".into()))
            }
        }
    }

    pub fn info(&self) -> &KeepAliveInfo {
        &self.info
    }
    pub fn events(&self) -> &Receiver<CaptureEvent> {
        &self.events
    }
    pub fn frames_rendered(&self) -> u64 {
        self.frames_rendered.load(Relaxed)
    }
    /// Wake-ups at which the render buffer was found completely empty (padding 0).
    pub fn underruns(&self) -> u32 {
        self.underruns.load(Relaxed)
    }
    pub fn is_lost(&self) -> bool {
        self.lost.load(Relaxed)
    }
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
    pub fn stop(&mut self) {
        if let Some(t) = self.thread.take() {
            // SAFETY: the stop event stays valid until closed below.
            unsafe { let _ = SetEvent(self.stop.get()); }
            let _ = t.join();
            // SAFETY: thread joined.
            unsafe { let _ = CloseHandle(self.stop.get()); }
        }
    }
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        self.stop();
    }
}

struct RenderOpened {
    client: IAudioClient,
    render: IAudioRenderClient,
    fmt: FormatInfo,
    buffer_frames: u32,
    period_frames: u32,
    mode: CaptureMode,
}

fn open_render(dev: &IMMDevice, low_latency: bool, event: HANDLE, log: &mut Vec<String>) -> Result<RenderOpened> {
    let fin = |client: IAudioClient, fmt, period, mode| -> Result<RenderOpened> {
        // SAFETY: initialised for event callback; event valid.
        unsafe {
            client.SetEventHandle(event).map_err(CaptureError::com("SetEventHandle"))?;
            let buffer_frames = client.GetBufferSize().map_err(CaptureError::com("GetBufferSize"))?;
            let render: IAudioRenderClient =
                client.GetService().map_err(CaptureError::com("GetService(IAudioRenderClient)"))?;
            Ok(RenderOpened { client, render, fmt, buffer_frames, period_frames: period, mode })
        }
    };
    if low_latency {
        let r = (|| -> Result<RenderOpened> {
            let probe = activate_client(dev)?;
            let mix = MixFormat::from_client(&probe)?;
            let fmt = mix.info()?;
            // SAFETY: as in try_low_latency.
            unsafe {
                let c3 = dev
                    .Activate::<IAudioClient3>(CLSCTX_ALL, None)
                    .map_err(CaptureError::com("Activate(IAudioClient3)"))?;
                let (mut d, mut f, mut mn, mut mx) = (0u32, 0u32, 0u32, 0u32);
                c3.GetSharedModeEnginePeriod(mix.ptr(), &mut d, &mut f, &mut mn, &mut mx)
                    .map_err(CaptureError::com("GetSharedModeEnginePeriod"))?;
                c3.InitializeSharedAudioStream(EVENTCB, mn, mix.ptr(), None)
                    .map_err(CaptureError::com("InitializeSharedAudioStream(render)"))?;
                let c: IAudioClient = c3.cast().map_err(CaptureError::com("cast IAudioClient3"))?;
                fin(c, fmt, mn, CaptureMode::LowLatency)
            }
        })();
        match r {
            Ok(o) => return Ok(o),
            Err(e) => log.push(format!("keep-alive IAudioClient3 min period: {}", describe(&e))),
        }
    }
    let client = activate_client(dev)?;
    let mix = MixFormat::from_client(&client)?;
    let fmt = mix.info()?;
    let mut def = 0i64;
    // SAFETY: valid pointers.
    unsafe {
        client.GetDevicePeriod(Some(&mut def), None).map_err(CaptureError::com("GetDevicePeriod"))?;
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, EVENTCB, 0, 0, mix.ptr(), None)
            .map_err(CaptureError::com("IAudioClient::Initialize(render, shared)"))?;
    }
    let period = (def as f64 * fmt.sample_rate as f64 / 1e7).round() as u32;
    fin(client, fmt, period, CaptureMode::Shared)
}

#[allow(clippy::too_many_arguments)]
fn keepalive_thread(
    device_id: String,
    options: KeepAliveOptions,
    frames_rendered: Arc<AtomicU64>,
    underruns: Arc<AtomicU32>,
    lost: Arc<AtomicBool>,
    stop: SendHandle,
    events: Sender<CaptureEvent>,
    ready: mpsc::SyncSender<Result<KeepAliveInfo>>,
) {
    let _com = match ComGuard::init() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut task_index = 0u32;
    // SAFETY: valid task name and out pointer.
    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }.ok();
    // SAFETY: auto-reset event signalled by WASAPI; closed at the end.
    let event = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(CaptureError::Com { context: "CreateEventW", source: e }));
            return;
        }
    };
    let result = (|| -> Result<()> {
        let dev = open_device(&device_id)?;
        let mut log = Vec::new();
        let o = open_render(&dev, options.low_latency, event, &mut log)?;
        if o.mode != CaptureMode::LowLatency && options.low_latency {
            let _ = events.send(CaptureEvent::Fallback {
                requested: CaptureMode::LowLatency,
                using: o.mode,
                reason: log.join(" | "),
            });
        }
        let mut gen = options.signal.map(|(k, db)| SignalGen::new(k, o.fmt.sample_rate, db));
        let block = o.fmt.block_align();
        let write = |gen: &mut Option<SignalGen>, n: u32| -> windows::core::Result<()> {
            if n == 0 {
                return Ok(());
            }
            // SAFETY: GetBuffer yields `n * block` writable bytes until ReleaseBuffer.
            unsafe {
                let p = o.render.GetBuffer(n)?;
                let buf = std::slice::from_raw_parts_mut(p, n as usize * block);
                fill_render(buf, n as usize, &o.fmt, gen.as_mut());
                o.render.ReleaseBuffer(n, 0)?;
            }
            frames_rendered.fetch_add(n as u64, Relaxed);
            Ok(())
        };
        // Pre-fill the whole buffer, then start.
        write(&mut gen, o.buffer_frames).map_err(CaptureError::com("prefill"))?;
        // SAFETY: initialised client.
        unsafe { o.client.Start() }.map_err(CaptureError::com("Start(render)"))?;
        let _ = events.send(CaptureEvent::Started { format: o.fmt, period_frames: o.period_frames, mode: o.mode });
        let _ = ready.send(Ok(KeepAliveInfo {
            format: o.fmt,
            period_frames: o.period_frames,
            buffer_frames: o.buffer_frames,
            mode: o.mode,
            attempts: log,
        }));
        let handles = [event, stop.get()];
        loop {
            // SAFETY: valid handles.
            let w = unsafe { WaitForMultipleObjects(&handles, false, 1000) };
            if w.0 == WAIT_OBJECT_0.0 + 1 {
                break;
            }
            let step = (|| -> windows::core::Result<()> {
                // SAFETY: valid client.
                let pad = unsafe { o.client.GetCurrentPadding()? };
                if pad == 0 && w == WAIT_OBJECT_0 {
                    underruns.fetch_add(1, Relaxed);
                }
                write(&mut gen, o.buffer_frames.saturating_sub(pad))
            })();
            if let Err(e) = step {
                lost.store(true, Relaxed);
                let _ = events.send(lost_event(&e));
                break;
            }
            if w != WAIT_OBJECT_0 && w != WAIT_TIMEOUT {
                let _ = events.send(CaptureEvent::Error(format!("wait failed: {w:?}")));
                break;
            }
        }
        // SAFETY: stopping is always valid.
        unsafe { let _ = o.client.Stop(); }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = ready.send(Err(e));
    }
    // SAFETY: handles owned by this thread.
    unsafe {
        let _ = CloseHandle(event);
        if let Some(h) = mmcss {
            let _ = AvRevertMmThreadCharacteristics(h);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Recorder (probe / analyzer support)
// ---------------------------------------------------------------------------------------------

/// Write stereo f32 planar data as a 32-bit float WAV (same layout as `capture-probe record`).
pub fn write_wav_f32(path: &std::path::Path, rate: u32, l: &[f32], r: &[f32]) -> std::io::Result<()> {
    use std::io::Write;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::SampleFormat;

    fn fmt(ch: u16, f: SampleFormat) -> FormatInfo {
        FormatInfo {
            sample_rate: 48000,
            channels: ch,
            bits_per_sample: (f.bytes_per_sample() * 8) as u16,
            valid_bits: (f.bytes_per_sample() * 8) as u16,
            channel_mask: 0,
            format: f,
        }
    }

    #[test]
    fn hresult_classes() {
        assert_eq!(classify_hresult(AUDCLNT_E_DEVICE_INVALIDATED.0), HrClass::DeviceGone);
        assert_eq!(classify_hresult(AUDCLNT_E_RESOURCES_INVALIDATED.0), HrClass::DeviceGone);
        assert_eq!(classify_hresult(0x8007_0490_u32 as i32), HrClass::DeviceGone);
        assert_eq!(classify_hresult(AUDCLNT_E_DEVICE_IN_USE.0), HrClass::Busy);
        assert_eq!(classify_hresult(0x8000_4005_u32 as i32), HrClass::Other);
        assert!(is_endpoint_missing(&CaptureError::NotFound("x".into())));
        assert!(!is_endpoint_missing(&CaptureError::Thread("x".into())));
    }

    #[test]
    fn polling_interval() {
        assert_eq!(polling_interval_ms(480, 48000), 5);
        assert_eq!(polling_interval_ms(144, 48000), 2); // 3 ms period -> 1.5 -> 2
        assert_eq!(polling_interval_ms(48, 48000), 1);
        assert_eq!(polling_interval_ms(480, 0), 1);
    }

    #[test]
    fn arrivals_regular_vs_paired() {
        let regular: Vec<f64> = (0..200).map(|i| i as f64 * 10_000.0).collect();
        let s = arrival_stats(&regular, 10_000.0).unwrap();
        assert_eq!(s.pattern(), "regular");
        assert!((s.mean_us - 10_000.0).abs() < 1e-6 && s.max_us == s.min_us);
        // pairs: 20 ms cadence, two packets back-to-back
        let mut paired = Vec::new();
        for i in 0..100 {
            paired.push(i as f64 * 20_000.0);
            paired.push(i as f64 * 20_000.0 + 50.0);
        }
        let p = arrival_stats(&paired, 10_000.0).unwrap();
        assert_eq!(p.pattern(), "bursty (back-to-back packets)");
        assert!(p.min_us < 100.0 && p.max_us > 19_000.0);
        assert!(arrival_stats(&[1.0], 10.0).is_none());
    }

    #[test]
    fn arrivals_late_detected() {
        let mut t: Vec<f64> = (0..200).map(|i| i as f64 * 3_000.0).collect();
        for x in t.iter_mut().skip(100) {
            *x += 9_000.0; // one 12 ms hole
        }
        let s = arrival_stats(&t, 3_000.0).unwrap();
        assert_eq!(s.late, 1);
        assert!((s.max_us - 12_000.0).abs() < 1e-6);
    }

    #[test]
    fn fill_silence_and_signal_multichannel() {
        let f = fmt(6, SampleFormat::Float32);
        let mut buf = vec![0xAAu8; 16 * f.block_align()];
        fill_render(&mut buf, 16, &f, None);
        assert!(buf.iter().all(|&b| b == 0));
        let mut g = SignalGen::new(SignalKind::Tone(1000.0), 48000, -50.0);
        fill_render(&mut buf, 16, &f, Some(&mut g));
        // channels 2.. stay zero, channel 0/1 non-zero somewhere, never above -50 dBFS (+margin)
        let mut max = 0.0f32;
        for i in 0..16 {
            for c in 0..6 {
                let o = i * f.block_align() + c * 4;
                let v = f32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
                if c >= 2 {
                    assert_eq!(v, 0.0);
                }
                max = max.max(v.abs());
            }
        }
        assert!(max > 0.0 && max < 0.0033, "{max}");
    }

    #[test]
    fn keepalive_rejects_loud_signal() {
        let r = KeepAlive::start("none", KeepAliveOptions { low_latency: false, signal: Some((SignalKind::Tone(1000.0), -30.0)) });
        assert!(matches!(r, Err(CaptureError::UnsupportedFormat(_))));
    }
}
