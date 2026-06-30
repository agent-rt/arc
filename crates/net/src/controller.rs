//! Controller-side helper over a [`Session`]: turns a [`Command`] into its
//! [`Reply`] with request/response correlation, optionally forwarding interim
//! [`Event`]s. Shared by the MCP server and the `arc` CLI.

use arc_proto::id::{RequestId, Role};
use arc_proto::wire::{Command, Event, Frame, RemoteError, Reply, Request};
use tokio::sync::mpsc;

use crate::{NetError, Session, SessionConfig};

/// Error from a controller request: a fatal link/transport failure, or a
/// structured failure the runner returned for the command (link stays usable).
#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    /// Link/transport failure; re-establish the connection.
    #[error(transparent)]
    Net(#[from] NetError),

    /// The runner ran the command but returned a structured failure.
    #[error("runner error [{:?}]: {}", .0.kind, .0.message)]
    Remote(RemoteError),
}

impl ControllerError {
    /// Whether this error invalidates the link (so the caller should reconnect).
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        !matches!(self, Self::Remote(_))
    }
}

/// A connected controller session to the runner.
pub struct Controller {
    session: Session,
    next_id: u64,
}

impl Controller {
    /// Establishes the link as the Noise initiator.
    ///
    /// # Errors
    /// Returns [`ControllerError`] on transport or crypto failure.
    pub async fn connect(config: &SessionConfig) -> Result<Self, ControllerError> {
        let session = Session::connect(config, Role::Controller).await?;
        Ok(Self {
            session,
            next_id: 0,
        })
    }

    /// Sends a [`Command`] and awaits its correlated [`Reply`].
    ///
    /// # Errors
    /// [`ControllerError::Remote`] for a runner failure, else
    /// [`ControllerError::Net`] for a fatal transport error.
    pub async fn request(&mut self, command: Command) -> Result<Reply, ControllerError> {
        self.dispatch(command, None).await
    }

    /// Like [`request`](Self::request) but forwards each interim [`Event`] to
    /// `events` as it arrives (for live output streaming).
    ///
    /// # Errors
    /// As [`request`](Self::request).
    pub async fn request_streaming(
        &mut self,
        command: Command,
        events: &mpsc::Sender<Event>,
    ) -> Result<Reply, ControllerError> {
        self.dispatch(command, Some(events)).await
    }

    async fn dispatch(
        &mut self,
        command: Command,
        events: Option<&mpsc::Sender<Event>>,
    ) -> Result<Reply, ControllerError> {
        self.next_id += 1;
        let id = RequestId(self.next_id);
        self.session
            .send_frame(&Frame::Request(Request { id, command }))
            .await?;

        loop {
            match self.session.recv_frame().await? {
                Some(Frame::Response(response)) if response.id == id => {
                    return response.result.map_err(ControllerError::Remote);
                }
                Some(Frame::Event(event)) => match events {
                    Some(sink) => {
                        // Receiver gone just means nobody consumes progress.
                        let _ = sink.send(event).await;
                    }
                    None => tracing::debug!(?event, "interim event"),
                },
                // A stray frame for another id; keep reading.
                Some(_) => continue,
                None => return Err(ControllerError::Net(NetError::Closed)),
            }
        }
    }
}
