# Changelog

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
