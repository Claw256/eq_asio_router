//! Worker thread that owns the [`Engine`] so the UI thread never blocks on it.

use eq_core::{Caps, Config};
use router_engine::{ApplyOutcome, Engine, EngineError, EngineOptions, EngineStatus};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Busy {
    Starting,
    Stopping,
}

/// What the UI sees (cheap to clone).
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// An engine instance exists (it may be restarting or failed inside).
    pub engine_present: bool,
    pub busy: Option<Busy>,
    pub status: Option<EngineStatus>,
    pub caps: Option<Caps>,
    /// Error of the last start attempt.
    pub start_error: Option<EngineError>,
    /// Last `apply` result text.
    pub last_apply: Option<String>,
}

enum Cmd {
    Start(Config),
    Stop,
    Apply(Config),
    Restart,
    RetryNow,
    Learn,
    Shutdown,
}

pub struct EngineHost {
    tx: Sender<Cmd>,
    shared: Arc<Mutex<Snapshot>>,
    thread: Option<JoinHandle<()>>,
}

fn lock(m: &Mutex<Snapshot>) -> std::sync::MutexGuard<'_, Snapshot> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl EngineHost {
    pub fn new() -> EngineHost {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name("engine-host".into())
            .spawn(move || {
                let mut engine: Option<Engine> = None;
                loop {
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                        Ok(Cmd::Start(cfg)) => {
                            if engine.is_none() {
                                lock(&sh).busy = Some(Busy::Starting);
                                // Devices may be late (login-time start): keep retrying instead of failing.
                                let opts = EngineOptions { retry_unavailable: true, ..EngineOptions::default() };
                                match Engine::start_with(cfg, opts) {
                                    Ok(e) => {
                                        engine = Some(e);
                                        lock(&sh).start_error = None;
                                    }
                                    Err(e) => lock(&sh).start_error = Some(e),
                                }
                                lock(&sh).busy = None;
                            }
                        }
                        Ok(Cmd::Stop) => {
                            if let Some(e) = engine.take() {
                                lock(&sh).busy = Some(Busy::Stopping);
                                e.stop();
                                drop(e);
                                lock(&sh).busy = None;
                            }
                            lock(&sh).start_error = None;
                        }
                        Ok(Cmd::Apply(cfg)) => {
                            if let Some(e) = &engine {
                                let text = match e.apply(cfg) {
                                    Ok(ApplyOutcome::Unchanged) => "unchanged".to_string(),
                                    Ok(ApplyOutcome::HotApplied) => "applied live".to_string(),
                                    Ok(ApplyOutcome::RestartScheduled(r)) => format!("restart scheduled ({r})"),
                                    Err(err) => format!("apply failed: {err}"),
                                };
                                lock(&sh).last_apply = Some(text);
                            }
                        }
                        Ok(Cmd::Restart) => {
                            if let Some(e) = &engine {
                                e.restart();
                            }
                        }
                        Ok(Cmd::Learn) => {
                            if let Some(e) = &engine {
                                e.learn_jitter();
                            }
                        }
                        Ok(Cmd::RetryNow) => {
                            if let Some(e) = &engine {
                                e.retry_now();
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    let (status, caps) = match &engine {
                        Some(e) => (Some(e.status()), e.caps()),
                        None => (None, None),
                    };
                    let mut g = lock(&sh);
                    g.engine_present = engine.is_some();
                    g.status = status;
                    if caps.is_some() {
                        g.caps = caps;
                    }
                }
                if let Some(e) = engine.take() {
                    e.stop();
                }
            })
            .expect("spawn engine-host thread");
        EngineHost { tx, shared, thread: Some(thread) }
    }

    pub fn snapshot(&self) -> Snapshot {
        lock(&self.shared).clone()
    }

    pub fn start(&self, cfg: Config) {
        {
            let mut g = lock(&self.shared);
            if g.busy.is_none() && !g.engine_present {
                g.busy = Some(Busy::Starting);
                g.start_error = None;
            }
        }
        let _ = self.tx.send(Cmd::Start(cfg));
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn apply(&self, cfg: Config) {
        let _ = self.tx.send(Cmd::Apply(cfg));
    }

    pub fn retry_now(&self) {
        let _ = self.tx.send(Cmd::RetryNow);
    }

    pub fn restart(&self) {
        let _ = self.tx.send(Cmd::Restart);
    }

    /// Starts the engine's 30 s source-jitter measurement ("Learn").
    pub fn learn(&self) {
        let _ = self.tx.send(Cmd::Learn);
    }

    /// Stop the engine and join the worker (releases the master driver).
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for EngineHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}
