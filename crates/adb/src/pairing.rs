//! Android 11+ adb wireless *pairing* handshake, ported byte-for-byte from AOSP
//! (`PAIRING_SPEC.md`) and verified against a real `adbd`. On success adbd stores
//! our [`AdbKey`]'s public key as authorized, so the later `A_STLS` connect (with
//! a cert wrapping the same key) is accepted with no further prompt.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::crypto::Cipher;
use crate::key::AdbKey;
use crate::spake2::Spake2;
use crate::{AdbError, Result};

const PKT_SPAKE2_MSG: u8 = 0;
const PKT_PEER_INFO: u8 = 1;
const HEADER_VERSION: u8 = 1;
const MAX_PAYLOAD: usize = 16384; // kMaxPeerInfoSize * 2
const PEER_INFO_SIZE: usize = 8192;
const ADB_RSA_PUB_KEY: u8 = 0;
/// RFC5705 exporter label — adb passes `sizeof("adb-label") = 10`, i.e. with NUL.
const EXPORT_LABEL: &[u8] = b"adb-label\0";
const EXPORT_LEN: usize = 64;

/// A `ServerCertVerifier` that accepts any certificate — adbd does the same on
/// its side during pairing (the SPAKE2 code, not the cert, is the trust anchor).
#[derive(Debug)]
struct AcceptAnyServerCert;

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

/// Builds a rustls client config presenting `key`'s cert and trusting any server.
fn client_config(key: &AdbKey) -> Result<ClientConfig> {
    // Install a crypto provider once; ignore if already set.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let (cert_der, key_der) = key.tls_identity()?;
    ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert))
        .with_client_auth_cert(vec![cert_der], key_der)
        .map_err(|e| AdbError::Tls(format!("client config: {e}")))
}

/// The 8192-byte PeerInfo struct: `{ u8 type; u8 data[8191] }`, zero-padded,
/// carrying our ANDROID_PUBKEY line.
fn build_peer_info(pubkey_line: &str) -> Vec<u8> {
    let mut info = vec![0u8; PEER_INFO_SIZE];
    info[0] = ADB_RSA_PUB_KEY;
    let line = pubkey_line.as_bytes();
    let n = line.len().min(PEER_INFO_SIZE - 1);
    info[1..1 + n].copy_from_slice(&line[..n]);
    info
}

async fn write_packet<W: AsyncWriteExt + Unpin>(w: &mut W, typ: u8, payload: &[u8]) -> Result<()> {
    let mut header = [0u8; 6];
    header[0] = HEADER_VERSION;
    header[1] = typ;
    header[2..6].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&header).await?;
    w.write_all(payload).await?;
    w.flush().await?;
    tracing::debug!(typ, len = payload.len(), "sent pairing packet");
    Ok(())
}

async fn read_packet<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 6];
    r.read_exact(&mut header).await?;
    if header[0] != HEADER_VERSION {
        return Err(AdbError::Protocol(format!(
            "bad header version {}",
            header[0]
        )));
    }
    let typ = header[1];
    let len = u32::from_be_bytes(header[2..6].try_into().unwrap()) as usize;
    if len == 0 || len > MAX_PAYLOAD {
        return Err(AdbError::Protocol(format!("bad payload size {len}")));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    tracing::debug!(typ, len, "recv pairing packet");
    Ok((typ, payload))
}

/// Pairs with `host_port`'s `adbd` using the 6-digit `code`, authorizing `key`
/// (advertised under `name`, e.g. `arc@host`). On success adbd stores the key.
pub async fn pair(host_port: &str, code: &str, key: &AdbKey, name: &str) -> Result<()> {
    let config = client_config(key)?;
    let connector = TlsConnector::from(Arc::new(config));

    tracing::info!(%host_port, "connecting");
    let tcp = TcpStream::connect(host_port).await?;
    // adbd doesn't verify the SNI; any name works.
    let server_name = ServerName::try_from("adb").map_err(|e| AdbError::Tls(e.to_string()))?;
    let mut tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| AdbError::Tls(format!("handshake: {e}")))?;
    tracing::info!("tls established");

    // Channel-bind: append 64 bytes of exported keying material to the code.
    let ekm = tls
        .get_ref()
        .1
        .export_keying_material([0u8; EXPORT_LEN], EXPORT_LABEL, None)
        .map_err(|e| AdbError::Tls(format!("export keying material: {e}")))?;
    let mut pswd = code.as_bytes().to_vec();
    pswd.extend_from_slice(&ekm);

    // SPAKE2 exchange.
    let mut spake = Spake2::new_client();
    let our_msg = spake.generate_msg(&pswd);
    write_packet(&mut tls, PKT_SPAKE2_MSG, &our_msg).await?;

    let (typ, their_msg) = read_packet(&mut tls).await?;
    if typ != PKT_SPAKE2_MSG {
        return Err(AdbError::Protocol(format!(
            "expected SPAKE2_MSG, got {typ}"
        )));
    }
    let key_material = spake
        .process_msg(&their_msg)
        .ok_or(AdbError::PairingRejected)?;
    tracing::info!("spake2 key derived");

    // PeerInfo exchange under AES-128-GCM.
    let mut cipher = Cipher::from_spake2_key(&key_material);
    let ciphertext = cipher.encrypt(&build_peer_info(&key.android_pubkey_line(name)))?;
    write_packet(&mut tls, PKT_PEER_INFO, &ciphertext).await?;

    let (typ, their_ct) = read_packet(&mut tls).await?;
    if typ != PKT_PEER_INFO {
        return Err(AdbError::Protocol(format!("expected PEER_INFO, got {typ}")));
    }
    let their_info = cipher.decrypt(&their_ct)?;
    if their_info.len() != PEER_INFO_SIZE {
        return Err(AdbError::Protocol(format!(
            "peer info size {} != {PEER_INFO_SIZE}",
            their_info.len()
        )));
    }
    tracing::info!(
        peer_type = their_info[0],
        "paired — decrypted device PeerInfo"
    );
    Ok(())
}
