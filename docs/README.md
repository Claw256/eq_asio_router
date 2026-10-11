# Documentation index

Project overview: [../README.md](../README.md).

## User and operator documents

| Document | Content |
|---|---|
| [setup-guide.md](setup-guide.md) | Windows, cable and Yamaha setup; VAC Line 1 quick start; tray and run at login |
| [configuration.md](configuration.md) | Every `config.toml` field, `app_settings.toml`, status block, CLI reference (`router-cli`, `capture-probe`, `asio-probe`) |
| [latency.md](latency.md) | Latency budget, how to reduce it, investigation verdicts, how to measure, first-run validation checklist for VAC Line 1 |
| [troubleshooting.md](troubleshooting.md) | Symptom, cause, fix; log locations; reverting changes |

## Developer documents

| Document | Content |
|---|---|
| [architecture.md](architecture.md) | Data flow, threads, real-time rules, state machine, crate responsibilities, timing model |
| [development.md](development.md) | Build and test commands, test strategy, conventions, limitations, licensing |

## Latency investigation reports (2026-10-10)

Read-only research with a few quiet measurements on the author's machine (Windows 11 build 26200).

| Report | Question | Short answer |
|---|---|---|
| [latency/capture_apis.md](latency/capture_apis.md) | Which capture API gives a smaller or more regular period than the Hi-Fi Cable? | Process loopback (exclude own pid) is the best option with no new software; IAudioClient3, exclusive, WDM-KS, MME, AudioGraph are dead ends here |
| [latency/apo_tap.md](latency/apo_tap.md) | Can an audio-engine APO tap replace the cable? | Feasible but only about 33-38 ms on a 10 ms endpoint, high risk, 6-10 days; not recommended now |
| [latency/vac_lite.md](latency/vac_lite.md) | Is VAC lite "Line 1" a usable low-latency source? | Yes, with a fixed shared period of at least 5 ms; later testing made 6 ms (288 frames) the supported default. Also the design of the VAC Line 1 work, which is now implemented (hardware validation pending) |

## Reverse-engineering reports (2026-10-10)

Static analysis of installed vendor binaries for interoperability and latency tuning. Nothing was executed or loaded.

| Report | Subject |
|---|---|
| [reverse/yamaha_usb_driver.md](reverse/yamaha_usb_driver.md) | Yamaha Steinberg USB Driver 2.1.9: ASIO driver, registry values, LatencyMode presets, reported-latency formula, control panel |
| [reverse/steinberg_asio_and_stack.md](reverse/steinberg_asio_and_stack.md) | Steinberg built-in ASIO driver and the UR22C driver stack |
| [reverse/steinberg_urc_software.md](reverse/steinberg_urc_software.md) | dspMixFx UR-C 3.3.0 mixer software and how it coexists with the ASIO driver |

## Conventions used in these documents

- Paths use `%APPDATA%` and `%TEMP%` rather than personal paths.
- "Measured" means measured on the author's machine in the cited report; "estimate" means inferred.
- "Implemented, hardware validation pending" (VAC Line 1 low-latency source): the code exists, builds and has unit tests, but the real fixed-period capture, the render keep-alive and a live fallback have not yet been exercised on hardware. Every VAC latency figure is a model estimate until measured; see [latency.md](latency.md#7-first-run-validation-checklist-vac-line-1) for the checklist and [latency/vac_lite.md](latency/vac_lite.md) for the findings and design.
- `router-cli latency` (end-to-end click measurement via the Yamaha's Streaming L/R loopback inputs) is implemented, has run live on the Yamaha, and stores its result (`latency_measured.toml`) for the latency readout; see [latency.md](latency.md#5a-end-to-end-measurement-router-cli-latency) and the render-side matrix in [latency.md](latency.md#5b-what-determines-the-render-side-vac-line-1-render-matrix).
