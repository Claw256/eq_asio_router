//! router-cli: headless front end of the router engine.
//!
//! ```text
//! router-cli devices
//! router-cli caps [--master NAME]
//! router-cli holders [--master NAME]
//! router-cli --help | -h | help        (any subcommand too; unknown flags/subcommands exit 2 before any engine start)
//! router-cli run (--seconds N | --forever) [--config PATH] [--master NAME] [--rate HZ]
//!                [--safety-ms F] [--preamp-db F] [--bypass] [--learn]
//!                [--dump-dir DIR]
//! router-cli latency [--sink NAME] [--count N] [--interval-ms M] [--db -40] [--config PATH]
//!                    [--allow-sink NAME] [--shape burst:18500:2] [--hp-hz N|off] [--json]
//! router-cli latency --baseline [--seconds 3] [--dump FILE.wav] | --selftest
//! router-cli analyze FILE [--freq HZ] [--skip S] [--tail S] [--channel N] [--mads X]
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use eq_core::Config;
use router_engine::{Engine, EngineOptions, EngineState, EngineStatus};
use wasapi_capture::{list_endpoints, Flow};
use windows::core::BOOL;
use windows::Win32::System::Console::SetConsoleCtrlHandler;

mod latency;

pub(crate) static STOP: AtomicBool = AtomicBool::new(false);

pub(crate) unsafe extern "system" fn on_ctrl(_kind: u32) -> BOOL {
    STOP.store(true, Ordering::SeqCst);
    BOOL(1)
}

const USAGE: &str = "usage:
  router-cli devices
  router-cli caps [--master NAME]
  router-cli holders [--master NAME]
  router-cli --help | -h | help   (also after any subcommand; prints this text, exit 0; unknown flags/subcommands exit 2)
  router-cli run (--seconds N | --forever) [--config PATH] [--master NAME] [--rate HZ]
                 [--safety-ms F] [--preamp-db F] [--bypass]
                 [--learn]   (after 5 s run the 30 s source jitter measurement and print the learned margin)
                 [--dump-dir DIR]   (writes capture_in.wav, after_resample.wav, after_eq.wav)
                 [--synthetic RATE[:PACKET[:JITTER_MS[:FREQ,..]]] [--synthetic-db DB]]
  router-cli latency [--sink NAME] [--count N<=10] [--interval-ms M] [--db -40 (max -35)] [--config PATH]
                     [--allow-sink NAME] [--shape burst|chirp] [--json]
                     [--cap-threshold-db X] [--lb-threshold-db X] [--hp-hz N|off]
                     (end-to-end click latency: injects a very quiet click into the VBMatrix render endpoint
                      (VBMatrix In *) and detects it on the bridge source side (stage 1, ring-A tap) and on
                      the Yamaha's loopback inputs (stage 2); preamp forced to -20 dB, limiter on)
  router-cli latency --baseline [--seconds 3 (0.5..10)] [--dump FILE.wav]
                     (no click, nothing played: levels, DC, noise floor and spectrum at the bridge source side and the Yamaha
                      loopback inputs, and whether a click would be detectable; use it when `latency` reports a noisy loopback)
  router-cli latency --selftest   (no hardware: validates the detection/timing maths on a synthetic loopback)
  router-cli analyze FILE [--freq HZ] [--skip S] [--tail S] [--channel N] [--mads X]
  (run exits with code 2 if --seconds elapses while the master is still held by another program)";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Validate the whole command line BEFORE anything touches a device or starts the engine.
    let cmd = match parse_command(&args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("error: {msg}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = dispatch(cmd, &args) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// A validated command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Help,
    Devices,
    Caps,
    Holders,
    Run,
    Analyze,
    Latency,
}

/// `(flag, takes_value)` tables of the flags each subcommand accepts.
const RUN_FLAGS: &[(&str, bool)] = &[
    ("--config", true),
    ("--seconds", true),
    ("--forever", false),
    ("--master", true),
    ("--rate", true),
    ("--safety-ms", true),
    ("--preamp-db", true),
    ("--bypass", false),
    ("--learn", false),
    ("--dump-dir", true),
    ("--synthetic", true),
    ("--synthetic-db", true),
];
const LATENCY_FLAGS: &[(&str, bool)] = &[
    ("--sink", true),
    ("--count", true),
    ("--interval-ms", true),
    ("--db", true),
    ("--config", true),
    ("--allow-sink", true),
    ("--shape", true),
    ("--hp-hz", true),
    ("--json", false),
    ("--cap-threshold-db", true),
    ("--lb-threshold-db", true),
    ("--baseline", false),
    ("--selftest", false),
    ("--seconds", true),
    ("--dump", true),
    ("--no-save", false),
    ("--save", false),
    ("--render-mode", true),
    ("--render-buffer-frames", true),
    ("--render-fill", true),
    ("--render-prefill", true),
    ("--master", true),
    ("--rate", true),
    ("--safety-ms", true),
    ("--preamp-db", true),
    ("--bypass", false),
];
const MASTER_FLAGS: &[(&str, bool)] = &[("--master", true)];
const ANALYZE_FLAGS: &[(&str, bool)] =
    &[("--freq", true), ("--skip", true), ("--tail", true), ("--channel", true), ("--mads", true)];

fn is_help(a: &str) -> bool {
    matches!(a, "--help" | "-h" | "help")
}

/// Rejects unknown flags, stray positionals and flags without a value. `positional` allows one leading
/// non-flag argument (the file of `analyze`).
fn check_flags(sub: &str, args: &[String], table: &[(&str, bool)], positional: bool) -> Result<(), String> {
    let mut i = 0;
    let mut first = true;
    while i < args.len() {
        let a = args[i].as_str();
        if let Some(&(_, takes)) = table.iter().find(|(f, _)| *f == a) {
            if takes {
                if i + 1 >= args.len() {
                    return Err(format!("{sub}: {a} needs a value"));
                }
                i += 1;
            }
        } else if a.starts_with('-') {
            return Err(format!("{sub}: unknown flag {a}"));
        } else if !(positional && first) {
            return Err(format!("{sub}: unexpected argument {a}"));
        }
        first = false;
        i += 1;
    }
    Ok(())
}

/// Validates the command line without side effects. Help requests (`--help`, `-h`, `help`, at top level or
/// anywhere after a subcommand) return `Command::Help`; anything unknown is an error (exit code 2).
fn parse_command(args: &[String]) -> Result<Command, String> {
    let Some(first) = args.first().map(String::as_str) else {
        return Ok(Command::Help);
    };
    if is_help(first) || args[1..].iter().any(|a| is_help(a)) {
        return Ok(Command::Help);
    }
    let rest = &args[1..];
    match first {
        "devices" => check_flags("devices", rest, &[], false).map(|_| Command::Devices),
        "caps" => check_flags("caps", rest, MASTER_FLAGS, false).map(|_| Command::Caps),
        "holders" => check_flags("holders", rest, MASTER_FLAGS, false).map(|_| Command::Holders),
        "analyze" => check_flags("analyze", rest, ANALYZE_FLAGS, true).map(|_| Command::Analyze),
        "latency" => check_flags("latency", rest, LATENCY_FLAGS, false).map(|_| Command::Latency),
        "run" => {
            check_flags("run", rest, RUN_FLAGS, false)?;
            let has = |f: &str| rest.iter().any(|a| a == f);
            if has("--seconds") && has("--forever") {
                return Err("run: --seconds and --forever are mutually exclusive".into());
            }
            if !has("--seconds") && !has("--forever") {
                return Err("run: an unbounded run needs --forever (or give --seconds N)".into());
            }
            Ok(Command::Run)
        }
        other => Err(format!("unknown subcommand {other}")),
    }
}

fn dispatch(cmd: Command, args: &[String]) -> Result<()> {
    match cmd {
        Command::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Command::Devices => devices(),
        Command::Caps => caps(&args[1..]),
        Command::Holders => holders(&args[1..]),
        Command::Run => run(&args[1..]),
        Command::Analyze => analyze(&args[1..]),
        Command::Latency => latency::run(&args[1..]),
    }
}

fn devices() -> Result<()> {
    println!("ASIO drivers:");
    match asio_host::enumerate() {
        Ok(list) if !list.is_empty() => {
            for d in list {
                let note = if d.name.eq_ignore_ascii_case("EQ ASIO Router") {
                    "  (frozen v1 virtual driver; not usable as master)"
                } else if d.name.eq_ignore_ascii_case("Yamaha Steinberg USB ASIO") {
                    "  (default master)"
                } else {
                    ""
                };
                println!("  {}{note}", d.name);
            }
        }
        Ok(_) => println!("  (none)"),
        Err(e) => println!("  (cannot enumerate: {e})"),
    }
    for (flow, label) in [(Flow::Capture, "capture endpoints"), (Flow::Render, "playback endpoints")] {
        println!("{label}:");
        match list_endpoints(flow) {
            Ok(list) => {
                for e in list {
                    let mut tags = Vec::new();
                    if e.is_default {
                        tags.push("default");
                    }
                    let tag = if tags.is_empty() { String::new() } else { format!("  ({})", tags.join(", ")) };
                    let rate = if e.name.to_lowercase().contains("cable") {
                        match wasapi_capture::get_endpoint_device_format(&e.id) {
                            Ok(f) => format!("  [{} Hz]", f.sample_rate),
                            Err(_) => String::new(),
                        }
                    } else {
                        String::new()
                    };
                    println!("  {}{rate}{tag}", e.name);
                }
            }
            Err(e) => println!("  (cannot enumerate: {e})"),
        }
    }
    Ok(())
}

/// Returns the value following `flag`, if present.
fn opt<'a>(args: &'a [String], flag: &str) -> Result<Option<&'a str>> {
    match args.iter().position(|a| a == flag) {
        None => Ok(None),
        Some(i) => args.get(i + 1).map(|s| Some(s.as_str())).ok_or_else(|| anyhow!("{flag} needs a value")),
    }
}

/// Lists the processes that have the master driver's DLL(s) loaded.
fn holders(args: &[String]) -> Result<()> {
    let master = opt(args, "--master")?.unwrap_or("").trim();
    let name = if master.is_empty() { router_engine::logic::DEFAULT_MASTER } else { master };
    let info = asio_host::find_driver(name).map_err(|_| anyhow!("ASIO driver \"{name}\" is not registered"))?;
    let dlls = asio_host::busy::driver_dll_names(&info);
    println!("driver        : {name}");
    println!("driver DLLs   : {}", if dlls.is_empty() { "(none found in registry)".to_string() } else { dlls.join(", ") });
    let list = asio_host::find_driver_holders(&info);
    if list.is_empty() {
        println!("holders       : none (driver is free)");
    } else {
        println!("holders       : {}", asio_host::describe_holders(&list));
        for h in &list {
            let path = h.exe_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(path unavailable)".into());
            println!("  PID {:>6}  {}  {}", h.pid, h.exe_name, path);
        }
    }
    Ok(())
}

fn caps(args: &[String]) -> Result<()> {
    let master = opt(args, "--master")?.unwrap_or("");
    let c = router_engine::probe_master(master)?;
    let ms = |frames: i32| c.frames_to_ms(f64::from(frames));
    println!("master        : {}", c.master_name);
    println!("driver version: {}", c.driver_version);
    println!("channels      : {} in / {} out", c.in_channels, c.out_channels);
    println!("inputs        : {}", c.in_names.join(", "));
    println!("outputs       : {}", c.out_names.join(", "));
    println!("sample type   : {} (ASIO)", c.sample_type);
    println!("sample rate   : {} Hz", c.sample_rate);
    let rates: Vec<String> = c.supported_rates.iter().map(|r| r.to_string()).collect();
    println!("supported     : {} Hz", rates.join(", "));
    println!(
        "buffer sizes  : min {} / preferred {} / max {} / granularity {} frames ({:.2} ms preferred)",
        c.buffer_min,
        c.buffer_preferred,
        c.buffer_max,
        c.buffer_granularity,
        ms(c.buffer_preferred)
    );
    println!(
        "latency (pre-createBuffers): in {} samples ({:.2} ms), out {} samples ({:.2} ms)",
        c.input_latency,
        ms(c.input_latency),
        c.output_latency,
        ms(c.output_latency)
    );
    println!("(driver released)");
    Ok(())
}

fn db_str(db: f32) -> String {
    if db <= -150.0 {
        "  -inf".to_string()
    } else {
        format!("{db:6.1}")
    }
}

fn hardening_line(h: &router_engine::HardeningReport) -> String {
    format!("hardening: {}", h.summary())
}

/// Header lines about the source: name, rate and period of the running session.
fn source_lines(s: &EngineStatus) -> Vec<String> {
    let mut v = Vec::new();
    if s.cable_rate > 0 {
        let ms = f64::from(s.capture_period_us) / 1000.0;
        v.push(format!(
            "source: \"{}\" {} Hz, period {} fr ({ms:.1} ms)",
            s.source_name, s.cable_rate, s.capture_period_frames,
        ));
    }
    v
}

fn status_line(t: f64, s: &EngineStatus, peak_in: [f32; 2], peak_out: [f32; 2]) -> String {
    let state = match &s.state {
        EngineState::Restarting { reason } => format!("Restarting({reason})"),
        EngineState::WaitingForDevice { holders } => format!("WaitingForDevice({})", holders.len()),
        other => format!("{other:?}"),
    };
    format!(
        "[{t:4.0}s] {state} | {} {} Hz ({:.1} ms period) -> asio {} Hz buf {} | ring {}/{} fr | under {} over {} xrun {} discarded {} | fault {:?} | cpu {:.2}% (max {:.2}%, {:.0}/{:.0} us) | peak in {} / out {} dBFS | lat {:.1} ms (out {:.1} + ring {:.1} + cap {:.1} + render {:.0} {}) | ring mean {} fr, headroom {}/{} fr, arrival p99.9 gap {:.1} ms dip {} fr | safety {:.1} ms drift {:+.0} ppm",
        s.source_name,
        s.cable_rate,
        f64::from(s.capture_period_us) / 1000.0,
        s.asio_rate,
        s.buffer_size,
        s.ring_fill_frames,
        s.ring_target_frames,
        s.underruns,
        s.overruns,
        s.xruns,
        s.discarded_frames,
        s.fault,
        s.cpu_avg_pct,
        s.cpu_max_pct,
        s.cpu_avg_us,
        s.cpu_max_us,
        db_str(peak_in[0].max(peak_in[1])),
        db_str(peak_out[0].max(peak_out[1])),
        s.latency.total_ms,
        s.latency.master_out_ms,
        s.latency.ring_ms,
        s.latency.capture_ms,
        s.latency.render_side_ms,
        if s.latency.render_basis.is_measured() { "measured" } else { "est, not measured" },
        s.ring_mean_fill_frames,
        s.jitter_headroom_frames,
        s.required_headroom_frames,
        s.arrival_gap_p999_ms,
        s.arrival_dip_p999_frames,
        s.safety_buffer_ms,
        s.drift_ppm,
    )
}

/// Builds the run configuration from the config file and the shared flags (`--config`, `--master`,
/// `--rate`, `--safety-ms`, `--preamp-db`, `--bypass`); used by `run` and `latency`.
fn build_run_config(args: &[String]) -> Result<(Config, Option<PathBuf>)> {
    let cfg_path: Option<PathBuf> = match opt(args, "--config")? {
        Some(p) => Some(PathBuf::from(p)),
        None => eq_core::default_path().filter(|p| p.exists()),
    };
    let mut cfg = match &cfg_path {
        Some(p) if p.exists() => {
            let c = Config::load(p).with_context(|| format!("loading {}", p.display()))?;
            if let Some(n) = eq_core::take_deprecation_notice() {
                println!("note: {n}");
            }
            c.sanitized()
        }
        Some(p) => bail!("config file {} does not exist", p.display()),
        None => Config::default(),
    };
    if let Some(m) = opt(args, "--master")? {
        cfg.master_driver = m.to_string();
    }
    if let Some(r) = opt(args, "--rate")? {
        cfg.sample_rate = r.parse().context("--rate")?;
    }
    if let Some(s) = opt(args, "--safety-ms")? {
        cfg.safety_buffer_ms = s.parse().context("--safety-ms")?;
    }
    if let Some(p) = opt(args, "--preamp-db")? {
        cfg.profile.preamp_db = p.parse().context("--preamp-db")?;
    }
    if args.iter().any(|a| a == "--bypass") {
        cfg.bypass = true;
    }
    Ok((cfg.sanitized(), cfg_path))
}

fn run(args: &[String]) -> Result<()> {
    let (cfg, cfg_path) = build_run_config(args)?;
    let seconds: Option<f64> = opt(args, "--seconds")?.map(str::parse).transpose().context("--seconds")?;
    println!(
        "master \"{}\"  rate {}  preamp {:+.1} dB  bypass {}  limiter {}  safety {:.1} ms{}  filters {}",
        if cfg.master_driver.is_empty() { "(default)" } else { &cfg.master_driver },
        if cfg.sample_rate == 0 { "keep".to_string() } else { cfg.sample_rate.to_string() },
        cfg.profile.preamp_db,
        cfg.bypass,
        cfg.limiter,
        cfg.safety_buffer_ms,
        if cfg.safety_buffer_auto { " (auto)" } else { "" },
        cfg.profile.filters.len(),
    );

    // SAFETY: the handler only stores into an atomic.
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(on_ctrl), true);
    }
    let dump_dir = opt(args, "--dump-dir")?.map(PathBuf::from);
    // --synthetic RATE[:PACKET[:JITTER_MS[:FREQ[,FREQ..]]]] replaces the cable capture by a
    // generated tone (measurements at any cable rate without touching Windows settings).
    let synthetic = match opt(args, "--synthetic")? {
        None => None,
        Some(spec) => {
            let mut it = spec.split(':');
            let rate: u32 = it.next().unwrap_or("48000").parse().context("--synthetic rate")?;
            let packet: u32 = it.next().map(str::parse).transpose().context("--synthetic packet")?.unwrap_or(rate / 100);
            let jitter: f32 = it.next().map(str::parse).transpose().context("--synthetic jitter")?.unwrap_or(0.0);
            let freqs: Vec<f32> = match it.next() {
                Some(f) => f.split(',').map(str::parse).collect::<Result<_, _>>().context("--synthetic freqs")?,
                None => vec![1000.0],
            };
            let db: f32 = opt(args, "--synthetic-db")?.map(str::parse).transpose().context("--synthetic-db")?.unwrap_or(-30.0);
            Some(router_engine::SyntheticSource { rate, packet_frames: packet, freqs, dbfs: db, jitter_ms: jitter })
        }
    };
    let opts = EngineOptions {
        config_path: cfg_path,
        tap: dump_dir.clone().map(|dir| router_engine::TapConfig { dir }),
        synthetic,
        ..EngineOptions::default()
    };
    let engine = Engine::start_with(cfg, opts).map_err(|e| anyhow!("{e}"))?;
    let started = engine.status();
    if started.cable_rate > 0 {
        println!(
            "rate: {} Hz{}",
            started.cable_rate,
            if started.cable_rate == started.asio_rate || started.asio_rate == 0 {
                " (matches the Yamaha)".to_string()
            } else {
                " (differs from the Yamaha; resampling)".to_string()
            }
        );
    }
    for l in source_lines(&started) {
        println!("{l}");
    }
    println!("{}", hardening_line(&started.hardening));
    println!("engine started; Ctrl+C to stop{}", seconds.map(|s| format!(" (auto-stop after {s} s)")).unwrap_or_default());

    let t0 = Instant::now();
    let learn = args.iter().any(|a| a == "--learn");
    let mut learn_started = false;
    let mut learn_done = false;
    let mut announced_wait = false;
    let mut next_print = t0 + Duration::from_secs(1);
    let mut hold_in = [-200.0f32; 2];
    let mut hold_out = [-200.0f32; 2];
    let result = loop {
        if STOP.load(Ordering::SeqCst) {
            println!("Ctrl+C: stopping");
            break Ok(());
        }
        let elapsed = t0.elapsed();
        if seconds.is_some_and(|s| elapsed.as_secs_f64() >= s) {
            break Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
        let s = engine.status();
        if learn && !learn_started && !learn_done && s.state.is_running() && elapsed.as_secs_f64() >= 5.0 {
            learn_started = true;
            engine.learn_jitter();
            println!("learn: measuring for 30 s ...");
        }
        if learn && learn_started && !s.learn.active && s.learn.finished > 0 {
            match (&s.learn.result_ms, &s.learn.error) {
                (Some(ms), _) => println!("learn: lowest safe jitter margin {ms:.1} ms (stored with Config::set_jitter_margin; on-disk key jitter_margin_by_device[\"vbmatrix\"] = {ms:.1})"),
                (_, Some(e)) => println!("learn failed: {e}"),
                _ => {}
            }
            learn_started = false;
            learn_done = true;
        }
        for i in 0..2 {
            hold_in[i] = hold_in[i].max(s.peak_in_db[i]);
            hold_out[i] = hold_out[i].max(s.peak_out_db[i]);
        }
        if s.state.is_waiting() {
            if !announced_wait {
                announced_wait = true;
                println!("waiting for the Yamaha to be released...");
                println!("{}", s.last_error.clone().unwrap_or_default());
            }
            if Instant::now() >= next_print {
                next_print += Duration::from_secs(1);
            }
            continue;
        }
        announced_wait &= !s.state.is_running();
        if Instant::now() >= next_print {
            next_print += Duration::from_secs(1);
            println!("{}", status_line(elapsed.as_secs_f64(), &s, hold_in, hold_out));
            hold_in = [-200.0; 2];
            hold_out = [-200.0; 2];
            if s.state == EngineState::Failed {
                break Err(anyhow!("engine failed: {}", s.last_error.unwrap_or_default()));
            }
        }
    };
    let last = engine.status();
    let timed_out_waiting = last.state.is_waiting();
    engine.stop();
    // The line printed at start is taken before the threads run (items still "pending"); this is the live state.
    println!("{}", hardening_line(&last.hardening).replacen("hardening:", "hardening (at stop):", 1));
    println!(
        "final: callbacks {} underruns {} overruns {} xruns {} restarts {} fault {:?} cpu avg {:.2}% max {:.2}% burst max {} fr, discarded {} fr",
        last.callbacks,
        last.underruns,
        last.overruns,
        last.xruns,
        last.restarts,
        last.fault,
        last.cpu_avg_pct,
        last.cpu_max_pct,
        last.capture_burst_max_frames,
        last.discarded_frames,
    );
    if let Some(e) = last.last_error {
        println!("last error: {e}");
    }
    println!("engine stopped; master released");
    if let Some(d) = &dump_dir {
        match router_engine::tap::read_report(d) {
            Some(r) => print!("tap dump in {}:
{r}", d.display()),
            None => println!("tap dump: no report written in {}", d.display()),
        }
    }
    if timed_out_waiting {
        eprintln!("timed out: the master driver was still in use by another program");
        std::process::exit(2);
    }
    result
}

/// `router-cli analyze FILE`: sine-fit glitch analysis plus music-safe statistics.
fn analyze(args: &[String]) -> Result<()> {
    use router_core::analyze::{analyze_sine, format_report, SineParams};
    let file = args.first().filter(|a| !a.starts_with("--")).ok_or_else(|| anyhow!("analyze needs a WAV file"))?;
    let mut p = SineParams::default();
    if let Some(v) = opt(args, "--freq")? {
        p.freq_hint = v.parse().context("--freq")?;
    }
    if let Some(v) = opt(args, "--skip")? {
        p.skip_s = v.parse().context("--skip")?;
    }
    if let Some(v) = opt(args, "--tail")? {
        p.tail_s = v.parse().context("--tail")?;
    }
    if let Some(v) = opt(args, "--mads")? {
        p.spike_mads = v.parse().context("--mads")?;
    }
    let wav = router_core::wav::read(std::path::Path::new(file)).with_context(|| format!("reading {file}"))?;
    let only: Option<usize> = opt(args, "--channel")?.map(str::parse).transpose().context("--channel")?;
    for (i, ch) in wav.channels.iter().enumerate() {
        if only.is_some_and(|c| c != i) {
            continue;
        }
        let r = analyze_sine(ch, wav.rate, &p);
        print!("{}", format_report(&format!("{file} ch{i}"), &r));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from(args: &[&str]) -> Config {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!("eq_router_cli_test_{}_{}.toml", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        Config::default().save(&path).unwrap();
        let mut v: Vec<String> = vec!["--config".into(), path.to_string_lossy().into_owned()];
        v.extend(args.iter().map(|s| s.to_string()));
        let cfg = build_run_config(&v).unwrap().0;
        let _ = std::fs::remove_file(&path);
        cfg
    }

    #[test]
    fn removed_capture_flags_are_rejected() {
        // M3a hard cutover: the WASAPI-era source flags no longer exist.
        for a in [
            &["--source", "vac"][..],
            &["--capture", "X"][..],
            &["--period-ms", "6"][..],
            &["--keepalive"][..],
            &["--no-keepalive"][..],
            &["--mode", "shared"][..],
            &["--no-match-cable"][..],
        ] {
            let mut args = vec!["--forever"];
            args.extend_from_slice(a);
            assert!(p(&args).is_err(), "run {a:?} must be rejected");
            assert!(p(&["latency"]).is_ok());
            let mut l = vec!["latency"];
            l.extend_from_slice(a);
            assert!(p(&l).is_err(), "latency {a:?} must be rejected");
        }
    }

    fn p(args: &[&str]) -> Result<Command, String> {
        parse_command(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn shared_overrides_apply() {
        assert_eq!(cfg_from(&["--master", "Other ASIO"]).master_driver, "Other ASIO");
        assert_eq!(cfg_from(&["--rate", "96000"]).sample_rate, 96000);
        assert_eq!(cfg_from(&["--safety-ms", "7"]).safety_buffer_ms, 7.0);
        assert!(cfg_from(&["--bypass"]).bypass);
        assert!(!cfg_from(&[]).bypass);
    }

    #[test]
    fn help_is_accepted_everywhere() {
        for a in [&["--help"][..], &["-h"], &["help"], &[], &["run", "--help"], &["latency", "-h"], &["caps", "help"]] {
            assert_eq!(p(a), Ok(Command::Help), "{a:?}");
        }
    }

    #[test]
    fn unknown_flags_and_subcommands_are_rejected() {
        for a in [
            &["--bogus"][..],
            &["--seconds", "5"],
            &["frobnicate"],
            &["run", "--bogus", "--forever"],
            &["run", "--seconds", "5", "--oops"],
            &["latency", "--bogus"],
            &["caps", "--nope"],
            &["holders", "extra"],
            &["devices", "--x"],
            &["run", "--seconds"],
        ] {
            assert!(p(a).is_err(), "{a:?}");
        }
    }

    #[test]
    fn unbounded_run_needs_forever() {
        assert!(p(&["run"]).is_err());
        assert!(p(&["run", "--learn"]).is_err());
        assert_eq!(p(&["run", "--forever"]), Ok(Command::Run));
        assert_eq!(p(&["run", "--seconds", "30", "--preamp-db", "-20"]), Ok(Command::Run));
        assert!(p(&["run", "--seconds", "3", "--forever"]).is_err());
    }

    #[test]
    fn known_subcommands_pass() {
        assert_eq!(p(&["devices"]), Ok(Command::Devices));
        assert_eq!(p(&["caps", "--master", "X"]), Ok(Command::Caps));
        assert_eq!(p(&["holders"]), Ok(Command::Holders));
        assert_eq!(p(&["analyze", "f.wav", "--freq", "1000"]), Ok(Command::Analyze));
        assert_eq!(p(&["latency", "--shape", "burst:18500:2", "--db", "-35", "--json"]), Ok(Command::Latency));
        assert_eq!(p(&["latency", "--baseline"]), Ok(Command::Latency));
    }
}
