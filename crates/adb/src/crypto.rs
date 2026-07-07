//! The AES-128-GCM cipher adb uses to protect the PeerInfo exchange, keyed off
//! the SPAKE2 shared secret. Byte-exact with `pairing_auth/aes_128_gcm.cpp`
//! (see `PAIRING_SPEC.md`):
//!
//! - key = HKDF-SHA256(ikm = 64-byte SPAKE2 key, salt = none,
//!   info = `"adb pairing_auth aes-128-gcm key"` (32 bytes, no NUL)) → 16 bytes.
//! - nonce = 12 bytes: a little-endian u64 sequence counter in bytes `[0,8)`,
//!   zeros in `[8,12)`. Separate enc/dec counters, both from 0, post-increment.
//! - no AAD; 16-byte GCM tag appended to the ciphertext.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

const HKDF_INFO: &[u8] = b"adb pairing_auth aes-128-gcm key"; // 32 bytes, no NUL

/// A keyed cipher with independent send/receive sequence counters.
pub struct Cipher {
    aead: Aes128Gcm,
    enc_seq: u64,
    dec_seq: u64,
}

impl Cipher {
    /// Derives the AES-128 key from the SPAKE2 shared `key_material` via adb's HKDF.
    pub fn from_spake2_key(key_material: &[u8]) -> Self {
        let hk = Hkdf::<Sha256>::new(None, key_material);
        let mut key = [0u8; 16];
        hk.expand(HKDF_INFO, &mut key)
            .expect("16 is a valid HKDF-SHA256 output length");
        let aead = Aes128Gcm::new((&key).into());
        Self {
            aead,
            enc_seq: 0,
            dec_seq: 0,
        }
    }

    fn nonce(seq: u64) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[..8].copy_from_slice(&seq.to_le_bytes());
        n
    }

    /// Encrypts `plaintext` under the current enc counter, then advances it.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> crate::Result<Vec<u8>> {
        let nonce = Self::nonce(self.enc_seq);
        let out = self
            .aead
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| crate::AdbError::Protocol("aes-gcm encrypt failed".into()))?;
        self.enc_seq += 1;
        Ok(out)
    }

    /// Decrypts `ciphertext` (data ++ 16-byte tag) under the current dec counter,
    /// then advances it.
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> crate::Result<Vec<u8>> {
        let nonce = Self::nonce(self.dec_seq);
        let out = self
            .aead
            .decrypt(Nonce::from_slice(&nonce), ciphertext)
            .map_err(|_| crate::AdbError::Protocol("aes-gcm decrypt failed".into()))?;
        self.dec_seq += 1;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_advances_counters() {
        // Two peers derive the same cipher from the same SPAKE2 key; one's enc
        // stream is the other's dec stream (matching enc/dec seq per direction).
        let key = [7u8; 64];
        let mut a = Cipher::from_spake2_key(&key);
        let mut b = Cipher::from_spake2_key(&key);
        let ct = a.encrypt(b"hello").unwrap();
        assert_eq!(ct.len(), 5 + 16, "ciphertext = plaintext + GCM tag");
        assert_eq!(b.decrypt(&ct).unwrap(), b"hello");
        // Second message uses seq=1 on both sides.
        let ct2 = a.encrypt(b"world").unwrap();
        assert_eq!(b.decrypt(&ct2).unwrap(), b"world");
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let mut a = Cipher::from_spake2_key(&[1u8; 64]);
        let mut b = Cipher::from_spake2_key(&[2u8; 64]);
        let ct = a.encrypt(b"secret").unwrap();
        assert!(b.decrypt(&ct).is_err());
    }
}
