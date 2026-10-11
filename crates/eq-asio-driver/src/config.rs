//! Driver-side view of the shared [`eq_core::Config`].

use std::path::PathBuf;

pub use eq_core::Config;

use crate::ids::DRIVER_NAME;

pub const DEFAULT_MASTER: &str = "Yamaha Steinberg USB ASIO";

/// `%APPDATA%\eq_asio_router\config.toml`.
pub fn config_path() -> Option<PathBuf> {
    eq_core::default_path()
}

/// Loads the config (defaults when the file is missing or unreadable).
pub fn load() -> Config {
    match config_path() {
        Some(p) => Config::load_or_default(p).unwrap_or_default(),
        None => Config::default(),
    }
}

/// Master driver name to host: the configured one, or the Yamaha default when empty.
pub fn effective_master(cfg: &Config) -> String {
    let n = cfg.master_driver.trim();
    if n.is_empty() {
        DEFAULT_MASTER.to_string()
    } else {
        n.to_string()
    }
}

/// True if `name` designates this driver (it must never host itself).
pub fn is_self(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case(DRIVER_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_master(name: &str) -> Config {
        Config { master_driver: name.into(), ..Config::default() }
    }

    #[test]
    fn default_when_empty() {
        assert_eq!(effective_master(&Config::default()), DEFAULT_MASTER);
        assert_eq!(effective_master(&with_master("   ")), DEFAULT_MASTER);
    }

    #[test]
    fn configured_value_is_used() {
        assert_eq!(effective_master(&with_master("ASIO4ALL v2")), "ASIO4ALL v2");
    }

    #[test]
    fn parses_through_eq_core() {
        let c: Config = toml_parse("# c\nbypass = true\nmaster_driver = \"ASIO4ALL v2\"  # note\n");
        assert!(c.bypass);
        assert_eq!(effective_master(&c), "ASIO4ALL v2");
        assert_eq!(effective_master(&toml_parse("master_driver = \"\"")), DEFAULT_MASTER);
        assert_eq!(effective_master(&toml_parse("other = 1")), DEFAULT_MASTER);
    }

    fn toml_parse(text: &str) -> Config {
        let p = std::env::temp_dir().join(format!("eqdrv_cfg_{}_{}.toml", std::process::id(), text.len()));
        std::fs::write(&p, text).unwrap();
        let c = Config::load(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        c
    }

    #[test]
    fn never_self() {
        assert!(is_self("EQ ASIO Router"));
        assert!(is_self(" eq asio router "));
        assert!(!is_self(DEFAULT_MASTER));
    }
}
