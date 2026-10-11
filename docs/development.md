# Development

Back to the [documentation index](README.md). Architecture first: [architecture.md](architecture.md).

## 1. Build and test

Windows, MSVC Rust toolchain. Commands from the repository root:

```powershell
cargo build --release                         # whole workspace (x64)
cargo build --release -p eq-asio-app -p router-cli -p wasapi-capture
cargo test --workspace
cargo clippy --workspace --all-targets
```

- The running tray app locks its exe, so a rebuild into the same `target\` fails ("access denied"). Either quit the app first, or build into a separate directory, for example `cargo build --release --target-dir target\alt` (the investigation tools were built this way). The engine tests also need the engine mutex `Local\eq_asio_router_engine`: a running tray app makes some engine tests fail, so quit it before `cargo test`.
- The frozen v1 driver is a 32-bit DLL for the VB-Audio ASIO Bridge: `cargo build -p eq-asio-driver --target i686-pc-windows-msvc --release` (needs `rustup target add i686-pc-windows-msvc`). Not needed for the router.
- Release profile: `opt-level 3`, thin LTO, `panic = "abort"`. A panic therefore ends the process in release builds; the audio path must not panic (see conventions).
- Edition 2021, `windows` crate 0.62, workspace version 0.1.0.

## 2. Crate map

| Crate | Kind | Notes |
|---|---|---|
| `eq-core` | lib | Pure Rust, no `unsafe`: DSP, `Config`, AutoEQ, `Caps`, `StatusBlock` layout |
| `router-core` | lib | Ring, resampler, arrival model, processor/`Handoff`, logger, watchdog, status writer, analyzer, WAV |
| `router-engine` | lib | `Engine`, `pipeline`, `priority`, `cable` (VB cables only), `source` (capture planning, `FallbackState`, source readiness), `tap`, `synth` |
| `wasapi-capture` | lib + bin `capture-probe` | Capture (`CaptureMode::FixedPeriod`, `CaptureOptions`, `MonitorPolicy`), loopback, `RenderKeepAlive`, `CorruptionMonitor`, KS listing, policy (endpoint formats) |
| `asio-abi`, `asio-host` | libs | ASIO ABI written from the public contract; hosting and holder detection |
| `router-cli` | bin | Headless engine and diagnostics ([configuration.md](configuration.md#6-router-cli)) |
| `eq-asio-app` | bin | egui GUI and tray |
| `asio-probe` | bin | Console ASIO host for testing drivers |
| `eq-asio-driver` | cdylib | Frozen v1 virtual driver; do not extend |

## 3. Test strategy

- Unit tests next to the code. The decision logic of the engine is clock-free (`router-engine/src/logic.rs`: change classification, back-off, stall tracking, auto safety), and so are the resampler, the drift controller, the arrival model, the watchdog core and the fault detector, so they run on simulated time.
- `router-core/tests/stream_integrity.rs`: a stream simulator drives the real ring, rate matcher and EQ with synthetic packets (Hi-Fi Cable pairs, 44.1 to 48 kHz, jitter, clock offsets in both directions, stalls and gate re-arm, live target ramp down) and checks the output with the analyzer (THD+N bound, no glitches, no mid-stream discards, no underruns).
- Analyzer (`router-core/src/analyze.rs`): sine fit per segment (amplitude, phase, DC, frequency), glitch count, phase-step tracker for dropped or duplicated samples, plus music-safe statistics. Also available as `router-cli analyze`.
- Alignment checks: `capture-probe check` compares a recording with the generated reference (mismatches, slips) to prove that a capture path is sample-accurate.
- Fake-master seam: `EngineOptions.master_loader` replaces the ASIO driver loader (tests inject "driver busy", "not registered" and so on); `cable_formats` replaces the Windows cable-format access; `synthetic` replaces the WASAPI capture by a generated source; `config_path`, `publish_caps` and `status_mapping` keep tests away from user files and kernel objects. Engine tests are serialized because of the process-wide mutex.
- COM smoke test for the frozen driver: `eq-asio-driver/tests/com_smoke.rs`.
- Pipeline tests: `router-engine/src/pipeline_tests.rs` drive the callback logic with synthetic ring data.
- VAC Line 1 logic is unit-tested without hardware: `eq-core/src/config.rs` (presets, sanitizing, per-device margins, old-file migration), `router-engine/src/source.rs` (`plan_capture`, `FallbackState` one-shot fallback and its reset rules, readiness evaluation), `router-engine/src/engine.rs` (keep-alive period conversion and the not-a-virtual-cable guard), `router-engine/src/cable.rs` (VAC is never touched), `router-engine/src/priority.rs` (MMCSS 1552 counts as OK), `wasapi-capture/src/session.rs` and `monitor.rs` (period planning, monitor policy, corruption rules) and `eq-asio-app/src/logic.rs` (preset texts, period choices, readiness rows). None of these exercise a real VAC driver; that is the pending hardware validation.

## 4. Conventions

- Real-time rules (no allocation, locks, syscalls beyond QPC, no `unwrap`/`expect`/indexing panics on the audio path; logging only through `rlog!`) are listed in [architecture.md](architecture.md#3-real-time-rules). New audio-path code needs a test that runs it without the OS.
- Additive changes: new config and status fields must have defaults so that old `config.toml` files keep loading (`#[serde(default)]`), and the status block layout is versioned (`STATUS_VERSION`, fixed 128 bytes, `_reserved` space). Never reorder existing fields.
- Keep `eq-core` free of platform code and `unsafe`.
- Every `unsafe` block has a `// SAFETY:` comment.
- Frozen things: `eq-asio-driver` and the `EQ ASIO Router` name (refused as master).
- Be conservative with Windows state: the router changes the cable format only when `match_cable_rate` is on, never kills or closes other programs by itself, never writes the Yamaha registry values.

## 5. Safe live testing

- Quit the tray app first: it owns the engine mutex and the Yamaha.
- For the VAC Line 1 path follow the [first-run validation checklist](latency.md#7-first-run-validation-checklist-vac-line-1): quit the tray app, `router-cli holders`, quiet tests, a 10-minute music soak, and the counters to watch (underruns 0, xruns 0, fault Ok, ring mean fill versus target).
- Use quiet test signals: `capture-probe tone` refuses levels above -20 dBFS, the loopback and VAC tools refuse above -40 dBFS; use -45 to -50 dBFS and short runs (8 to 15 s). Aim tones at a virtual cable (`Hi-Fi Cable Input`, `Line 1`), not at a real output.
- `router-cli run --synthetic ...` exercises the whole path with a generated source without touching Windows audio settings; add `--no-match-cable` to avoid format changes.
- Before the first real run of a new build, lower the system and amplifier volume. A fault mutes the output, but a wrong EQ profile can still be loud.
- Do not run measurement tools against the Yamaha while another program holds it.
- Read-only investigation tools (`capture-probe info`, `ks-list`, `list`, `format`) are safe at any time.

## 6. Known limitations and open items

- The render side of the latency (app to capture event) cannot be observed by the router. `router-cli latency` measures it end to end and stores it (`latency_measured.toml`), and the readout uses that when a fresh matching entry exists, else an estimate (20 ms VAC, 100 ms Hi-Fi Cable) labelled as such. The measured render side depends on the client: about 25 ms inj->capture for a typical 10 ms shared client, about 7 ms for an IAudioClient3 client (see [latency.md](latency.md#5b-what-determines-the-render-side-vac-line-1-render-matrix)); real games and players have not been measured.
- VAC Line 1 support (fixed capture period, render keep-alive, source presets, per-device jitter margins, corruption fallback, readiness panel and GUI) is implemented and builds, but its hardware validation is pending: real fixed-period capture, the keep-alive and a live fallback have not been run on hardware. Its latency figures (about 26-34 ms at 5 ms, 1-2 ms more at 6 ms; planner target about 600 frames, start target 784 at 288-frame packets) are model estimates. `capture-probe vac-check` now defaults to 288 frames (240 proved unreliable).
- A fixed-period capture that trips the corruption monitor is reopened once in plain Shared with a warning (`capture_fallback`); it is not retried until the user presses "Retry fixed period" or changes the device or period.
- `router-cli latency` (end-to-end measurement through the Yamaha's Streaming L/R loopback inputs) is implemented and has run live; it needs a quiet Yamaha loopback (a live microphone in the Streaming mix can make the noise floor too high, then the run aborts at the baseline without changing any limit).
- Yamaha latency formula: only validated at 48 kHz for two settings plus the author's third (256, Low Latency, which selects the driver's "test" preset row). Other rates and USB modes are predictions.
- Exclusive capture is disabled because of the Hi-Fi Cable driver.
- Only ASIO sample types that are little-endian (Int16/24/32 LSB, Int32 LSB16/18/20/24, Float32/64 LSB) are supported; MSB types are not.
- Only two output channels are used; no input passthrough.
- No decompiler was available for the reverse-engineering reports; unverified items are listed at the end of each report.

## 7. Licensing

- This repository: MIT (workspace `Cargo.toml`).
- Steinberg ASIO SDK: not included and not needed. `asio-abi` is written from the public interface contract (type layouts and the `IASIO` vtable), and hosting an already installed ASIO driver (registry lookup, COM creation) needs no SDK files. ASIO is a trademark of Steinberg Media Technologies; do not copy SDK headers into the repository, and read the SDK licence before distributing anything built from SDK files.
- Yamaha / Steinberg drivers and software: analyzed read-only for interoperability and latency tuning; no vendor binaries or code are included. See the reverse reports for scope.
- VB-Audio Hi-Fi Cable and VB-Audio Matrix: donationware; the router only captures from the installed endpoint.
- Virtual Audio Cable lite (Eugene Muzychenko): feature-limited freeware for private home use that is not associated with income generation; do not recommend it for commercial use or redistribute it.
- Equalizer APO / Peace: GPL-2.0 (Equalizer APO). The APO investigation only studied the public registration pattern; do not copy their code into this MIT repository. Microsoft's SwapAPO / DelayAPO samples are MS-PL.
- Third-party reference trees that may sit in the working directory (`Windows-driver-samples`, `low-latency-audio`) are not part of the workspace and carry their own licences.
