//! Win32 window behaviour: keeping keyboard focus in CSP, and spotting when CSP is in front.

use std::ffi::c_void;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, VK_LBUTTON,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIcon, DispatchMessageW, GetAncestor, GetClassNameW, GetCursorPos, GetForegroundWindow, GetSystemMetrics,
    GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    MsgWaitForMultipleObjects, PeekMessageW, SendMessageTimeoutW, SendMessageW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, WindowFromPoint, EVENT_SYSTEM_FOREGROUND, GA_ROOTOWNER, GWL_EXSTYLE,
    GW_HWNDPREV, HICON, HTCAPTION, HWND_NOTOPMOST, HWND_TOPMOST, ICON_BIG, ICON_SMALL, MA_NOACTIVATE, MSG,
    PM_REMOVE, QS_ALLINPUT, SMTO_ABORTIFHUNG, SM_CXICON, SM_CXSMICON, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, STYLESTRUCT, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, WINDOWPOS, WINEVENT_OUTOFCONTEXT, WM_DPICHANGED, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE,
    WM_MOUSEACTIVATE, WM_NCHITTEST, WM_SETICON, WM_STYLECHANGING, WM_WINDOWPOSCHANGING, WS_EX_NOACTIVATE,
    WS_EX_TOPMOST,
};

/// Set while a text field holds the keyboard. There is only one palette window.
static TYPING: AtomicBool = AtomicBool::new(false);
/// Set while the window should act like any other, because the user is not in CSP.
static ORDINARY_WINDOW: AtomicBool = AtomicBool::new(false);
/// Whether the user is dragging or resizing the window right now.
static IN_SIZE_MOVE: AtomicBool = AtomicBool::new(false);
/// The scale winit knows about, which lags the real one during a drag.
static APPLIED_DPI: AtomicU32 = AtomicU32::new(0);
/// Set by the foreground event, to wake the thread that keeps the window's level.
static FOREGROUND_CHANGED: AtomicBool = AtomicBool::new(false);
/// Alternates the title bar click position so two quick clicks never count as a
/// double-click, which would maximise CSP.
static USE_SECOND_SPOT: AtomicBool = AtomicBool::new(false);

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
    // A window dragged across monitors of different scales gets resized at the moment
    // most of it crosses. The new size can put most of it back on the old monitor, which
    // changes the scale again, so it flickers and can even refuse to cross. Holding the
    // scale still until the drag ends avoids all of that.
    match message {
        WM_ENTERSIZEMOVE => IN_SIZE_MOVE.store(true, Ordering::Relaxed),
        WM_DPICHANGED if IN_SIZE_MOVE.load(Ordering::Relaxed) => return 0,
        WM_DPICHANGED => APPLIED_DPI.store(wparam as u32 & 0xFFFF, Ordering::Relaxed),
        WM_EXITSIZEMOVE => {
            IN_SIZE_MOVE.store(false, Ordering::Relaxed);
            // SAFETY: forwards the arguments Windows gave us. winit must see the end of
            // the drag first, or it would treat the change as still being dragged.
            let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
            apply_held_dpi(hwnd);
            return result;
        }
        _ => {}
    }
    // winit rewrites the whole extended style without knowing about the topmost state,
    // which `follow_csp` sets behind its back.
    if message == WM_STYLECHANGING && wparam as i32 == GWL_EXSTYLE {
        // SAFETY: for this message Windows passes a valid STYLESTRUCT.
        let style = unsafe { &mut *(lparam as *mut STYLESTRUCT) };
        style.styleNew = (style.styleNew & !WS_EX_TOPMOST) | (style.styleOld & WS_EX_TOPMOST);
    }
    if !TYPING.load(Ordering::Relaxed) && !ORDINARY_WINDOW.load(Ordering::Relaxed) {
        match message {
            WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
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
    // SAFETY: forwards the arguments Windows gave us.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

/// Delivers the scale change that was held back during a drag, if the window ended up
/// on a monitor with a different scale.
fn apply_held_dpi(hwnd: HWND) {
    // SAFETY: only reads the window's state; `rect` is valid for writes.
    let (dpi, rect) = unsafe {
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetWindowRect(hwnd, &mut rect);
        (GetDpiForWindow(hwnd), rect)
    };
    let applied = APPLIED_DPI.swap(dpi, Ordering::Relaxed);
    if dpi == 0 || applied == 0 || dpi == applied {
        return;
    }
    let suggested = scaled_rect(rect, applied, dpi);
    let dpi_pair = (dpi << 16 | dpi) as WPARAM;
    // SAFETY: the message's documented arguments; `suggested` outlives the call.
    unsafe { DefSubclassProc(hwnd, WM_DPICHANGED, dpi_pair, &suggested as *const RECT as LPARAM) };
}

/// The same top-left corner, with the size converted between two scales.
fn scaled_rect(rect: RECT, from_dpi: u32, to_dpi: u32) -> RECT {
    let scale = |length: i32| (length as i64 * to_dpi as i64 / from_dpi as i64) as i32;
    RECT {
        left: rect.left,
        top: rect.top,
        right: rect.left + scale(rect.right - rect.left),
        bottom: rect.top + scale(rect.bottom - rect.top),
    }
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
            APPLIED_DPI.store(GetDpiForWindow(palette.hwnd as HWND), Ordering::Relaxed);
            SetWindowSubclass(palette.hwnd as HWND, Some(palette_proc), 1, 0);
        }
        update_ex_style(palette.hwnd as HWND, |style| style | WS_EX_NOACTIVATE);
        Some(palette)
    }

    /// Text fields need the keyboard, so while the user is in CSP the window is activated
    /// only while one has focus, and returns the keyboard to CSP afterwards. Returns false
    /// if Windows refused to activate it.
    pub fn set_typing(&mut self, typing: bool) -> bool {
        let hwnd = self.hwnd as *mut c_void;
        TYPING.store(typing, Ordering::Relaxed);
        if typing {
            update_ex_style(hwnd, |style| style & !WS_EX_NOACTIVATE);
            // SAFETY: plain Win32 calls; a stale HWND only makes them fail.
            unsafe {
                self.previous = GetForegroundWindow() as isize;
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

    /// Sets the title bar and taskbar icons. eframe can only do this for an active
    /// window, which this one avoids being, so they would stay blank until focused.
    /// `pixels` gives the top-down RGBA image for a square size.
    pub fn set_icon(&self, pixels: impl Fn(usize) -> Vec<u8>) {
        // SAFETY: takes no pointers.
        let sizes = unsafe { [(ICON_BIG, GetSystemMetrics(SM_CXICON)), (ICON_SMALL, GetSystemMetrics(SM_CXSMICON))] };
        for (kind, size) in sizes {
            let icon = create_icon(&pixels(size as usize), size);
            if !icon.is_null() {
                // SAFETY: both handles are valid. The icon stays in use for as long
                // as the window, so it is never destroyed.
                unsafe { SendMessageW(self.hwnd as HWND, WM_SETICON, kind as WPARAM, icon as LPARAM) };
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
                if !shown || minimized {
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
    if ordinary {
        update_ex_style(hwnd, |style| style & !WS_EX_NOACTIVATE);
    } else if !TYPING.load(Ordering::Relaxed) {
        update_ex_style(hwnd, |style| style | WS_EX_NOACTIVATE);
    }
}

fn create_icon(rgba: &[u8], size: i32) -> HICON {
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
            return Err("CSP 不在前台");
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
    fn scaled_rect_keeps_the_corner_and_converts_the_size() {
        let rect = RECT { left: 2000, top: 100, right: 2570, bottom: 1000 };
        let scaled = scaled_rect(rect, 144, 168);
        assert_eq!((scaled.left, scaled.top), (2000, 100));
        assert_eq!((scaled.right - scaled.left, scaled.bottom - scaled.top), (665, 1050));
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
