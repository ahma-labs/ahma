//! Draining: how the per-user hub is replaced without tearing down the
//! sessions it is still serving (SPEC R-HUB.5).
//!
//! The old upgrade path was `POST /restart`, which terminated every session on
//! the shared bridge so that one newly-started client could have a matching
//! binary. With one hub per user that is every attached editor, mid-command.

use ahma_http_bridge::bridge::HubExit;
use ahma_http_bridge::session::{SessionManager, SessionManagerConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A worker that never answers, in memory on every OS: what these tests need
/// is sessions and requests that stay open, not a process.
struct ParkedPeer;

impl ahma_common::peer_factory::PeerFactory for ParkedPeer {
    fn create(
        &self,
        _options: ahma_common::peer_factory::PeerSpawnOptions,
    ) -> ahma_common::peer_factory::BoxFuture<anyhow::Result<ahma_common::peer_factory::PeerStreams>>
    {
        Box::pin(async move {
            let (bridge_end, peer_end) = tokio::io::duplex(64 * 1024);
            let (bridge_read, bridge_write) = tokio::io::split(bridge_end);
            std::mem::forget(peer_end);
            Ok(ahma_common::peer_factory::PeerStreams {
                stdin: Box::new(bridge_write),
                stdout: Box::new(bridge_read),
                stderr: None,
                shutdown_fn: Some(Box::new(|| Box::pin(async {}))),
                exit_cause: None,
            })
        })
    }
}

fn parked_manager() -> Arc<SessionManager> {
    Arc::new(SessionManager::new(SessionManagerConfig {
        server_command: "unused".to_string(),
        peer_factory: Some(Arc::new(ParkedPeer)),
        ..Default::default()
    }))
}

/// A draining hub keeps serving, new sessions included. Refusing them made a
/// drain an outage: every window opened while an hour-long build finished
/// elsewhere got `503` until it did.
#[tokio::test]
async fn a_draining_hub_keeps_accepting_sessions() {
    let exit = Arc::new(HubExit::new(Box::new(|_| {})));
    let manager = parked_manager();
    exit.attach_sessions(&manager);

    exit.request_drain();

    manager
        .create_session()
        .await
        .expect("a draining hub still accepts a session");
}

/// Draining is not stopping: the request only sets the flag, so live sessions
/// keep running and the composer decides when the process actually goes.
#[tokio::test]
async fn draining_does_not_stop_anything_by_itself() {
    let stops = Arc::new(AtomicUsize::new(0));
    let counter = stops.clone();
    let exit = HubExit::new(Box::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    }));

    exit.request_drain();
    assert!(exit.is_draining());
    assert_eq!(
        stops.load(Ordering::SeqCst),
        0,
        "a drain must not stop the hub while sessions are still live"
    );

    exit.request("idle");
    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "an explicit stop reaches the composer"
    );
}

/// The hub sees the requests its bridge is still answering, because a
/// request in flight is work that would be lost if it went now.
#[tokio::test]
async fn the_hub_sees_requests_in_flight() {
    let exit = HubExit::new(Box::new(|_| {}));
    assert_eq!(exit.requests_in_flight(), 0, "no bridge attached yet");

    let manager = parked_manager();
    exit.attach_sessions(&manager);
    let id = manager.create_session().await.expect("session");
    assert_eq!(exit.requests_in_flight(), 0);

    // The parked worker never answers, so the request stays pending.
    let request = manager
        .start_request(
            &id,
            &serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}),
        )
        .await
        .expect("request sent");
    assert_eq!(exit.requests_in_flight(), 1);
    drop(request);
    manager
        .terminate_session(
            &id,
            ahma_http_bridge::session::SessionTerminationReason::ClientRequested,
        )
        .await
        .expect("terminate");
    assert_eq!(exit.requests_in_flight(), 0, "an ended session has none");
}

/// At the drain cap the hub ends what is left (SPEC R-HUB.5): every session
/// is terminated, which answers each request still in flight with an error
/// rather than leaving it hanging on a process about to exit.
#[tokio::test]
async fn ending_the_sessions_leaves_none() {
    let exit = HubExit::new(Box::new(|_| {}));
    let manager = parked_manager();
    exit.attach_sessions(&manager);
    manager.create_session().await.expect("session");
    manager.create_session().await.expect("session");
    assert_eq!(manager.session_count(), 2);

    exit.end_sessions().await;
    assert_eq!(manager.session_count(), 0);
}

/// A session's options reach *its* worker and no other (SPEC R-HUB.4).
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
            // The hub's real translator, so this pins the whole path.
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
