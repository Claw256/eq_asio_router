//! Read-only WDM-KS (kernel streaming) enumeration: filters of a KS category and their pins,
//! pin data ranges, instance counts and (best effort) WaveRT property support.
//!
//! Only opens the *filter* handle and issues `IOCTL_KS_PROPERTY` GET / BASICSUPPORT queries; it
//! never instantiates a pin, so it cannot disturb the audio engine or any stream.

use std::ffi::c_void;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, SetupDiGetDeviceRegistryPropertyW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SPDRP_FRIENDLYNAME, SPDRP_DEVICEDESC, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::IO::DeviceIoControl;

pub const KSCATEGORY_CAPTURE: GUID = GUID::from_u128(0x65E8773D_8F56_11D0_A3B9_00A0C9223196);
pub const KSCATEGORY_RENDER: GUID = GUID::from_u128(0x65E8773E_8F56_11D0_A3B9_00A0C9223196);
pub const KSCATEGORY_AUDIO: GUID = GUID::from_u128(0x6994AD04_93EF_11D0_A3CC_00A0C9223196);
const KSPROPSETID_PIN: GUID = GUID::from_u128(0x8C134960_51AD_11CF_878A_94F801C10000);
const KSPROPSETID_RTAUDIO: GUID = GUID::from_u128(0xA855A48C_2F78_4729_9051_1968746B9EEF);
const IOCTL_KS_PROPERTY: u32 = 0x002F_0003;
const KSPROPERTY_TYPE_GET: u32 = 1;
const KSPROPERTY_TYPE_BASICSUPPORT: u32 = 0x200;

#[repr(C)]
#[derive(Clone, Copy)]
struct KsProperty {
    set: GUID,
    id: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KspPin {
    prop: KsProperty,
    pin_id: u32,
    reserved: u32,
}

/// One pin of a KS filter.
#[derive(Debug, Clone, Default)]
pub struct KsPin {
    pub id: u32,
    pub dataflow: String,
    pub communication: String,
    pub possible_instances: Option<u32>,
    pub current_instances: Option<u32>,
    /// Human readable data ranges ("PCM 2ch 16-32 bit 8000-192000 Hz").
    pub ranges: Vec<String>,
    /// Result of a BASICSUPPORT query of KSPROPERTY_RTAUDIO_BUFFER on the pin factory.
    pub rtaudio: String,
}

/// One KS filter (device interface).
#[derive(Debug, Clone, Default)]
pub struct KsFilter {
    pub name: String,
    pub path: String,
    pub open_error: Option<String>,
    pub pins: Vec<KsPin>,
}

fn guid_name(g: &GUID) -> String {
    let v = g.to_u128();
    match v {
        0x00000001_0000_0010_8000_00AA00389B71 => "PCM".into(),
        0x00000003_0000_0010_8000_00AA00389B71 => "IEEE_FLOAT".into(),
        0x00000000_0000_0010_8000_00AA00389B71 => "ANY".into(),
        0xE436EB7E_524F_11CE_9F53_0020AF0BA770 => "ANALOG".into(),
        _ => format!("{g:?}"),
    }
}

fn utf16z(buf: &[u8]) -> String {
    let w: Vec<u16> = buf.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|&c| c != 0).collect();
    String::from_utf16_lossy(&w)
}

/// # Safety
/// `inp` must be a valid property request of `in_size` bytes.
unsafe fn ks_ioctl(h: HANDLE, inp: *const c_void, in_size: usize, out: &mut [u8]) -> Result<usize, u32> {
    let mut ret = 0u32;
    match DeviceIoControl(h, IOCTL_KS_PROPERTY, Some(inp), in_size as u32, Some(out.as_mut_ptr() as *mut c_void), out.len() as u32, Some(&mut ret), None) {
        Ok(()) => Ok(ret as usize),
        Err(e) => Err((e.code().0 as u32) & 0xFFFF),
    }
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).unwrap_or(0)
}

fn pin_prop(h: HANDLE, pin: u32, id: u32, flags: u32) -> Result<Vec<u8>, u32> {
    let req = KspPin { prop: KsProperty { set: KSPROPSETID_PIN, id, flags }, pin_id: pin, reserved: 0 };
    let mut out = vec![0u8; 64 * 1024];
    // SAFETY: req is a valid KSP_PIN of the given size.
    let n = unsafe { ks_ioctl(h, &req as *const _ as *const c_void, std::mem::size_of::<KspPin>(), &mut out) }?;
    out.truncate(n);
    Ok(out)
}

fn parse_ranges(b: &[u8]) -> Vec<String> {
    // KSMULTIPLE_ITEM { Size, Count } followed by 8-byte aligned KSDATARANGE(_AUDIO) items.
    let count = u32_at(b, 4) as usize;
    let mut off = 8;
    let mut v = Vec::new();
    for _ in 0..count {
        let fsize = u32_at(b, off) as usize;
        if fsize < 64 || off + fsize > b.len() {
            break;
        }
        let sub = GUID::from_values(
            u32_at(b, off + 32),
            u16::from_le_bytes([b[off + 36], b[off + 37]]),
            u16::from_le_bytes([b[off + 38], b[off + 39]]),
            b[off + 40..off + 48].try_into().unwrap_or([0; 8]),
        );
        let mut s = guid_name(&sub);
        if fsize >= 84 {
            s += &format!(
                " <={}ch {}-{} bit {}-{} Hz",
                u32_at(b, off + 64),
                u32_at(b, off + 68),
                u32_at(b, off + 72),
                u32_at(b, off + 76),
                u32_at(b, off + 80)
            );
        }
        v.push(s);
        off += (fsize + 7) & !7;
    }
    v
}

fn describe_pin(h: HANDLE, id: u32) -> KsPin {
    let mut p = KsPin { id, ..Default::default() };
    p.dataflow = match pin_prop(h, id, 2, KSPROPERTY_TYPE_GET) {
        Ok(b) => match u32_at(&b, 0) {
            1 => "IN (render sink)".into(),
            2 => "OUT (capture source)".into(),
            x => format!("flow {x}"),
        },
        Err(e) => format!("err {e}"),
    };
    p.communication = match pin_prop(h, id, 7, KSPROPERTY_TYPE_GET) {
        Ok(b) => match u32_at(&b, 0) {
            0 => "none".into(),
            1 => "sink".into(),
            2 => "source".into(),
            3 => "both".into(),
            4 => "bridge".into(),
            x => format!("comm {x}"),
        },
        Err(e) => format!("err {e}"),
    };
    if let Ok(b) = pin_prop(h, id, 0, KSPROPERTY_TYPE_GET) {
        p.possible_instances = Some(u32_at(&b, 0));
        p.current_instances = Some(u32_at(&b, 4));
    }
    if let Ok(b) = pin_prop(h, id, 3, KSPROPERTY_TYPE_GET) {
        p.ranges = parse_ranges(&b);
    }
    // KSPROPERTY_RTAUDIO_*: HWLATENCY = 2, BUFFER_WITH_NOTIFICATION = 5. BASICSUPPORT on the pin
    // factory is only answered by drivers that expose WaveRT; a failure means "not determinable".
    let mut parts = Vec::new();
    for (pid, nm) in [(2u32, "HWLATENCY"), (5u32, "BUFFER_WITH_NOTIFICATION")] {
        let req = KspPin { prop: KsProperty { set: KSPROPSETID_RTAUDIO, id: pid, flags: KSPROPERTY_TYPE_BASICSUPPORT }, pin_id: id, reserved: 0 };
        let mut out = vec![0u8; 256];
        // SAFETY: valid request.
        match unsafe { ks_ioctl(h, &req as *const _ as *const c_void, std::mem::size_of::<KspPin>(), &mut out) } {
            Ok(n) => parts.push(format!("{nm}: yes ({n} B)")),
            Err(e) => parts.push(format!("{nm}: no/unknown (win32 {e})")),
        }
    }
    p.rtaudio = parts.join("; ");
    p
}

/// Enumerate present filters of `category` and describe their pins.
pub fn list_filters(category: &GUID) -> Result<Vec<KsFilter>, String> {
    let mut out = Vec::new();
    // SAFETY: SetupAPI enumeration with correctly sized structs; list destroyed at the end.
    unsafe {
        let set = SetupDiGetClassDevsW(Some(category), PCWSTR::null(), None, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)
            .map_err(|e| format!("SetupDiGetClassDevs: {e}"))?;
        let mut idx = 0;
        loop {
            let mut ifd = SP_DEVICE_INTERFACE_DATA { cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..Default::default() };
            if SetupDiEnumDeviceInterfaces(set, None, category, idx, &mut ifd).is_err() {
                break;
            }
            idx += 1;
            let mut need = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailW(set, &ifd, None, 0, Some(&mut need), None);
            let mut buf = vec![0u8; need.max(8) as usize + 8];
            let det = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*det).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            let mut dev = SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
            if SetupDiGetDeviceInterfaceDetailW(set, &ifd, Some(det), buf.len() as u32, None, Some(&mut dev)).is_err() {
                continue;
            }
            let pathp = std::ptr::addr_of!((*det).DevicePath) as *const u16;
            let mut n = 0;
            while *pathp.add(n) != 0 {
                n += 1;
            }
            let path_w = std::slice::from_raw_parts(pathp, n + 1).to_vec();
            let path = String::from_utf16_lossy(&path_w[..n]);
            let mut nb = vec![0u8; 512];
            let mut name = String::new();
            for prop in [SPDRP_FRIENDLYNAME, SPDRP_DEVICEDESC] {
                if SetupDiGetDeviceRegistryPropertyW(set, &dev, prop, None, Some(&mut nb), None).is_ok() {
                    name = utf16z(&nb);
                    break;
                }
            }
            let mut f = KsFilter { name, path, ..Default::default() };
            match CreateFileW(
                PCWSTR(path_w.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            ) {
                Ok(h) => {
                    if let Ok(b) = pin_prop(h, 0, 1, KSPROPERTY_TYPE_GET) {
                        let ctypes = u32_at(&b, 0);
                        for pid in 0..ctypes {
                            f.pins.push(describe_pin(h, pid));
                        }
                    } else {
                        f.open_error = Some("opened, but KSPROPERTY_PIN_CTYPES failed".into());
                    }
                    let _ = CloseHandle(h);
                }
                Err(e) => f.open_error = Some(format!("CreateFile: {e}")),
            }
            out.push(f);
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_audio_datarange() {
        // KSMULTIPLE_ITEM{size,count=1} + one 88-byte KSDATARANGE_AUDIO with PCM subformat.
        let mut b = vec![0u8; 8 + 88];
        b[4] = 1;
        b[8] = 84; // FormatSize
        let pcm = GUID::from_u128(0x00000001_0000_0010_8000_00AA00389B71);
        b[8 + 32..8 + 36].copy_from_slice(&pcm.data1.to_le_bytes());
        b[8 + 36..8 + 38].copy_from_slice(&pcm.data2.to_le_bytes());
        b[8 + 38..8 + 40].copy_from_slice(&pcm.data3.to_le_bytes());
        b[8 + 40..8 + 48].copy_from_slice(&pcm.data4);
        b[8 + 64..8 + 68].copy_from_slice(&2u32.to_le_bytes());
        b[8 + 68..8 + 72].copy_from_slice(&16u32.to_le_bytes());
        b[8 + 72..8 + 76].copy_from_slice(&24u32.to_le_bytes());
        b[8 + 76..8 + 80].copy_from_slice(&44100u32.to_le_bytes());
        b[8 + 80..8 + 84].copy_from_slice(&48000u32.to_le_bytes());
        assert_eq!(parse_ranges(&b), vec!["PCM <=2ch 16-24 bit 44100-48000 Hz".to_string()]);
    }

    #[test]
    fn utf16z_stops_at_nul() {
        assert_eq!(utf16z(&[b'A', 0, b'B', 0, 0, 0, b'C', 0]), "AB");
    }
}
