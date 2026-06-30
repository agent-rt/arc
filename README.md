# arc

[![CI](https://github.com/agent-rt/arc/actions/workflows/ci.yml/badge.svg)](https://github.com/agent-rt/arc/actions/workflows/ci.yml)

**Agent remote control.** Let an AI agent on one machine (e.g. Claude Code on
macOS) drive another machine (e.g. Windows) over an encrypted link — build and
run apps, operate GUIs, move files — through **semantic, structured tools**, not
a pixel stream.

It is *not* a remote desktop: there's no screen-sharing for humans. The agent
calls tools — run a command, list/click UI elements, type, screenshot, transfer
files — over an end-to-end-encrypted channel. Part of
[`agent-rt`](https://github.com/agent-rt) (`rt` = runtime).

## Install

**Controller** (macOS / Linux) — the `arc` CLI, which is also the MCP server:

```bash
brew install agent-rt/tap/arc
```

**Runner** (Windows) — the machine being driven:

```powershell
winget install agent-rt.arc-runner
```

## Quickstart

Over Tailscale (recommended — no relay, no pairing code to copy):

1. On the **Windows** runner, register it (autostarts at logon). On Tailscale
   it's one flag — `--tailscale` auto-detects the tailnet IP and restricts
   access to this machine's Tailscale owner:

   ```powershell
   arc-runner install --tailscale
   ```

   (Equivalent to `--listen <tailnet-ip>:8787 --trust-tailnet --allow <owner>`,
   still passable explicitly; `--allow-any` permits any tailnet peer, `--port`
   changes the port, `--dry-run` previews.) It mints credentials and prints a
   ready-to-paste `[targets.win]` block.

2. On the **controller**, drop that block into `~/.config/arc/config.toml`:

   ```toml
   default = "win"

   [targets.win]
   direct = "<tailnet-ip>:8787"
   trust_tailnet = true
   ```

3. Drive it:

   ```bash
   arc -t win shell --cmd 'ver'
   ```

`arc-runner uninstall` removes the task; `arc-runner upgrade` self-updates it to
the latest release (download → validate → swap → restart). For relay mode
instead of Tailscale, use `arc-runner install --relay <ws-url>`.

> Coming from `ssh` + `scp`? See **[docs/from-ssh.md](docs/from-ssh.md)** for a
> side-by-side migration of the common workflow.

## Usage

```bash
arc shell --cmd 'dotnet build'      # run a command (streams live; --timeout <secs>)
arc run ./fix.ps1 -Port 8788        # ship & run a local script (.ps1/.bat); args pass through
arc push ./app C:/work/app          # send a tree up (incremental, .gitignore-aware)
arc watch ./app C:/work/app --on-change 'cargo build'   # auto-push + rebuild on every save
arc pull C:/work/app/bin ./bin      # fetch a tree (or file) back
arc cat C:/work/app/log.txt         # print a remote file
arc tail C:/work/app/log.txt -f     # follow a remote log (streams new lines)
arc shot ui.png --launch notepad    # launch → wait for render → screenshot, one shot
arc screencap shot.png --window N   # screenshot to a file (.png/.webp by extension)
arc screencap now.png --window N --baseline before.png --diff diff.png   # regression diff
arc windows --filter notepad        # list top-level windows (--json for structured)
arc elements <hwnd> --json          # list a window's UI Automation elements
arc find <hwnd> --type Button --name Save   # query elements by attribute (no full dump)
arc wait <hwnd> --name Done --timeout 30     # block until a matching element appears
arc open notepad                    # launch an app
arc activate <hwnd>                 # restore + foreground a window (before capture/input)
arc ps notepad                      # list remote processes; arc kill <pid|name>
arc click <element-id>              # click a UI element (from `elements`)
arc read <element-id>               # read one control's text (verify without a screenshot)
arc set <element-id> 'text'         # set a control's value directly
arc type 'hello' --into <element-id>   # focus an element, then type into it
arc type "$(cat big.txt)" --into <id> --paste   # paste long text via clipboard (fast)
arc key ctrl+a delete enter         # key chords in sequence: enter, f5, ctrl+c, alt+f4…
arc mouse drag 40 80 300 400        # move / click / down / up / scroll / drag
arc clip get                        # read the remote clipboard; arc clip set 'text'
```

### Connecting

`arc` finds a runner via a saved **named target** (`-t <name>`, or the config's
`default`), explicit flags (`--direct` / `--relay` / `--session` / `--pairing`),
or env vars (`ARC_DIRECT` / `ARC_RELAY_URL` / `ARC_SESSION` / `ARC_PAIRING`).
Config lives in `~/.config/arc/config.toml`:

```toml
default = "win"

[targets.win]                  # Tailscale-direct, identity-authenticated
direct = "100.x.y.z:8787"
trust_tailnet = true

[targets.public]               # over a relay
relay   = "wss://relay.example/v1/relay"
session = "0011…ffff"
pairing = "XXXX-XXXX"
```

### Files & the dev loop

`push`/`pull` are direction-by-verb (controller→runner / runner→controller). A
single file is copied wholesale; a **directory transfers incrementally** —
content-hashed with one round-trip to skip unchanged files, never walking build
dirs (`target`/`bin`/`obj`/…). Flags: `--delete` (mirror the source),
`--dry-run`, `--whole` (skip the diff). `arc watch` keeps a tree mirrored live
(debounced, build-churn-ignoring); add `--on-change '<build>'` to rebuild on the
box after every sync — a zero-ssh edit → build → run → screenshot loop. To run a
local script on the box,
`arc run ./script.ps1 [args]` ships its contents and executes it (interpreter by
extension; `.ps1` runs with `-ExecutionPolicy Bypass`) — no `push` first, no
cross-shell quoting to escape, args passed straight through.

> The model is *code lives on your Mac; the Windows box is a build/run
> container.* With **Windows App SDK 2.x** an unpackaged WinUI 3 app builds with
> just the .NET SDK (no Visual Studio), so a fresh runner + .NET SDK builds WPF /
> WinForms / console / WinUI 3.

### MCP

The same `arc` binary runs as a stdio MCP server with `arc --mcp`, exposing every
capability as a tool (`run_command`, `run_script`, `screenshot`, `list_windows`,
`list_elements`, `find_elements`, `click`, `type_text`, `set_value`, `read_file`,
`write_file`, …). Register it with, e.g.,
[`mcpctl`](https://github.com/agent-rt/mcpctl):

```bash
mcpctl server add arc --command arc --args --mcp \
  --env ARC_DIRECT=<tailnet-ip>:8787 --env ARC_PAIRING=XXXX-XXXX
```

It connects lazily on the first tool call and reconnects on drops. `run_command`
streams output (relayed as MCP progress when the client supplies a
`progressToken`), so an agent can watch a long build in real time.

## Modes

The relay exists to solve NAT traversal; on a shared private network it's
unnecessary.

- **Direct** (recommended on Tailscale / LAN): the runner listens
  (`arc-runner --listen <ip>:8787`) and the controller dials it — no relay, no
  extra process, lower latency.
- **Relay**: both peers dial an `arc-relay`, which matches them by `SessionId`
  and forwards opaque ciphertext (it holds no key).
- **Trusted-tailnet auto-pairing** (`trust_tailnet = true`): the runner
  authenticates each caller by **verified Tailscale identity** (`tailscale
  whois`, optionally restricted to `allow_logins`), so no pairing code is
  exchanged — authentication is the WireGuard transport plus identity, with a
  fixed public key for the (defense-in-depth) Noise channel.

## Architecture

```
 controller (macOS/Linux)        relay (optional)           runner (Windows)
┌────────────────────┐  wss  ┌──────────────┐  wss  ┌──────────────────────┐
│  arc  /  arc --mcp  │──────►│  arc-relay   │──────►│  arc-runner          │
│  (CLI / MCP server) │◄──────│ opaque relay │◄──────│ shell·UIA·input·     │
└────────────────────┘       │  (no key)    │       │ screenshot·files     │
         └────────── L2: Noise channel, end-to-end encrypted ──────────┘
```

- **L1 — relay** (`proto::relay`): `Hello` + `SessionId` + `Role`, then opaque
  `Relay{data}` frames forwarded between the session's two peers. Zero-knowledge.
  Skipped entirely in direct mode.
- **L2 — end-to-end** (`proto::wire`): `Request` / `Response` / `Event` carrying
  `Command`s, sealed in a Noise channel (`proto::crypto`) and chunked through the
  32 MiB frame cap.

Both transports share one `Session` (`arc-net`), used by the CLI, the MCP
server, and the runner.

### Capability tiers (by session state)

| Capability | Mechanism | In a detached session? |
|---|---|---|
| `list_*`, `set`, element `click` | UI Automation (COM) | ✅ works |
| single-window `screencap` | GDI `PrintWindow` | ✅ works |
| `type`, `key`, coordinate `mouse`, full-screen `screencap` | `SendInput` / DXGI | ⚠️ needs an active console/RDP session |

UIA *pattern-based* control works headlessly; raw input and full-screen capture
need a connected desktop. `ElementId` is `"<hwnd>:<RuntimeId>"`, resolved by
RuntimeId identity — it keeps pointing at the same control across tree changes,
or returns `NotFound`.

The runner serves requests **concurrently** (a hung command never blocks the
link or other commands) and enforces a default 10-minute command deadline
(`--timeout <secs>`; `0` to disable).

## Security

The relay sees only ciphertext. The low-entropy pairing code is never used as a
key directly: both peers run a symmetric **SPAKE2** PAKE to derive a
high-entropy key that keys `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`. Recording
the handshake gives no offline dictionary attack; a wrong code makes the Noise
handshake fail. In `trust_tailnet` mode the boundary is Tailscale ACLs plus
verified identity.

## Build from source

```bash
just build      # release `arc` (CLI + MCP server)
just lint       # clippy -D warnings (host crates)
just test
```

| Crate | Role |
|---|---|
| `arc-proto`  | wire protocol, CBOR framing, Noise channel (chunked), SPAKE2 |
| `arc-net`    | shared client transport — relay/direct `Session` |
| `arc-relay`  | zero-knowledge `wss` relay (`axum`) |
| `arc-runner` | Windows runner: shell, screenshot, UI Automation (`windows-rs`), input (`enigo`) — **Windows-only** |
| `arc-cli`    | the `arc` binary: CLI **and** `arc --mcp` server |

`arc-runner` builds only on Windows (`windows-rs`, `xcap`); the controller
crates build everywhere. Release/packaging is in [`RELEASING.md`](RELEASING.md).

## License

MIT OR Apache-2.0
