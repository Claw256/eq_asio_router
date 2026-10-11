//! "Run at login" via the per-user registry Run key (no admin rights, no service).
//!
//! DESIGN: this is deliberately NOT a Windows Service. Services run in session 0, which is
//! isolated from the user's desktop: they cannot show a tray icon and cannot reliably use the
//! user's WASAPI endpoints or the Yamaha ASIO driver (a COM/apartment driver with a control
//! panel, loaded per user). Instead a value under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` launches this GUI at login with
//! `--background`: the window stays hidden (tray only) and the engine auto-starts.
//!
//! The registry location is parameterised ([`Autostart`]) so tests use a throwaway key
//! under `HKCU\Software\eq_asio_router_test` and never touch the real Run key.

use std::path::{Path, PathBuf};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
};

/// The real per-user Run key.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Name of our value inside the Run key.
pub const VALUE_NAME: &str = "EQ ASIO Router";
/// Argument given to the login-time launch.
pub const BACKGROUND_ARG: &str = "--background";

/// What the Run entry currently looks like.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// No entry.
    Disabled,
    /// Entry launches this very executable.
    Enabled,
    /// Entry exists but launches another path (moved/rebuilt exe); holds the stored command.
    PointsElsewhere(String),
}

impl Status {
    /// Checkbox state: only a correct entry counts as "on".
    pub fn is_on(&self) -> bool {
        matches!(self, Status::Enabled)
    }
}

/// Handle on one registry value under `HKCU`.
#[derive(Clone, Debug)]
pub struct Autostart {
    key: String,
    name: String,
}

impl Autostart {
    /// The real Run key / value (what the app uses).
    pub fn real() -> Autostart {
        Autostart { key: RUN_KEY.into(), name: VALUE_NAME.into() }
    }

    /// Custom location (tests).
    #[cfg(test)]
    pub fn with(key: &str, name: &str) -> Autostart {
        Autostart { key: key.into(), name: name.into() }
    }

    /// Current state relative to `exe` (the running executable).
    pub fn status(&self, exe: &Path) -> Status {
        match self.read() {
            None => Status::Disabled,
            Some(cmd) if command_matches(&cmd, exe) => Status::Enabled,
            Some(cmd) => Status::PointsElsewhere(cmd),
        }
    }

    /// Writes (or fixes) the entry for `exe`.
    pub fn enable(&self, exe: &Path) -> Result<(), String> {
        let cmd = command_line(exe);
        let key = wide(&self.key);
        let name = wide(&self.name);
        let data: Vec<u8> = wide(&cmd).iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut h = HKEY::default();
        // SAFETY: valid NUL-terminated strings; `h` is closed below.
        unsafe {
            let rc = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut h,
                None,
            );
            if rc != ERROR_SUCCESS {
                return Err(format!("could not open the Run key: error {}", rc.0));
            }
            let rc = RegSetValueExW(h, PCWSTR(name.as_ptr()), None, REG_SZ, Some(&data));
            let _ = RegCloseKey(h);
            if rc != ERROR_SUCCESS {
                return Err(format!("could not write the Run value: error {}", rc.0));
            }
        }
        Ok(())
    }

    /// Removes the entry (not an error if it does not exist).
    pub fn disable(&self) -> Result<(), String> {
        let key = wide(&self.key);
        let name = wide(&self.name);
        let mut h = HKEY::default();
        // SAFETY: valid strings; handle closed below.
        unsafe {
            let rc = RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), None, KEY_SET_VALUE, &mut h);
            if rc == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            if rc != ERROR_SUCCESS {
                return Err(format!("could not open the Run key: error {}", rc.0));
            }
            let rc = RegDeleteValueW(h, PCWSTR(name.as_ptr()));
            let _ = RegCloseKey(h);
            if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
                return Err(format!("could not delete the Run value: error {}", rc.0));
            }
        }
        Ok(())
    }

    /// Stored command line, if any.
    fn read(&self) -> Option<String> {
        let key = wide(&self.key);
        let name = wide(&self.name);
        let mut bytes = 0u32;
        // SAFETY: valid strings; size query then read into an adequately sized buffer.
        unsafe {
            let rc = RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut bytes),
            );
            if rc != ERROR_SUCCESS || bytes < 2 {
                return None;
            }
            let mut buf = vec![0u16; bytes as usize / 2 + 1];
            let mut cap = (buf.len() * 2) as u32;
            let rc = RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut cap),
            );
            if rc != ERROR_SUCCESS {
                return None;
            }
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..end]))
        }
    }

    /// Deletes the whole key (tests clean up their throwaway key with this).
    #[cfg(test)]
    fn delete_key(&self) {
        use windows::Win32::System::Registry::RegDeleteKeyW;
        let key = wide(&self.key);
        // SAFETY: valid NUL-terminated key name.
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()));
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// `"<exe>" --background`
pub fn command_line(exe: &Path) -> String {
    format!("\"{}\" {BACKGROUND_ARG}", exe.display())
}

/// Executable path of a stored command (quoted or unquoted form).
pub fn command_exe(cmd: &str) -> Option<PathBuf> {
    let cmd = cmd.trim();
    if let Some(rest) = cmd.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some(PathBuf::from(&rest[..end]));
    }
    let end = cmd.find(" --").unwrap_or(cmd.len());
    let p = cmd[..end].trim();
    (!p.is_empty()).then(|| PathBuf::from(p))
}

fn norm(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").to_lowercase()
}

/// True if the stored command launches `exe` (case/slash-insensitive).
pub fn command_matches(cmd: &str, exe: &Path) -> bool {
    command_exe(cmd).is_some_and(|p| norm(&p) == norm(exe))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_command() {
        assert_eq!(
            command_line(Path::new(r"C:\Apps\eq-asio-app.exe")),
            r#""C:\Apps\eq-asio-app.exe" --background"#
        );
    }

    #[test]
    fn parses_and_compares_commands() {
        let exe = Path::new(r"C:\Apps\eq-asio-app.exe");
        assert!(command_matches(&command_line(exe), exe));
        assert!(command_matches(r#""c:/apps/EQ-ASIO-APP.EXE" --background"#, exe));
        assert!(command_matches(r"C:\Apps\eq-asio-app.exe --background", exe));
        assert!(!command_matches(r#""C:\Other\eq-asio-app.exe" --background"#, exe));
        assert!(!command_matches("", exe));
        assert_eq!(command_exe(r#""C:\a b\x.exe" --background"#), Some(PathBuf::from(r"C:\a b\x.exe")));
    }

    #[test]
    fn registry_round_trip_on_test_key() {
        let key = format!(r"Software\eq_asio_router_test\autostart_{}", std::process::id());
        let a = Autostart::with(&key, "EQ ASIO Router test");
        let exe = Path::new(r"C:\Apps\eq-asio-app.exe");
        let other = Path::new(r"C:\Moved\eq-asio-app.exe");
        assert_eq!(a.status(exe), Status::Disabled);
        a.enable(exe).unwrap();
        assert_eq!(a.status(exe), Status::Enabled);
        assert!(a.status(exe).is_on());
        match a.status(other) {
            Status::PointsElsewhere(c) => assert_eq!(c, command_line(exe)),
            s => panic!("{s:?}"),
        }
        assert!(!a.status(other).is_on());
        // Fix: re-enable for the new path.
        a.enable(other).unwrap();
        assert_eq!(a.status(other), Status::Enabled);
        a.disable().unwrap();
        assert_eq!(a.status(other), Status::Disabled);
        a.disable().unwrap(); // idempotent
        a.delete_key();
        let parent = Autostart::with(r"Software\eq_asio_router_test", "x");
        parent.delete_key();
    }

    #[test]
    fn real_key_constants() {
        assert_eq!(RUN_KEY, r"Software\Microsoft\Windows\CurrentVersion\Run");
        assert_eq!(VALUE_NAME, "EQ ASIO Router");
    }
}
