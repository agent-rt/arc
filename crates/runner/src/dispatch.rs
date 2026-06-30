//! Maps an incoming [`Command`] to a [`Reply`] (or [`RemoteError`]) and wraps
//! the outcome in a response [`Frame`].
//!
//! Capture, UI Automation and input injection are blocking, thread-affine
//! operations, so they run on [`tokio::task::spawn_blocking`] rather than
//! occupying an async worker.

use arc_proto::id::RequestId;
use arc_proto::wire::{
    ClickTarget, Command, Frame, RemoteError, RemoteErrorKind, Reply, Request, Response,
};
use tokio::sync::mpsc;

use crate::{apps, capture, exec, files, input, uia};

/// Result alias for per-command handlers: success [`Reply`] or structured
/// [`RemoteError`] returned to the controller.
pub type RemoteResult<T> = Result<T, RemoteError>;

/// Executes a request, sending its outcome frames to `out` (drained by the
/// session writer task). Runs as its own task, so a slow command never blocks
/// the receive loop or other in-flight commands.
///
/// A streaming [`Command::RunCommand`] emits interim
/// [`Event`](arc_proto::wire::Event)s before the terminal response; every
/// other command sends a single response. A closed `out` (writer gone) just
/// ends the handler.
pub async fn handle(request: Request, out: &mpsc::Sender<Frame>) {
    let id = request.id;
    match request.command {
        Command::RunCommand {
            shell,
            command,
            timeout_ms,
            stream: true,
        } => exec::run_command_streaming(out, id, shell, &command, timeout_ms).await,
        Command::RunScript {
            shell,
            content,
            args,
            timeout_ms,
            stream: true,
        } => exec::run_script_streaming(out, id, shell, &content, &args, timeout_ms).await,
        command => {
            let result = dispatch_once(id, command).await;
            let _ = out.send(Frame::Response(Response { id, result })).await;
        }
    }
}

async fn dispatch_once(id: RequestId, command: Command) -> RemoteResult<Reply> {
    match command {
        Command::RunCommand {
            shell,
            command,
            timeout_ms,
            stream: false,
        } => exec::run_command(shell, &command, timeout_ms).await,
        Command::RunScript {
            shell,
            content,
            args,
            timeout_ms,
            stream: false,
        } => exec::run_script(id, shell, &content, &args, timeout_ms).await,
        Command::Screenshot { target } => blocking(move || capture::screenshot(target)).await,
        Command::OpenApp { target, args } => blocking(move || apps::open_app(&target, &args)).await,
        Command::ListWindows => blocking(apps::list_windows).await,
        Command::ListElements { window } => blocking(move || uia::list_elements(window)).await,
        Command::Click { target } => blocking(move || click(target)).await,
        Command::TypeText { text } => blocking(move || input::type_text(&text)).await,
        Command::KeyChord { modifiers, key } => {
            blocking(move || input::key_chord(&modifiers, key)).await
        }
        Command::Mouse { action } => blocking(move || input::mouse(action)).await,
        Command::SetValue { element, value } => {
            blocking(move || uia::set_value(&element.0, &value)).await
        }
        Command::ReadFile {
            path,
            offset,
            max_len,
        } => files::read_file(&path, offset, max_len).await,
        Command::WriteFile {
            path,
            contents,
            offset,
        } => files::write_file(&path, &contents, offset).await,
        Command::HashFiles { root, paths } => files::hash_files(&root, &paths).await,
        Command::ListTree { root } => files::list_tree(&root).await,
        Command::DeleteFile { path } => files::delete_file(&path).await,
        // `Command` is `#[non_exhaustive]`; reject anything added upstream that
        // this runner build does not yet implement.
        other => Err(RemoteError {
            kind: RemoteErrorKind::Invalid,
            message: format!("command not implemented in this runner: {other:?}"),
        }),
    }
}

fn click(target: ClickTarget) -> RemoteResult<Reply> {
    match target {
        ClickTarget::Element(element) => uia::click_element(&element.0),
        ClickTarget::Point { x, y, button } => input::click_point(x, y, button),
    }
}

/// Runs a blocking handler on the blocking thread pool.
async fn blocking<F>(f: F) -> RemoteResult<Reply>
where
    F: FnOnce() -> RemoteResult<Reply> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => Err(os_error(format!("worker task failed: {e}"))),
    }
}

/// Builds an `Os`-category error.
pub fn os_error(message: String) -> RemoteError {
    RemoteError {
        kind: RemoteErrorKind::Os,
        message,
    }
}

/// Builds a `NotFound`-category error.
pub fn not_found(message: String) -> RemoteError {
    RemoteError {
        kind: RemoteErrorKind::NotFound,
        message,
    }
}

/// Builds a `Timeout`-category error.
pub fn timeout_error(message: String) -> RemoteError {
    RemoteError {
        kind: RemoteErrorKind::Timeout,
        message,
    }
}
