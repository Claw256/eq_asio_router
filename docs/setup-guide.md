# Setup guide (Windows)

Back to the [documentation index](README.md). Commands assume the repository root as the working directory and a release build (`cargo build --release -p eq-asio-app -p router-cli -p wasapi-capture`).

## 1. Prerequisites

| Item | Notes |
|---|---|
| Yamaha Steinberg USB Driver (tested 2.1.9) | Registers the ASIO driver `Yamaha Steinberg USB ASIO`, the default master |
| VBMatrix (VB-Audio Matrix) | The routing matrix and the source: Windows audio goes to the `VBMatrix In` endpoints and VBMatrix feeds the bridge slot. It replaces the former Hi-Fi Cable / VAC sources |
| The "EQ ASIO Router" bridge driver | Registered by `scripts\install-driver.ps1` (see below); VBMatrix loads it in one of its ASIO slots |
| The router binaries | `eq-asio-app.exe` (tray GUI), `router-cli.exe` (headless and diagnostics) |

Nothing needs administrator rights at run time (only the one-time driver registration elevates). The router does not install a service.

## 2. Register the bridge driver and wire up VBMatrix

1. Build and register the bridge DLL (self-elevating; VBMatrix is x64, so the 64-bit DLL is registered by default; `-Both` adds the 32-bit DLL for 32-bit ASIO hosts):

```powershell
cargo build --release -p eq-asio-driver
cargo build --release -p eq-asio-driver --target i686-pc-windows-msvc   # only for -Both
.\scripts\install-driver.ps1                                             # 64-bit; add -Both for 32-bit
```

The DLLs are registered in place under `target\`, so rebuilding later only needs a re-run of the script with VBMatrix closed. `scripts\uninstall-driver.ps1` unregisters (same `-Both` logic).
2. In VBMatrix, pick **"EQ ASIO Router"** in an ASIO slot and patch your `VBMatrix In` strips to it.
3. Keep VBMatrix's master clock on **Internal Clock** and never select "EQ ASIO Router" as a Device Master: the bridge DLL is self-clocked (its own pacing thread) and the router engine refuses to use it as its own master (`master_driver` stays the Yamaha).
4. Windows Settings > System > Sound > Output: choose a `VBMatrix In` endpoint as the default playback device (or choose it per app in the volume mixer).
5. Do not set the Yamaha itself as the Windows default output while the router runs: the router holds the Yamaha exclusively through ASIO.

The pre-bridge Hi-Fi Cable / VAC "Line 1" sources and their quick start were removed with the M3a cutover; their measurements and setup notes are kept in [latency/](latency/) marked **Historical (pre-bridge)**.

## 3. Make sure nothing else holds the Yamaha

The Yamaha ASIO driver can be opened by one process only. If something else has it, the router shows `Waiting for Yamaha (in use by ...)` and starts as soon as it is free.

```powershell
.\target\release\router-cli.exe holders
```

Typical holders: a DAW, VirtualDJ, VBMatrix (if its output device is the Yamaha), the Yamaha control panel in a session that opened the device. The command only lists processes that have the driver DLL loaded; it never closes anything. Close the program (or stop its ASIO engine, or choose another device in it).

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
- At login the tray app starts hidden and starts the engine. If the Yamaha or VBMatrix is not ready yet, it keeps retrying with back-off instead of giving up.
- If you move or rebuild the exe, the entry points elsewhere; the GUI shows this and offers to fix it. Rebuild with the tray app closed (see [development.md](development.md)).
- Remove the entry: untick "Run at login", or delete the value `EQ ASIO Router` from the Run key.

## 8. Rates and formats (the router changes nothing in Windows)

The cable rate matching of the pre-bridge sources was removed with the M3a cutover: the router never changes any Windows device format any more. VBMatrix's endpoints keep whatever format Windows gives them; the bridge slot negotiates its own rate and buffer with VBMatrix, and the resampler handles any difference to the Yamaha's rate (44.1 / 48 / 96 kHz ...). If you want a different `VBMatrix In` format, set it yourself (Sound > endpoint Properties > Advanced).

## 9. Music apps

- Use shared mode. The router needs nothing from exclusive mode; it already outputs through the Yamaha ASIO driver.
- Windows blocks exclusive playback on the `VBMatrix In` endpoints by default ("Allow applications to take exclusive control" is off). If an app with an "Exclusive mode" option is forced onto one and at a rate different from the endpoint's format, the pitch can be wrong. Turn "Exclusive mode" off in the app (for example Spotify) and keep its output on `VBMatrix In`.
- In shared mode Windows converts any app format correctly, and VBMatrix handles the routing.

Problems: [troubleshooting.md](troubleshooting.md).
