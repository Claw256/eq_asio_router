//! Driver registration via elevated `regsvr32` (isolated from the UI).

use std::path::{Path, PathBuf};
use windows::core::{w, PCWSTR};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub const DLL_NAME: &str = "eq_asio_driver.dll";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bitness {
    X86,
    X64,
}

/// Candidate DLL locations, in priority order, for the given exe directory.
pub fn dll_candidates(exe_dir: &Path, bitness: Bitness) -> Vec<PathBuf> {
    let profile = exe_dir
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| *n == "debug" || *n == "release")
        .unwrap_or("release");
    match bitness {
        Bitness::X86 => vec![
            exe_dir.join("driver32").join(DLL_NAME),
            // Dev layout: <target>\<profile>\app.exe next to <target>\i686-pc-windows-msvc\<profile>\.
            exe_dir
                .join("..")
                .join("i686-pc-windows-msvc")
                .join(profile)
                .join(DLL_NAME),
        ],
        Bitness::X64 => vec![exe_dir.join("driver64").join(DLL_NAME), exe_dir.join(DLL_NAME)],
    }
}

/// First existing candidate (and true), else the first candidate (and false).
pub fn resolve_dll(exe_dir: &Path, bitness: Bitness) -> (PathBuf, bool) {
    let cands = dll_candidates(exe_dir, bitness);
    for c in &cands {
        if c.is_file() {
            return (strip_verbatim(&c.canonicalize().unwrap_or_else(|_| c.clone())), true);
        }
    }
    (cands.into_iter().next().unwrap_or_default(), false)
}

/// `regsvr32.exe` for the given bitness (32-bit lives in SysWOW64).
pub fn regsvr32_path(system_root: &Path, bitness: Bitness) -> PathBuf {
    let dir = match bitness {
        Bitness::X86 => "SysWOW64",
        Bitness::X64 => "System32",
    };
    system_root.join(dir).join("regsvr32.exe")
}

/// regsvr32 argument string for a DLL.
pub fn regsvr32_args(dll: &Path, unregister: bool) -> String {
    let flag = if unregister { "/u " } else { "" };
    format!("{flag}\"{}\"", strip_verbatim(dll).display())
}

/// Remove the `\\?\` prefix `canonicalize` adds; regsvr32 dislikes it.
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p.to_path_buf(),
    }
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

fn system_root() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

/// Resolved paths for one bitness, for display in the UI.
#[derive(Clone, Debug)]
pub struct Target {
    pub bitness: Bitness,
    pub dll: PathBuf,
    pub dll_exists: bool,
    pub regsvr32: PathBuf,
}

pub fn resolve_target(bitness: Bitness) -> Target {
    let dir = exe_dir().unwrap_or_default();
    let (dll, dll_exists) = resolve_dll(&dir, bitness);
    Target { bitness, dll, dll_exists, regsvr32: regsvr32_path(&system_root(), bitness) }
}

/// Launch elevated regsvr32 (UAC prompt appears). Asynchronous: success only
/// means the process was started.
pub fn run_regsvr32(target: &Target, unregister: bool) -> Result<(), String> {
    let exe: Vec<u16> = target.regsvr32.to_string_lossy().encode_utf16().chain(Some(0)).collect();
    let args: Vec<u16> = regsvr32_args(&target.dll, unregister).encode_utf16().chain(Some(0)).collect();
    // SAFETY: all strings are NUL-terminated and live across the call.
    let h = unsafe {
        ShellExecuteW(None, w!("runas"), PCWSTR(exe.as_ptr()), PCWSTR(args.as_ptr()), PCWSTR::null(), SW_SHOWNORMAL)
    };
    let code = h.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecute failed or was cancelled (code {code})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_x86_dev_layout() {
        let c = dll_candidates(Path::new(r"C:\p\target\debug"), Bitness::X86);
        assert!(c[0].ends_with(Path::new("driver32").join(DLL_NAME)));
        let s = c[1].to_string_lossy().replace('/', "\\");
        assert!(s.ends_with(r"..\i686-pc-windows-msvc\debug\eq_asio_driver.dll"), "{s}");
    }

    #[test]
    fn candidates_default_profile_is_release() {
        let c = dll_candidates(Path::new(r"C:\Apps\eq"), Bitness::X86);
        assert!(c[1].to_string_lossy().contains("release"));
    }

    #[test]
    fn regsvr_paths_and_args() {
        let r = regsvr32_path(Path::new(r"C:\Windows"), Bitness::X86);
        assert!(r.ends_with(Path::new("SysWOW64").join("regsvr32.exe")));
        let r = regsvr32_path(Path::new(r"C:\Windows"), Bitness::X64);
        assert!(r.ends_with(Path::new("System32").join("regsvr32.exe")));
        assert_eq!(regsvr32_args(Path::new(r"C:\a b\x.dll"), true), r#"/u "C:\a b\x.dll""#);
        assert_eq!(regsvr32_args(Path::new(r"\\?\C:\x.dll"), false), r#""C:\x.dll""#);
    }

    #[test]
    fn missing_dll_reports_not_found() {
        let (p, ok) = resolve_dll(Path::new(r"C:\definitely\not\here\debug"), Bitness::X86);
        assert!(!ok);
        assert!(p.ends_with(Path::new("driver32").join(DLL_NAME)));
    }
}
