//! Change / read an audio endpoint's Windows "device format" (the Sound > Advanced default
//! format, which is also the shared-mode mix format) through the undocumented-but-widely-used
//! `IPolicyConfig` COM interface.
//!
//! This is what the Sound control panel itself uses. It normally needs no administrator rights.
//! The vtable is hand-written (`IPolicyConfigVtbl`); only the methods we call are typed, the
//! rest are opaque slots that keep the layout right.

use crate::device::{find_device, ComGuard};
use crate::{CaptureError, Flow};
use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID, HRESULT, IUnknown, PCWSTR};
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Media::Audio::WAVEFORMATEX;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

/// CLSID_PolicyConfigClient {870AF99C-171D-4F9E-AF0D-E63DF40C2BC9}
pub const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
/// IPolicyConfig (Windows 7+ / 10) {F8679F50-850A-41CF-9C72-430F290290C8}
pub const IID_IPOLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

/// Errors of the policy functions.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("access denied changing the format of \"{0}\" (try running as administrator, or set it in Windows Sound settings)")]
    AccessDenied(String),
    #[error("{context}: {source}")]
    Com {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
    #[error("endpoint lookup failed: {0}")]
    Lookup(String),
    #[error("unexpected device format blob: {0}")]
    BadFormat(String),
    #[error("Windows accepted the change but \"{name}\" still reports {actual} Hz instead of {wanted} Hz (the driver may have reverted it)")]
    NotApplied { name: String, wanted: u32, actual: u32 },
}

/// Opaque vtable slot for methods we never call.
type Slot = *const c_void;

#[repr(C)]
struct IPolicyConfigVtbl {
    query_interface: Slot,
    add_ref: Slot,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    /// GetMixFormat(PCWSTR, WAVEFORMATEX**)
    get_mix_format: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut *mut WAVEFORMATEX) -> HRESULT,
    /// GetDeviceFormat(PCWSTR, BOOL bDefault, WAVEFORMATEX**)
    get_device_format: unsafe extern "system" fn(*mut c_void, PCWSTR, i32, *mut *mut WAVEFORMATEX) -> HRESULT,
    reset_device_format: Slot,
    /// SetDeviceFormat(PCWSTR, WAVEFORMATEX* endpointFormat, WAVEFORMATEX* mixFormat)
    set_device_format:
        unsafe extern "system" fn(*mut c_void, PCWSTR, *mut WAVEFORMATEX, *mut WAVEFORMATEX) -> HRESULT,
    get_processing_period: Slot,
    set_processing_period: Slot,
    get_share_mode: Slot,
    set_share_mode: Slot,
    get_property_value: Slot,
    set_property_value: Slot,
    set_default_endpoint: Slot,
    set_endpoint_visibility: Slot,
}

/// Number of pointer-sized slots: IUnknown (3) + 12 IPolicyConfig methods.
pub const POLICY_VTBL_SLOTS: usize = 15;

struct PolicyConfig(*mut *const IPolicyConfigVtbl);

impl PolicyConfig {
    fn create() -> Result<Self, PolicyError> {
        let com = |context| move |source| PolicyError::Com { context, source };
        // SAFETY: COM initialised by the caller's ComGuard; standard CoCreateInstance + QueryInterface.
        unsafe {
            let unk: IUnknown = CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)
                .map_err(com("CoCreateInstance(PolicyConfigClient)"))?;
            let mut raw: *mut c_void = std::ptr::null_mut();
            unk.query(&IID_IPOLICY_CONFIG, &mut raw).ok().map_err(com("QueryInterface(IPolicyConfig)"))?;
            Ok(PolicyConfig(raw as *mut *const IPolicyConfigVtbl))
        }
    }
    fn vtbl(&self) -> &IPolicyConfigVtbl {
        // SAFETY: a valid COM object pointer; the first field is the vtable pointer.
        unsafe { &**self.0 }
    }
    fn this(&self) -> *mut c_void {
        self.0 as *mut c_void
    }
}

impl Drop for PolicyConfig {
    fn drop(&mut self) {
        // SAFETY: balances the reference obtained by QueryInterface.
        unsafe { (self.vtbl().release)(self.this()) };
    }
}

fn wide(id: &str) -> Vec<u16> {
    id.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copy a COM-allocated WAVEFORMATEX(+extension) into a byte vector and free it.
///
/// # Safety
/// `p` must be a CoTaskMem block holding a valid WAVEFORMATEX.
unsafe fn take_format(p: *mut WAVEFORMATEX) -> Result<Vec<u8>, PolicyError> {
    if p.is_null() {
        return Err(PolicyError::BadFormat("null format".into()));
    }
    let cb = std::ptr::read_unaligned(std::ptr::addr_of!((*p).cbSize)) as usize;
    let tag = std::ptr::read_unaligned(std::ptr::addr_of!((*p).wFormatTag));
    let len = if tag == 0 { 18 } else { 18 + cb };
    let v = std::slice::from_raw_parts(p as *const u8, len).to_vec();
    CoTaskMemFree(Some(p as *const c_void));
    Ok(v)
}

/// Decoded endpoint device format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    /// Raw WAVEFORMATEX / WAVEFORMATEXTENSIBLE bytes as returned by Windows.
    pub raw: Vec<u8>,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Decode the common WAVEFORMATEX header fields from a raw blob.
pub fn decode_format(raw: &[u8]) -> Result<DeviceFormat, PolicyError> {
    if raw.len() < 16 {
        return Err(PolicyError::BadFormat(format!("only {} bytes", raw.len())));
    }
    Ok(DeviceFormat {
        channels: u16_at(raw, 2),
        sample_rate: u32_at(raw, 4),
        bits_per_sample: u16_at(raw, 14),
        raw: raw.to_vec(),
    })
}

/// Same channels / bit depth / container / channel mask / sub-format, new sample rate.
/// `nBlockAlign` is made consistent with channels x container bytes and `nAvgBytesPerSec`
/// with rate x block.
pub fn retarget_format(raw: &[u8], rate: u32) -> Result<Vec<u8>, PolicyError> {
    let f = decode_format(raw)?;
    if f.channels == 0 || f.bits_per_sample == 0 || rate == 0 {
        return Err(PolicyError::BadFormat("zero channels/bits/rate".into()));
    }
    let block = f.channels as u32 * (f.bits_per_sample as u32).div_ceil(8);
    let mut out = raw.to_vec();
    out[4..8].copy_from_slice(&rate.to_le_bytes());
    out[8..12].copy_from_slice(&(rate * block).to_le_bytes());
    out[12..14].copy_from_slice(&(block as u16).to_le_bytes());
    Ok(out)
}

/// Read the endpoint's current device format (`bDefault = false`).
pub fn get_endpoint_device_format(endpoint_id: &str) -> Result<DeviceFormat, PolicyError> {
    let _com = ComGuard::init().map_err(com_err)?;
    let pc = PolicyConfig::create()?;
    let id = wide(endpoint_id);
    let mut p: *mut WAVEFORMATEX = std::ptr::null_mut();
    // SAFETY: valid object, NUL-terminated id, out pointer is a local.
    let hr = unsafe { (pc.vtbl().get_device_format)(pc.this(), PCWSTR(id.as_ptr()), 0, &mut p) };
    hr.ok().map_err(|source| PolicyError::Com { context: "IPolicyConfig::GetDeviceFormat", source })?;
    // SAFETY: Windows returned a CoTaskMem WAVEFORMATEX.
    decode_format(&unsafe { take_format(p)? })
}

/// Read the endpoint's shared-mode mix format (read-only; informational).
pub fn get_endpoint_mix_format(endpoint_id: &str) -> Result<DeviceFormat, PolicyError> {
    let _com = ComGuard::init().map_err(com_err)?;
    let pc = PolicyConfig::create()?;
    let id = wide(endpoint_id);
    let mut p: *mut WAVEFORMATEX = std::ptr::null_mut();
    // SAFETY: as in `get_endpoint_device_format`.
    let hr = unsafe { (pc.vtbl().get_mix_format)(pc.this(), PCWSTR(id.as_ptr()), &mut p) };
    hr.ok().map_err(|source| PolicyError::Com { context: "IPolicyConfig::GetMixFormat", source })?;
    // SAFETY: Windows returned a CoTaskMem WAVEFORMATEX.
    decode_format(&unsafe { take_format(p)? })
}

fn com_err(e: CaptureError) -> PolicyError {
    match e {
        CaptureError::Com { context, source } => PolicyError::Com { context, source },
        other => PolicyError::Lookup(other.to_string()),
    }
}

/// Set the endpoint's device format to the current one with `rate` as sample rate, then re-read
/// (up to ~1 s) to verify. Changes the Windows default format of that endpoint!
pub fn set_endpoint_device_format(endpoint_id: &str, rate: u32) -> Result<(), PolicyError> {
    let cur = get_endpoint_device_format(endpoint_id)?;
    if cur.sample_rate == rate {
        return Ok(());
    }
    let mut new_fmt = retarget_format(&cur.raw, rate)?;
    {
        let _com = ComGuard::init().map_err(com_err)?;
        let pc = PolicyConfig::create()?;
        let id = wide(endpoint_id);
        let p = new_fmt.as_mut_ptr() as *mut WAVEFORMATEX;
        // SAFETY: `new_fmt` is a valid WAVEFORMATEX(EXTENSIBLE) blob that outlives the call; the
        // same pointer is passed as endpoint and mix format (typical usage).
        let hr = unsafe { (pc.vtbl().set_device_format)(pc.this(), PCWSTR(id.as_ptr()), p, p) };
        if hr == E_ACCESSDENIED || hr.0 == 0x8007_0005u32 as i32 {
            return Err(PolicyError::AccessDenied(endpoint_id.to_string()));
        }
        hr.ok().map_err(|source| PolicyError::Com { context: "IPolicyConfig::SetDeviceFormat", source })?;
    }
    let deadline = Instant::now() + Duration::from_millis(1000);
    loop {
        let now = get_endpoint_device_format(endpoint_id)?;
        if now.sample_rate == rate {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(PolicyError::NotApplied {
                name: endpoint_id.to_string(),
                wanted: rate,
                actual: now.sample_rate,
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Derive the cable base name ("Hi-Fi Cable") from an endpoint name such as
/// "Hi-Fi Cable Output" / "Hi-Fi Cable Input" (also "CABLE Output (VB-Audio ...)").
pub fn cable_base_name(name: &str) -> String {
    let t = name.trim();
    for suffix in [" Output", " Input"] {
        if let Some(i) = t.rfind(suffix) {
            return t[..i].trim().to_string();
        }
    }
    t.to_string()
}

/// Outcome for one endpoint of [`match_cable_rates`].
#[derive(Debug)]
pub struct CableRateResult {
    pub flow: Flow,
    pub endpoint_name: String,
    pub endpoint_id: String,
    /// Rate before the call (None if it could not be read).
    pub before: Option<u32>,
    /// Rate after the call (None if it could not be read).
    pub after: Option<u32>,
    /// Ok(()) also when nothing needed to change.
    pub result: Result<(), PolicyError>,
}

impl CableRateResult {
    pub fn changed(&self) -> bool {
        self.result.is_ok() && self.before.is_some() && self.before != self.after
    }
}

/// Locate every cable endpoint (render AND capture) whose name contains one of `cable_names`
/// (e.g. `["Hi-Fi Cable"]`) and set its device format to `rate`.
pub fn match_cable_rates(cable_names: &[&str], rate: u32) -> Vec<CableRateResult> {
    let mut out = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for name in cable_names {
        for flow in [Flow::Render, Flow::Capture] {
            let ep = match find_device(flow, name) {
                Ok(ep) => ep,
                Err(_) => continue,
            };
            if seen.contains(&ep.id) {
                continue;
            }
            seen.push(ep.id.clone());
            let before = get_endpoint_device_format(&ep.id).ok().map(|f| f.sample_rate);
            let result = set_endpoint_device_format(&ep.id, rate);
            let after = get_endpoint_device_format(&ep.id).ok().map(|f| f.sample_rate);
            out.push(CableRateResult {
                flow,
                endpoint_name: ep.name,
                endpoint_id: ep.id,
                before,
                after,
                result,
            });
        }
    }
    if out.is_empty() {
        out.push(CableRateResult {
            flow: Flow::Capture,
            endpoint_name: cable_names.join(", "),
            endpoint_id: String::new(),
            before: None,
            after: None,
            result: Err(PolicyError::Lookup(format!("no endpoint matching {cable_names:?}"))),
        });
    }
    out
}

/// Current device-format rates of every cable endpoint (render, capture) matching `cable_name`.
pub fn read_cable_rates(cable_name: &str) -> Vec<(Flow, String, String, Result<u32, PolicyError>)> {
    let mut out = Vec::new();
    for flow in [Flow::Render, Flow::Capture] {
        if let Ok(ep) = find_device(flow, cable_name) {
            let r = get_endpoint_device_format(&ep.id).map(|f| f.sample_rate);
            out.push((flow, ep.name, ep.id, r));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vtable_size() {
        assert_eq!(std::mem::size_of::<IPolicyConfigVtbl>(), POLICY_VTBL_SLOTS * std::mem::size_of::<usize>());
        // SetDeviceFormat is the 7th method after IUnknown's 3 -> slot index 6.
        assert_eq!(std::mem::offset_of!(IPolicyConfigVtbl, set_device_format), 6 * std::mem::size_of::<usize>());
        assert_eq!(std::mem::offset_of!(IPolicyConfigVtbl, get_device_format), 4 * std::mem::size_of::<usize>());
    }

    fn ext(rate: u32, ch: u16, bits: u16) -> Vec<u8> {
        let block = ch * bits / 8;
        let mut v = Vec::new();
        v.extend(0xFFFEu16.to_le_bytes());
        v.extend(ch.to_le_bytes());
        v.extend(rate.to_le_bytes());
        v.extend((rate * block as u32).to_le_bytes());
        v.extend(block.to_le_bytes());
        v.extend(bits.to_le_bytes());
        v.extend(22u16.to_le_bytes());
        v.extend(bits.to_le_bytes()); // valid bits
        v.extend(3u32.to_le_bytes()); // mask
        v.extend([0u8; 16]); // subformat
        v
    }

    #[test]
    fn retarget_changes_only_rate_fields() {
        let raw = ext(44100, 2, 32);
        let out = retarget_format(&raw, 48000).unwrap();
        let f = decode_format(&out).unwrap();
        assert_eq!(f.sample_rate, 48000);
        assert_eq!(f.channels, 2);
        assert_eq!(u32_at(&out, 8), 48000 * 8);
        assert_eq!(u16_at(&out, 12), 8);
        assert_eq!(out.len(), raw.len());
        assert_eq!(out[14..], raw[14..]);
    }

    #[test]
    fn retarget_rejects_garbage() {
        assert!(retarget_format(&[0; 4], 48000).is_err());
        assert!(retarget_format(&ext(44100, 2, 32), 0).is_err());
    }

    #[test]
    fn base_names() {
        assert_eq!(cable_base_name("Hi-Fi Cable Output"), "Hi-Fi Cable");
        assert_eq!(cable_base_name("Hi-Fi Cable Input"), "Hi-Fi Cable");
        assert_eq!(cable_base_name("Hi-Fi Cable"), "Hi-Fi Cable");
    }
}
