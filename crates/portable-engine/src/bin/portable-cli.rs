//! Headless CLI of the portable engine (Phase C wiring): loads the shared config
//! (+ the `[portable]` section), builds a [`Session`] + pipeline through the frozen
//! [`seams`](portable_engine::seams), opens the per-OS backend and runs the
//! supervision loop until Ctrl+C (or `--duration-s`), printing a status line
//! (ring fill, rate, drift ppm, xruns) once per second.
//!
//! `portable-cli --selftest` runs the fake-driven pipeline without any hardware
//! (platform-independent; used by CI on Windows / Linux / macOS). Plain `std` only —
//! Ctrl+C handling is hand-written FFI (`SetConsoleCtrlHandler` / `signal`), per the
//! A8 no-wrapper-crate rule.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eq_core::Config;
use portable_engine::pipeline::{Live, Pipeline, PipelineCallback, PipelineInit, OUT_CH};
use portable_engine::session::{self as psession, Session};
use portable_engine::status::InProcTransport;
use portable_engine::{
    backend, Clock, ControlAction, OutputSink, StatusTransport, StreamRequest, StreamSpec,
    SystemSource,
};
use router_core::processor::{EqProcessor, Handoff, Snapshot};
use router_core::resample::{RateMatcher, RateMatcherConfig};
use router_core::ring;
use router_core::watchdog::Heartbeat;

/// Fallback clock (Instant-based microseconds) for builds without a backend
/// (not constructed when a backend feature is compiled in).
#[allow(dead_code)]
struct StdClock {
    t0: Instant,
}

#[allow(dead_code)]
impl StdClock {
    fn new() -> Self {
        StdClock { t0: Instant::now() }
    }
}

impl Clock for StdClock {
    fn now_ticks(&self) -> i64 {
        self.t0.elapsed().as_micros() as i64
    }
    fn ticks_per_second(&self) -> i64 {
        1_000_000
    }
    fn sleep_until(&self, ticks: i64) {
        let now = self.now_ticks();
        if ticks > now {
            std::thread::sleep(Duration::from_micros((ticks - now) as u64));
        }
    }
}

/// [`Clock`] over `clock_gettime(CLOCK_MONOTONIC)` — the tick domain the ALSA
/// backend passes to `render` (`backend::linux::monotonic_ns`).
#[cfg(all(target_os = "linux", feature = "backend-linux"))]
struct LinuxClock;

#[cfg(all(target_os = "linux", feature = "backend-linux"))]
impl Clock for LinuxClock {
    fn now_ticks(&self) -> i64 {
        portable_engine::backend::linux::monotonic_ns()
    }
    fn ticks_per_second(&self) -> i64 {
        portable_engine::backend::linux::TICKS_PER_SECOND
    }
    fn sleep_until(&self, ticks: i64) {
        let now = self.now_ticks();
        if ticks > now {
            std::thread::sleep(Duration::from_nanos((ticks - now) as u64));
        }
    }
}

// ------------------------------------------------------------------------------------
// Ctrl+C (hand-written FFI, A8: no `ctrlc` crate)
// ------------------------------------------------------------------------------------

mod ctrl {
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

    static STOP: AtomicBool = AtomicBool::new(false);

    pub fn stop_requested() -> bool {
        STOP.load(Relaxed)
    }

    #[cfg(target_os = "windows")]
    mod imp {
        use super::STOP;
        use std::sync::atomic::Ordering::Relaxed;

        #[link(name = "kernel32")]
        extern "system" {
            fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
        }

        /// Console control handler (Ctrl+C / Ctrl+Break / close): latch the stop flag
        /// and tell the console we handled it, so the run loop can shut down cleanly.
        extern "system" fn on_ctrl(_kind: u32) -> i32 {
            STOP.store(true, Relaxed);
            1
        }

        pub fn install() {
            // SAFETY: plain process-local handler registration.
            unsafe {
                SetConsoleCtrlHandler(Some(on_ctrl), 1);
            }
        }
    }

    #[cfg(unix)]
    mod imp {
        use super::STOP;
        use std::sync::atomic::Ordering::Relaxed;

        extern "C" {
            fn signal(sig: i32, handler: Option<extern "C" fn(i32)>) -> usize;
        }

        const SIGINT: i32 = 2;
        const SIGTERM: i32 = 15;

        extern "C" fn on_signal(_sig: i32) {
            STOP.store(true, Relaxed);
        }

        pub fn install() {
            // SAFETY: `signal` with a plain function pointer; the handler only stores
            // an atomic bool (async-signal-safe).
            unsafe {
                signal(SIGINT, Some(on_signal));
                signal(SIGTERM, Some(on_signal));
            }
        }
    }

    #[cfg(not(any(target_os = "windows", unix)))]
    mod imp {
        pub fn install() {}
    }

    pub fn install() {
        imp::install();
    }
}

// ------------------------------------------------------------------------------------
// Arguments
// ------------------------------------------------------------------------------------

struct Args {
    config: Option<PathBuf>,
    /// 0 = run until Ctrl+C.
    duration_s: f64,
    tick_ms: u64,
    selftest: bool,
    help: bool,
}

const USAGE: &str = "\
portable-cli — headless cross-platform EQ engine (portable-engine)

USAGE:
    portable-cli [--config PATH] [--duration-s N] [--tick-ms N]
    portable-cli --selftest

OPTIONS:
    --config PATH     config file (default: eq_core::default_path(); built-in
                      defaults if missing). The [portable] section selects the
                      source/output devices and an AutoEQ profile path.
    --duration-s N    run for N seconds, then exit (default: 0 = until Ctrl+C)
    --tick-ms N       supervision tick in milliseconds (default: 100)
    --selftest        run the fake-driven pipeline without hardware and exit
                      (CI smoke test; 0 = pass)
    -h, --help        print this help

STATUS LINE (once per second):
    ring=<fill> rate=<Hz> drift=<ppm> xruns=<n> ...
    latency: unmeasured (pending hardware validation)
";

fn parse_args() -> Result<Args, String> {
    let mut args = Args { config: None, duration_s: 0.0, tick_ms: 100, selftest: false, help: false };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => args.help = true,
            "--selftest" => args.selftest = true,
            "--config" => {
                args.config = Some(PathBuf::from(it.next().ok_or("--config needs a path")?));
            }
            "--duration-s" => {
                args.duration_s = it.next().ok_or("--duration-s needs a number")?.parse().map_err(|e| format!("--duration-s: {e}"))?;
            }
            "--tick-ms" => {
                args.tick_ms = it.next().ok_or("--tick-ms needs a number")?.parse().map_err(|e| format!("--tick-ms: {e}"))?;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(args)
}

// ------------------------------------------------------------------------------------
// Per-OS source lifecycle (the frozen SystemSource seam has no start/stop)
// ------------------------------------------------------------------------------------

/// Start/stop of a system source around the capture session. The B1/B2 backends
/// expose these as inherent methods (or fold them into attach/drop); this private,
/// object-safe shim keeps the shared run loop generic without touching the frozen
/// [`SystemSource`] seam.
trait SourceLifecycle: SystemSource {
    fn start_capture(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn stop_capture(&mut self) {}
}

#[cfg(all(target_os = "linux", feature = "backend-linux"))]
impl SourceLifecycle for portable_engine::backend::pipewire::PipeWireSource {
    // capture thread starts at `attach` and stops on drop.
}

#[cfg(all(target_os = "macos", feature = "backend-macos"))]
impl SourceLifecycle for portable_engine::backend::macos_tap::MacTap {
    fn start_capture(&mut self) -> Result<(), String> {
        self.start().map_err(|e| e.to_string())
    }
    fn stop_capture(&mut self) {
        let _ = self.stop();
    }
}

// ------------------------------------------------------------------------------------
// Shared run loop (generic over the per-OS OutputSink)
// ------------------------------------------------------------------------------------

/// Builds ring + pipeline over the (optional) source, starts the sink and runs the
/// supervision loop until Ctrl+C / `--duration-s` / supervisor stop. Mirrors
/// `router-engine`'s pipeline construction (safety buffer -> target fill -> matcher
/// -> `EqProcessor`) with the eq-core config knobs wired straight through
/// (`bypass`/`eq_enabled`/`limiter`/`safety_buffer_*`/`resampler_quality`).
fn drive<S: OutputSink>(
    sink: &mut S,
    spec: StreamSpec,
    period_frames: u32,
    wrap: impl FnOnce(PipelineCallback) -> S::Callback,
    mut source: Option<&mut dyn SourceLifecycle>,
    cfg: &Config,
    args: &Args,
    clock: Arc<dyn Clock>,
    live: Arc<Live>,
    status: Arc<InProcTransport>,
    session: &mut Session,
) -> Result<(), String> {
    let out_rate = spec.rate_hz;
    let buffer = spec.buffer_frames.max(1);
    let cable_rate = source.as_ref().map(|s| s.negotiated_rate()).unwrap_or(out_rate).max(1);
    let period = period_frames.max(1);

    // Ring geometry: safety buffer (+ auto extra) -> target fill, capacity for the
    // maximum safety setting (router-engine `Session::open` semantics).
    let now0 = clock.now_ticks();
    let safety_ms = session.safety_ms(now0);
    let burst = psession::burst_estimate(period, 0, 0);
    let target = psession::plan_target(safety_ms, buffer, cable_rate, out_rate, burst);
    let capacity = psession::plan_capacity(buffer, cable_rate, out_rate, 4096);
    let (prod, cons) = ring::channel(capacity);
    if let Some(s) = source.as_mut() {
        s.attach(prod).map_err(|e| format!("source attach failed: {e}"))?;
        s.start_capture()?;
    }

    let hb = Arc::new(Heartbeat::new());
    let handoff = Arc::new(Handoff::<Snapshot>::new());
    // eq-core parity: bypass / eq_enabled / limiter / profile / headroom / mute-on-
    // fault all flow through `Snapshot::from_config` inside `EqProcessor`.
    let eq = EqProcessor::new(f64::from(out_rate), cfg, OUT_CH, handoff.clone());
    let matcher = RateMatcher::new(RateMatcherConfig::new(cable_rate, out_rate, target, cfg.resampler_quality));
    let pipe = Pipeline::new(PipelineInit {
        source: cons,
        matcher,
        eq,
        frames: buffer as usize,
        out_rate,
        tick_hz: clock.ticks_per_second(),
        live: live.clone(),
        status: Some(status.clone()),
        heartbeat: hb,
        target_frames: target as u32,
        quality: cfg.resampler_quality,
    });
    let cb = PipelineCallback::new(pipe, clock.clone());
    sink.start(wrap(cb)).map_err(|e| format!("output start failed: {e}"))?;
    session.set_effective_rate(out_rate);
    session.set_running(true);
    println!(
        "streaming: {out_rate} Hz out / {cable_rate} Hz source, buffer {buffer} frames, target fill {target} frames ({safety_ms:.1} ms safety)"
    );

    let tick_us = (args.tick_ms.max(1) * 1000) as i64;
    let start = clock.now_ticks();
    let deadline = start + (args.duration_s.max(0.0) * 1e6) as i64;
    let mut last_report = start;
    loop {
        if ctrl::stop_requested() {
            println!("stop requested (Ctrl+C)");
            break;
        }
        let now = clock.now_ticks();
        match session.tick(now) {
            ControlAction::None | ControlAction::Retry => {}
            ControlAction::HotApply => println!("supervisor: hot-apply"),
            ControlAction::Restart(reason) => {
                // The headless v1 supervisor re-arms instead of rebuilding the
                // hardware stream (stream rebuild on driver reset is OPEN).
                eprintln!("supervisor: restart requested ({reason}); re-arming");
                let uptime = Duration::from_micros((now - start).max(0) as u64);
                let retry_at = session.note_run_ended(now, uptime);
                clock.sleep_until(retry_at);
                session.set_running(true);
            }
            ControlAction::Stop => break,
        }
        if now - last_report >= 1_000_000 {
            last_report = now;
            let fill = live.ring_fill.load(Relaxed);
            let drift = f32::from_bits(live.trim_ppm.load(Relaxed));
            println!(
                "t={:.1}s ring={} rate={}Hz drift={:+.2}ppm xruns={} underruns={} fault={} latency: unmeasured (pending hardware validation)",
                (now - start) as f64 / 1e6,
                fill,
                out_rate,
                drift,
                live.xruns.load(Relaxed),
                live.underruns.load(Relaxed),
                live.fault.load(Relaxed),
            );
        }
        if args.duration_s > 0.0 && now >= deadline {
            break;
        }
        clock.sleep_until(now + tick_us);
    }
    session.set_running(false);
    if let Err(e) = sink.stop() {
        eprintln!("warning: output stop: {e}");
    }
    if let Some(s) = source.as_mut() {
        s.stop_capture();
    }
    println!(
        "done: {} callbacks, {} restarts, {} xruns, latency: unmeasured (pending hardware validation)",
        live.callbacks.load(Relaxed),
        session.restarts(),
        live.xruns.load(Relaxed)
    );
    Ok(())
}

// ------------------------------------------------------------------------------------
// Per-OS dispatch
// ------------------------------------------------------------------------------------

#[cfg(all(target_os = "windows", feature = "backend-windows"))]
fn make_clock() -> Arc<dyn Clock> {
    Arc::new(portable_engine::backend::windows::QpcClock::new())
}

#[cfg(all(target_os = "linux", feature = "backend-linux"))]
fn make_clock() -> Arc<dyn Clock> {
    Arc::new(LinuxClock)
}

#[cfg(all(target_os = "macos", feature = "backend-macos"))]
fn make_clock() -> Arc<dyn Clock> {
    Arc::new(portable_engine::backend::macos::MachClock::new())
}

#[cfg(not(any(
    all(target_os = "windows", feature = "backend-windows"),
    all(target_os = "linux", feature = "backend-linux"),
    all(target_os = "macos", feature = "backend-macos"),
)))]
fn make_clock() -> Arc<dyn Clock> {
    Arc::new(StdClock::new())
}

/// Windows: master ASIO output over `asio-host` ([portable] `output_device` or the
/// shared `master_driver` name; the frozen v1 driver is refused). The portable
/// Windows *source* is not wired yet (OPEN): the run is output-only.
#[cfg(all(target_os = "windows", feature = "backend-windows"))]
fn run_os(
    args: &Args,
    cfg: &Config,
    clock: Arc<dyn Clock>,
    live: Arc<Live>,
    status: Arc<InProcTransport>,
    session: &mut Session,
) -> Result<(), String> {
    use portable_engine::backend::windows::{default_master_loader, LoaderFn, WindowsCallback, WindowsSink};

    let pc = &cfg.portable;
    let name = if pc.output_device.is_empty() { psession::effective_master(cfg) } else { pc.output_device.clone() };
    if psession::is_forbidden_master(&name) {
        return Err(format!("master \"{name}\" is the frozen v1 virtual driver and must never be used as master"));
    }
    if !matches!(pc.source.to_ascii_lowercase().as_str(), "auto" | "none" | "") {
        println!("note: source \"{}\" is not wired on Windows yet (OPEN item); running output-only", pc.source);
    }
    let loader: Arc<LoaderFn> = Arc::new(default_master_loader);
    let mut sink = WindowsSink::open_with(&name, cfg.output_channels, loader).map_err(|e| e.to_string())?;
    println!("backend: {} — output: ASIO \"{name}\", source: none (Windows portable source OPEN)", backend::BACKEND_NAME);
    let want = StreamRequest { rate_hz: cfg.sample_rate, buffer_frames: 0, channels: OUT_CH as u16 };
    let spec = sink.negotiate(&want).map_err(|e| e.to_string())?;
    println!("negotiated: {} Hz, {} frames (driver-preferred, clamped)", spec.rate_hz, spec.buffer_frames);
    let live_cb = live.clone();
    drive(
        &mut sink,
        spec,
        spec.buffer_frames,
        move |cb| WindowsCallback::new(cb).with_live(live_cb),
        None,
        cfg,
        args,
        clock,
        live,
        status,
        session,
    )
}

/// Linux: ALSA output to the interface + PipeWire/Pulse monitor source
/// ([portable] `output_device` / `input_device`, `source = "none"` for output-only).
#[cfg(all(target_os = "linux", feature = "backend-linux"))]
fn run_os(
    args: &Args,
    cfg: &Config,
    clock: Arc<dyn Clock>,
    live: Arc<Live>,
    status: Arc<InProcTransport>,
    session: &mut Session,
) -> Result<(), String> {
    use portable_engine::backend::linux::{AlsaCallback, AlsaSink};
    use portable_engine::backend::pipewire::PipeWireSource;

    let pc = &cfg.portable;
    let dev = if pc.output_device.is_empty() { "default".to_string() } else { pc.output_device.clone() };
    let want_rate = if cfg.sample_rate == 0 { 48_000 } else { cfg.sample_rate };
    let want = StreamRequest { rate_hz: want_rate, buffer_frames: 0, channels: OUT_CH as u16 };

    let mut sink = AlsaSink::new(dev.clone());
    let mut source: Option<PipeWireSource> = if pc.source.to_ascii_lowercase() == "none" {
        println!("backend: {} — output: ALSA \"{dev}\", source: none (output only)", backend::BACKEND_NAME);
        None
    } else {
        let mut s = PipeWireSource::new(want_rate);
        if pc.input_device.is_empty() {
            println!("backend: {} — output: ALSA \"{dev}\", source: PipeWire default monitor", backend::BACKEND_NAME);
        } else {
            s = s.with_source(pc.input_device.clone());
            println!(
                "backend: {} — output: ALSA \"{dev}\", source: PipeWire monitor \"{}\"",
                backend::BACKEND_NAME,
                pc.input_device
            );
        }
        Some(s)
    };
    let spec = sink.negotiate(&want).map_err(|e| e.to_string())?;
    let (period_frames, periods) = sink.period();
    println!(
        "negotiated: {} Hz, {} frames, {} periods of {} frames",
        spec.rate_hz, spec.buffer_frames, periods, period_frames
    );
    let period = source.as_ref().map(|s| s.chunk_frames()).unwrap_or(spec.buffer_frames);
    drive(
        &mut sink,
        spec,
        period,
        AlsaCallback::new,
        source.as_mut().map(|s| s as &mut dyn SourceLifecycle),
        cfg,
        args,
        clock,
        live,
        status,
        session,
    )
}

/// macOS: Core Audio output to the interface + BlackHole loopback tap as the system
/// source ([portable] `output_device` / `input_device`, `source = "none"` for
/// output-only).
#[cfg(all(target_os = "macos", feature = "backend-macos"))]
fn run_os(
    args: &Args,
    cfg: &Config,
    clock: Arc<dyn Clock>,
    live: Arc<Live>,
    status: Arc<InProcTransport>,
    session: &mut Session,
) -> Result<(), String> {
    use portable_engine::backend::macos::{DeviceSelector, MacCallback, MacSink};
    use portable_engine::backend::macos_tap::MacTap;

    let pc = &cfg.portable;
    let out_sel =
        if pc.output_device.is_empty() { DeviceSelector::default_output() } else { DeviceSelector::by_name(&pc.output_device) };
    let want_rate = if cfg.sample_rate == 0 { 48_000 } else { cfg.sample_rate };
    let want = StreamRequest { rate_hz: want_rate, buffer_frames: 0, channels: OUT_CH as u16 };

    let mut sink = MacSink::open(&out_sel).map_err(|e| e.to_string())?;
    let spec = sink.negotiate(&want).map_err(|e| e.to_string())?;
    println!("backend: {} — output: Core Audio \"{}\"", backend::BACKEND_NAME, sink.name);

    let mut tap: Option<MacTap> = if pc.source.to_ascii_lowercase() == "none" {
        println!("source: none (output only)");
        None
    } else {
        let want_src = StreamRequest { rate_hz: spec.rate_hz, buffer_frames: spec.buffer_frames, channels: 2 };
        let t = if pc.input_device.is_empty() {
            MacTap::open(&want_src)
        } else {
            MacTap::open_with(&want_src, &DeviceSelector::by_name(&pc.input_device))
        }
        .map_err(|e| e.to_string())?;
        println!("source: BlackHole tap \"{}\" ({} Hz)", t.name, t.rate);
        Some(t)
    };
    let period = tap.as_ref().map(|t| t.buffer_frames).unwrap_or(spec.buffer_frames);
    drive(
        &mut sink,
        spec,
        period,
        MacCallback::new,
        tap.as_mut().map(|t| t as &mut dyn SourceLifecycle),
        cfg,
        args,
        clock,
        live,
        status,
        session,
    )
}

#[cfg(not(any(
    all(target_os = "windows", feature = "backend-windows"),
    all(target_os = "linux", feature = "backend-linux"),
    all(target_os = "macos", feature = "backend-macos"),
)))]
fn run_os(
    _args: &Args,
    _cfg: &Config,
    _clock: Arc<dyn Clock>,
    _live: Arc<Live>,
    _status: Arc<InProcTransport>,
    _session: &mut Session,
) -> Result<(), String> {
    Err("no output backend compiled in (build with default features)".to_string())
}

// ------------------------------------------------------------------------------------
// Config + main
// ------------------------------------------------------------------------------------

/// Loads the shared config and applies the `[portable]` AutoEQ profile override
/// (`profile_path` -> `parse_parametric_eq`; the DSP itself stays in eq-core).
fn load_config(path: &Option<PathBuf>) -> Result<(Config, String), String> {
    let mut cfg = match path {
        Some(p) => Config::load_or_default(p).map_err(|e| format!("cannot load config {}: {e}", p.display()))?,
        None => Config::default(),
    };
    if let Some(pp) = cfg.portable.profile_path.clone() {
        let text = std::fs::read_to_string(&pp).map_err(|e| format!("cannot read EQ profile {}: {e}", pp.display()))?;
        let name = pp.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let profile = eq_core::parse_parametric_eq(&text, &name).map_err(|e| format!("cannot parse EQ profile {}: {e}", pp.display()))?;
        cfg.profile_source = Some(pp.display().to_string());
        cfg.profile = profile;
    }
    Ok((cfg, path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "<built-in defaults>".to_string())))
}

fn selftest() -> ExitCode {
    println!("portable-cli --selftest: fake-driven pipeline, no hardware");
    let r = portable_engine::testing::run_selftest();
    println!(
        "selftest: periods={} callbacks={} underruns={} overruns={} fault={} restarts={} gain={:+.2} dB",
        r.periods, r.callbacks, r.underruns, r.overruns, r.fault, r.restarts, r.gain_db
    );
    if r.ok {
        println!("selftest: OK");
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "selftest: FAILED (want: callbacks=={} && underruns==0 && overruns==0 && fault==0 && restarts==0 && |gain-6.0|<0.5 dB)",
            r.periods
        );
        ExitCode::FAILURE
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    if args.help {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.selftest {
        return selftest();
    }

    let path = args.config.clone().or_else(eq_core::default_path);
    let (cfg, cfg_desc) = match load_config(&path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(n) = eq_core::take_deprecation_notice() {
        println!("{n}");
    }

    ctrl::install();
    let clock = make_clock();
    let live = Arc::new(Live::default());
    let status = Arc::new(InProcTransport::new());
    let mut session =
        Session::new(cfg.clone(), live.clone(), status.clone() as Arc<dyn StatusTransport>, clock.ticks_per_second());
    status.init(&session.static_info());

    println!("portable-engine {} — backend: {}", env!("CARGO_PKG_VERSION"), backend::BACKEND_NAME);
    println!("config: {cfg_desc}");
    println!(
        "profile: \"{}\" (source: {}) bypass={} eq_enabled={} limiter={} safety={}ms auto={}",
        cfg.profile.name,
        cfg.profile_source.as_deref().unwrap_or("built-in"),
        cfg.bypass,
        cfg.eq_enabled,
        cfg.limiter,
        cfg.safety_buffer_ms,
        cfg.safety_buffer_auto,
    );

    match run_os(&args, &cfg, clock, live, status, &mut session) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
