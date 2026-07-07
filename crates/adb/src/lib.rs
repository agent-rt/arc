//! `arc-adb` — a Rust adb client for the arc Android bootstrap.
//!
//! The arc Android runner runs at **shell uid (2000)** — the only uid that owns
//! `input`/`screencap`/`uiautomator`. On a non-root device, the sole way to
//! (re)launch a shell-uid process without a PC is to speak adb to the device's
//! own `adbd` over localhost (Android 11+ *Wireless debugging*), the way Shizuku
//! does. This crate implements that adb client entirely in Rust so the eventual
//! APK is a thin Compose shell over a JNI `.so`; see `docs/ANDROID_BOOTSTRAP.md`.
//!
//! Two phases, in order of risk:
//!
//! 1. [`pairing`] — the one-time Android 11 *pairing* handshake: TLS 1.3 to
//!    `adbd`'s pairing port, a SPAKE2 exchange keyed by the 6-digit code, then an
//!    AES-GCM-protected exchange of adb public keys. SPAKE2 crypto is FFI'd to
//!    BoringSSL (guaranteed wire-compatible); the framing is ported byte-for-byte
//!    from AOSP adb + libadb-android and verified against a real `adbd`.
//! 2. `connect` (later) — the recurring `A_STLS` connect using the stored key.
//!
//! Development harness: the `arc-adb-pair` binary runs on the host and pairs with
//! a device's `adbd` over the network, logging every packet, so the protocol is
//! nailed down before any JNI/APK packaging exists.

pub mod crypto;
pub mod key;
pub mod pairing;
pub mod spake2;

/// Errors from the adb client.
#[derive(Debug, thiserror::Error)]
pub enum AdbError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls: {0}")]
    Tls(String),
    #[error("pairing rejected by adbd (wrong code, or framing mismatch)")]
    PairingRejected,
    #[error("protocol: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, AdbError>;
