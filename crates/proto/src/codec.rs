//! CBOR (de)serialization helpers with a hard size ceiling.
//!
//! Both protocol layers are length-delimited by the WebSocket transport (one
//! binary frame per message), so no extra framing is needed here — only
//! encoding, decoding and a guard against pathologically large frames.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::ProtoError;

/// Upper bound on a single decoded frame (32 MiB). Comfortably fits a
/// full-screen WebP screenshot while bounding memory a hostile peer can force
/// us to allocate.
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Serializes a value to a CBOR byte buffer.
///
/// # Errors
/// Returns [`ProtoError::Encode`] if serialization fails, or
/// [`ProtoError::FrameTooLarge`] if the result exceeds [`MAX_FRAME_BYTES`].
pub fn to_cbor<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtoError> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf).map_err(|e| ProtoError::Encode(e.to_string()))?;
    if buf.len() > MAX_FRAME_BYTES {
        return Err(ProtoError::FrameTooLarge {
            size: buf.len(),
            max: MAX_FRAME_BYTES,
        });
    }
    Ok(buf)
}

/// Deserializes a value from a CBOR byte buffer.
///
/// # Errors
/// Returns [`ProtoError::FrameTooLarge`] if the input exceeds
/// [`MAX_FRAME_BYTES`], or [`ProtoError::Decode`] if the bytes are not valid
/// CBOR for `T`.
pub fn from_cbor<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtoError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtoError::FrameTooLarge {
            size: bytes.len(),
            max: MAX_FRAME_BYTES,
        });
    }
    ciborium::from_reader(bytes).map_err(|e| ProtoError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::ClientMsg;

    #[test]
    fn client_msg_round_trips() {
        let msg = ClientMsg::Relay {
            data: vec![1, 2, 3, 4],
        };
        let bytes = to_cbor(&msg).expect("encode");
        let back: ClientMsg = from_cbor(&bytes).expect("decode");
        assert_eq!(msg, back);
    }
}
