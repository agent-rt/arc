//! Shared runner core.
//!
//! A runner is "transport (arc-net) + a [`Backend`] that maps
//! [`Command`](arc_proto::wire::Command)s to OS capabilities". This crate holds
//! the parts that don't depend on the OS: the [`Backend`] trait (with
//! "unsupported" defaults, so a backend only implements what it can), the
//! request→reply [`dispatch`] mapper, OS-agnostic file transfer ([`files`]), and
//! the error constructors. Each platform crate (Windows, Android) provides a
//! `Backend` impl and its own connection/serve loop (streaming and port
//! forwarding are serve-loop concerns and live there).

mod backend;
pub mod dispatch;
pub mod files;

pub use backend::Backend;

use arc_proto::wire::{RemoteError, RemoteErrorKind};

/// Per-command result: a success [`Reply`](arc_proto::wire::Reply) or a
/// structured [`RemoteError`] returned to the controller (link stays usable).
pub type RemoteResult<T> = Result<T, RemoteError>;

/// Builds an `Os`-category error.
pub fn os_error(message: impl Into<String>) -> RemoteError {
    RemoteError {
        kind: RemoteErrorKind::Os,
        message: message.into(),
    }
}

/// Builds a `NotFound`-category error.
pub fn not_found(message: impl Into<String>) -> RemoteError {
    RemoteError {
        kind: RemoteErrorKind::NotFound,
        message: message.into(),
    }
}

/// Builds an `Invalid`-category error.
pub fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError {
        kind: RemoteErrorKind::Invalid,
        message: message.into(),
    }
}

/// Builds a `Timeout`-category error.
pub fn timeout_error(message: impl Into<String>) -> RemoteError {
    RemoteError {
        kind: RemoteErrorKind::Timeout,
        message: message.into(),
    }
}
