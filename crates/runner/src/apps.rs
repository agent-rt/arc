//! Launching applications and enumerating top-level windows.
//!
//! Window enumeration reuses `xcap` (already a dependency for capture), keeping
//! this module free of platform `unsafe`.

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

/// Enumerates top-level windows.
pub fn list_windows() -> RemoteResult<Reply> {
    let windows = xcap::Window::all().map_err(|e| os_error(format!("enumerate windows: {e}")))?;
    let infos = windows
        .into_iter()
        .map(|w| WindowInfo {
            id: WindowId(u64::from(w.id())),
            title: w.title().to_string(),
            process: w.app_name().to_string(),
            focused: false,
        })
        .collect();
    Ok(Reply::Windows(infos))
}
