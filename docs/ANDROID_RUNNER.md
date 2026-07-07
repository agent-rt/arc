# Android runner — design

Status: **proposal / not built.** Extends arc from "remote Windows machine" to
"remote Android device" using the *same* encrypted arc protocol — not by wrapping
`adb` on the host (that's just local adb; no value), but by running an arc runner
**on the device**, reachable over the relay / Tailscale from anywhere, no USB.

## Principle

Mirror the Windows runner: a native binary that speaks arc's wire protocol and
maps arc's verbs to OS capabilities. Reuse everything above the OS boundary,
implement only the device-specific capability layer.

- **Reused as-is:** `arc-proto` (wire, Noise, CBOR) and `arc-net` (relay/direct
  `Session`) — pure Rust, cross-compile to `aarch64-linux-android` with
  `cargo-ndk`. The transport, encryption, pairing, and relay reach are unchanged.
- **New:** an Android capability backend + a small binary crate for it.

## The one hard constraint (Android sandbox)

An app cannot self-elevate. Cross-app screenshot / input injection / arbitrary
shell / other apps' files require the **`shell` (adb) user or root** — a process
only has those if it was *launched by* adb / root. This is a kernel + SELinux
boundary; no tool (arc or Shizuku) bypasses it. So the runner must be **started
with shell privilege**. Options, in order of preference for a dev tool:

1. **`adb shell` launch (MVP).** `adb push arc-runner-android /data/local/tmp/ &&
   adb shell /data/local/tmp/arc-runner-android …`. The process runs as `shell`
   and can do everything adb can. Developers already have adb; simplest, no
   third-party dep. Cost: must re-launch after reboot (non-root).
2. **Wireless-debugging self-start (no PC).** On Android 11+, the runner (or a
   tiny helper) pairs with the *local* ADB over wireless debugging and starts
   itself — no PC needed. This is the ADB TLS pairing dance that **Shizuku already
   implements**; we either reimplement it or lean on Shizuku here.
3. **Root.** One step, fully persistent (autostart), full power — for rooted
   devices / emulators.
4. **Shizuku (optional convenience).** Instead of #2 we can be a Shizuku client
   and borrow its shell service. Not required; a dependency + binder integration.

Without any of the above, only an **app + Accessibility + MediaProjection** path
exists (user-granted): UI automation + screen capture + own files only, **no
shell**. That's a separate, more limited fallback for locked-down retail devices
— consider later, not for the MVP.

## Capability mapping (shell-privileged runner)

| arc verb | Android (shell-out) |
|---|---|
| `shell` / `run` | exec directly (already `shell`/root) |
| `shot` / `screencap` | `screencap -p` |
| `windows` / `elements` / `find` | `uiautomator dump` → parse XML tree |
| `click` / `type` / `key` / `mouse` | `input tap`/`text`/`keyevent`/`swipe` (bounds from uiautomator) |
| `open` / `ps` / `kill` / `install` | `am start` / `ps` / `am force-stop` / `pm install` |
| `push` / `pull` | filesystem (shell can read/write broadly) |
| `tail` | `logcat`, or `tail`/read a file |
| `forward` | already generic in arc-net; the runner dials `127.0.0.1:port` on-device |
| `procdump` | N/A (Windows-only); Android analogue is a bug report / tombstone — later |

## Suggested refactor: abstract the capability layer

Today `arc-runner`'s `dispatch.rs` calls `exec`/`capture`/`uia`/`input`/`files`/
`apps` directly (all `windows-rs`). To add Android as a *parallel* backend rather
than a fork, pull the capability surface behind a trait (or a few traits):

```
trait Backend {
    async fn run_command(...) -> ...;
    fn screenshot(...) -> ...;
    fn list_elements(...) / find / click / type / key / set / read ...;
    fn open_app(...); fn list_processes(...); fn kill(...);
    // files (read/write/hash/tree/delete) can stay shared where OS-agnostic.
}
```

`dispatch.rs` dispatches to the active `Backend`. Windows impl = today's code
moved behind the trait; Android impl = shell-out. This refactor is useful on its
own (clean seam) and is the natural first step. Some verbs are semantic (list UI
elements, click element) and map per-platform; a few are platform-specific
(`procdump` Windows-only) and return `Unsupported` elsewhere — the capability
negotiation added in 0.8.0 already handles "this runner doesn't do that."

## Transport & reach

`arc-net` works unchanged. The device connects out to the relay, or (on Tailscale,
via the Android Tailscale app) is reachable by its tailnet IP in direct mode. So
the full "drive a phone anywhere, no USB" story is just: runner on device →
relay/tailnet → `arc -t phone …`.

## Packaging & persistence

- Build: `cargo-ndk` → `aarch64-linux-android` (and `x86_64` for emulators).
- Distribute: a bare binary pushed to `/data/local/tmp` (MVP).
- Config/pairing: same `runner.toml`-style creds; `arc-runner-android pair`.

### The privilege constraint (measured, not assumed)

The runner works only because `adb shell` launches it under the **shell uid
(2000)**, whose group set includes `input (1004)` — that is what makes
`screencap` / `input` / `uiautomator` succeed. This rules out the obvious
"foreground service APK" persistence story: an APK service runs under its *own*
app uid and loses those privileges, so it can drive nothing. That is precisely
the gap Shizuku fills (it keeps a shell-uid server alive and brokers calls).

So persistence splits into two independently-achievable tiers:

1. **Detach + self-respawn (no APK, no root) — implemented.** `setsid` detaches
   the process from the launching `adb shell` (survives disconnect) and
   `--supervise` re-execs the server as a child, restarting on crash with capped
   backoff:

   ```sh
   adb shell "setsid /data/local/tmp/arc-runner-android --supervise 0.0.0.0:8787 CODE \
     </dev/null >/data/local/tmp/arc-runner.log 2>&1 &"
   ```

   Covers shell-disconnect and crashes. Does **not** survive reboot — nothing
   re-launches a shell-uid process after boot without external help.

2. **Reboot auto-start (needs Shizuku or root).** Only a shell-uid daemon that
   itself survives boot can relaunch the runner; on a non-root device that is
   Shizuku's auto-start. This is coupled to the no-PC bootstrap milestone.

## Milestones

1. **Backend trait refactor** (Windows-only, no Android yet) — extract the seam.
   Ships value immediately (cleaner runner) and de-risks Android.
2. **Android MVP** — cross-compile; on an emulator or an `adb shell`-launched
   binary, prove the loop over the relay: `shell` + `shot` (screencap) +
   `click`/`type` (input) + `elements` (uiautomator). Reuse proto/net verbatim.
3. **Reach & persistence** — Tailscale direct mode; `setsid --supervise` for
   detach + crash-restart (done); wireless-debug self-start or Shizuku for no-PC
   bootstrap + reboot re-launch (a shell-uid daemon, *not* an app service).
4. **Retail-device fallback (optional)** — APK + Accessibility + MediaProjection
   for UI-only automation where shell privilege isn't available.

## Open questions

- Wireless-debug self-start: reimplement the ADB pairing/TLS, or depend on
  Shizuku? (Trade self-containment vs. effort.)
- One binary crate `arc-runner-android`, or a shared `arc-runner-core` + per-OS
  binaries? (Depends on how much the trait refactor shares.)
- UI-tree fidelity: `uiautomator dump` (shell) vs. AccessibilityNodeInfo (app) —
  pick based on the privilege path.
