//! `arc` — an adb-style CLI for arc.
//!
//! A thin client over the shared [`Controller`] transport: run shell commands
//! (output streams live), push/pull files (chunked), capture screenshots, and
//! drive UI Automation — without the verbosity of an MCP tool call.
//!
//! Config comes from `--relay/--session/--pairing` or the `ARC_RELAY_URL`
//! / `ARC_SESSION` / `ARC_PAIRING` environment variables.

#![forbid(unsafe_code)]

mod mcp;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use arc_net::{Controller, SessionConfig, Transport};
use arc_proto::id::{ElementId, PairingCode, SessionId, WindowId};
use arc_proto::wire::{
    CaptureTarget, ClickTarget, Command, ElementInfo, ElementQuery, Event, ImageFormat,
    MouseAction, MouseButton, Reply, Shell,
};
use blake2::{Blake2s256, Digest};
use clap::{Parser, Subcommand};
use notify::Watcher as _;
use tokio::sync::mpsc;

/// Directories never synced (build outputs, VCS) on top of `.gitignore` rules.
const SKIP_DIRS: &[&str] = &["target", "bin", "obj", "node_modules", ".git"];

/// Bytes per file-transfer frame (under the protocol's 32 MiB frame cap).
const CHUNK: usize = 8 * 1024 * 1024;

#[derive(Parser)]
#[command(
    name = "arc",
    version,
    about = "Remote-control a Windows machine over the arc relay (adb-style)."
)]
struct Cli {
    /// Named target from the config file (`~/.config/arc/config.toml`, or
    /// $ARC_CONFIG). Falls back to the file's `default`. Explicit flags and
    /// env vars below still override individual fields.
    #[arg(short = 't', long, global = true)]
    target: Option<String>,
    /// Relay WebSocket URL (else config target, else ARC_RELAY_URL).
    #[arg(long, global = true)]
    relay: Option<String>,
    /// Connect directly to the runner at host:port (e.g. its Tailscale IP),
    /// bypassing the relay (else config target, else ARC_DIRECT). Takes
    /// precedence over --relay.
    #[arg(long, global = true)]
    direct: Option<String>,
    /// Session id, 32 hex chars (else ARC_SESSION). Optional in --direct
    /// mode (no relay routing); defaults to all-zero.
    #[arg(long, global = true)]
    session: Option<String>,
    /// Pairing code XXXX-XXXX (else ARC_PAIRING).
    #[arg(long, global = true)]
    pairing: Option<String>,
    /// Run as an MCP server over stdio (for Agent tool-calling) instead of a
    /// one-shot command. Connects lazily on first tool call.
    #[arg(long, global = true)]
    mcp: bool,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a shell command; stdout/stderr stream live. Exits with its code.
    Shell {
        /// Use cmd.exe instead of PowerShell.
        #[arg(long)]
        cmd: bool,
        /// Kill the command after this many seconds (the runner enforces it).
        /// Omitted = a default safety limit (10 min); `0` = no limit.
        #[arg(long)]
        timeout: Option<u64>,
        /// The command and arguments (joined with spaces).
        #[arg(trailing_var_arg = true, required = true)]
        args: Vec<String>,
    },
    /// Run a local script on the runner: ships its *contents* (no pre-`push`, no
    /// shell quoting to escape) and runs it with the matching interpreter
    /// inferred from the extension — `.ps1` → PowerShell (`-ExecutionPolicy
    /// Bypass`), `.bat`/`.cmd` → cmd. Output streams live; args after the script
    /// pass through to it.
    Run {
        /// Path to a local script file (`.ps1`, `.bat`, or `.cmd`).
        script: String,
        /// Kill the script after this many seconds. Omitted = 10 min; `0` = no limit.
        #[arg(long)]
        timeout: Option<u64>,
        /// Arguments passed through to the script.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Send local → runner. A single file is always copied; a directory
    /// transfers incrementally (content-hash diff, `.gitignore`-aware, build
    /// dirs skipped) — `--whole` forces a full copy, `--delete` mirrors.
    Push {
        local: String,
        remote: String,
        /// Delete files on the runner not present locally (mirror).
        #[arg(long)]
        delete: bool,
        /// Show what would transfer/delete without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Transfer every file regardless of whether it already matches.
        #[arg(long)]
        whole: bool,
    },
    /// Fetch runner → local. A single file is always copied; a directory
    /// transfers incrementally (content-hash diff, build dirs excluded) —
    /// `--whole` forces a full copy, `--delete` mirrors.
    Pull {
        remote: String,
        local: String,
        /// Delete local files not present on the runner (mirror).
        #[arg(long)]
        delete: bool,
        /// Show what would transfer/delete without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Transfer every file regardless of whether it already matches.
        #[arg(long)]
        whole: bool,
    },
    /// Watch a local directory and auto-push changes to the runner as they
    /// happen (incremental, `.gitignore`-aware, build dirs ignored). Runs until
    /// interrupted. The dev-loop companion to a one-shot `push`.
    Watch { local: String, remote: String },
    /// Capture a screenshot to a file. Encoding follows the file extension
    /// (`.png` → PNG, else WebP) — no client-side conversion needed.
    Screencap {
        /// Output file path (`.png` or `.webp`).
        out: String,
        /// Capture only this window handle (else the full screen).
        #[arg(long)]
        window: Option<u64>,
        /// Capture only this element (id from `elements`/`find`) — its bounding box.
        #[arg(long)]
        element: Option<String>,
    },
    /// One-shot "verify the UI": optionally launch an app, find its window, wait
    /// for it to render (two stable frames), and screenshot it. Replaces the
    /// open → blind-sleep → windows → grep → screencap dance.
    Shot {
        /// Output file (`.png` or `.webp`).
        out: String,
        /// Match a window by title/process substring (case-insensitive).
        #[arg(long)]
        app: Option<String>,
        /// Capture this exact window handle (skip the search).
        #[arg(long)]
        window: Option<u64>,
        /// Launch this executable first, then capture its window.
        #[arg(long)]
        launch: Option<String>,
        /// Max seconds to wait for the window to appear and render.
        #[arg(long, default_value_t = 15)]
        wait: u64,
    },
    /// List top-level windows. Text is `handle | process | title`; `--json` emits
    /// structured records (handle, title, process, focused, rect).
    Windows {
        /// Emit JSON instead of pipe-delimited text.
        #[arg(long)]
        json: bool,
    },
    /// List a window's UI Automation elements. `--json` emits structured records
    /// (id, control_type, name, automation_id, value, rect, actionable).
    Elements {
        window: u64,
        /// Emit JSON instead of pipe-delimited text.
        #[arg(long)]
        json: bool,
    },
    /// Find elements in a window by attribute (no full-tree dump). Prints the
    /// matches as `id | control_type | actionable? | automation_id | name`.
    Find {
        /// Window handle (from `windows`).
        window: u64,
        /// Match the accessible name.
        #[arg(long)]
        name: Option<String>,
        /// Match `--name` as a substring instead of the whole name.
        #[arg(long)]
        contains: bool,
        /// Match the UIA AutomationId.
        #[arg(long = "id")]
        automation_id: Option<String>,
        /// Match the control type (e.g. `Button`, `Edit`).
        #[arg(long = "type")]
        control_type: Option<String>,
        /// Only enabled, on-screen elements.
        #[arg(long)]
        actionable: bool,
        /// Emit JSON instead of pipe-delimited text.
        #[arg(long)]
        json: bool,
    },
    /// Wait until an element matching the query appears (polls the runner), then
    /// print it. Exits non-zero on timeout. Same filters as `find`.
    Wait {
        /// Window handle (from `windows`).
        window: u64,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        contains: bool,
        #[arg(long = "id")]
        automation_id: Option<String>,
        #[arg(long = "type")]
        control_type: Option<String>,
        #[arg(long)]
        actionable: bool,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 10)]
        timeout: u64,
        /// Emit JSON instead of pipe-delimited text.
        #[arg(long)]
        json: bool,
    },
    /// Launch an application by path or name. Arguments after it (including
    /// `--flags`) pass through to the app: `arc open notepad C:\x.txt`,
    /// `arc open myapp.exe --port 9000`.
    Open {
        /// Executable path or registered app name. (Named `app`, not `target`,
        /// to avoid colliding with the global `-t/--target` arg id.)
        app: String,
        /// Arguments passed to the launched app.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Click a UI element by its id (from `elements`).
    Click { element_id: String },
    /// Type Unicode text into the focused element.
    Type {
        text: String,
        /// Focus this element first (id from `elements`/`find`) — more reliable
        /// than typing into whatever currently has focus.
        #[arg(long, value_name = "ELEMENT_ID")]
        into: Option<String>,
    },
    /// Press a key or chord — or a sequence of them: `enter`, `esc`, `f5`,
    /// `ctrl+c`, `ctrl+shift+esc`, `alt+f4`. Modifiers (`ctrl`/`alt`/`shift`/
    /// `win`) join the key with `+`. Multiple chords run in order on one
    /// connection (e.g. `arc key ctrl+a delete enter`).
    Key {
        /// One or more chords, applied in order.
        #[arg(required = true)]
        chords: Vec<String>,
    },
    /// Inject a mouse action at screen coordinates.
    Mouse {
        #[command(subcommand)]
        action: MouseCmd,
    },
    /// Set a UI element's value directly.
    Set { element_id: String, value: String },
}

/// Mouse sub-actions for `arc mouse` (coordinates are virtual-desktop pixels).
/// Each accepts negative coordinates/deltas (e.g. `scroll 0 -3`).
#[derive(Subcommand)]
enum MouseCmd {
    /// Move the cursor without clicking.
    #[command(allow_negative_numbers = true)]
    Move { x: i32, y: i32 },
    /// Move to (x, y) and click (`--count 2` for double-click).
    #[command(allow_negative_numbers = true)]
    Click {
        x: i32,
        y: i32,
        #[arg(long, value_enum, default_value_t = ArcButton::Left)]
        button: ArcButton,
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    /// Move to (x, y) and press (hold) the button.
    #[command(allow_negative_numbers = true)]
    Down {
        x: i32,
        y: i32,
        #[arg(long, value_enum, default_value_t = ArcButton::Left)]
        button: ArcButton,
    },
    /// Move to (x, y) and release the button.
    #[command(allow_negative_numbers = true)]
    Up {
        x: i32,
        y: i32,
        #[arg(long, value_enum, default_value_t = ArcButton::Left)]
        button: ArcButton,
    },
    /// Scroll by dx/dy notches (positive = right/down).
    #[command(allow_negative_numbers = true)]
    Scroll { dx: i32, dy: i32 },
    /// Press at (from_x, from_y), move to (to_x, to_y), release.
    #[command(allow_negative_numbers = true)]
    Drag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        #[arg(long, value_enum, default_value_t = ArcButton::Left)]
        button: ArcButton,
    },
}

/// CLI mouse-button selector, mapped to [`MouseButton`].
#[derive(Clone, Copy, clap::ValueEnum)]
enum ArcButton {
    Left,
    Right,
    Middle,
}

impl From<ArcButton> for MouseButton {
    fn from(b: ArcButton) -> Self {
        match b {
            ArcButton::Left => MouseButton::Left,
            ArcButton::Right => MouseButton::Right,
            ArcButton::Middle => MouseButton::Middle,
        }
    }
}

impl MouseCmd {
    fn into_action(self) -> MouseAction {
        match self {
            MouseCmd::Move { x, y } => MouseAction::Move { x, y },
            MouseCmd::Click {
                x,
                y,
                button,
                count,
            } => MouseAction::Click {
                x,
                y,
                button: button.into(),
                count,
            },
            MouseCmd::Down { x, y, button } => MouseAction::Down {
                x,
                y,
                button: button.into(),
            },
            MouseCmd::Up { x, y, button } => MouseAction::Up {
                x,
                y,
                button: button.into(),
            },
            MouseCmd::Scroll { dx, dy } => MouseAction::Scroll { dx, dy },
            MouseCmd::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                button,
            } => MouseAction::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                button: button.into(),
            },
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let code = match run(Cli::parse()).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("arc: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<i32> {
    let config = resolve_config(&cli)?;

    // MCP server mode connects lazily (on first tool call), so don't dial here.
    if cli.mcp {
        run_mcp(config).await?;
        return Ok(0);
    }
    let Some(command) = cli.command else {
        bail!("no command — see `arc --help`, or pass `--mcp` to run as an MCP server");
    };

    let mut controller = Controller::connect(&config)
        .await
        .context("connecting to runner")?;

    match command {
        Cmd::Shell { cmd, timeout, args } => {
            return shell(&mut controller, cmd, timeout, args).await;
        }
        Cmd::Run {
            script,
            timeout,
            args,
        } => {
            return run_script(&mut controller, &script, timeout, args).await;
        }
        Cmd::Push {
            local,
            remote,
            delete,
            dry_run,
            whole,
        } => push(&mut controller, &local, &remote, delete, dry_run, whole).await?,
        Cmd::Pull {
            remote,
            local,
            delete,
            dry_run,
            whole,
        } => pull(&mut controller, &remote, &local, delete, dry_run, whole).await?,
        Cmd::Watch { local, remote } => watch(&mut controller, &local, &remote).await?,
        Cmd::Screencap {
            out,
            window,
            element,
        } => screencap(&mut controller, &out, window, element).await?,
        Cmd::Shot {
            out,
            app,
            window,
            launch,
            wait,
        } => shot(&mut controller, &out, app, window, launch, wait).await?,
        Cmd::Windows { json } => windows(&mut controller, json).await?,
        Cmd::Elements { window, json } => elements(&mut controller, window, json).await?,
        Cmd::Find {
            window,
            name,
            contains,
            automation_id,
            control_type,
            actionable,
            json,
        } => {
            let query = ElementQuery {
                name,
                name_contains: contains,
                automation_id,
                control_type,
                actionable_only: actionable,
            };
            return find_elements(&mut controller, window, query, None, json).await;
        }
        Cmd::Wait {
            window,
            name,
            contains,
            automation_id,
            control_type,
            actionable,
            timeout,
            json,
        } => {
            let query = ElementQuery {
                name,
                name_contains: contains,
                automation_id,
                control_type,
                actionable_only: actionable,
            };
            return find_elements(
                &mut controller,
                window,
                query,
                Some(timeout.saturating_mul(1000)),
                json,
            )
            .await;
        }
        Cmd::Open { app, args } => open(&mut controller, app, args).await?,
        Cmd::Click { element_id } => {
            ack(
                &mut controller,
                Command::Click {
                    target: ClickTarget::Element(ElementId(element_id)),
                },
            )
            .await?;
        }
        Cmd::Type { text, into } => {
            ack(
                &mut controller,
                Command::TypeText {
                    text,
                    into: into.map(ElementId),
                },
            )
            .await?
        }
        Cmd::Key { chords } => keys(&mut controller, chords).await?,
        Cmd::Mouse { action } => {
            ack(
                &mut controller,
                Command::Mouse {
                    action: action.into_action(),
                },
            )
            .await?;
        }
        Cmd::Set { element_id, value } => {
            ack(
                &mut controller,
                Command::SetValue {
                    element: ElementId(element_id),
                    value,
                },
            )
            .await?;
        }
    }
    Ok(0)
}

/// Runs the MCP server over stdio (everything but the protocol goes to stderr,
/// which `main` already configures). Connects to the runner lazily.
async fn run_mcp(config: SessionConfig) -> Result<()> {
    use rmcp::ServiceExt as _;
    let service = mcp::AgentRc::new(config)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

/// A `[targets.<name>]` block in the config file.
#[derive(Debug, Default, serde::Deserialize)]
struct TargetCfg {
    relay: Option<String>,
    direct: Option<String>,
    session: Option<String>,
    pairing: Option<String>,
    /// Direct + trusted tailnet: authenticate by Tailscale identity, so no
    /// pairing code is needed (uses the well-known [`PairingCode::tailnet_auto`]).
    trust_tailnet: Option<bool>,
}

/// The config file: a `default` target name plus named `targets`.
#[derive(Debug, Default, serde::Deserialize)]
struct ConfigFile {
    default: Option<String>,
    #[serde(default)]
    targets: HashMap<String, TargetCfg>,
}

fn resolve_config(cli: &Cli) -> Result<SessionConfig> {
    let file = load_config_file()?;
    let target = select_target(cli, &file)?;

    // Per field: explicit flag > selected config target > environment variable.
    let pick3 = |flag: &Option<String>, from_target: Option<String>, env: &str| -> Option<String> {
        flag.clone()
            .or(from_target)
            .or_else(|| std::env::var(env).ok())
            .filter(|s| !s.is_empty())
    };

    let direct = pick3(
        &cli.direct,
        target.and_then(|t| t.direct.clone()),
        "ARC_DIRECT",
    );
    let is_direct = direct.is_some();
    let transport = match direct {
        Some(addr) => Transport::Direct { addr },
        None => Transport::Relay {
            url: pick3(
                &cli.relay,
                target.and_then(|t| t.relay.clone()),
                "ARC_RELAY_URL",
            )
            .context(
                "endpoint: set --direct/--relay, a config target, or \
                 ARC_DIRECT/ARC_RELAY_URL",
            )?,
        },
    };

    // Relay mode routes by session id (required); direct mode does not.
    let session_raw = match pick3(
        &cli.session,
        target.and_then(|t| t.session.clone()),
        "ARC_SESSION",
    ) {
        Some(s) => s,
        None if is_direct => "0".repeat(32),
        None => bail!("session: set --session, a config target, or ARC_SESSION (relay mode)"),
    };
    let session = session_raw
        .parse::<SessionId>()
        .map_err(|_| anyhow!("session must be 32 hex chars"))?;

    // A trusted-tailnet direct target needs no pairing code (identity is the
    // gate); fall back to the well-known constant when none is supplied.
    let trust_tailnet = target.and_then(|t| t.trust_tailnet) == Some(true);
    let pairing = match pick3(
        &cli.pairing,
        target.and_then(|t| t.pairing.clone()),
        "ARC_PAIRING",
    ) {
        Some(raw) => PairingCode::parse(&raw).map_err(|_| anyhow!("pairing must be XXXX-XXXX"))?,
        None if trust_tailnet && is_direct => PairingCode::tailnet_auto(),
        None => bail!("pairing: set --pairing, a config target, or ARC_PAIRING"),
    };

    Ok(SessionConfig {
        transport,
        session,
        pairing,
    })
}

/// Loads the config file from `$ARC_CONFIG` or
/// `~/.config/arc/config.toml`; an absent file is `Ok(None)`.
fn load_config_file() -> Result<Option<ConfigFile>> {
    let path = match std::env::var("ARC_CONFIG") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".config/arc/config.toml"),
            None => return Ok(None),
        },
    };
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(cfg))
}

/// Picks the active target: `-t <name>`, else the file's `default`, else none.
fn select_target<'a>(cli: &Cli, file: &'a Option<ConfigFile>) -> Result<Option<&'a TargetCfg>> {
    let Some(file) = file else {
        if let Some(name) = &cli.target {
            bail!("--target '{name}' given but no config file found");
        }
        return Ok(None);
    };
    match cli.target.clone().or_else(|| file.default.clone()) {
        Some(n) => file
            .targets
            .get(&n)
            .map(Some)
            .ok_or_else(|| anyhow!("no target '{n}' in config file")),
        None => Ok(None),
    }
}

async fn shell(
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

/// Reads a local script and runs it on the runner via [`Command::RunScript`] —
/// shipping its contents (no pre-`push`, no shell quoting) under the
/// interpreter inferred from its extension.
async fn run_script(
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
async fn stream_run(controller: &mut Controller, command: Command) -> Result<i32> {
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

/// Sends `local` to the runner. A single file is copied wholesale; a directory
/// transfers incrementally (skipping files whose content already matches, unless
/// `whole`) and, with `delete`, prunes runner files absent locally.
async fn push(
    controller: &mut Controller,
    local: &str,
    remote: &str,
    delete: bool,
    dry_run: bool,
    whole: bool,
) -> Result<()> {
    let meta = std::fs::metadata(local).with_context(|| format!("stat {local}"))?;
    if !meta.is_dir() {
        let data = std::fs::read(local).with_context(|| format!("reading {local}"))?;
        if dry_run {
            println!("would push {local} ({} bytes) -> {remote}", data.len());
            return Ok(());
        }
        push_bytes(controller, remote, &data).await?;
        println!("pushed {local} ({} bytes) -> {remote}", data.len());
        return Ok(());
    }

    let Stats {
        changed,
        total,
        bytes,
        removed,
    } = push_tree(controller, local, remote, delete, dry_run, whole).await?;
    print_transfer_summary(
        dry_run, changed, total, bytes, delete, removed, local, remote,
    );
    Ok(())
}

/// Per-transfer counters returned by [`push_tree`].
#[derive(Default)]
struct Stats {
    changed: u64,
    total: usize,
    bytes: u64,
    removed: u64,
}

/// Incrementally pushes the directory `local` to `remote` (the body shared by
/// `push` of a directory and `watch`), printing each transferred/deleted file
/// and returning the counters for the caller to summarize.
async fn push_tree(
    controller: &mut Controller,
    local: &str,
    remote: &str,
    delete: bool,
    dry_run: bool,
    whole: bool,
) -> Result<Stats> {
    let files = collect_files(Path::new(local))?;
    if files.is_empty() {
        return Ok(Stats::default());
    }
    let mut local_hashes: Vec<(String, PathBuf, String)> = Vec::with_capacity(files.len());
    for (rel, abs) in files {
        let data = std::fs::read(&abs).with_context(|| format!("reading {}", abs.display()))?;
        local_hashes.push((rel, abs, blake2_hex(&data)));
    }

    // One round-trip: what does the runner already have? (Skipped for --whole.)
    let remote_hashes: HashMap<String, Option<String>> = if whole {
        HashMap::new()
    } else {
        let paths: Vec<String> = local_hashes.iter().map(|(rel, _, _)| rel.clone()).collect();
        match controller
            .request(Command::HashFiles {
                root: remote.to_owned(),
                paths,
            })
            .await?
        {
            Reply::FileHashes(list) => list.into_iter().map(|h| (h.path, h.hash)).collect(),
            other => bail!("unexpected reply: {other:?}"),
        }
    };

    let (mut changed, mut bytes) = (0u64, 0u64);
    for (rel, abs, local_hash) in &local_hashes {
        if !whole && remote_hashes.get(rel).and_then(|h| h.as_deref()) == Some(local_hash.as_str())
        {
            continue; // identical on the runner
        }
        changed += 1;
        if dry_run {
            println!("would push {rel}");
            continue;
        }
        let data = std::fs::read(abs)?;
        push_bytes(controller, &join_remote(remote, rel), &data).await?;
        bytes += data.len() as u64;
        println!("→ {rel} ({} bytes)", data.len());
    }

    let mut removed = 0u64;
    if delete {
        let local_set: HashSet<&str> = local_hashes
            .iter()
            .map(|(rel, _, _)| rel.as_str())
            .collect();
        let remote_tree = match controller
            .request(Command::ListTree {
                root: remote.to_owned(),
            })
            .await?
        {
            Reply::Tree(paths) => paths,
            other => bail!("unexpected reply: {other:?}"),
        };
        for rel in &remote_tree {
            if local_set.contains(rel.as_str()) {
                continue;
            }
            removed += 1;
            if dry_run {
                println!("would delete {rel}");
                continue;
            }
            ack(
                controller,
                Command::DeleteFile {
                    path: join_remote(remote, rel),
                },
            )
            .await?;
            println!("✗ {rel}");
        }
    }

    Ok(Stats {
        changed,
        total: local_hashes.len(),
        bytes,
        removed,
    })
}

/// Watches `local` and incrementally pushes changes to `remote` until Ctrl+C.
/// Build/VCS dirs are ignored at the watcher level (no churn during `cargo
/// build`); each burst of events is debounced, then a hash-diff sync transfers
/// only what actually changed.
async fn watch(controller: &mut Controller, local: &str, remote: &str) -> Result<()> {
    let initial = push_tree(controller, local, remote, false, false, false).await?;
    println!(
        "initial sync: {}/{} files ({} bytes) {local} -> {remote}",
        initial.changed, initial.total, initial.bytes
    );

    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && !matches!(event.kind, notify::EventKind::Access(_))
            && event.paths.iter().any(|p| is_syncable(p))
        {
            let _ = tx.send(());
        }
    })
    .context("initialising file watcher")?;
    watcher
        .watch(Path::new(local), notify::RecursiveMode::Recursive)
        .with_context(|| format!("watching {local}"))?;
    println!("watching {local} (Ctrl+C to stop)…");

    while rx.recv().await.is_some() {
        // Debounce: keep draining until the filesystem goes quiet.
        loop {
            match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => return Ok(()),
                Err(_) => break,
            }
        }
        match push_tree(controller, local, remote, false, false, false).await {
            Ok(s) if s.changed > 0 => println!("synced {} files ({} bytes)", s.changed, s.bytes),
            Ok(_) => {}
            Err(e) => eprintln!("arc: sync error: {e:#}"),
        }
    }
    Ok(())
}

/// True if `path` is not inside a build/VCS directory (the watcher filter).
fn is_syncable(path: &Path) -> bool {
    !path
        .components()
        .any(|c| SKIP_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref()))
}

/// Prints the trailing one-line summary shared by directory `push`/`pull`.
#[allow(clippy::too_many_arguments)]
fn print_transfer_summary(
    dry_run: bool,
    changed: u64,
    total: usize,
    bytes: u64,
    delete: bool,
    removed: u64,
    src: &str,
    dst: &str,
) {
    let verb = if dry_run {
        "would transfer"
    } else {
        "transferred"
    };
    let deleted = if delete {
        format!(", {removed} deleted")
    } else {
        String::new()
    };
    println!("{verb} {changed}/{total} files ({bytes} bytes){deleted}: {src} -> {dst}");
}

/// Writes a whole file to `remote` in chunks (offset 0 truncates/creates).
async fn push_bytes(controller: &mut Controller, remote: &str, data: &[u8]) -> Result<()> {
    if data.is_empty() {
        return ack(
            controller,
            Command::WriteFile {
                path: remote.to_owned(),
                contents: Vec::new(),
                offset: 0,
            },
        )
        .await;
    }
    let mut offset = 0u64;
    for chunk in data.chunks(CHUNK) {
        ack(
            controller,
            Command::WriteFile {
                path: remote.to_owned(),
                contents: chunk.to_vec(),
                offset,
            },
        )
        .await?;
        offset += chunk.len() as u64;
    }
    Ok(())
}

/// Walks `root` respecting `.gitignore`, additionally skipping [`SKIP_DIRS`];
/// returns `(forward-slash relative path, absolute path)` per file.
fn collect_files(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    for entry in ignore::WalkBuilder::new(root).hidden(false).build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if rel
            .components()
            .any(|c| SKIP_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref()))
        {
            continue;
        }
        files.push((
            rel.to_string_lossy().replace('\\', "/"),
            entry.path().to_path_buf(),
        ));
    }
    files.sort();
    Ok(files)
}

fn join_remote(remote: &str, rel: &str) -> String {
    format!("{}/{rel}", remote.trim_end_matches('/'))
}

fn blake2_hex(data: &[u8]) -> String {
    let mut hasher = Blake2s256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// Fetches `remote` to the runner. A single file is copied wholesale; a
/// directory transfers incrementally (skipping files already matching locally,
/// unless `whole`) and, with `delete`, prunes local files absent on the runner.
///
/// A non-empty [`Command::ListTree`] means `remote` is a directory (build dirs
/// excluded); an empty one means a single file (or absent) → a file pull, which
/// also lets you fetch one artifact from inside an otherwise-skipped build dir.
async fn pull(
    controller: &mut Controller,
    remote: &str,
    local: &str,
    delete: bool,
    dry_run: bool,
    whole: bool,
) -> Result<()> {
    let tree = match controller
        .request(Command::ListTree {
            root: remote.to_owned(),
        })
        .await?
    {
        Reply::Tree(paths) => paths,
        other => bail!("unexpected reply: {other:?}"),
    };
    if tree.is_empty() {
        if dry_run {
            println!("would pull {remote} -> {local}");
            return Ok(());
        }
        let bytes = pull_to(controller, remote, Path::new(local)).await?;
        println!("pulled {remote} -> {local} ({bytes} bytes)");
        return Ok(());
    }

    let remote_hashes: HashMap<String, Option<String>> = if whole {
        HashMap::new()
    } else {
        match controller
            .request(Command::HashFiles {
                root: remote.to_owned(),
                paths: tree.clone(),
            })
            .await?
        {
            Reply::FileHashes(list) => list.into_iter().map(|h| (h.path, h.hash)).collect(),
            other => bail!("unexpected reply: {other:?}"),
        }
    };

    let local_root = Path::new(local);
    let (mut changed, mut bytes) = (0u64, 0u64);
    for rel in &tree {
        if !whole {
            let remote_hash = remote_hashes.get(rel).and_then(|h| h.clone());
            let local_path = local_root.join(rel);
            let local_hash = if local_path.is_file() {
                Some(blake2_hex(&std::fs::read(&local_path)?))
            } else {
                None
            };
            if remote_hash.is_some() && local_hash == remote_hash {
                continue; // identical locally
            }
        }
        changed += 1;
        if dry_run {
            println!("would pull {rel}");
            continue;
        }
        bytes += pull_to(controller, &join_remote(remote, rel), &local_root.join(rel)).await?;
        println!("← {rel}");
    }

    let mut removed = 0u64;
    if delete && local_root.exists() {
        let remote_set: HashSet<&str> = tree.iter().map(|s| s.as_str()).collect();
        for (rel, abs) in collect_files(local_root)? {
            if remote_set.contains(rel.as_str()) {
                continue;
            }
            removed += 1;
            if dry_run {
                println!("would delete (local) {rel}");
                continue;
            }
            std::fs::remove_file(&abs).with_context(|| format!("deleting {}", abs.display()))?;
            println!("✗ (local) {rel}");
        }
    }

    print_transfer_summary(
        dry_run,
        changed,
        tree.len(),
        bytes,
        delete,
        removed,
        remote,
        local,
    );
    Ok(())
}

/// Reads `remote` in chunks into `local` (creating parent dirs); returns bytes.
async fn pull_to(controller: &mut Controller, remote: &str, local: &Path) -> Result<u64> {
    if let Some(parent) = local.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file =
        std::fs::File::create(local).with_context(|| format!("creating {}", local.display()))?;
    let mut offset = 0u64;
    loop {
        let reply = controller
            .request(Command::ReadFile {
                path: remote.to_owned(),
                offset,
                max_len: CHUNK as u64,
            })
            .await?;
        let bytes = match reply {
            Reply::FileContents(bytes) => bytes,
            other => bail!("unexpected reply: {other:?}"),
        };
        let read = bytes.len();
        file.write_all(&bytes)?;
        offset += read as u64;
        if read < CHUNK {
            break;
        }
    }
    Ok(offset)
}

async fn screencap(
    controller: &mut Controller,
    out: &str,
    window: Option<u64>,
    element: Option<String>,
) -> Result<()> {
    let target = if let Some(id) = element {
        CaptureTarget::Element(ElementId(id))
    } else if let Some(handle) = window {
        CaptureTarget::Window(WindowId(handle))
    } else {
        CaptureTarget::FullScreen
    };
    // Encode to match the output extension (.png → PNG, else WebP) so there's
    // no client-side conversion step.
    let lower = out.to_ascii_lowercase();
    let format = if lower.ends_with(".png") {
        Some(ImageFormat::Png)
    } else if lower.ends_with(".webp") {
        Some(ImageFormat::Webp)
    } else {
        None
    };
    match controller
        .request(Command::Screenshot {
            target,
            format,
            settle_ms: None,
            settle_await_change: false,
        })
        .await?
    {
        Reply::Image(image) => {
            std::fs::write(out, &image.data).with_context(|| format!("writing {out}"))?;
            println!(
                "saved {out} ({}x{}, {:?}, {} bytes)",
                image.width,
                image.height,
                image.format,
                image.data.len()
            );
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Infers the encoding from a file extension (`.png` → PNG, `.webp` → WebP).
fn format_from_ext(out: &str) -> Option<ImageFormat> {
    let lower = out.to_ascii_lowercase();
    if lower.ends_with(".png") {
        Some(ImageFormat::Png)
    } else if lower.ends_with(".webp") {
        Some(ImageFormat::Webp)
    } else {
        None
    }
}

/// One-shot "verify the UI": optionally launch an app, find its window, wait for
/// it to render (until two frames are stable), and screenshot it.
async fn shot(
    controller: &mut Controller,
    out: &str,
    app: Option<String>,
    window: Option<u64>,
    launch: Option<String>,
    wait: u64,
) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(wait);

    if let Some(exe) = &launch {
        match controller
            .request(Command::OpenApp {
                target: exe.clone(),
                args: vec![],
            })
            .await?
        {
            Reply::AppOpened { .. } => {}
            other => bail!("unexpected reply launching {exe}: {other:?}"),
        }
    }

    let hwnd = if let Some(handle) = window {
        handle
    } else {
        let needle = app
            .clone()
            .or_else(|| {
                launch.as_deref().map(|e| {
                    std::path::Path::new(e)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(e)
                        .to_owned()
                })
            })
            .ok_or_else(|| anyhow!("pass --window <handle>, --app <substr>, or --launch <exe>"))?;
        find_window(controller, &needle, deadline).await?
    };

    let remaining = deadline
        .saturating_duration_since(std::time::Instant::now())
        .as_millis() as u64;
    let settle_ms = remaining.max(1500);
    match controller
        .request(Command::Screenshot {
            target: CaptureTarget::Window(WindowId(hwnd)),
            format: format_from_ext(out),
            settle_ms: Some(settle_ms),
            // A just-launched window starts on a static backdrop; wait for it to
            // actually render before settling.
            settle_await_change: launch.is_some(),
        })
        .await?
    {
        Reply::Image(image) => {
            std::fs::write(out, &image.data).with_context(|| format!("writing {out}"))?;
            println!(
                "saved {out} (window {hwnd}, {}x{}, {:?}, {} bytes)",
                image.width,
                image.height,
                image.format,
                image.data.len()
            );
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Polls the window list until one matches `needle` (title or process substring,
/// case-insensitive) or the deadline passes.
async fn find_window(
    controller: &mut Controller,
    needle: &str,
    deadline: std::time::Instant,
) -> Result<u64> {
    let needle = needle.to_lowercase();
    loop {
        if let Reply::Windows(windows) = controller.request(Command::ListWindows).await?
            && let Some(w) = windows.iter().find(|w| {
                w.title.to_lowercase().contains(&needle)
                    || w.process.to_lowercase().contains(&needle)
            })
        {
            return Ok(w.id.0);
        }
        if std::time::Instant::now() >= deadline {
            bail!("no window matching '{needle}' appeared within the wait");
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

async fn windows(controller: &mut Controller, json: bool) -> Result<()> {
    match controller.request(Command::ListWindows).await? {
        Reply::Windows(windows) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&windows)?);
            } else {
                for w in &windows {
                    println!("{} | {} | {}", w.id.0, w.process, w.title);
                }
            }
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

async fn elements(controller: &mut Controller, window: u64, json: bool) -> Result<()> {
    match controller
        .request(Command::ListElements {
            window: WindowId(window),
        })
        .await?
    {
        Reply::Elements(elements) => {
            print_elements(&elements, json)?;
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Finds elements by attribute (`wait_ms = None`) or waits for one to appear
/// (`wait_ms = Some`), printing the matches. A `wait` that times out surfaces
/// as the runner's error.
async fn find_elements(
    controller: &mut Controller,
    window: u64,
    query: ElementQuery,
    wait_ms: Option<u64>,
    json: bool,
) -> Result<i32> {
    match controller
        .request(Command::FindElements {
            window: WindowId(window),
            query,
            wait_ms,
        })
        .await?
    {
        Reply::Elements(elements) => {
            print_elements(&elements, json)?;
            Ok(0)
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Prints elements as JSON or one pipe-delimited row each.
fn print_elements(elements: &[ElementInfo], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(elements)?);
    } else {
        for e in elements {
            print_element(e);
        }
    }
    Ok(())
}

/// Prints one element row: `id | control_type | actionable? | automation_id | name`.
fn print_element(e: &ElementInfo) {
    println!(
        "{} | {} | {} | {} | {}",
        e.id.0,
        e.control_type,
        if e.actionable {
            "actionable"
        } else {
            "inactive"
        },
        e.automation_id.as_deref().unwrap_or(""),
        e.name.as_deref().unwrap_or(""),
    );
}

async fn open(controller: &mut Controller, app: String, args: Vec<String>) -> Result<()> {
    match controller
        .request(Command::OpenApp { target: app, args })
        .await?
    {
        Reply::AppOpened { window, pid } => {
            println!("launched pid={pid} window={window:?}");
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

async fn ack(controller: &mut Controller, command: Command) -> Result<()> {
    match controller.request(command).await? {
        Reply::Ack => Ok(()),
        other => bail!("expected ack, got {other:?}"),
    }
}

/// Sends a sequence of key chords in order on one connection. All chords are
/// parsed up front (so a bad chord aborts before anything is pressed), then
/// applied with a short gap between them for reliable delivery to WinUI apps.
async fn keys(controller: &mut Controller, chords: Vec<String>) -> Result<()> {
    let parsed = chords
        .iter()
        .map(|c| arc_proto::wire::parse_chord(c).map_err(|e| anyhow!("{c}: {e}")))
        .collect::<Result<Vec<_>>>()?;
    let last = parsed.len().saturating_sub(1);
    for (i, (modifiers, key)) in parsed.into_iter().enumerate() {
        ack(controller, Command::KeyChord { modifiers, key }).await?;
        if i < last {
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
    }
    Ok(())
}
