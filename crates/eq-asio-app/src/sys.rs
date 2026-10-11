//! Thin Windows wrappers: registry lookup of the ASIO DLL folder and process list.

use crate::logic;
use std::path::PathBuf;
use windows::core::PCWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Default value of `HKLM\SOFTWARE\Classes\CLSID\{clsid}\InprocServer32`.
pub fn inproc_server(clsid: &str) -> Option<String> {
    let key = wide(&logic::inproc_subkey(clsid));
    let mut bytes = 0u32;
    // SAFETY: valid NUL-terminated key; null value name selects the default value.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR::null(),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
            None,
            None,
            Some(&mut bytes),
        )
    };
    if rc.is_err() || bytes < 2 {
        return None;
    }
    let mut buf = vec![0u16; bytes as usize / 2 + 1];
    let mut cap = (buf.len() * 2) as u32;
    // SAFETY: buffer of `cap` bytes; same key as above.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR::null(),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut cap),
        )
    };
    if rc.is_err() {
        return None;
    }
    Some(logic::wide_to_string(&buf))
}

/// `ysusb_cp.exe` next to the ASIO DLL of the named driver, if it exists.
pub fn find_control_panel(driver_name: &str) -> Option<PathBuf> {
    let info = asio_host::find_driver(driver_name).ok()?;
    let g = info.clsid;
    let clsid = logic::format_clsid(g.data1, g.data2, g.data3, g.data4);
    let path = logic::panel_path_from_inproc(&inproc_server(&clsid)?)?;
    path.is_file().then_some(path)
}

/// Names of all running processes (empty on failure).
pub fn process_names() -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: snapshot handle is closed below; entry is initialised with dwSize.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                out.push(logic::wide_to_string(&e.szExeFile));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

pub fn bridge_running() -> bool {
    logic::bridge_in(process_names())
}

/// Opens Windows' Sound settings page (`ms-settings:sound`) with the default handler. Only
/// called on a user click; no elevation, nothing is changed.
pub fn open_sound_settings() -> Result<(), String> {
    use windows::core::w;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    // SAFETY: all strings are static NUL-terminated wide literals.
    let h = unsafe { ShellExecuteW(None, w!("open"), w!("ms-settings:sound"), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    let code = h.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!("Could not open Windows Sound settings (code {code})"))
    }
}
