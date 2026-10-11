//! capture-probe: list / info / run / tone for WASAPI capture endpoints.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wasapi_capture::loopback::{
    arrival_stats, current_engine_period, endpoint_volume, write_wav_f32, KeepAlive, KeepAliveOptions,
    LoopbackOptions, LoopbackSession, MAX_TEST_SIGNAL_DBFS,
};
use wasapi_capture::{
    find_capture_device, find_device, get_capture_info, list_endpoints, CaptureEvent, CaptureMode, CaptureOptions,
    CaptureSession, FrameSink, Flow, MonitorPolicy, RenderKeepAlive, RenderKeepAliveOptions, DEFAULT_CAPTURE_NAME,
};

#[derive(Default)]
struct Meter {
    peak_l: f32,
    peak_r: f32,
    last: Option<Instant>,
    n: u64,
    mean: f64,
    m2: f64,
    max_gap: f64,
    min_gap: f64,
    frames_in_calls: u64,
    max_frames: usize,
    /// packet arrival times (us since the sink's first packet)
    pkt_us: Vec<f64>,
    t0: Option<Instant>,
}

struct ProbeSink(Arc<Mutex<Meter>>);

impl FrameSink for ProbeSink {
    fn on_packet(&mut self, _frames: u32, _flags: wasapi_capture::PacketFlags) {
        let now = Instant::now();
        let mut m = self.0.lock().unwrap();
        let t0 = *m.t0.get_or_insert(now);
        m.pkt_us.push(now.duration_since(t0).as_secs_f64() * 1e6);
    }
    fn on_frames(&mut self, l: &[f32], r: &[f32]) {
        let now = Instant::now();
        let mut m = self.0.lock().unwrap();
        for &x in l {
            m.peak_l = m.peak_l.max(x.abs());
        }
        for &x in r {
            m.peak_r = m.peak_r.max(x.abs());
        }
        if let Some(t) = m.last {
            let dt = now.duration_since(t).as_secs_f64() * 1000.0;
            m.n += 1;
            let d = dt - m.mean;
            m.mean += d / m.n as f64;
            m.m2 += d * (dt - m.mean);
            m.max_gap = m.max_gap.max(dt);
            m.min_gap = if m.n == 1 { dt } else { m.min_gap.min(dt) };
        }
        m.last = Some(now);
        m.frames_in_calls += l.len() as u64;
        m.max_frames = m.max_frames.max(l.len());
    }
}

fn db(x: f32) -> f64 {
    if x <= 0.0 {
        -999.0
    } else {
        20.0 * (x as f64).log10()
    }
}

fn parse_mode(s: &str) -> Result<CaptureMode, String> {
    match s {
        "shared" => Ok(CaptureMode::Shared),
        "low" | "lowlatency" | "low-latency" => Ok(CaptureMode::LowLatency),
        "exclusive" => Ok(CaptureMode::Exclusive),
        "fixed" | "fixed-period" => Ok(CaptureMode::FixedPeriod),
        _ => Err(format!("unknown mode {s} (shared|low|exclusive|fixed)")),
    }
}

fn has_flag(args: &[String], f: &str) -> bool {
    args.iter().any(|a| a == f)
}

/// Capture options from the common flags: `--period-frames N` (FixedPeriod), `--allow-unsafe-period`,
/// `--force-min-period`, `--rate HZ`, `--monitor-report` / `--no-monitor`.
fn capture_options(args: &[String], mode: CaptureMode) -> Result<CaptureOptions, Box<dyn std::error::Error>> {
    let period: Option<u32> = flag_val(args, "--period-frames").map(|s| s.parse()).transpose()?;
    let mode = if period.is_some() { CaptureMode::FixedPeriod } else { mode };
    let monitor = if has_flag(args, "--no-monitor") {
        MonitorPolicy::Off
    } else if has_flag(args, "--monitor-report") {
        MonitorPolicy::ReportOnly
    } else if mode == CaptureMode::FixedPeriod {
        MonitorPolicy::StopOnTrip
    } else {
        // read-only observation of the engine-default period: count, do not stop
        MonitorPolicy::ReportOnly
    };
    Ok(CaptureOptions {
        mode,
        period_frames: period,
        preferred_rate: flag_val(args, "--rate").map(|s| s.parse()).transpose()?,
        allow_unsafe_period: has_flag(args, "--allow-unsafe-period"),
        force_min_period: has_flag(args, "--force-min-period"),
        monitor,
    })
}

/// `--keepalive`: silent minimum-period render stream on the cable's render counterpart.
fn start_keepalive(args: &[String], capture_name: &str) -> Result<Option<RenderKeepAlive>, Box<dyn std::error::Error>> {
    if !has_flag(args, "--keepalive") {
        return Ok(None);
    }
    let allow: Vec<String> = flag_val(args, "--keepalive-allow").map(|s| vec![s.clone()]).unwrap_or_default();
    let k = RenderKeepAlive::start_for_capture_endpoint(
        capture_name,
        RenderKeepAliveOptions { allow_list: allow, ..Default::default() },
    )?;
    let i = k.info();
    let rate = i.stream.format.sample_rate;
    println!(
        "keep-alive on \"{}\": stream period {}; engine period before {} -> after {}",
        i.render_name,
        fmt_period(i.stream.period_frames, rate),
        i.engine_period_before.map(|(p, r)| fmt_period(p, r)).unwrap_or_else(|| "?".into()),
        i.engine_period_after.map(|(p, r)| fmt_period(p, r)).unwrap_or_else(|| "?".into()),
    );
    Ok(Some(k))
}

fn print_arrival(a: &wasapi_capture::loopback::ArrivalStats) {
    println!(
        "gaps us (n={}): min {:.0} mean {:.0} p99 {:.0} p99.9 {:.0} max {:.0}; back-to-back {} late {} => {}",
        a.gaps, a.min_us, a.mean_us, a.p99_us, a.p999_us, a.max_us, a.back_to_back, a.late, a.pattern()
    );
}

fn flag_val<'a>(args: &'a [String], f: &str) -> Option<&'a String> {
    args.iter().position(|a| a == f).and_then(|i| args.get(i + 1))
}

fn render_opts(args: &[String], secs: f32) -> Result<wasapi_capture::tone::RenderOptions, Box<dyn std::error::Error>> {
    use wasapi_capture::signal::SignalKind;
    use wasapi_capture::tone::{ExclusiveFmt, RenderBits, RenderOptions};
    let freq: f64 = flag_val(args, "--freq").map(|s| s.parse()).transpose()?.unwrap_or(1000.0);
    let sig = flag_val(args, "--signal").map(String::as_str).unwrap_or("tone");
    let signal = SignalKind::parse(sig, freq).ok_or("unknown --signal (tone|sweep|noise|multitone)")?;
    let dbfs: f32 = flag_val(args, "--db").map(|s| s.parse()).transpose()?.unwrap_or(-30.0);
    if dbfs > -20.0 {
        return Err("refusing --db above -20 (safety)".into());
    }
    let fmt_req = |need: bool| -> Result<Option<ExclusiveFmt>, Box<dyn std::error::Error>> {
        let Some(r) = flag_val(args, "--rate") else {
            return if need { Err("--exclusive needs --rate".into()) } else { Ok(None) };
        };
        let bits = RenderBits::parse(flag_val(args, "--bits").map(String::as_str).unwrap_or("24"))
            .ok_or("--bits must be 16|24|24in32|32|float")?;
        Ok(Some(ExclusiveFmt { rate: r.parse()?, bits }))
    };
    let is_excl = args.iter().any(|a| a == "--exclusive");
    let exclusive = if is_excl { fmt_req(true)? } else { None };
    let shared_fmt = if is_excl { None } else { fmt_req(false)? };
    Ok(RenderOptions {
        name: flag_val(args, "--target").cloned().unwrap_or_else(|| "Hi-Fi Cable Input".into()),
        seconds: secs,
        signal,
        dbfs,
        exclusive,
        shared_fmt,
    })
}

/// Sample-accurate comparison of a recorded WAV against the regenerated reference signal.
#[derive(Default, Debug)]
struct CheckResult {
    tone_ratio: Option<f64>,
    /// None = alignment not attempted
    aligned: Option<bool>,
    mismatched: usize,
    slips: usize,
    corrupt_runs: usize,
    frames: usize,
}

impl CheckResult {
    fn sample_accurate(&self) -> bool {
        self.aligned == Some(true) && self.mismatched == 0 && self.slips == 0 && self.corrupt_runs == 0
    }
}

fn check_file(
    file: &str,
    opts: &wasapi_capture::tone::RenderOptions,
    render_rate: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    check_file_report(file, opts, render_rate, false).map(|_| ())
}

fn check_file_report(
    file: &str,
    opts: &wasapi_capture::tone::RenderOptions,
    render_rate: u32,
    align_tones: bool,
) -> Result<CheckResult, Box<dyn std::error::Error>> {
    let mut res = CheckResult::default();
    use wasapi_capture::signal::{check_alignment, crossing_freq, read_f32_wav, SignalGen, SignalKind};
    let bytes = std::fs::read(file)?;
    let (rate, ch) = read_f32_wav(&bytes).ok_or("not a 32-bit float WAV")?;
    let x = &ch[0];
    res.frames = x.len();
    let pk = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    println!("ANALYSIS {file}: {} frames @ {} Hz, peak {:.1} dBFS", x.len(), rate, db(pk));
    let skip = rate as usize; // ignore first second (start-up, render delay)
    if let SignalKind::Tone(f) = opts.signal {
        let mid = &x[(x.len() / 4).min(x.len())..(x.len() * 3 / 4).min(x.len())];
        if let Some(m) = crossing_freq(mid, f64::from(rate)) {
            println!("  tone measured {:.2} Hz vs expected {:.2} Hz => ratio {:.4}", m, f, m / f);
            res.tone_ratio = Some(m / f);
        }
    }
    // Only the steady part (the renderer fades the last 20 ms).
    if matches!(opts.signal, SignalKind::Tone(_)) && !align_tones {
        println!("  (pure tone is periodic: sample-accurate alignment is ambiguous; use router-cli analyze for THD+N / glitches)");
        return Ok(res);
    }
    let n = ((opts.seconds.max(1.0) * render_rate as f32) as usize).saturating_sub(render_rate as usize / 50 + 1);
    let n = n.min(60 * render_rate as usize);
    let reference = SignalGen::new(opts.signal, render_rate, opts.dbfs).take(n);
    let amp = SignalGen::new(opts.signal, render_rate, opts.dbfs).amp();
    let rep = check_alignment(x, &reference, skip, amp * 0.02 + 1e-4);
    res.aligned = Some(rep.aligned);
    res.mismatched = rep.mismatched;
    res.slips = rep.slips.len();
    res.corrupt_runs = rep.corrupt_runs;
    if !rep.aligned {
        println!("  reference NOT found in the recording (rate/pitch/format mismatch or heavy damage)");
    } else {
        println!(
            "  aligned to reference@{} Hz (start {}), gain {:.4}; compared {} samples: mismatched {}, slips {} (net {} samples), corrupt runs {} => {}",
            render_rate, rep.start_offset, rep.gain, rep.compared, rep.mismatched, rep.slips.len(), rep.dropped_total,
            rep.corrupt_runs, if rep.clean() { "SAMPLE-ACCURATE" } else { "DAMAGED" }
        );
        for (i, d) in rep.slips.iter().take(8) {
            println!("    slip at rec sample {i} ({:.3}s): {d:+}", *i as f64 / f64::from(rate));
        }
    }
    Ok(res)
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("list") => {
            for (flow, label) in [(Flow::Capture, "capture"), (Flow::Render, "render")] {
                println!("{label} endpoints:");
                for e in list_endpoints(flow)? {
                    println!("  {}{}  [{}]", e.name, if e.is_default { "  (default)" } else { "" }, e.id);
                }
            }
        }
        Some("info") if args.iter().any(|a| a == "--render") => {
            let name = args.iter().skip(1).find(|a| !a.starts_with("--")).map(String::as_str);
            render_info(name)?;
        }
        Some("render-list") => {
            for e in list_endpoints(Flow::Render)? {
                println!("{}{}  [{}]", e.name, if e.is_default { "  (default)" } else { "" }, e.id);
            }
        }
        Some("proc-loopback") => {
            // capture-probe proc-loopback <pid|name> <secs> [--rate HZ]
            let spec = args.get(1).ok_or("usage: proc-loopback <pid|name> <secs> [--rate HZ]")?;
            let secs: f32 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(10.0);
            let rate: u32 = flag_val(&args, "--rate").map(|s| s.parse()).transpose()?.unwrap_or(48000);
            let pid = wasapi_capture::process_loopback::resolve_pid(spec)?;
            println!("process loopback of pid {pid} for {secs}s at {rate} Hz float32 stereo ...");
            let r = wasapi_capture::process_loopback::run(pid, secs, rate, args.iter().any(|a| a == "--exclude"))?;
            println!("packets {}  frames {}  silent {}  discontinuities {}  first packet after {:.1} ms  peak {:.1} dBFS", r.packets, r.frames, r.silent_packets, r.discontinuities, r.first_packet_ms, r.peak_dbfs);
            println!("packet sizes (frames x count): {:?}", r.sizes);
            if let Some(a) = &r.arrival {
                println!("inter-arrival us: min {:.0} mean {:.0} p50 {:.0} p99 {:.0} max {:.0}; back-to-back {} late {} of {} gaps => {}", a.min_us, a.mean_us, a.p50_us, a.p99_us, a.max_us, a.back_to_back, a.late, a.gaps, a.pattern());
            }
            for n in &r.notes {
                println!("note: {n}");
            }
        }
        Some("ks-list") => {
            // capture-probe ks-list [capture|render|audio]  (read-only; opens filters only, never pins)
            use wasapi_capture::ks;
            let cat = match args.get(1).map(String::as_str) {
                Some("render") => ks::KSCATEGORY_RENDER,
                Some("audio") => ks::KSCATEGORY_AUDIO,
                _ => ks::KSCATEGORY_CAPTURE,
            };
            for f in ks::list_filters(&cat)? {
                println!("filter: {}
  path: {}", f.name, f.path);
                if let Some(e) = &f.open_error {
                    println!("  {e}");
                }
                for p in &f.pins {
                    println!("  pin {}: {} / {} instances cur {:?} max {:?}", p.id, p.dataflow, p.communication, p.current_instances, p.possible_instances);
                    for r in &p.ranges {
                        println!("      range: {r}");
                    }
                    println!("      rtaudio: {}", p.rtaudio);
                }
            }
        }
        Some("loopback-run") => loopback_run(&args, None)?,
        Some("loopback-record") => {
            let out = args
                .get(3)
                .filter(|a| !a.starts_with("--"))
                .ok_or("usage: loopback-record <name> <secs> OUT.wav [flags]")?;
            loopback_run(&args, Some(out.clone()))?
        }
        Some("info") => {
            let ep = find_capture_device(args.get(1).map(String::as_str))?;
            let i = get_capture_info(&ep.id)?;
            println!("device : {}\nid     : {}", i.name, i.id);
            println!(
                "mix    : {} Hz, {} ch, {} ({} bit container, {} valid), mask 0x{:X}",
                i.mix.sample_rate,
                i.mix.channels,
                i.mix.format.name(),
                i.mix.bits_per_sample,
                i.mix.valid_bits,
                i.mix.channel_mask
            );
            println!(
                "GetDevicePeriod: default {:.3} ms, min {:.3} ms",
                i.default_period_ms(),
                i.min_period_ms()
            );
            match i.engine {
                Some(e) => println!(
                    "IAudioClient3 engine period (frames/ms): default {}/{:.3}  fundamental {}/{:.3}  min {}/{:.3}  max {}/{:.3}",
                    e.default_frames, i.frames_to_ms(e.default_frames),
                    e.fundamental_frames, i.frames_to_ms(e.fundamental_frames),
                    e.min_frames, i.frames_to_ms(e.min_frames),
                    e.max_frames, i.frames_to_ms(e.max_frames)
                ),
                None => println!("IAudioClient3 engine period: not available"),
            }
            println!("exclusive mode accepts mix format: {}", i.exclusive_supported);
        }
        Some("run") => {
            let name = args.get(1).map(String::as_str).unwrap_or(DEFAULT_CAPTURE_NAME);
            let secs: f32 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(5.0);
            let mode = parse_mode(args.get(3).filter(|a| !a.starts_with("--")).map(String::as_str).unwrap_or("shared"))?;
            let copts = capture_options(&args, mode)?;
            let mode = copts.mode;
            let ep = find_capture_device(Some(name))?;
            let _ka = start_keepalive(&args, &ep.name)?;
            let meter = Arc::new(Mutex::new(Meter { pkt_us: Vec::with_capacity(60_000), ..Default::default() }));
            let t0 = Instant::now();
            let mut s = CaptureSession::start_with_options(&ep.id, copts, Box::new(ProbeSink(meter.clone())))?;
            println!("started in {:.1} ms: {} ", t0.elapsed().as_secs_f64() * 1000.0, ep.name);
            let fmt = s.format();
            println!(
                "requested {:?}, in use {:?}; {} Hz {} ch {}; period {} frames = {:.3} ms",
                mode,
                s.mode_in_use(),
                fmt.sample_rate,
                fmt.channels,
                fmt.format.name(),
                s.period_frames(),
                s.period_frames() as f64 * 1000.0 / fmt.sample_rate as f64
            );
            println!(
                "granted period {} frames; engine period after init {} frames; monitor policy {}",
                s.period_frames(),
                s.stats().engine_period_frames.load(std::sync::atomic::Ordering::Relaxed),
                ["off", "report-only", "stop-on-trip"][s.stats().monitor_policy.load(std::sync::atomic::Ordering::Relaxed).min(2) as usize]
            );
            let end = Instant::now() + Duration::from_secs_f32(secs);
            let mut last = (Instant::now(), 0u64);
            let mut lost = false;
            while Instant::now() < end && !lost {
                std::thread::sleep(Duration::from_millis(1000).min(end - Instant::now()));
                let st = s.stats().snapshot();
                let now = Instant::now();
                let rate = (st.frames_captured - last.1) as f64 / now.duration_since(last.0).as_secs_f64();
                last = (now, st.frames_captured);
                let (pl, pr) = {
                    let m = meter.lock().unwrap();
                    (m.peak_l, m.peak_r)
                };
                println!(
                    "  frames {:>9}  {:>8.0} fr/s  pk L {:>7.1} dBFS R {:>7.1} dBFS  glitches {}  silent pkts {}/{}  timeouts {}",
                    st.frames_captured, rate, db(pl), db(pr), st.discontinuities, st.silent_packets, st.packets, st.wait_timeouts
                );
                while let Ok(ev) = s.events().try_recv() {
                    match ev {
                        CaptureEvent::DeviceLost | CaptureEvent::Error(_) | CaptureEvent::Corruption(_) => {
                            println!("  event: {ev:?}");
                            lost |= !matches!(ev, CaptureEvent::Corruption(_)) || copts.monitor == MonitorPolicy::StopOnTrip;
                        }
                        other => println!("  event: {other:?}"),
                    }
                }
                if !s.is_running() {
                    lost = true;
                }
            }
            s.stop();
            let st = s.stats().snapshot();
            let m = meter.lock().unwrap();
            let sd = if m.n > 1 { (m.m2 / (m.n - 1) as f64).sqrt() } else { 0.0 };
            let elapsed = t0.elapsed().as_secs_f64();
            println!("--- summary ---");
            println!(
                "frames {} ({:.0} fr/s over {:.2}s), packets {}, discontinuities {}, silent packets {}, ts errors {}, wait timeouts {}",
                st.frames_captured, st.frames_captured as f64 / elapsed, elapsed, st.packets,
                st.discontinuities, st.silent_packets, st.timestamp_errors, st.wait_timeouts
            );
            println!("peak L {:.1} dBFS, R {:.1} dBFS", db(m.peak_l), db(m.peak_r));
            println!(
                "callback interval: mean {:.3} ms, jitter(sd) {:.3} ms, min {:.3}, max {:.3}; mean frames/callback {:.1}, max {}",
                m.mean, sd, m.min_gap, m.max_gap,
                m.frames_in_calls as f64 / (m.n + 1) as f64, m.max_frames
            );
            let pms = st.period_frames as f64 * 1000.0 / st.sample_rate.max(1) as f64;
            let nominal_us = st.period_frames as f64 * 1e6 / st.sample_rate.max(1) as f64;
            if let Some(a) = wasapi_capture::loopback::arrival_stats(&m.pkt_us, nominal_us) {
                print_arrival(&a);
            }
            {
                use std::sync::atomic::Ordering::Relaxed;
                let c = s.stats();
                println!(
                    "monitor: trips {} max-ahead {} frames repeated-blocks {} (discontinuity flags {} are NOT used as evidence)",
                    c.corruption_trips.load(Relaxed), c.max_ahead_frames.load(Relaxed), c.repeated_blocks.load(Relaxed), st.discontinuities
                );
            }
            println!(
                "effective capture latency: period {:.3} ms (+ up to one period of wait = ~{:.3}..{:.3} ms)",
                pms, pms, pms * 2.0
            );
        }
        Some("format") => {
            // Read-only: current Windows device format of the cable endpoints (or of `name`).
            let name = args.get(1).map(String::as_str).unwrap_or("Hi-Fi Cable");
            let rows = wasapi_capture::read_cable_rates(name);
            if rows.is_empty() {
                println!("no endpoints matching \"{name}\"");
            }
            for (flow, ename, id, r) in rows {
                println!("{:?} \"{}\" [{}]", flow, ename, id);
                match wasapi_capture::get_endpoint_device_format(&id) {
                    Ok(f) => println!("  device format: {} Hz, {} ch, {} bit container", f.sample_rate, f.channels, f.bits_per_sample),
                    Err(e) => println!("  device format: error: {e}"),
                }
                if let Ok(rate) = r {
                    println!("  rate: {rate} Hz");
                }
                if flow == Flow::Capture {
                    if let Ok(i) = get_capture_info(&id) {
                        println!("  WASAPI mix format: {} Hz {}", i.mix.sample_rate, i.mix.format.name());
                    }
                }
            }
        }
        Some("set-rate") => {
            // CHANGES the system's cable format (both Input and Output endpoints).
            let name = args.get(1).ok_or("usage: capture-probe set-rate <name> <rate>")?;
            let rate: u32 = args.get(2).ok_or("usage: capture-probe set-rate <name> <rate>")?.parse()?;
            let base = wasapi_capture::cable_base_name(name);
            println!("setting all endpoints matching \"{base}\" to {rate} Hz ...");
            let mut failed = false;
            for r in wasapi_capture::match_cable_rates(&[base.as_str()], rate) {
                match &r.result {
                    Ok(()) => println!("  {:?} \"{}\": {:?} -> {:?} Hz ok", r.flow, r.endpoint_name, r.before, r.after),
                    Err(e) => {
                        failed = true;
                        println!("  {:?} \"{}\": {:?} -> {:?} Hz FAILED: {e}", r.flow, r.endpoint_name, r.before, r.after)
                    }
                }
            }
            if failed {
                return Err("one or more endpoints could not be changed".into());
            }
        }
        Some("record") => {
            // capture-probe record FILE [secs] [shared|low|exclusive] [--rate HZ] [--device NAME]
            let file = args.get(1).ok_or("record needs an output FILE")?;
            let secs: f32 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(10.0);
            let mode = parse_mode(args.get(3).filter(|a| !a.starts_with("--")).map(String::as_str).unwrap_or("shared"))?;
            let flag = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1));
            let copts = capture_options(&args, mode)?;
            let dev = flag("--device").map(String::as_str).or(Some(DEFAULT_CAPTURE_NAME));
            let _ka = start_keepalive(&args, dev.unwrap_or(DEFAULT_CAPTURE_NAME))?;
            println!("{}", wasapi_capture::record::record_with_options(std::path::Path::new(file), dev, secs, copts)?.summary);
        }
        Some("tone") => {
            // capture-probe tone [secs] [--exclusive --rate HZ --bits 16|24|24in32|32|float]
            //                   [--signal tone|sweep|noise|multitone] [--freq HZ] [--db -30]
            let secs: f32 = args.get(1).filter(|a| !a.starts_with("--")).map(|s| s.parse()).transpose()?.unwrap_or(5.0);
            let opts = render_opts(&args, secs)?;
            let _ = find_device(Flow::Render, &opts.name)?;
            println!("rendering {:?} {secs}s at {} dBFS into {} ...", opts.signal, opts.dbfs, opts.name);
            println!("{}", wasapi_capture::tone::play_signal(&opts)?);
        }
        Some("impulses") => {
            // capture-probe impulses [--target RENDER] [--device CAPTURE] [--count 3] [--interval-ms 1000] [--db -40]
            // Quiet clicks into a virtual cable's render endpoint with QPC timestamps; with --device the cable's
            // capture endpoint is recorded too and the injection -> capture delivery time is printed (no ASIO needed).
            use std::sync::atomic::{AtomicI64, Ordering};
            use std::sync::Arc;
            struct ClickSink(Arc<AtomicI64>, f32);
            impl wasapi_capture::FrameSink for ClickSink {
                fn on_frames(&mut self, l: &[f32], r: &[f32]) {
                    if self.0.load(Ordering::Relaxed) == 0 && l.iter().chain(r.iter()).any(|v| v.abs() >= self.1) {
                        self.0.store(wasapi_capture::qpc_ticks(), Ordering::Relaxed);
                    }
                }
            }
            let target = flag_val(&args, "--target").map(String::as_str).unwrap_or("Hi-Fi Cable Input");
            let count: u32 = flag_val(&args, "--count").map(|s| s.parse()).transpose()?.unwrap_or(3);
            let interval: f32 = flag_val(&args, "--interval-ms").map(|s| s.parse()).transpose()?.unwrap_or(1000.0);
            let db: f32 = flag_val(&args, "--db").map(|s| s.parse()).transpose()?.unwrap_or(wasapi_capture::DEFAULT_IMPULSE_DBFS);
            let mut ro = wasapi_capture::ImpulseRenderOptions::new(target);
            ro.peak_dbfs = db;
            ro.apply_render_args(&|k| flag_val(&args, k).cloned())?;
            // Optional render keep-alive on the paired cable endpoint (needs --device to name the cable).
            let _ka = match flag_val(&args, "--device") {
                Some(d) => start_keepalive(&args, d)?,
                None => None,
            };
            let hz = wasapi_capture::qpc_hz();
            let cap_thr = 10f32.powf((db - 20.0) / 20.0);
            let seen = Arc::new(AtomicI64::new(0));
            let cap = match flag_val(&args, "--device") {
                Some(d) => {
                    let ep = find_device(Flow::Capture, d)?;
                    let cmode = parse_mode(flag_val(&args, "--cmode").map(String::as_str).unwrap_or("shared"))?;
                    let copts = capture_options(&args, cmode)?;
                    let c = wasapi_capture::CaptureSession::start_with_options(&ep.id, copts, Box::new(ClickSink(seen.clone(), cap_thr)))?;
                    println!("capturing \"{}\" ({} Hz, period {} fr)", ep.name, c.sample_rate(), c.period_frames());
                    Some(c)
                }
                None => None,
            };
            let mut r = wasapi_capture::ImpulseRenderer::start(&ro)?;
            let i = r.info().clone();
            println!(
                "render \"{}\" {} Hz {} ch {}, mode {}, buffer {} fr, period {} fr, stream latency {:.2} ms, click {} frames at {} dBFS",
                i.endpoint, i.format.sample_rate, i.format.channels, i.format.format.name(), i.mode.name(), i.buffer_frames, i.period_frames, i.stream_latency_ms, i.click_frames, i.peak_dbfs
            );
            println!(
                "render: fill {}, prefill {} fr, endpoint min/default engine period {}/{} fr, engine period before {} -> after {}",
                i.fill.describe(),
                i.prefill_frames,
                i.min_period_frames.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
                i.default_period_frames.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
                i.engine_before.map(|(p, r)| fmt_period(p, r)).unwrap_or_else(|| "?".into()),
                i.engine_after.map(|(p, r)| fmt_period(p, r)).unwrap_or_else(|| "?".into()),
            );
            std::thread::sleep(std::time::Duration::from_millis(800));
            for k in 0..count {
                seen.store(0, Ordering::Relaxed);
                let t = r.fire()?;
                std::thread::sleep(std::time::Duration::from_millis(300));
                let (q, which) = t.render_delay_ms(hz);
                let cap_ms = match seen.load(Ordering::Relaxed) {
                    0 => "-".to_string(),
                    c => format!("{:.2} ms", (c - t.submit_ticks) as f64 * 1000.0 / hz as f64),
                };
                println!(
                    "#{}: padding {} fr, render queue {q:.2} ms ({which}; padding model {:.2} ms), inject -> capture delivery {cap_ms}",
                    k + 1,
                    t.padding_frames,
                    t.padding_delay_ms(hz)
                );
                if k + 1 < count {
                    std::thread::sleep(std::time::Duration::from_secs_f32((interval / 1000.0 - 0.3).max(0.0)));
                }
            }
            r.stop();
            drop(cap);
        }
        Some("exp") => {
            // One experiment: render a signal into a cable's render endpoint while recording its capture endpoint.
            // capture-probe exp OUT.wav [render options as for `tone`; --target NAME] [--device CAPTURE_NAME]
            //   [--cmode shared|low|exclusive|fixed] [--crate HZ] [--period-frames N] [--keepalive]
            //   [--allow-unsafe-period] [--no-monitor|--monitor-report]
            let out = args.get(1).ok_or("exp needs OUT.wav")?.clone();
            let secs: f32 = flag_val(&args, "--secs").map(|s| s.parse()).transpose()?.unwrap_or(8.0);
            let cmode = parse_mode(flag_val(&args, "--cmode").map(String::as_str).unwrap_or("shared"))?;
            let mut copts = capture_options(&args, cmode)?;
            if let Some(r) = flag_val(&args, "--crate") {
                copts.preferred_rate = Some(r.parse()?);
            }
            let opts = render_opts(&args, secs)?;
            let dev = flag_val(&args, "--device").cloned().unwrap_or_else(|| DEFAULT_CAPTURE_NAME.to_string());
            let _ka = start_keepalive(&args, &dev)?;
            let o = run_exp(&out, &opts, &dev, copts, false)?;
            if let Some(r) = &o.report {
                println!("monitor: {}", r.summary.lines().last().unwrap_or(""));
            }
        }
        Some("vac-check") => vac_check(&args)?,
        Some("wdm-baseline") => wdm_baseline(&args[1..])?,
        Some("check") => {
            // capture-probe check FILE.wav [--rrate HZ (render rate of the reference)] [--signal ..] [--freq ..] [--db ..]
            let file = args.get(1).ok_or("check needs FILE.wav")?;
            let chk_secs: f32 = flag_val(&args, "--secs").map(|s| s.parse()).transpose()?.unwrap_or(0.0);
            let opts = render_opts(&args, chk_secs)?;
            let rr: u32 = flag_val(&args, "--rrate").map(|s| s.parse()).transpose()?.unwrap_or(48000);
            check_file(file, &opts, rr)?;
        }
        _ => {
            eprintln!("usage: capture-probe list | proc-loopback <pid|name> <secs> [--rate HZ] [--exclude] | ks-list [capture|render|audio] | info --render [name] | render-list | loopback-run <name> <secs> [--keepalive] [--tone-db -50] [--signal tone|noise|multitone|sweep] [--low|--lowkeep|--lowloop] [--noevent] [--poll [--poll-ms N]] | loopback-record <name> <secs> OUT.wav [same flags; default keepalive + tone -50] | info [name] | run [name] [seconds] [shared|low|exclusive|fixed] [--rate HZ] [--period-frames N] [--keepalive] [--allow-unsafe-period] [--no-monitor|--monitor-report] | vac-check [--device NAME] [--secs 12] [--period-frames 288 (default; 240 is known-unreliable)] [--repeats N] [--keepalive] | format [name] | set-rate <name> <hz> | record FILE [seconds] [shared|low|exclusive] [--rate HZ] | impulses [--target RENDER] [--device CAPTURE] [--cmode shared|fixed --period-frames N] [--keepalive] [--count N] [--interval-ms M] [--db -40] [--render-mode shared-default|shared-lowlatency|shared-min-buffer] [--render-buffer-frames N] [--render-fill full|period|N] [--render-prefill full|period|N] | tone [seconds] [[--exclusive] --rate HZ --bits 16|24|24in32|32|float] [--signal tone|sweep|noise|multitone] [--freq HZ] [--db -30] | exp OUT.wav [tone options] [--target RENDER] [--device CAPTURE] [--cmode shared|low|exclusive|fixed] [--crate HZ] [--period-frames N] [--keepalive] [--allow-unsafe-period] | check FILE.wav [--rrate HZ] [--signal ..] | wdm-baseline --allow-sink \"Music (Steinberg UR22C)\" [--confirm-knob-down] [--target RENDER] [--device CAPTURE] [--count N<=10] [--interval-ms M>=1200] [--db -40 (<= -35)] [--router-total MS] | wdm-baseline --selftest");
        }
    }
    Ok(())
}

struct ExpOutcome {
    render_ok: bool,
    report: Option<wasapi_capture::record::RecordReport>,
    check: Option<CheckResult>,
}

/// Render `opts` (into `opts.name`) while recording the capture endpoint `dev`; prints everything.
fn run_exp(
    out: &str,
    opts: &wasapi_capture::tone::RenderOptions,
    dev: &str,
    copts: CaptureOptions,
    align_tones: bool,
) -> Result<ExpOutcome, Box<dyn std::error::Error>> {
    let o2 = opts.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        wasapi_capture::tone::play_signal(&o2)
    });
    let t0 = Instant::now();
    let rec = wasapi_capture::record::record_with_options(std::path::Path::new(out), Some(dev), opts.seconds + 2.0, copts);
    let wall = t0.elapsed().as_secs_f64();
    let ren = h.join().map_err(|_| "render thread panicked")?;
    match &ren {
        Ok(r) => println!("RENDER  {r}"),
        Err(e) => println!("RENDER  FAILED: {e}"),
    }
    let report = match rec {
        Ok(r) => {
            println!("CAPTURE {}  (wall {:.2}s)", r.summary.replace('\n', "\n        "), wall);
            Some(r)
        }
        Err(e) => {
            println!("CAPTURE FAILED: {e}");
            None
        }
    };
    let mut check = None;
    if let (Ok(r), Some(_)) = (&ren, &report) {
        check = Some(check_file_report(out, opts, r.format.sample_rate, align_tones)?);
    }
    Ok(ExpOutcome { render_ok: ren.is_ok(), report, check })
}

/// `vac-check`: PASS/FAIL matrix on the VAC lite "Line 1" endpoints (default 288 frames; 240 is known-unreliable).
fn vac_check(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use wasapi_capture::signal::SignalKind;
    use wasapi_capture::tone::RenderOptions;
    let cap_name = flag_val(args, "--device").cloned().unwrap_or_else(|| "Line 1 (Virtual Audio Cable)".into());
    let secs: f32 = flag_val(args, "--secs").map(|s| s.parse()).transpose()?.unwrap_or(12.0);
    let frames: u32 = flag_val(args, "--period-frames").map(|s| s.parse()).transpose()?.unwrap_or(288);
    let dbfs: f32 = flag_val(args, "--db").map(|s| s.parse()).transpose()?.unwrap_or(-50.0);
    if dbfs > MAX_TEST_SIGNAL_DBFS {
        return Err(format!("refusing --db above {MAX_TEST_SIGNAL_DBFS}").into());
    }
    let repeats: usize = flag_val(args, "--repeats").map(|s| s.parse()).transpose()?.unwrap_or(1);
    let cap = list_endpoints(Flow::Capture)?
        .into_iter()
        .find(|e| e.name.to_lowercase().contains(&cap_name.to_lowercase()));
    let Some(cap) = cap else {
        println!("SKIP vac-check: no ACTIVE capture endpoint matching \"{cap_name}\" (VAC lite Line 1 capture side is inactive or absent); nothing was rendered.");
        return Ok(());
    };
    let Some(ren) = list_endpoints(Flow::Render)?
        .into_iter()
        .find(|e| wasapi_capture::pair_render_name(&cap.name).is_some_and(|p| e.name.eq_ignore_ascii_case(&p)))
    else {
        println!("SKIP vac-check: no active render endpoint paired with \"{}\"; nothing was rendered.", cap.name);
        return Ok(());
    };
    println!("vac-check: capture \"{}\" <- render \"{}\" (only this render endpoint is used), {frames} frames, {secs}s, {dbfs} dBFS, x{repeats}", cap.name, ren.name);
    let _ka = start_keepalive(args, &cap.name)?;
    let mut all_pass = true;
    let dir = std::env::temp_dir().join("capture_probe_vac_check");
    std::fs::create_dir_all(&dir)?;
    for rep in 0..repeats {
        // 997 Hz: not commensurate with the packet length, so repeated-block detection is meaningful
        for (label, signal) in [("tone 997 Hz", SignalKind::Tone(997.0)), ("noise", SignalKind::Noise)] {
            let out = dir.join(format!("vac_check_{}_{}.wav", label.split(' ').next().unwrap_or("x"), rep));
            let opts = RenderOptions { name: ren.name.clone(), seconds: secs, signal, dbfs, exclusive: None, shared_fmt: None };
            let mut copts = capture_options(args, CaptureMode::FixedPeriod)?;
            copts.mode = CaptureMode::FixedPeriod;
            copts.period_frames = Some(frames);
            println!("--- run {}/{}: {label} ---", rep + 1, repeats);
            let o = run_exp(&out.to_string_lossy(), &opts, &cap.name, copts, true)?;
            let mut why: Vec<String> = Vec::new();
            if !o.render_ok {
                why.push("render failed".into());
            }
            match &o.report {
                None => why.push("capture failed".into()),
                Some(r) => {
                    if r.mode_in_use != CaptureMode::FixedPeriod {
                        why.push(format!("fell back to {:?} ({})", r.mode_in_use, r.fallbacks.join("; ")));
                    }
                    if r.corruption_trips > 0 || !r.corruption_reasons.is_empty() {
                        why.push(format!("monitor tripped ({} x)", r.corruption_trips));
                    }
                    if r.ended_early {
                        why.push("capture ended early".into());
                    }
                }
            }
            match &o.check {
                None => why.push("no analysis".into()),
                Some(c) => {
                    if matches!(signal, SignalKind::Tone(_)) {
                        match c.tone_ratio {
                            Some(x) if (x - 1.0).abs() < 0.0005 => {}
                            other => why.push(format!("tone ratio {other:?}")),
                        }
                    }
                    if !c.sample_accurate() {
                        why.push(format!(
                            "not sample-accurate (aligned {:?}, mismatched {}, slips {}, corrupt runs {})",
                            c.aligned, c.mismatched, c.slips, c.corrupt_runs
                        ));
                    }
                }
            }
            if why.is_empty() {
                println!("RESULT {label}: PASS");
            } else {
                all_pass = false;
                println!("RESULT {label}: FAIL: {}", why.join("; "));
            }
        }
    }
    println!("VAC-CHECK {}", if all_pass { "PASS" } else { "FAIL" });
    Ok(())
}

/// `wdm-baseline`: the no-router latency baseline. A quiet 18.5 kHz burst (the one `router-cli latency` uses) is rendered
/// by a default legacy shared-mode client straight into a Windows output endpoint and detected on the Yamaha WDM
/// "Streaming" capture endpoint (device-side loopback). All limits are enforced by `wasapi_capture::wdm_baseline`.
fn wdm_baseline(raw: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use wasapi_capture::loopback::probe_stream_latency_ms;
    use wasapi_capture::wdm_baseline as wb;
    let a = wb::parse_args(raw)?;
    if a.selftest {
        for l in wb::selftest()? {
            println!("selftest: {l}");
        }
        println!("SELFTEST PASS (synthetic data only; no audio device was opened)");
        return Ok(());
    }
    // 1. read-only look at both endpoints; the guard is checked again on the resolved name
    let sink = find_device(Flow::Render, &a.sink)?;
    if !wasapi_capture::is_sink_allowed(&sink.name, &a.allow) {
        return Err(format!("render endpoint \"{}\" is not covered by --allow-sink {:?}; refusing", sink.name, a.allow).into());
    }
    let cap = find_device(Flow::Capture, &a.capture)?;
    println!("wdm-baseline: render \"{}\" -> capture \"{}\" (router must be stopped; Yamaha ASIO must not be open)", sink.name, cap.name);
    // 2. volume / mute gate (read-only; never changes anything)
    let vol = endpoint_volume(&sink.id).ok();
    println!("==============================================================================");
    match vol {
        Some((v, m)) => println!(" WINDOWS ENDPOINT VOLUME of \"{}\" (read-only): scalar {v:.3}, mute {m}", sink.name),
        None => println!(" WINDOWS ENDPOINT VOLUME of \"{}\": COULD NOT BE READ", sink.name),
    }
    println!(" Click level {:.0} dBFS; the physical Yamaha headphone knob should be LOWERED before this test.", a.db);
    println!("==============================================================================");
    if let wb::KnobGate::Refuse(msg) = wb::knob_gate(vol, a.confirm_knob_down) {
        eprintln!("REFUSED: {msg}");
        std::process::exit(2);
    }
    println!(
        "run plan: {} click(s) every {:.0} ms at {:.0} dBFS, total about {:.1} s (limit {:.0} s)",
        a.count,
        a.interval_ms,
        a.db,
        wb::total_run_seconds(a.count, a.interval_ms),
        wb::MAX_TOTAL_S
    );
    // 3. endpoint facts
    let cinfo = get_capture_info(&cap.id)?;
    if let Ok(i) = get_capture_info(&sink.id) {
        println!(
            "Music endpoint: {} Hz {} ch {}; GetDevicePeriod default {:.3} ms min {:.3} ms; engine period {}",
            i.mix.sample_rate,
            i.mix.channels,
            i.mix.format.name(),
            i.default_period_ms(),
            i.min_period_ms(),
            current_engine_period(&sink.id).map(|(p, r)| fmt_period(p, r)).unwrap_or_else(|| "?".into())
        );
    }
    let cap_latency = probe_stream_latency_ms(&cap.id);
    println!(
        "Streaming capture endpoint: {} Hz {}; GetDevicePeriod default {:.3} ms min {:.3} ms; engine period {}; GetStreamLatency {}",
        cinfo.mix.sample_rate,
        cinfo.mix.format.name(),
        cinfo.default_period_ms(),
        cinfo.min_period_ms(),
        current_engine_period(&cap.id).map(|(p, r)| fmt_period(p, r)).unwrap_or_else(|| "?".into()),
        cap_latency.map(|v| format!("{v:.3} ms")).unwrap_or_else(|| "unavailable".into())
    );
    // 4. capture (shared, default period) with a QPC per packet; nothing is rendered until the baseline is judged
    let run_start = Instant::now();
    let rec = Arc::new(Mutex::new(wb::Recording::with_capacity(cinfo.mix.sample_rate, wb::MAX_TOTAL_S + 2.0)));
    let copts = CaptureOptions { monitor: MonitorPolicy::ReportOnly, ..CaptureOptions::default() };
    let mut session = CaptureSession::start_with_options(&cap.id, copts, Box::new(wb::RecordingSink::new(rec.clone())))?;
    let rate = session.sample_rate();
    rec.lock().unwrap().rate = rate;
    let cap_period_ms = session.period_frames() as f64 * 1000.0 / f64::from(rate.max(1));
    println!(
        "capturing \"{}\": {} Hz, mode {}, period {} frames = {:.3} ms",
        cap.name,
        rate,
        session.mode_in_use().name(),
        session.period_frames(),
        cap_period_ms
    );
    std::thread::sleep(Duration::from_secs_f64(wb::PRE_BASELINE_WAIT_S));
    let snap = rec.lock().unwrap().clone();
    let env0 = wb::envelope(&snap, wb::HP_HZ);
    let plan = match wb::plan_baseline(&snap, &env0, a.db) {
        Ok(p) => p,
        Err(e) => {
            session.stop();
            return Err(format!("{e} (nothing was rendered)").into());
        }
    };
    println!(
        "baseline ({:.1} s after a {:.1} s warm-up): high-passed ({:.0} Hz) envelope floor {:.1} dBFS, raw deviation {:.1} dBFS; expected click envelope {:.1} dBFS; threshold {:.1} dBFS (floor + {:.0} dB)",
        wb::BASELINE_S,
        wb::BASELINE_WARMUP_S,
        wb::HP_HZ,
        plan.floor_db,
        plan.raw_dev_db,
        plan.expected_db,
        plan.thr_db,
        wb::FLOOR_MARGIN_DB
    );
    drop(snap);
    drop(env0);
    // 5. render the clicks with the default legacy profile (one period per event, one period queued)
    let mut ro = wasapi_capture::ImpulseRenderOptions::new(&sink.name);
    ro.allow_endpoint = a.allow.clone();
    ro.shape = wb::SHAPE;
    ro.peak_dbfs = a.db;
    let mut renderer = match wasapi_capture::ImpulseRenderer::start(&ro) {
        Ok(r) => r,
        Err(e) => {
            session.stop();
            return Err(e.into());
        }
    };
    let i = renderer.info().clone();
    println!(
        "render \"{}\" {} Hz {} ch {}, mode {}, buffer {} fr, period {} fr, stream latency {:.2} ms, click {} frames at {} dBFS, fill {}, prefill {} fr",
        i.endpoint,
        i.format.sample_rate,
        i.format.channels,
        i.format.format.name(),
        i.mode.name(),
        i.buffer_frames,
        i.period_frames,
        i.stream_latency_ms,
        i.click_frames,
        i.peak_dbfs,
        i.fill.describe(),
        i.prefill_frames
    );
    std::thread::sleep(Duration::from_secs_f64(wb::RENDER_SETTLE_S - 0.15));
    let hz = wasapi_capture::qpc_hz();
    let mut times = Vec::new();
    let first = Instant::now();
    let mut failure: Option<String> = None;
    for k in 0..a.count {
        if run_start.elapsed().as_secs_f64() > wb::MAX_TOTAL_S - wb::TAIL_S {
            failure = Some(format!("the {:.0} s run limit was reached after {} click(s)", wb::MAX_TOTAL_S, k));
            break;
        }
        match renderer.fire() {
            Ok(t) => times.push(t),
            Err(e) => {
                failure = Some(format!("click {} failed: {e}", k + 1));
                break;
            }
        }
        if k + 1 < a.count {
            let next = first + Duration::from_secs_f64(f64::from(k + 1) * f64::from(a.interval_ms) / 1000.0);
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    }
    std::thread::sleep(Duration::from_secs_f64(wb::TAIL_S));
    renderer.stop();
    let st = session.stats().snapshot();
    session.stop();
    if let Some(f) = &failure {
        println!("WARNING: {f}");
    }
    // 6. analysis
    let rec = std::mem::take(&mut *rec.lock().unwrap());
    let env = wb::envelope(&rec, wb::HP_HZ);
    let inject: Vec<i64> = times.iter().map(|t| t.submit_ticks).collect();
    let det = wb::detect_clicks(&rec, &env, &plan, &inject, hz);
    let (mut totals, mut queues) = (Vec::new(), Vec::new());
    for (t, d) in times.iter().zip(&det) {
        let (q, which) = t.render_delay_ms(hz);
        queues.push(q);
        match d {
            Some(d) => {
                totals.push(d.total_ms);
                println!(
                    "#{}: inject -> detected {:.2} ms (onset bias {:.1} fr), render queue {:.2} ms ({which}; padding {} fr), envelope peak {:.1} dBFS",
                    t.index + 1,
                    d.total_ms,
                    d.bias_frames,
                    q,
                    t.padding_frames,
                    d.peak_db
                );
            }
            None => println!("#{}: NOT detected (render queue {:.2} ms ({which}))", t.index + 1, q),
        }
    }
    println!(
        "capture packets {} frames {} discontinuities {} silent packets {}",
        st.packets, st.frames_captured, st.discontinuities, st.silent_packets
    );
    let Some(s) = wb::summarize(&totals) else {
        return Err("no click was detected on the capture endpoint: is the Yamaha WDM Streaming loopback carrying the Music output? (check the Yamaha mixer routing / the endpoint name)".into());
    };
    println!("--- direct WDM (no router): injection -> Streaming capture callback, ms ---");
    println!("n {}  min {:.2}  median {:.2}  mean {:.2}  max {:.2}  sigma {:.2}", s.n, s.min, s.median, s.mean, s.max, s.sigma);
    if let Some(q) = wb::summarize(&queues) {
        println!("render queue (IAudioClock estimate, injection -> leaves the render client): median {:.2} ms (min {:.2}, max {:.2})", q.median, q.min, q.max);
    }
    let buffering = cap_period_ms + cap_latency.unwrap_or(0.0);
    println!(
        "capture-side buffering estimate: capture period {:.2} ms + GetStreamLatency {} = {:.2} ms",
        cap_period_ms,
        cap_latency.map(|v| format!("{v:.2} ms")).unwrap_or_else(|| "n/a (counted as 0)".into()),
        buffering
    );
    for l in wb::comparison_lines(&wb::compare(a.router_total_ms, s.median, buffering)) {
        println!("{l}");
    }
    Ok(())
}

fn fmt_period(frames: u32, rate: u32) -> String {
    format!("{} fr / {:.3} ms", frames, frames as f64 * 1000.0 / rate.max(1) as f64)
}

/// Read-only description of render endpoint(s): mix format, periods, IAudioClient3, volume/mute.
fn render_info(name: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let eps: Vec<_> = match name {
        Some(n) => vec![find_device(Flow::Render, n)?],
        None => list_endpoints(Flow::Render)?,
    };
    for ep in eps {
        println!("=== {}{}", ep.name, if ep.is_default { "  (default)" } else { "" });
        println!("id     : {}", ep.id);
        match get_capture_info(&ep.id) {
            Ok(i) => {
                println!(
                    "mix    : {} Hz, {} ch, {} ({} bit container, {} valid), mask 0x{:X}",
                    i.mix.sample_rate, i.mix.channels, i.mix.format.name(), i.mix.bits_per_sample, i.mix.valid_bits, i.mix.channel_mask
                );
                println!("GetDevicePeriod: default {:.3} ms, min {:.3} ms", i.default_period_ms(), i.min_period_ms());
                match i.engine {
                    Some(e) => println!(
                        "IAudioClient3 engine period: default {}  fundamental {}  min {}  max {}",
                        fmt_period(e.default_frames, i.mix.sample_rate),
                        fmt_period(e.fundamental_frames, i.mix.sample_rate),
                        fmt_period(e.min_frames, i.mix.sample_rate),
                        fmt_period(e.max_frames, i.mix.sample_rate)
                    ),
                    None => println!("IAudioClient3 engine period: not available"),
                }
                if let Some((p, r)) = current_engine_period(&ep.id) {
                    println!("current engine period: {}", fmt_period(p, r));
                }
                println!("exclusive mode accepts mix format: {}", i.exclusive_supported);
            }
            Err(e) => println!("info error: {e}"),
        }
        match endpoint_volume(&ep.id) {
            Ok((v, m)) => println!("endpoint volume (read-only): scalar {:.3}, mute {}", v, m),
            Err(e) => println!("endpoint volume: error: {e}"),
        }
    }
    Ok(())
}

#[derive(Default)]
struct LbWin {
    /// (arrival us since t0, frames)
    pkts: Vec<(f64, u32)>,
    peak: f32,
    sumsq: f64,
    n: u64,
}

type RecBuf = Arc<Mutex<(Vec<f32>, Vec<f32>)>>;

struct LbSink {
    t0: Instant,
    w: Arc<Mutex<LbWin>>,
    rec: Option<RecBuf>,
}

impl FrameSink for LbSink {
    fn on_packet(&mut self, frames: u32, _f: wasapi_capture::PacketFlags) {
        let us = self.t0.elapsed().as_secs_f64() * 1e6;
        self.w.lock().unwrap().pkts.push((us, frames));
    }
    fn on_frames(&mut self, l: &[f32], r: &[f32]) {
        let mut w = self.w.lock().unwrap();
        for &x in l {
            w.peak = w.peak.max(x.abs());
            w.sumsq += (x as f64) * (x as f64);
        }
        w.n += l.len() as u64;
        drop(w);
        if let Some(b) = &self.rec {
            let mut g = b.lock().unwrap();
            g.0.extend_from_slice(l);
            g.1.extend_from_slice(r);
        }
    }
}

fn hist_text(pk: &[(f64, u32)]) -> String {
    let mut h: std::collections::BTreeMap<u32, usize> = Default::default();
    for (_, f) in pk {
        *h.entry(*f).or_default() += 1;
    }
    h.iter().map(|(k, v)| format!("{k}x{v}")).collect::<Vec<_>>().join(" ")
}

fn loopback_run(args: &[String], record_to: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    use wasapi_capture::signal::SignalKind;
    let name = args.get(1).ok_or("usage: loopback-run <name> <secs> [flags]")?;
    let secs: f32 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(10.0);
    let rec_mode = record_to.is_some();
    let has = |f: &str| args.iter().any(|a| a == f);
    let keepalive = has("--keepalive") || (rec_mode && !has("--idle"));
    let tone_db: Option<f32> = match flag_val(args, "--tone-db") {
        Some(s) => Some(s.parse()?),
        None if rec_mode => Some(-50.0),
        None => None,
    };
    if let Some(d) = tone_db {
        if d > MAX_TEST_SIGNAL_DBFS {
            return Err(format!("refusing --tone-db above {MAX_TEST_SIGNAL_DBFS}").into());
        }
    }
    let kind = SignalKind::parse(flag_val(args, "--signal").map(String::as_str).unwrap_or("tone"), 1000.0)
        .ok_or("unknown --signal")?;
    let low_keep = has("--low") || has("--lowkeep");
    let low_loop = has("--low") || has("--lowloop");
    let ep = find_device(Flow::Render, name)?;
    println!("endpoint: {}  [{}]", ep.name, ep.id);
    if let Some((p, r)) = current_engine_period(&ep.id) {
        println!("engine period before start: {}", fmt_period(p, r));
    }
    if let Ok((v, m)) = endpoint_volume(&ep.id) {
        println!("endpoint volume (read-only): scalar {v:.3}, mute {m}");
    }
    let mut ka = None;
    if keepalive || tone_db.is_some() {
        let k = KeepAlive::start(
            &ep.id,
            KeepAliveOptions { low_latency: low_keep, signal: tone_db.map(|d| (kind, d)) },
        )?;
        let i = k.info();
        println!(
            "keep-alive: {} {} Hz {} ch {}; period {} (buffer {} fr); signal {}; attempts: {:?}",
            match i.mode {
                CaptureMode::LowLatency => "IAudioClient3-min",
                _ => "shared-default",
            },
            i.format.sample_rate,
            i.format.channels,
            i.format.format.name(),
            fmt_period(i.period_frames, i.format.sample_rate),
            i.buffer_frames,
            tone_db.map(|d| format!("{} {d} dBFS", kind.name())).unwrap_or_else(|| "digital silence".into()),
            i.attempts
        );
        ka = Some(k);
        std::thread::sleep(Duration::from_millis(400));
        if let Some((p, r)) = current_engine_period(&ep.id) {
            println!("engine period with keep-alive running: {}", fmt_period(p, r));
        }
    } else {
        println!("no render stream (idle test)");
    }
    let win = Arc::new(Mutex::new(LbWin::default()));
    let recbuf: Option<RecBuf> = rec_mode.then(|| Arc::new(Mutex::new((Vec::new(), Vec::new()))));
    let t0 = Instant::now();
    let sink = LbSink { t0, w: win.clone(), rec: recbuf.clone() };
    let mut s = LoopbackSession::start(
        &ep.id,
        LoopbackOptions {
            mode: if low_loop { CaptureMode::LowLatency } else { CaptureMode::Shared },
            allow_polling: !has("--noevent"),
            force_polling: has("--poll"),
            buffer_hns: flag_val(args, "--buf-hns").map(|s| s.parse()).transpose()?,
            poll_ms: flag_val(args, "--poll-ms").map(|s| s.parse()).transpose()?,
        },
        Box::new(sink),
    )
    .map_err(|e| {
        format!(
            "loopback start failed: {e} ({})",
            if wasapi_capture::loopback::is_endpoint_missing(&e) { "endpoint missing" } else { "other" }
        )
    })?;
    let info = s.info().clone();
    let rate = info.format.sample_rate;
    println!(
        "loopback: requested {}, in use {}, wake {:?}; {} Hz {} ch {}; period {}",
        if low_loop { "low-latency" } else { "shared" },
        info.mode.name(),
        info.wake,
        rate,
        info.format.channels,
        info.format.format.name(),
        fmt_period(info.period_frames, rate)
    );
    for a in &info.attempts {
        println!("  failed attempt: {a}");
    }
    if let Some((p, r)) = current_engine_period(&ep.id) {
        println!("engine period with loopback running: {}", fmt_period(p, r));
    }
    let nominal_us = info.period_frames as f64 * 1e6 / rate as f64;
    let end = Instant::now() + Duration::from_secs_f32(secs);
    let mut all: Vec<(f64, u32)> = Vec::new();
    let (mut peak_all, mut sumsq_all, mut n_all) = (0.0f32, 0.0f64, 0u64);
    let mut last = (Instant::now(), 0u64);
    let mut lost = false;
    while Instant::now() < end && !lost {
        std::thread::sleep(Duration::from_millis(1000).min(end.saturating_duration_since(Instant::now())));
        let now = Instant::now();
        let w = std::mem::take(&mut *win.lock().unwrap());
        let st = s.stats().snapshot();
        let dt = now.duration_since(last.0).as_secs_f64();
        let fps = (st.frames_captured - last.1) as f64 / dt;
        last = (now, st.frames_captured);
        let times: Vec<f64> = w.pkts.iter().map(|p| p.0).collect();
        let a = arrival_stats(&times, nominal_us);
        let rms = if w.n > 0 { (w.sumsq / w.n as f64).sqrt() } else { 0.0 };
        println!(
            "  {:>5.0} pk/s {:>7.0} fr/s | sizes {} | gap us min/mean/p99/max {} | {} | disc {} silent {}/{} | peak {:>6.1} rms {:>6.1} dBFS | wakes {} timeouts {}",
            w.pkts.len() as f64 / dt,
            fps,
            hist_text(&w.pkts),
            a.as_ref()
                .map(|a| format!("{:.0}/{:.0}/{:.0}/{:.0}", a.min_us, a.mean_us, a.p99_us, a.max_us))
                .unwrap_or_else(|| "-".into()),
            a.as_ref().map(|a| a.pattern()).unwrap_or("no data"),
            st.discontinuities,
            st.silent_packets,
            st.packets,
            db(w.peak),
            db(rms as f32),
            s.wakeups(),
            st.wait_timeouts
        );
        peak_all = peak_all.max(w.peak);
        sumsq_all += w.sumsq;
        n_all += w.n;
        all.extend(w.pkts);
        while let Ok(ev) = s.events().try_recv() {
            println!("  event: {ev:?}");
            if matches!(ev, CaptureEvent::DeviceLost | CaptureEvent::Error(_)) {
                lost = true;
            }
        }
        if let Some(k) = &ka {
            while let Ok(ev) = k.events().try_recv() {
                println!("  keep-alive event: {ev:?}");
            }
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    s.stop();
    let st = s.stats().snapshot();
    println!("--- summary ---");
    println!(
        "wall {:.2}s: packets {} frames {} ({:.1} fr/s vs nominal {}), discontinuities {}, silent packets {}, ts errors {}, wake-ups {}, wait timeouts {}",
        wall,
        st.packets,
        st.frames_captured,
        st.frames_captured as f64 / wall,
        rate,
        st.discontinuities,
        st.silent_packets,
        st.timestamp_errors,
        s.wakeups(),
        st.wait_timeouts
    );
    let times: Vec<f64> = all.iter().map(|p| p.0).collect();
    if let Some(a) = arrival_stats(&times, nominal_us) {
        println!(
            "inter-arrival us: min {:.0} mean {:.0} p50 {:.0} p99 {:.0} max {:.0}; back-to-back {}/{}; late(>1.5x period) {}; pattern: {}; packet sizes {}",
            a.min_us, a.mean_us, a.p50_us, a.p99_us, a.max_us, a.back_to_back, a.gaps, a.late, a.pattern(), hist_text(&all)
        );
        let rms = if n_all > 0 { (sumsq_all / n_all as f64).sqrt() } else { 0.0 };
        println!("level: peak {:.2} dBFS, rms {:.2} dBFS (L)", db(peak_all), db(rms as f32));
    } else {
        println!("NO PACKETS delivered (idle loopback: nothing rendering => no data); peak {:.1} dBFS", db(peak_all));
    }
    if let (Some(p), Some(b)) = (record_to, recbuf) {
        let g = b.lock().unwrap();
        write_wav_f32(std::path::Path::new(&p), rate, &g.0, &g.1)?;
        println!("recorded {} frames ({:.2}s) at {} Hz -> {}", g.0.len(), g.0.len() as f64 / rate as f64, rate, p);
        if let Some(d) = tone_db {
            println!("rendered {} {d} dBFS (expected L rms {:.2} dBFS for a tone)", kind.name(), d - 3.0103);
        }
    }
    if let Some(k) = &ka {
        println!("keep-alive: rendered {} frames, empty-buffer wakeups {}, lost {}", k.frames_rendered(), k.underruns(), k.is_lost());
    }
    Ok(())
}
