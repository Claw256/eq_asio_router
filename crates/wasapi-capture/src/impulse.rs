//! Quiet click / chirp injection into a virtual cable's playback endpoint, with QPC timestamps.
//!
//! Used by `router-cli latency`: a short windowed click is rendered (WASAPI shared mode,
//! event driven, like a normal application) into the cable's render endpoint and the exact QPC
//! instant of the frame submission is recorded, together with an estimate of the instant the
//! click leaves the render client (`IAudioClock::GetPosition` + QPC, cross-checked with
//! `GetCurrentPadding` + `GetStreamLatency`).
//!
//! * The pure part ([`click_samples`], [`ImpulseTrain`], [`onset_index`], [`validate_peak_dbfs`])
//!   has no COM and is unit-tested.
//! * Safety: a click louder than [`MAX_IMPULSE_DBFS`] (-35 dBFS peak) is refused, and the sink must be
//!   a known virtual cable or a VBMatrix render endpoint ([`is_sink_allowed"]) unless the caller passes
//!   an explicit allow-list entry. The test signal is silence apart from the click.

use crate::convert::{write_sample, FormatInfo};
use crate::device::{activate_client, find_device, open_device, ComGuard, Flow, MixFormat};
use crate::keepalive::is_virtual_cable_name;
use crate::loopback::current_engine_period;
use crate::{CaptureError, Result};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::core::Interface;
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioClient3, IAudioClock, IAudioRenderClient, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject,
};

/// Loudest click peak ever rendered (dBFS). Anything above is refused.
pub const MAX_IMPULSE_DBFS: f32 = -35.0;
/// Default click peak (dBFS).
pub const DEFAULT_IMPULSE_DBFS: f32 = -40.0;
/// Quietest click accepted (below this the detection thresholds of the measurement are meaningless).
pub const MIN_IMPULSE_DBFS: f32 = -80.0;

// The pure signal maths lives in `router-core` (shared with `router-engine`, which must not depend on
// this crate); re-exported here so the existing `crate::impulse::*` paths keep working.
pub use router_core::click::{click_samples, onset_index, ImpulseShape};

/// `QueryPerformanceCounter` ticks (the same clock as the engine's callbacks).
pub fn qpc_ticks() -> i64 {
    let mut t = 0i64;
    // SAFETY: valid out pointer; cannot fail on supported Windows versions.
    unsafe {
        let _ = QueryPerformanceCounter(&mut t);
    }
    t
}

/// `QueryPerformanceFrequency` in Hz (never 0).
pub fn qpc_hz() -> i64 {
    let mut f = 0i64;
    // SAFETY: valid out pointer.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut f);
    }
    f.max(1)
}

// ------------------------------------------------------------------------------------
// Pure signal part (see the `router_core::click` re-export above)
// ------------------------------------------------------------------------------------

/// Checks a requested peak level: finite, at most [`MAX_IMPULSE_DBFS`], at least [`MIN_IMPULSE_DBFS`].
pub fn validate_peak_dbfs(dbfs: f32) -> std::result::Result<(), String> {
    if !dbfs.is_finite() {
        return Err("impulse level must be a number".into());
    }
    if dbfs > MAX_IMPULSE_DBFS {
        return Err(format!("impulse level {dbfs} dBFS is above the {MAX_IMPULSE_DBFS} dBFS safety limit (headphones are connected)"));
    }
    if dbfs < MIN_IMPULSE_DBFS {
        return Err(format!("impulse level {dbfs} dBFS is below {MIN_IMPULSE_DBFS} dBFS: too quiet to detect"));
    }
    Ok(())
}

/// Pure generator of an impulse train (mono): silence, with a click every `interval` frames.
#[derive(Clone, Debug)]
pub struct ImpulseTrain {
    click: Vec<f32>,
    interval_frames: u64,
    lead_frames: u64,
    count: u32,
    pos: u64,
}

impl ImpulseTrain {
    /// `count` clicks, `interval_ms` apart; the first one after one interval of silence.
    /// Refuses levels above [`MAX_IMPULSE_DBFS`].
    pub fn new(rate: u32, shape: ImpulseShape, peak_dbfs: f32, interval_ms: f32, count: u32) -> std::result::Result<Self, String> {
        validate_peak_dbfs(peak_dbfs)?;
        if !(interval_ms.is_finite() && interval_ms >= 50.0) {
            return Err("interval must be at least 50 ms".into());
        }
        let click = click_samples(shape, rate, peak_dbfs);
        let interval_frames = (f64::from(interval_ms) * f64::from(rate) / 1000.0).round() as u64;
        if interval_frames <= click.len() as u64 {
            return Err("interval shorter than the click".into());
        }
        Ok(ImpulseTrain { click, interval_frames, lead_frames: interval_frames, count, pos: 0 })
    }

    /// Frame index (from the start of the train) of the first frame of click `k`.
    pub fn click_start_frame(&self, k: u32) -> u64 {
        self.lead_frames + u64::from(k) * self.interval_frames
    }

    /// Total frames until the last click has finished.
    pub fn total_frames(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.click_start_frame(self.count - 1) + self.click.len() as u64
        }
    }

    pub fn is_done(&self) -> bool {
        self.pos >= self.total_frames()
    }

    pub fn click_len(&self) -> usize {
        self.click.len()
    }

    /// Next sample of the train.
    pub fn next_sample(&mut self) -> f32 {
        let p = self.pos;
        self.pos += 1;
        if p < self.lead_frames {
            return 0.0;
        }
        let rel = p - self.lead_frames;
        let (k, off) = (rel / self.interval_frames, rel % self.interval_frames);
        if k < u64::from(self.count) && (off as usize) < self.click.len() {
            self.click[off as usize]
        } else {
            0.0
        }
    }
}

/// True for VBMatrix's render endpoints ("VBMatrix In 1", "VBMatrix In 3 (VB-Audio Matrix VAIO)", ...):
/// audio played there is routed by VBMatrix into the virtual ASIO driver, so a quiet click injected
/// there travels the whole router chain — the injection sink of `router-cli latency` since M3a, and
/// what `capture-probe impulses` measures the engine periods of.
pub fn is_vbmatrix_render_name(name: &str) -> bool {
    name.trim().to_lowercase().starts_with("vbmatrix in")
}

/// True when `name` may be used as an injection sink: a known virtual cable, a VBMatrix render
/// endpoint ("VBMatrix In *"), or matching an explicit allow-list entry (case-insensitive
/// substring; empty entries never match).
pub fn is_sink_allowed(name: &str, allow: &[String]) -> bool {
    let n = name.to_lowercase();
    is_virtual_cable_name(name)
        || is_vbmatrix_render_name(name)
        || allow.iter().any(|a| !a.trim().is_empty() && n.contains(&a.trim().to_lowercase()))
}

// ------------------------------------------------------------------------------------
// Rendering
// ------------------------------------------------------------------------------------

/// How the injection stream is opened (what kind of application it imitates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderMode {
    /// `IAudioClient::Initialize`, event driven, buffer duration 0 (the system default buffer): a legacy / typical app.
    SharedDefault,
    /// `IAudioClient3::InitializeSharedAudioStream` at the endpoint's minimum engine period: a low-latency aware app.
    SharedLowLatency,
    /// `IAudioClient::Initialize` with an explicit small buffer (`buffer_frames`, default two engine periods).
    SharedMinBuffer,
}

impl RenderMode {
    /// Parses `shared-default`, `shared-lowlatency`, `shared-min-buffer`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "shared-default" | "default" => Some(RenderMode::SharedDefault),
            "shared-lowlatency" | "shared-low-latency" | "lowlatency" => Some(RenderMode::SharedLowLatency),
            "shared-min-buffer" | "min-buffer" => Some(RenderMode::SharedMinBuffer),
            _ => None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            RenderMode::SharedDefault => "shared-default",
            RenderMode::SharedLowLatency => "shared-lowlatency",
            RenderMode::SharedMinBuffer => "shared-min-buffer",
        }
    }
}

/// How many frames the imitated application writes per wake-up event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderFill {
    /// Everything that is free (tops the buffer up completely, like the common sample code): queue = buffer.
    Full,
    /// One engine period of the stream per event.
    Period,
    /// A fixed number of frames per event.
    Frames(u32),
}

impl RenderFill {
    /// Parses `full`, `period` or a frame count.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Some(RenderFill::Full),
            "period" => Some(RenderFill::Period),
            n => n.parse::<u32>().ok().filter(|&v| v > 0).map(RenderFill::Frames),
        }
    }
    /// Chunk limit in frames for a stream with the given period (`u32::MAX` = unlimited).
    pub fn chunk_frames(&self, period_frames: u32) -> u32 {
        match *self {
            RenderFill::Full => u32::MAX,
            RenderFill::Period => period_frames.max(1),
            RenderFill::Frames(n) => n.max(1),
        }
    }
    pub fn describe(&self) -> String {
        match *self {
            RenderFill::Full => "full".into(),
            RenderFill::Period => "period".into(),
            RenderFill::Frames(n) => format!("{n} fr"),
        }
    }
}

/// How much silence is queued before the stream starts (this sets the steady-state queue when the client
/// writes only one period per event).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderPrefill {
    Full,
    Period,
    Frames(u32),
}

impl RenderPrefill {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Some(RenderPrefill::Full),
            "period" => Some(RenderPrefill::Period),
            n => n.parse::<u32>().ok().map(RenderPrefill::Frames),
        }
    }
    /// Prefill in frames, at most `buffer_frames`.
    pub fn frames(&self, buffer_frames: u32, period_frames: u32) -> u32 {
        match *self {
            RenderPrefill::Full => buffer_frames,
            RenderPrefill::Period => period_frames.min(buffer_frames),
            RenderPrefill::Frames(n) => n.min(buffer_frames),
        }
    }
}

/// Options of [`ImpulseRenderer::start`].
#[derive(Clone, Debug)]
pub struct ImpulseRenderOptions {
    /// Render endpoint name (substring, case-insensitive).
    pub endpoint: String,
    /// Extra endpoint-name substrings that may be used although they are not a known virtual cable.
    pub allow_endpoint: Vec<String>,
    pub shape: ImpulseShape,
    /// Click peak in dBFS; refused above [`MAX_IMPULSE_DBFS`].
    pub peak_dbfs: f32,
    /// Stream type (default: `SharedDefault`, a typical shared-mode event-driven application).
    pub mode: RenderMode,
    /// Explicit buffer size in frames (`SharedDefault` / `SharedMinBuffer`; ignored by `SharedLowLatency`).
    pub buffer_frames: Option<u32>,
    /// Frames written per event (default `Period`: one engine period per event, like a typical application; the
    /// tool used to top the buffer up completely (`Full`), which adds about 10 ms of queue that real apps rarely have).
    pub fill: RenderFill,
    /// Silence queued before the start (default `Period`: one period; with `Period` fill this is the steady queue).
    pub prefill: RenderPrefill,
}

impl ImpulseRenderOptions {
    /// Applies `--render-mode`, `--render-buffer-frames`, `--render-fill` and `--render-prefill` values (as looked
    /// up by `get`) to these options. Shared by `capture-probe impulses` and `router-cli latency`.
    pub fn apply_render_args(&mut self, get: &dyn Fn(&str) -> Option<String>) -> std::result::Result<(), String> {
        if let Some(v) = get("--render-mode") {
            self.mode = RenderMode::parse(&v).ok_or("--render-mode: use shared-default, shared-lowlatency or shared-min-buffer")?;
        }
        if let Some(v) = get("--render-buffer-frames") {
            let n: u32 = v.parse().map_err(|_| "--render-buffer-frames: not a number".to_string())?;
            if !(8..=48000).contains(&n) {
                return Err("--render-buffer-frames must be 8..=48000".into());
            }
            self.buffer_frames = Some(n);
        }
        if let Some(v) = get("--render-fill") {
            self.fill = RenderFill::parse(&v).ok_or("--render-fill: use full, period or a frame count")?;
        }
        if let Some(v) = get("--render-prefill") {
            self.prefill = RenderPrefill::parse(&v).ok_or("--render-prefill: use full, period or a frame count")?;
        }
        Ok(())
    }

    pub fn new(endpoint: &str) -> Self {
        ImpulseRenderOptions {
            endpoint: endpoint.to_string(),
            allow_endpoint: Vec::new(),
            shape: ImpulseShape::default(),
            peak_dbfs: DEFAULT_IMPULSE_DBFS,
            mode: RenderMode::SharedDefault,
            buffer_frames: None,
            fill: RenderFill::Period,
            prefill: RenderPrefill::Period,
        }
    }
}

/// What the render stream looks like.
#[derive(Clone, Debug)]
pub struct RenderInfo {
    pub endpoint: String,
    pub format: FormatInfo,
    pub buffer_frames: u32,
    /// Engine period of the endpoint's shared stream (default period), frames.
    pub period_frames: u32,
    /// `IAudioClient::GetStreamLatency`, milliseconds.
    pub stream_latency_ms: f64,
    pub click_frames: usize,
    pub peak_dbfs: f32,
    pub mode: RenderMode,
    pub fill: RenderFill,
    /// Silence queued before the start, frames.
    pub prefill_frames: u32,
    /// Endpoint minimum / default shared engine period (IAudioClient3), frames, if reported.
    pub min_period_frames: Option<u32>,
    pub default_period_frames: Option<u32>,
    /// Shared engine period `(frames, rate)` of the endpoint just before this stream opened (with whatever
    /// keep-alive is running) and after it started.
    pub engine_before: Option<(u32, u32)>,
    pub engine_after: Option<(u32, u32)>,
}

/// Timestamps of one injected click (QPC ticks of [`qpc_hz`]).
#[derive(Clone, Copy, Debug)]
pub struct ImpulseTimes {
    pub index: u32,
    /// QPC taken right before `ReleaseBuffer` of the buffer holding the click's first frame: the
    /// moment the frames were handed to the audio engine. The injection reference of the measurement.
    pub submit_ticks: i64,
    /// Estimated QPC at which the click's first frame leaves the render client towards the
    /// endpoint (`IAudioClock::GetPosition` and its QPC). `None` if the clock was unusable.
    pub audible_clock_ticks: Option<i64>,
    /// Same estimate from `GetCurrentPadding` (queued frames ahead of the click) plus `GetStreamLatency`.
    pub audible_padding_ticks: i64,
    /// Frames queued ahead of the click when it was written.
    pub padding_frames: u32,
    /// Stream frame index of the click's first frame.
    pub stream_frame: u64,
}

impl ImpulseTimes {
    /// Render-side queue delay (ms) submit -> audible, from the clock estimate when available,
    /// else from the padding estimate. Second value: which one it is.
    pub fn render_delay_ms(&self, hz: i64) -> (f64, &'static str) {
        let hz = hz.max(1) as f64;
        match self.audible_clock_ticks {
            Some(a) => ((a - self.submit_ticks) as f64 * 1000.0 / hz, "clock"),
            None => ((self.audible_padding_ticks - self.submit_ticks) as f64 * 1000.0 / hz, "padding"),
        }
    }
    /// The padding-based delay (ms).
    pub fn padding_delay_ms(&self, hz: i64) -> f64 {
        (self.audible_padding_ticks - self.submit_ticks) as f64 * 1000.0 / hz.max(1) as f64
    }
}

struct FireCmd {
    index: u32,
    reply: Sender<Result<ImpulseTimes>>,
}

/// A running silent render stream on a cable's playback endpoint that can inject a click on request.
/// Dropping it stops the stream.
pub struct ImpulseRenderer {
    tx: Sender<FireCmd>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    info: RenderInfo,
    next_index: u32,
}

impl ImpulseRenderer {
    /// Opens the stream and starts rendering silence. Refuses a too loud click and a sink that is not
    /// a known virtual cable (unless allow-listed).
    pub fn start(opts: &ImpulseRenderOptions) -> Result<Self> {
        validate_peak_dbfs(opts.peak_dbfs).map_err(CaptureError::Refused)?;
        let (tx, rx) = mpsc::channel::<FireCmd>();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<RenderInfo>>(1);
        let (o2, s2) = (opts.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("impulse-render".into())
            .spawn(move || render_thread(o2, rx, s2, ready_tx))
            .map_err(|e| CaptureError::Thread(e.to_string()))?;
        let info = match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(i)) => i,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                stop.store(true, Relaxed);
                return Err(CaptureError::Thread("impulse render stream did not start".into()));
            }
        };
        Ok(ImpulseRenderer { tx, stop, thread: Some(thread), info, next_index: 0 })
    }

    pub fn info(&self) -> &RenderInfo {
        &self.info
    }

    /// Injects one click at the next opportunity (within one engine period) and returns its timestamps.
    pub fn fire(&mut self) -> Result<ImpulseTimes> {
        let (rtx, rrx) = mpsc::channel();
        let index = self.next_index;
        self.next_index += 1;
        self.tx.send(FireCmd { index, reply: rtx }).map_err(|_| CaptureError::Thread("render thread ended".into()))?;
        rrx.recv_timeout(Duration::from_secs(2)).map_err(|_| CaptureError::Thread("click was not submitted within 2 s".into()))?
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ImpulseRenderer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Renders `count` clicks `interval_ms` apart into `endpoint` (shared mode, event driven) and returns
/// their timestamps. Blocks for about `count * interval_ms`. Refuses levels above -35 dBFS.
pub fn render_impulses(opts: &ImpulseRenderOptions, count: u32, interval_ms: f32) -> Result<Vec<ImpulseTimes>> {
    if count == 0 || count > 10 {
        return Err(CaptureError::Refused("count must be 1..=10".into()));
    }
    if !(interval_ms.is_finite() && (200.0..=10_000.0).contains(&interval_ms)) {
        return Err(CaptureError::Refused("interval must be 200..=10000 ms".into()));
    }
    let mut r = ImpulseRenderer::start(opts)?;
    std::thread::sleep(Duration::from_millis(500));
    let mut out = Vec::new();
    for k in 0..count {
        let t = Instant::now();
        out.push(r.fire()?);
        if k + 1 < count {
            std::thread::sleep(Duration::from_secs_f32(interval_ms / 1000.0).saturating_sub(t.elapsed()));
        }
    }
    std::thread::sleep(Duration::from_millis(100));
    r.stop();
    Ok(out)
}

fn render_thread(opts: ImpulseRenderOptions, rx: Receiver<FireCmd>, stop: Arc<AtomicBool>, ready: mpsc::SyncSender<Result<RenderInfo>>) {
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
    // SAFETY: auto-reset event signalled by WASAPI; closed below.
    let event = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(CaptureError::Com { context: "CreateEventW", source: e }));
            return;
        }
    };
    let mut ready = Some(ready);
    let result = (|| -> Result<()> {
        let ep = find_device(Flow::Render, &opts.endpoint)?;
        if !is_sink_allowed(&ep.name, &opts.allow_endpoint) {
            return Err(CaptureError::Refused(format!(
                "render endpoint \"{}\" is not a known virtual cable; refusing to inject audio into it",
                ep.name
            )));
        }
        let dev = open_device(&ep.id)?;
        let probe = activate_client(&dev)?;
        let mix = MixFormat::from_client(&probe)?;
        let fmt = mix.info()?;
        let rate_u = fmt.sample_rate.max(1);
        let engine_before = current_engine_period(&ep.id);
        // Endpoint engine periods (IAudioClient3), when the driver reports them.
        let periods = crate::device::engine_periods(&dev, &mix);
        let cur_frames = engine_before.filter(|&(f, r)| f > 0 && r == rate_u).map(|(f, _)| f);
        // SAFETY: shared event-driven init; the format pointer is valid for the calls.
        let (client, render, clock, buffer_frames, latency_hns, period_frames) = unsafe {
            let (client, period_frames): (IAudioClient, u32) = match opts.mode {
                RenderMode::SharedLowLatency => {
                    let c3 = dev
                        .Activate::<IAudioClient3>(CLSCTX_ALL, None)
                        .map_err(CaptureError::com("Activate(IAudioClient3)"))?;
                    let (mut d, mut f, mut mn, mut mx) = (0u32, 0u32, 0u32, 0u32);
                    c3.GetSharedModeEnginePeriod(mix.ptr(), &mut d, &mut f, &mut mn, &mut mx)
                        .map_err(CaptureError::com("GetSharedModeEnginePeriod"))?;
                    c3.InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, mn, mix.ptr(), None)
                        .map_err(CaptureError::com("InitializeSharedAudioStream(render)"))?;
                    (c3.cast().map_err(CaptureError::com("cast IAudioClient3"))?, mn)
                }
                RenderMode::SharedDefault | RenderMode::SharedMinBuffer => {
                    let client = activate_client(&dev)?;
                    let mut period = 0i64;
                    let _ = client.GetDevicePeriod(Some(&mut period), None);
                    let period_frames = (period as f64 * f64::from(rate_u) / 1e7).round() as u32;
                    let want_frames = match (opts.mode, opts.buffer_frames) {
                        (_, Some(n)) => Some(n.max(8)),
                        (RenderMode::SharedMinBuffer, None) => Some(2 * cur_frames.unwrap_or(period_frames).max(4)),
                        _ => None,
                    };
                    let hns = want_frames.map(|n| (f64::from(n) * 1e7 / f64::from(rate_u)).round() as i64).unwrap_or(0);
                    client
                        .Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, hns, 0, mix.ptr(), None)
                        .map_err(CaptureError::com("Initialize(render, shared)"))?;
                    (client, period_frames)
                }
            };
            client.SetEventHandle(event).map_err(CaptureError::com("SetEventHandle"))?;
            let render: IAudioRenderClient = client.GetService().map_err(CaptureError::com("GetService(IAudioRenderClient)"))?;
            let clock: Option<IAudioClock> = client.GetService().ok();
            let frames = client.GetBufferSize().map_err(CaptureError::com("GetBufferSize"))?;
            let lat = client.GetStreamLatency().unwrap_or(0);
            (client, render, clock, frames, lat, period_frames)
        };
        let rate = fmt.sample_rate;
        let hz = qpc_hz();
        let prefill_frames = opts.prefill.frames(buffer_frames, period_frames);
        let chunk = opts.fill.chunk_frames(period_frames);
        let click = click_samples(opts.shape, rate, opts.peak_dbfs);
        let block = fmt.block_align();
        let bps = fmt.format.bytes_per_sample();
        let clock_freq = clock.as_ref().and_then(|c| unsafe { c.GetFrequency() }.ok()).unwrap_or(0);
        let stream_latency_ms = latency_hns as f64 / 10_000.0;

        // Pre-fill with silence, then start.
        // SAFETY: GetBuffer yields buffer_frames * block writable bytes until ReleaseBuffer.
        unsafe {
            if prefill_frames > 0 {
                let p = render.GetBuffer(prefill_frames).map_err(CaptureError::com("GetBuffer(prefill)"))?;
                std::ptr::write_bytes(p, 0, prefill_frames as usize * block);
                render.ReleaseBuffer(prefill_frames, 0).map_err(CaptureError::com("ReleaseBuffer(prefill)"))?;
            }
            client.Start().map_err(CaptureError::com("Start(render)"))?;
        }
        std::thread::sleep(Duration::from_millis(150));
        let engine_after = current_engine_period(&ep.id);
        let info = RenderInfo {
            endpoint: ep.name.clone(),
            format: fmt,
            buffer_frames,
            period_frames,
            stream_latency_ms,
            click_frames: click.len(),
            peak_dbfs: opts.peak_dbfs,
            mode: opts.mode,
            fill: opts.fill,
            prefill_frames,
            min_period_frames: periods.map(|p| p.min_frames),
            default_period_frames: periods.map(|p| p.default_frames),
            engine_before,
            engine_after,
        };
        if let Some(r) = ready.take() {
            let _ = r.send(Ok(info));
        }

        let mut written: u64 = u64::from(prefill_frames);
        let mut pending: Option<FireCmd> = None;
        while !stop.load(Relaxed) {
            // SAFETY: valid event handle.
            let w = unsafe { WaitForSingleObject(event, 100) };
            if stop.load(Relaxed) {
                break;
            }
            if w != WAIT_OBJECT_0 {
                continue;
            }
            if pending.is_none() {
                pending = rx.try_recv().ok();
            }
            // SAFETY: valid client; padding <= buffer_frames.
            let pad = unsafe { client.GetCurrentPadding() }.map_err(CaptureError::com("GetCurrentPadding"))?;
            // A click longer than the chunk (1 ms periods) is written in one go so that it stays contiguous.
            let limit = if pending.is_some() { chunk.max(click.len() as u32) } else { chunk };
            let avail = buffer_frames.saturating_sub(pad).min(limit);
            if avail == 0 {
                continue;
            }
            let with_click = pending.is_some() && avail as usize >= click.len();
            // SAFETY: GetBuffer returns `avail * block` writable bytes until ReleaseBuffer.
            unsafe {
                let p = render.GetBuffer(avail).map_err(CaptureError::com("GetBuffer(render)"))?;
                let buf = std::slice::from_raw_parts_mut(p, avail as usize * block);
                buf.fill(0);
                if with_click {
                    for (i, &v) in click.iter().enumerate() {
                        for c in 0..fmt.channels as usize {
                            let o = i * block + c * bps;
                            write_sample(fmt.format, v, &mut buf[o..o + bps]);
                        }
                    }
                }
                // Clock reading just before the hand-over, so position and submit time belong together.
                let mut pos = 0u64;
                let mut qpc100 = 0u64;
                let clock_ok = with_click && clock.as_ref().is_some_and(|c| c.GetPosition(&mut pos, Some(&mut qpc100)).is_ok());
                let submit = qpc_ticks();
                render.ReleaseBuffer(avail, 0).map_err(CaptureError::com("ReleaseBuffer(render)"))?;
                if with_click {
                    if let Some(cmd) = pending.take() {
                        let rate_f = f64::from(rate);
                        let audible_pad = submit + ((f64::from(pad) / rate_f + stream_latency_ms / 1000.0) * hz as f64) as i64;
                        let audible_clock = if clock_ok && clock_freq > 0 && qpc100 > 0 {
                            let pos_frames = pos as f64 / clock_freq as f64 * rate_f;
                            let delta_s = (written as f64 - pos_frames) / rate_f;
                            let q_ticks = (qpc100 as f64 * hz as f64 / 1e7) as i64;
                            // The clock reading is only plausible if the click is 0..1 s ahead of it.
                            (0.0..1.0).contains(&delta_s).then(|| q_ticks + (delta_s * hz as f64) as i64)
                        } else {
                            None
                        };
                        let _ = cmd.reply.send(Ok(ImpulseTimes {
                            index: cmd.index,
                            submit_ticks: submit,
                            audible_clock_ticks: audible_clock,
                            audible_padding_ticks: audible_pad,
                            padding_frames: pad,
                            stream_frame: written,
                        }));
                    }
                }
            }
            written += u64::from(avail);
        }
        // SAFETY: stopping is always valid.
        unsafe {
            let _ = client.Stop();
        }
        Ok(())
    })();
    if let Err(e) = result {
        if let Some(r) = ready.take() {
            let _ = r.send(Err(e));
        } else {
            // Failed while running: tell a waiting `fire` instead of letting it time out.
            if let Ok(cmd) = rx.try_recv() {
                let _ = cmd.reply.send(Err(e));
            }
        }
    }
    // SAFETY: handles owned by this thread.
    unsafe {
        let _ = CloseHandle(event);
        if let Some(h) = mmcss {
            let _ = AvRevertMmThreadCharacteristics(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_peak_and_window() {
        let c = click_samples(ImpulseShape::default(), 48000, -40.0);
        assert_eq!(c.len(), 96);
        assert_eq!(c[0], 0.0);
        let peak = c.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!((20.0 * peak.log10() + 40.0).abs() < 1e-3, "peak {peak}");
        assert!(c[95].abs() < 0.2 * peak, "tail must be windowed down");
        // finite and has both polarities
        assert!(c.iter().any(|&x| x > 0.0) && c.iter().any(|&x| x < 0.0));
    }

    #[test]
    fn chirp_peak_and_length() {
        let c = click_samples(ImpulseShape::Chirp { f0_hz: 800.0, f1_hz: 4000.0, dur_ms: 4.0 }, 48000, -45.0);
        assert_eq!(c.len(), 192);
        let peak = c.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!((20.0 * peak.log10() + 45.0).abs() < 1e-3);
    }

    #[test]
    fn onset_grows_with_threshold_ratio() {
        let c = click_samples(ImpulseShape::default(), 48000, -40.0);
        let (a, b, d) = (onset_index(&c, 0.02), onset_index(&c, 0.2), onset_index(&c, 0.8));
        assert!(a <= b && b <= d, "{a} {b} {d}");
        assert!(d > a);
        assert!(onset_index(&c, 1.0) > 0);
        assert_eq!(onset_index(&[0.0; 8], 0.5), 0);
    }

    #[test]
    fn levels_above_minus_35_are_refused() {
        assert!(validate_peak_dbfs(-35.0).is_ok());
        assert!(validate_peak_dbfs(-34.9).is_err());
        assert!(validate_peak_dbfs(0.0).is_err());
        assert!(validate_peak_dbfs(f32::NAN).is_err());
        assert!(validate_peak_dbfs(-90.0).is_err());
        assert!(ImpulseTrain::new(48000, ImpulseShape::default(), -30.0, 1000.0, 3).is_err());
        let o = ImpulseRenderOptions { peak_dbfs: -20.0, ..ImpulseRenderOptions::new("Line 1") };
        assert!(matches!(ImpulseRenderer::start(&o), Err(CaptureError::Refused(_))));
        assert!(matches!(render_impulses(&o, 3, 1000.0), Err(CaptureError::Refused(_))));
    }

    #[test]
    fn train_spacing_and_content() {
        let mut t = ImpulseTrain::new(48000, ImpulseShape::default(), -40.0, 100.0, 3).unwrap();
        assert_eq!(t.click_start_frame(0), 4800);
        assert_eq!(t.click_start_frame(2), 4800 + 2 * 4800);
        let total = t.total_frames();
        assert_eq!(total, 4800 * 3 + 96);
        let all: Vec<f32> = (0..total).map(|_| t.next_sample()).collect();
        assert!(t.is_done());
        assert!(all[..4800].iter().all(|&x| x == 0.0));
        let nz: Vec<usize> = all.iter().enumerate().filter(|(_, &x)| x != 0.0).map(|(i, _)| i).collect();
        for k in 0..3u64 {
            let s = (4800 + k * 4800) as usize;
            assert!(nz.iter().any(|&i| i >= s && i < s + 96), "click {k}");
        }
        assert!(nz.iter().all(|&i| (i % 4800) < 96));
        assert!(ImpulseTrain::new(48000, ImpulseShape::default(), -40.0, 10.0, 3).is_err());
    }

    #[test]
    fn shape_parse() {
        assert_eq!(ImpulseShape::parse("burst"), Some(ImpulseShape::default()));
        assert_eq!(ImpulseShape::parse("burst:2000:1.5"), Some(ImpulseShape::Burst { freq_hz: 2000.0, dur_ms: 1.5 }));
        assert!(matches!(ImpulseShape::parse("chirp"), Some(ImpulseShape::Chirp { .. })));
        assert_eq!(ImpulseShape::parse("burst:5"), None);
        assert_eq!(ImpulseShape::parse("noise"), None);
    }

    #[test]
    fn render_option_parsing() {
        assert_eq!(RenderMode::parse("shared-default"), Some(RenderMode::SharedDefault));
        assert_eq!(RenderMode::parse("Shared-LowLatency"), Some(RenderMode::SharedLowLatency));
        assert_eq!(RenderMode::parse("shared-min-buffer"), Some(RenderMode::SharedMinBuffer));
        assert_eq!(RenderMode::parse("exclusive"), None);
        assert_eq!(RenderFill::parse("full"), Some(RenderFill::Full));
        assert_eq!(RenderFill::parse("period"), Some(RenderFill::Period));
        assert_eq!(RenderFill::parse("96"), Some(RenderFill::Frames(96)));
        assert_eq!(RenderFill::parse("0"), None);
        assert_eq!(RenderFill::Full.chunk_frames(480), u32::MAX);
        assert_eq!(RenderFill::Period.chunk_frames(480), 480);
        assert_eq!(RenderPrefill::parse("full"), Some(RenderPrefill::Full));
        assert_eq!(RenderPrefill::Full.frames(1056, 480), 1056);
        assert_eq!(RenderPrefill::Period.frames(1056, 480), 480);
        assert_eq!(RenderPrefill::Frames(5000).frames(1056, 480), 1056);
        assert_eq!(RenderPrefill::parse("x"), None);
        let args: Vec<(&str, &str)> = vec![("--render-mode", "shared-min-buffer"), ("--render-buffer-frames", "96"), ("--render-fill", "period"), ("--render-prefill", "48")];
        let get = |k: &str| args.iter().find(|(a, _)| *a == k).map(|(_, v)| v.to_string());
        let mut o = ImpulseRenderOptions::new("Line 1");
        o.apply_render_args(&get).unwrap();
        assert_eq!((o.mode, o.buffer_frames, o.fill, o.prefill), (RenderMode::SharedMinBuffer, Some(96), RenderFill::Period, RenderPrefill::Frames(48)));
        let bad = |k: &str| (k == "--render-mode").then(|| "exclusive".to_string());
        assert!(ImpulseRenderOptions::new("x").apply_render_args(&bad).is_err());
        let bad2 = |k: &str| (k == "--render-buffer-frames").then(|| "2".to_string());
        assert!(ImpulseRenderOptions::new("x").apply_render_args(&bad2).is_err());
        // the defaults imitate a typical 10 ms event-driven application
        let o = ImpulseRenderOptions::new("Line 1");
        assert_eq!((o.mode, o.fill, o.prefill, o.buffer_frames), (RenderMode::SharedDefault, RenderFill::Period, RenderPrefill::Period, None));
    }

    #[test]
    fn sink_guard() {
        assert!(is_sink_allowed("Hi-Fi Cable Input (VB-Audio Hi-Fi Cable)", &[]));
        assert!(is_sink_allowed("Line 1 (Virtual Audio Cable)", &[]));
        assert!(is_sink_allowed("VBMatrix In 3 (VB-Audio Matrix VAIO)", &[]), "bridge render endpoint");
        assert!(is_sink_allowed("vbmatrix in 1", &[]));
        assert!(!is_sink_allowed("VBMatrix Out 1", &[]), "only the In side is a render endpoint");
        assert!(!is_sink_allowed("Headphones (Yamaha UR22C)", &[]));
        assert!(!is_sink_allowed("Speakers (Realtek)", &[String::new(), "  ".into()]));
        assert!(is_sink_allowed("Speakers (Realtek)", &["realtek".to_string()]));
    }

    #[test]
    fn render_delay_prefers_clock() {
        let t = ImpulseTimes {
            index: 0,
            submit_ticks: 1_000_000,
            audible_clock_ticks: Some(1_020_000),
            audible_padding_ticks: 1_030_000,
            padding_frames: 480,
            stream_frame: 0,
        };
        let (ms, which) = t.render_delay_ms(1_000_000);
        assert_eq!(which, "clock");
        assert!((ms - 20.0).abs() < 1e-9);
        assert!((t.padding_delay_ms(1_000_000) - 30.0).abs() < 1e-9);
        let t2 = ImpulseTimes { audible_clock_ticks: None, ..t };
        assert_eq!(t2.render_delay_ms(1_000_000).1, "padding");
    }
}
