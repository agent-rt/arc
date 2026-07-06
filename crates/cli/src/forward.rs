//! `arc forward` — a TCP proxy over the encrypted link (adb/ssh `-L` style).
//!
//! Listens on a local port; each accepted connection opens its **own** session
//! to the runner, sends [`Command::Forward`], and then pipes raw bytes both ways
//! as [`Frame::TunnelData`]. One session per connection keeps it simple and needs
//! no stream multiplexing — ideal in direct (Tailscale) mode, where the runner's
//! listener accepts each independently.

use anyhow::{Context, Result, bail};
use arc_net::{Session, SessionConfig, Transport};
use arc_proto::id::{RequestId, Role};
use arc_proto::wire::{Command, Frame, Request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Parses `<localport>:<remoteport>` or `<localport>:<remotehost>:<remoteport>`.
fn parse_spec(spec: &str) -> Result<(u16, Option<String>, u16)> {
    let parts: Vec<&str> = spec.split(':').collect();
    let bad = || {
        format!(
            "invalid forward spec `{spec}` (want <localport>:<remoteport> or <localport>:<host>:<remoteport>)"
        )
    };
    match parts.as_slice() {
        [local, remote] => Ok((
            local.parse().with_context(bad)?,
            None,
            remote.parse().with_context(bad)?,
        )),
        [local, host, remote] => Ok((
            local.parse().with_context(bad)?,
            Some((*host).to_owned()),
            remote.parse().with_context(bad)?,
        )),
        _ => bail!(bad()),
    }
}

/// Binds the local port and tunnels each accepted connection to the runner.
pub(crate) async fn run(config: &SessionConfig, spec: &str) -> Result<i32> {
    let (local, host, port) = parse_spec(spec)?;
    if matches!(config.transport, Transport::Relay { .. }) {
        eprintln!(
            "arc: warning — forward over a relay shares one session id, so concurrent \
             connections evict each other; use direct (Tailscale) mode for reliable forwarding."
        );
    }
    let addr = format!("127.0.0.1:{local}");
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let target = host.clone().unwrap_or_else(|| "127.0.0.1".to_owned());
    println!("forwarding {addr} -> runner {target}:{port}  (Ctrl+C to stop)");

    loop {
        let (socket, _peer) = listener.accept().await.context("accept")?;
        let config = config.clone();
        let host = host.clone();
        tokio::spawn(async move {
            if let Err(e) = tunnel(&config, host, port, socket).await {
                eprintln!("arc: forward connection closed: {e:#}");
            }
        });
    }
}

/// Opens one session, requests the forward, and pipes bytes until either end
/// closes.
async fn tunnel(
    config: &SessionConfig,
    host: Option<String>,
    port: u16,
    socket: TcpStream,
) -> Result<()> {
    let mut session = Session::connect(config, Role::Controller)
        .await
        .context("connecting a tunnel session")?;
    session
        .send_frame(&Frame::Request(Request {
            id: RequestId(1),
            command: Command::Forward { host, port },
        }))
        .await?;
    match session.recv_frame().await? {
        Some(Frame::Response(resp)) => {
            resp.result
                .map_err(|e| anyhow::anyhow!("runner refused forward: {}", e.message))?;
        }
        Some(_) | None => bail!("no forward acknowledgement from runner"),
    }

    let (mut writer, mut reader) = session.split();
    let (mut rd, mut wr) = socket.into_split();

    // Local socket → runner. On local EOF, tell the runner and stop this half.
    let uplink = async move {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) => {
                    let _ = writer.send_frame(&Frame::TunnelEof).await;
                    break;
                }
                Ok(n) => {
                    if writer
                        .send_frame(&Frame::TunnelData(buf[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };
    // Runner → local socket.
    let downlink = async move {
        loop {
            match reader.recv_frame().await {
                Ok(Some(Frame::TunnelData(bytes))) => {
                    if wr.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Ok(Some(Frame::TunnelEof)) | Ok(None) => break,
                Ok(Some(_)) => {}
                Err(_) => break,
            }
        }
        let _ = wr.shutdown().await;
    };

    // Whichever direction ends first tears down the other (dropping it closes
    // the socket/session halves).
    tokio::select! {
        () = uplink => {}
        () = downlink => {}
    }
    Ok(())
}
