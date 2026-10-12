# Plan: VBMatrix bridge refactor (virtual ASIO driver v2)

Goal: replace the WASAPI capture source (Hi-Fi Cable / VAC) with a low-latency ASIO
path through VBMatrix. EQ ASIO Router gains a **virtual ASIO driver** ("EQ ASIO
Router", v2, written in Rust) that VBMatrix loads in its ASIO slot. VBMatrix does all
routing/patching; the router keeps the **Yamaha UR22C as master** and keeps running the EQ.

Decisions (confirmed):

1. **Bridge DLL + app engine** — the DLL only shuttles samples through shared memory;
   the EQ Router process owns the Yamaha and runs the EQ (as today).
2. **Source = VBMatrix inputs directly** — Windows default playback becomes VBMatrix's
   "In" endpoints; the Hi-Fi Cable / VAC chain is removed.
3. **Post-EQ loopback** — the driver's ASIO *input* side (what VBMatrix reads back)
   carries the processed audio, so VBMatrix can patch it onward.
4. **Remove the WASAPI path** — no fallback source.

```
 VBMatrix process (ASIO host)                EQ Router process
┌─────────────────────────────┐  shm ring A  ┌───────────────────────────────────────┐
│ apps -> VBMatrix In (WDM)   │  (source)    │  ring A -> RateMatcher -> EqProcessor │
│ matrix patching             │────────────►│    -> asio_convert -> Yamaha outputs  │
│   -> ASIO slot: our DLL     │  shm ring B  │    (master: Yamaha Steinberg USB ASIO)│
│   reads outputs / feeds     │◄────────────│  loopback feeder: EQ tap -> resampler │
│   inputs (post-EQ)          │  (loopback)  │    -> ring B                          │
└─────────────────────────────┘              └───────────────────────────────────────┘
```

## Facts this plan is built on

- VBMatrix is a standard ASIO host ("ASIO SLOT"), strict-syncs *its own* virtual
  drivers to the master clock; a third-party driver is self-clocked from its own
  `bufferSwitch` (VB manual + forum; see research notes below).
- EQ Router holds the Yamaha exclusively while running, so VBMatrix cannot use the
  Yamaha as its master → VBMatrix runs on its Internal Clock → **two clock domains
  remain; the drift-compensated resampler stays** (both directions).
- ASIO registration: `HKLM\SOFTWARE\ASIO\<name>\CLSID` + `HKCR\CLSID\{...}\InprocServer32`;
  many hosts also `LoadLibrary` + `DllGetClassObject` directly. Both must be exported.
- `asio-host`/`asio-probe` can drive any registered driver → driver can be bench-tested
  without VBMatrix (`router-cli` / `asio-probe run "EQ ASIO Router" 5`).
- The frozen v1 (`crates/eq-asio-driver`) hosted the Yamaha *inside the host process*.
  v2 drops that entirely: no `asio-host` dependency in the driver, no master, no DSP.
  Same CLSID, same registered name (re-registration overwrites v1).

## Component 1 — `bridge-abi` (new crate): the shared contract

`crates/bridge-abi`, `cdylib`-free (`rlib` + `#[repr(C)]` POD, no `windows` API in the
layout itself). Used by `eq-asio-driver` and `router-engine`. **Write this first.**

- Mapping: named file mapping `Local\eq_asio_router_bridge_v2` (session-local),
  fixed total size, created by first comer (`CreateFileMappingA`, both sides pass the
  same size; on `ERROR_ALREADY_EXISTS` open existing). x86/x64 share the same layout
  (all fixed-size types).
- Layout (all `#[repr(C)]`, 64-byte aligned cache lines for the hot atomics):

  | Field | Type | Writer | Notes |
  |---|---|---|---|
  | `magic` | `u64` | creator | `0x4551425249444732` ("EQRIDGE2"), reject on mismatch |
  | `version` | `u32` | creator | `1` |
  | `channels` | `u32` | creator | `8` (in = out) |
  | `sample_rate` | `f64` | driver | negotiated with VBMatrix (`set_sample_rate`) |
  | `buffer_frames` | `u32` | driver | negotiated ASIO buffer size |
  | `driver_running` | `AtomicU32` | driver | 1 between `start()`/`stop()` |
  | `driver_beat` | `AtomicU64` | driver | +1 per driver callback (heartbeat) |
  | `app_beat` | `AtomicU64` | app | +1 per app control tick (100 ms) |
  | `a_write` / `a_read` | `AtomicU64` | driver / app | ring A indices (frames) |
  | `b_write` / `b_read` | `AtomicU64` | app / driver | ring B indices (frames) |
  | `a_overruns`, `a_discarded` | `AtomicU64` | driver / app | diagnostics |
  | `b_underruns`, `b_dropped` | `AtomicU64` | app / driver | diagnostics |
  | `qpc_freq` | `i64` | creator | for time conversions |

- Rings: two interleaved stereo `f32` regions of `RING_FRAMES = 16384` (power of two,
  128 KiB each) after the header page. Cross-process SPSC, index wrap on `u64`.
  Semantics mirror `router-core::ring`: producer **drops new** frames when full
  (count `overrun`); consumer may discard oldest when fill exceeds a target (count).
  Ordering: `Release` stores of indices, `Acquire` loads (lock-prefixed ops are
  coherent across processes on the same machine). No locks, no OS calls in push/pop.
- API: `Header::validate`, `ring_a_push`/`ring_a_pop`, `ring_b_push`/`ring_b_pop`,
  `fill_a()`/`fill_b()`, `open() -> Option<Mapping>` (both sides; `open_or_create`),
  all `unsafe` where they dereference raw shared memory. Unit tests spawn a child
  process to prove cross-process push/pop.

## Component 2 — `eq-asio-driver` v2 (rewrite as pure bridge)

Keep: `com.rs` (class factory, `DllGetClassObject`, `DllRegisterServer/Unregister`
under `HKLM\SOFTWARE\ASIO\EQ ASIO Router`), `ids.rs` (same CLSID/name; bump
`DRIVER_VERSION` to 2, description "virtual ASIO bridge for VBMatrix"),
the `asio_method!` ABI macro pattern, `log.rs` (`%TEMP%\eq_asio_router_driver.log`),
`convert.rs` (f32 ⇄ host sample type).
Delete: master hosting (`engine.rs`, `watcher.rs`, `processor.rs`, `config.rs`,
`status.rs` as master-side pieces, the `asio-host` dependency).

Behavior:

- Channels: **8 in / 8 out** (matches the VASIO8 topology the matrix links against).
  Engine uses 0/1; out 2..8 discarded after shuttle copy, in 2..8 returned as silence.
- Sample type: `Int32LSB` for all channels (v1 precedent; convert.rs already saturates).
  If VBMatrix rejects it during validation, add `Float32LSB` (risk register).
- Rates: `canSampleRate` accepts 44.1k/48k/88.2k/96k/176.4k/192k. Buffer sizes
  64..2048, preferred 256, granularity 64. `controlPanel` = no-op `ASE_OK`.
- `init` attaches/creates the shm mapping (never fails hard if the app is absent —
  the driver creates the mapping and works standalone; audio just drops).
- **Pacing thread** (the driver has no hardware): high-resolution waitable timer
  (`CreateWaitableTimerEx` + `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`), MMCSS
  "Pro Audio", QPC-anchored absolute schedule (`next += period`, never re-anchor,
  so no long-term drift), fires the host's `bufferSwitch`/`bufferSwitchTimeInfo` at
  `sample_rate / buffer_frames`. Started by `start()`, stopped by `stop()` (joins
  outside the callback if needed — v1's `ResetThread` pattern is the reference).
- Per callback (RT rules: no alloc, no locks, no syscalls): copy host-written ASIO
  output buffers (Int32 → f32) → `ring_a_push`; `ring_b_pop` → f32 → Int32 into ASIO
  input buffers (silence on underrun); update `driver_beat`, sample position
  (monotonic frames × 1e9 / rate), then invoke the host callback with
  `directProcess = TRUE`. If the host requested `createBuffers` with
  `directProcess = FALSE`, return `ASE_INVALID_MODE` (validated in M0).
- `getLatencies`: `buffer_frames` in, `buffer_frames` out.
- App absent → `ring_a_push` counts overrun and drops; `ring_b` empty → input silence.
  App attaches/detaches live (mapping is stable; indices persist).
- Build: `x86_64-pc-windows-msvc` **and** `i686-pc-windows-msvc`; `scripts/install-driver.ps1`
  updated (default: register the 64-bit DLL for VBMatrix, `-Both` for 32-bit hosts;
  text now says VBMatrix, not ASIO Bridge).

## Component 3 — app side: source swap + loopback feeder

- New `router-engine::bridge` module replacing the WASAPI session as the ring producer:
  - Ring A becomes the shm ring — the Yamaha callback consumes `ring_a_pop` directly
    (same `fill` → startup gate → RateMatcher → EQ path; only the producer moved
    process). `RateMatcher`, headroom planner, fault detector, arrival/fill statistics
    and "Learn (30 s)" keep working, fed from shm fill + `driver_beat` deltas instead
    of WASAPI arrival timestamps.
  - Control thread attaches to the mapping with the existing `retry_unavailable`
    machinery; status gains `bridge_attached`, `vbmatrix_running` (via
    `driver_running` + `driver_beat` staleness), negotiated rate/buffer, fills,
    overruns/underruns.
  - **Loopback feeder thread** (not on any audio callback): taps the EQ output
    (small in-process SPSC written inside the Yamaha callback after `EqProcessor`),
    resamples Yamaha-rate → `sample_rate` using `router-core::resample` + a PI drift
    controller on ring B fill (mirror of the ring A controller), then `ring_b_push`.
- GUI: replace capture-device / source panels with a bridge readiness panel
  (VBMatrix process found?, driver registered?, mapping attached?, slot running?,
  fills, negotiated format). Keep Learn; retitle semantics to ring-fill.
- Latency: `router-cli impulses` allowlist gains VBMatrix In endpoints (they are
  render endpoints); `latency_measured.toml` keying moves from capture device to
  the fixed `"vbmatrix"` source.

## Component 4 — removals (hard cutover)

- `router-engine`: `cable.rs` (cable-format helper), `source.rs` WASAPI readiness,
  keep-alive wiring, fixed-period capture mode, `--source` CLI switch, capture-device
  selection, `jitter_margin_by_device` (keep a single global margin), VAC presets.
- `eq-asio-app`: capture-device picker, VAC Line 1 preset UI, cable rate-match toggle.
- Config: remove `capture_device`, `source`, `capture_mode`, `keepalive`,
  `cable_rate_match`; keep parsing old files (serde `default` + ignore unknown keys,
  log a deprecation notice). Add `[bridge]` (`ring_target_ms`, `loopback_enabled`).
- `wasapi-capture`: engine stops using it; the crate stays **only** for
  `capture-probe` (documented measurement tool — it measures VBMatrix In periods now).
- Keep `ForbiddenMaster` ("EQ ASIO Router" must never be the master) — now genuinely
  load-bearing: the engine must refuse to open our own bridge as master.
- Docs: README architecture diagram, `architecture.md` §1/§8, `setup-guide.md`
  (VBMatrix install, default playback = VBMatrix In, master clock = Internal Clock),
  `configuration.md`, `troubleshooting.md`, `development.md`, `latency.md` (new
  budget; VAC/Hi-Fi sections marked historical).

## Milestones (each ends with `cargo build --workspace --all-targets && cargo test`)

- **M0 — driver skeleton + validation gate.** `bridge-abi` + v2 driver with COM,
  pacing thread, shm attached but shoveling locally (loopback in-process), registered.
  Gate: `asio-probe run "EQ ASIO Router" 5` shows steady callbacks, then **manual**:
  VBMatrix opens the slot (checks `directProcess`, sample type, strict-sync behavior,
  buffer negotiation). Findings may adjust: sample type, directProcess, pacing model.
- **M1 — full bridge.** Real shm rings both directions, heartbeats, counters;
  cross-process tests; driver dropped when app absent. Gate: two-process test moves
  audio through the rings with measured fill stability across ≥10 min.
- **M2 — app source swap.** `router-engine::bridge`, loopback feeder, status +
  readiness panel; engine runs end-to-end VBMatrix → Yamaha on hardware.
  Gate: no crackle, `router-cli latency` measures the new chain.
- **M3 — removals + docs + scripts.** Workspace clean, docs updated, install scripts
  register both bitnesses, CI green.
- **M4 — hardware validation checklist** (user-run): VBMatrix slot stable for 1 h,
  fill/underrun counters flat, end-to-end latency recorded in `latency.md`.

## Risk register

| Risk | Mitigation |
|---|---|
| VBMatrix drives third-party slots differently than expected (pull model, `directProcess=FALSE`) | M0 manual gate before app work; adapter fallback documented (extra driver thread mode) |
| VBMatrix rejects `Int32LSB` | switch to `Float32LSB` (one `getChannelInfo` change) |
| Cross-clock instability on ring A/B under long runs | both rings keep PI drift controllers; counters surfaced in status; 16384-frame capacity = ~340 ms @48k headroom |
| Two mappings created with different sizes (32/64-bit host vs app) | fixed `RING_FRAMES`/layout constants in `bridge-abi` + `magic`/`version` check |
| Driver loaded before app forever (VBMatrix autostart) | mapping created by driver; app attaches later; startup gate keeps Yamaha silent until fill reached |
| Old registered v1 DLL left behind | same name+CLSID; install script re-registers in place; uninstall script unchanged |
