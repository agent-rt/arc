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

/// Prepends console-encoding setup so non-ASCII output isn't mangled by the
/// host's legacy code page. Windows PowerShell 5.1 and cmd default their console
/// to the OS ANSI/OEM code page (e.g. GBK on a Chinese install, 932 on Japanese),
/// so a UTF-8 world sees mojibake in both directions. Forcing UTF-8 (65001) plus
/// setting `[Console]::OutputEncoding` makes the interpreter's *and* its child
/// processes' output decodable as UTF-8 on our side. The `[Console]` assignment
/// is wrapped in `try/catch` because it throws when no console is attached; the
/// `$OutputEncoding` variable (governing pipes to native tools) always applies.
fn utf8_prefixed(shell: Shell, command: &str) -> String {
    match shell {
        Shell::PowerShell => format!(
            "$OutputEncoding = [System.Text.UTF8Encoding]::new($false); \
             try {{ [Console]::OutputEncoding = [Console]::InputEncoding = $OutputEncoding }} catch {{}}; \
             chcp 65001 > $null; {command}"
        ),
        Shell::Cmd => format!("chcp 65001 > nul & {command}"),
        // sh output is already UTF-8; no code-page dance.
        Shell::Sh => command.to_owned(),
    }
}

/// Builds the process for an inline command string with piped stdio.
fn build(shell: Shell, command: &str, env: &[(String, String)]) -> Command {
    let command = utf8_prefixed(shell, command);
    let mut builder = match shell {
        Shell::PowerShell => {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command", &command]);
            c
        }
        Shell::Cmd => {
            let mut c = Command::new("cmd");
            c.args(["/C", &command]);
            c
        }
        Shell::Sh => {
            let mut c = Command::new("sh");
            c.args(["-c", &command]);
            c
        }
    };
    builder.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    piped(&mut builder);
    builder
}

/// The temp-file extension a shell's script must carry.
fn script_ext(shell: Shell) -> &'static str {
    match shell {
        Shell::PowerShell => "ps1",
        Shell::Cmd => "bat",
        Shell::Sh => "sh",
    }
}

/// Writes `content` to a temp script file keyed by the request `id` (unique
/// per in-flight request), returning its path. The caller deletes it after.
fn write_temp_script(id: RequestId, shell: Shell, content: &str) -> std::io::Result<PathBuf> {
    let mut path = std::env::temp_dir();
    path.push(format!("arc-run-{id}.{}", script_ext(shell)));
    // Windows PowerShell 5.1 decodes a BOM-less `.ps1` as the system ANSI code
    // page (e.g. GBK), so a UTF-8 script's non-ASCII bytes mojibake — and a
    // stray byte can even desync the parser (`missing terminator '`). A UTF-8
    // BOM forces it to read the file as UTF-8. cmd reads `.bat` as the OEM code
    // page and a BOM would corrupt the first line, so only PowerShell gets one.
    match shell {
        Shell::PowerShell => {
            let mut bytes = Vec::with_capacity(content.len() + 3);
            bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            bytes.extend_from_slice(content.as_bytes());
            std::fs::write(&path, bytes)?;
        }
        // cmd (OEM code page) and sh (UTF-8) both take the bytes as-is.
        Shell::Cmd | Shell::Sh => std::fs::write(&path, content)?,
    }
    Ok(path)
}

/// Builds the process that runs the script at `path` with `args`. PowerShell
/// runs with `-ExecutionPolicy Bypass -File` so no policy blocks it and `args`
/// bind to the script's `param()`; cmd runs it via `/C`.
fn build_script(shell: Shell, path: &Path, args: &[String], env: &[(String, String)]) -> Command {
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
        Shell::Sh => {
            let mut c = Command::new("sh");
            c.arg(path).args(args);
            c
        }
    };
    builder.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
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
    env: &[(String, String)],
    timeout_ms: Option<u64>,
) -> RemoteResult<Reply> {
    capture_process(build(shell, command, env), timeout_ms).await
}

/// Writes `content` to a temp script, runs it with `args` (buffered), and
/// deletes the temp file regardless of outcome.
pub async fn run_script(
    id: RequestId,
    shell: Shell,
    content: &str,
    args: &[String],
    env: &[(String, String)],
    timeout_ms: Option<u64>,
) -> RemoteResult<Reply> {
    let path = write_temp_script(id, shell, content)
        .map_err(|e| os_error(format!("writing temp script: {e}")))?;
    let result = capture_process(build_script(shell, &path, args, env), timeout_ms).await;
    let _ = tokio::fs::remove_file(&path).await;
    result
}

/// Launches a script **detached**: writes it to a temp file, spawns it with
/// stdout+stderr redirected to a sibling `.log` file, and returns immediately
/// with the pid and log path. Uses [`std::process`] (not tokio) so the child is
/// never killed on drop — dropping the handle below leaves it running, writing
/// to the log, after this request (and even the connection) ends. The temp
/// script and log are intentionally left on the runner: the process reads the
/// script and keeps writing the log after we return. Follow-up is via `tail`
/// (log) and `ps`/`kill` (pid).
pub fn run_detached(
    id: RequestId,
    shell: Shell,
    content: &str,
    args: &[String],
    env: &[(String, String)],
) -> RemoteResult<Reply> {
    let script = write_temp_script(id, shell, content)
        .map_err(|e| os_error(format!("writing temp script: {e}")))?;
    let mut log_path = std::env::temp_dir();
    log_path.push(format!("arc-detach-{id}.log"));
    let log = std::fs::File::create(&log_path)
        .map_err(|e| os_error(format!("creating log {}: {e}", log_path.display())))?;
    let log_err = log
        .try_clone()
        .map_err(|e| os_error(format!("cloning log handle: {e}")))?;

    let mut builder = match shell {
        Shell::PowerShell => {
            let mut c = std::process::Command::new("powershell");
            c.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]);
            c.arg(&script).args(args);
            c
        }
        Shell::Cmd => {
            let mut c = std::process::Command::new("cmd");
            c.arg("/C").arg(&script).args(args);
            c
        }
        Shell::Sh => {
            let mut c = std::process::Command::new("sh");
            c.arg(&script).args(args);
            c
        }
    };
    builder
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));

    let child = builder
        .spawn()
        .map_err(|e| os_error(format!("spawn failed: {e}")))?;
    let pid = child.id();
    // Drop `child` without waiting: std's Child does not kill on drop, so the
    // process detaches and runs on, redirected to the log file.
    Ok(Reply::Detached {
        pid,
        log_path: log_path.to_string_lossy().into_owned(),
    })
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
    env: &[(String, String)],
    timeout_ms: Option<u64>,
) {
    stream_process(out, id, build(shell, command, env), timeout_ms).await;
}

/// Writes `content` to a temp script, streams its output, then deletes the
/// temp file (the streaming analogue of [`run_script`]).
pub async fn run_script_streaming(
    out: &mpsc::Sender<Frame>,
    id: RequestId,
    shell: Shell,
    content: &str,
    args: &[String],
    env: &[(String, String)],
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
    stream_process(out, id, build_script(shell, &path, args, env), timeout_ms).await;
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
