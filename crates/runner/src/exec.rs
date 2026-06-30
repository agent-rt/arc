//! Shell command execution — the keystone of the "develop a Windows app"
//! workflow (build, run, test). Supports both buffered capture and live
//! streaming of output as [`Event`]s.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use arc_proto::id::RequestId;
use arc_proto::wire::{Event, Frame, Reply, Response, Shell};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use crate::dispatch::{RemoteResult, os_error, timeout_error};

/// Applies the shared stdio config: no stdin, piped output, kill-on-drop.
fn piped(builder: &mut Command) {
    builder
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
}

/// Builds the process for an inline command string with piped stdio.
fn build(shell: Shell, command: &str) -> Command {
    let mut builder = match shell {
        Shell::PowerShell => {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command", command]);
            c
        }
        Shell::Cmd => {
            let mut c = Command::new("cmd");
            c.args(["/C", command]);
            c
        }
    };
    piped(&mut builder);
    builder
}

/// The temp-file extension a shell's script must carry.
fn script_ext(shell: Shell) -> &'static str {
    match shell {
        Shell::PowerShell => "ps1",
        Shell::Cmd => "bat",
    }
}

/// Writes `content` to a temp script file keyed by the request `id` (unique
/// per in-flight request), returning its path. The caller deletes it after.
fn write_temp_script(id: RequestId, shell: Shell, content: &str) -> std::io::Result<PathBuf> {
    let mut path = std::env::temp_dir();
    path.push(format!("arc-run-{id}.{}", script_ext(shell)));
    std::fs::write(&path, content)?;
    Ok(path)
}

/// Builds the process that runs the script at `path` with `args`. PowerShell
/// runs with `-ExecutionPolicy Bypass -File` so no policy blocks it and `args`
/// bind to the script's `param()`; cmd runs it via `/C`.
fn build_script(shell: Shell, path: &Path, args: &[String]) -> Command {
    let mut builder = match shell {
        Shell::PowerShell => {
            let mut c = Command::new("powershell");
            c.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]);
            c.arg(path).args(args);
            c
        }
        Shell::Cmd => {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(path).args(args);
            c
        }
    };
    piped(&mut builder);
    builder
}

/// Runs `command`, capturing stdout/stderr into a single [`Reply`].
///
/// # Errors
/// Returns a [`RemoteError`](arc_proto::wire::RemoteError) with `Os` on
/// spawn/wait failure or `Timeout` when the deadline elapses.
pub async fn run_command(
    shell: Shell,
    command: &str,
    timeout_ms: Option<u64>,
) -> RemoteResult<Reply> {
    capture_process(build(shell, command), timeout_ms).await
}

/// Writes `content` to a temp script, runs it with `args` (buffered), and
/// deletes the temp file regardless of outcome.
pub async fn run_script(
    id: RequestId,
    shell: Shell,
    content: &str,
    args: &[String],
    timeout_ms: Option<u64>,
) -> RemoteResult<Reply> {
    let path = write_temp_script(id, shell, content)
        .map_err(|e| os_error(format!("writing temp script: {e}")))?;
    let result = capture_process(build_script(shell, &path, args), timeout_ms).await;
    let _ = tokio::fs::remove_file(&path).await;
    result
}

/// Spawns `builder`, capturing stdout/stderr into a single [`Reply`].
async fn capture_process(mut builder: Command, timeout_ms: Option<u64>) -> RemoteResult<Reply> {
    let child = builder
        .spawn()
        .map_err(|e| os_error(format!("spawn failed: {e}")))?;

    let wait = child.wait_with_output();
    let output = match timeout_ms {
        Some(ms) => match timeout(Duration::from_millis(ms), wait).await {
            Ok(result) => result.map_err(|e| os_error(format!("wait failed: {e}")))?,
            Err(_) => return Err(timeout_error(format!("exceeded {ms} ms"))),
        },
        None => wait
            .await
            .map_err(|e| os_error(format!("wait failed: {e}")))?,
    };

    Ok(Reply::CommandOutput {
        // Windows console output is rarely valid UTF-8; decode lossily for now.
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code(),
    })
}

/// Which stream a chunk came from.
#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// Runs `command`, streaming output to the controller as [`Event`]s and then
/// sending a terminal [`Response`] (with empty buffers — the bytes were already
/// streamed). The controller reassembles the full output from the events.
///
/// Frames are sent to `out` (the session writer's outbox); command-level
/// failures are delivered *as* the terminal response. A closed `out` (writer
/// gone) ends streaming early.
pub async fn run_command_streaming(
    out: &mpsc::Sender<Frame>,
    id: RequestId,
    shell: Shell,
    command: &str,
    timeout_ms: Option<u64>,
) {
    stream_process(out, id, build(shell, command), timeout_ms).await;
}

/// Writes `content` to a temp script, streams its output, then deletes the
/// temp file (the streaming analogue of [`run_script`]).
pub async fn run_script_streaming(
    out: &mpsc::Sender<Frame>,
    id: RequestId,
    shell: Shell,
    content: &str,
    args: &[String],
    timeout_ms: Option<u64>,
) {
    let path = match write_temp_script(id, shell, content) {
        Ok(path) => path,
        Err(e) => {
            let _ = out
                .send(done(id, Err(os_error(format!("writing temp script: {e}")))))
                .await;
            return;
        }
    };
    stream_process(out, id, build_script(shell, &path, args), timeout_ms).await;
    let _ = tokio::fs::remove_file(&path).await;
}

/// Spawns `builder`, streaming output to the controller as [`Event`]s and then
/// a terminal [`Response`] (empty buffers — the bytes were already streamed).
async fn stream_process(
    out: &mpsc::Sender<Frame>,
    id: RequestId,
    mut builder: Command,
    timeout_ms: Option<u64>,
) {
    let mut child = match builder.spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = out
                .send(done(id, Err(os_error(format!("spawn failed: {e}")))))
                .await;
            return;
        }
    };

    let (tx, mut rx) = mpsc::channel::<(Stream, String)>(64);
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(pump(stdout, Stream::Stdout, tx.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(pump(stderr, Stream::Stderr, tx.clone()));
    }
    drop(tx); // so `rx` closes once both reader tasks finish

    // A monitor task owns the child: it waits for the *direct* child to exit
    // (killing it on timeout) and reports the outcome. We finish when the child
    // exits — NOT when the pipes hit EOF — because a detached grandchild (e.g.
    // `start "" app.exe`) inherits the pipe handles and would hold them open
    // forever, hanging the request.
    let (done_tx, mut done_rx) = oneshot::channel::<Outcome>();
    tokio::spawn(async move {
        let outcome = match timeout_ms {
            Some(ms) => match timeout(Duration::from_millis(ms), child.wait()).await {
                Ok(status) => Outcome::Exited(status),
                Err(_) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    Outcome::Timeout(ms)
                }
            },
            None => Outcome::Exited(child.wait().await),
        };
        let _ = done_tx.send(outcome);
    });

    // Forward output until the child finishes (or pipes close), then drain
    // whatever is already buffered without blocking on inherited pipe handles.
    let outcome = loop {
        tokio::select! {
            biased;
            chunk = rx.recv() => match chunk {
                Some((stream, chunk)) => {
                    if forward(out, id, stream, chunk).await.is_err() {
                        return; // writer gone
                    }
                }
                None => break (&mut done_rx).await.unwrap_or(Outcome::Lost),
            },
            res = &mut done_rx => {
                while let Ok(Some((stream, chunk))) =
                    timeout(Duration::from_millis(50), rx.recv()).await
                {
                    if forward(out, id, stream, chunk).await.is_err() {
                        return;
                    }
                }
                break res.unwrap_or(Outcome::Lost);
            }
        }
    };

    let result = match outcome {
        Outcome::Exited(Ok(status)) => Ok(Reply::CommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: status.code(),
        }),
        Outcome::Exited(Err(e)) => Err(os_error(format!("wait failed: {e}"))),
        Outcome::Timeout(ms) => Err(timeout_error(format!("exceeded {ms} ms"))),
        Outcome::Lost => Err(os_error("child monitor task ended unexpectedly".to_owned())),
    };
    let _ = out.send(done(id, result)).await;
}

/// Outcome of waiting on a streamed child process.
enum Outcome {
    /// The direct child exited with this status.
    Exited(std::io::Result<std::process::ExitStatus>),
    /// The deadline (`ms`) elapsed and the child was killed.
    Timeout(u64),
    /// The monitor task vanished before reporting (should not happen).
    Lost,
}

/// Sends one output chunk as an [`Event`]; `Err` means the writer is gone.
async fn forward(
    out: &mpsc::Sender<Frame>,
    id: RequestId,
    stream: Stream,
    chunk: String,
) -> Result<(), ()> {
    let event = match stream {
        Stream::Stdout => Event::Stdout { id, chunk },
        Stream::Stderr => Event::Stderr { id, chunk },
    };
    out.send(Frame::Event(event)).await.map_err(|_| ())
}

/// Reads a child stream in chunks, forwarding lossily-decoded text.
async fn pump<R: AsyncReadExt + Unpin>(
    mut reader: R,
    stream: Stream,
    tx: mpsc::Sender<(Stream, String)>,
) {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&buffer[..n]).into_owned();
                if tx.send((stream, text)).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn done(id: RequestId, result: RemoteResult<Reply>) -> Frame {
    Frame::Response(Response { id, result })
}
