//! Tiny append-only debug log in `%TEMP%\eq_asio_router_driver.log`.
//! Control-path only: never call from the audio callback.

use std::io::Write;

pub fn log(msg: &str) {
    let Some(dir) = std::env::var_os("TEMP") else { return };
    let path = std::path::PathBuf::from(dir).join("eq_asio_router_driver.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let bits = if cfg!(target_pointer_width = "32") { 32 } else { 64 };
        let _ = writeln!(f, "[{t}] pid {} ({bits}-bit) tid {:?}: {msg}", std::process::id(), std::thread::current().id());
    }
}

#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => { $crate::log::log(&format!($($arg)*)) };
}
