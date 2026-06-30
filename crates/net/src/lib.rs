//! Shared client transport for **arc**.
//!
//! Both endpoints of a session — the controller (macOS `arc` CLI / `arc --mcp`) and
//! the runner (Windows) — speak the same wire stack: a WebSocket to the relay
//! (L1), a Noise channel keyed by the pairing code (L2 security), and CBOR
//! [`Frame`](arc_proto::wire::Frame)s chunked through it. This crate
//! captures that stack once as [`Session`], so each side only writes its own
//! usage logic (request/response correlation, command dispatch) on top.
//!
//! The two sides differ only in handshake direction — the controller is the
//! Noise initiator, the runner the responder — which [`Session::connect`]
//! selects from the [`Role`](arc_proto::id::Role).

#![forbid(unsafe_code)]

mod config;
mod controller;
mod error;
mod session;

pub use config::{SessionConfig, Transport};
pub use controller::{Controller, ControllerError};
pub use error::NetError;
pub use session::{Session, SessionReader, SessionWriter};
