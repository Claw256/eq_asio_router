//! System tray icon (direct Win32 `Shell_NotifyIconW`, no extra event loop).
//!
//! A dedicated thread owns a hidden top-level window (not message-only: message-only
//! windows do NOT receive the broadcast `TaskbarCreated`, which we need to re-add the icon
//! after explorer.exe restarts) and runs the message loop. The thread never touches egui
//! state: menu choices are sent to the app through an `mpsc` channel followed by
//! `egui::Context::request_repaint()`, and the app pushes state (icon colour, tooltip, menu
//! checkmarks) back with [`Tray::update`], which posts a message to the tray window.
//!
//! The pure parts (state -> icon kind / tooltip / menu, id -> command, throttling, icon
//! pixels) are free functions and unit-tested.

use crate::host::{Busy, Snapshot};
use eq_core::FaultState;
use router_engine::EngineState;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------
// Pure model
// ---------------------------------------------------------------------------------------

/// Badge colour = engine state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconKind {
    /// Running.
    Green,
    /// Starting / waiting for a device / restarting.
    Amber,
    /// Stopped.
    Grey,
    /// Failed or muted by the fault detector.
    Red,
}

impl IconKind {
    #[cfg(test)]
    pub const ALL: [IconKind; 4] = [IconKind::Green, IconKind::Amber, IconKind::Grey, IconKind::Red];

    fn index(self) -> usize {
        self as usize
    }

    /// Badge fill colour (R, G, B).
    pub fn rgb(self) -> [u8; 3] {
        match self {
            IconKind::Green => [60, 190, 90],
            IconKind::Amber => [235, 175, 40],
            IconKind::Grey => [140, 140, 140],
            IconKind::Red => [225, 60, 55],
        }
    }
}

/// Everything the tray shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayState {
    pub kind: IconKind,
    pub tooltip: String,
    /// An engine instance exists (menu shows "Stop engine" instead of "Start engine").
    pub engine_on: bool,
    pub engine_busy: bool,
    pub bypass: bool,
    pub run_at_login: bool,
}

impl Default for TrayState {
    fn default() -> Self {
        TrayState {
            kind: IconKind::Grey,
            tooltip: tooltip("Stopped", None),
            engine_on: false,
            engine_busy: false,
            bypass: false,
            run_at_login: false,
        }
    }
}

/// What the user chose in the tray menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    Show,
    ToggleEngine,
    ToggleBypass,
    ToggleRunAtLogin,
    Quit,
}

pub const ID_SHOW: u32 = 1;
pub const ID_ENGINE: u32 = 2;
pub const ID_BYPASS: u32 = 3;
pub const ID_LOGIN: u32 = 4;
pub const ID_QUIT: u32 = 5;

/// Menu id -> command.
pub fn command_for_id(id: u32) -> Option<TrayCommand> {
    match id {
        ID_SHOW => Some(TrayCommand::Show),
        ID_ENGINE => Some(TrayCommand::ToggleEngine),
        ID_BYPASS => Some(TrayCommand::ToggleBypass),
        ID_LOGIN => Some(TrayCommand::ToggleRunAtLogin),
        ID_QUIT => Some(TrayCommand::Quit),
        _ => None,
    }
}

/// One line of the context menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuEntry {
    /// 0 = separator.
    pub id: u32,
    pub label: String,
    pub checked: bool,
    pub enabled: bool,
}

/// Context menu for `st`.
pub fn menu_entries(st: &TrayState) -> Vec<MenuEntry> {
    let item = |id, label: &str, checked, enabled| MenuEntry { id, label: label.into(), checked, enabled };
    vec![
        item(ID_SHOW, "Show window", false, true),
        item(ID_ENGINE, if st.engine_on { "Stop engine" } else { "Start engine" }, false, !st.engine_busy),
        item(ID_BYPASS, "Bypass EQ", st.bypass, true),
        item(0, "", false, true),
        item(ID_LOGIN, "Run at login", st.run_at_login, true),
        item(ID_QUIT, "Quit", false, true),
    ]
}

/// Colour of the badge for an engine snapshot.
pub fn icon_kind(snap: &Snapshot) -> IconKind {
    if snap.busy == Some(Busy::Starting) || snap.busy == Some(Busy::Stopping) {
        return IconKind::Amber;
    }
    if !snap.engine_present {
        return if snap.start_error.is_some() { IconKind::Red } else { IconKind::Grey };
    }
    match snap.status.as_ref().map(|s| (&s.state, s.fault)) {
        Some((EngineState::Running, FaultState::Muted)) => IconKind::Red,
        Some((EngineState::Running, _)) => IconKind::Green,
        Some((EngineState::Starting | EngineState::Restarting { .. } | EngineState::WaitingForDevice { .. }, _)) => {
            IconKind::Amber
        }
        Some((EngineState::Failed, _)) => IconKind::Red,
        Some((EngineState::Stopped, _)) | None => IconKind::Grey,
    }
}

/// "EQ ASIO Router — Running (48000 Hz, 30.2 ms)". `running` = (sample rate, total latency ms).
pub fn tooltip(label: &str, running: Option<(u32, f64, bool)>) -> String {
    let base = format!("EQ ASIO Router \u{2014} {label}");
    let t = match running {
        // an estimated render side makes the total a model: say so ("est."), a measured one is plain
        Some((rate, ms, measured)) if rate > 0 => format!("{base} ({rate} Hz, {ms:.1} ms{})", if measured { "" } else { " est." }),
        _ => base,
    };
    clamp_tip(&t)
}

/// `NOTIFYICONDATAW::szTip` holds 127 UTF-16 units plus NUL.
fn clamp_tip(s: &str) -> String {
    let mut out = String::new();
    let mut units = 0;
    for c in s.chars() {
        units += c.len_utf16();
        if units > 127 {
            break;
        }
        out.push(c);
    }
    out
}

/// Full tray state from the engine snapshot and the app flags.
pub fn tray_state(snap: &Snapshot, label: &str, bypass: bool, run_at_login: bool) -> TrayState {
    let kind = icon_kind(snap);
    let running = (kind == IconKind::Green)
        .then(|| snap.status.as_ref().map(|s| (s.asio_rate, s.latency.total_ms, s.latency.render_basis.is_measured())))
        .flatten();
    TrayState {
        kind,
        tooltip: tooltip(label, running),
        engine_on: snap.engine_present,
        engine_busy: snap.busy.is_some(),
        bypass,
        run_at_login,
    }
}

/// Pushes state to the tray at most once per `min_gap` and only when it changed.
#[derive(Debug)]
pub struct Throttle {
    min_gap: Duration,
    last_sent: Option<(Instant, TrayState)>,
}

impl Throttle {
    pub fn new(min_gap: Duration) -> Self {
        Throttle { min_gap, last_sent: None }
    }

    /// Returns true (and remembers `st`) if it should be sent now.
    pub fn should_send(&mut self, now: Instant, st: &TrayState) -> bool {
        let send = match &self.last_sent {
            None => true,
            Some((t, prev)) => prev != st && now.duration_since(*t) >= self.min_gap,
        };
        if send {
            self.last_sent = Some((now, st.clone()));
        }
        send
    }
}

/// Round badge, `size` x `size` pixels, straight-alpha RGBA, anti-aliased edge, darker rim
/// and a soft highlight.
pub fn render_icon_rgba(kind: IconKind, size: usize) -> Vec<u8> {
    let [r, g, b] = kind.rgb();
    let s = size as f32;
    let c = s / 2.0;
    let radius = c - 0.5;
    let rim = (s * 0.09).max(1.0);
    let mut px = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - c;
            let dy = y as f32 + 0.5 - c;
            let d = (dx * dx + dy * dy).sqrt();
            let cover = (radius + 0.5 - d).clamp(0.0, 1.0);
            if cover <= 0.0 {
                continue;
            }
            // Darker rim, brighter upper-left highlight.
            let rim_t = ((d - (radius - rim)) / rim).clamp(0.0, 1.0);
            let hl = (1.0 - (((dx + dy * 0.8) / s) + 0.35).abs() * 3.0).clamp(0.0, 1.0) * 0.18;
            let shade = |v: u8| -> u8 {
                let v = f32::from(v) * (1.0 - 0.35 * rim_t);
                (v + (255.0 - v) * hl).clamp(0.0, 255.0) as u8
            };
            let i = (y * size + x) * 4;
            px[i] = shade(r);
            px[i + 1] = shade(g);
            px[i + 2] = shade(b);
            px[i + 3] = (cover * 255.0) as u8;
        }
    }
    px
}

// ---------------------------------------------------------------------------------------
// Win32 implementation
// ---------------------------------------------------------------------------------------

const WM_TRAY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;
const WM_APP_UPDATE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 2;
const WM_APP_QUIT: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 3;

struct Shared {
    tx: Mutex<Sender<TrayCommand>>,
    ctx: egui::Context,
    state: Mutex<TrayState>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Handle to the tray thread. Dropping it removes the icon.
pub struct Tray {
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Tray {
    /// Creates the icon. Fails (returns the reason) if the window or icon cannot be created,
    /// in which case the caller must not hide the main window.
    pub fn spawn(tx: Sender<TrayCommand>, ctx: egui::Context, initial: TrayState) -> Result<Tray, String> {
        let shared = Shared { tx: Mutex::new(tx), ctx, state: Mutex::new(initial) };
        if SHARED.set(shared).is_err() {
            return Err("tray already created".into());
        }
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let thread = std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || win::run(ready_tx))
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Tray { thread: Some(thread) }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("tray thread did not start".into()),
        }
    }

    /// Hands new state to the tray window (icon colour, tooltip, menu).
    pub fn update(&self, st: &TrayState) {
        if let Some(sh) = SHARED.get() {
            *lock(&sh.state) = st.clone();
            win::post(WM_APP_UPDATE);
        }
    }

    /// Removes the icon and stops the thread.
    pub fn shutdown(&mut self) {
        win::post(WM_APP_QUIT);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.shutdown();
    }
}

mod win {
    use super::*;
    use std::cell::RefCell;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
        DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, PostMessageW, PostQuitMessage,
        RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu, TranslateMessage, HICON, ICONINFO,
        MF_CHECKED, MF_DISABLED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, SM_CXSMICON, TPM_BOTTOMALIGN, TPM_RETURNCMD,
        TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_NULL, WM_RBUTTONUP,
        WNDCLASSW, WS_OVERLAPPED,
    };

    struct Local {
        icons: [Option<HICON>; 4],
        shown_kind: Option<IconKind>,
        shown_tip: String,
        added: bool,
        taskbar_created: u32,
    }

    thread_local! {
        static LOCAL: RefCell<Local> = const { RefCell::new(Local {
            icons: [None; 4],
            shown_kind: None,
            shown_tip: String::new(),
            added: false,
            taskbar_created: 0,
        }) };
    }

    pub fn post(msg: u32) {
        let raw = HWND_RAW.load(Ordering::Acquire);
        if raw != 0 {
            // SAFETY: posting to a window handle; a stale handle just fails.
            unsafe {
                let _ = PostMessageW(Some(HWND(raw as *mut _)), msg, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn make_icon(kind: IconKind) -> Option<HICON> {
        // SAFETY: plain GDI/user32 object creation; temporary bitmaps are deleted after the
        // icon (which copies them) has been created.
        unsafe {
            let size = GetSystemMetrics(SM_CXSMICON).clamp(16, 64) as usize;
            let rgba = render_icon_rgba(kind, size);
            let mut bgra = rgba.clone();
            for p in bgra.as_chunks_mut::<4>().0 {
                p.swap(0, 2);
            }
            let color = CreateBitmap(size as i32, size as i32, 1, 32, Some(bgra.as_ptr().cast()));
            let mask = CreateBitmap(size as i32, size as i32, 1, 1, None);
            let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
            let icon = CreateIconIndirect(&info).ok();
            let _ = DeleteObject(color.into());
            let _ = DeleteObject(mask.into());
            icon
        }
    }

    fn icon_for(local: &mut Local, kind: IconKind) -> HICON {
        let slot = &mut local.icons[kind.index()];
        if slot.is_none() {
            *slot = make_icon(kind);
        }
        slot.unwrap_or_default()
    }

    fn nid(hwnd: HWND, icon: HICON, tip: &str) -> NOTIFYICONDATAW {
        let mut n = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_TIP | NIF_MESSAGE,
            uCallbackMessage: WM_TRAY,
            hIcon: icon,
            ..Default::default()
        };
        for (d, s) in n.szTip.iter_mut().take(127).zip(tip.encode_utf16()) {
            *d = s;
        }
        n
    }

    /// Adds or modifies the icon to match `st`; `force_add` re-adds after explorer restarts.
    fn apply(hwnd: HWND, st: &TrayState, force_add: bool) -> bool {
        LOCAL.with(|l| {
            let mut l = l.borrow_mut();
            let icon = icon_for(&mut l, st.kind);
            let data = nid(hwnd, icon, &st.tooltip);
            let ok = if force_add || !l.added {
                // SAFETY: `data` is fully initialised for the call.
                let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool();
                l.added = ok;
                ok
            } else if l.shown_kind != Some(st.kind) || l.shown_tip != st.tooltip {
                // SAFETY: as above.
                unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool()
            } else {
                true
            };
            if ok {
                l.shown_kind = Some(st.kind);
                l.shown_tip = st.tooltip.clone();
            }
            ok
        })
    }

    fn send(cmd: TrayCommand) {
        if let Some(sh) = SHARED.get() {
            let _ = lock(&sh.tx).send(cmd);
            sh.ctx.request_repaint();
        }
    }

    fn show_menu(hwnd: HWND) {
        let Some(sh) = SHARED.get() else { return };
        let st = lock(&sh.state).clone();
        // SAFETY: menu is created, used and destroyed here; strings outlive the calls.
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            for e in menu_entries(&st) {
                if e.id == 0 {
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                    continue;
                }
                let label: Vec<u16> = e.label.encode_utf16().chain(Some(0)).collect();
                let mut flags = MF_STRING;
                if e.checked {
                    flags |= MF_CHECKED;
                }
                if !e.enabled {
                    flags |= MF_GRAYED | MF_DISABLED;
                }
                let _ = AppendMenuW(menu, flags, e.id as usize, PCWSTR(label.as_ptr()));
            }
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Required so the menu closes when the user clicks elsewhere.
            let _ = SetForegroundWindow(hwnd);
            let id = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, None, hwnd, None);
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            if id.0 > 0 {
                if let Some(c) = command_for_id(id.0 as u32) {
                    send(c);
                }
            }
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        let taskbar_created = LOCAL.with(|l| l.borrow().taskbar_created);
        match msg {
            WM_TRAY => {
                match (lp.0 as u32) & 0xFFFF {
                    WM_LBUTTONDBLCLK => send(TrayCommand::Show),
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_APP_UPDATE => {
                if let Some(sh) = SHARED.get() {
                    let st = lock(&sh.state).clone();
                    apply(hwnd, &st, false);
                }
                LRESULT(0)
            }
            WM_APP_QUIT => {
                // SAFETY: destroying our own window from its thread.
                let _ = unsafe { DestroyWindow(hwnd) };
                LRESULT(0)
            }
            WM_DESTROY => {
                LOCAL.with(|l| {
                    let mut l = l.borrow_mut();
                    if l.added {
                        let d = NOTIFYICONDATAW {
                            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                            hWnd: hwnd,
                            uID: 1,
                            ..Default::default()
                        };
                        // SAFETY: delete our own icon.
                        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &d) };
                        l.added = false;
                    }
                    for i in l.icons.iter_mut() {
                        if let Some(h) = i.take() {
                            // SAFETY: icon created by us, no longer displayed.
                            let _ = unsafe { DestroyIcon(h) };
                        }
                    }
                });
                // SAFETY: ends this thread's message loop.
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            m if m != 0 && m == taskbar_created => {
                // explorer.exe restarted: our icon is gone, add it again.
                if let Some(sh) = SHARED.get() {
                    let st = lock(&sh.state).clone();
                    apply(hwnd, &st, true);
                    crate::dlog("tray: re-added after TaskbarCreated");
                }
                LRESULT(0)
            }
            // SAFETY: default handling for everything else.
            _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
        }
    }

    pub fn run(ready: Sender<Result<(), String>>) {
        // SAFETY: standard window class registration / creation on this thread.
        unsafe {
            let hinst = GetModuleHandleW(None).unwrap_or_default();
            let class = w!("EqAsioRouterTrayWindow");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let tc = RegisterWindowMessageW(w!("TaskbarCreated"));
            LOCAL.with(|l| l.borrow_mut().taskbar_created = tc);
            // Hidden top-level window (never shown): receives TaskbarCreated broadcasts.
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("EQ ASIO Router tray"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinst.into()),
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready.send(Err(format!("tray window: {e}")));
                    return;
                }
            };
            HWND_RAW.store(hwnd.0 as isize, Ordering::Release);
            let st = SHARED.get().map(|s| lock(&s.state).clone()).unwrap_or_default();
            if !apply(hwnd, &st, true) {
                let _ = ready.send(Err("Shell_NotifyIconW(NIM_ADD) failed".into()));
                let _ = DestroyWindow(hwnd);
                return;
            }
            crate::dlog("tray icon created");
            let _ = ready.send(Ok(()));
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            HWND_RAW.store(0, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Snapshot;
    use eq_core::LatencyBreakdown;
    use router_engine::{EngineError, EngineStatus};

    fn snap(state: EngineState) -> Snapshot {
        Snapshot {
            engine_present: true,
            status: Some(EngineStatus {
                state,
                asio_rate: 48_000,
                latency: LatencyBreakdown { total_ms: 30.24, ..Default::default() },
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn kind_per_state() {
        assert_eq!(icon_kind(&snap(EngineState::Running)), IconKind::Green);
        assert_eq!(icon_kind(&snap(EngineState::Starting)), IconKind::Amber);
        assert_eq!(icon_kind(&snap(EngineState::Restarting { reason: "x".into() })), IconKind::Amber);
        assert_eq!(icon_kind(&snap(EngineState::WaitingForDevice { holders: vec![] })), IconKind::Amber);
        assert_eq!(icon_kind(&snap(EngineState::Stopped)), IconKind::Grey);
        assert_eq!(icon_kind(&snap(EngineState::Failed)), IconKind::Red);
        assert_eq!(icon_kind(&Snapshot::default()), IconKind::Grey);
        let mut m = snap(EngineState::Running);
        m.status.as_mut().unwrap().fault = FaultState::Muted;
        assert_eq!(icon_kind(&m), IconKind::Red);
        let busy = Snapshot { busy: Some(Busy::Starting), ..Default::default() };
        assert_eq!(icon_kind(&busy), IconKind::Amber);
        let err = Snapshot { start_error: Some(EngineError::AlreadyRunning), ..Default::default() };
        assert_eq!(icon_kind(&err), IconKind::Red);
    }

    #[test]
    fn tooltip_text() {
        let s = tray_state(&snap(EngineState::Running), "Running", false, false);
        assert_eq!(s.tooltip, "EQ ASIO Router \u{2014} Running (48000 Hz, 30.2 ms est.)");
        let s = tray_state(&Snapshot::default(), "Stopped", false, false);
        assert_eq!(s.tooltip, "EQ ASIO Router \u{2014} Stopped");
        let long = tooltip(&"x".repeat(300), None);
        assert_eq!(long.encode_utf16().count(), 127);
    }

    #[test]
    fn menu_labels_and_checks() {
        let mut st = TrayState::default();
        let m = menu_entries(&st);
        assert_eq!(m[1].label, "Start engine");
        assert!(!m[2].checked && !m[4].checked);
        st.engine_on = true;
        st.bypass = true;
        st.run_at_login = true;
        let m = menu_entries(&st);
        assert_eq!(m[1].label, "Stop engine");
        assert!(m[2].checked && m[4].checked);
        assert_eq!(m.iter().filter(|e| e.id == 0).count(), 1, "one separator");
        assert_eq!(m.last().unwrap().label, "Quit");
        st.engine_busy = true;
        assert!(!menu_entries(&st)[1].enabled);
    }

    #[test]
    fn id_mapping_covers_menu() {
        for e in menu_entries(&TrayState::default()).iter().filter(|e| e.id != 0) {
            assert!(command_for_id(e.id).is_some(), "{}", e.label);
        }
        assert_eq!(command_for_id(ID_QUIT), Some(TrayCommand::Quit));
        assert_eq!(command_for_id(ID_BYPASS), Some(TrayCommand::ToggleBypass));
        assert_eq!(command_for_id(0), None);
        assert_eq!(command_for_id(99), None);
    }

    #[test]
    fn throttle_once_per_second_on_change_only() {
        let mut t = Throttle::new(Duration::from_secs(1));
        let t0 = Instant::now();
        let a = TrayState::default();
        let mut b = a.clone();
        b.kind = IconKind::Green;
        assert!(t.should_send(t0, &a), "first always sent");
        assert!(!t.should_send(t0 + Duration::from_secs(5), &a), "unchanged");
        assert!(!t.should_send(t0 + Duration::from_millis(500), &b), "changed but too soon");
        assert!(t.should_send(t0 + Duration::from_millis(1100), &b), "changed and due");
        assert!(!t.should_send(t0 + Duration::from_millis(1200), &a), "flip back too soon");
    }

    #[test]
    fn icon_pixels() {
        for kind in IconKind::ALL {
            let px = render_icon_rgba(kind, 16);
            assert_eq!(px.len(), 16 * 16 * 4);
            assert_eq!(px[3], 0, "corner transparent");
            let centre = (8 * 16 + 8) * 4;
            assert_eq!(px[centre + 3], 255, "centre opaque");
        }
        let g = render_icon_rgba(IconKind::Green, 32);
        let r = render_icon_rgba(IconKind::Red, 32);
        let c = (16 * 32 + 16) * 4;
        assert!(g[c + 1] > g[c] && r[c] > r[c + 1], "green is green, red is red");
    }
}
