//! Maps a non-streaming [`Command`] to its [`Reply`] via a [`Backend`] (for OS
//! capabilities) and [`crate::files`] (for the OS-agnostic file ops). Shared by
//! every runner.
//!
//! Not handled here (they are serve-loop concerns, per platform): streaming
//! `RunCommand`/`RunScript` (`stream: true`), and `Forward` (raw tunnel). A
//! runner's serve loop intercepts those and calls this for everything else.

use arc_proto::id::RequestId;
use arc_proto::wire::{Capability, Command, Reply};

use crate::{Backend, RemoteResult, files, invalid};

/// Capabilities every runner has regardless of platform: file transfer is
/// handled here (OS-agnostic [`files`]) and the tunnel by the serve loop, so a
/// backend never declares them — the dispatcher merges them into the report.
const ALWAYS: &[Capability] = &[
    Capability::ReadFile,
    Capability::WriteFile,
    Capability::HashFiles,
    Capability::ListTree,
    Capability::DeleteFile,
    Capability::Forward,
];

/// Executes one request against `backend`, returning its reply. `id` tags the
/// request (used for detached-job temp names).
pub async fn dispatch<B: Backend>(
    backend: &B,
    id: RequestId,
    command: Command,
) -> RemoteResult<Reply> {
    match command {
        // Buffered run (a serve loop handles `stream: true` before calling here).
        Command::RunCommand {
            shell,
            command,
            env,
            timeout_ms,
            ..
        } => backend.run_command(shell, command, env, timeout_ms).await,
        Command::RunScript {
            shell,
            content,
            args,
            env,
            timeout_ms,
            ..
        } => {
            backend
                .run_script(id, shell, content, args, env, timeout_ms)
                .await
        }
        Command::RunDetached {
            shell,
            content,
            args,
            env,
        } => backend.run_detached(id, shell, content, args, env).await,
        Command::Screenshot {
            target,
            format,
            settle_ms,
            settle_await_change,
        } => {
            backend
                .screenshot(target, format, settle_ms, settle_await_change)
                .await
        }
        Command::OpenApp {
            target,
            args,
            watch_ms,
        } => backend.open_app(target, args, watch_ms).await,
        Command::ProcDump { pid } => backend.proc_dump(pid).await,
        Command::ListWindows => backend.list_windows().await,
        Command::ActivateWindow { window } => backend.activate_window(window).await,
        Command::ListElements { window } => backend.list_elements(window).await,
        Command::FindElements {
            window,
            query,
            wait_ms,
        } => backend.find_elements(window, query, wait_ms).await,
        Command::Click { target } => backend.click(target).await,
        Command::TypeText { text, into, paste } => backend.type_text(text, into, paste).await,
        Command::KeyChord { modifiers, key } => backend.key_chord(modifiers, key).await,
        Command::Mouse { action } => backend.mouse(action).await,
        Command::SetValue { element, value } => backend.set_value(element, value).await,
        Command::ReadElement { element } => backend.read_element(element).await,
        Command::FocusElement { element } => backend.focus_element(element).await,
        Command::ClipboardGet => backend.clipboard_get().await,
        Command::ClipboardSet { text } => backend.clipboard_set(text).await,
        Command::ListProcesses { filter, with_cpu } => {
            backend.list_processes(filter, with_cpu).await
        }
        Command::KillProcess { target, dry_run } => backend.kill_process(target, dry_run).await,
        Command::Identity => backend.identity().await,

        // Self-description: the backend's declared ops plus the always-on ones.
        Command::Capabilities => {
            let mut commands = backend.capabilities();
            commands.extend_from_slice(ALWAYS);
            Ok(Reply::Capabilities {
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                runner_version: env!("CARGO_PKG_VERSION").to_string(),
                commands,
            })
        }

        // OS-agnostic file transfer.
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
        Command::ListTree { root, all } => files::list_tree(&root, all).await,
        Command::DeleteFile { path } => files::delete_file(&path).await,

        // Serve-loop concerns should never reach here.
        Command::Forward { .. } => Err(invalid("forward must be handled by the serve loop")),

        // A newer controller sent a command this build doesn't know
        // (`#[serde(other)]` decoded it here) — answer clearly, keep the link.
        Command::Unsupported => Err(invalid(format!(
            "unrecognized command — this runner ({}) is older than the controller; \
             upgrade it",
            env!("CARGO_PKG_VERSION")
        ))),
        other => Err(invalid(format!("command not implemented: {other:?}"))),
    }
}
