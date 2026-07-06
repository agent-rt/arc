//! The Windows [`Backend`]: implements every capability by delegating to the
//! `exec`/`capture`/`apps`/`uia`/`input`/`clipboard` modules. Capture, UI
//! Automation and input injection are blocking, thread-affine (COM / SendInput)
//! operations, so they run on [`tokio::task::spawn_blocking`] rather than
//! occupying an async worker. File transfer is handled by the shared dispatcher.

use arc_proto::id::{ElementId, RequestId, WindowId};
use arc_proto::wire::{
    CaptureTarget, ClickTarget, ElementQuery, ImageFormat, Key, Modifier, MouseAction, Reply, Shell,
};
use arc_runner_core::{Backend, RemoteResult, os_error};

use crate::{apps, capture, clipboard, exec, input, uia};

pub struct WindowsBackend;

impl Backend for WindowsBackend {
    async fn run_command(
        &self,
        shell: Shell,
        command: String,
        env: Vec<(String, String)>,
        timeout_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        exec::run_command(shell, &command, &env, timeout_ms).await
    }

    async fn run_script(
        &self,
        id: RequestId,
        shell: Shell,
        content: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        timeout_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        exec::run_script(id, shell, &content, &args, &env, timeout_ms).await
    }

    async fn run_detached(
        &self,
        id: RequestId,
        shell: Shell,
        content: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> RemoteResult<Reply> {
        blocking(move || exec::run_detached(id, shell, &content, &args, &env)).await
    }

    async fn screenshot(
        &self,
        target: CaptureTarget,
        format: Option<ImageFormat>,
        settle_ms: Option<u64>,
        settle_await_change: bool,
    ) -> RemoteResult<Reply> {
        blocking(move || capture::screenshot(target, format, settle_ms, settle_await_change)).await
    }

    async fn open_app(
        &self,
        target: String,
        args: Vec<String>,
        watch_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        blocking(move || apps::open_app(&target, &args, watch_ms)).await
    }

    async fn proc_dump(&self, pid: u32) -> RemoteResult<Reply> {
        blocking(move || apps::proc_dump(pid)).await
    }

    async fn list_windows(&self) -> RemoteResult<Reply> {
        blocking(apps::list_windows).await
    }

    async fn activate_window(&self, window: WindowId) -> RemoteResult<Reply> {
        blocking(move || apps::activate_window(window)).await
    }

    async fn list_elements(&self, window: WindowId) -> RemoteResult<Reply> {
        blocking(move || uia::list_elements(window)).await
    }

    async fn find_elements(
        &self,
        window: WindowId,
        query: ElementQuery,
        wait_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        blocking(move || uia::find_elements(window, &query, wait_ms)).await
    }

    async fn click(&self, target: ClickTarget) -> RemoteResult<Reply> {
        blocking(move || click(target)).await
    }

    async fn type_text(
        &self,
        text: String,
        into: Option<ElementId>,
        paste: bool,
    ) -> RemoteResult<Reply> {
        blocking(move || {
            if paste {
                // Clipboard paste: set the clipboard, focus the target, Ctrl+V.
                // Far faster/more reliable than per-key injection for long text.
                clipboard::set(&text)?;
                if let Some(element) = into {
                    uia::focus(&element.0)?;
                }
                input::key_chord(&[Modifier::Ctrl], Key::Char('v'))
            } else {
                // Focus the target element first (more reliable than typing into
                // whatever happens to have focus), then send real keys.
                if let Some(element) = into {
                    uia::focus(&element.0)?;
                }
                input::type_text(&text)
            }
        })
        .await
    }

    async fn key_chord(&self, modifiers: Vec<Modifier>, key: Key) -> RemoteResult<Reply> {
        blocking(move || input::key_chord(&modifiers, key)).await
    }

    async fn mouse(&self, action: MouseAction) -> RemoteResult<Reply> {
        blocking(move || input::mouse(action)).await
    }

    async fn set_value(&self, element: ElementId, value: String) -> RemoteResult<Reply> {
        blocking(move || uia::set_value(&element.0, &value)).await
    }

    async fn read_element(&self, element: ElementId) -> RemoteResult<Reply> {
        blocking(move || uia::read_element(&element.0).map(Reply::Text)).await
    }

    async fn focus_element(&self, element: ElementId) -> RemoteResult<Reply> {
        blocking(move || uia::focus(&element.0).map(|()| Reply::Ack)).await
    }

    async fn clipboard_get(&self) -> RemoteResult<Reply> {
        blocking(|| clipboard::get().map(Reply::Text)).await
    }

    async fn clipboard_set(&self, text: String) -> RemoteResult<Reply> {
        blocking(move || clipboard::set(&text).map(|()| Reply::Ack)).await
    }
}

/// Invoke a UI element or click a raw point.
fn click(target: ClickTarget) -> RemoteResult<Reply> {
    match target {
        ClickTarget::Element(element) => uia::click_element(&element.0),
        ClickTarget::Point { x, y, button } => input::click_point(x, y, button),
    }
}

/// Runs a blocking capability handler on the blocking thread pool.
async fn blocking<F>(f: F) -> RemoteResult<Reply>
where
    F: FnOnce() -> RemoteResult<Reply> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => Err(os_error(format!("worker task failed: {e}"))),
    }
}
