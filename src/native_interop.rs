use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM, RECT};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETTASKBARPOS, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Absolute path to a program that ships in the Windows system directory.
///
/// `Command::new("wsl.exe")` does not mean "the Windows one". Rust resolves a
/// bare program name by searching the directory of the current executable
/// before the system directory, so a file dropped next to a portable
/// `gengchou.exe` is what runs - and a downloads folder, where a portable
/// build normally lives, is exactly where a stray executable ends up.
/// Confirmed on the pinned toolchain rather than assumed: with a decoy
/// `wsl.exe` beside a test binary the decoy ran, and the real one only ran
/// once the decoy was removed.
///
/// `GetSystemDirectoryW` first, rather than `%SystemRoot%`, because an
/// environment variable can be pointed elsewhere. `%SystemRoot%\System32` is
/// tried only if that call fails, because a spoofable absolute path is still
/// strictly better than the bare name: the bare name resolves against this
/// executable's own directory, which is the hole this function exists to
/// close. Falling back to it would have reopened that hole on the one path
/// nobody tests.
pub fn system_program(name: &str) -> PathBuf {
    static SYSTEM_DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
    let directory = SYSTEM_DIRECTORY.get_or_init(|| {
        let mut buffer = [0u16; 260];
        let length = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
        if length > 0 && length <= buffer.len() {
            return Some(PathBuf::from(OsString::from_wide(&buffer[..length])));
        }
        let from_environment = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .filter(|root| root.is_absolute())
            .map(|root| root.join("System32"));
        crate::diagnose::log(match &from_environment {
            Some(root) => format!(
                "unable to resolve the Windows system directory; using {}",
                root.display()
            ),
            None => "unable to resolve the Windows system directory, and SystemRoot is unusable"
                .to_string(),
        });
        from_environment
    });
    match directory {
        Some(directory) => directory.join(name),
        // Nothing absolute is available. A bare name here would run whatever
        // sits beside a portable build, so keep it absolute and let the launch
        // fail loudly instead.
        None => PathBuf::from(r"\\?\GLOBALROOT\SystemRoot\System32").join(name),
    }
}

/// Why a child process did not produce a usable answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessRunError {
    SpawnFailed,
    TimedOut,
    WaitFailed,
}

/// Run a command, wait up to `timeout`, and return everything it wrote.
///
/// Two things this does that `Child::wait_with_output` after a `try_wait` loop
/// does not:
///
/// * It drains stdout and stderr while the child is still running.
///   `wait_with_output` only reads after the process has exited, and a child
///   that fills the pipe buffer never gets there: it blocks in `write`, the
///   loop hits the deadline, and an ordinary answer that was merely large is
///   reported as a probe that never responded. The credential probes read
///   token files whose size is not ours to bound.
/// * It kills the whole process tree on timeout. `Child::kill` ends only the
///   process that was spawned, and the Claude CLI is normally reached through
///   `claude.cmd`, so killing it left the `cmd.exe` shim's `node` running with
///   the timed-out command still in flight.
///
/// The job object is assigned right after spawn rather than before, because
/// `std` gives no way to resume a suspended child. A grandchild created in
/// that window escapes the kill; everything after it does not, which is the
/// difference between a leaked tree and one straggler.
pub fn run_with_timeout(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Result<std::process::Output, ProcessRunError> {
    let mut child = command.spawn().map_err(|_| ProcessRunError::SpawnFailed)?;
    let job = ProcessTreeJob::containing(&child);
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                end_process_tree(&job, &mut child);
                // Deliberately not joined. The output is discarded on this
                // path, and a reader only finishes at EOF - which never comes
                // if a descendant that escaped the job inherited the write
                // end. Joining here turned a reported timeout into a caller
                // that never returns at all, hanging the poll thread for the
                // life of the process.
                drop(stdout);
                drop(stderr);
                return Err(ProcessRunError::TimedOut);
            }
            // A `try_wait` failure means this child can no longer be observed,
            // which is not a reason to walk away from it: returning here used
            // to leave the process, its tree and both reader threads running
            // with nobody left to end them.
            Err(_) => {
                end_process_tree(&job, &mut child);
                drop(stdout);
                drop(stderr);
                return Err(ProcessRunError::WaitFailed);
            }
        }
    };

    // The child has exited, so every write end it owned is closed and the
    // readers are at EOF already. The grace covers the one case where they are
    // not: a descendant that escaped the job before it was assigned, still
    // holding the inherited pipe. Waiting on that forever is the same hang as
    // the timeout path above, so it is bounded and reported.
    Ok(std::process::Output {
        status,
        stdout: collect(stdout, DRAIN_GRACE)?,
        stderr: collect(stderr, DRAIN_GRACE)?,
    })
}

/// How long a reader may still take once the child itself has exited.
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// End the child and everything it started, then reap it.
fn end_process_tree(job: &Option<ProcessTreeJob>, child: &mut std::process::Child) {
    match job {
        Some(job) => job.terminate(),
        None => {
            let _ = child.kill();
        }
    }
    let _ = child.wait();
}

type DrainHandle = Option<std::sync::mpsc::Receiver<Vec<u8>>>;

fn drain(pipe: Option<impl std::io::Read + Send + 'static>) -> DrainHandle {
    pipe.map(|mut pipe| {
        // A channel rather than a `JoinHandle`, because a join cannot be
        // bounded and this wait has to be.
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = std::io::Read::read_to_end(&mut pipe, &mut buffer);
            let _ = sender.send(buffer);
        });
        receiver
    })
}

fn collect(handle: DrainHandle, grace: std::time::Duration) -> Result<Vec<u8>, ProcessRunError> {
    let Some(handle) = handle else {
        return Ok(Vec::new());
    };
    handle.recv_timeout(grace).map_err(|error| {
        // Either the reader is still blocked on a pipe somebody else holds
        // open, or it died without sending. Neither has output to report, and
        // an empty answer would not be honest.
        crate::diagnose::log(format!("a child's output could not be collected: {error}"));
        ProcessRunError::WaitFailed
    })
}

/// A job object holding one child and everything it goes on to start.
struct ProcessTreeJob(HANDLE);

impl ProcessTreeJob {
    fn containing(child: &std::process::Child) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;

        let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.ok()?;
        let assigned = unsafe {
            AssignProcessToJobObject(job, HANDLE(child.as_raw_handle() as isize as *mut _))
        };
        if assigned.is_err() {
            // Nested jobs need Windows 8; without one the caller still kills
            // the direct child, which is what it did before.
            crate::diagnose::log("unable to place a child process in a job object");
            unsafe {
                let _ = CloseHandle(job);
            }
            return None;
        }
        Some(Self(job))
    }

    fn terminate(&self) {
        let _ = unsafe { TerminateJobObject(self.0, 1) };
    }
}

impl Drop for ProcessTreeJob {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// Window style constants
pub const WS_POPUP_STYLE: u32 = 0x80000000;
pub const WS_CHILD_STYLE: u32 = 0x40000000;
pub const WS_CLIPSIBLINGS_STYLE: u32 = 0x04000000;

// Win event constants
pub const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
pub const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;

// Timer IDs
pub const TIMER_POLL: usize = 1;
pub const TIMER_COUNTDOWN: usize = 2;
pub const TIMER_RESET_POLL: usize = 3;
pub const TIMER_UPDATE_CHECK: usize = 4;
/// Re-checks the credentials on disk while polling is paused after an auth
/// failure, so re-authenticating is noticed in seconds rather than at the
/// next poll interval (up to an hour).
pub const TIMER_AUTH_WATCH: usize = 5;

// Custom messages
pub const WM_APP: u32 = 0x8000;
pub const WM_APP_USAGE_UPDATED: u32 = WM_APP + 1;
pub const WM_APP_TRAY: u32 = WM_APP + 3;

#[derive(Clone, Copy, Debug)]
pub struct TaskbarWindow {
    pub hwnd: HWND,
    pub rect: RECT,
}

pub fn find_taskbars() -> Vec<TaskbarWindow> {
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let taskbars = &mut *(lparam.0 as *mut Vec<TaskbarWindow>);
        let mut class_name = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, &mut class_name) };
        if len > 0 {
            let class_name = String::from_utf16_lossy(&class_name[..len as usize]);
            if class_name == "Shell_TrayWnd" || class_name == "Shell_SecondaryTrayWnd" {
                if let Some(rect) = get_taskbar_rect(hwnd).or_else(|| get_window_rect_safe(hwnd)) {
                    taskbars.push(TaskbarWindow { hwnd, rect });
                }
            }
        }
        BOOL(1)
    }

    let mut taskbars: Vec<TaskbarWindow> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut taskbars as *mut _ as isize));
    }
    taskbars.sort_by_key(|taskbar| {
        (
            taskbar.rect.top,
            taskbar.rect.left,
            taskbar.rect.bottom,
            taskbar.rect.right,
        )
    });
    taskbars
}

fn wide_array_to_string(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

/// Resolve a GDI display name (for example `\\.\DISPLAY1`) to the more
/// stable monitor device path exposed by DisplayConfig. DisplayConfig can be
/// transiently unavailable during shell/display rebuilds, so callers must
/// retain the GDI name as a fallback identity.
fn query_stable_monitor_device_path(gdi_device_name: &str) -> Option<String> {
    unsafe {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count).0
            != 0
        {
            return None;
        }

        // The display topology can change between the size query and the
        // data query. Retry with fresh buffer sizes instead of treating that
        // ordinary race as a permanent identity failure.
        for _ in 0..3 {
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            let mut actual_path_count = path_count;
            let mut actual_mode_count = mode_count;
            let result = QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut actual_path_count,
                paths.as_mut_ptr(),
                &mut actual_mode_count,
                modes.as_mut_ptr(),
                None,
            );
            if result.0 != 0 {
                if GetDisplayConfigBufferSizes(
                    QDC_ONLY_ACTIVE_PATHS,
                    &mut path_count,
                    &mut mode_count,
                )
                .0 != 0
                {
                    return None;
                }
                continue;
            }
            paths.truncate(actual_path_count as usize);

            for path in paths {
                let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                        size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                        adapterId: path.sourceInfo.adapterId,
                        id: path.sourceInfo.id,
                    },
                    ..Default::default()
                };
                if DisplayConfigGetDeviceInfo(&mut source.header) != 0
                    || !wide_array_to_string(&source.viewGdiDeviceName)
                        .eq_ignore_ascii_case(gdi_device_name)
                {
                    continue;
                }

                let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                        size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                        adapterId: path.targetInfo.adapterId,
                        id: path.targetInfo.id,
                    },
                    ..Default::default()
                };
                if DisplayConfigGetDeviceInfo(&mut target.header) != 0 {
                    continue;
                }
                let device_path = wide_array_to_string(&target.monitorDevicePath);
                if !device_path.is_empty() {
                    return Some(device_path);
                }
            }
            return None;
        }
        None
    }
}

static MONITOR_DEVICE_PATH_CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> =
    OnceLock::new();

pub fn stable_monitor_device_path(gdi_device_name: &str) -> Option<String> {
    let cache = MONITOR_DEVICE_PATH_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(gdi_device_name)
        .cloned()
    {
        return cached;
    }
    let resolved = query_stable_monitor_device_path(gdi_device_name);
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(gdi_device_name.to_string(), resolved.clone());
    resolved
}

pub fn clear_monitor_device_path_cache() {
    if let Some(cache) = MONITOR_DEVICE_PATH_CACHE.get() {
        cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

/// Find a child window by class name
pub fn find_child_window(parent: HWND, class_name: &str) -> Option<HWND> {
    unsafe {
        let class = wide_str(class_name);
        match FindWindowExW(
            parent,
            HWND::default(),
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::null(),
        ) {
            Ok(h) if h != HWND::default() => Some(h),
            _ => None,
        }
    }
}

/// Get taskbar position via SHAppBarMessage
pub fn get_taskbar_rect(taskbar_hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut class_name = [0u16; 64];
        let len = GetClassNameW(taskbar_hwnd, &mut class_name);
        if len > 0 {
            let class_name = String::from_utf16_lossy(&class_name[..len as usize]);
            if class_name == "Shell_SecondaryTrayWnd" {
                return get_window_rect_safe(taskbar_hwnd);
            }
        }

        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: taskbar_hwnd,
            ..Default::default()
        };
        let result = SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd);
        if result == 0 {
            return None;
        }
        Some(abd.rc)
    }
}

/// Get the bounding rectangle of a window
pub fn get_window_rect_safe(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            Some(rect)
        } else {
            None
        }
    }
}

fn embedding_state_is_valid(parent: HWND, taskbar_hwnd: HWND, style: u32) -> bool {
    parent == taskbar_hwnd && style & WS_CHILD_STYLE != 0 && style & WS_POPUP_STYLE == 0
}

/// Verify the live relationship instead of inferring it from a transient
/// taskbar enumeration. During display/RDP transitions Explorer can briefly
/// omit a still-valid taskbar HWND from `EnumWindows`.
pub fn is_embedded_in_taskbar(hwnd: HWND, taskbar_hwnd: HWND) -> bool {
    unsafe {
        if !IsWindow(hwnd).as_bool() || !IsWindow(taskbar_hwnd).as_bool() {
            return false;
        }
        let parent = GetAncestor(hwnd, GA_PARENT);
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        embedding_state_is_valid(parent, taskbar_hwnd, style)
    }
}

fn popup_state_is_valid(owner_or_parent: HWND, style: u32) -> bool {
    owner_or_parent == HWND::default() && style & WS_CHILD_STYLE == 0 && style & WS_POPUP_STYLE != 0
}

/// Embed our window as a child of the taskbar and verify the resulting Shell
/// relationship. Win32 setters can report an ambiguous zero return value, so
/// the final parent/style state is the source of truth.
pub fn embed_in_taskbar(hwnd: HWND, taskbar_hwnd: HWND) -> Result<(), String> {
    unsafe {
        if !IsWindow(hwnd).as_bool() || !IsWindow(taskbar_hwnd).as_bool() {
            return Err("widget or taskbar window handle is no longer valid".to_string());
        }

        // Preserve existing extended style, add tool window + no activate
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongW(
            hwnd,
            GWL_EXSTYLE,
            ex_style | WS_EX_TOOLWINDOW.0 as i32 | WS_EX_NOACTIVATE.0 as i32,
        );

        // Change from popup to child
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let new_style = (style & !WS_POPUP_STYLE) | WS_CHILD_STYLE | WS_CLIPSIBLINGS_STYLE;
        let _ = SetWindowLongW(hwnd, GWL_STYLE, new_style as i32);

        let _ = SetParent(hwnd, taskbar_hwnd);

        let _ = SetWindowPos(
            hwnd,
            HWND::default(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        let parent = GetAncestor(hwnd, GA_PARENT);
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        if embedding_state_is_valid(parent, taskbar_hwnd, style) {
            Ok(())
        } else {
            Err(format!(
                "taskbar embedding verification failed: parent={parent:?} expected={taskbar_hwnd:?} style={style:#010x}"
            ))
        }
    }
}

/// Undo `embed_in_taskbar`: turn the window back into a top-level popup
/// style. Callers keep that transitional window hidden until taskbar
/// re-embedding succeeds.
pub fn detach_to_popup(hwnd: HWND) -> Result<(), String> {
    unsafe {
        if !IsWindow(hwnd).as_bool() {
            return Err("widget window handle is no longer valid".to_string());
        }
        // Clear WS_CHILD before re-parenting to the desktop, per SetParent docs.
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let new_style = (style & !WS_CHILD_STYLE) | WS_POPUP_STYLE;
        let _ = SetWindowLongW(hwnd, GWL_STYLE, new_style as i32);
        let _ = SetParent(hwnd, HWND::default());
        let _ = SetWindowPos(
            hwnd,
            HWND::default(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        // GetAncestor(GA_PARENT) reports the desktop window for an unowned
        // top-level popup. GetParent reports its owner instead, which is null
        // for the detached window we create.
        let owner_or_parent = GetParent(hwnd).unwrap_or_default();
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        if popup_state_is_valid(owner_or_parent, style) {
            Ok(())
        } else {
            Err(format!(
                "detached popup verification failed: owner_or_parent={owner_or_parent:?} style={style:#010x}"
            ))
        }
    }
}

/// Move the window
pub fn move_window(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let _ = MoveWindow(hwnd, x, y, w, h, true);
    }
}

/// Set up a WinEvent hook for tray location changes
pub fn set_tray_event_hook(
    thread_id: u32,
    callback: unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32, u32),
) -> Option<HWINEVENTHOOK> {
    unsafe {
        let hook = SetWinEventHook(
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(callback),
            0,
            thread_id,
            WINEVENT_OUTOFCONTEXT,
        );
        if hook.is_invalid() {
            None
        } else {
            Some(hook)
        }
    }
}

/// Get the thread ID that owns a window
pub fn get_window_thread_id(hwnd: HWND) -> u32 {
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

/// Unhook a WinEvent hook
pub fn unhook_win_event(hook: HWINEVENTHOOK) {
    unsafe {
        let _ = UnhookWinEvent(hook);
    }
}

/// Convert a Rust string to a null-terminated wide string
pub fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// COLORREF wrapper (RGB packed into u32)
pub fn colorref(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// Color helper
#[derive(Clone, Copy, Debug)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn from_hex(hex: &str) -> Self {
        let hex = hex.strip_prefix('#').unwrap_or(hex);
        if hex.len() != 6 || !hex.bytes().all(|value| value.is_ascii_hexdigit()) {
            return Self::new(0, 0, 0);
        }
        let channel = |range| {
            hex.get(range)
                .and_then(|value| u8::from_str_radix(value, 16).ok())
                .unwrap_or(0)
        };
        let r = channel(0..2);
        let g = channel(2..4);
        let b = channel(4..6);
        Self { r, g, b }
    }

    pub const fn from_colorref(value: u32) -> Self {
        Self {
            r: (value & 0xFF) as u8,
            g: ((value >> 8) & 0xFF) as u8,
            b: ((value >> 16) & 0xFF) as u8,
        }
    }

    pub fn to_colorref(self) -> u32 {
        colorref(self.r, self.g, self.b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every Windows program Gengchou starts must be named by absolute path.
    ///
    /// A bare name is resolved against this executable's own directory before
    /// the system directory, so a portable build sitting in a downloads folder
    /// would run whatever `wsl.exe` happens to have landed beside it - and the
    /// credential probes are what run `wsl.exe`. Asserted on `system_program`
    /// because that is what every call site now passes to `Command::new`.
    #[test]
    fn a_system_program_is_named_by_absolute_path() {
        let wsl = system_program("wsl.exe");
        assert!(wsl.is_absolute(), "got {}", wsl.display());
        assert_eq!(
            wsl.file_name().and_then(|name| name.to_str()),
            Some("wsl.exe")
        );

        let directory = wsl.parent().expect("an absolute path has a parent");
        assert!(
            directory.is_dir(),
            "the resolved system directory must exist: {}",
            directory.display()
        );
        // Windows reports this directory in mixed case, so compare folded.
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            let expected = PathBuf::from(system_root).join("System32");
            assert_eq!(
                directory.to_string_lossy().to_lowercase(),
                expected.to_string_lossy().to_lowercase()
            );
        }

        // A nested name stays under the same root.
        let powershell = system_program(r"WindowsPowerShell\v1.0\powershell.exe");
        assert!(powershell
            .to_string_lossy()
            .to_lowercase()
            .starts_with(&directory.to_string_lossy().to_lowercase()));
        assert_eq!(
            powershell.file_name().and_then(|name| name.to_str()),
            Some("powershell.exe")
        );
    }

    fn powershell() -> std::process::Command {
        let mut command =
            std::process::Command::new(system_program(r"WindowsPowerShell\v1.0\powershell.exe"));
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]);
        command
    }

    /// A large but perfectly ordinary answer must not read as a dead probe.
    ///
    /// The pipe buffer is a few kilobytes. Waiting for exit before reading it
    /// deadlocks a child that writes more than that: it blocks in `write`, the
    /// deadline passes, and the probe is reported as one that never answered.
    #[test]
    fn a_child_that_outgrows_the_pipe_buffer_still_reports_its_output() {
        const SIZE: usize = 300_000;
        let mut command = powershell();
        command
            .arg(format!("[Console]::Out.Write('x' * {SIZE})"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let output = run_with_timeout(&mut command, std::time::Duration::from_secs(30))
            .expect("a child that writes a lot is not a timeout");
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), SIZE);
    }

    /// Ending a run must take the child's descendants with it.
    ///
    /// The Claude CLI is normally `claude.cmd`, so the process actually doing
    /// the work is a `node` under a `cmd.exe` shim. Ending only the direct
    /// child left that work running past the deadline meant to stop it.
    ///
    /// This drives `end_process_tree`, which both of `run_with_timeout`'s
    /// failure paths route through, rather than driving a real timeout. An
    /// earlier version did the latter and was flaky: it needed the grandchild
    /// to finish starting before the deadline expired, so a cold machine and a
    /// genuine regression produced the same red test. Waiting for the
    /// grandchild to exist first removes the race - a slow interpreter start
    /// costs seconds here instead of a false failure.
    #[test]
    fn ending_a_run_takes_the_grandchildren_with_it() {
        // No parentheses or spaces in the name: `cmd /c` re-parses its
        // argument, and a name like `ThreadId(3)` is a syntax error there.
        let stem = std::env::temp_dir().join(format!(
            "gengchou-tree-kill-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default()
        ));
        let marker = stem.with_extension("pid");
        // A `.cmd` shim around a long-running interpreter: the exact shape of
        // `claude.cmd` around `node`.
        let shim = stem.with_extension("cmd");
        let _ = std::fs::remove_file(&marker);
        std::fs::write(
            &shim,
            format!(
                "@echo off\r\n\"{}\" -NoLogo -NoProfile -NonInteractive -Command \"$PID | Set-Content -LiteralPath '{}'; Start-Sleep -Seconds 120\"\r\n",
                system_program(r"WindowsPowerShell\v1.0\powershell.exe").display(),
                marker.display()
            ),
        )
        .expect("write the shim");

        let mut child = std::process::Command::new(system_program("cmd.exe"))
            .arg("/c")
            .arg(&shim)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the shim");

        let job = ProcessTreeJob::containing(&child);
        assert!(
            job.is_some(),
            "a freshly spawned child must be placeable in a job object"
        );

        let grandchild = process_id_once_recorded(&marker, std::time::Duration::from_secs(60));
        end_process_tree(&job, &mut child);
        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::remove_file(&shim);

        assert!(
            process_has_exited(grandchild),
            "grandchild {grandchild} outlived the end of the run that started it"
        );
    }

    /// Block until the grandchild has written a usable process id, or give up.
    ///
    /// The deadline is generous on purpose: it bounds a hang, it does not time
    /// the interpreter. A partially written file simply is not parseable yet.
    fn process_id_once_recorded(marker: &std::path::Path, deadline: std::time::Duration) -> u32 {
        let started = std::time::Instant::now();
        loop {
            if let Some(pid) = std::fs::read_to_string(marker)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
            {
                return pid;
            }
            assert!(
                started.elapsed() < deadline,
                "the grandchild recorded no process id within {deadline:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Whether a process id refers to something that is no longer running.
    ///
    /// A pid that cannot be opened at all counts as gone: the only handle this
    /// test ever had was through the job object that just terminated it.
    fn process_has_exited(pid: u32) -> bool {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };

        let Ok(handle) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
            return true;
        };
        // Two seconds of slack: TerminateJobObject is asynchronous.
        let result = unsafe { WaitForSingleObject(handle, 2_000) };
        unsafe {
            let _ = CloseHandle(handle);
        }
        result == WAIT_OBJECT_0
    }

    #[test]
    fn embedding_validation_requires_expected_parent_and_child_style() {
        let taskbar = HWND(1usize as *mut _);
        let other = HWND(2usize as *mut _);
        assert!(embedding_state_is_valid(taskbar, taskbar, WS_CHILD_STYLE));
        assert!(!embedding_state_is_valid(other, taskbar, WS_CHILD_STYLE));
        assert!(!embedding_state_is_valid(taskbar, taskbar, WS_POPUP_STYLE));
    }

    #[test]
    fn popup_validation_rejects_child_or_parented_windows() {
        assert!(popup_state_is_valid(HWND::default(), WS_POPUP_STYLE));
        assert!(!popup_state_is_valid(HWND::default(), WS_CHILD_STYLE));
        assert!(!popup_state_is_valid(
            HWND(1usize as *mut _),
            WS_POPUP_STYLE
        ));
    }

    #[test]
    fn colorref_round_trip_preserves_rgb_channels() {
        let color = Color::new(12, 34, 56);
        assert_eq!(Color::from_colorref(color.to_colorref()).r, 12);
        assert_eq!(Color::from_colorref(color.to_colorref()).g, 34);
        assert_eq!(Color::from_colorref(color.to_colorref()).b, 56);
    }

    #[test]
    fn color_from_hex_accepts_exact_rgb_and_rejects_invalid_input() {
        let prefixed = Color::from_hex("#0c2238");
        let plain = Color::from_hex("0C2238");
        for color in [prefixed, plain] {
            assert_eq!((color.r, color.g, color.b), (12, 34, 56));
        }

        for invalid in ["#fff", "#GG0000", "é00000", "##FFFFFF"] {
            let color = Color::from_hex(invalid);
            assert_eq!((color.r, color.g, color.b), (0, 0, 0));
        }
    }
}
