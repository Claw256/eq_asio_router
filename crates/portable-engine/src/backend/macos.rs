//! Core Audio output backend (macOS) — [`MacSink`] implements the
//! [`OutputSink`](crate::seams::OutputSink) seam over a raw Core Audio IOProc
//! (`AudioDeviceCreateIOProcID` + `AudioDeviceStart`).
//!
//! Per A4/A8 (docs/plans/cross-platform.md): **all bindings are hand-written
//! `#[link]` extern FFI** — no `-sys` crates, no new dependencies. Linked
//! frameworks: `CoreAudio` (device/stream properties + IOProc), `AudioToolbox` and
//! `CoreMedia` (linkage kept per the A8 decision; they carry AudioConverter /
//! CMTime facilities reserved for Phase C work), `CoreFoundation` (the CFString
//! that carry device UIDs/names), plus `libSystem`'s `mach_absolute_time` for the
//! [`MachClock`] tick domain.
//!
//! Negotiation policy (A4 rationale — the engine's ring arithmetic needs ground
//! truth for rate × buffer):
//! 1. `kAudioDevicePropertyNominalSampleRate` is set to the requested rate after
//!    checking `kAudioDevicePropertyAvailableNominalSampleRates`;
//! 2. `kAudioDevicePropertyBufferFrameSize` is set to the requested size clamped
//!    to `kAudioDevicePropertyBufferFrameSizeRange` ([`clamp_buffer_frames`]);
//! 3. both are read back and must match what was set, otherwise negotiation
//!    fails ([`negotiation_step`], [`NegotiationState`]);
//! 4. every output stream is asked for a Float32 PCM physical/virtual format; any
//!    other format is [`SinkError::Negotiation`].
//!
//! Device removal / stream interruption: `kAudioDevicePropertyDeviceIsAlive` and
//! `kAudioHardwarePropertyDevices` property listeners (plain C function pointers,
//! no ObjC blocks) latch [`DeviceWatch::fault`]; the IOProc then renders silence
//! instead of delivering audio, and the next `stop`/`start` reports the failure
//! cleanly.
//!
//! The pure helpers ([`match_rank`], [`pick_output_index`], [`clamp_buffer_frames`],
//! [`rate_supported`], [`negotiation_step`], [`frames_to_ticks`], ...) are `pub` so
//! the hardware-free `tests/macos_*.rs` suites can exercise them on any machine
//! (they compile only for `target_os = "macos"` + `feature = "backend-macos"`).

#![allow(non_snake_case)] // Core Audio FFI mirrors the C naming (mSelector, AudioObjectGetPropertyData, ...)
#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::seams::{Clock, OutputCallback, OutputSink, SinkError, StreamRequest, StreamSpec};

// ------------------------------------------------------------------------------------
// Hand-written Core Audio FFI (A8: no -sys crates)
// ------------------------------------------------------------------------------------

/// `OSStatus` — 0 on success, otherwise a FourCC-ish error code.
pub type OSStatus = i32;
pub type AudioObjectID = u32;
pub type AudioDeviceID = u32;
/// Opaque handle returned by `AudioDeviceCreateIOProcID`.
pub type AudioDeviceIOProcID = *mut c_void;

/// The RT entry point Core Audio calls once per IO cycle (output and input).
pub type AudioDeviceIOProc = Option<
    unsafe extern "C" fn(
        inDevice: AudioDeviceID,
        inNow: *const c_void,           // AudioTimeStamp (unused: MachClock reads mach_absolute_time)
        inInputData: *const AudioBufferList,
        inInputTime: *const c_void,     // AudioTimeStamp
        outOutputData: *mut AudioBufferList,
        inOutputTime: *const c_void,    // AudioTimeStamp
        inClientData: *mut c_void,
    ) -> OSStatus,
>;

/// Property-listener callback (`AudioObjectAddPropertyListener`): C fn pointer,
/// no ObjC block — keeps the FFI plain and link-free at runtime.
pub type AudioObjectPropertyListenerProc = Option<
    unsafe extern "C" fn(
        inObjectID: AudioObjectID,
        inNumberAddresses: u32,
        inAddresses: *const AudioObjectPropertyAddress,
        inClientData: *mut c_void,
    ) -> OSStatus,
>;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioObjectPropertyAddress {
    pub mSelector: u32,
    pub mScope: u32,
    pub mElement: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioValueRange {
    pub mMinimum: f64,
    pub mMaximum: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioBuffer {
    pub mNumberChannels: u32,
    pub mDataByteSize: u32,
    pub mData: *mut c_void,
}

/// Variable-length in C (`mBuffers` is declared `[AudioBuffer; 1]` and
/// over-allocated); use [`abl_buffer`] for indexed access.
#[repr(C)]
pub struct AudioBufferList {
    pub mNumberBuffers: u32,
    pub mBuffers: [AudioBuffer; 1],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioStreamBasicDescription {
    pub mSampleRate: f64,
    pub mFormatID: u32,
    pub mFormatFlags: u32,
    pub mBytesPerPacket: u32,
    pub mFramesPerPacket: u32,
    pub mBytesPerFrame: u32,
    pub mChannelsPerFrame: u32,
    pub mBitsPerChannel: u32,
    pub mReserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MachTimebaseInfo {
    pub numer: u32,
    pub denom: u32,
}

/// FourCC constructor for the property selectors/scopes below.
pub const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes([s[0], s[1], s[2], s[3]])
}

// kAudioObject* / kAudioHardware* / kAudioDevice* / kAudioStream* property ids.
pub const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;
pub const K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = fourcc(b"glob");
pub const K_AUDIO_OBJECT_PROPERTY_SCOPE_INPUT: u32 = fourcc(b"inp ");
pub const K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT: u32 = fourcc(b"outp");
pub const K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN: u32 = 0;
pub const K_AUDIO_OBJECT_PROPERTY_NAME: u32 = fourcc(b"lnam");
pub const K_AUDIO_HARDWARE_PROPERTY_DEVICES: u32 = fourcc(b"dev#");
pub const K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE: u32 = fourcc(b"dOut");
pub const K_AUDIO_HARDWARE_PROPERTY_DEFAULT_INPUT_DEVICE: u32 = fourcc(b"dIn ");
pub const K_AUDIO_DEVICE_PROPERTY_DEVICE_UID: u32 = fourcc(b"uid ");
pub const K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE: u32 = fourcc(b"nsrt");
pub const K_AUDIO_DEVICE_PROPERTY_AVAILABLE_NOMINAL_SAMPLE_RATES: u32 = fourcc(b"nsr#");
pub const K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE: u32 = fourcc(b"fsiz");
pub const K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE_RANGE: u32 = fourcc(b"fsz#");
pub const K_AUDIO_DEVICE_PROPERTY_STREAM_CONFIGURATION: u32 = fourcc(b"slay");
pub const K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_ALIVE: u32 = fourcc(b"livl");
pub const K_AUDIO_DEVICE_PROPERTY_STREAMS: u32 = fourcc(b"stm#");
pub const K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT: u32 = fourcc(b"pft ");
pub const K_AUDIO_STREAM_PROPERTY_VIRTUAL_FORMAT: u32 = fourcc(b"sfmt");
pub const K_AUDIO_FORMAT_LINEAR_PCM: u32 = fourcc(b"lpcm");
pub const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;
pub const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;
pub const K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;
pub const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// Largest channel count the RT render path maps onto `&mut [f32]` slices without
/// heap allocation (stack array of planes).
pub const MAX_CHANNELS: usize = 8;

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    pub fn AudioObjectGetPropertyDataSize(
        inObjectID: AudioObjectID,
        inAddress: *const AudioObjectPropertyAddress,
        inQualifierDataSize: u32,
        inQualifierData: *const c_void,
        outDataSize: *mut u32,
    ) -> OSStatus;
    pub fn AudioObjectGetPropertyData(
        inObjectID: AudioObjectID,
        inAddress: *const AudioObjectPropertyAddress,
        inQualifierDataSize: u32,
        inQualifierData: *const c_void,
        ioDataSize: *mut u32,
        outData: *mut c_void,
    ) -> OSStatus;
    pub fn AudioObjectSetPropertyData(
        inObjectID: AudioObjectID,
        inAddress: *const AudioObjectPropertyAddress,
        inQualifierDataSize: u32,
        inQualifierData: *const c_void,
        inDataSize: u32,
        inData: *const c_void,
    ) -> OSStatus;
    pub fn AudioObjectHasProperty(
        inObjectID: AudioObjectID,
        inAddress: *const AudioObjectPropertyAddress,
    ) -> u8;
    pub fn AudioObjectAddPropertyListener(
        inObjectID: AudioObjectID,
        inAddress: *const AudioObjectPropertyAddress,
        inListener: AudioObjectPropertyListenerProc,
        inClientData: *mut c_void,
    ) -> OSStatus;
    pub fn AudioObjectRemovePropertyListener(
        inObjectID: AudioObjectID,
        inAddress: *const AudioObjectPropertyAddress,
        inListener: AudioObjectPropertyListenerProc,
        inClientData: *mut c_void,
    ) -> OSStatus;
    pub fn AudioDeviceCreateIOProcID(
        inDevice: AudioDeviceID,
        inProc: AudioDeviceIOProc,
        inClientData: *mut c_void,
        outDeviceIOProcID: *mut AudioDeviceIOProcID,
    ) -> OSStatus;
    pub fn AudioDeviceDestroyIOProcID(
        inDevice: AudioDeviceID,
        inDeviceIOProcID: AudioDeviceIOProcID,
    ) -> OSStatus;
    pub fn AudioDeviceStart(inDevice: AudioDeviceID, inDeviceIOProcID: AudioDeviceIOProcID) -> OSStatus;
    pub fn AudioDeviceStop(inDevice: AudioDeviceID, inDeviceIOProcID: AudioDeviceIOProcID) -> OSStatus;
}

// AudioToolbox / CoreMedia linkage per the A8 decision (see module doc). The
// blocks are intentionally declarations-free today; they keep the framework
// linkage auditable in one place for Phase C (AudioConverter / CMTime work).
#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {}
#[link(name = "CoreMedia", kind = "framework")]
extern "C" {}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub fn CFRelease(cf: *const c_void);
    pub fn CFStringGetCString(
        theString: *const c_void,
        buffer: *mut c_char,
        bufferSize: usize,
        encoding: u32,
    ) -> u8;
}

extern "C" {
    // libSystem (auto-linked on Darwin).
    pub fn mach_absolute_time() -> u64;
    pub fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

/// Human-readable OSStatus (FourCC when printable) for error messages.
pub fn status_text(status: OSStatus) -> String {
    let b = status.to_be_bytes();
    let printable = b.iter().all(|c| (0x20..=0x7E).contains(c));
    if printable {
        format!("{status} ('{}')", String::from_utf8_lossy(&b))
    } else {
        format!("{status}")
    }
}

// ------------------------------------------------------------------------------------
// Pure helpers (hardware-free; exercised by tests/macos_*.rs)
// ------------------------------------------------------------------------------------

/// How a device matched a selector (ordered by priority).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchRank {
    /// No match at all.
    None,
    /// Name contains the wanted substring (case-insensitive), e.g. "BlackHole 2ch"
    /// for the wanted name "BlackHole".
    NameContains,
    /// Name equals the wanted name (case-insensitive).
    NameExact,
    /// UID equals the wanted UID (exact, case-sensitive).
    UidExact,
}

/// Selector priority: exact UID > exact name > name contains (case-insensitive).
pub fn match_rank(uid: &str, name: &str, want_uid: Option<&str>, want_name: Option<&str>) -> MatchRank {
    if let Some(w) = want_uid {
        if w == uid {
            return MatchRank::UidExact;
        }
    }
    if let Some(w) = want_name {
        if name.eq_ignore_ascii_case(w) {
            return MatchRank::NameExact;
        }
        if !w.is_empty() && name.to_ascii_lowercase().contains(&w.to_ascii_lowercase()) {
            return MatchRank::NameContains;
        }
    }
    MatchRank::None
}

/// Enumerated Core Audio device (see [`discover_devices`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: AudioDeviceID,
    pub uid: String,
    pub name: String,
    pub input_channels: u32,
    pub output_channels: u32,
}

/// Pure device-selection helper: highest [`MatchRank`] wins; ties keep the first
/// device in enumeration order. Only devices passing `eligible` are considered.
fn pick_index<F>(devices: &[DeviceInfo], want_uid: Option<&str>, want_name: Option<&str>, eligible: F) -> Option<usize>
where
    F: Fn(&DeviceInfo) -> bool,
{
    let mut best: Option<(MatchRank, usize)> = None;
    for (i, d) in devices.iter().enumerate() {
        if !eligible(d) {
            continue;
        }
        let rank = match_rank(&d.uid, &d.name, want_uid, want_name);
        if rank == MatchRank::None {
            continue;
        }
        if best.map_or(true, |(r, _)| rank > r) {
            best = Some((rank, i));
        }
    }
    best.map(|(_, i)| i)
}

/// Output-device selection for [`MacSink`]: selector match (UID/name) over
/// output-capable devices, falling back to the system default output device.
pub fn pick_output_index(
    devices: &[DeviceInfo],
    want_uid: Option<&str>,
    want_name: Option<&str>,
    default_output_uid: Option<&str>,
) -> Option<usize> {
    let out = |d: &DeviceInfo| d.output_channels > 0;
    pick_index(devices, want_uid, want_name, out).or_else(|| {
        let uid = default_output_uid?;
        devices.iter().position(|d| d.uid == uid && out(d))
    })
}

/// Input-device selection for [`crate::backend::macos_tap::MacTap`] (A5 v1):
/// exact UID → exact name → case-insensitive name-contains "BlackHole" →
/// the system default input device, unless that is the system default *output*
/// device (never capture our own output). Pure: all ids are passed in.
pub fn pick_tap_index(
    devices: &[DeviceInfo],
    want_uid: Option<&str>,
    want_name: Option<&str>,
    default_input_uid: Option<&str>,
    default_output_uid: Option<&str>,
) -> Option<usize> {
    let input = |d: &DeviceInfo| d.input_channels > 0;
    if let Some(i) = pick_index(devices, want_uid, want_name, input) {
        return Some(i);
    }
    if want_name.map_or(true, |w| !w.eq_ignore_ascii_case("BlackHole")) {
        if let Some(i) = pick_index(devices, None, Some("BlackHole"), input) {
            return Some(i);
        }
    }
    let uid = default_input_uid?;
    // Never capture the device we are playing to (would feed the ring its own output).
    if Some(uid) == default_output_uid {
        return None;
    }
    devices.iter().position(|d| d.uid == uid && input(d))
}

/// Clamp the requested IO buffer size to the device's `kAudioBufferFrameSizeRange`.
/// Returns 0 for an unusable range (min > max) so callers can fail negotiation.
pub fn clamp_buffer_frames(requested: u32, range_min: u32, range_max: u32) -> u32 {
    if range_min > range_max {
        return 0;
    }
    requested.max(range_min).min(range_max)
}

/// Is `rate_hz` inside any of the device's `kAudioDevicePropertyAvailableNominalSampleRates`
/// ranges? An empty range list means the device publishes no restriction → accept.
pub fn rate_supported(rate_hz: u32, ranges: &[(f64, f64)]) -> bool {
    if ranges.is_empty() {
        return true;
    }
    let r = f64::from(rate_hz);
    ranges.iter().any(|(lo, hi)| r >= *lo && r <= *hi)
}

/// Negotiation state machine (pure). Mirrors the strict read-back policy of A4:
/// what the device granted must equal what we set, otherwise the stream shape is
/// not ground truth and negotiation fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NegotiationState {
    Init,
    RateApplied,
    BufferApplied,
    Confirmed,
    Failed,
}

/// Events consumed by [`negotiation_step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NegotiationEvent {
    RateOk,
    RateRejected,
    BufferOk,
    BufferRejected,
    ReadBackMatches,
    ReadBackMismatch,
}

/// Advance the negotiation state machine. Any unexpected event fails closed.
pub fn negotiation_step(state: NegotiationState, event: NegotiationEvent) -> NegotiationState {
    use NegotiationEvent as E;
    use NegotiationState as S;
    match (state, event) {
        (S::Init, E::RateOk) => S::RateApplied,
        (S::RateApplied, E::BufferOk) => S::BufferApplied,
        (S::BufferApplied, E::ReadBackMatches) => S::Confirmed,
        (S::Init, E::RateRejected)
        | (S::RateApplied, E::BufferRejected)
        | (S::BufferApplied, E::ReadBackMismatch)
        | (S::Init | S::RateApplied | S::BufferApplied, _) => S::Failed,
        (S::Confirmed | S::Failed, _) => S::Failed,
    }
}

/// IO scheduling math: duration of one IO period in seconds.
pub fn period_seconds(frames: u32, rate_hz: u32) -> f64 {
    if rate_hz == 0 {
        return 0.0;
    }
    f64::from(frames) / f64::from(rate_hz)
}

/// IO scheduling math: ticks ([`MachClock`] domain) spanned by one period.
pub fn frames_to_ticks(frames: u32, rate_hz: u32, ticks_per_second: i64) -> i64 {
    if rate_hz == 0 {
        return 0;
    }
    (i64::from(frames) * ticks_per_second) / i64::from(rate_hz)
}

/// IO scheduling math: ticks → seconds.
pub fn ticks_to_seconds(ticks: i64, ticks_per_second: i64) -> f64 {
    if ticks_per_second <= 0 {
        return 0.0;
    }
    ticks as f64 / ticks_per_second as f64
}

/// `mach_absolute_time` tick frequency from a `mach_timebase_info` ratio
/// (`ticks/s = 1e9 * denom / numer`). Pure so it is hardware-free testable.
pub fn ticks_per_second_from_timebase(numer: u32, denom: u32) -> i64 {
    if numer == 0 {
        return 0;
    }
    (1_000_000_000i128 * i128::from(denom) / i128::from(numer)) as i64
}

// ------------------------------------------------------------------------------------
// Mach clock (the tick domain `now_ticks` is reported in)
// ------------------------------------------------------------------------------------

/// [`Clock`] over `mach_absolute_time` (A3: "mach_absolute_time on macOS").
/// `now_ticks` passed to [`OutputCallback::render`] reads the same domain.
pub struct MachClock {
    tps: i64,
}

impl MachClock {
    pub fn new() -> MachClock {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        let ok = unsafe { mach_timebase_info(&mut info) } == 0;
        let tps = if ok { ticks_per_second_from_timebase(info.numer, info.denom) } else { 0 };
        MachClock { tps: tps.max(1) }
    }
}

impl Default for MachClock {
    fn default() -> Self {
        Self::new()
    }
}

/// `mach_absolute_time` ticks per second (shared by the tap's arrival
/// timestamps — same tick domain as [`MachClock`]).
pub fn mach_ticks_per_second() -> i64 {
    let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
    let ok = unsafe { mach_timebase_info(&mut info) } == 0;
    if ok {
        ticks_per_second_from_timebase(info.numer, info.denom).max(1)
    } else {
        1
    }
}

impl Clock for MachClock {
    fn now_ticks(&self) -> i64 {
        unsafe { mach_absolute_time() as i64 }
    }
    fn ticks_per_second(&self) -> i64 {
        self.tps
    }
    fn sleep_until(&self, ticks: i64) {
        let dt = ticks - self.now_ticks();
        if dt > 0 {
            thread::sleep(Duration::from_secs_f64(ticks_to_seconds(dt, self.tps)));
        }
    }
}

// ------------------------------------------------------------------------------------
// Device discovery / property helpers (shared with macos_tap)
// ------------------------------------------------------------------------------------

fn status_err(context: &str, status: OSStatus) -> String {
    format!("{context}: OSStatus {}", status_text(status))
}

fn addr(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    }
}

fn get_prop_size(id: AudioObjectID, a: &AudioObjectPropertyAddress) -> Result<u32, String> {
    let mut size: u32 = 0;
    let s = unsafe { AudioObjectGetPropertyDataSize(id, a, 0, std::ptr::null(), &mut size) };
    if s != 0 {
        return Err(status_err("AudioObjectGetPropertyDataSize", s));
    }
    Ok(size)
}

fn get_prop_bytes(id: AudioObjectID, a: &AudioObjectPropertyAddress, out: &mut [u8]) -> Result<(), String> {
    let mut size = out.len() as u32;
    let s = unsafe {
        AudioObjectGetPropertyData(id, a, 0, std::ptr::null(), &mut size, out.as_mut_ptr() as *mut c_void)
    };
    if s != 0 {
        return Err(status_err("AudioObjectGetPropertyData", s));
    }
    Ok(())
}

fn set_prop_bytes(id: AudioObjectID, a: &AudioObjectPropertyAddress, data: &[u8]) -> Result<(), String> {
    let s = unsafe {
        AudioObjectSetPropertyData(id, a, 0, std::ptr::null(), data.len() as u32, data.as_ptr() as *const c_void)
    };
    if s != 0 {
        return Err(status_err("AudioObjectSetPropertyData", s));
    }
    Ok(())
}

fn get_u32(id: AudioObjectID, selector: u32) -> Result<u32, String> {
    let mut v: u32 = 0;
    get_prop_bytes(id, &addr(selector, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL), unsafe {
        std::slice::from_raw_parts_mut((&mut v as *mut u32) as *mut u8, 4)
    })?;
    Ok(v)
}

fn set_u32(id: AudioObjectID, selector: u32, v: u32) -> Result<(), String> {
    set_prop_bytes(id, &addr(selector, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL), &v.to_ne_bytes())
}

fn get_f64(id: AudioObjectID, selector: u32) -> Result<f64, String> {
    let mut v: f64 = 0.0;
    get_prop_bytes(id, &addr(selector, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL), unsafe {
        std::slice::from_raw_parts_mut((&mut v as *mut f64) as *mut u8, 8)
    })?;
    Ok(v)
}

fn set_f64(id: AudioObjectID, selector: u32, v: f64) -> Result<(), String> {
    set_prop_bytes(id, &addr(selector, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL), &v.to_ne_bytes())
}

/// Read a CFString-typed property (UID, name) and release the returned object.
fn get_cfstring(id: AudioObjectID, selector: u32) -> Result<String, String> {
    let mut cf: *const c_void = std::ptr::null();
    get_prop_bytes(id, &addr(selector, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL), unsafe {
        std::slice::from_raw_parts_mut((&mut cf as *mut *const c_void) as *mut u8, std::mem::size_of::<*const c_void>())
    })?;
    if cf.is_null() {
        return Err(format!("CFString property {selector:#x} is null"));
    }
    let mut buf = [0i8; 512];
    let ok = unsafe {
        CFStringGetCString(cf, buf.as_mut_ptr() as *mut c_char, buf.len(), K_CF_STRING_ENCODING_UTF8)
    };
    unsafe { CFRelease(cf) };
    if ok == 0 {
        return Err(format!("CFStringGetCString failed for property {selector:#x}"));
    }
    let bytes: Vec<u8> = buf.iter().map(|c| *c as u8).take_while(|b| *b != 0).collect();
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn get_id_array(id: AudioObjectID, selector: u32, scope: u32) -> Result<Vec<AudioDeviceID>, String> {
    let a = addr(selector, scope);
    let size = get_prop_size(id, &a)?;
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut raw = vec![0u8; size as usize];
    get_prop_bytes(id, &a, &mut raw)?;
    let n = (size as usize) / std::mem::size_of::<AudioDeviceID>();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut id_buf = [0u8; 4];
        id_buf.copy_from_slice(&raw[i * 4..i * 4 + 4]);
        out.push(AudioDeviceID::from_ne_bytes(id_buf));
    }
    Ok(out)
}

fn get_ranges(id: AudioObjectID, selector: u32) -> Result<Vec<(f64, f64)>, String> {
    let a = addr(selector, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL);
    let size = get_prop_size(id, &a)?;
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut raw = vec![0u8; size as usize];
    get_prop_bytes(id, &a, &mut raw)?;
    let n = (size as usize) / std::mem::size_of::<AudioValueRange>();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut lo = [0u8; 8];
        let mut hi = [0u8; 8];
        lo.copy_from_slice(&raw[i * 16..i * 16 + 8]);
        hi.copy_from_slice(&raw[i * 16 + 8..i * 16 + 16]);
        out.push((f64::from_ne_bytes(lo), f64::from_ne_bytes(hi)));
    }
    Ok(out)
}

/// Read an `AudioBufferList`-typed property (stream configuration) and return a
/// copy of its buffers (only channel counts are meaningful: `mData` is not valid
/// outside the property read).
fn get_abl_buffers(id: AudioObjectID, selector: u32, scope: u32) -> Result<Vec<AudioBuffer>, String> {
    let a = addr(selector, scope);
    let size = get_prop_size(id, &a)?;
    if (size as usize) < std::mem::size_of::<u32>() {
        return Ok(Vec::new());
    }
    let mut raw = vec![0u8; size as usize];
    get_prop_bytes(id, &a, &mut raw)?;
    let count = u32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
    let mut out = Vec::with_capacity(count);
    let abl_hdr = std::mem::size_of::<u32>(); // mNumberBuffers
    for i in 0..count {
        let off = abl_hdr + i * std::mem::size_of::<AudioBuffer>();
        if off + std::mem::size_of::<AudioBuffer>() > raw.len() {
            break;
        }
        let mut ch = [0u8; 4];
        let mut sz = [0u8; 4];
        let mut ptr = [0u8; 8];
        ch.copy_from_slice(&raw[off..off + 4]);
        sz.copy_from_slice(&raw[off + 4..off + 8]);
        ptr.copy_from_slice(&raw[off + 8..off + 16]);
        out.push(AudioBuffer {
            mNumberChannels: u32::from_ne_bytes(ch),
            mDataByteSize: u32::from_ne_bytes(sz),
            mData: usize::from_ne_bytes(ptr) as *mut c_void,
        });
    }
    Ok(out)
}

/// Total channels in a stream-configuration scope (sum of the ABL buffers).
pub fn scope_channels(id: AudioDeviceID, scope: u32) -> Result<u32, String> {
    let bufs = get_abl_buffers(id, K_AUDIO_DEVICE_PROPERTY_STREAM_CONFIGURATION, scope)?;
    Ok(bufs.iter().map(|b| b.mNumberChannels).sum())
}

/// Enumerate all Core Audio devices with UID, name and channel counts.
pub fn discover_devices() -> Result<Vec<DeviceInfo>, String> {
    let ids = get_id_array(K_AUDIO_OBJECT_SYSTEM_OBJECT, K_AUDIO_HARDWARE_PROPERTY_DEVICES, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let uid = get_cfstring(id, K_AUDIO_DEVICE_PROPERTY_DEVICE_UID).unwrap_or_default();
        let name = get_cfstring(id, K_AUDIO_OBJECT_PROPERTY_NAME).unwrap_or_default();
        let input_channels = scope_channels(id, K_AUDIO_OBJECT_PROPERTY_SCOPE_INPUT).unwrap_or(0);
        let output_channels = scope_channels(id, K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT).unwrap_or(0);
        out.push(DeviceInfo { id, uid, name, input_channels, output_channels });
    }
    Ok(out)
}

/// The system default output (`output == true`) or input device id.
pub fn default_device_id(output: bool) -> Result<AudioDeviceID, String> {
    let selector = if output {
        K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE
    } else {
        K_AUDIO_HARDWARE_PROPERTY_DEFAULT_INPUT_DEVICE
    };
    get_u32(K_AUDIO_OBJECT_SYSTEM_OBJECT, selector)
}

/// Which device a sink/source should open (defaults to the system default output).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceSelector {
    /// Exact device UID (`kAudioDevicePropertyDeviceUID`), highest priority.
    pub uid: Option<String>,
    /// Device name; exact (case-insensitive) first, then substring.
    pub name: Option<String>,
}

impl DeviceSelector {
    /// System default output device.
    pub fn default_output() -> DeviceSelector {
        DeviceSelector::default()
    }
    pub fn by_uid(uid: &str) -> DeviceSelector {
        DeviceSelector { uid: Some(uid.to_string()), name: None }
    }
    pub fn by_name(name: &str) -> DeviceSelector {
        DeviceSelector { uid: None, name: Some(name.to_string()) }
    }
}

/// Output-stream ids of a device (`kAudioDevicePropertyStreams`, output scope).
pub fn output_streams(device: AudioDeviceID) -> Result<Vec<AudioDeviceID>, String> {
    get_id_array(device, K_AUDIO_DEVICE_PROPERTY_STREAMS, K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT)
}

/// Input-stream ids of a device (`kAudioDevicePropertyStreams`, input scope).
pub fn input_streams(device: AudioDeviceID) -> Result<Vec<AudioDeviceID>, String> {
    get_id_array(device, K_AUDIO_DEVICE_PROPERTY_STREAMS, K_AUDIO_OBJECT_PROPERTY_SCOPE_INPUT)
}

fn get_asbd(stream: AudioDeviceID, selector: u32) -> Result<AudioStreamBasicDescription, String> {
    let mut asbd = AudioStreamBasicDescription::default();
    let a = addr(selector, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL);
    get_prop_bytes(stream, &a, unsafe {
        std::slice::from_raw_parts_mut(
            (&mut asbd as *mut AudioStreamBasicDescription) as *mut u8,
            std::mem::size_of::<AudioStreamBasicDescription>(),
        )
    })?;
    Ok(asbd)
}

fn set_asbd(stream: AudioDeviceID, selector: u32, asbd: &AudioStreamBasicDescription) -> Result<(), String> {
    let a = addr(selector, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL);
    set_prop_bytes(stream, &a, unsafe {
        std::slice::from_raw_parts(
            (asbd as *const AudioStreamBasicDescription) as *const u8,
            std::mem::size_of::<AudioStreamBasicDescription>(),
        )
    })
}

fn is_f32_pcm(asbd: &AudioStreamBasicDescription) -> bool {
    asbd.mFormatID == K_AUDIO_FORMAT_LINEAR_PCM
        && (asbd.mFormatFlags & K_AUDIO_FORMAT_FLAG_IS_FLOAT) != 0
        && (asbd.mFormatFlags & K_AUDIO_FORMAT_FLAG_IS_PACKED) != 0
}

/// Ask every stream in `streams` for a Float32 PCM physical *and* virtual format
/// (keeping its rate/channels/interleave), then verify the read-back. Returns an
/// error listing the offending format when a stream cannot go f32.
pub fn configure_f32_streams(streams: &[AudioDeviceID]) -> Result<(), String> {
    for &stream in streams {
        for selector in [K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT, K_AUDIO_STREAM_PROPERTY_VIRTUAL_FORMAT] {
            let cur = get_asbd(stream, selector)?;
            if is_f32_pcm(&cur) {
                continue;
            }
            let want = AudioStreamBasicDescription {
                mSampleRate: cur.mSampleRate,
                mFormatID: K_AUDIO_FORMAT_LINEAR_PCM,
                mFormatFlags: K_AUDIO_FORMAT_FLAG_IS_FLOAT
                    | K_AUDIO_FORMAT_FLAG_IS_PACKED
                    | (cur.mFormatFlags & K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED),
                mBytesPerPacket: if (cur.mFormatFlags & K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED) != 0 {
                    4
                } else {
                    4 * cur.mChannelsPerFrame.max(1)
                },
                mFramesPerPacket: 1,
                mBytesPerFrame: if (cur.mFormatFlags & K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED) != 0 {
                    4
                } else {
                    4 * cur.mChannelsPerFrame.max(1)
                },
                mChannelsPerFrame: cur.mChannelsPerFrame,
                mBitsPerChannel: 32,
                mReserved: 0,
            };
            set_asbd(stream, selector, &want)?;
            let back = get_asbd(stream, selector)?;
            if !is_f32_pcm(&back) {
                return Err(format!(
                    "stream {stream} refused Float32 PCM (format id {:#x}, flags {:#x})",
                    back.mFormatID, back.mFormatFlags
                ));
            }
        }
    }
    Ok(())
}

/// Strict rate + buffer negotiation (policy in the module doc), threaded through
/// the pure [`negotiation_step`] state machine. Returns `(rate_hz, buffer_frames)`
/// as read back from the device. Used by both [`MacSink`] and `macos_tap::MacTap`.
pub fn negotiate_rate_buffer(
    device: AudioDeviceID,
    want_rate_hz: u32,
    want_buffer_frames: u32,
) -> Result<(u32, u32), String> {
    let mut state = NegotiationState::Init;

    // 1. Rate: only set when the device advertises it.
    let ranges = get_ranges(device, K_AUDIO_DEVICE_PROPERTY_AVAILABLE_NOMINAL_SAMPLE_RATES)?;
    if want_rate_hz == 0 || !rate_supported(want_rate_hz, &ranges) {
        state = negotiation_step(state, NegotiationEvent::RateRejected);
        debug_assert_eq!(state, NegotiationState::Failed);
        return Err(format!(
            "sample rate {want_rate_hz} Hz not in device ranges {ranges:?}"
        ));
    }
    set_f64(device, K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE, f64::from(want_rate_hz))?;
    state = negotiation_step(state, NegotiationEvent::RateOk);

    // 2. Buffer frames: clamp to the advertised range, then set.
    let buf_ranges = get_ranges(device, K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE_RANGE)?;
    let (lo, hi) = match buf_ranges.first() {
        Some((lo, hi)) => (*lo as u32, *hi as u32),
        None => (1, 4096),
    };
    let frames = clamp_buffer_frames(want_buffer_frames, lo, hi);
    if frames == 0 {
        state = negotiation_step(state, NegotiationEvent::BufferRejected);
        debug_assert_eq!(state, NegotiationState::Failed);
        return Err(format!("buffer frame size {want_buffer_frames} not in device range {lo}..={hi}"));
    }
    set_u32(device, K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE, frames)?;
    state = negotiation_step(state, NegotiationEvent::BufferOk);

    // 3. Strict read-back: the granted shape must equal what we set (A4 ground truth).
    let got_rate = get_f64(device, K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE)? as u32;
    let got_frames = get_u32(device, K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE)?;
    if got_rate != want_rate_hz || got_frames != frames {
        state = negotiation_step(state, NegotiationEvent::ReadBackMismatch);
        debug_assert_eq!(state, NegotiationState::Failed);
        return Err(format!(
            "device granted rate/buffer {got_rate}/{got_frames}, expected {want_rate_hz}/{frames}"
        ));
    }
    state = negotiation_step(state, NegotiationEvent::ReadBackMatches);
    debug_assert_eq!(state, NegotiationState::Confirmed);
    Ok((got_rate, got_frames))
}

// ------------------------------------------------------------------------------------
// Device-removal watch (property listeners, plain C fn pointers)
// ------------------------------------------------------------------------------------

/// Shared between the property listeners (any thread) and the IOProc / control
/// API: `fault` latches when the device dies or disappears.
#[derive(Debug)]
pub struct DeviceWatch {
    pub device: AudioDeviceID,
    pub fault: AtomicBool,
}

impl DeviceWatch {
    pub fn new(device: AudioDeviceID) -> DeviceWatch {
        DeviceWatch { device, fault: AtomicBool::new(false) }
    }
    pub fn faulted(&self) -> bool {
        self.fault.load(Ordering::Relaxed)
    }
}

fn device_ids_now() -> Result<Vec<AudioDeviceID>, String> {
    get_id_array(
        K_AUDIO_OBJECT_SYSTEM_OBJECT,
        K_AUDIO_HARDWARE_PROPERTY_DEVICES,
        K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
    )
}

/// Listener for `kAudioDevicePropertyDeviceIsAlive` on the opened device.
pub unsafe extern "C" fn device_is_alive_listener(
    device: AudioObjectID,
    _n: u32,
    _addrs: *const AudioObjectPropertyAddress,
    client: *mut c_void,
) -> OSStatus {
    if client.is_null() {
        return 0;
    }
    let watch = &*(client as *const DeviceWatch);
    let alive = get_u32(device, K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_ALIVE).unwrap_or(0);
    if alive == 0 {
        watch.fault.store(true, Ordering::Release);
    }
    0
}

/// Listener for `kAudioHardwarePropertyDevices` on the system object: latches the
/// fault when the watched device disappears from the device list (unplugged /
/// driver unloaded).
pub unsafe extern "C" fn devices_changed_listener(
    _object: AudioObjectID,
    _n: u32,
    _addrs: *const AudioObjectPropertyAddress,
    client: *mut c_void,
) -> OSStatus {
    if client.is_null() {
        return 0;
    }
    let watch = &*(client as *const DeviceWatch);
    let still_there = device_ids_now().map(|ids| ids.contains(&watch.device)).unwrap_or(false);
    if !still_there {
        watch.fault.store(true, Ordering::Release);
    }
    0
}

/// Install the removal listeners for `watch` (device alive + system device list).
pub fn add_removal_listeners(watch: &Arc<DeviceWatch>) -> Result<(), String> {
    let client = Arc::as_ptr(watch) as *mut c_void;
    let dev_addr = addr(K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_ALIVE, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL);
    let s1 = unsafe {
        AudioObjectAddPropertyListener(watch.device, &dev_addr, Some(device_is_alive_listener), client)
    };
    if s1 != 0 {
        return Err(status_err("AudioObjectAddPropertyListener(is_alive)", s1));
    }
    let sys_addr = addr(K_AUDIO_HARDWARE_PROPERTY_DEVICES, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL);
    let s2 = unsafe {
        AudioObjectAddPropertyListener(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &sys_addr,
            Some(devices_changed_listener),
            client,
        )
    };
    if s2 != 0 {
        unsafe {
            AudioObjectRemovePropertyListener(watch.device, &dev_addr, Some(device_is_alive_listener), client);
        }
        return Err(status_err("AudioObjectAddPropertyListener(devices)", s2));
    }
    Ok(())
}

/// Remove the listeners installed by [`add_removal_listeners`] (idempotent-ish:
/// ignores listener-removal failures on a dead device).
pub fn remove_removal_listeners(watch: &Arc<DeviceWatch>) {
    let client = Arc::as_ptr(watch) as *mut c_void;
    let dev_addr = addr(K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_ALIVE, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL);
    unsafe {
        AudioObjectRemovePropertyListener(watch.device, &dev_addr, Some(device_is_alive_listener), client);
    }
    let sys_addr = addr(K_AUDIO_HARDWARE_PROPERTY_DEVICES, K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL);
    unsafe {
        AudioObjectRemovePropertyListener(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &sys_addr,
            Some(devices_changed_listener),
            client,
        );
    }
}

// ------------------------------------------------------------------------------------
// AudioBufferList mapping helpers (RT side)
// ------------------------------------------------------------------------------------

/// `&AudioBuffer` of an `AudioBufferList` at `index` (the C struct is variable-length).
pub unsafe fn abl_buffer<'a>(abl: *const AudioBufferList, index: u32) -> &'a AudioBuffer {
    &*((*abl).mBuffers.as_ptr().add(index as usize))
}

/// `&mut AudioBuffer` of an `AudioBufferList` at `index`.
pub unsafe fn abl_buffer_mut<'a>(abl: *mut AudioBufferList, index: u32) -> &'a mut AudioBuffer {
    &mut *(((*abl).mBuffers.as_mut_ptr()).add(index as usize))
}

/// One f32 plane view of an AudioBuffer (`mData` as `&mut [f32]`).
pub unsafe fn buffer_plane_mut(buf: &AudioBuffer, frames: usize) -> &mut [f32] {
    std::slice::from_raw_parts_mut(buf.mData as *mut f32, frames)
}

/// One f32 plane view of an AudioBuffer (`mData` as `&[f32]`).
pub unsafe fn buffer_plane(buf: &AudioBuffer, frames: usize) -> &[f32] {
    std::slice::from_raw_parts(buf.mData as *const f32, frames)
}

// ------------------------------------------------------------------------------------
// Callback wrapper + RT context + IOProc
// ------------------------------------------------------------------------------------

/// The [`OutputSink::Callback`] type of [`MacSink`]: wraps any
/// [`OutputCallback`] (typically `pipeline::PipelineCallback`) behind a trait
/// object so the raw IOProc can drive it.
pub struct MacCallback {
    inner: Box<dyn OutputCallback>,
}

impl MacCallback {
    pub fn new<C: OutputCallback>(inner: C) -> MacCallback {
        MacCallback { inner: Box::new(inner) }
    }
}

impl OutputCallback for MacCallback {
    fn render(&mut self, out: &mut [&mut [f32]], now_ticks: i64) {
        self.inner.render(out, now_ticks)
    }
    fn on_xrun(&mut self) {
        self.inner.on_xrun()
    }
}

/// Heap context the output IOProc gets as `inClientData`. Allocation-free at RT:
/// everything is pre-sized at `start`.
pub struct IoContext {
    pub callback: Option<MacCallback>,
    pub channels: usize,
    pub frames: usize,
    /// Interleaved layout needs scratch planes (render then interleave).
    pub scratch: Box<[f32]>,
    pub watch: Arc<DeviceWatch>,
}

/// Zero every output buffer (device-removal path / layout anomaly).
pub unsafe fn silence_output(out: *mut AudioBufferList) {
    if out.is_null() {
        return;
    }
    let n = (*out).mNumberBuffers;
    for i in 0..n {
        let buf = abl_buffer_mut(out, i);
        if !buf.mData.is_null() {
            let bytes = buf.mDataByteSize as usize;
            std::ptr::write_bytes(buf.mData as *mut u8, 0, bytes);
        }
    }
}

unsafe extern "C" fn output_io_proc(
    _device: AudioDeviceID,
    _now: *const c_void,
    _in: *const AudioBufferList,
    _in_time: *const c_void,
    out: *mut AudioBufferList,
    _out_time: *const c_void,
    client: *mut c_void,
) -> OSStatus {
    if client.is_null() || out.is_null() {
        return 0;
    }
    let ctx = &mut *(client as *mut IoContext);
    let now_ticks = mach_absolute_time() as i64;

    if ctx.watch.faulted() {
        // Device gone: render silence, stop delivering to the engine.
        silence_output(out);
        return 0;
    }

    let ch = ctx.channels;
    let max_frames = ctx.frames;
    let buffers = (*out).mNumberBuffers;

    // Layout detection per cycle (cheap, branch-only): non-interleaved = one
    // single-channel buffer per channel; interleaved = one buffer with `ch`
    // channels. Anything else is an anomaly: silence + xrun.
    let mut non_interleaved = buffers as usize == ch && ch > 0;
    if non_interleaved {
        for i in 0..buffers {
            if abl_buffer(out, i).mNumberChannels != 1 {
                non_interleaved = false;
                break;
            }
        }
    }
    let interleaved = !non_interleaved && buffers == 1 && ch > 0 && abl_buffer(out, 0).mNumberChannels == ch as u32;
    if !non_interleaved && !interleaved {
        silence_output(out);
        if let Some(cb) = ctx.callback.as_mut() {
            cb.on_xrun();
        }
        return 0;
    }

    // Frames this cycle (clamped to what the scratch/planes were sized for).
    let frames = if non_interleaved {
        let mut f = max_frames;
        for i in 0..buffers {
            let b = abl_buffer(out, i);
            f = f.min((b.mDataByteSize as usize) / 4);
        }
        f
    } else {
        let b = abl_buffer(out, 0);
        ((b.mDataByteSize as usize) / (4 * ch)).min(max_frames)
    };

    let mut planes: [&mut [f32]; MAX_CHANNELS] = Default::default();
    if non_interleaved {
        for i in 0..ch.min(buffers as usize) {
            let b = abl_buffer_mut(out, i as u32);
            planes[i] = buffer_plane_mut(b, frames);
        }
        if let Some(cb) = ctx.callback.as_mut() {
            cb.render(&mut planes[..ch], now_ticks);
        }
    } else {
        // Render into pre-allocated scratch planes, then interleave into the buffer.
        let mut ptr = ctx.scratch.as_mut_ptr();
        for i in 0..ch {
            planes[i] = std::slice::from_raw_parts_mut(ptr, frames);
            ptr = ptr.add(frames);
        }
        if let Some(cb) = ctx.callback.as_mut() {
            cb.render(&mut planes[..ch], now_ticks);
        }
        let buf = abl_buffer_mut(out, 0);
        if !buf.mData.is_null() {
            let dst = std::slice::from_raw_parts_mut(buf.mData as *mut f32, frames * ch);
            for f in 0..frames {
                for c in 0..ch {
                    dst[f * ch + c] = planes[c][f];
                }
            }
        }
    }
    0
}

// ------------------------------------------------------------------------------------
// MacSink: OutputSink over Core Audio
// ------------------------------------------------------------------------------------

/// Core Audio master output ([`OutputSink`]). Open with [`MacSink::open`], then
/// `negotiate` → `start` → `stop`. A UR22C (class-compliant UAC2) or any other
/// Core Audio output device works through this path unchanged.
pub struct MacSink {
    pub device: AudioDeviceID,
    pub uid: String,
    pub name: String,
    pub out_channels: u32,
    pub spec: StreamSpec,
    pub configured: bool,
    pub proc_id: AudioDeviceIOProcID,
    pub ctx: Option<Box<IoContext>>,
    pub watch: Option<Arc<DeviceWatch>>,
}

impl MacSink {
    /// Open the output device selected by `selector` (empty selector = system
    /// default output device).
    pub fn open(selector: &DeviceSelector) -> Result<MacSink, SinkError> {
        let devices = discover_devices().map_err(SinkError::Unavailable)?;
        let default_uid = default_device_id(true)
            .ok()
            .and_then(|id| devices.iter().find(|d| d.id == id).map(|d| d.uid.clone()));
        let idx = pick_output_index(
            &devices,
            selector.uid.as_deref(),
            selector.name.as_deref(),
            default_uid.as_deref(),
        )
        .ok_or_else(|| SinkError::Unavailable(format!("no Core Audio output device matches {selector:?}")))?;
        let info = &devices[idx];
        Ok(MacSink {
            device: info.id,
            uid: info.uid.clone(),
            name: info.name.clone(),
            out_channels: info.output_channels,
            spec: StreamSpec::default(),
            configured: false,
            proc_id: std::ptr::null_mut(),
            ctx: None,
            watch: None,
        })
    }

    /// Is the output device still alive / present?
    pub fn faulted(&self) -> bool {
        self.watch.as_ref().map_or(false, |w| w.faulted())
    }

    fn teardown(&mut self) {
        if !self.proc_id.is_null() {
            unsafe {
                AudioDeviceDestroyIOProcID(self.device, self.proc_id);
            }
            self.proc_id = std::ptr::null_mut();
        }
        if let Some(w) = self.watch.take() {
            remove_removal_listeners(&w);
        }
        self.ctx = None;
    }
}

impl OutputSink for MacSink {
    type Callback = MacCallback;

    fn negotiate(&mut self, want: &StreamRequest) -> Result<StreamSpec, SinkError> {
        if !self.proc_id.is_null() {
            return Err(SinkError::Negotiation("stream is running; stop before renegotiating".into()));
        }
        if want.channels == 0 || usize::from(want.channels) > MAX_CHANNELS {
            return Err(SinkError::Negotiation(format!(
                "requested {} channels, supported range 1..={MAX_CHANNELS}",
                want.channels
            )));
        }
        if u64::from(want.channels) > u64::from(self.out_channels) {
            return Err(SinkError::Negotiation(format!(
                "device '{}' has {} output channels, {} requested",
                self.name, self.out_channels, want.channels
            )));
        }

        // Format first: the RT path only speaks f32 planes.
        let streams = output_streams(self.device).map_err(|e| SinkError::Negotiation(e))?;
        if streams.is_empty() {
            return Err(SinkError::Negotiation(format!("device '{}' reports no output streams", self.name)));
        }
        configure_f32_streams(&streams).map_err(SinkError::Negotiation)?;

        // Rate + buffer (strict read-back; see module doc).
        let (rate_hz, buffer_frames) = negotiate_rate_buffer(self.device, want.rate_hz, want.buffer_frames)
            .map_err(SinkError::Negotiation)?;

        self.spec = StreamSpec { rate_hz, buffer_frames, channels: want.channels };
        self.configured = true;
        Ok(self.spec)
    }

    fn start(&mut self, cb: Self::Callback) -> Result<(), SinkError> {
        if !self.proc_id.is_null() {
            return Err(SinkError::Start("stream already running".into()));
        }
        if !self.configured {
            return Err(SinkError::Start("negotiate() must succeed before start()".into()));
        }
        let watch = Arc::new(DeviceWatch::new(self.device));
        let ctx = Box::new(IoContext {
            callback: Some(cb),
            channels: usize::from(self.spec.channels),
            frames: self.spec.buffer_frames as usize,
            scratch: vec![0.0f32; usize::from(self.spec.channels) * self.spec.buffer_frames as usize]
                .into_boxed_slice(),
            watch: watch.clone(),
        });
        let client = &*ctx as *const IoContext as *mut c_void;
        self.ctx = Some(ctx);
        self.watch = Some(watch);

        let mut proc_id: AudioDeviceIOProcID = std::ptr::null_mut();
        let s = unsafe { AudioDeviceCreateIOProcID(self.device, Some(output_io_proc), client, &mut proc_id) };
        if s != 0 {
            self.teardown();
            return Err(SinkError::Start(status_err("AudioDeviceCreateIOProcID", s)));
        }
        self.proc_id = proc_id;

        if let Some(w) = &self.watch {
            if let Err(e) = add_removal_listeners(w) {
                self.teardown();
                return Err(SinkError::Start(e));
            }
        }
        let s = unsafe { AudioDeviceStart(self.device, self.proc_id) };
        if s != 0 {
            self.teardown();
            return Err(SinkError::Start(status_err("AudioDeviceStart", s)));
        }
        if self.faulted() {
            self.teardown();
            return Err(SinkError::Start(format!("device '{}' disappeared while starting", self.name)));
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        if self.proc_id.is_null() {
            return Ok(());
        }
        let s = unsafe { AudioDeviceStop(self.device, self.proc_id) };
        let faulted = self.faulted();
        self.teardown();
        if faulted {
            return Err(SinkError::Stop(format!(
                "device '{}' was removed or died during the stream",
                self.name
            )));
        }
        if s != 0 {
            return Err(SinkError::Stop(status_err("AudioDeviceStop", s)));
        }
        Ok(())
    }

    fn spec(&self) -> &StreamSpec {
        &self.spec
    }
}
