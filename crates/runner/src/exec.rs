//! Shell command execution — the keystone of the "develop a Windows app"
//! workflow (build, run, test). Supports both buffered capture and live
//! streaming of output as [`Event`]s.

use std::process::Stdio;
use std::time::Duration;

use arc_proto::id::RequestId;
use arc_proto::wire::{Event, Frame, Reply, Response, Shell};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout, timeout_at};

use crate::dispatch::{RemoteResult, os_error, timeout_error};

/// Builds the process with piped stdio and kill-on-drop.
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
    builder
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
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
    let child = build(shell, command)
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
    let mut child = match build(shell, command).spawn() {
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

    let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));

    // Forward chunks as they arrive.
    loop {
        let received = match deadline {
            Some(at) => match timeout_at(at, rx.recv()).await {
                Ok(chunk) => chunk,
                Err(_) => return kill_and_timeout(out, id, &mut child, timeout_ms).await,
            },
            None => rx.recv().await,
        };
        let Some((stream, chunk)) = received else {
            break; // readers done
        };
        let event = match stream {
            Stream::Stdout => Event::Stdout { id, chunk },
            Stream::Stderr => Event::Stderr { id, chunk },
        };
        if out.send(Frame::Event(event)).await.is_err() {
            return; // writer gone; stop streaming
        }
    }

    // Reap the process for its exit code.
    let status = match deadline {
        Some(at) => match timeout_at(at, child.wait()).await {
            Ok(status) => status,
            Err(_) => return kill_and_timeout(out, id, &mut child, timeout_ms).await,
        },
        None => child.wait().await,
    };
    let result = match status {
        Ok(status) => Ok(Reply::CommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: status.code(),
        }),
        Err(e) => Err(os_error(format!("wait failed: {e}"))),
    };
    let _ = out.send(done(id, result)).await;
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

/// Kills the child and emits a terminal timeout response.
async fn kill_and_timeout(
    out: &mpsc::Sender<Frame>,
    id: RequestId,
    child: &mut tokio::process::Child,
    timeout_ms: Option<u64>,
) {
    let _ = child.start_kill();
    let ms = timeout_ms.unwrap_or(0);
    let _ = out
        .send(done(id, Err(timeout_error(format!("exceeded {ms} ms")))))
        .await;
}

fn done(id: RequestId, result: RemoteResult<Reply>) -> Frame {
    Frame::Response(Response { id, result })
}
