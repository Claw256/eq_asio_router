//! Config watcher: polls the config file's mtime every 250 ms and publishes a
//! new [`Snapshot`] to the audio thread through a [`Handoff`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use eq_core::Config;

use crate::dlog;
use crate::processor::{Handoff, Snapshot};

pub const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Pure poll step, separated from the thread so it can be driven by a fake clock.
pub struct WatchState {
    last: Option<SystemTime>,
    n_out: usize,
}

impl WatchState {
    pub fn new(initial_mtime: Option<SystemTime>, n_out: usize) -> Self {
        WatchState { last: initial_mtime, n_out }
    }

    /// One tick: frees retired snapshots, and if `mtime` changed, loads and
    /// publishes a new snapshot. Returns true if one was published.
    /// A failed load keeps the current config (the mtime is still recorded so a
    /// broken file is not re-parsed every tick).
    pub fn tick(
        &mut self,
        handoff: &Handoff<Snapshot>,
        mtime: Option<SystemTime>,
        load: impl FnOnce() -> Option<Config>,
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
            Some(cfg) => {
                handoff.publish(Snapshot::from_config(&cfg, self.n_out));
                true
            }
            None => false,
        }
    }
}

pub struct ConfigWatcher {
    stop: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
    pub published: Arc<AtomicU64>,
}

impl ConfigWatcher {
    pub fn spawn(path: Option<PathBuf>, handoff: Arc<Handoff<Snapshot>>, n_out: usize) -> Option<Self> {
        let path = path?;
        let (tx, rx) = channel::<()>();
        let published = Arc::new(AtomicU64::new(0));
        let pub2 = published.clone();
        let handle = std::thread::Builder::new()
            .name("eq-asio-config".into())
            .spawn(move || {
                let mut st = WatchState::new(eq_core::modified(&path), n_out);
                // Ends on a stop message or when the sender is dropped.
                while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(POLL_INTERVAL) {
                    let mtime = eq_core::modified(&path);
                    let sent = st.tick(&handoff, mtime, || match Config::load(&path) {
                        Ok(c) => Some(c),
                        Err(e) => {
                            dlog!("config reload failed (keeping previous): {e}");
                            None
                        }
                    });
                    if sent {
                        pub2.fetch_add(1, Ordering::Relaxed);
                        dlog!("config reloaded");
                    }
                }
                handoff.collect();
            })
            .ok()?;
        Some(ConfigWatcher { stop: Some(tx), handle: Some(handle), published })
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

    #[test]
    fn tick_publishes_only_on_mtime_change() {
        let h = Handoff::new();
        let mut st = WatchState::new(t(1), 2);
        assert!(!st.tick(&h, t(1), || panic!("must not load")));
        assert!(h.take().is_none());
        let c = Config { bypass: true, ..Config::default() };
        assert!(st.tick(&h, t(2), || Some(c)));
        let s = h.take().unwrap();
        assert!(s.settings.bypass);
        h.retire(s);
        // vanished file and failed parse publish nothing
        assert!(!st.tick(&h, None, || panic!("no load")));
        assert!(!st.tick(&h, t(3), || None));
        assert!(!st.tick(&h, t(3), || panic!("broken file not retried")));
        assert!(h.take().is_none());
    }

    #[test]
    fn tick_frees_retired() {
        let h = Handoff::new();
        let mut st = WatchState::new(None, 2);
        assert!(st.tick(&h, t(5), || Some(Config::default())));
        let s = h.take().unwrap();
        h.retire(s);
        st.tick(&h, t(5), || None);
        assert_eq!(h.collect(), 0);
    }

    #[test]
    fn thread_reloads_file_and_stops() {
        let dir = std::env::temp_dir().join(format!("eqdrv_watch_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        Config::default().save(&path).unwrap();
        let h = Arc::new(Handoff::new());
        let mut w = ConfigWatcher::spawn(Some(path.clone()), h.clone(), 2).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(1100)); // mtime granularity
        Config { bypass: true, ..Config::default() }.save(&path).unwrap();
        let mut got = None;
        for _ in 0..40 {
            if let Some(s) = h.take() {
                got = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        w.shutdown();
        assert!(got.expect("snapshot published").settings.bypass);
        assert!(w.published.load(Ordering::Relaxed) >= 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
