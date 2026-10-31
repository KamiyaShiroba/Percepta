//! The self-drawn window frame: where the window can be dragged and resized, its rounded
//! corners, and the shadow it casts while active. The title bar itself is painted by egui.

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Once;

use windows_sys::core::{w, BOOL};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CLOAK, DWMWA_CLOAKED, DWMWA_TRANSITIONS_FORCEDISABLED,
};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection, CreateRoundRectRgn, DeleteDC, DeleteObject,
    GetDC, InvalidateRect, ReleaseDC, SelectObject, SetWindowRgn,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetWindowLongPtrW, GetWindowRect, IsWindowVisible, RegisterClassW,
    SendMessageW, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, UpdateLayeredWindow, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTLEFT, HTRIGHT, HTTOP,
    GWL_EXSTYLE, HTTOPLEFT, HTTOPRIGHT, HWND_TOPMOST, LWA_ALPHA, SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SW_HIDE, ULW_ALPHA, WM_ACTIVATE, WM_APP, WM_DPICHANGED, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_MOUSEMOVE, WM_NCDESTROY,
    WM_MOVING, WM_NCHITTEST, WM_SIZING, WM_WINDOWPOSCHANGED, WM_WINDOWPOSCHANGING, WINDOWPOS, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TRANSPARENT, WS_POPUP,
};

/// All of these are at 100% scale.
pub const TITLE_BAR_HEIGHT: f32 = 20.0;
/// How far the pen has to go before pressing on something becomes dragging it.
pub const DRAG_THRESHOLD: f32 = 8.0;
pub const TITLE_BUTTON_WIDTH: f32 = 28.0;
pub const CORNER_RADIUS: f32 = 2.0;
const RESIZE_BORDER: f32 = 4.0;
/// Like the CSS `box-shadow: 1px 1px 2px 0`: towards the bottom right, with a soft edge
/// that runs from this far inside the shadow's outline to this far outside it.
const SHADOW_OFFSET: f32 = 1.0;
const SHADOW_BLUR: f32 = 2.0;
/// How dark the shadow is where it is solid.
const SHADOW_ALPHA: f32 = 100.0;
const BASE_DPI: f32 = 96.0;
const SUBCLASS_ID: usize = 2;
/// Not among the window messages that windows-sys lists with the others.
const WM_MOUSELEAVE: u32 = 0x02A3;
/// Passes `caption_left` to the window's frame, in `wparam`.
const WM_SET_CAPTION_LEFT: u32 = WM_APP + 16;
/// How opaque a window is while it is being dragged.
const DRAGGED_ALPHA: u8 = 170;
const DROP_FILL: u32 = premultiplied([255, 40, 40], 130);
const DROP_EDGE: u32 = premultiplied([110, 110, 110], 230);

/// Set when a drag of a window's frame ends, until the UI has taken note.
static DRAG_ENDED: AtomicBool = AtomicBool::new(false);
/// The window whose title bar is being dragged right now.
static MOVING: AtomicIsize = AtomicIsize::new(0);
/// The last window to have been moved by its title bar, until the UI has taken note.
static MOVE_ENDED: AtomicIsize = AtomicIsize::new(0);
/// The window to repaint while another one moves, to show where that one would land.
static REPAINT_ON_MOVE: AtomicIsize = AtomicIsize::new(0);

/// What a point on the window does, other than reaching the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Zone {
    Client,
    Caption,
    Resize(u32),
}

/// `caption_left` is where the part of the title bar that drags the window begins, at 100%
/// scale. Anything to its left is for the UI, such as tabs.
fn zone_at(x: f32, y: f32, width: f32, height: f32, scale: f32, resizable: bool, caption_left: f32) -> Zone {
    let border = RESIZE_BORDER * scale;
    if resizable {
        let (left, right) = (x < border, x >= width - border);
        let (top, bottom) = (y < border, y >= height - border);
        match (left, right, top, bottom) {
            (true, _, true, _) => return Zone::Resize(HTTOPLEFT),
            (_, true, true, _) => return Zone::Resize(HTTOPRIGHT),
            (true, _, _, true) => return Zone::Resize(HTBOTTOMLEFT),
            (_, true, _, true) => return Zone::Resize(HTBOTTOMRIGHT),
            (true, ..) => return Zone::Resize(HTLEFT),
            (_, true, ..) => return Zone::Resize(HTRIGHT),
            (_, _, true, _) => return Zone::Resize(HTTOP),
            (_, _, _, true) => return Zone::Resize(HTBOTTOM),
            _ => {}
        }
    }
    if y < TITLE_BAR_HEIGHT * scale && x >= caption_left * scale && x < width - TITLE_BUTTON_WIDTH * scale {
        Zone::Caption
    } else {
        Zone::Client
    }
}

struct FrameState {
    shadow: HWND,
    /// Asked on top of the window being active.
    shadow_allowed: fn(HWND) -> bool,
    resizable: bool,
    /// The window size the region was made for.
    region_size: (i32, i32),
    /// The window size the shadow was drawn for.
    shadow_size: (i32, i32),
    active: bool,
    /// Whether the user is dragging or resizing the window right now.
    in_size_move: bool,
    /// The same, and it is the size that is being changed, by an edge.
    sizing: bool,
    /// The same, and it has really moved.
    moved: bool,
    /// Where the window and the cursor were when the drag began.
    move_start: RECT,
    cursor_start: POINT,
    caption_left: f32,
    /// The scale winit knows about, which lags the real one during a drag.
    applied_dpi: u32,
}

/// Gives a borderless window its frame. Lasts until the window is destroyed.
pub fn install(hwnd: HWND, resizable: bool, shadow_allowed: fn(HWND) -> bool) {
    let state = Box::new(FrameState {
        shadow: create_shadow_window(),
        shadow_allowed,
        resizable,
        region_size: (0, 0),
        shadow_size: (0, 0),
        active: false,
        in_size_move: false,
        sizing: false,
        moved: false,
        move_start: RECT { left: 0, top: 0, right: 0, bottom: 0 },
        cursor_start: POINT { x: 0, y: 0 },
        caption_left: 0.0,
        // SAFETY: takes no pointers, and a stale handle reads as 0.
        applied_dpi: unsafe { GetDpiForWindow(hwnd) },
    });
    let state = Box::into_raw(state);
    let no_transitions: BOOL = 1;
    // SAFETY: called on the thread that owns the window. The state is freed by the
    // procedure when the window is destroyed.
    unsafe {
        // Windows appear and go at once, like CSP's, without fading or zooming.
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED as u32,
            (&raw const no_transitions).cast(),
            size_of::<BOOL>() as u32,
        );
        SetWindowSubclass(hwnd, Some(frame_proc), SUBCLASS_ID, state as usize);
        update_region(hwnd, &mut *state);
    }
}

/// Whether the title bar or an edge of a window was let go of since the last call. Windows
/// runs a drag of the frame itself, so egui never sees its press or release.
pub fn take_drag_end() -> bool {
    DRAG_ENDED.swap(false, Ordering::Relaxed)
}

/// The window whose title bar is being dragged right now, if any.
pub fn moving_window() -> isize {
    MOVING.load(Ordering::Relaxed)
}

/// The last window to have been moved by its title bar since the last call.
pub fn take_move_end() -> isize {
    MOVE_ENDED.swap(0, Ordering::Relaxed)
}

/// Asks for `hwnd` to be repainted while any window is moving.
pub fn repaint_on_move(hwnd: HWND) {
    REPAINT_ON_MOVE.store(hwnd as isize, Ordering::Relaxed);
}

/// Where a window is, in physical pixels.
pub fn window_origin(hwnd: HWND) -> [i32; 2] {
    let rect = window_rect(hwnd);
    [rect.left, rect.top]
}

/// Leaves the title bar to the left of `caption_left` to the UI.
pub fn set_caption_left(hwnd: HWND, caption_left: f32) {
    // SAFETY: a message that `frame_proc` handles, and a stale handle only fails.
    unsafe { SendMessageW(hwnd, WM_SET_CAPTION_LEFT, caption_left.ceil() as WPARAM, 0) };
}

/// Hides or shows a window on screen, while Windows and eframe still count it as visible.
/// eframe stops running the UI of a window that is really hidden, and the UI must keep
/// going for the tray menu to reach it.
pub fn set_cloaked(hwnd: HWND, cloaked: bool) {
    let value: BOOL = cloaked as BOOL;
    // SAFETY: the pointer and size describe `value`, and a stale handle only fails.
    unsafe { DwmSetWindowAttribute(hwnd, DWMWA_CLOAK as u32, (&raw const value).cast(), size_of::<BOOL>() as u32) };
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut value = 0u32;
    // SAFETY: the pointer and size describe `value`, which stays 0 if the call fails.
    unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, (&raw mut value).cast(), size_of::<u32>() as u32) };
    value != 0
}

fn scale_of(hwnd: HWND) -> f32 {
    // SAFETY: takes no pointers, and a stale handle reads as 0.
    (unsafe { GetDpiForWindow(hwnd) } as f32 / BASE_DPI).max(1.0)
}

fn cursor_position() -> POINT {
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: `point` is valid for writes.
    unsafe { GetCursorPos(&mut point) };
    point
}

fn window_rect(hwnd: HWND) -> RECT {
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: `rect` is valid for writes, and a stale handle only fails.
    unsafe { GetWindowRect(hwnd, &mut rect) };
    rect
}

unsafe extern "system" fn frame_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: `data` is the box from `install`, freed only below, and all of this runs
    // on the window's own thread.
    let state = unsafe { &mut *(data as *mut FrameState) };
    match message {
        // A window dragged across monitors of different scales gets resized at the moment
        // most of it crosses. The new size can put most of it back on the old monitor, which
        // changes the scale again, so it flickers and can even refuse to cross. Holding the
        // scale still until the drag ends avoids all of that. Windows resizes the window
        // even if it isn't told about the change, so the size has to be held as well.
        WM_ENTERSIZEMOVE => {
            state.in_size_move = true;
            state.move_start = window_rect(hwnd);
            state.cursor_start = cursor_position();
        }
        WM_SET_CAPTION_LEFT => {
            state.caption_left = wparam as f32;
            return 0;
        }
        WM_SIZING => state.sizing = true,
        WM_MOVING => {
            if !state.moved {
                // SAFETY: for this message Windows passes a valid RECT, which it then uses.
                let rect = unsafe { &mut *(lparam as *mut RECT) };
                let threshold = (DRAG_THRESHOLD * scale_of(hwnd)) as i32;
                let start = state.move_start;
                // Measured from where the drag began. Windows adds each step to the rectangle
                // it gets back, so what it offers here is no measure of the whole way.
                let cursor = cursor_position();
                let (dx, dy) = (cursor.x - state.cursor_start.x, cursor.y - state.cursor_start.y);
                if dx.abs() < threshold && dy.abs() < threshold {
                    *rect = start;
                    return 1;
                }
                *rect = RECT {
                    left: start.left + dx,
                    top: start.top + dy,
                    right: start.right + dx,
                    bottom: start.bottom + dy,
                };
                state.moved = true;
                set_translucent(hwnd, true);
            }
            MOVING.store(hwnd as isize, Ordering::Relaxed);
            let repaint = REPAINT_ON_MOVE.load(Ordering::Relaxed) as HWND;
            // SAFETY: a null rectangle means the whole window, and a stale handle only fails.
            unsafe { InvalidateRect(repaint, ptr::null(), 0) };
        }
        WM_WINDOWPOSCHANGING if state.in_size_move && !state.sizing => {
            // SAFETY: for this message Windows passes a valid WINDOWPOS.
            unsafe { (*(lparam as *mut WINDOWPOS)).flags |= SWP_NOSIZE };
        }
        WM_DPICHANGED if state.in_size_move => return 0,
        WM_DPICHANGED => state.applied_dpi = wparam as u32 & 0xFFFF,
        WM_EXITSIZEMOVE => {
            state.in_size_move = false;
            state.sizing = false;
            MOVING.store(0, Ordering::Relaxed);
            if std::mem::take(&mut state.moved) {
                set_translucent(hwnd, false);
                MOVE_ENDED.store(hwnd as isize, Ordering::Relaxed);
            }
            // SAFETY: forwards the arguments Windows gave us. winit must see the end of
            // the drag first, or it would treat the change as still being dragged.
            let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
            apply_held_dpi(hwnd, state);
            DRAG_ENDED.store(true, Ordering::Relaxed);
            // SAFETY: a null rectangle means the whole window. The UI may be idle, and this
            // makes it run a frame to notice.
            unsafe { InvalidateRect(hwnd, ptr::null(), 0) };
            return result;
        }
        WM_NCHITTEST => {
            let rect = window_rect(hwnd);
            let x = (lparam & 0xFFFF) as i16 as i32 - rect.left;
            let y = ((lparam >> 16) & 0xFFFF) as i16 as i32 - rect.top;
            let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
            let zone = zone_at(
                x as f32,
                y as f32,
                width as f32,
                height as f32,
                scale_of(hwnd),
                state.resizable,
                state.caption_left,
            );
            match zone {
                Zone::Caption => return HTCAPTION as LRESULT,
                Zone::Resize(hit) => return hit as LRESULT,
                Zone::Client => {}
            }
        }
        WM_MOUSELEAVE => {
            // winit only reports a move to a new position, and egui ignores a press until it
            // has seen one after the cursor left. A pen tapping the same pixel again would
            // do nothing, so make winit forget where the cursor was.
            // SAFETY: a message winit handles like any other move, with made-up coordinates.
            unsafe { DefSubclassProc(hwnd, WM_MOUSEMOVE, 0, -1) };
        }
        WM_ACTIVATE => {
            state.active = wparam & 0xFFFF != 0;
            update_shadow(hwnd, state);
        }
        WM_WINDOWPOSCHANGED => {
            update_region(hwnd, state);
            update_shadow(hwnd, state);
        }
        WM_NCDESTROY => {
            // SAFETY: the shadow belongs to this window, and nothing uses the state after
            // this, as the subclass is removed first.
            unsafe {
                DestroyWindow(state.shadow);
                RemoveWindowSubclass(hwnd, Some(frame_proc), SUBCLASS_ID);
                let result = DefSubclassProc(hwnd, message, wparam, lparam);
                drop(Box::from_raw(data as *mut FrameState));
                return result;
            }
        }
        _ => {}
    }
    // SAFETY: forwards the arguments Windows gave us.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

/// Lets what is below show through a window while it is being dragged.
fn set_translucent(hwnd: HWND, translucent: bool) {
    // SAFETY: takes no pointers, and a stale handle only fails.
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if translucent {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (style | WS_EX_LAYERED) as isize);
            SetLayeredWindowAttributes(hwnd, 0, DRAGGED_ALPHA, LWA_ALPHA);
        } else {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (style & !WS_EX_LAYERED) as isize);
        }
    }
}

/// Delivers the scale change that was held back during a drag, if the window ended up
/// on a monitor with a different scale.
fn apply_held_dpi(hwnd: HWND, state: &mut FrameState) {
    let rect = window_rect(hwnd);
    // SAFETY: takes no pointers, and a stale handle reads as 0.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let applied = std::mem::replace(&mut state.applied_dpi, dpi);
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

fn update_region(hwnd: HWND, state: &mut FrameState) {
    let rect = window_rect(hwnd);
    let size = (rect.right - rect.left, rect.bottom - rect.top);
    if size == state.region_size || size.0 <= 0 || size.1 <= 0 {
        return;
    }
    state.region_size = size;
    let diameter = (CORNER_RADIUS * 2.0 * scale_of(hwnd)).round() as i32;
    // SAFETY: takes no pointers. On success Windows owns the region, so it is not freed.
    unsafe {
        let region = CreateRoundRectRgn(0, 0, size.0 + 1, size.1 + 1, diameter, diameter);
        SetWindowRgn(hwnd, region, 1);
    }
}

/// Looks again at whether the shadow should show, after a change that sends no message of
/// its own. Asynchronous, so that any thread can call this without waiting for the UI.
pub fn refresh_shadow(hwnd: HWND) {
    // SAFETY: a no-op position change, which arrives back at `frame_proc` as a message.
    unsafe {
        SetWindowPos(
            hwnd,
            ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_ASYNCWINDOWPOS,
        );
    }
}

fn update_shadow(hwnd: HWND, state: &mut FrameState) {
    // SAFETY: takes no pointers, and a stale handle reads as not visible.
    let shown = state.active && (state.shadow_allowed)(hwnd) && unsafe { IsWindowVisible(hwnd) } != 0 && !is_cloaked(hwnd);
    if !shown {
        // SAFETY: a stale handle only fails.
        unsafe { ShowWindow(state.shadow, SW_HIDE) };
        return;
    }
    let rect = window_rect(hwnd);
    let size = (rect.right - rect.left, rect.bottom - rect.top);
    let margin = ((SHADOW_OFFSET + SHADOW_BLUR) * scale_of(hwnd)).ceil() as i32;
    if size != state.shadow_size {
        state.shadow_size = size;
        draw_shadow(state.shadow, rect, margin, scale_of(hwnd));
    }
    // Directly below the window, so it also follows it into and out of the topmost band.
    // SAFETY: takes no pointers, and stale handles only fail.
    unsafe {
        SetWindowPos(
            state.shadow,
            hwnd,
            rect.left - margin,
            rect.top - margin,
            size.0 + margin * 2,
            size.1 + margin * 2,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

fn create_shadow_window() -> HWND {
    unsafe extern "system" fn shadow_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // SAFETY: forwards the arguments Windows gave us.
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }
    static REGISTER: Once = Once::new();
    let class = w!("PerceptaShadow");
    // SAFETY: the class name is a static string, and the struct is only read during the call.
    unsafe {
        let instance = GetModuleHandleW(ptr::null());
        REGISTER.call_once(|| {
            let mut info: WNDCLASSW = std::mem::zeroed();
            info.lpfnWndProc = Some(shadow_proc);
            info.hInstance = instance;
            info.lpszClassName = class;
            RegisterClassW(&info);
        });
        // Transparent to the mouse, so that clicks around the window reach what is below.
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class,
            ptr::null(),
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
    }
}

/// How opaque the shadow is `distance` pixels outside its outline, negative being inside.
fn shadow_alpha(distance: f32, blur: f32) -> u8 {
    let t = ((blur - distance) / (blur * 2.0)).clamp(0.0, 1.0);
    (SHADOW_ALPHA * t * t * (3.0 - 2.0 * t)) as u8
}

fn draw_shadow(shadow: HWND, window: RECT, margin: i32, scale: f32) {
    let (window_width, window_height) = (window.right - window.left, window.bottom - window.top);
    let (width, height) = (window_width + margin * 2, window_height + margin * 2);
    let radius = CORNER_RADIUS * scale;
    let half = (window_width as f32 / 2.0 - radius, window_height as f32 / 2.0 - radius);
    let offset = SHADOW_OFFSET * scale;
    let blur = SHADOW_BLUR * scale;
    let center = (width as f32 / 2.0 + offset, height as f32 / 2.0 + offset);
    let position = POINT { x: window.left - margin, y: window.top - margin };
    draw_layered(shadow, position, width, height, |column, row| {
        let dx = (column as f32 + 0.5 - center.0).abs() - half.0;
        let dy = (row as f32 + 0.5 - center.1).abs() - half.1;
        let distance = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - radius;
        // Black, so premultiplying leaves only the alpha.
        (shadow_alpha(distance, blur) as u32) << 24
    });
}

/// Fills a layered window with premultiplied pixels, given by column and row.
fn draw_layered(window: HWND, position: POINT, width: i32, height: i32, pixel: impl Fn(i32, i32) -> u32) {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = width;
    // Negative for top-down rows.
    info.bmiHeader.biHeight = -height;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;

    // SAFETY: the DIB's buffer holds `width * height` 32-bit pixels as requested in `info`,
    // and every GDI object is released before returning.
    unsafe {
        let dc = CreateCompatibleDC(ptr::null_mut());
        let mut bits = ptr::null_mut();
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0);
        if bitmap.is_null() || bits.is_null() {
            DeleteDC(dc);
            return;
        }
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (width * height) as usize);
        for row in 0..height {
            for column in 0..width {
                pixels[(row * width + column) as usize] = pixel(column, row);
            }
        }
        let previous = SelectObject(dc, bitmap);
        let origin = POINT { x: 0, y: 0 };
        let size = SIZE { cx: width, cy: height };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        UpdateLayeredWindow(window, ptr::null_mut(), &position, &size, dc, &origin, 0, &blend, ULW_ALPHA);
        SelectObject(dc, previous);
        DeleteObject(bitmap);
        DeleteDC(dc);
    }
}

/// What follows the pen while a panel is being dragged: a see-through picture of it, so that
/// nothing changes in the layout until the pen lifts. Transparent to the mouse, like the shadow.
pub struct DragGhost {
    window: HWND,
}

impl DragGhost {
    pub fn new() -> Self {
        Self { window: create_shadow_window() }
    }

    /// Takes its picture of `rect`, from the screen, in physical pixels, and shows it there.
    pub fn show(&self, rect: RECT) {
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        // SAFETY: every GDI object is released before returning, and a stale handle only fails.
        unsafe {
            let screen = GetDC(ptr::null_mut());
            let dc = CreateCompatibleDC(screen);
            let bitmap = CreateCompatibleBitmap(screen, width, height);
            let previous = SelectObject(dc, bitmap);
            BitBlt(dc, 0, 0, width, height, screen, rect.left, rect.top, SRCCOPY);
            let origin = POINT { x: 0, y: 0 };
            let position = POINT { x: rect.left, y: rect.top };
            let size = SIZE { cx: width, cy: height };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: DRAGGED_ALPHA,
                AlphaFormat: 0,
            };
            UpdateLayeredWindow(self.window, ptr::null_mut(), &position, &size, dc, &origin, 0, &blend, ULW_ALPHA);
            SelectObject(dc, previous);
            DeleteObject(bitmap);
            DeleteDC(dc);
            ReleaseDC(ptr::null_mut(), screen);
        }
        self.move_to([rect.left, rect.top]);
    }

    pub fn move_to(&self, [x, y]: [i32; 2]) {
        // SAFETY: takes no pointers, and a stale handle only fails.
        unsafe { SetWindowPos(self.window, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW) };
    }

    pub fn hide(&self) {
        // SAFETY: a stale handle only fails.
        unsafe { ShowWindow(self.window, SW_HIDE) };
    }
}

/// `color` at `alpha`, as a layered window wants it.
const fn premultiplied([r, g, b]: [u32; 3], alpha: u32) -> u32 {
    alpha << 24 | (r * alpha / 255) << 16 | (g * alpha / 255) << 8 | b * alpha / 255
}

/// Shows where a dragged window would land: above everything, even that window, so nothing
/// hides it, and transparent to the mouse, like the shadow.
pub struct DropIndicator {
    window: HWND,
    /// What it is showing: the area and the edges, in physical pixels.
    shown: Option<(RECT, [i32; 2])>,
}

impl DropIndicator {
    pub fn new() -> Self {
        Self { window: create_shadow_window(), shown: None }
    }

    /// Covers `rect`, with edges on its left and right `edges[0]` wide, and on its top and
    /// bottom `edges[1]` wide. All in physical pixels. A line has edges on its long sides only.
    pub fn show(&mut self, rect: RECT, edges: [i32; 2]) {
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        if width <= 0 || height <= 0 {
            return self.hide();
        }
        let same = self.shown.is_some_and(|(shown, shown_edges)| {
            (shown.left, shown.top, shown.right, shown.bottom) == (rect.left, rect.top, rect.right, rect.bottom)
                && shown_edges == edges
        });
        if !same {
            draw_layered(self.window, POINT { x: rect.left, y: rect.top }, width, height, |column, row| {
                let on_edge = column < edges[0]
                    || column >= width - edges[0]
                    || row < edges[1]
                    || row >= height - edges[1];
                if on_edge { DROP_EDGE } else { DROP_FILL }
            });
            self.shown = Some((rect, edges));
        }
        // Every time, as what is being dragged keeps going back on top.
        // SAFETY: takes no pointers, and a stale handle only fails.
        unsafe {
            SetWindowPos(self.window, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
    }

    pub fn hide(&mut self) {
        if self.shown.take().is_some() {
            // SAFETY: a stale handle only fails.
            unsafe { ShowWindow(self.window, SW_HIDE) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 570.0;
    const H: f32 = 960.0;
    const SCALE: f32 = 1.5;

    fn zone(x: f32, y: f32) -> Zone {
        zone_at(x, y, W, H, SCALE, true, 0.0)
    }

    #[test]
    fn scaled_rect_keeps_the_corner_and_converts_the_size() {
        let rect = RECT { left: 2000, top: 100, right: 2570, bottom: 1000 };
        let scaled = scaled_rect(rect, 144, 168);
        assert_eq!((scaled.left, scaled.top), (2000, 100));
        assert_eq!((scaled.right - scaled.left, scaled.bottom - scaled.top), (665, 1050));
    }

    #[test]
    fn the_title_bar_drags_except_over_its_buttons() {
        assert_eq!(zone(100.0, 15.0), Zone::Caption);
        assert_eq!(zone(W - 200.0, 29.0), Zone::Caption);
        assert_eq!(zone(100.0, 31.0), Zone::Client);
        // The button of 28 at 1.5x takes up the last 42 pixels.
        assert_eq!(zone(W - 43.0, 15.0), Zone::Caption);
        assert_eq!(zone(W - 41.0, 15.0), Zone::Client);
    }

    #[test]
    fn tabs_at_the_left_of_the_title_bar_are_left_to_the_ui() {
        // Tabs of 100 at 1.5x, below the 6 pixels of the top edge.
        assert_eq!(zone_at(149.0, 15.0, W, H, SCALE, true, 100.0), Zone::Client);
        assert_eq!(zone_at(151.0, 15.0, W, H, SCALE, true, 100.0), Zone::Caption);
    }

    #[test]
    fn the_edges_resize_and_the_corners_take_both_directions() {
        assert_eq!(zone(0.0, 300.0), Zone::Resize(HTLEFT));
        assert_eq!(zone(W - 1.0, 300.0), Zone::Resize(HTRIGHT));
        assert_eq!(zone(300.0, 0.0), Zone::Resize(HTTOP));
        assert_eq!(zone(300.0, H - 1.0), Zone::Resize(HTBOTTOM));
        assert_eq!(zone(0.0, 0.0), Zone::Resize(HTTOPLEFT));
        assert_eq!(zone(W - 1.0, H - 1.0), Zone::Resize(HTBOTTOMRIGHT));
    }

    #[test]
    fn a_fixed_window_has_no_resize_edges() {
        assert_eq!(zone_at(0.0, 300.0, W, H, SCALE, false, 0.0), Zone::Client);
        assert_eq!(zone_at(300.0, 0.0, W, H, SCALE, false, 0.0), Zone::Caption);
    }

    #[test]
    fn the_shadow_is_solid_inside_and_fades_out_across_its_blur() {
        assert_eq!(shadow_alpha(-3.0, 3.0), SHADOW_ALPHA as u8);
        assert_eq!(shadow_alpha(0.0, 3.0), (SHADOW_ALPHA / 2.0) as u8);
        assert!(shadow_alpha(2.0, 3.0) < shadow_alpha(1.0, 3.0));
        assert_eq!(shadow_alpha(3.0, 3.0), 0);
        assert_eq!(shadow_alpha(9.0, 3.0), 0);
    }
}
