//! The tray icon, which replaces the taskbar button. It also marks the running instance:
//! a second start and the installer both find it by its window class.

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};
use std::sync::OnceLock;

use eframe::egui;
use windows_sys::core::{w, PCWSTR};
use crate::layout::{Panel, PANELS};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, FindWindowW,
    GetCursorPos, GetForegroundWindow, PostMessageW, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    TrackPopupMenu, HICON, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_LBUTTONDBLCLK,
    WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_POPUP,
};

/// installer/percepta.iss finds the running app by this name.
const WINDOW_CLASS: PCWSTR = w!("PerceptaTray");
const TRAY_MESSAGE: u32 = WM_APP + 1;
/// installer/percepta.iss sends this number to make the running app exit.
const QUIT_MESSAGE: u32 = WM_APP + 2;
const SHOW_MESSAGE: u32 = WM_APP + 3;
/// A window's own menu button, with where to open in `wparam` and `lparam`.
const WINDOW_MENU_MESSAGE: u32 = WM_APP + 4;
/// A module's title strip, likewise.
const FLEXIBLE_MENU_MESSAGE: u32 = WM_APP + 5;
const ICON_ID: u32 = 1;
const ICON_SIZE: i32 = 32;
const MENU_SHOW: usize = 1;
const MENU_SETTINGS: usize = 2;
const MENU_QUIT: usize = 3;
/// Followed by the other panels, in the order of `PANELS`.
const MENU_FIRST_PANEL: usize = 4;
const MENU_FLEXIBLE: usize = 1;

const REQUEST_NONE: u8 = 0;
const REQUEST_SHOW: u8 = 1;
const REQUEST_SETTINGS: u8 = 2;
const REQUEST_QUIT: u8 = 3;
/// Followed by the other panels, in the order of `PANELS`.
const REQUEST_FIRST_PANEL: u8 = 4;
const REQUEST_FLEXIBLE: u8 = 100;

/// The last thing asked for through the tray, until the UI picks it up.
static REQUEST: AtomicU8 = AtomicU8::new(REQUEST_NONE);
static CONTEXT: OnceLock<egui::Context> = OnceLock::new();
static TRAY_WINDOW: AtomicIsize = AtomicIsize::new(0);
/// One bit for each panel on screen, in the order of `PANELS`, for the menu's check marks.
static SHOWN_PANELS: AtomicU8 = AtomicU8::new(0);
/// Set when a menu opened from a window has closed, until the UI has taken note.
static WINDOW_MENU_ENDED: AtomicBool = AtomicBool::new(false);
/// Whether the module that the next flexible menu is for is the flexible one.
static FLEXIBLE_CHECKED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Show,
    Settings,
    Quit,
    TogglePanel(Panel),
    ToggleFlexible,
}

/// Returns what the user asked for since the last call.
pub fn take_request() -> Option<Request> {
    match REQUEST.swap(REQUEST_NONE, Ordering::Relaxed) {
        REQUEST_SHOW => Some(Request::Show),
        REQUEST_SETTINGS => Some(Request::Settings),
        REQUEST_QUIT => Some(Request::Quit),
        REQUEST_FLEXIBLE => Some(Request::ToggleFlexible),
        value => PANELS.get((value as usize).wrapping_sub(REQUEST_FIRST_PANEL as usize)).copied().map(Request::TogglePanel),
    }
}

fn request(value: u8) {
    REQUEST.store(value, Ordering::Relaxed);
    if let Some(ctx) = CONTEXT.get() {
        ctx.request_repaint();
    }
}

/// Says which panels are on screen, for the next time the menu opens.
pub fn set_shown_panels(shown: impl Fn(Panel) -> bool) {
    let bits = PANELS.iter().enumerate().filter(|(_, &panel)| shown(panel)).fold(0, |bits, (index, _)| bits | 1 << index);
    SHOWN_PANELS.store(bits, Ordering::Relaxed);
}

/// Opens the same menu as the tray's, from a window's own button, with its corner at this
/// point in physical pixels. Not right here: the menu runs its own message loop, which
/// belongs outside of the frame being drawn.
pub fn open_menu([x, y]: [i32; 2]) {
    let hwnd = TRAY_WINDOW.load(Ordering::Relaxed) as HWND;
    // SAFETY: takes no pointers, and a missing window only makes it fail.
    unsafe { PostMessageW(hwnd, WINDOW_MENU_MESSAGE, x as isize as WPARAM, y as LPARAM) };
}

/// Opens the menu of a module's title strip at this point, in the same way as `open_menu`.
pub fn open_flexible_menu([x, y]: [i32; 2], checked: bool) {
    FLEXIBLE_CHECKED.store(checked, Ordering::Relaxed);
    let hwnd = TRAY_WINDOW.load(Ordering::Relaxed) as HWND;
    // SAFETY: takes no pointers, and a missing window only makes it fail.
    unsafe { PostMessageW(hwnd, FLEXIBLE_MENU_MESSAGE, x as isize as WPARAM, y as LPARAM) };
}

/// Whether a menu opened from a window has closed since the last call.
pub fn take_menu_end() -> bool {
    WINDOW_MENU_ENDED.swap(false, Ordering::Relaxed)
}

/// If Percepta is already running, brings it up and returns true.
pub fn show_running_instance() -> bool {
    // SAFETY: the class name is a static string, and a null title matches any.
    unsafe {
        let running = FindWindowW(WINDOW_CLASS, ptr::null());
        if running.is_null() {
            return false;
        }
        PostMessageW(running, SHOW_MESSAGE, 0, 0);
        true
    }
}

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
}

impl Tray {
    /// Must be created and dropped on the UI thread, whose message loop serves the icon.
    pub fn new(ctx: &egui::Context, pixels: impl Fn(usize) -> Vec<u8>) -> Option<Self> {
        let _ = CONTEXT.set(ctx.clone());
        // SAFETY: the class name is static, and the struct is only read during the call.
        let hwnd = unsafe {
            let instance = GetModuleHandleW(ptr::null());
            let mut info: WNDCLASSW = std::mem::zeroed();
            info.lpfnWndProc = Some(tray_proc);
            info.hInstance = instance;
            info.lpszClassName = WINDOW_CLASS;
            RegisterClassW(&info);
            // An ordinary top-level window that is never shown, as a message-only one
            // could not be found by the name.
            CreateWindowExW(
                0,
                WINDOW_CLASS,
                w!("Percepta"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                instance,
                ptr::null(),
            )
        };
        if hwnd.is_null() {
            return None;
        }
        TRAY_WINDOW.store(hwnd as isize, Ordering::Relaxed);
        let icon = crate::platform::create_icon(&pixels(ICON_SIZE as usize), ICON_SIZE);
        ICON.store(icon as usize, Ordering::Relaxed);
        add_icon(hwnd, icon);
        Some(Self { hwnd, icon })
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let data = icon_data(self.hwnd, self.icon);
        // SAFETY: `data` is valid for the call, and the window is this struct's own.
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &data);
            DestroyWindow(self.hwnd);
        }
    }
}

/// For adding the icon back after Explorer restarts.
static ICON: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn icon_data(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
    // SAFETY: an all-zero NOTIFYICONDATAW is valid.
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = ICON_ID;
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = TRAY_MESSAGE;
    data.hIcon = icon;
    for (slot, unit) in data.szTip.iter_mut().zip("Percepta".encode_utf16()) {
        *slot = unit;
    }
    data
}

fn add_icon(hwnd: HWND, icon: HICON) {
    let data = icon_data(hwnd, icon);
    // SAFETY: `data` is valid for the call.
    unsafe { Shell_NotifyIconW(NIM_ADD, &data) };
}

/// Opens the menu at the cursor, or at `at` for a window's own button.
fn show_menu(hwnd: HWND, at: Option<POINT>) {
    // Tray menus only close properly when clicking away if their window is in front, and
    // that must be undone afterwards or CSP would lose the keyboard.
    // SAFETY: plain Win32 calls on a menu made here and destroyed before returning.
    unsafe {
        let previous = GetForegroundWindow();
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, w!("窗口"));
        let shown = SHOWN_PANELS.load(Ordering::Relaxed);
        for (index, name) in [w!("颜色"), w!("色环"), w!("滑块")].into_iter().enumerate() {
            let check = if shown & 1 << index != 0 { MF_CHECKED } else { 0 };
            AppendMenuW(menu, MF_STRING | check, MENU_FIRST_PANEL + index, name);
        }
        AppendMenuW(menu, MF_STRING, MENU_SHOW, w!("显示全部"));
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_SETTINGS, w!("设置"));
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("退出 Percepta"));
        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        let point = at.unwrap_or(cursor);
        SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            hwnd,
            ptr::null(),
        ) as usize;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        if !previous.is_null() {
            SetForegroundWindow(previous);
        }
        if at.is_some() {
            WINDOW_MENU_ENDED.store(true, Ordering::Relaxed);
        }
        match chosen {
            0 => request(REQUEST_NONE),
            MENU_SHOW => request(REQUEST_SHOW),
            MENU_SETTINGS => request(REQUEST_SETTINGS),
            MENU_QUIT => request(REQUEST_QUIT),
            panel => request(REQUEST_FIRST_PANEL + (panel - MENU_FIRST_PANEL) as u8),
        }
    }
}

fn show_flexible_menu(hwnd: HWND, point: POINT) {
    // SAFETY: as in `show_menu`.
    unsafe {
        let previous = GetForegroundWindow();
        let menu = CreatePopupMenu();
        let check = if FLEXIBLE_CHECKED.load(Ordering::Relaxed) { MF_CHECKED } else { 0 };
        AppendMenuW(menu, MF_STRING | check, MENU_FLEXIBLE, w!("随窗口伸缩"));
        SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, point.x, point.y, 0, hwnd, ptr::null()) as usize;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        if !previous.is_null() {
            SetForegroundWindow(previous);
        }
        WINDOW_MENU_ENDED.store(true, Ordering::Relaxed);
        if chosen == MENU_FLEXIBLE {
            request(REQUEST_FLEXIBLE);
        }
    }
}

unsafe extern "system" fn tray_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
    // SAFETY: the name is a static string.
    let taskbar_created = *TASKBAR_CREATED.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) });
    match message {
        TRAY_MESSAGE => match lparam as u32 {
            WM_LBUTTONDBLCLK => request(REQUEST_SHOW),
            WM_RBUTTONUP => show_menu(hwnd, None),
            _ => {}
        },
        QUIT_MESSAGE => request(REQUEST_QUIT),
        SHOW_MESSAGE => request(REQUEST_SHOW),
        WINDOW_MENU_MESSAGE => show_menu(hwnd, Some(POINT { x: wparam as i32, y: lparam as i32 })),
        FLEXIBLE_MENU_MESSAGE => show_flexible_menu(hwnd, POINT { x: wparam as i32, y: lparam as i32 }),
        _ if message == taskbar_created => add_icon(hwnd, ICON.load(Ordering::Relaxed) as HICON),
        _ => {}
    }
    // SAFETY: forwards the arguments Windows gave us.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
