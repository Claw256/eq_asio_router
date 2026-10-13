# Configuration and command-line reference

Back to the [documentation index](README.md). Source of truth: `crates/eq-core/src/config.rs`, `crates/eq-core/src/status.rs`, `crates/router-engine/src/status.rs`, `crates/eq-asio-app/src/settings.rs`.

## 1. Files

| File | Written by | Purpose |
|---|---|---|
| `%APPDATA%\eq_asio_router\config.toml` | GUI, or by hand | Router configuration (section 2). Saved atomically. Hot reload: the engine polls the modification time |
| `%APPDATA%\eq_asio_router\app_settings.toml` | GUI | Tray and startup behaviour (section 3) |
| `%APPDATA%\eq_asio_router\caps.toml` | engine | Capabilities of the master driver (buffer sizes, rates, latencies, channel names), for the GUI |
| `%APPDATA%\eq_asio_router\latency_measured.toml` | `router-cli latency` | Stored end-to-end measurements (one `[[entries]]` per source — the fixed key `vbmatrix` — ASIO buffer and render profile: median render side, inj->capture, capture->tap, total, date, engine parameters). Read by the engine once per session so that the latency readout shows the measured render side instead of an estimate; entries older than 30 days or for a different source or engine setting are ignored. Safe to delete |
| `%TEMP%\eq_asio_router_engine.log` | engine | Log, rotated at about 1 MB (current plus one old file) |

Missing fields use their defaults, so old files keep loading. A file that does not parse is ignored (the last good config stays in force) and the error is shown as `config_error`.

## 2. `config.toml`

All values are passed through `Config::sanitized()`; out-of-range numbers are clamped and NaN/infinity is replaced by the default.

| Field | Type | Default | Meaning | Sanitized range |
|---|---|---|---|---|
| `master_driver` | string | `""` (= `Yamaha Steinberg USB ASIO`) | Name of the ASIO driver to host. `EQ ASIO Router` (our own bridge driver) is refused (`ForbiddenMaster`) | any non-empty name, else default |
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
| `resampler_quality` | `"linear"`, `"cubic"` | `"cubic"` | Resampler kernel: linear, or 32-tap Kaiser sinc (live) | |
| `auto_headroom` | bool | `true` | Lower the effective preamp so the profile's maximum boost never exceeds 0 dBFS (live) | |
| `mute_on_fault` | bool | `true` | Fade-mute on detected faults | |
| `jitter_margin_ms` | f32 | `0.0` | Legacy single learned value, kept for older readers (see the bridge section below). The engine reads the margin through `Config::jitter_margin_for()`, i.e. the `"vbmatrix"` entry of `jitter_margin_by_device`, or this value only while that map is empty. 0 = not learned (start conservatively, measure live). When set, the engine starts at it and never goes below it (live) | 0..40 |

Restart-class changes (engine restarts by itself): `master_driver`, `sample_rate`, `output_channels`, `[bridge] loopback_enabled`. Everything else is applied live.

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

### `[bridge]` settings (VBMatrix bridge)

Status: added in M2 of the [bridge plan](plans/vbmatrix-bridge.md); old config files load unchanged (a missing `[bridge]` table uses the defaults). Source of truth: `crates/eq-core/src/config.rs` (`BridgeConfig`), `crates/router-engine/src/bridge.rs`.

| Field | Type | Default | Meaning | Sanitized range |
|---|---|---|---|---|
| `ring_target_ms` | f32 | `0.0` | Minimum fill target of the source ring (ring A) in milliseconds. The engine always derives a target from the negotiated driver buffer plus the safety buffer; this value only acts as a *floor* on top of that (0 = no floor, derive purely from the buffers) | 0..200 (NaN becomes 0) |
| `loopback_enabled` | bool | `true` | Feed the post-EQ stereo audio back to VBMatrix through ring B (the bridge driver's ASIO input side). Off = the driver's inputs stay silent. Restart-class | |

### Removed keys (pre-bridge source) and the one-shot deprecation notice

The WASAPI source and everything around it were removed with the M3a cutover. The keys `capture_device`, `capture_mode`, `capture_period_ms`, `render_keepalive`, `keepalive`, `match_cable_rate`, `cable_rate_match`, `source_preset` and `source` are **ignored**: old files still load (serde defaults plus unknown keys ignored), and the first load that finds any of them reports a one-shot notice per process (logged and shown in the GUI) through `eq_core::take_deprecation_notice()`:

```text
config: removed setting(s) ignored: capture_device, source — since M3a the audio source is the VBMatrix bridge; delete these keys from the file
```

The same applies to the removed CLI flags `--source`, `--capture`, `--mode`, `--period-ms`, `--keepalive`, `--no-keepalive` and `--no-match-cable`: `router-cli` rejects them like any unknown flag (usage on stderr, exit code 2).

### Jitter margins and source naming

The bridge is the only source, under the fixed key `"vbmatrix"` (`eq_core::BRIDGE_SOURCE_KEY`). `jitter_margin_by_device` maps that key to the learned margin (ms, 0..40, at most 16 entries): `Config::set_jitter_margin` (the GUI's "Learn (30 s)") writes `jitter_margin_by_device["vbmatrix"]` and mirrors the legacy `jitter_margin_ms`; `Config::sanitized` migrates legacy per-device keys and the old single value into the `"vbmatrix"` entry. The same key names the source in `latency_measured.toml` entries.

Helper functions and constants in `eq-core`: `BRIDGE_SOURCE_KEY`, `frames_for_ms(ms, rate)`.

Status fields (in `EngineStatus`, not in the 192-byte status block):

| Field | Meaning |
|---|---|
| `bridge_attached` | The engine is attached to the bridge mapping `Local\eq_asio_router_bridge_v2` |
| `vbmatrix_running` | The driver slot is running and its heartbeat is fresh (`driver_running` plus a `driver_beat` within the last second) |
| `bridge_sample_rate`, `bridge_buffer_frames` | Format negotiated by the driver with VBMatrix |
| bridge counters | ring A/B fills, `a_overruns`, `a_discarded`, `b_starved`, `b_dropped` (also in the status block, section 5) |

### JBL Bass Mode

`jbl_bass_mode` (bool, default `false`) turns the car-subwoofer effect on (bass shelf, resonance, clipping distortion, bass-keyed pumping, lo-fi) after the EQ. `jbl_intensity` (0..100, default 90) is the master scaler. The `[jbl]` table exposes the individual stages. Every value in it is the endpoint reached at 100 % Intensity; lower Intensity interpolates from the idle (neutral) value toward it. The defaults are deliberately exaggerated ("fun"): deeper pumping, a bigger shelf and resonance, more drive and the Rattle LFO on, with the loudness compensated to about 1 dB below dry at any input volume (see "Auto level" below; the optional "Level guard" can additionally cap it at the input loudness). They were tuned against measurements (see `crates/eq-core/tests/jbl_audit.rs`): the pump depth, release and attack sit in the usual sidechain ranges, and the resonance is a broad box-like boost rather than a narrow ring. The mids and highs were later lifted (loudness-matched, versus the previous defaults) by easing the clipper and opening the treble low-pass (shelf 14 to 10 dB, resonance 9 to 6.5 dB, drive 5.5 to 8, kick drive 3.0 (unchanged), release 150 to 120 ms, low-pass 14000/7500 to 16000/10000 Hz), by a small **mids/highs pass-through** (`mid_pass`, below: first 0.9, then cut to 0.15 so the bass takes over and the whole signal distorts and ducks again; at 0.9 the 1 kHz THD was only -38.5 dB and the sound stayed clean, now it is about -19 dB, with a per-kick mid-band dip of about -15 dB versus dry), and by the **ducking floor** (`duck_floor_db`, below). The ducking itself is unchanged (`duck_depth = 4.0`, attack 5 ms, release 120 ms: same curve, timing and rhythm); the floor only limits how far the level may drop on a bass hit (default -9 dB, measured per-kick mid-band dip about 3.6 dB instead of about 7 dB, so the ducked level is about 3.5 dB louder while the pumping stays clearly audible). The small pass-through is ducked together with the rest, so the pump moves the vocals too. The Default's `out_trim_db` is +1.1 dB (part of the loudness calibration, which lands the effect about 1 dB below dry, measured). A missing `[jbl]` table or key uses the default. Out-of-range values are clamped by `Config::sanitized()`; NaN/infinity becomes the default. Configs saved by an earlier version carry their old explicit `[jbl]` values (older defaults: `duck_depth = 2.0`, `duck_release_ms = 120.0`, `shelf_db = 12.0`, `res_db = 6.0`, `res_q = 2.0`, `drive_max = 4.0`, `kick_drive_max = 2.2`, `lfo_on = false`, `lfo_rate_hz = 6.0`, intensity 80); delete those keys (a `[jbl]` table without `mid_pass` or `duck_floor_db` loads them as 0 and -24 = off, i.e. it keeps its old sound; a missing `mid_pass_hz` is 400), pick the "Default" preset or use "Reset to defaults" to adopt the new ones. The other presets keep their own values and were only re-trimmed in loudness. A config saved before **Auto level** existed has no `auto_level` key and loads as `true` (the explicit `[jbl]` values are kept, so such a sound now follows the input level; set `auto_level = false` for the old fixed-drive behaviour). All changes apply live.

| Key | Default | Range | Meaning |
|---|---|---|---|
| `duck_on` | `true` | | Pumping: lower the level on each bass hit |
| `duck_depth` | `4.0` | 0..6 | How far the level is pulled down |
| `duck_floor_db` | `-9.0` | -24..0 | Ducking floor: the most the ducker may lower the level on a bass hit (curve shape, attack and release unchanged). -24 = no floor (the plain curve; the other presets and a missing key), 0 = no ducking. Higher = louder while pumping. GUI: Advanced > Pump > Floor |
| `duck_attack_ms` | `5.0` | 0.5..50 | Duck attack time |
| `duck_release_ms` | `120.0` | 10..800 | Duck release time |
| `detect_hz` | `100.0` | 40..250 | Bass detector frequency that triggers the pumping |
| `shelf_hz` | `70.0` | 30..200 | Low-shelf corner frequency |
| `shelf_db` | `10.0` | 0..18 | Low-shelf boost |
| `res_on` | `true` | | Enable the resonant boom |
| `res_hz` | `50.0` | 25..120 | Resonance centre frequency |
| `res_db` | `6.5` | 0..18 | Resonance gain |
| `res_q` | `2.5` | 0.5..10 | Resonance Q (higher is narrower) |
| `drive_max` | `8.0` | 1..12 | Clipping drive |
| `bias_max` | `0.35` | 0..0.8 | Clipping asymmetry (adds even harmonics) |
| `kick_on` | `true` | | Enable the extra kick-band saturation |
| `kick_hz` | `130.0` | 60..300 | Kick band upper edge |
| `kick_drive_max` | `3.0` | 1..6 | Kick-band drive |
| `crush_on` | `true` | | Enable the bit crusher |
| `crush_bits_min` | `10.0` | 6..16 | Bit depth at 100 % |
| `hold_on` | `true` | | Enable sample hold |
| `hold_max` | `3.0` | 1..8 | Hold length at 100 % |
| `lpf_on` | `true` | | Enable the treble low-pass |
| `lpf_hz_start` | `16000.0` | 2000..20000 | Low-pass cutoff at low Intensity |
| `lpf_hz_end` | `10000.0` | 2000..20000 | Low-pass cutoff at 100 % |
| `lfo_on` | `true` | | Enable the rattle LFO (slow level wobble) |
| `lfo_rate_hz` | `5.0` | 0.5..20 | LFO rate |
| `lfo_depth` | `0.3` | 0..1 | LFO depth (0 = no effect) |
| `lfo_follow_bass` | `true` | | Scale the LFO by the bass envelope so it only wobbles on hits |
| `lfo_target` | `"Gain"` | `"Gain"`, `"BassBand"`, `"Both"` | Modulate output gain, the kick band level, or both |
| `out_trim_db` | `1.1` | -12..+12 | Output trim, added to the automatic loudness compensation (the compensation is calibrated for the effect landing about 1 dB below dry with `auto_level` on, so 0 dB trim change keeps that). With `loudness_guard` on a positive trim can only lift the effect up to the input loudness (the guard still caps it there) |
| `bass_boost_db` | `0.0` | 0..12 | Clean low-shelf gain on the effect output, after the distortion (0 = off). The GUI's **Bass boost** slider under Intensity; the wet signal is soft-limited while it is above 0 |
| `bass_boost_hz` | `80.0` | 30..200 | Corner frequency of the Bass boost shelf (Advanced > Low end > Boost freq) |
| `loudness_guard` | `false` | | Level guard (optional, off by default since Auto level): the effect is never louder than its input (plus `bass_boost_db`), see below. GUI: Advanced > Output > Level guard. A missing key means `false`; a saved `true` stays `true` |
| `auto_level` | `true` | | Auto level: the effect follows the input loudness so the distortion amount and the output loudness do not depend on the input volume, see below. GUI: Advanced > Output > Auto level. A missing key means `true`. `false` = the legacy fixed-drive path (bit-identical to before Auto level, calibrated for about -18 dBFS RMS programme only) |
| `mid_pass` | `0.15` | 0..1 | Mids/highs pass-through amount: that share of the processed signal above `mid_pass_hz` is replaced by the dry (original) signal. The default is low (15 %) so the clipper, ducker and lo-fi hit the whole signal and the bass takes over; high values (the earlier default was 0.9) keep mids/highs clean (1 kHz THD about -38 dB instead of about -20 dB). 0 = off (bit-identical to the old path; all presets except Default; a missing key). GUI: Advanced > Mids/highs |
| `mid_pass_hz` | `400.0` | 100..1000 | Crossover frequency of the pass-through (4th-order Linkwitz-Riley pair, sums flat) |

#### Auto level

JBL Bass Mode is a saturating effect: a clipper, a kick-band saturator, a bass-keyed ducker and a bit crusher, all calibrated against fixed internal levels. Without compensation its loudness and character therefore depended on the input volume: the loudness was matched only for programme at about -18 dBFS RMS, so modern loud masters (-11 to -7 dBFS RMS) came out 8 to 12 dB **quieter** and flatter with the mode on, and quiet material came out several dB louder and grittier (measured through the full processor with the Default settings and intensity 90: +9.7 dB at -27, -1.1 dB at -17, -7.6 dB at -11, -11.6 dB at -7 and -12.9 dB at -3 dBFS RMS). **Auto level** (`auto_level`, on by default) makes the effect level-aware:

1. A mean-square follower on the dry input (one scalar gain for both channels from L+R power, so the stereo image is preserved) tracks the programme level. It attacks with a 40 ms and releases with a 300 ms time constant (slow enough not to follow the bass cycles or pump), and catches up with a 25 ms time constant when the level really drops (more than 4 dB below a 40 ms follower), so a sudden volume cut is not followed by a long burst of under-driven output.
2. The input of the wet path (shelf, resonance, kick saturation, clipper, bit crush, bass detector for the ducking) is scaled to a fixed reference level of -18 dBFS RMS, the level the clipper, ducker and lo-fi stages were calibrated at. The wet path therefore always sees the same drive, so the amount of distortion (THD), the grit, the per-kick pump depth and the bass boom are the same at every input volume.
3. The same factor is undone on the wet output (after the lo-fi stages, before the pass-through and the clean Bass boost). The effect's loudness therefore follows the input loudness 1:1, and the Default lands about 1 dB below dry whatever the volume.
4. The mids/highs pass-through (`mid_pass`) takes the true dry signal (it is never pre-gained), so its balance against the wet part is the same at every level.

The gain is limited to +24 dB of boost (below about -42 dBFS RMS the gain stays at +24 dB, so silence and noise are never boosted without bound and silence stays silence) and 30 dB of cut. The follower keeps running while the effect is off, so switching it on mid-song starts at the right gain, and level changes are smooth (no zipper): after an 18 dB step the level has settled within about 300 to 500 ms (after a big cut the output is briefly, for about 100 to 250 ms, louder than the new level). The calibration is stored per `auto_level` setting, so with `auto_level = false` the signal path is bit-identical to the one before Auto level existed (loudness only right at about -18 dBFS RMS; the presets' `out_trim_db` values were re-trimmed for Auto level, so with it off they are a few dB off the old loudness; use the Trim slider). Measured with the Default and the presets (music-like test signals, guard off): about -1.0 dB versus dry (Default; -0.1 to -1.1 for the presets) at -30, -24, -18, -12, -6 and -3 dBFS RMS at 48 kHz; the 60 Hz THD (-9.7 dB), the 1 kHz THD (-18.6 dB) and the per-kick mid dip versus dry (-14.7 dB) are identical from -30 to -12 dBFS (and within 0.3 dB at -6).

#### Level guard

The optional **level guard** (`loudness_guard`, off by default since Auto level) keeps the effect from ever being louder than its input: per sample it follows the mean-square (RMS) level of the input and of the effect's wet signal with a 150 ms one-pole window and attenuates the wet signal by `min(1, sqrt(input / wet))` (with the mids/highs pass-through only the processed part is turned down; the pass-through is counted in the level but never attenuated by the guard). The gain falls with a 40 ms time constant and recovers over 300 ms, so it is smooth (no clicks or pumping of its own), only ever turns the effect down, and stays at unity for silence and signals below about -100 dBFS. With the guard on the effect is at most about 0 dB versus the input for every preset, at any input level and sample rate (with or without Auto level). Only the **Bass boost** slider raises the ceiling, by its dB amount; a positive **Trim** can bring a quiet preset up to, but not past, the input loudness. With Auto level on the effect already sits just under the input loudness, so the guard rarely acts; it is mainly a safety net for custom settings with a large positive Trim, and it is the tool to use with `auto_level = false` if quiet material is too loud. Switch the guard off (default) to hear the unrestrained effect.

#### Presets

The GUI's Fun section has a **Preset** drop-down that loads a ready-made sound: it sets `jbl_intensity` and the whole `[jbl]` table in one go. Presets are applied from the GUI only and are not stored as a name; the config just holds the resulting values. The drop-down shows the matching preset's name, or "Custom" once any value no longer equals a preset. Each preset's `out_trim_db` loudness-matches it to the default sound with Auto level on (within about 0.1 dB at -30 to -18 dBFS RMS, within 0.5 dB up to -6 dBFS, and the same holds at 44.1/48/96 kHz), so every preset lands within about 1 dB of dry at any volume (the Default about -1.0 dB, the others -0.1 to -1.1 dB). Several presets therefore carry a large positive trim (up to +10.2 dB for Pumper). All presets have the level guard off and Auto level on. The presets live in `eq_core::jbl_presets()`.

| Preset | `jbl_intensity` | Sound |
|---|---|---|
| Default | 90 | The tuned "fun" sound described above: big bass, pumping, grit and a light rattle, with only a little of the dry mids/highs let through (15 %), so the bass takes over and distorts the whole sound |
| Mild | 45 | Subtle warmth: gentle shelf, small resonance, light pump, no lo-fi or rattle |
| Clean Boost | 80 | Bass lift only (shelf and boom): no pumping, distortion, lo-fi or rattle |
| Club Sub | 85 | Heavy clean sub around 47 Hz, light slow pump, open treble |
| Trunk Rattle | 85 | Fast 8 Hz rattle on every bass hit (gain and bass band) with some grit |
| Pumper | 90 | Extreme sidechain wobble: maximum duck depth, 1.5 ms attack, 200 ms release |
| Blown Speaker | 95 | Heavy clipping, 6.5-bit crush, 5x sample hold and a dull 4.5 kHz top |

Example:

```toml
jbl_bass_mode = true
jbl_intensity = 90

[jbl]
shelf_hz = 60.0
lfo_rate_hz = 4.0
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
| `preset_confirmed` | `false` | Legacy (the source-preset switch dialog was removed with the WASAPI source); kept so old `app_settings.toml` files load unchanged |

GUI command line: `--background` (hidden, engine starts), `--minimized` (hidden), `--no-engine` (never auto-start, for testing), `-h` / `--help`.

## 4. Single-instance and kernel object names

| Name | Owner |
|---|---|
| `Local\eq_asio_router_engine` | engine mutex (one engine per user session) |
| `Local\eq_asio_router_gui`, `Local\eq_asio_router_gui_show` | GUI mutex and show-event |
| `Local\eq_asio_router_status` | status block mapping |

## 5. Status block

Shared memory `Local\eq_asio_router_status`, 192 bytes, magic `EQST`, layout version 3, written by the engine (relaxed atomic stores from the audio thread), read with `StatusReader`. v3 grew the block to 192 bytes and added the bridge fields (`bridge_attached` .. `b_dropped`); old v2 readers reject a v3 block and vice versa.

| Field | Type | Meaning |
|---|---|---|
| `magic`, `version` | u32 | `EQST`, 3 |
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
| `capture_period_us`, `safety_buffer_us` | u32 | Source period (negotiated bridge buffer); effective safety buffer |
| `dropped_logs` | u32 | Log lines dropped by the logger ring |
| `bridge_attached` | u32 | v3: the engine is attached to the bridge mapping |
| `vbmatrix_running` | u32 | v3: the driver slot is running and its heartbeat is fresh |
| `bridge_sample_rate`, `bridge_buffer_frames` | u32 | v3: format negotiated by the driver with VBMatrix (Hz, frames) |
| `fill_a`, `fill_b` | u32 | v3: frames currently in ring A (source) and ring B (loopback) |
| `a_overruns`, `a_discarded`, `b_underruns`, `b_dropped` | u32 | v3: ring counters (driver drops on full ring A, app latency trim, ring-B underrun fed as silence by the driver, app drops on full ring B) |

`EngineStatus` (in-process, richer; `crates/router-engine/src/status.rs`) adds, among others: `state` (`Stopped`, `Starting`, `Running`, `Restarting{reason}`, `WaitingForDevice{holders}`, `Failed`), `cable_rate`, `ring_target_frames`, `ring_mean_fill_frames`, `drift_ppm`, `gate_rearms`, `discarded_frames`, CPU percent of the callback period, `jitter_headroom_frames` / `required_headroom_frames`, fill dip p99.9, `capture_staleness_ms` (source staleness), `learn` progress and result, `hardening` (six hardening steps), `restarts`, `last_error`, `busy_holders`, `config_error`, `uptime_ms`, the bridge fields listed above (section 2), and the latency breakdown (master output, ring, source, render-side estimate, total).

## 6. `router-cli`

```text
router-cli devices
router-cli caps [--master NAME]
router-cli holders [--master NAME]
router-cli --help | -h | help          (also after any subcommand: prints usage, exit 0)
router-cli run (--seconds N | --forever) [--config PATH] [--master NAME] [--rate HZ]
               [--safety-ms F] [--preamp-db F] [--bypass] [--learn] [--dump-dir DIR]
               [--synthetic RATE[:PACKET[:JITTER_MS[:FREQ,..]]] [--synthetic-db DB]]
router-cli latency [--sink NAME] [--count N] [--interval-ms M] [--db -40] [--config PATH]
                   [--allow-sink NAME] [--shape burst|chirp] [--json]
                   [--cap-threshold-db X] [--lb-threshold-db X] [--hp-hz N|off]
router-cli latency --baseline [--seconds 3] [--dump FILE.wav]
router-cli latency --selftest
router-cli analyze FILE [--freq HZ] [--skip S] [--tail S] [--channel N] [--mads X]
```

The removed source flags `--source`, `--capture`, `--mode`, `--period-ms`, `--keepalive`, `--no-keepalive` and `--no-match-cable` are rejected like any unknown flag (usage on stderr, exit code 2).

| Command | What it does | Side effects |
|---|---|---|
| `devices` | Lists ASIO drivers, capture and playback endpoints (cable endpoints with their rate) | none |
| `caps` | Briefly opens the master, prints channels, rates, buffer sizes and latencies, releases it | opens the Yamaha briefly (fails if it is in use) |
| `holders` | Lists processes that have the master driver DLL loaded (PID, exe, path) | none; never closes anything |
| `run` | Runs the engine headless, prints a status line per second; exit code 2 if `--seconds` elapses while the master is still held by another program | plays audio to the Yamaha |
| `latency` | End-to-end click latency: injects a very quiet click into a "VBMatrix In" render endpoint, runs it through the real router and detects it at the bridge source (stage 1, ring-A tap) and on the Yamaha's loopback inputs (stage 2) (details below and in [latency.md](latency.md#5a-end-to-end-measurement-router-cli-latency)); `--selftest` needs no hardware | **opens the Yamaha** (does not start and exits with code 2 if another program holds it) and plays a click of at most -35 dBFS into "VBMatrix In"; the router output is about -55 dBFS |
| `analyze` | Sine-fit glitch analysis of a recorded WAV (THD+N, glitches, phase steps, clipping, zero runs) | reads a file only |

Command-line safety: the whole command line is validated before anything touches a device. An unknown flag or subcommand is an error (usage on stderr, exit code 2) and never starts the engine; `--help`, `-h` and `help` print the usage and exit 0. `run` must be bounded: give `--seconds N` or, for an unbounded run (stop with Ctrl+C), `--forever` explicitly (the two are mutually exclusive). `devices`, `caps`, `holders`, `latency` and `analyze` behave as before, apart from rejecting unknown flags.

`run` details: without `--config` it uses `%APPDATA%\eq_asio_router\config.toml` if present. At start `run` prints the source summary (`VBMatrix bridge`, negotiated rate and buffer, ring fills) and the per-second status line. `--learn` waits 5 s, then runs the 30 s source jitter measurement and prints the margin to put in `jitter_margin_by_device["vbmatrix"]` (mirrored into `jitter_margin_ms`). `--dump-dir` writes `capture_in.wav`, `after_resample.wav`, `after_eq.wav` and a report (a measurement tap). `--synthetic` replaces the bridge source by a generated tone (default -30 dBFS, 1 kHz) so rate and packet combinations can be tested without touching Windows settings. `--rate`, `--safety-ms` etc. override the file for this run only.

`latency` details: loads the config like `run` (`--config` behaves the same) and starts the real engine with the preamp forced to -20 dB, the limiter on, the EQ not bypassed, no config watcher (nothing can change the level during the run) and no status mapping or caps file written. The click goes into `--sink` (a name or substring of a playback endpoint); by default a `VBMatrix In` render endpoint (stage 1 is the bridge source tap: the ring-A tap where the click reappears after VBMatrix; stage 2 is the Yamaha's loopback inputs). Safety rules enforced in code: `--db` defaults to -40 and anything above -35 is refused (also in the library); the sink must be an allowed injection endpoint (a known virtual cable or a `VBMatrix In *` render endpoint), otherwise the command refuses unless `--allow-sink NAME` names it explicitly; `--count` is at most 10 (default 5); `--interval-ms` 500..5000 (default 1200); each click waits at most 4 s for detection; before the first click a 1 s quiet baseline is measured at both detectors (see "Baseline and thresholds" below) and the run aborts if the router output is above -70 dBFS, if anything above -60 dBFS arrives at the sink (something else is playing into VBMatrix In), if the loopback carries programme audio (deviation above -40 dBFS, or above -20 dBFS only for a burst of at least 15 kHz with the high-pass detector) or if the noise floor is too high to detect the click at the loopback; the message names the detector and the measured levels; the run aborts at once if the router output peak exceeds -40 dBFS; Ctrl+C stops cleanly. The Yamaha is checked first with the same holder detection as `holders`: if the tray app or anything else has it, nothing is started, nothing is closed, and the command prints that the live run is pending and exits with code 2. `--shape` selects `burst[:HZ[:MS]]` (default 1500 Hz, 2 ms raised cosine) or `chirp[:F0:F1[:MS]]`; `--cap-threshold-db` and `--lb-threshold-db` override the detector thresholds (the defaults come from the measured baseline, below); `--hp-hz N|off` sets or disables the loopback detector high-pass (automatic when the lowest frequency of the click is at least 15 kHz, e.g. `--shape burst:18500:2` gives a 15.75 kHz corner); `--json` appends one JSON object with every impulse and the statistics. **Render stream options:** `--render-mode shared-default|shared-lowlatency|shared-min-buffer`, `--render-buffer-frames N`, `--render-fill full|period|N` and `--render-prefill full|period|N` choose what kind of application the click imitates (default: a typical 10 ms shared-mode client that writes one period per event and keeps one period queued); the run prints the stream's buffer, period, the endpoint's engine period before and after and the padding at submit time. **Saving:** after a successful run with at least 3 clicks the result is written to `%APPDATA%\eq_asio_router\latency_measured.toml` (additive; `--no-save` never writes; a non-default render stream is written only with `--save`, under its own render profile); the GUI latency panel, the tray tooltip and the `router-cli run` status line then use the measured render side for the `vbmatrix` source and label it `measured <date>`, otherwise they show an estimate (20 ms: a typical shared-mode client into `VBMatrix In`) labelled as such. The output is a per-impulse table, min / median / mean / max / sigma per figure, outlier flags and a breakdown against the latency panel's model; see [latency.md](latency.md#5a-end-to-end-measurement-router-cli-latency) for the definitions and for how to read it. **Baseline and thresholds.** Before the clicks the command measures 1 s of both detector inputs with nothing injected: per channel peak, RMS, DC offset (mean) and the robust noise floor (99th percentile over blocks of the per-block deviation from the mean). The thresholds follow from that, not from fixed offsets: source `max(click - 20, floor + 12)` dBFS, loopback `max(click + preamp - 25, floor + 12)` dBFS, where the loopback floor is the deviation from the measured mean (the detector subtracts that DC offset first, so DC cannot trigger it). A threshold must stay at least 6 dB below the expected click level (source ring-A tap: `click`; loopback: `click + preamp`, and with the high-pass detector plus the envelope gain of the click shape, about -4 dB for `burst:18500:2`), otherwise the run aborts with the computed requirement (with the -20 dB preamp a -35 dBFS click reaches the loopback at about -55 dBFS, so the floor must be below -73 dBFS; for -40 dBFS below -78 dBFS). Safety aborts stay: router output above -40 dBFS, anything above -60 dBFS at the sink, loopback deviation above -40 dBFS (relaxed to -20 dBFS only for `--shape burst:<HZ >= 15000>` with the high-pass active, where the run prints a `*** SAFETY NOTE` line with the measured loopback peak; the router output limit of -40 dBFS is never relaxed). A detected loopback crossing whose level is more than 12 dB from the expected click level is ignored as a stray event and the detector is re-armed. **High-pass detector:** the Yamaha "Streaming" loopback mixes whatever the mixer sends into it, typically the microphone inputs, so on a machine with a live microphone it carries broadband noise (measured: about -46 dBFS RMS, -33 dBFS peak, band-limited to about 12 kHz) that hides any click at -55 dBFS. With a high-frequency click (`--shape burst:18500:2`) the loopback detector looks through an 8th-order Butterworth high-pass followed by a 32-sample sliding RMS (which rejects single-sample spikes), where the same loopback measured -87 dBFS RMS; the onset bias of that detector is computed by running the same filter on the nominal click. **`latency --baseline`** (`--seconds 0.5..10`, default 3; `--dump FILE.wav` writes the first 1.4 s of the loopback pair as a float WAV) starts the engine the same way (preamp -20 dB, limiter on) but renders nothing and writes nothing to any endpoint, and prints per channel: peak, RMS, AC RMS, DC offset, raw minimum / maximum (left-aligned Int32), the fraction of blocks with a peak at or above -90 / -80 / -70 / -60 dBFS, steady or time-varying, the high-passed levels, the strongest spectral peaks, the L/R correlation, the read-only state of the interface settings file (`ur22c_setting.xml`: age and the few plain fields; the mixer parameters are an unnamed binary blob and cannot be decoded, and the file is the last save of the app, not live state), and for click levels -40 and -35 dBFS whether a full-band 1.5 kHz click and a high-frequency click are detectable. Library API: `router_engine::baseline` (`Monitor`, `plan_thresholds`, `HighPass`), `LatencyProbe::{monitor_start, monitor_stop, monitor_hp, set_highpass, arm_with_dc}`.

`latency --selftest` runs the detectors and the timing maths against a synthetic loopback with known delays (three buffer geometries, several phases) and checks the safety guards; it never opens a device. Library API: `EngineOptions::loopback_probe` (`LoopbackProbe`) plus `Engine::latency_probe()` (`LatencyProbe::arm`, `take_result`, `info`), and `wasapi_capture::{ImpulseRenderer, render_impulses, ImpulseTrain, click_samples}`.

## 7. `capture-probe`

Built from `crates/wasapi-capture` (`cargo build --release -p wasapi-capture`). Names are substrings of the endpoint name. The engine no longer captures over WASAPI: `capture-probe` is kept as a measurement tool (since M3a it measures the `VBMatrix In` periods and the bridge chain). Commands:

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
