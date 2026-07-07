//! The adb identity: an RSA-2048 key that is both (a) encoded as an
//! `ANDROID_PUBKEY` line for the pairing PeerInfo and (b) wrapped in a
//! self-signed X.509 cert for TLS client auth (pairing + the later `A_STLS`
//! connect). adbd stores the pubkey at pairing and matches the connect cert's
//! key against it, so both must come from the *same* RSA key.
//!
//! `ANDROID_PUBKEY` (524-byte `RSAPublicKey`, all little-endian; see AOSP
//! `libcrypto_utils/android_pubkey.c`):
//! `{ u32 modulus_size_words=64; u32 n0inv; u8 modulus[256]; u8 rr[256]; u32 e }`
//! where `n0inv = -1/n[0] mod 2^32` and `rr = 2^4096 mod n`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use num_bigint_dig::BigUint;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::{AdbError, Result};

const MODULUS_BYTES: usize = 256; // 2048-bit
const ENCODED_SIZE: usize = 4 + 4 + MODULUS_BYTES + MODULUS_BYTES + 4; // 524

/// An adb RSA-2048 identity.
pub struct AdbKey {
    private: RsaPrivateKey,
}

impl AdbKey {
    /// Generates a fresh RSA-2048 key.
    pub fn generate() -> Result<Self> {
        let mut rng = rand::thread_rng();
        let private = RsaPrivateKey::new(&mut rng, 2048)
            .map_err(|e| AdbError::Protocol(format!("rsa keygen: {e}")))?;
        Ok(Self { private })
    }

    /// Loads a PKCS#8 PEM private key (e.g. `~/.android/adbkey`). Trims
    /// surrounding whitespace first — the strict RFC7468 parser rejects trailing
    /// bytes, and storage round-trips (e.g. Android SharedPreferences XML) can
    /// append indentation whitespace to the value.
    pub fn from_pkcs8_pem(pem: &str) -> Result<Self> {
        let private = RsaPrivateKey::from_pkcs8_pem(pem.trim())
            .map_err(|e| AdbError::Protocol(format!("load adbkey: {e}")))?;
        Ok(Self { private })
    }

    /// Serializes the private key as PKCS#8 PEM for persistence.
    pub fn to_pkcs8_pem(&self) -> Result<String> {
        Ok(self
            .private
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|e| AdbError::Protocol(format!("encode key: {e}")))?
            .to_string())
    }

    /// A self-signed X.509 cert wrapping this RSA key, plus the key, both in the
    /// DER forms rustls wants — for TLS client auth in pairing and `A_STLS`
    /// connect. adbd matches the connect cert's key against the paired pubkey, so
    /// the cert must wrap this same key.
    pub fn tls_identity(&self) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
        let pem = self.to_pkcs8_pem()?;
        let key_pair = rcgen::KeyPair::from_pkcs8_pem_and_sign_algo(&pem, &rcgen::PKCS_RSA_SHA256)
            .map_err(|e| AdbError::Tls(format!("rcgen key: {e}")))?;
        let params = rcgen::CertificateParams::new(vec!["arc".to_string()])
            .map_err(|e| AdbError::Tls(format!("cert params: {e}")))?;
        let cert = params
            .self_signed(&key_pair)
            .map_err(|e| AdbError::Tls(format!("self-sign: {e}")))?;
        let cert_der = cert.der().clone();
        let pkcs8 = self
            .private
            .to_pkcs8_der()
            .map_err(|e| AdbError::Tls(format!("pkcs8: {e}")))?;
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8.as_bytes().to_vec()));
        Ok((cert_der, key_der))
    }

    /// The `ANDROID_PUBKEY` line: `base64(524-byte struct) + " " + name`.
    pub fn android_pubkey_line(&self, name: &str) -> String {
        let encoded = self.encode_android_pubkey();
        format!("{} {}", B64.encode(encoded), name)
    }

    fn encode_android_pubkey(&self) -> [u8; ENCODED_SIZE] {
        let pubkey = RsaPublicKey::from(&self.private);
        let n = pubkey.n();
        let e = pubkey.e();

        let mut buf = [0u8; ENCODED_SIZE];
        // modulus_size_words = 64
        buf[0..4].copy_from_slice(&64u32.to_le_bytes());
        // n0inv = -1 / n[0] mod 2^32
        let n_le = n.to_bytes_le();
        let mut n0 = [0u8; 4];
        n0.copy_from_slice(&n_le[..4]);
        let n0inv = modinv_pow2_32(u32::from_le_bytes(n0)).wrapping_neg();
        buf[4..8].copy_from_slice(&n0inv.to_le_bytes());
        // modulus (LE, zero-padded to 256)
        write_le_padded(&mut buf[8..8 + MODULUS_BYTES], &n_le);
        // rr = 2^4096 mod n (LE, padded)
        let rr = BigUint::from(2u32).modpow(&BigUint::from(4096u32), n);
        write_le_padded(&mut buf[264..264 + MODULUS_BYTES], &rr.to_bytes_le());
        // exponent (small — 65537 — fits in a u32)
        let mut e_word = [0u8; 4];
        write_le_padded(&mut e_word, &e.to_bytes_le());
        buf[520..524].copy_from_slice(&e_word);
        buf
    }
}

/// Writes `le_bytes` into `dst` (already zeroed) little-endian, truncating extra
/// high zero bytes if the source is longer.
fn write_le_padded(dst: &mut [u8], le_bytes: &[u8]) {
    let n = le_bytes.len().min(dst.len());
    dst[..n].copy_from_slice(&le_bytes[..n]);
}

/// Modular inverse of an odd `a` modulo 2^32, via Hensel lifting (`inv` starts
/// correct mod 2 and each step doubles the correct bits: 2→4→…→2^32).
fn modinv_pow2_32(a: u32) -> u32 {
    let mut inv: u32 = 1;
    for _ in 0..5 {
        inv = inv.wrapping_mul(2u32.wrapping_sub(a.wrapping_mul(inv)));
    }
    inv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_identity_builds() {
        // rcgen must sign an RSA self-signed cert with the aws-lc-rs backend.
        let key = AdbKey::generate().unwrap();
        let (cert, _key) = key.tls_identity().expect("build tls identity");
        assert!(!cert.as_ref().is_empty());
    }

    #[test]
    fn from_pkcs8_pem_tolerates_surrounding_whitespace() {
        // Regression: Android SharedPreferences XML round-trips the key with
        // trailing indentation whitespace, which the strict PEM parser rejected.
        let key = AdbKey::generate().unwrap();
        let pem = key.to_pkcs8_pem().unwrap();
        AdbKey::from_pkcs8_pem(&format!("  {pem}    \n")).expect("trim before parse");
    }

    #[test]
    fn modinv_is_inverse() {
        for a in [1u32, 3, 5, 65537, 0x9F3B_1D01, u32::MAX] {
            assert_eq!(a.wrapping_mul(modinv_pow2_32(a)), 1, "a={a:#x}");
        }
    }

    /// Cross-validate our ANDROID_PUBKEY encoding against the system adb's own
    /// output: load `~/.android/adbkey` and check we reproduce the base64 in
    /// `~/.android/adbkey.pub`. Skips if the oracle files aren't present.
    #[test]
    fn matches_system_adb_pubkey() {
        let home = std::env::var("HOME").unwrap();
        let key_path = format!("{home}/.android/adbkey");
        let pub_path = format!("{home}/.android/adbkey.pub");
        let (Ok(pem), Ok(publine)) = (
            std::fs::read_to_string(&key_path),
            std::fs::read_to_string(&pub_path),
        ) else {
            eprintln!("skip: no ~/.android/adbkey oracle");
            return;
        };
        let key = AdbKey::from_pkcs8_pem(&pem).expect("load adbkey");
        let ours = B64.encode(key.encode_android_pubkey());
        let theirs = publine.split_whitespace().next().unwrap();
        assert_eq!(ours, theirs, "ANDROID_PUBKEY base64 must match system adb");
    }
}
