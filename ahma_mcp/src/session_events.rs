//! Server-side session-health event emitter (issue #485).
//!
//! Fans one event out over both wire forms defined in
//! [`ahma_common::session_event`]: the canonical
//! `notifications/ahma/session_event` plus its `notifications/message` mirror
//! for foreign clients. Owns the per-emitter monotonic `seq`.
//!
//! Emission is **best-effort and information-only**: failures (no peer yet,
//! closed transport) are logged at `debug` and never propagate — an event must
//! never fail the operation that triggered it.
//!
//! The mirror is sent as a raw [`CustomNotification`] carrying the standard
//! `notifications/message` wire shape rather than through rmcp's typed logging
//! API, which rmcp 2.0 deprecates (SEP-2577 removes logging from MCP). Clients
//! that still support logging render it today; when the mirror stops earning
//! its keep, dropping it is a one-line change here.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use ahma_common::session_event::{
    MESSAGE_METHOD, SESSION_EVENT_METHOD, SessionEventKind, event_params,
};
use rmcp::model::CustomNotification;
use rmcp::service::{Peer, RoleServer};

/// A fire-and-forget sink for session-health events, behind a trait so
/// emitters (the permission broker) can be tested without a live MCP peer —
/// the same precedent as
/// [`ElicitationSurface`](crate::sandbox::ElicitationSurface).
pub trait SessionEventSink: Send + Sync + std::fmt::Debug {
    /// Deliver one event, best-effort, without blocking the caller.
    fn emit_event(&self, kind: SessionEventKind, detail: serde_json::Value);
}

/// Emits session-health events to the connected MCP client, if any.
///
/// Shares the service's peer slot (the same pattern as
/// [`PeerElicitationSurface`](crate::sandbox::PeerElicitationSurface)): the
/// sender is built before any client connects, and starts delivering the
/// moment the handshake fills the slot.
#[derive(Debug)]
pub struct SessionEventSender {
    peer: Arc<RwLock<Option<Peer<RoleServer>>>>,
    seq: Arc<AtomicU64>,
}

impl SessionEventSender {
    /// Wrap the service's peer slot as an event sink.
    pub fn new(peer: Arc<RwLock<Option<Peer<RoleServer>>>>) -> Self {
        Self {
            peer,
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Emit one event (canonical notification + logging mirror), best-effort.
    pub async fn emit(&self, kind: SessionEventKind, detail: serde_json::Value) {
        let peer = self.peer.read().unwrap().clone();
        let Some(peer) = peer else {
            tracing::debug!(kind = kind.as_str(), "session event dropped: no MCP peer");
            return;
        };
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let now = ahma_common::keepalive::current_timestamp_ms();
        let params = event_params(kind, seq, now, detail);

        let mirror_params = serde_json::json!({
            "level": kind.mirror_level(),
            "logger": "ahma.session",
            "data": params,
        });
        for (method, p) in [
            (SESSION_EVENT_METHOD, params),
            (MESSAGE_METHOD, mirror_params),
        ] {
            if let Err(e) = peer
                .send_notification(rmcp::model::ServerNotification::CustomNotification(
                    CustomNotification::new(method, Some(p)),
                ))
                .await
            {
                tracing::debug!(
                    kind = kind.as_str(),
                    method,
                    error = ?e,
                    "session event emission failed (non-fatal)"
                );
            }
        }
    }
}

impl SessionEventSink for SessionEventSender {
    fn emit_event(&self, kind: SessionEventKind, detail: serde_json::Value) {
        let sender = SessionEventSender {
            peer: self.peer.clone(),
            seq: self.seq.clone(),
        };
        tokio::spawn(async move {
            sender.emit(kind, detail).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emit_without_peer_is_a_silent_noop() {
        let sender = SessionEventSender::new(Arc::new(RwLock::new(None)));
        // Must not panic or error — events are information-only.
        sender
            .emit(SessionEventKind::Health, serde_json::json!({}))
            .await;
    }
}
