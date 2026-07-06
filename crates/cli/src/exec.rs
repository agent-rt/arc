use std::io::Write;

use anyhow::{Context, Result, bail};
use arc_net::Controller;
use arc_proto::wire::{Command, Event, Reply, Shell};
use tokio::sync::mpsc;

pub(crate) async fn shell(
    controller: &mut Controller,
    use_cmd: bool,
    timeout_secs: Option<u64>,
    env: Vec<String>,
    env_file: Option<String>,
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
            env: parse_env(&env, env_file.as_deref())?,
            timeout_ms: timeout_to_ms(timeout_secs),
            stream: true,
        },
    )
    .await
}

/// Parses `--env KEY=VAL` pairs and an optional `--env-file` into `(name, value)`
/// tuples. File lines are `KEY=VALUE`; blank lines and `#` comments are skipped.
/// Explicit `--env` flags are applied after (and so override) the file.
pub(crate) fn parse_env(pairs: &[String], file: Option<&str>) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    if let Some(path) = file {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading env file {path}"))?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .with_context(|| format!("env file line missing '=': {line}"))?;
            out.push((k.trim().to_owned(), v.to_owned()));
        }
    }
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .with_context(|| format!("--env expects KEY=VALUE, got: {p}"))?;
        out.push((k.to_owned(), v.to_owned()));
    }
    Ok(out)
}

/// Lists remote processes via PowerShell, optionally filtered by name substring.
/// With `cpu`, samples `TotalProcessorTime` twice ~500ms apart to add an
/// instantaneous, per-core-normalized CPU% column (busy vs blocked) and sorts by
/// it; without it, the fast working-set listing sorted by memory.
pub(crate) async fn ps(
    controller: &mut Controller,
    pattern: Option<&str>,
    cpu: bool,
) -> Result<i32> {
    stream_run(
        controller,
        Command::RunCommand {
            shell: Shell::PowerShell,
            command: ps_command(pattern, cpu),
            env: Vec::new(),
            timeout_ms: timeout_to_ms(Some(30)),
            stream: true,
        },
    )
    .await
}

/// Builds the `Get-Process` PowerShell for `arc ps` and the MCP `list_processes`
/// tool (shared so they can't drift). `pattern` filters by name substring; `cpu`
/// adds an instantaneous, per-core-normalized CPU% column (two
/// `TotalProcessorTime` samples ~500ms apart) and sorts by it — a spinning
/// process reads high, a blocked one ~0%, the busy-vs-blocked signal for hangs.
pub(crate) fn ps_command(pattern: Option<&str>, cpu: bool) -> String {
    let filter = match pattern {
        Some(p) => format!(
            " | Where-Object {{ $_.ProcessName -like '*{}*' }}",
            p.replace('\'', "''")
        ),
        None => String::new(),
    };
    if cpu {
        format!(
            "$n=[Environment]::ProcessorCount; $a=@{{}}; \
             Get-Process{filter} | ForEach-Object {{ $a[$_.Id]=$_.TotalProcessorTime.TotalMilliseconds }}; \
             Start-Sleep -Milliseconds 500; \
             Get-Process{filter} | Select-Object Id, ProcessName, \
               @{{Name='CPU%';Expression={{ $p=$a[$_.Id]; \
                 if ($null -ne $p -and $_.TotalProcessorTime) \
                 {{ [math]::Round((($_.TotalProcessorTime.TotalMilliseconds-$p)/500/$n)*100,1) }} else {{ 0 }} }}}}, \
               @{{Name='MB';Expression={{[math]::Round($_.WS/1MB,1)}}}} | \
             Sort-Object -Descending 'CPU%' | Format-Table -AutoSize | Out-String -Width 200"
        )
    } else {
        format!(
            "Get-Process{filter} | Sort-Object -Descending WS | \
             Select-Object Id, ProcessName, @{{Name='MB';Expression={{[math]::Round($_.WS/1MB,1)}}}} | \
             Format-Table -AutoSize | Out-String -Width 200"
        )
    }
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
            env: Vec::new(),
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
            env: Vec::new(),
            // No timeout: a follow runs until interrupted, and a plain tail is quick.
            timeout_ms: None,
            stream: true,
        },
    )
    .await
}

/// Reads a script (a local file, or `-` for stdin) and runs it on the runner via
/// [`Command::RunScript`] — shipping its contents (no pre-`push`, no shell
/// quoting) under the interpreter from `--lang`, or inferred from the extension.
/// Reading from `-` lets an Agent pipe a multi-line here-doc straight through
/// with zero quoting to escape.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_script(
    controller: &mut Controller,
    script: &str,
    lang: Option<&str>,
    detach: bool,
    timeout_secs: Option<u64>,
    env: Vec<String>,
    env_file: Option<String>,
    args: Vec<String>,
) -> Result<i32> {
    let (shell, content) = if script == "-" {
        // stdin has no extension to infer from — default to PowerShell (arc's
        // default shell) unless `--lang` says otherwise.
        let shell = shell_for_lang(lang)?.unwrap_or(Shell::PowerShell);
        let mut content = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut content)
            .context("reading script from stdin")?;
        (shell, content)
    } else {
        // `--lang` overrides the extension; otherwise infer from the extension.
        let shell = match shell_for_lang(lang)? {
            Some(s) => s,
            None => shell_for_script(script)?,
        };
        let content =
            std::fs::read_to_string(script).with_context(|| format!("reading {script}"))?;
        (shell, content)
    };
    let env = parse_env(&env, env_file.as_deref())?;

    if detach {
        // Fire-and-forget: the runner redirects output to a log file and returns
        // the pid + log path immediately, so a long task doesn't block the call.
        return match controller
            .request(Command::RunDetached {
                shell,
                content,
                args,
                env,
            })
            .await?
        {
            Reply::Detached { pid, log_path } => {
                println!("detached pid={pid}");
                println!("log:    {log_path}");
                println!("follow: arc tail -f '{log_path}'   (or: arc kill {pid})");
                Ok(0)
            }
            other => bail!("unexpected reply: {other:?}"),
        };
    }

    stream_run(
        controller,
        Command::RunScript {
            shell,
            content,
            args,
            env,
            timeout_ms: timeout_to_ms(timeout_secs),
            stream: true,
        },
    )
    .await
}

/// Maps an explicit `--lang` (`ps1`/`bat`/`cmd`, with or without a leading dot)
/// to an interpreter. `None` means the flag was omitted (fall back to the
/// extension); an unrecognized value is an error.
fn shell_for_lang(lang: Option<&str>) -> Result<Option<Shell>> {
    match lang.map(|l| l.trim_start_matches('.').to_ascii_lowercase()) {
        None => Ok(None),
        Some(l) => match l.as_str() {
            "ps1" | "powershell" | "pwsh" => Ok(Some(Shell::PowerShell)),
            "bat" | "cmd" => Ok(Some(Shell::Cmd)),
            other => bail!("unsupported --lang `{other}` (expected ps1, bat, or cmd)"),
        },
    }
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
