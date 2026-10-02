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
//!
//! A drain (SPEC R-HUB.5) is the other way out: the hub keeps serving, starts
//! the build now at its own path as a successor that waits on the lock, and
//! goes the moment no work is in flight — or ends what is left at the drain
//! cap.

use crate::shell::cli::AppConfig;
use ahma_common::hub::{HubBindError, HubServer};
use ahma_http_bridge::{BridgeConfig, HubExit, ListenerKind, start_bridge};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// How often the hub checks whether anything is attached, and whether its
/// socket is still where clients will look for it.
const IDLE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Should an idle hub exit now?
///
/// Split out as a pure function because the interesting part is the policy, not
/// the timer: idleness spans both halves of the hub.
pub(crate) fn idle_exit_due(
    hub_connections: usize,
    mcp_sessions: usize,
    idle_for: std::time::Duration,
    timeout_secs: u64,
) -> bool {
    if hub_connections > 0 || mcp_sessions > 0 {
        return false;
    }
    // A zero timeout means "stay forever", which is what an operator who runs
    // the hub deliberately wants.
    timeout_secs > 0 && idle_for.as_secs() >= timeout_secs
}

/// Consecutive quiet looks a drain needs before it hands over.
///
/// One is not enough: a tool call answered a moment ago may not have reported
/// its operation to the hub yet, and in that instant nothing looks in flight.
const DRAIN_QUIET_CHECKS: u32 = 2;

/// What a draining hub does next (SPEC R-HUB.5).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DrainStep {
    /// Work is in flight and the cap is not reached: keep serving.
    Wait,
    /// Nothing is in flight: end the sessions, whose clients reconnect to the
    /// successor, and go.
    HandOver,
    /// The cap is reached with work still running: end it, disclosed and
    /// recorded as interrupted, and go.
    Interrupt,
}

/// Decide a draining hub's next step.
///
/// `work_in_flight` is operations still running in session workers plus
/// requests not yet answered. Open sessions and subscribers are deliberately
/// not work: an idle session loses nothing when its client reconnects to the
/// successor, and waiting for every session to end kept an outdated hub
/// alive for as long as any editor window stayed open.
pub(crate) fn drain_step(
    work_in_flight: usize,
    quiet_checks: u32,
    draining_for: std::time::Duration,
    cap_secs: u64,
) -> DrainStep {
    if work_in_flight == 0 {
        return if quiet_checks >= DRAIN_QUIET_CHECKS {
            DrainStep::HandOver
        } else {
            DrainStep::Wait
        };
    }
    if cap_secs > 0 && draining_for.as_secs() >= cap_secs {
        DrainStep::Interrupt
    } else {
        DrainStep::Wait
    }
}

/// How long the hub has had nothing attached, in wall-clock time (SPEC
/// R-HUB.3).
///
/// Not a tick count and not [`std::time::Instant`]: the monotonic clock stops
/// while a Mac sleeps, so a hub that was idle when the lid closed woke up
/// with its timer where it had left it, and an hour's timeout could stretch
/// over days.
#[derive(Debug, Default)]
pub(crate) struct IdleClock {
    since: Option<std::time::SystemTime>,
}

impl IdleClock {
    /// Record one observation and return how long the hub has been idle.
    pub(crate) fn observe(
        &mut self,
        busy: bool,
        now: std::time::SystemTime,
    ) -> std::time::Duration {
        if busy {
            self.since = None;
            return std::time::Duration::ZERO;
        }
        let since = *self.since.get_or_insert(now);
        now.duration_since(since).unwrap_or_else(|_| {
            // Stepped backwards: restart the window rather than guess.
            self.since = Some(now);
            std::time::Duration::ZERO
        })
    }
}

/// Which file sits at a socket path: `(device, inode)` on Unix.
///
/// Windows exposes no file identity without opening a handle, so there it is
/// presence alone — which still catches the case that matters, a cleared
/// runtime directory.
#[cfg(unix)]
fn socket_file_id(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn socket_file_id(path: &std::path::Path) -> Option<(u64, u64)> {
    std::fs::metadata(path).ok().map(|_| (0, 0))
}

/// Is the socket at the hub's path still the one it serves on?
///
/// Clients find the hub by that path and nothing else. Once the file has
/// gone — `$XDG_RUNTIME_DIR` cleared at logout, a stray `rm` — or another has
/// taken its place, no client can reach this hub again, and staying on would
/// leave a zombie beside the hub the next client starts (SPEC R-HUB.3).
#[derive(Debug, Default)]
pub(crate) struct SocketWatch {
    bound: Option<(u64, u64)>,
}

impl SocketWatch {
    /// `current` is [`socket_file_id`] now. Before the host has bound the
    /// path there is nothing to lose, so the first identity seen is the one
    /// the hub owns.
    pub(crate) fn still_ours(&mut self, current: Option<(u64, u64)>) -> bool {
        match self.bound {
            None => {
                self.bound = current;
                true
            }
            Some(bound) => current == Some(bound),
        }
    }
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

/// How often the hub looks at its own executable (SPEC R-HUB.5): every 30 s,
/// or every second under a test harness, so a test can watch it happen.
fn self_check_interval() -> std::time::Duration {
    if ahma_common::test_isolation::spawned_under_test_harness() {
        std::time::Duration::from_secs(1)
    } else {
        std::time::Duration::from_secs(30)
    }
}

/// When the hub next looks at whether its executable has been replaced.
///
/// Every [`self_check_interval`], and whenever a session has arrived since
/// the last look: a new session is the moment a new build matters most, and
/// the one most likely to follow an install.
#[derive(Debug, Default)]
pub(crate) struct SelfCheck {
    last: Option<std::time::SystemTime>,
    sessions: usize,
}

impl SelfCheck {
    /// Is a look due now, with `sessions` open?
    pub(crate) fn due(
        &mut self,
        now: std::time::SystemTime,
        sessions: usize,
        interval: std::time::Duration,
    ) -> bool {
        let new_session = sessions > self.sessions;
        self.sessions = sessions;
        let interval_passed = self
            .last
            .is_none_or(|last| now.duration_since(last).map_or(true, |d| d >= interval));
        if new_session || interval_passed {
            self.last = Some(now);
        }
        new_session || interval_passed
    }
}

/// Remove the builds a Windows install moved aside next to `exe` (SPEC
/// R-HUB.5): `<stem>.old`, or `<stem>.<secs>.old` when an earlier one was
/// still running. Windows will not replace a running executable, only rename
/// it, so every install leaves one. One still running — a hub that has not
/// finished draining — cannot be removed yet; the next start tries again.
/// Returns how many were removed.
///
/// Called only on Windows: elsewhere `ahma update` keeps `<stem>.old` on
/// purpose, as the build to roll back to.
// Tested on every OS; run only on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
async fn remove_builds_moved_aside(exe: &std::path::Path) -> usize {
    let (Some(dir), Some(stem)) = (exe.parent(), exe.file_stem()) else {
        return 0;
    };
    let prefix = format!("{}.", stem.to_string_lossy());
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return 0;
    };
    let mut removed = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(middle) = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix("old"))
        else {
            continue;
        };
        let moved_aside = middle.is_empty()
            || middle
                .strip_suffix('.')
                .is_some_and(|secs| !secs.is_empty() && secs.chars().all(|c| c.is_ascii_digit()));
        if moved_aside && tokio::fs::remove_file(entry.path()).await.is_ok() {
            removed += 1;
        }
    }
    removed
}

/// How often a successor looks for the rendezvous coming free.
const SUCCESSOR_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// Take the rendezvous once the draining hub that started this one lets it go
/// (SPEC R-HUB.5).
///
/// Stands down — as [`HubBindError::AlreadyRunning`], the same quiet exit as
/// losing a startup race — once a hub that is *not* draining answers on the
/// socket, because a client's own spawn got there first, or once the drain
/// cap and a minute's grace have passed, by which time the draining hub has
/// gone whatever happened.
async fn take_rendezvous_as_successor(
    socket: &std::path::Path,
    drain_cap_secs: u64,
) -> std::result::Result<HubServer, HubBindError> {
    let patience =
        (drain_cap_secs > 0).then(|| std::time::Duration::from_secs(drain_cap_secs + 60));
    let started = std::time::SystemTime::now();
    let socket_str = socket.to_string_lossy();
    loop {
        match HubServer::lock_at(socket.to_path_buf()).await {
            Err(HubBindError::AlreadyRunning) => {}
            taken => return taken,
        }
        if super::server::query_uds_health(&socket_str)
            .await
            .is_some_and(|h| !h.draining)
        {
            return Err(HubBindError::AlreadyRunning);
        }
        if patience.is_some_and(|p| started.elapsed().unwrap_or_default() >= p) {
            return Err(HubBindError::AlreadyRunning);
        }
        tokio::time::sleep(SUCCESSOR_POLL).await;
    }
}

/// Start the binary now at this hub's own path as its successor (SPEC
/// R-HUB.5).
///
/// From the path, not this process's image: after an install the path holds
/// the new build, which is the point. At this hub's own spawn depth, as a
/// sibling rather than a child: every upgrade would otherwise nest one level
/// deeper, and the one after [`ahma_common::process_guard::MAX_SPAWN_DEPTH`]
/// would refuse to start. A failure is only logged: the next client to find
/// no hub starts one anyway.
fn spawn_successor(
    exe: &std::path::Path,
    socket: &std::path::Path,
    idle_timeout_secs: Option<u64>,
) {
    if !super::hub_client::may_spawn_hub_from(exe) {
        return;
    }
    if let Err(why) = super::hub_client::may_spawn_hub_here() {
        tracing::warn!("ahma hub: no successor: {why}");
        return;
    }
    let mut cmd = super::hub_client::hub_command(exe, &socket.to_string_lossy(), idle_timeout_secs);
    cmd.arg("--successor");
    cmd.env(
        ahma_common::process_guard::SPAWN_DEPTH_ENV,
        ahma_common::process_guard::current_spawn_depth().to_string(),
    );
    match cmd.spawn() {
        Ok(_) => tracing::info!(exe = %exe.display(), "ahma hub: successor started"),
        Err(e) => tracing::warn!("ahma hub: could not start a successor: {e}"),
    }
}

/// Run the per-user hub: hub plus MCP endpoint, one runtime, one exit.
///
/// `successor` is set when a draining hub started this one to replace it
/// (SPEC R-HUB.5): it waits for the rendezvous to come free instead of
/// standing down because a hub is already running.
pub async fn run_hub_mode(config: AppConfig, successor: bool) -> Result<()> {
    if let Err(msg) = ahma_common::process_guard::check_spawn_depth() {
        tracing::error!("{msg}");
        return Err(anyhow::anyhow!(msg));
    }
    // Captured now: once an install replaces the file, Linux reports this
    // process's image as "(deleted)", and the successor must come from the
    // path.
    let exe = std::env::current_exe().context("Failed to get current executable path")?;
    // Pinned before anything else, for the same reason: this is the build
    // the hub runs, and the file it compares against to notice an install.
    let identity = ahma_common::exe_identity::ExeIdentity::this_process();
    #[cfg(windows)]
    {
        let removed = remove_builds_moved_aside(&exe).await;
        if removed > 0 {
            tracing::info!(removed, "ahma hub: removed builds an install moved aside");
        }
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

    let taken = if successor {
        take_rendezvous_as_successor(&socket, config.hub_drain_timeout_secs).await
    } else {
        HubServer::lock_at(socket.clone()).await
    };
    let mut hub = match taken {
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
    let interrupted = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let idle_task = spawn_idle_watcher(
        hub_connections,
        active_sessions.clone(),
        idle_timeout_secs,
        config.hub_drain_timeout_secs,
        hub.events(),
        exit.clone(),
        interrupted.clone(),
    );

    // ── Serve until something asks us to stop ────────────────────────────────
    let mut socket_watch = SocketWatch::default();
    let mut socket_check = tokio::time::interval(IDLE_CHECK_INTERVAL);
    let mut successor_started = false;
    let mut self_check = SelfCheck::default();
    let mut serving = std::pin::pin!(start_bridge(bridge));
    let mut signal = std::pin::pin!(shutdown_signal());
    let reason = loop {
        tokio::select! {
            result = &mut serving => {
                break match result {
                    Ok(()) => "listener ended".to_string(),
                    Err(e) => format!("listener failed: {e}"),
                };
            }
            stop = stop_rx.recv() => {
                break stop.unwrap_or_else(|| "stop requested".to_string());
            }
            _ = &mut signal => break "signal".to_string(),
            _ = socket_check.tick(), if !hub.relinquished() => {
                if !socket_watch.still_ours(socket_file_id(&socket)) {
                    // Nobody can reach us by that path any more. Give the
                    // rendezvous to whoever the next client starts, and
                    // finish what we have (SPEC R-HUB.3).
                    tracing::warn!(
                        socket = %socket.display(),
                        "ahma hub: its socket was removed or replaced; handing over \
                         and draining"
                    );
                    hub.relinquish();
                    exit.request_drain();
                } else if exit.is_draining() && !successor_started {
                    // Ready and waiting on the lock the moment this hub
                    // goes, so the handover is a gap of milliseconds rather
                    // than a cold start on some client's next request.
                    successor_started = true;
                    spawn_successor(&exe, &socket, config.idle_timeout_secs);
                } else if !exit.is_draining()
                    && self_check.due(
                        std::time::SystemTime::now(),
                        active_sessions.load(Ordering::SeqCst),
                        self_check_interval(),
                    )
                    && identity.is_some_and(|me| me.replaced_on_disk())
                {
                    // An install (cargo, brew, `ahma update`, a script)
                    // wrote a new build over ours: hand over to it, without
                    // interrupting anyone (SPEC R-HUB.5).
                    tracing::info!(
                        exe = %exe.display(),
                        "ahma hub: its executable was replaced by a new install; \
                         handing over to it"
                    );
                    exit.request_drain();
                }
            }
        }
    };
    idle_task.abort();

    tracing::info!("ahma hub: stopping ({reason})");
    // Say why, for whoever next asks about an operation this process
    // issued (SPEC R-LIFECYCLE.4): what is still running now is what this
    // exit interrupts.
    let mut interrupted_ops = interrupted.lock().clone();
    for id in hub.events().operations_in_flight().await {
        if !interrupted_ops.contains(&id) {
            interrupted_ops.push(id);
        }
    }
    {
        let path = ahma_common::hub_history::last_exit_path_for(&socket);
        let record = ahma_common::hub_history::LastExit {
            version: env!("CARGO_PKG_VERSION").to_string(),
            build_id: ahma_common::BUILD_ID.to_string(),
            reason: exit_reason(&reason, hub.relinquished()),
            at_epoch_ms: ahma_common::keepalive::current_timestamp_ms(),
            interrupted_ops,
        };
        if let Err(e) = ahma_common::hub_history::write_last_exit(&path, &record).await {
            tracing::warn!("ahma hub: could not record why it exited: {e}");
        }
    }
    if let Some(writer) = history_writer {
        // Flush before the sockets go: a record written but not yet on disk is
        // exactly the last thing that happened, which is what someone opening a
        // TUI afterwards is most likely to be looking for.
        writer.shutdown().await;
    }
    // One last look before unlinking anything: a path lost since the last
    // tick may already be a successor's (SPEC R-ISO.3).
    if !hub.relinquished() && !socket_watch.still_ours(socket_file_id(&socket)) {
        hub.relinquish();
    }
    // Last: dropping the hub unlinks the socket and then releases the lock, so
    // nothing above can touch a successor's files (SPEC R-ISO.3).
    drop(hub);
    Ok(())
}

/// Classify the reason the serve loop ended, for `last-exit.json`.
///
/// `socket_lost` tells a drain the hub started because nobody could reach it
/// from one a newer build asked for.
fn exit_reason(reason: &str, socket_lost: bool) -> ahma_common::hub_history::ExitReason {
    use ahma_common::hub_history::ExitReason;
    match reason {
        "idle" => ExitReason::Idle,
        "drained" if socket_lost => ExitReason::SocketRemoved,
        "drained" => ExitReason::Upgrade,
        "drain cap reached" => ExitReason::DrainTimeout,
        r if r.starts_with("listener failed") => ExitReason::Failed,
        _ => ExitReason::Stopped,
    }
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
/// attached for the configured window — or, once it is draining, once no work
/// is in flight (SPEC R-HUB.5).
fn spawn_idle_watcher(
    hub_connections: Arc<AtomicUsize>,
    active_sessions: Arc<AtomicUsize>,
    timeout_secs: u64,
    drain_cap_secs: u64,
    events: ahma_common::hub::HubEvents,
    exit: Arc<HubExit>,
    interrupted: Arc<parking_lot::Mutex<Vec<String>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut clock = IdleClock::default();
        let mut drain_started: Option<std::time::SystemTime> = None;
        let mut quiet_checks = 0u32;
        loop {
            tokio::time::sleep(IDLE_CHECK_INTERVAL).await;
            let now = std::time::SystemTime::now();
            if exit.is_draining() {
                let started = *drain_started.get_or_insert(now);
                let running = events.operations_in_flight().await;
                let work = running.len() + exit.requests_in_flight();
                quiet_checks = if work == 0 {
                    quiet_checks.saturating_add(1)
                } else {
                    0
                };
                let draining_for = now.duration_since(started).unwrap_or_default();
                match drain_step(work, quiet_checks, draining_for, drain_cap_secs) {
                    DrainStep::Wait => continue,
                    DrainStep::HandOver => {
                        // Sessions are left open on purpose: when the socket
                        // goes, each client sees its endpoint gone and
                        // reconnects to the successor at once, where a
                        // terminated session would first look like a failing
                        // request.
                        exit.request("drained");
                    }
                    DrainStep::Interrupt => {
                        tracing::warn!(
                            work,
                            drain_cap_secs,
                            "ahma hub: drain cap reached with work still in flight; \
                             ending it so the successor can take over"
                        );
                        // Recorded before the sessions end: once their
                        // workers are gone the hub no longer lists them.
                        interrupted.lock().extend(running);
                        // Answers each request still waiting with an error
                        // rather than leaving it on a process about to exit.
                        exit.end_sessions().await;
                        exit.request("drain cap reached");
                    }
                }
                return;
            }
            let hub_now = hub_connections.load(Ordering::Relaxed);
            let sessions_now = active_sessions.load(Ordering::SeqCst);
            let idle_for = clock.observe(hub_now > 0 || sessions_now > 0, now);
            if idle_exit_due(hub_now, sessions_now, idle_for, timeout_secs) {
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
        assert!(!idle_exit_due(1, 0, Duration::from_secs(600), 60));
        assert!(!idle_exit_due(0, 1, Duration::from_secs(600), 60));
        assert!(!idle_exit_due(0, 0, Duration::from_secs(59), 60));
        assert!(idle_exit_due(0, 0, Duration::from_secs(60), 60));
    }

    /// A draining hub hands over the moment no work is in flight — with
    /// sessions still open, because an idle session loses nothing: its
    /// client reconnects to the successor (SPEC R-HUB.5). Waiting for every
    /// session to end instead kept an outdated hub alive for as long as any
    /// editor window stayed open.
    #[test]
    fn a_drain_hands_over_once_nothing_is_in_flight() {
        let minute = Duration::from_secs(60);
        assert_eq!(drain_step(2, 0, minute, 3600), DrainStep::Wait);
        assert_eq!(
            drain_step(0, 1, minute, 3600),
            DrainStep::Wait,
            "one quiet look is not enough: a tool call answered a moment ago \
             may not have reported its operation yet"
        );
        assert_eq!(drain_step(0, 2, minute, 3600), DrainStep::HandOver);
    }

    /// Work that never goes quiet is ended at the cap, which is disclosed
    /// and recorded rather than waited on forever (SPEC R-HUB.5).
    #[test]
    fn a_drain_ends_what_is_left_at_its_cap() {
        let hour = Duration::from_secs(3600);
        assert_eq!(
            drain_step(1, 0, hour - Duration::from_secs(1), 3600),
            DrainStep::Wait
        );
        assert_eq!(drain_step(1, 0, hour, 3600), DrainStep::Interrupt);
        assert_eq!(
            drain_step(1, 0, hour * 24, 0),
            DrainStep::Wait,
            "0 waits for as long as the work takes"
        );
    }

    /// A subscriber has no work in the hub, so it counts toward idleness but
    /// never toward a drain: a TUI left attached would otherwise keep an
    /// outdated hub, or one whose socket is gone, alive for as long as the
    /// window stayed open.
    #[test]
    fn a_subscriber_does_not_hold_a_draining_hub_open() {
        assert_eq!(
            drain_step(0, 2, Duration::ZERO, 3600),
            DrainStep::HandOver,
            "subscribers are not in-flight work"
        );
        assert!(
            !idle_exit_due(3, 0, Duration::from_secs(600), 60),
            "outside a drain a subscriber still counts as attached"
        );
    }

    /// Idle time is wall-clock time (SPEC R-HUB.3). The monotonic clock stops
    /// while a Mac sleeps, so a hub idle when the lid closed at night used to
    /// wake up with its timer exactly where it had left it.
    #[test]
    fn idle_time_is_wall_clock_time() {
        let t0 = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut clock = IdleClock::default();
        assert_eq!(clock.observe(false, t0), Duration::ZERO);
        // Eight hours asleep between two one-second ticks.
        let woke = t0 + Duration::from_secs(8 * 3600);
        assert_eq!(clock.observe(false, woke), Duration::from_secs(8 * 3600));

        assert_eq!(
            clock.observe(true, woke),
            Duration::ZERO,
            "anything attached restarts the window"
        );
        assert_eq!(
            clock.observe(false, woke + Duration::from_secs(5)),
            Duration::ZERO,
            "and idleness is counted from when it resumed"
        );
        assert_eq!(
            clock.observe(false, woke + Duration::from_secs(7)),
            Duration::from_secs(2)
        );
    }

    /// A clock stepped backwards (NTP, a manual change) restarts the window
    /// rather than underflowing or exiting early.
    #[test]
    fn a_clock_stepped_backwards_restarts_the_window() {
        let t0 = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut clock = IdleClock::default();
        clock.observe(false, t0);
        let earlier = t0 - Duration::from_secs(600);
        assert_eq!(clock.observe(false, earlier), Duration::ZERO);
        assert_eq!(
            clock.observe(false, earlier + Duration::from_secs(3)),
            Duration::from_secs(3)
        );
    }

    /// The hub watches the one path every client finds it by (SPEC R-HUB.3).
    /// Before the host has bound it there is nothing to lose; after, the
    /// file going away — or another file taking its place — means no client
    /// can reach this hub any more.
    #[test]
    fn the_socket_watch_notices_the_path_being_lost() {
        let mut watch = SocketWatch::default();
        assert!(watch.still_ours(None), "not bound yet");
        assert!(watch.still_ours(Some((1, 7))), "bound");
        assert!(watch.still_ours(Some((1, 7))));
        assert!(!watch.still_ours(None), "removed");

        let mut watch = SocketWatch::default();
        watch.still_ours(Some((1, 7)));
        assert!(!watch.still_ours(Some((1, 8))), "replaced by another file");
    }

    /// The identity the watch compares is read from a real socket file, on
    /// every OS: Windows `AF_UNIX` sockets leave a file too.
    #[tokio::test]
    async fn a_socket_file_has_an_identity_until_it_is_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("hub.sock");
        assert_eq!(socket_file_id(&sock), None);
        let listener = ahma_common::local_socket::LocalListener::bind(&sock).expect("bind");
        let bound = socket_file_id(&sock);
        assert!(bound.is_some(), "a bound socket has an identity");
        let mut watch = SocketWatch::default();
        assert!(watch.still_ours(bound));
        std::fs::remove_file(&sock).expect("remove the socket file");
        assert!(!watch.still_ours(socket_file_id(&sock)));
        drop(listener);
    }

    /// The hub looks at its own executable on a timer and whenever a session
    /// arrives (SPEC R-HUB.5): a new session is when a new build matters
    /// most, and the one most likely to follow an install.
    #[test]
    fn the_hub_looks_at_its_binary_on_a_timer_and_on_each_new_session() {
        let t0 = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let every = Duration::from_secs(30);
        let mut check = SelfCheck::default();
        assert!(check.due(t0, 0, every), "the first look is immediate");
        assert!(!check.due(t0 + Duration::from_secs(10), 0, every));
        assert!(
            check.due(t0 + Duration::from_secs(11), 1, every),
            "a new session brings the next look forward"
        );
        assert!(
            !check.due(t0 + Duration::from_secs(12), 1, every),
            "the same session does not"
        );
        assert!(
            !check.due(t0 + Duration::from_secs(13), 0, every),
            "nor does one leaving"
        );
        assert!(check.due(t0 + Duration::from_secs(41), 0, every));
    }

    /// What a Windows install moved aside is removed on the next start, and
    /// nothing else is: not another program's `.old`, not the running build.
    #[tokio::test]
    async fn builds_moved_aside_by_an_install_are_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("ahma.exe");
        for name in [
            "ahma.exe",
            "ahma.old",
            "ahma.1759000000.old",
            "ahma.notes.old",
            "other.old",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        assert_eq!(remove_builds_moved_aside(&exe).await, 2);
        let mut left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["ahma.exe", "ahma.notes.old", "other.old"]);
    }

    /// `last-exit.json` names why the hub went in words a reader can act on
    /// (SPEC R-LIFECYCLE.4).
    #[test]
    fn the_exit_reason_is_classified() {
        use ahma_common::hub_history::ExitReason;
        assert_eq!(exit_reason("idle", false), ExitReason::Idle);
        assert_eq!(exit_reason("drained", false), ExitReason::Upgrade);
        assert_eq!(exit_reason("drained", true), ExitReason::SocketRemoved);
        assert_eq!(
            exit_reason("drain cap reached", false),
            ExitReason::DrainTimeout
        );
        assert_eq!(
            exit_reason("listener failed: boom", false),
            ExitReason::Failed
        );
        assert_eq!(exit_reason("signal", false), ExitReason::Stopped);
        assert_eq!(exit_reason("hub Shutdown", false), ExitReason::Stopped);
    }

    /// `0` is an operator saying "stay": a hub started deliberately in a
    /// terminal should not disappear because nobody happened to be attached.
    #[test]
    fn a_zero_timeout_never_expires() {
        assert!(!idle_exit_due(0, 0, Duration::from_secs(86_400), 0));
    }

    /// A successor waits for the draining hub to let the rendezvous go, then
    /// takes it (SPEC R-HUB.5): pre-spawned, so the handover is the moment the
    /// old hub leaves rather than a cold start on some client's next request.
    #[tokio::test]
    async fn a_successor_takes_the_rendezvous_once_it_comes_free() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("hub.sock");
        let draining = HubServer::lock_at(sock.clone()).await.expect("free");

        let waiting = tokio::spawn({
            let sock = sock.clone();
            async move { take_rendezvous_as_successor(&sock, 3600).await }
        });
        // Still held: the successor must not have given up.
        tokio::time::sleep(SUCCESSOR_POLL * 2).await;
        assert!(!waiting.is_finished(), "a held rendezvous is waited for");

        drop(draining);
        let taken = tokio::time::timeout(
            ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick),
            waiting,
        )
        .await
        .expect("the successor notices the rendezvous come free")
        .expect("join");
        assert!(taken.is_ok(), "and takes it: {:?}", taken.err());
    }

    /// A successor stands down when a hub that is not draining already
    /// serves: a client's own spawn won, and a second hub is exactly what the
    /// rendezvous exists to prevent.
    #[tokio::test]
    async fn a_successor_stands_down_for_a_hub_that_is_not_draining() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("hub.sock");
        let _held = HubServer::lock_at(sock.clone()).await.expect("free");
        let listener = ahma_common::local_socket::LocalListener::bind(&sock).expect("bind");
        let serving = tokio::spawn(async move {
            loop {
                let Ok(mut stream) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let body = r#"{"status":"OK","version":"0.0.0+x","draining":false}"#;
                let reply = format!(
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            }
        });

        let outcome = tokio::time::timeout(
            ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick),
            take_rendezvous_as_successor(&sock, 3600),
        )
        .await
        .expect("a successor with nothing to succeed does not wait");
        assert!(matches!(outcome, Err(HubBindError::AlreadyRunning)));
        serving.abort();
    }
}
