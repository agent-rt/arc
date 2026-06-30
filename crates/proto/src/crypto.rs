//! **L2 transport security.** A Noise channel between controller and runner,
//! keyed by a high-entropy secret established from the pairing code via a
//! balanced PAKE.
//!
//! ## Key agreement ([`Pake`])
//!
//! The pairing code is low-entropy, so it is **never** used directly as a key.
//! Both peers run a symmetric SPAKE2 exchange ([`Pake::start`] →
//! [`Pake::finish`]): one message each way yields a shared 32-byte key that an
//! eavesdropper cannot recover, and that only parties knowing the code can
//! derive. A wrong code yields a *different* key on each side, which the
//! subsequent Noise handshake then rejects (key confirmation). This defeats the
//! offline dictionary attack a raw low-entropy PSK would invite.
//!
//! ## Pattern
//!
//! The PAKE key feeds `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` as its PSK.
//! Neither side has a static identity key; authentication comes from the shared
//! PAKE key. The Noise handshake is two messages:
//!
//! ```text
//! initiator → responder :  psk, e
//! responder → initiator :  e, ee
//! ```
//!
//! ## Chunking
//!
//! Noise caps a single message at 65535 bytes, but L2 frames (screenshots) can
//! be tens of MiB. [`Channel::seal`] therefore splits a logical message into
//! ordered chunks, each a self-contained Noise record carrying a 1-byte
//! continuation flag, and [`Channel::open`] reassembles them.

use blake2::{Blake2s256, Digest};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::error::ProtoError;
use crate::id::{PairingCode, Role};

/// Noise protocol parameters; see the module docs.
const NOISE_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";

/// Hard cap on a single Noise message, per the Noise specification.
const NOISE_MAX_MESSAGE: usize = 65535;

/// AEAD tag length for ChaChaPoly.
const TAG_LEN: usize = 16;

/// One leading flag byte per chunk: `0` = more follow, `1` = final chunk.
const FLAG_LEN: usize = 1;

/// Maximum plaintext data bytes carried per chunk (excluding flag and tag).
const MAX_CHUNK_DATA: usize = NOISE_MAX_MESSAGE - TAG_LEN - FLAG_LEN;

/// Continuation flag values prefixed to each chunk's plaintext.
const FLAG_MORE: u8 = 0;
const FLAG_FINAL: u8 = 1;

/// Generous handshake-message scratch headroom (ephemeral key + tag + slack).
const HANDSHAKE_OVERHEAD: usize = 512;

fn crypto_err(e: impl std::fmt::Display) -> ProtoError {
    ProtoError::Crypto(e.to_string())
}

/// Fixed symmetric-SPAKE2 identity; both peers must use the same value.
const SPAKE_IDENTITY: &[u8] = b"arc/spake/v1";

/// One side of a symmetric SPAKE2 password-authenticated key exchange.
///
/// Usage: [`start`](Pake::start) yields this state plus a message to send to
/// the peer; feed the peer's message to [`finish`](Pake::finish) to obtain the
/// shared 32-byte key. Both sides use the *same* pairing code and identity.
#[derive(Debug)]
pub struct Pake {
    inner: Spake2<Ed25519Group>,
}

impl Pake {
    /// Begins the exchange, returning the outbound message to relay to the peer.
    #[must_use]
    pub fn start(code: &PairingCode) -> (Self, Vec<u8>) {
        let (inner, message) = Spake2::<Ed25519Group>::start_symmetric(
            &Password::new(code.as_str().as_bytes()),
            &Identity::new(SPAKE_IDENTITY),
        );
        (Self { inner }, message)
    }

    /// Completes the exchange with the peer's message, deriving the 32-byte
    /// Noise pre-shared key. A mismatched pairing code produces a *different*
    /// key here (no error); the divergence is caught by the Noise handshake.
    ///
    /// # Errors
    /// Returns [`ProtoError::Crypto`] if the peer's message is malformed.
    pub fn finish(self, peer_message: &[u8]) -> Result<[u8; 32], ProtoError> {
        let shared = self.inner.finish(peer_message).map_err(crypto_err)?;
        // Domain-separate and normalize to a fixed 32-byte PSK regardless of the
        // group's raw key length.
        let mut hasher = Blake2s256::new();
        hasher.update(b"arc/psk/v2");
        hasher.update(&shared);
        Ok(hasher.finalize().into())
    }
}

/// The handshake half of the channel. Drive it to completion with
/// [`Handshake::write`]/[`Handshake::read`], then call [`Handshake::finish`].
#[derive(Debug)]
pub struct Handshake {
    inner: snow::HandshakeState,
}

impl Handshake {
    /// Builds the initiator (the [`Role::Controller`]) side.
    ///
    /// # Errors
    /// Returns [`ProtoError::Crypto`] if Noise parameters or keys are invalid.
    pub fn initiator(psk: &[u8; 32]) -> Result<Self, ProtoError> {
        Self::build(psk, Role::Controller)
    }

    /// Builds the responder (the [`Role::Runner`]) side.
    ///
    /// # Errors
    /// Returns [`ProtoError::Crypto`] if Noise parameters or keys are invalid.
    pub fn responder(psk: &[u8; 32]) -> Result<Self, ProtoError> {
        Self::build(psk, Role::Runner)
    }

    fn build(psk: &[u8; 32], role: Role) -> Result<Self, ProtoError> {
        let params = NOISE_PARAMS.parse().map_err(crypto_err)?;
        let builder = snow::Builder::new(params).psk(0, psk);
        let inner = match role {
            Role::Controller => builder.build_initiator(),
            Role::Runner => builder.build_responder(),
        }
        .map_err(crypto_err)?;
        Ok(Self { inner })
    }

    /// Writes the next handshake message to send to the peer.
    ///
    /// # Errors
    /// Returns [`ProtoError::Crypto`] on Noise failure.
    pub fn write(&mut self) -> Result<Vec<u8>, ProtoError> {
        let mut buf = vec![0u8; HANDSHAKE_OVERHEAD];
        let n = self
            .inner
            .write_message(&[], &mut buf)
            .map_err(crypto_err)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Reads a handshake message received from the peer.
    ///
    /// # Errors
    /// Returns [`ProtoError::Crypto`] on Noise failure (including a wrong PSK,
    /// i.e. a mismatched pairing code).
    pub fn read(&mut self, message: &[u8]) -> Result<(), ProtoError> {
        let mut buf = vec![0u8; message.len() + HANDSHAKE_OVERHEAD];
        self.inner
            .read_message(message, &mut buf)
            .map_err(crypto_err)?;
        Ok(())
    }

    /// Whether the handshake has exchanged all required messages.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.inner.is_handshake_finished()
    }

    /// Converts a completed handshake into the bidirectional transport channel.
    ///
    /// # Errors
    /// Returns [`ProtoError::Crypto`] if the handshake is not yet finished.
    pub fn finish(self) -> Result<Channel, ProtoError> {
        let transport = self.inner.into_transport_mode().map_err(crypto_err)?;
        Ok(Channel {
            inner: transport,
            reassembly: Vec::new(),
        })
    }
}

/// An established, bidirectional encrypted channel.
///
/// **Ordering matters:** Noise records are sequenced by an internal nonce
/// counter, so chunks produced by [`seal`](Channel::seal) must be delivered to
/// the peer's [`open`](Channel::open) in order. The relay preserves order over
/// a single WebSocket connection, so this holds in practice.
#[derive(Debug)]
pub struct Channel {
    inner: snow::TransportState,
    reassembly: Vec<u8>,
}

impl Channel {
    /// Seals a logical L2 message into one or more ordered Noise records, each
    /// suitable for a single [`ClientMsg::Relay`](crate::relay::ClientMsg::Relay)
    /// payload.
    ///
    /// # Errors
    /// Returns [`ProtoError::Crypto`] on AEAD failure.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<Vec<u8>>, ProtoError> {
        // `chunks` yields nothing for an empty slice, so handle that explicitly
        // to guarantee at least one (final) record is always emitted.
        let mut records = Vec::new();
        let mut iter = plaintext.chunks(MAX_CHUNK_DATA).peekable();
        if iter.peek().is_none() {
            records.push(self.seal_chunk(FLAG_FINAL, &[])?);
            return Ok(records);
        }
        while let Some(chunk) = iter.next() {
            let flag = if iter.peek().is_none() {
                FLAG_FINAL
            } else {
                FLAG_MORE
            };
            records.push(self.seal_chunk(flag, chunk)?);
        }
        Ok(records)
    }

    fn seal_chunk(&mut self, flag: u8, data: &[u8]) -> Result<Vec<u8>, ProtoError> {
        let mut framed = Vec::with_capacity(FLAG_LEN + data.len());
        framed.push(flag);
        framed.extend_from_slice(data);
        let mut out = vec![0u8; framed.len() + TAG_LEN];
        let n = self
            .inner
            .write_message(&framed, &mut out)
            .map_err(crypto_err)?;
        out.truncate(n);
        Ok(out)
    }

    /// Feeds one received Noise record into the reassembly buffer.
    ///
    /// Returns `Ok(Some(message))` once a record marked final completes a
    /// logical message, or `Ok(None)` while more chunks are expected.
    ///
    /// # Errors
    /// Returns [`ProtoError::Crypto`] on AEAD failure or
    /// [`ProtoError::ReassemblyOverflow`] if a peer streams an oversized
    /// message.
    pub fn open(&mut self, record: &[u8]) -> Result<Option<Vec<u8>>, ProtoError> {
        let mut out = vec![0u8; record.len()];
        let n = self
            .inner
            .read_message(record, &mut out)
            .map_err(crypto_err)?;
        out.truncate(n);

        let Some((&flag, data)) = out.split_first() else {
            // A record must always contain at least the flag byte.
            return Err(ProtoError::Crypto("empty noise record".into()));
        };

        if self.reassembly.len() + data.len() > crate::codec::MAX_FRAME_BYTES {
            self.reassembly.clear();
            return Err(ProtoError::ReassemblyOverflow {
                max: crate::codec::MAX_FRAME_BYTES,
            });
        }
        self.reassembly.extend_from_slice(data);

        if flag == FLAG_FINAL {
            Ok(Some(std::mem::take(&mut self.reassembly)))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a full symmetric SPAKE2 exchange between two codes and returns both
    /// derived PSKs.
    fn pake_keys(code_a: &str, code_b: &str) -> ([u8; 32], [u8; 32]) {
        let (a, msg_a) = Pake::start(&PairingCode::parse(code_a).expect("a"));
        let (b, msg_b) = Pake::start(&PairingCode::parse(code_b).expect("b"));
        let key_a = a.finish(&msg_b).expect("finish a");
        let key_b = b.finish(&msg_a).expect("finish b");
        (key_a, key_b)
    }

    fn paired_channels() -> (Channel, Channel) {
        let (psk_a, psk_b) = pake_keys("TEST-1234", "TEST-1234");
        assert_eq!(psk_a, psk_b, "same code must agree on a key");
        let mut initiator = Handshake::initiator(&psk_a).expect("initiator");
        let mut responder = Handshake::responder(&psk_b).expect("responder");

        // initiator → responder
        let m1 = initiator.write().expect("m1");
        responder.read(&m1).expect("read m1");
        // responder → initiator
        let m2 = responder.write().expect("m2");
        initiator.read(&m2).expect("read m2");

        assert!(initiator.is_finished() && responder.is_finished());
        (
            initiator.finish().expect("c1"),
            responder.finish().expect("c2"),
        )
    }

    #[test]
    fn small_message_single_chunk_round_trip() {
        let (mut ctrl, mut runner) = paired_channels();
        let msg = b"open notepad";
        let records = ctrl.seal(msg).expect("seal");
        assert_eq!(records.len(), 1);
        let mut got = None;
        for r in &records {
            got = runner.open(r).expect("open");
        }
        assert_eq!(got.as_deref(), Some(&msg[..]));
    }

    #[test]
    fn large_message_chunks_and_reassembles() {
        let (mut ctrl, mut runner) = paired_channels();
        // ~200 KiB simulated screenshot → must span several Noise records.
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let records = ctrl.seal(&payload).expect("seal");
        assert!(records.len() >= 4, "expected multiple chunks");

        let mut assembled = None;
        for r in &records {
            if let Some(done) = runner.open(r).expect("open") {
                assembled = Some(done);
            }
        }
        assert_eq!(assembled, Some(payload));
    }

    #[test]
    fn pake_same_code_agrees() {
        let (key_a, key_b) = pake_keys("TEST-1234", "TEST-1234");
        assert_eq!(key_a, key_b);
    }

    #[test]
    fn pake_different_code_diverges() {
        let (key_a, key_b) = pake_keys("TEST-1234", "ZZZZ-ZZZZ");
        assert_ne!(key_a, key_b, "different codes must not agree");
    }

    #[test]
    fn wrong_pairing_code_fails_handshake() {
        // A mismatched code yields divergent PAKE keys; the Noise handshake then
        // rejects the first message (key confirmation).
        let (psk_a, psk_b) = pake_keys("TEST-1234", "ZZZZ-ZZZZ");
        let mut initiator = Handshake::initiator(&psk_a).expect("init");
        let mut responder = Handshake::responder(&psk_b).expect("resp");
        let m1 = initiator.write().expect("m1");
        assert!(responder.read(&m1).is_err());
    }
}
