use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use arc_net::{SessionConfig, Transport};
use arc_proto::id::{PairingCode, SessionId};

use crate::Cli;

/// A `[targets.<name>]` block in the config file.
#[derive(Debug, Default, serde::Deserialize)]
struct TargetCfg {
    relay: Option<String>,
    direct: Option<String>,
    session: Option<String>,
    pairing: Option<String>,
    /// Direct + trusted tailnet: authenticate by Tailscale identity, so no
    /// pairing code is needed (uses the well-known [`PairingCode::tailnet_auto`]).
    trust_tailnet: Option<bool>,
}

/// The config file: a `default` target name plus named `targets`.
#[derive(Debug, Default, serde::Deserialize)]
struct ConfigFile {
    default: Option<String>,
    #[serde(default)]
    targets: HashMap<String, TargetCfg>,
}

pub(crate) fn resolve_config(cli: &Cli) -> Result<SessionConfig> {
    let file = load_config_file()?;
    let target = select_target(cli, &file)?;

    // Per field: explicit flag > selected config target > environment variable.
    let pick3 = |flag: &Option<String>, from_target: Option<String>, env: &str| -> Option<String> {
        flag.clone()
            .or(from_target)
            .or_else(|| std::env::var(env).ok())
            .filter(|s| !s.is_empty())
    };

    let direct = pick3(
        &cli.direct,
        target.and_then(|t| t.direct.clone()),
        "ARC_DIRECT",
    );
    let is_direct = direct.is_some();
    let transport = match direct {
        Some(addr) => Transport::Direct { addr },
        None => Transport::Relay {
            url: pick3(
                &cli.relay,
                target.and_then(|t| t.relay.clone()),
                "ARC_RELAY_URL",
            )
            .context(
                "endpoint: set --direct/--relay, a config target, or \
                 ARC_DIRECT/ARC_RELAY_URL",
            )?,
        },
    };

    // Relay mode routes by session id (required); direct mode does not.
    let session_raw = match pick3(
        &cli.session,
        target.and_then(|t| t.session.clone()),
        "ARC_SESSION",
    ) {
        Some(s) => s,
        None if is_direct => "0".repeat(32),
        None => bail!("session: set --session, a config target, or ARC_SESSION (relay mode)"),
    };
    let session = session_raw
        .parse::<SessionId>()
        .map_err(|_| anyhow!("session must be 32 hex chars"))?;

    // A trusted-tailnet direct target needs no pairing code (identity is the
    // gate); fall back to the well-known constant when none is supplied.
    let trust_tailnet = target.and_then(|t| t.trust_tailnet) == Some(true);
    let pairing = match pick3(
        &cli.pairing,
        target.and_then(|t| t.pairing.clone()),
        "ARC_PAIRING",
    ) {
        Some(raw) => PairingCode::parse(&raw).map_err(|_| anyhow!("pairing must be XXXX-XXXX"))?,
        None if trust_tailnet && is_direct => PairingCode::tailnet_auto(),
        None => bail!("pairing: set --pairing, a config target, or ARC_PAIRING"),
    };

    Ok(SessionConfig {
        transport,
        session,
        pairing,
    })
}

/// Loads the config file from `$ARC_CONFIG` or
/// `~/.config/arc/config.toml`; an absent file is `Ok(None)`.
fn load_config_file() -> Result<Option<ConfigFile>> {
    let path = match std::env::var("ARC_CONFIG") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".config/arc/config.toml"),
            None => return Ok(None),
        },
    };
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(cfg))
}

/// Picks the active target: `-t <name>`, else the file's `default`, else none.
fn select_target<'a>(cli: &Cli, file: &'a Option<ConfigFile>) -> Result<Option<&'a TargetCfg>> {
    let Some(file) = file else {
        if let Some(name) = &cli.target {
            bail!("--target '{name}' given but no config file found");
        }
        return Ok(None);
    };
    match cli.target.clone().or_else(|| file.default.clone()) {
        Some(n) => file
            .targets
            .get(&n)
            .map(Some)
            .ok_or_else(|| anyhow!("no target '{n}' in config file")),
        None => Ok(None),
    }
}
