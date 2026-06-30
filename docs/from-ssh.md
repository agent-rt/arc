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
arc-runner install --listen <tailnet-ip>:8787 --trust-tailnet --allow you@example.com
```

- `<tailnet-ip>` is the Windows machine's Tailscale IP (`tailscale ip -4`).
- `--trust-tailnet --allow <login>` authenticates you by **Tailscale identity** —
  no pairing code to copy. (Drop these for a pairing-code flow instead.)

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
| `ssh win 'start app.exe'` | `arc open C:/work/app/bin/app.exe` |
| connect RDP to see the window | `arc screencap shot.webp --window <handle>` |
| click around by hand over RDP | `arc windows` → `arc elements <handle>` → `arc click <id>` / `arc set <id> 'text'` / `arc type` / `arc key ctrl+s` |
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

The agent then calls `run_command`, `screenshot`, `list_windows`,
`list_elements`, `click`, `type_text`, `set_value`, `read_file`, `write_file`, …
as tools. Long builds stream back as progress.

## Good to know

- **Session state matters.** UIA `click`/`set`/`list_*` and single-window
  `screencap` work even when the session is disconnected; `type`, `key`,
  coordinate `mouse`, and full-screen `screencap` need an **active** console/RDP
  session (they use `SendInput`/DXGI). The runner runs in your interactive
  session precisely so these work.
- **No port forwarding.** With Tailscale, the runner binds its tailnet IP and the
  Mac dials it directly — gate access with Tailscale ACLs.
- **Updating the runner:** `winget upgrade agent-rt.arc-runner` (or re-download
  the release binary and re-run `arc-runner install --repair`).
