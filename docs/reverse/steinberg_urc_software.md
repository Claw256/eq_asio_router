# Steinberg UR-C software (dspMixFx UR-C 3.3.0) - static analysis for UR22C

> Purpose: how the dspMixFx UR-C mixer software talks to the UR22C and how it coexists with the ASIO driver the router hosts.
> Date: 2026-10-10. Method: read-only static analysis of the installed software (pefile/capstone, strings, registry reads); nothing executed.
> Back to the [documentation index](../README.md).

Scope: `C:\Program Files\Steinberg\UR-C\` (read-only, static). Purpose: interoperability and latency tuning for the Rust router that hosts "Yamaha Steinberg USB ASIO". Nothing was executed, loaded, flashed or written. No device I/O. Registry and AppData were only read.

## 0. Method, tools, hashes

- Tools: no Ghidra/IDA/rizin/radare2/dumpbin/llvm available. Used an isolated venv (`scratchpad\venv`, `pefile` + `capstone` only), `strings` (msys2), PowerShell (`Get-AuthenticodeSignature`, `Get-FileHash`, read-only registry provider). Scripts live in `scratchpad\tools`, run with `python -I`. No decompiler, so function roles are inferred from imports, strings, vtable use and short disassembly. A full decompile would need Ghidra or IDA with MSVC x64 C++ RTTI/vtable recovery (the mixer is MSVC C++, Qt 6.2.13, RTTI present) to walk the COM interface vtables between the exe and bulklib, plus a disassembly of the kernel driver for the wire protocol.
- Copies analysed under `scratchpad\re_urc\{bin,fw,drv,other}`.
- Install tree: 553 files. Own binaries: `dspMixFx_UR-C.exe` (1,770,496 B, v3.3.0.0, NOT signed), `MemAPI_UR22C/24C/44C/816C.dll` (352,256 B for 22C, v3.1.0.0, NOT signed), `fx\*_UR-C.dll` (14 VST3 effect modules, v3.3.0.0, NOT signed: ChannelStrip, Comp, Delay, Ducker, Gate, M.B.Comp, Pitch_Fix, RevHall/Plate/Room, 4x GA_Classics), `firmware\{UR22C,UR24C,UR44C,UR816C}_Firm.udf`, `font\mplus-1c-*.ttf`, `image\*.svg`, Qt 6.2.13 (Valid, signed by The Qt Company), `vc_redist.x64.exe` (14.34.31938, Valid, Microsoft), `d3dcompiler_47.dll` (Valid), `opengl32sw.dll`.
- SHA-256:
  - dspMixFx_UR-C.exe `2af798d18a8f8f34817b5287636a65ffba822791a9dd2fba10c789b8909ebed1` (PE timestamp 1750130865, ImageBase 0x140000000, entry RVA 0xE000C)
  - MemAPI_UR22C.dll `a7c984ac15f419b9e16af403063dd279980f71dc6d68086f9aacb1da5f82c796` (timestamp 1750130877, ImageBase 0x180000000)
  - UR22C_Firm.udf `276bf3436633d54412e201370a536b3a79fcb0ce2bfc8e00e8604527089f59ac` (744,632 B)
- The mixer needs the installed Yamaha Steinberg USB Driver (HKLM `Wow6432Node\Yamaha\Yamaha Steinberg USB Driver\env`: Version 2.1.9). Driver files were analysed only for the parts the mixer talks to, in `C:\Program Files (x86)\Yamaha\Yamaha Steinberg USB Driver\`: `ysusb_bulklib64.dll` (2.1.9, Yamaha-signed), `ysusb_asio64.dll` (2.1.8, Yamaha-signed), `ysusb_cp.exe` (2.1.9), `ysusb_w10_64.sys` (2.1.9.0, WHCP-signed, not disassembled), `ysusb_w10.inf`.

## 1. Inventory and architecture

### Layering (high confidence)

```
dspMixFx_UR-C.exe (Qt6 Widgets/QML + YQ* framework, Yamaha::IValue parameter tree)
   |  LoadLibraryA / GetProcAddress      -> MemAPI_UR22C.dll  (pure parameter-memory model, NO device I/O)
   |  CoCreateInstance (wrapper 0x140006EC0) -> ysusb_bulklib64.dll  ("USB Bulk Transfer Library", in the DRIVER package)
   |        SetupDi* + CreateFileW + DeviceIoControl
   v
ysusb_w10_64.sys  (function driver, device interface "asiocontrol")
   v
UR22C  USB VID_0499 PID_172F
```

- `dspMixFx_UR-C.exe` imports no setupapi/winusb/hid. It has no device-handle code. It imports `CoCreateInstance`, `LoadLibraryA/GetProcAddress`, `FindFirstFileA` (MemAPI/plugin discovery), `CreateEventA/OpenEventA/SetEvent/WaitForSingleObject`, `CreateThread/SuspendThread/TerminateThread`, `CreateMutexW`, `RegisterWindowMessageW/SendMessageTimeoutW/EnumWindows`. The latter are single-instance handling: mutex "dspMixFx UR-C Mutex", window message "dspMixFx UR-C Message"; `OpenProcess`+`ReadProcessMemory` at VA 0x140097040-0x14009707E sits in the same startup code and is most likely the second instance handing over its command line (unrelated to the device). RTTI shows observer classes: `IUsbBulkDevObserver`, `IUsbBulkObserver`, `IUsbBulkPortObserver`, `UsbBulkPortObserver`, `AsyncHandler`, `AsyncHandlerForEditor`, `MyEventDriver`, `CEventImp`, `CWin32EventImp`, `YQDevice`, `YQDeviceList`, `YQFirmwareUpdater`, `YQDeviceEventID`, `YQSystemEventID`, `StreamUsbEffectSlot/Window`, `UR24CMonitorModeArea`.
- The CoCreateInstance wrapper at VA 0x140006EC0 is called from 0x14000BBE6 and 0x14001A1E3 with the GUID at VA 0x1400EEC30: `{EC3FF018-843A-492D-BEEA-6C4E8C047F2E}`. The same 16 bytes also appear in `ysusb_bulklib64.dll` .rdata (file offset 0x1FF10). It is not registered under HKLM `Classes\CLSID` or `WOW6432Node\CLSID` (checked), so whether it is a CLSID or an IID, and how the class gets located, is unresolved (section 6).
- Observer/event names (from `ysusb_bulklib64.dll` debug strings) show the driver pushes state to the app: `AsioDevice`, `SampleRate`, `InChannels`/`OutChannels`, `InBytesPerSample`/`OutBytesPerSample`, `InLatencyOffset`/`OutLatencyOffset`, `AsioBufferPeriod`, each logged as "changed, %u to %u". The mixer also uses names `AsioAudioActive`, `AsioInputChannels`, `AsioOutputChannels`, `AsioSampleRate` (strings next to the HKLM "...USB Driver\env" path in the exe). The `env` key currently only holds Version/CopyrightYear because nothing is streaming, so these are probably transient values.

### MemAPI_UR22C.dll (high confidence)

PDB path embedded: `...\urc-axr-apps\Source\IO\MemAPI\XR_URC\win\x64\UR22C Release\MemAPI.pdb`. MSVC x64 DLL; imports only the CRT, `CoTaskMemAlloc/Free`, critical sections, `strtod/strtof/strtoul/pow`. No CreateFile/DeviceIoControl/file mapping/pipes. It is a table of device parameters (name, id, range, scaling), not a transport.

Exports (3):

| Ordinal | RVA | Name | Inferred signature / behaviour |
|---|---|---|---|
| 1 | 0x215D0 | `queryMemAPIGUID` | `bool(GUID* out)`: writes constant `{F629ABBB-F4DB-463A-A258-6D7DA57D83F1}`, returns true |
| 2 | 0x21560 | `queryMemAPI` | `bool(IMemAPI** out)`: `operator new(0x78)`, ctor at 0x18006A30, stores the interface pointer, calls vtable slot 3 (+0x18, init), returns true when init succeeds. The object holds the UR22C parameter table |
| 3 | 0x215E0 | `queryVenderProductID` | `bool(uint16* vid, uint16* pid, uint16* idx, int n)`: when n>=1 writes VID 0x0499, PID 0x172F, index 1 |

So the DLL tells the app which USB IDs it serves and exposes the UR22C parameter model. The other three MemAPI DLLs have the same exports; the exe picks the module by the connected PID.

Names embedded in MemAPI: `/jp.co.yamaha.ur22c.pid_sh`, `/jp.co.yamaha.ur22c.shmutx`, `jp.co.yamaha.ur22c.event`, `jp.co.yamaha.ur22c.async`, `jp.co.yamaha.ur22c.asyncevt`, `str_shardmmmutx_name`, the driver name "Yamaha Steinberg USB ASIO" (`str_driver_name`), "Steinberg UR22C", and `\Steinberg\UR-C\dspMixFx_UR-C.exe` (`str_editor_path`). MemAPI itself creates none of these objects; the exe consumes event names (`CreateEventA`/`OpenEventA` wrappers at 0x140018FC0 / 0x140019010, the latter with EVENT_ALL_ACCESS 0x1F0003). Whether the event/shared-memory objects are created by the driver stack or by the app was not proven.

### How the device is reached (high confidence for the path, medium for IOCTL meaning)

`ysusb_bulklib64.dll` (in-proc COM server; exports DllGetClassObject/DllCanUnloadNow/DllRegisterServer/DllUnregisterServer; description "USB Bulk Transfer Library"; class `CUsbBulk`, debug text "CUsbBulk::OpenDev() : completed hResult %08x"):

1. `SetupDiGetClassDevsW` (flags 0x12 = DIGCF_PRESENT|DIGCF_DEVICEINTERFACE) + `SetupDiEnumDeviceInterfaces` on interface class `{677BB410-D9A0-4D16-9AF2-8D2E23F247D6}` (file offset 0x1FBC0, .rdata VA 0x180020DC0).
2. `HKCU\SOFTWARE\Yamaha\Yamaha Steinberg USB Driver\AsioDevice` holds the chosen device path (bulklib and the ASIO DLL both reference it). On this PC: `\\?\usb#vid_0499&pid_172f#8&107d0b5&0&4#{677bb410-d9a0-4d16-9af2-8d2e23f247d6}\asiocontrol`. So the symbolic link is the USB device interface `{677bb410-...}` with reference string `\asiocontrol`.
3. `CreateFileW(path, GENERIC_READ|GENERIC_WRITE, FILE_SHARE_READ|FILE_SHARE_WRITE, NULL, OPEN_EXISTING, flags 0)` at VA 0x1800035C3 and 0x180003EF0 (non-overlapped). `ysusb_asio64.dll` opens the same interface with identical flags (`CreateFileA` at ~0x180005E4C, then `DeviceIoControl` 0x222000).
4. `DeviceIoControl` for everything. No WinUSB, no HID, no named pipe from the mixer. A second GUID `{DDA54A40-1E4C-11D1-A050-405705C10000}` (VA 0x180021008) is also enumerated around 0x180003E1E-0x180003EF0; purpose not confirmed.

### IOCTL table (CTL_CODE: DeviceType 0x22 FILE_DEVICE_UNKNOWN, Access ANY, Method BUFFERED; function = (code>>2)&0xFFF)

Mixer path (`ysusb_bulklib64.dll`, call-site VAs):

| Code | Func | Call site | Observed buffer shape | Inferred role (confidence) |
|---|---|---|---|---|
| 0x222000 | 0x800 | 0x18000348E, 0x180003604 | first call after open | open/handshake/version query (medium; also the first call in the ASIO DLL) |
| 0x222028 | 0x80A | 0x180005C1E | no input; 4-byte value via bytes-returned | start/arm or release of a session; E_INVALIDARG (0x80070057) if its flag arg is nonzero (low-medium) |
| 0x222044 | 0x811 | 0x180006DD3 | in = 0x28-byte struct {size 0x28, session id (obj+0xD4), 0x10000 buffer size, three pointers/handles}; overlapped-style 8th arg | post a 64 KiB read/notification request (low-medium) |
| 0x222048 | 0x812 | 0x18000643A | in = 4 bytes (session id) | register/unregister the session (low-medium) |
| 0x22204C | 0x813 | 0x180006E78, 0x180006FB9 | in = 8 bytes, out buffer = obj+0x198, size 0x10010 | fetch pending bulk data (parameter/meter/status packets) (medium) |
| 0x222050 | 0x814 | 0x1800068CB, 0x18000720D | in = 12 bytes {session id, a, b}, out 4 bytes | write a parameter/command, result code returned (low-medium) |

ASIO DLL (`ysusb_asio64.dll`, for context): 0x222000, 0x222004, 0x222010, 0x222014, 0x222018, 0x222020, 0x222034, 0x222038, 0x22203C, 0x222040, 0x222054 (functions 0x800, 0x801, 0x804, 0x805, 0x806, 0x808, 0x80D, 0x80E, 0x80F, 0x810, 0x815). The call at ~0x180002890 (IOCTL 0x222010) sits next to rate constants 0x5DC00 (384000), 0xAC440 (705600), 0xBB800 (768000), consistent with a "set sample rate" request, unverified. Role names are mine; the kernel driver was not disassembled.

## 2. Device protocol sketch

Confidence: the USB-level framing is NOT known (it lives in `ysusb_w10_64.sys`, out of scope). Host-side model:

- Transport: the kernel driver exposes one control/bulk channel through `asiocontrol`. The mixer submits requests with the IOCTLs above (RTTI in bulklib: `BulkDump`, `BulkObserver`, `BulkPortObserver`, `BulkDevObserver`, `SendObserverThread`, `*NotificationBridge[ForEditor]`). `IPrmObserver/IPrmObserver2`, `IMeterObserver`, `IStatusObserver` (`TNotificationContainer<...>`) indicate three notification streams: parameters, meters, status.
- Notifications are push (event + pending-buffer fetch via 0x22204C), not blind polling. A waitable timer (`CreateWaitableTimerW`/`SetWaitableTimer`, VA 0x180002EA2-0x180002F43; due time 0xFFB3B4C0 = -5,000,000 x 100 ns = 0.5 s relative, period 0x1F4 = 500 ms) drives periodic service (likely meter/keep-alive).
- Threads: dedicated send/notification thread(s) in bulklib (`SendObserverThread`, `SetThreadPriority`, `PostThreadMessageW` into the app), the Qt main thread, plus `AsyncHandler` / `AsyncHandlerForEditor`.
- Parameter model (MemAPI + scene file): flat typed store addressed by 16-bit parameter IDs, each 1 or 2 bytes wide; layout descriptor chunk `F_<name>` then data chunk `D_<name>`. The scene XML's `CurrentData` base64 decodes to: chunk name `"F_CurrentScene"` (24-byte field), u32 0x27DC (layout id/hash), u32 0x574 (descriptor length), then groups `80 01 00 <count>` followed by 4-byte entries `00 <id> 01 <size>`; then chunk `"D_CurrentScene"` (u32 0x27DC, u32 0xE65 length) with raw values. `END_CHUNK` also appears in MemAPI. Error handling: HRESULT style (E_INVALIDARG in bulklib); the app shows "Insufficient DSP Resources." when an insert effect does not fit, and "InvalidSamplingRate" for FX that cannot run at the current rate.
- Parameter families (MemAPI camel-case; app side snake-case):
  - Input channel (`Ch*`, `inpt_*`, `in_*`): `HaInputGain`, `HaP48VOnf` (phantom 48 V; `in_ha_p48v_onf`, hardware switch state `inpt_hs_hp48v`), `in_ha_hize_onf` (Hi-Z), `in_pad_onf`, `ChHpfOnf/ChHpfFreq`, `ChPhaseOnf`, `ChPairOnf`, `ChFaderLinkOnf`, insert FX (`ChFxIns{Onf,Type,Patch,Point}1/2`, `ChGtrFxIns{Pre,Post}Patch`), sends `ChSendMixBus{1..4}{Level,Onf,Pan,SoloOnf}`, `ChSendFxBus{Level,Onf}`.
  - DAW (USB playback) channel `Daw*`: `DawSendMixBus{1..4}{Level,Bal,Onf,SoloOnf}`, `DawSendFxBus*`, `DawMixBus{1..4}UsageState`.
  - WDM (Windows system sound) channel `Wdm*`: `WdmSendMixBus{1..4}...`, independent of DAW sends, so the ASIO stream and the WDM stream have separate mixer sends.
  - Master/mix buses: `MasterMixBus{1..4}{Level,Bal,Onf,UsageState}`, `mxbs_*`, `stbs_*` (streaming bus), `pb_*` (playback), `st_*`.
  - FX: Compressor (`FxComp*`), EQ (`FxEq*`), Gate, Ducker, Maximizer (3-band), Pitch Fix, Delay, REV-X Hall/Plate/Room (`FxRev*`), Morphing channel strip (`Morph*`), four guitar amp models (`GtrFx{Clean,Crunch,Drive,Lead}*`), and "Streaming" effects (`Stream*`, `FX_STREAMING_*`) applied on the USB/loopback/stream side.
  - Patch/routing: `PatchInputDawMix`, `PatchMasterMeter`, `PatchDawMeter`, `PatchFxSend`, `PatchAuxPhoneOutput`, `PatchMonoSw`, `PatchLoopbackMode`, `PatchRecnShareLoopOnf`, `ot_hp_ptch`, `ot_hp_a_ptch`, `ot_hp_b_ptch`, `out_line_ptch`, `out_main_ptch`, `out_analog_ptch`, `out_digital_ptch`, `mxbs_dawmtr_ptch`, `mxbs_mstrmtr_ptch`.
  - System: `SysFsCode`/`sys_fscd`/`sys_req_fscd`/`sys_ref_fscd` (sample-rate code), `sys_samplingrate`, `sys_adc_dac_fltr_mode`, `sys_dgtlmd*`, `sys_aesebu_wclksrcch`, `sys_req_wclk_src` (word clock; shared with bigger models, not UR22C), `sys_lpbk_onf` (loopback), `sys_hs_loopback`, `sys_streaming_onf`, `SysRecnShareMode`, `SysUsbSubMode`, `SysEnableSuperSpeed`, `SysUsbChOrder`, `SysDelayEdit`, `sys_dim_att`, `sys_link_dim`, `sys_mute_onf`, `sys_pnl_lck`, `SysSoloOnf` and `SysSolo*Backup`, `sys_peak_hold_onf`, `sys_meter_rng`, `sys_lst_scene_no`, `CurrentScene`.
  - Meters: `meter_inpt`, `meter_pb`, `meter_mxbs`, `meter_streamingbus`, `meter_otpt`, `meter_comp_gr`, etc.
- Sample-rate lists inside the mixer app: `Rate44100Hz`, `Rate48000Hz`, `Rate88200Hz`, `Rate96000Hz` (`NumberOfSamplingRates`, 4 entries). The driver control panel also lists 176.4 and 192 kHz and DSD. DSP effects therefore stop at 96 kHz.
- Not recovered: the USB-level opcodes and byte layout of parameter read/write, checksum, packet size. It travels as opaque buffers through 0x222050 / 0x22204C and is encoded in the kernel driver. A USBPcap capture or a disassembly of `ysusb_w10_64.sys` is needed.

## 3. Settings, persistence, coexistence

- Per-user file: `%APPDATA%\Yamaha\dspMixFx UR-C\ur22c_setting.xml` (7,317 B here). Root `<UR22CFile>`: `<Header>UR22C File</Header>`, `<Version>1.0.0</Version>`, `<SceneData/>` (stored scenes, empty here), `<CurrentScene>1</CurrentScene>`, `<CurrentData sz="5145" cvtSz="6860">` (base64 blob described above), `<UserSceneDirty>`, `<InputPortEnable>`, `<BusSelect>`. Path template in the exe: `\Yamaha\dspMixFx UR-C\%1_setting.xml`, `%1` = lowercase model. Scene import/export uses `*.%1` files; effect presets use `.vstpreset` (keys `ChannelStripPresetPath`, `GuitarEffect{Clean,Crunch,Drive,Lead}PresetPath`, `RevX*PresetPath`, `StreamingFx*PresetPath`, `DocumentsPath`). Effect window positions: `Channel%1Slot%2StreamingUsbEffectWindowPositionX/Y` (storage location not found; the exe references `com.yamaha.yqframework`).
- `%APPDATA%\Yamaha\DeviceCenter\settings.enc` (144 B, encrypted; HKCU `Yamaha\DeviceCenter\AESKey`) belongs to Yamaha Device Center, not dspMixFx; not inspected.
- HKCU `Software\Yamaha\Yamaha Steinberg USB Driver` is the driver's tuning store (section 5), edited by `ysusb_cp.exe`; dspMixFx only shows driver/firmware versions.
- Mixer state lives in the device DSP and is restored from the scene on connect. Closing the app does not stop the on-device mixer.
- ASIO vs WDM: the mixer does not need either audio path; it uses only the control interface (`asiocontrol`) and does not touch the ASIO DLL. The INF registers WDM wave/MIDI endpoints on the same driver.
- Coexistence while the router holds the ASIO driver: `ysusb_asio64.dll` and `ysusb_bulklib64.dll` both open the same `asiocontrol` interface with read/write access and `FILE_SHARE_READ|FILE_SHARE_WRITE`, non-overlapped. Nothing in the mixer takes an exclusive lock, and the driver design (mixer is told of `AsioDevice`/`AsioBufferPeriod`/`SampleRate` changes) assumes both run together. Conclusion: dspMixFx can run while a router holds the ASIO device (high confidence on the open flags, medium on kernel-side policy). Caveats: (a) do not change the sample rate from dspMixFx or Windows while the ASIO host streams; the driver owns the rate (`SysFsCode`) and the host must re-init; (b) the app allows one instance (mutex "dspMixFx UR-C Mutex"); (c) enabling stream-side FX adds DSP delay on the loopback/stream path only.

## 4. Firmware image UR22C_Firm.udf (static only)

- 744,632 bytes (0xB5CB8); not a PE, so no Authenticode result. The UR44C image has the identical size, and the file contains UR44C and UR22C port-name strings ("UR44C Streaming L", "Steinberg UR44C", "UR22C Music L/R", "DAW Mix1 L", "DIRECT OUT n", "Yamaha Corporation"), so it carries tables for several models.
- Container: ASCII magic `MMPFUPDD` + `V1.00` (16-byte field). LE words: 0x10 = 4, 0x14 = 0xE4, 0x18 = 0x9000, product name `UR-C` at 0x1C, block name `MAIN` at 0x2C, version string `V3.20` at 0x5C, a count of 9 and a table of 16-byte entries (ids 0x50..0x58, 0x012C, 0x012C, size 0x1000, one checksum byte), i.e. nine 4 KiB flash pages (total 0x9000). A Cortex-M vector table follows at 0xF4: initial SP 0x200065C0, reset vector 0x0000021D (Thumb). Three more block headers repeat the layout at 0x9100 (sub-version "00.33", `BOOT`, build string "May 19 2023"), 0xA164 (`MAIN`, ids from 0x70) and 0xB4C58 (`MAIN`, sub-version "03.20", build string "Sep  5 2024", ids from 0x7E01). The last 4 bytes `52 D3 31 1C` look like a CRC/checksum; padding is 0xFF.
- Firmware version in the image: V3.20 (UR24C image shows the same header with "UR24C"). The device's installed firmware version was not queried (needs device access).
- Update flow in the app (high level): `FirmwareUpdaterDialog` / `YQFirmwareUpdater` (`firmwareVersionCheck()`, `firmwareUpdate()`, `updateFirmwareVersion()`), file name pattern `%1_Firm.udf` in `<install>\firmware\`, messages "Connect only one device for firmware update.", "Only one device can be updated at a time.", "The latest firmware is already installed.", "Firmware update in progress." The app compares the device-reported version with the udf version and then sends the file over the same driver channel. Nothing further analysed, by design.

## 5. Latency-relevant controls and interaction with the ASIO driver

dspMixFx / MemAPI (device-side; they change what is heard and where audio is routed, not the ASIO buffer):

- Direct (zero-latency) monitoring is the on-device DSP mixer: input channels `Ch*` -> `ChSendMixBus1..4` -> `MasterMixBus1..4` -> `PatchAuxPhoneOutput` / `ot_hp_*` / line outs. No USB or ASIO buffer is in that path, so it is independent of the reported 728/871-sample latency. Monitor through the mixer and mute the DAW/router return to avoid doubled audio; DAW return level to the same buses is `DawSendMixBus*`.
- Loopback / streaming: `sys_lpbk_onf`, `PatchLoopbackMode`, `sys_hs_loopback`, `sys_streaming_onf`, `SysRecnShareMode` / `PatchRecnShareLoopOnf` (record-share: DAW playback mixed into the USB capture). Ports "Streaming L/R", "Voice L/R", "Music L/R" appear in the firmware strings. Stream-bus FX (`Stream*`) sit in this path and add DSP delay (amount not quantified).
- `SysDelayEdit` (likely a delay/compensation edit flag for the FX/loopback path; semantics unconfirmed).
- `SysFsCode`: sample rate set by the driver, the mixer follows. `sys_adc_dac_fltr_mode`: ADC/DAC digital-filter mode (could change converter group delay; not exposed in the UI strings found; unverified).
- `SysUsbSubMode`, `SysEnableSuperSpeed`, `SysUsbChOrder`: USB-side modes. "Monitor Mode" (`UR24CMonitorModeArea`) is UR24C only.
- dspMixFx has no ASIO buffer-size, latency or clock-source control. These live in the driver control panel `ysusb_cp.exe` (tabs ASIO / Advanced / About / Error Info; fields Buffer Size, Input Latency, Output Latency, Clock Source with Internal / WCLK In / ADAT In, rates 44.1-192 kHz, DSD).

Driver tunables, HKCU `Software\Yamaha\Yamaha Steinberg USB Driver` (values read on this PC; the ASIO DLL and control panel use them):

| Value | Current | Notes |
|---|---|---|
| PeriodFrames | 512 | ASIO buffer size in frames |
| LatencyMode | 2 | `ysusb_cp.exe` strings: "Low Latency", "Standard", "Stable" (plus "Auto", "No Look Ahead", "TEST ONLY"); mapping of 0/1/2 to names not verified (list order suggests 2 = Stable) |
| DropoutDetection | 1 | USB error detection ("Audio interruption due to USB error...") |
| BufferFlags / BufferFlagsSetting | 0 / 0 | |
| BasePeriod, IsoOffset, BulkBuffer | 0xFFFFFFFF | looks like "unset/auto" |
| ClassicFramesPerIrp(2), BulkPacketsPerIrp, InBufferOperationOffset, OutBufferOperationOffset, OutBulkOperationOffset | 0 | auto |
| AsioDevice | `\\?\usb#vid_0499&pid_172f#...#{677bb410-...}\asiocontrol` | selected control interface |

Other names the ASIO DLL / control panel know (not set now): `FirstPacketLatency`, `PreSendFrames`, `OutputFrameDelay`, `MaxIrpNumber`, `DelayedOutputBufferSwitch`, `OutHubOffset`, `InHubOffset`, `BufferThreadPriority`, `BulkRequestPeriod`. These feed the driver's reported input/output latency (the `InLatencyOffset`/`OutLatencyOffset` the bulklib tracks). The reported 728 / 871 samples at 512 equal 512 + 216 and 512 + 359; those 216 / 359 look like fixed offsets (converter + USB look-ahead) that depend on LatencyMode and the offset values, but the derivation was not verified. The tuning path is LatencyMode and PeriodFrames via `ysusb_cp.exe` with the ASIO stream stopped, not dspMixFx.

## 6. Uncertain / unverified

1. USB-level protocol (opcodes, parameter encoding, checksums): not recovered (kernel driver not analysed; needs Ghidra on `ysusb_w10_64.sys` or a USBPcap capture).
2. Exact meaning of the six bulklib IOCTLs (0x222000/28/44/48/4C/50) beyond buffer shapes and call sites.
3. What `{EC3FF018-843A-492D-BEEA-6C4E8C047F2E}` is (CLSID vs IID) and how `CoCreateInstance` finds `ysusb_bulklib64.dll` without a registry class (registration-free or another CLSID; app manifest not checked).
4. Who creates the `/jp.co.yamaha.ur22c.*` shared memory, events and mutex (driver or app) and their content.
5. Mapping of `LatencyMode` 0/1/2 to UI names; origin of the 216/359-sample offsets.
6. `SysDelayEdit`, `sys_pfm_mode_onf`, `sys_adc_dac_fltr_mode` semantics.
7. Installed device firmware version (needs the device; untouched).
8. Process note: one stray interactive Python (system interpreter, started by a mistaken empty heredoc in the analysis directory) ran briefly and was terminated; nothing was imported from or written to the data directory.
