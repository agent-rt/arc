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

    // Adding a `#[serde(default)]` field to an existing struct-enum variant must
    // stay wire-compatible in BOTH directions: an old peer (no field) decoding a
    // new peer's frame ignores the extra field, and a new peer decoding an old
    // frame defaults the field. Verifies ciborium encodes struct variants as
    // field-keyed maps (tolerant), not positional arrays.
    #[test]
    fn added_default_field_is_wire_compatible() {
        #[derive(serde::Serialize, serde::Deserialize)]
        enum NewCmd {
            Run {
                a: u32,
                #[serde(default)]
                env: Vec<(String, String)>,
            },
        }
        #[derive(serde::Serialize, serde::Deserialize)]
        enum OldCmd {
            Run { a: u32 },
        }

        // New (with env) → old peer must decode by ignoring the unknown field.
        let new = NewCmd::Run {
            a: 1,
            env: vec![("K".into(), "V".into())],
        };
        let bytes = to_cbor(&new).expect("encode new");
        let OldCmd::Run { a } = from_cbor::<OldCmd>(&bytes).expect("old decodes new");
        assert_eq!(a, 1);

        // Old (no env) → new peer must decode by defaulting env to empty.
        let old = OldCmd::Run { a: 2 };
        let bytes = to_cbor(&old).expect("encode old");
        let NewCmd::Run { a, env } = from_cbor::<NewCmd>(&bytes).expect("new decodes old");
        assert_eq!(a, 2);
        assert!(env.is_empty());
    }

    // A command variant added by a newer peer must decode to the `#[serde(other)]`
    // sentinel on an older peer — not fail the whole frame. This is what lets a
    // runner answer "unsupported command" and keep the link instead of resetting
    // it. (`Command::Unsupported` relies on exactly this ciborium behavior.)
    #[test]
    fn unknown_variant_decodes_to_serde_other() {
        #[derive(serde::Serialize)]
        enum Newer {
            Future { data: String },
        }
        #[derive(serde::Deserialize, Debug, PartialEq)]
        enum Older {
            #[allow(dead_code)]
            Known,
            #[serde(other)]
            Unsupported,
        }
        let bytes = to_cbor(&Newer::Future { data: "x".into() }).expect("encode");
        assert_eq!(
            from_cbor::<Older>(&bytes).expect("unknown variant decodes, not errors"),
            Older::Unsupported
        );
    }
}
