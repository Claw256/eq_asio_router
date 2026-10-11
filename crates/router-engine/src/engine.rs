//! `Engine`: the control thread (STA + message pump) that owns the ASIO driver, brings
//! the pipeline up, supervises it and restarts it with back-off.
//!
//! Teardown order (see `Session::drop`): heartbeat off, watchdog stopped, ASIO stream
//! stopped, buffers disposed and the driver released, then the render keep-alive stopped,
//! then the capture session stopped and the config watcher joined, so the Yamaha is always
//! left released and the audio callback never outlives the ring it reads. The keep-alive goes
//! BEFORE the capture: it is a pure render-side helper (a silent stream into the cable); ending it
//! first lets the cable's render engine period settle back while the capture is still draining,
//! so the capture thread never sees the period renegotiation as an anomaly of its own session.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use asio_host::AsioDriver;
use eq_core::{Caps, Config, EngineParams, FaultState, MeasuredLatency, COMMON_RATES};
use router_core::arrival::{
    initial_headroom_frames, min_headroom_frames, required_headroom_frames, ArrivalStats, HeadroomParams,
    HeadroomPlanner, DIP_MARGIN,
};
use router_core::logger::{Level, Logger};
use router_core::processor::{validate_output_channels, EqProcessor, Handoff, Snapshot};
use router_core::resample::{RateMatcher, RateMatcherConfig};
use router_core::ring;
use router_core::rlog;
use router_core::status_writer::{StaticInfo, StatusWriter};
use router_core::watchdog::{Component, Heartbeat, Watchdog, WatchdogEvent};
use router_core::watcher::ConfigWatcher;
use wasapi_capture::{is_virtual_cable_name, CaptureEvent, CaptureSession, RenderKeepAlive, RenderKeepAliveOptions};
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND};
use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, SetEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, TranslateMessage, MSG, MWMO_INPUTAVAILABLE,
    PM_REMOVE, QS_ALLINPUT,
};

use crate::cable::{self, CableFormatsHandle, WindowsCableFormats};
use crate::logic::{
    choose_buffer_size, classify_change, cpu_percent, effective_master, frames_to_ms, is_forbidden_master,
    plan_capacity, plan_target, to_db, AutoSafety, Backoff, ConfigChange, HolderTracker, StallTracker, WAIT_RETRY,
};
use crate::pipeline::{
    qpc_freq, quality_to_u8, AudioCallback, CaptureSink, Live, OutputSet, Pipeline, PipelineInit, PtrTable,
    RESET_PANIC, RESET_RATE, RESET_REQUEST,
};
use crate::priority::{self, ControlMmcss, HardeningReport, TimerResolution};
use crate::probe::{select_input_channels, LatencyProbe, LoopbackProbe, LoopbackTap, ProbeInfo, ProbeShared};
use crate::synth::{self, SynthHandle, SyntheticSource};
use crate::tap::{self, TapConfig, TapSession};
use crate::source::{plan_capture, CorruptionAction, FallbackState};
use crate::status::{
    map_open_error, ApplyOutcome, EngineError, EngineState, EngineStatus, HolderInfo, KeepAliveStatus, LearnStatus,
};

/// Interval of the slow-field status publisher.
const TICK: Duration = Duration::from_millis(100);
/// How often the config file is polled for restart-class changes.
const FILE_POLL: Duration = Duration::from_millis(500);
/// Granularity of the control loop wake-ups.
const WAKE_MS: u32 = 25;
/// Largest capture period the ring is dimensioned for.
const MAX_PERIOD_FRAMES: u32 = 4096;
/// Length of the "Learn" measurement window (seconds).
pub const LEARN_SECS: f64 = 30.0;
/// Arrival events needed for a learn run to count.
const LEARN_MIN_EVENTS: u32 = 500;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Opens (finds + loads + inits) the master driver by name. Test seam: the default is
/// [`default_master_loader`]; tests inject fakes (e.g. "driver is busy").
pub type LoaderFn = dyn Fn(&str) -> Result<AsioDriver, EngineError> + Send + Sync;

#[derive(Clone)]
pub struct MasterLoader(pub Arc<LoaderFn>);

impl std::fmt::Debug for MasterLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterLoader")
    }
}

/// Real loader: registry lookup, then `AsioDriver::load_diagnosed` (holders on failure).
pub fn default_master_loader(name: &str) -> Result<AsioDriver, EngineError> {
    let info = asio_host::find_driver(name).map_err(|_| EngineError::MasterNotFound(name.to_string()))?;
    AsioDriver::load_diagnosed(&info, HWND::default()).map_err(|e| map_open_error(name, e))
}

/// Options of [`Engine::start_with`].
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// Replaces the master driver loader (tests). `None` = the real one.
    pub master_loader: Option<MasterLoader>,
    /// Config file watched for live edits (default: `eq_core::default_path()`).
    pub config_path: Option<PathBuf>,
    /// Write `caps.toml` once the master is known.
    pub publish_caps: bool,
    /// Create the `Local\eq_asio_router_status` mapping.
    pub status_mapping: bool,
    /// Measurement tap (writes WAV dumps of the stream at three points); `None` = off.
    pub tap: Option<TapConfig>,
    /// Replaces the Windows cable-format reader/writer (tests). `None` = the real one.
    pub cable_formats: Option<CableFormatsHandle>,
    /// Replaces the WASAPI capture by a synthetic source (measurements); `None` = real capture.
    pub synthetic: Option<SyntheticSource>,
    /// Keep retrying (back-off capped at 5 s) when a device is not available yet, even on the
    /// very first start: ASIO driver not registered/enumerated (`MasterNotFound`), capture
    /// device missing (`CaptureDeviceNotFound`), audio service not ready, driver open failures.
    /// Meant for login-time starts where devices appear a few seconds late. When `false`
    /// (default, used by `router-cli`) such errors on the first start are returned to the caller.
    pub retry_unavailable: bool,
    /// Latency measurement (`router-cli latency`): also create the Yamaha's loopback input buffers
    /// ("UR22C Streaming L/R") and let [`Engine::latency_probe`] detect an injected click there and on the
    /// capture side. `None` (default) creates no extra buffers and adds no work to the callbacks.
    pub loopback_probe: Option<LoopbackProbe>,
    /// Read `latency_measured.toml` (written by `router-cli latency`) so the latency readout uses the measured
    /// render side. `router-cli latency` itself turns this off, so its model line shows the pure estimate.
    pub load_measured: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            master_loader: None,
            config_path: eq_core::default_path(),
            publish_caps: true,
            status_mapping: true,
            tap: None,
            cable_formats: None,
            synthetic: None,
            retry_unavailable: false,
            loopback_probe: None,
            load_measured: true,
        }
    }
}

enum Cmd {
    Stop,
    Restart(&'static str),
    RetryNow,
    ControlPanel,
    Watchdog(WatchdogEvent),
}

// ------------------------------------------------------------------------------------
// Win32 helpers
// ------------------------------------------------------------------------------------

/// Auto-reset event used to wake the control thread's message pump.
struct Wake(isize);

impl Wake {
    fn new() -> Result<Wake, EngineError> {
        // SAFETY: plain event creation.
        let h = unsafe { CreateEventW(None, false, false, None) }.map_err(|e| EngineError::Thread(e.to_string()))?;
        Ok(Wake(h.0 as isize))
    }
    fn handle(&self) -> HANDLE {
        HANDLE(self.0 as *mut _)
    }
    fn set(&self) {
        // SAFETY: the handle stays valid until Drop.
        unsafe {
            let _ = SetEvent(self.handle());
        }
    }
}

impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: handle created in `new`.
        unsafe {
            let _ = CloseHandle(self.handle());
        }
    }
}

/// Named mutex `Local\eq_asio_router_engine`: one engine per user session.
struct InstanceGuard(isize);

impl InstanceGuard {
    fn acquire() -> Result<InstanceGuard, EngineError> {
        // SAFETY: plain named mutex creation; GetLastError is read immediately.
        unsafe {
            let h = CreateMutexW(None, false, w!("Local\\eq_asio_router_engine"))
                .map_err(|e| EngineError::Thread(e.to_string()))?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(h);
                return Err(EngineError::AlreadyRunning);
            }
            Ok(InstanceGuard(h.0 as isize))
        }
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: handle created in `acquire`.
        unsafe {
            let _ = CloseHandle(HANDLE(self.0 as *mut _));
        }
    }
}

/// Waits up to `ms` for the wake event or window messages and dispatches messages
/// (ASIO drivers may post to the thread's queue).
fn pump_wait(wake: &Wake, ms: u32) {
    // SAFETY: valid handle slice; the message structs are local.
    unsafe {
        let _ = MsgWaitForMultipleObjectsEx(Some(&[wake.handle()]), ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// ------------------------------------------------------------------------------------
// Shared state
// ------------------------------------------------------------------------------------

struct Shared {
    status: Mutex<EngineStatus>,
    config: Mutex<Config>,
    caps: Mutex<Option<Caps>>,
    live: Arc<Live>,
    handoff: Arc<Handoff<Snapshot>>,
    wake: Wake,
    /// Base safety buffer (f32 bits), updated by `apply`.
    base_safety: AtomicU32,
    safety_auto: AtomicBool,
    /// Learned jitter headroom floor in ms (f32 bits) of the current capture device
    /// (`Config::jitter_margin_for`).
    jitter_floor_ms: AtomicU32,
    /// "Learn (30 s)" requested (consumed by the status tick).
    learn_req: AtomicBool,
    /// Forget a corruption fallback ("Retry fixed period"); consumed before the next session start.
    reset_fallback_req: AtomicBool,
    n_out: AtomicU32,
    /// Last cable-matching attempt (target rate, when, succeeded): never retried in a loop.
    cable_attempt: Mutex<Option<(u32, Instant, bool)>>,
    /// Latency probe state (only when `EngineOptions::loopback_probe` is set).
    probe: Option<Arc<ProbeShared>>,
    opts: EngineOptions,
}

impl Shared {
    fn set_state(&self, st: EngineState) {
        lock(&self.status).state = st;
    }
    /// Takes over the bookkeeping shared by `apply` and the file poller.
    fn adopt(&self, new: Config, effective_rate: u32) -> ConfigChange {
        let mut cur = lock(&self.config);
        let change = classify_change(&cur, &new, effective_rate);
        self.base_safety.store(new.safety_buffer_ms.to_bits(), Relaxed);
        self.safety_auto.store(new.safety_buffer_auto, Relaxed);
        self.jitter_floor_ms.store(new.jitter_margin_for(&new.capture_device).to_bits(), Relaxed);
        self.live.quality.store(quality_to_u8(new.resampler_quality), Relaxed);
        *cur = new;
        change
    }
}

// ------------------------------------------------------------------------------------
// Public engine handle
// ------------------------------------------------------------------------------------

/// The router engine: Hi-Fi Cable (WASAPI) -> EQ -> master ASIO driver.
pub struct Engine {
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
    thread: Mutex<Option<JoinHandle<()>>>,
    guard: Mutex<Option<InstanceGuard>>,
    /// 1 ms timer resolution while the engine runs.
    timer: Mutex<Option<TimerResolution>>,
}

impl Engine {
    /// Starts the engine with default options (watches `eq_core::default_path()`).
    /// Blocks until the first start-up has succeeded or failed; later failures are
    /// retried with back-off and reported through [`status`](Self::status).
    pub fn start(config: Config) -> Result<Engine, EngineError> {
        Self::start_with(config, EngineOptions::default())
    }

    /// As [`start`](Self::start) with explicit options.
    pub fn start_with(config: Config, opts: EngineOptions) -> Result<Engine, EngineError> {
        let guard = InstanceGuard::acquire()?;
        let hardening = priority::harden_process();
        let timer = TimerResolution::begin();
        let cfg = config.sanitized();
        let _ = Logger::init();
        rlog!(Level::Info, "engine starting (master \"{}\")", effective_master(&cfg));
        rlog!(Level::Info, "hardening: {} (1 ms timer {})", hardening.summary(), if timer.is_active() { "on" } else { "unavailable" });
        let live = Arc::new(Live::default());
        live.quality.store(quality_to_u8(cfg.resampler_quality), Relaxed);
        let shared = Arc::new(Shared {
            status: Mutex::new(EngineStatus { state: EngineState::Starting, ..EngineStatus::default() }),
            base_safety: AtomicU32::new(cfg.safety_buffer_ms.to_bits()),
            safety_auto: AtomicBool::new(cfg.safety_buffer_auto),
            jitter_floor_ms: AtomicU32::new(cfg.jitter_margin_for(&cfg.capture_device).to_bits()),
            learn_req: AtomicBool::new(false),
            reset_fallback_req: AtomicBool::new(false),
            config: Mutex::new(cfg),
            caps: Mutex::new(None),
            live,
            handoff: Arc::new(Handoff::new()),
            wake: Wake::new()?,
            n_out: AtomicU32::new(0),
            cable_attempt: Mutex::new(None),
            probe: opts.loopback_probe.as_ref().map(|_| Arc::new(ProbeShared::default())),
            opts,
        });
        let (tx, rx) = mpsc::channel::<Cmd>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), EngineError>>(1);
        let (sh2, tx2) = (shared.clone(), tx.clone());
        let handle = std::thread::Builder::new()
            .name("router-control".into())
            .spawn(move || control_thread(sh2, rx, tx2, ready_tx))
            .map_err(|e| EngineError::Thread(e.to_string()))?;
        let engine = Engine {
            shared,
            tx,
            thread: Mutex::new(Some(handle)),
            guard: Mutex::new(Some(guard)),
            timer: Mutex::new(Some(timer)),
        };
        match ready_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => Ok(engine),
            Ok(Err(e)) => {
                engine.stop();
                Err(e)
            }
            Err(_) => {
                engine.stop();
                Err(EngineError::Timeout)
            }
        }
    }

    /// Current status snapshot (updated every 100 ms and on state changes).
    pub fn status(&self) -> EngineStatus {
        lock(&self.shared.status).clone()
    }

    /// Handle on the latency probe (`None` unless started with `EngineOptions::loopback_probe`).
    pub fn latency_probe(&self) -> Option<LatencyProbe> {
        self.shared.probe.clone().map(LatencyProbe::new)
    }

    /// Master driver capabilities, once known.
    pub fn caps(&self) -> Option<Caps> {
        lock(&self.shared.caps).clone()
    }

    /// The config currently in force (sanitised).
    pub fn config(&self) -> Config {
        lock(&self.shared.config).clone()
    }

    /// Writes the master's caps to `Caps::default_path()` (done automatically after
    /// each successful start when `EngineOptions::publish_caps` is set).
    pub fn publish_caps(&self) -> Result<(), EngineError> {
        let caps = self.caps().ok_or_else(|| EngineError::Asio("master not probed yet".into()))?;
        let path = Caps::default_path().ok_or_else(|| EngineError::Asio("no %APPDATA%".into()))?;
        caps.save(path).map_err(|e| EngineError::Asio(e.to_string()))
    }

    /// Takes over a new config: EQ, bypass, limiter, headroom, safety buffer and resampler
    /// quality are applied live; master driver, sample rate, capture device/mode and output
    /// channels trigger a controlled restart.
    pub fn apply(&self, config: Config) -> Result<ApplyOutcome, EngineError> {
        if lock(&self.thread).is_none() {
            return Err(EngineError::Stopped);
        }
        let new = config.sanitized();
        let rate = self.shared.live.asio_rate.load(Relaxed);
        let change = self.shared.adopt(new.clone(), rate);
        let running = self.shared.status.lock().map(|s| s.state.is_running()).unwrap_or(false);
        match change {
            ConfigChange::Unchanged => Ok(ApplyOutcome::Unchanged),
            ConfigChange::NeedsRestart(r) => {
                self.send(Cmd::Restart(r));
                Ok(ApplyOutcome::RestartScheduled(r))
            }
            ConfigChange::HotApply => {
                let n_out = self.shared.n_out.load(Relaxed) as usize;
                if running && n_out >= 2 {
                    self.shared.handoff.publish(Snapshot::from_config(&new, n_out, rate));
                    Ok(ApplyOutcome::HotApplied)
                } else {
                    // Not streaming (failed / backing off): try again with the new config.
                    self.send(Cmd::Restart("config changed"));
                    Ok(ApplyOutcome::RestartScheduled("config changed"))
                }
            }
        }
    }

    /// Starts the "Learn (30 s)" measurement: forgets the arrival statistics, measures for
    /// [`LEARN_SECS`] and publishes the lowest safe jitter headroom in `EngineStatus::learn`
    /// (the GUI stores it per capture device via `Config::set_jitter_margin` into `jitter_margin_by_device`). No effect unless the engine is streaming.
    pub fn learn_jitter(&self) {
        self.shared.learn_req.store(true, Relaxed);
        self.shared.wake.set();
    }

    /// Opens the master driver's own control panel (runs on the control thread).
    pub fn open_control_panel(&self) {
        self.send(Cmd::ControlPanel);
    }

    /// Retries opening the master right now (skips the 2 s wait while the device is busy).
    pub fn retry_now(&self) {
        self.send(Cmd::RetryNow);
    }

    /// Requests a controlled restart of the stream.
    pub fn restart(&self) {
        self.send(Cmd::Restart("restart requested"));
    }

    /// Forgets a corruption fallback and restarts the stream so fixed-period capture is tried again
    /// (a plain re-send of the same config would not reset it: only a device/period change does).
    pub fn reset_fallback(&self) {
        self.shared.reset_fallback_req.store(true, Relaxed);
        self.send(Cmd::Restart("retrying fixed capture period"));
    }

    fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
        self.shared.wake.set();
    }

    /// Stops the stream and releases the master driver. Idempotent.
    pub fn stop(&self) {
        let h = lock(&self.thread).take();
        if let Some(h) = h {
            self.send(Cmd::Stop);
            let _ = h.join();
            self.shared.set_state(EngineState::Stopped);
            lock(&self.shared.status).busy_holders.clear();
            rlog!(Level::Info, "engine stopped");
            Logger::shutdown();
        }
        lock(&self.guard).take();
        lock(&self.timer).take();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

// ------------------------------------------------------------------------------------
// Probing
// ------------------------------------------------------------------------------------

fn asio_err(call: &str, e: asio_host::HostError) -> EngineError {
    EngineError::Asio(format!("{call}: {e}"))
}

/// Reads everything the GUI wants to know about an initialised driver.
pub fn probe_driver(d: &AsioDriver, name: &str) -> Result<Caps, EngineError> {
    let (n_in, n_out) = d.channels().map_err(|e| asio_err("getChannels", e))?;
    let sizes = d.buffer_sizes().map_err(|e| asio_err("getBufferSize", e))?;
    let rate = d.sample_rate().map_err(|e| asio_err("getSampleRate", e))?;
    let (il, ol) = d.latencies().unwrap_or((0, 0));
    let supported_rates = COMMON_RATES.iter().copied().filter(|&r| d.can_sample_rate(f64::from(r))).collect();
    let names = |input: bool, n: i32| -> Vec<String> {
        (0..n.clamp(0, 64)).map(|i| d.channel_info(i, input).map(|c| c.name).unwrap_or_default()).collect()
    };
    let sample_type = d.channel_info(0, false).map(|c| c.sample_type).unwrap_or(0);
    Ok(Caps {
        master_name: name.to_string(),
        driver_version: d.driver_version(),
        sample_rate: rate.round() as u32,
        supported_rates,
        buffer_min: sizes.min,
        buffer_preferred: sizes.preferred,
        buffer_max: sizes.max,
        buffer_granularity: sizes.granularity,
        input_latency: il,
        output_latency: ol,
        in_channels: n_in.max(0) as u32,
        out_channels: n_out.max(0) as u32,
        in_names: names(true, n_in),
        out_names: names(false, n_out),
        sample_type,
        updated_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
    })
}

/// Briefly opens `master` (empty = default), reads its caps and releases it again.
/// Initialises COM (STA) on the calling thread. Fails if the driver is in use.
pub fn probe_master(master: &str) -> Result<Caps, EngineError> {
    asio_host::init_com_sta();
    let name = if master.trim().is_empty() { crate::logic::DEFAULT_MASTER.to_string() } else { master.trim().to_string() };
    if is_forbidden_master(&name) {
        return Err(EngineError::ForbiddenMaster(name));
    }
    let driver = default_master_loader(&name)?;
    probe_driver(&driver, &name)
}

// ------------------------------------------------------------------------------------
// Session: one bring-up of driver + capture + pipeline
// ------------------------------------------------------------------------------------

struct SessionInfo {
    master_name: String,
    cable_rate: u32,
    asio_rate: u32,
    buffer: u32,
    capture_period: u32,
    capture_device: String,
    capture_requested: wasapi_capture::CaptureMode,
    /// Fixed period the config asked for (frames; 0 = driver default).
    period_requested: u32,
    outputs: [usize; 2],
    ring_capacity: u32,
}

struct Session {
    driver: Option<AsioDriver>,
    buffers_created: bool,
    streaming: bool,
    capture: Option<CaptureSession>,
    /// Silent render stream on the cable's render endpoint (stopped before `capture`).
    keepalive: Option<RenderKeepAlive>,
    keepalive_status: Option<KeepAliveStatus>,
    synth: Option<SynthHandle>,
    watchdog: Option<Watchdog>,
    cfg_watcher: Option<ConfigWatcher>,
    tap: Option<TapSession>,
    heartbeat: Arc<Heartbeat>,
    armed: Arc<AtomicBool>,
    status_writer: Option<Arc<StatusWriter>>,
    info: SessionInfo,
    caps: Caps,
    started: Instant,
    /// Arrival statistics of the capture sink and the planner that turns them into the headroom.
    arrival: Arc<ArrivalStats>,
    planner: HeadroomPlanner,
    /// Engine time at which the running "Learn" window started.
    learn_started: Option<Duration>,
    /// Stored `router-cli latency` results (empty when not loaded): the render side of the latency readout.
    measured: MeasuredLatency,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.heartbeat.set_running(false);
        self.armed.store(false, Relaxed);
        if let Some(w) = self.watchdog.as_mut() {
            w.shutdown();
        }
        if let Some(mut d) = self.driver.take() {
            if self.streaming {
                if let Err(e) = d.stop() {
                    rlog!(Level::Warn, "ASIO stop failed: {e}");
                }
            }
            if self.buffers_created {
                if let Err(e) = d.dispose_buffers() {
                    rlog!(Level::Warn, "ASIO disposeBuffers failed: {e}");
                }
            }
            drop(d); // Release the Yamaha before the capture side goes away.
        }
        if let Some(mut k) = self.keepalive.take() {
            k.stop(); // before the capture, see the module docs
        }
        if let Some(mut c) = self.capture.take() {
            c.stop();
        }
        if let Some(mut y) = self.synth.take() {
            y.stop();
        }
        if let Some(w) = self.cfg_watcher.as_mut() {
            w.shutdown();
        }
        if let Some(t) = self.tap.as_mut() {
            t.finish();
        }
        if let Some(sw) = &self.status_writer {
            sw.set_running(false);
        }
    }
}

/// Applies `match_cable_rate`: sets the cable endpoints to `rate` when they differ and records the
/// outcome in the status. Runs once per start; a failed attempt for the same rate is not
/// repeated within 60 s. Failure is non-fatal (the resampler copes with the mismatch).
fn run_cable_match(sh: &Shared, cfg: &Config, rate: u32) {
    if !cfg.match_cable_rate || !cable::is_cable_name(&cfg.capture_device) || rate == 0 {
        let mut st = lock(&sh.status);
        st.cable_rate_changed_from = None;
        st.cable_rate_note = None;
        return;
    }
    let now = Instant::now();
    let last = *lock(&sh.cable_attempt);
    if !cable::should_attempt(last, rate, now) {
        rlog!(Level::Info, "cable rate matching to {rate} Hz skipped (failed recently)");
        return;
    }
    let base = wasapi_capture::cable_base_name(&cfg.capture_device);
    let provider: Arc<dyn cable::CableFormats> = match &sh.opts.cable_formats {
        Some(h) => h.0.clone(),
        None => Arc::new(WindowsCableFormats),
    };
    let out = cable::match_cable(&*provider, &base, rate, Duration::from_millis(1500), Duration::from_millis(100));
    if let Some(note) = &out.note {
        rlog!(if out.failed { Level::Warn } else { Level::Info }, "{note}");
    }
    *lock(&sh.cable_attempt) = if out.failed { Some((rate, now, false)) } else { None };
    if out.note.is_some() {
        let mut st = lock(&sh.status);
        st.cable_rate_changed_from = out.changed_from;
        st.cable_rate_note = out.note;
    }
}

/// Why Exclusive capture is not offered (shown in the status panel and logged).
pub const EXCLUSIVE_DISABLED_NOTE: &str = "Exclusive capture is disabled: the Hi-Fi Cable driver duplicates/drops packets in exclusive mode (30-70 % of the samples were wrong at 3-5 ms periods; only 10 ms periods were clean, which is no faster than Shared). Using Shared capture instead.";

/// Maps the configured mode to the one actually opened. Exclusive is mapped to Shared
/// (measured sample-inaccurate on the Hi-Fi Cable); the returned note explains it.
pub fn effective_capture_mode(m: eq_core::CaptureMode) -> (wasapi_capture::CaptureMode, Option<&'static str>) {
    match m {
        eq_core::CaptureMode::Shared => (wasapi_capture::CaptureMode::Shared, None),
        eq_core::CaptureMode::LowLatency => (wasapi_capture::CaptureMode::LowLatency, None),
        eq_core::CaptureMode::Exclusive => (wasapi_capture::CaptureMode::Shared, Some(EXCLUSIVE_DISABLED_NOTE)),
    }
}

/// Engine period `(frames, rate)` as milliseconds.
fn period_ms(p: Option<(u32, u32)>) -> Option<f32> {
    p.filter(|&(_, r)| r > 0).map(|(f, r)| (f64::from(f) * 1000.0 / f64::from(r)) as f32)
}

/// Starts the render keep-alive if the config asks for it and the capture endpoint is a known
/// virtual cable. Never fatal: a failure only produces a note.
fn start_keepalive(cfg: &Config, capture_name: &str) -> (Option<RenderKeepAlive>, Option<KeepAliveStatus>, Option<String>) {
    if !cfg.render_keepalive {
        return (None, None, None);
    }
    if !is_virtual_cable_name(capture_name) {
        let n = format!("Render keep-alive skipped: \"{capture_name}\" is not a known virtual cable.");
        rlog!(Level::Warn, "{n}");
        return (None, None, Some(n));
    }
    match RenderKeepAlive::start_for_capture_endpoint(capture_name, RenderKeepAliveOptions::default()) {
        Ok(k) => {
            let i = k.info();
            let st = KeepAliveStatus {
                render_name: i.render_name.clone(),
                engine_period_before_ms: period_ms(i.engine_period_before),
                engine_period_after_ms: period_ms(i.engine_period_after),
            };
            rlog!(
                Level::Info,
                "render keep-alive on \"{}\": engine period {:?} -> {:?} ms",
                st.render_name,
                st.engine_period_before_ms,
                st.engine_period_after_ms
            );
            (Some(k), Some(st), None)
        }
        Err(e) => {
            let n = format!("Render keep-alive not started: {e}");
            rlog!(Level::Warn, "{n}");
            (None, None, Some(n))
        }
    }
}

impl Session {
    fn open(
        sh: &Arc<Shared>,
        cfg: &Config,
        tx: &Sender<Cmd>,
        safety_ms: f32,
        fallback: &FallbackState,
    ) -> Result<Session, EngineError> {
        let name = effective_master(cfg);
        if is_forbidden_master(&name) {
            return Err(EngineError::ForbiddenMaster(name));
        }
        let driver = match &sh.opts.master_loader {
            Some(l) => (l.0)(&name)?,
            None => default_master_loader(&name)?,
        };
        // From here on `session` owns the driver: any early return releases it in order.
        let (n_in, n_out) = driver.channels().map_err(|e| asio_err("getChannels", e))?;
        let _ = n_in;
        if n_out < 2 {
            return Err(EngineError::TooFewOutputs { found: n_out });
        }

        // Sample rate (before createBuffers).
        if cfg.sample_rate != 0 {
            let want = f64::from(cfg.sample_rate);
            let cur = driver.sample_rate().unwrap_or(0.0);
            if (cur - want).abs() > 0.5 {
                if driver.can_sample_rate(want) {
                    if let Err(e) = driver.set_sample_rate(want) {
                        rlog!(Level::Warn, "setSampleRate({}) failed: {e}; keeping {cur} Hz", cfg.sample_rate);
                    }
                } else {
                    rlog!(Level::Warn, "master does not support {} Hz; keeping {cur} Hz", cfg.sample_rate);
                }
            }
        }
        let mut caps = probe_driver(&driver, &name)?;
        let asio_rate = caps.sample_rate;
        let buffer = choose_buffer_size(caps.buffer_min, caps.buffer_max, caps.buffer_preferred).max(1) as u32;
        let outputs = validate_output_channels(cfg.output_channels, n_out as usize);
        let mut types = [0i32; 2];
        for (i, &o) in outputs.iter().enumerate() {
            let ci = driver.channel_info(o as i32, false).map_err(|e| asio_err("getChannelInfo", e))?;
            if !router_core::asio_convert::is_supported(ci.sample_type) {
                return Err(EngineError::UnsupportedSampleType(ci.sample_type));
            }
            types[i] = ci.sample_type;
        }
        if let Some(p) = Caps::default_path().filter(|_| sh.opts.publish_caps) {
            if let Err(e) = caps.save(&p) {
                rlog!(Level::Warn, "cannot write caps: {e}");
            }
        }
        *lock(&sh.caps) = Some(caps.clone());

        // Make the Hi-Fi Cable run at the master's rate (once per start, never fatal).
        run_cable_match(sh, cfg, asio_rate);

        // Capture side.
        let synthetic = sh.opts.synthetic.clone();
        let (ep_id, ep_name, hint_rate) = match &synthetic {
            Some(sy) => (String::new(), "synthetic source".to_string(), sy.rate),
            None => {
                let ep = wasapi_capture::find_capture_device(Some(&cfg.capture_device)).map_err(|e| match e {
                    wasapi_capture::CaptureError::NotFound(_) => EngineError::CaptureDeviceNotFound(cfg.capture_device.clone()),
                    other => EngineError::Capture(other.to_string()),
                })?;
                let hint = wasapi_capture::mix_format_sample_rate(&ep.id).unwrap_or(48000).max(8000);
                (ep.id.clone(), ep.name.clone(), hint)
            }
        };
        let capacity = plan_capacity(buffer, hint_rate, asio_rate, MAX_PERIOD_FRAMES);
        let (producer, consumer) = ring::channel(capacity);
        let ring_capacity = producer.capacity() as u32;
        let heartbeat = Arc::new(Heartbeat::new());
        let armed = Arc::new(AtomicBool::new(false));
        let live = sh.live.clone();
        live.reset_counters();
        let plan = plan_capture(cfg, &ep_name, hint_rate, fallback.forced_shared());
        let requested = plan.options.mode;
        if let Some(n) = &plan.note {
            rlog!(Level::Warn, "{n}");
        }
        {
            let mut st = lock(&sh.status);
            st.capture_note = plan.note.clone();
            st.capture_fallback = fallback.text().map(str::to_string);
            st.capture_period_frames_requested = plan.requested_period_frames;
            st.source_preset = cfg.source_preset;
            st.keepalive = None;
            st.keepalive_note = None;
        }
        let (tap_pts, tap_ses) = match &sh.opts.tap {
            Some(c) => match tap::start(c, asio_rate) {
                Ok((p, s)) => (Some(p), Some(s)),
                Err(e) => {
                    rlog!(Level::Warn, "measurement tap not started: {e}");
                    (None, None)
                }
            },
            None => (None, None),
        };
        let (tap_cap, tap_rs, tap_eq) = match tap_pts {
            Some(p) => (Some(p.capture_in), Some(p.after_resample), Some(p.after_eq)),
            None => (None, None, None),
        };
        let arrival = ArrivalStats::new(f64::from(hint_rate));
        let sink = CaptureSink::new(producer, heartbeat.clone(), armed.clone(), live.clone())
            .with_tap(tap_cap)
            .with_arrival(Some(arrival.clone()))
            .with_probe(sh.probe.clone());
        let (capture, synth_handle, cable_rate, period, mode_in_use) = match synthetic {
            Some(sy) => {
                let (rate, period) = (sy.rate, sy.packet_frames);
                let h = synth::spawn(sy, Box::new(sink)).map_err(|e| EngineError::Capture(e.to_string()))?;
                (None, Some(h), rate, period, None)
            }
            None => {
                let options = wasapi_capture::CaptureOptions { preferred_rate: Some(asio_rate), ..plan.options };
                let c = CaptureSession::start_with_options(&ep_id, options, Box::new(sink))
                    .map_err(|e| EngineError::Capture(e.to_string()))?;
                let (r, p, m) = (c.sample_rate(), c.period_frames(), c.mode_in_use());
                (Some(c), None, r, p, Some(m))
            }
        };
        if let Some(t) = &tap_ses {
            t.set_cable_rate(cable_rate);
        }
        arrival.set_rate(f64::from(cable_rate));
        rlog!(
            Level::Info,
            "capture \"{}\" {} Hz, period {} frames, mode {:?} (requested {:?})",
            ep_name,
            cable_rate,
            period,
            mode_in_use,
            requested
        );

        // Render keep-alive (declared after `capture`, so an early return drops it first).
        let (keepalive, keepalive_status, keepalive_note) = if capture.is_some() {
            start_keepalive(cfg, &ep_name)
        } else {
            (None, None, None)
        };
        lock(&sh.status).keepalive_note = keepalive_note;

        let mut session = Session {
            driver: Some(driver),
            buffers_created: false,
            streaming: false,
            capture,
            keepalive,
            keepalive_status,
            synth: synth_handle,
            watchdog: None,
            cfg_watcher: None,
            tap: tap_ses,
            heartbeat: heartbeat.clone(),
            armed: armed.clone(),
            status_writer: None,
            info: SessionInfo {
                master_name: name.clone(),
                cable_rate,
                asio_rate,
                buffer,
                capture_period: period,
                capture_device: ep_name.clone(),
                capture_requested: requested,
                period_requested: plan.requested_period_frames,
                outputs,
                ring_capacity,
            },
            caps: caps.clone(),
            started: Instant::now(),
            arrival,
            planner: HeadroomPlanner::new(0, 0),
            learn_started: None,
            measured: if sh.opts.load_measured { MeasuredLatency::load_default() } else { MeasuredLatency::default() },
        };

        // Pipeline.
        // Start-up headroom: the learned margin if there is one, else the conservative two packets
        // (the former fixed burst rule); the status tick then measures and ramps it down.
        let learned =
            (f64::from(cfg.jitter_margin_for(&cfg.capture_device)) * f64::from(cable_rate) / 1000.0).ceil() as usize;
        // Fixed-period capture of a virtual cable delivers small regular packets: start at 1.5 x period
        // instead of the two-packet burst of the Hi-Fi Cable (see `initial_headroom_frames`).
        let fixed = mode_in_use == Some(wasapi_capture::CaptureMode::FixedPeriod);
        let start_headroom =
            if learned > 0 { learned.max(min_headroom_frames(period)) } else { initial_headroom_frames(period, fixed) };
        session.planner = HeadroomPlanner::new(start_headroom, learned);
        let target = plan_target(safety_ms, buffer, cable_rate, asio_rate, start_headroom as u32);
        let matcher = RateMatcher::new(RateMatcherConfig::new(cable_rate, asio_rate, target, cfg.resampler_quality));
        drop(sh.handoff.take()); // stale snapshot from before this start
        let eq = EqProcessor::new(f64::from(asio_rate), cfg, n_out as usize, sh.handoff.clone());
        let status_writer = if sh.opts.status_mapping { StatusWriter::create_mapped().map(Arc::new) } else { None };
        session.status_writer = status_writer.clone();
        live.asio_rate.store(asio_rate, Relaxed);
        live.target_frames.store(target as u32, Relaxed);
        live.open_bias.store(open_bias(start_headroom), Relaxed);
        sh.n_out.store(n_out as u32, Relaxed);
        let pipe = Pipeline::new(PipelineInit {
            consumer,
            matcher,
            eq,
            frames: buffer as usize,
            asio_rate,
            tick_hz: qpc_freq(),
            live: live.clone(),
            status: status_writer.clone(),
            heartbeat: heartbeat.clone(),
            target_frames: target as u32,
            quality: cfg.resampler_quality,
            tap_resample: tap_rs,
            tap_eq,
        });
        let ptrs = Arc::new(PtrTable::default());
        let out = OutputSet { ptrs: ptrs.clone(), types };

        let drv = session.driver.as_mut().ok_or_else(|| EngineError::Asio("driver missing".into()))?;
        let out_ready = Some(drv.output_ready_handle());
        // Latency probe: the loopback input pair is requested only when asked for.
        let mut probe_inputs: Option<([usize; 2], [i32; 2], [String; 2])> = None;
        if let (Some(lp), Some(_)) = (&sh.opts.loopback_probe, &sh.probe) {
            let chans = select_input_channels(&caps.in_names, lp.input_channels).map_err(EngineError::Asio)?;
            let mut in_types = [0i32; 2];
            for (k, &c) in chans.iter().enumerate() {
                let ci = drv.channel_info(c as i32, true).map_err(|e| asio_err("getChannelInfo(input)", e))?;
                if !router_core::asio_convert::is_supported(ci.sample_type) {
                    return Err(EngineError::UnsupportedSampleType(ci.sample_type));
                }
                in_types[k] = ci.sample_type;
            }
            let names = [caps.in_names.get(chans[0]).cloned().unwrap_or_default(), caps.in_names.get(chans[1]).cloned().unwrap_or_default()];
            rlog!(Level::Info, "latency probe: loopback inputs {} \"{}\" and {} \"{}\"", chans[0], names[0], chans[1], names[1]);
            probe_inputs = Some((chans, in_types, names));
        }
        let mut cb = AudioCallback::new(pipe, OutputSet { ptrs: ptrs.clone(), types }, out_ready, live.clone());
        let in_ptrs = Arc::new(PtrTable::default());
        let mut bufs = vec![(false, outputs[0] as i32), (false, outputs[1] as i32)];
        if let (Some((chans, in_types, _)), Some(shared)) = (&probe_inputs, &sh.probe) {
            bufs.push((true, chans[0] as i32));
            bufs.push((true, chans[1] as i32));
            cb = cb.with_loopback(Some(LoopbackTap::new(in_ptrs.clone(), *in_types, buffer as usize, asio_rate, shared.clone())));
        }
        drv.create_buffers(&bufs, buffer as i32, Box::new(cb)).map_err(|e| asio_err("createBuffers", e))?;
        session.buffers_created = true;
        for ch in 0..2 {
            ptrs.set(ch, drv.buffer_ptrs(ch));
        }
        if probe_inputs.is_some() {
            for k in 0..2 {
                in_ptrs.set(k, drv.buffer_ptrs(2 + k));
            }
        }
        // Start from silence in both halves.
        // SAFETY: pointers come from the driver's createBuffers for `buffer` samples.
        unsafe {
            out.silence(0, buffer as usize);
            out.silence(1, buffer as usize);
        }
        // Latencies are only reliable after createBuffers.
        if let Ok((il, ol)) = drv.latencies() {
            caps.input_latency = il;
            caps.output_latency = ol;
            session.caps = caps.clone();
            *lock(&sh.caps) = Some(caps.clone());
            if let Some(p) = Caps::default_path().filter(|_| sh.opts.publish_caps) {
                let _ = caps.save(p);
            }
        }
        if let (Some((chans, _, names)), Some(shared)) = (&probe_inputs, &sh.probe) {
            shared.set_info(ProbeInfo {
                asio_rate,
                cable_rate,
                buffer_frames: buffer,
                input_latency: caps.input_latency,
                output_latency: caps.output_latency,
                input_channels: *chans,
                input_names: names.clone(),
                capture_period_frames: period,
            });
        }
        if let Some(sw) = &status_writer {
            sw.init(&StaticInfo {
                sample_rate: asio_rate,
                buffer_size: buffer,
                in_latency: caps.input_latency.max(0) as u32,
                out_latency: caps.output_latency.max(0) as u32,
                pid: std::process::id(),
                effective_rate: asio_rate,
                capture_period_us: (f64::from(period) * 1e6 / f64::from(cable_rate.max(1))) as u32,
                safety_buffer_us: (safety_ms * 1000.0) as u32,
            });
        }

        let (sh2, tx2) = (sh.clone(), tx.clone());
        session.watchdog = Watchdog::spawn(heartbeat.clone(), move |ev| {
            let _ = tx2.send(Cmd::Watchdog(ev));
            sh2.wake.set();
        })
        .map_err(|e| rlog!(Level::Warn, "watchdog not started: {e}"))
        .ok();
        session.cfg_watcher =
            ConfigWatcher::spawn(sh.opts.config_path.clone(), sh.handoff.clone(), n_out as usize, live.asio_rate.clone());

        drv.start().map_err(|e| asio_err("start", e))?;
        session.streaming = true;
        if let Some(sw) = &status_writer {
            sw.set_running(true);
        }
        heartbeat.set_running(true);
        armed.store(true, Relaxed);
        session.started = Instant::now();
        rlog!(
            Level::Info,
            "streaming: {} Hz asio / {} Hz cable, buffer {}, target fill {} frames, outputs {:?}",
            asio_rate,
            cable_rate,
            buffer,
            target,
            outputs
        );
        Ok(session)
    }
}

// ------------------------------------------------------------------------------------
// Control thread
// ------------------------------------------------------------------------------------

enum RunEnd {
    Stop,
    Restart(String),
}

fn control_thread(sh: Arc<Shared>, rx: Receiver<Cmd>, tx: Sender<Cmd>, ready: mpsc::SyncSender<Result<(), EngineError>>) {
    asio_host::init_com_sta();
    let _mmcss = ControlMmcss::register();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| control_loop(&sh, &rx, &tx, &ready)));
    if r.is_err() {
        rlog!(Level::Error, "control thread panicked");
        let mut st = lock(&sh.status);
        st.state = EngineState::Failed;
        st.last_error = Some("internal error: control thread panicked".into());
        drop(st);
        let _ = ready.try_send(Err(EngineError::Thread("control thread panicked".into())));
    }
}

/// Waits `dur` (None = until a command arrives) while pumping messages.
/// Returns `Some(end)` if a command ends the wait.
fn wait_for(sh: &Shared, rx: &Receiver<Cmd>, dur: Option<Duration>) -> Option<RunEnd> {
    let deadline = dur.map(|d| Instant::now() + d);
    loop {
        while let Ok(c) = rx.try_recv() {
            match c {
                Cmd::Stop => return Some(RunEnd::Stop),
                Cmd::Restart(r) => return Some(RunEnd::Restart(r.to_string())),
                Cmd::RetryNow => return Some(RunEnd::Restart("retry requested".to_string())),
                Cmd::ControlPanel | Cmd::Watchdog(_) => {}
            }
        }
        let ms = match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return None;
                }
                (left.as_millis() as u32).min(WAKE_MS)
            }
            None => WAKE_MS,
        };
        pump_wait(&sh.wake, ms);
    }
}

/// True if `e` should be retried with back-off instead of failing (see
/// [`EngineOptions::retry_unavailable`]).
fn retry_wanted(e: &EngineError, retry_unavailable: bool) -> bool {
    retry_unavailable && e.is_unavailable()
}

fn control_loop(sh: &Arc<Shared>, rx: &Receiver<Cmd>, tx: &Sender<Cmd>, ready: &mpsc::SyncSender<Result<(), EngineError>>) {
    let t0 = Instant::now();
    let mut backoff = Backoff::standard();
    let mut auto = AutoSafety::default();
    let mut first = true;
    let mut holders_seen = HolderTracker::default();
    // Persists across restarts: set once fixed-period capture was abandoned (until the capture
    // device / period changes), so a corrupting source is reopened in Shared exactly once.
    let mut fallback = FallbackState::default();
    loop {
        if !lock(&sh.status).state.is_waiting() {
            sh.set_state(EngineState::Starting);
        }
        let cfg = lock(&sh.config).clone();
        if sh.reset_fallback_req.swap(false, Relaxed) {
            fallback.reset();
            lock(&sh.status).capture_fallback = None;
        }
        if fallback.note_config(&cfg) {
            rlog!(Level::Info, "capture source settings changed: fixed-period capture is allowed again");
            lock(&sh.status).capture_fallback = None;
        }
        let base = cfg.safety_buffer_ms;
        auto.new_stream(t0.elapsed());
        let safety = if cfg.safety_buffer_auto { (base + auto.extra_ms()).min(crate::logic::MAX_SAFETY_MS) } else { base };
        match Session::open(sh, &cfg, tx, safety, &fallback) {
            Ok(mut sess) => {
                if first {
                    first = false;
                    let _ = ready.send(Ok(()));
                }
                {
                    let mut st = lock(&sh.status);
                    st.state = EngineState::Running;
                    st.peak_in_db = [-200.0; 2];
                    st.peak_out_db = [-200.0; 2];
                    st.last_error = None;
                    st.capture_error = None;
                    st.busy_holders.clear();
                }
                holders_seen.reset();
                let end = run_session(sh, rx, &mut sess, &mut auto, t0, &mut fallback);
                let uptime = sess.started.elapsed();
                drop(sess);
                backoff.note_run_ended(uptime);
                match end {
                    RunEnd::Stop => return,
                    RunEnd::Restart(reason) => {
                        rlog!(Level::Warn, "restarting: {reason}");
                        let d = backoff.next_delay();
                        {
                            let mut st = lock(&sh.status);
                            st.state = EngineState::Restarting { reason: reason.clone() };
                            st.restarts += 1;
                            st.last_error = Some(reason);
                        }
                        if let Some(RunEnd::Stop) = wait_for(sh, rx, Some(d)) {
                            return;
                        }
                    }
                }
            }
            Err(EngineError::MasterBusy { name, holders }) => {
                // Not an error to give up on: wait for the other program to let go.
                let text = crate::status::busy_message(&name, &holders);
                let infos: Vec<HolderInfo> = holders.iter().map(HolderInfo::from).collect();
                let pids: Vec<u32> = infos.iter().map(|h| h.pid).collect();
                if holders_seen.changed(&pids) {
                    rlog!(Level::Warn, "waiting for master: {text}");
                }
                {
                    let mut st = lock(&sh.status);
                    st.state = EngineState::WaitingForDevice { holders: infos.clone() };
                    st.busy_holders = infos;
                    st.last_error = Some(text);
                }
                if first {
                    first = false;
                    let _ = ready.send(Ok(()));
                }
                if let Some(RunEnd::Stop) = wait_for(sh, rx, Some(WAIT_RETRY)) {
                    return;
                }
            }
            Err(e) => {
                rlog!(Level::Error, "start failed: {e}");
                holders_seen.reset();
                lock(&sh.status).busy_holders.clear();
                let retry = retry_wanted(&e, sh.opts.retry_unavailable);
                if first {
                    if !retry {
                        let _ = ready.send(Err(e.clone()));
                        return;
                    }
                    first = false;
                    let _ = ready.send(Ok(()));
                }
                let msg = if retry { e.waiting_text() } else { e.to_string() };
                {
                    let mut st = lock(&sh.status);
                    st.last_error = Some(msg.clone());
                }
                if e.is_fatal() && !retry {
                    sh.set_state(EngineState::Failed);
                    match wait_for(sh, rx, None) {
                        Some(RunEnd::Stop) => return,
                        _ => continue,
                    }
                }
                let d = backoff.next_delay();
                {
                    let mut st = lock(&sh.status);
                    st.state = EngineState::Restarting { reason: msg };
                    st.restarts += 1;
                }
                if let Some(RunEnd::Stop) = wait_for(sh, rx, Some(d)) {
                    return;
                }
            }
        }
    }
}

fn reset_reason_text(code: u8) -> &'static str {
    match code {
        RESET_REQUEST => "driver reset request (buffer size / configuration changed)",
        RESET_RATE => "driver sample rate changed",
        RESET_PANIC => "audio callback panicked",
        _ => "driver reset",
    }
}

fn run_session(
    sh: &Arc<Shared>,
    rx: &Receiver<Cmd>,
    sess: &mut Session,
    auto: &mut AutoSafety,
    t0: Instant,
    fallback: &mut FallbackState,
) -> RunEnd {
    let mut stalls = StallTracker::default();
    let mut next_tick = Instant::now() + TICK;
    let mut next_file = Instant::now() + FILE_POLL;
    let mut last_mtime = sh.opts.config_path.as_deref().and_then(eq_core::modified);
    let mut last_target = sh.live.target_frames.load(Relaxed);
    let live = sh.live.clone();
    loop {
        pump_wait(&sh.wake, WAKE_MS);
        let now = t0.elapsed();
        while let Ok(c) = rx.try_recv() {
            match c {
                Cmd::Stop => return RunEnd::Stop,
                Cmd::Restart(r) => return RunEnd::Restart(r.to_string()),
                Cmd::RetryNow => {}
                Cmd::ControlPanel => {
                    if let Some(d) = sess.driver.as_ref() {
                        if let Err(e) = d.control_panel() {
                            rlog!(Level::Warn, "controlPanel failed: {e}");
                        }
                    }
                }
                Cmd::Watchdog(ev) => {
                    rlog!(Level::Warn, "watchdog: {ev:?}");
                    match ev {
                        WatchdogEvent::Stalled(Component::Audio) => stalls.stalled(0, now),
                        WatchdogEvent::Stalled(Component::Capture) => stalls.stalled(1, now),
                        WatchdogEvent::Recovered(Component::Audio) => stalls.recovered(0),
                        WatchdogEvent::Recovered(Component::Capture) => stalls.recovered(1),
                    }
                }
            }
        }
        if live.reset_requested.load(Relaxed) {
            return RunEnd::Restart(reset_reason_text(live.reset_reason.load(Relaxed)).to_string());
        }
        if let Some(i) = stalls.due(now) {
            return RunEnd::Restart(if i == 0 { "audio callback stalled" } else { "capture stalled" }.to_string());
        }
        if let Some(cap) = sess.capture.as_ref() {
            while let Ok(ev) = cap.events().try_recv() {
                match ev {
                    CaptureEvent::Started { .. } => {}
                    CaptureEvent::Fallback { requested, using, reason } => {
                        rlog!(Level::Warn, "capture fallback {requested:?} -> {using:?}: {reason}");
                        let msg = format!("Capture mode {} -> {}: {reason}", requested.name(), using.name());
                        let mut st = lock(&sh.status);
                        st.capture_note = Some(match st.capture_note.take() {
                            Some(old) if !old.is_empty() => format!("{old} {msg}"),
                            _ => msg,
                        });
                    }
                    CaptureEvent::DeviceLost => {
                        return RunEnd::Restart("capture device lost".to_string());
                    }
                    CaptureEvent::Error(e) => {
                        lock(&sh.status).capture_error = Some(e.clone());
                        return RunEnd::Restart(format!("capture error: {e}"));
                    }
                    // The monitor stopped a fixed-period capture: restart ONCE in plain Shared and tell the user.
                    CaptureEvent::Corruption(r) => match fallback.on_corruption(cap.mode_in_use(), &r.to_string()) {
                        CorruptionAction::RestartInShared(note) => {
                            rlog!(Level::Warn, "{note}");
                            lock(&sh.status).capture_fallback = Some(note.clone());
                            return RunEnd::Restart(note);
                        }
                        CorruptionAction::Ignore => {
                            rlog!(Level::Warn, "capture corruption reported in {:?}: {r} (no restart)", cap.mode_in_use());
                            lock(&sh.status).capture_error = Some(format!("capture corruption: {r}"));
                        }
                    },
                }
            }
            if !cap.is_running() {
                return RunEnd::Restart("capture thread ended".to_string());
            }
        }
        let t = Instant::now();
        if t >= next_file {
            next_file = t + FILE_POLL;
            if let Some(p) = sh.opts.config_path.as_deref() {
                let m = eq_core::modified(p);
                if m != last_mtime {
                    last_mtime = m;
                    if m.is_some() {
                        if let Ok(c) = Config::load(p) {
                            let rate = live.asio_rate.load(Relaxed);
                            // EQ-class changes are published by the ConfigWatcher; only
                            // restart-class ones need action here.
                            if let ConfigChange::NeedsRestart(r) = sh.adopt(c.sanitized(), rate) {
                                return RunEnd::Restart(r.to_string());
                            }
                        }
                    }
                }
            }
        }
        if t >= next_tick {
            next_tick = t + TICK;
            tick(sh, sess, auto, now, &mut last_target);
        }
    }
}

/// Start-up gate bias for a headroom: the measured dip is the headroom without its 25 % margin.
fn open_bias(headroom: usize) -> u32 {
    (headroom as f64 / DIP_MARGIN).round() as u32
}

/// 100 ms status publisher + auto safety buffer + measured-headroom planner + "Learn".
fn tick(sh: &Arc<Shared>, sess: &mut Session, auto: &mut AutoSafety, now: Duration, last_target: &mut u32) {
    let live = &sh.live;
    let i = &sess.info;
    // Auto safety.
    let base = f32::from_bits(sh.base_safety.load(Relaxed));
    let underruns = live.underruns.load(Relaxed);
    let safety = if sh.safety_auto.load(Relaxed) {
        auto.update(base, underruns, now)
    } else {
        base
    };
    // Measured arrival model -> jitter headroom (see `router_core::arrival`).
    let callbacks = live.callbacks.load(Relaxed);
    let snap = sess.arrival.snapshot();
    let params = HeadroomParams { period_frames: i.capture_period, cap_frames: MAX_PERIOD_FRAMES };
    let required = required_headroom_frames(&snap, &params);
    let learned_floor =
        (f64::from(f32::from_bits(sh.jitter_floor_ms.load(Relaxed))) * f64::from(i.cable_rate) / 1000.0).ceil() as usize;
    sess.planner.set_floor(learned_floor);
    let mean_fill = f32::from_bits(live.fill_mean.load(Relaxed));
    // Lower only once the drift controller has pulled the mean fill up to the current target.
    let may_lower = callbacks > 300 && mean_fill + 64.0 >= *last_target as f32;
    let headroom = sess.planner.update(now.as_secs_f64(), &snap, &params, may_lower);
    let target = plan_target(safety, i.buffer, i.cable_rate, i.asio_rate, headroom as u32) as u32;
    if target != *last_target {
        live.target_frames.store(target, Relaxed);
        live.open_bias.store(open_bias(headroom), Relaxed);
        *last_target = target;
    }
    // "Learn (30 s)".
    if sh.learn_req.swap(false, Relaxed) && sess.learn_started.is_none() {
        sess.arrival.request_reset();
        sess.planner.note_reset();
        sess.learn_started = Some(now);
        let mut st = lock(&sh.status);
        let prev = st.learn.clone();
        st.learn = LearnStatus { active: true, progress_s: 0.0, total_s: LEARN_SECS as f32, error: None, ..prev };
        rlog!(Level::Info, "learn: measuring capture arrival jitter for {LEARN_SECS} s");
    }
    if let Some(t0) = sess.learn_started {
        let el = now.saturating_sub(t0).as_secs_f64();
        let mut st = lock(&sh.status);
        st.learn.progress_s = el.min(LEARN_SECS) as f32;
        if el >= LEARN_SECS {
            sess.learn_started = None;
            st.learn.active = false;
            st.learn.finished += 1;
            if snap.events >= LEARN_MIN_EVENTS {
                let ms = required as f64 * 1000.0 / f64::from(i.cable_rate.max(1));
                st.learn.result_ms = Some(((ms * 10.0).ceil() / 10.0) as f32);
                st.learn.error = None;
                rlog!(
                    Level::Info,
                    "learn: {} events, dip p99.9 {} fr, gap p99.9 {:.1} ms -> headroom {} fr ({ms:.1} ms)",
                    snap.events,
                    snap.dip_p999,
                    snap.gap_p999_ms(),
                    required
                );
            } else {
                st.learn.result_ms = None;
                st.learn.error =
                    Some(format!("only {} capture events in {LEARN_SECS} s: is audio playing into the cable?", snap.events));
                rlog!(Level::Warn, "learn failed: {:?}", st.learn.error);
            }
        }
    }
    let peak = |a: &std::sync::atomic::AtomicU32| to_db(f32::from_bits(a.swap(0, Relaxed)));
    let peak_in = [peak(&live.peak_in[0]), peak(&live.peak_in[1])];
    let peak_out = [peak(&live.peak_out[0]), peak(&live.peak_out[1])];
    let period_us = f64::from(i.buffer) * 1e6 / f64::from(i.asio_rate.max(1));
    let avg = f32::from_bits(live.cb_avg_us.load(Relaxed));
    let max = f32::from_bits(live.cb_max_us.load(Relaxed));
    let period_ms = frames_to_ms(f64::from(i.capture_period), i.cable_rate);
    // Honest latency terms: the mean actual fill (the target until a mean exists) and the measured
    // mean staleness of captured frames (half a period if there are no statistics yet).
    let have_mean = callbacks > 300 && mean_fill > 0.0;
    let ring_ms = frames_to_ms(if have_mean { f64::from(mean_fill) } else { f64::from(target) }, i.cable_rate);
    let staleness_ms = if snap.events >= 50 { snap.staleness_s * 1000.0 } else { period_ms / 2.0 };
    let mut caps = sess.caps.clone();
    caps.sample_rate = i.asio_rate;
    let mode_in_use = sess.capture.as_ref().map(|c| c.mode_in_use());
    let cap_stats = sess.capture.as_ref().map(|c| c.stats().snapshot());
    if let Some(sw) = &sess.status_writer {
        sw.set_safety_buffer_us((safety * 1000.0) as u32);
        sw.set_dropped_logs(Logger::dropped());
    }
    sh.handoff.collect();
    // A keep-alive whose render endpoint went away is dropped (non-fatal).
    let mut ka_note = None;
    if sess.keepalive.as_ref().is_some_and(|k| k.is_lost() || !k.is_running()) {
        if let Some(mut k) = sess.keepalive.take() {
            k.stop();
        }
        sess.keepalive_status = None;
        ka_note = Some("Render keep-alive stopped: its render endpoint was lost.".to_string());
        rlog!(Level::Warn, "render keep-alive stopped (endpoint lost)");
    }
    let cfg_err = sess.cfg_watcher.as_ref().and_then(|w| w.last_error());
    let mut st = lock(&sh.status);
    st.master_name = i.master_name.clone();
    st.cable_rate = i.cable_rate;
    st.asio_rate = i.asio_rate;
    st.buffer_size = i.buffer;
    st.input_latency = sess.caps.input_latency.max(0) as u32;
    st.output_latency = sess.caps.output_latency.max(0) as u32;
    st.output_channels = i.outputs;
    st.supported_rates = sess.caps.supported_rates.clone();
    st.ring_fill_frames = live.ring_fill.load(Relaxed);
    st.ring_target_frames = target;
    st.ring_capacity_frames = i.ring_capacity;
    st.underruns = underruns;
    st.overruns = live.overruns.load(Relaxed);
    st.xruns = live.xruns.load(Relaxed);
    st.discarded_frames = live.discarded.load(Relaxed);
    st.gate_rearms = live.rearms.load(Relaxed);
    st.callbacks = live.callbacks.load(Relaxed);
    st.drift_ppm = f32::from_bits(live.trim_ppm.load(Relaxed));
    st.fault = match live.fault.load(Relaxed) {
        0 => FaultState::Ok,
        1 => FaultState::Warn,
        _ => FaultState::Muted,
    };
    st.non_finite = live.non_finite.load(Relaxed);
    st.capture_mode_requested = Some(i.capture_requested);
    st.capture_mode_in_use = mode_in_use;
    st.capture_device = i.capture_device.clone();
    st.capture_period_frames = i.capture_period;
    st.capture_period_frames_granted = i.capture_period;
    st.capture_period_frames_requested = i.period_requested;
    st.keepalive = sess.keepalive_status.clone();
    if ka_note.is_some() {
        st.keepalive_note = ka_note;
    }
    st.capture_period_us = (period_ms * 1000.0) as u32;
    st.capture_burst_max_frames = live.burst_max.load(Relaxed);
    if let Some(cs) = cap_stats {
        st.capture_packets = cs.packets;
        st.capture_discontinuities = cs.discontinuities;
        st.capture_silent_packets = cs.silent_packets;
    }
    st.cpu_avg_us = avg;
    st.cpu_max_us = max;
    st.cpu_avg_pct = cpu_percent(f64::from(avg), period_us) as f32;
    st.cpu_max_pct = cpu_percent(f64::from(max), period_us) as f32;
    st.peak_in_db = peak_in;
    st.peak_out_db = peak_out;
    st.safety_buffer_ms = safety;
    let render_side = sess.measured.render_side(
        &EngineParams {
            capture_device: i.capture_device.clone(),
            asio_buffer: i.buffer,
            capture_period_frames: i.capture_period,
            keepalive: sess.keepalive_status.is_some(),
        },
        eq_core::now_unix_s(),
    );
    st.latency = caps.latency_breakdown(ring_ms, staleness_ms, render_side);
    st.ring_mean_fill_frames = if have_mean { mean_fill.round() as u32 } else { 0 };
    st.jitter_headroom_frames = headroom as u32;
    st.required_headroom_frames = required as u32;
    st.arrival_gap_p999_ms = snap.gap_p999_ms() as f32;
    st.arrival_dip_p999_frames = snap.dip_p999;
    st.capture_staleness_ms = staleness_ms as f32;
    st.arrival_observed_s = snap.observed_s as f32;
    st.hardening = HardeningReport::current(live.mmcss_asio.load(Relaxed));
    st.config_error = cfg_err;
    st.dropped_logs = Logger::dropped();
    st.uptime_ms = sess.started.elapsed().as_millis() as u64;
}

#[cfg(test)]
mod tests {
    use super::*;
    use asio_host::Holder;
    use std::sync::atomic::AtomicUsize;

    /// The engine owns a process-wide named mutex, so engine tests must not overlap.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn keepalive_period_conversion_and_guard() {
        assert_eq!(period_ms(Some((48, 48000))), Some(1.0));
        assert_eq!(period_ms(Some((480, 48000))), Some(10.0));
        assert_eq!(period_ms(Some((480, 0))), None);
        assert_eq!(period_ms(None), None);
        // off by default; never started on a real output device (no endpoint is touched)
        let cfg = Config::default();
        assert!(start_keepalive(&cfg, "Line 1 (Virtual Audio Cable)").0.is_none());
        let on = Config { render_keepalive: true, ..Config::default() };
        let (k, st, note) = start_keepalive(&on, "Speakers (Realtek(R) Audio)");
        assert!(k.is_none() && st.is_none());
        assert!(note.unwrap().contains("not a known virtual cable"));
    }

    #[test]
    fn exclusive_is_mapped_to_shared_with_note() {
        use eq_core::CaptureMode as C;
        use wasapi_capture::CaptureMode as W;
        assert_eq!(effective_capture_mode(C::Shared), (W::Shared, None));
        assert_eq!(effective_capture_mode(C::LowLatency), (W::LowLatency, None));
        let (m, note) = effective_capture_mode(C::Exclusive);
        assert_eq!(m, W::Shared);
        assert!(note.is_some_and(|n| n.contains("Exclusive") && n.contains("Shared")));
    }

    #[test]
    fn busy_master_waits_and_retries() {
        let _g = lock(&SERIAL);
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let loader = MasterLoader(Arc::new(move |name: &str| {
            c2.fetch_add(1, Relaxed);
            Err(EngineError::MasterBusy {
                name: name.to_string(),
                holders: vec![Holder { pid: 4242, exe_name: "fake.exe".into(), exe_path: None }],
            })
        }));
        let opts = EngineOptions {
            master_loader: Some(loader),
            config_path: None,
            publish_caps: false,
            status_mapping: false,
            tap: None,
            cable_formats: None,
            synthetic: None,
            retry_unavailable: false,
            loopback_probe: None,
            load_measured: false,
        };
        // Busy is not a start failure: start succeeds and the engine waits.
        let engine = Engine::start_with(Config::default(), opts).expect("busy must be non-fatal");
        let st = engine.status();
        match &st.state {
            EngineState::WaitingForDevice { holders } => {
                assert_eq!(holders, &vec![HolderInfo { pid: 4242, exe_name: "fake.exe".into() }]);
            }
            other => panic!("unexpected state {other:?}"),
        }
        assert_eq!(st.busy_holders.len(), 1);
        assert!(st.last_error.as_deref().unwrap_or("").contains("in use by fake.exe (PID 4242)"));
        assert_eq!(st.restarts, 0);
        let n = calls.load(Relaxed);
        assert!(n >= 1);
        engine.retry_now();
        let t = Instant::now();
        while calls.load(Relaxed) <= n && t.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(calls.load(Relaxed) > n, "retry_now must retry before the 2 s timer");
        engine.stop();
        assert!(engine.status().busy_holders.is_empty());
    }

    fn missing_opts(err: EngineError, retry: bool) -> EngineOptions {
        let loader = MasterLoader(Arc::new(move |_: &str| Err(err.clone())));
        EngineOptions {
            master_loader: Some(loader),
            config_path: None,
            publish_caps: false,
            status_mapping: false,
            tap: None,
            cable_formats: None,
            synthetic: None,
            retry_unavailable: retry,
            loopback_probe: None,
            load_measured: false,
        }
    }

    #[test]
    fn missing_master_is_returned_without_retry_flag() {
        let _g = lock(&SERIAL);
        let e = Engine::start_with(Config::default(), missing_opts(EngineError::MasterNotFound("Y".into()), false));
        assert!(matches!(e, Err(EngineError::MasterNotFound(_))));
    }

    #[test]
    fn missing_master_keeps_retrying_with_retry_flag() {
        let _g = lock(&SERIAL);
        let engine = Engine::start_with(Config::default(), missing_opts(EngineError::MasterNotFound("Y".into()), true))
            .expect("unavailable device must not fail start in retry mode");
        let t = Instant::now();
        let mut st = engine.status();
        while !matches!(st.state, EngineState::Restarting { .. }) && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
            st = engine.status();
        }
        match &st.state {
            EngineState::Restarting { reason } => assert!(reason.starts_with("Waiting for"), "{reason}"),
            other => panic!("unexpected state {other:?}"),
        }
        engine.stop();
    }
}
