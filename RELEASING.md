# Releasing

Distribution: **Homebrew** (macOS/Linux `arc`) and **winget** (Windows
`arc-runner`), both serving prebuilt binaries from GitHub Releases. No
crates.io. `arc` is both the CLI and the MCP server (`arc --mcp`). Mirrors the
`mcpctl` (cmcp) setup and reuses `agent-rt/homebrew-tap`.

## Cut a release

```bash
just release 0.1.0     # cargo-release: bump workspace version, commit, tag v0.1.0, push
```

`.github/workflows/release.yml` (on tag `v*`) then:

1. **build** (Linux + macOS arm/x64): builds `arc`, tars
   `arc-<ver>-<target>.tar.gz` (+ `.sha256`), and publishes them to **two**
   releases — the `arc` repo release `v<ver>`, and a release `arc-v<ver>` in
   `agent-rt/homebrew-tap`.
2. **windows**: builds `arc-runner` + `arc`, zips
   `arc-<ver>-x86_64-pc-windows-msvc.zip` (+ `.sha256`) to the `arc` release.
   (`arc-runner` is Windows-only — xcap/windows-rs — hence a separate job.)
3. **update-formula**: reads the macOS SHA256s and commits `Formula/arc.rb`
   (installs `arc`) to the tap `main` as `agent-rt-bot`.

### One-time setup

- Repo secret **`GH_DIST_TOKEN`**: a PAT with `contents:write` on
  `agent-rt/homebrew-tap` (same token mcpctl uses).
- `cargo install cargo-release` locally (for `just release`).

## Install (end users)

**macOS / Linux — controller:**
```bash
brew install agent-rt/tap/arc       # arc (CLI + `arc --mcp` server)
```

**Windows — runner:** the zip is a portable winget package; submit a manifest
for `agent-rt.arc-runner` to winget-pkgs (or via `winget-releaser` on release),
`InstallerType: zip`, `NestedInstallerType: portable`, alias `arc-runner`,
pointing at the `arc` release zip URL. Then:
```powershell
winget install agent-rt.arc-runner
arc-runner install --listen <tailnet-ip>:8787 --trust-tailnet --allow you@example.com
```
`arc-runner install` mints credentials, registers a logon-autostart task
(interactive session, so GUI/UIA work), starts it, and prints the controller
`[targets.win]` block for `~/.config/arc/config.toml`. `arc-runner uninstall`
removes it.

## TODO

- winget manifest + `winget-releaser` wiring (the one piece not yet automated).
- Code signing: unsigned Windows binaries trigger SmartScreen — document the
  bypass, or add Azure Trusted Signing / sigstore to the `windows` job.
