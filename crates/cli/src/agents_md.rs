use clap::CommandFactory;

use crate::Cli;

/// Hand-written guidance that prefixes the generated command reference — the
/// things a clap `--help` can't convey: what arc is, how to connect, the core
/// workflows, and the session-capability gotchas.
const AGENTS_PREAMBLE: &str = r#"# arc — agent reference

`arc` is an adb-style CLI that drives a remote **Windows** machine. You run it on
macOS/Linux (the controller); commands execute on the Windows `arc-runner` over
an encrypted link. This document is generated from the CLI itself, so it always
matches the installed version (`arc --version`).

## Connecting

Every command needs a target runner. Easiest is a named target in
`~/.config/arc/config.toml` selected with `-t <name>` (or the file's `default`):

```toml
default = "win"
[targets.win]
direct = "100.x.y.z:8787"   # the runner's Tailscale IP
trust_tailnet = true        # or: pairing = "XXXX-XXXX"
```

Then e.g. `arc -t win shell --cmd ver`. Flags `--direct/--relay/--session/--pairing`
and `ARC_*` env vars override per field.

## Core workflows

- **Run a command / build:** `arc shell --cmd 'dotnet build'` (streams live). For
  a local script, `arc run ./fix.ps1` (ships its contents — no quoting). To run a
  multi-line script with **no file and no escaping**, pipe it via stdin:
  `arc run --lang ps1 - <<'EOF'` … `EOF` (flags must precede `-`).
- **Pass secrets/config:** `arc shell --env KEY=VAL …` / `--env-file f` (also on
  `arc run`) injects into the process environment, never the command line — the
  safe way to feed a token or password.
- **Sync files:** `arc push ./src C:/work/src` — a directory push *is* an
  incremental sync (re-run it, only changed files transfer; `.gitignore`-aware,
  build dirs skipped). No separate `sync` verb — direction is the verb (`push`
  up, `pull` down). To ship **build artifacts** (which are `.gitignore`'d),
  `arc push --no-ignore ./dist C:/work/dist` (aim it at the output dir, not the
  repo root). Best for platform-agnostic outputs (JS/managed/assets); for native
  Windows binaries it's usually simpler to build on the box. The reverse — build
  on the box, flow outputs back — is `arc pull --watch <remote> ./out` (poll +
  pull changes; add `--no-ignore` to fetch from a build dir like `target/`).
- **Inner dev loop:** `arc watch ./src C:/work/src --on-change 'cargo build'` —
  auto-syncs on save and rebuilds on the box, output streaming (add `--no-ignore`
  to watch a build-output dir and re-ship artifacts as they're produced).
- **Long tasks (installer, restore):** `arc run --detach <script>` returns a pid +
  log path immediately instead of blocking; follow with `arc tail -f <log>`,
  manage with `arc ps` / `arc kill <pid>`. The job outlives the connection.
- **Reach a service on the box:** `arc forward 5000:5000` tunnels local
  `127.0.0.1:5000` to the runner's `127.0.0.1:5000` — hit a dev server / API /
  debugger port on the box from your machine. `arc forward 5000:host:5000` dials
  another host from the box. Runs until interrupted; direct (Tailscale) mode
  recommended. (On Tailscale you can also reach `0.0.0.0`-bound services at the
  box's tailnet IP directly; forward is for localhost-bound ones.)
- **Debug via log files:** have the program write a detailed log, then read it
  back with `arc tail -f C:/…/app.log` (or `arc cat`). Reading the program's own
  log is the most reliable way to pin down where something hangs or fails —
  reach for it before trying to inspect runtime state.
- **Diagnose a hang:** `arc ps --cpu <name>` adds an instantaneous CPU% column —
  a spinning process reads high, a thread blocked (e.g. on I/O) reads ~0%. Tells
  a busy loop from a deadlock/wait. For the actual stacks, `arc procdump <pid>`
  writes a minidump on the box and pulls it back; open it locally in WinDbg/cdb
  with symbols.
- **See the UI:** `arc shot ui.png --app <substr>` launches/finds the window,
  waits for it to render, activates it, and screenshots — one shot (with
  `--launch`, it fails fast if the process exits before a window appears). Or
  `arc screencap out.png --window <hwnd>`.
- **Inspect the UI tree:** `arc windows --json` → pick a handle →
  `arc elements <hwnd> --json` or `arc find <hwnd> --type Button --name Save`.
- **Drive a control:** prefer UIA — `arc click <id>`, `arc set <id> 'text'`,
  `arc read <id>` (read back to verify, no screenshot needed). For typing,
  `arc type 'text' --into <id>` (focuses first); long text: add `--paste`.
- **Regression check:** `arc screencap now.png --window <h> --baseline ref.png
  --diff diff.png` (exits non-zero past `--threshold`).

## Gotchas (read these)

- **Launch GUI apps with `arc open <exe>`, not `arc shell 'start ...'`** — a
  `shell` start can hang on the pipe. `open` briefly watches the process: if it
  crashes on startup it reports the exit code + the matching Application error
  event (faulting module + exception code) and exits non-zero, instead of looking
  like a success. Raise `--watch <ms>` for a slow (WER-mediated) crash, or `0` to
  not wait.
- **`arc shell` (no `--cmd`) already *is* PowerShell — write PowerShell directly.**
  Don't wrap it in `powershell -Command "…"`: the outer shell then expands your
  `$vars` first, so `$src` / `$_` / `$env:X` arrive empty. Just
  `arc shell '$x=42; Write-Output $x'`. (arc passes the command verbatim — it does
  no variable substitution of its own.)
- **`arc shell --cmd` runs through cmd.exe** — nested quotes, pipes, and
  PowerShell one-liners get mangled by cmd parsing. For anything quote-heavy or
  multi-line, use `arc run` (ships the script as data, no shell to escape).
- **Check privilege with `arc whoami`.** An `arc-runner install` runs the runner
  **non-elevated** (Medium integrity, not admin) — so commands already reflect a
  real user. If yours reports `admin: True`, remember that hides issues a normal
  user hits (AV on unsigned exes, UAC, code-signing) — and, conversely, a
  non-admin runner can't do admin-only ops (writing Program Files, Defender
  exclusions). `arc whoami` tells you which world you're in.
- **Session-capability tiers:** UIA paths (`windows`/`elements`/`find`/`click`/
  `set`/`read`) and per-window `screencap`/`shot` work **even when RDP is
  disconnected**. Raw input (`type`/`key`/`mouse`) and **full-screen** capture
  need an **active** session (connected RDP, or a console/virtual display).
  Run `arc doctor` to see the current tier (and `arc whoami` for privilege).
- **A minimized window captures as a sliver** — `arc activate <hwnd>` first (or
  use `shot`, which activates automatically).
- **Output and scripts are UTF-8.** Non-ASCII (e.g. 中文 / 日本語) in commands,
  script contents, and output is handled correctly — no need to stick to ASCII.
- **Most list commands take `--json`** for structured output instead of text.
- **Verify with `arc read <id>`** instead of screenshotting when you just need a
  control's text — far cheaper.

---
"#;

/// Generates a complete Markdown reference of every command and flag straight
/// from the clap definitions, so it can never drift from the real CLI.
pub(crate) fn agents_md() -> String {
    let mut out = String::from(AGENTS_PREAMBLE);
    let cmd = Cli::command();

    out.push_str("\n## Global options\n\nThese apply to every command:\n\n");
    for arg in cmd.get_arguments() {
        if matches!(arg.get_id().as_str(), "help" | "version") {
            continue;
        }
        out.push_str(&render_arg(arg));
    }

    out.push_str("\n## Commands\n");
    let mut subs: Vec<_> = cmd
        .get_subcommands()
        .filter(|c| !matches!(c.get_name(), "help" | "agents-md"))
        .collect();
    subs.sort_by_key(|c| c.get_name().to_owned());
    for sub in subs {
        render_command(sub, "arc", 3, &mut out);
    }
    out
}

/// Renders one command (recursing into nested subcommands) as a Markdown section.
fn render_command(c: &clap::Command, parent: &str, level: usize, out: &mut String) {
    let path = format!("{parent} {}", c.get_name());
    out.push_str(&format!("\n{} `{path}`\n\n", "#".repeat(level)));
    if let Some(about) = c.get_long_about().or_else(|| c.get_about()) {
        out.push_str(&format!("{}\n\n", about.to_string().replace('\n', " ")));
    }

    let args: Vec<_> = c
        .get_arguments()
        .filter(|a| !matches!(a.get_id().as_str(), "help" | "version"))
        .collect();
    let positionals: Vec<_> = args.iter().filter(|a| a.is_positional()).collect();
    let options: Vec<_> = args.iter().filter(|a| !a.is_positional()).collect();
    if !positionals.is_empty() {
        out.push_str("Arguments:\n\n");
        for a in &positionals {
            out.push_str(&render_arg(a));
        }
        out.push('\n');
    }
    if !options.is_empty() {
        out.push_str("Options:\n\n");
        for a in &options {
            out.push_str(&render_arg(a));
        }
        out.push('\n');
    }

    let mut subs: Vec<_> = c
        .get_subcommands()
        .filter(|s| s.get_name() != "help")
        .collect();
    subs.sort_by_key(|s| s.get_name().to_owned());
    for sub in subs {
        render_command(sub, &path, level + 1, out);
    }
}

/// Renders a single argument/flag as a Markdown bullet.
fn render_arg(a: &clap::Arg) -> String {
    // A flag only takes a value if its action consumes one (skip bool/count).
    let takes_value = !matches!(
        a.get_action(),
        clap::ArgAction::SetTrue | clap::ArgAction::SetFalse | clap::ArgAction::Count
    );
    let value_name = a
        .get_value_names()
        .and_then(|v| v.first())
        .map(|s| s.to_string());
    let token = if a.is_positional() {
        format!(
            "`<{}>`",
            value_name.unwrap_or_else(|| a.get_id().to_string())
        )
    } else {
        let mut flag = String::new();
        if let Some(s) = a.get_short() {
            flag.push_str(&format!("-{s}, "));
        }
        if let Some(l) = a.get_long() {
            flag.push_str(&format!("--{l}"));
        }
        if let (true, Some(vn)) = (takes_value, value_name) {
            flag.push_str(&format!(" <{vn}>"));
        }
        format!("`{flag}`")
    };
    let help = a
        .get_help()
        .map(|h| h.to_string().replace('\n', " "))
        .unwrap_or_default();
    let mut tags = Vec::new();
    if a.is_required_set() {
        tags.push("required".to_owned());
    }
    let defaults: Vec<String> = a
        .get_default_values()
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    if !defaults.is_empty() {
        tags.push(format!("default: {}", defaults.join(" ")));
    }
    let suffix = if tags.is_empty() {
        String::new()
    } else {
        format!(" _({})_", tags.join(", "))
    };
    // Drop the dangling em-dash when an argument has no help text.
    match (help.is_empty(), suffix.is_empty()) {
        (true, true) => format!("- {token}\n"),
        (true, false) => format!("- {token}{suffix}\n"),
        (false, _) => format!("- {token} — {help}{suffix}\n"),
    }
}
