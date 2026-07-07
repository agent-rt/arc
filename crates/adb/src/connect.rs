//! adb transport connect over Android 11+ TLS (`A_STLS`), then run one shell
//! command. Ported from AOSP `adb.h`/`transport.cpp` (see `PAIRING_SPEC.md §7`).
//!
//! Wire sequence (we are the client):
//! 1. → `CNXN(A_VERSION, MAX_PAYLOAD, "host::features=…")`
//! 2. ← `STLS` (adbd requests TLS)
//! 3. → `STLS`, then a TLS 1.3 handshake presenting our key's cert
//! 4. ← `CNXN` (adbd's banner, now over TLS) — connection is online
//! 5. → `OPEN(local_id, 0, "shell:<cmd>\0")`
//! 6. ← `OKAY` then `WRTE`(output)… we `OKAY` each, until `CLSE`
//!
//! Every message is a 24-byte header + payload; `data_check = 0` (we advertise
//! `A_VERSION_SKIP_CHECKSUM`), `magic = command ^ 0xffffffff`, all little-endian.

use std::sync::Arc;

use rustls_pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::key::AdbKey;
use crate::tls::client_config;
use crate::{AdbError, Result};

const A_CNXN: u32 = 0x4e58_4e43;
const A_OPEN: u32 = 0x4e45_504f;
const A_OKAY: u32 = 0x5941_4b4f;
const A_CLSE: u32 = 0x4553_4c43;
const A_WRTE: u32 = 0x4554_5257;
const A_STLS: u32 = 0x534c_5453;
const A_VERSION: u32 = 0x0100_0001; // == A_VERSION_SKIP_CHECKSUM
const A_STLS_VERSION: u32 = 0x0100_0000;
const MAX_PAYLOAD: u32 = 1024 * 1024;
const LOCAL_ID: u32 = 1;

/// An adb protocol message.
struct Message {
    command: u32,
    arg0: u32,
    arg1: u32,
    payload: Vec<u8>,
}

async fn write_msg<W: AsyncWrite + Unpin>(w: &mut W, m: &Message) -> Result<()> {
    let mut header = [0u8; 24];
    header[0..4].copy_from_slice(&m.command.to_le_bytes());
    header[4..8].copy_from_slice(&m.arg0.to_le_bytes());
    header[8..12].copy_from_slice(&m.arg1.to_le_bytes());
    header[12..16].copy_from_slice(&(m.payload.len() as u32).to_le_bytes());
    header[16..20].copy_from_slice(&0u32.to_le_bytes()); // data_check (skipped)
    header[20..24].copy_from_slice(&(m.command ^ 0xffff_ffff).to_le_bytes());
    w.write_all(&header).await?;
    if !m.payload.is_empty() {
        w.write_all(&m.payload).await?;
    }
    w.flush().await?;
    tracing::debug!(cmd = %fourcc(m.command), arg0 = m.arg0, arg1 = m.arg1, len = m.payload.len(), "→");
    Ok(())
}

async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> Result<Message> {
    let mut header = [0u8; 24];
    r.read_exact(&mut header).await?;
    let command = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let arg0 = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let arg1 = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let len = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    let magic = u32::from_le_bytes(header[20..24].try_into().unwrap());
    if magic != command ^ 0xffff_ffff {
        return Err(AdbError::Protocol(format!(
            "bad magic for cmd {}",
            fourcc(command)
        )));
    }
    if len > MAX_PAYLOAD as usize {
        return Err(AdbError::Protocol(format!("payload too big: {len}")));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    tracing::debug!(cmd = %fourcc(command), arg0, arg1, len, "←");
    Ok(Message {
        command,
        arg0,
        arg1,
        payload,
    })
}

fn fourcc(c: u32) -> String {
    String::from_utf8_lossy(&c.to_le_bytes()).into_owned()
}

/// Connects to `adbd` at `host_port` (a `_adb-tls-connect` endpoint) authorizing
/// with `key`, runs `shell:<command>`, and returns the raw combined output.
pub async fn run_shell(host_port: &str, key: &AdbKey, command: &str) -> Result<Vec<u8>> {
    let config = client_config(key)?;
    let connector = TlsConnector::from(Arc::new(config));

    // --- pre-TLS: CNXN, then the STLS exchange, on the raw socket ---
    let mut tcp = TcpStream::connect(host_port).await?;
    write_msg(
        &mut tcp,
        &Message {
            command: A_CNXN,
            arg0: A_VERSION,
            arg1: MAX_PAYLOAD,
            payload: b"host::features=shell_v2,cmd".to_vec(),
        },
    )
    .await?;

    let msg = read_msg(&mut tcp).await?;
    if msg.command != A_STLS {
        return Err(AdbError::Protocol(format!(
            "expected STLS, got {}",
            fourcc(msg.command)
        )));
    }
    write_msg(
        &mut tcp,
        &Message {
            command: A_STLS,
            arg0: A_STLS_VERSION,
            arg1: 0,
            payload: Vec::new(),
        },
    )
    .await?;

    // --- upgrade to TLS; adbd matches our client cert against the paired key ---
    let server_name = ServerName::try_from("adb").map_err(|e| AdbError::Tls(e.to_string()))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| AdbError::Tls(format!("handshake: {e}")))?;
    tracing::info!("tls established");
    let (mut rd, mut wr) = tokio::io::split(tls);

    // --- post-TLS: adbd sends its CNXN banner ---
    let banner = read_msg(&mut rd).await?;
    if banner.command != A_CNXN {
        return Err(AdbError::Protocol(format!(
            "expected CNXN banner, got {}",
            fourcc(banner.command)
        )));
    }
    tracing::info!(banner = %String::from_utf8_lossy(&banner.payload), "online");

    // --- open a shell stream for the command ---
    let mut svc = format!("shell:{command}").into_bytes();
    svc.push(0); // adb service strings are NUL-terminated
    write_msg(
        &mut wr,
        &Message {
            command: A_OPEN,
            arg0: LOCAL_ID,
            arg1: 0,
            payload: svc,
        },
    )
    .await?;

    // --- pump the stream: collect WRTE payloads until CLSE ---
    let mut remote_id = 0u32;
    let mut output = Vec::new();
    loop {
        let m = read_msg(&mut rd).await?;
        match m.command {
            A_OKAY => {
                remote_id = m.arg0;
            }
            A_WRTE => {
                remote_id = m.arg0;
                output.extend_from_slice(&m.payload);
                // Acknowledge so adbd keeps streaming.
                write_msg(
                    &mut wr,
                    &Message {
                        command: A_OKAY,
                        arg0: LOCAL_ID,
                        arg1: remote_id,
                        payload: Vec::new(),
                    },
                )
                .await?;
            }
            A_CLSE => {
                // Politely close our end and finish.
                let _ = write_msg(
                    &mut wr,
                    &Message {
                        command: A_CLSE,
                        arg0: LOCAL_ID,
                        arg1: remote_id,
                        payload: Vec::new(),
                    },
                )
                .await;
                break;
            }
            other => {
                return Err(AdbError::Protocol(format!(
                    "unexpected {} on shell stream",
                    fourcc(other)
                )));
            }
        }
    }
    Ok(output)
}
