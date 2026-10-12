//! `router-cli latency`: end-to-end click latency of the router.
//!
//! A very quiet windowed click is rendered into VBMatrix's render endpoint ("VBMatrix In *", the
//! app -> VBMatrix side), travels VBMatrix -> virtual ASIO driver -> ring A -> resampler -> EQ ->
//! Yamaha ASIO output, and is detected at two points with QPC timestamps: on the bridge source
//! side (stage 1, the ring-A source tap of the running engine) and on the Yamaha's own loopback
//! inputs ("UR22C Streaming L/R", stage 2). See `docs/latency.md` for the definitions.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use router_engine::baseline::{
    auto_highpass_hz, channel_similarity, plan_thresholds_checked, spectrum_peaks, ChannelReport, HP_LOOSENING_MIN_HZ, MONITOR_THRESHOLDS_DB,
};
use router_engine::probe::{
    band_gain_db, band_onset_bias_frames, measure, onset_bias_frames, outliers, stats, synthetic_run, Measurement, SyntheticPath, Timebase,
};
use router_engine::{Engine, EngineOptions, EngineState, EngineStatus, LatencyProbe, LoopbackProbe};
use wasapi_capture::{
    find_device, is_sink_allowed, is_vbmatrix_render_name, list_endpoints, qpc_hz, validate_peak_dbfs, Flow, ImpulseRenderOptions,
    ImpulseRenderer, ImpulseShape, ImpulseTimes, RenderFill, RenderMode, RenderPrefill, DEFAULT_IMPULSE_DBFS, MAX_IMPULSE_DBFS,
};
use eq_core::{MeasuredEntry, MeasuredLatency, DEFAULT_RENDER_PROFILE, MIN_CLICKS_TO_SAVE};

/// Preamp forced for the measurement (dB): with a click of at most -35 dBFS the Yamaha output stays near -55 dBFS.
pub const FORCED_PREAMP_DB: f32 = -20.0;
/// Hard limits.
const MAX_COUNT: u32 = 10;
const MAX_WAIT_PER_CLICK: Duration = Duration::from_secs(4);
/// The router's output must stay below this while measuring (dBFS); above it the run is aborted at once.
const OUTPUT_ABORT_DBFS: f32 = -40.0;
/// Before the first click the router output must be quieter than this (the source must be idle).
const IDLE_OUTPUT_DBFS: f32 = -70.0;
/// Length of the quiet baseline that precedes the clicks (thresholds are derived from it).
const BASELINE_MS: u64 = 1000;
/// A detected loopback click must have a peak within this many dB of the expected level, else it is ignored.
const PEAK_WINDOW_DB: f64 = 12.0;
/// `--baseline` window (seconds): default and limits.
const BASELINE_DEFAULT_S: f32 = 3.0;
const BASELINE_MAX_S: f32 = 10.0;
/// High-pass corner of the `--baseline` report (Hz).
const BASELINE_HP_HZ: f32 = 16000.0;

fn opt<'a>(args: &'a [String], flag: &str) -> Result<Option<&'a str>> {
    match args.iter().position(|a| a == flag) {
        None => Ok(None),
        Some(i) => args.get(i + 1).map(|s| Some(s.as_str())).ok_or_else(|| anyhow!("{flag} needs a value")),
    }
}

fn flag(args: &[String], f: &str) -> bool {
    args.iter().any(|a| a == f)
}

/// Options after parsing and validation.
#[derive(Clone, Debug)]
pub struct Options {
    pub count: u32,
    pub interval_ms: f32,
    pub db: f32,
    pub shape: ImpulseShape,
    pub sink: Option<String>,
    pub allow_sink: Vec<String>,
    pub json: bool,
    pub cap_threshold_db: Option<f32>,
    pub lb_threshold_db: Option<f32>,
    /// Baseline-only diagnostic: no clicks, report the levels of the capture and loopback detectors.
    pub baseline: bool,
    pub seconds: f32,
    /// `--baseline --dump PATH`: also write the first samples of the loopback pair as a float WAV.
    pub dump: Option<String>,
    /// `--hp-hz N` (Some(Some(N))), `--hp-hz off` (Some(None)) or automatic from the click shape (None).
    pub hp_hz: Option<Option<f32>>,
    /// Render stream options (`--render-mode`, `--render-buffer-frames`, `--render-fill`, `--render-prefill`).
    pub render: ImpulseRenderOptions,
    /// `--no-save`: never write `latency_measured.toml`.
    pub no_save: bool,
    /// `--save`: also store a run made with non-default render options (kept under its own render profile).
    pub save_non_default: bool,
}

/// Name of the render profile stored with a measurement: `default` for the tool's standard stream, else the options.
pub fn render_profile(r: &ImpulseRenderOptions) -> String {
    if r.mode == RenderMode::SharedDefault && r.buffer_frames.is_none() && r.fill == RenderFill::Period && r.prefill == RenderPrefill::Period {
        return DEFAULT_RENDER_PROFILE.to_string();
    }
    format!(
        "{}/buf-{}/fill-{}/prefill-{}",
        r.mode.name(),
        r.buffer_frames.map(|b| b.to_string()).unwrap_or_else(|| "auto".into()),
        r.fill.describe(),
        match r.prefill {
            RenderPrefill::Full => "full".to_string(),
            RenderPrefill::Period => "period".to_string(),
            RenderPrefill::Frames(n) => n.to_string(),
        }
    )
}

fn parse_options(args: &[String]) -> Result<Options> {
    let count: u32 = opt(args, "--count")?.map(str::parse).transpose().context("--count")?.unwrap_or(5);
    if count == 0 || count > MAX_COUNT {
        bail!("--count must be 1..={MAX_COUNT}");
    }
    let interval_ms: f32 = opt(args, "--interval-ms")?.map(str::parse).transpose().context("--interval-ms")?.unwrap_or(1200.0);
    if !(500.0..=5000.0).contains(&interval_ms) {
        bail!("--interval-ms must be 500..=5000");
    }
    let db: f32 = opt(args, "--db")?.map(str::parse).transpose().context("--db")?.unwrap_or(DEFAULT_IMPULSE_DBFS);
    validate_peak_dbfs(db).map_err(|e| anyhow!("--db: {e}"))?;
    let shape = match opt(args, "--shape")? {
        None => ImpulseShape::default(),
        Some(s) => ImpulseShape::parse(s).ok_or_else(|| anyhow!("--shape: use burst[:HZ[:MS]] or chirp[:F0:F1[:MS]]"))?,
    };
    let seconds: f32 = opt(args, "--seconds")?.map(str::parse).transpose().context("--seconds")?.unwrap_or(BASELINE_DEFAULT_S);
    if !(0.5..=BASELINE_MAX_S).contains(&seconds) {
        bail!("--seconds must be 0.5..={BASELINE_MAX_S}");
    }
    let mut render = ImpulseRenderOptions::new("");
    render.apply_render_args(&|k| opt(args, k).ok().flatten().map(str::to_string)).map_err(|e| anyhow!("{e}"))?;
    Ok(Options {
        render,
        no_save: flag(args, "--no-save"),
        save_non_default: flag(args, "--save"),
        baseline: flag(args, "--baseline"),
        dump: opt(args, "--dump")?.map(str::to_string),
        hp_hz: match opt(args, "--hp-hz")? {
            None => None,
            Some("off") => Some(None),
            Some(v) => {
                let f: f32 = v.parse().context("--hp-hz")?;
                if !(2000.0..=22000.0).contains(&f) {
                    bail!("--hp-hz must be 2000..=22000 (or off)");
                }
                Some(Some(f))
            }
        },
        seconds,
        count,
        interval_ms,
        db,
        shape,
        sink: opt(args, "--sink")?.map(str::to_string),
        allow_sink: opt(args, "--allow-sink")?.map(|s| vec![s.to_string()]).unwrap_or_default(),
        json: flag(args, "--json"),
        cap_threshold_db: opt(args, "--cap-threshold-db")?.map(str::parse).transpose().context("--cap-threshold-db")?,
        lb_threshold_db: opt(args, "--lb-threshold-db")?.map(str::parse).transpose().context("--lb-threshold-db")?,
    })
}

/// Default sink: VBMatrix's render endpoint ("VBMatrix In ..."), where applications render since
/// the bridge cutover — the only place a click enters the router chain. `None` if VBMatrix exposes
/// no such endpoint (pass `--sink` explicitly).
pub fn default_sink() -> Option<String> {
    list_endpoints(Flow::Render).ok()?.into_iter().find(|e| is_vbmatrix_render_name(&e.name)).map(|e| e.name)
}

/// One finished impulse.
#[derive(Clone, Debug)]
struct Row {
    m: Measurement,
    render_queue_clock: bool,
    padding_queue_ms: f64,
    padding_frames: u32,
    rate_hz: f64,
    capture_seen: bool,
}

pub fn run(args: &[String]) -> Result<()> {
    if flag(args, "--selftest") {
        return selftest();
    }
    let o = parse_options(args)?;
    let (mut cfg, _cfg_path) = crate::build_run_config(args)?;
    // Hard safety: forced very quiet path, limiter on, EQ not bypassed (the forced preamp must apply).
    cfg.profile.preamp_db = f64::from(FORCED_PREAMP_DB);
    cfg.limiter = true;
    cfg.bypass = false;
    cfg.eq_enabled = true;
    let cfg = cfg.sanitized();
    if o.baseline {
        return run_baseline(args, &o, &cfg);
    }

    let sink_name = match (&o.sink, default_sink()) {
        (Some(s), _) => s.clone(),
        (None, Some(s)) => s,
        (None, None) => bail!("no VBMatrix render endpoint (\"VBMatrix In ...\") found; pass --sink"),
    };
    // Resolve and vet the sink BEFORE touching the Yamaha.
    let ep = find_device(Flow::Render, &sink_name).map_err(|e| anyhow!("sink \"{sink_name}\": {e}"))?;
    if !is_sink_allowed(&ep.name, &o.allow_sink) {
        bail!(
            "refusing sink \"{}\": it is not a known virtual cable or a VBMatrix render endpoint (a real output could be loud). Use \"VBMatrix In ...\", or --allow-sink \"{}\" if you really mean it",
            ep.name,
            ep.name
        );
    }
    println!(
        "latency: click {:.0} dBFS peak ({}), {} impulses every {:.0} ms into \"{}\"; preamp forced to {FORCED_PREAMP_DB} dB, limiter on",
        o.db,
        describe_shape(o.shape),
        o.count,
        o.interval_ms,
        ep.name
    );
    if !is_vbmatrix_render_name(&ep.name) {
        println!(
            "note: the sink \"{}\" is not a VBMatrix render endpoint (\"VBMatrix In ...\"): the click reaches the router only if VBMatrix routes it there",
            ep.name
        );
    }

    let engine = start_engine(args, &cfg)?;
    // The engine is dropped (and the Yamaha released) on every path out of `measure_all`.
    let result = measure_all(&engine, &cfg, &o, &ep.name);
    let last = engine.status();
    engine.stop();
    println!("engine stopped; Yamaha released");
    let _ = last;
    match result {
        Ok(Outcome::Done) => Ok(()),
        Ok(Outcome::Pending(why)) => {
            println!("{why}");
            std::process::exit(2);
        }
        Err(e) => Err(e),
    }
}

/// Checks that the Yamaha is free (never waits, never closes anything of the user's) and starts the engine with
/// the loopback probe: no config file writes, no capture publication, no status mapping.
fn start_engine(args: &[String], cfg: &eq_core::Config) -> Result<Engine> {
    let master = if cfg.master_driver.trim().is_empty() { router_engine::logic::DEFAULT_MASTER.to_string() } else { cfg.master_driver.clone() };
    if let Ok(info) = asio_host::find_driver(&master) {
        let holders = asio_host::find_driver_holders(&info);
        if !holders.is_empty() {
            println!("holders: {}", asio_host::describe_holders(&holders));
            println!("The Yamaha ({master}) is in use; not waiting and not closing anything. The live measurement is PENDING.");
            println!("When it is free (quit the tray app), run:  router-cli latency {}", args.join(" "));
            std::process::exit(2);
        }
    }
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleCtrlHandler(Some(crate::on_ctrl), true);
    }
    let opts = EngineOptions {
        config_path: None,
        publish_caps: false,
        status_mapping: false,
        loopback_probe: Some(LoopbackProbe::default()),
        // the model line of the run shows the pure estimate, not an older measurement
        load_measured: false,
        ..EngineOptions::default()
    };
    Engine::start_with(cfg.clone(), opts).map_err(|e| anyhow!("{e}"))
}

/// `latency --baseline`: levels at the bridge source side and the Yamaha loopback inputs with nothing injected.
fn run_baseline(args: &[String], o: &Options, cfg: &eq_core::Config) -> Result<()> {
    println!(
        "latency --baseline: {:.1} s of levels at the bridge source side and at the Yamaha loopback inputs; no click is rendered and nothing is played into any endpoint; preamp forced to {FORCED_PREAMP_DB} dB, limiter on",
        o.seconds
    );
    let engine = start_engine(args, cfg)?;
    let result = baseline_all(&engine, o);
    engine.stop();
    println!("engine stopped; Yamaha released");
    match result {
        Ok(Outcome::Done) => Ok(()),
        Ok(Outcome::Pending(why)) => {
            println!("{why}");
            std::process::exit(2);
        }
        Err(e) => Err(e),
    }
}

enum Outcome {
    Done,
    Pending(String),
}

/// Lowest frequency with significant energy in the click (Hz): the burst's centre minus about two over its duration
/// (main lobe of the raised-cosine window), the chirp's lower end minus one over its duration.
fn shape_low_edge_hz(s: ImpulseShape) -> f32 {
    match s {
        ImpulseShape::Burst { freq_hz, dur_ms } => freq_hz - 2000.0 / dur_ms.max(0.1),
        ImpulseShape::Chirp { f0_hz, f1_hz, dur_ms } => f0_hz.min(f1_hz) - 1000.0 / dur_ms.max(0.1),
    }
}

/// True if the raw loopback abort limit may be relaxed from -40 to -20 dBFS: only for `--shape burst:<hz >= 15000>`
/// with the high-pass active (the click then lies far above the band of a microphone / programme audio).
fn raw_limit_relaxed(o: &Options, hp: Option<f32>) -> bool {
    hp.is_some() && matches!(o.shape, ImpulseShape::Burst { freq_hz, .. } if freq_hz >= HP_LOOSENING_MIN_HZ)
}

/// The high-pass corner the loopback detector uses for this run.
fn detection_highpass(o: &Options) -> Option<f32> {
    match o.hp_hz {
        Some(v) => v,
        None => auto_highpass_hz(shape_low_edge_hz(o.shape)),
    }
}

fn describe_shape(s: ImpulseShape) -> String {
    match s {
        ImpulseShape::Burst { freq_hz, dur_ms } => format!("{dur_ms} ms raised-cosine burst of {freq_hz} Hz"),
        ImpulseShape::Chirp { f0_hz, f1_hz, dur_ms } => format!("{dur_ms} ms windowed chirp {f0_hz}-{f1_hz} Hz"),
    }
}

fn stopped() -> bool {
    crate::STOP.load(Ordering::SeqCst)
}

/// Level guard: tracks the highest output peak and returns an error text if it exceeds `limit`.
fn level_guard(s: &EngineStatus, limit: f32) -> Option<String> {
    let p = s.peak_out_db[0].max(s.peak_out_db[1]);
    (p > limit).then(|| format!("router output peak {p:.1} dBFS exceeded the safety limit {limit:.0} dBFS"))
}

/// Waits until the engine streams. `Ok(Some(msg))` = the Yamaha is busy (pending).
fn wait_running(engine: &Engine) -> Result<Option<String>> {
    let t_start = Instant::now();
    let mut announced = false;
    loop {
        let s = engine.status();
        if s.state.is_running() {
            break;
        }
        if s.state.is_waiting() {
            return Ok(Some(format!("The Yamaha is busy ({}). Live measurement PENDING; not waiting.", s.last_error.unwrap_or_default())));
        }
        if s.state == EngineState::Failed || t_start.elapsed() > Duration::from_secs(20) {
            bail!("engine did not start: {}", s.last_error.unwrap_or_else(|| format!("{:?}", s.state)));
        }
        if !announced && t_start.elapsed() > Duration::from_secs(3) {
            announced = true;
            println!("waiting for the engine ({:?}) ...", s.state);
        }
        if stopped() {
            bail!("interrupted");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(None)
}

fn print_loopback_info(info: &router_engine::ProbeInfo) {
    println!(
        "loopback inputs: {} \"{}\" and {} \"{}\"; asio {} Hz, buffer {} fr, driver latency in {} / out {} samples",
        info.input_channels[0],
        info.input_names[0],
        info.input_channels[1],
        info.input_names[1],
        info.asio_rate,
        info.buffer_frames,
        info.input_latency,
        info.output_latency
    );
}

/// Steady state: >= 3 s of running, gate open (mean fill known), no new underruns for 1 s; the output stays quiet.
fn wait_steady(engine: &Engine, idle_msg: &str) -> Result<()> {
    let t_run = Instant::now();
    let mut last_under = engine.status().underruns;
    let mut last_change = Instant::now();
    loop {
        if stopped() {
            bail!("interrupted");
        }
        let s = engine.status();
        if let Some(e) = level_guard(&s, OUTPUT_ABORT_DBFS) {
            bail!("{e} {idle_msg}; aborting");
        }
        if s.underruns != last_under {
            last_under = s.underruns;
            last_change = Instant::now();
        }
        if !s.state.is_running() {
            bail!("engine left the running state: {:?}", s.state);
        }
        if s.uptime_ms >= 3000 && s.ring_mean_fill_frames > 0 && last_change.elapsed() >= Duration::from_secs(1) {
            break;
        }
        if t_run.elapsed() > Duration::from_secs(25) {
            bail!("no steady state after 25 s (callbacks {}, ring mean {} fr, underruns {})", s.callbacks, s.ring_mean_fill_frames, s.underruns);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let s = engine.status();
    // The status snapshot is refreshed periodically: printing the header only now shows the live source.
    for l in capture_summary(&s) {
        println!("{l}");
    }
    println!("steady state reached (ring mean {} fr, target {} fr, underruns {})", s.ring_mean_fill_frames, s.ring_target_frames, s.underruns);
    Ok(())
}

fn measure_all(engine: &Engine, cfg: &eq_core::Config, o: &Options, sink: &str) -> Result<Outcome> {
    let probe = engine.latency_probe().ok_or_else(|| anyhow!("engine has no latency probe"))?;
    if let Some(msg) = wait_running(engine)? {
        return Ok(Outcome::Pending(msg));
    }
    let info = wait_for_info(&probe)?;
    print_loopback_info(&info);

    // Render stream (silence until a click is requested). Also keeps VBMatrix's render clock running.
    let mut ropts = o.render.clone();
    ropts.endpoint = sink.to_string();
    ropts.allow_endpoint = o.allow_sink.clone();
    ropts.shape = o.shape;
    ropts.peak_dbfs = o.db;
    let mut renderer = ImpulseRenderer::start(&ropts).map_err(|e| anyhow!("render stream: {e}"))?;
    let ri = renderer.info().clone();
    println!(
        "render: \"{}\" {} Hz {} ch {}, mode {}, buffer {} fr, period {} fr ({:.1} ms), stream latency {:.1} ms",
        ri.endpoint,
        ri.format.sample_rate,
        ri.format.channels,
        ri.format.format.name(),
        ri.mode.name(),
        ri.buffer_frames,
        ri.period_frames,
        f64::from(ri.period_frames) * 1000.0 / f64::from(ri.format.sample_rate.max(1)),
        ri.stream_latency_ms
    );
    let fp = |v: Option<(u32, u32)>| v.map(|(f, r)| format!("{f} fr ({:.1} ms)", f64::from(f) * 1000.0 / f64::from(r.max(1)))).unwrap_or_else(|| "?".into());
    println!(
        "render: fill {}, prefill {} fr, endpoint min/default engine period {}/{} fr, engine period before {} -> after {} (render profile: {})",
        ri.fill.describe(),
        ri.prefill_frames,
        ri.min_period_frames.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
        ri.default_period_frames.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
        fp(ri.engine_before),
        fp(ri.engine_after),
        render_profile(&ropts)
    );

    wait_steady(engine, "before any click (something else is playing into VBMatrix)")?;

    // Quiet baseline: statistics of both detectors' inputs; the thresholds are derived from them (DC removed).
    let click_db = o.db;
    let hp = detection_highpass(o);
    probe.set_highpass(hp);
    if let Some(fc) = hp {
        println!("loopback detector: high-pass {fc:.0} Hz (8th-order Butterworth) so the click is detected above band-limited programme audio");
    }
    probe.monitor_start();
    let mut worst_out = -200.0f32;
    let t_base = Instant::now();
    while t_base.elapsed() < Duration::from_millis(BASELINE_MS) {
        let s = engine.status();
        worst_out = worst_out.max(s.peak_out_db[0].max(s.peak_out_db[1]));
        std::thread::sleep(Duration::from_millis(25));
    }
    let rep = probe.monitor_stop();
    let rep_hp = hp.map(|_| probe.monitor_hp());
    print_baseline_short(&rep, rep_hp.as_ref());
    if worst_out > IDLE_OUTPUT_DBFS {
        bail!("baseline is not quiet at the router output: peak {worst_out:.1} dBFS (limit {IDLE_OUTPUT_DBFS:.0}): another program is playing into VBMatrix; aborting");
    }
    let env_gain = hp.map(|fc| band_gain_db(o.shape, info.asio_rate, fc)).unwrap_or(0.0);
    let thr = plan_thresholds_checked(
        &rep,
        rep_hp.as_ref(),
        env_gain,
        click_db,
        cfg.profile.preamp_db as f32,
        o.cap_threshold_db,
        o.lb_threshold_db,
        raw_limit_relaxed(o, hp),
    )
    .map_err(|e| anyhow!("{e}"))?;
    for n in &thr.notes {
        if n.starts_with("SAFETY NOTE") {
            println!("*** {n}");
        } else {
            println!("{n}");
        }
    }
    println!(
        "thresholds: source {:.1} dBFS (floor {:.1}, click {:.0}); loopback {:.1} dBFS (floor {:.1}, expected click {:.0} dBFS); baseline means (linear) removed: source {:+.2e}/{:+.2e}, loopback {:+.2e}/{:+.2e}",
        thr.cap_db, thr.cap_floor_db, click_db, thr.lb_db, thr.lb_floor_db, thr.lb_expected_db, thr.dc[0], thr.dc[1], thr.dc[2], thr.dc[3]
    );
    let (cap_thr, lb_thr, dc, lb_expected_db) = (thr.cap_db, thr.lb_db, thr.dc, thr.lb_expected_db);
    let mut spurious = 0u32;

    // Measure.
    let hz = qpc_hz();
    let mut rows: Vec<Row> = Vec::new();
    let mut failures = 0u32;
    let mut aborted: Option<String> = None;
    println!();
    println!("{:>3}  {:>10}  {:>10}  {:>12}  {:>10}  {:>10}  {:>11}  notes", "#", "inj->cap", "(a) total", "(b) corrected", "cap->tap", "render q", "to-DAC est");
    for k in 0..o.count {
        if stopped() {
            aborted = Some("interrupted".into());
            break;
        }
        let t_iter = Instant::now();
        probe.arm_with_dc(cap_thr, lb_thr, dc);
        let times = match renderer.fire() {
            Ok(t) => t,
            Err(e) => {
                probe.disarm();
                aborted = Some(format!("click injection failed: {e}"));
                break;
            }
        };
        let deadline = Instant::now() + MAX_WAIT_PER_CLICK;
        let mut result = None;
        let mut held_cap = None;
        while Instant::now() < deadline {
            if let Some(mut r) = probe.take_result() {
                // A genuine click arrives at about the expected level; a much weaker or stronger crossing is a stray
                // event in the loopback (a noise burst, a keystroke on the microphone): ignore it and keep waiting.
                let pk_db = 20.0 * f64::from(r.loopback.peak.max(1e-9)).log10();
                if (pk_db - lb_expected_db).abs() > PEAK_WINDOW_DB {
                    spurious += 1;
                    println!("      ignored a loopback crossing at {pk_db:.1} dBFS (expected about {lb_expected_db:.0} +-{PEAK_WINDOW_DB:.0}): not the click");
                    if r.capture.is_some() {
                        held_cap = r.capture;
                    }
                    probe.rearm_loopback(lb_thr, dc);
                    continue;
                }
                if r.capture.is_none() {
                    r.capture = held_cap;
                }
                result = Some(r);
                break;
            }
            let s = engine.status();
            if let Some(e) = level_guard(&s, OUTPUT_ABORT_DBFS) {
                aborted = Some(e);
                break;
            }
            if stopped() {
                aborted = Some("interrupted".into());
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        probe.disarm();
        if let Some(a) = &aborted {
            println!("ABORT: {a}");
            break;
        }
        match result {
            None => {
                failures += 1;
                println!(
                    "{:>3}  {:>10}  no click detected within {:.0} s (stage 1 {})",
                    k + 1,
                    "-",
                    MAX_WAIT_PER_CLICK.as_secs_f64(),
                    if probe.capture_seen() { "saw it: the loopback input did not" } else { "did not see it either: wrong sink, or no audio path" }
                );
            }
            Some(r) => {
                let info = probe.info().ok_or_else(|| anyhow!("probe info vanished"))?;
                let tb = Timebase {
                    tick_hz: hz,
                    asio_rate: info.asio_rate,
                    buffer_frames: info.buffer_frames,
                    input_latency: info.input_latency,
                    output_latency: info.output_latency,
                };
                let bias = match hp {
                    Some(fc) => band_onset_bias_frames(o.shape, info.asio_rate, fc, r.loopback.threshold, r.loopback.peak),
                    None => onset_bias_frames(o.shape, info.asio_rate, r.loopback.threshold, r.loopback.peak),
                };
                let audible = times.audible_clock_ticks.unwrap_or(times.audible_padding_ticks);
                let m = measure(&tb, times.submit_ticks, Some(audible), r.capture.as_ref(), &r.loopback, bias);
                if !plausible(&m) {
                    spurious += 1;
                    println!(
                        "{:>3}  discarded: stray loopback event, not the click (stage-1 detection {}, total {:.1} ms, loopback peak {:.0} dBFS)",
                        k + 1,
                        if m.stage1_ms.is_some() { "present" } else { "missing" },
                        m.total_ms,
                        20.0 * r.loopback.peak.max(1e-9).log10()
                    );
                    continue;
                }
                let row = Row {
                    m,
                    render_queue_clock: times.audible_clock_ticks.is_some(),
                    padding_queue_ms: times.padding_delay_ms(hz),
                    padding_frames: times.padding_frames,
                    rate_hz: f64::from(ri.format.sample_rate),
                    capture_seen: r.capture.is_some(),
                };
                let f = |v: Option<f64>| v.map(|x| format!("{x:8.2} ms")).unwrap_or_else(|| "         -".into());
                println!(
                    "{:>3}  {}  {:7.2} ms  {:9.2} ms  {}  {}  {:8.2} ms  {}",
                    k + 1,
                    f(m.stage1_ms),
                    m.total_ms,
                    m.corrected_ms,
                    f(m.stage2_ms),
                    f(m.render_queue_ms),
                    m.to_dac_mid_ms(),
                    note_for(&row, &times, r.loopback.peak)
                );
                rows.push(row);
            }
        }
        if k + 1 < o.count {
            let wait = Duration::from_secs_f32(o.interval_ms / 1000.0).saturating_sub(t_iter.elapsed());
            std::thread::sleep(wait);
        }
    }
    renderer.stop();
    let end = engine.status();
    println!();
    if rows.is_empty() {
        println!("no valid measurement ({failures} failed).");
        if let Some(a) = aborted {
            bail!("{a}");
        }
        bail!("no click was detected on the Yamaha loopback inputs");
    }
    if spurious > 0 {
        println!("ignored {spurious} stray loopback crossings (level outside the expected click level +-{PEAK_WINDOW_DB:.0} dB)");
    }
    print_summary(&rows, &end, &probe, o, failures);
    if let Some(a) = aborted {
        println!("not saved: the run was aborted");
        bail!("run aborted early: {a}");
    }
    save_measurement(&rows, &end, o);
    Ok(Outcome::Done)
}

/// Minimal stereo 32-bit float WAV writer (diagnostic dump of the loopback samples).
fn write_wav(path: &str, l: &[f32], r: &[f32], rate: u32) -> std::io::Result<()> {
    use std::io::Write;
    let n = l.len().min(r.len());
    let data = (n * 8) as u32;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&3u16.to_le_bytes())?; // IEEE float
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * 8).to_le_bytes())?;
    f.write_all(&8u16.to_le_bytes())?;
    f.write_all(&32u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data.to_le_bytes())?;
    for i in 0..n {
        f.write_all(&l[i].to_le_bytes())?;
        f.write_all(&r[i].to_le_bytes())?;
    }
    f.flush()
}

fn fmt_db(x: f64) -> String {
    if x <= -199.0 {
        "  -inf".to_string()
    } else {
        format!("{x:6.1}")
    }
}

/// One line per channel (printed before the clicks).
fn print_baseline_short(rep: &[ChannelReport; 4], hp: Option<&[ChannelReport; 2]>) {
    let names = ["source L", "source R", "loopback L", "loopback R"];
    for (n, c) in names.iter().zip(rep) {
        println!(
            "baseline {n:<10} peak {} rms {} dc {} deviation {} (p99 {}) dBFS ({}, {} blocks)",
            fmt_db(c.peak_db),
            fmt_db(c.rms_db),
            fmt_db(c.dc_db),
            fmt_db(c.dev_peak_db),
            fmt_db(c.floor_dev_db),
            c.character.describe(),
            c.blocks
        );
    }
    if let Some(h) = hp {
        for (n, c) in ["loopback L hp", "loopback R hp"].iter().zip(h) {
            println!("baseline {n:<10} envelope peak {} (p99 {}) rms {} dBFS ({}, {} blocks)", fmt_db(c.peak_db), fmt_db(c.floor_peak_db), fmt_db(c.rms_db), c.character.describe(), c.blocks);
        }
    }
}

/// Full per-channel table of `--baseline`.
fn print_baseline_full(rep: &[ChannelReport; 4]) {
    let names = ["source L", "source R", "loopback in L", "loopback in R"];
    for (k, (n, c)) in names.iter().zip(rep).enumerate() {
        println!("{n}: {} blocks, {} samples", c.blocks, c.samples);
        println!(
            "    peak {} dBFS   rms {} dBFS   ac-rms {} dBFS   dc {:+.3e} ({} dBFS)   deviation peak {} dBFS (p99 of blocks {})",
            fmt_db(c.peak_db),
            fmt_db(c.rms_db),
            fmt_db(c.ac_rms_db),
            c.dc,
            fmt_db(c.dc_db),
            fmt_db(c.dev_peak_db),
            fmt_db(c.floor_dev_db)
        );
        if k >= 2 {
            println!(
                "    raw min/max (left-aligned i32): {} / {}   (one 24-bit LSB = 256)",
                (f64::from(c.min) * 2147483648.0).round() as i64,
                (f64::from(c.max) * 2147483648.0).round() as i64
            );
        } else {
            println!("    min/max (float): {:+.3e} / {:+.3e}", c.min, c.max);
        }
        let fr: Vec<String> = MONITOR_THRESHOLDS_DB.iter().zip(c.above_fraction).map(|(t, f)| format!("{t:.0}: {:.1}%", f * 100.0)).collect();
        println!("    blocks with peak >= dBFS  {}", fr.join("   "));
        println!("    level over the window: {} (block rms mean {} dBFS, sd {:.1} dB)", c.character.describe(), fmt_db(c.block_rms_mean_db), c.block_rms_sd_db);
    }
}

/// Read-only look at the interface mixer's per-user file (never written; dspMixFx is not started).
fn print_mixer_state() {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        println!("mixer: %APPDATA% unknown");
        return;
    };
    let path = std::path::Path::new(&appdata).join("Yamaha").join("dspMixFx UR-C").join("ur22c_setting.xml");
    match std::fs::metadata(&path) {
        Err(_) => println!("mixer: {} not found (dspMixFx settings unavailable)", path.display()),
        Ok(md) => {
            let age = md.modified().ok().and_then(|m| std::time::SystemTime::now().duration_since(m).ok()).map(|d| d.as_secs() / 86400);
            println!("mixer: {} ({} bytes, last written {} days ago; read only)", path.display(), md.len(), age.map(|a| a.to_string()).unwrap_or_else(|| "?".into()));
            if let Ok(t) = std::fs::read_to_string(&path) {
                let tag = |name: &str| t.split(&format!("<{name}>")).nth(1).and_then(|x| x.split('<').next()).map(str::to_string).unwrap_or_else(|| "?".into());
                println!(
                    "       CurrentScene {}, BusSelect {}, UserSceneDirty {}. The mixer parameters (sys_lpbk_onf, PatchLoopbackMode, SysRecnShareMode, sends) sit in the binary CurrentData blob under numeric ids without a name table, so what the loopback mixes cannot be decoded from it; it is also the last save of the app, not live device state",
                    tag("CurrentScene"),
                    tag("BusSelect"),
                    tag("UserSceneDirty")
                );
            }
        }
    }
}

fn baseline_all(engine: &Engine, o: &Options) -> Result<Outcome> {
    let probe = engine.latency_probe().ok_or_else(|| anyhow!("engine has no latency probe"))?;
    if let Some(msg) = wait_running(engine)? {
        return Ok(Outcome::Pending(msg));
    }
    let info = wait_for_info(&probe)?;
    print_loopback_info(&info);
    wait_steady(engine, "while measuring the baseline (something is playing into VBMatrix)")?;
    let hp_fc = match o.hp_hz {
        Some(Some(f)) => f,
        _ => BASELINE_HP_HZ,
    };
    probe.set_highpass(Some(hp_fc));
    println!("measuring {:.1} s ...", o.seconds);
    probe.monitor_start_raw();
    let t0 = Instant::now();
    let mut worst_out = -200.0f32;
    while t0.elapsed() < Duration::from_secs_f32(o.seconds) {
        if stopped() {
            let _ = probe.monitor_stop();
            bail!("interrupted");
        }
        let s = engine.status();
        worst_out = worst_out.max(s.peak_out_db[0].max(s.peak_out_db[1]));
        if let Some(e) = level_guard(&s, OUTPUT_ABORT_DBFS) {
            let _ = probe.monitor_stop();
            bail!("{e}; aborting");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let rep = probe.monitor_stop();
    let rep_hp = probe.monitor_hp();
    let raw = probe.monitor_raw();
    println!();
    println!("router output peak over the window: {worst_out:.1} dBFS");
    print_baseline_full(&rep);
    println!("loopback after an 8th-order Butterworth high-pass at {hp_fc:.0} Hz (what a high-frequency click would be detected through):");
    for (n, c) in ["loopback in L", "loopback in R"].iter().zip(&rep_hp) {
        println!(
            "    {n}: envelope peak {} dBFS (p99 of blocks {})   rms {} dBFS   blocks >= -90/-80/-70/-60 dBFS: {:.0}%/{:.0}%/{:.0}%/{:.0}%   {}",
            fmt_db(c.peak_db),
            fmt_db(c.floor_peak_db),
            fmt_db(c.rms_db),
            c.above_fraction[0] * 100.0,
            c.above_fraction[1] * 100.0,
            c.above_fraction[2] * 100.0,
            c.above_fraction[3] * 100.0,
            c.character.describe()
        );
    }
    for (name, x) in [("source L", &raw[0]), ("loopback L", &raw[2])] {
        let pk = spectrum_peaks(x, info.asio_rate.max(1), 6);
        if !pk.is_empty() {
            let t: Vec<String> = pk.iter().map(|(f, d)| format!("{f:.0} Hz {d:.1} dBFS")).collect();
            println!("spectrum {name} (first {} samples): strongest peaks {}", x.len().min(32768), t.join(", "));
        }
    }
    let (corr, maxd) = channel_similarity(&raw[2], &raw[3]);
    println!("loopback L/R: correlation {corr:.4}, largest |L - R| {:.1} dBFS", router_engine::baseline::lin_to_db(maxd));
    if let Some(path) = &o.dump {
        match write_wav(path, &raw[2], &raw[3], info.asio_rate) {
            Ok(()) => println!("loopback samples written to {path}"),
            Err(e) => println!("could not write {path}: {e}"),
        }
    }
    println!();
    print_mixer_state();
    println!();
    for click in [-40.0f32, -35.0] {
        let g = band_gain_db(ImpulseShape::Burst { freq_hz: 18500.0, dur_ms: 2.0 }, info.asio_rate, hp_fc);
        for (label, h) in [("full-band 1.5 kHz click", None), ("high-frequency click (--shape burst:18500:2)", Some(&rep_hp))] {
            match plan_thresholds_checked(&rep, h, g, click, FORCED_PREAMP_DB, o.cap_threshold_db, o.lb_threshold_db, h.is_some()) {
                Ok(t) => println!(
                    "click {click:.0} dBFS, {label}: detectable; thresholds source {:.1} dBFS, loopback {:.1} dBFS (floor {:.1}, expected click at the loopback {:.0} dBFS, margin {:.1} dB)",
                    t.cap_db,
                    t.lb_db,
                    t.lb_floor_db,
                    t.lb_expected_db,
                    t.lb_expected_db - t.lb_floor_db
                ),
                Err(e) => println!("click {click:.0} dBFS, {label}: NOT measurable: {e}"),
            }
        }
    }
    Ok(Outcome::Done)
}

/// Capture -> loopback tap beyond this is not a router path (measured 21 to 30 ms on VAC, about 45 ms on the Hi-Fi Cable).
const MAX_PLAUSIBLE_CAP_TO_TAP_MS: f64 = 300.0;

/// A genuine click is seen by the source side first (stage 1, the ring-A tap) and reaches the loopback tap later (injection -> source within
/// a second, source -> tap within [`MAX_PLAUSIBLE_CAP_TO_TAP_MS`]). A loopback crossing without a stage-1 detection, or before it, or seconds late is a stray
/// event (microphone noise at about the expected level) and is not a measurement.
fn plausible(m: &Measurement) -> bool {
    match (m.stage1_ms, m.stage2_ms) {
        (Some(s1), Some(s2)) => s1 > 0.0 && s1 < 1000.0 && s2 > 0.0 && s2 < MAX_PLAUSIBLE_CAP_TO_TAP_MS && m.corrected_ms > 0.0,
        _ => false,
    }
}

fn note_for(row: &Row, t: &ImpulseTimes, lb_peak: f32) -> String {
    let mut n = Vec::new();
    if !row.capture_seen {
        n.push("no stage-1 detection (bridge tap)".to_string());
    }
    n.push(format!("lb peak {:.0} dBFS", 20.0 * lb_peak.max(1e-9).log10()));
    n.push(format!("pad {} fr ({:.1} ms)", t.padding_frames, f64::from(t.padding_frames) * 1000.0 / row.rate_hz.max(1.0)));
    n.join(", ")
}

fn wait_for_info(p: &LatencyProbe) -> Result<router_engine::ProbeInfo> {
    let t = Instant::now();
    loop {
        if let Some(i) = p.info() {
            return Ok(i);
        }
        if t.elapsed() > Duration::from_secs(10) {
            bail!("the engine did not report the loopback probe session");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn capture_summary(s: &EngineStatus) -> Vec<String> {
    vec![format!(
        "router: source \"{}\" {} Hz period {} fr ({:.1} ms) -> asio {} Hz buffer {} fr (stage 1 = bridge source tap)",
        s.source_name,
        s.cable_rate,
        s.capture_period_frames,
        f64::from(s.capture_period_us) / 1000.0,
        s.asio_rate,
        s.buffer_size
    )]
}

fn fmt_stats(label: &str, v: &[f64]) -> Option<String> {
    stats(v).map(|s| {
        format!(
            "{label:<28} n {}  min {:7.2}  median {:7.2}  mean {:7.2}  max {:7.2}  sigma {:5.2}  (ms)",
            s.n, s.min, s.median, s.mean, s.max, s.sigma
        )
    })
}

/// Everything the summary and the JSON need.
struct Summary {
    total: Vec<f64>,
    corrected: Vec<f64>,
    stage1: Vec<f64>,
    stage2: Vec<f64>,
    render_q: Vec<f64>,
    dac_lo: Vec<f64>,
    dac_hi: Vec<f64>,
    dac_mid: Vec<f64>,
}

fn collect(rows: &[Row]) -> Summary {
    Summary {
        total: rows.iter().map(|r| r.m.total_ms).collect(),
        corrected: rows.iter().map(|r| r.m.corrected_ms).collect(),
        stage1: rows.iter().filter_map(|r| r.m.stage1_ms).collect(),
        stage2: rows.iter().filter_map(|r| r.m.stage2_ms).collect(),
        render_q: rows.iter().filter_map(|r| r.m.render_queue_ms).collect(),
        dac_lo: rows.iter().map(|r| r.m.to_dac_range_ms[0]).collect(),
        dac_hi: rows.iter().map(|r| r.m.to_dac_range_ms[1]).collect(),
        dac_mid: rows.iter().map(|r| r.m.to_dac_mid_ms()).collect(),
    }
}

fn print_summary(rows: &[Row], end: &EngineStatus, probe: &LatencyProbe, o: &Options, failures: u32) {
    let sm = collect(rows);
    let info = probe.info().unwrap_or_default();
    println!("statistics over {} impulses ({} failed):", rows.len(), failures);
    for (label, v) in [
        ("(a) total inj->loopback", &sm.total),
        ("(b) corrected (device in)", &sm.corrected),
        ("inj->bridge src (stage 1)", &sm.stage1),
        ("bridge src->loopback tap", &sm.stage2),
        ("render queue (WASAPI)", &sm.render_q),
        ("to-DAC estimate (mid)", &sm.dac_mid),
    ] {
        if let Some(l) = fmt_stats(label, v) {
            println!("{l}");
        }
    }
    let pads: Vec<f64> = rows.iter().map(|r| f64::from(r.padding_frames)).collect();
    if let (Some(ps), Some(r)) = (stats(&pads), rows.first()) {
        println!(
            "render stream: frames queued ahead of the click at submit time (padding): median {:.0} fr = {:.1} ms (min {:.0}, max {:.0})",
            ps.median,
            ps.median * 1000.0 / r.rate_hz.max(1.0),
            ps.min,
            ps.max
        );
    }
    let flags = outliers(&sm.corrected, 2.5);
    let bad: Vec<String> = flags.iter().enumerate().filter(|(_, &b)| b).map(|(i, _)| format!("#{} ({:.1} ms)", i + 1, sm.corrected[i])).collect();
    if bad.is_empty() {
        println!("outliers (|x - median| > max(2.5 ms, 3 sigma_MAD)): none");
    } else {
        println!("outliers (|x - median| > max(2.5 ms, 3 sigma_MAD)): {}", bad.join(", "));
    }

    // Breakdown against the latency panel's model.
    let lat = &end.latency;
    let (n_samples, rate) = (f64::from(info.buffer_frames), f64::from(info.asio_rate.max(1)));
    let med = |v: &[f64]| stats(v).map(|s| s.median);
    let (corr, s1, s2) = (med(&sm.corrected), med(&sm.stage1), med(&sm.stage2));
    let (lo, hi, mid) = (med(&sm.dac_lo), med(&sm.dac_hi), med(&sm.dac_mid));
    println!();
    println!(
        "model (latency panel): Yamaha out {:.1} + ring {:.1} + source staleness {:.1} + render side {:.0} ({}) = {:.1} ms",
        lat.master_out_ms,
        lat.ring_ms,
        lat.capture_ms,
        lat.render_side_ms,
        lat.render_basis.label(0),
        lat.total_ms
    );
    if let (Some(lo), Some(hi), Some(mid)) = (lo, hi, mid) {
        let known = lat.master_out_ms + lat.ring_ms + lat.capture_ms;
        println!(
            "measured to-DAC equivalent (estimate): {:.1} ms (range {:.1}..{:.1}); median corrected loopback time {:.1} ms; block {:.1} ms, driver in/out {}/{} samples",
            mid,
            lo,
            hi,
            corr.unwrap_or(0.0),
            n_samples * 1000.0 / rate,
            info.input_latency,
            info.output_latency
        );
        println!(
            "=> implied render side = measured - (Yamaha out + ring + source staleness) = {:.1} ms (range {:.1}..{:.1}) versus the default estimate of {:.0} ms (measured total {:.1} ms vs model total {:.1} ms with that estimate)",
            mid - known,
            lo - known,
            hi - known,
            lat.render_estimate_ms,
            mid,
            known + lat.render_estimate_ms
        );
    }
    if let (Some(s1), Some(s2)) = (s1, s2) {
        println!(
            "stages: injection -> bridge source arrival {:.1} ms (stage 1 via the ring-A source tap; model render {:.0} + source staleness {:.1} = {:.1}); bridge src -> loopback tap {:.1} ms (model ring {:.1} + ASIO block {:.1} + part of out)",
            s1,
            lat.render_side_ms,
            lat.capture_ms,
            lat.render_side_ms + lat.capture_ms,
            s2,
            lat.ring_ms,
            n_samples * 1000.0 / rate
        );
        println!(
            "   direct render side = injection->source {:.1} - measured source staleness {:.1} = {:.1} ms; render queue in the WASAPI stream (clock estimate) {} ms",
            s1,
            lat.capture_ms,
            s1 - lat.capture_ms,
            med(&sm.render_q).map(|x| format!("{x:.1}")).unwrap_or_else(|| "?".into())
        );
    }
    println!(
        "router state during the run: ring mean {} fr (target {}), underruns {}, xruns {}, fault {:?}, source period {} fr",
        end.ring_mean_fill_frames, end.ring_target_frames, end.underruns, end.xruns, end.fault, end.capture_period_frames
    );
    println!("(a) = injection -> click at the Yamaha loopback input stream (block-end anchored); (b) = (a) minus the driver's input latency beyond the block; to-DAC = (b) + 0..(out latency - block). All driver-latency terms are estimates.");
    if o.json {
        println!("{}", to_json(rows, &sm, end, &info, o));
    }
}

/// The record `router-cli latency` stores after a successful run: medians over the clicks, the implied render side
/// (to-DAC equivalent minus Yamaha out + ring + capture staleness, as in the printed breakdown) and the engine
/// parameters of the run. `None` for fewer than [`MIN_CLICKS_TO_SAVE`] clicks or without a to-DAC figure.
fn build_entry(rows: &[Row], end: &EngineStatus, o: &Options, now_unix_s: u64) -> Option<MeasuredEntry> {
    if rows.len() < MIN_CLICKS_TO_SAVE as usize {
        return None;
    }
    let sm = collect(rows);
    let med = |v: &[f64]| stats(v).map(|s| s.median);
    let lat = &end.latency;
    let to_dac = med(&sm.dac_mid)?;
    let known = lat.master_out_ms + lat.ring_ms + lat.capture_ms;
    let render_side = (to_dac - known).max(0.0);
    if !render_side.is_finite() || !end.bridge_attached {
        return None;
    }
    Some(MeasuredEntry {
        source: eq_core::BRIDGE_SOURCE_KEY.to_string(),
        render_side_ms: render_side,
        render_queue_ms: med(&sm.render_q).unwrap_or(0.0),
        inj_to_capture_ms: med(&sm.stage1).unwrap_or(0.0),
        capture_to_tap_ms: med(&sm.stage2).unwrap_or(0.0),
        total_ms: med(&sm.corrected)?,
        to_dac_ms: to_dac,
        clicks: rows.len() as u32,
        date: eq_core::date_from_unix(now_unix_s),
        measured_unix_s: now_unix_s,
        asio_buffer: end.buffer_size,
        capture_period_frames: end.capture_period_frames,
        safety_ms: end.safety_buffer_ms,
        target_frames: end.ring_target_frames,
        render_profile: render_profile(&o.render),
    })
}

/// Stores the result unless `--no-save`, a non-default render stream (without `--save`) or too few clicks.
fn save_measurement(rows: &[Row], end: &EngineStatus, o: &Options) {
    let profile = render_profile(&o.render);
    if o.no_save {
        println!("not saved (--no-save)");
    } else if profile != DEFAULT_RENDER_PROFILE && !o.save_non_default {
        println!("not saved: non-default render stream ({profile}); add --save to keep it under that profile (the latency panel only uses the default profile)");
    } else if rows.len() < MIN_CLICKS_TO_SAVE as usize {
        println!("not saved: {} clicks, at least {MIN_CLICKS_TO_SAVE} are needed", rows.len());
    } else {
        match build_entry(rows, end, o, eq_core::now_unix_s()) {
            None => println!("not saved: the result is incomplete (no to-DAC estimate or no bridge session)"),
            Some(e) => match MeasuredLatency::record_default(e.clone()) {
                Ok(path) => println!(
                    "saved to {} (source {}, render side {:.1} ms, total {:.1} ms, source period {} fr, ASIO buffer {} fr)",
                    path.display(),
                    e.source,
                    e.render_side_ms,
                    e.to_dac_ms,
                    e.capture_period_frames,
                    e.asio_buffer
                ),
                Err(err) => println!("could not save the measurement: {err}"),
            },
        }
    }
}

fn jnum(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.4}")
    } else {
        "null".into()
    }
}

fn jopt(v: Option<f64>) -> String {
    v.map(jnum).unwrap_or_else(|| "null".into())
}

fn jarr(v: &[f64]) -> String {
    format!("[{}]", v.iter().map(|&x| jnum(x)).collect::<Vec<_>>().join(","))
}

fn jstats(v: &[f64]) -> String {
    match stats(v) {
        Some(s) => format!(
            "{{\"n\":{},\"min\":{},\"median\":{},\"mean\":{},\"max\":{},\"sigma\":{}}}",
            s.n,
            jnum(s.min),
            jnum(s.median),
            jnum(s.mean),
            jnum(s.max),
            jnum(s.sigma)
        ),
        None => "null".into(),
    }
}

fn to_json(rows: &[Row], sm: &Summary, end: &EngineStatus, info: &router_engine::ProbeInfo, o: &Options) -> String {
    let lat = &end.latency;
    let impulses: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            format!(
                "{{\"n\":{},\"stage1_ms\":{},\"total_ms\":{},\"corrected_ms\":{},\"stage2_ms\":{},\"render_queue_ms\":{},\"render_queue_source\":\"{}\",\"padding_queue_ms\":{},\"padding_frames\":{},\"raw_callback_ms\":{},\"to_dac_low_ms\":{},\"to_dac_high_ms\":{}}}",
                i + 1,
                jopt(r.m.stage1_ms),
                jnum(r.m.total_ms),
                jnum(r.m.corrected_ms),
                jopt(r.m.stage2_ms),
                jopt(r.m.render_queue_ms),
                if r.render_queue_clock { "clock" } else { "padding" },
                jnum(r.padding_queue_ms),
                r.padding_frames,
                jnum(r.m.raw_callback_ms),
                jnum(r.m.to_dac_range_ms[0]),
                jnum(r.m.to_dac_range_ms[1])
            )
        })
        .collect();
    format!(
        "{{\"click_dbfs\":{},\"count\":{},\"interval_ms\":{},\"asio_rate\":{},\"cable_rate\":{},\"buffer_frames\":{},\"input_latency_samples\":{},\"output_latency_samples\":{},\"capture_period_frames\":{},\"impulses\":[{}],\"total\":{},\"corrected\":{},\"stage1\":{},\"stage2\":{},\"render_queue\":{},\"to_dac_mid\":{},\"corrected_values\":{},\"model\":{{\"out_ms\":{},\"ring_ms\":{},\"capture_ms\":{},\"render_estimate_ms\":{},\"total_ms\":{}}}}}",
        jnum(f64::from(o.db)),
        rows.len(),
        jnum(f64::from(o.interval_ms)),
        info.asio_rate,
        info.cable_rate,
        info.buffer_frames,
        info.input_latency,
        info.output_latency,
        info.capture_period_frames,
        impulses.join(","),
        jstats(&sm.total),
        jstats(&sm.corrected),
        jstats(&sm.stage1),
        jstats(&sm.stage2),
        jstats(&sm.render_q),
        jstats(&sm.dac_mid),
        jarr(&sm.corrected),
        jnum(lat.master_out_ms),
        jnum(lat.ring_ms),
        jnum(lat.capture_ms),
        jnum(lat.render_side_ms),
        jnum(lat.total_ms)
    )
}

// ------------------------------------------------------------------------------------
// --selftest
// ------------------------------------------------------------------------------------

/// Validates the maths on a synthetic loopback with known delays (no hardware, nothing is played).
fn selftest() -> Result<()> {
    println!("latency selftest: synthetic click path with known timing, 48 kHz");
    let mut failures = 0;
    // Guards.
    for (db, ok) in [(-35.0f32, true), (-40.0, true), (-34.0, false), (-10.0, false), (0.0, false)] {
        let accepted = validate_peak_dbfs(db).is_ok();
        println!("  guard: {db:>6.1} dBFS click -> {} (limit {MAX_IMPULSE_DBFS})", if accepted { "accepted" } else { "refused" });
        if accepted != ok {
            failures += 1;
        }
    }
    for (name, ok) in [
        ("Hi-Fi Cable Input (VB-Audio Hi-Fi Cable)", true),
        ("Line 1 (Virtual Audio Cable)", true),
        ("VBMatrix In 3 (VB-Audio Matrix VAIO)", true),
        ("Headphones (Yamaha UR22C)", false),
    ] {
        let accepted = is_sink_allowed(name, &[]);
        println!("  guard: sink \"{name}\" -> {}", if accepted { "allowed" } else { "refused" });
        if accepted != ok {
            failures += 1;
        }
    }
    println!();
    println!("{:>6} {:>5} {:>6} {:>6} {:>10} {:>11} {:>11} {:>9}", "block", "in", "out", "phase", "truth (b)", "measured(b)", "error", "stage1");
    let mut worst = 0.0f64;
    for (block, in_lat, out_lat, shape) in [
        (256u32, 280i32, 375i32, ImpulseShape::default()),
        (128, 200, 290, ImpulseShape::default()),
        (512, 728, 871, ImpulseShape::Chirp { f0_hz: 800.0, f1_hz: 4000.0, dur_ms: 4.0 }),
    ] {
        for phase in [0i64, 2500, 7000, 13_000, 19_500] {
            let p = SyntheticPath { block, in_lat, out_lat, shape, ..SyntheticPath::default() };
            match synthetic_run(&p, phase) {
                Some(o) => {
                    let err = o.measurement.corrected_ms - o.truth_corrected_ms;
                    worst = worst.max(err.abs());
                    println!(
                        "{block:>6} {in_lat:>5} {out_lat:>6} {phase:>6} {:>8.3} ms {:>9.3} ms {:>8.3} ms {:>7.2} ms",
                        o.truth_corrected_ms,
                        o.measurement.corrected_ms,
                        err,
                        o.measurement.stage1_ms.unwrap_or(f64::NAN)
                    );
                    if err.abs() > 0.06 {
                        failures += 1;
                    }
                }
                None => {
                    println!("{block:>6} {in_lat:>5} {out_lat:>6} {phase:>6}  NOT DETECTED");
                    failures += 1;
                }
            }
        }
    }
    println!("worst |error| {worst:.3} ms (tolerance 0.060 ms = about 3 samples)");
    if failures > 0 {
        bail!("selftest FAILED ({failures} checks)");
    }
    println!("selftest OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn option_limits() {
        assert!(parse_options(&a(&[])).is_ok());
        assert!(parse_options(&a(&["--count", "11"])).is_err());
        assert!(parse_options(&a(&["--count", "0"])).is_err());
        assert!(parse_options(&a(&["--interval-ms", "100"])).is_err());
        assert!(parse_options(&a(&["--db", "-30"])).is_err());
        assert!(parse_options(&a(&["--db", "-35"])).is_ok());
        assert!(parse_options(&a(&["--db", "-95"])).is_err());
        assert!(parse_options(&a(&["--shape", "chirp"])).is_ok());
        assert!(parse_options(&a(&["--shape", "saw"])).is_err());
        let o = parse_options(&a(&["--allow-sink", "Foo", "--json"])).unwrap();
        assert_eq!(o.allow_sink, vec!["Foo".to_string()]);
        assert!(o.json);
    }

    #[test]
    fn baseline_and_detector_options() {
        let o = parse_options(&a(&["--baseline", "--seconds", "2.5", "--dump", "x.wav"])).unwrap();
        assert!(o.baseline && (o.seconds - 2.5).abs() < 1e-6 && o.dump.as_deref() == Some("x.wav"));
        assert!(!parse_options(&a(&[])).unwrap().baseline);
        assert!(parse_options(&a(&["--seconds", "0.1"])).is_err());
        assert!(parse_options(&a(&["--seconds", "11"])).is_err());
        assert_eq!(parse_options(&a(&["--hp-hz", "off"])).unwrap().hp_hz, Some(None));
        assert_eq!(parse_options(&a(&["--hp-hz", "16000"])).unwrap().hp_hz, Some(Some(16000.0)));
        assert!(parse_options(&a(&["--hp-hz", "100"])).is_err());
        assert!(parse_options(&a(&["--hp-hz", "x"])).is_err());
    }

    #[test]
    fn high_pass_is_automatic_only_for_high_frequency_clicks() {
        assert_eq!(detection_highpass(&parse_options(&a(&[])).unwrap()), None);
        assert_eq!(detection_highpass(&parse_options(&a(&["--shape", "chirp"])).unwrap()), None);
        assert_eq!(detection_highpass(&parse_options(&a(&["--shape", "burst:18500:2"])).unwrap()), Some(15750.0));
        assert_eq!(detection_highpass(&parse_options(&a(&["--shape", "burst:18500:2", "--hp-hz", "off"])).unwrap()), None);
        assert_eq!(detection_highpass(&parse_options(&a(&["--hp-hz", "12000"])).unwrap()), Some(12000.0));
    }

    #[test]
    fn vbmatrix_render_endpoints_are_allowed_sinks() {
        // The default sink is "VBMatrix In ..." (found by enumeration at runtime); the pure
        // allowlist rules are what matter here (no hardware).
        assert!(is_vbmatrix_render_name("VBMatrix In 3 (VB-Audio Matrix VAIO)"));
        assert!(is_vbmatrix_render_name("vbmatrix in 1"));
        assert!(!is_vbmatrix_render_name("VBMatrix Out 1"));
        assert!(is_sink_allowed("VBMatrix In 3 (VB-Audio Matrix VAIO)", &[]));
        assert!(!is_sink_allowed("Headphones (Yamaha UR22C)", &[]));
    }

    #[test]
    fn render_and_save_options() {
        let o = parse_options(&a(&[])).unwrap();
        assert_eq!(render_profile(&o.render), DEFAULT_RENDER_PROFILE);
        assert!(!o.no_save && !o.save_non_default);
        let o = parse_options(&a(&["--render-mode", "shared-lowlatency", "--render-prefill", "full", "--no-save", "--save"])).unwrap();
        assert_eq!(o.render.mode, RenderMode::SharedLowLatency);
        assert_eq!(render_profile(&o.render), "shared-lowlatency/buf-auto/fill-period/prefill-full");
        let legacy = parse_options(&a(&["--render-fill", "full", "--render-prefill", "full"])).unwrap();
        assert_eq!(render_profile(&legacy.render), "shared-default/buf-auto/fill-full/prefill-full");
        assert!(o.no_save && o.save_non_default);
        assert!(parse_options(&a(&["--render-mode", "exclusive"])).is_err());
        assert!(parse_options(&a(&["--render-buffer-frames", "3"])).is_err());
    }

    fn row(total: f64, corrected: f64, s1: f64, s2: f64, q: f64, dac: f64) -> Row {
        Row {
            m: Measurement {
                stage1_ms: Some(s1),
                render_queue_ms: Some(q),
                total_ms: total,
                corrected_ms: corrected,
                stage2_ms: Some(s2),
                to_dac_range_ms: [dac - 1.0, dac + 1.0],
                ..Measurement::default()
            },
            render_queue_clock: true,
            padding_queue_ms: 12.0,
            padding_frames: 576,
            rate_hz: 48000.0,
            capture_seen: true,
        }
    }

    #[test]
    fn stray_loopback_events_are_not_measurements() {
        let ok = Measurement { stage1_ms: Some(22.0), stage2_ms: Some(30.0), corrected_ms: 51.0, ..Measurement::default() };
        assert!(plausible(&ok));
        assert!(!plausible(&Measurement { stage1_ms: None, ..ok }), "no stage-1 detection");
        assert!(!plausible(&Measurement { stage2_ms: Some(-3.0), ..ok }), "loopback before the capture packet");
        assert!(!plausible(&Measurement { stage2_ms: Some(3523.0), ..ok }), "seconds late");
        assert!(!plausible(&Measurement { stage2_ms: Some(507.0), ..ok }), "half a second is no router path");
        assert!(!plausible(&Measurement { corrected_ms: -6.0, ..ok }));
    }

    #[test]
    fn entry_needs_three_clicks_and_uses_medians() {
        let mut end = EngineStatus { bridge_attached: true, buffer_size: 256, capture_period_frames: 288, ..EngineStatus::default() };
        end.latency = eq_core::LatencyBreakdown { master_out_ms: 7.8, ring_ms: 12.0, capture_ms: 3.0, render_side_ms: 20.0, ..Default::default() };
        let o = parse_options(&a(&[])).unwrap();
        let rows: Vec<Row> = [62.0, 63.0, 64.0].iter().map(|&c| row(c + 1.5, c, 40.0, 23.0, 34.0, c + 1.5)).collect();
        assert!(build_entry(&rows[..2], &end, &o, 1_791_590_400).is_none());
        let e = build_entry(&rows, &end, &o, 1_791_590_400).unwrap();
        assert_eq!((e.clicks, e.capture_period_frames, e.asio_buffer), (3, 288, 256));
        assert_eq!(e.source, eq_core::BRIDGE_SOURCE_KEY, "keyed on the fixed bridge source");
        assert_eq!(e.date, "2026-10-10");
        assert!((e.total_ms - 63.0).abs() < 1e-9 && (e.to_dac_ms - 64.5).abs() < 1e-9);
        // implied render side = to-DAC median - (out + ring + capture) = 64.5 - 22.8
        assert!((e.render_side_ms - (64.5 - 22.8)).abs() < 1e-9, "{}", e.render_side_ms);
        assert_eq!((e.render_queue_ms, e.inj_to_capture_ms, e.capture_to_tap_ms), (34.0, 40.0, 23.0));
        assert_eq!(e.render_profile, DEFAULT_RENDER_PROFILE);
        // no bridge session: nothing to key the record on
        end.bridge_attached = false;
        assert!(build_entry(&rows, &end, &o, 1).is_none());
    }

    #[test]
    fn raw_loopback_limit_is_relaxed_only_for_high_bursts_with_the_high_pass() {
        let hi = parse_options(&a(&["--shape", "burst:18500:2"])).unwrap();
        assert!(raw_limit_relaxed(&hi, detection_highpass(&hi)));
        assert!(!raw_limit_relaxed(&hi, None));
        let lo = parse_options(&a(&["--hp-hz", "12000"])).unwrap();
        assert!(!raw_limit_relaxed(&lo, detection_highpass(&lo)), "a 1.5 kHz burst keeps the -40 dBFS limit even with a manual high-pass");
        let chirp = parse_options(&a(&["--shape", "chirp:16000:19000:4", "--hp-hz", "15000"])).unwrap();
        assert!(!raw_limit_relaxed(&chirp, detection_highpass(&chirp)), "only bursts relax the limit");
        let b15 = parse_options(&a(&["--shape", "burst:15000:2", "--hp-hz", "13000"])).unwrap();
        assert!(raw_limit_relaxed(&b15, detection_highpass(&b15)));
        let b14 = parse_options(&a(&["--shape", "burst:14000:2", "--hp-hz", "12000"])).unwrap();
        assert!(!raw_limit_relaxed(&b14, detection_highpass(&b14)));
    }

    #[test]
    fn selftest_passes() {
        selftest().unwrap();
    }

    #[test]
    fn level_guard_trips_above_limit() {
        let quiet = EngineStatus { peak_out_db: [-60.0, -55.0], ..EngineStatus::default() };
        assert!(level_guard(&quiet, OUTPUT_ABORT_DBFS).is_none());
        let loud = EngineStatus { peak_out_db: [-60.0, -30.0], ..EngineStatus::default() };
        assert!(level_guard(&loud, OUTPUT_ABORT_DBFS).is_some());
    }
}
