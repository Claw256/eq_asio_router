# Troubleshooting

Back to the [documentation index](README.md). Logs first: `%TEMP%\eq_asio_router_engine.log` (engine, rotated at about 1 MB, one old file kept). The tray GUI status panel and `router-cli run` show the same state live.

## Symptom, cause, fix

| Symptom | Cause | Fix |
|---|---|---|
| `Start failed: cannot open ASIO driver "Yamaha Steinberg USB ASIO": driver init failed (is another program using it?)` | The driver's `init` failed. Most often another process has the Yamaha open but holder detection found nothing (the check looks for the driver DLL in other processes), the interface was released a moment ago (the driver enforces a short cooldown and retries for about 2.5 s), or the interface is switched off or unplugged | Run `router-cli holders`; close VirtualDJ or any other ASIO host that holds the Yamaha, or VBMatrix if another slot holds it; check USB and power; press Start again. The tray app started at login retries by itself |
| `Waiting for Yamaha (in use by <program> (PID n))`, state `WaitingForDevice` | A listed process holds the Yamaha exclusively. The router retries every 2 s and starts as soon as it is free | Close that program, or stop its ASIO engine, or pick another device in it. "Retry now" skips the 2 s wait. The router never closes other programs by itself; the GUI offers an "Ask <exe> to close" button per holder that only posts a normal window-close request when you click it |
| Crackle or clicks | (a) Old builds: the drift controller was replaced; rebuild. (b) Rate mismatch in an app (next rows). (c) Ring underruns because the margin is too small or the Yamaha period too aggressive. (d) Another program loading the CPU or USB | Check the status: `underruns`, `gate_rearms`, `xrun`, `fault`. Raise `safety_buffer_ms` or `[bridge] ring_target_ms`, or run "Learn (30 s)"; raise the Yamaha `PeriodFrames`; close CPU-heavy programs |
| Music sounds sped up or distorted; a player has "Exclusive mode" | Exclusive-mode playback bypasses the Windows mixer onto a VBMatrix In endpoint at a different rate, or nothing reaches the slot | Turn "Exclusive mode" off, output to the VBMatrix In endpoints, and check the VBMatrix slot shows signal |
| Hardening row `MMCSS ASIO` shows FAILED (log: "mmcss ... asio FAILED") | Windows error 1552 (`ERROR_INVALID_TASK_INDEX`) means the thread already has an MMCSS registration: the Yamaha driver registers its own callback thread, so the router's second registration is refused. It is harmless: the thread already runs under "Pro Audio" | Nothing to do. `crates\router-engine\src\priority.rs` now counts 1552 as OK; the row turns green after you rebuild and restart the app. Any other error code is a real failure |
| Other hardening rows `FAILED` or `pending` | `pending` is normal until the first ASIO callback and the first capture packet. A real `FAILED` means Windows refused the call: `EcoQoS opt-out` or `Priority class` (process settings, policy may forbid), `1 ms timer` (`timeBeginPeriod` refused), `MMCSS control` / `MMCSS capture` (MMCSS service disabled or the "Pro Audio" task missing from the registry) | Check that the Multimedia Class Scheduler service is running. A failed row reduces robustness against Windows throttling; audio still works. Priority class is only raised when the process runs at Normal priority; a higher class chosen by you is left alone and counts as OK |
| Device missing at login (`Waiting for the bridge (VBMatrix)...` or `Waiting for ASIO driver ...`) | The bridge mapping appears only when the driver is loaded by VBMatrix | Start VBMatrix and open the ASIO slot; the tray app retries with back-off (200 ms doubling to 5 s) until it appears |
| Tray icon not visible | Windows hides new tray icons in the overflow ("^") | Drag it out of the overflow, or Settings > Personalization > Taskbar > Other system tray icons. Launching `eq-asio-app.exe` again shows the running window (single instance) |
| `another router engine instance is already running` (`AlreadyRunning`) | The tray app (or another `router-cli run`) already owns the engine mutex `Local\eq_asio_router_engine` | Use the running instance, or quit it (tray menu > Quit) before starting another. Rebuilding also needs the tray app closed |
| Second launch of the GUI does nothing visible | Single instance: it asked the first instance to show its window | Look in the tray; use the tray menu |
| `ASIO driver "X" is not registered` | The `master_driver` name does not match a registered ASIO driver | `router-cli devices` lists the names |
| `master driver "EQ ASIO Router" is not allowed` | The virtual bridge driver must never be opened as master (ForbiddenMaster) | Set `master_driver` to the Yamaha or leave it empty |
| `master has 1 output channel(s); at least 2 are required` / `output sample type N is not supported` | Unsuitable master | Choose another driver or `output_channels`; MSB sample types are not supported |
| Audio stops and the state shows `Restarting` with a reason | Self-healing: stalled callback (1 s) or capture (3 s), driver reset request, device lost | Read the reason and the log. Frequent restarts point at USB trouble, a busy cable or a failing driver |
| `config_error` in the status | `config.toml` does not parse; the last good config stays in force | Fix the TOML syntax; the engine reloads on save |
| Silence, state Running, peaks at -inf | Nothing is patched to the slot in VBMatrix (or the slot is stopped), or Windows default playback is not a VBMatrix In endpoint | Check the VBMatrix matrix has a source routed to the "EQ ASIO Router" slot and the slot is started (Device Master checked); check the bridge panel fills (ring A) in the GUI |

## Log locations

| File | Content |
|---|---|
| `%TEMP%\eq_asio_router_engine.log` | Engine: start, hardening summary, capture mode and period, capture fallback / corruption reasons, render keep-alive start (engine period before and after), restarts and reasons, waiting-for-device, learn results |
| `%TEMP%\eq_asio_router_driver.log` | The virtual bridge driver (loaded inside VBMatrix): pacing, shm and callback diagnostics |
| `%APPDATA%\eq_asio_router\caps.toml` | What the master reported at the last start |

## Reverting changes

| Change | How to revert |
|---|---|
| Run at login | Untick "Run at login" in the GUI, or delete the value `EQ ASIO Router` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` |
| Yamaha latency settings | Yamaha control panel: set Mode to Standard and Buffer Size back (the original values on the author's machine were 512 and Standard, before tuning); the router does not write these registry values itself |
| EQ profile | Bypass in the GUI, or delete the `[profile]` section of `config.toml` |
| All router settings | Close the app, delete `%APPDATA%\eq_asio_router\` (the router recreates defaults) |
| Old virtual driver registration (only if you ever installed it) | `scripts\uninstall-driver.ps1` (self-elevating) |
