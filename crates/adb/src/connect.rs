//! adb transport connect over Android 11+ TLS (`A_STLS`), then adb *services*
//! (`shell:`, `sync:`). Ported from AOSP `adb.h`/`transport.cpp`/
//! `file_sync_protocol.h` (see `PAIRING_SPEC.md §7`).
//!
//! Connect sequence (we are the client):
//! 1. → `CNXN(A_VERSION, MAX_PAYLOAD, "host::features=…")`
//! 2. ← `STLS` (adbd requests TLS)
//! 3. → `STLS`, then a TLS 1.3 handshake presenting our key's cert
//! 4. ← `CNXN` (adbd's banner, over TLS) — online
//!
//! Then a *stream* is opened per service with `OPEN`/`OKAY`, and bytes flow in
//! `WRTE` messages under adb's stop-and-wait flow control (every `WRTE` is
//! answered by an `OKAY` before the next).

use std::sync::Arc;
use std::time::Duration;

use rustls_pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf, split};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

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
const SYNC_DATA_MAX: usize = 64 * 1024;

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

/// A live, TLS-authenticated adb connection with one open service stream.
pub struct AdbStream {
    rd: ReadHalf<TlsStream<TcpStream>>,
    wr: WriteHalf<TlsStream<TcpStream>>,
    remote_id: u32,
}

impl AdbStream {
    /// Connects, does the STLS/TLS handshake with `key`, and opens `service`
    /// (e.g. `"shell:id"`, `"sync:"`).
    pub async fn open(host_port: &str, key: &AdbKey, service: &str) -> Result<Self> {
        let config = client_config(key)?;
        let connector = TlsConnector::from(Arc::new(config));

        // pre-TLS: CNXN then the STLS exchange on the raw socket.
        let mut tcp = TcpStream::connect(host_port).await?;
        write_msg(
            &mut tcp,
            &msg(
                A_CNXN,
                A_VERSION,
                MAX_PAYLOAD,
                b"host::features=shell_v2,cmd",
            ),
        )
        .await?;
        let m = read_msg(&mut tcp).await?;
        if m.command != A_STLS {
            return Err(AdbError::Protocol(format!(
                "expected STLS, got {}",
                fourcc(m.command)
            )));
        }
        write_msg(&mut tcp, &msg(A_STLS, A_STLS_VERSION, 0, &[])).await?;

        // upgrade to TLS; adbd matches our cert against the paired key.
        let server_name = ServerName::try_from("adb").map_err(|e| AdbError::Tls(e.to_string()))?;
        let tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| AdbError::Tls(format!("handshake: {e}")))?;
        let (mut rd, mut wr) = split(tls);

        // post-TLS: adbd's CNXN banner.
        let banner = read_msg(&mut rd).await?;
        if banner.command != A_CNXN {
            return Err(AdbError::Protocol(format!(
                "expected CNXN banner, got {}",
                fourcc(banner.command)
            )));
        }
        tracing::info!(banner = %String::from_utf8_lossy(&banner.payload), "online");

        // open the service stream.
        let mut svc = service.as_bytes().to_vec();
        svc.push(0); // service strings are NUL-terminated
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
        let ok = read_msg(&mut rd).await?;
        if ok.command != A_OKAY {
            return Err(AdbError::Protocol(format!(
                "OPEN {service} refused: {}",
                fourcc(ok.command)
            )));
        }
        Ok(Self {
            rd,
            wr,
            remote_id: ok.arg0,
        })
    }

    /// Sends one `WRTE` and waits for the peer's `OKAY` (adb flow control).
    async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        write_msg(
            &mut self.wr,
            &Message {
                command: A_WRTE,
                arg0: LOCAL_ID,
                arg1: self.remote_id,
                payload: data.to_vec(),
            },
        )
        .await?;
        let ack = read_msg(&mut self.rd).await?;
        if ack.command != A_OKAY {
            return Err(AdbError::Protocol(format!(
                "expected OKAY after WRTE, got {}",
                fourcc(ack.command)
            )));
        }
        Ok(())
    }

    /// Reads the next `WRTE` payload, acknowledging it; `None` on `CLSE`.
    async fn read_next(&mut self) -> Result<Option<Vec<u8>>> {
        let m = read_msg(&mut self.rd).await?;
        match m.command {
            A_WRTE => {
                write_msg(
                    &mut self.wr,
                    &Message {
                        command: A_OKAY,
                        arg0: LOCAL_ID,
                        arg1: self.remote_id,
                        payload: Vec::new(),
                    },
                )
                .await?;
                Ok(Some(m.payload))
            }
            A_CLSE => Ok(None),
            A_OKAY => Ok(Some(Vec::new())),
            other => Err(AdbError::Protocol(format!(
                "unexpected {} on stream",
                fourcc(other)
            ))),
        }
    }

    /// Closes our end of the stream.
    async fn close(&mut self) {
        let _ = write_msg(
            &mut self.wr,
            &Message {
                command: A_CLSE,
                arg0: LOCAL_ID,
                arg1: self.remote_id,
                payload: Vec::new(),
            },
        )
        .await;
    }
}

fn msg(command: u32, arg0: u32, arg1: u32, payload: &[u8]) -> Message {
    Message {
        command,
        arg0,
        arg1,
        payload: payload.to_vec(),
    }
}

/// Finds the adb wireless **connect** port by probing localhost — mDNS-free and
/// immune to multicast noise (mDNS discovery of `_adb-tls-connect` is unreliable
/// on busy networks, but the port is a stable localhost-reachable LISTEN socket).
/// TCP-scans the ephemeral range, then confirms a candidate speaks adb by the
/// `CNXN → STLS` exchange. Returns the port, or `None` if wireless debugging is off.
pub async fn find_connect_port() -> Option<u16> {
    // The scan is a burst of blocking connects on OS threads (a closed localhost
    // port RSTs instantly). Async connects were ~35s here — per-connect epoll
    // registration serialized them; blocking threads finish in well under a second.
    let open = tokio::task::spawn_blocking(scan_localhost).await.ok()?;
    // Confirm which open port is adb's connect endpoint (responds STLS to CNXN).
    // Probe concurrently — localhost has many open ports (system services) and a
    // sequential probe would pay a read timeout for each non-adb one.
    let mut set = tokio::task::JoinSet::new();
    for p in open {
        set.spawn(async move { if speaks_adb(p).await { Some(p) } else { None } });
    }
    while let Some(res) = set.join_next().await {
        if let Ok(Some(p)) = res {
            return Some(p);
        }
    }
    None
}

/// Blocking parallel scan of the ephemeral range for open localhost ports.
fn scan_localhost() -> Vec<u16> {
    use std::net::{SocketAddr, TcpStream};
    use std::sync::mpsc;

    const LO: u16 = 32768;
    const HI: u16 = 60999; // Android's ephemeral range; observed connect ports sit here
    // Android filters (drops) SYNs to closed ports rather than RSTing, so each
    // closed port costs the full timeout — heavy parallelism is what makes this
    // bounded. Small stacks keep hundreds of threads cheap; adbd accepts on
    // localhost in well under the timeout.
    const THREADS: usize = 128;
    const TIMEOUT: Duration = Duration::from_millis(60);

    let ports: Vec<u16> = (LO..=HI).collect();
    let per = ports.len().div_ceil(THREADS);
    let (tx, rx) = mpsc::channel();
    for slice in ports.chunks(per) {
        let slice = slice.to_vec();
        let tx = tx.clone();
        // 512 KiB stack: comfortably above bionic's PTHREAD_STACK_MIN so spawns
        // actually succeed (128 KiB silently failed, collapsing the parallelism).
        let _ = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || {
                for p in slice {
                    let addr = SocketAddr::from(([127, 0, 0, 1], p));
                    if TcpStream::connect_timeout(&addr, TIMEOUT).is_ok() {
                        let _ = tx.send(p);
                    }
                }
            });
    }
    drop(tx);
    rx.iter().collect()
}

/// Whether `127.0.0.1:port` answers a `CNXN` with `STLS` (an adb wireless port).
async fn speaks_adb(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(Ok(mut tcp)) =
        tokio::time::timeout(Duration::from_millis(300), TcpStream::connect(addr)).await
    else {
        return false;
    };
    if write_msg(
        &mut tcp,
        &msg(
            A_CNXN,
            A_VERSION,
            MAX_PAYLOAD,
            b"host::features=shell_v2,cmd",
        ),
    )
    .await
    .is_err()
    {
        return false;
    }
    matches!(
        tokio::time::timeout(Duration::from_millis(300), read_msg(&mut tcp)).await,
        Ok(Ok(m)) if m.command == A_STLS,
    )
}

/// Connects, runs `shell:<command>`, and returns the raw combined output.
pub async fn run_shell(host_port: &str, key: &AdbKey, command: &str) -> Result<Vec<u8>> {
    let mut s = AdbStream::open(host_port, key, &format!("shell:{command}")).await?;
    let mut output = Vec::new();
    while let Some(chunk) = s.read_next().await? {
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

/// Pushes `data` to `remote_path` with `mode` (e.g. `0o755`) via the `sync:`
/// service, using SEND_V1 framing (`"<path>,<mode>"`). Runs on our own adb.
pub async fn push_file(
    host_port: &str,
    key: &AdbKey,
    data: &[u8],
    remote_path: &str,
    mode: u32,
    mtime: u32,
) -> Result<()> {
    let mut s = AdbStream::open(host_port, key, "sync:").await?;

    // SEND: "<path>,<mode>"
    let spec = format!("{remote_path},{mode}");
    s.write_all(&sync_frame(b"SEND", spec.as_bytes())).await?;

    // DATA chunks (<= 64 KiB each per the sync protocol), but coalesced into
    // ~MAX_PAYLOAD WRTEs to avoid a stop-and-wait round trip per 64 KiB.
    let mut batch = Vec::with_capacity(MAX_PAYLOAD as usize);
    for chunk in data.chunks(SYNC_DATA_MAX) {
        batch.extend_from_slice(&sync_frame(b"DATA", chunk));
        if batch.len() >= MAX_PAYLOAD as usize - SYNC_DATA_MAX - 8 {
            s.write_all(&batch).await?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        s.write_all(&batch).await?;
    }

    // DONE: the length field carries mtime, no payload.
    let mut done = Vec::with_capacity(8);
    done.extend_from_slice(b"DONE");
    done.extend_from_slice(&mtime.to_le_bytes());
    s.write_all(&done).await?;

    // Status: OKAY (success) or FAIL + message.
    let resp = s.read_next().await?.unwrap_or_default();
    let result = parse_sync_status(&resp);
    s.close().await;
    result
}

/// A `sync:` message: 4-byte id, u32-LE length, then `data`.
fn sync_frame(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + data.len());
    out.extend_from_slice(id);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out
}

fn parse_sync_status(resp: &[u8]) -> Result<()> {
    if resp.len() < 8 {
        return Err(AdbError::Protocol("short sync status".into()));
    }
    let id = &resp[0..4];
    let len = u32::from_le_bytes(resp[4..8].try_into().unwrap()) as usize;
    if id == b"OKAY" {
        Ok(())
    } else if id == b"FAIL" {
        let msg = String::from_utf8_lossy(&resp[8..8 + len.min(resp.len() - 8)]);
        Err(AdbError::Protocol(format!("sync push failed: {msg}")))
    } else {
        Err(AdbError::Protocol(format!(
            "unexpected sync status {}",
            String::from_utf8_lossy(id)
        )))
    }
}
