# Capture API survey for the EQ ASIO router (Windows 11 25H2, this machine)

> Purpose: survey of capture APIs that could give the router a smaller or more regular period than the Hi-Fi Cable.
> Date: 2026-10-10. Method: read-only enumeration and short quiet measurements with `capture-probe`; legend M / E / I below separates measured, earlier-measured and inferred values.
> Back to the [documentation index](../README.md).
>
> Corrections/updates: recommendation 3 (check whether VAC "Line 1" can capture at 1 ms) was followed up in [vac_lite.md](vac_lite.md): the endpoint offers a 1 ms capture minimum, but the VAC lite driver corrupts data below 5 ms, and 6 ms (288 frames) became the supported default.

Goal: find a way to get system / cable audio into the router with a smaller and more regular
period than WASAPI shared capture of the VB Hi-Fi Cable (480-frame packets delivered as
back-to-back pairs; router total 56.2 ms at the time of this survey: Yamaha out 7.8, ring mean fill 27.6, capture staleness
9.2, render side ~10 estimated; later measured on VAC Line 1: 23.8 ms for a legacy client, see [latency.md](../latency.md#where-the-render-side-comes-from-238-ms)).

Legend: **M** = measured on this machine in this session, **E** = earlier measurement quoted from
the brief, **I** = inferred from documentation / driver behaviour (not measured).

Router model used for estimates:
* staleness = E[b^2] / (2 r E[b]), b = packet size in frames, r = 48000. Regular 480-frame
  packets give 5.00 ms, regular 128-frame packets 1.33 ms, pair-wise delivery of 480-frame packets
  behaves like 960-frame packets every 20 ms = 10.0 ms (measured 9.2 ms).
* ring target = block 256 (5.33 ms) + safety + 1.25 x p99.9 dip + 2 frames. The dip scales with the
  largest arrival gap: pairs make it ~2 packet periods, regular delivery ~1 period plus scheduler
  jitter. Estimated ring saving from removing pairs: 8-12 ms (I).

## 1. Summary table

| # | API / path | Min period achievable here | Cadence measured | Engine bypass | Per-app | Rust effort | Expected router gain (vs 56.2 ms) | Verdict |
|---|---|---|---|---|---|---|---|---|
| 0 | WASAPI shared, Hi-Fi Cable (baseline) | 10 ms (engine fixed) | E: 480-fr pairs back-to-back | no | no | done | 0 | baseline |
| 1 | WASAPI shared + IAudioClient3 on capture endpoints | **10 ms on every active capture endpoint** (default = fundamental = min = max = 480 fr) | M: info only | no | no | trivial | none | dead end here |
| 2 | WASAPI exclusive capture | no clean exclusive format on any non-Yamaha capture endpoint (all report "exclusive accepts mix format: false"); Hi-Fi Cable exclusive corrupted by driver (E: 30 % samples OK at 3 ms) | not run | yes | no | done (session.rs) | n/a | dead end |
| 3a | **Process loopback**, include one process tree | engine period 10 ms (client buffer 480 fr) | **M: 999 packets, all 480 fr, mean 10.010 ms, p50 10.03, p99 10.74, max 42 ms (start-up outlier), 2 back-to-back, 0 discontinuities, peak -45.0 dBFS = the test tone** | no (taps stream pre-mix) | yes | low (prototype done) | I: -4.2 ms staleness, -8..12 ms ring | good (per-app mode) |
| 3b | **Process loopback, EXCLUDE a process tree (router's own pid)** = whole system minus router | same 10 ms | **M (3 runs, 10-15 s): all 480 fr; mean 10.009-10.013 ms; p99 10.9 / 11.0 / 16.3 ms; max 23-29 ms; 0 discontinuities; 0-7 back-to-back** | no | all other apps | low | I: -4.2 staleness, -6..12 ring, plus the cable capture stage disappears (I: 0..10 ms more) | **best option, no new software** |
| 4 | WDM-KS direct pin | cable capture pin: PCM 8-24 bit only (no float), 8-384 kHz, <= 2 instances; WaveRT support not determinable without opening a pin | M: enumeration only | yes | no | high | I: none vs exclusive (same driver ring) | not worth it |
| 5 | ASIO capture from VB-Matrix VASIO / Voicemeeter VASIO | I: set by host app, typically 128-512 fr; render endpoint "VBMatrix In 3" can run a 2.667 ms engine (M) | not loadable now (host apps not running) | yes | no | high | I: -10..-25 ms, speculative | only if the user accepts an extra always-on app |
| 6 | MME / DirectSound capture | I: >= 10 ms per buffer, >= 2 buffers; emulated on WASAPI | not measured | no | no | low | worse | reject |
| 7 | WinRT AudioGraph (LowestLatency) | = endpoint engine min = 10 ms on all capture endpoints here | not measured | no | no | medium | none | reject |
| 8 | HD Audio loopback (AORUS AD27QD) | 10 ms | E: regular 10 ms packets, never smaller; IAudioClient3 + loopback rejected | no | no | done (loopback.rs) | I: like 3b (-4.2 staleness, smaller dip) | works, but see section 5 |

## 2. Measured facts (read-only enumeration, this machine)

### Capture endpoints (`capture-probe list` / `info`, active ones)
| Endpoint | mix | GetDevicePeriod default / min | IAudioClient3 engine period default / fundamental / min / max | Exclusive accepts mix fmt |
|---|---|---|---|---|
| Hi-Fi Cable Output | 48 kHz 2 ch f32 | 10 / 3 ms | 480 / 480 / 480 / 480 fr (10 ms) | false |
| Microphone (2- Beyond) | same | 10 / 3 | 480 x4 | false |
| Microphone (Steam Streaming Microphone) | same | 10 / 3 | 480 x4 | false |
| VBMatrix Out 1, VBMatrix Out 2 | same | 10 / 3 | 480 x4 | false |
| Steinberg UR22C (4 endpoints) | not queried (Yamaha, held by the tray app) | | | |

No active capture endpoint can run its engine below 10 ms, so IAudioClient3 on the capture side
gives nothing. The "min 3 ms" of GetDevicePeriod is the exclusive-mode minimum.

### Render endpoints (apps -> sink side)
| Endpoint | engine default / fundamental / min / max | current |
|---|---|---|
| Line 1 (Virtual Audio Cable) | 480 / 1 / 48 (1.000 ms) / 480 | 10 ms |
| VBMatrix In 3 | 480 / 1 / 128 (2.667 ms) / 480 | 10 ms |
| Hi-Fi Cable Input (default render), CABLE Input, AORUS AD27QD, Steam Streaming Speakers, Beyond Audio Strap | 480 / 480 / 480 / 480 | 10 ms |

### Registry (MMDevices DeviceState)
* Active capture (state 1): Hi-Fi Cable Output, Microphone (2- Beyond), Microphone (Steam),
  VBMatrix Out 1 / 2, four Steinberg UR22C endpoints.
* Present in the registry but not returned by an active enumeration (state 0x10000001, undocumented
  high bit): Voicemeeter Out A1-A5 / B1-B3, VBMatrix Out 3-8, CABLE Output, and **Line 1** (the VAC
  capture counterpart; its render sibling is the one with the 1 ms engine). Their state cannot be
  changed here by rule. The VAC KS capture filter exists (section 4).
* Voicemeeter and VB-Matrix host apps are **not running**; `audiodg` is.

## 3. Process loopback (new, measured)

How it works: `ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, IAudioClient)`
with `AUDIOCLIENT_ACTIVATION_PARAMS { PROCESS_LOOPBACK, pid, INCLUDE|EXCLUDE_TARGET_PROCESS_TREE }`
in a VT_BLOB PROPVARIANT. The resulting IAudioClient has no mix format: the caller Initializes with
its own PCM format (`LOOPBACK | EVENTCALLBACK | AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`; we used
48 kHz f32 stereo). It returns the audio of the chosen processes **before the endpoint mix, endpoint
routing and device**. Windows 10 2004+ (build 19041); this machine is 25H2.

Measurement (`capture-probe proc-loopback <pid|name> <secs> [--rate HZ] [--exclude]`; the target was
`capture-probe tone 13 --db -45`, a second process rendering a -45 dBFS 1 kHz tone into the default
render, Hi-Fi Cable Input):

```
include target (tone pid):
  packets 999 frames 479520 silent 0 discontinuities 0 first packet after 15.8 ms peak -45.0 dBFS
  sizes: 480 fr x 999
  inter-arrival us: min 0 mean 10010 p50 10026 p99 10738 max 42368; back-to-back 2, late 1 of 998
exclude (unrelated shell pid) 10 s, with the tone and normal system audio:
  480 fr x 999; mean 10009; p50 10036; p99 16350; max 28922; back-to-back 7, late 12 (flagged irregular)
exclude, 15 s run A (peak -15.9 dBFS): 480 fr x 1499; mean 10009; p50 10070; p99 11000; max 25255; b2b 2; late 6
exclude, 15 s run B (peak -50.0 dBFS): 480 fr x 1498; mean 10013; p50 10070; p99 10901; max 23565; b2b 0; late 2
client buffer 480 frames (10.00 ms). 0 data discontinuities in all runs.
```

Findings
* The period is the engine's 10 ms; there is no way to ask for less (virtual client; no
  IAudioClient3 path; the documented Initialize uses buffer duration 0 and returned 480 fr).
* Delivery is **one 480-frame packet every ~10.01 ms with no pairs**. Staleness 5.0 ms instead of
  ~9.2 ms (-4.2 ms). Single stalls of 23-29 ms still occur (p99 10.9-11 ms in two runs, 16 ms in
  one), so the ring dip must still cover them; they are not systematic pairs.
* EXCLUDE mode captures all other processes' audio, independent of which endpoint they render to
  or later default-device changes. Not tested: behaviour with the endpoint muted or its volume < 1.
* Limits: protected (DRM) streams are silent; exclusive-mode and ASIO apps are not captured; apps
  still render audibly to their endpoint, so the default sink must stay a device the user does not
  hear (Hi-Fi Cable Input, as today). The cable is still in the render path, but the cable's capture
  pin and driver buffering leave the capture path (I: up to ~10 ms, unmeasured).
* INCLUDE mode captures a single app tree (per-app routing) with the same cadence.

## 4. Candidate details

### (1) IAudioClient3 on capture endpoints
`info` shows default = fundamental = min = max = 480 fr on every active capture endpoint. The drivers
publish no smaller period. Only render endpoints Line 1 (1 ms) and VBMatrix In 3 (2.667 ms) go lower.
The VAC capture counterpart is not active. If the user enabled VAC "Line 1" capture, `capture-probe
info "Line 1"` would show whether it also has a 1 ms capture min (I: probably symmetric, but its KS
pin is PCM 8-16 bit, <= 2 ch, <= 48 kHz, one instance only).

### (2) WASAPI exclusive capture on other devices
Every active non-Yamaha capture endpoint reports `exclusive accepts mix format: false` for the float
mix format (exclusive needs an integer PCM format). Hi-Fi Cable exclusive is known corrupt. The
other capture devices are real microphones, not a system-audio path. No timed test was run.

### (4) WDM-KS (read-only `capture-probe ks-list [capture|render|audio]`)
Only filter handles are opened and only IOCTL_KS_PROPERTY GET / BASICSUPPORT queries issued; no pin
is created. (The enumeration also opened, read only, the Steinberg UR22C filter handles; no pins,
no streaming state touched.)

| Filter | capture pin data range | pin instances (current / max) | note |
|---|---|---|---|
| VB-Audio Hi-Fi Cable | PCM <= 8 ch, 8-24 bit, 8000-384000 Hz (no float) | 0 / 2 | render pin also 0 / 2 |
| VB-Audio Virtual Cable | PCM <= 8 ch, 8-24 bit, 8000-192000 Hz | 0 / 2 | |
| Virtual Audio Cable (VAC) | PCM <= 2 ch, 8-16 bit, 22050-48000 Hz | 0 / 1 | driver of the 1 ms endpoint |
| VB-Audio Matrix VAIO (16 filters) | PCM <= 8 ch, 16-24 bit, 11025-192000 Hz | 0 / 1 and 0 / 4 | |
| VB-Audio Voicemeeter VAIO (16 filters) | same | same | host app not running |
| NVIDIA High Definition Audio (render only) | PCM 2 ch 16/24 bit 32/44.1/48 (96) kHz | 0 / 1 | |

* Current instances 0 for the cable: the engine opens its pin only while a stream is active.
  Max 2 means the driver allows a second opener, so a KS client and the engine could coexist at
  the pin level (I: the driver may still share one ring, as the exclusive test suggests).
* WaveRT: BASICSUPPORT of KSPROPERTY_RTAUDIO_HWLATENCY / BUFFER_WITH_NOTIFICATION on a pin factory
  returned ERROR_SET_NOT_FOUND (1170) for all pins. That is expected: WaveRT properties are answered
  by a pin instance, so support is **not determinable** without opening a pin (out of scope).
  WASAPI exclusive already goes through the same pin and corrupts at 3 ms, so KS direct would very
  likely do the same (I).
* Effort: high (SetupAPI path, KsCreatePin with a KSDATAFORMAT, WaveRT buffer mapping, position
  register, event notification, 24-bit PCM conversion, device removal). Gain vs exclusive: nil.

### (5) ASIO capture from virtual ASIO drivers (not loaded; reasoned from registry)
Registered (64 and 32 bit): VB-Matrix VASIO-8/32/64A/64B/128/256A/256B/512
(`c:\program files (x86)\vb\vbaudiomatrix\vbmatrix_vasioN_driver64.dll`, v1.0.2.2), Voicemeeter
Virtual ASIO / AUX / Insert / Potato Insert / VAIO3 (`...\voicemeeter\vbvm*_asiodriver64.dll`),
Steinberg built-in ASIO (`asiobtin.dll` v1.0.9.26, a shared-mode WASAPI wrapper: no period gain and
it could open real endpoints), Realtek ASIO, FL Studio ASIO, DDJ-FLX10, Yamaha Steinberg USB ASIO,
EQ ASIO Router. **None were loaded**: Voicemeeter and VB-Matrix are not running (VASIO drivers are
clients of their host apps; loading may fail or start the app / its engine), and the Steinberg
wrapper may open the UR22C. Buffer sizes (I, from VB-Audio documentation): chosen in the host app,
typically 128-512 fr, plus the host app's own routing buffer. The attractive part: render endpoint
"VBMatrix In 3" supports a 2.667 ms engine period (M), so app -> Matrix could be a ~2.7 ms hop.
Costs: (a) a second ASIO driver in one process: asio-host has a single global callback slot and ASIO
callbacks carry no user data, so two drivers need per-driver trampolines (macro/const-generic
statics) and separate threads; (b) the user must keep VB-Matrix or Voicemeeter running (extra
process and clock domain; drift is already handled by the resampler); (c) the Matrix to ASIO internals
are opaque. Effort high (2-4 days), gain speculative.

### (6) MME / DirectSound
waveIn needs >= 2 buffers, typically 10-20 ms each; DirectSoundCapture notification positions are
>= 10 ms. Both are emulated over the shared engine. Strictly worse. Not measured.

### (7) WinRT AudioGraph
`QuantumSizeSelectionMode.LowestLatency` selects the endpoint's minimum engine period through
IAudioClient3 (~2.67-3 ms on endpoints that publish it). Every capture endpoint here publishes 10 ms,
an AudioDeviceInputNode cannot loopback a render endpoint, and process loopback is not exposed by
AudioGraph. No gain, extra WinRT dependency. Reject.

### (8) Windows 11 engine-period behaviour and the render side
When any client uses IAudioClient3::InitializeSharedAudioStream with a period below the endpoint
default, the engine runs **that endpoint** at the smaller period for all shared clients until that
stream closes. It requires the driver to publish a smaller min. Here:
* Only render endpoints Line 1 (1 ms) and VBMatrix In 3 (2.667 ms) could shorten the app -> sink
  hop: a tiny low-period keep-alive render stream (router or helper) on such an endpoint makes the
  engine tick every 1-2.7 ms for unmodified apps rendering to it (their own application buffers,
  typically 10-30 ms, still apply). The current default sink, Hi-Fi Cable Input, only publishes 10 ms.
* The capture side of those two sinks is either not available (VAC capture inactive) or fixed at
  10 ms (Matrix WDM outs), so the benefit is real only with a VASIO / VAC capture path.
* With process loopback the capture side taps the stream before the endpoint, so the sink's engine
  period matters less (the tap rate itself is 10 ms).

## 5. HD Audio loopback vs a cable
Earlier (E): loopback of the NVIDIA/AORUS endpoint delivers regular 10 ms packets, never smaller;
IAudioClient3 + loopback is rejected (AUDCLNT_E_INVALID_STREAM_FLAG). New (M): that render endpoint
publishes default = fundamental = min = max = 480 fr, so a smaller period does not exist even
without loopback, and its KS filters offer only 2 ch 16/24-bit at 32/44.1/48 (96) kHz, one instance.
Versus the cable's pairs, loopback gives regular 10 ms delivery: staleness 5.0 vs 9.2 ms and a
smaller dip (I: about -10..-16 ms in total). It does not beat process loopback: it needs a
keep-alive render stream, the monitor / DisplayPort link must stay up (loopback dies with
AUDCLNT_E_DEVICE_INVALIDATED) and audio becomes audible on the monitor unless that endpoint is
lowered (whether loopback is post-volume depends on the driver: I, untested). The cable is better
only in being silent and monitor-independent; HD loopback is better only in cadence. Process loopback
has both properties.

## 6. Recommendation
1. Add **process loopback in EXCLUDE mode (own pid)** as a new source in `CaptureSession`
   (additive; `process_loopback.rs` already shows 480-frame, non-paired delivery, mean 10.01 ms,
   p99 ~11 ms). Keep Hi-Fi Cable Input as the default sink so PC speakers stay silent. Predicted
   router total ~40-44 ms (I): -12..-16 ms from staleness (-4.2) and ring dip (-8..-12); the missing
   cable capture stage may add more but is unmeasured. Add an INCLUDE-pid mode for per-app routing.
2. Verify end to end with the router running (outside this task): ring mean fill and capture
   staleness with the new source, behaviour with the sink muted / volume < 1, and exclusive-mode
   players (not captured).
3. For < 10 ms the only real routes are the Matrix/Voicemeeter VASIO path (VB host app always
   running + two-driver ASIO host work) or enabling the VAC "Line 1" capture endpoint and checking
   `capture-probe info "Line 1"` for a 1 ms capture min.
4. Do not pursue: IAudioClient3 on current capture endpoints, WASAPI exclusive, WDM-KS,
   MME/DirectSound, AudioGraph.

## 7. Tools added (all additive, `crates/wasapi-capture`)
* `src/process_loopback.rs`, `capture-probe proc-loopback <pid|name> <secs> [--rate HZ] [--exclude]`
* `src/ks.rs`, `capture-probe ks-list [capture|render|audio]`
* Cargo features `Win32_System_Diagnostics_ToolHelp`, `Win32_System_IO`, `Win32_Storage_FileSystem`,
  `Win32_Devices_DeviceAndDriverInstallation`, plus dependency `windows-core = "0.62"` (needed by
  `#[implement]`).
* Built with `--target-dir target\alt`; `cargo test -p wasapi-capture` 33/33 pass; clippy clean on
  the touched files. Binary: `target\alt\release\capture-probe.exe`.
* Test audio: `capture-probe tone ... --db -45`, 12-13 s, five runs into Hi-Fi Cable Input.
