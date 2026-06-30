//! Wire protocol, framing and end-to-end crypto for **arc**.
//!
//! The protocol is deliberately split into two layers:
//!
//! * **L1 — relay layer** ([`relay`]): the only thing the public relay can
//!   read. It carries a [`SessionId`], the peer [`Role`] and otherwise *opaque*
//!   ciphertext. The relay forwards bytes between the two peers of a session and
//!   never sees plaintext.
//! * **L2 — end-to-end layer** ([`wire`]): [`Request`]/[`Response`]/[`Event`]
//!   messages exchanged between the controller (macOS) and the runner
//!   (Windows). They are sealed with a Noise channel ([`crypto`]) whose key is
//!   derived from the out-of-band pairing code, so the relay cannot decrypt
//!   them.
//!
//! [`Request`]: wire::Request
//! [`Response`]: wire::Response
//! [`Event`]: wire::Event

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod codec;
pub mod crypto;
pub mod error;
pub mod id;
pub mod relay;
pub mod wire;

pub use error::ProtoError;
pub use id::{ElementId, PairingCode, RequestId, Role, SessionId, WindowId};

/// Protocol version negotiated in [`relay::ClientMsg::Hello`]. Peers with a
/// mismatching major version are rejected by the relay.
pub const PROTOCOL_VERSION: u16 = 1;
