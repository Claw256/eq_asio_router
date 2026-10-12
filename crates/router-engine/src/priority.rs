//! Process / thread hardening so that Windows does not throttle the router while it runs hidden
//! in the tray (efficiency mode / EcoQoS, coarse timers, background scheduling).
//!
//! What is done, and where:
//!
//! * [`harden_process`] (tray app at process start, and the engine at start; idempotent):
//!   * *EcoQoS / power-throttling opt-out*: `SetProcessInformation(ProcessPowerThrottling)` with
//!     `ControlMask = EXECUTION_SPEED | IGNORE_TIMER_RESOLUTION` and `StateMask = 0`, i.e. "I take
//!     control of both policies and want neither": no efficiency-core parking of the audio threads and
//!     timer-resolution requests are honoured although the process has no visible window.
//!   * priority class `ABOVE_NORMAL`, only when the process still runs at `NORMAL` (never lowers or
//!     overrides a choice of the user; not REALTIME, which can starve the system).
//! * [`TimerResolution`]: RAII `timeBeginPeriod(1)` / `timeEndPeriod(1)` held by the engine while it
//!   runs (1 ms timers for the watchdog, status ticks and the capture thread's waits).
//! * [`ControlMmcss`]: MMCSS "Pro Audio" registration of the engine control thread (restored on drop).
//! * [`CallbackMmcss`]: MMCSS "Pro Audio" + `AVRT_PRIORITY_CRITICAL` for the thread that runs the
//!   ASIO callbacks. That thread belongs to the driver, so the registration is done **once, from the
//!   first callback** (and again only if the driver ever calls us from a different thread id; a
//!   `GetCurrentThreadId` compare per callback, no allocation, no syscall beyond that). The task handle
//!   is remembered but never reverted from the audio thread: reverting there could race with the
//!   driver tearing the thread down, and the registration vanishes with the thread anyway.
//!
//! [`HardeningReport`] says which of these succeeded (shown in the GUI status panel and the CLI header).

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering::Relaxed};
use std::sync::Once;

use windows::core::w;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod, TIMERR_NOERROR};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, GetCurrentProcess,
    GetCurrentThreadId, GetPriorityClass, ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
    ABOVE_NORMAL_PRIORITY_CLASS, AVRT_PRIORITY_CRITICAL, NORMAL_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE,
};

/// 0 = not attempted, 1 = ok, 2 = failed.
const NO: u8 = 0;
const OK: u8 = 1;
const FAIL: u8 = 2;

static ECOQOS: AtomicU8 = AtomicU8::new(NO);
static PRIO_CLASS: AtomicU8 = AtomicU8::new(NO);
static TIMER: AtomicU8 = AtomicU8::new(NO);
static TIMER_HOLDERS: AtomicU32 = AtomicU32::new(0);
static MMCSS_CONTROL: AtomicU8 = AtomicU8::new(NO);
static HARDEN_ONCE: Once = Once::new();

fn tri(v: u8) -> Option<bool> {
    match v {
        OK => Some(true),
        FAIL => Some(false),
        _ => None,
    }
}

/// Which hardening steps succeeded (`None` = not attempted yet, e.g. the first ASIO callback has not run).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HardeningReport {
    /// EcoQoS / power-throttling opt-out of the process.
    pub ecoqos_optout: Option<bool>,
    /// Process priority class raised to ABOVE_NORMAL (`Some(true)` also when the user already ran it higher).
    pub priority_class: Option<bool>,
    /// `timeBeginPeriod(1)` is active (engine running).
    pub timer_res: Option<bool>,
    /// MMCSS "Pro Audio" on the engine control thread.
    pub mmcss_control: Option<bool>,
    /// MMCSS "Pro Audio" + CRITICAL on the driver-owned ASIO callback thread.
    pub mmcss_asio: Option<bool>,
}

impl HardeningReport {
    /// Process-wide part of the report plus the ASIO flag from the live counters (`asio` as stored by
    /// [`CallbackMmcss`]: 0 = not yet, 1 = ok, 2 = failed).
    pub fn current(asio: u8) -> HardeningReport {
        HardeningReport {
            ecoqos_optout: tri(ECOQOS.load(Relaxed)),
            priority_class: tri(PRIO_CLASS.load(Relaxed)),
            timer_res: tri(TIMER.load(Relaxed)),
            mmcss_control: tri(MMCSS_CONTROL.load(Relaxed)),
            mmcss_asio: tri(asio),
        }
    }

    /// `(label, state)` rows for the UIs.
    pub fn rows(&self) -> [(&'static str, Option<bool>); 5] {
        [
            ("EcoQoS opt-out", self.ecoqos_optout),
            ("Priority class", self.priority_class),
            ("1 ms timer", self.timer_res),
            ("MMCSS control", self.mmcss_control),
            ("MMCSS ASIO", self.mmcss_asio),
        ]
    }

    /// Compact one-line text, e.g. `ecoqos ok, prio ok, timer ok, mmcss ctl ok/asio ok`.
    pub fn summary(&self) -> String {
        let s = |v: Option<bool>| match v {
            Some(true) => "ok",
            Some(false) => "FAILED",
            None => "pending",
        };
        format!(
            "ecoqos {}, prio {}, timer {}, mmcss ctl {}/asio {}",
            s(self.ecoqos_optout),
            s(self.priority_class),
            s(self.timer_res),
            s(self.mmcss_control),
            s(self.mmcss_asio)
        )
    }
}

/// The `PROCESS_POWER_THROTTLING_STATE` that opts out of EcoQoS and honours timer resolution requests.
pub fn ecoqos_optout_state() -> PROCESS_POWER_THROTTLING_STATE {
    PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    }
}

/// Process-level hardening (EcoQoS opt-out, modest priority class). Idempotent; cheap after the first call.
pub fn harden_process() -> HardeningReport {
    HARDEN_ONCE.call_once(|| {
        let st = ecoqos_optout_state();
        // SAFETY: pseudo handle of the current process; `st` outlives the call and the size matches.
        let r = unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &st as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
        ECOQOS.store(if r.is_ok() { OK } else { FAIL }, Relaxed);
        // SAFETY: current-process pseudo handle.
        let class = unsafe { GetPriorityClass(GetCurrentProcess()) };
        let ok = if class == NORMAL_PRIORITY_CLASS.0 {
            // SAFETY: as above.
            unsafe { SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS) }.is_ok()
        } else {
            class != 0 // already raised (or lowered on purpose by the user): leave it alone
        };
        PRIO_CLASS.store(if ok { OK } else { FAIL }, Relaxed);
    });
    HardeningReport::current(0)
}

/// RAII `timeBeginPeriod(1)` / `timeEndPeriod(1)`.
pub struct TimerResolution {
    active: bool,
}

impl TimerResolution {
    /// Requests 1 ms timer resolution until dropped.
    pub fn begin() -> TimerResolution {
        // SAFETY: plain winmm call; every successful begin is paired with an end in Drop.
        let ok = unsafe { timeBeginPeriod(1) } == TIMERR_NOERROR;
        if ok {
            TIMER_HOLDERS.fetch_add(1, Relaxed);
            TIMER.store(OK, Relaxed);
        } else {
            TIMER.store(FAIL, Relaxed);
        }
        TimerResolution { active: ok }
    }

    /// True if the request was accepted.
    pub fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: paired with the successful begin.
            unsafe {
                let _ = timeEndPeriod(1);
            }
            if TIMER_HOLDERS.fetch_sub(1, Relaxed) == 1 {
                TIMER.store(NO, Relaxed);
            }
        }
    }
}

/// MMCSS "Pro Audio" for the current thread (normal priority); reverted on drop.
pub struct ControlMmcss {
    handle: Option<HANDLE>,
}

impl ControlMmcss {
    /// Registers the calling thread.
    pub fn register() -> ControlMmcss {
        let mut idx = 0u32;
        // SAFETY: NUL-terminated task name literal and a valid out pointer.
        let h = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut idx) }.ok();
        MMCSS_CONTROL.store(if h.is_some() { OK } else { FAIL }, Relaxed);
        ControlMmcss { handle: h }
    }
}

impl Drop for ControlMmcss {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            // SAFETY: handle returned by AvSetMmThreadCharacteristicsW on this very thread.
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(h);
            }
        }
    }
}

/// `ERROR_INVALID_TASK_INDEX` (1552): what `AvSetMmThreadCharacteristicsW` returns when the calling
/// thread already has an MMCSS registration (verified on this machine: second call on one thread).
const ERROR_INVALID_TASK_INDEX: u32 = 1552;

/// True when `code` is the HRESULT form of "this thread is already registered with MMCSS".
fn is_already_registered(code: windows::core::HRESULT) -> bool {
    code == windows::core::HRESULT::from_win32(ERROR_INVALID_TASK_INDEX)
}

/// One-time MMCSS registration of the (driver-owned) ASIO callback thread, driven from the callback.
///
/// Real-time rules: after the first call `ensure` is one `GetCurrentThreadId` and a compare. The
/// registration itself (one syscall, may allocate inside Windows) happens in the very first
/// callback, which is a priming callback whose block is silence anyway. The handle is kept for
/// diagnostics and **never** reverted from the audio thread.
#[derive(Default)]
pub struct CallbackMmcss {
    tid: u32,
    handle: isize,
}

impl CallbackMmcss {
    /// Registers the calling thread if it is not registered yet; returns the result code to store
    /// (`0` = nothing done this call, `1` = ok, `2` = failed).
    #[inline]
    pub fn ensure(&mut self) -> u8 {
        // SAFETY: trivial query.
        let tid = unsafe { GetCurrentThreadId() };
        if tid == self.tid {
            return 0;
        }
        self.tid = tid;
        let mut idx = 0u32;
        // SAFETY: NUL-terminated task name literal and a valid out pointer.
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut idx) } {
            Ok(h) => {
                self.handle = h.0 as isize;
                // SAFETY: handle just returned for this thread.
                if unsafe { AvSetMmThreadPriority(h, AVRT_PRIORITY_CRITICAL) }.is_ok() {
                    OK
                } else {
                    FAIL
                }
            }
            // A thread can belong to only one MMCSS task. The Yamaha driver registers its own
            // buffer-switch thread ("Pro Audio", critical), so our second registration is refused with
            // ERROR_INVALID_TASK_INDEX: the thread is already boosted, which is the goal.
            Err(e) if is_already_registered(e.code()) => OK,
            Err(_) => FAIL,
        }
    }

    /// The remembered MMCSS handle (0 if none).
    pub fn handle(&self) -> isize {
        self.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecoqos_state_shape() {
        let s = ecoqos_optout_state();
        assert_eq!(s.Version, 1);
        assert_eq!(s.StateMask, 0, "StateMask 0 = policy OFF (do not throttle)");
        assert_eq!(s.ControlMask, 0x1 | 0x4);
    }

    #[test]
    fn report_text_and_rows() {
        let r = HardeningReport { ecoqos_optout: Some(true), mmcss_asio: Some(false), ..Default::default() };
        assert_eq!(r.summary(), "ecoqos ok, prio pending, timer pending, mmcss ctl pending/asio FAILED");
        assert_eq!(r.rows()[0], ("EcoQoS opt-out", Some(true)));
        assert_eq!(r.rows()[4], ("MMCSS ASIO", Some(false)));
        assert_eq!(tri(0), None);
        assert_eq!(tri(1), Some(true));
        assert_eq!(tri(2), Some(false));
    }

    #[test]
    fn already_registered_thread_counts_as_ok() {
        // 1552 in HRESULT form is "already registered"; anything else is a real failure.
        assert!(is_already_registered(windows::core::HRESULT::from_win32(1552)));
        assert!(!is_already_registered(windows::core::HRESULT::from_win32(5)));
        // The driver case: the thread was registered by someone else before our first callback.
        std::thread::spawn(|| {
            let mut idx = 0u32;
            // SAFETY: NUL-terminated literal, valid out pointer; reverted below on this same thread.
            let pre = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut idx) };
            let mut m = CallbackMmcss::default();
            let r = m.ensure();
            if let Ok(h) = pre {
                assert_eq!(r, OK, "a thread the driver already registered must not be reported as failed");
                // SAFETY: handle returned above for this thread.
                let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
            }
        })
        .join()
        .unwrap();
    }

    #[test]
    fn process_hardening_is_idempotent_and_reports() {
        let a = harden_process();
        let b = harden_process();
        println!("hardening on this machine: {}", HardeningReport::current(0).summary());
        assert_eq!(a.ecoqos_optout, b.ecoqos_optout);
        assert!(HardeningReport::current(0).ecoqos_optout.is_some(), "attempted exactly once");
        assert!(HardeningReport::current(0).priority_class.is_some());
    }

    #[test]
    fn timer_resolution_guard_is_balanced() {
        // The counter is process-global (other tests may run an engine in parallel), so only the
        // local guards are checked: both succeed or fail together and dropping them is clean.
        let a = TimerResolution::begin();
        let b = TimerResolution::begin();
        assert_eq!(a.is_active(), b.is_active());
        println!("timeBeginPeriod(1): {}", a.is_active());
        drop(a);
        drop(b);
    }

    #[test]
    fn mmcss_registration_on_a_plain_thread() {
        let h = std::thread::spawn(|| {
            let mut c = CallbackMmcss::default();
            let first = c.ensure();
            let second = c.ensure();
            (first, second)
        });
        let (first, second) = h.join().unwrap();
        assert!(first == OK || first == FAIL);
        assert_eq!(second, 0, "second call on the same thread does nothing");
        let c = std::thread::spawn(|| {
            let g = ControlMmcss::register();
            g.handle.is_some()
        });
        println!("MMCSS available: callback thread {}, control thread {}", first == OK, c.join().unwrap());
    }
}
