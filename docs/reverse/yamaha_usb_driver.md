# Yamaha Steinberg USB Driver 2.1.9 - static analysis (UR22C, VID_0499 PID_172F)

> Purpose: how the Yamaha Steinberg USB ASIO driver computes and reports latency, which registry values and control-panel settings change it, and how a host should behave.
> Date: 2026-10-10. Method: read-only static analysis of installed binaries (disassembly with pefile/capstone, strings, registry reads); nothing executed or loaded.
> Back to the [documentation index](../README.md).
>
> Corrections/updates: open question U1 (section 7) is now confirmed by a real reading. On the author machine `PeriodFrames` 256 with `LatencyMode` 1 (Low Latency) reports 280 in / 375 out samples at 48 kHz (engine `caps.toml`), which equals the period plus the "test" row (24 / 119), not the Low Latency row of the section 3.2 table (120 / 167). So Low Latency selects the "test" row on this device. The measured output latency is 7.8 ms.

Scope: interoperability and latency tuning for the user's own installed driver and hardware. Read-only static analysis of
copies; nothing was executed, loaded, installed or sent to the device. Registry was only read (`reg query`).
Confidence tags: [H] high (read directly in code and cross-checked), [M] medium (inferred, consistent with data), [L] low / guess.

## 0. Method, tools, files

* Tools found: Python (miniconda), MSYS `strings`/`objdump`. No Ghidra/IDA/rizin/dumpbin/sigcheck. So: isolated venv in the
  scratchpad (`pefile`, `capstone`), own scripts (kept outside the data dir, run with `python -I`), linear disassembly with
  RIP-relative string/import annotation, hand analysis. **No decompiler was used**; everything below is hand-read disassembly,
  so register/field naming is my interpretation. A full decompile (Ghidra) would be needed to close the items in section 7.
* Copies + disassembly live in `...\scratchpad\re_yamaha\{bin,str,dis}`; scripts in `...\scratchpad\re_scripts`.
* Files (SHA-256), versions, all from `C:\Program Files (x86)\Yamaha\Yamaha Steinberg USB Driver\`:

| File | Size | Version | SHA-256 |
|---|---|---|---|
| ysusb_asio64.dll | 193896 | 2.1.8 | D87FF6307D508F01A2F866CD92687EB93F01D829F66E0BA1AA673AEB36B5F852 |
| ysusb_asio32.dll | 153448 | 2.1.8 | B873F1E121BC543CF7ECF93E4D2D12D52743A0A931159F7ACDDC35CDD2634344 |
| ysusb_bulklib64.dll | 173928 | 2.1.9 | C87BB0450868CB7C216C1CA8A954795B6B2BC64A5C254E051F0690AEF941C1B1 |
| ysusb_bulklib32.dll | 134504 | 2.1.9 | B55B965F4CC63527FF3C33A38A4ACA77D90681B637CFD09FD4B3278C47D25D4A |
| ysusb_cp.exe (x86 MFC) | 2136424 | 2.1.9 | F4A844A90BF772F9A8B2DA53B7BF128392EDEC35D7E263E9A2216D54D6B9B320 |
| ysusb_w10_64.sys | 198064 | 2.1.9.0 | 53EAB0EFD0F70D77669B3AA9182C9535680D031207F2DFF9FCE5B8BE9155E3C6 |
| ysusb_w10.inf | 29711 | DriverVer 05/20/2025,2.1.9.0 | 122F978A3F1520FFF4A8857B7E7DAE1D9695D2CBCBEBC35E0AFADD63399EDA8A |
| ysusb_w10.cat | 24646 | | DE81F67F5DB74D37CCA1307EF59239B467922B2160C649BDFE358836D7112275 |

The .sys is WHQL-signed (Microsoft Windows Hardware Compatibility Publisher, valid). Installed as `oem88.inf`; service `ysusb_w10_64`.
Source PDB paths (strings): `C:\Users\kurzw\source\CIOdriver\...` (project "CIOdriver").

## 1. Architecture

```
 DAW / our router            ysusb_cp.exe (MFC, 32-bit)            (3rd party / Yamaha apps)
   |  COM (ASIO)                |  registry + DeviceIoControl           |  COM
 ysusb_asio{32,64}.dll          |                                  ysusb_bulklib{32,64}.dll
   |  CreateFile + DeviceIoControl on  \\?\usb#vid_0499&pid_172f#...#{677bb410-d9a0-4d16-9af2-8d2e23f247d6}\asiocontrol
   +---------------------------+--------------------------------------+
                               v
              ysusb_w10_64.sys  (PortCls/WDM-KS audio miniport driver + private "AsioControl" interface, talks USBD URBs)
                               v  isochronous (and bulk) URBs + EP0 control transfers
                           UR22C hardware
```

* **ysusb_w10_64.sys** [H]: imports `portcls.sys` (PcInitializeAdapterDriver, PcAddAdapterDevice, PcNewPort/Miniport, PcRegisterSubdevice),
  `USBD.SYS` (USBD_ParseConfigurationDescriptorEx), ntoskrnl IRP/URB/DPC/timer/`PsCreateSystemThread`/`ExSetTimerResolution`/`ExAllocateTimer`.
  It is a WDM/KS audio driver (Wave + 16 MIDI ports per INF, `Include=ks.inf,wdmaudio.inf`, class MEDIA) that drives USB itself (no WinUSB, no libusb,
  no KMDF). The same driver serves Windows audio (MME/WASAPI endpoints: Input 1/2, Monitor, Streaming, Voice, Music, Speaker; loopback pins
  "Loopback Stream/Voice/AUX1/AUX2 L/R") and the private ASIO channel. Extra private device interface
  `{677BB410-D9A0-4D16-9AF2-8D2E23F247D6}` with reference string `\AsioControl` (sys wide string at 0x140020980; the GUID is also embedded in asio, bulklib, cp). [H]
  The live registry value `HKCU\...\AsioDevice` holds the full interface path (`\\?\usb#vid_0499&pid_172f#8&107d0b5&0&4#{677bb410-...}\asiocontrol`),
  written by the ASIO dll after enumeration (SetupDiGetClassDevs/EnumDeviceInterfaces/GetDeviceInterfaceDetail) and re-read by bulklib/cp. [H]
* **ysusb_asio{32,64}.dll** [H]: in-proc COM server, CLSID `{CB7F9FFD-A33B-48B2-8BC0-437D94F37142}` (confirmed as raw GUID at .rdata 0x18002ba80;
  `HKLM\SOFTWARE\ASIO\Yamaha Steinberg USB ASIO` matches). Exports DllGetClassObject / DllCanUnloadNow / DllRegisterServer / DllUnregisterServer / DllMain.
  Imports: SETUPAPI, KERNEL32 (CreateFileA, DeviceIoControl, CreateThread, CreateEventA, WaitForMultipleObjects, QueryPerformanceCounter, Sleep, CreateProcessA,
  TerminateThread, critical sections), **AVRT** (AvSetMmThreadCharacteristicsA, AvSetMmThreadPriority), WINMM **timeGetTime only** (no timeBeginPeriod),
  ADVAPI32 (registry), USER32 MessageBoxA (registration errors), ole32. PDB `ysusb_asio.pdb`. No named mutex/event/shared-memory strings. No libusb/WinUSB.
* **ysusb_bulklib{32,64}.dll** [M]: separate in-proc COM server "USB Bulk Transfer Library" (`CUsbBulk`, `CUsbBulkDev`). Opens the same `\asiocontrol`
  interface (reads `AsioDevice` from HKCU), uses IOCTLs 0x222000, 0x222028, 0x222044, 0x222048, 0x22204c, 0x222050, a waitable timer and
  PostThreadMessage to poll the driver and calls observers ("SampleRate/AsioDevice/AsioBufferPeriod/In|OutChannels/In|OutBytesPerSample/In|OutLatencyOffset changed"
  log strings). It does not do USB I/O itself and is **not** used by the ASIO dll (asio imports nothing from it). Consumers are other Yamaha apps / MIDI path
  (driver has `MergeBulkLibToMidi` value). Its use of the info is read-only status.
* **ysusb_cp.exe** [H]: control panel ("Yamaha Steinberg USB Driver Control Panel", MFC). Reads/writes registry and queries the driver via IOCTLs 0x222000,
  0x222024, 0x222028, 0x22202c, 0x222030, 0x22203c, 0x222040, 0x222054, 0x222058, 0x222060.

### IOCTLs on `\asiocontrol` (CTL_CODE: DeviceType 0x22 (FILE_DEVICE_UNKNOWN), Access 0, Function = (code>>2)&0xFFF, Method = code&3) [H unless noted]

| Code | Func | Method | Used by | Meaning (from caller context) |
|---|---|---|---|---|
| 0x222000 | 0x800 | BUFFERED | asio, cp, bulklib | GET_STATE: copies a 0x2ec-byte state block (driver devext+0x4dbc); contains rate mask, in/out latency, channel counts, clock list, "internal buffer samples" |
| 0x222004 | 0x801 | BUFFERED | asio | SET_CONFIG: 0x54-byte (21 dword) latency/buffer config, validated, see section 3 |
| 0x222010 | 0x804 | BUFFERED | asio | set sample rate (4-byte arg) |
| 0x222014 | 0x805 | BUFFERED | asio buffer thread | arm/wait before the loop (called once at thread start) |
| 0x222018 | 0x806 | BUFFERED | asio | stop streaming (in `stop()`) |
| 0x22201F | 0x807 | NEITHER (3) | asio createBuffers | create/lock buffers: 0x48-byte struct, user buffer pointers + 3 event handles (see sec 2) |
| 0x222020 | 0x808 | BUFFERED | asio dispose | release buffers |
| 0x222034 | 0x80D | BUFFERED | asio init | **claim device (exclusive owner)**; sleeps 500 ms and retries up to 5x, else "Device could not be opened." |
| 0x222038 / 0x22203C | 0x80E / 0x80F | BUFFERED | asio, cp | variable-size info (ERROR_MORE_DATA 0xEA two-call pattern): channel/clock/format descriptors, and shared status block |
| 0x222040 | 0x810 | BUFFERED | asio setClockSource | select clock source index |
| 0x222054 | 0x815 | BUFFERED | asio | set stream/DSD format mode |
| others (0x222024/28/2C/30/44/48/4C/50/58/60) | | | cp / bulklib | status / error-info / loopback / power queries (not decoded) [L] |

Exclusivity: no named object. Driver handler for 0x222034 (sys 0x14000e03f) takes the device mutex, and succeeds only if `devext+0x1e8` (owner FileObject) is NULL and the
current `InterruptTime` is >= a cooldown timestamp at `devext+0x810`; otherwise returns STATUS_ACCESS_DENIED (0xC0000022). So exactly one handle owns ASIO at a time, and a short
cooldown follows release (that is why the DLL retries). [H for owner check, M for cooldown semantics]

## 2. ASIO driver behaviour (ysusb_asio64.dll)

vtable at 0x180026dd0 (24 slots): init 0x180001fc0, getDriverName 1f50, getDriverVersion 1f70, getErrorMessage 1f80, start 1ff0, stop 20d0, getChannels 21d0,
getLatencies 2290, getBufferSize 2380, canSampleRate 23e0, getSampleRate 25e0, setSampleRate 2670, getClockSources 2950, setClockSource 2ab0, getSamplePosition 2bf0,
getChannelInfo 2ca0, createBuffers 2e70, disposeBuffers 3630, controlPanel 37e0, future 3960, outputReady 3c20. [H]

* **getBufferSize** (0x180002380): min = max = preferred = `PeriodFrames_effective` (obj+0xaf8), granularity 0. [H]
  `PeriodFrames_effective = PeriodFrames(registry) * k(rate)` where k = 1 (<=50 kHz), 2 (88.2/96k), 4 (176.4/192k), 8 (352.8/384k), 16 (>384k), then x16/x32 for DSD
  modes (code at 0x180004360-0x18000457). So the panel value is "frames at the base rate"; at 96 kHz the ASIO buffer is twice as many samples (same milliseconds). [H]
* **getLatencies** (0x180002290): issues IOCTL 0x222000, then
  `inputLatency = state.inLat * m + PeriodEff`, `outputLatency = state.outLat * m + PeriodEff` where `m` = obj+0xf08 = 1 for PCM (0x10/0x20 for DSD formats). `state.inLat/outLat` are
  computed **in the kernel driver** (function 0x14000a8b4, stored at devext+0x82c / +0x840, exported at state block offsets 0x140 / 0x150). [H]
* **Derivation of the user's 728 / 871 (PeriodFrames 512, LatencyMode 2, 48 kHz assumed)** [H for the algebra, M that rate=48k and the table row]:
  * Driver preset row for period 512, mode index 2 (Standard): ClassicFramesPerIrp2 = 4, OutBufferOperationOffset = 0xB0000007 (low 28 bits = 7), InBufferOperationOffset = 0x90000000.
  * `inDev  = tblIn(rate) + floor(rate * ClassicFramesPerIrp2 / 1000)` = 24 + 48*4 = **216**
  * `outDev = tblOut(rate) + floor(rate * (OutBufferOperationOffset & 0x0FFFFFFF) / 1000)` = 23 + 48*7 = **359**
  * `getLatencies = PeriodEff + dev` -> **728 / 871**. 
  * `tblIn/tblOut` are a per-model, per-rate table in the .sys .rdata (candidate for UR22C at 0x1400225D0, entries `{rate, in, out, 0}`):
    44.1k 24/23, 48k 24/23, 88.2k 35/34, 96k 36/35, 176.4k 58/57, 192k 60/60. 24 and 23 samples = about 0.5 ms fixed (USB/DSP/converter delay reported by the device table).
    The 216/359 "fixed offsets" are therefore NOT fixed: 24/23 are the true constant part; the rest (192 and 336 samples @48k = 4 ms and 7 ms) is the IRP/OS-side extra buffering
    chosen by LatencyMode and PeriodFrames.
  * Unit note: the "ms" fields are 1 ms USB frames; if bit 30 (0x40000000) is set in an *OperationOffset* the value counts in 1/8 ms (microframes, only used by the rows for periods <= 24 frames and by cp's "ISO Extra buffer"). Bit 31 selects the "sum of ms + table value" form used above.
  * Several terms in the driver formula (`[tbl+0xc]`, `[tbl+0x10]`, `[tbl+0x18]` of the capability struct at devext+0x908, and HubOffset) were assumed 0; this assumption is validated only by the fact that
    it reproduces both observed numbers exactly. There is also a bulk-mode branch (devext+0x4e80 == 2) with a different formula (not exercised here).
* **canSampleRate** (0x1800023e0): if registry `FixedSamplingRate` != 0 only that rate is accepted (else ASE_NoClock -996 / 0xFFFFFC1D). Otherwise walks the rate list
  {11025,22050,32000,44100,48000,88200,96000,176400,192000,352800,384000,705600,768000} (times the format factor) and tests it against the 13-bit rate mask returned by the driver
  (state offset 0x114 = obj+0xccc), under a critical section. setSampleRate -> IOCTL 0x222010 with the index. [H]
* **Buffers / data path**: `createBuffers` allocates the ASIO host buffers in user mode (Int32LSB, "channel %u" names), creates 3 events (obj+0xfb8/0xfc0/0xfc8) and passes
  **raw user pointers + events to the kernel in a METHOD_NEITHER IOCTL 0x22201F** (0x48-byte struct: size=0x48, +4=0x02000000 flag, +8 = frames per buffer (PeriodEff / m),
  +0xC/+0x10 = in/out channel counts, +0x18/+0x20/+0x28 = event handles, +0x30/+0x38 = in/out buffer pointers, +0x40 = 1). The driver locks the pages (MmProbeAndLockPages) and the
  USB completion/DPC path converts between the device format and Int32LSB directly in the ASIO buffers; user mode does no per-sample conversion. [H for structure, M for conversion location]
* **Buffer-switch thread** (start() -> CRT thread -> function 0x180006020): 
  * MMCSS: `AvSetMmThreadCharacteristicsA("Pro Audio")` then `AvSetMmThreadPriority(h, 2)` (AVRT_PRIORITY_CRITICAL); obj+0xfe0 is forced to 2 by the config loader; the constructor default -2 would instead make `start()` call `SetThreadPriority(THREAD_PRIORITY_TIME_CRITICAL)` (fallback, normally unused). No registry knob. [H]
  * Loop: `WaitForMultipleObjects(2, {stopEvent obj+0xfd0, driverEvent obj+0xfb8}, timeout)`; first timeout 3000 ms, afterwards about one buffer period in ms. Event 1 = driver says a buffer finished: reads
    the shared status block (mapped through IOCTL 0x22203C into obj+0xfa0: flags dword (bit1 rate change, bit2 overload, bit3/4 reset/latency change), write position, timestamp), computes
    how many periods are pending (`(pos - last) * m / PeriodEff`, SetEvent obj+0xfe8 if > 3), then calls the host: `callbacks->bufferSwitchTimeInfo(&timeInfo, index, TRUE)` if the host
    supports time info (obj+0xb12) else `bufferSwitch(index, TRUE)` (function 0x180003b20). directProcess is always TRUE, so the host callback runs on this MMCSS thread. [H]
  * No spinning: pure event wait + QPC timestamping (QueryPerformanceCounter, up to 120000 samples of jitter statistics, obj+...). No timer.
  * Timestamps: `timeGetTime()` (ms) and driver QPC-like 100 ns counter -> ASIO time info; getSamplePosition uses the driver position.
* **outputReady** (0x180003c20): returns ASE_NotPresent (-1000 / 0xFFFFFC18) if BufferFlags bit 4 (0x10) set; if the host called outputReady support (obj+0xb10) and BufferFlags bit 2 set (default flags 0x14434F has it)
  it signals the output event, i.e. the driver can release the output buffer early ("delayed output buffer switch" behaviour). [M]
* **DropoutDetection**: registry bool -> obj+0xba1. `future(kAsioCanReportOverload = 0x24042012)` returns ASE_SUCCESS only if enabled (sets obj+0xba2). When the driver flags an overload
  the thread calls `asioMessage(kAsioOverload = 15)`; rate/latency changes raise `sampleRateDidChange` and `asioMessage(6 = kAsioLatenciesChanged)`; a watchdog timeout raises a reset request. The router should
  handle kAsioOverload and kAsioResetRequest/LatenciesChanged. Other `future` selectors implemented: kAsioCanDoIoFormat/GetIoFormat (DSD), kAsioGetInternalBufferSamples (0x25042012: returns state[0x280], state[0x288]). [H]
* Error strings: "Device could not be opened.", "This setting requires more CPU power than is available." (shown via getErrorMessage when the driver latency query returns 0 after 5 retries of Sleep(500)). [H]
* Exclusivity: see section 1 (driver-side owner object, IOCTL 0x222034).
* controlPanel(): `CreateProcessA` of `ysusb_cp.exe` from the DLL's directory (format "%s%s%s"). [H]

## 3. Registry / configuration

Key: `HKCU\Software\Yamaha\Yamaha Steinberg USB Driver` (read by asio dll `init`, i.e. every time the ASIO object is initialised; also by cp, bulklib reads only `AsioDevice`).
Registry values are only sent to the kernel in IOCTL 0x222004 during `init`; **there is no live reload**: changes apply the next time the host creates/initialises the ASIO driver (our router must unload/re-`init` the COM object, not just stop/start).
The sys additionally reads a second set of the same names plus `DefaultSampleRate`, `UseDeviceSampleFormat`, `SuggestedBufferPeriod`, `MergeBulkLibToMidi`, `InMute/OutMute/InVolume/OutVolume` from the *device* (HKLM Enum ...\Device Parameters / driver `Settings`) key at AddDevice time; none of these exist on this machine (Device Parameters only has EnumerationRetryCount/PortNameGuid, `Settings` created by INF with Mute/Volume = 0).

### 3.1 Values read by the ASIO dll (loader at 0x180003cc0; default = compiled-in default applied before reading) [H]

Config block sent to the driver (IOCTL 0x222004, 0x54 bytes). "Validation" = driver check (sys 0x14000be52); a failed check returns an error (ASIO init fails).

| Dword idx | Registry value | Default | Driver validation | Meaning (derived) |
|---|---|---|---|---|
| 0 | FirstPacketLatency | 20 (0x14) | <= 0x400 | frames/ms of lead before first packet; startup only [L] |
| 1 | ClassicFramesPerIrp | 4 | <= 0x20 | iso frames (1 ms each) per IRP, full-speed path |
| 2 | MaxIrpNumber | 4 | 2..8 | number of iso IRPs kept in flight [M] |
| 3 | PreSendFrames | 0 | <= 1000 | silence frames pre-sent to the output pipe at start [M] |
| 4 | OutputFrameDelay | 0 | <= 16 | extra output delay frames [L] |
| 5 | DelayedOutputBufferSwitch | 0 | (none) | 0/1: delay output buffer switch relative to input [M] |
| 7 | InBufferOperationOffset | 0x90000000 | low 28 bits <= 0x800 | extra input buffering (see encoding below) |
| 8 | InHubOffset | 0 | <= 0x800 | extra input scheduling margin for hub (USB2) [L] |
| 9 | OutBufferOperationOffset | 0x90000004 | low 28 bits <= 0x800 | extra output buffering (ms) |
| 10 | OutHubOffset | 0 | <= 0x800 | as above for output [L] |
| 11 | BufferThreadPriority | 30 (0x1E) | <= 0x1F | kernel thread priority of the driver's streaming thread (KeSetPriorityThread); 30 is already near maximum |
| 12 | BulkRequestPeriod | 10000 (0x2710) | <= 0x4E200 | bulk polling/request period in 100 ns units (1 ms default, max 32 ms), bulk path only |
| 13 | BufferFlags | 0x14434F | | flag word, see 3.3 |
| 14 | ClassicFramesPerIrp2 | 4 | | same, high-speed (microframe) path; **this is the one that determines input latency in the formula** |
| 15 | BulkPacketsPerIrp | 0 | | bulk packets per IRP (>0 only for periods <= 16 in presets) |
| 16 | OutBulkOperationOffset | 8 | | output extra buffering on the bulk path |
| 18 | LatencyMode | 2 | | 0 = use registry values as is; 1/2/3 = driver presets (below) |
| 19 | PeriodFrames | 512 (0x200) | must match a preset row | ASIO buffer size at base rate |
| - | DropoutDetection | 1 | | ASIO overload reporting (3.4) |
| - | FixedSamplingRate | 0 | | if nonzero, canSampleRate accepts only this rate |
| - | AsioDevice (REG_SZ) | | | device interface path (written by dll/cp) |

Encoding of *OperationOffset values: bits 0-27 value; bits 28-29 select a base term (1 = capability-table base, 2/3 variants add a device-reported hub/base term; 0 none); bit 30 = value in 1/8 ms
(microframes) instead of ms; bit 31 = "signed/absolute form" (latency = value-time + table constant). Presets use 0x9.., 0xA.., 0xB.., 0xE.. prefixes only. 0xFFFFFFFF in the *panel* keys (BasePeriod, IsoOffset, BulkBuffer) = "Auto".
Important quirk: if `LatencyMode != 0` the ASIO dll overwrites BufferFlags with 0x14434F in its copy and the **driver then overwrites** dwords 1,14,9,7,15,16 (Classic*, Classic2, Out/InBufferOperationOffset, BulkPacketsPerIrp, OutBulkOperationOffset) and the flag word from its internal preset table (sys 0x14000b6ac), keeping only flag bits 0x400000 and 0x800000 from the registry flags. The zeros that cp writes for those values in "Auto" mode are therefore harmless, and **manual values for them only take effect with LatencyMode = 0**.

### 3.2 LatencyMode and the control panel

| LatencyMode | Panel "Mode:" | Driver preset row index | Flags | Notes |
|---|---|---|---|---|
| 0 | (Advanced / override) | registry values | BufferFlags registry (default 0x14434F) | no preset |
| 1 | Low Latency | **index 1 normally; replaced by index 0 + flags 0x1540EF if the device's rate-mask field (state+0x114) is non-zero** [see uncertainty U1] | 0x14434F or 0x1540EF | |
| 2 | Standard (current) | 2 | 0x14434F | |
| 3 | Stable | 3 | 0x14434F | |

Preset table in the sys (0x1400238F0, 18 rows x 100 bytes: period, then 4 modes x {ClassicFramesPerIrp, ClassicFramesPerIrp2, OutBufferOperationOffset, InBufferOperationOffset, BulkPacketsPerIrp, OutBulkOperationOffset}).
Valid PeriodFrames values (exact match required): **2048, 1536, 1024, 768, 512, 384, 256, 192, 128, 96, 64, 48, 32, 24, 16, 12, 8, 4**. The control panel lists only the first 13 (>= 32); 24..4 are in the driver but hidden. [H]
Extracted rows (ClassicFramesPerIrp2 / Out offset low bits in ms) for 48 kHz and the resulting device-side latency add-on in samples (in/out, excluding PeriodFrames), computed with the formula in section 2:

| Period | m0 "test" (flags 1540EF) | m1 Low Latency | m2 Standard | m3 Stable |
|---|---|---|---|---|
| 2048..768 | 24 / 215 | 168 / 215 | 216 / 407 | 408 / 791 |
| 512 | 24 / 167 | 168 / 215 | **216 / 359** | 408 / 695 |
| 384 | 24 / 167 | 120 / 167 | 168 / 311 | 408 / 599 |
| 256 | 24 / 119 | 120 / 167 | 168 / 263 | 408 / 503 |
| 192, 128 | 24 / 119 | 120 / 119 | 168 / 215 | 408 / 407 |
| 96, 64 | 24 / 71 | 72 / 119 | 120 / 167 | 312 / 311 |
| 48, 32 | 24 / 71 | 72 / 71 | 72 / 119 | 216 / 215 |

(For m0 input the classic-frames term is dropped because flag 0x80 is set in 0x1540EF.) Row values for periods < 32: m2 uses Classic2 = 1 and out offset 2 ms (periods 32/48) down to 1-2 ms; rows <= 24 use bulk packets (1) and microframe offsets (0xE0000004 = 4 x 125 us) - see dump in scratch notes. Predictions: [M].

### 3.3 Values only used by the control panel (cp "Advanced" tab) [H for names/mapping, M for meaning]

* `BufferFlagsSetting` (0 = auto/presets; 1..5 = Advanced "Flag Override" combo: 1 "1540ef TEST ONLY", 2/3/4 = 14434f Low Latency/Standard/Stable, 5 "14434e No Look Ahead"). When non-zero cp writes **LatencyMode = 0**, `BufferFlags` = chosen flag word (+0x400000 "Skip Initial Samples"? / +0x800000 "Overload Detection"? checkboxes - labels vs bits unverified), and the explicit preset row values (copied from its own copy of the preset table at cp 0x5AD570) into ClassicFramesPerIrp(2), Out/InBufferOperationOffset, BulkPacketsPerIrp, OutBulkOperationOffset.
* `BasePeriod` (cp list {Auto, 1,2,3,4,5,6,7,8,10,12,16,20,24,32...}): overrides ClassicFramesPerIrp *and* ClassicFramesPerIrp2 (ms of iso buffering).
* `IsoOffset` ("ISO Extra buffer"; list Auto,0,1,2,3,4,6,8,12,16,24,32,48,64,96,128,192,256): written as `OutBufferOperationOffset = 0xE0000000 + N` (N microframes = 125 us each... if M=8).
* `BulkBuffer` ("BULK Extra buffer"; same list): `OutBulkOperationOffset = N`.
* `BufferFlags` bits seen consumed in the driver (devext+0x250): 0x80 (input classic-frame term), 0x100, 0x200 (completion event behaviours), 0x4000, 0x10 / 0x04 (ASIO outputReady behaviour), 0x100000, 0x400000, 0x800000 (affect iso/URB submission and start-up sample skipping). Exact semantics unresolved [L].
* Panel keys `PeriodFrames`, `DropoutDetection`, `LatencyMode`, `AsioDevice` are the normal ones. cp also reads `HKCU\...\Yamaha Steinberg USB Driver\env` (version/CopyrightYear) - cosmetic.

### 3.4 Live values on this machine (read-only query)

`BufferFlagsSetting 0, PeriodFrames 0x200, BasePeriod/IsoOffset/BulkBuffer 0xFFFFFFFF (Auto), BufferFlags 0, ClassicFramesPerIrp(2) 0, Out/InBufferOperationOffset 0, BulkPacketsPerIrp 0, OutBulkOperationOffset 0, LatencyMode 2, DropoutDetection 1, AsioDevice = ...#8&107d0b5&0&4#{677bb410-...}\asiocontrol`.
HKLM: `SOFTWARE\Yamaha\Yamaha Steinberg USB Driver Installer` (OemInfFileName0000=oem88.inf), `SOFTWARE\ASIO\Yamaha Steinberg USB ASIO` (CLSID above). Three Enum entries exist for VID_0499&PID_172F (two stale), service `ysusb_w10_64`, FriendlyName "Steinberg UR22C". This matches "Auto + Standard + 512".

## 4. Control panel (ysusb_cp.exe)

* Tabs/fields (UTF-16 strings): "Buffer Size:", "Input Latency:", "Output Latency:" (ms, from 0x222000 state), "Dropout Detection", "Mode:" (Low Latency / Standard / Stable), "Sample Rate:", "Clock Source:" (Internal / WCLK In / ADAT In), "ASIO Device", "Enable Power Management", "Enable Loopback / Mix:", "FirmVersion", **"USB Mode:" = SuperSpeed (USB 3.1 Gen 1) / High-Speed (USB 2.0)** (a bus-speed selector, not the latency profile) with the note "* Please quit the application using the audio device to change the USB Mode."; Advanced tab: "Flag Override", "Skip Initial Samples", "Overload Detection", "Other Options", "ISO Extra buffer", "BULK Extra buffer", "Base Period", "USB Frame Continuous Sync"; error/diagnostics tab (USBTS, CTRL, ZLI, DPC/T, ORDY, SO/BS, ITD/OTD ...). Messages: "The USB bus does't have enough bandwidth. Change the USB mode or select lower sample rate.", "Audio interruption due to USB error is detected...", "Connection is established in USB2.0. The USB port or the USB cable might not support USB3.0."
* Apply (save routine at 0x406B70): `RegCreateKeyEx(HKCU\Software\Yamaha\Yamaha Steinberg USB Driver)`, writes the values above (REG_DWORD, `AsioDevice` REG_SZ). **No IOCTL 0x222004 is sent by cp** - settings are picked up only by the next ASIO `init` (host must reload the driver). The USB-mode change goes to the device/driver and needs no stream open. [H for registry-only apply; M for no live push]
* Session detection: the cp opens `\asiocontrol` (without the claim IOCTL) and polls state; the "quit the application" notice is presumably driven by the state block's open/claim status. Mechanism not decoded [L].
* Command line / single instance: no switch strings found (only MFC `ShowCmd` for shell open). Single-instance logic not found in strings (MFC app; `CreateMutexW`/`FindWindowW` are imported but the usage was not traced) [L].
* The ASIO `controlPanel()` simply launches `ysusb_cp.exe` (no arguments found).

## 5. USB / device interaction (from the kernel driver) [M overall; request values H, semantics M/L]

* The driver builds URBs itself (USBD_INTERFACE_VERSION_602/603, `USBD_CreateConfigurationRequestEx`, "Insufficient Memory to allocate Isoch URB", `drvBulkRecvAsync`). Audio data uses **isochronous** endpoints ("Classic" path:
  ClassicFramesPerIrp frames of 1 ms (full-speed framing) or ...Irp2 (high-speed microframe framing); MaxIrpNumber IRPs in flight) and a **bulk path** ("BulkPacketsPerIrp", "BulkRequestPeriod") selected per device/bus mode (devext+0x4e80: 1 = iso, 2 = bulk). The user's numbers fit the iso formula, so the UR22C is currently streaming over iso (HS framing, per the Classic2 term; whether the link is SuperSpeed or HS cannot be told statically). The latency state also keeps per-rate delay tables.
* Control transfers go through one helper (sys 0x14001322C: type, request, wValue, wIndex, length, timeout 1000/100/10 ms). Observed (bmRequestType / bRequest):
  * `0xA1/0x21` class-interface **UAC2 clock requests** (bRequest 1 = CUR, 2 = RANGE; wValue 0x0100 = CS_SAM_FREQ_CONTROL, 0x0200 = CS_CLOCK_VALID; wIndex = entity<<8 | interface): query/set sample rate, clock source selector (Internal/WCLK/ADAT), and mixer/feature units (volume/mute/loopback mix).
  * `0xA2/0x22` class-endpoint (bRequest 1 / 0x81): endpoint-level sampling-frequency/pitch control (UAC1-style fallback).
  * `0xC0` **vendor IN**: bRequest 2 (wValue = rate-related byte, e.g. [dev+0x12c/0x130], used around sample-rate 44.1 kHz = 0xAC44 check), 4 (len 1), 7 (len 3), 8 (len 1), 0xA (len 4, wValue variable), 0xC (len 0x48, wValue 0 or 0x8000: capability / firmware-version / state blob).
  * `0x40/0x41` **vendor OUT**: bRequest 5 (wValue = 16-bit), 9 (wValue = driver word at devext+0x850, probably bus-mode/format selector), 0xB (wValue = 0x36 + x or computed): mode/DSP/clock commands (USB speed switch, loopback). 
  * `0x02` standard endpoint recipient (bRequest 1 = CLEAR_FEATURE) for endpoint halt recovery.
* Clock/sync: standard UAC2 clock source; sample rate set via CUR on the clock source then confirm via valid-bit; async feedback is not handled by user code (kernel iso feedback handling not decoded) [L].
* Latency contributors inside the driver (what the "216/359" are made of): device table constant (24/23 samples at 44.1/48k), IRP scheduling buffer in whole ms (ClassicFramesPerIrp2 on the input side, OutBufferOperationOffset on the output side), optional hub offsets, optional "extra buffer" overrides.
* Loopback ("Loopback Stream/Voice/AUX1/AUX2" pins; ASIO "Streaming L/R" inputs 2/3) is a DSP-side mix in the interface controlled through the control requests above; it consumes no host buffering of its own (the same input latency applies).

## 6. Latency levers (ranked) for the router

Current: Standard, 512, 48 kHz = 728 in / 871 out samples (15.2 / 18.1 ms). Predictions use the formula in section 2 [M]; verify with `getLatencies` after each change.

1. **PeriodFrames** (dominant lever, 512 samples = 10.7 ms of the 15/18 ms). 256 -> in 168+256 = 424 (8.8 ms) / out 263+256 = 519 (10.8 ms); 128 -> 296 / 343 (6.2 / 7.1 ms); 64 -> 184 / 231; 32 -> 104 / 151. The panel stops at 32; the driver also accepts 24, 16, 12, 8, 4 by writing PeriodFrames directly (preset rows exist; unvalidated on real hardware, expect dropouts on Windows well before 32). Constraint: must equal a table value exactly; for rates >= 88.2k the effective buffer is multiplied (x2, x4).
2. **LatencyMode 1 (Low Latency)**: at 512 saves 48 in / 144 out vs Standard (m1 row: 168/215), and if the driver picks the "test" row (U1) saves 192 in / 192 out (24/167 + 512 = 536 in, 679 out = 11.2 / 14.1 ms). Risk: fewer queued IRPs -> more dropouts under DPC load or on bad USB hubs. Easy to try: set `LatencyMode=1`, restart the ASIO session.
3. **LatencyMode 0 + manual values** (needs `BufferFlags=0x14434F`, set `ClassicFramesPerIrp2` (input ms) and `OutBufferOperationOffset=0xB0000000|N` (N ms) yourself; validation limits ClassicFramesPerIrp <= 32, offsets <= 0x800, MaxIrpNumber 2..8). Lower bound is below the shipped presets: e.g. Classic2 = 1..2 and N = 2..3 at 256/512 would give 24+48..96 in and 23+96..144 out; **no safety net**, expect glitches when N or Classic2 are below the preset for that period. 
4. **Stable (mode 3)** is the opposite direction (+192 in / +336 out at 512) and only for diagnosing dropouts.
5. `DelayedOutputBufferSwitch`, `OutputFrameDelay`, `PreSendFrames`, `FirstPacketLatency`, `MaxIrpNumber`: not part of the computed latency number, but they change how many IRPs/frames are queued ahead; lowering `MaxIrpNumber` (min 2) and `PreSendFrames` may cut real (unreported) latency at the cost of margin [L, unverified]. `FirstPacketLatency` only affects start-up.
6. **BufferThreadPriority** already 30 of 31; the user-mode switch thread already runs under MMCSS "Pro Audio" at CRITICAL. Raising to 31 is allowed but yields no meaningful gain. Our router's own callback thread work must stay short because bufferSwitch is invoked with directProcess=TRUE on the driver's MMCSS thread.
7. USB mode (SuperSpeed vs High-Speed) in the cp: the link must be USB-stable; HS vs SS changes microframe framing (`M`); possibly the iso/bulk choice. Not a pure latency knob; test with the "bandwidth" warnings in mind.
8. A fixed 24/23-sample device-side delay (about 0.5 ms) cannot be removed. Higher sample rates raise the base but lower the ms values (e.g. 96 kHz: dev 36/35 + ms*96 -> same ms, bigger sample counts).

Recommended experiments in order (all reversible, HKCU):
1. `LatencyMode=1` (keep 512) -> read getLatencies; stress-test for dropouts.
2. `PeriodFrames=256` with LatencyMode 2, then 1.
3. If stable: `PeriodFrames=128`, LatencyMode 2/1. 
4. Only then LatencyMode 0 tinkering.
Always re-init the ASIO driver (unload/reload) after a registry change.

## 7. Uncertain / unverified list

* U1: In the preset resolver (sys 0x14000B6AC) `LatencyMode==1` selects row 0 + flags 0x1540EF when `devext[0x4ED0] != 0`. I mapped that field to the rate bitmask (state+0x114 = ASIO obj+0xCCC) which would be always non-zero, making "Low Latency" effectively the "TEST ONLY" row on all devices. That offset mapping is solid, but the intent and actual runtime behaviour (and its stability) are unverified - measure it.
* The latency formula's unobserved terms (`tbl+0xC/0x10/0x18`, hub offsets, the `ebx>1` condition = `devext+0x9E0`, `M = devext+0x40C`) were assumed 0/HS; the formula reproduces both observed numbers exactly at 48 kHz but other rates, USB modes and rows are predictions. Which table belongs to PID 172F (0x1400225D0 chosen by value match, not by tracing the PID lookup).
* Meaning of most BufferFlags bits, of FirstPacketLatency, OutputFrameDelay, PreSendFrames, DelayedOutputBufferSwitch, HubOffsets.
* Exact UR22C endpoint layout (iso vs bulk, feedback endpoint), what vendor requests 5/9/0xA/0xB/0xC and the 0xC0/ bRequest 2/4/7/8 do semantically; firmware/DSP command channel beyond these (the DSP mixer, "dspMixFx", is a separate app).
* Session-in-use detection and single-instance logic in ysusb_cp.exe; command-line options; role of IOCTLs 0x222024-0x222060.
* bulklib consumers (CLSID not extracted) and its function in the product.
* Whether changes can be applied live: statically no (config only sent in `init`).
* Not done: dynamic confirmation (not allowed), full decompilation (no Ghidra available).

## 8. What a full decompile would need

Ghidra (or IDA) with the three PDB-less binaries: ysusb_w10_64.sys (focus: IOCTL dispatcher 0x14000bd02+, latency calc 0x14000A8B4, preset resolver 0x14000B6AC, device capability init for 0x908/0x40C/0x9E0/0x4E80, iso URB builder, DPC completion), ysusb_asio64.dll (already mostly covered), and ysusb_cp.exe (MFC; only the save/load routines at 0x4063F0-0x4070FF matter).
