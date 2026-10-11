# Architecture

Back to the [documentation index](README.md).

## 1. Data flow

```
 Capture endpoint (WASAPI shared, event driven): Hi-Fi Cable Output, or Line 1 (Virtual Audio Cable)
        |  capture thread (MTA, MMCSS "Pro Audio")      wasapi-capture::session
        v     convert to stereo f32, push frames, arrival statistics
 SPSC ring (power-of-two, stereo f32)                     router-core::ring
        |
        |  ASIO callback (thread owned by the Yamaha driver)   router-engine::pipeline
        v
 RateMatcher        startup gate -> PI drift controller -> Kaiser-sinc resampler
        v                                                 router-core::resample
 EqProcessor        preamp -> up to 32 biquads -> soft limiter -> fault detector
        v                                                 router-core::processor, eq-core::dsp
 asio_convert       f32 -> the master's sample type (saturating)
        v                                                 router-core::asio_convert
 Yamaha ASIO output buffers (channels from `output_channels`) -> USB -> DAC
```

- The capture endpoint is chosen by `capture_device`. For a known virtual cable the capture can run in `CaptureMode::FixedPeriod` (IAudioClient3 `InitializeSharedAudioStream` with an explicit period, section 5a); otherwise it is plain Shared. Everything after the ring is identical for both sources.
- The producer never overwrites unread data. If the ring is full, the excess new frames are dropped and counted (`overruns`). Latency is trimmed on the consumer side: when the fill exceeds twice the target the oldest frames are discarded (`discarded_frames`).
- Startup gate: the output stays silent until the ring fill reaches the target. If underrun blocks repeat, the gate re-arms (`gate_rearms`) and the stream restarts cleanly instead of crackling.
- Drift controller: estimates the clock offset between the cable and the ASIO clock and trims the resample ratio by at most +-0.5 % (`MAX_TRIM`) with a slew limit of 40 ppm/s. The current trim is `drift_ppm` in the status.
- Resampler: `Cubic` (default) is a 32-tap Kaiser-windowed sinc whose cutoff follows the nominal ratio, so both up- and downsampling are band-limited; `Linear` is the cheapest mode. Selected live with `resampler_quality`.
- Only the first two ASIO output channels used are written (`output_channels`); the engine creates buffers for those two outputs only.

## 2. Threads

| Thread | Owner | Job | Real-time? |
|---|---|---|---|
| Control (`router-control`) | engine | COM STA + message pump (ASIO drivers may post to its queue); opens/tears down the session; 100 ms status tick; config-file poll (500 ms) for restart-class changes; retry/back-off | No (MMCSS "Pro Audio" at normal priority) |
| Capture | `CaptureSession` | WASAPI event loop, writes the ring, heartbeat, arrival statistics | Yes (MMCSS, CRITICAL) |
| ASIO callback | Yamaha driver | `bufferSwitch` runs the whole pipeline (`directProcess` is always TRUE) | Yes (the driver registers MMCSS "Pro Audio" itself; the engine tries once more and counts error 1552 as OK) |
| Watchdog | `router-core::watchdog` | polls heartbeats every 200 ms, reports edge-triggered `Stalled` / `Recovered` | No |
| Logger writer | `router-core::logger` | drains a lock-free MPSC ring to `%TEMP%\eq_asio_router_engine.log` | No |
| Config watcher | `router-core::watcher` | polls the config mtime every 250 ms, loads, sanitizes, publishes a `Snapshot` through `Handoff` | No |
| Cable-format helper | `router-engine::cable` | short-lived COM MTA thread for reading/setting cable formats (VB-Audio cables only; never touches VAC) | No |
| Render keep-alive (optional) | `wasapi-capture::keepalive` (`RenderKeepAlive`) | renders silence at the minimum IAudioClient3 period into the cable's render endpoint while the session runs; started and dropped with the session | Not on the audio path |
| Source-readiness probe | `router-engine::source` (`source_readiness`) | short-lived thread with its own COM apartment: read-only check of the cable's endpoints for the GUI | No |
| GUI worker | `eq-asio-app::host` | owns the `Engine` so the UI thread never blocks | No |
| GUI (egui) | `eq-asio-app` | window, tray, reads `EngineStatus` | No |
| Tap writer (optional) | `router-engine::tap` | writes measurement WAVs when `--dump-dir` is used | No |

Teardown order (`Session::drop`): heartbeat off, watchdog stopped, ASIO stream stopped, buffers disposed and the driver released, then the capture session stopped and the config watcher joined. The Yamaha is therefore always released first, and the audio callback never outlives the ring it reads.

## 3. Real-time rules

Everything reachable from `AudioCallback::buffer_switch` must be allocation-free, lock-free and syscall-light.

- No allocation, no mutex, no file or network I/O, no blocking. Only atomics (relaxed), QueryPerformanceCounter and `rlog!`.
- `rlog!` formats into a stack buffer and pushes into a bounded lock-free ring; if the ring is full the record is dropped and counted (`dropped_logs`).
- Config changes reach the callback through `Handoff`: the watcher builds a complete `Snapshot` (sanitized profile, settings, auto-headroom gain) off-thread and publishes it with one `AtomicPtr` swap. The callback takes it with one atomic swap and retires the old one into a fixed array of slots; the watcher frees it later. Filter state is kept, so a profile change does not click.
- Status goes to the GUI through atomics and the shared-memory block, never through locks on the audio path.
- No `unwrap` / `expect` on the audio path; a panic in the callback is caught and reported as a reset (`RESET_PANIC`), which restarts the engine.
- The one-time MMCSS registration of the driver's callback thread happens in the very first (silent priming) callback.

## 4. Config and status plumbing

- Config file: `%APPDATA%\eq_asio_router\config.toml` (see [configuration.md](configuration.md)). Saved atomically (temp file plus rename). `Config::sanitized` clamps every value; a file that fails to parse keeps the last good config in force and the error is shown as `config_error`.
- Two paths watch the file. The `ConfigWatcher` (250 ms) publishes EQ-class changes to the audio thread through `Handoff`. The control thread (500 ms) classifies changes: master driver, sample rate, capture device, capture mode, `capture_period_ms`, `render_keepalive`, output channels and enabling `match_cable_rate` cause a controlled restart; everything else is hot.
- `Engine::apply` does the same classification for GUI edits and returns `Unchanged`, `HotApplied` or `RestartScheduled(reason)`.
- Hot (live): EQ profile, bypass, AutoEQ enabled, limiter, auto headroom, safety buffer, auto safety, resampler quality, jitter margin. `source_preset` is only a label: changing it without changing the fields it copies restarts nothing.
- Status block: `Local\eq_asio_router_status`, 128 bytes, magic `EQST`, layout version 2 (`eq-core/src/status.rs`). The engine creates the mapping once; the audio thread only does relaxed stores into it; external readers use `StatusReader`. Field list in [configuration.md](configuration.md#5-status-block). The in-process GUI mostly reads the richer `EngineStatus` snapshot (updated every 100 ms).
- Capabilities of the master (`caps.toml` next to the config) are written by the engine because the GUI cannot open the exclusive ASIO driver itself.

## 5. Engine state machine

```
        start
          v
     +-> Starting --open ok--> Running --stall/reset/capture lost/config restart--+
     |      | open failed                                                          |
     |      +-- device busy ------> WaitingForDevice (retry every 2 s, or retry_now)
     |      +-- unavailable/other -> Restarting{reason} (back-off 200 ms doubling to 5 s)
     |      +-- fatal -------------> Failed (waits for a new config or stop)        |
     +----------------------------------------------<-- Restarting{reason} <-------+
```

- `Stopped` is the initial and final state.
- Back-off: 200 ms doubling to a 5 s cap; a run of at least 10 s resets it.
- Fatal errors (no retry without a config change): frozen v1 driver named as master, master not registered, fewer than 2 outputs, unsupported sample type, another instance running. With `retry_unavailable` (used by the tray app, so that login-time starts survive devices that appear late) a missing master or capture device is retried instead.
- Restart triggers while running: ASIO reset request, ASIO rate change, callback panic, audio callback stalled for 1 s, capture stalled for 3 s, capture device lost, capture error, capture thread ended, restart-class config change. A capture `Fallback` event (the requested mode could not be opened, for example a refused period) is only recorded as `capture_note`. A capture-corruption event from a fixed-period session restarts the session once in plain Shared (section 5a); in any other mode it is only logged and shown as `capture_error`.
- Single instance: the engine takes the named mutex `Local\eq_asio_router_engine` (error `AlreadyRunning` if held). The GUI has its own `Local\eq_asio_router_gui` mutex and a show-event: a second launch shows the first window and exits.

### 5a. Fixed-period capture, corruption fallback and keep-alive

Code: `crates/router-engine/src/source.rs`, `engine.rs`, `crates/wasapi-capture/src/{session,monitor,keepalive}.rs`. Status: implemented and unit-tested; hardware validation (real fixed-period capture, keep-alive, live fallback) is pending.

- Planning: `plan_capture(cfg, endpoint_name, endpoint_rate, forced_shared)` maps the config to `CaptureOptions`. `FixedPeriod` is chosen only when `capture_period_ms > 0`, the effective mode is Shared and the endpoint is a known virtual cable (`is_virtual_cable_name`: VAC and the VB-Audio cables). Otherwise the period is ignored and a note is shown (`capture_note`). A 6 ms request is passed as "no explicit period" so the capture layer rounds 6 ms up to a multiple of the engine's fundamental period (288 frames at 48 kHz); any other value is converted to frames explicitly. Exclusive is still mapped to Shared; `LowLatency` is never silently upgraded to `FixedPeriod`.
- Capture layer defaults (`wasapi-capture`): `DEFAULT_FIXED_PERIOD_MS` 6 ms (288 frames at 48 kHz), safe floor `SAFE_PERIOD_FLOOR_MS` 5 ms (below it the request is refused and falls back to Shared unless `CaptureOptions::allow_unsafe_period`, lab use only), and `MIN_PERIOD_GUARD_MS` 2.5 ms: `LowLatency` on an endpoint whose IAudioClient3 minimum is below 2.5 ms (VAC lite reports 48 frames) is refused unless `force_min_period` is set. The granted period is validated against `GetSharedModeEnginePeriod`.
- Corruption monitor: `CorruptionMonitor` runs on the capture thread (`MonitorPolicy`: `Auto` = `StopOnTrip` for `FixedPeriod` and `Off` otherwise; `ReportOnly` for diagnostics). It trips on frames arriving faster than real time, repeated identical blocks, or sustained packet-size/gap anomalies (`CorruptionReason`); `DATA_DISCONTINUITY` flags are deliberately not used. On a trip the capture thread stops and sends `CaptureEvent::Corruption`.
- Fallback (`FallbackState`, `router-engine/src/source.rs`): the engine restarts the session once in plain Shared and sets `EngineStatus::capture_fallback` ("Capture fell back to Shared (10 ms): <reason>"). While the flag is set, `plan_capture` returns Shared but still reports the requested period, so the status shows requested 288 versus granted 480. A further corruption event is ignored (no restart loops). The flag is cleared when the capture device or `capture_period_ms` changes (`FallbackState::note_config`), never by a plain restart, or explicitly by `Engine::reset_fallback()` (the GUI's "Retry fixed period"), which also restarts the stream.
- Render keep-alive: when `render_keepalive` is on and the capture endpoint is a known virtual cable, the session starts a `RenderKeepAlive` on the paired render endpoint (`pair_render_name`: for Line 1 the same name, for `Hi-Fi Cable Output` the `... Input` name). Its engine period before and after is reported in `EngineStatus::keepalive` (VAC Line 1: 10 ms to 1 ms expected, not yet verified on hardware). It is never fatal: a failure only sets `keepalive_note`; if its endpoint disappears it is stopped with a note. It is stopped and dropped with the session and never runs on a real output device.
- Readiness: `source_readiness(cfg)` (read-only, nothing is opened for streaming) reports whether the capture and render endpoints are active, whether the render endpoint is the Windows default playback device, and whether both are stereo at the same rate, plus one-sentence notes for the GUI. A missing `Line 1` capture endpoint is "unavailable", not fatal: the engine shows "Waiting for Line 1 (Virtual Audio Cable)..." and keeps retrying.
- Cable-rate matching (`router-engine/src/cable.rs`) matches VB-Audio cable names only (`eq_core::is_vb_cable_name`); a name containing "Virtual Audio Cable" is never a VB cable, so VAC endpoints are never touched.
- Process hardening: the MMCSS fix in `router-engine/src/priority.rs` (error 1552 counted as OK) takes effect after rebuilding and restarting the app.

## 6. Crates

| Crate | Responsibility |
|---|---|
| `eq-core` | Pure Rust, no `unsafe`: biquads and EQ chain, limiter, fault detector, AutoEQ parser/writer, `Config` (with `SourcePreset`, `apply_source_preset`, per-device jitter margins, VAC / VB cable name helpers), `Caps`, `StatusBlock` layout |
| `router-core` | Ring, resampler and drift controller, arrival model, EQ processor and `Handoff`, ASIO sample conversion, logger, watchdog, config watcher, status writer/reader, offline analyzer, WAV I/O |
| `router-engine` | `Engine` (control thread, state machine, fallback and keep-alive lifecycle), real-time `pipeline`, `source` (capture planning, `FallbackState`, source readiness), process hardening (`priority`), VB cable rate matching (`cable`), measurement tap, synthetic source |
| `wasapi-capture` | WASAPI capture (shared, low-latency, fixed-period), device listing, endpoint format policy, keep-alive render, corruption monitor, process loopback, WDM-KS listing, `capture-probe` |
| `asio-abi` | Hand-written ASIO ABI (types, `IASIO` vtable); no SDK files |
| `asio-host` | Hosts an installed ASIO driver: registry enumeration, COM creation, wrapper, holder detection (`busy`) |
| `router-cli` | Headless front end: devices, caps, holders, run, analyze |
| `eq-asio-app` | egui GUI, tray, run at login, single instance, settings |
| `asio-probe` | Console ASIO host for testing a driver |
| `eq-asio-driver` | Frozen v1 virtual ASIO driver (see below) |

## 7. Timing and headroom model

Implemented in `crates/router-core/src/arrival.rs` and `router-engine/src/engine.rs` (`tick`).

- The producer delivers audio in arrival events (calls less than 0.5 ms apart are merged). The Hi-Fi Cable hands over two 480-frame packets back to back every 20 ms, i.e. one 960-frame event per 20 ms.
- The deviation `D(t) = A(t) - r t` is a sawtooth; the ring fill at each pull is `D(t) + c`, and the drift controller keeps the mean fill at the target. A stream never underruns if `target >= block + lookahead + max dip`, where the dip of an event is `mean(D) - D` just before it. For regular events every G seconds the dip is half the gap in frames (480 frames for the cable's pairs).
- The capture sink measures the dip and the gap directly and aggregates them into histograms over a sliding 30 s window. Required headroom = p99.9 dip times 1.25, plus a small look-ahead.
- Ring target (cable frames) = safety buffer + one master buffer (converted to cable frames) + jitter headroom (`target_fill_frames`). The ring capacity is dimensioned for the maximum safety (20 ms) and 4096-frame capture periods so the sliders can move live.
- `HeadroomPlanner`: starts conservatively (two capture periods, or the learned margin for the current capture device if set: `Config::jitter_margin_for`), raises at once when the measurement demands it, lowers slowly (steps of 48 frames, hold 5 s, every 2 s) once statistics are at least 10 s old and the mean fill has reached the target. A learned margin is a floor.
- "Learn (30 s)" resets the statistics, measures for 30 s (needs at least 500 events) and reports the lowest safe headroom in ms; the GUI stores it per capture device in `jitter_margin_by_device` (`Config::set_jitter_margin`, which also mirrors the legacy `jitter_margin_ms`). Switching source therefore never reuses the other source's margin.
- Model estimates for the VAC source (not measurements): alternating-gap simulation of a 288-frame fixed period (gaps alternating short and long around 6 ms) gives a planner target of about 600 frames, and the start target is 784 frames. The real arrival pattern of the fixed-period capture has not been recorded in the router yet.
- Auto safety (`AutoSafety`) adds to `safety_buffer_ms` after underruns, up to 20 ms in total.
- Auto headroom (EQ) is separate: it lowers the preamp so the profile's maximum boost is not above 0 dBFS (`eq-core::dsp::auto_headroom_db`).
- The live latency breakdown (`Caps::latency_breakdown`) is master output latency + mean ring fill + measured capture staleness + the render side. The render side cannot be observed by the router: it is the median measured by `router-cli latency` (stored in `latency_measured.toml`, read once per session, used when the capture device, keep-alive state, ASIO buffer and capture period (within 25 %) match and it is at most 30 days old, label `measured <date>`) and otherwise a default estimate (20 ms VAC Line 1, 100 ms Hi-Fi Cable; `eq_core::measured`), labelled as an estimate in the GUI, the tray tooltip (`est.`) and the status line. The old fixed 10 ms was too low on every measurement.

## 8. The frozen v1 driver

`eq-asio-driver` is a virtual ASIO driver ("EQ ASIO Router") that was written for the VB-Audio ASIO Bridge. It is frozen and no longer used: the router now hosts the Yamaha driver directly. It stays in the workspace for reference and must not be extended. The engine refuses to use it as master (`ForbiddenMaster`). It is a 32-bit DLL for the Bridge (i686 target); `scripts\install-driver.ps1` and `uninstall-driver.ps1` register and unregister it with `regsvr32` (self-elevating, registered in place under `target\`). Its debug log is `%TEMP%\eq_asio_router_driver.log`. Do not install it unless you know you need it.
