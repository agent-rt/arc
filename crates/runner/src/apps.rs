//! Launching applications and enumerating top-level windows.

use arc_proto::id::WindowId;
use arc_proto::wire::{Reply, WindowInfo};

use crate::dispatch::{RemoteResult, os_error};

/// Launches an application, returning its process id.
///
/// The main window is not resolved synchronously (apps surface windows
/// asynchronously); callers can follow up with [`list_windows`].
pub fn open_app(target: &str, args: &[String]) -> RemoteResult<Reply> {
    let child = std::process::Command::new(target)
        .args(args)
        .spawn()
        .map_err(|e| os_error(format!("failed to launch '{target}': {e}")))?;
    Ok(Reply::AppOpened {
        window: None,
        pid: child.id(),
    })
}

/// Enumerates visible, titled top-level windows.
#[cfg(windows)]
pub fn list_windows() -> RemoteResult<Reply> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetForegroundWindow, IsWindowVisible};

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
            })
        })
        .collect();
    Ok(Reply::Windows(infos))
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
    path.rsplit(['\\', '/'])
        .next()
        .unwrap_or(&path)
        .to_owned()
}

#[cfg(not(windows))]
pub fn list_windows() -> RemoteResult<Reply> {
    Err(os_error("window enumeration is only supported on Windows".to_owned()))
}
