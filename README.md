# arc

**Remote Control for Agents.** The remote-control sibling of [`agent-rt`](https://github.com/agent-rt)
(`rt` = runtime, `rc` = remote control).

`arc` lets an Agent running on one machine (e.g. Claude Code on **macOS**)
drive a second machine (e.g. **Windows**) over `https`/`wss` — to build and run a
Windows app, or to open and operate an existing Windows application. It is *not*
a remote desktop: there is no pixel stream for humans. Instead it exposes
**semantic, structured tools** (run a command, list UI elements, click a
control, take a screenshot) that an Agent calls directly.

## Validated dev loop

The end goal — *a macOS Agent develops a Windows GUI app* — is verified end to
end, entirely through `mcpctl` (no `ssh`): scaffold (`run_command: dotnet new`)
→ author code (`write_file`) → build (`run_command: dotnet build`) → launch
(`open_app`) → **visually verify** (`screenshot` → the window renders the
Agent's custom UI). Demonstrated end to end with both a **WPF** hello-world and a **WinUI 3**
hello-world (authored → built → launched → screenshot shows the rendered window),
all via the `arc` CLI.

> **WinUI 3 toolchain:** with **Windows App SDK 2.x** the MSIX/PRI MSBuild tasks
> ship in the NuGet package, so an unpackaged self-contained WinUI 3 app builds
> with just the .NET SDK — no Visual Studio component needed (earlier 1.6/1.7
> referenced a VS-installed task and failed CLI-only). So a fresh box needs only
> the runner + .NET SDK to build WPF / WinForms / console **and** WinUI 3 (2.x).

## Architecture

Two protocol layers keep the public relay **zero-knowledge**:

```
 macOS (controller)            Relay (public)              Windows (runner)
┌────────────────┐   wss   ┌──────────────────┐   wss   ┌────────────────────┐
│ arc --mcp      │────────►│  arc-relay  │◄────────│ arc-runner    │
│ (MCP server)   │◄────────│  routes opaque   │────────►│ shell · UIA ·      │
│   ▲ MCP/stdio  │  L1: SessionId + ciphertext │         │ screenshot · files │
│ Claude / Agent │         │  (cannot decrypt) │         └────────────────────┘
└────────────────┘         └──────────────────┘
        └──────────── L2: Noise channel, end-to-end encrypted ──────────────┘
```

- **L1 — relay layer** (`proto::relay`): all the relay can read. `Hello` +
  `SessionId` + `Role`, then opaque `Relay { data }` frames it forwards between
  the two peers of a session.
- **L2 — end-to-end layer** (`proto::wire`): `Request`/`Response`/`Event`
  carrying `Command`s (`RunCommand`, `Screenshot`, `ListElements`, `Click`, …),
  sealed in a Noise channel (`proto::crypto`) keyed from the out-of-band pairing
  code. The relay never holds a key.

### Relay vs. direct

The relay exists to solve NAT traversal — neither peer needs a public IP. But if
both machines already share a private network (**Tailscale**, LAN), the relay is
redundant: the controller can dial the runner directly.

- **Relay mode** (default): both peers dial the relay (`--relay wss://…` /
  `ARC_RELAY_URL`), which matches them by `SessionId`.
- **Direct mode**: the runner listens itself (`arc-runner --listen
  100.x.y.z:8787`, binding its Tailscale interface) and the controller dials it
  (`arc --direct 100.x.y.z:8787` / `ARC_DIRECT`) — no relay, no extra
  process, lower latency. The same Noise + pairing handshake runs over the raw
  WebSocket, so it stays end-to-end encrypted and authenticated; bind to the
  tailnet interface and use Tailscale ACLs as the network boundary. (`SessionId`
  is only a routing key for the relay, so direct mode doesn't need one.)

Both modes share one [`Session`]; only the payload transport differs (relay
`Relay{data}` envelope vs. raw binary), selected by `transport` in the config.

**Trusted-tailnet auto-pairing.** On a tailnet you can drop the pairing code
entirely: set `trust_tailnet = true` in the runner's `runner.toml` and the
controller target. The runner then authenticates each caller by its **verified
Tailscale identity** — it asks the local `tailscaled` (`tailscale whois`) who is
at the connecting IP and accepts only a real tailnet peer (optionally restricted
to `allow_logins = ["you@example.com"]`); a non-tailnet caller is dropped before
the handshake. Authentication is thus WireGuard transport + Tailscale identity,
and a fixed public pairing keys the (redundant, defense-in-depth) Noise channel —
so no secret need ever be exchanged. Leave `trust_tailnet` off to require the
pairing code.

## Workspace

| Crate | Status | Role |
|---|---|---|
| `arc-proto` | ✅ done (8 tests, clippy-clean) | Wire protocol, CBOR framing, Noise channel with chunked seal/open |
| `arc-net`   | ✅ done | Shared client transport: relay WebSocket + Noise `Session` with framed L2 exchange (used by both `mcp` and `runner`) |
| `arc-relay` | ✅ done (boots, healthz, 2 tests) | Zero-knowledge `wss` relay (`axum`) |
| `arc-runner` | ✅ v2 (verified on Windows 11) | Shell, screenshot (WebP, GDI fallback), **UI Automation** (windows-rs) + input (`enigo`) |
| `arc-cli`    | ✅ (`arc`, verified) | the `arc` binary: adb-style CLI (`shell` / `push` / `pull` / `watch` / `screencap` / `windows` / `open` / `click` / `type` / `key` / `mouse` / `set`) **and** the MCP server via `arc --mcp` (`rmcp`, stdio) for Agent tool-calling |

### `arc` — the CLI (adb-style)

For direct/scripting use, `arc` is far terser than an MCP tool call. Point it at
a runner via env (`ARC_RELAY_URL` or `ARC_DIRECT` / `ARC_SESSION`
/ `ARC_PAIRING`), the matching flags (`--relay`/`--direct`/`--session`/
`--pairing`), or — easiest — a saved **named target** (see below) selected with
`-t <name>`. Then:

```bash
arc shell --cmd 'dotnet build'          # run a command (output streams live)
arc shell --timeout 30 --cmd 'flaky.exe' # kill it after 30s if it hangs
arc push ./myapp C:/work/myapp          # send a tree up (incremental, .gitignore-aware)
arc watch ./myapp C:/work/myapp         # auto-push on every save (the inner dev loop)
arc pull C:/work/myapp/bin ./bin        # fetch a tree (or file) back down
arc screencap shot.webp --window 132484 # screenshot straight to a file
arc windows                             # list windows
arc open notepad                        # launch an app
arc type 'hello world'                  # type Unicode text
arc key ctrl+s                          # press a key / chord (Enter, Esc, F5, Alt+F4…)
arc mouse drag 40 80 300 400            # move / click / down / up / scroll / drag
```

**Named targets.** Rather than juggle env vars, save runners in
`~/.config/arc/config.toml` (or `$ARC_CONFIG`) and pick one with
`-t <name>`; with no `-t`, the file's `default` is used. Explicit flags and env
vars still override individual fields.

```toml
default = "winbuild"

[targets.winbuild]            # Tailscale, direct (no relay, no session id)
direct  = "100.x.y.z:8787"
pairing = "A1B2-C3D4"

[targets.winpublic]           # over the public relay
relay   = "wss://relay.agent-rt.io/v1/relay"
session = "00112233445566778899aabbccddeeff"
pairing = "A1B2-C3D4"
```

```bash
arc -t winbuild shell --cmd 'ver'   # uses the winbuild target
arc shell --cmd 'ver'               # uses default = winbuild
```

**Input** comes in three flavours: `type` for Unicode text, `key` for keys and
chords (`enter`, `esc`, `f5`, `ctrl+c`, `ctrl+shift+esc`, `alt+f4` — modifiers
`ctrl`/`alt`/`shift`/`win` join with `+`), and `mouse` for coordinate actions
(`move`/`click [--count 2]`/`down`/`up`/`scroll`/`drag`, `--button left|right|
middle`). Prefer the semantic `click`/`set` (UI Automation) when an element fits;
`key`/`mouse` are the raw-input fallback. Raw input rides `SendInput`, so it
needs an active input desktop — UIA `click`/`set` work even on a disconnected
session, but `key`/`mouse`/coordinate `click` require the runner's session to be
connected (console/RDP).

**Transfer is just `push` (local → runner) and `pull` (runner → local)** — one
implicit remote per session, so the *verb* is the direction (no rsync `host:`
prefix to collide with Windows `C:` paths; a future multi-host build adds a
`-t/--target <id>` selector, orthogonal to direction).

A single named file is always copied. A **directory transfers incrementally**:
it content-hashes the source tree (skipping `.gitignore`d paths and build dirs)
and asks the other side for matching hashes in one round-trip, so a re-run moves
**only the files that changed** — never walking build outputs. Flags compose on
either verb:

- `--delete` — mirror: prune files on the destination not present on the source
  (scoped to the synced set; `target`/`bin`/`obj`/… are never enumerated or
  removed). `arc push ./app C:/work/app --delete` makes the runner an exact
  mirror; `arc pull … --delete` does the same locally.
- `--dry-run` — preview transfers/deletions without changing anything.
- `--whole` — skip the hash diff and copy every file (full transfer).

This suits the *"code lives on my Mac, the Windows box is just a build/run
container"* model: `push` source up, `pull` artifacts/logs back, both cheap.
For the inner loop, `arc watch ./src C:/work/src` keeps the runner mirrored
live — it does one initial incremental push, then re-syncs (debounced) on every
save, ignoring build-dir churn so a `cargo`/`dotnet build` never triggers it.

One `arc` binary is two front-ends over the shared `arc-net` controller
transport: the adb-style CLI for direct/scripting use, and an MCP server
(`arc --mcp`) for an Agent's tool-calling. (`arc-net::Controller` is the shared
core.)

The runner now serves the full v1 command set: `run_command`, `screenshot`,
`open_app`, `list_windows`, `list_elements`, `click` (element or point),
`type_text`, `set_value`, `read_file`, `write_file`. The MCP server exposes each
as a tool.

**File transfer** (`read_file` / `write_file`) pushes source onto the runner to
build and pulls artifacts/logs back — text (UTF-8) or binary (base64), parent
dirs auto-created. Files larger than the 32 MiB frame cap transfer in **chunks**:
`write_file` takes a byte `offset` (`0` creates/truncates, `> 0` seeks and
overwrites), and `read_file` takes `offset` + `length` for ranged reads (a short
read signals EOF). Verified end-to-end via dogfood (multi-chunk write at offsets
+ ranged read-back).

`run_command` **streams** output: the runner emits `Event::Stdout`/`Stderr`
chunks as they are produced; the controller reassembles the full output for the
tool result and, when the MCP client supplies a `progressToken`, relays each
chunk as a live progress notification — so an Agent can watch a long build in
real time. Verified end-to-end (`ping -n 4` output streamed back, exit 0).

### UI Automation verification (runner v2)

Driven from macOS via `mcpctl` against a real Notepad on Windows 11:

- ✅ `list_windows` → real top-level windows (handles, titles, processes)
- ✅ `list_elements` → the window's full semantic tree (control types + names:
  `Document`, `MenuItem` File/Edit/View, `Button` Bold/Italic, …)
- ✅ `set_value` on the editor → **observable effect**: the document changed and
  the tab re-read as `"Hello from arc UIA. Modified."`

`ElementId` is `"<hwnd>:<runtime-id>"`, where `runtime-id` is the element's UIA
**RuntimeId** (stable for its lifetime). Resolution matches an element *by
RuntimeId identity*, not tree position — so an id keeps pointing at the same
control even if siblings are added/removed/reordered between listing and acting;
if the element is gone we return `NotFound` rather than acting on the wrong one.
Verified live: after `set_value` mutated the document (tree state changed), the
editor's id `…:42.67712` was unchanged and a second `set_value` with that same
id still resolved to it — confirmed by the tab title updating both times.

**Two capability tiers, by session state** (mirrors the screenshot caveat):

| Path | Mechanism | Detached / disconnected session |
|---|---|---|
| `list_*`, `set_value`, element `click` (Invoke pattern) | UIA COM | ✅ works |
| `type_text`, coordinate `click`, `screenshot` | `SendInput` / DXGI | ⚠️ needs an *active* input desktop (else `UIPI` / `0x80070006`) |

So UIA *pattern-based* control works headlessly; raw input and capture need a
connected console/RDP session.

### Full-chain verification (mcp v1)

End-to-end across two machines: an Agent on macOS calls an MCP tool, which is
executed on Windows and returned over the encrypted channel.

```
mcpctl (macOS) → arc --mcp (controller) → relay (Windows) → arc-runner (Windows)
```

Verified with [`mcpctl`](https://crates.io/crates/mcpctl):

```bash
mcpctl server add arc \
  --command target/debug/arc --args --mcp \
  --env ARC_RELAY_URL=ws://<relay-host>:8787/v1/relay \
  --env ARC_SESSION=<32-hex> \
  --env ARC_PAIRING=XXXX-XXXX

mcpctl introspect arc                       # lists run_command, screenshot
mcpctl arc/run_command --arg command="hostname" --arg shell=cmd
#  → exit_code: 0 … DESKTOP-XXXXXXX
```

The runner **serves requests concurrently**: a reader loop dispatches each into
its own task and a single writer task drains their result frames (keeping Noise
records ordered), so a slow or hung command never blocks the receive loop, other
in-flight commands, or disconnect detection — and on close, in-flight handlers
are aborted (their `kill_on_drop` children die with them). Commands also carry a
**default 10-minute deadline** (`arc shell --timeout <secs>` / MCP `timeout_ms`
to change it; `0` for none) so a runaway command is eventually killed even if
the controller stays connected.

The MCP server connects lazily on the first tool call, completes the Noise
handshake with the runner, and reconnects automatically on a dropped link.
Runner-side failures propagate back as structured MCP errors (e.g. the
screenshot `0x80070006` in a detached session).

### End-to-end verification (runner v1)

Verified on a real Windows 11 box: `relay` + `runner` + the `probe` example
(a stand-in controller) complete the Noise handshake and round-trip commands.

```bash
# on Windows, all three locally (see scripts/e2e_windows.bat):
cargo run -p arc-runner -- ws://127.0.0.1:8787/v1/relay <session-hex> <PAIR-CODE>
cargo run -p arc-runner --example probe -- ws://127.0.0.1:8787/v1/relay <session-hex> <PAIR-CODE>
```

`RunCommand` works (shell output round-trips, exit code `0`). **Screenshot caveat:**
`xcap` uses DXGI desktop duplication, which needs an *attached* session — it
returns `0x80070006` from an SSH or **disconnected RDP** session. Capture works
when a user is connected at the console/RDP. A GDI/`PrintWindow` fallback for
detached sessions is a planned follow-up.

## Pairing (first run)

`arc-runner pair` mints a fresh session id + pairing code, saves them to
`runner.toml` (`%APPDATA%\arc\` on Windows), and prints a ready-to-paste
controller target:

```text
$ arc-runner pair --listen 100.x.y.z:8787
Paired. Credentials saved to C:\Users\me\AppData\Roaming\arc\runner.toml

  Session: a3f9…e21c
  Pairing: A1B2-C3D4

On the controller, add to ~/.config/arc/config.toml:

  [targets.win]
  direct  = "100.x.y.z:8787"
  pairing = "A1B2-C3D4"

then: arc -t win shell --cmd ver
```

The runner then reads those credentials from `runner.toml` at startup, so it can
run with no env/args once paired. The **pairing code is the one secret** to move
out-of-band (read it off the runner, type it on the controller) — on a trusted
tailnet you may simply copy the printed target block.

## Deployment (dogfood)

One-time bootstrap on the Windows box — no script, just the runner's own
subcommand (after `winget install agent-rt.arc-runner`, or a `cargo build`):

```powershell
arc-runner install --listen <tailnet-ip>:8787 --trust-tailnet --allow you@example.com
```

`arc-runner install` mints + persists credentials (`pair`), registers a
**logon-autostart** scheduled task in the user's interactive session (so
screenshots / UI Automation work), starts it, and prints the controller
`[targets.win]` block to paste into `~/.config/arc/config.toml`. No credentials
are baked into anything — the runner reads `%APPDATA%\arc\runner.toml`.
`arc-runner uninstall` removes the task. (`--relay <ws-url>` instead of
`--listen` for relay mode; the `scripts/*-loop.bat` build-aware supervisor is a
dev-only dogfood path that recompiles on each launch.) After install, the macOS
Agent drives the machine entirely through `arc -t win …` / `mcpctl arc/<tool>` —
`run_command`, file
transfer, screenshots, UI control — with **no `ssh`/`scp`** in the loop. Even
updating the runner is self-hosted with **no ssh**: the runner supervisor is
*build-aware* (rebuilds before each launch, falling back to the existing binary
if a build fails), so a self-update is just `write_file` the changed sources →
`run_command "taskkill … arc-runner.exe"`; the supervisor recompiles and
relaunches the new binary. Verified end-to-end (pushed a source change, killed
the runner, confirmed the auto-recompile in `update.log` and the new binary
serving) — entirely through `mcpctl`.

## Run the relay

```bash
cargo run -p arc-relay              # binds 0.0.0.0:8787
ARC_RELAY_ADDR=127.0.0.1:18787 cargo run -p arc-relay
curl http://127.0.0.1:18787/healthz      # -> ok
```

## Security

End-to-end encryption means the relay sees only ciphertext. The pairing code is
**never** used as a key directly: both peers run a symmetric **SPAKE2** PAKE
(`crypto::Pake`) — one message each way — to turn the low-entropy code into a
high-entropy shared key, which then keys
`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`. An eavesdropper who records the
handshake cannot mount an offline dictionary attack (the defining PAKE
property); a wrong code yields divergent keys that the Noise handshake rejects.
Verified end-to-end (mac controller ↔ Windows runner: identical derived PSK,
`link established`).

A future hardening is a runner-side command/path allow-list (`RemoteErrorKind::Denied`).

## License

MIT OR Apache-2.0
