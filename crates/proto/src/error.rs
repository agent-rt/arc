//! Structured error type shared across the protocol crate.

/// Errors produced while encoding, decoding, sealing or parsing protocol data.
///
/// All fallible operations in this crate return [`ProtoError`] rather than
/// panicking, so callers can decide whether a malformed frame is fatal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProtoError {
    /// CBOR serialization of a value failed.
    #[error("cbor encode failed: {0}")]
    Encode(String),

    /// CBOR deserialization of received bytes failed.
    #[error("cbor decode failed: {0}")]
    Decode(String),

    /// A Noise handshake or transport operation failed.
    #[error("crypto failure: {0}")]
    Crypto(String),

    /// A pairing code did not match the expected `XXXX-XXXX` shape.
    #[error("invalid pairing code")]
    InvalidPairingCode,

    /// A session id was not a 32-character lowercase hex string.
    #[error("invalid session id")]
    InvalidSessionId,

    /// The OS random number generator failed while sampling a credential.
    #[error("rng failure: {0}")]
    Rng(String),

    /// A single frame exceeded [`codec::MAX_FRAME_BYTES`].
    ///
    /// [`codec::MAX_FRAME_BYTES`]: crate::codec::MAX_FRAME_BYTES
    #[error("frame too large: {size} bytes (max {max})")]
    FrameTooLarge {
        /// Actual size of the offending frame.
        size: usize,
        /// Configured upper bound.
        max: usize,
    },

    /// A reassembled L2 message exceeded [`codec::MAX_FRAME_BYTES`] before its
    /// final chunk arrived, indicating a buggy or hostile peer.
    ///
    /// [`codec::MAX_FRAME_BYTES`]: crate::codec::MAX_FRAME_BYTES
    #[error("reassembly buffer overflow (max {max} bytes)")]
    ReassemblyOverflow {
        /// Configured upper bound.
        max: usize,
    },
}
