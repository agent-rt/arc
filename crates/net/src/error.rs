//! Transport-level error type shared by both endpoints.

/// Errors from establishing or running a relay session. These are all
/// link-fatal: the caller should reconnect. Application-level failures (a
/// runner rejecting a command) travel as
/// [`RemoteError`](arc_proto::wire::RemoteError) inside a normal frame and
/// are *not* represented here.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// Protocol encode/decode or crypto failure.
    #[error("protocol error: {0}")]
    Proto(#[from] arc_proto::ProtoError),

    /// Underlying WebSocket transport failure (boxed: the error is large).
    #[error("websocket error: {0}")]
    Ws(Box<tokio_tungstenite::tungstenite::Error>),

    /// The relay refused or terminated the connection.
    #[error("relay error: {0}")]
    Relay(String),

    /// The relay closed the connection.
    #[error("relay closed the connection")]
    Closed,

    /// The peer left before or during the handshake.
    #[error("peer disconnected")]
    PeerGone,

    /// Missing or malformed startup configuration.
    #[error("configuration error: {0}")]
    Config(String),
}

impl From<tokio_tungstenite::tungstenite::Error> for NetError {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::Ws(Box::new(error))
    }
}
