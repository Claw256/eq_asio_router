# Steinberg built-in ASIO Driver and the UR22C driver stack (static analysis)

> Purpose: what the Steinberg built-in ASIO driver is, how the UR22C driver stack is layered, and which parts matter for router latency.
> Date: 2026-10-10. Method: read-only static analysis of installed binaries (pefile/capstone, strings); nothing executed.
> Back to the [documentation index](../README.md).

Date: 2026-10-10. Machine: the user's own, Windows 11 26200. Read-only analysis.

## 0. Method, tools, hashes

Tools present: python (3.14 / miniconda), msys `strings`. Not present: Ghidra, rizin/r2, IDA, dumpbin, llvm-objdump. I built an isolated venv in the scratchpad (`re_asio\venv`, pefile + capstone only) and wrote small scripts in `re_asio\tools` (outside the sample folder, run with `python -I`). I did NOT run any binary, installer or DLL. Samples were copied to `...\scratchpad\re_asio\samples\`.

Not possible with these tools: full decompilation (only targeted capstone disassembly and import/string/GUID/vtable-offset scanning), and any dynamic confirmation (actual buffer sizes at runtime, thread timing).

| File | Size | Version | SHA-256 | Signature |
|---|---|---|---|---|
| `C:\Program Files\Steinberg\Asio\asiobtin.dll` (x64) | 620,032 | 1.0.9.26 (built 2024-12-03) | 6D57F699254F62FBA570D7D37CCE1C6F95AA52998BC8484AA0A980E22FDDD9DA | NotSigned |
| `...\Asio\asiobtinsetup.exe` (x64) | 896,512 | 1.0.9.15 | F8EC7237D0275B3FA429D6CF05395E8976B1D2F849B25229D17DFB7B858BE79D | NotSigned |
| `...\Asio\soxr.dll` | 307,712 | 0.1.3.63 | 2FADC0780C31F7AA5D6215E7D44E97560CD87BC0322C96CC3808203194CCF32B | NotSigned |
| `...\Asio\Help\asiobtin_{ENG,FRA,GER,ITA,JPN,SPA}.htm` | ~1-3 KB | | | n/a |
| `C:\Program Files (x86)\Yamaha\Yamaha Steinberg USB Driver\ysusb_asio64.dll` | 193,896 | 2.1.8 | D87FF6307D508F01A2F866CD92687EB93F01D829F66E0BA1AA673AEB36B5F852 | Valid, Yamaha Corporation |

Folder `Asio` has only these files (no 32-bit DLL, no MSI/cab). Other Yamaha files: `ysusb_asio32.dll` 2.1.8, `ysusb_bulklib32/64.dll` 2.1.9, `ysusb_cp.exe` 2.1.9, `ysusb_w10_64.sys` 2.1.9.0, `ysusb_w10.inf/.cat` (all validly signed).

## 1. What is asiobtinsetup.exe

- "bt in" = BuilT-IN (not Bluetooth). Product string: "Steinberg built-In ASIO Driver Setup", PDB path `...\asiobuiltin\setup\win\x64\Release\asiobtinsetup.pdb`.
- It is NOT the installer. It is the driver's setup/control-panel launcher. Evidence: it contains the driver's CLSID bytes {B979B4A6-691F-4490-9C62-7811B5A3AEF5} (once), imports CoInitialize/CoCreateInstance (so it instantiates the driver COM object, which the ASIO API lets it open the control panel on), has a string "%s dll could not be loaded." and ASIOBTIN, shows errors via MessageBoxA, and has no registry, file-copy, service, SCM or MSI/cab/inf APIs at all. Window title string: "Steinberg built-in ASIO Setup". The actual installer is an MSI-like package: Uninstall key `{DE26CB00-5D4F-40B4-BEF5-5FBFDC7DCA52}` "Steinberg built-in ASIO Driver 64bit" (1.0.9, InstallSource `C:\Users\Rexami\Downloads\Steinberg_built_in_ASIO_Driver\Steinberg built-in ASIO Driver 1.0.9\Additional Content\Installer\`), InstallLocation `C:\Program Files\Steinberg\Asio\`. Installer itself is not on disk in this scope.
- Manifest: asInvoker (no elevation). No embedded payloads (resources: icons, version, manifest only). No evidence of command-line switch parsing beyond the CRT (GetCommandLine only comes from the CRT).
- Registration is done by `asiobtin.dll`'s `DllRegisterServer`/`DllUnregisterServer` (strings "Register Server failed ! (%d)", `CLSID\%s`, `CLSID\%s\InprocServer32`, `SOFTWARE\ASIO\%s`, ThreadingModel). Registry as found:
  - `HKLM\SOFTWARE\ASIO\Steinberg built-in ASIO Driver`: CLSID {B979B4A6-691F-4490-9C62-7811B5A3AEF5}, Description "Steinberg built-in ASIO Driver".
  - `HKLM\SOFTWARE\Classes\CLSID\{B979B4A6-...}\InprocServer32` = `C:\Program Files\Steinberg\Asio\asiobtin.dll`, ThreadingModel = Apartment.
  - 64-bit only. There is no WOW6432Node ASIO entry and no WOW6432 CLSID, so 32-bit hosts cannot see this driver. `router-cli devices` lists it under ASIO drivers.
  - It also contains the string `SOFTWARE\ASIO\ASIO WASAPI Driver` (probably legacy name handling; that key does not exist here).

## 2. The Steinberg built-in ASIO Driver (asiobtin.dll)

Summary: a generic WASAPI shared-mode ASIO wrapper ("Generic Low Latency ASIO Driver"), 64-bit, with a soxr sample-rate converter. It sits ON TOP of the Windows audio engine.

Evidence (static):
- Exports: DllGetClassObject, DllCanUnloadNow, DllRegisterServer, DllUnregisterServer, DllMain. Imports: AVRT (AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, AvRevertMmThreadCharacteristics, AvQuerySystemResponsiveness), WINMM (timeBeginPeriod, timeGetTime), CreateThread/CreateEvent/WaitForSingleObject, ole32 CoCreateInstance. MMCSS task name string "Pro Audio". Thread/event name "MyAsioWinWasapiEvent".
- WASAPI/MMDevice GUIDs present in the binary: CLSID_MMDeviceEnumerator, IMMDeviceEnumerator, IMMNotificationClient, IAudioClient3, IAudioRenderClient, IAudioCaptureClient. No exclusive/DeviceTopology/KS/WaveRT/WDM-KS strings. No direct KS usage.
- Opens endpoints via MMDevice ids; reads `SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\{Render|Capture}\<id>\Properties` for names (friendly name keys {a45c254e-df1c-4efd-8020-67d146a850e0},2 and the {b3f8fa53-...} set). Loopback is supported: strings "[Loopback] %s", and the Initialize flags in the stream-open routine are only the AUDCLNT_STREAMFLAGS_LOOPBACK bit (0x20000) or 0.
- Stream open (disassembly around RVA 0x3240-0x3480): `IAudioClient::GetDevicePeriod` -> default period, falling back to 100000 (10 ms) default and 30000 (3 ms) min if the call returns nothing; `GetMixFormat`; `IAudioClient::Initialize` with ShareMode = 0 (AUDCLNT_SHAREMODE_SHARED) and hnsBufferDuration = the engine's period; then `GetStreamLatency`, `GetBufferSize`, `GetService` (render/capture client), `Reset`. No exclusive-mode calls found. No EVENTCALLBACK flag seen in the Initialize flags, so the pump appears to be polled (GetCurrentPadding is called heavily, plus Sleep/timeBeginPeriod, MMCSS "Pro Audio") rather than event-driven. (Medium confidence; I did not trace every path.)
- IAudioClient3 is used: `GetSharedModeEnginePeriod`/`GetCurrentSharedModeEnginePeriod` are called (vtable +0x90 / +0x98) to query min/default/max shared-engine periods in frames. This is the "low-latency shared mode" mechanism available since Windows 10. I did not find a clear InitializeSharedAudioStream call site in the main path (one `call [reg+0xa0]` byte pattern exists in the stream-open region around RVA 0x1944 but that is unrelated code), so whether it actually requests the small period is unconfirmed.
- ASIO getBufferSize implementation (RVA 0x1900): min = max = preferred = granularity = one value stored in the object. So there is a SINGLE fixed buffer size, derived from the engine period; the user can not choose a buffer size. No "buffer" option exists in the panel strings.
- Control panel (asiobtinsetup.exe / dll dialogs): lists Output Ports and Input Ports (checkbox per endpoint, from the endpoint list), "Device Name", "Audio Channel", "Sample Rate". Help text says deactivating unused inputs "may improve audio performance (lower latency)" and warns outputs must stay connected. Languages: EN/FR/DE/IT/JA/ES.
- Sample-rate conversion: loads `soxr.dll` from its own folder (functions soxr_create/process/...; error string "Cannot load soxr.dll! No SRC!"). So it runs ASIO at a rate you pick even if the endpoint mix format differs (adds latency/CPU).
- Persisted settings (HKCU): `HKCU\Software\ASIO\Steinberg built-in ASIO Driver` = SampleRate 44100 (0xAC44), Output = `{0.0.0.00000000}.{09697ca6-880e-4a18-8be7-fe57d2a8ceee}` (not a UR22C endpoint id and not in the present list; probably a stale device from earlier use), Input = "No Selection".

Can it target the UR22C? Yes, but only through its Windows WDM endpoints (Music/DAW/Voice playback, Input 1/2, Streaming, Voice capture), i.e. through the Windows audio engine (AudioDG) in shared mode. It cannot reach the Yamaha ASIO bulk/iso channel path.

Viable alternative low-latency path? Not for this goal. Shared mode through the audio engine has a floor of roughly one engine period (IAudioClient3 min is typically ~2-3 ms on a class-compliant/WDM driver, 10 ms default) on each of input and output, plus the endpoint driver's own WDM buffering, plus soxr if rates differ, and the stream is polled. The Yamaha ASIO driver talks directly to the USB device with `PeriodFrames` 512 here. The built-in driver would also bring mix-format constraints (the endpoints are 2-channel only, 48 kHz, 24-bit device format), no direct monitoring/DSP integration, and it presents the endpoints as separate stereo pairs, not a single full-duplex device. It is useful only as a fallback/diagnostic (e.g. to check whether the WDM endpoints deliver audio) or when the Yamaha ASIO driver is busy.

Coexistence with the Yamaha ASIO driver: the built-in driver does not touch the USB device; it uses the audio engine. Whether the Yamaha kernel driver keeps the WDM endpoints alive while its ASIO client is open is not determined statically (see section 3). In principle both could be open, but the same hardware clock is shared; with the Yamaha ASIO open the sample rate is owned by it and the WDM engine format may need to match (48 kHz device format). Treat coexistence as unverified.

## 3. UR22C system stack

- Device: Steinberg UR22C, `USB\VID_0499&PID_172F` (REV_0320), instance `USB\VID_0499&PID_172F\8&107D0B5&0&4`, "Yamaha Steinberg USB Audio", class MEDIA, USB composite-style compatible IDs (but driver binds the device node itself via ysusb, no USBCCGP split visible: single device node, and 6 audio endpoint children plus MIDI). Container {CDBF3BA2-A958-11F1-B640-D8B32F2DFF78}.
- Driver package: `oem88.inf` (original `ysusb_w10.inf`, section Ysusb.Dev.NTamd64), provider Yamaha Corporation, version 2.1.9.0 dated 2025-05-20, WHCP attested; installed 2026-06-09. Service `ysusb_w10_64`: kernel driver (Type 1), Start 3 (manual/demand), ImagePath in DriverStore `ysusb_w10.inf_amd64_efc739eb19c8486d\ysusb_w10_64.sys`, no group. Device stack: `\Driver\ksthunk` over `\Driver\ysusb_w10_64` over `\Driver\ACPI` over `\Driver\USBHUB3`. Lower filter/upper filters: none seen. Class-installer help: ks.inf, wdmaudio.inf are included (standard WDM audio/KS).
- Applications: "Yamaha Steinberg USB Driver" (uninstall {CFD13083-...} and WOW6432 yUninstall), "Steinberg UR-C Applications" (dspMixFx UR-C, in `C:\Program Files\Steinberg\UR-C`; MemAPI_UR22C.dll, firmware UR22C_Firm.udf). Extra extension DLLs in `Common Files\Steinberg\Shared Components`: `ysusb_aadm_extension.dll`, `ur-c_extension.dll`.
- ASIO driver: `HKLM\SOFTWARE\ASIO\Yamaha Steinberg USB ASIO` = CLSID {CB7F9FFD-A33B-48B2-8BC0-437D94F37142}. 64-bit: `...\Yamaha Steinberg USB Driver\ysusb_asio64.dll`; WOW6432Node: `ysusb_asio32.dll`; both ThreadingModel Apartment (and an odd obfuscated extra `InprocServer32` value, a Yamaha-specific blob; not interpreted).
- ASIO link to the kernel driver: HKCU value `AsioDevice` = `\\?\usb#vid_0499&pid_172f#8&107d0b5&0&4#{677bb410-d9a0-4d16-9af2-8d2e23f247d6}\asiocontrol`. So the ASIO DLL opens a custom device interface (GUID {677BB410-D9A0-4D16-9AF2-8D2E23F247D6}, reference string `asiocontrol`) exposed by ysusb_w10_64.sys. The ASIO DLL imports SetupAPI (device interface enumeration), AVRT (MMCSS) and WINMM only; no WASAPI. So ASIO and WDM are served by the same kernel driver (ysusb_w10_64) and the same USB isochronous streams; the ASIO path bypasses the Windows audio engine.
- WDM endpoints currently present (OK): Render: Music, DAW, Voice (each stereo); Capture: Input 1/2, Streaming, Voice. Device format of all endpoints (MMDevices `{f19f064d-...},0`): extensible, 2 ch, 48000 Hz, 24 bit; shared-mode engine format 32-bit float (shown as 32 bit/48 kHz). Capture "Streaming"/"Voice"/"Input 1/2" additionally have 16 kHz mono float formats listed (voice/AEC hint). All Render and Capture endpoints have DeviceState 1 (active). The Render "Music" endpoint has an FX key (SysFx/APO GUIDs {C18E2F7E-...}, {EACD2258-...}, {EC1CC9CE-...}); the other endpoints show none in my filtered view. No exclusive-mode-disable values were found in the properties I filtered ({b3f8fa53,3} / {b3f8fa53,4} absent), so Windows defaults (apps may take exclusive control, exclusive has priority) apply.
- Interface GUIDs: USB device `{a5dcbf10-6530-11d2-901f-00c04fb951ed}` (GUID_DEVINTERFACE_USB_DEVICE); custom `{677BB410-D9A0-4D16-9AF2-8D2E23F247D6}` (asiocontrol); port name GUID {30b346b1-5471-4493-879a-375762a718c9}. KS audio interfaces enumerate under the MMDevAPI endpoints.
- Does opening ASIO silence WDM? Not determinable statically (kernel driver is not analyzed here). The design (one driver, one USB device) normally means the device is clocked at one rate and the ASIO client has exclusive use of the streams used by ASIO; WDM endpoints usually remain present but may stall or get reconfigured. The project's own findings with the router are the better evidence.
- Stale devices: `Get-PnpDevice` lists about 50 ghost entries for the UR22C (non-present endpoints "Voice (2- Steinberg UR22C)" etc., older USB instances 6&6889988&0&2/3, 8&18264D93&0&16, A&308493AA&0&6, MIDI endpoints). Harmless but they indicate the interface has been plugged into many ports.
- Driver-level registry (HKCU\Software\Yamaha\Yamaha Steinberg USB Driver): `PeriodFrames` = 0x200 (512 frames), `LatencyMode` = 2, `DropoutDetection` = 1, `BufferFlags/BufferFlagsSetting/ClassicFramesPerIrp/ClassicFramesPerIrp2/BulkPacketsPerIrp/*BufferOperationOffset/OutBulkOperationOffset` = 0, `BasePeriod/IsoOffset/BulkBuffer` = 0xFFFFFFFF (auto/unset), plus `AsioDevice`. `ysusb_asio64.dll` also contains the names BufferThreadPriority, BulkRequestPeriod, DelayedOutputBufferSwitch, FirstPacketLatency, InHubOffset, OutHubOffset (hidden tunables read from that key; values not present now). These are the driver's own tuning knobs; meanings of the numeric values are not documented (inferred from names).
- UR22C needs the "Steinberg UR22C" firmware; dspMixFx handles device mixing/DSP (MemAPI_UR22C).

USB topology (PnP):
- Device -> `USB Root Hub (USB 3.0)` (`USB\ROOT_HUB30\7&2F44DE9B&0&0`) -> ASMedia USB 3.20 xHCI (VEN_1B21 DEV_2426, driver Microsoft inbox 10.0.26100.9444, 2026-09-03) -> PCIe downstream switch port -> upstream switch port -> AMD PCIe root port (VEN_1022 DEV_14DB). LocationInfo `Port_#0004.Hub_#0007`; ACPI path `...GPP8.U4UP.U4P2.UXHC.UHUB.UHS2`. No external hub between the interface and the root hub (it sits directly on the root-hub port 4 of the ASMedia controller). The ASMedia controller also hosts the USB4 host router (VEN_1B21 DEV_2425), i.e. it is the board's USB4/ASM4242 controller behind a PCIe switch.
- Other USB devices exist on other controllers (AMD xHCI, VR devices, Generic USB Hubs 0424:3803), not on the UR22C controller as far as visible.
- Power: `DEVPKEY_Device_PowerData` decodes to most recent state D0, capabilities 0x19 (D0, D3, wake-from-D0). RemovalPolicy 3 (surprise-removal OK). No `SelectiveSuspend`/`DeviceIdleEnabled`/`EnhancedPowerManagementEnabled` values were found in the device's Enum Device Parameters key. Active power plan: High performance; USB selective suspend setting: AC = Disabled, DC = Enabled.

## 4. Latency-relevant takeaways

Observations:
1. USB path is good: direct to a root hub on an xHCI, no hub, inbox Microsoft xHCI driver. Slight caveat: it is an ASMedia controller behind a PCIe switch and shares the USB4 router; if dropouts appear, test a port on a different controller (the AMD ones listed).
2. USB selective suspend is Disabled on AC (plan High performance); only Enabled on DC (battery), irrelevant on a desktop. Nothing in the device key enables idle suspend.
3. The Yamaha driver runs at `PeriodFrames` = 512 with `LatencyMode` = 2 and `DropoutDetection` = 1 (the HKCU key). Those are the real buffer controls, not Windows settings. 512 frames at 48 kHz is 10.7 ms per period; round trip will be about 2 periods plus USB frame/safety (not measured here).
4. WDM endpoints are 48 kHz/24-bit stereo; the shared-engine format is float 32. Keep the Windows default format at 48 kHz so a rate change does not force a re-clock when ASIO opens.
5. No exclusive-mode restrictions on the endpoints (defaults), so another application could grab an endpoint in exclusive mode and stall the device; Windows-audio apps using Music/DAW/Voice/Streaming endpoints while ASIO runs add load on the same USB stream.
6. Enhancements: only the Music render endpoint has an APO chain registered; for any endpoint also used by apps, disable "audio enhancements" to avoid extra processing.

Recommended read-only checks:
- Confirm current ASIO buffer/rate in the Yamaha control panel (`ysusb_cp.exe`) and compare with HKCU `PeriodFrames`.
- `powercfg /q SCHEME_CURRENT 2a737441-1930-4402-8d77-b2bebba308a3` (already done: AC disabled), `powercfg /requests`.
- `Get-PnpDeviceProperty` on the xHCI and root hub for any "allow computer to turn off this device" setting (Device Manager > Power Management tab); check the Yamaha device and Root Hub entries.
- LatencyMon/ETW DPC check for `ysusb_w10_64.sys`, `USBXHCI.sys`, NVIDIA and network DPCs.
- `Get-WinEvent` Kernel-PnP / USBHUB3 logs for resets on the device.

Safe tuning steps (not applied by me):
- Raise buffer in the Yamaha panel (or `PeriodFrames` to 512/1024 only through the control panel) if dropouts occur; reduce only after stable.
- Keep High performance (or at least disable USB selective suspend and "turn off this device to save power" for the Yamaha device and its Root Hub).
- Prefer a rear-panel root port on the same controller (not through a hub); avoid front-panel hubs.
- Disable hot ghost endpoints only by uninstalling stale devices via Device Manager "show hidden devices" if desired; harmless otherwise.
- Do not use the Steinberg built-in ASIO driver for the low-latency path; use it only as a fallback.
- Do not change the hidden tunables (BufferThreadPriority, BulkRequestPeriod, etc.) without testing; their semantics are inferred.

## 5. Uncertain / not verified

- Whether built-in driver actually calls InitializeSharedAudioStream (small period) or just default-period Initialize; whether pump is purely polled.
- Exact built-in-driver buffer size at runtime (depends on engine period of the chosen endpoint; likely 10 ms default, possibly lower via IAudioClient3).
- Behaviour of WDM endpoints while Yamaha ASIO is open, and whether built-in ASIO can run at the same time.
- Exact meaning of Yamaha registry values (inferred from names); the odd value under the Yamaha `InprocServer32` key.
- The setup EXE's exact role (launching the control panel) is inferred from imports/strings/CLSID, not from full decompilation; command-line switches not found.
- `Output` in HKCU built-in settings points to an endpoint not in the present list (likely stale).
- Kernel driver `ysusb_w10_64.sys` was not analyzed (out of scope).
