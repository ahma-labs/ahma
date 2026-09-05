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

/// A session's options reach *its* worker and no other (SPEC R-DAEMON.4).
///
/// This is the property that replaced process-wide inheritance, where the first
/// client to start the bridge configured every later one.
#[tokio::test]
async fn session_options_reach_only_the_session_that_asked() {
    use ahma_common::peer_factory::{BoxFuture, PeerFactory, PeerSpawnOptions, PeerStreams};
    use std::sync::Mutex;

    /// Records what each session's worker would have been spawned with.
    struct RecordingFactory {
        seen: Arc<Mutex<Vec<PeerSpawnOptions>>>,
    }

    impl PeerFactory for RecordingFactory {
        fn create(&self, options: PeerSpawnOptions) -> BoxFuture<anyhow::Result<PeerStreams>> {
            self.seen.lock().unwrap().push(options);
            Box::pin(async move {
                let (bridge_end, peer_end) = tokio::io::duplex(4096);
                let (bridge_read, bridge_write) = tokio::io::split(bridge_end);
                // The peer end is parked: this test is about what the worker
                // would be spawned with, not about serving a handshake.
                std::mem::forget(peer_end);
                Ok(PeerStreams {
                    stdin: Box::new(bridge_write),
                    stdout: Box::new(bridge_read),
                    stderr: None,
                    shutdown_fn: Some(Box::new(|| Box::pin(async {}))),
                    exit_cause: None,
                })
            })
        }
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let manager = SessionManager::new(SessionManagerConfig {
        server_command: "echo".to_string(),
        peer_factory: Some(Arc::new(RecordingFactory { seen: seen.clone() })),
        session_options: Some(Arc::new(|query: &str| {
            // The daemon's real translator, so this pins the whole path.
            let pairs = ahma_mcp::shell::modes::session_options::parse_session_query(query)?;
            ahma_mcp::shell::modes::session_options::session_query_to_worker_args(&pairs)
        })),
        ..Default::default()
    });

    let args_a = manager
        .worker_args_for_query("tools=simplify")
        .expect("known option");
    let id_a = manager
        .create_session_with_args(args_a)
        .await
        .expect("session A");
    let id_b = manager
        .create_session_with_args(Vec::new())
        .await
        .expect("session B");
    assert_ne!(id_a, id_b);

    let recorded = seen.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2);
    assert_eq!(
        recorded[0].extra_args,
        vec!["--tools".to_string(), "simplify".to_string()],
        "the asking session's worker gets its options"
    );
    assert!(
        recorded[1].extra_args.is_empty(),
        "and the next session inherits none of them: {:?}",
        recorded[1].extra_args
    );
    assert_eq!(
        recorded[0].session_id, id_a,
        "each worker is told which session it serves, so its hub identity is stable"
    );
}

/// A misspelled option is refused before a session exists, rather than being
/// dropped on the floor.
#[tokio::test]
async fn an_unknown_session_option_is_refused() {
    let manager = SessionManager::new(SessionManagerConfig {
        server_command: "echo".to_string(),
        session_options: Some(Arc::new(|query: &str| {
            let pairs = ahma_mcp::shell::modes::session_options::parse_session_query(query)?;
            ahma_mcp::shell::modes::session_options::session_query_to_worker_args(&pairs)
        })),
        ..Default::default()
    });
    let err = manager
        .worker_args_for_query("tolls=simplify")
        .expect_err("a typo must not be ignored");
    assert!(err.contains("tolls"), "{err}");
}
