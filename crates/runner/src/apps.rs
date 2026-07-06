//! Launching applications and enumerating top-level windows.

use arc_proto::id::WindowId;
#[cfg(windows)]
use arc_proto::wire::Rect;
use arc_proto::wire::{Reply, WindowInfo};

use crate::dispatch::{RemoteResult, os_error};

/// Default window to watch a freshly-launched process for a startup crash when
/// the caller doesn't specify one. A fast loader/init failure (missing DLL, bad
/// image) dies well within this; a healthy GUI app is still running after it.
#[cfg(windows)]
const DEFAULT_LAUNCH_WATCH_MS: u64 = 800;

/// Launches an application, returning its process id.
///
/// Watches the child for up to `watch_ms` (`None` → [`DEFAULT_LAUNCH_WATCH_MS`],
/// `0` → don't wait): if it exits within it, the launch is reported as a startup
/// crash — [`Reply::AppOpened::exit_code`] is filled and, on abnormal exit,
/// [`Reply::AppOpened::diagnostic`] carries the most recent matching Application
/// error event. A process still running when the window elapses is left running
/// (dropping the handle doesn't kill it) and reported with `exit_code: None`. The
/// main window is not resolved synchronously (apps surface windows
/// asynchronously); callers can follow up with [`list_windows`].
pub fn open_app(target: &str, args: &[String], watch_ms: Option<u64>) -> RemoteResult<Reply> {
    #[allow(unused_mut)]
    let mut child = std::process::Command::new(target)
        .args(args)
        .spawn()
        .map_err(|e| os_error(format!("failed to launch '{target}': {e}")))?;
    let pid = child.id();

    #[cfg(windows)]
    {
        let watch = std::time::Duration::from_millis(watch_ms.unwrap_or(DEFAULT_LAUNCH_WATCH_MS));
        let start = std::time::Instant::now();
        while !watch.is_zero() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let exit_code = status.code();
                    // Only an abnormal (non-zero) exit is worth an event-log dig.
                    let diagnostic = match exit_code {
                        Some(0) => None,
                        _ => recent_app_error(target),
                    };
                    return Ok(Reply::AppOpened {
                        window: None,
                        pid,
                        exit_code,
                        diagnostic,
                    });
                }
                Ok(None) if start.elapsed() < watch => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                _ => break, // still running, or wait errored — treat as launched
            }
        }
    }
    let _ = watch_ms; // consumed only on Windows

    Ok(Reply::AppOpened {
        window: None,
        pid,
        exit_code: None,
        diagnostic: None,
    })
}

/// Looks up the most recent Application-log **Error** event (last 30s) whose
/// message mentions `exe`'s file name — the faulting-module + exception-code
/// record Windows writes when a process crashes. Best-effort: any failure (no
/// matching event, `Get-WinEvent` unavailable) yields `None`.
#[cfg(windows)]
fn recent_app_error(exe: &str) -> Option<String> {
    let name = std::path::Path::new(exe)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(exe)
        .replace('\'', "''");
    let query = format!(
        "$ErrorActionPreference='SilentlyContinue'; \
         Get-WinEvent -FilterHashtable @{{LogName='Application'; Level=2; \
         StartTime=(Get-Date).AddSeconds(-30)}} -MaxEvents 20 | \
         Where-Object {{ $_.Message -match [regex]::Escape('{name}') }} | \
         Select-Object -First 1 -ExpandProperty Message"
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &query])
        .output()
        .ok()?;
    let msg = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if msg.is_empty() { None } else { Some(msg) }
}

/// Writes a minidump of process `pid` to `%TEMP%\arc-procdump-<pid>.dmp` and
/// returns its path + size. Captures thread stacks + thread info + module list
/// (not full memory) — small, but enough to see where a hung process is stuck
/// once opened in a debugger with symbols. The controller pulls the file back.
#[cfg(windows)]
pub fn proc_dump(pid: u32) -> RemoteResult<Reply> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::Debug::{
        MINIDUMP_TYPE, MiniDumpNormal, MiniDumpWithThreadInfo, MiniDumpWriteDump,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    let mut path = std::env::temp_dir();
    path.push(format!("arc-procdump-{pid}.dmp"));
    let file = std::fs::File::create(&path)
        .map_err(|e| os_error(format!("creating dump {}: {e}", path.display())))?;

    // SAFETY: `process` is a valid handle from OpenProcess (checked); `file`'s
    // handle outlives the call (dropped after); the three optional params are
    // null. The process handle is always closed before returning.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid)
            .map_err(|e| os_error(format!("OpenProcess({pid}): {e}")))?;
        let dump_type = MINIDUMP_TYPE(MiniDumpNormal.0 | MiniDumpWithThreadInfo.0);
        let result = MiniDumpWriteDump(
            process,
            pid,
            HANDLE(file.as_raw_handle()),
            dump_type,
            None,
            None,
            None,
        );
        let _ = CloseHandle(process);
        result.map_err(|e| os_error(format!("MiniDumpWriteDump({pid}): {e}")))?;
    }
    drop(file); // flush + close the dump file

    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok(Reply::Dumped {
        path: path.to_string_lossy().into_owned(),
        size,
    })
}

#[cfg(not(windows))]
pub fn proc_dump(_pid: u32) -> RemoteResult<Reply> {
    Err(os_error("procdump is only supported on Windows".to_owned()))
}

/// Enumerates visible, titled top-level windows.
#[cfg(windows)]
pub fn list_windows() -> RemoteResult<Reply> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetForegroundWindow, IsWindowVisible,
    };

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        // SAFETY: `lparam` carries the `&mut Vec<HWND>` we passed to EnumWindows.
        let handles = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        handles.push(hwnd);
        true.into() // keep enumerating
    }

    let mut handles: Vec<HWND> = Vec::new();
    // SAFETY: `collect` only dereferences the pointer we pass, valid for the call.
    unsafe {
        EnumWindows(
            Some(collect),
            LPARAM(&mut handles as *mut Vec<HWND> as isize),
        )
    }
    .map_err(|e| os_error(format!("EnumWindows: {e}")))?;

    // SAFETY: no preconditions.
    let foreground = unsafe { GetForegroundWindow() };
    let infos = handles
        .into_iter()
        // SAFETY: handles came from EnumWindows this call.
        .filter(|&h| unsafe { IsWindowVisible(h) }.as_bool())
        .filter_map(|h| {
            let title = window_title(h);
            (!title.is_empty()).then(|| WindowInfo {
                id: WindowId(h.0 as u64),
                title,
                process: process_name(h),
                focused: h == foreground,
                rect: window_rect(h),
            })
        })
        .collect();
    Ok(Reply::Windows(infos))
}

/// Reads a window's screen rectangle.
#[cfg(windows)]
fn window_rect(hwnd: windows::Win32::Foundation::HWND) -> Rect {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let mut r = RECT::default();
    // SAFETY: `hwnd` is live for this call; GetWindowRect errors on a stale handle.
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_ok() {
        Rect {
            x: r.left,
            y: r.top,
            width: r.right - r.left,
            height: r.bottom - r.top,
        }
    } else {
        Rect::default()
    }
}

/// Reads a window's title text.
#[cfg(windows)]
fn window_title(hwnd: windows::Win32::Foundation::HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowTextLengthW, GetWindowTextW};
    // SAFETY: `hwnd` is live for this call; the buffer is sized to the reported
    // length + 1 for the NUL.
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; len as usize + 1];
    let written = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..written as usize])
}

/// Resolves the executable file name owning `hwnd` (e.g. `notepad.exe`).
#[cfg(windows)]
fn process_name(hwnd: windows::Win32::Foundation::HWND) -> String {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    use windows::core::PWSTR;

    let mut pid = 0u32;
    // SAFETY: `hwnd` is live; `pid` receives the owning process id.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return String::new();
    }
    // SAFETY: querying our own session's process by id; handle closed below.
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return String::new();
    };
    let mut buffer = vec![0u16; 260];
    let mut size = buffer.len() as u32;
    // SAFETY: `handle` is a live process handle; `buffer`/`size` describe the
    // output buffer.
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    };
    // SAFETY: balances OpenProcess.
    unsafe {
        let _ = CloseHandle(handle);
    }
    if result.is_err() {
        return String::new();
    }
    let path = String::from_utf16_lossy(&buffer[..size as usize]);
    path.rsplit(['\\', '/']).next().unwrap_or(&path).to_owned()
}

/// Restores a window if minimized and brings it to the foreground, so a
/// subsequent capture or input lands on a real, visible window.
#[cfg(windows)]
pub fn activate_window(window: WindowId) -> RemoteResult<Reply> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        IsIconic, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
    };
    let hwnd = HWND(window.0 as *mut std::ffi::c_void);
    // SAFETY: `hwnd` is a window handle supplied by the controller (from
    // `list_windows`); these calls are no-ops on a stale/invalid handle.
    unsafe {
        let cmd = if IsIconic(hwnd).as_bool() {
            SW_RESTORE
        } else {
            SW_SHOW
        };
        let _ = ShowWindow(hwnd, cmd);
        // Best-effort: foreground can be refused by the OS focus-stealing rules,
        // but the restore above is what unblanks a minimized capture.
        let _ = SetForegroundWindow(hwnd);
    }
    Ok(Reply::Ack)
}

#[cfg(not(windows))]
pub fn list_windows() -> RemoteResult<Reply> {
    Err(os_error(
        "window enumeration is only supported on Windows".to_owned(),
    ))
}

#[cfg(not(windows))]
pub fn activate_window(_window: WindowId) -> RemoteResult<Reply> {
    Err(os_error(
        "window activation is only supported on Windows".to_owned(),
    ))
}
