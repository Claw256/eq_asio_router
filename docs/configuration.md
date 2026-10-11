# Configuration and command-line reference

Back to the [documentation index](README.md). Source of truth: `crates/eq-core/src/config.rs`, `crates/eq-core/src/status.rs`, `crates/router-engine/src/status.rs`, `crates/eq-asio-app/src/settings.rs`.

## 1. Files

| File | Written by | Purpose |
|---|---|---|
| `%APPDATA%\eq_asio_router\config.toml` | GUI, or by hand | Router configuration (section 2). Saved atomically. Hot reload: the engine polls the modification time |
| `%APPDATA%\eq_asio_router\app_settings.toml` | GUI | Tray and startup behaviour (section 3) |
| `%APPDATA%\eq_asio_router\caps.toml` | engine | Capabilities of the master driver (buffer sizes, rates, latencies, channel names), for the GUI |
| `%APPDATA%\eq_asio_router\latency_measured.toml` | `router-cli latency` | Stored end-to-end measurements (one `[[entries]]` per capture device, keep-alive state, capture period, ASIO buffer and render profile: median render side, inj->capture, capture->tap, total, date, engine parameters). Read by the engine once per session so that the latency readout shows the measured render side instead of an estimate; entries older than 30 days or for a different source or engine setting are ignored. Safe to delete |
| `%TEMP%\eq_asio_router_engine.log` | engine | Log, rotated at about 1 MB (current plus one old file) |

Missing fields use their defaults, so old files keep loading. A file that does not parse is ignored (the last good config stays in force) and the error is shown as `config_error`.

## 2. `config.toml`

All values are passed through `Config::sanitized()`; out-of-range numbers are clamped and NaN/infinity is replaced by the default.

| Field | Type | Default | Meaning | Sanitized range |
|---|---|---|---|---|
| `master_driver` | string | `""` (= `Yamaha Steinberg USB ASIO`) | Name of the ASIO driver to host. `EQ ASIO Router` (frozen v1 driver) is refused | any non-empty name, else default |
| `output_channels` | `[u32, u32]` | `[0, 1]` | Master output channel indexes that receive the stereo pair | both must exist and differ, else `[0, 1]` |
| `passthrough_inputs` | bool | `true` | From the v1 driver. Carried into the processor settings, but the router creates output buffers only, so it has no audible effect | |
| `bypass` | bool | `false` | Bypass the whole chain, AutoEQ and JBL Bass Mode (live) | |
| `eq_enabled` | bool | `true` | Apply the AutoEQ stage (preamp and bands). `false` crossfades only that stage to dry over ~10 ms (live); JBL Bass Mode and the limiter keep working | |
| `limiter` | bool | `true` | Soft limiter after the EQ, knee about -1 dBFS (live) | |
| `profile_source` | string, optional | absent | Where the profile came from (informational) | |
| `profile` | table | empty | Active EQ profile: `name`, `preamp_db`, `[[profile.filters]]` | see below |
| `sample_rate` | u32 | `0` | Engine rate in Hz; 0 keeps the master's current rate | 0 or nearest of 44100, 48000, 88200, 96000, 176400, 192000 |
| `safety_buffer_ms` | f32 | `2.0` | Extra ring prefill (latency versus robustness) (live) | 0..20 |
| `safety_buffer_auto` | bool | `true` | Let the engine raise the safety buffer after underruns, total at most 20 ms (live) | |
| `capture_device` | string | `Hi-Fi Cable Output` | WASAPI capture endpoint name (substring match); empty becomes the default | non-empty |
| `capture_mode` | `"shared"`, `"lowlatency"`, `"exclusive"` | `"shared"` | Capture mode. `lowlatency` asks for the smallest IAudioClient3 period (no gain on the Hi-Fi Cable; unsafe on VAC). `exclusive` is mapped to `shared` with a note, because the Hi-Fi Cable driver corrupts exclusive capture | |
| `resampler_quality` | `"linear"`, `"cubic"` | `"cubic"` | Resampler kernel: linear, or 32-tap Kaiser sinc (live) | |
| `auto_headroom` | bool | `true` | Lower the effective preamp so the profile's maximum boost never exceeds 0 dBFS (live) | |
| `mute_on_fault` | bool | `true` | Fade-mute on detected faults | |
| `match_cable_rate` | bool | `true` | Set the cable endpoints' Windows format to the master's rate once per start. Enabling it while running restarts the engine | |
| `jitter_margin_ms` | f32 | `0.0` | Legacy single learned value, kept for older readers (see `jitter_margin_by_device` below). The engine reads the margin through `Config::jitter_margin_for(capture_device)`: the per-device entry, or this value only while the per-device map is empty. 0 = not learned (start conservatively, measure live). When set, the engine starts at it and never goes below it (live) | 0..40 |

Restart-class changes (engine restarts by itself): `master_driver`, `sample_rate`, `capture_device`, `capture_mode`, `capture_period_ms`, `render_keepalive`, `output_channels`, turning `match_cable_rate` on. Everything else is applied live.

Profile fields:

| Field | Meaning | Sanitized range |
|---|---|---|
| `profile.name` | Display name | |
| `profile.preamp_db` | Preamp in dB | -40..+12 |
| `[[profile.filters]]` `kind` | `PK` (peaking), `LSC` (low shelf), `HSC` (high shelf), `LP`, `HP` | |
| `fc_hz` | Center/corner frequency | 10..24000 |
| `gain_db` | Gain | -24..+24 |
| `q` | Q | 0.1..20 (shelves 0.1..4) |
| `enabled` | Filter on/off | |

At most 32 filters are kept; the rest are cut. AutoEQ `ParametricEQ.txt` lines such as `Filter 1: ON PK Fc 31 Hz Gain 1.5 dB Q 1.41` and `Preamp: -6.4 dB` are parsed by `eq-core::autoeq`.

### Low-latency source fields (VAC Line 1)

Status: implemented in `eq-core` (config), `router-engine` (engine, status), `wasapi-capture` (capture layer) and the GUI (`crates/eq-asio-app/src/panels/source.rs`); unit-tested and building. Hardware validation (real fixed-period capture, keep-alive, live fallback) is pending, so treat the behaviour as unproven on real devices and the VAC latency as a model estimate. Design and measurements: [latency/vac_lite.md](latency/vac_lite.md). All fields are additive; old files load unchanged.

| Field | Type | Default | Meaning | Sanitized range |
|---|---|---|---|---|
| `capture_period_ms` | f32 | `0.0` | Fixed shared capture period (IAudioClient3 `InitializeSharedAudioStream`) for a known virtual cable (names matching Virtual Audio Cable or a VB-Audio cable). 0 = the driver's default shared period. Ignored, with a `capture_note`, for other endpoints and in `lowlatency` mode. Recommended 6.0 (288 frames at 48 kHz); 5.0 (240 frames) is the accepted minimum but proved unreliable. Constants: `RECOMMENDED_CAPTURE_PERIOD_MS` 6.0, `MIN_CAPTURE_PERIOD_MS` 5.0, `MAX_CAPTURE_PERIOD_MS` 20.0. Restart-class | 0, or 5.0..20.0 (values between 0 and 5 are raised to 5; VAC lite corrupts audio below 5 ms; NaN becomes 0) |
| `render_keepalive` | bool | `false` | Run a silent minimum-period render stream on the cable's paired render endpoint so apps rendering into it see a 1 ms engine period (VAC Line 1: 10 ms to 1 ms expected). Only on known virtual cables, never on real outputs; failure is non-fatal (`keepalive_note`). Restart-class | |
| `source_preset` | `"cable"`, `"vac"`, `"custom"` | `"cable"` (files without it: `cable` if `capture_device` contains "Hi-Fi", else `custom`) | Informational label of where the settings came from; it changes no behaviour by itself. `Config::apply_source_preset` copies preset values into the individual fields: `vac` = `capture_device` `Line 1 (Virtual Audio Cable)` (`VAC_CAPTURE_DEVICE`), `capture_mode` shared, `capture_period_ms` 6.0, `render_keepalive` on, `match_cable_rate` off; `cable` = `Hi-Fi Cable Output`, shared, period 0, keep-alive off, `match_cable_rate` on; `custom` only sets the label. Loading an old file never applies a preset. Editing a field in the GUI switches the label to `custom` | |
| `jitter_margin_by_device` | table name to f32 | empty (not written when empty) | Learned jitter headroom per capture device (case-insensitive lookup), 0..40 ms, at most 16 entries, blank names dropped. `Config::sanitized` migrates the legacy `jitter_margin_ms` into this map under the current `capture_device` while the map is empty; `Config::set_jitter_margin` (used by "Learn (30 s)") writes the entry for the current device and mirrors it into `jitter_margin_ms` | 0..40 |

Helper functions and constants in `eq-core`: `is_vac_name` (name contains "virtual audio cable"), `is_vb_cable_name` (VB-Audio names; a VAC name is never one), `frames_for_ms(ms, rate)`, `DEFAULT_CAPTURE_DEVICE`, `VAC_CAPTURE_DEVICE`, `SourcePreset::{label, parse}` (`parse` also accepts `hifi`, `hi-fi`, `line1`).

Status fields (in `EngineStatus`, not in the 128-byte status block, which is unchanged at layout version 2):

| Field | Meaning |
|---|---|
| `capture_period_frames_requested` | Period the config asks for, in frames at the capture rate (0 = driver default). Kept while a fallback is active |
| `capture_period_frames_granted` | Period actually granted by WASAPI (same value as `capture_period_frames`) |
| `capture_fallback` | `Capture fell back to Shared (10 ms): <reason>` after the corruption monitor tripped a fixed-period capture and the engine reopened it once in plain Shared. Cleared when the capture device or `capture_period_ms` changes, or by `Engine::reset_fallback()` (GUI button "Retry fixed period") |
| `keepalive` | While the keep-alive runs: render endpoint name and its engine period before and after, in ms |
| `keepalive_note` | Why a requested keep-alive is not running, or a remark about it (non-fatal) |
| `source_preset` | Preset label of the config in force |

`capture_mode` in `config.toml` stays `shared`, `lowlatency` or `exclusive`; fixed-period capture is not a config value but the result of `capture_period_ms > 0` in Shared mode on a known virtual cable (it shows up as capture mode in use "fixed-period (IAudioClient3)"). Never use `lowlatency` on VAC (the capture layer refuses an IAudioClient3 minimum below 2.5 ms unless forced in a lab tool). VAC endpoints are never touched by `match_cable_rate`: only VB-Audio cable names count as cables to match (`is_vb_cable_name`).

### JBL Bass Mode

`jbl_bass_mode` (bool, default `false`) turns the car-subwoofer effect on (bass shelf, resonance, clipping distortion, bass-keyed pumping, lo-fi) after the EQ. `jbl_intensity` (0..100, default 80) is the master scaler. The `[jbl]` table exposes the individual stages. Every value in it is the endpoint reached at 100 % Intensity; lower Intensity interpolates from the idle (neutral) value toward it. The defaults were tuned against measurements (see `crates/eq-core/tests/jbl_audit.rs`): the pump depth, release and attack sit in the usual sidechain ranges, and the resonance is a broad box-like boost rather than a narrow ring. A missing `[jbl]` table or key uses the default. Out-of-range values are clamped by `Config::sanitized()`; NaN/infinity becomes the default. Configs saved by an earlier version carry their old explicit `[jbl]` values (previous defaults: `res_db = 9.0`, `res_q = 3.0`, `drive_max = 5.0`); delete those keys or use "Reset to defaults" to adopt the new ones. All changes apply live.

| Key | Default | Range | Meaning |
|---|---|---|---|
| `duck_on` | `true` | | Pumping: lower the level on each bass hit |
| `duck_depth` | `2.0` | 0..6 | How far the level is pulled down |
| `duck_attack_ms` | `5.0` | 0.5..50 | Duck attack time |
| `duck_release_ms` | `120.0` | 10..800 | Duck release time |
| `detect_hz` | `100.0` | 40..250 | Bass detector frequency that triggers the pumping |
| `shelf_hz` | `70.0` | 30..200 | Low-shelf corner frequency |
| `shelf_db` | `12.0` | 0..18 | Low-shelf boost |
| `res_on` | `true` | | Enable the resonant boom |
| `res_hz` | `50.0` | 25..120 | Resonance centre frequency |
| `res_db` | `6.0` | 0..18 | Resonance gain |
| `res_q` | `2.0` | 0.5..10 | Resonance Q (higher is narrower) |
| `drive_max` | `4.0` | 1..12 | Clipping drive |
| `bias_max` | `0.35` | 0..0.8 | Clipping asymmetry (adds even harmonics) |
| `kick_on` | `true` | | Enable the extra kick-band saturation |
| `kick_hz` | `130.0` | 60..300 | Kick band upper edge |
| `kick_drive_max` | `2.2` | 1..6 | Kick-band drive |
| `crush_on` | `true` | | Enable the bit crusher |
| `crush_bits_min` | `10.0` | 6..16 | Bit depth at 100 % |
| `hold_on` | `true` | | Enable sample hold |
| `hold_max` | `3.0` | 1..8 | Hold length at 100 % |
| `lpf_on` | `true` | | Enable the treble low-pass |
| `lpf_hz_start` | `14000.0` | 2000..20000 | Low-pass cutoff at low Intensity |
| `lpf_hz_end` | `7500.0` | 2000..20000 | Low-pass cutoff at 100 % |
| `lfo_on` | `false` | | Enable the rattle LFO (slow level wobble) |
| `lfo_rate_hz` | `6.0` | 0.5..20 | LFO rate |
| `lfo_depth` | `0.3` | 0..1 | LFO depth (0 = no effect) |
| `lfo_follow_bass` | `true` | | Scale the LFO by the bass envelope so it only wobbles on hits |
| `lfo_target` | `"Gain"` | `"Gain"`, `"BassBand"`, `"Both"` | Modulate output gain, the kick band level, or both |
| `out_trim_db` | `0.0` | -12..+6 | Output trim, added to the automatic loudness compensation |

Example:

```toml
jbl_bass_mode = true
jbl_intensity = 80

[jbl]
shelf_hz = 60.0
lfo_on = true
lfo_target = "Both"
```

The GUI shows these under Fun > Advanced (greyed out while the mode is off), with a "Reset to defaults" button.

## 3. `app_settings.toml`

| Field | Default | Meaning |
|---|---|---|
| `auto_start` | `false` | Start the engine when the app launches |
| `close_to_tray` | `true` | The window close button hides to the tray instead of quitting |
| `start_minimized` | `false` | Start with the window hidden (`--background` and `--minimized` force it for one launch) |
| `run_at_login` | `false` | Last known choice only; the registry Run value is the truth and is re-read at startup |
| `preset_confirmed` | `false` | Set once you confirm the first source-preset switch ("Switch source preset?" dialog); later switches apply without asking |

GUI command line: `--background` (hidden, engine starts), `--minimized` (hidden), `--no-engine` (never auto-start, for testing), `-h` / `--help`.

## 4. Single-instance and kernel object names

| Name | Owner |
|---|---|
| `Local\eq_asio_router_engine` | engine mutex (one engine per user session) |
| `Local\eq_asio_router_gui`, `Local\eq_asio_router_gui_show` | GUI mutex and show-event |
| `Local\eq_asio_router_status` | status block mapping |

## 5. Status block

Shared memory `Local\eq_asio_router_status`, 128 bytes, magic `EQST`, layout version 2, written by the engine (relaxed atomic stores from the audio thread), read with `StatusReader`.

| Field | Type | Meaning |
|---|---|---|
| `magic`, `version` | u32 | `EQST`, 2 |
| `sample_rate`, `buffer_size` | u32 | ASIO rate and buffer size |
| `in_latency`, `out_latency` | u32 | Driver latencies in samples |
| `callbacks`, `xruns` | u64 | ASIO callbacks; ASIO overload reports |
| `peak_in_l/r`, `peak_out_l/r` | f32 | Peaks before and after the EQ |
| `running`, `pid` | u32 | Non-zero while streaming; engine process id |
| `fault_state` | u32 | 0 Ok, 1 Warn, 2 Muted |
| `effective_rate` | u32 | Rate in use (Hz) |
| `non_finite` | u32 | Non-finite samples sanitized |
| `ring_fill_frames`, `underruns`, `overruns` | u32 | Ring fill; ring underruns and overruns |
| `max_callback_us`, `avg_callback_us` | u32 | Callback duration |
| `capture_period_us`, `safety_buffer_us` | u32 | Capture period; effective safety buffer |
| `dropped_logs` | u32 | Log lines dropped by the logger ring |

`EngineStatus` (in-process, richer; `crates/router-engine/src/status.rs`) adds, among others: `state` (`Stopped`, `Starting`, `Running`, `Restarting{reason}`, `WaitingForDevice{holders}`, `Failed`), `cable_rate`, `ring_target_frames`, `ring_mean_fill_frames`, `drift_ppm`, `gate_rearms`, `discarded_frames`, capture mode requested/in use, `capture_burst_max_frames`, CPU percent of the callback period, `jitter_headroom_frames` / `required_headroom_frames`, arrival gap and dip p99.9, `capture_staleness_ms`, `learn` progress and result, `hardening` (six hardening steps), `restarts`, `last_error`, `busy_holders`, `capture_error`, `capture_note`, `cable_rate_changed_from`, `cable_rate_note`, capture packet / discontinuity / silent-packet counters, `config_error`, `uptime_ms`, and the latency breakdown (master output, ring, capture, render-side estimate, total).

## 6. `router-cli`

```text
router-cli devices
router-cli caps [--master NAME]
router-cli holders [--master NAME]
router-cli --help | -h | help          (also after any subcommand: prints usage, exit 0)
router-cli run (--seconds N | --forever) [--config PATH] [--master NAME] [--capture NAME] [--rate HZ]
               [--safety-ms F] [--preamp-db F] [--bypass] [--mode shared|low|exclusive]
               [--learn] [--no-match-cable] [--dump-dir DIR]
               [--source vac|cable] [--period-ms F] [--keepalive|--no-keepalive]
               [--synthetic RATE[:PACKET[:JITTER_MS[:FREQ,..]]] [--synthetic-db DB]]
router-cli latency [--sink NAME] [--count N] [--interval-ms M] [--db -40] [--config PATH]
                   [--source vac|cable] [--no-match-cable] [--allow-sink NAME] [--shape burst|chirp] [--json]
                   [--cap-threshold-db X] [--lb-threshold-db X] [--hp-hz N|off]
router-cli latency --baseline [--seconds 3] [--dump FILE.wav]
router-cli latency --selftest
router-cli analyze FILE [--freq HZ] [--skip S] [--tail S] [--channel N] [--mads X]
```

| Command | What it does | Side effects |
|---|---|---|
| `devices` | Lists ASIO drivers, capture and playback endpoints (cable endpoints with their rate) | none |
| `caps` | Briefly opens the master, prints channels, rates, buffer sizes and latencies, releases it | opens the Yamaha briefly (fails if it is in use) |
| `holders` | Lists processes that have the master driver DLL loaded (PID, exe, path) | none; never closes anything |
| `run` | Runs the engine headless, prints a status line per second; exit code 2 if `--seconds` elapses while the master is still held by another program | plays audio to the Yamaha; may change the cable's Windows rate (use `--no-match-cable` to avoid) |
| `latency` | End-to-end click latency: injects a very quiet click into the cable's playback endpoint, runs it through the real router and detects it on the Yamaha's loopback inputs (details below and in [latency.md](latency.md#5a-end-to-end-measurement-router-cli-latency)); `--selftest` needs no hardware | **opens the Yamaha** (does not start and exits with code 2 if another program holds it) and plays a click of at most -35 dBFS into a virtual cable; the router output is about -55 dBFS |
| `analyze` | Sine-fit glitch analysis of a recorded WAV (THD+N, glitches, phase steps, clipping, zero runs) | reads a file only |

Command-line safety: the whole command line is validated before anything touches a device. An unknown flag or subcommand is an error (usage on stderr, exit code 2) and never starts the engine; `--help`, `-h` and `help` print the usage and exit 0. `run` must be bounded: give `--seconds N` or, for an unbounded run (stop with Ctrl+C), `--forever` explicitly (the two are mutually exclusive). `devices`, `caps`, `holders`, `latency` and `analyze` behave as before, apart from rejecting unknown flags.

`run` details: without `--config` it uses `%APPDATA%\eq_asio_router\config.toml` if present. If the file does not set `capture_mode`, shared mode is used. Source flags (override the file for this run only): `--source vac` applies the VAC preset (capture `Line 1 (Virtual Audio Cable)`, shared, 6 ms fixed period, keep-alive on, no Windows rate change), `--source cable` applies the Hi-Fi Cable defaults (`custom` is not accepted here); `--period-ms F` sets `capture_period_ms` (0 = driver default, otherwise 5..20); `--keepalive` / `--no-keepalive` set `render_keepalive`. `--source` is applied first, so the other flags can override single values; `--capture` without `--source` marks the preset as `custom` (the period and keep-alive flags leave the label alone). At start `run` prints the source summary, the capture line (mode, requested versus granted period), a `WARNING:` line for a fallback and the keep-alive line; the per-second status line ends with `src <preset> period <granted>/<requested> fr keepalive on|off`. `--learn` waits 5 s, then runs the 30 s jitter measurement and prints the margin to put in `jitter_margin_ms`. `--dump-dir` writes `capture_in.wav`, `after_resample.wav`, `after_eq.wav` and a report (a measurement tap). `--synthetic` replaces the capture by a generated tone (default -30 dBFS, 1 kHz) so rate and packet combinations can be tested without touching Windows settings. `--rate`, `--safety-ms` etc. override the file for this run only.

`latency` details: loads the config like `run` (`--config`, `--source`, `--no-match-cable` behave the same) and starts the real engine with the preamp forced to -20 dB, the limiter on, the EQ not bypassed, no config watcher (nothing can change the level during the run) and no status mapping or caps file written. The click goes into `--sink` (a name or substring of a playback endpoint); by default the render endpoint paired with the configured capture device (`Hi-Fi Cable Input` for the cable source, `Line 1 (Virtual Audio Cable)` for `--source vac`). Safety rules enforced in code: `--db` defaults to -40 and anything above -35 is refused (also in the library); the sink must be a known virtual cable (`is_virtual_cable_name`), otherwise the command refuses unless `--allow-sink NAME` names it explicitly; `--count` is at most 10 (default 5); `--interval-ms` 500..5000 (default 1200); each click waits at most 4 s for detection; before the first click a 1 s quiet baseline is measured at both detectors (see "Baseline and thresholds" below) and the run aborts if the router output is above -70 dBFS, if anything above -60 dBFS arrives at the capture sink (something else is playing into the cable), if the loopback carries programme audio (deviation above -40 dBFS, or above -20 dBFS only for a burst of at least 15 kHz with the high-pass detector) or if the noise floor is too high to detect the click at the loopback; the message names the detector and the measured levels; the run aborts at once if the router output peak exceeds -40 dBFS; Ctrl+C stops cleanly. The Yamaha is checked first with the same holder detection as `holders`: if the tray app or anything else has it, nothing is started, nothing is closed, and the command prints that the live run is pending and exits with code 2. `--shape` selects `burst[:HZ[:MS]]` (default 1500 Hz, 2 ms raised cosine) or `chirp[:F0:F1[:MS]]`; `--cap-threshold-db` and `--lb-threshold-db` override the detector thresholds (the defaults come from the measured baseline, below); `--hp-hz N|off` sets or disables the loopback detector high-pass (automatic when the lowest frequency of the click is at least 15 kHz, e.g. `--shape burst:18500:2` gives a 15.75 kHz corner); `--json` appends one JSON object with every impulse and the statistics. **Render stream options:** `--render-mode shared-default|shared-lowlatency|shared-min-buffer`, `--render-buffer-frames N`, `--render-fill full|period|N` and `--render-prefill full|period|N` choose what kind of application the click imitates (default: a typical 10 ms shared-mode client that writes one period per event and keeps one period queued); the run prints the stream's buffer, period, the endpoint's engine period before and after and the padding at submit time. **Saving:** after a successful run with at least 3 clicks the result is written to `%APPDATA%\eq_asio_router\latency_measured.toml` (additive; `--no-save` never writes; a non-default render stream is written only with `--save`, under its own render profile); the GUI latency panel, the tray tooltip and the `router-cli run` status line then use the measured render side for that source and label it `measured <date>`, otherwise they show an estimate (20 ms VAC, 100 ms Hi-Fi Cable) labelled as such. The output is a per-impulse table, min / median / mean / max / sigma per figure, outlier flags and a breakdown against the latency panel's model; see [latency.md](latency.md#5a-end-to-end-measurement-router-cli-latency) for the definitions and for how to read it. **Baseline and thresholds.** Before the clicks the command measures 1 s of both detector inputs with nothing injected: per channel peak, RMS, DC offset (mean) and the robust noise floor (99th percentile over blocks of the per-block deviation from the mean). The thresholds follow from that, not from fixed offsets: capture `max(click - 20, floor + 12)` dBFS, loopback `max(click + preamp - 25, floor + 12)` dBFS, where the loopback floor is the deviation from the measured mean (the detector subtracts that DC offset first, so DC cannot trigger it). A threshold must stay at least 6 dB below the expected click level (capture: `click`; loopback: `click + preamp`, and with the high-pass detector plus the envelope gain of the click shape, about -4 dB for `burst:18500:2`), otherwise the run aborts with the computed requirement (with the -20 dB preamp a -35 dBFS click reaches the loopback at about -55 dBFS, so the floor must be below -73 dBFS; for -40 dBFS below -78 dBFS). Safety aborts stay: router output above -40 dBFS, anything above -60 dBFS at the capture sink, loopback deviation above -40 dBFS (relaxed to -20 dBFS only for `--shape burst:<HZ >= 15000>` with the high-pass active, where the run prints a `*** SAFETY NOTE` line with the measured loopback peak; the router output limit of -40 dBFS is never relaxed). A detected loopback crossing whose level is more than 12 dB from the expected click level is ignored as a stray event and the detector is re-armed. **High-pass detector:** the Yamaha "Streaming" loopback mixes whatever the mixer sends into it, typically the microphone inputs, so on a machine with a live microphone it carries broadband noise (measured: about -46 dBFS RMS, -33 dBFS peak, band-limited to about 12 kHz) that hides any click at -55 dBFS. With a high-frequency click (`--shape burst:18500:2`) the loopback detector looks through an 8th-order Butterworth high-pass followed by a 32-sample sliding RMS (which rejects single-sample spikes), where the same loopback measured -87 dBFS RMS; the onset bias of that detector is computed by running the same filter on the nominal click. **`latency --baseline`** (`--seconds 0.5..10`, default 3; `--dump FILE.wav` writes the first 1.4 s of the loopback pair as a float WAV) starts the engine the same way (preamp -20 dB, limiter on) but renders nothing and writes nothing to any endpoint, and prints per channel: peak, RMS, AC RMS, DC offset, raw minimum / maximum (left-aligned Int32), the fraction of blocks with a peak at or above -90 / -80 / -70 / -60 dBFS, steady or time-varying, the high-passed levels, the strongest spectral peaks, the L/R correlation, the read-only state of the interface settings file (`ur22c_setting.xml`: age and the few plain fields; the mixer parameters are an unnamed binary blob and cannot be decoded, and the file is the last save of the app, not live state), and for click levels -40 and -35 dBFS whether a full-band 1.5 kHz click and a high-frequency click are detectable. Library API: `router_engine::baseline` (`Monitor`, `plan_thresholds`, `HighPass`), `LatencyProbe::{monitor_start, monitor_stop, monitor_hp, set_highpass, arm_with_dc}`.

`latency --selftest` runs the detectors and the timing maths against a synthetic loopback with known delays (three buffer geometries, several phases) and checks the safety guards; it never opens a device. Library API: `EngineOptions::loopback_probe` (`LoopbackProbe`) plus `Engine::latency_probe()` (`LatencyProbe::arm`, `take_result`, `info`), and `wasapi_capture::{ImpulseRenderer, render_impulses, ImpulseTrain, click_samples}`.

## 7. `capture-probe`

Built from `crates/wasapi-capture` (`cargo build --release -p wasapi-capture`). Names are substrings of the endpoint name. Commands:

| Command | Purpose | Safety |
|---|---|---|
| `list` | Capture endpoints | read-only |
| `render-list` | Render endpoints | read-only |
| `info [name]`, `info --render [name]` | Formats, device periods, IAudioClient3 engine periods, exclusive support | read-only |
| `format [name]` | Current Windows device format of an endpoint | read-only |
| `set-rate <name> <hz>` | Sets the Windows device format of all endpoints matching the cable base name (Input and Output) | **Changes Windows settings.** Revert in Sound > Properties > Advanced |
| `run [name] [seconds] [shared\|low\|exclusive\|fixed] [--rate HZ] [--period-frames N] [--keepalive] [--allow-unsafe-period] [--force-min-period] [--no-monitor\|--monitor-report]` | Captures and prints packet cadence statistics. `fixed` uses `CaptureMode::FixedPeriod` (default 288 frames = 6 ms at 48 kHz; floor 5 ms unless `--allow-unsafe-period`). `low` on an endpoint whose minimum period is below 2.5 ms is refused unless `--force-min-period`. The corruption monitor runs automatically in `fixed` mode (`--monitor-report` only reports, `--no-monitor` turns it off) | captures only; `--keepalive` renders silence into a virtual cable endpoint |
| `record FILE [seconds] [shared\|low\|exclusive] [--rate HZ] [--device NAME]` | Records a capture endpoint to WAV | writes a file |
| `tone [seconds] [--target NAME] [--exclusive --rate HZ --bits 16\|24\|24in32\|32\|float] [--signal tone\|sweep\|noise\|multitone] [--freq HZ] [--db -30]` | **Plays a test signal** into a render endpoint (default `Hi-Fi Cable Input`) | **Plays audio.** Refuses levels above -20 dBFS. Keep it quiet; if the target is a real output it is audible. `--exclusive` takes the device exclusively |
| `exp OUT.wav [tone options] [--target RENDER] [--device CAPTURE] [--cmode shared\|low\|exclusive\|fixed] [--crate HZ] [--period-frames N] [--keepalive] [--allow-unsafe-period] [--secs N]` | Plays a signal and records the capture endpoint at the same time | **Plays audio** (as `tone`) |
| `check FILE.wav [--rrate HZ] [--signal ..] [--freq ..] [--db ..] [--secs N]` | Compares a recording with the generated reference: mismatches, slips | reads a file |
| `loopback-run <name> <secs> [--keepalive] [--tone-db -50] [--signal ..] [--low\|--lowkeep\|--lowloop] [--noevent] [--poll [--poll-ms N]]` | Loopback capture of a render endpoint with optional keep-alive and tone | plays a quiet tone if `--tone-db` is given (refuses above -40 dBFS) |
| `loopback-record <name> <secs> OUT.wav [same flags]` | As above, recording to WAV (default keep-alive and tone -50) | quiet tone, writes a file |
| `proc-loopback <pid\|name> <secs> [--rate HZ] [--exclude]` | Process loopback capture (include or exclude a process tree) | captures only |
| `ks-list [capture\|render\|audio]` | WDM-KS filters and pins | read-only, opens filter handles only |
| `impulses [--target RENDER] [--device CAPTURE] [--cmode shared\|fixed --period-frames N] [--keepalive] [--count N] [--interval-ms M] [--db -40] [--render-mode shared-default\|shared-lowlatency\|shared-min-buffer] [--render-buffer-frames N] [--render-fill full\|period\|N] [--render-prefill full\|period\|N]` | Quiet windowed clicks (refused above -35 dBFS, only into virtual cables) into a cable's render endpoint with QPC timestamps; with `--device` it also prints the injection to capture-delivery time (the render side measured without the Yamaha); the render options choose the stream type (legacy / IAudioClient3 / small buffer) and how it is fed, and the output reports buffer, period, padding and the endpoint's engine period before and after (with `--keepalive` the render keep-alive runs first). Matrix of results in [latency.md](latency.md#5b-what-determines-the-render-side-vac-line-1-render-matrix) | quiet clicks, virtual cable only |
| `vac-check [--device NAME] [--secs 12] [--period-frames 288] [--repeats N] [--keepalive]` | PASS/FAIL integrity check of a VAC "Line 1" capture at one period (default 288 frames; 240 is known to be unreliable, see [latency/vac_lite.md](latency/vac_lite.md)). Skips with a message if the Line 1 capture endpoint is not active | **Plays a quiet signal** into Line 1 (refuses above -40 dBFS; default -50) |
| `wdm-baseline --allow-sink "Music (Steinberg UR22C)" [--confirm-knob-down] [--target RENDER] [--device CAPTURE] [--count N] [--interval-ms M] [--db -40] [--router-total MS]` | No-router baseline: renders the `burst:18500:2` click with the default legacy client into a Windows output and detects it on the Yamaha WDM "Streaming" capture; prints per-click latency, a summary, the capture-side buffering estimate and the comparison with the router. See [latency.md](latency.md#no-router-baseline) | **Plays quiet clicks into a REAL output (the headphones).** Needs `--allow-sink`; prints the Windows volume/mute (read-only) and exits 2 before rendering if the volume is above 0.5 without `--confirm-knob-down`; at most 10 clicks, interval >= 1200 ms, run <= 20 s, click <= -35 dBFS (default -40); only `burst:18500:2`; unknown flags (no render-profile flags) are refused; never changes volume or mute. Lower the physical Yamaha headphone knob first, router stopped |
| `wdm-baseline --selftest` | The detector, timing arithmetic, abort paths and safety gates against synthetic data | no audio device is opened |

## 8. `asio-probe`

```text
asio-probe list
asio-probe info "<driver name>"
asio-probe run  "<driver name>" [seconds]
```

`list` shows the drivers visible to this 32- or 64-bit process. `info` loads the driver and prints channels, buffer sizes, rate and latency. `run` additionally creates buffers and counts callbacks for N seconds (default 3) without producing audio. `info` and `run` open the driver exclusively; quit the router first.
