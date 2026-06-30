//! Session registry: maps a [`SessionId`] to its (≤2) connected peers and
//! routes opaque payloads between them.
//!
//! Each connection is identified by a unique id and carries an eviction
//! [`Notify`]. When a new connection joins a role that is already occupied, the
//! incumbent is **evicted** (not the newcomer rejected) — so a peer reconnecting
//! over a half-open socket is never locked out by its own stale session. Leave
//! is id-checked, so an evicted connection's teardown never disturbs the
//! connection that replaced it.
//!
//! No `await` is ever held across a [`DashMap`] guard: methods return the
//! channel handle(s) the caller must notify after releasing the lock.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use tokio::sync::{Notify, mpsc};

use arc_proto::id::{Role, SessionId};
use arc_proto::relay::ServerMsg;

/// Bounded outbound queue depth per peer. A peer that lets this fill is treated
/// as a stalled consumer and disconnected, rather than letting the relay buffer
/// unbounded memory on its behalf.
pub const PEER_QUEUE_DEPTH: usize = 256;

/// Sending half of a peer's outbound queue.
pub type PeerTx = mpsc::Sender<ServerMsg>;

/// Receiving half, owned by the connection's writer task.
pub type PeerRx = mpsc::Receiver<ServerMsg>;

/// A registered connection occupying a role slot.
struct Peer {
    /// Unique per-connection id, for id-checked [`Hub::leave`].
    id: u64,
    /// Outbound queue to this connection's socket.
    tx: PeerTx,
    /// Fired to evict this connection when it is superseded.
    evict: Arc<Notify>,
}

#[derive(Default)]
struct Session {
    controller: Option<Peer>,
    runner: Option<Peer>,
}

impl Session {
    fn slot(&mut self, role: Role) -> &mut Option<Peer> {
        match role {
            Role::Controller => &mut self.controller,
            Role::Runner => &mut self.runner,
        }
    }

    fn peer_tx(&self, role: Role) -> Option<PeerTx> {
        match role {
            Role::Controller => self.controller.as_ref().map(|p| p.tx.clone()),
            Role::Runner => self.runner.as_ref().map(|p| p.tx.clone()),
        }
    }

    fn is_empty(&self) -> bool {
        self.controller.is_none() && self.runner.is_none()
    }
}

/// The relay's shared session table.
#[derive(Default)]
pub struct Hub {
    sessions: DashMap<SessionId, Session>,
    next_id: AtomicU64,
}

/// A connection that was evicted to make room for a newcomer.
pub struct Evicted {
    /// The evicted connection's outbound queue (to send it a final notice).
    pub tx: PeerTx,
    /// Its eviction signal, to wake its socket loop so it tears down.
    pub evict: Arc<Notify>,
}

/// Outcome of a [`Hub::join`].
pub struct Joined {
    /// Whether the opposite role was already connected at join time.
    pub peer_present: bool,
    /// If a peer was present, its queue handle so the caller can notify it of
    /// the new arrival with [`ServerMsg::PeerJoined`].
    pub peer_tx: Option<PeerTx>,
    /// An incumbent that was displaced from this role, if any.
    pub evicted: Option<Evicted>,
}

impl Hub {
    /// Creates an empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocates a unique id for a new connection.
    #[must_use]
    pub fn new_connection_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers a connection for `role`, evicting any incumbent.
    pub fn join(
        &self,
        session: SessionId,
        role: Role,
        id: u64,
        tx: PeerTx,
        evict: Arc<Notify>,
    ) -> Joined {
        let mut entry = self.sessions.entry(session).or_default();
        let displaced = entry.slot(role).replace(Peer { id, tx, evict });
        let peer_tx = entry.peer_tx(role.peer());
        Joined {
            peer_present: peer_tx.is_some(),
            peer_tx,
            evicted: displaced.map(|p| Evicted {
                tx: p.tx,
                evict: p.evict,
            }),
        }
    }

    /// Returns the opposite peer's queue handle so the caller can forward an
    /// opaque payload to it, or `None` if no peer is currently connected.
    #[must_use]
    pub fn peer_of(&self, session: &SessionId, role: Role) -> Option<PeerTx> {
        self.sessions.get(session)?.peer_tx(role.peer())
    }

    /// Removes `role` from `session` **only if** the slot still holds the
    /// connection identified by `id` (an evicted connection's teardown is thus
    /// a no-op). Returns the counterpart handle to notify with
    /// [`ServerMsg::PeerLeft`] when a removal actually happened.
    #[must_use]
    pub fn leave(&self, session: &SessionId, role: Role, id: u64) -> Option<PeerTx> {
        let (peer_tx, remove) = {
            let mut entry = self.sessions.get_mut(session)?;
            match entry.slot(role) {
                Some(peer) if peer.id == id => *entry.slot(role) = None,
                // Superseded by a newer connection (or already gone): leave it be.
                _ => return None,
            }
            (entry.peer_tx(role.peer()), entry.is_empty())
        };
        if remove {
            self.sessions
                .remove_if(session, |_, s| s.controller.is_none() && s.runner.is_none());
        }
        peer_tx
    }

    /// Number of live sessions, for metrics and tests.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(n: u8) -> SessionId {
        SessionId::from_bytes([n; 16])
    }

    fn peer() -> (PeerTx, Arc<Notify>) {
        let (tx, _rx) = mpsc::channel(PEER_QUEUE_DEPTH);
        (tx, Arc::new(Notify::new()))
    }

    #[test]
    fn join_reports_peer_presence() {
        let hub = Hub::new();
        let (c_tx, c_ev) = peer();
        let (r_tx, r_ev) = peer();

        let first = hub.join(
            sid(1),
            Role::Controller,
            hub.new_connection_id(),
            c_tx,
            c_ev,
        );
        assert!(!first.peer_present);
        assert!(first.evicted.is_none());

        let second = hub.join(sid(1), Role::Runner, hub.new_connection_id(), r_tx, r_ev);
        assert!(second.peer_present);
        assert!(second.peer_tx.is_some());
        assert!(second.evicted.is_none());
    }

    #[test]
    fn duplicate_role_evicts_incumbent() {
        let hub = Hub::new();
        let (tx1, ev1) = peer();
        let id1 = hub.new_connection_id();
        hub.join(sid(2), Role::Runner, id1, tx1, ev1);

        let (tx2, ev2) = peer();
        let id2 = hub.new_connection_id();
        let joined = hub.join(sid(2), Role::Runner, id2, tx2, ev2);
        assert!(joined.evicted.is_some(), "incumbent must be evicted");

        // The evicted (id1) tearing down must NOT remove the new (id2) slot.
        assert!(hub.leave(&sid(2), Role::Runner, id1).is_none());
        assert_eq!(hub.session_count(), 1);
        // The current holder (id2) leaving reaps the session.
        assert!(hub.leave(&sid(2), Role::Runner, id2).is_none());
        assert_eq!(hub.session_count(), 0);
    }

    #[test]
    fn leave_notifies_peer_and_reaps_empty_session() {
        let hub = Hub::new();
        let (c_tx, c_ev) = peer();
        let (r_tx, r_ev) = peer();
        let cid = hub.new_connection_id();
        let rid = hub.new_connection_id();
        hub.join(sid(3), Role::Controller, cid, c_tx, c_ev);
        hub.join(sid(3), Role::Runner, rid, r_tx, r_ev);

        // Runner leaves → controller is returned for a PeerLeft notification.
        assert!(hub.leave(&sid(3), Role::Runner, rid).is_some());
        assert_eq!(hub.session_count(), 1);
        // Controller leaves → no peer remains, session reaped.
        assert!(hub.leave(&sid(3), Role::Controller, cid).is_none());
        assert_eq!(hub.session_count(), 0);
    }
}
