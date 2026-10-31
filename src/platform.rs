//! Win32 window behaviour: keeping keyboard focus in CSP, and spotting when CSP is in front.

use std::ffi::c_void;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, VK_LBUTTON,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIcon, DispatchMessageW, FindWindowW, GetAncestor, GetClassNameW, GetCursorPos, GetForegroundWindow,
    GetSystemMetrics, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, MsgWaitForMultipleObjects, PeekMessageW, SendMessageTimeoutW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, WindowFromPoint, EVENT_SYSTEM_FOREGROUND, GA_ROOTOWNER, GWL_EXSTYLE,
    GW_HWNDNEXT, GW_HWNDPREV, HICON, HTCAPTION, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, MA_NOACTIVATE, MSG,
    PM_REMOVE, QS_ALLINPUT, SMTO_ABORTIFHUNG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, STYLESTRUCT, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    WINDOWPOS, WINEVENT_OUTOFCONTEXT, WM_MOUSEACTIVATE,
    WM_NCHITTEST, WM_STYLECHANGING, WM_WINDOWPOSCHANGING, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST,
};

use crate::frame;

/// Set while a text field holds the keyboard. There is only one palette window.
static TYPING: AtomicBool = AtomicBool::new(false);
/// Set while the window should act like any other, because the user is not in CSP.
static ORDINARY_WINDOW: AtomicBool = AtomicBool::new(false);
/// Set by the foreground event, to wake the thread that keeps the window's level.
static FOREGROUND_CHANGED: AtomicBool = AtomicBool::new(false);
/// Alternates the title bar click position so two quick clicks never count as a
/// double-click, which would maximise CSP.
static USE_SECOND_SPOT: AtomicBool = AtomicBool::new(false);
/// Set while the window is hidden in the tray.
static CLOAKED: AtomicBool = AtomicBool::new(false);

/// The settings window while it is open, for the thread that follows CSP.
static SETTINGS_WINDOW: AtomicIsize = AtomicIsize::new(0);
/// The same as `TYPING`, for the settings window.
static SETTINGS_TYPING: AtomicBool = AtomicBool::new(false);

/// Why there was nothing to restore, as is normal while the user is in another program.
pub const CSP_NOT_IN_FRONT: &str = "CSP 不在前台";
const HIT_TEST_TIMEOUT_MS: u32 = 50;
/// A point this far in from the window's corner lies on its title bar.
const TITLE_BAR_PROBE: i32 = 20;
const TOPMOST_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const TOPMOST_REASSERT_INTERVAL: Duration = Duration::from_secs(3);
/// How long the foreground must stay away from CSP before this window steps back.
const TOPMOST_LOWER_DELAY: Duration = Duration::from_millis(120);
const ORDINARY_SETTLE_TIME: Duration = Duration::from_secs(1);
/// Far enough apart to fall outside the system's double-click rectangle.
const TITLE_SPOT_STEP: i32 = 24;
const TITLE_ROW_STEP: i32 = 6;
const TITLE_ROW_LIMIT: i32 = 60;
/// Between the main window and the settings window when it opens beside it.
const WINDOW_GAP: i32 = 8;

/// winit rewrites the extended style whenever its own window flags change, and
/// shows the window with a plain `SW_SHOW` after the first time. Both would undo
/// a style that was only set once, so these messages are guarded at the source.
unsafe extern "system" fn palette_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    // winit rewrites the whole extended style without knowing about the topmost state,
    // which `follow_csp` sets behind its back.
    if message == WM_STYLECHANGING && wparam as i32 == GWL_EXSTYLE {
        // SAFETY: for this message Windows passes a valid STYLESTRUCT.
        let style = unsafe { &mut *(lparam as *mut STYLESTRUCT) };
        style.styleNew = (style.styleNew & !WS_EX_TOPMOST) | (style.styleOld & WS_EX_TOPMOST);
        // Keeps it off the taskbar and out of Alt+Tab, as the tray icon stands in for both.
        style.styleNew = (style.styleNew & !WS_EX_APPWINDOW) | WS_EX_TOOLWINDOW;
    }
    if message == WM_MOUSEACTIVATE && keeps_inactive(&TYPING) {
        return MA_NOACTIVATE as LRESULT;
    }
    if keeps_inactive(&TYPING) {
        guard_activation(message, wparam, lparam);
    }
    // SAFETY: forwards the arguments Windows gave us.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

/// The settings window stays out of the way of CSP just as the palette does.
unsafe extern "system" fn settings_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_MOUSEACTIVATE && keeps_inactive(&SETTINGS_TYPING) {
        return MA_NOACTIVATE as LRESULT;
    }
    if keeps_inactive(&SETTINGS_TYPING) {
        guard_activation(message, wparam, lparam);
    }
    // SAFETY: forwards the arguments Windows gave us.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

fn keeps_inactive(typing: &AtomicBool) -> bool {
    !typing.load(Ordering::Relaxed) && !ORDINARY_WINDOW.load(Ordering::Relaxed)
}

fn guard_activation(message: u32, wparam: WPARAM, lparam: LPARAM) {
    match message {
        WM_STYLECHANGING if wparam as i32 == GWL_EXSTYLE => {
            // SAFETY: for this message Windows passes a valid STYLESTRUCT.
            unsafe { (*(lparam as *mut STYLESTRUCT)).styleNew |= WS_EX_NOACTIVATE };
        }
        WM_WINDOWPOSCHANGING => {
            // SAFETY: for this message Windows passes a valid WINDOWPOS.
            unsafe { (*(lparam as *mut WINDOWPOS)).flags |= SWP_NOACTIVATE };
        }
        _ => {}
    }
}

/// Starts keeping a newly opened settings window from taking the foreground from CSP.
pub fn adopt_settings(settings: HWND) {
    SETTINGS_WINDOW.store(settings as isize, Ordering::Relaxed);
    // SAFETY: called on the thread that owns the window, and the procedure needs no
    // reference data.
    unsafe { SetWindowSubclass(settings, Some(settings_proc), 1, 0) };
    if !ORDINARY_WINDOW.load(Ordering::Relaxed) {
        update_ex_style(settings, |style| style | WS_EX_NOACTIVATE);
    }
}

/// The settings window has closed.
pub fn forget_settings() {
    SETTINGS_WINDOW.store(0, Ordering::Relaxed);
}

/// A window that takes mouse input without becoming the active window, so CSP
/// keeps its shortcuts while the user works the sliders.
pub struct PaletteWindow {
    hwnd: isize,
    /// The window to hand the keyboard back to after typing.
    previous: isize,
}

impl PaletteWindow {
    pub fn new(window: &impl HasWindowHandle) -> Option<Self> {
        let RawWindowHandle::Win32(handle) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        let palette = Self { hwnd: handle.hwnd.get(), previous: 0 };
        // SAFETY: called on the thread that owns the window, and the procedure
        // needs no reference data.
        unsafe {
            SetWindowSubclass(palette.hwnd as HWND, Some(palette_proc), 1, 0);
        }
        update_ex_style(palette.hwnd as HWND, |style| style | WS_EX_NOACTIVATE);
        frame::install(palette.hwnd as HWND, true, palette_shadow_allowed);
        Some(palette)
    }

    /// Takes the window off the screen, and the keyboard back from it if it had it.
    pub fn hide(&self) {
        let hwnd = self.hwnd as HWND;
        CLOAKED.store(true, Ordering::Relaxed);
        frame::set_cloaked(hwnd, true);
        frame::refresh_shadow(hwnd);
        // SAFETY: plain window queries, and stale handles only fail.
        unsafe {
            if GetForegroundWindow() == hwnd {
                let next = next_app_window(hwnd);
                if !next.is_null() {
                    SetForegroundWindow(next);
                }
            }
        }
    }

    /// Puts the window back in front of the normal windows.
    pub fn show(&self) {
        let hwnd = self.hwnd as HWND;
        CLOAKED.store(false, Ordering::Relaxed);
        frame::set_cloaked(hwnd, false);
        // SAFETY: takes no pointers, and a stale handle only fails. While following CSP
        // the window can't be activated, and being topmost is all it needs.
        unsafe {
            SetWindowPos(hwnd, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            if ORDINARY_WINDOW.load(Ordering::Relaxed) {
                SetForegroundWindow(hwnd);
            }
        }
    }

    /// Text fields need the keyboard, so while the user is in CSP a window is activated
    /// only while one has focus, and returns the keyboard to CSP afterwards. `settings` is
    /// the settings window if that is the one, or else it is this one. Returns false if
    /// Windows refused to activate it.
    pub fn set_typing(&mut self, settings: Option<HWND>, typing: bool) -> bool {
        let hwnd = settings.unwrap_or(self.hwnd as HWND);
        let flag = if settings.is_some() { &SETTINGS_TYPING } else { &TYPING };
        flag.store(typing, Ordering::Relaxed);
        if typing {
            update_ex_style(hwnd, |style| style & !WS_EX_NOACTIVATE);
            // SAFETY: plain Win32 calls; a stale HWND only makes them fail.
            unsafe {
                let front = GetForegroundWindow() as isize;
                // Going straight from a field in one window to a field in the other.
                if front != self.hwnd && front != SETTINGS_WINDOW.load(Ordering::Relaxed) {
                    self.previous = front;
                }
                SetForegroundWindow(hwnd) != 0
            }
        } else {
            if !ORDINARY_WINDOW.load(Ordering::Relaxed) {
                update_ex_style(hwnd, |style| style | WS_EX_NOACTIVATE);
                // SAFETY: as above.
                unsafe {
                    if GetForegroundWindow() == hwnd && self.previous != 0 {
                        SetForegroundWindow(self.previous as *mut c_void);
                    }
                }
            }
            frame::refresh_shadow(hwnd);
            true
        }
    }

    /// The top-left corner and outer size in physical pixels, or `None` while minimized,
    /// when Windows reports meaningless values.
    pub fn placement(&self) -> Option<([i32; 2], [i32; 2])> {
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: `rect` is valid for writes, and a stale handle only fails.
        let ok = unsafe { IsIconic(self.hwnd as HWND) == 0 && GetWindowRect(self.hwnd as HWND, &mut rect) != 0 };
        ok.then_some(([rect.left, rect.top], [rect.right - rect.left, rect.bottom - rect.top]))
    }

    /// Gives the settings window its last size, and puts it where it was relative to this
    /// one, or beside this one if it has no such place or that would be off every monitor.
    pub fn place_settings(&self, settings: HWND, offset: Option<[i32; 2]>, size: Option<[i32; 2]>) {
        let mut main = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let mut own = main;
        // SAFETY: an all-zero MONITORINFO is valid, every pointer is to a local that outlives
        // its call, and stale handles only make the calls fail.
        unsafe {
            if let Some([width, height]) = size.filter(|&[width, height]| width > 0 && height > 0) {
                SetWindowPos(settings, ptr::null_mut(), 0, 0, width, height, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
            GetWindowRect(self.hwnd as HWND, &mut main);
            GetWindowRect(settings, &mut own);
            let mut monitor: MONITORINFO = std::mem::zeroed();
            monitor.cbSize = size_of::<MONITORINFO>() as u32;
            let nearest = MonitorFromWindow(self.hwnd as HWND, MONITOR_DEFAULTTONEAREST);
            GetMonitorInfoW(nearest, &mut monitor);
            let remembered = offset.map(|[x, y]| [main.left + x, main.top + y]).filter(|[x, y]| {
                let title_bar = POINT { x: x + TITLE_BAR_PROBE, y: y + TITLE_BAR_PROBE };
                !MonitorFromPoint(title_bar, MONITOR_DEFAULTTONULL).is_null()
            });
            let size = [own.right - own.left, own.bottom - own.top];
            let [x, y] = remembered.unwrap_or_else(|| beside(main, size, monitor.rcWork));
            SetWindowPos(settings, ptr::null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    /// The settings window's top-left corner, where that is relative to this window, and
    /// its size. `None` if either window has gone.
    pub fn settings_placement(&self, settings: HWND) -> Option<([i32; 2], [i32; 2], [i32; 2])> {
        let mut main = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let mut own = main;
        // SAFETY: both pointers are to locals, and stale handles only make the calls fail.
        let ok = unsafe { GetWindowRect(self.hwnd as HWND, &mut main) != 0 && GetWindowRect(settings, &mut own) != 0 };
        ok.then_some((
            [own.left, own.top],
            [own.left - main.left, own.top - main.top],
            [own.right - own.left, own.bottom - own.top],
        ))
    }

    /// Puts the window back where it was, unless that would leave its title bar off every
    /// monitor, as happens when a monitor has been unplugged since it was saved.
    pub fn restore_placement(&self, [x, y]: [i32; 2], size: Option<[i32; 2]>) {
        let title_bar = POINT { x: x + TITLE_BAR_PROBE, y: y + TITLE_BAR_PROBE };
        let hwnd = self.hwnd as HWND;
        // SAFETY: takes no pointers, and a stale handle only fails.
        unsafe {
            if MonitorFromPoint(title_bar, MONITOR_DEFAULTTONULL).is_null() {
                return;
            }
            SetWindowPos(hwnd, ptr::null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            // Separately and afterwards: the move can rescale the window for its monitor,
            // and the saved size is already in that monitor's pixels.
            if let Some([width, height]) = size {
                SetWindowPos(hwnd, ptr::null_mut(), 0, 0, width, height, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
    }

    /// While the user is in CSP, keeps the window above everything without ever taking
    /// the keyboard. Otherwise it acts like any other window. Runs for as long as the app
    /// does, on its own thread, because this has to work while nothing makes the UI repaint.
    pub fn follow_csp(&self) {
        let hwnd = self.hwnd;
        thread::spawn(move || {
            let hwnd = hwnd as HWND;
            // Polling alone would leave CSP covering this window until the next check.
            // SAFETY: the procedure lives for the whole program, and this thread pumps
            // the messages it arrives through.
            unsafe {
                SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_SYSTEM_FOREGROUND,
                    ptr::null_mut(),
                    Some(foreground_changed),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                );
            }
            let mut watch = ForegroundWatch::default();
            let mut topmost = false;
            let mut visible = false;
            let mut last_assert = Instant::now();
            let mut away_since: Option<Instant> = None;
            let mut mode = WindowMode::Ordinary;
            let mut applied_mode = WindowMode::FollowingCsp;
            let mut shown_at = Instant::now();
            loop {
                let waiting_to_leave = away_since.is_some() && mode == WindowMode::FollowingCsp;
                wait_for_foreground_change(if waiting_to_leave {
                    TOPMOST_LOWER_DELAY
                } else {
                    TOPMOST_CHECK_INTERVAL
                });
                // SAFETY: only reads the window's state; a stale handle reads as false.
                let (shown, minimized) = unsafe { (IsWindowVisible(hwnd) != 0, IsIconic(hwnd) != 0) };
                if !shown || minimized || is_cloaked() {
                    visible = false;
                    continue;
                }
                // A window appears at the top of the normal windows, without taking the
                // foreground, so it needs placing the moment it first shows.
                let just_shown = !visible;
                visible = true;
                if just_shown {
                    shown_at = Instant::now();
                }

                // The foreground passes through nothing while switching between CSP's own
                // windows; leaving for that would make this window flicker.
                let front = watch.classify();
                away_since = if front == Front::Other { away_since.or(Some(Instant::now())) } else { None };
                let settled_away = away_since.is_some_and(|t| t.elapsed() >= TOPMOST_LOWER_DELAY);
                mode = next_mode(mode, front, settled_away);

                // winit can still activate a window just after showing it, which only
                // the guards stop, so an ordinary window gives them up a little later.
                let settled_in = shown_at.elapsed() >= ORDINARY_SETTLE_TIME;
                if mode != applied_mode && (mode == WindowMode::FollowingCsp || settled_in) {
                    apply_mode(hwnd, mode);
                    applied_mode = mode;
                }

                let want_top = mode == WindowMode::FollowingCsp;
                match topmost_action(want_top, topmost, last_assert.elapsed(), just_shown) {
                    TopmostAction::None => {}
                    TopmostAction::Raise => {
                        place_window(hwnd, HWND_TOPMOST);
                        topmost = true;
                        last_assert = Instant::now();
                    }
                    TopmostAction::Lower => {
                        place_window(hwnd, HWND_NOTOPMOST);
                        // Leaving the topmost band puts it above all normal windows.
                        tuck_under_foreground(hwnd);
                        topmost = false;
                        last_assert = Instant::now();
                    }
                    TopmostAction::Recheck => {
                        if !is_below_foreground(hwnd) {
                            tuck_under_foreground(hwnd);
                        }
                        last_assert = Instant::now();
                    }
                }
            }
        });
    }

}

fn update_ex_style(hwnd: HWND, change: impl FnOnce(u32) -> u32) {
    // SAFETY: a stale handle only makes these fail.
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, change(style) as isize);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowMode {
    FollowingCsp,
    Ordinary,
}

/// While this window itself is in front the mode stays: it got there either by the user
/// typing in it during CSP work, or by being clicked as an ordinary window.
fn next_mode(mode: WindowMode, front: Front, settled_away: bool) -> WindowMode {
    match front {
        Front::Csp => WindowMode::FollowingCsp,
        Front::Other if settled_away => WindowMode::Ordinary,
        _ => mode,
    }
}

fn apply_mode(hwnd: HWND, mode: WindowMode) {
    let ordinary = mode == WindowMode::Ordinary;
    ORDINARY_WINDOW.store(ordinary, Ordering::Relaxed);
    let settings = SETTINGS_WINDOW.load(Ordering::Relaxed) as HWND;
    for (window, typing) in [(hwnd, &TYPING), (settings, &SETTINGS_TYPING)] {
        if window.is_null() {
            continue;
        }
        if ordinary {
            update_ex_style(window, |style| style & !WS_EX_NOACTIVATE);
        } else if !typing.load(Ordering::Relaxed) {
            update_ex_style(window, |style| style | WS_EX_NOACTIVATE);
        }
        frame::refresh_shadow(window);
    }
}

/// To the right of `main` if there is room on its monitor, or else to its left, and kept
/// on the monitor either way.
fn beside(main: RECT, [width, height]: [i32; 2], work: RECT) -> [i32; 2] {
    let right = main.right + WINDOW_GAP;
    let left = main.left - WINDOW_GAP - width;
    let x = if right + width <= work.right || left < work.left { right } else { left };
    let x = x.min(work.right - width).max(work.left);
    let y = main.top.min(work.bottom - height).max(work.top);
    [x, y]
}

/// The shadow marks the window as really being in use: while typing, or while the user
/// has come to it from somewhere other than CSP.
fn palette_shadow_allowed() -> bool {
    TYPING.load(Ordering::Relaxed) || ORDINARY_WINDOW.load(Ordering::Relaxed)
}

/// Whether the palette window is hidden in the tray.
pub fn is_cloaked() -> bool {
    CLOAKED.load(Ordering::Relaxed)
}

/// The first window below `hwnd` that a user would count as an open window.
fn next_app_window(hwnd: HWND) -> HWND {
    // SAFETY: plain window queries; a window that has vanished ends the walk.
    unsafe {
        let mut next = GetWindow(hwnd, GW_HWNDNEXT);
        while !next.is_null() {
            let tool = GetWindowLongPtrW(next, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0;
            if IsWindowVisible(next) != 0 && IsIconic(next) == 0 && !tool {
                return next;
            }
            next = GetWindow(next, GW_HWNDNEXT);
        }
    }
    ptr::null_mut()
}

/// Finds a window of this process by its title, for the ones eframe opens on its own.
pub fn find_own_window(title: &str) -> Option<HWND> {
    let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
    // SAFETY: `title` is NUL-terminated and outlives the call, and `pid` is valid for writes.
    unsafe {
        let hwnd = FindWindowW(ptr::null(), title.as_ptr());
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        (!hwnd.is_null() && pid == GetCurrentProcessId()).then_some(hwnd)
    }
}

pub fn create_icon(rgba: &[u8], size: i32) -> HICON {
    // Icons take blue first. The alpha channel decides the shape, so the separate
    // 1-bit mask, padded to whole 16-bit words per row, stays empty.
    let bgra: Vec<u8> = rgba
        .chunks_exact(4)
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], pixel[3]])
        .collect();
    let mask_row_bytes = (size as usize).div_ceil(16) * 2;
    let mask = vec![0u8; mask_row_bytes * size as usize];
    // SAFETY: both buffers have the sizes that `size` and the 32-bit format call for,
    // and are only read during the call.
    unsafe { CreateIcon(ptr::null_mut(), size, size, 1, 32, mask.as_ptr(), bgra.as_ptr()) }
}

#[derive(Debug, PartialEq, Eq)]
enum TopmostAction {
    None,
    Raise,
    Lower,
    /// Just appeared on top without being topmost; make sure it is not above the foreground.
    Recheck,
}

/// While wanted, the state is asserted again now and then, because window managers
/// sometimes drop it without any notice.
fn topmost_action(want_top: bool, topmost: bool, since_assert: Duration, just_shown: bool) -> TopmostAction {
    match (want_top, topmost) {
        (true, false) => TopmostAction::Raise,
        (true, true) if since_assert >= TOPMOST_REASSERT_INTERVAL => TopmostAction::Raise,
        (false, true) => TopmostAction::Lower,
        (false, false) if just_shown => TopmostAction::Recheck,
        _ => TopmostAction::None,
    }
}

unsafe extern "system" fn foreground_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    FOREGROUND_CHANGED.store(true, Ordering::Relaxed);
}

/// Sleeps for up to `timeout`, waking early when the foreground window changes.
fn wait_for_foreground_change(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // SAFETY: waits on no handles, and `message` is valid for writes. The events
        // reach `foreground_changed` while this thread dispatches its messages.
        unsafe {
            MsgWaitForMultipleObjects(0, ptr::null(), 0, remaining.as_millis() as u32, QS_ALLINPUT);
            let mut message: MSG = std::mem::zeroed();
            while PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                DispatchMessageW(&message);
            }
        }
        if FOREGROUND_CHANGED.swap(false, Ordering::Relaxed) || Instant::now() >= deadline {
            return;
        }
    }
}

fn is_below_foreground(hwnd: HWND) -> bool {
    // SAFETY: plain window queries; a window that has vanished ends the walk.
    unsafe {
        let front = GetForegroundWindow();
        let mut above = GetWindow(hwnd, GW_HWNDPREV);
        while !above.is_null() {
            if above == front {
                return true;
            }
            above = GetWindow(above, GW_HWNDPREV);
        }
    }
    false
}

/// Puts the window directly behind the foreground window, where it cannot cover it.
/// Some foreground windows must be left alone: going behind a topmost one would make
/// this window topmost too, and going behind the desktop would hide it.
fn tuck_under_foreground(hwnd: HWND) {
    // SAFETY: plain window queries; `class` is valid for its stated length.
    let front = unsafe {
        let front = GetForegroundWindow();
        if front.is_null() || front == hwnd || GetWindowLongPtrW(front, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0 {
            return;
        }
        let mut class = [0u16; 64];
        let len = GetClassNameW(front, class.as_mut_ptr(), class.len() as i32).max(0) as usize;
        if is_desktop_class(&String::from_utf16_lossy(&class[..len])) {
            return;
        }
        front
    };
    place_window(hwnd, front);
}

fn is_desktop_class(class: &str) -> bool {
    matches!(class, "Progman" | "WorkerW")
}

fn place_window(hwnd: HWND, insert_after: HWND) {
    // SAFETY: takes no pointers, and a stale handle only fails. Asynchronous, so a busy
    // UI thread cannot hold this one up.
    unsafe {
        SetWindowPos(
            hwnd,
            insert_after,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
        );
    }
}

/// Tells whether the user is working in CSP. Percepta counts too, because it is
/// only ever in front while the user types into it.
#[derive(Default)]
pub struct ForegroundWatch {
    last_pid: u32,
    last_is_csp: bool,
}

impl ForegroundWatch {
    pub fn csp_in_front(&mut self) -> bool {
        matches!(self.classify(), Front::Csp | Front::Percepta)
    }

    fn classify(&mut self) -> Front {
        let mut pid = 0;
        // SAFETY: GetWindowThreadProcessId only writes to `pid`, and leaves it 0
        // if the window is null or gone.
        unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
        if pid == 0 {
            return Front::Nothing;
        }
        // SAFETY: takes no arguments.
        if pid == unsafe { GetCurrentProcessId() } {
            return Front::Percepta;
        }
        if pid != self.last_pid {
            self.last_pid = pid;
            self.last_is_csp = process_image(pid).is_some_and(|path| is_csp_image(&path));
        }
        if self.last_is_csp {
            Front::Csp
        } else {
            Front::Other
        }
    }
}

/// What the foreground window belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Front {
    Csp,
    Percepta,
    Other,
    /// Passing between windows.
    Nothing,
}

pub fn pen_or_mouse_down() -> bool {
    // SAFETY: takes no pointers. The high bit is set while the button is down.
    unsafe { GetAsyncKeyState(VK_LBUTTON as i32) < 0 }
}

/// After the pen touches Percepta, the tablet driver treats it as the current
/// app and CSP's brush cursor stops following the pen. A real click on CSP's
/// title bar undoes both, while messages posted to CSP do not, so this clicks an
/// empty spot there and puts the cursor back.
pub fn click_csp_title_bar() -> Result<(), &'static str> {
    // SAFETY: plain Win32 queries; a window that has vanished only makes them fail.
    let window = unsafe {
        let mut pid = 0;
        let root = GetAncestor(GetForegroundWindow(), GA_ROOTOWNER);
        GetWindowThreadProcessId(root, &mut pid);
        if pid == 0 || pid == GetCurrentProcessId() || !process_image(pid).is_some_and(|p| is_csp_image(&p)) {
            return Err(CSP_NOT_IN_FRONT);
        }
        root
    };
    click_title_bar(window)
}

fn point_lparam(point: POINT) -> LPARAM {
    ((point.y as u16 as u32) << 16 | point.x as u16 as u32) as LPARAM
}

fn title_bar_spot(window: HWND) -> Result<POINT, &'static str> {
    let spots = title_bar_spots(window);
    let second = USE_SECOND_SPOT.fetch_xor(true, Ordering::Relaxed);
    spots
        .get(second as usize)
        .or(spots.first())
        .copied()
        .ok_or("标题栏没有可点击的空白处")
}

fn click_title_bar(window: HWND) -> Result<(), &'static str> {
    let spot = title_bar_spot(window)?;

    let mut cursor = POINT { x: 0, y: 0 };
    // SAFETY: `cursor` is valid for writes.
    if unsafe { GetCursorPos(&mut cursor) } == 0 {
        return Err("读不到光标位置");
    }
    let events = [
        mouse_event(spot, 0),
        mouse_event(spot, MOUSEEVENTF_LEFTDOWN),
        mouse_event(spot, MOUSEEVENTF_LEFTUP),
        mouse_event(cursor, 0),
    ];
    // SAFETY: the pointer and length describe `events`. SendInput inserts them as
    // one batch, so real pen movement cannot land between the move and the click.
    let sent = unsafe { SendInput(events.len() as u32, events.as_ptr(), size_of::<INPUT>() as i32) };
    if sent as usize == events.len() {
        Ok(())
    } else {
        Err("系统拒绝了模拟点击")
    }
}

/// Up to two title bar points that really are empty caption and not covered.
fn title_bar_spots(window: HWND) -> Vec<POINT> {
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: `rect` is valid for writes.
    if unsafe { GetWindowRect(window, &mut rect) } == 0 {
        return Vec::new();
    }
    let mut spots = Vec::new();
    for y in (rect.top + TITLE_ROW_STEP..rect.top + TITLE_ROW_LIMIT).step_by(TITLE_ROW_STEP as usize) {
        // From the centre leftwards: the window buttons sit at the right end.
        let middle = (rect.left + rect.right) / 2;
        for x in (rect.left..middle).rev().step_by(TITLE_SPOT_STEP as usize) {
            let spot = POINT { x, y };
            if is_empty_caption(window, spot) {
                spots.push(spot);
                if spots.len() == 2 {
                    return spots;
                }
            }
        }
        if !spots.is_empty() {
            break;
        }
    }
    spots
}

fn is_empty_caption(window: HWND, spot: POINT) -> bool {
    let mut hit = 0;
    // SAFETY: `hit` is valid for writes, and the timeout keeps a hung CSP from
    // blocking the UI thread for long.
    unsafe {
        WindowFromPoint(spot) == window
            && SendMessageTimeoutW(
                window,
                WM_NCHITTEST,
                0,
                point_lparam(spot),
                SMTO_ABORTIFHUNG,
                HIT_TEST_TIMEOUT_MS,
                &mut hit,
            ) != 0
            && hit == HTCAPTION as usize
    }
}

fn mouse_event(point: POINT, button: u32) -> INPUT {
    // SAFETY: takes no pointers.
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let (dx, dy) = normalized(point, left, top, width, height);
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | button,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Maps a pixel to SendInput's 0..=65535 virtual desktop range, aiming at the
/// pixel's centre so rounding cannot land on its neighbour.
fn normalized(point: POINT, left: i32, top: i32, width: i32, height: i32) -> (i32, i32) {
    let scale = |value: i32, origin: i32, size: i32| {
        (((value - origin) as i64 * 2 + 1) * 65536 / (size as i64 * 2)) as i32
    };
    (scale(point.x, left, width), scale(point.y, top, height))
}

fn process_image(pid: u32) -> Option<String> {
    let mut buffer = [0u16; 1024];
    let mut len = buffer.len() as u32;
    // SAFETY: `buffer` and `len` outlive the calls and `len` holds its capacity.
    // The handle is closed on every path after a successful open.
    let ok = unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process == ptr::null_mut() {
            return None;
        }
        let ok = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut len);
        CloseHandle(process);
        ok
    };
    (ok != 0).then(|| String::from_utf16_lossy(&buffer[..len as usize]))
}

/// The Simplified Chinese edition ships under the Udongman name.
fn is_csp_image(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    name.contains("clipstudiopaint") || name.contains("udongman")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_open_beside_the_main_window_and_stay_on_its_monitor() {
        let rect = |left, top, right, bottom| RECT { left, top, right, bottom };
        let work = rect(0, 0, 1920, 1040);
        // Room on the right.
        assert_eq!(beside(rect(100, 50, 670, 900), [480, 780], work), [678, 50]);
        // Only room on the left.
        assert_eq!(beside(rect(1300, 50, 1870, 900), [480, 780], work), [812, 50]);
        // Room on neither side, and too low to fit: it stays on the monitor.
        assert_eq!(beside(rect(200, 500, 1700, 1000), [480, 780], work), [1440, 260]);
    }

    #[test]
    fn topmost_follows_the_mode_and_is_reasserted_only_while_wanted() {
        let soon = Duration::ZERO;
        let later = TOPMOST_REASSERT_INTERVAL;
        assert_eq!(topmost_action(true, false, soon, false), TopmostAction::Raise);
        assert_eq!(topmost_action(true, true, soon, false), TopmostAction::None);
        assert_eq!(topmost_action(true, true, later, false), TopmostAction::Raise);
        assert_eq!(topmost_action(false, true, soon, false), TopmostAction::Lower);
        // An ordinary window is left alone, however long it has been.
        assert_eq!(topmost_action(false, false, later, false), TopmostAction::None);
        // Except for its placement when it first shows.
        assert_eq!(topmost_action(false, false, soon, true), TopmostAction::Recheck);
    }

    #[test]
    fn the_mode_follows_the_foreground_and_stays_while_percepta_is_in_front() {
        use WindowMode::{FollowingCsp, Ordinary};
        assert_eq!(next_mode(Ordinary, Front::Csp, false), FollowingCsp);
        assert_eq!(next_mode(FollowingCsp, Front::Other, true), Ordinary);
        // Not until the foreground has settled on another app.
        assert_eq!(next_mode(FollowingCsp, Front::Other, false), FollowingCsp);
        assert_eq!(next_mode(FollowingCsp, Front::Nothing, true), FollowingCsp);
        // Typing during CSP work, or clicked as an ordinary window.
        assert_eq!(next_mode(FollowingCsp, Front::Percepta, false), FollowingCsp);
        assert_eq!(next_mode(Ordinary, Front::Percepta, false), Ordinary);
    }

    #[test]
    fn stays_out_from_behind_the_desktop_only() {
        assert!(is_desktop_class("Progman"));
        assert!(is_desktop_class("WorkerW"));
        assert!(!is_desktop_class("Chrome_WidgetWin_1"));
    }

    #[test]
    fn normalizes_pixels_across_the_virtual_desktop() {
        // Two 1920-wide monitors with the second to the left of the primary.
        let at = |x, y| normalized(POINT { x, y }, -1920, 0, 3840, 1080);
        assert_eq!(at(-1920, 0).0, 8);
        assert_eq!(at(0, 0).0, 32776);
        assert_eq!(at(1919, 0).0, 65527);
        assert_eq!(at(0, 1079).1, 65505);
    }

    #[test]
    fn recognises_csp_by_file_name_only() {
        assert!(is_csp_image(r"C:\Program Files\CELSYS\CLIP STUDIO 1.5\CLIP STUDIO PAINT\CLIPStudioPaint.exe"));
        assert!(is_csp_image(r"D:\Apps\UdongmanPaint.exe"));
        assert!(!is_csp_image(r"C:\Program Files\CELSYS\CLIP STUDIO 1.5\CLIP STUDIO\CLIPStudio.exe"));
        assert!(!is_csp_image(r"D:\clipstudiopaint\notes.exe"));
    }
}
