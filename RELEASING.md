# Releasing

Distribution: **Homebrew** (macOS/Linux `arc`) and **winget** (Windows
`arc-runner`), both serving prebuilt binaries from GitHub Releases. No
crates.io. `arc` is both the CLI and the MCP server (`arc --mcp`). Mirrors the
`mcpctl` (cmcp) setup and reuses `agent-rt/homebrew-tap`.

## Cut a release

Versioning is **SemVer**; pre-1.0 the *minor* covers both new features and
breaking changes (0.1.0 → 0.2.0 added the `run` / `RunScript` command), the
*patch* is backward-compatible fixes only. Bump the workspace version, commit,
tag `vX.Y.Z`, and **push the tag** — the tag is what triggers the workflow.

With [`cargo-release`](https://github.com/crate-ci/cargo-release) installed:

```bash
just release 0.2.0     # bump workspace version, commit, tag v0.2.0, push
```

Without it (the manual path — what 0.2.0 actually shipped with):

```bash
# 1. bump `version` in [workspace.package] of Cargo.toml (all crates inherit it)
# 2. sync the lockfile so CI's `--locked` build matches the new version
cargo build -p arc-cli
# 3. commit the bump on main, then tag (gpg signing off — the repo defaults to
#    annotated/signed tags) and push the tag
git commit -am "release 0.2.0"
git -c tag.gpgSign=false tag -a v0.2.0 -m "arc 0.2.0"
git push origin main v0.2.0
```

> In the jj-colocated checkout, commit/bookmark with `jj` and `jj git push
> --bookmark main`, then create and push the git tag with the two `git` commands
> above (jj doesn't manage tags).

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
- `cargo install cargo-release` locally — optional, only for `just release`;
  the manual path above needs no extra tooling.

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
- **First submission is manual.** `winget-releaser` only *updates* a package
  that already exists in winget-pkgs — so the `winget` job is `continue-on-error`
  and fails (`Package … does not exist`) until the package is bootstrapped once.
  `komac`/`wingetcreate` need a TTY (they fail in CI/sandbox), so bootstrap by
  opening the first PR directly via the GitHub API: sync the
  `agent-rt/winget-pkgs` fork, add a branch with the three manifests
  (`version` / `installer` / `locale`; `InstallerType: portable`,
  `Commands: [arc-runner]`, **uppercase** `InstallerSha256`) under
  `manifests/a/agent-rt/arc-runner/<ver>/`, then
  `gh pr create --repo microsoft/winget-pkgs --head agent-rt:<branch>`.
  New publisher + unsigned exe → manual moderation, which can take days.
  - **Status:** PR
    [microsoft/winget-pkgs#395316](https://github.com/microsoft/winget-pkgs/pull/395316)
    (bootstraps `agent-rt.arc-runner` 0.1.0) is **open, CLA cleared, awaiting a
    moderator**.
  - Once it merges the package exists, so the *current* version is published by
    re-running the latest release's failed `winget` job —
    `gh run rerun --job <id> --repo agent-rt/arc` (e.g. job `84221036236` of the
    v0.2.0 run). From then on every release's `winget` job auto-bumps and goes
    green.
- The `winget-releaser` action is pinned to `@v2` (its maintained release tag).

## TODO

- Code signing: unsigned Windows binaries trigger SmartScreen — document the
  bypass, or add Azure Trusted Signing / sigstore to the `windows` job.
