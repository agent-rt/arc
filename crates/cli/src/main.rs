//! `arc` — an adb-style CLI for arc.
//!
//! A thin client over the shared [`Controller`] transport: run shell commands
//! (output streams live), push/pull files (chunked), capture screenshots, and
//! drive UI Automation — without the verbosity of an MCP tool call.
//!
//! Config comes from `--relay/--session/--pairing` or the `ARC_RELAY_URL`
//! / `ARC_SESSION` / `ARC_PAIRING` environment variables.

#![forbid(unsafe_code)]

mod agents_md;
mod capture;
mod config;
mod exec;
mod files;
mod forward;
mod mcp;
mod ui;

use anyhow::{Context, Result, bail};
use arc_net::{Controller, SessionConfig};
use arc_proto::id::{ElementId, WindowId};
use arc_proto::wire::{ClickTarget, Command, ElementQuery, MouseAction, MouseButton, Reply};
use clap::{Parser, Subcommand};

use agents_md::agents_md;
use capture::{screencap, shot};
use config::resolve_config;
use exec::{capabilities, doctor, kill, ps, run_script, shell, tail, whoami};
use files::{cat, procdump, pull, push, watch};
use ui::{clip, elements, find_elements, keys, open, windows};

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
        /// Set an environment variable for the command as `KEY=VALUE` (repeatable).
        /// Injected into the remote process's environment — never the command
        /// line — so it's the safe way to pass secrets/config.
        #[arg(long, value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Read `KEY=VALUE` lines from this file into the environment (`#`
        /// comments and blank lines ignored); `--env` flags override it.
        #[arg(long, value_name = "PATH")]
        env_file: Option<String>,
        /// The command and arguments (joined with spaces).
        #[arg(trailing_var_arg = true, required = true)]
        args: Vec<String>,
    },
    /// Run a local `.ps1`/`.bat`/`.sh` script on the runner (streams live).
    ///
    /// Ships its *contents* (no pre-`push`, no shell quoting to escape) and runs
    /// it with the interpreter inferred from the extension — `.ps1` → PowerShell
    /// (`-ExecutionPolicy Bypass`), `.bat`/`.cmd` → cmd, `.sh` → sh. An unknown
    /// or missing extension uses the runner's default shell (sh on a Unix runner
    /// like Android, PowerShell on Windows). Args after the script pass to it.
    Run {
        /// Local script file (`.ps1`/`.bat`/`.cmd`/`.sh`), or `-` to read from
        /// stdin — pipe a multi-line here-doc to run with no shell quoting.
        script: String,
        /// Interpreter to use: `ps1`, `bat`, `cmd`, or `sh`. Required with `-` on
        /// a Windows runner (stdin has no extension; a Unix runner defaults to
        /// sh); overrides the extension for a file. Flags must precede the script
        /// (e.g. `arc run --lang ps1 - <<'EOF'`).
        #[arg(long, value_name = "ps1|bat|cmd|sh")]
        lang: Option<String>,
        /// Run detached: the runner redirects output to a log file on the box and
        /// returns a pid + log path immediately, so a long task (installer, build)
        /// doesn't block. Follow it with `arc tail -f <log>`, manage with `ps`/
        /// `kill`. Ignores `--timeout` (there's no connection to time out).
        #[arg(long)]
        detach: bool,
        /// Kill the script after this many seconds. Omitted = 10 min; `0` = no limit.
        #[arg(long)]
        timeout: Option<u64>,
        /// Set an environment variable for the script as `KEY=VALUE` (repeatable).
        /// Injected into the remote process's environment, not the command line.
        #[arg(long, value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Read `KEY=VALUE` lines from this file into the environment (`#`
        /// comments and blank lines ignored); `--env` flags override it.
        #[arg(long, value_name = "PATH")]
        env_file: Option<String>,
        /// Arguments passed through to the script.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Send a file or directory to the runner (a directory push *is* an
    /// incremental sync — re-run it and only changed files transfer).
    ///
    /// A single file is always copied wholesale. A directory transfers
    /// incrementally (content-hash diff, one round-trip) and is `.gitignore`-aware,
    /// skipping build dirs (target/bin/obj/node_modules/.git) — so "code lives
    /// here, builds happen on the box" needs no config. `--whole` forces a full
    /// copy, `--delete` mirrors (prunes remote extras), `--no-ignore` includes
    /// ignored files + build dirs (for shipping build artifacts — point it at the
    /// specific output dir, e.g. `push --no-ignore ./dist C:/work/dist`, not the
    /// repo root). For a continuous sync, use `arc watch`.
    Push {
        /// Local file or directory to send.
        local: String,
        /// Destination path on the runner.
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
        /// Include `.gitignore`'d files and build dirs — for shipping build
        /// artifacts (aim it at the output dir, not the repo root).
        #[arg(long)]
        no_ignore: bool,
    },
    /// Fetch a file or directory from the runner (directories sync incrementally;
    /// `--watch` keeps pulling changes — build on the box, artifacts flow back).
    ///
    /// A single file is always copied; a directory transfers incrementally
    /// (content-hash diff, build dirs excluded) — `--whole` forces a full copy,
    /// `--delete` mirrors, `--no-ignore` includes build dirs (to pull artifacts
    /// out of e.g. `target/`), `--watch` polls the runner and pulls changes as
    /// they appear.
    Pull {
        /// Source path on the runner.
        remote: String,
        /// Local destination file or directory.
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
        /// Include the runner's build dirs (everything but `.git`) — for pulling
        /// build artifacts back.
        #[arg(long)]
        no_ignore: bool,
        /// Keep pulling: poll the runner and fetch changes until interrupted.
        #[arg(long)]
        watch: bool,
        /// With `--watch`, seconds between polls (default 2).
        #[arg(long, default_value_t = 2)]
        interval: u64,
    },
    /// Auto-push a directory on every save; `--on-change` rebuilds on the runner.
    ///
    /// Watches a local dir and pushes changes as they happen (incremental,
    /// `.gitignore`-aware, build dirs ignored). Runs until interrupted — the
    /// dev-loop companion to a one-shot `push`.
    Watch {
        /// Local directory to watch.
        local: String,
        /// Destination directory on the runner.
        remote: String,
        /// After each sync (and once at startup), run this PowerShell command on
        /// the runner — e.g. `--on-change 'cargo build'`. Output streams live.
        #[arg(long, value_name = "CMD")]
        on_change: Option<String>,
        /// Include `.gitignore`'d files and build dirs — e.g. watch a build
        /// output dir and re-ship artifacts as they're produced.
        #[arg(long)]
        no_ignore: bool,
    },
    /// Print a remote file to stdout (UTF-8, lossy).
    ///
    /// For binary, or to save a copy, use `pull`.
    Cat {
        /// File path on the runner.
        remote: String,
    },
    /// List remote processes (Id, name, working-set MB), heaviest first.
    ///
    /// An optional substring filters by process name. `--cpu` adds an
    /// instantaneous CPU% column (sampled over ~500ms) to tell a spinning
    /// process (busy) from a blocked one — useful for diagnosing hangs.
    Ps {
        /// Only show processes whose name contains this (case-insensitive).
        pattern: Option<String>,
        /// Add an instantaneous CPU% column (samples over ~500ms, so slower).
        #[arg(long)]
        cpu: bool,
    },
    /// Report the runner's identity: account, integrity level, admin, session.
    ///
    /// Whether the runner is elevated decides if a build/launch reflects a real
    /// (non-admin) user — an `arc-runner install` runs non-elevated (Medium IL).
    Whoami,
    /// Self-check: link, identity, session-activity tier, and a UIA smoke count.
    ///
    /// Interprets which capabilities work now — UIA + per-window capture work
    /// disconnected; raw input and full-screen capture need an Active session.
    Doctor,
    /// Report the runner's OS/arch/version and the commands it implements.
    ///
    /// The runner's capability surface varies by platform (a locked-down Android
    /// runner has no clipboard/procdump); this asks it directly. A legacy runner
    /// that predates the query is reported as such.
    Capabilities,
    /// Forward a local TCP port to the runner (adb/ssh `-L` style); runs until
    /// interrupted.
    ///
    /// Listens on `127.0.0.1:<localport>` and tunnels each connection over the
    /// encrypted link to the runner, which dials `<host>` (default `127.0.0.1`)
    /// on the box — so a service bound to the box's localhost (a dev server, a
    /// debugger port) is reachable locally. Best in direct (Tailscale) mode.
    Forward {
        /// `<localport>:<remoteport>` or `<localport>:<remotehost>:<remoteport>`.
        spec: String,
    },
    /// Kill a remote process by PID or name (`--dry-run` to preview).
    ///
    /// By PID (all digits) or by name (`-Force`); a name kills every matching
    /// process.
    Kill {
        /// PID (all digits) or process name (with or without `.exe`).
        process: String,
        /// List the matching processes without killing them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Write a minidump of a process (thread stacks + modules) and pull it back —
    /// for diagnosing a hang. Open it locally in WinDbg/cdb with symbols.
    Procdump {
        /// Target process id.
        pid: u32,
        /// Local output file (default `procdump-<pid>.dmp`).
        out: Option<String>,
    },
    /// Print the tail of a remote file; `-f` follows it (streams appended lines
    /// until interrupted) — for watching logs.
    Tail {
        /// File path on the runner.
        remote: String,
        /// Number of trailing lines to print first.
        #[arg(short = 'n', long, default_value_t = 10)]
        lines: u64,
        /// Follow: keep streaming new lines as the file grows.
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Screenshot to a file; `--element` one control, `--baseline` regression-diff.
    ///
    /// Encoding follows the file extension (`.png` → PNG, else WebP) — no
    /// client-side conversion needed.
    Screencap {
        /// Output file path (`.png` or `.webp`).
        out: String,
        /// Capture only this window handle (else the full screen).
        #[arg(long)]
        window: Option<u64>,
        /// Capture only this element (id from `elements`/`find`) — its bounding box.
        #[arg(long)]
        element: Option<String>,
        /// Compare the capture against this baseline image and report how much
        /// changed. Exits non-zero if the change exceeds `--threshold`.
        #[arg(long)]
        baseline: Option<String>,
        /// With `--baseline`, write a diff image here highlighting changed pixels.
        #[arg(long)]
        diff: Option<String>,
        /// Percent of pixels that may differ before it counts as a regression.
        #[arg(long, default_value_t = 0.1)]
        threshold: f64,
    },
    /// One-shot "verify the UI": launch/find a window, wait for render, screenshot.
    ///
    /// Optionally launch an app, find its window, wait for it to render (two
    /// stable frames), activate it, and screenshot. Replaces the open →
    /// blind-sleep → windows → grep → screencap dance.
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
    /// List top-level windows (`--json` for records, `--filter <substr>`).
    ///
    /// Text is `handle | process | title`; `--json` emits structured records
    /// (handle, title, process, focused, rect).
    Windows {
        /// Emit JSON instead of pipe-delimited text.
        #[arg(long)]
        json: bool,
        /// Only show windows whose title or process matches this substring
        /// (case-insensitive).
        #[arg(long)]
        filter: Option<String>,
    },
    /// List a window's UI Automation elements (`--json` for structured records).
    ///
    /// `--json` records: id, control_type, name, automation_id, value, rect,
    /// actionable.
    Elements {
        /// Window handle (from `windows`).
        window: u64,
        /// Emit JSON instead of pipe-delimited text.
        #[arg(long)]
        json: bool,
    },
    /// Find elements in a window by attribute (no full-tree dump).
    ///
    /// Prints matches as `id | control_type | actionable? | automation_id | name`
    /// (`--json` for structured records).
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
    /// Wait until a matching element appears, then print it (same filters as `find`).
    ///
    /// Polls the runner; exits non-zero on timeout.
    Wait {
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
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 10)]
        timeout: u64,
        /// Emit JSON instead of pipe-delimited text.
        #[arg(long)]
        json: bool,
    },
    /// Launch an application by path or name (args after it pass through).
    ///
    /// Arguments after it (including `--flags`) pass through to the app:
    /// `arc open notepad C:\x.txt`, `arc open myapp.exe --port 9000`.
    Open {
        /// Milliseconds to watch for a startup crash before returning: if the app
        /// exits within it, report its exit code + the matching Application error
        /// event, and exit non-zero. `0` = don't wait. Omitted = ~800ms (catches
        /// fast loader/init crashes; raise it for slow WER-mediated crashes).
        /// Must precede the app (args after it pass through).
        #[arg(long)]
        watch: Option<u64>,
        /// Executable path or registered app name. (Named `app`, not `target`,
        /// to avoid colliding with the global `-t/--target` arg id.)
        app: String,
        /// Arguments passed to the launched app.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Bring a window to the foreground, restoring it if minimized — so a
    /// capture or input lands on a real, visible window.
    // Field is `window`, not `target`, to avoid the global `-t/--target` arg id.
    Activate {
        /// Window handle (from `windows`).
        window: u64,
    },
    /// Click a UI element by its id (from `elements`).
    Click {
        /// Element id (from `elements`/`find`).
        element_id: String,
    },
    /// Read one element's text (its value, else accessible name) — cheaper than
    /// dumping the whole element tree to verify a single control.
    Read {
        /// Element id (from `elements`/`find`).
        element_id: String,
    },
    /// Type Unicode text; `--into <id>` targets a control, `--paste` for long text.
    Type {
        /// The Unicode text to type.
        text: String,
        /// Focus this element first (id from `elements`/`find`) — more reliable
        /// than typing into whatever currently has focus.
        #[arg(long, value_name = "ELEMENT_ID")]
        into: Option<String>,
        /// Paste via the clipboard (Ctrl+V) instead of per-key injection — far
        /// faster for long text. Clobbers the clipboard.
        #[arg(long)]
        paste: bool,
    },
    /// Press a key or chord, or a sequence (`--into` focuses an element first).
    ///
    /// E.g. `enter`, `esc`, `f5`, `ctrl+c`, `ctrl+shift+esc`, `alt+f4`. Modifiers
    /// (`ctrl`/`alt`/`shift`/`win`) join the key with `+`. Multiple chords run in
    /// order on one connection (e.g. `arc key ctrl+a delete enter`).
    Key {
        /// One or more chords, applied in order.
        #[arg(required = true)]
        chords: Vec<String>,
        /// Focus this element first (id from `elements`/`find`) before sending
        /// the chords — symmetric with `type --into`.
        #[arg(long, value_name = "ELEMENT_ID")]
        into: Option<String>,
    },
    /// Inject a mouse action at screen coordinates.
    Mouse {
        #[command(subcommand)]
        action: MouseCmd,
    },
    /// Set a UI element's value directly.
    Set {
        /// Element id (from `elements`/`find`).
        element_id: String,
        /// New value to set.
        value: String,
    },
    /// Read or write the remote clipboard.
    Clip {
        #[command(subcommand)]
        action: ClipCmd,
    },
    /// Print a full Markdown command reference for an AI agent (`arc agents-md`).
    ///
    /// Generated from the CLI itself (never drifts), prefixed with agent
    /// guidance. Feeds an agent the whole tool surface in one read; runs locally
    /// (no runner connection). Redirect into an AGENTS.md.
    AgentsMd,
}

/// Clipboard sub-actions for `arc clip`.
#[derive(Subcommand)]
enum ClipCmd {
    /// Print the remote clipboard's text to stdout.
    Get,
    /// Set the remote clipboard. Pass text, or `-` to read it from stdin.
    Set {
        /// Text to place on the clipboard (`-` reads it from stdin).
        text: String,
    },
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
    // A local doc command: no config, no runner connection.
    if matches!(cli.command, Some(Cmd::AgentsMd)) {
        print!("{}", agents_md());
        return Ok(0);
    }

    let config = resolve_config(&cli)?;

    // MCP server mode connects lazily (on first tool call), so don't dial here.
    if cli.mcp {
        run_mcp(config).await?;
        return Ok(0);
    }
    let Some(command) = cli.command else {
        bail!("no command — see `arc --help`, or pass `--mcp` to run as an MCP server");
    };

    // Forward opens its own per-connection sessions (a tunnel each) and never
    // uses the shared request/response controller, so handle it before dialing.
    if let Cmd::Forward { spec } = &command {
        return forward::run(&config, spec).await;
    }

    let mut controller = Controller::connect(&config)
        .await
        .context("connecting to runner")?;

    match command {
        Cmd::Shell {
            cmd,
            timeout,
            env,
            env_file,
            args,
        } => {
            return shell(&mut controller, cmd, timeout, env, env_file, args).await;
        }
        Cmd::Run {
            script,
            lang,
            detach,
            timeout,
            env,
            env_file,
            args,
        } => {
            return run_script(
                &mut controller,
                &script,
                lang.as_deref(),
                detach,
                timeout,
                env,
                env_file,
                args,
            )
            .await;
        }
        Cmd::Ps { pattern, cpu } => {
            return ps(&mut controller, pattern.as_deref(), cpu).await;
        }
        Cmd::Whoami => {
            return whoami(&mut controller).await;
        }
        Cmd::Doctor => {
            return doctor(&mut controller).await;
        }
        Cmd::Capabilities => {
            return capabilities(&mut controller).await;
        }
        // Handled before the controller connect (opens its own sessions).
        Cmd::Forward { .. } => unreachable!("forward is dispatched before connecting"),
        Cmd::Kill { process, dry_run } => {
            return kill(&mut controller, &process, dry_run).await;
        }
        Cmd::Procdump { pid, out } => {
            return procdump(&mut controller, pid, out.as_deref()).await;
        }
        Cmd::Tail {
            remote,
            lines,
            follow,
        } => {
            return tail(&mut controller, &remote, lines, follow).await;
        }
        Cmd::Push {
            local,
            remote,
            delete,
            dry_run,
            whole,
            no_ignore,
        } => {
            push(
                &mut controller,
                &local,
                &remote,
                delete,
                dry_run,
                whole,
                no_ignore,
            )
            .await?
        }
        Cmd::Pull {
            remote,
            local,
            delete,
            dry_run,
            whole,
            no_ignore,
            watch,
            interval,
        } => {
            pull(
                &mut controller,
                &remote,
                &local,
                delete,
                dry_run,
                whole,
                no_ignore,
                watch,
                interval,
            )
            .await?
        }
        Cmd::Watch {
            local,
            remote,
            on_change,
            no_ignore,
        } => {
            watch(
                &mut controller,
                &local,
                &remote,
                on_change.as_deref(),
                no_ignore,
            )
            .await?
        }
        Cmd::Cat { remote } => cat(&mut controller, &remote).await?,
        Cmd::Screencap {
            out,
            window,
            element,
            baseline,
            diff,
            threshold,
        } => {
            return screencap(
                &mut controller,
                &out,
                window,
                element,
                baseline.as_deref(),
                diff.as_deref(),
                threshold,
            )
            .await;
        }
        Cmd::Shot {
            out,
            app,
            window,
            launch,
            wait,
        } => shot(&mut controller, &out, app, window, launch, wait).await?,
        Cmd::Windows { json, filter } => windows(&mut controller, json, filter.as_deref()).await?,
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
        Cmd::Open { watch, app, args } => return open(&mut controller, app, args, watch).await,
        Cmd::Activate { window } => {
            ack(
                &mut controller,
                Command::ActivateWindow {
                    window: WindowId(window),
                },
            )
            .await?;
        }
        Cmd::Click { element_id } => {
            ack(
                &mut controller,
                Command::Click {
                    target: ClickTarget::Element(ElementId(element_id)),
                },
            )
            .await?;
        }
        Cmd::Read { element_id } => match controller
            .request(Command::ReadElement {
                element: ElementId(element_id),
            })
            .await?
        {
            Reply::Text(text) => println!("{text}"),
            other => bail!("unexpected reply: {other:?}"),
        },
        Cmd::Type { text, into, paste } => {
            ack(
                &mut controller,
                Command::TypeText {
                    text,
                    into: into.map(ElementId),
                    paste,
                },
            )
            .await?
        }
        Cmd::Key { chords, into } => keys(&mut controller, chords, into).await?,
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
        Cmd::Clip { action } => clip(&mut controller, action).await?,
        Cmd::AgentsMd => unreachable!("handled before config resolution"),
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

pub(crate) async fn ack(controller: &mut Controller, command: Command) -> Result<()> {
    match controller.request(command).await? {
        Reply::Ack => Ok(()),
        other => bail!("expected ack, got {other:?}"),
    }
}
