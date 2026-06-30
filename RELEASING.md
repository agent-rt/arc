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
The formula prints a caveat reminding you to set up the Windows runner
(`winget install agent-rt.arc-runner` → `arc-runner install …`) and add the
`[targets.win]` block it emits to `~/.config/arc/config.toml`.

**Windows — runner:** the `windows` job uploads a bare `arc-runner.exe`, and the
`winget` job (`vedantmgoyal9/winget-releaser`) submits/updates the
`agent-rt.arc-runner` portable manifest to winget-pkgs on each release. Then:
```powershell
winget install agent-rt.arc-runner
arc-runner install --listen <tailnet-ip>:8787 --trust-tailnet --allow you@example.com
```
`arc-runner install` mints credentials, registers a logon-autostart task
(interactive session, so GUI/UIA work), starts it, and prints the controller
`[targets.win]` block for `~/.config/arc/config.toml`. `arc-runner uninstall`
removes it.

### Extra one-time setup

- **Fork `microsoft/winget-pkgs` into the `agent-rt` org** (→ `agent-rt/winget-pkgs`).
  The `winget` job sets `fork-user: agent-rt`, so it pushes the manifest branch
  there and opens the upstream PR — reusing the existing **`GH_DIST_TOKEN`** org
  secret (no separate token needed).
  - Caveat: this only works if `GH_DIST_TOKEN` can push to `agent-rt/winget-pkgs`
    and open a PR to the public `microsoft/winget-pkgs` — i.e. a **classic PAT
    with `public_repo`** (the owner being an `agent-rt` member). If it's a
    fine-grained PAT scoped only to `homebrew-tap`, either add
    `agent-rt/winget-pkgs` (Contents + Pull requests: write) to it, or use a
    dedicated `WINGET_TOKEN`.
- First `agent-rt.*` submission to winget-pkgs is reviewed/merged by Microsoft;
  later releases just bump the version.
- Verify/pin the `winget-releaser` action ref (`@main` here) to a release tag.

## TODO

- Code signing: unsigned Windows binaries trigger SmartScreen — document the
  bypass, or add Azure Trusted Signing / sigstore to the `windows` job.
