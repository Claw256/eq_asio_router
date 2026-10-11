//! Small app-level settings (`app_settings.toml` next to `config.toml`).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Start the engine when the app launches (default off).
    pub auto_start: bool,
    /// The window close button hides the app to the tray instead of quitting (default on).
    pub close_to_tray: bool,
    /// Start with the window hidden in the tray (default off; `--background` / `--minimized`
    /// force it for one launch).
    pub start_minimized: bool,
    /// Last known "run at login" choice. Only a hint: the registry Run key is the truth and
    /// is re-read at startup (see `autostart`).
    pub run_at_login: bool,
    /// The user already confirmed one source-preset switch (the confirmation is only shown once).
    pub preset_confirmed: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings { auto_start: false, close_to_tray: true, start_minimized: false, run_at_login: false, preset_confirmed: false }
    }
}

pub fn path() -> Option<PathBuf> {
    eq_core::default_path().map(|p| p.with_file_name("app_settings.toml"))
}

impl AppSettings {
    pub fn load() -> AppSettings {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let p = path().ok_or("%APPDATA% is not set")?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = p.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &p).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_parse() {
        assert!(!AppSettings::default().auto_start);
        let s: AppSettings = toml::from_str("auto_start = true").unwrap();
        assert!(s.auto_start);
        let t: AppSettings = toml::from_str("").unwrap();
        assert_eq!(t, AppSettings::default());
    }

    #[test]
    fn new_fields_default_and_old_files_still_load() {
        let d = AppSettings::default();
        assert!(d.close_to_tray && !d.start_minimized && !d.run_at_login);
        // A file written before the tray options existed.
        let old: AppSettings = toml::from_str("auto_start = true
").unwrap();
        assert!(old.auto_start && old.close_to_tray && !old.start_minimized);
        let off: AppSettings = toml::from_str("close_to_tray = false
start_minimized = true").unwrap();
        assert!(!off.close_to_tray && off.start_minimized);
    }

    #[test]
    fn round_trips_through_toml() {
        let s = AppSettings { auto_start: true, close_to_tray: false, start_minimized: true, run_at_login: true, preset_confirmed: true };
        let back: AppSettings = toml::from_str(&toml::to_string_pretty(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
