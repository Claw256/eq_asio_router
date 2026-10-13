# Latency

Back to the [documentation index](README.md). Labels: **measured** (on the author's machine, in the cited report), **estimate** (inferred or modelled). The source is the VBMatrix bridge (`vbmatrix`) since the M3a cutover; the end-to-end measurement `router-cli latency` ([section 5a](#5a-end-to-end-measurement-router-cli-latency)) stores its result (`%APPDATA%\eq_asio_router\latency_measured.toml`, keyed by the fixed source `vbmatrix`) so that the GUI, the tray tooltip and the `router-cli run` status line show the **measured** render side and label it as such; without a fresh measurement they show an **estimate** (20 ms, a typical shared-mode client into `VBMatrix In`) and say so. What drives the render side, and what applications can do about it, is in [section 5b](#5b-what-determines-the-render-side-vac-line-1-render-matrix). **The end-to-end latency of the bridge chain has not been recorded yet** (the M4 hardware checklist is open); every figure below predates the bridge. Sections 3, 4, 5b, 6 and 7 describe the pre-bridge WASAPI sources (Hi-Fi Cable / VAC "Line 1") and are kept verbatim as history, each marked **Historical (pre-bridge)**.

## 1. Budget

**Bridge-era budget (source = bridge, `vbmatrix`).** The four terms are unchanged in shape, but the source term is now the bridge delivery, not a WASAPI capture:

| Term | Basis | What sets it |
|---|---|---|
| Yamaha output | measured (driver reports 375 samples at 48 kHz) | ASIO buffer plus USB buffering chosen by `LatencyMode` and `PeriodFrames` |
| Ring (ring-A mean fill) | live status | block (negotiated bridge buffer) + safety (2 ms) + jitter headroom |
| Source staleness | model until statistics exist | half the negotiated bridge buffer, `buffer / 2 / rate` (`router-engine/src/bridge.rs`) |
| Render side | **estimate** (20 ms) until `router-cli latency` has been run for the `vbmatrix` source, then **measured** | app buffer, Windows engine period and queue until the app writes into `VBMatrix In` |

The bridge chain's end-to-end figure is not yet recorded (M4 open). The measurement below is the pre-bridge budget and is kept as history:

> **Historical (pre-bridge).** Measured on the author's machine with the Hi-Fi Cable source (Yamaha UR22C, `PeriodFrames` 256, Low Latency mode, 48 kHz): reported total **56.2 ms**.

| Term | ms | Basis | What sets it |
|---|---|---|---|
| Yamaha output | 7.8 | measured (driver reports 375 samples at 48 kHz) | ASIO buffer plus USB buffering chosen by `LatencyMode` and `PeriodFrames` |
| Ring (mean fill) | 27.6 | measured | block (256 frames = 5.3 ms) + safety (2 ms) + jitter headroom. This run still used the old rule of two assumed 480-frame packets (target 1312 frames) |
| Capture staleness | 9.2 | measured | mean age of a captured frame at delivery. The Hi-Fi Cable delivers 480-frame packets in back-to-back pairs, which behaves like 960-frame events every 20 ms |
| Render side | 20 (VAC) / 100 (Hi-Fi Cable) default; the measured value when `latency_measured.toml` has a fresh entry for the source | **estimate** until `router-cli latency` has been run for this source, then **measured** (the old fixed 10 ms was too low on every measurement) | app buffer, Windows engine period, cable driver ring until the capture event; see [5b](#5b-what-determines-the-render-side-vac-line-1-render-matrix) |

The four terms add up to 54.6 ms (with the old 10 ms render guess); the reports quote 56.2 ms for the total (the difference is not explained in the reports, probably readings taken at different moments). That 56.2 ms was a model total: the end-to-end measurement of the same Hi-Fi Cable path later gave 152 to 164 ms. The GUI and the `router-cli run` status line show the same breakdown live: `lat X ms (out + ring + cap + render N measured|est, not measured)`; the GUI latency panel also prints where the render side comes from and, when it is measured, the end-to-end figure at that time next to the live model total, and the tray tooltip appends `est.` to the total while the render side is only an estimate.

**Which render side is used.** `Caps::latency_breakdown` takes a `RenderSide` (eq-core `measured` module): the newest entry of `latency_measured.toml` for the default render profile that matches the running session (same source key `vbmatrix`, same ASIO buffer, source period within 25 %) and is at most 30 days old; otherwise the default estimate for the bridge source (20 ms: a typical shared-mode 10 ms client into `VBMatrix In`). The file is read once when the engine starts a session.

Model for the source staleness (bridge): half the negotiated buffer period, `buffer / (2 r)` (r = rate), until fill statistics exist. (The pre-bridge arrival-event model was `E[b^2] / (2 r E[b])` over arrival events: regular 480-frame packets give 5.0 ms, regular 128-frame packets 1.3 ms.)

## 2. How to reduce each term

### Yamaha output

Set `PeriodFrames` and `LatencyMode` in the Yamaha control panel ([setup-guide.md](setup-guide.md#4-yamaha-settings), details in [reverse/yamaha_usb_driver.md](reverse/yamaha_usb_driver.md)). The buffer itself is the dominant part: 512 frames is 10.7 ms, 256 is 5.3 ms, 128 is 2.7 ms. Low Latency removes about 2 to 4 ms of USB buffering compared with Standard. Estimate: `PeriodFrames` 128 saves about 2.7 ms against today (the driver's table suggests 247 samples, 5.1 ms, if the same preset row is used). Risk: dropouts under CPU or USB load; the callback of the router is short, but the driver's own USB scheduling is the limit. Apply by restarting the engine; verify with `router-cli caps`.

### Ring

Ring target = safety buffer + one master buffer + jitter headroom. The headroom is the part that can shrink:

- The fill statistics measure the real dip of ring A (`router-engine/src/bridge.rs`, `router-core/src/arrival.rs`) and lower the headroom slowly once at least 10 s of statistics exist.
- "Learn (30 s)" (GUI button, or `router-cli run --learn --seconds 60`; `run` needs `--seconds N` or an explicit `--forever`, unknown flags are rejected with exit 2 before anything starts, `router-cli --help` prints the usage) measures the lowest safe headroom while audio is playing and stores it as `jitter_margin_by_device["vbmatrix"]` (mirrored into `jitter_margin_ms`). It is a floor: the engine will not go below it.
- `safety_buffer_ms` (default 2 ms) and `safety_buffer_auto` trade robustness for latency; the auto component rises after underruns.
- A small negotiated bridge buffer reduces the dip the headroom has to cover (see the source term); `[bridge] ring_target_ms` can only raise the target, never lower it.

### Source staleness and the bridge buffer

Staleness is half an event on average. On the bridge it is half the negotiated ASIO buffer of the slot (`buffer / 2 / rate`), so a smaller buffer negotiated with VBMatrix reduces it directly. (Historical: on the pre-bridge WASAPI sources staleness followed the capture period and delivery pattern — see sections 3 and 5b and [latency/](latency/), kept verbatim.)

### Render side

Applications write into a `VBMatrix In` endpoint with their own buffer and the Windows engine period; VBMatrix then hands the data to the bridge slot. The measured behaviour of a typical shared-mode client (pre-bridge measurements on the VAC/Hi-Fi path, [5b](#5b-what-determines-the-render-side-vac-line-1-render-matrix)): an ordinary 10 ms event-driven shared-mode client (what almost every player and game uses) sees about 20 ms from `ReleaseBuffer` to delivery, most of it inside Windows (engine period plus the engine's own buffering); only a client that opens the stream with `IAudioClient3` at the endpoint's minimum period gets down to about 6 to 7 ms. A test client that tops its 1056-frame buffer up completely adds about 11 ms of queue on top. `router-cli latency` measures the total effect end to end (section 5a) and stores it for the panel.

## 3. Investigation verdicts

> **Historical (pre-bridge).** These findings predate the VBMatrix bridge (they compare WASAPI capture APIs on the Hi-Fi Cable / VAC sources) and are kept verbatim.

Full reports: [latency/capture_apis.md](latency/capture_apis.md), [latency/apo_tap.md](latency/apo_tap.md), [latency/vac_lite.md](latency/vac_lite.md).

| Candidate | Verdict | Why |
|---|---|---|
| WASAPI shared, Hi-Fi Cable (today) | baseline | 480-frame pairs, 10 ms engine period |
| IAudioClient3 on capture endpoints | dead end here | every active capture endpoint reports 480 frames as default, minimum and maximum |
| WASAPI exclusive capture | dead end | the Hi-Fi Cable driver duplicates or drops packets (30 to 70 % of samples wrong at 3 to 5 ms, measured earlier); only 10 ms periods are clean, no faster than shared. Disabled in the router (mapped to shared) |
| HD Audio / NVIDIA loopback (monitor endpoint) | works, rejected | regular 10 ms packets (staleness 5.0 vs 9.2 ms) but never smaller; needs a keep-alive, depends on the monitor link, audible unless that endpoint is lowered |
| APO tap (audio-engine plug-in) | feasible, not recommended now | estimate 33 to 38 ms on a 10 ms endpoint (does not reach 25 ms), 6 to 10 days, an APO bug takes down all system audio, one more unsigned DLL in audiodg |
| Process loopback, exclude own pid | best option without new software, optional | measured regular 10 ms, no pairs, 0 discontinuities; estimate 40 to 44 ms total. Exclusive-mode players are not captured |
| WDM-KS direct pin | not worth it | same driver ring as exclusive, high effort |
| MME / DirectSound / AudioGraph | reject | no smaller period than 10 ms |
| ASIO capture from a VB or Voicemeeter virtual ASIO | speculative | needs an always-on third-party host and two-driver ASIO hosting |
| **VAC lite "Line 1"** | **viable; implemented; capture and full chain measured, soak/listening/live fallback still unrun** | render side supports a 1 ms engine period, but the lite capture side delivers clean data only at 5 ms or more (below that: repeated blocks and frames arriving faster than real time; 1 ms is unusable) |

VAC Line 1 details:

- Original estimate (model, superseded): router total about 26 to 34 ms with a 5 ms capture event and a 10 ms render side, against 56.2 ms today. Measured since (section 5b, end to end through the router): about 50 to 52 ms with an ordinary 10 ms shared-mode client (render side about 23.5 ms implied, 24 to 26 ms inj->capture, capture -> tap 24 to 30 ms); the 26 to 34 ms above only holds if the render side were 7 ms (IAudioClient3 client, not measured end to end). With plain shared capture of Line 1 and no code change, regular 10 ms packets: about 39 to 47 ms. Confidence medium: the capture side is measured, the render side is not.
- 240 frames (5 ms) was clean in the first four runs but proved unreliable in later testing (4 of 6 runs damaged). **288 frames (6 ms) is the supported default**, so the estimates above, which were computed at 5 ms, are slightly optimistic (estimate: 1 to 2 ms more).
- Capture integrity is protected by a corruption monitor (`CorruptionMonitor`, on by default for fixed-period capture): frames arriving faster than real time, repeated blocks or sustained packet-size anomalies mean the stream is damaged. The engine then reopens the capture once in plain Shared and shows `capture_fallback` ("Capture fell back to Shared (10 ms): <reason>", GUI banner with "Retry fixed period"; the estimated total in that state is the plain-Shared figure of about 39 to 47 ms). `DATA_DISCONTINUITY` flags are deliberately not used because they are spurious on the Hi-Fi Cable. Implemented and unit-tested; a live fallback has not been observed on hardware. See [architecture.md](architecture.md#5a-fixed-period-capture-corruption-fallback-and-keep-alive) and [latency/vac_lite.md](latency/vac_lite.md).
- Router-side model (alternating-gap simulation of the 288-frame fixed period, estimate): the headroom planner target comes out at about 600 frames, and the start target is 784 frames, with 288-frame packets. These are expectations for `ring_target_frames`, not measurements; compare them with the live status in the validation checklist.
- VAC lite is limited to 48 kHz, 16 bit, 2 channels, one cable, private non-commercial use.

## 4. Recommendations, in priority order

> **Historical (pre-bridge).** These recommendations were written for the WASAPI sources (items mentioning VAC Line 1 or the Hi-Fi Cable no longer apply since the bridge cutover) and are kept verbatim.

1. Use the new jitter model and run "Learn (30 s)" while normal audio plays. Largest controllable term (ring, 27.6 ms measured today); no hardware change, no risk beyond slightly less margin.
2. Make sure the Yamaha is at the lowest period that is stable (256 now; try 128 and confirm with `router-cli caps` and a listening test). Estimate: about 2.7 ms.
3. Switch the source to VAC Line 1 with a fixed 6 ms (288 frame) capture period (preset "VAC Line 1"; quick start in [setup-guide.md](setup-guide.md#vac-line-1-quick-start)), after the [first-run validation](#7-first-run-validation-checklist-vac-line-1) has passed on your machine. Measured: inj->capture drops from about 105 ms (Hi-Fi Cable) to about 25 ms for a typical 10 ms client and to about 7 ms for an IAudioClient3 client. Keep the Hi-Fi Cable preset as the fallback.
4. Render keep-alive on Line 1 (`render_keepalive`, on in the VAC preset): measured to lower the endpoint's engine period from 10 ms to 1 ms, but **no measurable gain for a legacy 10 ms client** (24.9 ms without, 23.8 ms with, inj->capture); it only keeps the 1 ms period stable for clients that ask for it through IAudioClient3. Harmless, so it stays on, but do not expect it to cut the render side by itself. App-side settings that do matter: [5b](#5b-what-determines-the-render-side-vac-line-1-render-matrix).
5. Optional: process loopback (exclude own pid) as a source if no low-period cable is acceptable.
6. Not recommended: APO tap, HD Audio loopback, exclusive or KS capture.

## 5. How to measure

- Live: the status panel in the GUI, or the per-second line of `router-cli run` (`ring mean`, `headroom`, `arrival p99.9 gap / dip`, `lat ...`, `under / over / xrun`).
- Capture cadence and integrity: `capture-probe run`, then `capture-probe exp OUT.wav ...` followed by `capture-probe check OUT.wav ...`: sample-accurate means 0 mismatches and 0 slips. Use quiet signals ([configuration.md](configuration.md#7-capture-probe)).
- Output quality: `router-cli run --dump-dir DIR`, then `router-cli analyze` on the WAVs.
- Source status (GUI bridge panel, or the first lines of `router-cli run`): mapping attached, slot running, negotiated rate/buffer, ring fills, ring counters.
- `router-cli latency`: end-to-end measurement with a click injected into a `VBMatrix In` endpoint and timestamped at the bridge source (stage 1, ring-A tap) and on the Yamaha's Streaming L/R loopback inputs (stage 2), section 5a. The result is stored in `latency_measured.toml` (source key `vbmatrix`) and replaces the render-side estimate in the panel (section 5a, "Storing the result").

## 5a. End-to-end measurement: `router-cli latency`

Quit the tray app first (it holds the Yamaha and the engine mutex); `router-cli holders` must list nothing. Then:

```text
router-cli latency --baseline                      # first: what do the detectors see with nothing injected?
router-cli latency --count 5 --interval-ms 1200 --db -35 --shape burst:18500:2
```

The high-frequency burst (`--shape burst:18500:2`) is needed whenever the Yamaha loopback carries a microphone (see "Baseline and thresholds"); on a machine with a silent loopback the default `--db -40` and the default 1.5 kHz burst work as before.

Add `--json` for machine-readable output. `router-cli latency --selftest` needs no hardware.

**Render stream options** (what kind of application the injected click imitates): `--render-mode shared-default|shared-lowlatency|shared-min-buffer` (`IAudioClient::Initialize` with the default buffer; `IAudioClient3::InitializeSharedAudioStream` at the endpoint's minimum engine period; `IAudioClient::Initialize` with an explicit small buffer, default two engine periods), `--render-buffer-frames N` (explicit buffer, 8..48000), `--render-fill full|period|N` (frames written per event: everything free, one period, or N) and `--render-prefill full|period|N` (silence queued before the start; with `period` fill this sets the steady queue). The default is a typical shared-mode 10 ms client: `shared-default`, one period written per event, one period queued (earlier versions of the tool topped the whole buffer up, `--render-fill full --render-prefill full`, which adds about 11 ms of queue that real applications rarely have). The run prints the stream's buffer size, period, the endpoint's minimum / default engine period, the engine period before and after the stream started, and per click the padding (frames queued ahead of the click) at submit time. `capture-probe impulses` takes the same options plus `--cmode`, `--period-frames` and `--keepalive`, and measures the render side alone (no Yamaha).

### What is injected and what is timestamped

1. **Injection.** A 2 ms raised-cosine burst of 1.5 kHz (or a short chirp, `--shape chirp`) at -40 dBFS peak (never above -35) is written into a `VBMatrix In` render endpoint through a normal shared-mode, event-driven WASAPI stream at the endpoint's mix format, like an application would. The QPC is taken right before `ReleaseBuffer` of the buffer holding the click. The stream renders silence the rest of the time, so the endpoint's render clock runs throughout. An estimate of when the click leaves the render client is recorded too (`IAudioClock::GetPosition` plus its QPC, cross-checked with `GetCurrentPadding` plus `GetStreamLatency`).
2. **Source-side detection (stage 1, ring-A tap).** At the bridge source tap (the ring-A frames the engine consumes) the first frame at or above a threshold (click level - 20 dB) gives the QPC at callback entry; this is where the click reappears after VBMatrix has routed it through the bridge slot.
3. **Loopback detection (stage 2).** The engine, started with `EngineOptions::loopback_probe`, creates two extra ASIO input buffers on the Yamaha's loopback pair ("UR22C Streaming L/R", picked by name, else inputs 2 and 3) and, only while armed, scans input half `index` in every `bufferSwitch` for the first sample above a threshold derived from the measured baseline (see "Baseline and thresholds"; the former fixed offset click + forced preamp - 25 dB is now only the lower bound). The result is the QPC at callback entry plus the sample offset in the block, and the click's peak, which is used to correct the threshold crossing back to the click's first frame (a 2 ms click crosses a low threshold a few samples after its start). Without the option no extra buffers exist and the callback does no extra work; when not armed the callback does one relaxed load.

The router runs its normal pipeline (bridge ring A, resampler, EQ, Yamaha ASIO output) with the preamp forced to -20 dB, the limiter on and no live config reload, so the click reaches the Yamaha at about -55 to -60 dBFS.

### Baseline and thresholds

`router-cli latency --baseline [--seconds 3]` renders nothing and prints, for both capture channels and both loopback inputs, peak, RMS, DC offset, raw min / max, the fraction of blocks above -90 / -80 / -70 / -60 dBFS, whether the level is steady or time-varying, the high-passed levels, the strongest spectral peaks and the read-only state of `%APPDATA%\Yamaha\dspMixFx UR-C\ur22c_setting.xml` (its mixer parameters are an unnamed binary blob; it is the last save, not live state). `latency` itself measures the same statistics for 1 s before the clicks and derives the thresholds from them:

| | threshold | must stay |
|---|---|---|
| capture | `max(click - 20, floor + 12)` dBFS | at most `click - 6` |
| loopback | `max(click + preamp - 25, floor + 12)` dBFS, DC (the baseline mean) removed first | at most `click + preamp - 6` (the expected level; with the high-pass detector plus the envelope gain, about -4 dB) |

`floor` is the 99th percentile over blocks of the per-block deviation from the mean (rare outliers do not set it). If the loopback floor is too high the run aborts and prints the requirement: with the forced -20 dB preamp a -35 dBFS click reaches the loopback at about -55 dBFS, so the floor must be below -73 dBFS (-40 dBFS click: below -78 dBFS). Programme audio still aborts: router output above -40 dBFS, anything above -60 dBFS at the capture sink, loopback deviation above -40 dBFS. The raw loopback limit is relaxed to -20 dBFS **only** for `--shape burst:<HZ >= 15000>` with the high-pass active (the click then lies far above a microphone's band); a chirp, a lower burst or a manual `--hp-hz` with such a shape keeps -40 dBFS. When the relaxed limit lets a loopback above -40 dBFS through, the run prints a one-line `*** SAFETY NOTE` with the measured raw peak. The router output limit of -40 dBFS is never relaxed. A crossing whose level is more than 12 dB from the expected click level is ignored (stray event) and the detector re-armed.

**High-pass detector.** The loopback "UR22C Streaming L/R" is the interface's mix, and with a live microphone on Input 1 monitored into that mix it carries room noise. Measured here (tray app closed, nothing playing, 3 s): RMS -46 dBFS, peak -32 dBFS, identical on L and R, time-varying (block RMS sd 2 to 8 dB), energy below about 12 kHz (above 13 kHz the level is -87 dBFS RMS white noise), at the same level as the Windows "Input 1/2" endpoint (left channel, Input 1) and the "Streaming" endpoint. A -55 dBFS click is buried there; the old fixed thresholds (-70 to -85 dBFS) crossed at once, which is why the baseline check aborted. A burst at 18.5 kHz is not buried: the detector then runs the loopback through an 8th-order Butterworth high-pass (15.75 kHz for this shape) and a 32-sample sliding RMS; floor about -82 dBFS, click envelope about -59 dBFS for a -35 dBFS click. The onset bias of that detector (filter delay, window, threshold) is computed by running the same filter on the nominal click. `--hp-hz N|off` overrides the automatic choice (on when the lowest frequency of the click is at least 15 kHz). The click has to survive the router EQ profile at 18.5 kHz; the measured level at the loopback (-59 dBFS) shows that it does with the profile in use.

### Definitions (milliseconds from the injection QPC)

| Figure | Definition |
|---|---|
| inj->cap | Injection to the capture packet that carried the click: render side + cable driver + capture event delivery. Compare with the model "render side + capture staleness". Each click lands at a different point of the packet cycle, so single values scatter over one packet (10 ms on the Hi-Fi Cable, delivered in pairs) |
| (a) total | Injection to the click's arrival on the Yamaha loopback input stream. Sample `j` of an `N`-sample input block is taken as `N - j` samples before the callback instant, so the figure has no block quantisation but still contains the driver's whole input latency |
| (b) corrected | (a) minus the driver-reported input latency beyond the block (`getLatencies` input latency `in_lat` minus `N`; 24 samples for 280 at 256): the time at which the click was at the device's input boundary, `t_callback - (in_lat - j) / rate`. **Estimate**: it trusts the driver's number and assumes the loopback path has the latency of an ordinary input |
| cap->tap | (b) minus inj->cap: ring + resampler + EQ + the ASIO block + the Yamaha's path to the loopback tap |
| to-DAC est. | (b) plus 0 to `out_lat - N` samples (375 - 256 = 119 samples = 2.5 ms): the part of the output path after the loopback tap. The tap is assumed to lie after the output buffer (`N` samples) and before the DAC, so the sample is at the tap between `N` and `out_lat` samples after the router wrote it. The table prints the midpoint, the breakdown gives the range. **Estimate** |
| render q | Injection to the click leaving the render client, clock estimate (padding estimate if the clock reading is unusable; both are in `--json`). Only the first part of the render side |

**Included** in (a) and (b): render queue, cable driver, capture delivery, ring, resampler, EQ, the router's output block, the Yamaha driver's output buffering up to the loopback tap and its input path to the host. **Not included**: the DAC and the analog stage, and the USB output path after the loopback tap (only covered by the to-DAC estimate). All timestamps are QPC (one clock); resolution is one sample on the loopback and one packet on the capture side.

### Reading the breakdown

The last lines compare the measurement with the latency panel's model `Yamaha out + ring + source staleness + render side` (the render side is the source default, **20 ms for the `vbmatrix` bridge source**, until a measured entry exists; the first model used 10 ms). The figures below are pre-bridge VAC measurements; the measured VAC value is **23.8 ms** (run saved 2026-10-10; see [Where the render side comes from](#where-the-render-side-comes-from-238-ms)):

- `measured to-DAC equivalent` is the median over the impulses, with its range.
- `implied render side = measured - (Yamaha out + ring + source staleness)` is the number to compare with the panel's render side (20 ms bridge default; the pre-bridge VAC run measured 23.8 ms). It also absorbs what the model leaves out (resampler and EQ delay, block quantisation), so a modest excess is expected.
- `injection -> capture delivery` against `render side + capture staleness`, and `capture -> loopback tap` against `ring + ASIO block`, show which stage is off. The direct render side is `inj->cap` minus the measured capture staleness.
- On the Hi-Fi Cable the ring is a sawtooth (pairs of packets), so single totals scatter by up to about one pair; min and max show that. An impulse is flagged as an outlier if it is more than `max(2.5 ms, 3 sigma_MAD)` from the median. Use at least 5 impulses, repeat after restarting the engine, and compare medians.

### Where the render side comes from (23.8 ms)

> **Historical (pre-bridge).** This saved run measured the VAC "Line 1" source and is kept verbatim.

Saved run (`%APPDATA%\eq_asio_router\latency_measured.toml`, VAC Line 1, legacy 10 ms shared-mode client). **Measured** unless marked model.

Formula: `render_side = to-DAC mid - (Yamaha out + ring + capture staleness)` = 51.49 - (7.8 + 16.9 + 3.0) = **23.8 ms** (measured; the terms are the router's own readouts, Yamaha out from the driver latency).

Decomposition of the 23.8 ms (the split is **model/inference**, only the sum is measured):

| Part | About | Nature |
|---|---|---|
| Client queue plus the Windows shared-mode engine pass for a legacy 10 ms client | 21 ms | inherent to ordinary Windows apps; it would exist with no router at all |
| VAC driver transfer plus capture wake-up | 2.3 ms | residual of inj->cap after the 21 ms |
| Residual (resampler, EQ delay, rounding) | 0.5 ms | what the model leaves out |

Points to keep straight:

- **The 10 ms + 10 ms split inside the 21 ms is a model.** `GetStreamLatency` returns 0.0 on this endpoint and the padding is 0, so neither shows the queue directly.
- **Nothing the router or the app requests changes it for a legacy client.** Render keep-alive (engine period 10 -> 1 ms), prefill 0 and a smaller requested buffer leave a legacy `IAudioClient::Initialize` stream on its own 10 ms period (measured 24.9 ms without keep-alive, 23.8 ms with, inside the scatter). Only an `IAudioClient3` client at the 1 ms period gets about 0.2 ms render queue and about 7 ms click -> capture (measured); there is no full-chain figure for it because the router missed most of its 1 ms clicks (cause unknown).
- **Caveats of the measurement.** The test client timestamps right after the buffer event, so it omits the 0 to 10 ms wait an interactive application adds before it writes. The implied render side absorbs ring mean fill swings (809 versus 931 frames is 2.5 ms). The to-DAC and corrected figures rely on the driver-reported latencies (280 in, 375 out).

### Storing the result

After a successful run with at least 3 detected clicks the command writes (read-modify-write, additive, atomic) `%APPDATA%\eq_asio_router\latency_measured.toml`: one `[[entries]]` per source (the fixed key `vbmatrix`), ASIO buffer, source-period field and render profile, holding the median `render_side_ms` (the implied render side: the to-DAC equivalent minus Yamaha out + ring + source staleness, as printed), `render_queue_ms`, `inj_to_capture_ms`, `capture_to_tap_ms`, `total_ms` (corrected), `to_dac_ms`, the number of clicks, the date and the engine parameters (`asio_buffer`, `capture_period_frames`, `safety_ms`, `target_frames`) and the render profile. `--no-save` never writes; an aborted run or fewer than 3 clicks never writes. A run with a non-default render stream is written only with `--save`, under its own render profile; the panel only uses the `default` profile (typical 10 ms client). The engine reads the file once per session; an entry is used when the source key matches, the ASIO buffer is equal, the source period is within 25 % and it is at most 30 days old. `router-cli latency` itself starts the engine without reading the file, so its model line shows the pure estimate. API: `eq_core::MeasuredLatency` (`load`, `save`, `upsert`, `lookup`, `render_side`), `RenderSide`, `RenderBasis`.

### Safety limits

Click at most -35 dBFS (default -40), refused above in the library and the CLI; router preamp forced to -20 dB, limiter on, so the Yamaha output is about -55 dBFS; at most 10 impulses per run, each waiting at most 4 s; the sink must be an allowed injection endpoint (a known virtual cable or a `VBMatrix In` render endpoint) unless `--allow-sink` names it; the run aborts if the router output exceeds -40 dBFS (always, during the baseline and every click wait), if the sink is not silent (above -60 dBFS), or if the loopback carries programme audio (above -40 dBFS raw; -20 dBFS only for a >= 15 kHz burst with the high-pass) or its noise floor is too high to detect the click (see above); the Yamaha is released on every exit path; nothing in Windows is changed.

### No-router baseline

`capture-probe wdm-baseline` measures the same quiet `burst:18500:2` click WITHOUT the router: a default legacy render client (shared mode, event driven, one period per event, one period queued, the `router-cli latency` default profile) writes into the Yamaha WDM endpoint "Music (Steinberg UR22C)" and the click is detected on the Yamaha WDM capture endpoint "Streaming (Steinberg UR22C)" (shared, default period, a QPC per packet), which carries a device-side loopback of the output mix (and the live Input-1 mic). It gives the latency of Windows' own path to compare with the router chain's `(b) corrected` figure (about 49 to 52 ms; saved VAC median 50.2 ms), i.e. what the router really adds.

Run with the router stopped, the Yamaha ASIO driver closed, and the **physical Yamaha headphone knob lowered first** (the click goes to the real output). Command line:

```
capture-probe wdm-baseline --allow-sink "Music (Steinberg UR22C)" --confirm-knob-down --count 10 --interval-ms 1500 --db -40 --router-total 50.2
```

`--confirm-knob-down` is required only when the Windows endpoint volume is above 0.5 (the Music endpoint is at 1.000); the tool never changes the Windows volume or mute. `capture-probe wdm-baseline --selftest` checks the detector and arithmetic on synthetic data without touching any device.

How it measures:

- Detection reuses the router's detector from `wasapi_capture::band` (shared with `router-engine`, which re-exports it): 8th-order Butterworth high-pass at 15750 Hz, 32-sample sliding RMS envelope, first crossing, onset-bias correction (`band_onset_bias_frames`). Threshold = floor + 12 dB, where the floor is the 99th percentile of the per-packet envelope peaks over the first 0.7 s of the capture (after a 0.1 s filter warm-up), measured BEFORE anything is rendered; the run aborts (nothing rendered) if the raw capture deviates by more than -20 dBFS (programme audio) or if the floor is too high to detect the click (threshold above the expected envelope minus 6 dB). A click is searched only in packets delivered after its injection.
- Timing: injection = QPC right before `ReleaseBuffer` of the buffer holding the click; detection time = the packet's QPC at callback entry with the packet's last frame anchored there, minus the onset bias. Per click it prints injection to detection (ms), the render queue by the IAudioClock estimate and the envelope peak, then n / min / median / mean / max / sigma, the Music and Streaming endpoint periods, and the capture-side buffering estimate (capture period + `GetStreamLatency`, measured on a throw-away initialised client, never started).
- Comparison: `router added latency = router_total - direct_total` (default router_total 50.2, `--router-total MS`), with the range `[router - direct, router - direct + capture-side buffering]`. Caveat (also printed): the direct figure still contains the capture-side buffering (up to one capture period plus stream latency), while the router's figure is referenced to the Yamaha input boundary with the ASIO input latency subtracted; both include the Windows engine pass and the Yamaha driver's own path; the Streaming endpoint carries the live mic (hence the 18.5 kHz burst); the figure is for the WDM path with the router stopped, WDM/ASIO coexistence is not tested by it.

Safety rules enforced in code: the sink must be named with `--allow-sink` (the existing `is_sink_allowed` guard, checked on the argument and on the resolved endpoint name); click at most -35 dBFS (default -40); the burst shape is fixed to `burst:18500:2` (other shapes are refused) and no render-profile flags exist; the Windows volume and mute are read (read-only) and printed prominently before anything is rendered, and above 0.5 (or unreadable) the run exits with status 2 without `--confirm-knob-down`; a muted endpoint exits 2; at most 10 clicks, interval 1200 ms or more, the planned run and a runtime watchdog are limited to 20 s. Not yet run on hardware: the numbers have to be recorded from a live run.

### What has been verified

- Unit tests: detector, threshold-bias correction, buffer-half and input-latency maths, statistics, guards, and a pipeline test in which a simulated loopback input mirrors the router's output delayed by N samples (the detected sample position equals the first output sample above the threshold plus N). `latency --selftest` repeats the maths on a synthetic loopback. The synthetic path uses the same input-latency convention as the code, so it proves the arithmetic, not the convention.
- Hardware, no Yamaha: `capture-probe impulses --target "Hi-Fi Cable Input" --device "Hi-Fi Cable Output" --count 4 --db -40` measured injection to capture delivery of 112 to 123 ms in one run and 127 to 137 ms in another (two groups of values 10 ms apart, the pairs; the offset as a whole changed between runs). The WASAPI render queue was 12 ms by padding (576 frames queued, 1056-frame buffer, 480-frame period). So the Hi-Fi Cable driver seems to add a large, run-dependent delay between its render and capture sides that the 10 ms render-side estimate does not contain. This is a first observation, not yet confirmed through the whole router.
- Hardware, Yamaha (live, tray app closed, 2026-10-10, debug build): the "Streaming" inputs carry the interface mix (here the microphone on Input 1, see "Baseline and thresholds"). With `--shape burst:18500:2 --db -35` (loopback detection through the high-pass): Hi-Fi Cable source, 10 impulses: inj->cap median 106.2 ms (101 to 111, pairs of packets), cap->tap 48.4 ms, (b) corrected 155.2 ms (152.6 to 156.2, sigma 1.3); a 5-impulse run gave 152.6 ms (sigma 0.4). VAC Line 1 (fixed-period 288 fr, keep-alive), 5 impulses: inj->cap 39.7 ms, cap->tap 23.5 ms, (b) 63.2 ms (sigma 1.6). Implied render side (measured - Yamaha out - ring - capture): about 104 ms on the Hi-Fi Cable and 34 ms on VAC against the panel's fixed 10 ms. The WASAPI render queue by the clock estimate was 34 ms on both (the padding estimate said 12 ms), so on the VAC the render side is the render queue plus the capture period; on the Hi-Fi Cable about 70 ms more sit inside the cable driver.
- Render options (2026-10-10, capture-probe on VAC Line 1, no Yamaha): see the matrix in [5b](#5b-what-determines-the-render-side-vac-line-1-render-matrix). The render queue of 34 ms seen with the first tool version is 12 ms of buffer top-up by the tool plus about 20 ms inside the Windows engine; a typical client sees inj->capture 24 to 25 ms and an IAudioClient3 client 6 to 7 ms; the keep-alive makes no difference to a legacy client.
- Full-chain runs (release build, tray app closed; the Yamaha loopback was intermittently noisy, floor -30 to -50 dBFS after the high-pass against the required -76, so many attempts aborted at the baseline without any limit being changed; the quiet runs are in the table in 5b). Two stray-event guards were added after one run: a loopback crossing without a capture-side detection, before it, or more than 300 ms after it is discarded, not counted.
- Not yet verified: a full-chain figure for an IAudioClient3 client (clicks of the 1 ms-period stream are lost between render and capture); real games and players (which stream type they open); the latency convention of the UR22C (driver input latency 280 and output 375 samples are trusted for the corrected / to-DAC figures); whether the Hi-Fi Cable delay is a fixed driver buffer or depends on the cable's configuration; why the clock-based render queue estimate is constant to 0.01 ms; the DAC and the USB output path after the loopback tap (only the to-DAC estimate covers them); the `--baseline` mixer-state readout is limited to the plain XML fields because the mixer parameters cannot be decoded.

## 5b. What determines the render side (VAC Line 1 render matrix)

> **Historical (pre-bridge).** These measurements predate the VBMatrix bridge and are kept verbatim.

Measured 2026-10-10 with `capture-probe impulses --target "Line 1" --device "Line 1" --cmode fixed --period-frames 288 [--keepalive] --count 10 --interval-ms 600 --db -35` and the render options of section 5a (no Yamaha involved, debug build, medians of 10 clicks, VAC Line 1 at 48 kHz, quiet clicks into the cable only). "render q" is the clock-based estimate (`IAudioClock`) of the time the click spends in the injecting WASAPI stream; "inj->cap" is injection to the capture packet that carries the click (the render side plus up to one 6 ms capture period). The engine period column is that of the render endpoint after the test stream started.

| Render client (what it imitates) | stream buffer / period | keep-alive | engine period after | render q (ms) | inj->cap (ms) |
|---|---|---|---|---|---|
| **typical**: `IAudioClient`, default buffer, one period written per event, one period queued | 1056 / 480 fr | off | 10 ms | 19.4 | **24.9** (21.1 to 29.1) |
| typical | 1056 / 480 | on | 1 ms | 19.0 | **23.8** (21.7 to 28.5) |
| typical, nothing queued (prefill 0) | 1056 / 480 | off / on | 10 / 1 ms | 20.0 / 18.4 | 22.6 / 23.7 |
| min-buffer request (explicit 96 or 960 frame buffer): the request is ignored, same stream as typical | 1056 / 480 | off / on | 10 / 1 ms | 21.7 / 21.8 | 24.8 / 23.6 |
| buffer topped up completely every event (what the tool did before) | 1056 / 480 | off / on | 10 / 1 ms | 30.1 / 31.9 | 36.1 / 37.2 |
| **IAudioClient3** at the endpoint's minimum period (1 ms), one period per event | 104 / 48 | off | 1 ms (the stream itself lowers it) | 0.2 | **7.1** (3.6 to 7.6) |
| IAudioClient3, buffer topped up | 104 / 48 | off / on | 1 ms | 0.2 / 0.2 | 6.9 / 6.1 |
| IAudioClient3 | 104 / 48 | on | 1 ms | 0.2 | 6.3 |
| typical, plain shared capture (10 ms / 480 fr packets instead of 288) | 1056 / 480 | off / on | 10 / 1 ms | 19.4 / 20.7 | 24.8 / 26.7 |
| IAudioClient3, plain shared capture | 104 / 48 | off / on | 1 ms | 0.2 / 0.2 | 7.0 / 6.8 |
| Hi-Fi Cable (for comparison), typical client | 1056 / 480 | n/a | 10 ms (minimum 10 ms) | 30.0 | **103.5** (102.8 to 113.3) |

**Full chain through the router** (`router-cli latency --source vac --count 8..10 --db -35 --shape burst:18500:2`, release build, Yamaha 256 frames, capture 288 fr, medians, 2026-10-10; (b) corrected = injection to the click at the Yamaha input boundary, to-DAC = estimate):

| Client / keep-alive | inj->cap | cap->tap | (b) corrected | to-DAC est | implied render side | clicks |
|---|---|---|---|---|---|---|
| typical, keep-alive on (saved, run 2) | 26.3 | 23.6 | **50.3** | **51.5** | 23.8 | 10 |
| typical, keep-alive on (run 1) | 22.5 | 30.0 | 51.2 | 52.4 | 23.3 | 8 |
| typical, keep-alive off | 25.9 | 26.3 | **51.8** | 53.0 | 23.6 | 10 |
| typical, keep-alive on, plain shared capture (480 fr) | 28.6 | 24.2 | 53.0 | 54.2 | 24.0 | 8 |
| buffer topped up (old tool behaviour), keep-alive off | 37.1 | 28.3 | 64.9 | 66.1 | 35.8 | 8 |
| IAudioClient3 client | (6 to 9) | - | not obtained | - | - | see below |

The runs differ by 1 to 2 ms between each other and keep-alive on versus off is inside that scatter (the router's ring fill, which varies from run to run, moves capture -> tap between 23.6 and 30.0 ms). The numbers match the composite of the probe matrix (about 25 ms inj->cap plus about 24 to 30 ms for the router). For the **IAudioClient3 client** no full-chain figure exists: through the router the capture side missed most of the 1 ms-period clicks (the probe lost 0 to 3 of 10, the router runs 4 to 7 of 8) and the few detected ones were stray or implausible (a half-second capture -> tap), so only the render-side figure of about 6 to 9 ms inj->cap (a handful of clicks) stands; for planning, an IAudioClient3 client should be about 50 minus 18 = about 32 ms. Why the clicks of a 48-frame-period stream are lost (driver, test thread timing, or the click spanning two periods) is not understood.

Composite estimates: typical client about **50 to 52 ms** corrected (51.5 to 53 ms to the DAC estimate); IAudioClient3 client about 32 ms (not measured end to end); Hi-Fi Cable about 150 ms (103.5 + 45; matches the 152 to 164 ms measured end to end).

What this says:

- **The 34 ms render queue seen earlier was partly the test tool.** Its stream topped the whole 1056-frame buffer up on every event, so the click waited 576 frames (12 ms, "padding") in the client buffer before the engine even saw it. A client that writes one period per event sees 19 to 21 ms instead (inj->cap 25 ms instead of 36 ms), which is why the typical profile is now the tool's default.
- **About 20 ms is the legacy shared-mode floor**: a stream opened with `IAudioClient::Initialize` runs on its own 10 ms period (device period 480 frames), and a buffer request below that is ignored (the min-buffer rows show the same 1056 / 480 stream). The click waits for the next engine pass and the engine adds one period of buffering. Prefilling nothing saves 1 to 2 ms at best and risks dropouts.
- **The render keep-alive does not lower this.** It does lower the endpoint's engine period (10 ms to 1 ms, read back with `GetCurrentSharedModeEnginePeriod`), but a client with a legacy stream keeps its 10 ms period and its 20 ms: 24.9 ms without against 23.8 ms with the keep-alive. It is only useful as a stable 1 ms anchor for IAudioClient3 clients; those lower the period themselves anyway (the 1 ms engine period shows after their stream started, also without keep-alive).
- **IAudioClient3 clients get about 7 ms**, a gain of about 18 ms over a legacy client: render side plus capture period instead of 25 ms.
- **The capture period (288 versus 480 frames)** changes the capture staleness by about 1 to 2 ms and, in this test, inj->cap by about 0 to 3 ms (the pair of values is within the scatter of one packet); the 288 frame fixed period remains the supported choice for its smaller ring headroom, not for the render side.
- **The Hi-Fi Cable** cannot be improved from the render side: about 100 ms inj->cap for any client; its minimum engine period is 10 ms (no IAudioClient3 gain) and the cable driver adds about 70 to 80 ms inside itself.

### What applications can do (recommendations for games and players)

Router configuration (already the VAC preset): capture endpoint `Line 1 (Virtual Audio Cable)`, 6 ms fixed period, `render_keepalive = true` (keeps the engine period at 1 ms for clients that use it; no effect on 10 ms clients), Yamaha `PeriodFrames` as low as stays stable.

Windows and application settings (the Windows ones are done by the user in Settings; the router changes none of them):

1. Send the application's audio to `Line 1` (the Windows default playback device, or the application's own output-device selector). An application that outputs to another device bypasses the router altogether.
2. Disable audio enhancements and spatial sound on the `Line 1` playback device (Sound settings, device properties): they add processing and buffering in the engine. Keep the format at 48 kHz, 16 bit, 2 channels (the VAC lite limits); applications that render at another rate add a sample-rate conversion in the engine.
3. Exclusive mode is not allowed on this path: an exclusive stream bypasses the shared mix that the router listens to (and the router never opens the cable exclusively), so do not choose a "WASAPI exclusive" output option in players or games for this route.
4. Games and engines that expose an audio-latency, buffer-size or "output latency" option: choose the lowest value that does not crackle. This only helps down to the engine period: with the default 10 ms shared stream the floor is the measured 20 ms regardless of a smaller requested buffer (min-buffer rows above). Anything above (for instance a 1024 or 2048 frame mixer buffer) adds to it one for one.
5. The only way below 20 ms render side is a client that opens its shared stream through `IAudioClient3` / low-latency shared mode at the endpoint's minimum period (1 ms on Line 1): measured 7 ms inj->cap. Which games and players do so is **not verified**; Windows' own low-latency audio paths (the `AudioGraph` lowest-latency mode, applications that use the "low latency" WASAPI shared option) are the candidates. Streaming players such as Spotify use ordinary 10 ms shared streams and should be expected to behave like the typical client (about 25 ms).
6. Where the application offers an ASIO or WASAPI-exclusive output, do not use it for this path (it would not reach Line 1 or the router).

Expected totals on VAC Line 1 with the current router (Yamaha 256 frames): typical client about 50 to 52 ms (measured end to end), IAudioClient3 client about 32 ms (inferred), against about 63 to 65 ms for the earlier tool client (buffer topped up) and about 152 to 164 ms on the Hi-Fi Cable. The 25 ms gaming goal needs both an IAudioClient3-capable client and a smaller router term: capture -> tap (24 to 30 ms) is now the largest remaining term the router controls, mostly ring fill (at the first steady state of these runs the mean ring fill was 800 to 1050 frames, 17 to 22 ms, against a planner target of about 680 frames, 14 ms).

## 6. When to back off

> **Historical (pre-bridge).** The symptoms and fixes below refer to the removed WASAPI sources and are kept verbatim.

Raise `safety_buffer_ms` or the Yamaha `PeriodFrames`, or go back to the Hi-Fi Cable and shared capture, if any of these appear:

- `underruns`, `overruns`, `xruns` or `gate_rearms` increase; audible crackle; the fault state leaves Ok.
- The callback load (`cpu_max_pct`) approaches 100 %.
- `capture_fallback` is set (the GUI shows the amber banner), `capture_error` mentions corruption, or capture discontinuities rise on a VAC source.
- Dropouts only under USB or CPU load: the Yamaha period or mode is too aggressive.

## 7. First-run validation checklist (VAC Line 1)

> **Historical (pre-bridge).** This checklist validates the removed VAC "Line 1" source and is kept verbatim; the bridge's hardware checklist (M4) is in [plans/vbmatrix-bridge.md](plans/vbmatrix-bridge.md).

Status: the VAC path has been measured on hardware (capture-side runs and full-chain `router-cli latency` runs, sections 5a and 5b: about 50 to 52 ms to the DAC with a legacy client, render side 23.8 ms). Still unrun: a long soak with listening, a live fallback to Shared, and persistence of the Line 1 capture endpoint across a reboot. The planner target (about 600 frames at 288-frame packets, start target 784) is a model. Work through this once, in order, and stop at the first failure (go back to the Hi-Fi Cable preset, keep the log `%TEMP%\eq_asio_router_engine.log`).

Preparation

1. Rebuild with the tray app closed (it locks its exe) and quit the tray app before any test: it owns the engine mutex and the Yamaha. Rebuilding is also what activates the MMCSS fix; afterwards the `MMCSS ASIO` hardening row should be green.
2. Lower the system and amplifier volume. Make sure "Line 1" is active under Recording and is the Windows default playback device ([setup-guide.md](setup-guide.md#vac-line-1-quick-start)).
3. `router-cli holders` must list nothing that holds the Yamaha.

Quiet tests (no music, nothing audible)

4. `capture-probe vac-check --keepalive --repeats 3` (default 288 frames, quiet signal into Line 1 only). All repeats must PASS. A SKIP means the Line 1 capture endpoint is not active.
5. `capture-probe run "Line 1" 12 fixed --period-frames 288 --keepalive`: packets of 288 frames at a mean gap of about 6 ms; the corruption monitor must not trip (`--monitor-report` reports trips without stopping the capture; `--no-monitor` turns it off).
6. `router-cli run --source vac --seconds 60` with silence playing: the first lines must show the capture mode in use as fixed-period, `requested 288 fr` and period `288`, a keep-alive line (expected engine period 10.0 ms to 1.0 ms), no `WARNING:` fallback line. Note the values you actually see; they replace the expectations in this document.

10-minute music soak

7. Play ordinary music at a low volume into Line 1 and run `router-cli run --source vac --learn --seconds 660` (or use the GUI with the VAC preset and press "Learn (30 s)", then watch for 10 minutes). Watch the per-second line or the status panel for the whole time.
8. Counters and states that must hold for the whole soak:

| Counter | Expected |
|---|---|
| `under` / `underruns` | 0 |
| `xrun` / `xruns` | 0 |
| `fault` | `Ok` (never `Warn` or `Muted`) |
| `over` / `overruns`, `discarded`, `gate_rearms` | 0 and not increasing |
| `ring mean` versus `ring_target_frames` (the status line shows `ring x/y` and `ring mean`) | mean fill converges to the target and stays there; compare the target with the model (about 600 frames after Learn, start 784); a large persistent gap or a target that keeps climbing means the arrival pattern is worse than modelled |
| `capture_fallback`, `capture_error` | none (no amber banner) |
| `capture disc` (discontinuities), `silent` | not increasing during music |
| keep-alive | on, with the engine period change shown |
| `cpu_max_pct` | well below 100 % |

9. Listen for clicks (a duplicated 5 ms block sounded as an isolated click roughly every 30 s at 240 frames; 288 frames was clean in all earlier runs).
10. Repeat from the GUI: choose the "VAC Line 1 (low latency)" preset (first switch asks for confirmation), Start, soak again for a few minutes, then switch back to "Hi-Fi Cable (compat)" and confirm the old behaviour returns (period back to the driver default, keep-alive off, rate matching on).

Record the observed requested and granted periods, ring target and mean fill, `lat` breakdown and any fallback text. Then run `router-cli latency --source vac --count 5` ([section 5a](#5a-end-to-end-measurement-router-cli-latency)) to measure the real end-to-end latency and the render side.
