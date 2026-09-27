//! # The per-user hub
//!
//! One process per user hosts **both** halves of ahma's background presence
//! (SPEC R-HUB.1): the MCP endpoint every editor's `ahma serve stdio`
//! proxies to, and the observability hub every instance reports to and every
//! TUI subscribes to.
//!
//! They used to be two independent singletons with two rendezvous points, two
//! lifetimes and no shared identity — which is why the TUI could end up owning
//! the hub while a detached bridge owned the MCP endpoint, and why quitting a
//! terminal window could take the event stream away from three editors that
//! knew nothing about it.
//!
//! ## What "one process" does and does not mean
//!
//! It does **not** mean one sandbox. Tools still run in one kernel-sandboxed
//! worker subprocess per MCP session, because on Linux a Landlock ruleset
//! restricts the process that applies it, irreversibly: a process cannot hold
//! two workspace scopes (SPEC R5.1). The hub is a control plane. It executes
//! nothing itself.
//!
//! ## Lifetime
//!
//! Both halves are served on **one socket** (SPEC R-HUB.2): the MCP bridge
//! listens on it, answering `/mcp` and `/health`, and hands `GET /events`
//! upgrades to the event stream. The rendezvous lock beside it is the mutex;
//! whoever takes it binds the socket. Idle is judged across *both* halves — no
//! MCP sessions **and** no hub connections — so a TUI watching an idle project
//! keeps the hub alive while an editor with no TUI attached does too. Exit
//! runs one choreography: stop the sessions, flush history, unlink the socket
//! while the lock is still held (SPEC R-ISO.3), and go.

use crate::shell::cli::AppConfig;
use ahma_common::hub::{HubBindError, HubServer};
use ahma_http_bridge::{BridgeConfig, HubExit, ListenerKind, start_bridge};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// How often the idle watcher checks whether anything is attached.
const IDLE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Should the hub exit now?
///
/// Split out as a pure function because the interesting part is the policy, not
/// the timer: idleness spans both halves of the hub, and a drain shortens
/// the wait to nothing so a replacement can take over the moment the last
/// session ends.
pub(crate) fn idle_exit_due(
    hub_connections: usize,
    mcp_sessions: usize,
    idle_for: std::time::Duration,
    timeout_secs: u64,
    draining: bool,
) -> bool {
    if hub_connections > 0 || mcp_sessions > 0 {
        return false;
    }
    if draining {
        // A drained hub has nothing left to finish; waiting out the idle
        // timer would only delay its successor.
        return true;
    }
    // A zero timeout means "stay forever", which is what an operator who runs
    // the hub deliberately wants.
    timeout_secs > 0 && idle_for.as_secs() >= timeout_secs
}

/// Is this rendezvous in the directory **ahma chose**, rather than one it was
/// told to use?
///
/// The R-HUB.2 ownership-and-mode guarantee is one ahma makes about the
/// runtime directory it creates `0700` itself. An operator who passes
/// `--unix-socket-path` (or `[http] unix_socket_path`) has made a deliberate
/// placement decision, and vetoing it is not ahma's call — `/tmp` is owned by
/// root on every Unix, so the veto refused to start at all.
fn rendezvous_is_ahma_owned(hub_socket: &std::path::Path) -> bool {
    ahma_common::hub::runtime_dir()
        .is_some_and(|chosen| hub_socket.parent() == Some(chosen.as_path()))
}

/// What to say about an operator-chosen directory others can write.
///
/// `None` when there is nothing to say. The sticky bit is the distinction that
/// matters: without it another local user can unlink our socket and bind their
/// own in its place, and every client would connect to theirs. With it — `/tmp`
/// and `/var/tmp` on every Unix — they cannot, which is why the shared temp
/// directories are a reasonable place to put a socket and a bare `0777`
/// directory is not.
#[cfg(unix)]
fn squattable_directory_notice(dir: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(dir).ok()?.permissions().mode();
    let others_can_write = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    (others_can_write && !sticky).then(|| {
        format!(
            "the ahma hub's rendezvous directory {} is writable by other users and has no \
             sticky bit, so another local user can replace its sockets with their own. This \
             path was chosen explicitly; ahma's own runtime directory is created 0700. Move \
             the socket, or chmod +t the directory.",
            dir.display()
        )
    })
}

#[cfg(not(unix))]
fn squattable_directory_notice(_dir: &std::path::Path) -> Option<String> {
    // Access control on Windows comes from the per-user profile ACL, which no
    // mode-bit inspection would describe.
    None
}

/// Run the per-user hub: hub plus MCP endpoint, one runtime, one exit.
pub async fn run_hub_mode(config: AppConfig) -> Result<()> {
    if let Err(msg) = ahma_common::process_guard::check_spawn_depth() {
        tracing::error!("{msg}");
        return Err(anyhow::anyhow!(msg));
    }

    // ── The rendezvous, and the mutex ────────────────────────────────────────
    //
    // One socket (SPEC R-HUB.2): the bridge below serves `/mcp`, `/health`
    // and the event stream on it. The hub takes its lock and leaves the bind
    // to the bridge.
    let socket = ahma_common::hub::default_socket_path();
    if let Some(dir) = socket.parent() {
        if rendezvous_is_ahma_owned(&socket) {
            if let Err(e) = ahma_common::hub::verify_runtime_dir_secure(dir) {
                // Refusing is the point: a directory another user can write is
                // a directory in which our socket can be replaced with theirs.
                return Err(e.context("refusing to start the ahma hub"));
            }
        } else if let Some(notice) = squattable_directory_notice(dir) {
            tracing::warn!("{notice}");
        }
    }

    let hub = match HubServer::lock_at(socket.clone()).await {
        Ok(server) => server,
        Err(HubBindError::AlreadyRunning) => {
            // The ordinary outcome of losing a startup race: the winner is
            // already serving, and our caller will connect to it.
            tracing::info!("ahma hub: another hub already owns the rendezvous; exiting");
            return Ok(());
        }
        Err(HubBindError::Failed(e)) => return Err(e),
    };

    let history_writer = hub
        .attach_history(ahma_common::hub_history::history_path())
        .await;

    // ── One exit path for both halves ────────────────────────────────────────
    let (stop_tx, mut stop_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let exit = Arc::new(HubExit::new(Box::new(move |reason: &str| {
        let _ = stop_tx.send(reason.to_string());
    })));
    hub.set_exit_hook({
        let exit = exit.clone();
        Arc::new(move |reason: &str| exit.request(reason))
    });

    // ── The one listener ─────────────────────────────────────────────────────
    let active_sessions = Arc::new(AtomicUsize::new(0));
    let mut bridge = build_bridge_config(&config, &socket, &active_sessions, &exit)?;
    bridge.hub_events = Some(hub.events());

    // An explicit --idle-timeout is a deliberate instruction and outranks the
    // settings value, as every other flag does (SPEC R-CFG1).
    let idle_timeout_secs = config
        .idle_timeout_secs
        .unwrap_or(config.hub_idle_timeout_secs);

    tracing::info!(
        socket = %socket.display(),
        idle_timeout_secs,
        "ahma hub: serving the MCP endpoint and the event stream"
    );

    let hub_connections = hub.connection_count();
    let idle_task = spawn_idle_watcher(
        hub_connections,
        active_sessions.clone(),
        idle_timeout_secs,
        exit.clone(),
    );

    // ── Serve until something asks us to stop ────────────────────────────────
    let reason = tokio::select! {
        result = start_bridge(bridge) => {
            match result {
                Ok(()) => "listener ended".to_string(),
                Err(e) => format!("listener failed: {e}"),
            }
        }
        stop = stop_rx.recv() => stop.unwrap_or_else(|| "stop requested".to_string()),
        _ = shutdown_signal() => "signal".to_string(),
    };
    idle_task.abort();

    tracing::info!("ahma hub: stopping ({reason})");
    if let Some(writer) = history_writer {
        // Flush before the sockets go: a record written but not yet on disk is
        // exactly the last thing that happened, which is what someone opening a
        // TUI afterwards is most likely to be looking for.
        writer.shutdown().await;
    }
    // Last: dropping the hub unlinks the socket and then releases the lock, so
    // nothing above can touch a successor's files (SPEC R-ISO.3).
    drop(hub);
    Ok(())
}

/// Build the MCP endpoint's configuration.
///
/// Deliberately **no** `default_sandbox_scope`: a scope set here would apply to
/// every session from every client (SPEC R5.1, R10.3). Each session locks its
/// own, from its own client's `roots/list`.
fn build_bridge_config(
    config: &AppConfig,
    socket: &std::path::Path,
    active_sessions: &Arc<AtomicUsize>,
    exit: &Arc<HubExit>,
) -> Result<BridgeConfig> {
    let server_command = std::env::current_exe()
        .context("Failed to get current executable path")?
        .to_string_lossy()
        .to_string();

    // Workers must report to *this* hub, not to whatever the default path
    // resolves to in their environment — a worker reporting somewhere else is
    // exactly the confusion the one-hub rule exists to remove. Said
    // explicitly even when it is the default, so a worker never has to
    // re-derive it.
    let mut server_args = super::build_stdio_server_args(config, "--tools", false);
    server_args.push("--unix-socket-path".to_string());
    server_args.push(socket.to_string_lossy().into_owned());

    Ok(BridgeConfig {
        bind_addr: "127.0.0.1:0".parse().expect("a literal loopback address"),
        server_command,
        server_args,
        enable_colored_output: true,
        default_sandbox_scope: None,
        handshake_timeout_secs: config.handshake_timeout_secs,
        request_timeout_secs: ahma_http_bridge::session::DEFAULT_REQUEST_TIMEOUT_SECS,
        tool_call_timeout_secs: ahma_http_bridge::session::DEFAULT_TOOL_CALL_TIMEOUT_SECS,
        // QUIC is UDP-based and has no meaning over a Unix socket.
        enable_quic: false,
        disable_http1_1: false,
        // The same `AF_UNIX` socket on every OS, Windows included (SPEC
        // R-HUB.2): until this was so, Windows bound a TCP port that no
        // client could discover.
        listener_kind: ListenerKind::Unix(socket.to_string_lossy().into_owned()),
        require_token: None,
        require_token_path: None,
        rate_limit_rps: 0,
        rate_limit_burst: 10,
        active_sessions: Some(active_sessions.clone()),
        // The hub owns the idle policy: the bridge's own timer counts only
        // MCP sessions and would exit while a TUI was still watching.
        idle_timeout_secs: None,
        max_sessions: config.max_sessions,
        peer_factory: None,
        bound_port_tx: None,
        // The hub owns the option allowlist, so it is the hub that
        // teaches the bridge how to read a session's query (SPEC R-HUB.4).
        session_options: Some(Arc::new(|query: &str| {
            let pairs = super::session_options::parse_session_query(query)?;
            super::session_options::session_query_to_worker_args(&pairs)
        })),
        exit: Some(exit.clone()),
        // Filled in by the caller, which owns the hub.
        hub_events: None,
    })
}

/// Watch both halves and ask the hub to stop once neither has anything
/// attached for the configured window.
fn spawn_idle_watcher(
    hub_connections: Arc<AtomicUsize>,
    active_sessions: Arc<AtomicUsize>,
    timeout_secs: u64,
    exit: Arc<HubExit>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut idle_for = std::time::Duration::ZERO;
        loop {
            tokio::time::sleep(IDLE_CHECK_INTERVAL).await;
            let hub_now = hub_connections.load(Ordering::Relaxed);
            let sessions_now = active_sessions.load(Ordering::SeqCst);
            if hub_now > 0 || sessions_now > 0 {
                idle_for = std::time::Duration::ZERO;
                continue;
            }
            idle_for += IDLE_CHECK_INTERVAL;
            if idle_exit_due(
                hub_now,
                sessions_now,
                idle_for,
                timeout_secs,
                exit.is_draining(),
            ) {
                exit.request("idle");
                return;
            }
        }
    })
}

/// Resolve when the process is asked to stop by the operating system.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return std::future::pending().await,
        };
        let mut int = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => return std::future::pending().await,
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// ahma vets the directory it chose, and only that one.
    ///
    /// The check exists because a `0600` socket inside a directory another user
    /// can write is still squattable (SPEC R-HUB.2) — a guarantee ahma makes
    /// about the runtime directory *it* creates. Applying it to a path an
    /// operator named turned that guarantee into a veto: `--unix-socket-path
    /// /tmp/x.sock` refused to start at all, because `/tmp` is owned by root.
    ///
    /// Every E2E test that drives the real binary passes such a path, and on
    /// Linux CI every one of them died on it. They passed on the developer's
    /// machine only because a live ahma bridge was listening on the HTTP
    /// fallback port and answered the readiness probe — the hub under test
    /// had never started at all. That is R-ISO.1's failure mode from the other
    /// side: not a test corrupting live state, but live state rescuing a test.
    #[test]
    fn ahma_vets_its_own_rendezvous_directory_and_not_one_it_was_given() {
        let chosen = ahma_common::hub::runtime_dir().expect("a runtime dir is available");
        assert!(
            rendezvous_is_ahma_owned(&chosen.join("hub.sock")),
            "the directory ahma resolved is ahma's to guarantee"
        );

        let elsewhere = tempfile::tempdir().expect("tempdir");
        assert!(
            !rendezvous_is_ahma_owned(&elsewhere.path().join("hub.sock")),
            "a path the operator named is theirs, and refusing it is not our call"
        );
    }

    /// Not vetting is not the same as saying nothing.
    ///
    /// An operator may put the rendezvous where they like, but a directory
    /// others can write, without the sticky bit that stops them unlinking our
    /// socket, is genuinely squattable — so it is disclosed (SPEC R7: ahma's
    /// own posture is never something a user has to infer).
    #[cfg(unix)]
    #[test]
    fn a_squattable_rendezvous_directory_is_disclosed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let set = |mode: u32| {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        };

        set(0o700);
        assert!(squattable_directory_notice(dir.path()).is_none(), "private");

        set(0o1777);
        assert!(
            squattable_directory_notice(dir.path()).is_none(),
            "sticky: another user cannot unlink our socket"
        );

        set(0o777);
        let notice = squattable_directory_notice(dir.path())
            .expect("world-writable without the sticky bit is worth saying out loud");
        assert!(
            notice.contains(&dir.path().display().to_string()),
            "the notice must name the directory: {notice}"
        );

        set(0o700);
    }

    /// A worker must report to *its own* hub, on the one socket (SPEC
    /// R-HUB.2) — told so explicitly, so it never re-derives the path.
    #[test]
    fn workers_are_told_which_hub_to_report_to() {
        let socket = std::path::Path::new("/run/user/1000/ahma-test/hub.sock");
        let sessions = Arc::new(AtomicUsize::new(0));
        let exit = Arc::new(HubExit::new(Box::new(|_| {})));

        let cfg = build_bridge_config(&AppConfig::default(), socket, &sessions, &exit)
            .expect("config builds");
        let idx = cfg
            .server_args
            .iter()
            .position(|a| a == "--unix-socket-path")
            .expect("the worker is told which hub to use");
        assert_eq!(cfg.server_args[idx + 1], socket.to_string_lossy());
        assert!(
            matches!(&cfg.listener_kind, ListenerKind::Unix(p) if p == &socket.to_string_lossy()),
            "and the bridge serves that same socket: {:?}",
            cfg.listener_kind
        );
    }

    /// The hub's MCP endpoint carries no scope of its own: one there would
    /// apply to every client (SPEC R5.1, R-HUB.9).
    #[test]
    fn the_hub_endpoint_has_no_process_wide_sandbox_scope() {
        let sessions = Arc::new(AtomicUsize::new(0));
        let exit = Arc::new(HubExit::new(Box::new(|_| {})));
        let cfg = build_bridge_config(
            &AppConfig::default(),
            std::path::Path::new("/tmp/hub.sock"),
            &sessions,
            &exit,
        )
        .expect("config builds");
        assert!(cfg.default_sandbox_scope.is_none());
        assert!(
            cfg.idle_timeout_secs.is_none(),
            "the hub owns the idle policy, not the bridge"
        );
    }

    /// Idleness spans both halves. Counting only MCP sessions would exit while
    /// a TUI sat watching an idle project; counting only hub connections would
    /// exit while an editor was mid-build with no TUI attached.
    #[test]
    fn idle_exit_needs_both_halves_empty_for_the_full_window() {
        assert!(!idle_exit_due(1, 0, Duration::from_secs(600), 60, false));
        assert!(!idle_exit_due(0, 1, Duration::from_secs(600), 60, false));
        assert!(!idle_exit_due(0, 0, Duration::from_secs(59), 60, false));
        assert!(idle_exit_due(0, 0, Duration::from_secs(60), 60, false));
    }

    /// A drained hub has nothing left to finish, so it goes as soon as the
    /// last session ends rather than making its successor wait out the timer.
    #[test]
    fn draining_shortens_the_wait_to_nothing() {
        assert!(idle_exit_due(0, 0, Duration::ZERO, 60, true));
        assert!(
            !idle_exit_due(0, 1, Duration::ZERO, 60, true),
            "but a live session still holds a draining hub open"
        );
    }

    /// `0` is an operator saying "stay": a hub started deliberately in a
    /// terminal should not disappear because nobody happened to be attached.
    #[test]
    fn a_zero_timeout_never_expires() {
        assert!(!idle_exit_due(0, 0, Duration::from_secs(86_400), 0, false));
    }
}
