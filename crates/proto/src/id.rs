//! Strongly-typed identifiers and roles.
//!
//! Following the *newtype* discipline, every identifier is its own type so that
//! a [`WindowId`] can never be passed where a [`RequestId`] is expected, and so
//! that illegal values (malformed pairing codes, non-hex session ids) are
//! rejected at construction time.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

use crate::error::ProtoError;

/// Identifies a control session shared by exactly one controller and one
/// runner. The relay routes opaque frames purely by this value.
///
/// Backed by 128 bits of randomness, rendered as lowercase hex for logs and
/// URLs. Construction from untrusted input goes through [`SessionId::from_str`],
/// which rejects anything that is not 32 hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId([u8; 16]);

impl SessionId {
    /// Wraps 16 raw bytes (e.g. freshly sampled from a CSPRNG) as a session id.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Returns the underlying 16 bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Samples a fresh random session id from the OS CSPRNG.
    ///
    /// # Errors
    /// Returns [`ProtoError::Rng`] if the OS random source is unavailable.
    pub fn generate() -> Result<Self, ProtoError> {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).map_err(|e| ProtoError::Rng(e.to_string()))?;
        Ok(Self(bytes))
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionId({self})")
    }
}

impl FromStr for SessionId {
    type Err = ProtoError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0u8; 16];
        if s.len() != 32 {
            return Err(ProtoError::InvalidSessionId);
        }
        for (i, byte) in bytes.iter_mut().enumerate() {
            let hi = hex_val(s.as_bytes()[i * 2]).ok_or(ProtoError::InvalidSessionId)?;
            let lo = hex_val(s.as_bytes()[i * 2 + 1]).ok_or(ProtoError::InvalidSessionId)?;
            *byte = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

const fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// The two endpoints of a session. A session admits at most one peer per role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    /// The macOS side driving the session (where the Agent runs).
    Controller,
    /// The Windows side executing commands.
    Runner,
}

impl Role {
    /// The opposite endpoint — the peer a message should be forwarded to.
    #[must_use]
    pub const fn peer(self) -> Self {
        match self {
            Self::Controller => Self::Runner,
            Self::Runner => Self::Controller,
        }
    }
}

/// Monotonically increasing request identifier, scoped to a single controller.
/// Responses and streamed events carry the id of the request they belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RequestId(pub u64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// A native window handle on the runner (Windows `HWND` widened to `u64`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowId(pub u64);

/// Opaque handle to a UI Automation element, as minted by the runner. Treated
/// as an opaque token by the controller; do not parse its contents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ElementId(pub String);

/// A short, human-transferable pairing code of the form `XXXX-XXXX` (Crockford
/// base32 alphabet, case-insensitive). It is shown by the runner and entered on
/// the controller; both sides derive the Noise pre-shared key from it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCode(String);

impl PairingCode {
    /// Validates and normalizes a pairing code to upper-case `XXXX-XXXX`.
    ///
    /// # Errors
    /// Returns [`ProtoError::InvalidPairingCode`] if the input is not two
    /// groups of four base32 characters separated by a single `-`.
    pub fn parse(raw: &str) -> Result<Self, ProtoError> {
        let normalized = raw.trim().to_ascii_uppercase();
        let mut groups = normalized.split('-');
        let (Some(a), Some(b), None) = (groups.next(), groups.next(), groups.next()) else {
            return Err(ProtoError::InvalidPairingCode);
        };
        let valid_group = |g: &str| g.len() == 4 && g.bytes().all(|c| Self::ALPHABET.contains(&c));
        if !valid_group(a) || !valid_group(b) {
            return Err(ProtoError::InvalidPairingCode);
        }
        Ok(Self(normalized))
    }

    /// Samples a fresh `XXXX-XXXX` pairing code from the OS CSPRNG. The 32-char
    /// alphabet divides 256 evenly, so the mapping is bias-free.
    ///
    /// # Errors
    /// Returns [`ProtoError::Rng`] if the OS random source is unavailable.
    pub fn generate() -> Result<Self, ProtoError> {
        let mut bytes = [0u8; 8];
        getrandom::getrandom(&mut bytes).map_err(|e| ProtoError::Rng(e.to_string()))?;
        let mut code = String::with_capacity(9);
        for (i, b) in bytes.iter().enumerate() {
            if i == 4 {
                code.push('-');
            }
            code.push(Self::ALPHABET[(*b as usize) % Self::ALPHABET.len()] as char);
        }
        Ok(Self(code))
    }

    /// The fixed pairing used in `trust_tailnet` mode, where authentication is
    /// the caller's verified Tailscale identity (LocalAPI WhoIs) plus WireGuard
    /// transport — not a secret code. Both peers use this constant so none need
    /// be exchanged; it is intentionally public.
    #[must_use]
    pub fn tailnet_auto() -> Self {
        Self("TNET-0000".to_owned())
    }

    /// Crockford base32 alphabet (no `I`, `L`, `O`, `U` to avoid confusion).
    const ALPHABET: &'static [u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    /// The normalized code as a string slice, for KDF input or display.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Avoid leaking the full pairing secret into logs.
        write!(f, "PairingCode(****-****)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_hex_round_trip() {
        let id = SessionId::from_bytes([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]);
        let text = id.to_string();
        assert_eq!(text, "00112233445566778899aabbccddeeff");
        assert_eq!(SessionId::from_str(&text), Ok(id));
    }

    #[test]
    fn session_id_rejects_bad_input() {
        assert_eq!(
            SessionId::from_str("xyz"),
            Err(ProtoError::InvalidSessionId)
        );
        assert_eq!(
            SessionId::from_str("zz112233445566778899aabbccddeeff"),
            Err(ProtoError::InvalidSessionId)
        );
    }

    #[test]
    fn pairing_code_normalizes_and_validates() {
        let code = PairingCode::parse(" test-1234 ").expect("valid code");
        assert_eq!(code.as_str(), "TEST-1234");
        assert!(PairingCode::parse("TEST").is_err());
        assert!(PairingCode::parse("TEST-123").is_err());
        assert!(PairingCode::parse("7I8H-JK2M").is_err()); // 'I' not in alphabet
    }

    #[test]
    fn generated_credentials_are_valid_and_fresh() {
        let s1 = SessionId::generate().expect("rng");
        let s2 = SessionId::generate().expect("rng");
        assert_ne!(s1, s2, "session ids should differ");
        assert_eq!(SessionId::from_str(&s1.to_string()), Ok(s1));

        let p = PairingCode::generate().expect("rng");
        // Round-trips through the validating parser.
        assert_eq!(PairingCode::parse(p.as_str()).expect("valid"), p);
    }

    #[test]
    fn tailnet_auto_is_a_valid_code() {
        let auto = PairingCode::tailnet_auto();
        assert_eq!(PairingCode::parse(auto.as_str()).expect("valid"), auto);
    }

    #[test]
    fn role_peer_is_involution() {
        assert_eq!(Role::Controller.peer(), Role::Runner);
        assert_eq!(Role::Controller.peer().peer(), Role::Controller);
    }
}
