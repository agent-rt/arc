//! Session configuration shared by controller and runner.

use arc_proto::id::{PairingCode, SessionId};

use crate::error::NetError;

/// How the controller reaches the runner.
#[derive(Debug, Clone)]
pub enum Transport {
    /// Dial a relay (public or colocated) that pairs the two peers by session
    /// id and forwards opaque payloads between them.
    Relay {
        /// Relay WebSocket URL, e.g. `wss://relay.example/v1/relay`.
        url: String,
    },
    /// Dial the runner directly at `host:port` (e.g. its Tailscale IP) — no
    /// relay and no matchmaking. The runner must be in listen mode. Lower
    /// latency and zero extra infrastructure when both peers share a network
    /// (Tailscale / LAN); the Noise + pairing layer still authenticates.
    Direct {
        /// `host:port` of the runner's listener.
        addr: String,
    },
}

/// Where to reach the peer, which session to join, and the pairing secret used
/// to derive the Noise key. Fields are public so callers can also build one
/// directly.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// How to reach the runner.
    pub transport: Transport,
    /// Session shared by the two peers (routing key in relay mode; identity
    /// only in direct mode).
    pub session: SessionId,
    /// Pairing code shared out-of-band.
    pub pairing: PairingCode,
}

impl SessionConfig {
    /// Reads configuration from positional arguments, falling back to
    /// environment variables — so the runner can take a command line while the
    /// MCP server (spawned with no args) reads its `env` block.
    ///
    /// ```text
    /// <prog> <relay-url> <session-hex> <pairing-code>
    /// # or
    /// ARC_RELAY_URL | ARC_DIRECT / ARC_SESSION / ARC_PAIRING
    /// ```
    ///
    /// # Errors
    /// Returns [`NetError::Config`] if any value is missing or malformed.
    pub fn from_args_and_env() -> Result<Self, NetError> {
        let mut args = std::env::args().skip(1);
        let endpoint = args.next();
        let session_raw = arg_or_env(args.next(), "ARC_SESSION", "session id")?;
        let pairing_raw = arg_or_env(args.next(), "ARC_PAIRING", "pairing code")?;
        Self::build(resolve_transport(endpoint)?, session_raw, pairing_raw)
    }

    /// Reads configuration from the environment only (no positional args) —
    /// suitable for clients that parse their own command line.
    ///
    /// # Errors
    /// Returns [`NetError::Config`] if any value is missing or malformed.
    pub fn from_env() -> Result<Self, NetError> {
        let session_raw = arg_or_env(None, "ARC_SESSION", "session id")?;
        let pairing_raw = arg_or_env(None, "ARC_PAIRING", "pairing code")?;
        Self::build(resolve_transport(None)?, session_raw, pairing_raw)
    }

    fn build(
        transport: Transport,
        session_raw: String,
        pairing_raw: String,
    ) -> Result<Self, NetError> {
        let session = session_raw
            .parse::<SessionId>()
            .map_err(|_| NetError::Config("session id must be 32 hex chars".into()))?;
        let pairing = PairingCode::parse(&pairing_raw)
            .map_err(|_| NetError::Config("pairing code must be XXXX-XXXX".into()))?;
        Ok(Self {
            transport,
            session,
            pairing,
        })
    }
}

/// Resolves the transport: an explicit positional relay URL wins, else
/// `ARC_DIRECT` (direct mode), else `ARC_RELAY_URL` (relay mode).
fn resolve_transport(positional_url: Option<String>) -> Result<Transport, NetError> {
    if let Some(url) = positional_url.filter(|s| !s.is_empty()) {
        return Ok(Transport::Relay { url });
    }
    if let Some(addr) = std::env::var("ARC_DIRECT").ok().filter(|s| !s.is_empty()) {
        return Ok(Transport::Direct { addr });
    }
    let url = std::env::var("ARC_RELAY_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| NetError::Config("missing endpoint (ARC_RELAY_URL or ARC_DIRECT)".into()))?;
    Ok(Transport::Relay { url })
}

fn arg_or_env(arg: Option<String>, env_key: &str, what: &str) -> Result<String, NetError> {
    arg.or_else(|| std::env::var(env_key).ok())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| NetError::Config(format!("missing {what} (arg or {env_key})")))
}
