# Cross-platform portable engine (Windows / Linux / macOS)

`portable-engine` is the headless, cross-platform heart of the EQ router: it captures
system audio, resamples it to the output device's rate with drift control, applies the
AutoEQ profile (plus bypass/limiter/safety-buffer settings) and plays it to your
interface. On all three OSes the same DSP core (`eq-core` + `router-core`) and the
same config file drive the engine; only the per-OS audio backends differ.

> **Status:** software validation is green on all three OSes (unit/integration tests +
> a hardware-free self-test). **Hardware behaviour is unvalidated** — every latency
> figure the tool prints is labelled `unmeasured (pending hardware validation)` until
> you run the [hardware validation checklist](#hardware-validation-checklist) below.

## What the portable engine does per OS

| | Output (your interface) | Source (system audio) | Backend |
|---|---|---|---|
| **Windows** | Master **ASIO** driver (e.g. "Yamaha Steinberg USB ASIO") | *not wired yet* — portable run is output-path only; the full source path is the Windows app | `asio-host` (registry + COM + IASIO) |
| **Linux** | **ALSA** playback (UAC2 class-compliant) | **PipeWire/Pulse monitor** of your default sink (or a named monitor) | hand-written FFI (`libasound`, `libpulse`) |
| **macOS** | **Core Audio** output device | **BlackHole** loopback tap (user-installed) | hand-written FFI (CoreAudio/AudioToolbox) |

The engine negotiates the device's sample rate and buffer size, sizes its ring from
the safety buffer, and continuously trims the resampler (drift ppm) so the ring never
runs dry or overflows.

## Device setup

### All OSes: the interface

The **Steinberg UR22C** (class-compliant UAC2) is the reference device:

- **Windows:** install the **Yamaha Steinberg USB ASIO** driver. The engine opens the
  master ASIO driver by name and drives it exactly like the router app. The frozen
  v1 virtual driver ("EQ ASIO Router") is refused as master on purpose.
- **Linux:** the UR22C is class-compliant — no vendor driver. Make sure it is the
  default (or selectable) ALSA device; PipeWire/Pulse must be running for the monitor
  source.
- **macOS:** class-compliant Core Audio; no vendor driver needed.

### macOS: BlackHole install + loopback routing

The macOS source taps system audio through a loopback audio device. v1 uses
**BlackHole** (installed by **you**, not by us):

1. Install BlackHole 2ch from its upstream project
   (<https://github.com/ExistentialAudio/BlackHole>) — e.g.
   `brew install blackhole-2ch`. **We ship no BlackHole code and link against none of
   it** (see [Licensing](#licensing-notes)).
2. Create a **Multi-Output Device** (Audio MIDI Setup): BlackHole 2ch + your UR22C,
   so you can still hear the audio while it is being looped.
3. Set the Multi-Output Device as the **system output**.
4. Route/play audio into it; the engine taps the **BlackHole input** (its default tap
   discovery finds "BlackHole 2ch" — or name the device explicitly with
   `[portable] input_device`).
5. Output goes to the UR22C (or `[portable] output_device` selects another Core Audio
   device; empty = system default output).

### Linux: PipeWire monitor selection

The Linux source captures the **monitor source** of your playback sink
(`@DEFAULT_SINK@.monitor` by default):

- Default: leave `[portable] input_device` empty and the default chain's monitor is
  used.
- Specific monitor: set `[portable] input_device` to the monitor name
  (e.g. `alsa_output.usb-Yamaha_Steinberg_UR22C-00.analog-stereo.monitor` — list with
  `pactl list short sources` or `pw-dump`).
- Output-only runs: `[portable] source = "none"`.

## Configuration

The engine shares the router's config file (`%APPDATA%\eq_asio_router\config.toml`
via `--config`; on Linux/macOS pass `--config PATH`). The `[portable]` section is
**new and separate** from the Windows-only `[bridge]` section:

```toml
# shared EQ settings (same as the Windows app):
bypass = false
eq_enabled = true
limiter = true
safety_buffer_ms = 2.0
safety_buffer_auto = true
sample_rate = 0            # 0 = device's current rate

[portable]
source = "auto"            # "auto" | "none" | per-OS kind ("pipewire", "blackhole")
output_device = ""         # ASIO name (Windows) / ALSA device / Core Audio name
input_device = ""          # PipeWire monitor name / BlackHole device name
profile_path = "eq/HD650.txt"   # AutoEQ ParametricEQ.txt (optional override)
```

`profile_path` loads an AutoEQ `ParametricEQ.txt` at startup; bypass, the EQ stage,
the soft limiter, auto-headroom and the safety buffer (incl. the automatic extra)
come from the same `eq-core` code the Windows app uses.

## Build and run

```sh
# anywhere: hardware-free self-test (exit 0 = pass)
cargo run -p portable-engine --bin portable-cli -- --selftest

# Windows (MSVC toolchain)
cargo build --workspace --all-targets
cargo run -p portable-engine --bin portable-cli -- --config PATH\to\config.toml

# Linux (needs libasound2-dev libpulse-dev to link)
sudo apt-get install -y libasound2-dev libpulse-dev
cargo run -p portable-engine --bin portable-cli -- --config config.toml

# macOS
cargo run -p portable-engine --bin portable-cli -- --config config.toml
```

CLI synopsis:

```
portable-cli [--config PATH] [--duration-s N] [--tick-ms N]
portable-cli --selftest
```

- `--config PATH` — config file (defaults to the router's config path)
- `--duration-s N` — run N seconds then exit (default 0 = run until **Ctrl+C**)
- `--tick-ms N` — supervision tick (default 100 ms)
- `--selftest` — fake-driven pipeline smoke test, no hardware (CI gate)

## Status line glossary

Once per second:

```
t=12.0s ring=1234 rate=48000Hz drift=+0.03ppm xruns=0 underruns=0 fault=0 latency: unmeasured (pending hardware validation)
```

| Field | Meaning |
|---|---|
| `ring` | source-ring fill (source frames) at the last render — the quantity the drift controller regulates |
| `rate` | negotiated output rate (Hz) |
| `drift` | resampler trim (ppm) compensating the source/output clock difference |
| `xruns` | output-side deadline misses reported by the backend |
| `underruns` | blocks rendered with missing source data |
| `fault` | EQ fault state (0 = healthy; non-zero = muting/protection active) |
| `latency` | **`unmeasured (pending hardware validation)`** — round-trip latency is only printed after the checklist below has been run with real hardware |

## HARDWARE VALIDATION CHECKLIST

Run this per OS with the interface attached. All four items must pass before the
engine is considered hardware-validated (and before any latency number is trusted):

1. **Start the engine.**
   - Windows: `portable-cli --config config.toml` (ASIO master per `[portable]
     output_device` / `master_driver`). *Note: the portable Windows source is not
     wired yet — see item 4.*
   - Linux: `portable-cli --config config.toml` with PipeWire running (monitor
     selected via `[portable] input_device`).
   - macOS: BlackHole + Multi-Output Device set up as above, then
     `portable-cli --config config.toml`.
2. **Confirm no xruns for 10 minutes.** Let the engine run ≥ 10 minutes playing
   continuous audio. The status line must show `xruns=0`, `underruns=0`, `fault=0`
   and a stable `ring` fill; `drift` should settle to a small (sub-100 ppm) value.
3. **Measure round-trip latency.** Loop the interface's output back into its input
   (analog cable or BlackHole/PipeWire loopback) and measure the delay (click/impulse
   in, recorded out), or compare the engine's own arrival statistics. Record the
   result next to the buffer size and rate used; only then replace the
   `unmeasured` label with a measured figure.
4. **Verify the EQ profile is applied.** Play music (or a test tone) and A/B
   `bypass = false` vs `bypass = true` (or swap `profile_path`): the tonal change
   must be audible/visible in a spectrum capture, and `fault=0` throughout.
   - Windows (portable CLI): source-side verification is pending the Windows
     portable source — verify the EQ path with `portable-cli --selftest` (checks a
     +6 dB stage end-to-end) or with the full Windows app, and treat item 4 as
     OPEN for the portable Windows run.

## Licensing notes

- **BlackHole is GPLv3 and user-installed.** We link no BlackHole code, bundle no
  BlackHole binaries and distribute nothing of it — the install step above fetches it
  from its upstream. This keeps the MIT-licensed project distribution clean; the GPL
  obligations (if any) are between you and the BlackHole upstream. This wording
  records the A5 decision (see `docs/plans/cross-platform.md` §A5/§Phase C results);
  it is not legal advice.
- **ScreenCaptureKit per-app capture** is a possible *future* fallback source on
  macOS 13+ — not implemented in v1.
- **In-house Audio Server Plug-in** loopback (MIT-licensable, self-contained) is the
  planned release-path replacement for the user-installed tap (A5 rank 2).
- **cpal was rejected for v1 (A4 rationale):** (1) the host may substitute our
  requested buffer size, which destroys the ring/latency arithmetic's ground truth;
  (2) it offers no producer-side arrival timestamps, which the `ArrivalTracker`
  drift model needs; (3) its `*-sys` dependencies probe system SDKs in `build.rs`,
  which would break the cross-`cargo check` gate. Raw ALSA/Core Audio/ASIO via
  hand-written FFI keeps negotiated buffers exact and the tree dependency-free; the
  `OutputSink` seam keeps a cpal fallback possible if raw backends ever prove
  unmaintainable.
- Raw **ALSA** is LGPL — dynamically linked, which is fine; **Core Audio** framework
  usage is unencumbered; **ASIO** licensing is handled by the existing Windows
  distribution story.
