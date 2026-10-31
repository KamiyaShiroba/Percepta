//! The tray icon, which replaces the taskbar button. It also marks the running instance:
//! a second start and the installer both find it by its window class.

use std::ptr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

use eframe::egui;
use windows_sys::core::{w, PCWSTR};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, FindWindowW,
    GetCursorPos, GetForegroundWindow, PostMessageW, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    TrackPopupMenu, HICON, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_LBUTTONDBLCLK,
    WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_POPUP,
};

/// installer/percepta.iss finds the running app by this name.
const WINDOW_CLASS: PCWSTR = w!("PerceptaTray");
const TRAY_MESSAGE: u32 = WM_APP + 1;
/// installer/percepta.iss sends this number to make the running app exit.
const QUIT_MESSAGE: u32 = WM_APP + 2;
const SHOW_MESSAGE: u32 = WM_APP + 3;
const ICON_ID: u32 = 1;
const ICON_SIZE: i32 = 32;
const MENU_SHOW: usize = 1;
const MENU_SETTINGS: usize = 2;
const MENU_QUIT: usize = 3;

const REQUEST_NONE: u8 = 0;
const REQUEST_SHOW: u8 = 1;
const REQUEST_SETTINGS: u8 = 2;
const REQUEST_QUIT: u8 = 3;

/// The last thing asked for through the tray, until the UI picks it up.
static REQUEST: AtomicU8 = AtomicU8::new(REQUEST_NONE);
static CONTEXT: OnceLock<egui::Context> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Show,
    Settings,
    Quit,
}

/// Returns what the user asked for since the last call.
pub fn take_request() -> Option<Request> {
    match REQUEST.swap(REQUEST_NONE, Ordering::Relaxed) {
        REQUEST_SHOW => Some(Request::Show),
        REQUEST_SETTINGS => Some(Request::Settings),
        REQUEST_QUIT => Some(Request::Quit),
        _ => None,
    }
}

fn request(value: u8) {
    REQUEST.store(value, Ordering::Relaxed);
    if let Some(ctx) = CONTEXT.get() {
        ctx.request_repaint();
    }
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

fn show_menu(hwnd: HWND) {
    // Tray menus only close properly when clicking away if their window is in front, and
    // that must be undone afterwards or CSP would lose the keyboard.
    // SAFETY: plain Win32 calls on a menu made here and destroyed before returning.
    unsafe {
        let previous = GetForegroundWindow();
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, MENU_SHOW, w!("显示窗口"));
        AppendMenuW(menu, MF_STRING, MENU_SETTINGS, w!("设置"));
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("退出 Percepta"));
        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            cursor.x,
            cursor.y,
            0,
            hwnd,
            ptr::null(),
        ) as usize;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        if !previous.is_null() {
            SetForegroundWindow(previous);
        }
        match chosen {
            MENU_SHOW => request(REQUEST_SHOW),
            MENU_SETTINGS => request(REQUEST_SETTINGS),
            MENU_QUIT => request(REQUEST_QUIT),
            _ => {}
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
            WM_RBUTTONUP => show_menu(hwnd),
            _ => {}
        },
        QUIT_MESSAGE => request(REQUEST_QUIT),
        SHOW_MESSAGE => request(REQUEST_SHOW),
        _ if message == taskbar_created => add_icon(hwnd, ICON.load(Ordering::Relaxed) as HICON),
        _ => {}
    }
    // SAFETY: forwards the arguments Windows gave us.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
