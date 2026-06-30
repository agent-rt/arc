//! Per-connection state machine: handshake the [`ClientMsg::Hello`], register
//! with the [`Hub`] (evicting any stale incumbent of the same role), then pump
//! opaque payloads until the socket closes or this connection is itself evicted.
//!
//! Each connection owns a single-consumer writer task. Everything destined for
//! this socket — forwarded payloads, peer notifications, `Pong`s, errors — is
//! pushed through one [`PeerTx`], so socket writes are naturally serialized.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use tokio::sync::{Notify, mpsc};

use arc_proto::PROTOCOL_VERSION;
use arc_proto::codec;
use arc_proto::id::{Role, SessionId};
use arc_proto::relay::{ClientMsg, RelayErrorKind, ServerMsg};

use crate::hub::{Hub, PEER_QUEUE_DEPTH, PeerRx, PeerTx};

/// Drives one WebSocket connection to completion. Always deregisters from the
/// hub on exit (id-checked, so it never disturbs a connection that replaced it).
pub async fn serve(socket: WebSocket, hub: Arc<Hub>) {
    let (sink, mut stream) = socket.split();
    let (tx, rx) = mpsc::channel::<ServerMsg>(PEER_QUEUE_DEPTH);

    // Writer task: the sole owner of the socket sink.
    let writer = tokio::spawn(write_loop(sink, rx));

    // Phase 1: the first frame must be a valid Hello.
    let Some((session, role)) = handshake(&mut stream, &tx).await else {
        drop(tx);
        let _ = writer.await;
        return;
    };

    // Phase 2: register, evicting any incumbent holding this role.
    let id = hub.new_connection_id();
    let evict = Arc::new(Notify::new());
    let joined = hub.join(session, role, id, tx.clone(), evict.clone());

    if let Some(old) = joined.evicted {
        let _ = old
            .tx
            .send(reject(
                RelayErrorKind::Replaced,
                "replaced by a new connection",
            ))
            .await;
        old.evict.notify_one();
    }

    if tx
        .send(ServerMsg::Welcome {
            peer_present: joined.peer_present,
        })
        .await
        .is_err()
    {
        let _ = hub.leave(&session, role, id);
        drop(tx);
        let _ = writer.await;
        return;
    }
    if let Some(peer) = joined.peer_tx {
        let _ = peer.send(ServerMsg::PeerJoined).await;
    }
    tracing::info!(%session, ?role, sessions = hub.session_count(), "peer joined");

    // Phase 3: forward payloads until the stream ends or we are evicted.
    pump(&mut stream, &hub, &session, role, &tx, &evict).await;

    // Phase 4: teardown (no-op if we were already superseded).
    if let Some(peer) = hub.leave(&session, role, id) {
        let _ = peer.send(ServerMsg::PeerLeft).await;
    }
    drop(tx);
    let _ = writer.await;
    tracing::info!(%session, ?role, "peer left");
}

/// Reads frames and forwards `Relay` payloads to the peer; answers `Ping`.
/// Returns when the stream ends, on a protocol violation, or on eviction.
async fn pump(
    stream: &mut (impl StreamExt<Item = Result<Message, axum::Error>> + Unpin),
    hub: &Hub,
    session: &SessionId,
    role: Role,
    tx: &PeerTx,
    evict: &Notify,
) {
    loop {
        let frame = tokio::select! {
            frame = stream.next() => frame,
            () = evict.notified() => {
                tracing::info!(%session, ?role, "connection replaced; closing");
                break;
            }
        };
        let Some(frame) = frame else { break };
        let bytes = match frame {
            Ok(Message::Binary(b)) => b,
            Ok(Message::Close(_)) | Err(_) => break,
            // Ignore text/ping/pong control frames; they carry no protocol data.
            Ok(_) => continue,
        };
        let msg: ClientMsg = match codec::from_cbor(&bytes) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(%session, %e, "undecodable frame; closing");
                let _ = tx
                    .send(reject(RelayErrorKind::ProtocolViolation, "bad frame"))
                    .await;
                break;
            }
        };
        match msg {
            ClientMsg::Relay { data } => {
                if let Some(peer) = hub.peer_of(session, role) {
                    // A full peer queue means a stalled consumer: drop the link.
                    if peer.send(ServerMsg::Relay { data }).await.is_err() {
                        break;
                    }
                }
                // If no peer is connected, the payload is silently dropped; the
                // controller learns of the absence via Welcome/PeerLeft.
            }
            ClientMsg::Ping => {
                if tx.send(ServerMsg::Pong).await.is_err() {
                    break;
                }
            }
            ClientMsg::Hello { .. } => {
                tracing::warn!(%session, "duplicate Hello; closing");
                let _ = tx
                    .send(reject(RelayErrorKind::ProtocolViolation, "duplicate hello"))
                    .await;
                break;
            }
        }
    }
}

/// Awaits and validates the opening [`ClientMsg::Hello`]. On any violation it
/// pushes an error to the writer and returns `None`.
async fn handshake(
    stream: &mut (impl StreamExt<Item = Result<Message, axum::Error>> + Unpin),
    tx: &PeerTx,
) -> Option<(SessionId, Role)> {
    loop {
        match stream.next().await? {
            Ok(Message::Binary(bytes)) => match codec::from_cbor::<ClientMsg>(&bytes) {
                Ok(ClientMsg::Hello {
                    session,
                    role,
                    protocol_version,
                }) => {
                    if protocol_version != PROTOCOL_VERSION {
                        let _ = tx
                            .send(reject(
                                RelayErrorKind::VersionMismatch,
                                "unsupported version",
                            ))
                            .await;
                        return None;
                    }
                    return Some((session, role));
                }
                Ok(_) => {
                    let _ = tx
                        .send(reject(RelayErrorKind::ProtocolViolation, "expected hello"))
                        .await;
                    return None;
                }
                Err(_) => {
                    let _ = tx
                        .send(reject(RelayErrorKind::ProtocolViolation, "bad hello"))
                        .await;
                    return None;
                }
            },
            // Tolerate leading control frames before the Hello.
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {
                let _ = tx
                    .send(reject(RelayErrorKind::ProtocolViolation, "expected hello"))
                    .await;
                return None;
            }
        }
    }
}

/// Owns the socket sink and serializes all outbound frames.
async fn write_loop(mut sink: impl SinkExt<Message, Error = axum::Error> + Unpin, mut rx: PeerRx) {
    while let Some(msg) = rx.recv().await {
        let bytes = match codec::to_cbor(&msg) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(%e, "failed to encode ServerMsg; dropping connection");
                break;
            }
        };
        if sink.send(Message::Binary(bytes.into())).await.is_err() {
            break;
        }
    }
    let _ = sink.close().await;
}

fn reject(kind: RelayErrorKind, message: &str) -> ServerMsg {
    ServerMsg::Error {
        kind,
        message: message.to_owned(),
    }
}
