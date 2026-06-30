# Migrating a Windows dev workflow from ssh to arc

If you currently develop on a remote Windows machine over `ssh` + `scp`, `arc`
replaces that with: incremental file sync, live-streamed builds, screenshots, UI
automation, and an MCP server for agents — all end-to-end encrypted, with no port
forwarding when both machines are on Tailscale.

## 1. Install

**Your Mac (the controller):**

```bash
brew install agent-rt/tap/arc
```

**The Windows machine (the runner):**

```powershell
winget install agent-rt.arc-runner
```

> Until the winget package is merged, grab the binary directly from the
> [latest release](https://github.com/agent-rt/arc/releases/latest) instead —
> download `arc-runner.exe` and put it on `PATH` (e.g. in a folder you've added,
> or just run it by full path in step 2).

## 2. Register the runner (once, on Windows)

```powershell
arc-runner install --tailscale
```

- `--tailscale` auto-detects the machine's Tailscale IP (`tailscale ip -4`),
  listens on `<ip>:8787`, and authenticates callers by **Tailscale identity** —
  no pairing code to copy — restricting access to this node's Tailscale owner.
- Spell it out instead with `--listen <ip>:8787 --trust-tailnet --allow <login>`
  (repeat `--allow` for more people, `--allow-any` for any tailnet peer). Drop
  `--trust-tailnet` for a pairing-code flow.

It mints credentials, registers a **logon-autostart** task in your interactive
session (so screenshots / UI automation work), starts it now, and prints a
`[targets.win]` block. `arc-runner uninstall` removes it.

## 3. Point the controller at it (on the Mac)

Paste the printed block into `~/.config/arc/config.toml`:

```toml
default = "win"

[targets.win]
direct = "<tailnet-ip>:8787"
trust_tailnet = true
```

Verify: `arc -t win shell --cmd ver`.

## 4. Translate your ssh habits

| Before (ssh / scp) | After (arc) |
|---|---|
| `scp -r ./app win:C:/work/app` | `arc push ./app C:/work/app` — incremental, `.gitignore`-aware |
| edit, then re-scp, repeat | `arc watch ./app C:/work/app` — auto-syncs on every save |
| `ssh win 'cd C:/work/app && dotnet build'` | `arc shell --cmd 'dotnet build C:/work/app'` — output streams live |
| `scp fix.ps1 win:tmp && ssh win 'powershell -File tmp/fix.ps1 -Port 8788'` | `arc run ./fix.ps1 -Port 8788` — ships the local script & runs it, no copy, no quoting |
| `ssh win 'start app.exe'` | `arc open C:/work/app/bin/app.exe` |
| connect RDP to see the window | `arc screencap shot.webp --window <handle>` |
| click around by hand over RDP | `arc windows` → `arc find <handle> --name Save` (or `arc elements <handle>`) → `arc click <id>` / `arc set <id> 'text'` / `arc type` / `arc key ctrl+a delete` |
| wait for a control to appear | `arc wait <handle> --name "Done" --timeout 30` |
| `scp win:C:/work/app/bin ./bin` | `arc pull C:/work/app/bin ./bin` |

## 5. The inner loop (e.g. a WinUI 3 / .NET app)

Two terminals, no ssh, no scp:

```bash
# Terminal A — keep the runner mirrored as you edit:
arc watch ./src C:/work/myapp

# Terminal B — build, run, look:
arc shell --cmd 'dotnet build C:/work/myapp -c Debug'
arc open C:/work/myapp/bin/Debug/net8.0-windows/myapp.exe
arc screencap shot.webp --window $(arc windows | grep -i myapp | head -1 | cut -d'|' -f1)
```

> A fresh runner + the .NET SDK builds WPF / WinForms / console / **WinUI 3**
> (Windows App SDK 2.x builds unpackaged from the CLI — no Visual Studio).

## 6. For Claude Code / agents (MCP)

The same binary is an MCP server. Register `arc --mcp` with your MCP client (e.g.
[`mcpctl`](https://github.com/agent-rt/mcpctl)):

```bash
mcpctl server add arc --command arc --args --mcp \
  --env ARC_DIRECT=<tailnet-ip>:8787 --env ARC_PAIRING=<code-if-not-trust-tailnet>
```

The agent then calls `run_command`, `run_script`, `screenshot`, `list_windows`,
`list_elements`, `find_elements`, `click`, `type_text`, `press_key`, `set_value`,
`read_file`, `write_file`, … as tools. Long builds stream back as progress.

## Good to know

- **Window screenshots now work disconnected — and aren't black.** Single-window
  `screencap` uses **Windows.Graphics.Capture**, so it captures DirectComposition
  apps (**WinUI 3**, Chromium/Electron) correctly rather than black, and works
  even when RDP is disconnected. UIA `click`/`set`/`list_*`/`find`/`wait` also
  work disconnected. Still needing an **active** console/RDP session: `type`,
  `key`, coordinate `mouse` (`SendInput`) and **full-screen** `screencap` (no
  composed desktop to capture when disconnected). The runner runs in your
  interactive session so these work whenever you're connected; while
  disconnected they fail cleanly (they don't hang).
- **Launching apps vs running commands.** `arc shell` runs a command to
  completion and streams its output — use it for builds, tests, scripts. To
  launch a GUI app, use `arc open <exe> [-- args]`: it returns immediately and
  doesn't tie a connection to the app's lifetime. (`arc shell 'start "" app'`
  instead keeps the command open for as long as the app holds the inherited
  console — prefer `arc open`.) Either way, the runner serves connections
  concurrently, so a long or stuck command never blocks other operations.
- **No port forwarding.** With Tailscale, the runner binds its tailnet IP and the
  Mac dials it directly — gate access with Tailscale ACLs.
- **Updating the runner:** `arc-runner upgrade` (downloads the latest release,
  validates it, swaps the binary, restarts the task) or `winget upgrade
  agent-rt.arc-runner`. Run `arc-runner upgrade` on the box (ssh/console) or via
  a *different* runner — not through the runner being upgraded, since the restart
  stops its task. `--dry-run` downloads + validates without swapping.
