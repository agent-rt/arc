# Android no-PC bootstrap (self-contained)

**Goal:** the arc runner starts on the phone with **no permanent PC tether** and
**survives reboot**, using **no Shizuku and no root**.

## Why this shape is forced (measured on-device, Android 12, non-root)

The runner is useful only at the **shell uid (2000)** — that uid's group set
includes `input(1004)`, which is what makes `screencap`/`input`/`uiautomator`
work. A normal APK runs under its own app uid and can drive nothing. So the
whole problem reduces to: *how does a shell-uid process get (re)started with no
PC?*

On stock non-root Android 12 the only thing that (a) runs at boot and (b) can
spawn a **shell-uid** child is **`adbd`** — and only for an *authorized adb
connection*. Everything else follows from that single fact:

- **Reboot trigger** — `init` runs no user scripts, there is no cron; the only
  boot hook a user app gets is an APK `BOOT_COMPLETED` receiver. The APK runs at
  app uid, but its job is only to *trigger* a self-connect to `adbd`; the runner
  `adbd` then spawns runs at shell uid. **⇒ an APK is unavoidable.**
- **The connection** — reaching `adbd` over the network requires Android 11+
  **Wireless debugging**: a one-time `adb pair` (SPAKE2-over-TLS, mDNS-advertised
  on `_adb-tls-pairing._tcp`) that stores a TLS keypair, then `adb connect` to
  the `_adb-tls-connect._tcp` port. This survives reboot (the pairing/key
  persists); only the **port changes each boot**, so it must be re-discovered.

Measured baseline on this device: `adb_enabled=1`, `adb_wifi_enabled=0`, no
on-device `adb` binary, no Shizuku.

## Architecture (reuse the real adb, don't reimplement the protocol)

```
┌─────────────────────────── phone ───────────────────────────┐
│  arc-runner-app  (APK, app uid)                              │
│    • bundles: arm64 `adb` client  +  arc-runner-android bin  │
│    • first run: guide user to enable Wireless debugging,     │
│                 run `adb pair`, store adbkey in app storage  │
│    • on boot / on launch:                                    │
│        mDNS find _adb-tls-connect._tcp port on 127.0.0.1     │
│        adb connect 127.0.0.1:<port>   (uses stored key)      │
│        adb shell  <runner> --supervise 0.0.0.0:8787 CODE  ───┼──┐
│    (app uid — only *triggers*)                               │  │
└──────────────────────────────────────────────────────────────┘ │
                                                                   ▼
                              adbd spawns the runner at SHELL UID (2000)
                              → screencap/input/uiautomator work
                              → setsid+--supervise already handle
                                detach + crash-restart (shipped)
```

Key decision: **bundle the real `adb` binary and drive it** rather than
reimplement adb's SPAKE2-over-TLS pairing in Rust. It keeps A self-contained
(zero third-party runtime dep, all arc) while reusing the audited, maintained
mechanism — reimplementing a security protocol is the last resort, not the first.

## Stage-0 ground truth (measured on KC-T304 / Android 12 / non-root)

Controlled experiments, not guesses. Results:

1. **Baseline (from any WiFi host, no USB) — ✅ works.** `adb pair 10.0.0.158:<pair>
   <code>` then mDNS `_adb-tls-connect._tcp` → `adb connect 10.0.0.158:<connect>`.
   The wireless shell is **uid 2000 with the `input` group** — the design's core
   bet holds over the wireless channel.
2. **On-device self-connect — deferred to S1, but effectively settled.** The
   shell uid is decided by `adbd`, not by who connects (the Mac got uid 2000);
   an app-uid client hitting the same wireless `adbd` port gets the same shell
   service. Residual to confirm when we bundle arm64 `adb`: localhost accept +
   the app's stored key authorizing. Shizuku already proves both.
3. **Reboot persistence — ❌ auto-start impossible (as predicted).** A reboot
   resets `adb_wifi_enabled` to `0`; adbd stops listening on WiFi, the old port
   refuses, and the stored key is unusable until Wireless debugging is
   **manually re-enabled** (a UI toggle no app can flip). Matches Shizuku's own
   documented limitation. Unattended reboot survival needs **root**.
4. **Key survives the toggle — ✅.** After a reboot + re-enabling Wireless
   debugging *without re-pairing*, the stored key connects on the new port and
   still yields uid 2000. So **pairing is one-time (survives reboot)**; only the
   port must be re-discovered (mDNS) each session.

**Verdict:** the self-contained path is viable. The per-reboot cost is minimal
and matches the accepted UX — user re-enables Wireless debugging (UI), then taps
"Start" in the arc app; no re-pairing. Auto-start is out of scope (needs root).

## Status: the Rust adb engine is proven end-to-end

The hard part — speaking adb to `adbd` in pure Rust — is done and verified on a
real device (KC-T304 / Android 12), no system `adb` involved (`crates/adb`):

- **Pairing** (`pairing.rs`): TLS 1.3 + RFC5705 exporter + SPAKE2 (BoringSSL FFI
  via aws-lc-sys) + AES-128-GCM PeerInfo exchange. Verified: full handshake
  completes and the device PeerInfo decrypts.
- **Identity** (`key.rs`): RSA-2048 + ANDROID_PUBKEY encoding cross-validated
  byte-for-byte against system adb's `adbkey.pub`; self-signed X.509 cert.
- **Connect** (`connect.rs`): CNXN → STLS → mid-stream TLS upgrade → `shell:`.
  Verified: the paired key authorizes the connect (so pairing stored it usably),
  and `shell:id` runs at **uid 2000 with the input group**.

Remaining is packaging, not protocol: (a) adb **sync** (push) to transfer the
runner binary, (b) cross-compile `arc-adb` + runner via cargo-ndk to a JNI `.so`
(UniFFI/`jni`), (c) a thin Compose APK (pairing UI, "Start" button, `NsdManager`
port discovery, deep-link to the Wireless-debugging toggle).

## Stages (each independently verifiable / committable)

- **S0 — protocol ground truth** (no new code): experiments 1–4 above. Needs
  Wireless debugging enabled (a one-time UI toggle on the device).
- **S1 — adb payload:** source/build an arm64 `adb` client; decide how it ships
  (bundled asset). Prove the exact `pair`→`connect`→`shell <runner>` command
  sequence by hand from a device shell.
- **S2 — APK skeleton + first-run pairing UI:** minimal Kotlin app; bundle adb +
  runner as assets; pairing screen; persist adbkey. (New Gradle/Kotlin
  subproject — the first non-Rust module in the workspace.)
- **S3 — self-connect + spawn on launch:** app foreground → mDNS port →
  self-connect → `adb shell <runner> --supervise`. Verify runner is live.
- **S4 — BOOT_COMPLETED:** receiver repeats S3 on boot. Verify across a real
  reboot: phone boots, no PC attached, `arc -t phone shell ...` works.
- **S5 — robustness:** port re-discovery, re-pair flow when the key is revoked,
  clear status/notification. (Crash-restart is already done via `--supervise`.)

## Build-system additions this introduces

- An **arm64 `adb` binary** as a bundled asset (source + license to settle).
- An **Android APK subproject** (Gradle + Kotlin) — new toolchain in a
  Rust-only workspace; needs its own CI lane.

## Fallback

If stage-0 experiments show localhost self-connect can't reach shell uid on this
device/build, switch to **option B (Shizuku client)** — smaller, reuses a solved
bootstrap, at the cost of a one-time third-party install.
