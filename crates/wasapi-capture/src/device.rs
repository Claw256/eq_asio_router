//! Endpoint enumeration and format / period discovery.

use crate::convert::{classify, FormatInfo, WAVE_FORMAT_EXTENSIBLE};
use crate::{CaptureError, Result};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioClient, IAudioClient3, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_EXCLUSIVE, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc, PROPVARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};


pub const DEFAULT_CAPTURE_NAME: &str = "Hi-Fi Cable Output";

/// Data flow direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Capture,
    Render,
}

/// One active endpoint.
#[derive(Clone, Debug)]
pub struct EndpointInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// `GetSharedModeEnginePeriod` results (frames at the mix-format rate).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnginePeriods {
    pub default_frames: u32,
    pub fundamental_frames: u32,
    pub min_frames: u32,
    pub max_frames: u32,
}

/// Everything we can learn about a capture endpoint without opening a stream.
#[derive(Clone, Debug)]
pub struct CaptureInfo {
    pub id: String,
    pub name: String,
    pub mix: FormatInfo,
    /// IAudioClient::GetDevicePeriod, 100 ns units.
    pub default_period_hns: i64,
    pub min_period_hns: i64,
    /// IAudioClient3 shared engine periods, if the interface/driver supports it.
    pub engine: Option<EnginePeriods>,
    /// Whether the mix format is accepted in exclusive mode.
    pub exclusive_supported: bool,
}

impl CaptureInfo {
    pub fn default_period_ms(&self) -> f64 {
        self.default_period_hns as f64 / 10_000.0
    }
    pub fn min_period_ms(&self) -> f64 {
        self.min_period_hns as f64 / 10_000.0
    }
    pub fn frames_to_ms(&self, frames: u32) -> f64 {
        frames as f64 * 1000.0 / self.mix.sample_rate as f64
    }
}

/// RAII COM apartment init (MTA). Tolerates a thread already initialised in another mode.
pub(crate) struct ComGuard(bool);

impl ComGuard {
    pub(crate) fn init() -> Result<Self> {
        // SAFETY: plain COM initialisation for the calling thread; balanced in Drop when it succeeded.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            Ok(ComGuard(true))
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(ComGuard(false)) // already initialised as STA; usable
        } else {
            Err(CaptureError::Com { context: "CoInitializeEx", source: windows::core::Error::from(hr) })
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: matches the successful CoInitializeEx in `init`.
            unsafe { CoUninitialize() };
        }
    }
}

/// Owned `WAVEFORMATEX*` from `GetMixFormat` (freed with CoTaskMemFree).
pub(crate) struct MixFormat(pub *mut WAVEFORMATEX);

impl MixFormat {
    pub(crate) fn from_client(client: &IAudioClient) -> Result<Self> {
        // SAFETY: valid client; returned block is owned by us and freed in Drop.
        let p = unsafe { client.GetMixFormat() }.map_err(CaptureError::com("GetMixFormat"))?;
        Ok(MixFormat(p))
    }
    pub(crate) fn ptr(&self) -> *const WAVEFORMATEX {
        self.0
    }
    pub(crate) fn info(&self) -> Result<FormatInfo> {
        // SAFETY: pointer returned by GetMixFormat and valid until Drop.
        unsafe { parse_format(self.0) }
    }
}

impl Drop for MixFormat {
    fn drop(&mut self) {
        // SAFETY: allocated by COM (GetMixFormat) with CoTaskMemAlloc.
        unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
    }
}

/// Decode a WAVEFORMATEX / WAVEFORMATEXTENSIBLE.
///
/// # Safety
/// `p` must point to a valid WAVEFORMATEX (with the extension if its tag says extensible).
pub(crate) unsafe fn parse_format(p: *const WAVEFORMATEX) -> Result<FormatInfo> {
    let w = std::ptr::read_unaligned(p);
    let tag = w.wFormatTag;
    let (sub, valid, mask) = if tag == WAVE_FORMAT_EXTENSIBLE {
        let e = std::ptr::read_unaligned(p as *const WAVEFORMATEXTENSIBLE);
        let g: GUID = e.SubFormat;
        let valid = e.Samples.wValidBitsPerSample;
        (Some(g.to_u128()), valid, e.dwChannelMask)
    } else {
        (None, w.wBitsPerSample, 0)
    };
    let bits = w.wBitsPerSample;
    let format = classify(tag, sub, bits, valid).ok_or_else(|| {
        CaptureError::UnsupportedFormat(format!("tag=0x{tag:04X} bits={bits} valid={valid}"))
    })?;
    Ok(FormatInfo {
        sample_rate: w.nSamplesPerSec,
        channels: w.nChannels,
        bits_per_sample: bits,
        valid_bits: valid,
        channel_mask: mask,
        format,
    })
}

pub(crate) fn enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: COM is initialised by the caller's ComGuard.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(CaptureError::com("CoCreateInstance(MMDeviceEnumerator)"))
}

fn device_id(dev: &IMMDevice) -> Result<String> {
    // SAFETY: GetId returns a CoTaskMem string we free after copying.
    unsafe {
        let p = dev.GetId().map_err(CaptureError::com("IMMDevice::GetId"))?;
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        Ok(s)
    }
}

fn friendly_name(dev: &IMMDevice) -> String {
    // SAFETY: standard property store read; PROPVARIANT cleared and string freed.
    unsafe {
        let Ok(store) = dev.OpenPropertyStore(STGM_READ) else { return String::new() };
        let Ok(mut pv): windows::core::Result<PROPVARIANT> = store.GetValue(&PKEY_Device_FriendlyName)
        else {
            return String::new();
        };
        let name = match PropVariantToStringAlloc(&pv) {
            Ok(p) => {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                s
            }
            Err(_) => String::new(),
        };
        let _ = PropVariantClear(&mut pv);
        name
    }
}

/// List active endpoints of the given flow.
pub fn list_endpoints(flow: Flow) -> Result<Vec<EndpointInfo>> {
    let _com = ComGuard::init()?;
    let en = enumerator()?;
    let df = if flow == Flow::Capture { eCapture } else { eRender };
    // SAFETY: valid enumerator; all returned interfaces are owned COM smart pointers.
    unsafe {
        let default_id = en.GetDefaultAudioEndpoint(df, eConsole).ok().and_then(|d| device_id(&d).ok());
        let coll = en
            .EnumAudioEndpoints(df, DEVICE_STATE_ACTIVE)
            .map_err(CaptureError::com("EnumAudioEndpoints"))?;
        let n = coll.GetCount().map_err(CaptureError::com("GetCount"))?;
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let dev = coll.Item(i).map_err(CaptureError::com("Item"))?;
            let id = device_id(&dev)?;
            let is_default = default_id.as_deref() == Some(id.as_str());
            out.push(EndpointInfo { name: friendly_name(&dev), id, is_default });
        }
        Ok(out)
    }
}

/// Find an active endpoint whose friendly name contains `name_substring` (case-insensitive).
pub fn find_device(flow: Flow, name_substring: &str) -> Result<EndpointInfo> {
    let needle = name_substring.to_lowercase();
    list_endpoints(flow)?
        .into_iter()
        .find(|e| e.name.to_lowercase().contains(&needle))
        .ok_or_else(|| CaptureError::NotFound(name_substring.to_string()))
}

/// Find a capture endpoint; `None` uses [`DEFAULT_CAPTURE_NAME`].
pub fn find_capture_device(name_substring: Option<&str>) -> Result<EndpointInfo> {
    find_device(Flow::Capture, name_substring.unwrap_or(DEFAULT_CAPTURE_NAME))
}

pub(crate) fn open_device(id: &str) -> Result<IMMDevice> {
    let en = enumerator()?;
    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    unsafe { en.GetDevice(PCWSTR(wide.as_ptr())) }.map_err(CaptureError::com("IMMDeviceEnumerator::GetDevice"))
}

pub(crate) fn activate_client(dev: &IMMDevice) -> Result<IAudioClient> {
    // SAFETY: standard activation of IAudioClient.
    unsafe { dev.Activate::<IAudioClient>(CLSCTX_ALL, None) }.map_err(CaptureError::com("Activate(IAudioClient)"))
}

/// Shared-mode mix-format sample rate of a capture endpoint.
pub fn mix_format_sample_rate(device_id: &str) -> Result<u32> {
    Ok(get_capture_info(device_id)?.mix.sample_rate)
}

/// Gather mix format, device periods and IAudioClient3 engine periods.
pub fn get_capture_info(device_id_str: &str) -> Result<CaptureInfo> {
    let _com = ComGuard::init()?;
    let dev = open_device(device_id_str)?;
    let name = friendly_name(&dev);
    let client = activate_client(&dev)?;
    let mix = MixFormat::from_client(&client)?;
    let info = mix.info()?;

    let (mut def, mut min) = (0i64, 0i64);
    // SAFETY: out-pointers are valid locals.
    unsafe { client.GetDevicePeriod(Some(&mut def), Some(&mut min)) }
        .map_err(CaptureError::com("GetDevicePeriod"))?;

    // SAFETY: valid client and format pointer; null closest-match is allowed in exclusive mode.
    let exclusive_supported = unsafe {
        client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, mix.ptr(), None) == windows::core::HRESULT(0)
    };

    let engine = engine_periods(&dev, &mix);
    Ok(CaptureInfo {
        id: device_id_str.to_string(),
        name,
        mix: info,
        default_period_hns: def,
        min_period_hns: min,
        engine,
        exclusive_supported,
    })
}

pub(crate) fn engine_periods(dev: &IMMDevice, mix: &MixFormat) -> Option<EnginePeriods> {
    // SAFETY: Activate of IAudioClient3 fails cleanly on older systems; out-params are locals.
    unsafe {
        let c3 = dev.Activate::<IAudioClient3>(CLSCTX_ALL, None).ok()?;
        let (mut d, mut f, mut mn, mut mx) = (0u32, 0u32, 0u32, 0u32);
        c3.GetSharedModeEnginePeriod(mix.ptr(), &mut d, &mut f, &mut mn, &mut mx).ok()?;
        Some(EnginePeriods { default_frames: d, fundamental_frames: f, min_frames: mn, max_frames: mx })
    }
}
