//! `arc-runner` — the Windows-side executor.
//!
//! Connects to the relay (via the shared `arc-net` transport), completes
//! the Noise handshake with the controller, then serves
//! [`Request`](arc_proto::wire::Request)s (run a command, take a
//! screenshot, drive UI Automation, …) until the link drops, reconnecting on a
//! fixed backoff.
//!
//! Configuration is read by [`SessionConfig::from_args_and_env`].

// `unsafe` is confined to the `uia` module (Windows UI Automation COM calls),
// each call carrying a `// SAFETY:` note; the rest of the crate is safe.
#![deny(unsafe_op_in_unsafe_fn)]

mod apps;
mod capture;
mod cfg;
mod dispatch;
mod exec;
mod files;
mod input;
mod uia;

use std::path::Path;
use std::time::Duration;

use arc_net::{Session, SessionConfig, Transport};
use arc_proto::id::{PairingCode, Role, SessionId};
use arc_proto::wire::Frame;
use cfg::RunnerConfig;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

type BoxError = Box<dyn std::error::Error>;

const RECONNECT_DELAY: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Management subcommands run and exit (no serving).
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("pair") => return run_pair(&args[1..]),
        Some("install") => return run_install(&args[1..]),
        Some("uninstall") => return run_uninstall(),
        Some("upgrade") => return run_upgrade(&args[1..]),
        Some("--version" | "-V" | "version") => {
            println!("arc-runner {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }

    let file = RunnerConfig::load();

    // Direct (Tailscale/LAN) mode: listen for controllers ourselves, no relay.
    if let Some(addr) = listen_addr().or_else(|| file.as_ref().and_then(|f| f.listen.clone())) {
        let trust = file.as_ref().and_then(|f| f.trust_tailnet) == Some(true);
        let allow = file
            .as_ref()
            .and_then(|f| f.allow_logins.clone())
            .unwrap_or_default();
        // In trust mode the pairing is a public constant; identity (WhoIs) is
        // the real gate, so no code need be configured.
        let pairing = if trust {
            PairingCode::tailnet_auto()
        } else {
            resolve_pairing(file.as_ref())?
        };
        return listen_loop(&addr, &pairing, trust, &allow).await;
    }

    // Relay mode: dial out to the relay and reconnect on a fixed backoff.
    let config = resolve_relay_config(file.as_ref())?;
    tracing::info!(transport = ?config.transport, session = %config.session, "runner starting (relay)");
    loop {
        match Session::connect(&config, Role::Runner).await {
            Ok(session) => {
                tracing::info!("link established");
                serve(session).await;
                tracing::info!("link closed");
            }
            Err(e) => tracing::warn!(%e, "connect failed"),
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// Generates fresh credentials, persists them to `runner.toml`, and prints the
/// connect info plus a ready-to-paste controller target. Optional `--listen
/// <addr>` / `--relay <url>` records the endpoint too.
fn run_pair(args: &[String]) -> Result<(), BoxError> {
    let (mut listen, mut relay) = (None, None);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--listen" => listen = it.next().cloned(),
            "--relay" => relay = it.next().cloned(),
            other => return Err(format!("unknown `pair` argument: {other}").into()),
        }
    }

    let session = SessionId::generate()?;
    let pairing = PairingCode::generate()?;
    let path = RunnerConfig {
        session: Some(session.to_string()),
        pairing: Some(pairing.to_string()),
        listen: listen.clone(),
        relay: relay.clone(),
        ..Default::default()
    }
    .save()?;

    println!("Paired. Credentials saved to {}\n", path.display());
    println!("  Session: {session}");
    println!("  Pairing: {pairing}\n");
    println!("On the controller, add to ~/.config/arc/config.toml:\n");
    println!("  [targets.win]");
    match (&listen, &relay) {
        (Some(addr), _) => println!("  direct  = \"{addr}\""),
        (None, Some(url)) => {
            println!("  relay   = \"{url}\"");
            println!("  session = \"{session}\"");
        }
        (None, None) => {
            println!("  # direct = \"<this-host-tailscale-ip>:8787\"  # if started with --listen");
            println!("  # relay  = \"wss://<relay-host>/v1/relay\"");
            println!("  session = \"{session}\"");
        }
    }
    println!("  pairing = \"{pairing}\"\n");
    println!("then: arc -t win shell --cmd ver");
    Ok(())
}

/// The logon-autostart scheduled task name created by `install`.
const TASK_NAME: &str = "arc-runner";

/// Default direct-mode port (`--tailscale` listens here unless `--port` overrides).
const DEFAULT_PORT: u16 = 8787;

/// `arc-runner install [--tailscale [--port <n>] [--allow-any]] [--listen <addr>
/// | --relay <url>] [--trust-tailnet] [--allow <login>]... [--repair]`: pair
/// (mint + persist credentials on first run), register a logon-autostart
/// scheduled task in the interactive session (so screenshots / UI Automation
/// work), start it, and print the controller target.
///
/// `--tailscale` is the one-flag common case: it reads this node's tailnet IP
/// (`tailscale ip -4`), listens on `<ip>:8787`, turns on trusted-identity
/// auth, and restricts access to the node's own Tailscale owner — equivalent to
/// `--listen <ip>:8787 --trust-tailnet --allow <owner>`, all auto-detected.
fn run_install(args: &[String]) -> Result<(), BoxError> {
    let (mut listen, mut relay, mut trust, mut repair) = (None, None, false, false);
    let (mut tailscale_mode, mut allow_any, mut dry_run) = (false, false, false);
    let mut port: u16 = DEFAULT_PORT;
    let mut allow: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--listen" => listen = it.next().cloned(),
            "--relay" => relay = it.next().cloned(),
            "--trust-tailnet" => trust = true,
            "--tailscale" => tailscale_mode = true,
            "--port" => {
                port = it
                    .next()
                    .and_then(|p| p.parse().ok())
                    .ok_or("--port needs a port number")?;
            }
            "--allow" => {
                if let Some(login) = it.next() {
                    allow.push(login.clone());
                }
            }
            "--allow-any" => allow_any = true,
            "--repair" => repair = true,
            "--dry-run" => dry_run = true,
            other => return Err(format!("unknown `install` argument: {other}").into()),
        }
    }

    // `--tailscale`: derive the direct-mode listen address + trusted identity
    // from the local Tailscale node, so the common case is a single flag.
    if tailscale_mode {
        if relay.is_some() {
            return Err("--tailscale is direct mode (no relay); drop --relay".into());
        }
        trust = true;
        if listen.is_none() {
            let ip = tailscale_ip().ok_or(
                "could not read this node's Tailscale IP (`tailscale ip -4`); is Tailscale up?",
            )?;
            listen = Some(format!("{ip}:{port}"));
        }
        if allow.is_empty() && !allow_any {
            let owner = tailscale_owner_login().ok_or(
                "could not detect this node's Tailscale owner (`tailscale status --json`); \
                 pass --allow <login> or --allow-any",
            )?;
            println!(
                "Restricting access to this node's Tailscale owner: {owner}\n  \
                 (--allow <login> to add others; --allow-any for any tailnet peer)\n"
            );
            allow.push(owner);
        }
    }

    if listen.is_none() && relay.is_none() {
        return Err(
            "pass --tailscale, or --listen <host:port> (direct), or --relay <ws-url>".into(),
        );
    }

    let exe = std::env::current_exe()?;
    let cfg_path = cfg::path().ok_or("no config directory")?;

    if dry_run {
        println!("install --dry-run (no changes made):");
        match (&listen, &relay) {
            (Some(addr), _) => println!("  mode    : direct, listen {addr}"),
            (None, Some(url)) => println!("  mode    : relay {url}"),
            _ => {}
        }
        println!("  trust   : {trust}");
        println!(
            "  allow   : {}",
            if allow.is_empty() {
                "<any tailnet peer>".to_owned()
            } else {
                allow.join(", ")
            }
        );
        println!(
            "  task    : would register '{TASK_NAME}' running {}",
            exe.display()
        );
        println!("  config  : {}", cfg_path.display());
        return Ok(());
    }

    // Mint credentials on first install (or --repair); otherwise keep them.
    let (session, pairing) = if repair || !cfg_path.exists() {
        let session = SessionId::generate()?;
        let pairing = if trust {
            None // trust mode authenticates by identity, not a code
        } else {
            Some(PairingCode::generate()?)
        };
        RunnerConfig {
            session: Some(session.to_string()),
            pairing: pairing.as_ref().map(ToString::to_string),
            listen: listen.clone(),
            relay: relay.clone(),
            trust_tailnet: trust.then_some(true),
            allow_logins: (!allow.is_empty()).then(|| allow.clone()),
        }
        .save()?;
        (session, pairing)
    } else {
        let existing =
            RunnerConfig::load().ok_or("existing runner.toml is unreadable; pass --repair")?;
        let session = existing
            .session
            .as_deref()
            .and_then(|s| s.parse().ok())
            .ok_or("existing runner.toml has no valid session; pass --repair")?;
        let pairing = existing
            .pairing
            .as_deref()
            .and_then(|p| PairingCode::parse(p).ok());
        println!(
            "Keeping existing credentials ({}); pass --repair to regenerate.\n",
            cfg_path.display()
        );
        (session, pairing)
    };

    register_task(&exe)?;
    println!("Installed: scheduled task '{TASK_NAME}' runs at logon (started now).\n");
    print_controller_target(&listen, &relay, trust, &session, pairing.as_ref());
    Ok(())
}

/// Registers (or replaces) the logon-autostart task running `exe` directly in
/// the user's interactive session, and starts it.
fn register_task(exe: &Path) -> Result<(), BoxError> {
    let run = format!("\"{}\"", exe.display());
    let created = std::process::Command::new("schtasks")
        .args([
            "/create", "/tn", TASK_NAME, "/tr", &run, "/sc", "onlogon", "/rl", "LIMITED", "/f",
        ])
        .status()?;
    if !created.success() {
        return Err("schtasks /create failed".into());
    }
    let _ = std::process::Command::new("schtasks")
        .args(["/run", "/tn", TASK_NAME])
        .status();
    Ok(())
}

/// `arc-runner uninstall`: stop and remove the scheduled task.
fn run_uninstall() -> Result<(), BoxError> {
    let _ = std::process::Command::new("schtasks")
        .args(["/end", "/tn", TASK_NAME])
        .status();
    let deleted = std::process::Command::new("schtasks")
        .args(["/delete", "/tn", TASK_NAME, "/f"])
        .status()?;
    if deleted.success() {
        println!("Uninstalled: scheduled task '{TASK_NAME}' removed.");
    } else {
        println!("No scheduled task '{TASK_NAME}' to remove.");
    }
    Ok(())
}

/// `arc-runner upgrade [--version <vX.Y.Z>]`: download the release
/// `arc-runner.exe` (latest by default), validate it, swap it in beside the
/// running binary, and restart the scheduled task.
///
/// Run this **on the box** (ssh / console) or via a *different* runner — not
/// through the runner being upgraded: the restart stops the `arc-runner` task,
/// the same self-kill rule as `taskkill` / `schtasks /end` on yourself.
fn run_upgrade(args: &[String]) -> Result<(), BoxError> {
    let mut version = None;
    let mut dry_run = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--version" => version = it.next().cloned(),
            "--dry-run" => dry_run = true,
            other => return Err(format!("unknown `upgrade` argument: {other}").into()),
        }
    }
    let url = match &version {
        Some(v) => format!("https://github.com/agent-rt/arc/releases/download/{v}/arc-runner.exe"),
        None => {
            "https://github.com/agent-rt/arc/releases/latest/download/arc-runner.exe".to_owned()
        }
    };
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or("current exe has no parent directory")?;
    let new = dir.join("arc-runner.exe.new");
    let bak = dir.join("arc-runner.exe.bak");

    // Download with PowerShell — always present on Windows, so no HTTP dep.
    println!("Downloading {url}");
    let status = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Invoke-WebRequest -UseBasicParsing -Uri '{url}' -OutFile '{}'",
                new.display()
            ),
        ])
        .status()?;
    if !status.success() {
        return Err("download failed".into());
    }

    // Validate without executing (an old build would hang in serve mode): a real
    // PE binary starts with "MZ" and the release exe is tens of MB.
    let len = std::fs::metadata(&new)?.len();
    let mut magic = [0u8; 2];
    {
        use std::io::Read;
        std::fs::File::open(&new)?.read_exact(&mut magic)?;
    }
    if &magic != b"MZ" || len < 1_000_000 {
        let _ = std::fs::remove_file(&new);
        return Err(format!("downloaded file is not a valid exe (size {len}); aborting").into());
    }

    if dry_run {
        let _ = std::fs::remove_file(&new);
        println!(
            "upgrade --dry-run: downloaded + validated {len} bytes (valid exe); would swap and restart '{TASK_NAME}'. No changes made."
        );
        return Ok(());
    }

    // Stop the task to release its lock, then swap (renaming a running exe is
    // allowed on Windows even though overwriting it in place is not) and restart.
    let _ = std::process::Command::new("schtasks")
        .args(["/end", "/tn", TASK_NAME])
        .status();
    std::thread::sleep(Duration::from_secs(3));
    let _ = std::fs::remove_file(&bak);
    std::fs::rename(&exe, &bak).map_err(|e| format!("moving old binary aside: {e}"))?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&bak, &exe); // roll back
        return Err(format!("installing new binary: {e}").into());
    }
    let _ = std::fs::remove_file(&bak); // best effort (may still be mapped)

    std::process::Command::new("schtasks")
        .args(["/run", "/tn", TASK_NAME])
        .status()?;
    println!("Upgraded arc-runner ({len} bytes) and restarted task '{TASK_NAME}'.");
    Ok(())
}

/// Runs the Tailscale CLI with `args`, trying `PATH` then the default install
/// dir (the task environment often lacks Tailscale's dir on `PATH`), returning
/// stdout on success. Mirrors [`whois_login`]'s lookup, synchronously.
fn tailscale(args: &[&str]) -> Option<String> {
    let mut candidates = vec!["tailscale".to_owned()];
    if let Ok(program_files) = std::env::var("ProgramFiles") {
        candidates.push(format!("{program_files}\\Tailscale\\tailscale.exe"));
    }
    for program in candidates {
        if let Ok(output) = std::process::Command::new(&program).args(args).output()
            && output.status.success()
        {
            return Some(String::from_utf8_lossy(&output.stdout).into_owned());
        }
    }
    None
}

/// This node's Tailscale IPv4 (`tailscale ip -4`, first line).
fn tailscale_ip() -> Option<String> {
    tailscale(&["ip", "-4"])?
        .lines()
        .next()
        .map(|l| l.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// The login that owns this node, from `tailscale status --json`
/// (`.Self.UserID` → `.User[<id>].LoginName`).
fn tailscale_owner_login() -> Option<String> {
    let json = tailscale(&["status", "--json"])?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    let uid = value["Self"]["UserID"].as_u64()?;
    value["User"]
        .get(uid.to_string())?
        .get("LoginName")?
        .as_str()
        .map(ToOwned::to_owned)
}

/// Prints the ready-to-paste `[targets.win]` block for the controller config.
fn print_controller_target(
    listen: &Option<String>,
    relay: &Option<String>,
    trust: bool,
    session: &SessionId,
    pairing: Option<&PairingCode>,
) {
    println!("On the controller, add to ~/.config/arc/config.toml:\n");
    println!("  [targets.win]");
    match (listen, relay) {
        (Some(addr), _) => println!("  direct  = \"{addr}\""),
        (None, Some(url)) => {
            println!("  relay   = \"{url}\"");
            println!("  session = \"{session}\"");
        }
        (None, None) => println!("  # set direct or relay"),
    }
    if trust {
        println!("  trust_tailnet = true");
    } else if let Some(pairing) = pairing {
        println!("  pairing = \"{pairing}\"");
    }
    println!("\nthen: arc -t win shell --cmd ver");
}

/// Direct-mode listen address from `--listen <addr>` or `ARC_LISTEN`
/// (e.g. `100.x.y.z:8787` to bind only the Tailscale interface).
fn listen_addr() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--listen" {
            return args.next();
        }
    }
    std::env::var("ARC_LISTEN").ok().filter(|s| !s.is_empty())
}

/// Pairing code from `ARC_PAIRING`, falling back to the persisted config.
fn resolve_pairing(file: Option<&RunnerConfig>) -> Result<PairingCode, BoxError> {
    let raw = std::env::var("ARC_PAIRING")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| file.and_then(|f| f.pairing.clone()))
        .ok_or("no pairing code (set ARC_PAIRING or run `arc-runner pair`)")?;
    PairingCode::parse(&raw).map_err(|_| "pairing code must be XXXX-XXXX".into())
}

/// Relay [`SessionConfig`] from args/env, falling back to the persisted config.
fn resolve_relay_config(file: Option<&RunnerConfig>) -> Result<SessionConfig, BoxError> {
    if let Ok(config) = SessionConfig::from_args_and_env() {
        return Ok(config);
    }
    let file = file.ok_or("no relay config (set ARC_* or run `arc-runner pair`)")?;
    let url = file.relay.clone().ok_or("runner.toml has no relay url")?;
    let session = file
        .session
        .as_deref()
        .ok_or("runner.toml has no session")?
        .parse::<SessionId>()?;
    let pairing = PairingCode::parse(
        file.pairing
            .as_deref()
            .ok_or("runner.toml has no pairing")?,
    )?;
    Ok(SessionConfig {
        transport: Transport::Relay { url },
        session,
        pairing,
    })
}

/// Accepts controller connections directly and serves each. With `trust`, each
/// caller is gated by its verified Tailscale identity (WhoIs) before the
/// handshake; otherwise the pairing code is the only gate.
async fn listen_loop(
    addr: &str,
    pairing: &PairingCode,
    trust: bool,
    allow_logins: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, trust_tailnet = trust, "runner listening (direct, no relay)");
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!(%e, "accept failed");
                continue;
            }
        };
        // Serve each controller in its own task: authorize, handshake, and serve
        // off the accept loop so one slow or hung connection never blocks new
        // controllers (each `arc` command is a fresh connection).
        let pairing = pairing.clone();
        let allow = allow_logins.to_vec();
        tokio::spawn(async move {
            if trust && !authorize_tailnet(&peer.ip().to_string(), &allow).await {
                return; // identity check failed; reason already logged
            }
            tracing::info!(%peer, "controller connecting");
            match Session::accept_direct(stream, &pairing).await {
                Ok(session) => {
                    tracing::info!("link established");
                    serve(session).await;
                    tracing::info!("link closed");
                }
                Err(e) => tracing::warn!(%e, "handshake failed"),
            }
        });
    }
}

/// Resolves the caller's Tailscale login via `tailscale whois` and checks it
/// against `allow_logins` (empty = any tailnet peer). Logs and returns `false`
/// on any failure, so a non-tailnet or disallowed caller is dropped.
async fn authorize_tailnet(ip: &str, allow_logins: &[String]) -> bool {
    let Some(login) = whois_login(ip).await else {
        tracing::warn!(%ip, "caller is not a known tailnet peer (or whois unavailable); rejecting");
        return false;
    };
    if allow_logins.is_empty() || allow_logins.iter().any(|a| a == &login) {
        tracing::info!(%ip, %login, "tailnet identity authorized");
        true
    } else {
        tracing::warn!(%ip, %login, "tailnet identity not in allow_logins; rejecting");
        false
    }
}

/// Runs `tailscale whois --json <ip>` and extracts `UserProfile.LoginName`.
/// Tries the CLI on `PATH` first, then the default install location, since the
/// runner's service/scheduled-task environment often lacks Tailscale's dir on
/// `PATH`. Returns `None` if it can't run or the peer isn't a tailnet node
/// (`whois` exits 0 even when unknown, so we gate on parsed JSON + login).
async fn whois_login(ip: &str) -> Option<String> {
    let mut candidates = vec!["tailscale".to_owned()];
    if let Ok(program_files) = std::env::var("ProgramFiles") {
        candidates.push(format!("{program_files}\\Tailscale\\tailscale.exe"));
    }
    for program in candidates {
        let Ok(output) = tokio::process::Command::new(&program)
            .args(["whois", "--json", ip])
            .output()
            .await
        else {
            continue; // not found at this location; try the next
        };
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
            && let Some(login) = value["UserProfile"]["LoginName"].as_str()
        {
            return Some(login.to_owned());
        }
    }
    None
}

/// Serves requests on an established session until it closes.
///
/// The reader loop only receives and dispatches; each request runs in its own
/// task and streams its result frames to a single writer task. So a slow or
/// hung command never blocks the receive loop, other in-flight commands, or
/// disconnect detection — and on close, in-flight handlers are aborted (their
/// `kill_on_drop` children die with them).
async fn serve(session: Session) {
    let (mut writer, mut reader) = session.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(128);

    // One writer task owns the send half, so Noise records stay ordered.
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if let Err(e) = writer.send_frame(&frame).await {
                tracing::warn!(%e, "send failed");
                break;
            }
        }
    });

    let mut handlers = tokio::task::JoinSet::new();
    loop {
        match reader.recv_frame().await {
            Ok(Some(Frame::Request(request))) => {
                tracing::info!(id = %request.id, "handling request");
                let out = out_tx.clone();
                handlers.spawn(async move { dispatch::handle(request, &out).await });
            }
            // The runner only ever receives requests; ignore stray frames.
            Ok(Some(_)) => tracing::warn!("ignoring unexpected non-request frame"),
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(%e, "receive error");
                break;
            }
        }
    }

    drop(out_tx); // close the outbox so the writer task can finish
    handlers.shutdown().await; // abort in-flight handlers (kills their children)
    let _ = writer_task.await;
}
