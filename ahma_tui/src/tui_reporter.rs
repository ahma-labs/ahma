//! The TUI's own reporter connection (SPEC R-DAEMON.9, R24.9).
//!
//! The TUI is a subscriber first — it watches everyone else's work — but it is
//! also a place work *happens*: a `!` command typed into the chat input runs
//! right here, outside the sandbox, at the user's full privilege. Until this
//! module existed those commands were the one kind of work the unified view
//! could not see. They appeared in the local group of whichever TUI ran them
//! and nowhere else: not in a second TUI, not in the history file, not after a
//! restart.
//!
//! So the TUI registers a second connection of its own, as an instance with
//! `mode: "tui"`, and reports its `!` commands through it like any other
//! client. What comes back the other way, through the subscriber, is the same
//! row every other client's work produces — which is the point: one view, one
//! vocabulary, no privileged local case.
//!
//! Two things this deliberately does **not** do:
//!
//! * **Start a daemon.** The subscriber already ensures one; a reporter that
//!   raced it would spawn a second process for the sake of bookkeeping.
//! * **Block the TUI.** `report` never awaits and never fails loudly. If the
//!   daemon is down, the command still runs and the local window still shows
//!   its output — the report is what is lost, not the work.

use std::time::Duration;

use ahma_common::daemon_hub::{ClientMsg, DaemonEvent, connect_to_daemon, send_msg};
use tokio::sync::mpsc;
use tracing::debug;

/// How many events may queue while the daemon is unreachable.
///
/// Bounded because the alternative is a TUI whose memory grows for as long as
/// the daemon is down. A `!` command that overruns this loses report lines, not
/// output: the window the user is watching is fed separately.
const REPORT_QUEUE: usize = 512;

/// Reconnect delay after the connection drops, and its ceiling.
const RETRY_START: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// Where the TUI sends its own work.
#[derive(Debug, Clone)]
pub struct TuiReporter {
    tx: mpsc::Sender<DaemonEvent>,
    /// This TUI's session id, stable for the life of the process, so the hub
    /// keeps one instance identity across reconnects (SPEC R-DAEMON.6).
    session_id: String,
}

impl TuiReporter {
    /// The session id this TUI registers under.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Send one event, or drop it.
    ///
    /// Deliberately infallible and synchronous: every call site is on the UI
    /// path, where an await would stall a keystroke and an error would be a
    /// dialog about telemetry.
    pub fn report(&self, event: DaemonEvent) {
        if self.tx.try_send(event).is_err() {
            debug!("tui_reporter: dropping an event (queue full or reporter stopped)");
        }
    }
}

/// The identity the TUI registers under. Split out so the register message and
/// the tests agree on it by construction rather than by transcription.
pub fn register_msg(session_id: &str, workspace: &str) -> ClientMsg {
    ClientMsg::Register {
        pid: std::process::id(),
        mode: "tui".to_string(),
        scope: workspace.to_string(),
        label: "ahma-tui".to_string(),
        client: Some("ahma-tui".to_string()),
        session_id: Some(session_id.to_string()),
        // The TUI is its own client: the pid that asked and the pid that ran
        // are the same one, and that is what makes its section read as "this
        // terminal (you)" rather than as some other window's work.
        client_pid: Some(std::process::id()),
    }
}

/// Build the `OpStarted` for a `!` command.
///
/// `unsandboxed` is not a parameter: this constructor exists for exactly one
/// kind of work, and a caller that could pass `false` would be a caller that
/// could mislabel an unconfined command as a confined one.
pub fn bang_started(op_id: &str, command: &str, cwd: &str) -> DaemonEvent {
    DaemonEvent::OpStarted {
        id: op_id.to_string(),
        tool_name: "shell".to_string(),
        description: format!("Execute {command} in {cwd}"),
        scope: cwd.to_string(),
        parent_id: None,
        started_epoch_ms: now_ms(),
        title: Some(command.to_string()),
        cwd: Some(cwd.to_string()),
        command: Some(command.to_string()),
        origin: Some("tui".to_string()),
        partial: false,
        unsandboxed: true,
    }
}

/// The operation id the next `!` command reports under.
///
/// Namespaced by the TUI's session so two TUIs cannot collide on the hub, and
/// counted rather than keyed by window id: window ids wrap at 100 and a re-run
/// reuses one, so two different commands would otherwise arrive as one
/// operation restarting.
pub fn next_bang_op_id(session_id: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    format!("tui_{session_id}_{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

fn now_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Start the reporter connection. Returns immediately.
pub fn spawn_tui_reporter(workspace: String) -> TuiReporter {
    let session_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::channel::<DaemonEvent>(REPORT_QUEUE);
    let handle = TuiReporter {
        tx,
        session_id: session_id.clone(),
    };
    tokio::spawn(reporter_task(session_id, workspace, rx));
    handle
}

/// Connect, register, forward; on any failure, back off and start again.
///
/// Events raised while disconnected wait in the channel and go out on the next
/// connection — which is what makes a `!` command run before the daemon came up
/// still show its finish.
async fn reporter_task(session_id: String, workspace: String, mut rx: mpsc::Receiver<DaemonEvent>) {
    let mut backoff = RETRY_START;
    loop {
        match connect_to_daemon().await {
            Ok(stream) => {
                backoff = RETRY_START;
                let (_read, mut writer) = tokio::io::split(stream);
                if send_msg(&mut writer, &register_msg(&session_id, &workspace))
                    .await
                    .is_err()
                {
                    debug!("tui_reporter: register failed; retrying");
                } else {
                    debug!("tui_reporter: registered as mode=tui");
                    while let Some(event) = rx.recv().await {
                        if send_msg(&mut writer, &ClientMsg::Event { payload: event })
                            .await
                            .is_err()
                        {
                            debug!("tui_reporter: write failed; reconnecting");
                            break;
                        }
                    }
                    // The channel closed: the TUI is going away, and so is this.
                    if rx.is_closed() && rx.is_empty() {
                        return;
                    }
                }
            }
            Err(e) => debug!("tui_reporter: connect failed ({e}); retry in {backoff:?}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RETRY_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahma_common::daemon_hub::{DaemonMsg, HubServer, recv_msg};
    use ahma_common::timeouts::TestTimeouts;
    use tokio::io::BufReader;

    /// Point the whole process at a socket only this test uses (SPEC R-ISO.1).
    fn isolate() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_DAEMON_SOCK", dir.path().join("d.sock")) };
        if let Ok(l) = std::net::TcpListener::bind("127.0.0.1:0")
            && let Ok(addr) = l.local_addr()
        {
            // SAFETY: as above.
            unsafe { std::env::set_var("AHMA_DAEMON_PORT", addr.port().to_string()) };
        }
        dir
    }

    /// A `!` command must arrive at the hub as an ordinary instance's ordinary
    /// work — and say that it ran unconfined.
    #[tokio::test]
    async fn a_bang_command_reaches_the_hub_as_an_unsandboxed_tui_instance() {
        let _guard = isolate();
        let hub = HubServer::bind_at(ahma_common::daemon_hub::default_socket_path())
            .await
            .expect("this test owns a freshly isolated socket");
        let hub_task = tokio::spawn(hub.serve());

        // A subscriber standing in for a second TUI watching this one.
        let sub = connect_to_daemon().await.expect("subscriber connect");
        let (sr, mut sw) = tokio::io::split(sub);
        let mut sub_reader = BufReader::new(sr);
        send_msg(&mut sw, &ClientMsg::Subscribe).await.unwrap();
        // The initial snapshot.
        let _ = recv_msg::<_, DaemonMsg>(&mut sub_reader).await.unwrap();

        let reporter = spawn_tui_reporter("/work/project".to_string());
        let op = next_bang_op_id(reporter.session_id());
        reporter.report(bang_started(&op, "rm -rf build", "/work/project"));
        reporter.report(DaemonEvent::OpOutput {
            id: op.clone(),
            line: "removed".to_string(),
            is_stderr: false,
        });
        reporter.report(DaemonEvent::OpFinished {
            id: op.clone(),
            status: ahma_common::daemon_hub::OpStatus::Completed,
            result_summary: Some("Completed".to_string()),
            duration_ms: 12,
            ended_epoch_ms: None,
            exit_code: Some(0),
            denial: None,
            interrupted: false,
        });

        let mut registered_tui = false;
        let mut saw_started = false;
        let mut saw_output = false;
        let mut saw_finished = false;
        let deadline = tokio::time::Instant::now() + TestTimeouts::scale_secs(10);
        while tokio::time::Instant::now() < deadline && !(saw_started && saw_output && saw_finished)
        {
            let Ok(Ok(msg)) = tokio::time::timeout(
                TestTimeouts::scale_secs(5),
                recv_msg::<_, DaemonMsg>(&mut sub_reader),
            )
            .await
            else {
                break;
            };
            match msg {
                DaemonMsg::InstanceRegistered { instance } if instance.mode == "tui" => {
                    registered_tui = true;
                    assert_eq!(instance.client.as_deref(), Some("ahma-tui"));
                    assert_eq!(instance.scope, "/work/project");
                    assert_eq!(
                        instance.session_id.as_deref(),
                        Some(reporter.session_id()),
                        "the instance must carry the session id the reporter minted"
                    );
                }
                DaemonMsg::Event { payload, .. } => match payload {
                    DaemonEvent::OpStarted {
                        origin,
                        unsandboxed,
                        title,
                        ..
                    } => {
                        saw_started = true;
                        assert_eq!(origin.as_deref(), Some("tui"));
                        assert!(unsandboxed, "a `!` command ran outside the sandbox");
                        assert_eq!(title.as_deref(), Some("rm -rf build"));
                    }
                    DaemonEvent::OpOutput { line, .. } => {
                        saw_output = true;
                        assert_eq!(line, "removed");
                    }
                    DaemonEvent::OpFinished { exit_code, .. } => {
                        saw_finished = true;
                        assert_eq!(exit_code, Some(0));
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        assert!(registered_tui, "the TUI must register as mode=tui");
        assert!(
            saw_started && saw_output && saw_finished,
            "the whole command must reach the hub: started={saw_started} output={saw_output} finished={saw_finished}"
        );

        hub_task.abort();
    }

    /// The report path must survive a daemon that is not there: the command
    /// still runs, and nothing waits on the socket.
    #[tokio::test]
    async fn reporting_without_a_daemon_neither_blocks_nor_panics() {
        let _guard = isolate();
        let reporter = spawn_tui_reporter("/work".to_string());
        for i in 0..10 {
            let _ = i;
            reporter.report(bang_started(&next_bang_op_id("s"), "echo hi", "/work"));
        }
        // Nothing above awaited; the only assertion available is that we got
        // here, and that the handle is still usable.
        assert!(!reporter.session_id().is_empty());
    }

    /// Two commands are two operations, even from one window and one session.
    #[test]
    fn every_bang_command_gets_its_own_op_id() {
        assert_ne!(next_bang_op_id("a"), next_bang_op_id("a"));
        assert_ne!(next_bang_op_id("a"), next_bang_op_id("b"));
    }

    /// The registration is what makes the section read as "this terminal
    /// (you)": `work_view` keys that off `mode == "tui"`.
    #[test]
    fn the_registration_identifies_this_terminal() {
        match register_msg("s1", "/w") {
            ClientMsg::Register {
                mode,
                client,
                client_pid,
                scope,
                ..
            } => {
                assert_eq!(mode, "tui");
                assert_eq!(client.as_deref(), Some("ahma-tui"));
                assert_eq!(client_pid, Some(std::process::id()));
                assert_eq!(scope, "/w");
            }
            other => panic!("expected Register, got {other:?}"),
        }
    }
}
