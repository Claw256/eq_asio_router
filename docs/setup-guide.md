# Setup guide (Windows)

Back to the [documentation index](README.md). Commands assume the repository root as the working directory and a release build (`cargo build --release -p eq-asio-app -p router-cli -p wasapi-capture`).

## 1. Prerequisites

| Item | Notes |
|---|---|
| Yamaha Steinberg USB Driver (tested 2.1.9) | Registers the ASIO driver `Yamaha Steinberg USB ASIO`, the default master |
| VB-Audio Hi-Fi Cable | The default source. Donationware, see [development.md](development.md#7-licensing) |
| Optional: Virtual Audio Cable lite 4.70 ("Line 1") | Low-latency source, see the [VAC Line 1 quick start](#vac-line-1-quick-start) and [latency.md](latency.md); private home use only |
| The router binaries | `eq-asio-app.exe` (tray GUI), `router-cli.exe` (headless and diagnostics) |

Nothing needs administrator rights at run time. The router does not install a service or a driver.

## 2. Route Windows audio into the cable

1. Windows Settings > System > Sound > Output: choose `Hi-Fi Cable Input` as the default playback device (or choose it per app in the volume mixer).
2. The router captures `Hi-Fi Cable Output` (config `capture_device`, default `Hi-Fi Cable Output`; a substring of the endpoint name is enough, a full name such as `Hi-Fi Cable Output (VB-Audio Hi-Fi Cable)` also works).
3. Do not set the Yamaha itself as the Windows default output while the router runs: the router holds the Yamaha exclusively through ASIO.

Hi-Fi Cable versus VAC Line 1:

| | Hi-Fi Cable | VAC lite "Line 1" |
|---|---|---|
| Status | Default, works today | Implemented (preset in the GUI, `--source vac` in the CLI); hardware validation pending, see [latency/vac_lite.md](latency/vac_lite.md) |
| Capture cadence (measured) | 480-frame packets in back-to-back pairs every 20 ms | Regular packets; clean at 5 ms or more |
| Router latency (measured / estimate) | model 56.2 ms with a 10 ms render guess; end to end measured 152-164 ms (render side about 100 ms inside the cable driver) | measured end to end 2026-10-10: about 50-52 ms for an ordinary 10 ms shared-mode app; an app that uses IAudioClient3 low-latency shared mode should reach about 32 ms (render side measured, not the full chain; real games not measured). The render side depends on the app, see [latency.md](latency.md#5b-what-determines-the-render-side-vac-line-1-render-matrix) |
| Formats | up to 24 bit, several rates | 48 kHz, 16 bit, 2 ch only |
| Licence | donationware | free for private, non-income use |

Never select `capture_mode = "lowlatency"` with VAC: the 1 ms request corrupts the stream (see [latency/vac_lite.md](latency/vac_lite.md)); the capture layer refuses it on endpoints whose minimum period is below 2.5 ms. The router never changes the Windows format of a VAC endpoint (only VB-Audio cable names are matched); VAC stays at 48 kHz and the resampler handles a Yamaha at another rate.

## VAC Line 1 quick start

Status: implemented and unit-tested; the real fixed-period capture, the render keep-alive and a live fallback have not been validated on hardware yet, and the latency of this path is a model estimate, not a measurement. Run the [first-run validation checklist](latency.md#7-first-run-validation-checklist-vac-line-1) before relying on it. The Hi-Fi Cable preset stays the safe default.

1. Confirm "Line 1" exists under Recording: Windows Sound control panel > Recording tab. If it is missing, right-click the empty area, tick "Show Disabled Devices", right-click "Line 1" and choose Enable. (On the author's machine the endpoint appeared late and unexplained once; the router keeps retrying and shows "Waiting for Line 1 (Virtual Audio Cable)...".) The app's "Source readiness" panel has an "Open Windows Sound settings" button that only opens the Windows Sound page.
2. Set Windows default playback to Line 1: Settings > System > Sound > Output > "Line 1 (Virtual Audio Cable)", or Sound control panel > Playback > right-click Line 1 > Set as Default Device. The router never does this for you. Do not make it the default communications device unless you want call audio routed there.
3. Quit anything else that holds the Yamaha (`router-cli holders`), then in the app's "Source" section choose the preset "VAC Line 1 (low latency)". The first switch shows a one-time confirmation ("Switch source preset?"; remembered as `preset_confirmed` in `%APPDATA%\eq_asio_router\app_settings.toml`). The preset sets capture device `Line 1 (Virtual Audio Cable)`, Shared mode, a 6 ms (288 frame) fixed capture period and the render keep-alive on, and turns "Match cable sample rate" off (VAC is fixed at 48 kHz and is never touched by the router). The engine restarts in about 1-2 s.
4. Press Start (or Restart if it is running). Check the Source section: capture period "requested 288 frames (6.0 ms) = granted 288 frames (6.0 ms)", keep-alive line "Line 1: engine period 10.0 -> 1.0 ms" (expected values, not yet confirmed on hardware), and the readiness rows green.
5. Play music, then press "Learn (30 s)" in the latency panel. The learned margin is stored per capture device (`jitter_margin_by_device`), so the Hi-Fi Cable and Line 1 keep separate values.
6. What the amber fallback banner means: "Capture fell back to Shared (10 ms): <reason>". The corruption monitor saw damaged data from the fixed-period capture (frames faster than real time, repeated blocks or packet anomalies), so the router restarted the capture once in plain Shared. Audio keeps working with the Line 1 cable at about 10 ms packets (the estimated total is then about 39-47 ms rather than 26-34 ms). It does not retry by itself. Press "Retry fixed period" to try again (restarts the stream, 1-2 s), or change the capture period or device, which also re-arms it. If it falls back repeatedly, keep Shared or go back to the Hi-Fi Cable preset and report the reason text. A render keep-alive failure is a separate, non-fatal note.
7. Go back to the Hi-Fi Cable preset: choose "Hi-Fi Cable (compat)" in the same selector (no confirmation after the first one). It restores capture device `Hi-Fi Cable Output`, the driver default period, keep-alive off and "Match cable sample rate" on, and restarts the engine. Then set Windows default playback back to `Hi-Fi Cable Input`. Headless equivalents: `router-cli run --source vac` and `router-cli run --source cable`; individual overrides are `--period-ms F` and `--keepalive` / `--no-keepalive` ([configuration.md](configuration.md#6-router-cli)).

Limits: VAC lite is 48 kHz, 16 bit, stereo, one cable, private non-commercial use. The preset's period and keep-alive controls stay editable; editing them switches the preset label to "Custom". Below 6 ms the app warns; 5 ms (240 frames) is accepted but proved unreliable, and values below 5 ms are raised to 5.

## 3. Make sure nothing else holds the Yamaha

The Yamaha ASIO driver can be opened by one process only. If something else has it, the router shows `Waiting for Yamaha (in use by ...)` and starts as soon as it is free.

```powershell
.\target\release\router-cli.exe holders
```

Typical holders: VB-Audio Matrix, VirtualDJ, the VB ASIO Bridge, a DAW, the Yamaha control panel in a session that opened the device. The command only lists processes that have the driver DLL loaded; it never closes anything. Close the program (or stop its ASIO engine, or choose another device in it).

## 4. Yamaha settings

### Control panel

`ysusb_cp.exe` lives in the Yamaha driver folder (`C:\Program Files (x86)\Yamaha\Yamaha Steinberg USB Driver\`); the router's GUI has an "open control panel" action for the master driver. Relevant settings:

| Setting | Meaning |
|---|---|
| Buffer Size | ASIO buffer, stored as registry `PeriodFrames` (frames at the base rate) |
| Mode | Low Latency / Standard / Stable, stored as registry `LatencyMode` (1 / 2 / 3) |
| USB Mode | SuperSpeed or High-Speed bus mode (not a latency profile) |

Changes are written to the registry only; they apply when the ASIO driver is initialised again. In the router that means Stop and Start, or "Restart engine". Closing the control panel is not enough if the router keeps the driver open.

### Registry

`HKCU\Software\Yamaha\Yamaha Steinberg USB Driver`:

| Value | Meaning | Valid values |
|---|---|---|
| `PeriodFrames` (REG_DWORD) | Buffer size at the base rate (multiplied by 2 at 88.2/96 kHz, by 4 at 176.4/192 kHz) | exactly one of 2048, 1536, 1024, 768, 512, 384, 256, 192, 128, 96, 64, 48, 32, 24, 16, 12, 8, 4 (the panel lists down to 32) |
| `LatencyMode` (REG_DWORD) | 0 = use manual registry values, 1 = Low Latency, 2 = Standard, 3 = Stable | 0..3 |

Prefer the control panel to editing the registry by hand. Details and the other values: [reverse/yamaha_usb_driver.md](reverse/yamaha_usb_driver.md).

### Reported latency (from the reverse report, formula verified against two measured values)

For PCM, the driver reports `latency = PeriodFrames_effective + device term` in samples, where the device term is a per-rate constant (about 0.5 ms: 24 input / 23 output samples at 44.1 and 48 kHz) plus USB buffering chosen by `LatencyMode` and `PeriodFrames` (whole milliseconds on the IRP schedule).

| Period (48 kHz) | Low Latency (in / out add-on samples) | Standard | Stable |
|---|---|---|---|
| 512 | 168 / 215 (see note) | 216 / 359 | 408 / 695 |
| 256 | 120 / 167 (see note) | 168 / 263 | 408 / 503 |

Note: the author's machine reports 280 in / 375 out samples (5.8 / 7.8 ms) at `PeriodFrames` 256 and `LatencyMode` 1. That equals 256 + 24 / 256 + 119, which is the driver's "test" preset row, not the Low Latency row of the table above. So Low Latency selects that row on this device, confirming open question U1 of the reverse report. Read the real value with `router-cli caps` after every change.

Lower is not automatically better: smaller buffers and fewer queued USB requests mean more dropouts under CPU or USB load. Change one setting at a time, restart the engine, check `router-cli caps` and listen. See [latency.md](latency.md) for the order of experiments.

## 5. Run the app

```powershell
.\target\release\eq-asio-app.exe          # window plus tray icon
.\target\release\router-cli.exe run       # headless; Ctrl+C stops
```

GUI flags: `--background` (hidden in the tray and starts the engine; used by Run at login), `--minimized` (hidden, engine start follows the `auto_start` setting), `--no-engine`, `--help`. Only one GUI runs per user session; launching it again shows the running window.

Closing the window hides it to the tray (setting `close_to_tray`). "Quit" in the header or in the tray menu stops the engine, releases the Yamaha and exits.

## 6. Load an AutoEQ profile

1. Download a `ParametricEQ.txt` for your headphones from AutoEQ.
2. GUI: "Load AutoEQ ParametricEQ.txt..." and pick the file. The profile (Preamp plus up to 32 filters of type PK, LSC or HSC; OFF filters are kept but disabled) is stored inside `config.toml`; `profile_source` remembers the path for information only.
3. The EQ changes live. If the profile boosts, "auto headroom" lowers the effective preamp so the maximum boost does not exceed 0 dBFS; a soft limiter is the second safety net. Bypass is a live switch.

## 7. Tray and run at login

- "Run at login" writes the value `EQ ASIO Router` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` with the command `"<path to eq-asio-app.exe>" --background`. No administrator rights, no service (a service would run in session 0 and could not reach your audio devices or the Yamaha).
- At login the tray app starts hidden and starts the engine. If the Yamaha or the cable is not ready yet, it keeps retrying with back-off instead of giving up.
- If you move or rebuild the exe, the entry points elsewhere; the GUI shows this and offers to fix it. Rebuild with the tray app closed (see [development.md](development.md)).
- Remove the entry: untick "Run at login", or delete the value `EQ ASIO Router` from the Run key.

## 8. Cable rate matching

With `match_cable_rate = true` (default) the router sets the Windows device format (Sound > Advanced default format) of every VB-Audio cable endpoint that matches the configured capture device (Input and Output) to the Yamaha's sample rate, once per engine start, using the same mechanism as the Sound control panel (`IPolicyConfig`, no administrator rights). It only touches VB-Audio cable endpoints (names such as "Hi-Fi Cable ...", "CABLE ..." or containing "VB-Audio"), never VAC or a microphone. If it fails, it does not retry in a loop (not again for the same rate within 60 s), audio keeps working through the resampler and the status says how to set it manually (Sound > Recording > cable Properties > Advanced).

Why: a cable at 44.1 kHz and a Yamaha at 48 kHz works (the resampler converts), but matching rates removes the conversion and the exclusive-mode pitfall below.

Disable it: untick the option in the GUI, set `match_cable_rate = false` in `config.toml`, or run `router-cli run --no-match-cable`.

Revert: Sound settings > cable endpoint > Properties > Advanced > choose the previous format (the status line shows "Cable set from X Hz to Y Hz").

## 9. Music apps

- Use shared mode. The router needs nothing from exclusive mode; it already outputs through the Yamaha ASIO driver.
- Windows blocks exclusive playback on `Hi-Fi Cable Input` by default ("Allow applications to take exclusive control" is off). If an app with an "Exclusive mode" option is forced onto it, the Hi-Fi Cable driver does not convert sample rates: 44.1 kHz data into a 48 kHz cable plays 8.8 % too fast with crackle. Turn "Exclusive mode" off in the app (for example Spotify) and keep the output on `Hi-Fi Cable Input`.
- In shared mode Windows converts any app format correctly (measured: 44.1/16, 44.1/float and 96/24 into a 48 kHz cable all give the exact pitch).

Problems: [troubleshooting.md](troubleshooting.md).
