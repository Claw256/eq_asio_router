# Architecture

Back to the [documentation index](README.md).

## 1. Data flow

```
 VBMatrix process (ASIO host)                    Engine process
┌───────────────────────────────┐  shm ring A  ┌───────────────────────────────────────────┐
│ apps -> VBMatrix In (WDM)     │  (source)    │ Yamaha ASIO callback (router-engine::     │
│ matrix patching               │─────────────►│   pipeline):                              │
│   -> ASIO slot: our DLL       │              │   BridgeSource pops ring A -> stereo f32  │
│     "EQ ASIO Router"          │  shm ring B  │     -> RateMatcher (startup gate, PI drift │
│     (eq_asio_driver.dll),     │  (loopback)  │        controller, Kaiser-sinc resampler)  │
│     self-clocked pacing       │◄─────────────│     -> EqProcessor (preamp, 32 biquads,   │
│     thread; reads the         │              │        soft limiter, fault detector)      │
│     outputs, feeds its        │              │     -> asio_convert -> Yamaha outputs     │
│     inputs with the post-EQ   │              │        -> USB -> DAC                      │
│     audio                     │              │ loopback feeder thread: EQ tap ->         │
│                               │              │   resampler -> ring B                     │
└───────────────────────────────┘              └───────────────────────────────────────────┘
```

- The only audio source is the bridge: the virtual ASIO driver "EQ ASIO Router" runs inside VBMatrix's ASIO slot and shuttles samples through the shared-memory mapping `Local\eq_asio_router_bridge_v2` (`bridge-abi`). There is no WASAPI capture in the engine. Ring A carries the matrix mix (driver -> app, stereo f32 at the negotiated driver rate); ring B carries the post-EQ audio back (app -> driver).
- The producer never overwrites unread data. If ring A is full, the excess new frames are dropped and counted (`a_overruns`); ring B likewise (`b_dropped`). Latency is trimmed on the consumer side: when ring A's fill exceeds twice the target the oldest frames are discarded (`a_discarded`). A ring-B underrun is fed to the driver's inputs as silence (`b_underruns`).
- Startup gate: the output stays silent until the ring fill reaches the target. If underrun blocks repeat, the gate re-arms (`gate_rearms`) and the stream restarts cleanly instead of crackling.
- Drift controller: estimates the clock offset between the bridge clock and the ASIO clock and trims the resample ratio by at most +-0.5 % (`MAX_TRIM`) with a slew limit of 40 ppm/s. Two clock domains remain (VBMatrix runs on its Internal Clock while the engine owns the Yamaha), so the resampler stays in both directions. The current trim is `drift_ppm` in the status.
- Resampler: `Cubic` (default) is a 32-tap Kaiser-windowed sinc whose cutoff follows the nominal ratio, so both up- and downsampling are band-limited; `Linear` is the cheapest mode. Selected live with `resampler_quality`.
- Only the first two ASIO output channels used are written (`output_channels`); the engine creates buffers for those two outputs only.

## 2. Threads

| Thread | Owner | Job | Real-time? |
|---|---|---|---|
| Control (`router-control`) | engine | COM STA + message pump (ASIO drivers may post to its queue); opens/tears down the session; 100 ms status + bridge tick (`BridgeMonitor` heartbeat freshness, `FillWindow` statistics, `app_beat`); config-file poll (500 ms) for restart-class changes; retry/back-off | No (MMCSS "Pro Audio" at normal priority) |
| ASIO callback | Yamaha driver | `bufferSwitch` runs the whole pipeline (`directProcess` is always TRUE) | Yes (the driver registers MMCSS "Pro Audio" itself; the engine tries once more and counts error 1552 as OK) |
| Loopback feeder | `router-engine::bridge` (`LoopbackFeeder`) | drains the in-process post-EQ tap, resamples Yamaha rate -> negotiated bridge rate (drift controller on `fill_b`) and pushes into ring B; started and dropped with the session when `[bridge] loopback_enabled` is on | No (the tap push inside the callback is allocation-free and lock-free) |
| Driver pacing thread | `eq-asio-driver` (inside VBMatrix) | high-resolution waitable timer, MMCSS "Pro Audio", QPC-anchored absolute schedule; fires the host's `bufferSwitch` at `sample_rate / buffer_frames`; per callback it shuttles the ASIO buffers and the shm rings | Yes |
| Watchdog | `router-core::watchdog` | polls heartbeats every 200 ms, reports edge-triggered `Stalled` / `Recovered` | No |
| Logger writer | `router-core::logger` | drains a lock-free MPSC ring to `%TEMP%\eq_asio_router_engine.log` | No |
| Config watcher | `router-core::watcher` | polls the config mtime every 250 ms, loads, sanitizes, publishes a `Snapshot` through `Handoff` | No |
| GUI worker | `eq-asio-app::host` | owns the `Engine` so the UI thread never blocks | No |
| GUI (egui) | `eq-asio-app` | window, tray, reads `EngineStatus` | No |
| Tap writer (optional) | `router-engine::tap` | writes measurement WAVs when `--dump-dir` is used | No |

Teardown order (`Session::drop`): heartbeat off, watchdog stopped, ASIO stream stopped, buffers disposed and the driver released, then the loopback feeder and the synthetic source (measurements only) stopped, the config watcher joined and the measurement tap finished. The Yamaha is therefore always released first, and the audio callback never outlives the ring it reads.

## 3. Real-time rules

Everything reachable from `AudioCallback::buffer_switch` must be allocation-free, lock-free and syscall-light.

- No allocation, no mutex, no file or network I/O, no blocking. Only atomics (relaxed), QueryPerformanceCounter and `rlog!`. There is no WASAPI anywhere in the engine's audio path.
- The shm rings (`bridge-abi`) are pure atomics with single-writer `u64` positions that wrap with `wrapping_*`: the driver writes only `driver_beat`, `a_write`, `b_read`, the app only `app_beat`, `b_write`, `a_read`, each group on its own 64-byte cache line so the two processes never false-share. The Yamaha callback reads ring A and the loopback tap writes ring B through those atomics only; the post-EQ tap push is allocation-free and lock-free.
- `rlog!` formats into a stack buffer and pushes into a bounded lock-free ring; if the ring is full the record is dropped and counted (`dropped_logs`).
- Config changes reach the callback through `Handoff`: the watcher builds a complete `Snapshot` (sanitized profile, settings, auto-headroom gain) off-thread and publishes it with one `AtomicPtr` swap. The callback takes it with one atomic swap and retires the old one into a fixed array of slots; the watcher frees it later. Filter state is kept, so a profile change does not click.
- Status goes to the GUI through atomics and the shared-memory block, never through locks on the audio path.
- No `unwrap` / `expect` on the audio path; a panic in the callback is caught and reported as a reset (`RESET_PANIC`), which restarts the engine.
- The one-time MMCSS registration of the driver's callback thread happens in the very first (silent priming) callback.

## 4. Config and status plumbing

- Config file: `%APPDATA%\eq_asio_router\config.toml` (see [configuration.md](configuration.md)). Saved atomically (temp file plus rename). `Config::sanitized` clamps every value; a file that fails to parse keeps the last good config in force and the error is shown as `config_error`.
- Two paths watch the file. The `ConfigWatcher` (250 ms) publishes EQ-class changes to the audio thread through `Handoff`. The control thread (500 ms) classifies changes: master driver, sample rate, output channels and `[bridge] loopback_enabled` cause a controlled restart; everything else is hot.
- `Engine::apply` does the same classification for GUI edits and returns `Unchanged`, `HotApplied` or `RestartScheduled(reason)`.
- Hot (live): EQ profile, bypass, AutoEQ enabled, limiter, auto headroom, safety buffer, auto safety, resampler quality, jitter margin.
- Status block: `Local\eq_asio_router_status`, 192 bytes, magic `EQST`, layout version 3 (`eq-core/src/status.rs`). The engine creates the mapping once; the audio thread only does relaxed stores into it; external readers use `StatusReader`. Field list in [configuration.md](configuration.md#5-status-block). The in-process GUI mostly reads the richer `EngineStatus` snapshot (updated every 100 ms).
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
- Fatal errors (no retry without a config change): the bridge driver "EQ ASIO Router" named as master (`ForbiddenMaster` — the engine must never open its own bridge as master), master not registered, fewer than 2 outputs, unsupported sample type, another instance running. With `retry_unavailable` (used by the tray app, so that login-time starts survive devices that appear late) a missing master is retried instead.
- Restart triggers while running: ASIO reset request, ASIO rate change, callback panic, audio callback stalled for 1 s, restart-class config change. A stalled driver heartbeat (the VBMatrix slot stopped feeding ring A) is a *source gap*, not a restart: the fade/gate-rearm path plays silence until the beat resumes (engine.rs).
- Single instance: the engine takes the named mutex `Local\eq_asio_router_engine` (error `AlreadyRunning` if held). The GUI has its own `Local\eq_asio_router_gui` mutex and a show-event: a second launch shows the first window and exits.

### 5a. Bridge source, fill statistics and loopback

Code: `crates/router-engine/src/bridge.rs`, `engine.rs`, `crates/bridge-abi`. The WASAPI fixed-period capture, corruption monitor/fallback, render keep-alive and cable-rate matching were removed with the old source in M3a (their measurements and design notes are kept historical in [docs/latency/](latency/)).

- Source: `BridgeSource` (a `FrameSource` for the unchanged `RateMatcher` chain) pops ring A inside the Yamaha callback. The producer lives in the VBMatrix process (the driver's callback), so there is no in-process capture ring any more. Ring A is created by the driver (it appears when VBMatrix loads the slot); the app attaches live and detaches live.
- Ring-A target: derived from the negotiated driver buffer plus the safety buffer; `[bridge] ring_target_ms` only acts as an optional floor on top of that (0 = no floor, clamped to 0..=200 ms; `bridge::apply_ring_target_floor`).
- Fill statistics replace the WASAPI arrival model: the shm header carries no producer timestamps, only `driver_beat` (one increment per driver callback). `FillWindow` records, per 100 ms control tick, the dip of the ring-A fill (mean minus the minimum seen at the Yamaha callbacks inside that tick) plus the `driver_beat` delta, and produces the `ArrivalSnapshot` the existing `HeadroomPlanner` consumes (frame staleness = `buffer / 2 / rate` until statistics exist). "Learn (30 s)" and the live planner keep working with ring-fill semantics.
- Learned margins are keyed by the fixed source name `"vbmatrix"` (`eq_core::BRIDGE_SOURCE_KEY`): `Config::set_jitter_margin` writes `jitter_margin_by_device["vbmatrix"]`; legacy per-device keys in old files are migrated by `Config::sanitized`.
- Loopback feeder (`LoopbackFeeder`, when `[bridge] loopback_enabled` is on, restart-class): a dedicated non-RT thread drains a small in-process SPSC that is tapped inside the Yamaha callback after `EqProcessor` (the tap push is allocation-free and lock-free), resamples Yamaha rate -> negotiated rate with a `DriftController` on `fill_b` (the mirror of the ring-A controller) and pushes into ring B. Ring-B underruns are fed to the driver's inputs as silence.
- Readiness: `BridgeMonitor` derives `vbmatrix_running` (`driver_running` plus a fresh `driver_beat`; stall limit `bridge::BEAT_STALL` = 1 s). The GUI bridge panel shows: VBMatrix process found, "EQ ASIO Router" registered, mapping attached, slot running, negotiated format and the ring fills; the status block carries the same in `bridge_attached` .. `b_dropped`.

## 6. Crates

| Crate | Responsibility |
|---|---|
| `eq-core` | Pure Rust, no `unsafe`: biquads and EQ chain, limiter, fault detector, AutoEQ parser/writer, `Config` (with `BridgeConfig`, the fixed `"vbmatrix"` source key and jitter-margin migration), `Caps`, `StatusBlock` layout (v3, 192 bytes) |
| `router-core` | Ring, resampler and drift controller, arrival/fill statistics, EQ processor and `Handoff`, ASIO sample conversion, logger, watchdog, config watcher, status writer/reader, offline analyzer, WAV I/O |
| `bridge-abi` | The shared-memory bridge contract between the driver and the app: mapping layout (`Local\eq_asio_router_bridge_v2`, fixed size), ring helpers, image validation |
| `router-engine` | `Engine` (control thread, state machine), real-time `pipeline`, `bridge` (shm source, fill statistics, loopback feeder, bridge monitor), process hardening (`priority`), measurement tap, synthetic source |
| `wasapi-capture` | Measurement tooling only (`capture-probe`): device listing, impulse and tone measurement into render endpoints (now "VBMatrix In"), WDM baseline; the engine no longer captures over WASAPI |
| `asio-abi` | Hand-written ASIO ABI (types, `IASIO` vtable); no SDK files |
| `asio-host` | Hosts an installed ASIO driver: registry enumeration, COM creation, wrapper, holder detection (`busy`) |
| `router-cli` | Headless front end: devices, caps, holders, run, latency, analyze |
| `eq-asio-app` | egui GUI (bridge readiness panel), tray, run at login, single instance, settings |
| `asio-probe` | Console ASIO host for testing a driver |
| `eq-asio-driver` | The v2 virtual ASIO bridge driver "EQ ASIO Router" (see below) |

## 7. Timing and headroom model

Implemented in `crates/router-engine/src/bridge.rs` (`FillWindow`), `router-core/src/arrival.rs` and `router-engine/src/engine.rs` (`tick`).

- The source is the bridge ring. The driver delivers audio in callback bursts; `FillWindow` records the dip of the ring-A fill per 100 ms control tick (mean minus the minimum seen at the Yamaha callbacks inside that tick) plus the `driver_beat` delta, and feeds the statistics to the planner (section 5a). Until statistics exist, the frame staleness is `buffer / 2 / rate` (half the negotiated bridge buffer).
- The deviation `D(t) = A(t) - r t` is a sawtooth; the ring fill at each pull is `D(t) + c`, and the drift controller keeps the mean fill at the target. A stream never underruns if `target >= block + lookahead + max dip`, where the dip of an event is `mean(D) - D` just before it.
- The fill statistics aggregate the measured dip over a sliding 30 s window. Required headroom = p99.9 dip times 1.25, plus a small look-ahead.
- Ring target (bridge frames) = safety buffer + one master buffer (converted to bridge-rate frames) + jitter headroom (`target_fill_frames`), with `[bridge] ring_target_ms` as an optional floor. The ring capacity is fixed at `bridge-abi::RING_FRAMES` (16384 frames, about 340 ms at 48 kHz).
- `HeadroomPlanner`: starts conservatively (the learned margin for the bridge source if set: `Config::jitter_margin_for`), raises at once when the measurement demands it, lowers slowly (steps of 48 frames, hold 5 s, every 2 s) once statistics are at least 10 s old and the mean fill has reached the target. A learned margin is a floor.
- "Learn (30 s)" resets the statistics, measures for 30 s (needs at least 500 events) and reports the lowest safe headroom in ms; the GUI stores it in `jitter_margin_by_device["vbmatrix"]` (`Config::set_jitter_margin`, the fixed `eq_core::BRIDGE_SOURCE_KEY` of the bridge era).
- Auto safety (`AutoSafety`) adds to `safety_buffer_ms` after underruns, up to 20 ms in total.
- Auto headroom (EQ) is separate: it lowers the preamp so the profile's maximum boost is not above 0 dBFS (`eq-core::dsp::auto_headroom_db`).
- The live latency breakdown (`Caps::latency_breakdown`) is master output latency + mean ring fill + source staleness + the render side. The render side cannot be observed by the router: it is the median measured by `router-cli latency` (stored in `latency_measured.toml` under the fixed source key `"vbmatrix"`, read once per session, used when the ASIO buffer matches and it is at most 30 days old, label `measured <date>`) and otherwise a default estimate (20 ms: a typical shared-mode 10 ms client into "VBMatrix In"; `eq_core::measured`), labelled as an estimate in the GUI, the tray tooltip (`est.`) and the status line.

## 8. The virtual ASIO bridge driver (v2)

`eq-asio-driver` is the virtual ASIO bridge driver **"EQ ASIO Router"** (v2) that VBMatrix loads in one of its ASIO slots. It is a pure bridge: no master hosting, no DSP — it shuttles samples between the ASIO host (VBMatrix) and the shm rings of `bridge-abi`, and the engine process owns the Yamaha and runs the EQ. Same registered name and CLSID as the old v1 DLL, so re-registering overwrites it in place; the engine still refuses to use it as master (`ForbiddenMaster`).

- Channels 8 in / 8 out (the engine pair rides channels 0/1; out 2..8 are discarded after the shuttle copy, in 2..8 return silence), `Int32LSB`, rates 44.1k..192k, buffer sizes 64..2048 (preferred 256, granularity 64).
- **Pacing thread** (the driver has no hardware of its own): high-resolution waitable timer, MMCSS "Pro Audio", QPC-anchored absolute schedule; it fires the host's `bufferSwitch`/`bufferSwitchTimeInfo` at `sample_rate / buffer_frames` and per callback (RT rules) copies the host-written outputs into ring A, pops ring B into the ASIO input buffers (silence on underrun), and updates `driver_beat` and the sample position. The shm mapping is created by the driver and works standalone (audio is dropped and counted when the app is absent); the app attaches and detaches live.
- Builds: `x86_64-pc-windows-msvc` (the default; VBMatrix is x64) and `i686-pc-windows-msvc` (for 32-bit ASIO hosts). `scripts\install-driver.ps1` registers the 64-bit DLL by default (`-Both` adds the 32-bit one), `uninstall-driver.ps1` mirrors that; both are self-elevating and register the DLL in place under `target\`. Its debug log is `%TEMP%\eq_asio_router_driver.log`.
