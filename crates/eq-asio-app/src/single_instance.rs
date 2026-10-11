//! One GUI process per user session.
//!
//! Two GUIs would fight for the Yamaha, so a second launch signals the first one to show its
//! window (named event) and exits quietly. The engine has its own, separate mutex
//! (`Local\eq_asio_router_engine`) which stays as it is.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject};

/// Names of the kernel objects used by the real app.
pub const MUTEX_NAME: &str = "Local\\eq_asio_router_gui";
pub const SHOW_EVENT_NAME: &str = "Local\\eq_asio_router_gui_show";

/// Names of the mutex and show-event; parameterised for tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    pub mutex: String,
    pub event: String,
}

impl Names {
    pub fn real() -> Names {
        Names { mutex: MUTEX_NAME.into(), event: SHOW_EVENT_NAME.into() }
    }

    /// Unique names derived from `tag` (tests).
    #[cfg(test)]
    pub fn test(tag: &str) -> Names {
        Names { mutex: format!("Local\\eq_asio_router_test_{tag}"), event: format!("Local\\eq_asio_router_test_{tag}_show") }
    }
}

/// Result of [`acquire`].
pub enum Outcome {
    /// We are the only GUI; keep the guard for the process lifetime.
    First(Guard),
    /// Another GUI exists and has been asked to show its window.
    Second,
}

/// Owns the mutex and the show-event of the first instance.
pub struct Guard {
    mutex: isize,
    event: isize,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn h(v: isize) -> HANDLE {
    HANDLE(v as *mut _)
}

/// Takes the instance mutex, or signals the running instance. On unexpected API failure we
/// behave as the first instance (never block the user from starting the app).
pub fn acquire(names: &Names) -> Outcome {
    let m = wide(&names.mutex);
    let e = wide(&names.event);
    // SAFETY: NUL-terminated names; GetLastError is read right after CreateMutexW.
    unsafe {
        let Ok(mutex) = CreateMutexW(None, false, PCWSTR(m.as_ptr())) else {
            return Outcome::First(Guard { mutex: 0, event: 0 });
        };
        let exists = GetLastError() == ERROR_ALREADY_EXISTS;
        // Auto-reset event: one signal = one show request.
        let event = CreateEventW(None, false, false, PCWSTR(e.as_ptr())).ok();
        if exists {
            if let Some(ev) = event {
                let _ = SetEvent(ev);
                let _ = CloseHandle(ev);
            }
            let _ = CloseHandle(mutex);
            return Outcome::Second;
        }
        Outcome::First(Guard { mutex: mutex.0 as isize, event: event.map_or(0, |e| e.0 as isize) })
    }
}

impl Guard {
    /// True once per show request from a second launch (non-blocking).
    pub fn take_show_request(&self) -> bool {
        if self.event == 0 {
            return false;
        }
        // SAFETY: the event handle stays valid until Drop.
        unsafe { WaitForSingleObject(h(self.event), 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // SAFETY: handles created in `acquire`.
        unsafe {
            if self.event != 0 {
                let _ = CloseHandle(h(self.event));
            }
            if self.mutex != 0 {
                let _ = CloseHandle(h(self.mutex));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_names() {
        let n = Names::real();
        assert_eq!(n.mutex, "Local\\eq_asio_router_gui");
        assert_eq!(n.event, "Local\\eq_asio_router_gui_show");
    }

    #[test]
    fn second_launch_signals_first() {
        let names = Names::test(&format!("{}", std::process::id()));
        let Outcome::First(g) = acquire(&names) else { panic!("first must win") };
        assert!(!g.take_show_request());
        assert!(matches!(acquire(&names), Outcome::Second));
        assert!(g.take_show_request());
        assert!(!g.take_show_request(), "auto-reset: one request, one signal");
        drop(g);
        // After the first instance exits, a new launch is first again.
        assert!(matches!(acquire(&names), Outcome::First(_)));
    }
}
