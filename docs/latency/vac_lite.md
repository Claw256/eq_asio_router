# Virtual Audio Cable (lite 4.70) "Line 1" as a low-latency router source

> Purpose: is Virtual Audio Cable lite "Line 1" a usable low-latency source for the router, and what would the integration need.
> Date: 2026-10-10. Method: read-only investigation plus quiet test audio (-50 dBFS, 8 to 14 s) into Line 1, replay of measured capture gaps through the arrival model.
> Back to the [documentation index](../README.md).
>
> **Superseded in part (2026-10-10, later measurements).** The router-side totals of section 4 (26 to 34 ms, 10 ms render side) are model estimates and are superseded by the measured results in [../latency.md section 5a](../latency.md#5a-end-to-end-measurement-router-cli-latency) and [section 5b](../latency.md#5b-what-determines-the-render-side-vac-line-1-render-matrix): render side measured 23.8 ms for a legacy 10 ms client (about 21 ms of it inside Windows), router total about 50 to 52 ms to the DAC; only an IAudioClient3 client reaches about 7 ms click -> capture. `router-cli latency` now exists and was run on this path. The capture-side measurements in sections 1 to 3 stand. Still unrun: soak/listening, live fallback, reboot persistence.
>
> Corrections/updates (later testing): **240 frames (5 ms) proved unreliable in later testing (4 of 6 runs damaged); 288 frames (6 ms) is the supported default.** Read every "240" and "5 ms" below as "the best case seen in the first four runs", and add roughly 1 to 2 ms (estimate) to the latency totals. The MMCSS finding of the router work (error 1552 on the ASIO thread, see [../troubleshooting.md](../troubleshooting.md)) is irrelevant to this report. The integration described in section 5 is now implemented (config, engine, capture layer, GUI; see section 8), and its capture and full-chain latency have since been measured on hardware (see the note above); the latency figures in section 4 below are the original model estimates.

Date 2026-10-10. Read-only investigation plus quiet test audio (noise/tone at -50 dBFS, 8-14 s, rendered only into "Line 1"; no audiorepeater or any other process captures Line 1, nothing was audible). Nothing in Windows, registry, endpoints, formats or volumes was changed, nothing installed, no process touched (eq-asio-app.exe PID 13124 untouched; Yamaha not opened). Raw data: `target\vac_spike\` (WAVs, gap logs).

## Verdict

**Viable, but not the 1 ms source the endpoint's IAudioClient3 numbers suggest.** The VAC *lite* driver cannot deliver clean data faster than a **5 ms capture event**: measured sample-accurate at 5 ms (240 frames), 6 ms, 7.5 ms and 10 ms, but corrupted (duplicated blocks, frames delivered faster than real time) at every period below 5 ms, including the 1 ms IAudioClient3 minimum that `CaptureMode::LowLatency` would request. Expected router total with a 5 ms capture event: **about 26-34 ms (vs 56.2 ms today)**, i.e. roughly 22-30 ms better. With no code change at all (plain Shared capture of "Line 1", regular 10 ms packets, no pairs) about 39-47 ms. Confidence medium: the capture side is measured, the render side (app -> VAC -> capture) is not measurable here and dominates the uncertainty.

## 1. Endpoints and state

| Item | Finding |
|---|---|
| Driver | `vrtaucbl.sys`, service `VirtualAudioCable_83ed7f0e-2028-4956-b0b4-39c76fdaef1d`, device `ROOT\{83ED7F0E-...}\0000` (Eugene Muzychenko, oem81.inf, v4.70.0.13049), installed 2026-02-01 (install.log). Status OK, no problem code. |
| Render endpoint | "Line 1 (Virtual Audio Cable)" `{0.0.0.00000000}.{d80df922-5f38-40b4-a1e8-e8c2b27404ab}`, MMDevices DeviceState = 1 (active). |
| Capture endpoint | "Line 1 (Virtual Audio Cable)" `{0.0.1.00000000}.{6e079f63-89ba-49b5-9310-da0d2e19efaa}`. **When I started it was NOT active**: DeviceState = 0x10000001 (low bits 1, plus an internal flag 0x10000000), `DEVPKEY_Device_IsPresent = False`, PnP status Unknown, absent from `capture-probe list`. The same pattern (0x10000001) applies to ~22 other stale capture endpoints on this machine (Voicemeeter, VBMatrix Out 3-8, CABLE Output ...). At **14:01:04** (Audio/Operational event 65, "Audio device state changed") it became DeviceState = 1, IsPresent = True, and has stayed active since. I did not enable anything; the trigger is unknown (my first `capture-probe info` ran about then; I cannot prove it was the cause or that it persists across reboot/audio-service restarts). It is **not** state 2 (disabled) and not hidden: no "Hide capture device" tunable is set. If it ever shows as disabled: Sound control panel > Recording > right-click empty area > Show Disabled Devices > right-click "Line 1" > Enable (I did not do this).
| Cables | Lite = **one cable only**. No driver or app tunables are set anywhere (no `...\Services\...\Parameters`, no `HKLM\SOFTWARE\EuMus Design`, nothing in HKCU), so everything runs on driver defaults. |
| Format | Both endpoints: device format 48000 Hz, 2 ch, **16-bit PCM** (Windows mixes float; audiodg converts to 16-bit for the cable, no dither) vs Hi-Fi Cable 24-bit. Lite maximum is 48 kHz / 16 bit / 2 ch. Mix format as seen by WASAPI: 48000 Hz float32. Only 48 kHz tested (no format changed). |
| Port type | Capture endpoint registry shows the WaveRT filter names `wave_c_rt` / `topo_c_rt` (the render side was not dumped; assumed the same). Port type is configurable per side in the VAC Control Panel (WaveCyclic/WavePci/WaveRT) but not touched. |
| Audio repeater | Not running, no startup entry, no scheduled task. Not needed: the router captures the recording side directly. |

## 2. Capture measurements ("Line 1" recording side, 48 kHz stereo)

`capture-probe info "Line 1 (Virtual Audio Cable)"`: GetDevicePeriod default 10.000 ms / min 1.000 ms; IAudioClient3 engine period default 480 / fundamental 1 / min 48 / max 480 frames; exclusive mode refuses the mix format. Render side identical (default 480, min 48). The render engine period of Line 1 drops from 480 to **48 fr (1 ms)** while a keep-alive stream with IAudioClient3-min runs (`loopback-run "Line 1" --keepalive --lowkeep`: "engine period with keep-alive running: 48 fr / 1.000 ms"), confirming the earlier spike.

Capture packet cadence with IAudioClient3 at various periods (`InitializeSharedAudioStream`; periods other than the minimum were tested with a temporary private build in the scratchpad, repo sources restored, no `VAC_SPIKE` left). Integrity = `capture-probe check` against noise rendered through the same Windows path (sample-accurate = mismatched 0, slips 0).

| Capture period | gaps us: min / p50 / mean / p99 / p99.9 / max | pattern | integrity (noise -50 dBFS, ~8 s ref) |
|---|---|---|---|
| Shared default 480 fr (10 ms) | 6600 / 9570 / 10002 / 12646 / 13071 / 13314 | one 480-fr packet per call, no back-to-back pairs (0 % <0.5 ms), sd 1.1 ms | **SAMPLE-ACCURATE** (several runs; 0 discontinuities) |
| 360 fr (7.5 ms) | mean 7.50, max 10.6 ms | regular | SAMPLE-ACCURATE (1 run) |
| 288 fr (6 ms) | mean 6.00, max 7.5 ms | regular | SAMPLE-ACCURATE (1 run) |
| **240 fr (5 ms)** | 52 / 6014 / 4997 / 7139 / 7450 / 7585 | bimodal gaps ~3.0 and ~6-7 ms (mean 5.0), 2 events/10 ms | **SAMPLE-ACCURATE in 4 of 4 runs** (0 discontinuities), CPU idle |
| 216 fr (4.5 ms) | mean 4.48, max 10.8 ms | 0 flags but frames exceed real time | DAMAGED (net -864 samples, slips) |
| 192 fr (4 ms) | mean 3.87, max 10.6 ms | recorded 10.32 s in 10 s | DAMAGED (reference lost / 272 slips) |
| 144 fr (3 ms) | 7 / 3058 / 3000 / 3865 / 4050 / 4834 | ~6 % back-to-back pairs | DAMAGED in 3 of 3 runs (100-130 slips of +-144 frames, discontinuity flags 7-28 per 3333) |
| 48 fr (1 ms, `CaptureMode::LowLatency`) | gaps 3 us ... 2.99 ms, mean 0.90 ms | 33 % of packets back-to-back, 42 % flagged DATA_DISCONTINUITY, frames/s 107-110 % of real time | **UNUSABLE**: ~49 % of 48-frame blocks are exact repeats of the previous block; reference not found |

Other points:
* A 1 kHz tone through shared capture measured 1000.00 Hz (ratio 1.0000); no sample-rate conversion happens (render = capture = 48 kHz; both formats identical, so no resampling question arises; other rates untested and not changed).
* A render keep-alive at 1 ms does **not** change the capture cadence (240-fr gaps p50 6.04 vs 6.01 ms, 144-fr p50 3.05 vs 3.06 ms). It only lowers the render endpoint's engine period (see below); it is not needed to make the capture side work.
* Cause (docs): VAC's own manual, "Free version limitations": single cable, max 48 kHz, 16-bit, 2 ch, **"Minimum milliseconds per event is 3"**, no multi-client support (a single pin instance and a single stream per cable side, i.e. only audiodg's shared engine stream, which is fine for the router), no clock/position registers. The driver's event granularity (3 ms floor) is out of phase with Windows' capture period; measured clean at multiples that avoid the problem (>=5 ms), damaged at <=4.5 ms.

## 3. Driver internals and tunables (read-only)

* Architecture: WaveRT miniports on both sides (RT audio, notification events), chosen per side via `set_port_type.cmd`/Control Panel (0..3: WaveCyclic / WavePci / WaveRT); lite exposes "Cable Range" mode only and no worker-thread control.
* Tunables (all via `setregdword.cmd`, written to `HKLM\SOFTWARE\EuMus Design\Virtual Audio Cable\4` and `HKLM\...\Services\VirtualAudioCable_...\Parameters`; **none exist on this machine, so all defaults**): `Enable packet mode on capture/render` (0/1; improves reliability but may report false over/underflows), `PortCls capture/render port type` (0-3), `Hide capture/render device` (0/1), `Format attribute support policy` (0-2), `Use standard PortCls engine` (0/1), `Timer precision` and `Timer resolution in percents of event period` (10-100), `Split DPC processing among CPUs` (0/1), `Number of cables to restrict client/KMixer thread affinity`, `Connected source lines mask`, `Source line types mask`, `Report instance count to AudioDG`, `Disable cable/stream signal level`. "Milliseconds per event" (called "Ms per int" in the Control Panel) is set in `vcctlpan.exe`, not by a script, and is floored at 3 ms in lite. Which of these lite honours was not tested (they are changes; out of scope). The `set_*` scripts would fail until the `EuMus Design` key exists.
* Install behaviour (log): the installer set VAC as default playback and recording device ("hardcoded Windows behavior"); the current defaults are Hi-Fi Cable Input (playback) and VBMatrix Out 1 (recording), i.e. already reassigned.

## 4. Router latency model

Method as in `docs/latency/apo_tap.md` section 4 and `crates/router-core/src/arrival.rs`: target = block (256) + safety (2 ms = 96) + headroom, headroom = 1.25 x dip p99.9 (16-frame bins) + 2; staleness = E[b^2]/(2 r E[b]) over arrival events (calls <0.5 ms apart merge). I replayed the **measured callback gap sequences** (12 s runs, wall-clock timestamps of the capture thread, same wake behaviour the router sees) through a port of `ArrivalTracker`:

| Case | events | dip p50 / p99.9 (fr) | headroom (fr) | target fr / ms | staleness ms |
|---|---|---|---|---|---|
| Hi-Fi Cable today (apo_tap.md, pairs) | pairs | 480 | | 954 / 19.9 formula (27.6 measured) | 9.2 |
| VAC Shared 480 (regular 10 ms) | 1195 | 240 / 322-351 | 422-442 | 774-794 / 16.1-16.5 | 5.0 |
| **VAC 240 fr (5 ms), render default** | 2395 | 85 / 219 | 282 | 634 / 13.2 | 2.5 |
| VAC 240 fr, render keep-alive 1 ms | 2384 | 54 / 210 | 282 | 634 / 13.2 | 2.5 |
| VAC 144 fr (3 ms; damaged data, for reference) | 3763 | 70 / 171-175 | 222 | 574 / 12.0 | 1.7 |

Totals (**model, superseded by the measured 50 to 52 ms, see the note at the top**; Yamaha out 7.8 fixed; render side is the old 10 ms estimate, now measured as 23.8 ms for a legacy client, unless noted; "pess" adds the 7.7 ms by which today's measured ring exceeds the formula):

| Source | Yamaha | ring | staleness | render side | **Total** | pess (+7.7) |
|---|---|---|---|---|---|---|
| Today, Hi-Fi Cable | 7.8 | 27.6 | 9.2 | ~10 | **56.2** | 56.2 |
| VAC Line 1, Shared 480 (no code change) | 7.8 | 16.3 | 5.0 | ~10 | **~39** | ~47 |
| VAC Line 1, 240-fr capture, apps at default 10 ms | 7.8 | 13.2 | 2.5 | ~10 | **~33.5** | ~41 |
| VAC Line 1, 240-fr capture + router keep-alive (render engine 1 ms) | 7.8 | 13.2 | 2.5 | 2-5 | **~26-29** | ~34-37 |

Original quoted estimate (superseded; measured is about 50 to 52 ms): **~26-34 ms**, central ~30. The "19-20 ms" in apo_tap.md for a hypothetical 1 ms source is **not** reachable with VAC lite (capture floor 5 ms clean, +2.5 ms staleness, +13 ms ring). Confidence: medium. The capture-side numbers are measured and repeatable here; the following are **not verified**:
* Render-side delay (app buffer -> Windows engine -> VAC render ring -> VAC capture event). I could not measure it (no common timestamp for render submit vs capture QPC position; VAC lite has no clock/position registers). The 10 ms in the table is the router's own estimate; with a 1 ms engine period it could drop to ~2-5 ms only for apps that render at the engine period; legacy shared-mode apps (browsers, games) opened at 10 ms default buffers may keep ~10 ms regardless of the engine period, in which case the 26-29 row becomes the 33.5 row. Whether lowering the engine period for streams already open changes their period is untested.
* Cable-internal transport: a frame written to the cable ring waits for the next 5 ms capture event (0-5 ms; not in the staleness figure above if the render and capture events are not aligned). This is probably part of what the 7.7 ms "pessimistic" offset covers.
* ASIO callback jitter on the Yamaha (the router would add its own jitter; not run here because the Yamaha is in use).
* Behaviour under CPU load (all runs were on an idle machine; a 5 ms period has only ~1 ms of margin before the next event, p99.9 gap 7.45 ms), and long-term (>15 s) clock drift (the router's drift controller handles it; VAC's capture/render clocks are the same cable clock).
* Persistence of the capture endpoint (see section 1) across reboot.

## 5. Integration requirements (original design; implemented, see section 8)

Router/GUI:
1. `capture_device` = `"Line 1 (Virtual Audio Cable)"` (the full name; the substring "Line 1" also worked in the probe but "Line" alone would match a Realtek line-in). Works today for Shared capture with no code changes.
2. **Do not use `CaptureMode::LowLatency` with VAC**: it requests the IAudioClient3 minimum (48 fr), which yields duplicated blocks. Add a capture mode with a fixed period (e.g. `FixedPeriod(240)` via `InitializeSharedAudioStream(period = 240)`; the probe change was two lines in `session.rs::init_shared`) and a per-device default; have the engine verify integrity (the discontinuity counter: 0 at 240 fr; 40 % of packets flagged at 48 fr) and fall back to Shared on a rise.
3. Optional render keep-alive on Line 1 (a quiet IAudioClient3-min render stream, as `loopback.rs::KeepAlive` already does; at -50 dBFS or silence) so the endpoint engine period is 1 ms for apps that follow it. Not required for correctness. The router has no cable keep-alive today.
4. `match_cable_rate`: currently `cable::is_cable_name` only matches the VB cables. VAC is already 48000 Hz on both sides and is capped at 48 kHz; if the Yamaha master runs at 96/192 kHz the resampler (cable_rate != asio_rate) already handles it. A VAC branch of `match_cable_rate` should be disabled (VAC can never exceed 48 kHz); nothing else needed. Note the 16-bit cable format (no 24-bit option in lite): about -96 dB noise floor, no dither.
5. GUI: show the true capture period/mode; warn that LowLatency is unsafe on VAC.

User steps (Windows, none performed by me):
1. Make sure "Line 1" appears under Recording (Sound control panel > Recording; if disabled: Show Disabled Devices > right-click Line 1 > Enable). It is active right now.
2. Set the router's capture device to Line 1 (config / tray GUI).
3. Set Windows default playback (or the app's output) to "Line 1 (Virtual Audio Cable)": Settings > System > Sound > Output > choose Line 1, or Sound control panel > Playback > right-click Line 1 > Set as Default Device. (Do not make it the default communications device if you do not want call audio routed there.) Keep Hi-Fi Cable/VBMatrix as they are; the router's own ASIO output goes to the Yamaha.

## 6. Risks and licence

* **License / lite limits** (license.txt, features): "Feature-limited FREEWARE (lite) version ... can be used only in a private home environment that is not associated with income generation." No time limit and no voice reminder in lite. Only the separate *trial* edition injects a female voice reminder (-6 dB, every few seconds) once the driver has run 30 minutes. install.log says `AppName="Virtual Audio Cable (lite)"`, so this is lite. All my recordings (driver up since the 13:16 boot, tests from about 14:02 to 14:40) matched the generated reference bit-exactly (mismatched 0), so **no injected audio was observed**; the driver had been up well over 30 minutes by then, which also fits lite.
* Single stream per side: only audiodg's shared stream; exclusive-mode capture is not offered (accepts mix format: false). Other apps capturing Line 1 share through audiodg; only one app can use exclusive on the render side.
* Driver is third-party kernel code (WHQL/attestation status not checked); a bug takes down audio like any virtual driver. Hi-Fi Cable and VBMatrix remain available as a fallback (the endpoint can simply be switched back).
* The capture endpoint's activation was lazy/unexplained (section 1): the router must retry "device not found" (it already shows "Waiting for ...").
* Clean data only at >= 5 ms; the 3 ms driver floor and 4 ms/4.5 ms damage are driver/Windows interplay that I cannot tune (lite exposes no event-period control beyond the Control Panel's value, which I did not touch).

## 7. Commands and artefacts

`capture-probe.exe`: `list`, `render-list`, `info`, `info --render`, `format`, `run "Line 1" 12 shared|low`, `record FILE 10 shared|low --device "Line 1"` with a concurrent `tone 8 --target "Line 1" --db -50 --signal noise`, `check FILE --secs 8 --signal noise --db -50`, `loopback-run "Line 1" 16 --keepalive --lowkeep --tone-db -50`. Period experiments used a private build (`scratchpad\tgt`, env `VAC_SPIKE_PERIOD`, gap dump `VAC_SPIKE_GAPS`), both edits reverted from `session.rs` and `capture_probe.rs` (verified by grep). Data: `target\vac_spike\*.wav`, `gaps_*.txt`. VAC manual decompiled to the scratchpad (`chm_out`) for the limitations text.

## 8. Implementation status (added after the investigation)

Status: implemented; fixed-period capture on Line 1, the render keep-alive and the full-chain latency have been run on hardware (see [../latency.md](../latency.md) 5a/5b). Not yet run: a long soak with listening, a live fallback to Shared, and reboot persistence of the capture endpoint. The findings above are unchanged; this section only maps the requirements of section 5 to the code as it is now. Operator steps: [../setup-guide.md](../setup-guide.md#vac-line-1-quick-start); validation: [../latency.md](../latency.md#7-first-run-validation-checklist-vac-line-1); design: [../architecture.md](../architecture.md#5a-fixed-period-capture-corruption-fallback-and-keep-alive).

| Section 5 requirement | Implementation |
|---|---|
| 1. `capture_device` = `Line 1 (Virtual Audio Cable)` | `VAC_CAPTURE_DEVICE`; the `vac` source preset (`Config::apply_source_preset`, GUI preset "VAC Line 1 (low latency)", `router-cli run --source vac`) |
| 2. No `LowLatency` on VAC; a fixed-period mode; integrity check and fallback | `CaptureMode::FixedPeriod` in `wasapi-capture` (default 288 frames = 6 ms, safe floor 5 ms, `allow_unsafe_period` for labs only); `LowLatency` refused on endpoints whose minimum period is below 2.5 ms unless `force_min_period`; `CorruptionMonitor` with `MonitorPolicy::Auto` (stop on trip for fixed-period); the engine (`FallbackState`) reopens once in plain Shared and sets `capture_fallback`; `Engine::reset_fallback` / GUI "Retry fixed period". Config: `capture_period_ms` (0 or 5..20, recommended 6.0) |
| 3. Optional render keep-alive | `RenderKeepAlive` (`render_keepalive`, on in the VAC preset); status `keepalive` (engine period before and after) and `keepalive_note`; non-fatal |
| 4. `match_cable_rate` must not touch VAC | `eq_core::is_vb_cable_name` (a VAC name is never a VB cable); `router-engine/src/cable.rs` is VB-cable-only; the VAC preset also turns `match_cable_rate` off |
| 5. GUI shows the true period and warns about LowLatency | Source section: preset selector (first-switch confirmation, `preset_confirmed`), period control with warnings below 6 ms, keep-alive checkbox, readiness panel (capture and playback endpoint active, Line 1 is the Windows default playback device, formats) with "Open Windows Sound settings", fallback banner with "Retry fixed period", requested versus granted period; per-device learned margin (`jitter_margin_by_device`) |
| Risk: lazy capture endpoint activation | A missing Line 1 capture endpoint is "unavailable", not fatal: "Waiting for Line 1 (Virtual Audio Cable)..." with retry |

Differences from the investigation text: the supported default period is 288 frames (6 ms), not 240; the monitor's rules are (frames faster than real time, repeated identical blocks including a slow rule for a lone duplicate, and sustained packet anomalies; `DATA_DISCONTINUITY` is not used); the MMCSS result of the router work is unrelated to this report.

Model estimates for the router (alternating-gap simulation, not measurements): with 288-frame packets the headroom planner target is about 600 frames and the start target is 784 frames. The totals of section 4 were computed at 5 ms; add 1 to 2 ms for the 6 ms default. `router-cli latency` (end-to-end measurement using the Yamaha's Streaming L/R loopback inputs) is implemented and replaced these estimates: see [../latency.md](../latency.md#5a-end-to-end-measurement-router-cli-latency) section 5a and [5b](../latency.md#5b-what-determines-the-render-side-vac-line-1-render-matrix) (render side 23.8 ms, total about 50 to 52 ms).
