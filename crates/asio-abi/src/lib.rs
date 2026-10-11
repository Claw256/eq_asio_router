//! Hand-written ASIO ABI definitions (types and the `IASIO` vtable).
//!
//! Written from the public ASIO interface contract; no Steinberg SDK files are
//! copied. Layout notes:
//! - All ASIO structs use 4-byte packing on Windows (`#pragma pack(push, 4)`).
//! - On Windows the SDK has no native int64, so `ASIOSamples`/`ASIOTimeStamp`
//!   are `{hi, lo}` pairs of `u32`.
//! - `IASIO` methods (everything after the three `IUnknown` slots) use the C++
//!   member-function convention: `thiscall` on x86, the single C convention on
//!   x86_64. The `IUnknown` slots are `stdcall`/`system`.
//! - The callbacks passed to `createBuffers` are plain `cdecl`/`C` functions.

use core::ffi::{c_char, c_void};

pub type AsioBool = i32;
pub type AsioError = i32;
pub type AsioSampleRate = f64;
pub type AsioSampleType = i32;

pub const ASIO_FALSE: AsioBool = 0;
pub const ASIO_TRUE: AsioBool = 1;

// ASIOError codes
pub const ASE_OK: AsioError = 0;
pub const ASE_SUCCESS: AsioError = 0x3f48_47a0;
pub const ASE_NOT_PRESENT: AsioError = -1000;
pub const ASE_HW_MALFUNCTION: AsioError = -999;
pub const ASE_INVALID_PARAMETER: AsioError = -998;
pub const ASE_INVALID_MODE: AsioError = -997;
pub const ASE_SP_NOT_ADVANCING: AsioError = -996;
pub const ASE_NO_CLOCK: AsioError = -995;
pub const ASE_NO_MEMORY: AsioError = -994;

// ASIOSampleType values
pub const ASIOST_INT16_MSB: AsioSampleType = 0;
pub const ASIOST_INT24_MSB: AsioSampleType = 1;
pub const ASIOST_INT32_MSB: AsioSampleType = 2;
pub const ASIOST_FLOAT32_MSB: AsioSampleType = 3;
pub const ASIOST_FLOAT64_MSB: AsioSampleType = 4;
pub const ASIOST_INT32_MSB16: AsioSampleType = 8;
pub const ASIOST_INT32_MSB18: AsioSampleType = 9;
pub const ASIOST_INT32_MSB20: AsioSampleType = 10;
pub const ASIOST_INT32_MSB24: AsioSampleType = 11;
pub const ASIOST_INT16_LSB: AsioSampleType = 16;
pub const ASIOST_INT24_LSB: AsioSampleType = 17;
pub const ASIOST_INT32_LSB: AsioSampleType = 18;
pub const ASIOST_FLOAT32_LSB: AsioSampleType = 19;
pub const ASIOST_FLOAT64_LSB: AsioSampleType = 20;
pub const ASIOST_INT32_LSB16: AsioSampleType = 24;
pub const ASIOST_INT32_LSB18: AsioSampleType = 25;
pub const ASIOST_INT32_LSB20: AsioSampleType = 26;
pub const ASIOST_INT32_LSB24: AsioSampleType = 27;

// asioMessage selectors
pub const K_ASIO_SELECTOR_SUPPORTED: i32 = 1;
pub const K_ASIO_ENGINE_VERSION: i32 = 2;
pub const K_ASIO_RESET_REQUEST: i32 = 3;
pub const K_ASIO_BUFFER_SIZE_CHANGE: i32 = 4;
pub const K_ASIO_RESYNC_REQUEST: i32 = 5;
pub const K_ASIO_LATENCIES_CHANGED: i32 = 6;
pub const K_ASIO_SUPPORTS_TIME_INFO: i32 = 7;
pub const K_ASIO_SUPPORTS_TIME_CODE: i32 = 8;
pub const K_ASIO_MMC_COMMAND: i32 = 9;
pub const K_ASIO_SUPPORTS_INPUT_MONITOR: i32 = 10;
pub const K_ASIO_SUPPORTS_INPUT_GAIN: i32 = 11;
pub const K_ASIO_SUPPORTS_INPUT_METER: i32 = 12;
pub const K_ASIO_SUPPORTS_OUTPUT_GAIN: i32 = 13;
pub const K_ASIO_SUPPORTS_OUTPUT_METER: i32 = 14;
pub const K_ASIO_OVERLOAD: i32 = 15;

// future() selectors
pub const K_ASIO_ENABLE_TIME_CODE_READ: i32 = 1;
pub const K_ASIO_DISABLE_TIME_CODE_READ: i32 = 2;
pub const K_ASIO_SET_INPUT_MONITOR: i32 = 3;
pub const K_ASIO_TRANSPORT: i32 = 4;
pub const K_ASIO_SET_INPUT_GAIN: i32 = 5;
pub const K_ASIO_GET_INPUT_METER: i32 = 6;
pub const K_ASIO_SET_OUTPUT_GAIN: i32 = 7;
pub const K_ASIO_GET_OUTPUT_METER: i32 = 8;
pub const K_ASIO_CAN_INPUT_MONITOR: i32 = 9;
pub const K_ASIO_CAN_TIME_INFO: i32 = 10;
pub const K_ASIO_CAN_TIME_CODE: i32 = 11;
pub const K_ASIO_CAN_TRANSPORT: i32 = 12;
pub const K_ASIO_CAN_INPUT_GAIN: i32 = 13;
pub const K_ASIO_CAN_INPUT_METER: i32 = 14;
pub const K_ASIO_CAN_OUTPUT_GAIN: i32 = 15;
pub const K_ASIO_CAN_OUTPUT_METER: i32 = 16;
pub const K_ASIO_OPTIONAL_ONE: i32 = 0x2_0000;
pub const K_ASIO_SET_IO_FORMAT: i32 = 0x2_3111;
pub const K_ASIO_GET_IO_FORMAT: i32 = 0x2_3112;
pub const K_ASIO_CAN_DO_IO_FORMAT: i32 = 0x2_3113;
pub const K_ASIO_CAN_REPORT_OVERLOAD: i32 = 0x2_3114;
pub const K_ASIO_GET_INTERNAL_BUFFER_SAMPLES: i32 = 0x2_3115;

// ASIOTimeInfo flags
pub const K_SYSTEM_TIME_VALID: u32 = 1;
pub const K_SAMPLE_POSITION_VALID: u32 = 1 << 1;
pub const K_SAMPLE_RATE_VALID: u32 = 1 << 2;
pub const K_SPEED_VALID: u32 = 1 << 3;
pub const K_SAMPLE_RATE_CHANGED: u32 = 1 << 4;
pub const K_CLOCK_SOURCE_CHANGED: u32 = 1 << 5;

/// `ASIOSamples` / `ASIOTimeStamp` on Windows: `{ unsigned long hi, lo }`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct AsioInt64 {
    pub hi: u32,
    pub lo: u32,
}

impl AsioInt64 {
    pub fn from_u64(v: u64) -> Self {
        Self { hi: (v >> 32) as u32, lo: v as u32 }
    }
    pub fn to_u64(self) -> u64 {
        ((self.hi as u64) << 32) | self.lo as u64
    }
}

pub type AsioSamples = AsioInt64;
pub type AsioTimeStamp = AsioInt64;

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioClockSource {
    pub index: i32,
    pub associated_channel: i32,
    pub associated_group: i32,
    pub is_current_source: AsioBool,
    pub name: [c_char; 32],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioChannelInfo {
    pub channel: i32,
    pub is_input: AsioBool,
    pub is_active: AsioBool,
    pub channel_group: i32,
    pub sample_type: AsioSampleType,
    pub name: [c_char; 32],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioBufferInfo {
    pub is_input: AsioBool,
    pub channel_num: i32,
    pub buffers: [*mut c_void; 2],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTimeInfo {
    pub speed: f64,
    pub system_time: AsioTimeStamp,
    pub sample_position: AsioSamples,
    pub sample_rate: AsioSampleRate,
    pub flags: u32,
    pub reserved: [u8; 12],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTimeCode {
    pub speed: f64,
    pub time_code_samples: AsioSamples,
    pub flags: u32,
    pub future: [u8; 64],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTime {
    pub reserved: [i32; 4],
    pub time_info: AsioTimeInfo,
    pub time_code: AsioTimeCode,
}

impl AsioTime {
    pub fn zeroed() -> Self {
        // SAFETY: plain-old-data, all-zero is a valid value.
        unsafe { core::mem::zeroed() }
    }
}

/// `ASIOCallbacks`: four `cdecl` function pointers supplied by the host.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioCallbacks {
    pub buffer_switch: unsafe extern "C" fn(index: i32, direct_process: AsioBool),
    pub sample_rate_did_change: unsafe extern "C" fn(rate: AsioSampleRate),
    pub asio_message:
        unsafe extern "C" fn(selector: i32, value: i32, message: *mut c_void, opt: *mut f64) -> i32,
    pub buffer_switch_time_info:
        unsafe extern "C" fn(time: *mut AsioTime, index: i32, direct_process: AsioBool) -> *mut AsioTime,
}

/// Function-pointer type for an `IASIO` method (after the `IUnknown` slots).
///
/// Usage: `asio_fn!(*mut IAsio, i32; AsioError)` = `fn(this, i32) -> AsioError`.
#[cfg(target_arch = "x86")]
#[macro_export]
macro_rules! asio_fn {
    ($($arg:ty),* ; $ret:ty) => { unsafe extern "thiscall" fn($($arg),*) -> $ret };
}
#[cfg(not(target_arch = "x86"))]
#[macro_export]
macro_rules! asio_fn {
    ($($arg:ty),* ; $ret:ty) => { unsafe extern "system" fn($($arg),*) -> $ret };
}

/// A COM-style pointer to an `IASIO` object: first field is the vtable.
#[repr(C)]
pub struct IAsio {
    pub vtbl: *const IAsioVtbl,
}

#[repr(C)]
pub struct IAsioVtbl {
    // IUnknown (stdcall)
    pub query_interface:
        unsafe extern "system" fn(*mut IAsio, *const Guid, *mut *mut c_void) -> i32,
    pub add_ref: unsafe extern "system" fn(*mut IAsio) -> u32,
    pub release: unsafe extern "system" fn(*mut IAsio) -> u32,
    // IASIO (thiscall on x86)
    pub init: asio_fn!(*mut IAsio, *mut c_void; AsioBool),
    pub get_driver_name: asio_fn!(*mut IAsio, *mut c_char; ()),
    pub get_driver_version: asio_fn!(*mut IAsio; i32),
    pub get_error_message: asio_fn!(*mut IAsio, *mut c_char; ()),
    pub start: asio_fn!(*mut IAsio; AsioError),
    pub stop: asio_fn!(*mut IAsio; AsioError),
    pub get_channels: asio_fn!(*mut IAsio, *mut i32, *mut i32; AsioError),
    pub get_latencies: asio_fn!(*mut IAsio, *mut i32, *mut i32; AsioError),
    pub get_buffer_size: asio_fn!(*mut IAsio, *mut i32, *mut i32, *mut i32, *mut i32; AsioError),
    pub can_sample_rate: asio_fn!(*mut IAsio, AsioSampleRate; AsioError),
    pub get_sample_rate: asio_fn!(*mut IAsio, *mut AsioSampleRate; AsioError),
    pub set_sample_rate: asio_fn!(*mut IAsio, AsioSampleRate; AsioError),
    pub get_clock_sources: asio_fn!(*mut IAsio, *mut AsioClockSource, *mut i32; AsioError),
    pub set_clock_source: asio_fn!(*mut IAsio, i32; AsioError),
    pub get_sample_position: asio_fn!(*mut IAsio, *mut AsioSamples, *mut AsioTimeStamp; AsioError),
    pub get_channel_info: asio_fn!(*mut IAsio, *mut AsioChannelInfo; AsioError),
    pub create_buffers:
        asio_fn!(*mut IAsio, *mut AsioBufferInfo, i32, i32, *const AsioCallbacks; AsioError),
    pub dispose_buffers: asio_fn!(*mut IAsio; AsioError),
    pub control_panel: asio_fn!(*mut IAsio; AsioError),
    pub future: asio_fn!(*mut IAsio, i32, *mut c_void; AsioError),
    pub output_ready: asio_fn!(*mut IAsio; AsioError),
}

/// Binary-compatible `GUID`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// Size in bytes of a sample of the given ASIO type (0 if unsupported/DSD).
pub fn bytes_per_sample(t: AsioSampleType) -> usize {
    match t {
        ASIOST_INT16_MSB | ASIOST_INT16_LSB => 2,
        ASIOST_INT24_MSB | ASIOST_INT24_LSB => 3,
        ASIOST_INT32_MSB
        | ASIOST_INT32_LSB
        | ASIOST_FLOAT32_MSB
        | ASIOST_FLOAT32_LSB
        | ASIOST_INT32_MSB16
        | ASIOST_INT32_MSB18
        | ASIOST_INT32_MSB20
        | ASIOST_INT32_MSB24
        | ASIOST_INT32_LSB16
        | ASIOST_INT32_LSB18
        | ASIOST_INT32_LSB20
        | ASIOST_INT32_LSB24 => 4,
        ASIOST_FLOAT64_MSB | ASIOST_FLOAT64_LSB => 8,
        _ => 0,
    }
}

/// Converts a NUL-terminated `c_char` array into a `String` (lossy).
pub fn cstr_to_string(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    #[test]
    fn struct_sizes_match_c_layout() {
        assert_eq!(size_of::<AsioInt64>(), 8);
        assert_eq!(size_of::<AsioClockSource>(), 48);
        assert_eq!(size_of::<AsioChannelInfo>(), 52);
        assert_eq!(align_of::<AsioTimeInfo>(), 4);
        assert_eq!(size_of::<AsioTimeInfo>(), 8 + 8 + 8 + 8 + 4 + 12);
        assert_eq!(size_of::<AsioTimeCode>(), 8 + 8 + 4 + 64);
        assert_eq!(size_of::<AsioTime>(), 16 + 48 + 84);
    }

    #[test]
    fn vtable_has_24_slots() {
        assert_eq!(size_of::<IAsioVtbl>(), 24 * size_of::<usize>());
    }

    #[test]
    fn int64_roundtrip() {
        let v = 0x1234_5678_9abc_def0u64;
        assert_eq!(AsioInt64::from_u64(v).to_u64(), v);
    }
}
