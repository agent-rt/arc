//! Routes a request to its reply. A streaming `RunCommand`/`RunScript`
//! (`stream: true`) is handled here (it emits interim
//! [`Event`](arc_proto::wire::Event)s before the terminal response); every other
//! command goes through the shared [`arc_runner_core::dispatch`] against the
//! [`WindowsBackend`](crate::backend::WindowsBackend). ([`Command::Forward`] is
//! intercepted earlier still, in the serve loop.)

use arc_proto::wire::{Command, Frame, Request, Response};
use tokio::sync::mpsc;

use crate::backend::WindowsBackend;
use crate::exec;

// Re-export the shared result alias + error constructors so the capability
// modules (`exec`/`apps`/`uia`/`input`/`capture`/`clipboard`) keep importing them
// from here after the move into `arc-runner-core`.
pub use arc_runner_core::{RemoteResult, not_found, os_error, timeout_error};

/// Executes a request, sending its outcome frames to `out` (drained by the
/// session writer task). Runs as its own task, so a slow command never blocks
/// the receive loop or other in-flight commands. A closed `out` (writer gone)
/// just ends the handler.
pub async fn handle(request: Request, out: &mpsc::Sender<Frame>) {
    let id = request.id;
    match request.command {
        Command::RunCommand {
            shell,
            command,
            env,
            timeout_ms,
            stream: true,
        } => exec::run_command_streaming(out, id, shell, &command, &env, timeout_ms).await,
        Command::RunScript {
            shell,
            content,
            args,
            env,
            timeout_ms,
            stream: true,
        } => exec::run_script_streaming(out, id, shell, &content, &args, &env, timeout_ms).await,
        command => {
            let result = arc_runner_core::dispatch::dispatch(&WindowsBackend, id, command).await;
            let _ = out.send(Frame::Response(Response { id, result })).await;
        }
    }
}
