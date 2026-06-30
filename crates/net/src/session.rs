//! The established, end-to-end-encrypted [`Session`] and the handshake that
//! builds it.
//!
//! The PAKE + Noise handshake and frame exchange are identical whatever the
//! underlying transport; only how an opaque payload is carried differs. A relay
//! connection wraps each payload in an `L1` `Relay` envelope and rides the
//! relay's matchmaking; a direct connection (controller dialing the runner's own
//! listener, e.g. over Tailscale) sends raw WebSocket binary frames.
//!
//! [`Session::split`] divides a session into an independently-owned
//! [`SessionReader`] and [`SessionWriter`] sharing the Noise [`Channel`] behind
//! a mutex, so a peer (the runner) can receive and send concurrently — serving
//! many in-flight commands without one blocking the link.

use std::sync::{Arc, Mutex};

use arc_proto::PROTOCOL_VERSION;
use arc_proto::codec;
use arc_proto::crypto::{Channel, Handshake, Pake};
use arc_proto::id::{PairingCode, Role, SessionId};
use arc_proto::relay::{ClientMsg, ServerMsg};
use arc_proto::wire::Frame;
use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, Stream, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, accept_async, connect_async};

use crate::config::{SessionConfig, Transport};
use crate::error::NetError;

/// Client-side WebSocket (we dialed out): to a relay, or directly to a runner.
type ClientWs = WebSocketStream<MaybeTlsStream<TcpStream>>;
/// Server-side WebSocket (we accepted): a runner's direct listener.
type ServerWs = WebSocketStream<TcpStream>;

/// The send half of the transport: how an opaque payload is framed outbound.
enum SinkHalf {
    /// Relay: wrap in `ClientMsg::Relay`.
    Relay(SplitSink<ClientWs, Message>),
    /// Direct, dialed: raw binary.
    RawClient(SplitSink<ClientWs, Message>),
    /// Direct, accepted: raw binary.
    RawServer(SplitSink<ServerWs, Message>),
}

/// The receive half of the transport: how an opaque payload is recovered.
enum StreamHalf {
    /// Relay: unwrap `ServerMsg::Relay`, skipping lifecycle frames.
    Relay(SplitStream<ClientWs>),
    /// Direct, dialed: raw binary.
    RawClient(SplitStream<ClientWs>),
    /// Direct, accepted: raw binary.
    RawServer(SplitStream<ServerWs>),
}

impl SinkHalf {
    /// Sends one opaque payload to the peer.
    async fn send_payload(&mut self, data: Vec<u8>) -> Result<(), NetError> {
        match self {
            SinkHalf::Relay(sink) => send_client(sink, &ClientMsg::Relay { data }).await,
            SinkHalf::RawClient(sink) => {
                sink.send(Message::Binary(data)).await?;
                Ok(())
            }
            SinkHalf::RawServer(sink) => {
                sink.send(Message::Binary(data)).await?;
                Ok(())
            }
        }
    }
}

impl StreamHalf {
    /// Receives the next opaque payload, or `None` if the peer left / closed.
    async fn recv_payload(&mut self) -> Result<Option<Vec<u8>>, NetError> {
        match self {
            StreamHalf::Relay(stream) => recv_relay_payload(stream).await,
            StreamHalf::RawClient(stream) => recv_direct_payload(stream).await,
            StreamHalf::RawServer(stream) => recv_direct_payload(stream).await,
        }
    }
}

/// A live, encrypted link to the peer. Exchanges whole L2 [`Frame`]s; chunking
/// and Noise sealing are handled internally.
pub struct Session {
    sink: SinkHalf,
    stream: StreamHalf,
    channel: Channel,
}

impl Session {
    /// Connects to the peer per `config.transport` and completes the Noise
    /// handshake (initiator if [`Role::Controller`], responder if
    /// [`Role::Runner`]). In direct mode only the controller dials; the runner
    /// accepts connections via [`Session::accept_direct`].
    ///
    /// # Errors
    /// Returns [`NetError`] on transport, relay-rejection or crypto failure.
    pub async fn connect(config: &SessionConfig, role: Role) -> Result<Self, NetError> {
        let (sink, stream) = match &config.transport {
            Transport::Relay { url } => {
                let (sink, stream) = join_relay(url, config.session, role).await?;
                (SinkHalf::Relay(sink), StreamHalf::Relay(stream))
            }
            Transport::Direct { addr } => {
                let (ws, _response) = connect_async(&format!("ws://{addr}/")).await?;
                let (sink, stream) = ws.split();
                (SinkHalf::RawClient(sink), StreamHalf::RawClient(stream))
            }
        };
        Self::handshake(sink, stream, &config.pairing, role).await
    }

    /// Accepts an inbound direct connection and completes the Noise handshake as
    /// the responder. Used by the runner's listen mode.
    ///
    /// # Errors
    /// Returns [`NetError`] on WebSocket-upgrade or crypto failure.
    pub async fn accept_direct(stream: TcpStream, pairing: &PairingCode) -> Result<Self, NetError> {
        let ws = accept_async(stream).await?;
        let (sink, stream) = ws.split();
        Self::handshake(
            SinkHalf::RawServer(sink),
            StreamHalf::RawServer(stream),
            pairing,
            Role::Runner,
        )
        .await
    }

    /// Runs the symmetric PAKE then the role-appropriate Noise handshake.
    async fn handshake(
        mut sink: SinkHalf,
        mut stream: StreamHalf,
        pairing: &PairingCode,
        role: Role,
    ) -> Result<Self, NetError> {
        // Symmetric SPAKE2: one message each way, deriving a high-entropy PSK
        // from the low-entropy pairing code.
        let (pake, our_message) = Pake::start(pairing);
        sink.send_payload(our_message).await?;
        let peer_message = stream.recv_payload().await?.ok_or(NetError::Closed)?;
        let psk = pake.finish(&peer_message)?;

        let channel = match role {
            Role::Controller => {
                let mut handshake = Handshake::initiator(&psk)?;
                sink.send_payload(handshake.write()?).await?;
                let message = stream.recv_payload().await?.ok_or(NetError::Closed)?;
                handshake.read(&message)?;
                handshake.finish()?
            }
            Role::Runner => {
                let mut handshake = Handshake::responder(&psk)?;
                let message = stream.recv_payload().await?.ok_or(NetError::Closed)?;
                handshake.read(&message)?;
                sink.send_payload(handshake.write()?).await?;
                handshake.finish()?
            }
        };

        Ok(Self {
            sink,
            stream,
            channel,
        })
    }

    /// Seals and sends one L2 [`Frame`], splitting it across as many payloads as
    /// the Noise record size requires.
    ///
    /// # Errors
    /// Returns [`NetError`] on transport, crypto or encode failure.
    pub async fn send_frame(&mut self, frame: &Frame) -> Result<(), NetError> {
        for record in seal_frame(&mut self.channel, frame)? {
            self.sink.send_payload(record).await?;
        }
        Ok(())
    }

    /// Receives the next decrypted L2 [`Frame`], or `Ok(None)` if the peer left
    /// or the link closed.
    ///
    /// # Errors
    /// Returns [`NetError`] on transport, crypto or decode failure.
    pub async fn recv_frame(&mut self) -> Result<Option<Frame>, NetError> {
        loop {
            match self.stream.recv_payload().await? {
                Some(data) => {
                    if let Some(frame) = open_frame(&mut self.channel, &data)? {
                        return Ok(Some(frame));
                    }
                }
                None => return Ok(None),
            }
        }
    }

    /// Splits into independently-owned read/write halves sharing the Noise
    /// channel, so receiving and sending can proceed concurrently.
    #[must_use]
    pub fn split(self) -> (SessionWriter, SessionReader) {
        let channel = Arc::new(Mutex::new(self.channel));
        (
            SessionWriter {
                sink: self.sink,
                channel: Arc::clone(&channel),
            },
            SessionReader {
                stream: self.stream,
                channel,
            },
        )
    }
}

/// The send half of a split [`Session`]. All sends should funnel through one
/// writer so Noise records stay ordered and contiguous on the wire.
pub struct SessionWriter {
    sink: SinkHalf,
    channel: Arc<Mutex<Channel>>,
}

impl SessionWriter {
    /// Seals and sends one L2 [`Frame`].
    ///
    /// # Errors
    /// Returns [`NetError`] on transport, crypto or encode failure.
    pub async fn send_frame(&mut self, frame: &Frame) -> Result<(), NetError> {
        // Seal under the lock (synchronous), then release before awaiting I/O.
        let records = {
            let mut channel = self.channel.lock().map_err(|_| NetError::Closed)?;
            seal_frame(&mut channel, frame)?
        };
        for record in records {
            self.sink.send_payload(record).await?;
        }
        Ok(())
    }
}

/// The receive half of a split [`Session`].
pub struct SessionReader {
    stream: StreamHalf,
    channel: Arc<Mutex<Channel>>,
}

impl SessionReader {
    /// Receives the next decrypted L2 [`Frame`], or `Ok(None)` on close.
    ///
    /// # Errors
    /// Returns [`NetError`] on transport, crypto or decode failure.
    pub async fn recv_frame(&mut self) -> Result<Option<Frame>, NetError> {
        loop {
            match self.stream.recv_payload().await? {
                Some(data) => {
                    let frame = {
                        let mut channel = self.channel.lock().map_err(|_| NetError::Closed)?;
                        open_frame(&mut channel, &data)?
                    };
                    if let Some(frame) = frame {
                        return Ok(Some(frame));
                    }
                }
                None => return Ok(None),
            }
        }
    }
}

/// Seals a frame to its (possibly chunked) Noise records.
fn seal_frame(channel: &mut Channel, frame: &Frame) -> Result<Vec<Vec<u8>>, NetError> {
    let bytes = codec::to_cbor(frame)?;
    Ok(channel.seal(&bytes)?)
}

/// Feeds one record into the channel; yields a frame once a message completes.
fn open_frame(channel: &mut Channel, data: &[u8]) -> Result<Option<Frame>, NetError> {
    match channel.open(data)? {
        Some(message) => Ok(Some(codec::from_cbor(&message)?)),
        None => Ok(None),
    }
}

/// Dials the relay, joins `session` in `role`, and waits for the peer so the
/// first PAKE payload is not dropped.
async fn join_relay(
    url: &str,
    session: SessionId,
    role: Role,
) -> Result<(SplitSink<ClientWs, Message>, SplitStream<ClientWs>), NetError> {
    let (ws, _response) = connect_async(url).await?;
    let (mut sink, mut stream) = ws.split();

    send_client(
        &mut sink,
        &ClientMsg::Hello {
            session,
            role,
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await?;

    let peer_present = match recv_server(&mut stream).await? {
        Some(ServerMsg::Welcome { peer_present }) => peer_present,
        Some(ServerMsg::Error { message, .. }) => return Err(NetError::Relay(message)),
        Some(_) => return Err(NetError::Relay("expected welcome".into())),
        None => return Err(NetError::Closed),
    };
    if !peer_present {
        wait_for_peer(&mut stream).await?;
    }
    Ok((sink, stream))
}

/// Next relay payload, skipping control/lifecycle frames; `None` on peer-left.
async fn recv_relay_payload(
    stream: &mut SplitStream<ClientWs>,
) -> Result<Option<Vec<u8>>, NetError> {
    loop {
        match recv_server(stream).await? {
            Some(ServerMsg::Relay { data }) => return Ok(Some(data)),
            Some(ServerMsg::PeerLeft) | None => return Ok(None),
            Some(ServerMsg::Error { message, .. }) => return Err(NetError::Relay(message)),
            Some(ServerMsg::Welcome { .. } | ServerMsg::PeerJoined | ServerMsg::Pong) => continue,
        }
    }
}

/// Next raw binary frame from a direct connection; `None` on close.
async fn recv_direct_payload<S>(stream: &mut SplitStream<S>) -> Result<Option<Vec<u8>>, NetError>
where
    S: Stream<Item = Result<Message, WsError>> + Unpin,
{
    while let Some(frame) = stream.next().await {
        match frame? {
            Message::Binary(data) => return Ok(Some(data)),
            Message::Close(_) => return Ok(None),
            _ => continue,
        }
    }
    Ok(None)
}

async fn wait_for_peer(stream: &mut SplitStream<ClientWs>) -> Result<(), NetError> {
    loop {
        match recv_server(stream).await? {
            Some(ServerMsg::PeerJoined | ServerMsg::Welcome { peer_present: true }) => {
                return Ok(());
            }
            Some(ServerMsg::Welcome {
                peer_present: false,
            }) => continue,
            Some(ServerMsg::Error { message, .. }) => return Err(NetError::Relay(message)),
            Some(_) => continue,
            None => return Err(NetError::Closed),
        }
    }
}

/// Encodes and sends one L1 client message.
async fn send_client(
    sink: &mut SplitSink<ClientWs, Message>,
    msg: &ClientMsg,
) -> Result<(), NetError> {
    let bytes = codec::to_cbor(msg)?;
    sink.send(Message::Binary(bytes)).await?;
    Ok(())
}

/// Receives the next L1 server message, skipping WebSocket control frames.
async fn recv_server(stream: &mut SplitStream<ClientWs>) -> Result<Option<ServerMsg>, NetError> {
    while let Some(frame) = stream.next().await {
        match frame? {
            Message::Binary(data) => return Ok(Some(codec::from_cbor(&data)?)),
            Message::Close(_) => return Ok(None),
            _ => continue,
        }
    }
    Ok(None)
}
