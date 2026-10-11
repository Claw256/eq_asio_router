# EQ ASIO Router

A Windows application written in Rust that takes system audio from a virtual cable (VB-Audio Hi-Fi Cable), runs it through an AutoEQ parametric equalizer and plays it out of a Yamaha/Steinberg USB ASIO device at the lowest latency the hardware allows. It replaces a Voicemeeter / Peace routing chain with one small tray application.

Status: working and clean (no crackle) on the author's machine (Steinberg UR22C). The latency model total with the Hi-Fi Cable source is 56.2 ms, but the end-to-end click measurement (`router-cli latency`) gave 152-164 ms because the cable driver adds about 100 ms on the render side. A lower-latency source (VAC "Line 1": fixed 6 ms capture period, render keep-alive, automatic fallback, GUI preset) measured end to end about 50-52 ms for an ordinary 10 ms shared-mode app; an app using IAudioClient3 low-latency shared mode should reach about 32 ms (render side measured with injected clicks, not the full chain; real games not measured). See [docs/latency.md](docs/latency.md) and the [VAC Line 1 quick start](docs/setup-guide.md#vac-line-1-quick-start).

## Features

- AutoEQ `ParametricEQ.txt` import (Preamp, PK / LSC / HSC filters, up to 32 bands), applied live with no click and no audio dropout.
- Live EQ: edit the profile, bypass, limiter, safety buffer and resampler quality while audio plays (config hot reload).
- Drift-compensated resampling: a Kaiser-sinc resampler with a PI drift controller keeps the cable clock and the Yamaha clock locked at any rate pair (44.1 / 48 / 96 kHz ...).
- Latency control: ring target derived from the measured capture arrival pattern ("Learn (30 s)" button), auto safety buffer.
- Safety: soft limiter, automatic headroom (preamp lowered so the profile never boosts above 0 dBFS), fault detector that fade-mutes on non-finite samples, huge peaks, underruns or capture gaps.
- Busy-device handling: if another program holds the Yamaha (VB Matrix, VirtualDJ, ASIO Bridge ...) the engine shows who, waits and starts by itself when the device is free. Failures restart with exponential back-off.
- Tray application with "run at login" (per-user registry Run value, no service) and single-instance protection.
- Process hardening: EcoQoS opt-out, 1 ms timer resolution, MMCSS "Pro Audio" on the control, capture and ASIO callback threads, a watchdog for stalled threads.
- Cable rate matching: sets the Hi-Fi Cable's Windows format to the Yamaha's rate (can be disabled). It only touches VB-Audio cable endpoints, never VAC.
- Low-latency source preset "VAC Line 1" (implemented, hardware validation pending): fixed shared capture period (default 288 frames = 6 ms) with a corruption monitor and one-shot fallback to plain Shared, silent render keep-alive on the cable, a readiness panel, and a learned jitter margin per capture device. `router-cli run --source vac|cable` selects the same presets headless.
- `router-cli latency`: end-to-end click latency measurement (a quiet click injected into the cable, detected on the Yamaha's loopback inputs; its result is stored and used by the latency readout), see [docs/latency.md](docs/latency.md#5a-end-to-end-measurement-router-cli-latency).

## Requirements

- Windows 10/11 x64.
- VB-Audio Hi-Fi Cable (donationware) as the system playback device; optionally Virtual Audio Cable lite "Line 1" (the low-latency source, private home use only).
- A Yamaha/Steinberg USB ASIO interface (default master: `Yamaha Steinberg USB ASIO`). Any ASIO driver that exposes at least two outputs with a supported sample type can be named in `master_driver`, but only the Yamaha is tested.
- Rust stable (MSVC toolchain) to build. No Steinberg ASIO SDK is needed.

## Quick start

1. Install the cable and the Yamaha driver; set Windows default playback to `Hi-Fi Cable Input` ([setup guide](docs/setup-guide.md)).
2. Close anything else using the Yamaha (`router-cli holders` shows who).
3. Build and start the tray app:

```powershell
cargo build --release -p eq-asio-app -p router-cli
.\target\release\eq-asio-app.exe
```

4. In the window, press Start, then load an AutoEQ `ParametricEQ.txt`. Tick "Run at login" if wanted.
5. Headless alternative: `.\target\release\router-cli.exe run` (Ctrl+C to stop).

## Architecture

```
 apps -> Windows mixer -> Hi-Fi Cable Input --(driver)--> Hi-Fi Cable Output
                                                              |
                                           WASAPI shared capture thread (MMCSS)
                                                              |
                                              lock-free SPSC ring (stereo f32)
                                                              |
      ASIO callback thread (owned by the Yamaha driver, runs the rest in the callback):
        RateMatcher (startup gate, PI drift controller, Kaiser-sinc resampler)
          -> EqProcessor (preamp, biquads, soft limiter, fault detector)
          -> asio_convert (f32 -> Int32LSB etc.)
          -> Yamaha ASIO output buffers -> USB -> DAC

 control thread (STA): engine state machine, retry/back-off, config watcher,
 watchdog, logger, status block (Local\eq_asio_router_status) <- GUI / tray
```

Details: [docs/architecture.md](docs/architecture.md).

## Documentation

| Document | Content |
|---|---|
| [docs/README.md](docs/README.md) | Index of all documents, including the reverse-engineering and latency reports |
| [docs/setup-guide.md](docs/setup-guide.md) | Windows, Yamaha and cable setup |
| [docs/configuration.md](docs/configuration.md) | `config.toml`, `app_settings.toml`, status block, CLI reference |
| [docs/latency.md](docs/latency.md) | Latency budget, measurements, how to reduce it |
| [docs/troubleshooting.md](docs/troubleshooting.md) | Symptom, cause, fix |
| [docs/development.md](docs/development.md) | Build, test, conventions, licensing |

## Crates

`eq-core` (DSP, config, AutoEQ, status layout), `router-core` (ring, resampler, arrival model, processor, logger, watchdog), `router-engine` (engine, pipeline, priority, cable), `wasapi-capture` (capture, probes), `asio-abi` / `asio-host` (ASIO hosting), `router-cli`, `eq-asio-app` (egui GUI and tray), `asio-probe`, and `eq-asio-driver` (frozen v1 virtual ASIO driver, no longer used).

## License

The repository is MIT licensed (workspace `Cargo.toml`). The Steinberg ASIO SDK is not included and not required. See [docs/development.md](docs/development.md) for third-party notes.
