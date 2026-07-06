# Changelog

## 0.8.0

Remote reach, hang/crash diagnostics, artifact sync, and forward-compatible
protocol negotiation. Adds `Forward` and `ProcDump` commands (plus additive,
serde-default fields), so upgrade a 0.7.x runner (`arc-runner upgrade`). From
this release on, a runner answers an unknown (newer) command with a clear
"upgrade" error instead of dropping the connection.

- **`arc forward <localport>:<remoteport>`** (or `<localport>:<host>:<remoteport>`)
  — tunnel a local TCP port to the runner over the encrypted link (adb/ssh `-L`
  style); reach a dev server, API, or debugger port bound to the box's localhost.
  Direct (Tailscale) mode recommended.
- **`arc procdump <pid>`** — write a minidump (thread stacks + modules) on the box
  and pull it back; open it locally in WinDbg/cdb with symbols. For diagnosing a
  hung process.
- **`arc whoami`** — account, integrity level, admin, and session at a glance.
- **`arc doctor`** — self-check: link, identity, session-activity tier, and a UIA
  smoke count, interpreting which capabilities work right now (UIA + per-window
  capture work disconnected; raw input + full-screen capture need an Active
  session).
- **`arc push --no-ignore` / `arc watch --no-ignore`** — include `.gitignore`'d
  files and build dirs to ship build artifacts (`.git` is always excluded).
- **`arc pull --watch [--interval N]`** — poll the runner and pull changes as they
  appear (build on the box, artifacts flow back). **`arc pull --no-ignore`** fetches
  from build dirs like `target/`.
- **Capability negotiation** — an unknown command from a newer controller now
  returns a clear "unsupported — upgrade the runner" error with the link intact,
  instead of a mysterious connection reset.
- **Code signing prep** — added `LICENSE-MIT` / `LICENSE-APACHE`, a code signing
  policy (`docs/CODE_SIGNING.md`), and a gated SignPath signing step in the
  release workflow (inert until configured).

## 0.7.0

Agent dev-loop improvements from real Windows usage (driving a WinUI 3 build
end-to-end from macOS). The protocol gained additive, serde-default fields plus
a new `RunDetached` command, so a 0.6.x runner should be upgraded
(`arc-runner upgrade`).

- **UTF-8 by default** — remote command output and `arc run` scripts now handle
  non-ASCII (中文 / 日本語) correctly: `.ps1` scripts are written with a BOM (so
  Windows PowerShell 5.1 stops decoding them as the ANSI code page → mojibake and
  parse errors) and inline commands set the console to UTF-8 (`chcp 65001` +
  `[Console]::OutputEncoding`).
- **`arc shell --env KEY=VAL` / `--env-file <path>`** (also on `arc run`) — inject
  environment variables into the remote process instead of the command line; the
  safe way to pass a secret or token.
- **`arc run -`** — read a script from stdin with `--lang ps1|bat|cmd`; pipe a
  multi-line here-doc and it runs with no shell quoting to escape.
- **`arc run --detach`** — launch a long task (installer, build, package restore)
  with output redirected to a log file on the box; returns a pid + log path
  immediately instead of blocking. Follow with `arc tail -f <log>`, manage with
  `arc ps` / `arc kill <pid>`.
- **`arc open` crash diagnostics** — watches the launched process briefly and, on
  a startup crash, reports the exit code + the most recent matching Application
  error event (faulting module + exception code) and exits non-zero. `--watch
  <ms>` tunes the window (`0` = don't wait).
- **`arc shot --launch`** now fails fast when the process exits before a window
  appears, instead of waiting out `--wait`.
- **`arc ps --cpu`** — adds an instantaneous CPU% column (sampled over ~500ms) to
  tell a busy (spinning) process from a blocked one — for diagnosing hangs.
- **`arc agents-md`** gained guidance on shell semantics (write PowerShell
  directly — don't nest `powershell -Command "..."`, which re-expands `$vars`),
  `--cmd` vs `arc run` for quote-heavy commands, launching GUI apps, log-file
  debugging, and the admin-session caveat.
- **MCP:** `list_processes` gains `cpu`, `open_app` gains `watch_ms`, and a new
  `run_detached` tool.

## 0.6.1

CLI-only release (no runner/protocol changes — a 0.6.0 runner works unchanged).

- **Internals:** the CLI crate's monolithic `main.rs` (~2150 lines) was split
  into domain modules (`config`, `exec`, `files`, `capture`, `ui`, `agents_md`);
  `main.rs` now holds just the command surface and dispatch. No behavior change.
- **`arc agents-md`** — print a complete Markdown reference of every command and
  flag (generated from the CLI itself, so it never drifts) preceded by
  agent-oriented guidance: connecting, core workflows, and the
  session-capability gotchas. Made for handing an AI agent the whole tool
  surface in one read instead of drilling into 24 `--help` screens. Runs locally
  — no runner connection. (`arc agents-md > AGENTS.md`.)
- **Help polish** — `arc --help` now shows a tight one-line summary per command
  with the key flags surfaced (e.g. `watch --on-change`, `screencap
  --baseline/--element`, `type --into/--paste`, `windows --filter`, `kill
  --dry-run`); full detail still lives in `arc <cmd> --help`. Documented
  previously-bare positional arguments so both `--help` and `agents-md` describe
  them, and removed an internal note that had leaked into `activate`'s help.

## 0.6.0

**Highlights: a much larger CLI/MCP surface driven by real agent usage —
structured `--json` output, reliable text entry, clipboard, log follow, a
screenshot regression diff, process control, and window activation. Fresh
windows also screenshot reliably (DWM is woken before capture).**

- **`arc key … --into <element-id>`** — focus an element before sending the key
  chords (symmetric with `type --into`); MCP `press_key` gains `into`. Backed by
  a new `FocusElement` command.
- **`arc kill --dry-run`** — list the processes a kill *would* hit (by PID or
  name) without killing them. MCP `kill_process` gains `dry_run`.
- **MCP `list_processes` / `kill_process`** — first-class process tools mirroring
  `arc ps` / `arc kill`, so the Agent doesn't have to hand-roll PowerShell.
- **`arc activate <hwnd>`** — restore (if minimized) and foreground a window, so
  a capture or input lands on a real, visible window instead of a title-bar
  sliver. `arc shot` now does this automatically before capturing. MCP:
  `activate_window`.
- **`arc type --paste`** — paste text via the clipboard (Ctrl+V) instead of
  per-key injection: one round-trip for a whole paragraph instead of 16 ms per
  character, and more robust for long text. Combine with `--into` to target a
  control. Clobbers the clipboard. MCP: `type_text` gains `paste`.
- **`arc read <element-id>`** — read one control's text (its Value-pattern value,
  else accessible name) without dumping the whole element tree — a token-cheap
  way to verify "did my input land / has it loaded" without a screenshot. MCP:
  `read_element`.
- **`arc ps [pattern]` / `arc kill <pid|name>`** — list remote processes (Id,
  name, working-set MB, heaviest first; optional name-substring filter) and kill
  one by PID or by name (`-Force`; a name kills every match, reporting each).
- **`arc screencap --baseline <img>`** — compare the capture against a baseline
  and print a verdict (`MATCH` / `DIFFERS: N% of pixels changed`), exiting
  non-zero past `--threshold` (default 0.1%) so it drops into a regression gate.
  `--diff <img>` writes an overlay with changed pixels painted magenta.
  Dimension mismatches count as a full change.
- **`arc watch … --on-change '<cmd>'`** — after each auto-sync (and once at
  startup), run a PowerShell command on the runner with live output, e.g.
  `arc watch ./src C:/work/src --on-change 'cargo build'`. Closes the
  edit → push → build inner loop in one command. A failing hook is reported but
  never stops the watch.
- **`arc tail <remote>`** — print a remote file's last lines (`-n N`); `-f`
  follows it, streaming appended lines until interrupted, for watching build/app
  logs without a shell incantation.
- **`arc cat <remote>`** — print a remote file to stdout (UTF-8, lossy), without
  saving a local copy. The quick read companion to `pull`.
- **`arc windows --filter <substr>`** — show only windows whose title or process
  matches (case-insensitive), instead of grepping the full list. MCP
  `list_windows` gains the same `filter` argument.
- **`arc clip get` / `arc clip set`** — read or write the remote machine's
  clipboard. `clip set -` reads the text from stdin. Useful for moving text both
  ways without typing it character-by-character, and for reading what an app
  copied. MCP gains `clipboard_get` / `clipboard_set`. (Verified round-trip incl.
  CJK, and reading text another app placed on the clipboard.)
- **`arc type --into <element-id>`** — focus a specific control (UIA `SetFocus`,
  id from `elements`/`find`) before typing, then send real keystrokes. More
  reliable than typing into whatever happens to have focus, and (unlike
  `set`/SetValue) it drives the app's real input handling. MCP `type_text` gains
  an `into` argument. (Verified into Win11 Notepad: focus lands in the right
  element, ASCII + CJK type cleanly.)
- **`screencap`/`shot` wake DWM before capturing** — on an idle session DWM
  throttles compositing, so a just-launched window's first frame can come back
  black. The runner now nudges the cursor (net-zero jiggle) before a capture, so
  fresh windows render without the Agent having to move the mouse first.
  (Verified: a backdrop-only Paint capture became the full UI, 918 B → 209 KB.)
- **`arc shot`** — one-shot "verify the UI": optionally `--launch` an app (or
  find it by `--app <substr>` / `--window`), wait for it to render (the runner
  re-captures until two frames are stable, not a blind sleep — and waits for the
  initial backdrop to actually change after a launch), then screenshot. Replaces
  the open → sleep → windows → grep → screencap → convert dance. Capture also
  gained a `settle_ms` option for this. (Composition still requires the session
  to have a display — see keep-display / a virtual display for headless boxes.)
- **`screencap` encodes by file extension** — `shot.png` → PNG, `shot.webp` →
  WebP. No more client-side conversion just to view a capture.
- **`screencap --element <id>`** — capture a single control's bounding box (id
  from `elements`/`find`). MCP `screenshot` gains an `element` argument too.
- **Runner is now per-monitor DPI-aware** — window/element rects and capture are
  all in physical pixels, so element crops, region captures and rect-based input
  line up on scaled (high-DPI) displays. (`windows --json` rects now match the
  captured image size.)
- **`--json` on `windows` / `elements` / `find` / `wait`** — structured output
  instead of pipe-delimited text, so agents stop scraping with `cut`/`grep`.
  Window records carry `id, title, process, focused, rect`; element records carry
  `id, control_type, name, automation_id, value, rect, actionable`. Elements now
  also include their **bounding `rect`** and current **`value`** (Value-pattern
  controls) in the text output too.

## 0.5.0

- **`arc-runner keep-display`** — keeps a remote machine composing across RDP
  disconnects so freshly-launched DirectComposition apps (WinUI 3, Chromium)
  still render and screenshot. Registers a SYSTEM task that, on each RDP
  disconnect, moves the session to the console display (`tscon … /dest:console`).
  Needs Administrator and a monitor connected to the machine (may be powered
  off); for a truly headless box use a virtual display driver instead.
  `--uninstall` removes it.

## 0.4.0

**Highlights: WinUI 3 / Chromium windows screenshot correctly (and work
disconnected), the runner can't be bricked by a stuck command, and richer UI
automation.**

- **Window capture rewritten on Windows.Graphics.Capture (WGC).** Single-window
  `screencap` now captures DirectComposition apps — **WinUI 3**, Chromium,
  Electron — as real pixels instead of black, and it works even when **RDP is
  disconnected** (per-window WGC, with a monitor-crop fallback for static
  windows, GDI last). Full-screen `screencap` still needs an active session.
- **A stuck command no longer bricks the runner.** Connections are served
  concurrently, so one hung/long command never blocks other operations. Detached
  GUI launches: prefer `arc open <exe> [-- args]` (returns immediately) over
  `arc shell 'start …'`.
- **`arc find` / `arc wait`** — locate UI elements by attribute without dumping
  the whole tree: `arc find <hwnd> --name Save --type Button`; `arc wait <hwnd>
  --name Done --timeout 30` polls until it appears. (MCP: `find_elements`.)
- **`arc key` accepts a sequence** — `arc key ctrl+a delete enter` runs chords in
  order on one connection. (MCP: `press_key` takes a `keys` array.)
- **`arc open <exe> -- <args>`** now passes flags through to the app (was broken).
- **`arc-runner install --tailscale`** — one flag: auto-detects the tailnet IP +
  owner, enables trusted-identity auth, restricts to the node owner.
- **`arc-runner upgrade`** — self-updates the runner to the latest release
  (download → validate → swap → restart); `--dry-run` to preview.
- **`arc run <script>`** — ship & run a local `.ps1`/`.bat` (no pre-`push`, no
  shell-quoting). (MCP: `run_script`.)
- **Reliable typing into WinUI apps** — keystrokes are paced so they aren't
  dropped.
- **Internals:** one `windows-rs` version across the build (dropped `xcap` and
  `enigo`); capture and input are now self-maintained crates.

Updating a runner: `arc-runner upgrade` (or `winget upgrade agent-rt.arc-runner`).

## 0.1.0 – 0.3.0

Initial releases: encrypted relay + Noise channel, Tailscale-direct mode with
trusted-identity auto-pairing, `shell` (live streaming) / `push` / `pull` /
`watch` (incremental, `.gitignore`-aware) file sync, screenshots, UI Automation
(`windows` / `elements` / `click` / `set` / `type` / `key` / `mouse`), the
`arc --mcp` MCP server, and `arc-runner install` / `uninstall`. Homebrew (`arc`)
and winget (`arc-runner`) packaging.
