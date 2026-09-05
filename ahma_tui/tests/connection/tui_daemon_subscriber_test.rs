//! The TUI is a subscriber, never the hub, and never a server (SPEC R-DAEMON.9).
//!
//! Two behaviours used to make one terminal window quietly decide things for
//! every other client: the TUI bound the hub socket when it started first, so
//! quitting it took the event stream away from attached editors; and it started
//! a server scoped to its own launch directory, so every editor session that
//! arrived afterwards locked to whichever folder that terminal happened to be
//! in.

use ahma_common::daemon_hub::{ClientMsg, DaemonMsg, HubServer, recv_msg, send_msg};
use ahma_common::timeouts::TestTimeouts;
use ahma_tui::daemon_source::spawn_daemon_source;
use ahma_tui::mcp_source::SourceEvent;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// The isolated hub socket for this test process.
fn isolated_socket() -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "ahma_tui_sub_{}_{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::set_var("AHMA_DAEMON_SOCK", &path) };
    let _ = std::fs::remove_file(&path);
    path
}

async fn next_event(rx: &mut mpsc::Receiver<SourceEvent>) -> SourceEvent {
    tokio::time::timeout(TestTimeouts::scale_secs(5), rx.recv())
        .await
        .expect("a source event should arrive")
        .expect("the source channel stays open")
}

/// An instance that has since disconnected still reaches the TUI with its work.
///
/// This is what makes a hooked command visible: it is an instance for the
/// length of one command, so by the time anyone looks at the TUI it has always
/// already gone (SPEC R-DAEMON.7, R-DAEMON.8).
#[tokio::test]
async fn a_departed_instance_still_reaches_the_tui_with_its_work() {
    let socket = isolated_socket();
    let hub = HubServer::bind_at(socket.clone())
        .await
        .expect("this test owns a freshly isolated socket");
    let hub_task = tokio::spawn(hub.serve());

    // A hook registers, runs one command, and exits — all before the TUI looks.
    {
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (r, mut w) = tokio::io::split(stream);
        let _reader = BufReader::new(r);
        send_msg(
            &mut w,
            &ClientMsg::Register {
                pid: 4242,
                mode: "hook".to_string(),
                scope: "/work/project".to_string(),
                label: "hook".to_string(),
                client: Some("claude-code".to_string()),
                session_id: Some("hook-1".to_string()),
                client_pid: None,
            },
        )
        .await
        .unwrap();
        send_msg(
            &mut w,
            &ClientMsg::Event {
                payload: ahma_common::daemon_hub::DaemonEvent::OpStarted {
                    id: "op-1".to_string(),
                    tool_name: "run_terminal_command".to_string(),
                    description: "lint".to_string(),
                    scope: "/work/project".to_string(),
                    parent_id: None,
                    started_epoch_ms: None,
                    title: Some("pre-commit lint".to_string()),
                    cwd: Some("/work/project".to_string()),
                    command: Some("pre-commit lint".to_string()),
                    origin: Some("hook".to_string()),
                    partial: false,
                    unsandboxed: false,
                },
            },
        )
        .await
        .unwrap();
        send_msg(
            &mut w,
            &ClientMsg::Event {
                payload: ahma_common::daemon_hub::DaemonEvent::OpFinished {
                    id: "op-1".to_string(),
                    status: ahma_common::daemon_hub::OpStatus::Completed,
                    result_summary: Some("ok".to_string()),
                    duration_ms: 5,
                    ended_epoch_ms: None,
                    exit_code: Some(0),
                    denial: None,
                    interrupted: false,
                },
            },
        )
        .await
        .unwrap();
        // Dropping both halves ends the hook's registration.
    }

    // Give the hub a beat to process the disconnect.
    tokio::time::sleep(TestTimeouts::short_delay()).await;

    let (tx, mut rx) = mpsc::channel::<SourceEvent>(64);
    spawn_daemon_source(tx);

    let mut instances_seen = Vec::new();
    let mut ops_seen = Vec::new();
    for _ in 0..6 {
        match next_event(&mut rx).await {
            SourceEvent::InstancesUpdated { instances } => instances_seen = instances,
            SourceEvent::OperationsUpdated { ops } => ops_seen = ops,
            _ => {}
        }
        if !instances_seen.is_empty() && !ops_seen.is_empty() {
            break;
        }
    }

    assert_eq!(
        instances_seen.len(),
        1,
        "the departed hook is still listed so its work has somewhere to belong: {instances_seen:?}"
    );
    assert!(
        instances_seen[0].ended_epoch_ms.is_some(),
        "and is marked historic rather than attached"
    );
    assert!(
        ops_seen.iter().any(|o| o.id == "op-1"),
        "its operation replays to a TUI that attached afterwards: {ops_seen:?}"
    );

    hub_task.abort();
    let _ = std::fs::remove_file(&socket);
}

/// Starting the TUI must not bind the hub: the daemon owns it, and a TUI that
/// took it would take the event stream away from every editor when it quit.
///
/// The hub is bound **first** here, and deliberately so. A source started with
/// no daemon running tries to start one, and from a test binary that means
/// `current_exe daemon` — the test harness itself, re-run with `daemon` as a
/// filter that matches these very tests. That is a fork bomb, and it emptied
/// this machine's process table once already; `spawn_detached_daemon` now
/// refuses under a test harness, and this test does not go looking for the
/// refusal.
#[tokio::test]
async fn the_tui_source_never_binds_the_hub() {
    let socket = isolated_socket();

    // The daemon's hub, bound before the TUI exists.
    let hub = HubServer::bind_at(socket.clone())
        .await
        .expect("this test owns a freshly isolated socket");
    let count = hub.connection_count();
    let task = tokio::spawn(hub.serve());

    let (tx, _rx) = mpsc::channel::<SourceEvent>(8);
    spawn_daemon_source(tx);

    // The TUI attaches as an ordinary subscriber...
    let subscribed = tokio::time::timeout(TestTimeouts::scale_secs(10), async {
        loop {
            if count.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                return true;
            }
            tokio::time::sleep(TestTimeouts::poll_interval()).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(subscribed, "the TUI must subscribe to the daemon's hub");

    // ...and the socket still belongs to the hub that bound it: a second bind
    // is refused as AlreadyRunning, which it would not be had the TUI taken it
    // over.
    match HubServer::bind_at(socket.clone()).await {
        Err(ahma_common::daemon_hub::HubBindError::AlreadyRunning) => {}
        Err(e) => panic!("expected AlreadyRunning, got {e}"),
        Ok(_) => panic!("the hub socket must still be owned by the daemon's hub"),
    }

    task.abort();
    let _ = std::fs::remove_file(&socket);
}

/// A one-shot query answers on the socket, which is what `ahma daemon`'s
/// callers use to tell a live rendezvous from a stale file.
#[tokio::test]
async fn list_instances_answers_on_the_hub_socket() {
    let socket = isolated_socket();
    let hub = HubServer::bind_at(socket.clone()).await.expect("bind");
    let task = tokio::spawn(hub.serve());

    let listed = ahma_common::daemon_hub::list_instances_at(&socket)
        .await
        .expect("the hub answers a one-shot query");
    assert!(listed.is_empty(), "nothing has registered yet");

    // And an unknown message does not kill the connection (R24.5).
    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);
    w.write_all(b"{\"type\":\"FromTheFuture\"}\n")
        .await
        .unwrap();
    send_msg(&mut w, &ClientMsg::ListInstances).await.unwrap();
    let msg = tokio::time::timeout(
        TestTimeouts::scale_secs(5),
        recv_msg::<_, DaemonMsg>(&mut reader),
    )
    .await
    .expect("the hub must answer after skipping what it did not understand")
    .expect("connection stays open");
    assert!(matches!(msg, DaemonMsg::InstanceList { .. }));

    task.abort();
    let _ = std::fs::remove_file(&socket);
}
