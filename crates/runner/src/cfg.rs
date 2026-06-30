//! Persisted runner credentials/endpoint (`runner.toml`), written by the `pair`
//! subcommand and read at startup as a fallback when env/args don't supply
//! them — so a paired runner starts with no configuration.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The on-disk runner configuration. Every field is optional so a partially
/// configured file (e.g. credentials but no endpoint) still parses.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RunnerConfig {
    /// Session id (hex). Relay mode only; direct mode ignores it.
    pub session: Option<String>,
    /// Pairing code (`XXXX-XXXX`).
    pub pairing: Option<String>,
    /// Direct-mode listen address, e.g. `100.x.y.z:8787`.
    pub listen: Option<String>,
    /// Relay WebSocket URL.
    pub relay: Option<String>,
    /// Direct mode: authenticate callers by verified Tailscale identity
    /// (LocalAPI WhoIs) instead of a pairing code. No code is exchanged.
    pub trust_tailnet: Option<bool>,
    /// Optional allowlist of Tailscale login names (e.g. `user@example.com`)
    /// permitted under `trust_tailnet`. Empty/absent means any tailnet peer.
    pub allow_logins: Option<Vec<String>>,
}

impl RunnerConfig {
    /// Loads the config from [`path`]; any read/parse error yields `None`.
    pub fn load() -> Option<Self> {
        let text = std::fs::read_to_string(path()?).ok()?;
        toml::from_str(&text).ok()
    }

    /// Writes the config to [`path`], creating parent directories; returns where.
    ///
    /// # Errors
    /// Returns an error if no config directory can be determined or the write
    /// fails.
    pub fn save(&self) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let path = path().ok_or("no config dir (set ARC_RUNNER_CONFIG, APPDATA, or HOME)")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(path)
    }
}

/// The config path: `$ARC_RUNNER_CONFIG`, else `%APPDATA%\arc\
/// runner.toml` (Windows), else `~/.config/arc/runner.toml`.
pub fn path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("ARC_RUNNER_CONFIG") {
        return Some(PathBuf::from(explicit));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(appdata).join("arc").join("runner.toml"));
    }
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("arc")
            .join("runner.toml")
    })
}
