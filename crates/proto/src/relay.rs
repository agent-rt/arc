//! **L1 — relay layer.** The only messages the public relay can read.
//!
//! A peer opens a WebSocket to the relay and sends [`ClientMsg::Hello`] to join
//! a [`SessionId`] in a given [`Role`]. Thereafter every [`ClientMsg::Relay`]
//! payload is forwarded verbatim to the other peer as [`ServerMsg::Relay`]. The
//! `data` field is *opaque ciphertext* produced by the L2 [`crypto`] channel —
//! the relay neither inspects nor needs a key for it.
//!
//! [`crypto`]: crate::crypto

use serde::{Deserialize, Serialize};

use crate::id::{Role, SessionId};

/// A message sent from a peer **to** the relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Join (creating it if absent) a session in the given role.
    Hello {
        /// Target session.
        session: SessionId,
        /// Role this peer is claiming.
        role: Role,
        /// L1/L2 protocol version; see [`PROTOCOL_VERSION`].
        ///
        /// [`PROTOCOL_VERSION`]: crate::PROTOCOL_VERSION
        protocol_version: u16,
    },
    /// An opaque, end-to-end-encrypted payload to forward to the peer.
    Relay {
        /// One Noise transport record (see [`crypto`](crate::crypto)).
        data: Vec<u8>,
    },
    /// Application-level keepalive, distinct from WebSocket ping frames.
    Ping,
}

/// A message sent from the relay **to** a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// The [`ClientMsg::Hello`] was accepted.
    Welcome {
        /// Whether the opposite role is already connected. If `false`, the peer
        /// should wait for [`ServerMsg::PeerJoined`] before starting the
        /// handshake.
        peer_present: bool,
    },
    /// An opaque payload forwarded from the peer.
    Relay {
        /// One Noise transport record originated by the peer.
        data: Vec<u8>,
    },
    /// The opposite role just connected.
    PeerJoined,
    /// The opposite role disconnected; any in-flight handshake is now invalid.
    PeerLeft,
    /// Reply to [`ClientMsg::Ping`].
    Pong,
    /// The relay is refusing or terminating the connection.
    Error {
        /// Machine-readable cause.
        kind: RelayErrorKind,
        /// Human-readable detail for logs.
        message: String,
    },
}

/// Machine-readable reasons the relay may reject a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RelayErrorKind {
    /// Both roles for the session are already occupied.
    RoleTaken,
    /// This connection was superseded by a newer one joining the same role;
    /// the relay evicts the stale connection so a reconnecting peer is never
    /// locked out by its own half-open session.
    Replaced,
    /// The peer's `protocol_version` is incompatible.
    VersionMismatch,
    /// A frame violated relay limits (size, rate, or ordering).
    ProtocolViolation,
    /// The relay hit an internal fault; the peer may retry later.
    Internal,
}
