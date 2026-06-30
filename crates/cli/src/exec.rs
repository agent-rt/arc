use std::io::Write;

use anyhow::{Context, Result, bail};
use arc_net::Controller;
use arc_proto::wire::{Command, Event, Reply, Shell};
use tokio::sync::mpsc;

pub(crate) async fn shell(
    controller: &mut Controller,
    use_cmd: bool,
    timeout_secs: Option<u64>,
    args: Vec<String>,
) -> Result<i32> {
    let shell = if use_cmd {
        Shell::Cmd
    } else {
        Shell::PowerShell
    };
    let command = args.join(" ");
    stream_run(
        controller,
        Command::RunCommand {
            shell,
            command,
            timeout_ms: timeout_to_ms(timeout_secs),
            stream: true,
        },
    )
    .await
}

/// Lists remote processes via PowerShell, optionally filtered by name substring.
pub(crate) async fn ps(controller: &mut Controller, pattern: Option<&str>) -> Result<i32> {
    let filter = match pattern {
        Some(p) => format!(
            " | Where-Object {{ $_.ProcessName -like '*{}*' }}",
            p.replace('\'', "''")
        ),
        None => String::new(),
    };
    let command = format!(
        "Get-Process{filter} | Sort-Object -Descending WS | \
         Select-Object Id, ProcessName, @{{Name='MB';Expression={{[math]::Round($_.WS/1MB,1)}}}} | \
         Format-Table -AutoSize | Out-String -Width 200"
    );
    stream_run(
        controller,
        Command::RunCommand {
            shell: Shell::PowerShell,
            command,
            timeout_ms: timeout_to_ms(Some(30)),
            stream: true,
        },
    )
    .await
}

/// Kills a remote process by PID (all-digit `target`) or by name (`-Force`).
/// With `dry_run`, lists the matching processes instead of killing them.
pub(crate) async fn kill(controller: &mut Controller, target: &str, dry_run: bool) -> Result<i32> {
    // The process set to act on, by PID or by name (with/without a `.exe`).
    let selector = if target.chars().all(|c| c.is_ascii_digit()) {
        format!("Get-Process -Id {target} -ErrorAction Stop")
    } else {
        let name = target
            .strip_suffix(".exe")
            .unwrap_or(target)
            .replace('\'', "''");
        format!("Get-Process -Name '{name}' -ErrorAction Stop")
    };
    let command = if dry_run {
        format!("{selector} | ForEach-Object {{ \"would kill $($_.ProcessName) (PID $($_.Id))\" }}")
    } else {
        format!(
            "{selector} | Stop-Process -Force -PassThru | \
             ForEach-Object {{ \"killed $($_.ProcessName) (PID $($_.Id))\" }}"
        )
    };
    stream_run(
        controller,
        Command::RunCommand {
            shell: Shell::PowerShell,
            command,
            timeout_ms: timeout_to_ms(Some(30)),
            stream: true,
        },
    )
    .await
}

/// Streams the tail of a remote file via PowerShell `Get-Content`. With
/// `follow`, uses `-Wait` and no timeout so appended lines stream until the user
/// interrupts — the remote-log companion to `tail -f`.
pub(crate) async fn tail(
    controller: &mut Controller,
    remote: &str,
    lines: u64,
    follow: bool,
) -> Result<i32> {
    // Single-quote the path for PowerShell (doubling any embedded quote) so
    // spaces and most metacharacters are taken literally.
    let escaped = remote.replace('\'', "''");
    let wait = if follow { " -Wait" } else { "" };
    let command = format!("Get-Content -LiteralPath '{escaped}' -Tail {lines}{wait}");
    stream_run(
        controller,
        Command::RunCommand {
            shell: Shell::PowerShell,
            command,
            // No timeout: a follow runs until interrupted, and a plain tail is quick.
            timeout_ms: None,
            stream: true,
        },
    )
    .await
}

/// Reads a local script and runs it on the runner via [`Command::RunScript`] —
/// shipping its contents (no pre-`push`, no shell quoting) under the
/// interpreter inferred from its extension.
pub(crate) async fn run_script(
    controller: &mut Controller,
    script: &str,
    timeout_secs: Option<u64>,
    args: Vec<String>,
) -> Result<i32> {
    let shell = shell_for_script(script)?;
    let content = std::fs::read_to_string(script).with_context(|| format!("reading {script}"))?;
    stream_run(
        controller,
        Command::RunScript {
            shell,
            content,
            args,
            timeout_ms: timeout_to_ms(timeout_secs),
            stream: true,
        },
    )
    .await
}

/// Picks the interpreter for a script by its file extension.
fn shell_for_script(script: &str) -> Result<Shell> {
    let ext = std::path::Path::new(script)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("ps1") => Ok(Shell::PowerShell),
        Some("bat" | "cmd") => Ok(Shell::Cmd),
        Some(other) => bail!("unsupported script type `.{other}` (expected .ps1, .bat, or .cmd)"),
        None => bail!("`{script}` has no extension; expected .ps1, .bat, or .cmd"),
    }
}

/// Omitted → default safety limit; explicit `0` → no limit; else seconds → ms.
fn timeout_to_ms(timeout_secs: Option<u64>) -> Option<u64> {
    match timeout_secs {
        None => Some(arc_proto::wire::DEFAULT_COMMAND_TIMEOUT_MS),
        Some(0) => None,
        Some(secs) => Some(secs.saturating_mul(1000)),
    }
}

/// Runs a streaming command, printing stdout/stderr live, and returns its exit
/// code. Shared by `shell` and `run`.
pub(crate) async fn stream_run(controller: &mut Controller, command: Command) -> Result<i32> {
    let (tx, mut rx) = mpsc::channel::<Event>(256);
    let printer = tokio::spawn(async move {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        while let Some(event) = rx.recv().await {
            match event {
                Event::Stdout { chunk, .. } => {
                    let _ = out.write_all(chunk.as_bytes());
                    let _ = out.flush();
                }
                Event::Stderr { chunk, .. } => {
                    let _ = err.write_all(chunk.as_bytes());
                    let _ = err.flush();
                }
                Event::Progress { message, .. } => {
                    let _ = writeln!(err, "{message}");
                }
            }
        }
    });

    let reply = controller.request_streaming(command, &tx).await?;
    drop(tx);
    let _ = printer.await;

    match reply {
        Reply::CommandOutput {
            stdout,
            stderr,
            exit_code,
        } => {
            // A pre-streaming runner returns full buffers instead of events.
            print!("{stdout}");
            eprint!("{stderr}");
            let _ = std::io::stdout().flush();
            Ok(exit_code.unwrap_or(0))
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}
