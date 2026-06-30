# Changelog

## Unreleased

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
