# Code signing policy

`arc`'s Windows binaries are code-signed through [SignPath.io](https://signpath.io),
using a free code-signing certificate granted to open-source projects by the
[SignPath Foundation](https://signpath.org).

## Signed artifacts

- **`arc-runner.exe`** — the Windows runner, distributed via GitHub Releases and
  winget (`agent-rt.arc-runner`).
- `arc.exe` — the controller CLI. It normally runs on macOS/Linux; the Windows
  build shipped in the release archive is signed by the same process.

The macOS/Linux binaries are not covered by this certificate (it is a Windows
Authenticode certificate).

## How signing works

- Binaries are built **from source in GitHub Actions** (`.github/workflows/release.yml`)
  from the tagged release commit — a build a third party can reproduce from the
  public repository. See [RELEASING.md](../RELEASING.md).
- The unsigned artifact is submitted to SignPath.io, which signs it with a
  certificate whose private key is held in a hardware security module (HSM); the
  key never reaches the build machine.
- **Every release is reviewed and manually approved by an Approver** (see roles)
  before it is signed. Only artifacts built from this repository's source are
  signed.

## Team & roles

The team responsible for code signing is the same team that develops and
maintains `arc` and owns this repository.

| Role | Responsibility | Members |
|------|----------------|---------|
| **Authors**   | Commit and modify the source. | `@linguofeng` |
| **Reviewers** | Review changes from non-committers before merge. | `@linguofeng` |
| **Approvers** | Authorize (approve) each signing request in SignPath.io. | `@linguofeng` |

> Update this table as maintainers are added. Contributions from non-committers
> are reviewed by a Reviewer before they can appear in a signed release.

## Privacy policy

`arc` collects and transmits **no telemetry and no personal data**. The runner
executes only the commands sent by an authenticated controller over an
end-to-end-encrypted (Noise) link. Connections are either peer-to-peer (direct,
e.g. over Tailscale) or via a relay operated by the user; pairing credentials
stay on the machines involved. `arc` does not modify system configuration without
the operator's explicit command, and provides `arc-runner uninstall` to remove
the installed service. See the [README](../README.md) for the full architecture
and security model.

## Attribution

Free code signing is provided by [SignPath.io](https://signpath.io), with a
certificate issued by the [SignPath Foundation](https://signpath.org).
