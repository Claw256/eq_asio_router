# Plan: Cross-platform EQ engine (macOS / Linux)

Status: drafted, **launch gated on completion of `vbmatrix-bridge.md` (M3a → M3b)**.
Decisions confirmed by the user:

1. **Deliverable: portable engine app** — no virtual device, no plug-in formats.
   Apps feed the engine; the engine EQs and plays out through the Yamaha (or any
   device) at the lowest latency the OS allows. Windows keeps its VBMatrix/ASIO chain
   untouched.
2. **Sources:** Linux = **PipeWire/Pulse monitor source** (system audio, the PipeWire
   analog of the VBMatrix source role). macOS = **driver-based system tap** (a
   loopback/virtual-device driver installed for capture; ScreenCaptureKit fallback is
   acceptable for app-specific capture on 13+).
3. **Launch shape: parallel wave, both OSes**, after the Windows plan is complete.

## What is portable today

| Piece | Portable? |
|---|---|
| `eq-core` (biquads, limiter, AutoEQ import, headroom) | yes — pure Rust |
| `router-core` (Kaiser-sinc resampler, PI drift controller, SPSC ring, logger) | mostly — audit MMCSS/Win32 bits and `cfg` them out |
| `bridge-abi` | logic yes; mapping layer is Windows-only (not needed on these OSes for v1) |
| `eq-asio-driver` | **no — stays Windows** (ASIO is a Windows driver model) |
| `asio-host` / Yamaha output | replaced per-OS: Core Audio (macOS), ALSA/UAC2 (Linux) — UR22C is class-compliant on both |

## Phase A — audit + seams (single agent, blocking)

- Map every `windows`-crate / Win32 dependency in `eq-core`, `router-core`,
  `router-engine`, `eq-asio-app`, `router-cli` (file × symbol table).
- Decide and document: **port `router-engine` behind traits** vs **write a new thin
  `portable-engine` crate reusing `eq-core` + `router-core` primitives**. Lean:
  thin new crate (the Windows engine carries COM STA, MMCSS, status-block, ASIO
  control machinery that has no meaning elsewhere); confirm with evidence.
- Define the seams as Rust traits: `OutputSink` (master device: start/stop/RT
  callback with negotiated rate/buffer), `SystemSource` (source frames + arrival
  stats), `Clock`, `StatusTransport`, `ControlLoop`. Windows keeps a `asio-host`
  implementation of `OutputSink` so both worlds share the engine core.
- Decide output backend per OS: raw Core Audio / ALSA vs **cpal** (cpal wraps both
  and is maintained; decide per evidence, cpal acceptable for v1).
- Decide GUI scope: v1 ships a **headless CLI** (router-cli-style) + shared config;
  egui (`eq-asio-app`) port is a stretch goal (eframe is cross-platform).
- Validation available in this environment: `rustup target add
  x86_64-unknown-linux-gnu aarch64-apple-darwin` then **`cargo check --target …`**
  (compile validation only; tests must stay runnable on Windows via `cargo test`).

## Phase B — parallel OS agents (after Phase A lands)

Agent B1 **Linux**:
- Output: ALSA sink to the UR22C (UAC2 class-compliant) at configurable period;
  PipeWire/Pulse **monitor source** for system audio (pipewire crate / libpulse —
  choose in Phase A), with rate matching through the existing drift controller.
- Headless CLI wired to the portable engine; config `[bridge]` section NOT reused —
  a new portable config section (source selector, buffer/period, EQ profile path).
- Gate: `cargo check --target x86_64-unknown-linux-gnu` clean; portable unit tests
  green on Windows.

Agent B2 **macOS**:
- Output: Core Audio to the UR22C.
- Source: system-tap **driver** = a minimal **Audio Server Plug-in (loopback)**
  component (C++/Objective-C is unavoidable for the ASP itself; installed under
  `~/Library/Audio/Plug-Ins/HAL`, coreaudiod restart) OR ship instructions +
  integrate an existing open-source loopback (BlackHole) — choose in Phase A with
  licensing noted (ASIO trademark/licensing is irrelevant here — no ASIO on macOS).
  ScreenCaptureKit fallback for per-app capture on 13+.
- Gate: `cargo check --target aarch64-apple-darwin` clean; portable unit tests green
  on Windows.

Overlap rule: B1 and B2 edit only their own backend modules (Phase A defines the
file layout: e.g. `portable-engine/src/backend/{linux,macos,windows}.rs` behind
`cfg`), plus separate Cargo features — no shared-file edits during the parallel wave.

## Phase C — synthesis (after B1+B2)

- Wire config/CLI parity, latency readout labels, docs (`docs/cross-platform.md`),
  CI matrix (windows-latest + ubuntu-latest + macos-latest `cargo check`/test),
  final gates, and the hardware validation checklist for the user (Linux + macOS
  with the UR22C).

## Risks

| Risk | Mitigation |
|---|---|
| macOS ASP loopback needs native code + signing/restart quirks | Phase A decision: own minimal ASP vs BlackHole integration vs ScreenCaptureKit-only v1 |
| cpal hides low-level period control needed for latency | Phase A evidence check; fall back to raw Core Audio/ALSA IOProc callbacks |
| Porting drags Windows regressions | Windows remains gated by the existing workspace `cargo test` on every phase |
| Two OS agents colliding on shared files | Phase A fixes the cfg module layout first; B1/B2 own disjoint files |

## Phase A results

*(audit at commit `0728f2c`; produced by 01_explore, Phase A single agent.)*

### A1. Win32 dependency map

Classification: **(a)** DSP/portable · **(b)** platform service needing a per-OS equivalent · **(c)** Windows-specific, stays Windows-only.

| Crate | Win32 surface (file:line) | Class |
|---|---|---|
| `eq-core` | **zero** `windows`/Win32 refs in `crates/eq-core/src/**` (grep `windows::|Win32_|AvSetMm|CreateFileMapping|QueryPerformance|CoInitialize` → 0 hits) | (a) all |
| `eq-core` config (`src/config.rs`) | pure serde/toml + `SystemTime` mtime polling (`config.rs:1-13`) | (a) |
| `eq-core` status (`src/status.rs`) | POD `StatusBlock` layout + magic/version consts only (`status.rs:1-30`); name const `"Local\eq_asio_router_status"` (`status.rs:7`) is a string, not an API call | (a) |
| `router-core` | only `status_writer.rs`: `windows::core::PCWSTR`/HANDLE imports (`status_writer.rs:14-17`), `CreateFileMappingW` (`:101`), `OpenFileMappingW` (`:264`), `MapViewOfFile`/`UnmapViewOfFile` | (b) → `StatusTransport` seam |
| `router-core` (rest) | `resample.rs` `FrameSource:19`, `RateMatcher:482`, `DriftController:264`, `StartupGate:373`; `arrival.rs` `ArrivalStats:111`, `ArrivalTracker:236` (clock-free); `processor.rs` `Handoff:121`/`EqProcessor`; `ring.rs`, `logger.rs`, `watchdog.rs`, `analyze.rs`, `click.rs`, `band.rs`, `wav.rs`; `asio_convert.rs` (pure `AsioSampleType` conversion; `asio-abi` has **no** windows dep) | (a) |
| `router-engine` | `engine.rs:31-35` Win32 imports; `CreateEventW` (`engine.rs:158`), `CreateMutexW` single-instance (`:188`), STA message pump `PeekMessageW/MsgWaitForMultipleObjectsEx` (`engine.rs:214-215`); `pipeline.rs:144,155` `QueryPerformanceCounter/Frequency` (`qpc():143`, `qpc_freq():154`); `priority.rs` (22 hits): `AvSetMmThreadCharacteristicsW` (`:201,:252`), `AvSetMmThreadPriority` (`:256`), `timeBeginPeriod` (`:30`), EcoQoS hardening; `synth.rs:68` `timeBeginPeriod` | (b) clock/MMCSS/timer-resolution = per-OS service; (c) STA pump + Win32 mutex/event lifecycle |
| `router-engine` (portable logic) | `logic.rs` — 521 lines "pure, clock-free decision logic" (`logic.rs:1`), **0** windows refs (`Backoff:71`, `StallTracker:147`, `classify_change:32`, `choose_buffer_size:202`, `AutoSafety:274`) | (a) |
| `eq-asio-app` (shell) | `tray.rs:257-259,325-332` (Win32 tray/messages/GDI), `autostart.rs:14-17,170` (Registry run-key), `install.rs:4-6` `ShellExecuteW`, `sys.rs:5-10` (ToolHelp + Registry), `single_instance.rs:7-9` (`CreateMutexW`/`CreateEventW`), `main.rs:38` `AttachConsole` | (c) |
| `eq-asio-app` (UI logic) | `host.rs` `EngineHost` worker thread (`host.rs:1-45`), `app.rs`, `logic.rs`, `panels/`, `settings.rs`, `plot.rs`, `debounce.rs` — 0 windows refs (eframe is cross-platform) | (a) |
| `router-cli` | `main.rs:25-26`, `latency.rs:246` `SetConsoleCtrlHandler` only; rest depends on `wasapi-capture` measurement tooling | (c) |
| `wasapi-capture` | whole crate COM/WASAPI: `session.rs` (21 hits, `IAudioClient3:467`), `loopback.rs` (27), `impulse.rs` (16), `device.rs` (14, `CoInitializeEx:79`), `process_loopback.rs` (11), `policy.rs:13-16` (COM `CoCreateInstance`), `ks.rs:8-16` (`CreateFileW`+`DeviceIoControl`), `tone.rs` | (c) — replaced per-OS |
| `bridge-abi` | `layout.rs` pure POD + SPSC rings (crate doc `Cargo.toml` note); `mapping.rs:6-14,143,169` `CreateFileMappingW`/`OpenFileMappingW`/`QueryPerformanceFrequency` | (a) layout/rings; (b) mapping layer (per-OS shm if ever needed; not needed v1 per plan) |
| `asio-host` | `lib.rs:13-18` COM (`CoGetClassObject`, `CoInitializeEx`), Registry ASIO enumeration (`lib.rs:92-108`); `busy.rs:12-25` ToolHelp/window enumeration | (c) — replaced by per-OS output |
| `asio-probe` | `main.rs:13` `HWND` for ASIO panel | (c) diagnostic |
| `eq-asio-driver` | `com.rs` (COM registration/DLL exports), `pacing.rs:26-30,251` (MMCSS + waitable timer), `driver.rs` ASIO v2 driver | (c) — stays Windows (ASIO driver model) |

Note: **no `cfg(windows)`/`cfg(target_os)` exists anywhere in `crates/**` today** — all Win32 code is unconditional. Every portability boundary must be introduced fresh in Phase B.

### A2. Architecture decision — DECIDED: thin new `portable-engine` crate (plan's lean CONFIRMED)

Evidence:
- `router-engine`'s raw Win32 call count is small (`engine.rs` 6, `pipeline.rs` 7, `priority.rs` 22, `synth.rs` 2 matched refs) but its coupling is **structural**, not just calls: `engine.rs` is a Win32 STA control thread with a `PeekMessageW` message pump (`engine.rs:214-215`) and owns an `AsioDriver` (14 `asio_host` refs in `engine.rs`, 6 in `status.rs` incl. `map_open_error` at `status.rs:240`); `pipeline.rs` implements `asio_host::Handler` (`pipeline.rs:14`, `buffer_switch` contract `asio-host/src/lib.rs:143`) and writes ASIO buffer halves through `PtrTable`/`OutputSet` (`pipeline.rs:174-231`); `bridge.rs` is the VBMatrix shm source — the Windows source that does not exist on the target OSes at all.
- What is reusable **untouched**: all of `eq-core` (0 Win32 refs) and `router-core` except `status_writer.rs` — Kaiser-sinc `RateMatcher` (`resample.rs:482`), `DriftController` (`resample.rs:264`), `ArrivalTracker`/`ArrivalStats` (`arrival.rs:236/:111`), `Handoff`/`EqProcessor` (`processor.rs:121`), SPSC `ring`, `logger`, `watchdog`, `analyze`.
- Portable but entangled: the RT path in `pipeline.rs` (`Pipeline::render` `pipeline.rs:441`, explicitly "clock-free, ticks passed in" `pipeline.rs:5`) is a good *template* (~500 lines of RT bookkeeping) but is welded to `asio_host` and `CallbackMmcss` (`pipeline.rs:28,412`). Extracting it in place would mean surgery on the 1568-line `engine.rs` that gates the green Windows workspace — the exact regression the plan's risk table forbids.
- Verdict: **port `router-engine` behind traits = refit; new thin crate = adopt + adapt.** The new crate copies/adapts the ~500-line RT core and the 521-line pure `logic.rs` decision helpers, depends only on `eq-core` + `router-core`, and leaves every existing crate byte-identical → Windows gates stay green by construction. (Phase C may later add a Windows `OutputSink` over `asio-host` so both worlds share the engine core; and/or move `router-engine/logic.rs` into `router-core` — deferred, not blocking.)

### A3. Seams — trait sketches (signatures only)

Type mapping: `FrameSource` = `router-core/src/resample.rs:19` · `ArrivalStats`/`ArrivalTracker` = `router-core/src/arrival.rs:111/:236` · `Handoff` = `router-core/src/processor.rs:121` · `StatusWriter` = `router-core/src/status_writer.rs:80` · `RateMatcher` = `resample.rs:482` · `EqProcessor` = `processor.rs` · QPC = `router-engine/src/pipeline.rs:143/:154` · `Handler::buffer_switch` = `asio-host/src/lib.rs:143` · `BridgeSource` (a `FrameSource`) = `router-engine/src/bridge.rs:136` · `FillWindow` = `bridge.rs:253`.

```rust
/// Negotiated stream shape (echoes bridge negotiated() at router-engine/src/bridge.rs:99
/// and logic::choose_buffer_size at router-engine/src/logic.rs:202).
pub struct StreamRequest { pub rate_hz: u32, pub buffer_frames: u32, pub channels: u16 }
pub struct StreamSpec    { pub rate_hz: u32, pub buffer_frames: u32, pub channels: u16 }

/// Master output device. RT side = OutputCallback::render, called per period,
/// allocation-free (same rules as Pipeline::render, router-engine/src/pipeline.rs:3-5).
pub trait OutputSink {
    type Callback: OutputCallback;
    fn negotiate(&mut self, want: &StreamRequest) -> Result<StreamSpec, SinkError>;
    fn start(&mut self, cb: Self::Callback) -> Result<(), SinkError>;
    fn stop(&mut self) -> Result<(), SinkError>;
    fn spec(&self) -> &StreamSpec;
}
pub trait OutputCallback: Send + 'static {
    fn render(&mut self, out: &mut [&mut [f32]], now_ticks: i64);
    fn on_xrun(&mut self) {}
}
// Windows impl wraps asio_host::AsioDriver + Handler::buffer_switch (asio-host/src/lib.rs:141-146);
// the portable RT core IS the callback body (port of router-engine/src/pipeline.rs Pipeline::render:441).
/// System-audio source: producer side fills the ring; consumer side is any FrameSource
/// (router-core/src/resample.rs:19) — today BridgeSource (bridge.rs:136) or PipelineSource (pipeline.rs:241).
pub trait SystemSource {
    fn negotiated_rate(&self) -> u32;
    fn attach(&mut self, ring: Producer) -> Result<(), SourceError>; // router-core::ring::Producer
    fn arrival(&mut self) -> &mut ArrivalTracker;                   // router-core/src/arrival.rs:236
    fn fill_snapshot(&mut self) -> ArrivalStats;                    // router-core/src/arrival.rs:111
    fn heartbeat(&self) -> &Heartbeat;                              // router-core/src/watchdog.rs
}

/// QPC on Windows (router-engine/src/pipeline.rs:143/154); clock_gettime(CLOCK_MONOTONIC)
/// on Linux; mach_absolute_time on macOS. Everything that consumes it is clock-free today.
pub trait Clock: Send + Sync {
    fn now_ticks(&self) -> i64;
    fn ticks_per_second(&self) -> i64;
    fn sleep_until(&self, ticks: i64);
}

/// Status publication. Windows impl = router-core/src/status_writer.rs:80 (shm StatusWriter,
/// eq-core/src/status.rs StatusBlock); portable impls = NullTransport / Unix-socket or file.
pub trait StatusTransport: Send + Sync {
    fn init(&self, info: &StaticInfo);                       // status_writer.rs:29,133
    fn set_running(&self, on: bool);                         // :176
    fn set_xruns(&self, n: u64);                             // :179
    fn publish_block(&self, r: &BlockReport);                // :43,213
    fn set_bridge(&self, r: &BridgeReport);                  // :60,197
}

/// Supervision: config-diff classification, restart back-off, stall tracking, auto safety.
/// Direct port of router-engine/src/logic.rs (classify_change:32, Backoff:71, StallTracker:147,
/// AutoSafety:274, plan_target:190).
pub trait ControlLoop {
    fn on_config(&mut self, old: &Config, new: &Config, effective_rate: u32) -> ConfigChange;
    fn on_tick(&mut self, now_ticks: i64, live: &Live) -> ControlAction;
    fn next_retry(&self) -> Option<i64>;
}
```

### A4. Output backend per OS — DECIDED: raw Core Audio (macOS) + raw ALSA (Linux) via minimal hand-written FFI; cpal rejected for v1

Evidence:
- The engine *requires* explicit negotiated buffer sizes: `logic::choose_buffer_size` (`router-engine/src/logic.rs:202`), `plan_target`/`plan_capacity` (`:190/:196`) size rings from period × rate; the source negotiates rates (`bridge.rs:99`). cpal only lets the app *request* `BufferSize::Fixed`; the host may substitute its own — the engine's ring arithmetic would lose its ground truth. cpal also gives no producer-side arrival timestamps, which `ArrivalTracker` (`router-core/src/arrival.rs:236`) needs on the source side.
- Event-driven callbacks at negotiated size exist in both raw APIs (Core Audio IOProc / `AudioDeviceCreateIOProcID` + `kAudioDevicePropertyBufferFrameSize`; ALSA `snd_pcm_hw_params` period/buffer + `snd_pcm_wait`/poll fd) — equivalent to the current ASIO `buffer_switch` contract (`asio-host/src/lib.rs:143`).
- Practical build evidence: `alsa-sys` (cpal's Linux backend) and similar `*-sys` crates probe pkg-config/system SDKs in `build.rs`, which breaks `cargo check --target x86_64-unknown-linux-gnu` from this Windows machine (see A7). Hand-written `#[link]` extern blocks type-check cross with no host SDK because `cargo check` does not link.
- Licensing/maintenance (EXTERNAL — needs librarian confirmation): cpal is dual MIT/Apache-2.0 and maintained; workspace license is MIT (`Cargo.toml [workspace.package]`) — no blocker either way. Raw ALSA is LGPL (dynamic linking fine); Core Audio framework usage is unencumbered.
- OPEN for Phase B (B1/B2): if raw ALSA/Core Audio proves costly, fall back to cpal behind `OutputSink` — the seam makes this reversible.

### A5. macOS source strategy — ranked with recommendation

| Rank | Option | Pros | Cons |
|---|---|---|---|
| 1 (v1 recommendation) | **BlackHole as user-installed system tap + ScreenCaptureKit per-app fallback (13+)** | driver-based tap matches the confirmed plan decision; zero native code in our tree; SCK fallback already sanctioned by plan | external dependency install (documented in setup guide); **licensing: GPLv3 (EXTERNAL — librarian must confirm)**; we must NOT bundle/link it — ship install instructions only, which keeps our MIT distribution clean |
| 2 (release path) | **Minimal in-house Audio Server Plug-in loopback** (`~/Library/Audio/Plug-Ins/HAL`, coreaudiod restart) | MIT-licensable, self-contained deliverable, full control of tap format/rate | C++/Obj-C unavoidable; signing/notarization + coreaudiod restart quirks (plan risk table); weeks of work; needs a Mac to develop/verify — not doable from this Windows machine |
| 3 | **ScreenCaptureKit-only v1** | no driver at all | violates the confirmed "driver-based system tap" decision for full system audio; 13+ only; capture permission (Screen Recording) UX; weaker latency story |

DECIDED (recommended): v1 = rank 1 (BlackHole documented + SCK fallback), Phase C hardens toward rank 2 for a self-contained release. OPEN: BlackHole license terms and redistribution wording (librarian); final call on ASP timing is a Phase C product decision.

### A6. Phase B file/feature layout — collision-free ownership

New crate `crates/portable-engine` (added to `[workspace.members]` — **root `Cargo.toml` owned by Phase A/C only**):

```
crates/portable-engine/
  Cargo.toml                 # Phase A/C. Target-specific deps only:
                             #   [target.'cfg(target_os="linux")'.dependencies]
                             #   [target.'cfg(target_os="macos")'.dependencies]
  src/lib.rs                 # Phase A/C (shared) — module wiring, frozen after Phase A
  src/seams.rs               # Phase A (shared) — the A3 traits; FROZEN, change needs leader
  src/pipeline.rs            # Phase A (shared) — portable RT core (port of router-engine/pipeline.rs)
  src/session.rs             # Phase A/C (shared) — supervision (port of router-engine/logic.rs)
  src/status.rs              # Phase A (shared) — StatusTransport: Null + in-proc impls
  src/backend/mod.rs         # Phase A (shared) — cfg dispatch only, frozen
  src/backend/linux.rs       # ALSA OutputSink ................... B1 OWNS
  src/backend/pipewire.rs    # PipeWire/Pulse monitor SystemSource B1 OWNS
  src/backend/macos.rs       # Core Audio OutputSink ............. B2 OWNS
  src/backend/macos_tap.rs   # BlackHole/SCK SystemSource ......... B2 OWNS
  src/backend/windows.rs     # asio-host OutputSink .............. Phase C (stub `unimplemented!` in Phase A)
  src/bin/portable-cli.rs    # Phase A skeleton + Phase C wiring (headless CLI per plan)
```

Feature names: `backend-linux`, `backend-macos`, `backend-windows` (for CI slicing; `cfg(target_os=…)` gates the modules regardless). Rules for the parallel wave: B1 edits **only** `backend/linux.rs`, `backend/pipewire.rs` and `tests/linux_*.rs`; B2 edits **only** `backend/macos.rs`, `backend/macos_tap.rs` and `tests/macos_*.rs`; any seam/shared-file change is a leader-mediated request applied by Phase C. No new workspace members, no edits to `router-core`/`eq-core`/`router-engine` during the wave.

### A7. Cross-target validation recipe (executable from this Windows machine)

Machine facts: `rustup 1.29.1`, `rustc 1.99.0`; installed targets today are only `x86_64-pc-windows-msvc`, `i686-pc-windows-msvc`. WSL Ubuntu-26.04 is present but has **no Rust installed**.

```powershell
rustup target add x86_64-unknown-linux-gnu aarch64-apple-darwin
cargo check -p portable-engine --target x86_64-unknown-linux-gnu   # B1 gate
cargo check -p portable-engine --target aarch64-apple-darwin       # B2 gate
cargo check -p portable-engine --target x86_64-pc-windows-msvc     # windows stub stays green
cargo test --workspace                                             # regression gate (must stay green every phase)
```

Why this works: `cargo check` does not link — no cross linker, Linux sysroot or macOS SDK needed — **provided** the backend modules use hand-written `#[link]` extern FFI and no target crate runs a `build.rs` that probes pkg-config/system SDKs (this is the A4 decision; `alsa-sys`/`coreaudio-sys` would break this). Proc macros and build scripts compile for the host (Windows), which is fine.

Optional deeper validation: install rustup inside WSL (`wsl -e bash -lc "curl … | sh"`) and run `cargo check`/`cargo test` natively on Linux (build scripts then run correctly); not required for the Phase B gates.

Cannot be tested without OS hardware: ALSA/PipeWire runtime behavior, PipeWire monitor-source arrival patterns (arrival model must be re-measured on Linux — `ArrivalTracker` feeds from the live source), Core Audio tap installation (coreaudiod restart, signing), UR22C UAC2 behavior, RT scheduling effects (MMCSS ↔ `SCHED_FIFO`/QoS equivalence), real rate drift. All of these get a hardware validation checklist in Phase C. Pure logic (`eq-core`, `router-core`, portable pipeline against fake sinks) is fully covered by `cargo test` on Windows.

### A8. Addendum — source-side bindings (PipeWire / Pulse)

The plan's Phase B text says "pipewire crate / libpulse — choose in Phase A"; that
choice was not recorded in A1–A7. Resolved here:

- **DECIDED** (gate-forced, same rationale as A4): **no `-sys`/wrapper crates for the
  source side** — no `pipewire-sys`, `libpulse-sys`, `pipewire` or `libpulse-binding`
  dependencies. Their `build.rs` scripts probe pkg-config/system libraries, which
  would break the A7 cross-`cargo check` gate from this Windows machine. Source-side
  bindings are hand-written `#[link]` extern blocks over the system libraries
  (`libpipewire-0.3` / `libpulse`), type-checked cross with no host SDK.
- **OPEN for B1** (Linux-only detail, not verifiable from this machine): which API
  level to target — PipeWire native stream API vs the PulseAudio simple/`parec`
  monitor path. B1 chooses under the constraint above and records the rationale in
  `backend/pipewire.rs`; changing the constraint (adopting a sys crate) is a
  leader-mediated request that also re-opens the A7 gate.

### Phase A decision summary

- **DECIDED** A2: thin new `portable-engine` crate (confirmed with evidence).
- **DECIDED** A3: seams as sketched; `StatusWriter`/`FrameSource`/`ArrivalTracker`/`Handoff` map 1:1.
- **DECIDED** A4: raw ALSA + raw Core Audio (hand-written FFI), cpal fallback option retained behind `OutputSink`.
- **DECIDED** A5: v1 macOS source = BlackHole (user-installed) + ScreenCaptureKit fallback; in-house ASP is the release-path option.
- **DECIDED** A6: file layout + ownership above; `backend/*.rs` split gives B1/B2 disjoint files.
- **OPEN** (librarian): BlackHole GPLv3 terms/wording; cpal license/maintenance status confirmation.
- **DECIDED** A8: source-side bindings are hand-written `#[link]` FFI (no `-sys`
  crates) — gate-forced by A7; API-level PipeWire-vs-Pulse choice is OPEN for B1
  under that constraint.
- **OPEN** (Phase B): per-OS arrival-pattern measurement; cpal fallback call; ALSA period policy defaults.
