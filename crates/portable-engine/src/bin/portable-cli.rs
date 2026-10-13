//! Headless CLI skeleton of the portable engine: parses a config, builds a
//! [`Session`] and runs the supervision loop against the status transport.
//!
//! Phase A skeleton: no OS backend is wired yet (`backend::BACKEND_NAME` reports
//! "none" / "stub"), so the loop exercises the control logic only; Phase C plugs the
//! per-OS `OutputSink`/`SystemSource` in where `TODO(Phase C)` is marked below.
//! Plain `std` only — no `SetConsoleCtrlHandler`; stop with Ctrl-C (process kill) or
//! `--duration-s`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eq_core::Config;
use portable_engine::pipeline::Live;
use portable_engine::session::Session;
use portable_engine::status::InProcTransport;
use portable_engine::{backend, Clock, ControlAction, StatusTransport};

/// Monotonic microsecond clock (`Instant`-based), ticking the [`Clock`] seam.
struct StdClock {
    t0: Instant,
}

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

struct Args {
    config: Option<PathBuf>,
    /// 0 = run until killed.
    duration_s: f64,
    tick_ms: u64,
    help: bool,
}

const USAGE: &str = "\
portable-cli — headless portable-engine skeleton

USAGE:
    portable-cli [--config PATH] [--duration-s N] [--tick-ms N]

OPTIONS:
    --config PATH     config file (default: eq_core::default_path())
    --duration-s N    run for N seconds, then exit (default: 10, 0 = until killed)
    --tick-ms N       supervision tick in milliseconds (default: 100)
    -h, --help        print this help
";

fn parse_args() -> Result<Args, String> {
    let mut args = Args { config: None, duration_s: 10.0, tick_ms: 100, help: false };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => args.help = true,
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
    let path = args.config.or_else(eq_core::default_path);
    let cfg = match &path {
        Some(p) => match Config::load_or_default(p) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: cannot load config {}: {e}", p.display());
                return ExitCode::FAILURE;
            }
        },
        None => Config::default(),
    };
    println!("portable-engine {} — backend: {}", env!("CARGO_PKG_VERSION"), backend::BACKEND_NAME);
    println!(
        "config: {}",
        path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "<built-in defaults>".to_string())
    );

    let clock = Arc::new(StdClock::new());
    let live = Arc::new(Live::default());
    let status = Arc::new(InProcTransport::new());
    let mut session = Session::new(cfg, live.clone(), status.clone(), clock.ticks_per_second());
    status.init(&session.static_info());
    session.set_running(true);

    // TODO(Phase C): open the per-OS OutputSink/SystemSource here (backend dispatch),
    // negotiate a StreamSpec, build the Pipeline + PipelineCallback and `sink.start(cb)`.

    let tick_us = (args.tick_ms.max(1) * 1000) as i64;
    let start = clock.now_ticks();
    let deadline = start + (args.duration_s.max(0.0) * 1e6) as i64;
    let mut last_report = start;
    loop {
        let now = clock.now_ticks();
        match session.tick(now) {
            ControlAction::None | ControlAction::Retry => {}
            ControlAction::HotApply => println!("supervisor: hot-apply"),
            ControlAction::Restart(reason) => {
                eprintln!("supervisor: restart requested ({reason})");
                let uptime = Duration::from_micros((now - start).max(0) as u64);
                let retry_at = session.note_run_ended(now, uptime);
                clock.sleep_until(retry_at);
                session.set_running(true);
            }
            ControlAction::Stop => break,
        }
        if now - last_report >= 1_000_000 {
            last_report = now;
            let b = status.block_report();
            println!(
                "t={:.1}s state={:?} callbacks={} underruns={} xruns={} restarts={} peak_out=[{:.3},{:.3}]",
                (now - start) as f64 / 1e6,
                session.state(),
                live.callbacks.load(Relaxed),
                b.underruns,
                status.xruns(),
                session.restarts(),
                b.peak_out[0],
                b.peak_out[1]
            );
        }
        if args.duration_s > 0.0 && now >= deadline {
            break;
        }
        clock.sleep_until(now + tick_us);
    }
    session.set_running(false);
    println!("done: {} callbacks, {} restarts", live.callbacks.load(Relaxed), session.restarts());
    ExitCode::SUCCESS
}
