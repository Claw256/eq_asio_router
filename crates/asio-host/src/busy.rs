//! Finding out which processes hold an (exclusive) ASIO driver.
//!
//! Many ASIO drivers (e.g. Yamaha Steinberg USB) can only be opened by one process. When
//! `AsioDriver::load` fails, the driver DLL is usually still loaded in the process that
//! owns the device. We resolve the DLL file names from the CLSID's `InprocServer32`
//! (both registry views) and look for those modules in every process (ToolHelp).
//!
//! Nothing here ever terminates a process; [`ask_holder_to_close`] only posts `WM_CLOSE`.

use std::path::PathBuf;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, Process32FirstW, Process32NextW, MODULEENTRY32W,
    PROCESSENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY,
    REG_EXPAND_SZ, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE,
};

use crate::DriverInfo;

/// A process that has the driver DLL loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    pub exe_name: String,
    pub exe_path: Option<PathBuf>,
}

// ---------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------

/// Lower-cased file name of a DLL path as stored in `InprocServer32` (quotes, blanks and
/// either slash tolerated). `None` if there is no file name.
pub fn dll_name_from_path(value: &str) -> Option<String> {
    let v = value.trim().trim_matches('"').trim();
    let name = v.rsplit(['\\', '/']).next()?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_ascii_lowercase())
    }
}

/// True if `module` equals one of `wanted` (case-insensitive).
pub fn module_matches(module: &str, wanted: &[String]) -> bool {
    wanted.iter().any(|w| w.eq_ignore_ascii_case(module))
}

/// `"in use by A.exe (PID 1), B.exe (PID 2)"`; `"not in use"` when empty.
pub fn describe_holders(holders: &[Holder]) -> String {
    if holders.is_empty() {
        return "not in use".to_string();
    }
    let list: Vec<String> = holders.iter().map(|h| format!("{} (PID {})", h.exe_name, h.pid)).collect();
    format!("in use by {}", list.join(", "))
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

// ---------------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------------

fn read_default_string(key: HKEY) -> Option<String> {
    let mut ty = REG_VALUE_TYPE(0);
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let empty = [0u16];
    // SAFETY: buffers are valid for the sizes passed; empty name selects the default value.
    let rc = unsafe {
        RegQueryValueExW(key, PCWSTR(empty.as_ptr()), None, Some(&mut ty), Some(buf.as_mut_ptr() as *mut u8), Some(&mut len))
    };
    if rc.is_err() || (ty != REG_SZ && ty != REG_EXPAND_SZ) {
        return None;
    }
    let chars = (len as usize / 2).min(buf.len());
    Some(wide_to_string(&buf[..chars]))
}

fn inproc_in_view(clsid: &str, view: REG_SAM_FLAGS) -> Option<String> {
    let path: Vec<u16> = format!("SOFTWARE\\Classes\\CLSID\\{clsid}\\InprocServer32")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut k = HKEY::default();
    // SAFETY: valid NUL-terminated path and out pointer.
    let rc = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(path.as_ptr()), None, KEY_READ | view, &mut k) };
    if rc.is_err() {
        return None;
    }
    let v = read_default_string(k);
    // SAFETY: opened above.
    unsafe {
        let _ = RegCloseKey(k);
    }
    v
}

fn clsid_string(info: &DriverInfo) -> String {
    let g = info.clsid;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1, g.data2, g.data3, g.data4[0], g.data4[1], g.data4[2], g.data4[3], g.data4[4], g.data4[5], g.data4[6], g.data4[7]
    )
}

/// DLL file names (lower case, deduplicated) the driver registers in both registry views,
/// e.g. `["ysusb_asio64.dll", "ysusb_asio32.dll"]`.
pub fn driver_dll_names(info: &DriverInfo) -> Vec<String> {
    let clsid = clsid_string(info);
    let mut out: Vec<String> = Vec::new();
    for view in [KEY_WOW64_64KEY, KEY_WOW64_32KEY] {
        if let Some(n) = inproc_in_view(&clsid, view).as_deref().and_then(dll_name_from_path) {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------
// Process enumeration
// ---------------------------------------------------------------------------------

fn image_path(pid: u32) -> Option<PathBuf> {
    // SAFETY: plain handle open/close; buffer is valid for the length passed.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_FORMAT(0), PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

fn process_has_module(pid: u32, wanted: &[String]) -> bool {
    // SAFETY: ToolHelp snapshot handles are closed before returning; structs are sized.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) else {
            return false; // access denied / process gone / protected
        };
        let mut e = MODULEENTRY32W { dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32, ..Default::default() };
        let mut found = false;
        if Module32FirstW(snap, &mut e).is_ok() {
            loop {
                if module_matches(&wide_to_string(&e.szModule), wanted) {
                    found = true;
                    break;
                }
                if Module32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// Processes (other than this one) that have any of `dll_names` loaded.
pub fn find_processes_with_modules(dll_names: &[String]) -> Vec<Holder> {
    let mut out = Vec::new();
    if dll_names.is_empty() {
        return out;
    }
    let me = std::process::id();
    // SAFETY: see `process_has_module`.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let pid = e.th32ProcessID;
                if pid != me && pid != 0 && process_has_module(pid, dll_names) {
                    out.push(Holder { pid, exe_name: wide_to_string(&e.szExeFile), exe_path: image_path(pid) });
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Processes that have the driver's DLL(s) loaded (excluding this process).
pub fn find_driver_holders(info: &DriverInfo) -> Vec<Holder> {
    find_processes_with_modules(&driver_dll_names(info))
}

// ---------------------------------------------------------------------------------
// Polite close request
// ---------------------------------------------------------------------------------

struct CloseCtx {
    pid: u32,
    posted: u32,
}

unsafe extern "system" fn enum_close(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    // SAFETY: lparam points at the CloseCtx owned by `ask_holder_to_close` for the call's duration.
    let ctx = &mut *(lparam.0 as *mut CloseCtx);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == ctx.pid && IsWindowVisible(hwnd).as_bool() && PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok() {
        ctx.posted += 1;
    }
    windows::core::BOOL(1)
}

/// Posts `WM_CLOSE` to the visible top-level windows of `pid` (a normal close request:
/// the program may ask to save, or ignore it). Never terminates the process.
pub fn ask_holder_to_close(pid: u32) -> Result<(), String> {
    let mut ctx = CloseCtx { pid, posted: 0 };
    // SAFETY: the callback only runs during this call and `ctx` outlives it.
    unsafe {
        let _ = EnumWindows(Some(enum_close), LPARAM(&mut ctx as *mut CloseCtx as isize));
    }
    if ctx.posted == 0 {
        Err("no window found".to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pid: u32, n: &str) -> Holder {
        Holder { pid, exe_name: n.into(), exe_path: None }
    }

    #[test]
    fn dll_names() {
        assert_eq!(
            dll_name_from_path("C:\\Program Files (x86)\\Yamaha\\ysusb_ASIO64.dll").as_deref(),
            Some("ysusb_asio64.dll")
        );
        assert_eq!(dll_name_from_path("  \"C:\\Y\\d.dll\" ").as_deref(), Some("d.dll"));
        assert_eq!(dll_name_from_path("C:/a/b.dll").as_deref(), Some("b.dll"));
        assert_eq!(dll_name_from_path("plain.dll").as_deref(), Some("plain.dll"));
        assert_eq!(dll_name_from_path("C:\\dir\\"), None);
        assert_eq!(dll_name_from_path(""), None);
    }

    #[test]
    fn matching() {
        let w = vec!["ysusb_asio64.dll".to_string(), "ysusb_asio32.dll".to_string()];
        assert!(module_matches("YSUSB_ASIO64.DLL", &w));
        assert!(!module_matches("kernel32.dll", &w));
        assert!(!module_matches("ysusb_asio64.dll", &[]));
    }

    #[test]
    fn describing() {
        assert_eq!(describe_holders(&[]), "not in use");
        assert_eq!(
            describe_holders(&[h(12088, "VBAudioMatrixCoconut_x64.exe")]),
            "in use by VBAudioMatrixCoconut_x64.exe (PID 12088)"
        );
        assert_eq!(describe_holders(&[h(1, "a.exe"), h(2, "b.exe")]), "in use by a.exe (PID 1), b.exe (PID 2)");
    }

    #[test]
    fn close_unknown_pid_fails() {
        assert_eq!(ask_holder_to_close(0xFFFF_FFF0), Err("no window found".to_string()));
    }

    #[test]
    fn no_dlls_no_holders() {
        assert!(find_processes_with_modules(&[]).is_empty());
    }
}
