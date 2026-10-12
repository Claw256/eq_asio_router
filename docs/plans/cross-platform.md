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
