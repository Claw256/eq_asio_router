//! Config watcher: polls the config file's mtime every 250 ms, loads and sanitises it,
//! and publishes a new [`Snapshot`] to the audio thread through a [`Handoff`]. Retired
//! snapshots are freed here, never on the audio thread.
//!
//! A config that fails to load keeps the previous (last good) one in force; the error
//! text is available from [`ConfigWatcher::last_error`] until the next successful load.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use eq_core::Config;

use crate::logger::Level;
use crate::processor::{Handoff, Snapshot};
use crate::rlog;

pub const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Shared slot for the last load error (written by the watcher, read by the GUI/engine).
pub type ErrorSlot = Arc<Mutex<Option<String>>>;

/// Pure poll step, separated from the thread so it can be driven by a fake clock.
pub struct WatchState {
    last: Option<SystemTime>,
    n_out: usize,
}

impl WatchState {
    pub fn new(initial_mtime: Option<SystemTime>, n_out: usize) -> Self {
        WatchState { last: initial_mtime, n_out }
    }

    /// One tick: frees retired snapshots, and if `mtime` changed, loads and publishes a
    /// new snapshot built for `sample_rate_hz`. Returns true if one was published.
    ///
    /// `load` returns the sanitised config or an error message. A failed load keeps the
    /// current config and stores the message in `error` (the mtime is still recorded so a
    /// broken file is not re-parsed every tick); a successful one clears `error`.
    pub fn tick(
        &mut self,
        handoff: &Handoff<Snapshot>,
        mtime: Option<SystemTime>,
        sample_rate_hz: u32,
        error: &ErrorSlot,
        load: impl FnOnce() -> Result<Config, String>,
    ) -> bool {
        handoff.collect();
        if mtime == self.last {
            return false;
        }
        self.last = mtime;
        if mtime.is_none() {
            return false; // file vanished: keep what we have
        }
        match load() {
            Ok(cfg) => {
                handoff.publish(Snapshot::from_config(&cfg, self.n_out, sample_rate_hz));
                *error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                true
            }
            Err(msg) => {
                *error.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
                false
            }
        }
    }
}

pub struct ConfigWatcher {
    stop: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
    /// Number of snapshots published so far.
    pub published: Arc<AtomicU64>,
    error: ErrorSlot,
}

impl ConfigWatcher {
    /// Starts the watcher thread. `sample_rate` is read on every reload so the
    /// auto-headroom gain matches the rate the engine currently runs at.
    pub fn spawn(
        path: Option<PathBuf>,
        handoff: Arc<Handoff<Snapshot>>,
        n_out: usize,
        sample_rate: Arc<AtomicU32>,
    ) -> Option<Self> {
        let path = path?;
        let (tx, rx) = channel::<()>();
        let published = Arc::new(AtomicU64::new(0));
        let error: ErrorSlot = Arc::new(Mutex::new(None));
        let (pub2, err2) = (published.clone(), error.clone());
        let handle = std::thread::Builder::new()
            .name("router-config".into())
            .spawn(move || {
                let mut st = WatchState::new(eq_core::modified(&path), n_out);
                // Ends on a stop message or when the sender is dropped.
                while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(POLL_INTERVAL) {
                    let mtime = eq_core::modified(&path);
                    let rate = sample_rate.load(Ordering::Relaxed);
                    let sent = st.tick(&handoff, mtime, rate, &err2, || {
                        Config::load(&path)
                            .map(|c| {
                                if let Some(n) = eq_core::take_deprecation_notice() {
                                    rlog!(Level::Warn, "{n}");
                                }
                                c.sanitized()
                            })
                            .map_err(|e| {
                                rlog!(Level::Warn, "config reload failed (keeping previous): {e}");
                                e.to_string()
                            })
                    });
                    if sent {
                        pub2.fetch_add(1, Ordering::Relaxed);
                        rlog!(Level::Info, "config reloaded");
                    }
                }
                handoff.collect();
            })
            .ok()?;
        Some(ConfigWatcher { stop: Some(tx), handle: Some(handle), published, error })
    }

    /// Message of the most recent failed reload, cleared by the next successful one.
    pub fn last_error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Shareable handle to the error slot (for the GUI/engine).
    pub fn error_slot(&self) -> ErrorSlot {
        self.error.clone()
    }

    /// Stops and joins the thread.
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    fn slot() -> ErrorSlot {
        Arc::new(Mutex::new(None))
    }

    #[test]
    fn tick_publishes_only_on_mtime_change() {
        let h = Handoff::new();
        let e = slot();
        let mut st = WatchState::new(t(1), 2);
        assert!(!st.tick(&h, t(1), 48000, &e, || panic!("must not load")));
        assert!(h.take().is_none());
        let c = Config { bypass: true, ..Config::default() };
        assert!(st.tick(&h, t(2), 48000, &e, || Ok(c)));
        let s = h.take().unwrap();
        assert!(s.settings.bypass);
        assert_eq!(s.sample_rate_hz, 48000);
        h.retire(s);
        // vanished file publishes nothing
        assert!(!st.tick(&h, None, 48000, &e, || panic!("no load")));
        assert!(h.take().is_none());
    }

    #[test]
    fn failed_load_keeps_last_good_and_records_error() {
        let h = Handoff::new();
        let e = slot();
        let mut st = WatchState::new(None, 2);
        assert!(st.tick(&h, t(1), 48000, &e, || Ok(Config::default())));
        assert!(e.lock().unwrap().is_none());
        if let Some(s) = h.take() {
            h.retire(s);
        }
        assert!(!st.tick(&h, t(2), 48000, &e, || Err("parse error at line 3".into())));
        assert!(h.take().is_none(), "nothing published on error");
        assert_eq!(e.lock().unwrap().as_deref(), Some("parse error at line 3"));
        // the broken file is not re-parsed
        assert!(!st.tick(&h, t(2), 48000, &e, || panic!("broken file not retried")));
        assert!(e.lock().unwrap().is_some(), "error stays until a good load");
        // fixed file clears the error
        assert!(st.tick(&h, t(3), 48000, &e, || Ok(Config::default())));
        assert!(e.lock().unwrap().is_none());
    }

    #[test]
    fn tick_frees_retired() {
        let h = Handoff::new();
        let e = slot();
        let mut st = WatchState::new(None, 2);
        assert!(st.tick(&h, t(5), 48000, &e, || Ok(Config::default())));
        let s = h.take().unwrap();
        h.retire(s);
        st.tick(&h, t(5), 48000, &e, || Err(String::new()));
        assert_eq!(h.collect(), 0, "tick already collected it");
    }

    #[test]
    fn snapshot_headroom_uses_given_rate() {
        use eq_core::{EqProfile, FilterKind, FilterSpec};
        let profile = EqProfile {
            name: "t".into(),
            preamp_db: 0.0,
            filters: vec![FilterSpec { kind: FilterKind::Peaking, fc_hz: 1000.0, gain_db: 6.0, q: 1.0, enabled: true }],
        };
        let c = Config { profile, ..Config::default() };
        let s = Snapshot::from_config(&c, 2, 96000);
        assert!(s.extra_gain_db < -5.5 && s.extra_gain_db > -6.5, "{}", s.extra_gain_db);
        let c = Config { auto_headroom: false, ..c };
        assert_eq!(Snapshot::from_config(&c, 2, 96000).extra_gain_db, 0.0);
    }

    #[test]
    fn thread_reloads_file_reports_errors_and_stops() {
        let dir = std::env::temp_dir().join(format!("router_core_watch_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        Config::default().save(&path).unwrap();
        let h = Arc::new(Handoff::new());
        let rate = Arc::new(AtomicU32::new(44100));
        let mut w = ConfigWatcher::spawn(Some(path.clone()), h.clone(), 2, rate).unwrap();
        std::thread::sleep(Duration::from_millis(1100)); // mtime granularity
        Config { bypass: true, safety_buffer_ms: 99.0, ..Config::default() }.save(&path).unwrap();
        let mut got = None;
        for _ in 0..60 {
            if let Some(s) = h.take() {
                got = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let s = got.expect("snapshot published");
        assert!(s.settings.bypass);
        assert_eq!(s.sample_rate_hz, 44100);
        assert!(w.published.load(Ordering::Relaxed) >= 1);
        assert!(w.last_error().is_none());
        // now break the file
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(&path, "this is = not [valid toml").unwrap();
        let mut err = None;
        for _ in 0..60 {
            err = w.last_error();
            if err.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(err.expect("error recorded").contains("parse"));
        assert!(h.take().is_none());
        w.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
