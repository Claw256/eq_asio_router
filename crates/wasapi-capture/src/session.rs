//! Event-driven WASAPI capture session on its own MMCSS-boosted MTA thread.

use crate::convert::{convert_to_stereo, FormatInfo, PacketFlags};
use crate::device::{
    activate_client, open_device, parse_format, ComGuard, MixFormat,
};
use crate::monitor::{packet_fingerprint, CorruptionMonitor, CorruptionReason, MonitorConfig};
use crate::{CaptureError, Result};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;
use windows::core::{w, GUID};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IAudioClient3, AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_RESOURCES_INVALIDATED, AUDCLNT_SHAREMODE_EXCLUSIVE,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, CreateEventW, SetEvent,
    WaitForMultipleObjects, AVRT_PRIORITY_CRITICAL,
};

/// Result of the capture thread's `AvSetMmThreadPriority(AVRT_PRIORITY_CRITICAL)` (0 = not run yet,
/// 1 = ok, 2 = failed); read by the engine's hardening report.
pub static MMCSS_CAPTURE_PRIORITY: AtomicU8 = AtomicU8::new(0);

/// Capture mode requested / in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    /// Shared mode, default engine period.
    Shared,
    /// Shared mode via IAudioClient3 with the minimum engine period (falls back to Shared).
    LowLatency,
    /// Exclusive mode (falls back to Shared, with a [`CaptureEvent::Fallback`]).
    ///
    /// DIAGNOSTIC ONLY: measured on the VB-Audio Hi-Fi Cable, the driver delivers duplicated and
    /// missing packets at periods below ~8 ms (30-70 % of the samples wrong at 3-5 ms) and only
    /// 10 ms multiples are clean, so it brings no latency gain. The router engine maps it to Shared.
    Exclusive,
    /// Event-driven shared stream through `IAudioClient3::InitializeSharedAudioStream` with an
    /// explicit period ([`CaptureOptions::period_frames`], default 6 ms). Validated against
    /// `GetSharedModeEnginePeriod`; falls back to [`CaptureMode::Shared`] with a
    /// [`CaptureEvent::Fallback`]. Never go below 5 ms on the VAC lite driver (see
    /// [`CaptureOptions::allow_unsafe_period`]).
    FixedPeriod,
}

impl CaptureMode {
    fn to_u8(self) -> u8 {
        match self {
            CaptureMode::Shared => 1,
            CaptureMode::LowLatency => 2,
            CaptureMode::Exclusive => 3,
            CaptureMode::FixedPeriod => 4,
        }
    }
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(CaptureMode::Shared),
            2 => Some(CaptureMode::LowLatency),
            3 => Some(CaptureMode::Exclusive),
            4 => Some(CaptureMode::FixedPeriod),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            CaptureMode::Shared => "shared",
            CaptureMode::LowLatency => "low-latency (IAudioClient3)",
            CaptureMode::Exclusive => "exclusive",
            CaptureMode::FixedPeriod => "fixed-period (IAudioClient3)",
        }
    }
}

/// What the corruption monitor does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MonitorPolicy {
    /// `FixedPeriod`: [`StopOnTrip`](Self::StopOnTrip); every other mode: [`Off`](Self::Off).
    #[default]
    Auto,
    Off,
    /// Count and emit [`CaptureEvent::Corruption`] but keep running (diagnostics).
    ReportOnly,
    /// Emit [`CaptureEvent::Corruption`] and stop the capture thread.
    StopOnTrip,
}

/// Frames of `ms` milliseconds at `rate` (rounded to nearest).
pub fn frames_for_ms(ms: f64, rate: u32) -> u32 {
    (ms * f64::from(rate) / 1000.0).round().max(0.0) as u32
}

/// Safe floor of the capture period in milliseconds. 5 ms is the smallest period measured
/// sample-accurate on VAC lite; 4.5 ms and below are damaged there.
pub const SAFE_PERIOD_FLOOR_MS: f64 = 5.0;

/// Period used by [`CaptureMode::FixedPeriod`] when no frame count is given. Deliberately above
/// the 5 ms floor: on this machine VAC lite at 240 frames (5 ms) showed an isolated duplicated block
/// in ~40 % of 12 s runs, while 264 / 288 / 360 frames were sample-accurate in 10 of 10 runs.
pub const DEFAULT_FIXED_PERIOD_MS: f64 = 6.0;

/// An endpoint whose IAudioClient3 minimum period is below this (ms) is never opened at that
/// minimum unless [`CaptureOptions::force_min_period`] is set. VAC lite reports 48 frames (1 ms)
/// but corrupts the audio (repeated blocks) at every period below 5 ms, so the "minimum" it
/// advertises is not usable for capture.
pub const MIN_PERIOD_GUARD_MS: f64 = 2.5;

/// Options for [`CaptureSession::start_with_options`].
#[derive(Clone, Copy, Debug)]
pub struct CaptureOptions {
    pub mode: CaptureMode,
    /// Requested period in frames for [`CaptureMode::FixedPeriod`] (`None` = [`DEFAULT_FIXED_PERIOD_MS`] (6 ms) rounded up to
    /// the engine's fundamental period). Ignored by the other modes.
    pub period_frames: Option<u32>,
    /// Preferred stream rate (only exclusive mode can choose a rate).
    pub preferred_rate: Option<u32>,
    /// Lab tests only: accept a period below [`SAFE_PERIOD_FLOOR_MS`].
    pub allow_unsafe_period: bool,
    /// Lab tests only: allow the `LowLatency` minimum period on an endpoint whose minimum is below
    /// [`MIN_PERIOD_GUARD_MS`].
    pub force_min_period: bool,
    pub monitor: MonitorPolicy,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            mode: CaptureMode::Shared,
            period_frames: None,
            preferred_rate: None,
            allow_unsafe_period: false,
            force_min_period: false,
            monitor: MonitorPolicy::Auto,
        }
    }
}

impl CaptureOptions {
    pub fn new(mode: CaptureMode) -> Self {
        Self { mode, ..Self::default() }
    }
    /// `FixedPeriod` with an explicit period (`None` = 6 ms default, `DEFAULT_FIXED_PERIOD_MS`).
    pub fn fixed_period(period_frames: Option<u32>) -> Self {
        Self { mode: CaptureMode::FixedPeriod, period_frames, ..Self::default() }
    }
    /// The policy actually applied (`Auto` resolved).
    pub fn effective_monitor(&self) -> MonitorPolicy {
        match (self.monitor, self.mode) {
            (MonitorPolicy::Auto, CaptureMode::FixedPeriod) => MonitorPolicy::StopOnTrip,
            (MonitorPolicy::Auto, _) => MonitorPolicy::Off,
            (p, _) => p,
        }
    }
}

/// Pure planner for a fixed-period request: returns the period to hand to
/// `InitializeSharedAudioStream` or the reason the request must be refused (-> Shared fallback).
pub fn plan_fixed_period(
    requested: Option<u32>,
    rate: u32,
    periods: &crate::device::EnginePeriods,
    allow_unsafe: bool,
) -> std::result::Result<u32, String> {
    let fund = periods.fundamental_frames.max(1);
    let floor = frames_for_ms(SAFE_PERIOD_FLOOR_MS, rate);
    let frames = match requested {
        Some(f) => f,
        None => frames_for_ms(DEFAULT_FIXED_PERIOD_MS, rate).max(floor).div_ceil(fund) * fund,
    };
    if frames < floor && !allow_unsafe {
        return Err(format!(
            "requested period {frames} frames ({:.2} ms) is below the safe floor {floor} frames ({SAFE_PERIOD_FLOOR_MS} ms)",
            frames as f64 * 1000.0 / rate.max(1) as f64
        ));
    }
    if frames < periods.min_frames || frames > periods.max_frames {
        return Err(format!(
            "requested period {frames} frames is outside the engine range {}..{}",
            periods.min_frames, periods.max_frames
        ));
    }
    if frames % fund != 0 {
        return Err(format!("requested period {frames} frames is not a multiple of the fundamental period {fund}"));
    }
    Ok(frames)
}

/// Receiver of captured audio. Called on the capture thread; must not block.
pub trait FrameSink: Send {
    fn on_frames(&mut self, left: &[f32], right: &[f32]);
    /// Called once per WASAPI packet, before its frames are delivered (the packet may be
    /// delivered in several `on_frames` chunks). Default: ignored.
    fn on_packet(&mut self, _frames: u32, _flags: PacketFlags) {}
}

/// Events delivered through [`CaptureSession::events`].
#[derive(Clone, Debug)]
pub enum CaptureEvent {
    Started { format: FormatInfo, period_frames: u32, mode: CaptureMode },
    /// The requested mode was unavailable; running in `using` instead.
    Fallback { requested: CaptureMode, using: CaptureMode, reason: String },
    /// Device removed / format changed / resources invalidated: session stopped, re-open it.
    DeviceLost,
    /// Unrecoverable runtime error: session stopped.
    Error(String),
    /// The corruption monitor tripped (damaged data: faster than real time, repeated blocks,
    /// packet anomalies). Unless the session runs with [`MonitorPolicy::ReportOnly`] it has
    /// stopped itself; re-open it in [`CaptureMode::Shared`].
    Corruption(CorruptionReason),
}

/// Live counters (all relaxed atomics).
#[derive(Default)]
pub struct CaptureStats {
    pub frames_captured: AtomicU64,
    pub packets: AtomicU64,
    pub discontinuities: AtomicU64,
    pub silent_packets: AtomicU64,
    pub timestamp_errors: AtomicU64,
    pub wait_timeouts: AtomicU64,
    pub period_frames: AtomicU32,
    pub sample_rate: AtomicU32,
    pub channels: AtomicU32,
    mode: AtomicU8,
    /// Corruption monitor trips (0 when the monitor is off).
    pub corruption_trips: AtomicU64,
    /// Identical consecutive packets seen by the monitor (harmless below the trip threshold).
    pub repeated_blocks: AtomicU64,
    /// Largest "ahead of real time" excess seen by the monitor, in frames.
    pub max_ahead_frames: AtomicU32,
    /// Shared-mode engine period of the endpoint right after the stream was initialised
    /// (`GetCurrentSharedModeEnginePeriod`, 0 = unknown). Another client (a render keep-alive)
    /// can lower it below the stream's own `period_frames`.
    pub engine_period_frames: AtomicU32,
    /// 0 = monitor off, 1 = report only, 2 = stop on trip.
    pub monitor_policy: AtomicU8,
}

/// Plain copy of [`CaptureStats`].
#[derive(Clone, Copy, Debug)]
pub struct StatsSnapshot {
    pub frames_captured: u64,
    pub packets: u64,
    pub discontinuities: u64,
    pub silent_packets: u64,
    pub timestamp_errors: u64,
    pub wait_timeouts: u64,
    pub period_frames: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub mode_in_use: Option<CaptureMode>,
}

impl CaptureStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            frames_captured: self.frames_captured.load(Relaxed),
            packets: self.packets.load(Relaxed),
            discontinuities: self.discontinuities.load(Relaxed),
            silent_packets: self.silent_packets.load(Relaxed),
            timestamp_errors: self.timestamp_errors.load(Relaxed),
            wait_timeouts: self.wait_timeouts.load(Relaxed),
            period_frames: self.period_frames.load(Relaxed),
            sample_rate: self.sample_rate.load(Relaxed),
            channels: self.channels.load(Relaxed),
            mode_in_use: CaptureMode::from_u8(self.mode.load(Relaxed)),
        }
    }
    pub fn mode_in_use(&self) -> Option<CaptureMode> {
        CaptureMode::from_u8(self.mode.load(Relaxed))
    }
    /// Account one packet (the first packet's discontinuity flag is expected and ignored).
    pub fn record_packet(&self, frames: u32, mut flags: PacketFlags) {
        if self.packets.load(Relaxed) == 0 {
            flags.data_discontinuity = false;
        }
        self.packets.fetch_add(1, Relaxed);
        self.frames_captured.fetch_add(frames as u64, Relaxed);
        self.discontinuities.fetch_add(flags.data_discontinuity as u64, Relaxed);
        self.silent_packets.fetch_add(flags.silent as u64, Relaxed);
        self.timestamp_errors.fetch_add(flags.timestamp_error as u64, Relaxed);
    }
}

/// Raw handle that may cross threads (event handles are process-wide kernel objects).
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

/// A running capture. Dropping it stops and joins the thread.
pub struct CaptureSession {
    stats: Arc<CaptureStats>,
    events: Receiver<CaptureEvent>,
    stop: SendHandle,
    thread: Option<JoinHandle<()>>,
    format: FormatInfo,
    period_frames: u32,
    mode: CaptureMode,
}

impl CaptureSession {
    /// Open `device_id` and start capturing into `sink`. Blocks until the stream is
    /// running (or failed), so errors are returned synchronously.
    pub fn start(device_id: &str, mode: CaptureMode, sink: Box<dyn FrameSink>) -> Result<Self> {
        Self::start_with_rate(device_id, mode, None, sink)
    }

    /// Like [`start`](Self::start), with the preferred stream rate (the ASIO master's rate).
    /// Only exclusive mode can choose a rate; shared mode always uses the endpoint mix format.
    pub fn start_with_rate(
        device_id: &str,
        mode: CaptureMode,
        preferred_rate: Option<u32>,
        sink: Box<dyn FrameSink>,
    ) -> Result<Self> {
        Self::start_with_options(device_id, CaptureOptions { preferred_rate, ..CaptureOptions::new(mode) }, sink)
    }

    /// Full-featured start (fixed period, corruption monitor, guards).
    pub fn start_with_options(device_id: &str, options: CaptureOptions, sink: Box<dyn FrameSink>) -> Result<Self> {
        // SAFETY: plain event creation; closed in `stop`/Drop.
        let stop = unsafe { CreateEventW(None, true, false, None) }
            .map_err(CaptureError::com("CreateEventW(stop)"))?;
        let stop_h = SendHandle::new(stop);
        let stats = Arc::new(CaptureStats::default());
        let (ev_tx, ev_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(FormatInfo, u32, CaptureMode)>>(1);
        let id = device_id.to_string();
        let st = stats.clone();
        let thread = std::thread::Builder::new()
            .name("wasapi-capture".into())
            .spawn(move || thread_main(id, options, sink, st, stop_h, ev_tx, ready_tx))
            .map_err(|e| CaptureError::Thread(e.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok((format, period_frames, mode))) => Ok(Self {
                stats,
                events: ev_rx,
                stop: stop_h,
                thread: Some(thread),
                format,
                period_frames,
                mode,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                // SAFETY: handle created above and no longer used by the thread.
                unsafe { let _ = CloseHandle(stop); }
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                // SAFETY: as above.
                unsafe { let _ = CloseHandle(stop); }
                Err(CaptureError::Thread("capture thread exited during start".into()))
            }
        }
    }

    pub fn stats(&self) -> &Arc<CaptureStats> {
        &self.stats
    }
    pub fn events(&self) -> &Receiver<CaptureEvent> {
        &self.events
    }
    /// Actual stream format (sample rate etc.).
    pub fn format(&self) -> FormatInfo {
        self.format
    }
    /// Actual stream sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.format.sample_rate
    }
    pub fn period_frames(&self) -> u32 {
        self.period_frames
    }
    pub fn mode_in_use(&self) -> CaptureMode {
        self.mode
    }
    /// True while the capture thread is alive.
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

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Opened {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    fmt: FormatInfo,
    buffer_frames: u32,
    period_frames: u32,
    /// Endpoint engine period right after init (0 = unknown).
    engine_period_frames: u32,
    mode: CaptureMode,
    fallback: Option<(CaptureMode, String)>,
}

const EVENTCB: u32 = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;

/// How a shared stream is initialised.
#[derive(Clone, Copy)]
enum SharedInit {
    /// Plain `IAudioClient::Initialize` (engine default period).
    Default,
    /// IAudioClient3 at the endpoint minimum; refused on endpoints whose minimum is < 2.5 ms
    /// unless `force`.
    MinPeriod { force: bool },
    /// IAudioClient3 at an explicit period.
    Fixed { frames: Option<u32>, allow_unsafe: bool },
}

fn init_shared(dev: &windows::Win32::Media::Audio::IMMDevice, event: HANDLE, how: SharedInit) -> Result<Opened> {
    let client = activate_client(dev)?;
    let mix = MixFormat::from_client(&client)?;
    let fmt = mix.info()?;
    let (mode, frames) = match how {
        SharedInit::Default => {
            let mut def = 0i64;
            // SAFETY: valid pointers; shared mode with event callback and engine-chosen buffer.
            unsafe {
                client
                    .GetDevicePeriod(Some(&mut def), None)
                    .map_err(CaptureError::com("GetDevicePeriod"))?;
                client
                    .Initialize(AUDCLNT_SHAREMODE_SHARED, EVENTCB, 0, 0, mix.ptr(), None)
                    .map_err(CaptureError::com("IAudioClient::Initialize(shared)"))?;
            }
            let period_frames = (def as f64 * fmt.sample_rate as f64 / 1e7).round() as u32;
            let mut o = finish_open(client, fmt, period_frames, CaptureMode::Shared, event, None)?;
            o.engine_period_frames = 0;
            return Ok(o);
        }
        SharedInit::MinPeriod { .. } => (CaptureMode::LowLatency, None),
        SharedInit::Fixed { frames, .. } => (CaptureMode::FixedPeriod, frames),
    };
    // SAFETY: IAudioClient3 on the same endpoint; mix pointer valid for the calls.
    let (c, period_frames, engine) = unsafe {
        let c3 = dev
            .Activate::<IAudioClient3>(windows::Win32::System::Com::CLSCTX_ALL, None)
            .map_err(CaptureError::com("Activate(IAudioClient3)"))?;
        let (mut d, mut f, mut mn, mut mx) = (0u32, 0u32, 0u32, 0u32);
        c3.GetSharedModeEnginePeriod(mix.ptr(), &mut d, &mut f, &mut mn, &mut mx)
            .map_err(CaptureError::com("GetSharedModeEnginePeriod"))?;
        let periods = crate::device::EnginePeriods {
            default_frames: d,
            fundamental_frames: f,
            min_frames: mn,
            max_frames: mx,
        };
        let period = match how {
            SharedInit::MinPeriod { force } => {
                if !force && (mn as f64) < frames_for_ms(MIN_PERIOD_GUARD_MS, fmt.sample_rate) as f64 {
                    return Err(CaptureError::Refused(format!(
                        "IAudioClient3 minimum period {mn} frames ({:.2} ms) is below {MIN_PERIOD_GUARD_MS} ms: such endpoints (VAC lite: 48 frames) corrupt captured audio at that period; use FixedPeriod >= 5 ms",
                        mn as f64 * 1000.0 / fmt.sample_rate as f64
                    )));
                }
                mn
            }
            SharedInit::Fixed { allow_unsafe, .. } => {
                plan_fixed_period(frames, fmt.sample_rate, &periods, allow_unsafe).map_err(CaptureError::Refused)?
            }
            SharedInit::Default => unreachable!(),
        };
        c3.InitializeSharedAudioStream(EVENTCB, period, mix.ptr(), None)
            .map_err(CaptureError::com("InitializeSharedAudioStream"))?;
        // Report what the engine runs at now (another client may already have lowered it).
        let mut pf: *mut WAVEFORMATEX = std::ptr::null_mut();
        let mut cur = 0u32;
        if c3.GetCurrentSharedModeEnginePeriod(&mut pf, &mut cur).is_err() {
            cur = 0;
        }
        if !pf.is_null() {
            windows::Win32::System::Com::CoTaskMemFree(Some(pf as *const _));
        }
        // keep the initialised client: QueryInterface back to IAudioClient
        use windows::core::Interface;
        let c: IAudioClient = c3.cast().map_err(CaptureError::com("cast IAudioClient3"))?;
        (c, period, cur)
    };
    let mut o = finish_open(c, fmt, period_frames, mode, event, None)?;
    o.engine_period_frames = engine;
    Ok(o)
}

fn finish_open(
    client: IAudioClient,
    fmt: FormatInfo,
    period_frames: u32,
    mode: CaptureMode,
    event: HANDLE,
    fallback: Option<(CaptureMode, String)>,
) -> Result<Opened> {
    // SAFETY: client is initialised for event callback; event handle is valid.
    unsafe {
        client.SetEventHandle(event).map_err(CaptureError::com("SetEventHandle"))?;
        let buffer_frames = client.GetBufferSize().map_err(CaptureError::com("GetBufferSize"))?;
        let capture: IAudioCaptureClient =
            client.GetService().map_err(CaptureError::com("GetService(IAudioCaptureClient)"))?;
        Ok(Opened { client, capture, fmt, buffer_frames, period_frames, engine_period_frames: 0, mode, fallback })
    }
}

pub(crate) fn make_ext(rate: u32, ch: u16, container_bits: u16, valid: u16, float: bool) -> WAVEFORMATEXTENSIBLE {
    let block = ch * container_bits / 8;
    let sub = if float {
        GUID::from_u128(crate::convert::SUBTYPE_FLOAT)
    } else {
        GUID::from_u128(crate::convert::SUBTYPE_PCM)
    };
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: crate::convert::WAVE_FORMAT_EXTENSIBLE,
            nChannels: ch,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block as u32,
            nBlockAlign: block,
            wBitsPerSample: container_bits,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: valid },
        dwChannelMask: if ch == 2 { 3 } else { 0 },
        SubFormat: sub,
    }
}

/// Exclusive-mode rate candidates. The Hi-Fi Cable driver does NOT convert sample rates: an
/// exclusive stream whose rate differs from the endpoint's device format is delivered
/// unconverted (measured: 44.1 kHz data in a 48 kHz stream = tone x1.0888, with dropped and
/// duplicated blocks), so only the device (mix) rate is ever offered; `preferred` is ignored.
pub fn exclusive_rate_candidates(_preferred: Option<u32>, mix_rate: u32) -> Vec<u32> {
    if mix_rate > 0 { vec![mix_rate] } else { Vec::new() }
}

fn init_exclusive(
    dev: &windows::Win32::Media::Audio::IMMDevice,
    event: HANDLE,
    preferred_rate: Option<u32>,
) -> Result<Opened> {
    let probe = activate_client(dev)?;
    let mix = MixFormat::from_client(&probe)?;
    let mix_fmt = mix.info()?;
    let mut min = 0i64;
    // SAFETY: valid out pointer.
    unsafe { probe.GetDevicePeriod(None, Some(&mut min)) }.map_err(CaptureError::com("GetDevicePeriod"))?;

    // Candidates: per rate (preferred rate first), the device mix format when its rate matches,
    // then 24-in-32, packed 24, 32-bit, 16-bit PCM and float32.
    let mut cands: Vec<(WAVEFORMATEXTENSIBLE, bool)> = Vec::new();
    for r in exclusive_rate_candidates(preferred_rate, mix_fmt.sample_rate) {
        if r == mix_fmt.sample_rate {
            cands.push((make_ext(r, 2, 32, 24, false), true)); // placeholder: use mix.ptr()
        }
        for (c, v) in [(32u16, 24u16), (24, 24), (32, 32), (16, 16)] {
            cands.push((make_ext(r, 2, c, v, false), false));
        }
        cands.push((make_ext(r, 2, 32, 32, true), false));
    }
    let mut last_err = CaptureError::UnsupportedFormat("no exclusive format accepted".into());
    for (cand, use_mix) in cands {
        let pf = &cand as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX;
        let client = activate_client(dev)?;
        let ptr = if use_mix { mix.ptr() } else { pf };
        // SAFETY: pointer is either the COM mix format or the live local `cand`.
        let ok = unsafe { client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, ptr, None) }
            == windows::core::HRESULT(0);
        if !ok {
            continue;
        }
        // SAFETY: ptr valid (see above).
        let fmt = unsafe { parse_format(ptr)? };
        // SAFETY: exclusive event-driven init; period == buffer duration.
        let r = unsafe {
            client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, EVENTCB, min, min, ptr, None)
        };
        let client = match r {
            Ok(()) => client,
            Err(e) if e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED => {
                // SAFETY: documented recovery: query aligned size, recreate client, retry.
                unsafe {
                    let frames = client.GetBufferSize().map_err(CaptureError::com("GetBufferSize"))?;
                    let hns = (1e7 / fmt.sample_rate as f64 * frames as f64 + 0.5) as i64;
                    let c2 = activate_client(dev)?;
                    match c2.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, EVENTCB, hns, hns, ptr, None) {
                        Ok(()) => c2,
                        Err(e) => {
                            last_err = CaptureError::Com { context: "Initialize(exclusive, realigned)", source: e };
                            continue;
                        }
                    }
                }
            }
            Err(e) => {
                last_err = CaptureError::Com { context: "Initialize(exclusive)", source: e };
                continue;
            }
        };
        let mut o = finish_open(client, fmt, 0, CaptureMode::Exclusive, event, None)?;
        o.period_frames = o.buffer_frames;
        return Ok(o);
    }
    Err(last_err)
}

fn open_stream(device_id: &str, options: &CaptureOptions, event: HANDLE) -> Result<Opened> {
    let dev = open_device(device_id)?;
    let fallback_shared = |requested: CaptureMode, e: CaptureError| -> Result<Opened> {
        let mut o = init_shared(&dev, event, SharedInit::Default)?;
        o.fallback = Some((requested, e.to_string()));
        Ok(o)
    };
    match options.mode {
        CaptureMode::Shared => init_shared(&dev, event, SharedInit::Default),
        CaptureMode::LowLatency => {
            match init_shared(&dev, event, SharedInit::MinPeriod { force: options.force_min_period }) {
                Ok(o) => Ok(o),
                Err(e) => fallback_shared(CaptureMode::LowLatency, e),
            }
        }
        CaptureMode::FixedPeriod => {
            let how = SharedInit::Fixed { frames: options.period_frames, allow_unsafe: options.allow_unsafe_period };
            match init_shared(&dev, event, how) {
                Ok(o) => Ok(o),
                Err(e) => fallback_shared(CaptureMode::FixedPeriod, e),
            }
        }
        CaptureMode::Exclusive => match init_exclusive(&dev, event, options.preferred_rate) {
            Ok(o) => Ok(o),
            Err(e) => fallback_shared(CaptureMode::Exclusive, e),
        },
    }
}

fn is_lost(e: &windows::core::Error) -> bool {
    let c = e.code();
    c == AUDCLNT_E_DEVICE_INVALIDATED || c == AUDCLNT_E_RESOURCES_INVALIDATED
}

#[allow(clippy::too_many_arguments)]
fn thread_main(
    device_id: String,
    options: CaptureOptions,
    mut sink: Box<dyn FrameSink>,
    stats: Arc<CaptureStats>,
    stop: SendHandle,
    events: Sender<CaptureEvent>,
    ready: mpsc::SyncSender<Result<(FormatInfo, u32, CaptureMode)>>,
) {
    let _com = match ComGuard::init() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // MMCSS "Pro Audio" for this thread; reverted on exit.
    let mut task_index = 0u32;
    // SAFETY: valid NUL-terminated task name and out pointer.
    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }.ok();
    if let Some(h) = mmcss {
        // SAFETY: `h` is the handle just returned for this thread.
        let ok = unsafe { AvSetMmThreadPriority(h, AVRT_PRIORITY_CRITICAL) }.is_ok();
        MMCSS_CAPTURE_PRIORITY.store(if ok { 1 } else { 2 }, Relaxed);
    } else {
        MMCSS_CAPTURE_PRIORITY.store(2, Relaxed);
    }

    // SAFETY: auto-reset event signalled by WASAPI; closed at the end of this function.
    let event = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(CaptureError::Com { context: "CreateEventW", source: e }));
            return;
        }
    };

    let result = (|| -> Result<()> {
        let o = open_stream(&device_id, &options, event)?;
        stats.sample_rate.store(o.fmt.sample_rate, Relaxed);
        stats.engine_period_frames.store(o.engine_period_frames, Relaxed);
        let policy = if o.fallback.is_some() && options.monitor == MonitorPolicy::Auto {
            // fell back to plain Shared: the engine default period is not suspect
            MonitorPolicy::Off
        } else {
            CaptureOptions { mode: o.mode, ..options }.effective_monitor()
        };
        stats.monitor_policy.store(
            match policy {
                MonitorPolicy::Off | MonitorPolicy::Auto => 0,
                MonitorPolicy::ReportOnly => 1,
                MonitorPolicy::StopOnTrip => 2,
            },
            Relaxed,
        );
        let mut monitor = (policy != MonitorPolicy::Off && policy != MonitorPolicy::Auto).then(|| MonitorRt {
            mon: CorruptionMonitor::new(MonitorConfig::new(o.fmt.sample_rate, o.period_frames)),
            t0: Instant::now(),
            stop_on_trip: policy == MonitorPolicy::StopOnTrip,
        });
        stats.channels.store(o.fmt.channels as u32, Relaxed);
        stats.period_frames.store(o.period_frames, Relaxed);
        stats.mode.store(o.mode.to_u8(), Relaxed);
        if let Some((req, reason)) = &o.fallback {
            let _ = events.send(CaptureEvent::Fallback { requested: *req, using: o.mode, reason: reason.clone() });
        }
        let _ = events.send(CaptureEvent::Started { format: o.fmt, period_frames: o.period_frames, mode: o.mode });
        let _ = ready.send(Ok((o.fmt, o.period_frames, o.mode)));
        run_loop(&o, &mut *sink, &stats, event, stop.get(), &events, monitor.as_mut());
        // SAFETY: stream started in run_loop; stopping is always valid.
        unsafe { let _ = o.client.Stop(); }
        Ok(())
    })();

    if let Err(e) = result {
        // Failure before `ready` was sent -> report to start(). (If ready was already
        // consumed the send simply fails.)
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

fn run_loop(
    o: &Opened,
    sink: &mut dyn FrameSink,
    stats: &CaptureStats,
    event: HANDLE,
    stop: HANDLE,
    events: &Sender<CaptureEvent>,
    mut monitor: Option<&mut MonitorRt>,
) {
    // Preallocated scratch: one buffer's worth of frames (packets never exceed it;
    // larger packets are processed in chunks anyway).
    let cap = (o.buffer_frames as usize).max(64);
    let mut left = vec![0.0f32; cap];
    let mut right = vec![0.0f32; cap];
    let handles = [event, stop];

    // SAFETY: client initialised; Start begins event signalling.
    if let Err(e) = unsafe { o.client.Start() } {
        let _ = events.send(CaptureEvent::Error(format!("Start: {e}")));
        return;
    }

    loop {
        // SAFETY: both handles are valid for the lifetime of the loop.
        let w = unsafe { WaitForMultipleObjects(&handles, false, 1000) };
        if w.0 == WAIT_OBJECT_0.0 + 1 {
            return; // stop requested
        }
        if w == WAIT_TIMEOUT {
            stats.wait_timeouts.fetch_add(1, Relaxed);
            continue;
        }
        if w != WAIT_OBJECT_0 {
            let _ = events.send(CaptureEvent::Error(format!("wait failed: {:?}", w)));
            return;
        }
        match drain(o, sink, stats, &mut left, &mut right, monitor.as_deref_mut()) {
            Ok(None) => {}
            Ok(Some(reason)) => {
                let stop_now = monitor.as_ref().is_some_and(|m| m.stop_on_trip);
                let _ = events.send(CaptureEvent::Corruption(reason));
                if stop_now {
                    return;
                }
            }
            Err(e) => {
                let ev = if is_lost(&e) {
                    CaptureEvent::DeviceLost
                } else {
                    CaptureEvent::Error(e.to_string())
                };
                let _ = events.send(ev);
                return;
            }
        }
    }
}

fn drain(
    o: &Opened,
    sink: &mut dyn FrameSink,
    stats: &CaptureStats,
    left: &mut [f32],
    right: &mut [f32],
    mut monitor: Option<&mut MonitorRt>,
) -> windows::core::Result<Option<CorruptionReason>> {
    let block = o.fmt.block_align();
    let mut tripped: Option<CorruptionReason> = None;
    loop {
        // SAFETY: valid capture client.
        let next = unsafe { o.capture.GetNextPacketSize()? };
        if next == 0 {
            return Ok(tripped);
        }
        let mut data: *mut u8 = std::ptr::null_mut();
        let (mut frames, mut flags) = (0u32, 0u32);
        // SAFETY: out pointers are locals; buffer is valid until ReleaseBuffer.
        unsafe { o.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };
        let pf = PacketFlags::from_raw(flags);
        stats.record_packet(frames, pf);
        sink.on_packet(frames, pf);
        let n = frames as usize;
        if let Some(m) = monitor.as_deref_mut() {
            let hash = if pf.silent || data.is_null() || n == 0 {
                None
            } else {
                // SAFETY: WASAPI guarantees `frames * block_align` readable bytes at `data`.
                packet_fingerprint(unsafe { std::slice::from_raw_parts(data, n * block) }, block)
            };
            let t_us = m.t0.elapsed().as_micros() as u64;
            let reason = m.mon.on_packet(t_us, frames, hash);
            stats.max_ahead_frames.fetch_max(m.mon.max_ahead_frames().clamp(0, u32::MAX as i64) as u32, Relaxed);
            stats.repeated_blocks.store(m.mon.repeated_blocks_total(), Relaxed);
            if let Some(r) = reason {
                stats.corruption_trips.fetch_add(1, Relaxed);
                tripped.get_or_insert(r);
            }
        }
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
        // SAFETY: releases exactly the frames obtained by GetBuffer.
        unsafe { o.capture.ReleaseBuffer(frames)? };
        if tripped.is_some() && monitor.as_ref().is_some_and(|m| m.stop_on_trip) {
            return Ok(tripped);
        }
    }
}

/// Monitor state owned by the capture thread.
struct MonitorRt {
    mon: CorruptionMonitor,
    t0: Instant,
    stop_on_trip: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_ignore_first_discontinuity() {
        let s = CaptureStats::default();
        s.record_packet(480, PacketFlags::from_raw(1));
        s.record_packet(480, PacketFlags::from_raw(1));
        s.record_packet(480, PacketFlags::from_raw(2));
        let snap = s.snapshot();
        assert_eq!(snap.packets, 3);
        assert_eq!(snap.frames_captured, 1440);
        assert_eq!(snap.discontinuities, 1);
        assert_eq!(snap.silent_packets, 1);
    }

    #[test]
    fn rate_candidates_only_device_rate() {
        // never a rate different from the device format: the driver does no SRC
        assert_eq!(exclusive_rate_candidates(Some(48000), 44100), vec![44100]);
        assert_eq!(exclusive_rate_candidates(Some(96000), 48000), vec![48000]);
        assert_eq!(exclusive_rate_candidates(None, 44100), vec![44100]);
        assert!(exclusive_rate_candidates(None, 0).is_empty());
    }

    fn periods() -> crate::device::EnginePeriods {
        crate::device::EnginePeriods { default_frames: 480, fundamental_frames: 48, min_frames: 48, max_frames: 480 }
    }

    #[test]
    fn plan_accepts_240_and_default() {
        assert_eq!(plan_fixed_period(Some(240), 48000, &periods(), false), Ok(240));
        assert_eq!(plan_fixed_period(None, 48000, &periods(), false), Ok(288));
        assert_eq!(plan_fixed_period(Some(480), 48000, &periods(), false), Ok(480));
    }

    #[test]
    fn plan_refuses_below_floor_unless_unsafe() {
        for f in [48u32, 144, 192] {
            assert!(plan_fixed_period(Some(f), 48000, &periods(), false).unwrap_err().contains("safe floor"));
            assert_eq!(plan_fixed_period(Some(f), 48000, &periods(), true), Ok(f));
        }
    }

    #[test]
    fn plan_validates_range_and_alignment() {
        assert!(plan_fixed_period(Some(250), 48000, &periods(), true).unwrap_err().contains("multiple"));
        assert!(plan_fixed_period(Some(960), 48000, &periods(), false).unwrap_err().contains("outside"));
        assert!(plan_fixed_period(Some(24), 48000, &periods(), true).unwrap_err().contains("outside"));
        // default period at 44.1 kHz rounds the 5 ms floor (221) up to the fundamental
        let p = crate::device::EnginePeriods { default_frames: 441, fundamental_frames: 441, min_frames: 441, max_frames: 4410 };
        assert_eq!(plan_fixed_period(None, 44100, &p, false), Ok(441));
    }

    #[test]
    fn frames_for_ms_rounds() {
        assert_eq!(frames_for_ms(5.0, 48000), 240);
        assert_eq!(frames_for_ms(2.5, 48000), 120);
        assert_eq!(frames_for_ms(10.0, 44100), 441);
    }

    #[test]
    fn monitor_policy_resolution() {
        assert_eq!(CaptureOptions::fixed_period(None).effective_monitor(), MonitorPolicy::StopOnTrip);
        assert_eq!(CaptureOptions::new(CaptureMode::Shared).effective_monitor(), MonitorPolicy::Off);
        let o = CaptureOptions { monitor: MonitorPolicy::ReportOnly, ..CaptureOptions::new(CaptureMode::Shared) };
        assert_eq!(o.effective_monitor(), MonitorPolicy::ReportOnly);
    }

    #[test]
    fn mode_roundtrip() {
        for m in [CaptureMode::Shared, CaptureMode::LowLatency, CaptureMode::Exclusive, CaptureMode::FixedPeriod] {
            assert_eq!(CaptureMode::from_u8(m.to_u8()), Some(m));
        }
        assert_eq!(CaptureMode::from_u8(0), None);
    }
}
