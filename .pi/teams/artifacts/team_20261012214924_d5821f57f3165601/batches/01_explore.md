=== Task 1: 01_explore (explorer) ===
Status: COMPLETED
Role: explorer
Result artifact: C:\Projects\eq_asio_router\.pi\teams\artifacts\team_20261012214924_d5821f57f3165601\results\01_explore.txt
Log artifact: C:\Projects\eq_asio_router\.pi\teams\artifacts\team_20261012214924_d5821f57f3165601\logs\01_explore.log
Transcript: C:\Projects\eq_asio_router\.pi\teams\artifacts\team_20261012214924_d5821f57f3165601\transcripts\01_explore.attempt-0.jsonl
Usage: {"input":5029,"output":5207,"cacheRead":59968,"cacheWrite":0}
## Handoff

### Goal
Phase A of the cross-platform plan: READ-ONLY Win32 audit of the eq_asio_router workspace plus evidence for the seven design deliverables, so a writer can append `## Phase A results` to `docs/plans/cross-platform.md`.

### Constraints & Preferences
- Read-only: **no file was modified**; only `docs/plans/cross-platform.md` is writable (by the writer step, not me). No branches, no commits.
- Zero overlap with the other crew editing docs/GUI strings — my output is report-only.
- Grounding rule: file:line for every claim; external-library facts (cpal, BlackHole license) flagged for librarian verification instead of asserted.

### Progress
**Done — all 7 deliverables researched (evidence below in Key Decisions / Critical Context):**
1. Win32 dependency map per crate (grep-verified, file:line).
2. Architecture verdict: thin new `portable-engine` crate — **confirmed**, with line-level coupling evidence.
3. Seam trait sketches mapped onto existing types.
4. Output backend decision (repo-grounded requirements + recommendation + OPEN verification points).
5. macOS source options ranked with recommendation.
6. Phase B file/feature layout + ownership table incl. the shared-file collision trap.
7. Validation recipe incl. a hard blocker found: `windows` crate is `#![cfg(windows)]`-empty off-Windows while the workspace has **zero** `cfg(windows)` gates.

**In Progress:** none (exploration complete; write-up not yet done).

**Blocked:** none.

### Key Decisions (with evidence)

**D1 — Win32 dependency map (deliverable 1).** Classification: (a) DSP/portable, (b) platform service needing per-OS equivalent, (c) Windows-only.

| Crate | Win32 usage (file:line) | Class |
|---|---|---|
| **eq-core** | none — deps are serde/toml/thiserror only (`crates/eq-core/Cargo.toml`); `src/status.rs:1-2` is layout-only ("mapping … done by the driver/GUI via Win32") | (a) |
| **router-core** | `status_writer.rs:14-17` (HANDLE, CreateFileMappingW/MapViewOfFile), `:101` CreateFileMappingW; features `Cargo.toml:11-13` — **only module**; `resample.rs/arrival.rs/ring.rs/processor.rs/logger.rs/watchdog.rs/watcher.rs/analyze.rs/band.rs/click.rs/wav.rs/asio_convert.rs` = 0 Win32 (`asio_convert.rs` = pure unsafe over asio-abi POD) | status_writer=(b), rest (a) |
| **router-engine** | `priority.rs:28-32` + whole 360-line file (MMCSS AvSetMm, timeBeginPeriod) → (b); `engine.rs:31-35` imports, `Wake`/`InstanceGuard`/`pump_wait` `:153-224` (MsgWaitForMultipleObjectsEx message pump — required by ASIO STA, doc `:1-2`), HWND control panel `:89`; `pipeline.rs:143-160` qpc/qpc_freq, `:414/:443` MMCSS, `Handler` impl `:634-669`; `synth.rs:68` timeBeginPeriod | mixed (b)+(c) |
| **eq-asio-app** | `tray.rs:257-334`, `autostart.rs:14-16`, `install.rs:4-6`, `single_instance.rs:7-9`, `sys.rs:5-10`, `main.rs:38`; features `Cargo.toml:19-29` | (c) GUI/shell (eframe port = stretch) |
| **router-cli** | `main.rs:25-30` SetConsoleCtrlHandler + `latency.rs:246`; but depends on `wasapi-capture` (`Cargo.toml`: devices/latency subcommands) | console=(b) tiny, latency tooling=(c) |
| **wasapi-capture** | whole crate: features `Cargo.toml:20-35`; MMCSS `session.rs:23/687`, `loopback.rs:552/919`, `impulse.rs:487`; COM `device.rs:79-93`; KS `ks.rs:9-16` | (c) measurement/capture tooling (engine never opens WASAPI — `router-engine/src/lib.rs:8-9`) |
| **bridge-abi** | `mapping.rs:6-14` (CreateFileMappingW), `:119/:143` QPC; doc `mapping.rs:1-2` "only module that touches the OS"; `layout.rs` pure | mapping=(c) for v1 per plan, layout=(a) |
| **asio-host / asio-probe** | `lib.rs:13-18,108,136` COM CoInitializeEx STA + registry, `busy.rs:12-25,218`; `asio-probe/main.rs:13` | (c) |
| **asio-abi** | no windows dep (315-line POD ABI) | (a) code / (c) usage |
| **eq-asio-driver** | `com.rs:8-11` registry/module path, `pacing.rs:26-30` QPC+waitable timer+MMCSS; features `Cargo.toml:14-24` | (c) stays Windows |

**D2 — Architecture: thin new `portable-engine` crate — CONFIRMED (deliverable 2).** Evidence: router-engine = 6,697 lines total; Win32/ASIO-entangled core is exactly the parts that don't port: `priority.rs` (360, 100% MMCSS/timer), `engine.rs` control thread = STA message pump (`pump_wait` called every iteration of `run_session`, `engine.rs:1141-1218`), single-instance mutex `:182-208`, `Session` bound to `AsioDriver` (`:540-605` incl. ASIO teardown in Drop) with ~365 lines of ASIO bring-up in `Session::open` (`:606-970`: rate negotiation `:626-635`, `choose_buffer_size` `:638`, channel sample types `:644-652`, bridge shm attach `:665-690`); `pipeline.rs` render core (`Pipeline` `:342-578`) is portable **except** qpc clock `:143-160`, MMCSS `:414`, and the ASIO adapter (`OutputSet/PtrTable` `:174-238`, `Handler` impl `:583-669` ≈ 90 lines). Reusable/portable as-is: `logic.rs` (521 lines, 0 Win32: back-off, `choose_buffer_size:202`, `burst_estimate`, capacity planning), `status.rs` (302, 0 Win32), `tap.rs` (189), `baseline.rs` (817), `bridge.rs` state machine (687, 0 Win32 imports but its *source role* is Windows-only), `pipeline_tests.rs`. Crucially, "port router-engine behind traits" is blocked by the fact that **no `cfg(windows)` exists anywhere in the workspace** — off-Windows the `windows` crate is an empty stub (`registry …/windows-0.62.2/src/lib.rs:9: #![cfg(windows)]`), so every one of its ~40 `use windows::…` sites in router-engine/router-core/router-cli must first be gated — that gating *is* most of the port. ⇒ Thin new crate; reuse eq-core + router-core + portable logic (move/copy `logic.rs` — see risk below).

**D3 — Seams (deliverable 3), signatures only:**
```rust
// portable-engine/src/seams.rs
pub struct StreamFormat { pub rate: u32, pub period_frames: u32, pub channels: u16 }
pub trait OutputSink {                                   // ← asio_host::Handler (asio-host/lib.rs:141)
    fn negotiate(&mut self, want: &StreamFormat) -> Result<StreamFormat>;   // ← caps probe + choose_buffer_size (engine.rs:638)
    fn start(&mut self, cb: Box<dyn FnMut(OutputBuf<'_>) + Send>) -> Result<()>; // ← AudioCallback::run (pipeline.rs:607)
    fn stop(&mut self);                                  // ← Session teardown (engine.rs:567-604)
}
pub struct OutputBuf<'a> { pub out: [&mut [f32]; 2], pub frames: usize, pub t: i64 } // t ← Clock
pub trait SystemSource {                                 // producer side of ring
    fn stats(&self) -> ArrivalSnapshot;                  // ← router_core::arrival::ArrivalStats (arrival.rs:111)
    fn pump(&mut self, dst: &mut dyn FrameSourceSink) -> Result<u32>; // pushes into ring::Producer
}
pub trait Clock { fn now(&self) -> i64; fn freq(&self) -> i64; }       // ← qpc()/qpc_freq() (pipeline.rs:143/155)
pub trait StatusTransport { fn create(&self) -> Option<Box<dyn StatusPub>>; } // ← StatusWriter (status_writer.rs:80) + StatusBlock (eq-core/status.rs)
pub trait ControlLoop { fn wait(&mut self, d: Duration) -> bool;        // ← pump_wait (engine.rs:210) — condvar/recv_timeout off-Windows
                          fn run(&mut self, cmds: Receiver<Cmd>, sess: &mut dyn Session); } // ← control_loop (engine.rs:1021)
```
Existing types that map directly: `FrameSource` (`resample.rs:19` — already portable & generic), `Handoff<T>` (`processor.rs:121` — snapshot publish, portable), `Live` (`pipeline.rs:48` — RT counters, portable; drop `mmcss_asio` field or cfg it), `Heartbeat/Watchdog` (`watchdog.rs:39/153`), `ConfigWatcher` (`watcher.rs:73`), `rlog` logger (portable), `EqProcessor`/`RateMatcher` (portable), `PipelineSource` (`pipeline.rs:241`) becomes `SystemSource` adapter.

**D4 — Output backend (deliverable 4).** Repo-grounded requirements: event-driven callback at a *negotiated fixed period* chosen from device min/max/preferred (`choose_buffer_size` `logic.rs:202`, used `engine.rs:638`); fixed `frames` per callback (`Pipeline` allocated per `frames`, `render()` handles n≤frames, ring sized `plan_capacity(buffer, …, MAX_PERIOD_FRAMES=4096)` `engine.rs:64`); RT stats to verify period stability (`Live::cb_avg_us/cb_max_us/gap` `pipeline.rs:48-80`, `begin()` gap ratio `:430`); output-only (source is a separate per-OS API ⇒ no duplex sync needed). **Recommendation: cpal for v1 on both OSes** (DECIDED, conditional): it exposes `BufferSize::Fixed` + event-driven callbacks and is the plan-acceptable choice (`cross-platform.md:38`); raw ALSA/CoreAudio is the documented fallback (plan risk `:84`) triggered if Phase B's `Live` stats show period instability (ALSA start_threshold/period granularity, CoreAudio IOProc are the escape hatches). OPEN: verify current cpal release/maintenance + whether `BufferSize::Fixed` is fully honored on both backends (librarian — external docs).

**D5 — macOS source ranking (deliverable 5):**
1. **Integrate an existing open-source loopback as an *installed dependency* (BlackHole)** — mirrors the Windows VBMatrix dependency model; our code merely *captures* from the device, no linking ⇒ its GPL does not touch our MIT workspace (mere aggregation). Fastest path to a true system tap; user installs via Homebrew cask. **Recommended for v1.** OPEN: exact BlackHole license/version + install UX (librarian).
2. **In-house minimal Audio Server Plug-in loopback** — full control, no third-party install, but unavoidable C++/Obj-C, HAL install path + coreaudiod restart + signing (plan `:59-61`); recommended as Phase C refinement if v1 friction matters.
3. **ScreenCaptureKit-only v1** — zero install, but macOS 13+, permission flow, per-app/system-audio scope limits, not a device tap; keep as *runtime fallback* (plan already accepts it `:11`).
   ⇒ DECIDED-for-writeup: 1 for v1 system tap, 3 as fallback, 2 deferred; BlackHole license claim OPEN-verify.

**D6 — Phase B layout (deliverable 6):**
```
crates/portable-engine/           # new crate, pure Rust core + seams
  src/lib.rs        (seams, engine orchestration)   [owner: B0/lead]
  src/backend/mod.rs              (declares mods via #[cfg(all(feature=…, target_os=…))]) [B0]
  src/backend/windows.rs          feature "backend-windows" [B0/lead — keeps Windows gate green]
  src/backend/linux.rs            feature "backend-linux"   [B1]
  src/backend/macos.rs            feature "backend-macos"   [B2]
  src/backend/mock.rs             feature "mock" (any OS — enables Windows-runnable unit tests) [B0]
crates/portable-cli/src/main.rs   (headless CLI; cfg-gated ctrl handler)  [B0 owns main, B1/B2 add subcommand-free code only]
```
- Cargo.toml must be **pre-seeded by B0** with `[target.'cfg(target_os = "linux")'.dependencies]` / `[…="macos"…]` sections, because a shared Cargo.toml otherwise collides.
- B0 (pre-wave, single owner) must also: cfg-gate `router-core/src/status_writer.rs` Win32 import, and either move `router-engine/src/logic.rs` → router-core or accept duplication (OPEN decision). No shared-file edits allowed during the B1/B2 wave (plan `:74-76`).
- Ownership: B1 = `backend/linux.rs` + linux dep additions *in its own section*; B2 = `backend/macos.rs`; neither touches `lib.rs`, `Cargo.toml` (except appending to own pre-made section), or router-core. OVERLAP RISK called out for the leader: these B0 steps are cross-cutting and must be sequenced before the parallel wave.

**D7 — Validation recipe (deliverable 7), executable from this machine:**
- Installed today: only `i686-pc-windows-msvc`, `x86_64-pc-windows-msvc` (rustup). Add: `rustup target add x86_64-unknown-linux-gnu aarch64-apple-darwin`.
- Commands: `cargo check -p portable-engine -p portable-cli --target x86_64-unknown-linux-gnu` / `… --target aarch64-apple-darwin`; Windows full gate `cargo check --workspace && cargo test --workspace` (CI does `cargo build/test --workspace --release` on windows-latest — `.github/workflows/ci.yml:16-22`).
- **Hard blocker found:** cross-checking the *existing* workspace cannot work until gating lands — the `windows` crate is `#![cfg(windows)]` (windows-0.62.2/src/lib.rs:9) so on unix/mac targets it compiles empty and the workspace's unconditional `use windows::…` fails (0 `cfg(windows)` hits repo-wide). Therefore cross-checks must be scoped `-p` to portable crates. Expected probe: `-p eq-core` passes, `-p router-core` fails at `status_writer.rs` until B0 gates it.
- Cannot be tested without OS hardware: ALSA/PipeWire device enumeration & xruns, coreaudiod/ASP, real period stability, end-to-end latency, hotplug. CAN be tested on Windows: portable unit tests (logic/resample/ring/EQ/config) via the `mock` backend + `cargo test -p …`.

### Next Steps
1. Writer step: append `## Phase A results` to `docs/plans/cross-platform.md` using the tables/decisions above (DECIDED: thin crate, cpal-v1-conditional, BlackHole-for-v1, layout; OPEN: BlackHole license verify, cpal Fixed-buffer honoring, logic.rs relocation, B0 sequencing).
2. Verify the D7 blocker empirically once targets are added: `cargo check -p router-core --target x86_64-unknown-linux-gnu` (expect fail at status_writer.rs).
3. Leader: schedule a **B0 pre-wave task** (single owner) for cfg-gating + crate scaffold before launching B1/B2 in parallel.

### Critical Context
- **Changed files: none** (read-only honored). Read/grepped: `docs/plans/cross-platform.md`, all `crates/*/Cargo.toml`, ~40 source files, `.github/workflows/ci.yml`, cached `windows-0.62.2/src/lib.rs`, `Cargo.lock` (no cpal/alsa/coreaudio/pipewire present today; only `libc`).
- **Verification evidence:** greps `windows::|Win32_` (400-match full sweep), `AvSetMm|CreateFileMapping|MMCSS|QueryPerformance|CoInitialize`, `cfg(windows)|target.gated deps` (0 hits), per-file hit counts via bash loops, `rustup target list --installed`, `git log -1` = `0728f2c` (matches packet; `.gitignore` modified by another worker — untouched by me).
- Risks: (1) someone may assume `cargo check --target` on the whole workspace works — it can't (windows stub); (2) B0 shared-file edits (router-core, Cargo.toml, lib.rs) are the true collision points — assign one owner; (3) external facts (cpal status, BlackHole GPL version, ScreenCaptureKit API level) unverified from this repo.

### Provenance
session=crew-team202610122149 model=xiaomi-token-plan-ams/mimo-v2.6-flash reasoning=medium

```
EXPLORER_RESULT: Full Phase A audit complete: Win32 map per crate, thin portable-engine decision confirmed (with the zero-cfg(windows)/empty-windows-stub cross-check blocker found), trait seams, cpal+BlackHole recommendations, collision-free Phase B layout, and executable cross-target validation recipe.
FILES:
  - docs/plans/cross-platform.md:1 — approved plan; deliverable slots into "## Phase A" (lines 31-44), overlap rule :74-76, cpal risk :84
  - crates/router-engine/src/engine.rs:210 — pump_wait STA message pump; :153-208 Wake/InstanceGuard; :606-970 ASIO Session::open; :1141-1218 control loop
  - crates/router-engine/src/priority.rs:28 — MMCSS/timeBeginPeriod, 360 lines 100% Win32 (class b)
  - crates/router-engine/src/pipeline.rs:143 — qpc clock; :174-238 OutputSet/PtrTable; :583-669 Handler impl; :48 Live stats
  - crates/router-engine/src/logic.rs:202 — choose_buffer_size; 521 lines 0 Win32 (portable, relocation OPEN)
  - crates/router-core/src/status_writer.rs:14 — only Win32 module in router-core (B0 gating target)
  - crates/router-core/src/resample.rs:19 — FrameSource trait seam; arrival.rs:111 ArrivalStats; processor.rs:121 Handoff
  - crates/asio-host/src/lib.rs:141 — Handler trait (OutputSink seam origin); :136 CoInitializeEx STA
  - crates/eq-core/src/status.rs:1 — layout-only status block, portable (a)
  - crates/bridge-abi/src/mapping.rs:6 — only OS module of bridge-abi; layout.rs portable
  - C:/Users/Rexami/.cargo/registry/src/.../windows-0.62.2/src/lib.rs:9 — #![cfg(windows)] ⇒ empty stub off-Windows (cross-check blocker)
  - .github/workflows/ci.yml:16 — CI runs cargo build/test --workspace on windows-latest only
ANSWER: (1) Win32 map produced: eq-core/asio-abi pure; router-core all-but-status_writer portable; router-engine entangled in priority/engine/pipeline-adapter; app/driver/asio/wasapi/bridge-mapping = Windows-only. (2) Thin portable-engine crate CONFIRMED (6.7k-line router-engine, control+session layers Windows-bound, pure logic only ~1.1k lines). (3) Seams sketched mapping FrameSource/ArrivalStats/Handoff/StatusWriter/Handler/Live. (4) cpal for v1 output both OSes, conditional on period-stability check; raw ALSA/CoreAudio fallback. (5) macOS: require installed BlackHole (no linking ⇒ GPL-safe) > in-house ASP > ScreenCaptureKit fallback. (6) Layout: portable-engine/src/backend/{windows,linux,macos,mock}.rs + B0 pre-wave owns shared files. (7) rustup add 2 targets + scoped cargo check -p … --target; whole-workspace cross-check impossible until cfg-gating lands.
UNCERTAIN: (a) empirical proof of cross-target compile failure — needs `rustup target add` (mutation I was forbidden to perform); high confidence from windows-stub + zero cfg hits. (b) External facts unverified: cpal maintenance/current version & BufferSize::Fixed honoring on CoreAudio/ALSA; BlackHole exact GPL version; ScreenCaptureKit API/permission levels — route to librarian.
ROUTING: analyst (compose ## Phase A results write-up into docs/plans/cross-platform.md) + librarian for cpal/BlackHole/ScreenCaptureKit external verification; leader must schedule the B0 pre-wave before B1/B2.
```