//! The background process: `percepta.exe --daemon`, registered to start with Windows. It has no
//! window to see and loads no UI, and all it does is follow CSP: start Percepta when CSP
//! starts, hide it while CSP is minimized, and close it when CSP closes.
//!
//! It sleeps until something happens. Windows wakes it when the foreground changes, which a
//! starting CSP causes, when a window is minimized or restored, and when CSP's process ends.

use crate::i18n::tr;
use std::collections::HashMap;
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::core::{w, PCWSTR};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, FALSE, HANDLE, HWND, LPARAM, LRESULT, TRUE, WAIT_OBJECT_0, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
use windows_sys::Win32::System::Threading::{OpenProcess, INFINITE, PROCESS_SYNCHRONIZE};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, EnumWindows, FindWindowW, GetWindow, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, MsgWaitForMultipleObjects, PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassW,
    SetTimer, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, GW_OWNER, MSG, PM_REMOVE,
    QS_ALLINPUT, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_QUIT, WM_TIMER, WNDCLASSW, WS_POPUP,
};

use crate::{platform, tray};

pub const ARGUMENT: &str = "--daemon";
/// installer/percepta.iss finds it by this name.
const WINDOW_CLASS: PCWSTR = w!("PerceptaDaemon");
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
/// installer/percepta.iss removes this when uninstalling.
const RUN_VALUE: PCWSTR = w!("Percepta Daemon");
/// For a CSP that starts without ever coming to the front, or gets minimized just while
/// Percepta is starting. Everything else arrives as an event.
const FALLBACK_CHECK_MS: u32 = 15_000;
const TIMER_ID: usize = 1;

/// Something that the windows may have changed through has happened.
static CHECK_DUE: AtomicBool = AtomicBool::new(true);

/// Registers the daemon to start with Windows and starts it now, or undoes both. Done again
/// each time the app starts while enabled, so that the entry follows a moved install.
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if !enabled {
        stop();
        // SAFETY: the names are static strings. A value that isn't there only makes it fail.
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) };
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let command: Vec<u16> = format!("\"{}\" {ARGUMENT}", exe.display()).encode_utf16().chain([0]).collect();
    // SAFETY: the names are static strings, and the pointer and size in bytes describe `command`.
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            REG_SZ,
            command.as_ptr().cast(),
            (command.len() * size_of::<u16>()) as u32,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(tr!("log.startup_entry_failed", status));
    }
    if !is_running() {
        Command::new(exe).arg(ARGUMENT).spawn().map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn find_window() -> HWND {
    // SAFETY: the class name is a static string, and a null title matches any.
    unsafe { FindWindowW(WINDOW_CLASS, ptr::null()) }
}

fn is_running() -> bool {
    !find_window().is_null()
}

fn stop() {
    // SAFETY: takes no pointers, and a missing window only makes it fail.
    unsafe { PostMessageW(find_window(), tray::QUIT_MESSAGE, 0, 0) };
}

/// Runs until it is told to quit.
pub fn run() {
    if is_running() {
        return;
    }
    // SAFETY: the class name is static, and the struct is only read during the call. The
    // window lives until the process ends, and this thread pumps its messages.
    unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let mut info: WNDCLASSW = std::mem::zeroed();
        info.lpfnWndProc = Some(daemon_proc);
        info.hInstance = instance;
        info.lpszClassName = WINDOW_CLASS;
        RegisterClassW(&info);
        // An ordinary top-level window that is never shown, as with the tray's.
        let hwnd = CreateWindowExW(
            0,
            WINDOW_CLASS,
            w!("Percepta Daemon"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }
        SetTimer(hwnd, TIMER_ID, FALLBACK_CHECK_MS, None);
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        let module = ptr::null_mut();
        SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, module, Some(window_event), 0, 0, flags);
        SetWinEventHook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND, module, Some(window_event), 0, 0, flags);

        let mut watch = Watch::default();
        let mut images = HashMap::new();
        // The process being waited for, to know the moment it ends.
        let mut waiting_for: Option<(u32, HANDLE)> = None;
        let mut message: MSG = std::mem::zeroed();
        loop {
            while PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if message.message == WM_QUIT {
                    return;
                }
                if message.message == WM_TIMER {
                    CHECK_DUE.store(true, Ordering::Relaxed);
                }
                DispatchMessageW(&message);
            }
            if CHECK_DUE.swap(false, Ordering::Relaxed) {
                let csp = check(&mut watch, &mut images);
                if csp != waiting_for.map(|(pid, _)| pid) {
                    if let Some((_, process)) = waiting_for.take() {
                        CloseHandle(process);
                    }
                    waiting_for = csp.and_then(|pid| {
                        let process = OpenProcess(PROCESS_SYNCHRONIZE, FALSE, pid);
                        (!process.is_null()).then_some((pid, process))
                    });
                }
            }
            let handles: Vec<HANDLE> = waiting_for.iter().map(|(_, process)| *process).collect();
            let woken = MsgWaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), FALSE, INFINITE, QS_ALLINPUT);
            if !handles.is_empty() && woken == WAIT_OBJECT_0 {
                CHECK_DUE.store(true, Ordering::Relaxed);
                if let Some((_, process)) = waiting_for.take() {
                    CloseHandle(process);
                }
            }
        }
    }
}

unsafe extern "system" fn window_event(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    CHECK_DUE.store(true, Ordering::Relaxed);
}

unsafe extern "system" fn daemon_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == tray::QUIT_MESSAGE {
        // SAFETY: takes no pointers.
        unsafe { PostQuitMessage(0) };
        return 0;
    }
    // SAFETY: forwards the arguments Windows gave us.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// Returns CSP's process, if it is running.
fn check(watch: &mut Watch, images: &mut HashMap<u32, bool>) -> Option<u32> {
    let (csp, pid) = csp_state(images);
    for action in watch.tick(csp, tray::is_running()) {
        match action {
            Action::Launch => launch(),
            Action::Quit => tray::post(tray::QUIT_MESSAGE),
            Action::Hide => tray::post(tray::CSP_MINIMIZED_MESSAGE),
            Action::Show => tray::post(tray::CSP_RESTORED_MESSAGE),
        }
    }
    pid
}

fn launch() {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut command = Command::new(&exe);
    if let Some(folder) = exe.parent() {
        command.current_dir(folder);
    }
    let _ = command.spawn();
}

#[derive(Default)]
struct TopWindows {
    /// Each process, with whether a main window of it is showing and not minimized, and
    /// whether it has any.
    processes: HashMap<u32, (bool, bool)>,
}

unsafe extern "system" fn collect_window(hwnd: HWND, found: LPARAM) -> i32 {
    // SAFETY: `found` is the pointer that `csp_state` passes, valid during the enumeration.
    let found = unsafe { &mut *(found as *mut TopWindows) };
    let mut pid = 0;
    // SAFETY: `pid` is valid for writes, and a stale handle only reads as false or null.
    let (main, open) = unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
        let main = IsWindowVisible(hwnd) != 0 && GetWindow(hwnd, GW_OWNER).is_null();
        (main, main && IsIconic(hwnd) == 0)
    };
    let entry = found.processes.entry(pid).or_insert((false, false));
    entry.0 |= open;
    entry.1 |= main;
    TRUE
}

fn csp_state(images: &mut HashMap<u32, bool>) -> (CspState, Option<u32>) {
    let mut found = TopWindows::default();
    // SAFETY: the procedure only uses the pointer during this call.
    if unsafe { EnumWindows(Some(collect_window), &raw mut found as LPARAM) } == FALSE {
        return (CspState::Gone, None);
    }
    // A process id can come back as something else, but not while it is still around.
    images.retain(|pid, _| found.processes.contains_key(pid));
    let mut state = CspState::Gone;
    let mut csp = None;
    for (pid, (open, main)) in found.processes {
        if !*images.entry(pid).or_insert_with(|| counts_as_csp(pid)) {
            continue;
        }
        csp = Some(pid);
        state = match (state, open, main) {
            (_, true, _) | (CspState::Open, ..) => CspState::Open,
            (_, false, true) | (CspState::Minimized, ..) => CspState::Minimized,
            // Only hidden windows: still starting up, or on its way out.
            (CspState::Gone, false, false) => CspState::Open,
        };
    }
    (state, csp)
}

fn counts_as_csp(pid: u32) -> bool {
    let is_csp = platform::is_csp_process(pid);
    // So that a stand-in can be tested on while the real CSP is open.
    #[cfg(debug_assertions)]
    if let Some(folder) = std::env::var_os("PERCEPTA_TEST_CSP_FOLDER") {
        let folder = folder.to_string_lossy().to_lowercase();
        return is_csp && platform::process_image(pid).is_some_and(|path| path.to_lowercase().contains(&folder));
    }
    is_csp
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CspState {
    Gone,
    Open,
    Minimized,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Launch,
    Quit,
    Hide,
    Show,
}

/// What has been seen of CSP so far. Only a change leads to anything, so that the user can
/// quit Percepta while CSP stays open, or bring it back from the tray while CSP is minimized.
#[derive(Default)]
struct Watch {
    csp_seen: bool,
    hidden: bool,
}

impl Watch {
    fn tick(&mut self, csp: CspState, app_running: bool) -> Vec<Action> {
        let mut actions = Vec::new();
        if csp == CspState::Gone {
            if std::mem::take(&mut self.csp_seen) && app_running {
                actions.push(Action::Quit);
            }
            self.hidden = false;
            return actions;
        }
        if !std::mem::replace(&mut self.csp_seen, true) && !app_running {
            actions.push(Action::Launch);
        }
        // Not until the app is there to tell, which is the next check at the soonest.
        if !app_running {
            self.hidden = false;
            return actions;
        }
        let minimized = csp == CspState::Minimized;
        if minimized != self.hidden {
            self.hidden = minimized;
            actions.push(if minimized { Action::Hide } else { Action::Show });
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CspState::{Gone, Minimized, Open};

    #[test]
    fn it_starts_percepta_with_csp_and_closes_it_with_csp() {
        let mut watch = Watch::default();
        assert_eq!(watch.tick(Gone, false), []);
        assert_eq!(watch.tick(Open, false), [Action::Launch]);
        assert_eq!(watch.tick(Open, true), []);
        assert_eq!(watch.tick(Gone, true), [Action::Quit]);
        assert_eq!(watch.tick(Gone, false), []);
        assert_eq!(watch.tick(Open, false), [Action::Launch]);
    }

    #[test]
    fn it_leaves_alone_a_percepta_that_the_user_started_or_quit() {
        let mut watch = Watch::default();
        // Already there when CSP starts.
        assert_eq!(watch.tick(Open, true), []);
        // Quit by the user while CSP stays open.
        assert_eq!(watch.tick(Open, false), []);
        assert_eq!(watch.tick(Open, false), []);
        // Started by hand with no CSP around, which has nothing to do with this.
        let mut watch = Watch::default();
        assert_eq!(watch.tick(Gone, true), []);
    }

    #[test]
    fn it_hides_percepta_only_when_csp_gets_minimized_and_back() {
        let mut watch = Watch::default();
        watch.tick(Open, false);
        assert_eq!(watch.tick(Open, true), []);
        assert_eq!(watch.tick(Minimized, true), [Action::Hide]);
        assert_eq!(watch.tick(Minimized, true), []);
        assert_eq!(watch.tick(Open, true), [Action::Show]);
        assert_eq!(watch.tick(Open, true), []);
    }

    #[test]
    fn a_csp_found_minimized_hides_percepta_once_it_has_started() {
        let mut watch = Watch::default();
        assert_eq!(watch.tick(Minimized, false), [Action::Launch]);
        assert_eq!(watch.tick(Minimized, false), []);
        assert_eq!(watch.tick(Minimized, true), [Action::Hide]);
    }
}
