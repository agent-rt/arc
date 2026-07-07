use std::io::Write;

use anyhow::{Context, Result, bail};
use arc_net::Controller;
use arc_proto::wire::{Capability, Command, Event, ProcessInfo, Reply, Shell};
use tokio::sync::mpsc;

/// A runner's self-description, from [`Command::Capabilities`].
pub(crate) struct RunnerCaps {
    pub os: String,
    pub arch: String,
    pub runner_version: String,
    pub commands: Vec<Capability>,
}

impl RunnerCaps {
    /// Whether the runner implements a given operation.
    pub fn has(&self, cap: Capability) -> bool {
        self.commands.contains(&cap)
    }
}

/// Asks the runner what it is and can do. `Ok(None)` means the runner predates
/// the `Capabilities` command (it answers with a non-fatal "unsupported" error,
/// so the link stays up) — callers treat that as a legacy Windows runner.
pub(crate) async fn fetch_capabilities(controller: &mut Controller) -> Result<Option<RunnerCaps>> {
    match controller.request(Command::Capabilities).await {
        Ok(Reply::Capabilities {
            os,
            arch,
            runner_version,
            commands,
        }) => Ok(Some(RunnerCaps {
            os,
            arch,
            runner_version,
            commands,
        })),
        Ok(other) => bail!("unexpected reply: {other:?}"),
        // A non-fatal remote error is a runner too old to know the command.
        Err(e) if !e.is_fatal() => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `arc capabilities`: print the runner's OS/arch/version and command surface.
pub(crate) async fn capabilities(controller: &mut Controller) -> Result<i32> {
    match fetch_capabilities(controller).await? {
        Some(caps) => {
            println!(
                "runner   : arc-runner {} ({}/{})",
                caps.runner_version, caps.os, caps.arch
            );
            let mut names: Vec<String> = caps.commands.iter().map(|c| format!("{c:?}")).collect();
            names.sort();
            println!("commands : {}", names.join(", "));
        }
        None => println!(
            "runner   : legacy — predates capability reporting (assume a full Windows surface)"
        ),
    }
    Ok(0)
}

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

/// Lists remote processes (semantic — works on any OS). `pattern` filters by
/// name substring; `cpu` adds a CPU% column and sorts by it (busy vs blocked),
/// else the list is sorted by memory.
pub(crate) async fn ps(
    controller: &mut Controller,
    pattern: Option<&str>,
    cpu: bool,
) -> Result<i32> {
    let procs = match controller
        .request(Command::ListProcesses {
            filter: pattern.map(str::to_owned),
            with_cpu: cpu,
        })
        .await?
    {
        Reply::Processes(p) => p,
        other => bail!("unexpected reply: {other:?}"),
    };
    print_processes(&procs, cpu);
    Ok(0)
}

/// Prints an aligned `PID [CPU%] MB NAME` table (MB from the KiB the runner
/// reports; blank where the backend didn't measure a field).
fn print_processes(procs: &[ProcessInfo], with_cpu: bool) {
    if procs.is_empty() {
        println!("(no matching processes)");
        return;
    }
    let mb = |p: &ProcessInfo| {
        p.memory_kb
            .map_or_else(String::new, |k| format!("{:.1}", k as f64 / 1024.0))
    };
    if with_cpu {
        println!("{:>7}  {:>6}  {:>9}  NAME", "PID", "CPU%", "MB");
        for p in procs {
            let cpu = p
                .cpu_percent
                .map_or_else(String::new, |c| format!("{c:.1}"));
            println!("{:>7}  {:>6}  {:>9}  {}", p.pid, cpu, mb(p), p.name);
        }
    } else {
        println!("{:>7}  {:>9}  NAME", "PID", "MB");
        for p in procs {
            println!("{:>7}  {:>9}  {}", p.pid, mb(p), p.name);
        }
    }
}

/// Self-check: reports link, runner identity, **session activity tier**, and a
/// UIA smoke count, then interprets which capabilities are available right now.
/// The session tier is the recurring gotcha — UIA + per-window capture work even
/// when RDP is disconnected, but raw input and full-screen capture need an active
/// session. Capability-aware: it first asks the runner what it is, so it skips
/// the Windows-only identity/session probe on other platforms (where those
/// concepts don't apply) instead of printing a wall of `?`.
pub(crate) async fn doctor(controller: &mut Controller) -> Result<i32> {
    use std::collections::HashMap;

    // What is this runner? `None` = a legacy runner (pre-capabilities), which
    // was Windows-only, so treat the unknown as Windows and run the full probe.
    let caps = fetch_capabilities(controller).await?;
    let is_windows = caps.as_ref().is_none_or(|c| c.os == "windows");
    // A capability is assumed present on a legacy runner (full Windows surface).
    let supports = |c: Capability| caps.as_ref().is_none_or(|caps| caps.has(c));

    println!("arc doctor");
    println!("  link         : connected");
    match &caps {
        Some(c) => println!(
            "  runner       : arc-runner {} ({}/{})",
            c.runner_version, c.os, c.arch
        ),
        None => println!("  runner       : legacy (predates capability reporting)"),
    }

    // Windows-only: identity (account/admin/integrity) + the RDP session tier.
    // These are meaningless on other OSes, so they're skipped there.
    let mut state = String::new();
    let mut keep_display = String::new();
    if is_windows {
        let probe = r#"
$id=[Security.Principal.WindowsIdentity]::GetCurrent()
Write-Output "account=$($id.Name)"
Write-Output "admin=$(([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator))"
Write-Output "integrity=$((whoami /groups /fo csv | ConvertFrom-Csv | Where-Object { $_.SID -like 'S-1-16-*' }).'Group Name')"
$sid=(Get-Process -Id $PID).SessionId
Write-Output "session=$sid"
$state='unknown'
foreach ($line in (query session 2>$null)) { if ($line -match "\s$sid\s+(Active|Disc|Listen)\b") { $state=$Matches[1] } }
Write-Output "sessionState=$state"
Write-Output "keepDisplay=$(if (schtasks /query /tn arc-keep-display 2>$null) {'present'} else {'absent'})"
"#;
        let stdout = match controller
            .request(Command::RunCommand {
                shell: Shell::PowerShell,
                command: probe.to_owned(),
                env: Vec::new(),
                timeout_ms: timeout_to_ms(Some(30)),
                stream: false,
            })
            .await?
        {
            Reply::CommandOutput { stdout, .. } => stdout,
            other => bail!("unexpected reply: {other:?}"),
        };
        let kv: HashMap<&str, &str> = stdout
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect();
        let get = |k: &str| kv.get(k).copied().unwrap_or("?");
        state = get("sessionState").to_string();
        keep_display = get("keepDisplay").to_string();
        println!("  account      : {}", get("account"));
        println!("  admin        : {}", get("admin"));
        println!("  integrity    : {}", get("integrity"));
        println!("  session      : {} ({state})", get("session"));
        println!("  keep-display : {keep_display}");
    }

    // UIA smoke count — only if the runner enumerates windows at all.
    if supports(Capability::ListWindows) {
        let windows = match controller.request(Command::ListWindows).await? {
            Reply::Windows(w) => w.len(),
            other => bail!("unexpected reply: {other:?}"),
        };
        println!("  uia          : {windows} top-level windows visible");
    }

    // On non-Windows, list the actual command surface (there's no session tier
    // to interpret; what matters is which ops exist).
    if let Some(c) = caps.as_ref().filter(|_| !is_windows) {
        let mut names: Vec<String> = c.commands.iter().map(|x| format!("{x:?}")).collect();
        names.sort();
        println!("  commands     : {}", names.join(", "));
    }

    println!();
    if is_windows {
        if state == "Active" {
            println!(
                "session Active → all capabilities work: UIA, per-window & full-screen capture, raw input (type/key/mouse)."
            );
        } else {
            let state = if state.is_empty() { "?" } else { &state };
            let keep = if keep_display.is_empty() {
                "?"
            } else {
                &keep_display
            };
            println!(
                "session {state} → UIA (windows/elements/click/set/read) and per-window \
                 screencap/shot work; raw input (type/key/mouse) and full-screen capture \
                 need an Active session — connect RDP, or rely on keep-display ({keep})."
            );
        }
    } else {
        let os = caps.as_ref().map_or("?", |c| c.os.as_str());
        println!(
            "runner OS {os} — UI automation runs through the platform's accessibility layer; \
             the Windows session/integrity tiers above don't apply. `commands` lists what's \
             implemented; anything absent returns an unsupported error."
        );
    }
    Ok(0)
}

/// Reports the runner's identity — account, integrity level, admin, session —
/// so it's obvious whether a command runs as a real (non-elevated) user or an
/// admin (which can mask AV/UAC/permission issues a real user would hit).
pub(crate) async fn whoami(controller: &mut Controller) -> Result<i32> {
    let lines = match controller.request(Command::Identity).await? {
        Reply::Identity(l) => l,
        other => bail!("unexpected reply: {other:?}"),
    };
    let width = lines.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, v) in &lines {
        println!("{k:<width$} : {v}");
    }
    Ok(0)
}

/// Kills a remote process by PID (all-digit `target`) or by exact name. With
/// `dry_run`, lists the matching processes instead of killing them. Semantic —
/// the runner maps it to its OS's mechanism, so it works cross-platform.
pub(crate) async fn kill(controller: &mut Controller, target: &str, dry_run: bool) -> Result<i32> {
    let procs = match controller
        .request(Command::KillProcess {
            target: target.to_owned(),
            dry_run,
        })
        .await?
    {
        Reply::Processes(p) => p,
        other => bail!("unexpected reply: {other:?}"),
    };
    if procs.is_empty() {
        println!("no matching process: {target}");
        return Ok(1);
    }
    let verb = if dry_run { "would kill" } else { "killed" };
    for p in &procs {
        println!("{verb} {} (PID {})", p.name, p.pid);
    }
    Ok(0)
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
    // The runner's default interpreter (sh on a Unix runner, PowerShell on
    // Windows/legacy), used when neither `--lang` nor the file extension decides.
    let default_shell = default_shell_for_runner(controller).await?;
    let (shell, content) = if script == "-" {
        // stdin has no extension to infer from — use the runner's default shell
        // unless `--lang` says otherwise.
        let shell = shell_for_lang(lang)?.unwrap_or(default_shell);
        let mut content = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut content)
            .context("reading script from stdin")?;
        (shell, content)
    } else {
        // `--lang` overrides the extension; otherwise infer from the extension.
        let shell = match shell_for_lang(lang)? {
            Some(s) => s,
            None => shell_for_script(script, default_shell)?,
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

/// The runner's default script interpreter, from the capability handshake:
/// `sh` on a non-Windows runner (Android/Linux), else PowerShell (also the
/// fallback for a legacy runner that predates the handshake — those are Windows).
async fn default_shell_for_runner(controller: &mut Controller) -> Result<Shell> {
    let os = fetch_capabilities(controller).await?.map(|c| c.os);
    Ok(match os.as_deref() {
        Some("windows") | None => Shell::PowerShell,
        Some(_) => Shell::Sh,
    })
}

/// Maps an explicit `--lang` (`ps1`/`bat`/`cmd`/`sh`, with or without a leading
/// dot) to an interpreter. `None` means the flag was omitted (fall back to the
/// extension); an unrecognized value is an error.
fn shell_for_lang(lang: Option<&str>) -> Result<Option<Shell>> {
    match lang.map(|l| l.trim_start_matches('.').to_ascii_lowercase()) {
        None => Ok(None),
        Some(l) => match l.as_str() {
            "ps1" | "powershell" | "pwsh" => Ok(Some(Shell::PowerShell)),
            "bat" | "cmd" => Ok(Some(Shell::Cmd)),
            "sh" | "bash" => Ok(Some(Shell::Sh)),
            other => bail!("unsupported --lang `{other}` (expected ps1, bat, cmd, or sh)"),
        },
    }
}

/// Picks the interpreter for a script by its file extension. An unknown or
/// missing extension falls back to the runner's `default` shell — except a
/// Windows/legacy runner (default PowerShell) keeps the strict error, since its
/// interpreters won't run arbitrary content and the guidance is helpful.
fn shell_for_script(script: &str, default: Shell) -> Result<Shell> {
    let ext = std::path::Path::new(script)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("ps1") => Ok(Shell::PowerShell),
        Some("bat" | "cmd") => Ok(Shell::Cmd),
        Some("sh" | "bash") => Ok(Shell::Sh),
        _ if default == Shell::Sh => Ok(Shell::Sh),
        Some(other) => {
            bail!("unsupported script type `.{other}` (expected .ps1, .bat, .cmd, or .sh)")
        }
        None => bail!("`{script}` has no extension; expected .ps1, .bat, .cmd, or .sh"),
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
