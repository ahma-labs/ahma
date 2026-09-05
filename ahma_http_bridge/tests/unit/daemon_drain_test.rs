//! Draining: how the per-user daemon is replaced without tearing down the
//! sessions it is still serving (SPEC R-DAEMON.5).
//!
//! The old upgrade path was `POST /restart`, which terminated every session on
//! the shared bridge so that one newly-started client could have a matching
//! binary. With one daemon per user that is every attached editor, mid-command.

use ahma_http_bridge::bridge::DaemonExit;
use ahma_http_bridge::error::BridgeError;
use ahma_http_bridge::session::{SessionManager, SessionManagerConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A drained daemon refuses a *new* session rather than starting one inside a
/// process that is about to leave.
#[tokio::test]
async fn a_draining_daemon_refuses_new_sessions() {
    let exit = Arc::new(DaemonExit::new(Box::new(|_| {})));
    let mut manager = SessionManager::new(SessionManagerConfig {
        server_command: "echo".to_string(),
        ..Default::default()
    });
    manager.draining = Some(exit.draining_flag());

    exit.request_drain();

    match manager.create_session().await {
        Err(BridgeError::Draining) => {}
        Err(other) => panic!("expected Draining, got {other}"),
        Ok(id) => panic!("a draining daemon must not accept session {id}"),
    }
}

/// Draining is not stopping: the request only sets the flag, so live sessions
/// keep running and the composer decides when the process actually goes.
#[tokio::test]
async fn draining_does_not_stop_anything_by_itself() {
    let stops = Arc::new(AtomicUsize::new(0));
    let counter = stops.clone();
    let exit = DaemonExit::new(Box::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    }));

    exit.request_drain();
    assert!(exit.is_draining());
    assert_eq!(
        stops.load(Ordering::SeqCst),
        0,
        "a drain must not stop the daemon while sessions are still live"
    );

    exit.request("idle");
    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "an explicit stop reaches the composer"
    );
}

/// Without a coordinator — an operator's own `ahma serve http|unix` — nothing
/// changes: that process owns itself.
#[tokio::test]
async fn a_standalone_bridge_has_no_drain_state() {
    let manager = SessionManager::new(SessionManagerConfig {
        server_command: "echo".to_string(),
        ..Default::default()
    });
    assert!(
        manager.draining.is_none(),
        "an explicitly started bridge is not part of a daemon's drain"
    );
}
