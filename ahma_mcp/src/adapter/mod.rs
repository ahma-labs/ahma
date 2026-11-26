//! # CLI Tool Adapter: The Execution Bridge
//!
//! This module provides the central "heavy lifter" of the Ahma engine. The [`Adapter`]
//! acts as a bridge between high-level, declarative tool requests (expressed via MTDF)
//! and the low-level reality of OS process execution, shell environments, and
//! filesystem security.
//!
//! ## Core Mission
//!
//! The adapter's primary responsibility is to execute commands efficiently while
//! strictly enforcing the security boundaries established by the [`Sandbox`](crate::sandbox::Sandbox).
//!
//! ## How it Works: The Execution Paths
//!
//! Every command runs one way: as a tracked operation in a spawned task
//! (`drive`), its result held by the
//! [`OperationMonitor`](crate::operation_monitor::OperationMonitor) and pushed
//! back via notifications. Synchronous is only a waiting policy at the edge
//! (SPEC R2.1): an MCP call that asks to wait waits for its operation within
//! the client's budget, and a caller with no later call to collect a result
//! with — a terminal hook, the CLI one-shot — waits for it to end
//! ([`Adapter::run_to_completion`]).
//!
//! ## Key Design Trade-offs
//!
//! - **Statelessness**: The adapter is intentionally stateless regarding *what* a tool
//!   is. It focuses entirely on *how* to run it. Tool discovery and argument parsing
//!   happen at the protocol layer ([`mcp_service`](crate::mcp_service)).
//! - **Security Gating**: Every execution request must pass through the
//!   [`Sandbox`](crate::sandbox::Sandbox) validation. If a working directory or a
//!   path argument falls outside the allowed scopes, the adapter terminates the
//!   request before the shell process is ever notified.
//! - **Resource RAII**: The adapter manages temporary files created for complex
//!   multi-line arguments, ensuring they are automatically cleaned up even if
//!   an operation times out or is cancelled.
//! - **Auditability**: every execution path writes a `tool_call` to the
//!   append-only [`audit`](crate::adapter::audit) log *before* spawning, and exactly one matching
//!   `tool_complete` on every terminal path. Output tells you what a command
//!   printed; the audit log is what tells you that it happened.

pub mod audit;
mod drive;
pub mod executor;
mod guards;
pub mod lane;
pub mod mutex_groups;
mod piped;
mod preparer;
mod pty_exec;
mod sink;
pub mod spill;
pub(crate) mod time_limit;
mod types;
pub mod workspace_queue;

pub use mutex_groups::CommandMutexRegistry;
pub use preparer::{
    TempFileManager, escape_shell_argument, format_option_flag, needs_file_handling,
    prepare_command_and_args,
};
pub use pty_exec::pty_available;
pub use types::{AsyncExecOptions, ExecutionMode};

use crate::operation_monitor::{Operation, OperationMonitor, OperationStatus, Stream, Terminal};
use crate::sandbox;
use crate::sandbox::handoff_watch::{HandoffReport, HandoffWatch, HandoffWatchMode};
use crate::shell_pool::{ProcessGroupGuard, ShellPoolManager, kill_process_tree};
use ahma_common::event_dispatcher::EventDispatcher;
use anyhow::Result;
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::Mutex, task::JoinHandle};

use crate::utils::operation::mint_id as generate_id;

const MAX_STREAM_COLLECTED_LINES: usize = 5_000;
const MAX_STREAM_COLLECTED_BYTES: usize = 1_000_000;

/// Upper bound on how long an operation must be output-silent before we start
/// sampling its process tree's CPU time.
///
/// The effective threshold is `min(this, idle_limit / 3)` — see
/// [`cpu_probe_threshold`]. A fixed value would be a latent bug: with an idle
/// limit shorter than the threshold, the watchdog would fire before a single
/// sample was ever taken, and silent-but-working operations would be killed
/// exactly as before.
const CPU_LIVENESS_PROBE_AFTER: Duration = Duration::from_secs(30);

/// How long to let an operation stay silent before probing its CPU.
///
/// `None` when the idle watchdog is disabled: nothing can kill a quiet operation,
/// so there is no reason to pay for a process scan.
fn cpu_probe_threshold(idle_limit: Option<Duration>) -> Option<Duration> {
    // A third of the budget leaves room for at least two samples (a rise needs
    // two) before the watchdog is entitled to fire.
    idle_limit.map(|limit| (limit / 3).min(CPU_LIVENESS_PROBE_AFTER))
}

/// `result` led by a trust-handoff alert (SPEC R6.1.7). A typed sandbox
/// denial keeps its type, because the MCP boundary turns it into a
/// structured `sandbox_denial` payload; the alert then reaches the log and
/// the audit trail only.
fn with_handoff_alert(
    result: Result<String, anyhow::Error>,
    alert: &str,
) -> Result<String, anyhow::Error> {
    match result {
        Ok(output) => Ok(format!("{alert}\n\n{output}")),
        Err(e) if e.downcast_ref::<sandbox::SandboxError>().is_some() => Err(e),
        Err(e) => Err(anyhow::anyhow!("{alert}\n\n{e}")),
    }
}

/// What a command stopped at its limit had printed, stdout then stderr: the
/// last lines say what it was stuck on.
fn printed_before_kill(raw: &sink::RawOutput) -> String {
    [&raw.stdout, &raw.stderr]
        .into_iter()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Default)]
struct BoundedLineCollector {
    lines: VecDeque<String>,
    bytes: usize,
    dropped_lines: usize,
    dropped_bytes: usize,
}

impl BoundedLineCollector {
    fn push(&mut self, line: String) {
        self.bytes = self.bytes.saturating_add(line.len());
        self.lines.push_back(line);

        while self.lines.len() > MAX_STREAM_COLLECTED_LINES
            || self.bytes > MAX_STREAM_COLLECTED_BYTES
        {
            let Some(evicted) = self.lines.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(evicted.len());
            self.dropped_lines = self.dropped_lines.saturating_add(1);
            self.dropped_bytes = self.dropped_bytes.saturating_add(evicted.len());
        }
    }

    fn dropped_lines(&self) -> usize {
        self.dropped_lines
    }

    fn dropped_bytes(&self) -> usize {
        self.dropped_bytes
    }

    fn rendered_output(&self) -> String {
        let body: Vec<&str> = self.lines.iter().map(String::as_str).collect();
        let body = body.join("\n");
        if self.dropped_lines > 0 {
            format!(
                "[output truncated: dropped {} earlier lines ({} bytes)]\n{}",
                self.dropped_lines, self.dropped_bytes, body
            )
        } else {
            body
        }
    }
}

/// The core execution engine for external tools.
///
/// The `Adapter` coordinates the execution of CLI commands, managing the lifecycle of
/// shell processes, operation tracking, and resource cleanup. It serves as the bridge
/// between incoming tool requests and the underlying system shell.
///
/// # Responsibilities
///
/// *   **Command Execution**: Executes tools via standard process spawning, each one a
///     tracked operation in its own task.
/// *   **Resource Management**: Manages temporary files created for complex arguments and
///     ensures they are cleaned up.
/// *   **Operation Tracking**: Uses `OperationMonitor` to track the status (running, completed,
///     failed) of asynchronous operations.
/// *   **Sandboxing**: Enforces path security by validating operations against a root directory.
///
/// # Thread Safety
///
/// The `Adapter` is designed to be shared across threads (`Arc<Adapter>`) and uses internal
/// synchronization to manage state safely.
///
/// # Event Stream (P2)
///
/// All operation lifecycle events are broadcast on [`Adapter::event_dispatcher`].  Tests
/// and monitoring components subscribe to this stream to assert on the deterministic event
/// sequence without racing against transport teardown.  Production callers continue to use
/// the `callback` / `OperationMonitor` paths until P5.
#[derive(Debug)]
pub struct Adapter {
    /// Operation monitor for async tasks.
    monitor: Arc<OperationMonitor>,
    /// Holder for the shared default command timeout.
    shell_pool: Arc<ShellPoolManager>,
    /// Security sandbox context.
    sandbox: Arc<sandbox::Sandbox>,
    /// Handles to spawned tasks for graceful shutdown.
    task_handles: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    /// Temporary file manager for multi-line arguments - cleaned up automatically when dropped
    temp_file_manager: preparer::TempFileManager,
    /// Custom command executor.
    pub command_executor: Arc<dyn executor::CommandExecutor>,
    /// Unified event dispatcher (P2).
    pub event_dispatcher: EventDispatcher,
    /// Persistent stateful shell sessions (`session_id` parameter).
    pub shell_sessions: Arc<crate::shell_session::ShellSessionManager>,
    /// Configurable per-(group, directory) command serialisation registry.
    pub mutex_registry: Arc<CommandMutexRegistry>,
    /// The workspace write queue (SPEC R2.7): exclusive operations in one
    /// workspace run one at a time, in arrival order, across every ahma
    /// process. Disabled unless the embedder opts in
    /// ([`Self::with_workspace_queue`]); the `ahma` binary always does.
    workspace_queue: workspace_queue::WorkspaceQueue,
    /// Told once when a caller that waits has to wait for its workspace (SPEC
    /// R2.7.1): a terminal hook writes it to stderr, so the harness's own
    /// shell result explains the delay instead of hanging silently.
    queue_wait_notice: Option<QueueWaitNotice>,
    /// Optional sink for auto-detected sandbox scope violations. When set, an
    /// out-of-scope path (rejected up front, or surfaced by a stderr denial) raises
    /// a "grant access to X?" prompt through this notifier. `None` disables
    /// detection (the default). The notifier only *persists* an approved grant — it
    /// never widens the live session (SPEC R5).
    scope_grant_notifier: Option<Arc<dyn sandbox::ScopeGrantNotifier>>,
    /// Gives each command an SSH key broker (SPEC R-CRED.1): its socket is the
    /// command's `SSH_AUTH_SOCK`, and what it refused is one line in the
    /// result. `None` leaves the human's own agent socket in place.
    #[cfg(unix)]
    credential_brokers: Option<Arc<dyn crate::credentials::ssh_agent::consent::BrokerFactory>>,
    /// Gives each command a link broker (SPEC R-LINK.1): `BROWSER` and
    /// `GH_BROWSER` name ahma's link helper and `AHMA_LINK_SOCK` its socket,
    /// and what it refused is one line in the result. `None` leaves the
    /// command's `BROWSER` alone. Served on the synchronous, asynchronous and
    /// PTY paths, in both lanes; a shell session's commands share one
    /// long-lived shell, which no per-command lease can reach.
    #[cfg(unix)]
    link_brokers: Option<Arc<dyn crate::links::host::LinkBrokerFactory>>,
    /// Whether the trust-handoff deny tier is inventoried around each command
    /// (SPEC R6.1.7). [`HandoffWatchMode::Auto`] watches exactly where the
    /// kernel does not hold the tier; [`Adapter::with_handoff_watch`] overrides it.
    handoff_watch: HandoffWatchMode,
}

impl Adapter {
    /// Creates a new `Adapter` instance.
    ///
    /// The adapter requires an `OperationMonitor` for tracking async tasks, a `ShellPoolManager`
    /// holding the default command timeout, and a `Sandbox` for security context.
    ///
    /// # Arguments
    ///
    /// * `monitor` - Shared reference to the operation monitor
    /// * `shell_pool` - Shared reference to the command-timeout configuration holder
    /// * `sandbox` - Shared reference to the security sandbox
    pub fn new(
        monitor: Arc<OperationMonitor>,
        shell_pool: Arc<ShellPoolManager>,
        sandbox: Arc<sandbox::Sandbox>,
    ) -> Result<Self> {
        // Default to an empty (no-op) registry; use `with_mutex_registry` to enable gating.
        let registry = Arc::new(CommandMutexRegistry::from_config(&[]));
        Self::new_with_registry(monitor, shell_pool, sandbox, registry)
    }

    /// Create an adapter with a configured command-mutex registry.
    ///
    /// Use this instead of [`Self::new`] when you have a [`CommandMutexRegistry`]
    /// built from settings (i.e. in `service_builder.rs`).
    pub fn new_with_registry(
        monitor: Arc<OperationMonitor>,
        shell_pool: Arc<ShellPoolManager>,
        sandbox: Arc<sandbox::Sandbox>,
        mutex_registry: Arc<CommandMutexRegistry>,
    ) -> Result<Self> {
        // Share the monitor's dispatcher so every component emits into ONE
        // unified event stream (SPEC R15) — the monitor owns lifecycle events,
        // the adapter only adds supplementary ones.
        let event_dispatcher = monitor.event_dispatcher().clone();
        Ok(Self {
            monitor,
            shell_pool,
            sandbox,
            task_handles: Arc::new(Mutex::new(HashMap::new())),
            temp_file_manager: preparer::TempFileManager::new(),
            command_executor: Arc::new(executor::DefaultCommandExecutor),
            event_dispatcher,
            shell_sessions: crate::shell_session::ShellSessionManager::new(),
            mutex_registry,
            workspace_queue: workspace_queue::WorkspaceQueue::disabled(),
            queue_wait_notice: None,
            scope_grant_notifier: None,
            #[cfg(unix)]
            credential_brokers: None,
            #[cfg(unix)]
            link_brokers: None,
            handoff_watch: HandoffWatchMode::Auto,
        })
    }

    /// Choose when the trust-handoff deny tier is watched (SPEC R6.1.7).
    ///
    /// The default, [`HandoffWatchMode::Auto`], watches wherever the kernel does
    /// not refuse writes to `<git_dir>/hooks` and `<workspace>/.ahma` itself —
    /// Linux, Windows, and any sandbox that is not enforcing. `Always` lets a
    /// test exercise detection on macOS too.
    pub fn with_handoff_watch(mut self, mode: HandoffWatchMode) -> Self {
        self.handoff_watch = mode;
        self
    }

    /// Whether a command in `lane` is watched. A read-only command is not: the
    /// read-only lane exists only where the kernel forbids it every write
    /// (SPEC R2.7.4), the deny tier included.
    fn watches_handoff(&self, lane: workspace_queue::Lane) -> bool {
        lane != workspace_queue::Lane::ReadOnly && self.handoff_watch.is_active(&self.sandbox)
    }

    /// Enable (or replace) the workspace write queue (SPEC R2.7).
    pub fn with_workspace_queue(mut self, queue: workspace_queue::WorkspaceQueue) -> Self {
        self.workspace_queue = queue;
        self
    }

    /// Where a caller that waits ([`Self::run_to_completion`]) says that it is waiting for its workspace
    /// (SPEC R2.7.1). Without one the wait is logged at `info`.
    pub fn with_queue_wait_notice(mut self, notice: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.queue_wait_notice = Some(QueueWaitNotice(notice));
        self
    }

    /// The workspace write queue, for callers that must ask whether a
    /// workspace is busy (the edit guard, SPEC R2.7.8) or why an operation has
    /// not started (SPEC R2.7.3).
    pub fn workspace_queue(&self) -> &workspace_queue::WorkspaceQueue {
        &self.workspace_queue
    }

    /// The workspace `working_dir` belongs to (SPEC R2.7.2).
    pub fn workspace_key_for(&self, working_dir: &std::path::Path) -> std::path::PathBuf {
        workspace_queue::workspace_key(working_dir, &self.sandbox.scopes())
    }

    /// Which lane a call runs in (SPEC R2.7.4): what the MTDF definition
    /// declares, else — for a shell command line run in `working_dir` — what
    /// the classifier says, else exclusive. A read-only verdict the kernel
    /// cannot enforce here is demoted to exclusive: the queue never trusts a
    /// classifier alone.
    pub fn resolve_lane(
        &self,
        args: Option<&Map<String, serde_json::Value>>,
        subcommand_config: Option<&crate::config::SubcommandConfig>,
        working_dir: &std::path::Path,
    ) -> workspace_queue::Lane {
        self.resolve_lane_with_reason(args, subcommand_config, working_dir)
            .0
    }

    /// [`Self::resolve_lane`], with the reason an exclusive shell line takes
    /// its turn (SPEC R2.7.4), for the notice it shows while it waits.
    pub fn resolve_lane_with_reason(
        &self,
        args: Option<&Map<String, serde_json::Value>>,
        subcommand_config: Option<&crate::config::SubcommandConfig>,
        working_dir: &std::path::Path,
    ) -> (workspace_queue::Lane, Option<String>) {
        use workspace_queue::Lane;
        let mut reason = None;
        let lane = subcommand_config
            .and_then(|s| s.concurrency)
            .unwrap_or_else(|| match shell_command_line(args) {
                // Declared by the human to run without the lease (SPEC R2.7.10).
                Some(line)
                    if lane::long_running_entry(line, self.workspace_queue.long_running())
                        .is_some() =>
                {
                    Lane::Service
                }
                // Asked for by the caller (SPEC R2.7.12): the kernel holds the
                // line to it, so the request needs no trust, only enforcement.
                Some(_) if asks_read_only(args) => {
                    if self.sandbox.can_enforce_read_only() {
                        Lane::ReadOnly
                    } else {
                        reason = Some(
                            "`read_only` was asked for, but the kernel cannot hold a command to \
                             the read-only lane here (Windows, or ahma inside another sandbox), \
                             so it takes its turn"
                                .to_string(),
                        );
                        Lane::Exclusive
                    }
                }
                Some(line) => {
                    // A redirection writes the workspace when its file lies in
                    // this workspace or in another repository the session can
                    // write. A scope that is no repository — the harness's own
                    // scratch directory, a cache, temp — is not a workspace
                    // (SPEC R2.7.2).
                    let workspace = self.workspace_key_for(working_dir);
                    let scopes: Vec<std::path::PathBuf> = self
                        .sandbox
                        .scopes()
                        .iter()
                        .filter(|s| s.ancestors().any(|a| a.join(".git").exists()))
                        .cloned()
                        .collect();
                    let writes_workspace =
                        |path: &std::path::Path| lane::writes_inside(path, &workspace, &scopes);
                    let site = lane::RunSite {
                        cwd: working_dir,
                        read_only_enforced: self.sandbox.can_enforce_read_only(),
                        writes_workspace: &writes_workspace,
                    };
                    let lane = lane::classify_shell_command_at(line, &site);
                    if lane == Lane::Exclusive {
                        reason = Some(lane::why_exclusive(line, &site));
                    }
                    lane
                }
                None => Lane::Exclusive,
            });
        if lane == Lane::ReadOnly && !self.sandbox.can_enforce_read_only() {
            let nested = "ahma runs inside another sandbox here, so read-only commands cannot \
                          be held to the read-only lane and every command takes its turn";
            (Lane::Exclusive, Some(nested.to_string()))
        } else {
            (lane, reason)
        }
    }

    /// How long an exclusive shell line holds the workspace when it holds it
    /// only out of doubt (SPEC R2.7.11): nothing in it is known to write. A
    /// lane a tool declared, and every other line, holds it until it ends.
    fn soft_hold_for(
        &self,
        args: Option<&Map<String, serde_json::Value>>,
        subcommand_config: Option<&crate::config::SubcommandConfig>,
        lane: workspace_queue::Lane,
    ) -> Option<Duration> {
        if lane != workspace_queue::Lane::Exclusive
            || subcommand_config.and_then(|s| s.concurrency).is_some()
        {
            return None;
        }
        let line = shell_command_line(args)?;
        lane::held_only_for_doubt(line).then_some(lane::SOFT_HOLD)
    }

    /// Let a run in `lane` into its workspace (SPEC R2.7), now, at arrival.
    ///
    /// A run that may write spends the session's once-grants (SPEC R-PERM.2);
    /// a read-only one cannot use a write grant, so it leaves them for the
    /// command that can. An exclusive run takes its place in line here, so
    /// arrival order is execution order (R2.7.1); `reason` is why it waits its
    /// turn (R2.7.4) and `soft_hold` how long it holds the workspace when it
    /// holds it only out of doubt (R2.7.11).
    fn admit(
        &self,
        lane: workspace_queue::Lane,
        working_dir: &std::path::Path,
        op_id: &str,
        title: &str,
        reason: Option<String>,
        soft_hold: Option<Duration>,
    ) -> workspace_queue::Admission {
        use workspace_queue::{Admission, Lane};
        if lane != Lane::ReadOnly {
            self.sandbox.spend_once_grants();
        }
        match lane {
            Lane::ReadOnly => Admission::ReadOnly,
            Lane::Service => Admission::Service,
            Lane::Exclusive => Admission::Exclusive(
                self.enqueue(working_dir, op_id, title)
                    .map(|t| Box::new(t.with_reason(reason).with_soft_hold(soft_hold))),
            ),
        }
    }

    /// Take a place in the workspace line for an exclusive operation.
    fn enqueue(
        &self,
        working_dir: &std::path::Path,
        op_id: &str,
        title: &str,
    ) -> Option<workspace_queue::Ticket> {
        let key = self.workspace_key_for(working_dir);
        let mut holder = workspace_queue::HolderInfo::new(op_id, title)
            .with_typical_secs(self.monitor.typical_duration_secs(title));
        let declared = self.workspace_queue.source_readers();
        if !declared.is_empty() {
            holder = holder.with_effect(lane::classify_source_effect(title, declared));
        }
        // A command run in a subtree of its workspace can only race edits
        // there (SPEC R2.7.8).
        let cwd = dunce::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf());
        if cwd != key && cwd.starts_with(&key) {
            holder = holder.with_footprint(cwd);
        }
        self.workspace_queue.enqueue(&key, holder)
    }

    /// Sets a custom command executor on the adapter.
    pub fn with_command_executor(mut self, executor: Arc<dyn executor::CommandExecutor>) -> Self {
        self.command_executor = executor;
        self
    }

    /// Sets the scope-grant notifier that auto-detected violations are routed to.
    ///
    /// With a notifier installed, an out-of-scope path rejected by validation, or a
    /// stderr denial from a sandboxed command, raises a "grant access to X?" prompt.
    /// Without one, detection is inert. The notifier never widens the live session;
    /// an approved grant is persisted for the next server start (SPEC R5).
    pub fn with_scope_grant_notifier(
        mut self,
        notifier: Arc<dyn sandbox::ScopeGrantNotifier>,
    ) -> Self {
        self.scope_grant_notifier = Some(notifier);
        self
    }

    /// Serve each command an SSH key broker from `factory` (SPEC R-CRED.1).
    #[cfg(unix)]
    pub fn with_credential_brokers(
        mut self,
        factory: Arc<dyn crate::credentials::ssh_agent::consent::BrokerFactory>,
    ) -> Self {
        self.credential_brokers = Some(factory);
        self
    }

    /// Serve each command a link broker from `factory` (SPEC R-LINK.1). A
    /// command that gets one may connect to its socket in the broker
    /// directory, whatever `[sandbox] ssh_agent` says; that is decided per
    /// command, as its sandbox is built (`links::host::building_for`).
    #[cfg(unix)]
    pub fn with_link_brokers(
        mut self,
        factory: Arc<dyn crate::links::host::LinkBrokerFactory>,
    ) -> Self {
        self.link_brokers = Some(factory);
        self
    }

    /// A whole-line `open <URL>` (SPEC R-LINK.10) as the line ahma runs
    /// instead — the link helper `BROWSER` names, given the address as
    /// checked — and the line that says so. `None` for any other line, or
    /// when no link broker serves this adapter's commands.
    #[cfg(unix)]
    fn link_open_substitute(&self, line: &str) -> Option<(String, String)> {
        use crate::links::policy::{exact_open_command, sh_single_quote};
        let factory = self.link_brokers.as_ref()?;
        let target = exact_open_command(line)?;
        let helper = factory.helper()?;
        let runs = format!(
            "{} {}",
            sh_single_quote(&helper.to_string_lossy()),
            sh_single_quote(target.as_str())
        );
        let said = format!(
            "ahma: `{}` ran as `ahma open-url '{}'` (SPEC R-LINK.10).",
            line.trim(),
            target.as_str()
        );
        Some((runs, said))
    }

    /// Validate a working directory against the sandbox scope, raising a scope-grant
    /// prompt (best-effort, never blocking the error) when it is rejected as
    /// out-of-scope. Returns the same `Result` as [`Sandbox::validate_path`](crate::sandbox::Sandbox::validate_path) so
    /// callers keep failing closed.
    async fn validate_working_dir(
        &self,
        working_dir: &str,
        tool: &str,
    ) -> Result<std::path::PathBuf> {
        // Every execution path validates its working directory first, so this is
        // where "a command starts" is observed: the ledger and leases are
        // brought up to date here (SPEC R-PERM.2). Once-grants move on only
        // when the command's lane is known ([`Self::admit`]).
        self.sandbox.begin_command();
        match self
            .sandbox
            .validate_path(std::path::Path::new(working_dir))
        {
            Ok(p) => Ok(p),
            Err(e) => {
                // A scope rejection is a security decision, so it gets the same
                // durable record a runtime denial does — otherwise the two halves
                // of the same story (refused up front vs refused by the kernel)
                // land in different places and only one survives the session.
                audit::record_sandbox_denial(
                    None,
                    std::path::Path::new(working_dir),
                    "working_directory",
                    tool,
                )
                .await;
                sandbox::grant_channel::notify_pre_exec(
                    &self.sandbox,
                    self.scope_grant_notifier.as_ref(),
                    &e,
                    tool,
                )
                .await;
                Err(e)
            }
        }
    }

    /// Run `task` as operation `op_id`'s background task. Its handle is kept
    /// for [`Self::shutdown`] and dropped when the task ends, however it ends:
    /// one place, so no early return can leave a handle behind. The handle is
    /// registered under the lock the task's own removal waits for, so a task
    /// that ends at once cannot be removed before it is registered.
    async fn spawn_operation_task(
        &self,
        op_id: &str,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        // The task ends the operation, so a cancel asks it to (SPEC R2.7.1).
        self.monitor.ends_by_task(op_id).await;
        let handles = self.task_handles.clone();
        let id = op_id.to_string();
        let mut registered = self.task_handles.lock().await;
        let handle = tokio::spawn(async move {
            task.await;
            handles.lock().await.remove(&id);
        });
        registered.insert(op_id.to_string(), handle);
    }

    /// Gracefully shuts down the adapter by cancelling active operations, aborting tasks,
    /// and shutting down shell pools. Uses timeouts to avoid hanging indefinitely.
    pub async fn shutdown(&self) {
        tracing::info!("Adapter shutdown initiated: cancelling operations and aborting tasks");

        // 1) Cancel all known operations tracked by this adapter (best-effort),
        //    together: each cancel waits for its task to end the operation.
        {
            let ids: Vec<String> = self.task_handles.lock().await.keys().cloned().collect();
            futures::future::join_all(ids.iter().map(|op_id| {
                self.monitor
                    .cancel_operation_with_reason(op_id, Some("Adapter shutdown".to_string()))
            }))
            .await;
        }

        // 2) Drain handles then give each task a grace period before aborting
        let drained: Vec<(String, JoinHandle<()>)> = {
            let mut handles = self.task_handles.lock().await;
            handles.drain().collect()
        };
        for (id, handle) in drained {
            drain_task_handle(&id, handle).await;
        }

        // 3) Kill persistent session shells
        self.shell_sessions.shutdown_all().await;

        tracing::info!("Adapter shutdown complete");
    }

    /// Get a reference to the sandbox.
    pub fn sandbox(&self) -> &crate::sandbox::Sandbox {
        &self.sandbox
    }

    /// Get a cloned `Arc` to the sandbox (for passing into spawned tasks).
    pub fn sandbox_arc(&self) -> std::sync::Arc<crate::sandbox::Sandbox> {
        self.sandbox.clone()
    }

    /// Raise a scope-grant request to the human approval surface (the TUI grant
    /// modal, or an actionable log line when no interactive surface is attached),
    /// deduplicated through the shared `GrantCoordinator`. Used by the
    /// `sandbox_grant` tool so the autonomous agent can *request* a grant but
    /// never persist one itself. Returns `true` if a notifier surface received
    /// the request, `false` if none is wired.
    pub async fn request_scope_grant(
        &self,
        path: &std::path::Path,
        access: ahma_common::config::ScopeAccess,
        tool: Option<String>,
    ) -> bool {
        match &self.scope_grant_notifier {
            Some(notifier) => {
                notifier
                    .notify_violation(
                        path,
                        access,
                        ahma_common::scope_grant::GrantReason::PreExecViolation,
                        tool,
                    )
                    .await;
                true
            }
            None => false,
        }
    }

    /// The question ladder this adapter raises grant questions through, when
    /// one is wired (server mode). For a question that is not about a refused
    /// path — `logs_approve` for an out-of-scope log target (SPEC R9.2) — so it
    /// goes through the same broker, dedup and budget as every other, never a
    /// second door.
    pub fn scope_grant_notifier(&self) -> Option<&Arc<dyn sandbox::ScopeGrantNotifier>> {
        self.scope_grant_notifier.as_ref()
    }

    /// Whether this session's automatic prompt budget is spent (SPEC R-PERM.4.5).
    pub fn grant_budget_exhausted(&self) -> bool {
        self.scope_grant_notifier
            .as_ref()
            .is_some_and(|n| n.budget_exhausted())
    }

    /// Whether a grant question that would settle `access` to `path` is
    /// waiting for a human now (SPEC R-PERM.3.8).
    pub fn grant_question_waiting(
        &self,
        path: &std::path::Path,
        access: ahma_common::config::ScopeAccess,
    ) -> bool {
        self.scope_grant_notifier
            .as_ref()
            .is_some_and(|n| n.is_waiting(path, access))
    }

    /// Where a grant question this adapter raised stands: still awaiting a
    /// human, answered (and how), or closed with nobody asked (SPEC R-PERM.9).
    /// `Closed` when no notifier is wired.
    pub fn grant_status(&self, decision_id: &str) -> ahma_common::scope_grant::GrantStatus {
        self.scope_grant_notifier
            .as_ref()
            .map_or(ahma_common::scope_grant::GrantStatus::Closed, |n| {
                n.status(decision_id)
            })
    }

    /// [`Self::request_scope_grant`] with the judgement aids the caller has
    /// (the agent's own stated reason above all). Returns the request that was
    /// raised, so the caller can relay the exact body the human sees; `None`
    /// when nothing was raised (no surface, already asked, refused, or over the
    /// prompt budget — see [`Self::grant_budget_exhausted`]).
    pub async fn request_scope_grant_with(
        &self,
        path: &std::path::Path,
        access: ahma_common::config::ScopeAccess,
        tool: Option<String>,
        agent_claim: Option<&str>,
    ) -> Option<ahma_common::scope_grant::ScopeGrantRequest> {
        let notifier = self.scope_grant_notifier.as_ref()?;
        let context = sandbox::grant_channel::build_context(
            &self.sandbox,
            path,
            None,
            tool.as_deref(),
            None,
            agent_claim,
            access.is_write(),
        );
        notifier
            .notify_violation_with(
                path,
                access,
                ahma_common::scope_grant::GrantReason::PreExecViolation,
                tool,
                context,
            )
            .await
    }

    /// Whether a human declared this shell line long-running (SPEC R2.7.10):
    /// what it starts may stay running after it (R-PROC.5).
    fn is_declared_long_running(&self, line: &str) -> bool {
        lane::long_running_entry(line, self.workspace_queue.long_running()).is_some()
    }

    /// The timeout `[tools] long_running` declares for this command line, if
    /// it is one (SPEC R2.7.10); the caller's own `timeout_seconds` wins.
    pub fn declared_timeout(&self, args: Option<&Map<String, serde_json::Value>>) -> Option<u64> {
        shell_command_line(args)
            .and_then(|line| lane::long_running_entry(line, self.workspace_queue.long_running()))
            .and_then(|entry| entry.timeout_secs)
    }

    /// Asynchronously starts a command, returning an operation ID immediately.
    ///
    /// This method queues the command for execution in a background task. The result will be
    /// reported via the `OperationMonitor` and any registered callbacks.
    ///
    /// # Arguments
    ///
    /// * `tool_name` - The logical name of the tool (for logging/monitoring).
    /// * `command` - The base command to run.
    /// * `args` - Optional arguments for the command.
    /// * `working_directory` - Directory to execute in.
    /// * `timeout` - Optional timeout in seconds.
    ///
    /// # Returns
    ///
    /// Returns a `String` containing the `id` (e.g., "op_123").
    /// The actual command result is not returned here.
    pub async fn execute_async_in_dir(
        &self,
        tool_name: &str,
        command: &str,
        args: Option<Map<String, serde_json::Value>>,
        working_directory: &str,
        timeout: Option<u64>,
    ) -> Result<String> {
        self.execute_async_in_dir_with_options(
            tool_name,
            command,
            working_directory,
            AsyncExecOptions {
                id: None,
                args,
                timeout,
                subcommand_config: None,
                log_monitor_config: None,
            },
        )
        .await
    }

    #[tracing::instrument(skip(self, options))]
    pub async fn execute_async_in_dir_with_options(
        &self,
        tool_name: &str,
        command: &str,
        working_dir: &str,
        options: AsyncExecOptions<'_>,
    ) -> Result<String> {
        // Boxed, as `drive` is: one more unboxed level here nests every
        // server future past the compiler's query depth limit.
        let (operation, prepared) =
            Box::pin(self.prepare_piped(tool_name, command, working_dir, options)).await?;
        Ok(self.drive(operation, prepared).await)
    }

    /// Run a command as a tracked operation and wait for it to end, however
    /// long it takes, returning its result as the caller reads it (SPEC
    /// R2.1): for a caller that cannot collect a result later — a terminal
    /// hook and the CLI one-shot. It runs as every other operation does
    /// (`drive`): queued in its workspace, gated by its mutex group,
    /// watched for drift and the deny tier, visible to `status`, the TUI and
    /// the audit log. Only the waiting is the caller's:
    ///
    /// - its wait for the workspace is announced once
    ///   ([`Self::with_queue_wait_notice`]) and bounded by the command's own
    ///   time limit, after which it is **not** run (SPEC R2.7.1);
    /// - its result is what the command printed, byte for byte, or a typed
    ///   error the caller can read: a `time_limit::LimitReached`, or a
    ///   [`sandbox::SandboxError`] for a refusal;
    /// - dropping the returned future cancels the operation, which kills the
    ///   command's whole process tree: a caller stopped by Ctrl-C or SIGTERM
    ///   takes its command with it.
    ///
    /// `id` names the operation, as the caller already recorded it (the task
    /// vault); `None` mints one. `command` also names the tool in the audit
    /// log.
    ///
    /// Boxed, as `drive` is, so its callers' futures do not nest past the
    /// compiler's query depth limit.
    pub fn run_to_completion<'a>(
        &'a self,
        id: Option<&'a str>,
        command: &'a str,
        args: Option<Map<String, serde_json::Value>>,
        working_dir: &'a str,
        timeout_seconds: Option<u64>,
        subcommand_config: Option<&'a crate::config::SubcommandConfig>,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = Result<String, anyhow::Error>> + Send + 'a>,
    > {
        Box::pin(self.wait_for_completion(
            id,
            command,
            args,
            working_dir,
            timeout_seconds,
            subcommand_config,
        ))
    }

    /// [`Self::run_to_completion`], unboxed.
    async fn wait_for_completion(
        &self,
        id: Option<&str>,
        command: &str,
        args: Option<Map<String, serde_json::Value>>,
        working_dir: &str,
        timeout_seconds: Option<u64>,
        subcommand_config: Option<&crate::config::SubcommandConfig>,
    ) -> Result<String, anyhow::Error> {
        let (operation, mut prepared) = self
            .prepare_piped(
                command,
                command,
                working_dir,
                AsyncExecOptions {
                    id: id.map(str::to_string),
                    args,
                    timeout: timeout_seconds,
                    subcommand_config,
                    log_monitor_config: None,
                },
            )
            .await?;
        let (reply, result) = tokio::sync::oneshot::channel();
        let (done, ended) = tokio::sync::oneshot::channel();
        prepared.waiter = Some(drive::Waiter {
            reply,
            notice: self.queue_wait_notice.clone(),
            done,
        });
        // Armed before the operation is registered: a caller dropped from
        // here on takes the command with it.
        let mut stop = StopOnDrop {
            monitor: self.monitor.clone(),
            op_id: prepared.op_id.clone(),
            token: operation.cancellation_token.clone(),
            armed: true,
        };
        let op_id = self.drive(operation, prepared).await;
        // The task has returned: the workspace is handed on, the ending
        // published and the end audited, so a caller that exits at once
        // leaves a complete record.
        let _ = ended.await;
        stop.armed = false;
        let ended = self.monitor.wait_for_operation_bounded(&op_id, None).await;
        match result.await {
            Ok(result) => result,
            // It ended before it ran: cancelled or given up while queued, or
            // its mutex group's wait gave up. The reason is its result.
            Err(_) => Err(match ended.and_then(|op| op.result) {
                Some(serde_json::Value::String(why)) => anyhow::anyhow!("{why}"),
                Some(other) => anyhow::anyhow!("{}", other["reason"].as_str().unwrap_or("")),
                None => anyhow::anyhow!("Operation {op_id} ended without a result"),
            }),
        }
    }

    /// Everything a piped command needs before it is driven: its working
    /// directory checked, its line prepared, its operation named and its
    /// place in its workspace's line taken on arrival (SPEC R2.7.1).
    async fn prepare_piped(
        &self,
        tool_name: &str,
        command: &str,
        working_dir: &str,
        options: AsyncExecOptions<'_>,
    ) -> Result<(Operation, drive::Prepared)> {
        let AsyncExecOptions {
            id,
            args,
            timeout,
            subcommand_config,
            log_monitor_config,
        } = options;

        // Validate working directory against sandbox scope.
        let safe_wd = self.validate_working_dir(working_dir, tool_name).await?;
        let safe_wd_str = safe_wd.to_string_lossy().into_owned();

        // A whole-line `open <URL>` runs as the link helper (SPEC R-LINK.10):
        // the operation and the audit keep the line as written, the process
        // runs the helper's line, and the operation says so in one alert.
        #[cfg(unix)]
        let (run_args, link_note) = match shell_command_line(args.as_ref())
            .and_then(|line| self.link_open_substitute(line))
        {
            Some((runs, said)) => {
                let mut run_args = args.clone();
                if let Some(map) = run_args.as_mut() {
                    map.insert("command".to_string(), serde_json::Value::String(runs));
                }
                (run_args, Some(said))
            }
            None => (args.clone(), None),
        };
        #[cfg(not(unix))]
        let (run_args, link_note): (
            Option<Map<String, serde_json::Value>>,
            Option<String>,
        ) = (args.clone(), None);

        // Validate command arguments
        let (program_with_subcommand, args_vec) = self
            .prepare_command_and_args(command, run_args.as_ref(), subcommand_config, &safe_wd)
            .await?;
        let keep_leftovers = shell_command_line(args.as_ref())
            .is_some_and(|line| self.is_declared_long_running(line));

        let op_id = id
            .map(|s| s.to_string())
            .unwrap_or_else(|| generate_id(tool_name, command));

        let timeout = timeout.or_else(|| self.declared_timeout(args.as_ref()));
        let timeout_duration = timeout.map(Duration::from_secs);

        let mut operation = Operation::new_with_timeout(
            op_id.clone(),
            tool_name.to_string(),
            format!("{} {:?}", command, args),
            None,
            timeout_duration,
        );
        // Name the operation *here*, where the command is actually known
        // (SPEC R24.7). Every observer downstream renders this title; none of them
        // has to guess one from the operation id, which is how TUI rows used to end
        // up reading `op_41_echo_hello`.
        operation.title = Some(ahma_common::op_identity::title_for(
            tool_name,
            args.as_ref(),
        ));
        operation.cwd = Some(safe_wd_str.clone());
        operation.command = args
            .as_ref()
            .and_then(|a| a.get("command"))
            .and_then(|c| c.as_str())
            .map(str::to_string);
        // Advertise the full-output spill file from the start so `status`
        // callers know where the complete output lives (stdout_tail is a
        // bounded window).  The file is created lazily by the streaming task.
        operation.output_file = Some(spill::operation_spill_path(&op_id));

        // Workspace write queue (SPEC R2.7): the place in line is taken here,
        // synchronously, before anything is spawned.
        let (lane, queue_reason) =
            self.resolve_lane_with_reason(run_args.as_ref(), subcommand_config, &safe_wd);
        let holder_title = operation
            .command
            .clone()
            .or_else(|| operation.title.clone())
            .unwrap_or_else(|| command.to_string());
        let soft_hold = self.soft_hold_for(args.as_ref(), subcommand_config, lane);
        let admission = self.admit(
            lane,
            &safe_wd,
            &op_id,
            &holder_title,
            queue_reason,
            soft_hold,
        );
        let workspace = self.workspace_key_for(&safe_wd);
        let drift_root = workspace_queue::drift_root(&workspace, &self.sandbox.scopes(), &safe_wd);
        // The command line as the caller wrote it (for a shell tool, the shell
        // string; `command` is then just the shell program).
        let line = shell_command_line(args.as_ref())
            .map(str::to_string)
            .unwrap_or_else(|| command.to_string());

        let prepared = drive::Prepared {
            call: drive::AuditCall {
                tool: tool_name.to_string(),
                args,
                working_dir: safe_wd_str,
                program: program_with_subcommand.clone(),
                argv: args_vec.clone(),
            },
            link_note,
            subject: command.to_string(),
            line,
            // A service is expected to write while it runs (SPEC R2.7.6).
            drift: (lane != workspace_queue::Lane::Service).then_some(DriftProbe {
                root: drift_root,
                lane,
            }),
            watch_handoff: self.watches_handoff(lane),
            limit: command_limit(&self.shell_pool, timeout),
            waiter: None,
            run: drive::Run::Piped {
                program: program_with_subcommand,
                args: args_vec,
                log_monitor: log_monitor_config,
                keep_leftovers,
                executor: self.command_executor.clone(),
                scope_grant_notifier: self.scope_grant_notifier.clone(),
            },
            op_id,
            admission,
            working_dir: safe_wd,
            workspace,
        };
        Ok((operation, prepared))
    }

    /// Start `command_str` asynchronously inside a pseudo-terminal (PTY).
    ///
    /// For tools that change behaviour when stdout is not a TTY (colours,
    /// progress bars, interactive prompts).  The PTY merges stderr into the
    /// terminal stream; lifecycle, streaming, spill, and events are identical
    /// to the piped path.
    pub async fn execute_pty_async(
        &self,
        tool_name: &str,
        command_str: &str,
        working_dir: &str,
        timeout: Option<u64>,
        id: Option<String>,
    ) -> Result<String> {
        let safe_wd = self.validate_working_dir(working_dir, tool_name).await?;
        let op_id = id.unwrap_or_else(|| generate_id(tool_name, command_str));
        // A PTY command is an arbitrary shell command: it may write, so it is
        // exclusive (SPEC R2.7), and takes its place in line on arrival.
        let admission = self.admit(
            workspace_queue::Lane::Exclusive,
            &safe_wd,
            &op_id,
            command_str,
            None,
            None,
        );
        // A whole-line `open <URL>` runs as the link helper (SPEC R-LINK.10);
        // the operation keeps the line as written.
        #[cfg(unix)]
        let (run_line, link_note) = match self.link_open_substitute(command_str) {
            Some((runs, said)) => (runs, Some(said)),
            None => (command_str.to_string(), None),
        };
        #[cfg(not(unix))]
        let (run_line, link_note): (String, Option<String>) = (command_str.to_string(), None);

        let mut operation = Operation::new_with_timeout(
            op_id.clone(),
            tool_name.to_string(),
            format!("[pty] {command_str}"),
            None,
            timeout.map(Duration::from_secs),
        );
        // Same identity, same source (SPEC R24.7) — a PTY command is still a command.
        operation.title = Some(ahma_common::op_identity::title_for_value(
            tool_name,
            Some(&serde_json::json!({ "command": command_str })),
        ));
        operation.cwd = Some(safe_wd.to_string_lossy().into_owned());
        operation.command = Some(command_str.to_string());
        operation.output_file = Some(spill::operation_spill_path(&op_id));
        let workspace = self.workspace_key_for(&safe_wd);
        let prepared = drive::Prepared {
            call: drive::AuditCall {
                tool: tool_name.to_string(),
                args: None,
                working_dir: safe_wd.to_string_lossy().into_owned(),
                program: "[pty]".to_string(),
                argv: vec![command_str.to_string()],
            },
            link_note,
            subject: tool_name.to_string(),
            line: command_str.to_string(),
            // This command is exclusive, never a service (SPEC R2.7.6).
            drift: Some(DriftProbe {
                root: workspace_queue::drift_root(&workspace, &self.sandbox.scopes(), &safe_wd),
                lane: workspace_queue::Lane::Exclusive,
            }),
            watch_handoff: self.watches_handoff(workspace_queue::Lane::Exclusive),
            limit: command_limit(&self.shell_pool, timeout),
            waiter: None,
            run: drive::Run::Pty {
                line: run_line,
                keep_leftovers: self.is_declared_long_running(command_str),
            },
            op_id,
            admission,
            working_dir: safe_wd,
            workspace,
        };
        Ok(self.drive(operation, prepared).await)
    }

    /// Start `command_str` asynchronously in the persistent shell session
    /// named `session_id` (created on first use, rooted at `working_dir`).
    ///
    /// Commands with the same `session_id` run sequentially in the SAME
    /// shell, so `cd`, exported variables, and sourced environments persist
    /// between tool calls.
    pub async fn execute_session_async(
        &self,
        tool_name: &str,
        session_id: &str,
        command_str: &str,
        working_dir: &str,
        timeout: Option<u64>,
        id: Option<String>,
    ) -> Result<String> {
        let safe_wd = self.validate_working_dir(working_dir, tool_name).await?;
        let op_id = id.unwrap_or_else(|| generate_id(tool_name, command_str));
        // A session command is an arbitrary shell command: it may write, so it
        // is exclusive (SPEC R2.7), and takes its place in line on arrival.
        let admission = self.admit(
            workspace_queue::Lane::Exclusive,
            &safe_wd,
            &op_id,
            command_str,
            None,
            None,
        );

        let mut operation = Operation::new_with_timeout(
            op_id.clone(),
            tool_name.to_string(),
            format!("[session {session_id}] {command_str}"),
            None,
            timeout.map(Duration::from_secs),
        )
        // Session commands nest under a synthetic `session:<id>` group so
        // observers (TUI task tree) render them as children of the session.
        .with_parent(format!("session:{session_id}"));
        operation.output_file = Some(spill::operation_spill_path(&op_id));
        let workspace = self.workspace_key_for(&safe_wd);
        let prepared = drive::Prepared {
            call: drive::AuditCall {
                tool: tool_name.to_string(),
                args: None,
                working_dir: safe_wd.to_string_lossy().into_owned(),
                program: format!("[session {session_id}]"),
                argv: vec![command_str.to_string()],
            },
            link_note: None,
            subject: tool_name.to_string(),
            line: command_str.to_string(),
            // This command is exclusive, never a service (SPEC R2.7.6).
            drift: Some(DriftProbe {
                root: workspace_queue::drift_root(&workspace, &self.sandbox.scopes(), &safe_wd),
                lane: workspace_queue::Lane::Exclusive,
            }),
            watch_handoff: self.watches_handoff(workspace_queue::Lane::Exclusive),
            limit: command_limit(&self.shell_pool, timeout),
            waiter: None,
            run: drive::Run::Session {
                id: session_id.to_string(),
                sessions: self.shell_sessions.clone(),
            },
            op_id,
            admission,
            working_dir: safe_wd,
            workspace,
        };
        Ok(self.drive(operation, prepared).await)
    }

    /// Parses the command string and arguments into a program and argument list.
    ///
    /// This helper handles complications such as:
    /// - Splitting the base command string (e.g., "python script.py" -> program: "python", args: ["script.py"])
    /// - Converting structured JSON arguments into CLI flags and positional arguments
    /// - Applying subcommand configurations (aliases, hardcoded args)
    /// - Creating temporary files for multi-line string arguments
    async fn prepare_command_and_args(
        &self,
        command: &str,
        args: Option<&Map<String, Value>>,
        subcommand_config: Option<&crate::config::SubcommandConfig>,
        working_dir: &std::path::Path,
    ) -> Result<(String, Vec<String>)> {
        preparer::prepare_command_and_args(
            command,
            args,
            subcommand_config,
            working_dir,
            &self.temp_file_manager,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Extracted execution helpers (batch vs streaming)
// ---------------------------------------------------------------------------

/// What a caller that waits for its command is told, and with what (SPEC
/// R2.1): its result is explained here, once, whichever way it ran.
struct WaitedRun<'a> {
    sandbox: &'a sandbox::Sandbox,
    /// Offered an out-of-scope path a failure names (SPEC R5).
    scope_grant_notifier: Option<&'a Arc<dyn sandbox::ScopeGrantNotifier>>,
    op_id: &'a str,
    /// The command line its failure is explained by.
    command: &'a str,
    working_dir: &'a Path,
    /// What it holds until it has ended: its brokers, whose refusals its
    /// result says.
    guards: &'a guards::RunGuards,
}

/// The result a caller that waits for its command reads, from what the
/// command wrote and how it exited: its output, or an error naming its exit
/// code and output, then what its brokers refused (SPEC R-CRED.10, R-LINK.6)
/// and what it left running (R-PROC.5), and, for a failure, what the sandbox
/// says about it ([`explain_failed_run`]). A typed error — a sandbox denial,
/// an unrunnable program — keeps its type for the caller to read.
async fn waited_result(
    run: &WaitedRun<'_>,
    exit_code: Option<i32>,
    raw: sink::RawOutput,
    denial_tag: Option<&str>,
    ran_for: Duration,
) -> Result<String, anyhow::Error> {
    #[cfg(not(unix))]
    let _ = run.guards;
    // A non-zero exit may be a runtime sandbox denial the kernel did not name;
    // scan stderr and (best-effort, never blocking the result) offer to grant
    // an out-of-scope path it references.
    let stderr = String::from_utf8_lossy(&raw.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&raw.stdout).into_owned();
    let result = interpret_sync_command_output(exit_code, stdout.clone(), stderr.clone());
    // What the SSH key broker refused this command, one line each
    // (SPEC R-CRED.10), on whichever side the result is.
    #[cfg(unix)]
    let result = match run.guards.broker().and_then(broker_refusals) {
        Some(lines) => match result {
            Ok(out) => Ok(format!("{out}\n{lines}")),
            Err(e) => Err(anyhow::anyhow!("{e}\n\n{lines}")),
        },
        None => result,
    };
    // What the link broker refused, once each, unless the helper's own
    // line already reached the output (SPEC R-LINK.6): a tool that ran
    // `$BROWSER` may have swallowed it.
    #[cfg(unix)]
    let result = match run
        .guards
        .links()
        .and_then(|l| link_refusals(l, &stdout, &stderr))
    {
        Some(lines) => match result {
            Ok(out) => Ok(format!("{out}\n{lines}")),
            Err(e) => Err(anyhow::anyhow!("{e}\n\n{lines}")),
        },
        None => result,
    };
    let result = match raw.left_running {
        Some(line) => match result {
            Ok(out) => Ok(format!("{out}\n{line}")),
            Err(e) => Err(anyhow::anyhow!("{e}\n\n{line}")),
        },
        None => result,
    };
    if result.is_err() {
        return explain_failed_run(run, &stdout, &stderr, result, denial_tag, ran_for).await;
    }
    result
}

/// A failed command's result, scanning stderr/stdout for a sandbox denial the
/// kernel did not name: the waiting caller's twin of
/// `record_failure_diagnostics`, which says the same as operation alerts.
async fn explain_failed_run(
    run: &WaitedRun<'_>,
    stdout: &str,
    stderr: &str,
    result: Result<String, anyhow::Error>,
    denial_tag: Option<&str>,
    ran_for: Duration,
) -> Result<String, anyhow::Error> {
    let WaitedRun {
        sandbox,
        scope_grant_notifier,
        op_id,
        command,
        working_dir: safe_wd,
        ..
    } = *run;
    // A program that refuses any sandbox by its own check outranks every record (SPEC
    // R-ESCAPE.1): the kernel refused nothing that mattered.
    let result = match self_refused(result, stderr, stdout) {
        Ok(fatal) => return Err(fatal),
        Err(result) => result,
    };
    // The kernel's own record decides where it can be read (SPEC R-DENY).
    if let Some(report) = sandbox::grant_channel::kernel_denial_lines(
        sandbox,
        scope_grant_notifier,
        (stderr, stdout),
        denial_tag,
        ran_for,
        command,
        Some(op_id),
    )
    .await
    {
        return kernel_report_result(
            sandbox,
            report,
            result,
            op_id,
            command,
            sandbox::launch_services::refused_link(stderr, stdout).as_deref(),
        )
        .await;
    }
    let result = match fatal_unrunnable(result, stderr, stdout) {
        Ok(fatal) => return Err(fatal),
        Err(result) => result,
    };
    sandbox::grant_channel::raise_stderr_denial_question(
        sandbox,
        scope_grant_notifier,
        stderr,
        stdout,
        command,
        Some(safe_wd),
        Some(op_id),
    );
    // When the failure was a kernel denial on an out-of-scope path, return
    // it as a typed error so the MCP boundary attaches a structured
    // `sandbox_denial` payload (path + grant->restart->retry remediation)
    // instead of leaving the agent with a raw `os error 1`.
    let hit = sandbox::scan_denial_streams(stderr, stdout)
        .filter(|hit| !sandbox.is_path_in_scope_in_dir(&hit.path, safe_wd));
    if let Some(hit) = hit {
        let target =
            sandbox::grant_channel::resolve_grant_target(&hit.path, Some(safe_wd), sandbox);
        // Same payload, durable copy: the structured error reaches the
        // agent now, the audit line survives the session (SPEC R5.4.7).
        audit::record_sandbox_denial(Some(op_id), &target, hit.access.label(), command).await;
        let details = result.err().map(|e| e.to_string()).unwrap_or_default();
        return Err(sandbox::SandboxError::RuntimeDenial {
            path: target,
            access: hit.access,
            scopes: sandbox.scopes().to_vec(),
            details,
            program: hit.program,
        }
        .into());
    }
    // sccache prints a bare `Operation not permitted` with no path when the
    // cache the user set lies outside the scope; name that directory as the
    // denial, so every harness offers its grant (SPEC R-DOCTOR.9).
    if let Some(dir) = sandbox::build_diagnostics::sccache_cache_outside_scope(
        stderr,
        stdout,
        std::env::var("SCCACHE_DIR").ok(),
        &|dir| sandbox.is_path_in_scope_in_dir(dir, safe_wd),
    ) {
        audit::record_sandbox_denial(
            Some(op_id),
            &dir,
            ahma_common::config::ScopeAccess::Rw.label(),
            command,
        )
        .await;
        let error = result.err().map(|e| e.to_string()).unwrap_or_default();
        let note = sandbox::build_diagnostics::sccache_cache_grant_note(&dir);
        return Err(sandbox::SandboxError::RuntimeDenial {
            path: dir,
            access: ahma_common::config::ScopeAccess::Rw,
            scopes: sandbox.scopes().to_vec(),
            details: format!("{error}\n\n{note}"),
            program: Some("sccache".to_string()),
        }
        .into());
    }
    explain_ungrantable_failure(result, stderr, stdout, &|hint| {
        remediation_in_workspace(hint, sandbox, Some(safe_wd))
    })
}

/// Interpret a completed command's exit and output as success text or an error.
fn interpret_sync_command_output(
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
) -> Result<String, anyhow::Error> {
    if exit_code != Some(0) {
        // Append a remediation line when the failure is sandbox build
        // contamination (in-scope EPERM / sccache), so the CLI user sees what to
        // do instead of a bare `os error 1`. See `sandbox::build_diagnostics`.
        let remediation = sandbox::build_diagnostics::diagnose(&stderr)
            .map(|h| format!("\n\nhint: {}", h.remediation))
            .unwrap_or_default();
        return Err(anyhow::anyhow!(
            "Command failed with exit code {}: stderr: {}, stdout: {}{}",
            exit_code.unwrap_or(-1),
            stderr,
            stdout,
            remediation
        ));
    }
    Ok(combine_stdout_stderr(stdout, stderr))
}

impl From<&Terminal> for audit::Outcome {
    /// The audit record's view of an ending: the same verdict as the state.
    fn from(terminal: &Terminal) -> Self {
        match terminal.status() {
            OperationStatus::Completed => Self::Completed,
            OperationStatus::TimedOut => Self::TimedOut,
            OperationStatus::Cancelled => Self::Cancelled,
            OperationStatus::Failed | OperationStatus::Pending | OperationStatus::InProgress => {
                Self::Failed
            }
        }
    }
}

/// Publishing how a command ended. A runner cannot publish: it returns a
/// [`sink::RunEnd`] and writes through a [`sink::OpSink`], which holds the
/// monitor privately. The caller first gives up the workspace lease and the
/// guards around the run, then publishes, so a caller woken by the terminal
/// state never finds the workspace still held (SPEC R2.7.1).
pub(crate) trait PublishEnding {
    /// Finish the operation, and return what the audit record of the call's
    /// end carries.
    async fn publish(
        self,
        monitor: &OperationMonitor,
        op_id: &str,
    ) -> (audit::Outcome, Option<i32>);
}

impl PublishEnding for Terminal {
    async fn publish(
        self,
        monitor: &OperationMonitor,
        op_id: &str,
    ) -> (audit::Outcome, Option<i32>) {
        let audited = (audit::Outcome::from(&self), self.exit_code());
        monitor.finish(op_id, self).await;
        audited
    }
}

/// A command's time limit: its own `timeout_seconds` when it asked for one,
/// else the configured default. One `Duration` from here on; the paths used
/// to carry it as milliseconds in some places and seconds in others.
fn command_limit(shell_pool: &ShellPoolManager, timeout_secs: Option<u64>) -> Duration {
    timeout_secs
        .map(Duration::from_secs)
        .unwrap_or_else(|| shell_pool.config().command_timeout)
}

/// Report each signature `broker` refuses as an alert on operation `op_id`
/// while it still runs (SPEC R-CRED.10).
#[cfg(unix)]
fn alert_broker_refusals(
    broker: &crate::credentials::ssh_agent::host::BrokerLease,
    monitor: &Arc<OperationMonitor>,
    op_id: &str,
) {
    let (monitor, op_id) = (monitor.clone(), op_id.to_string());
    let runtime = tokio::runtime::Handle::current();
    broker.observe(Arc::new(move |event| {
        if let crate::credentials::ssh_agent::broker::BrokerEvent::Refused { why, .. } = event {
            let (monitor, op_id, why) = (monitor.clone(), op_id.clone(), why.clone());
            runtime.spawn(async move { monitor.append_alert_unique(&op_id, why).await });
        }
    }));
}

/// Tell operation `op_id`, as an alert while it still runs, each line its
/// link broker refuses with and the question it is waiting on (SPEC R-LINK.6).
#[cfg(unix)]
fn watch_links(
    links: &crate::links::host::LinkLease,
    monitor: &Arc<OperationMonitor>,
    op_id: &str,
) {
    let (monitor, op_id) = (monitor.clone(), op_id.to_string());
    let runtime = tokio::runtime::Handle::current();
    links.watch(Arc::new(move |line: &str| {
        let (monitor, op_id, line) = (monitor.clone(), op_id.clone(), line.to_string());
        runtime.spawn(async move { monitor.append_alert_unique(&op_id, line).await });
    }));
}

/// The shell command line of a `run_terminal_command`-style call (`command`
/// plus `c_flag`), which is what the lane classifier and the mutex groups must
/// Whether `run_terminal_command` was asked to run in the read-only lane
/// (`read_only: true`, SPEC R2.7.12).
fn asks_read_only(args: Option<&Map<String, serde_json::Value>>) -> bool {
    args.and_then(|a| a.get("read_only"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// look at — the adapter's own `command` is then only the shell program.
fn shell_command_line(args: Option<&Map<String, serde_json::Value>>) -> Option<&str> {
    let args = args?;
    if args.get("c_flag").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    args.get("command").and_then(|v| v.as_str())
}

/// What ahma says while an operation waits for its workspace (SPEC R2.7.3).
///
/// The waiting command comes first: harnesses merge this line into the
/// command's own output and summarise from its first line, and a notice that
/// opened with the *holder's* command made that summary name the wrong one
/// (field report, 2026-10-31).
pub(crate) fn queued_message(
    waiting: Option<&str>,
    reason: Option<&str>,
    ahead: &[workspace_queue::HolderInfo],
) -> String {
    let who = match waiting {
        Some(cmd) => format!(
            "NOT started — `{}` is waiting for the workspace",
            short_command(cmd)
        ),
        None => "NOT started — waiting for the workspace".to_string(),
    };
    let holders = match ahead {
        [] => "another ahma process holds it".to_string(),
        [only] => format!("{} holds it", only.describe()),
        [first, rest @ ..] => format!("{} and {} more hold it", first.describe(), rest.len()),
    };
    match reason {
        Some(why) => format!("{who}; {holders}. Queued because {why}."),
        None => format!("{who}; {holders}."),
    }
}

/// A command line short enough to name in one notice line.
fn short_command(cmd: &str) -> String {
    let cmd = cmd.trim();
    match cmd.char_indices().nth(80) {
        Some((at, _)) => format!("{}…", &cmd[..at]),
        None => cmd.to_string(),
    }
}

/// How a caller that waits for its command waits for the command's turn in
/// its workspace (SPEC R2.7.1): told once that it waits, and for no longer
/// than the command's own time limit.
struct QueueWait {
    /// The command's time limit, which also bounds its wait for its turn.
    limit: Duration,
    /// Told once when the command has to wait; logged at `info` without one.
    notice: Option<QueueWaitNotice>,
}

/// Wait for a ticket's turn (SPEC R2.7.1), keeping the operation visibly alive
/// meanwhile: queued is not stalled, so the idle-output watchdog must not kill
/// it, and observers see who it waits for. `Err` is how the operation ends
/// without running: cancelled while queued, or, for a caller that waits
/// (`queue`), a turn that did not come within the command's time limit — the
/// command was **not** run, and who held the workspace is named, never a
/// silent hang until the caller gives up.
async fn wait_for_lease(
    ticket: Option<workspace_queue::Ticket>,
    monitor: &Arc<OperationMonitor>,
    op_id: &str,
    cancel: &tokio_util::sync::CancellationToken,
    queue: Option<&QueueWait>,
) -> Result<Option<workspace_queue::Lease>, Terminal> {
    let Some(ticket) = ticket else {
        return Ok(None);
    };
    let started = Instant::now();
    let waiting = ticket.title().to_string();
    let reason = ticket.reason().map(str::to_string);
    let last_ahead = parking_lot::Mutex::new(Vec::new());
    let announced = std::sync::atomic::AtomicBool::new(false);
    let observe = |ahead: &[workspace_queue::HolderInfo]| {
        monitor.try_note_liveness(op_id);
        let message = queued_message(Some(&waiting), reason.as_deref(), ahead);
        if let Some(queue) = queue {
            *last_ahead.lock() = ahead.to_vec();
            if !announced.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let line = format!("ahma: {message}");
                match &queue.notice {
                    Some(notice) => (notice.0)(&line),
                    None => tracing::info!("{line}"),
                }
            }
        }
        monitor.note_progress(op_id, message);
    };
    let acquired = match queue {
        Some(queue) => {
            match tokio::time::timeout(queue.limit, ticket.acquire(cancel, &observe)).await {
                Ok(acquired) => acquired,
                Err(_) => {
                    let ahead = last_ahead.lock().clone();
                    return Err(Terminal::Failed(format!(
                        "Not run: the workspace stayed busy for its whole {}s timeout. {}. \
                         Retry once that finishes, or cancel it.",
                        queue.limit.as_secs(),
                        queued_message(Some(&waiting), reason.as_deref(), &ahead)
                    )));
                }
            }
        }
        None => ticket.acquire(cancel, &observe).await,
    };
    let lease = acquired.map_err(|_| cancelled())?;
    let waited = started.elapsed();
    if waited >= QUEUE_WAIT_WORTH_REPORTING {
        monitor.left_queue(op_id, waited).await;
    }
    Ok(Some(lease))
}

/// Cancels a waited-for operation when its caller stops waiting
/// ([`Adapter::run_to_completion`]): the caller's future was dropped, by a
/// Ctrl-C, a SIGTERM or a caller of its own giving up. The token is
/// signalled at once, which is what the run's executor watches to kill the
/// whole process tree; the recorded cancel, with its reason, follows on the
/// runtime when there is one. Disarmed once the operation has ended.
struct StopOnDrop {
    monitor: Arc<OperationMonitor>,
    op_id: String,
    token: tokio_util::sync::CancellationToken,
    armed: bool,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (monitor, op_id) = (self.monitor.clone(), std::mem::take(&mut self.op_id));
            runtime.spawn(async move {
                monitor
                    .cancel_operation_with_reason(
                        &op_id,
                        Some("its caller stopped waiting".to_string()),
                    )
                    .await
            });
        }
        self.token.cancel();
    }
}

/// Where a caller that waits announces a wait for its workspace; see
/// [`Adapter::with_queue_wait_notice`].
#[derive(Clone)]
struct QueueWaitNotice(Arc<dyn Fn(&str) + Send + Sync>);

impl std::fmt::Debug for QueueWaitNotice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("QueueWaitNotice")
    }
}

/// A wait shorter than this is not worth a line in the result.
const QUEUE_WAIT_WORTH_REPORTING: Duration = Duration::from_millis(500);

/// The unique refusal lines a broker recorded for this command (SPEC R-CRED.10).
#[cfg(unix)]
fn broker_refusal_lines(lease: &crate::credentials::ssh_agent::host::BrokerLease) -> Vec<String> {
    use crate::credentials::ssh_agent::broker::BrokerEvent;
    let mut lines: Vec<String> = Vec::new();
    for event in lease.events() {
        if let BrokerEvent::Refused { why, .. } = event
            && !lines.contains(&why)
        {
            lines.push(why);
        }
    }
    lines
}

/// The lines a broker's refusals leave in a command's result, once each.
#[cfg(unix)]
fn broker_refusals(lease: &crate::credentials::ssh_agent::host::BrokerLease) -> Option<String> {
    let lines = broker_refusal_lines(lease);
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Build one command's sandboxed process with `build`, saying whether a
/// link broker serves it (SPEC R-LINK.1): its profile, and no other, may
/// then reach the broker directory. Windows serves no link broker
/// (R-LINK.12).
fn build_for_this_command<T>(served_links: bool, build: impl FnOnce() -> T) -> T {
    #[cfg(unix)]
    {
        crate::links::host::building_for(served_links, build)
    }
    #[cfg(not(unix))]
    {
        let _ = served_links;
        build()
    }
}

/// `result` with `line` said before it (SPEC R-LINK.10's one line). A typed
/// error (a sandbox denial, a time limit) is left as it is: its caller reads
/// its type, and its own text says what happened.
fn said_first(result: Result<String, anyhow::Error>, line: &str) -> Result<String, anyhow::Error> {
    match result {
        Ok(out) => Ok(format!("{line}\n{out}")),
        Err(e)
            if e.downcast_ref::<sandbox::SandboxError>().is_none()
                && e.downcast_ref::<time_limit::LimitReached>().is_none() =>
        {
            Err(anyhow::anyhow!("{line}\n{e}"))
        }
        Err(e) => Err(e),
    }
}

/// The lines the link broker refused this command (SPEC R-LINK.6), once
/// each, leaving out any the helper already printed into `stdout`/`stderr`:
/// one line per refusal, never two copies, and never none because the tool
/// that ran `$BROWSER` hid its output.
#[cfg(unix)]
fn link_refusals(
    lease: &crate::links::host::LinkLease,
    stdout: &str,
    stderr: &str,
) -> Option<String> {
    let lines: Vec<String> = lease
        .refusal_lines()
        .into_iter()
        .filter(|line| !stdout.contains(line.as_str()) && !stderr.contains(line.as_str()))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// A failed command's result, as the kernel's records describe it (SPEC
/// R-DENY): a grantable denial is the typed [`sandbox::SandboxError::RuntimeDenial`]
/// the MCP boundary turns into a structured `sandbox_denial` payload, with
/// every record's line in its details; other denials are their lines; no
/// records leave the failure as it was, with nothing suggested (R-DENY.3).
/// `link`: the web link a refused LaunchServices launch was for
/// ([`sandbox::launch_services::refused_link`]), which is explained, never
/// offered as a run outside the sandbox ([`explain_refused_link`]).
async fn kernel_report_result(
    sandbox: &sandbox::Sandbox,
    report: sandbox::grant_channel::KernelReport,
    result: Result<String, anyhow::Error>,
    op_id: &str,
    command: &str,
    link: Option<&str>,
) -> Result<String, anyhow::Error> {
    let report = explain_refused_link(report, link);
    if report.lines.is_empty() {
        return result;
    }
    let lines = report.lines.join("\n");
    match report.grant {
        Some((path, access)) => {
            audit::record_sandbox_denial(Some(op_id), &path, access.label(), command).await;
            let details = result.err().map(|e| e.to_string()).unwrap_or_default();
            Err(sandbox::SandboxError::RuntimeDenial {
                path,
                access,
                scopes: sandbox.scopes().to_vec(),
                details: format!("{details}\n\n{lines}"),
                program: report.grant_process,
            }
            .into())
        }
        None if report.unfixable => Err(sandbox::SandboxError::Unfixable {
            details: format!(
                "{}\n\n{lines}",
                result.err().map(|e| e.to_string()).unwrap_or_default()
            ),
        }
        .into()),
        None => match result {
            Ok(out) => Ok(format!("{out}\n\n{lines}")),
            Err(e) => Err(anyhow::anyhow!("{e}\n\n{lines}")),
        },
    }
}

/// What every kernel line for a refused LaunchServices launch contains
/// (`kernel_denials::one_line`).
const LSOPEN_LINE: &str = "refused `lsopen`";

/// The kernel's report for a run whose LaunchServices launch was of a web
/// link (SPEC R6.2.10): the `lsopen` record's line, which offers one run
/// outside the sandbox, gives way to the exact `ahma open-url` that opens
/// the link with the human's consent, and the run is unfixable only if
/// something else no grant allows was refused too. No R-ESCAPE is recorded
/// for it, so no harness asks to run it outside the sandbox.
fn explain_refused_link(
    mut report: sandbox::grant_channel::KernelReport,
    link: Option<&str>,
) -> sandbox::grant_channel::KernelReport {
    let Some(url) = link else {
        return report;
    };
    report.lines.retain(|line| !line.contains(LSOPEN_LINE));
    // Every other unfixable record's line is `Blocked: …` (`one_line`).
    report.unfixable &= report.lines.iter().any(|l| l.starts_with("Blocked: "));
    report.lines.push(sandbox::launch_services::link_note(url));
    report
}

/// The first line of the [`sandbox::SandboxError::Unfixable`] a nested
/// `sandbox-exec` refusal becomes; the hook quotes it in the escape dialog
/// (SPEC R-ESCAPE.1).
const NESTED_SANDBOX_BLOCKED: &str = "Blocked: ahma's kernel sandbox refused a nested \
     `sandbox-exec` (macOS does not allow nested sandboxes).";

/// A failed command's result when it died of something no grant opens, even
/// though other lines of its output name a refused path (SPEC R6.2.13): a
/// refused Mach registration ends a Chromium-based program at startup, and
/// any path also refused in that run is incidental. `Ok` carries that error;
/// `Err` hands the result back unchanged when the run succeeded or shows no
/// such refusal, so the path scan goes on as before.
#[allow(clippy::result_large_err)]
fn fatal_unrunnable(
    result: Result<String, anyhow::Error>,
    stderr: &str,
    stdout: &str,
) -> Result<anyhow::Error, Result<String, anyhow::Error>> {
    let service = match (&result, sandbox::mach::refused_mach_service(stderr, stdout)) {
        (Err(_), Some(service)) => service,
        _ => return Err(result),
    };
    let error = result.err().map(|e| e.to_string()).unwrap_or_default();
    Ok(sandbox::SandboxError::Unfixable {
        details: format!("{error}\n\n{}", sandbox::mach::remediation(&service)),
    }
    .into())
}

/// A failed command's result when the program refused to run inside any sandbox by its own
/// check (`log: Cannot run while sandboxed`, SPEC R-ESCAPE.1): unrunnable, whatever the run's
/// kernel records hold, since the kernel refused nothing that mattered. `Ok` carries that error;
/// `Err` hands the result back unchanged.
#[allow(clippy::result_large_err)]
fn self_refused(
    result: Result<String, anyhow::Error>,
    stderr: &str,
    stdout: &str,
) -> Result<anyhow::Error, Result<String, anyhow::Error>> {
    let program = match (
        &result,
        sandbox::self_refusal::refused_inside_sandbox(stderr, stdout),
    ) {
        (Err(_), Some(program)) => program,
        _ => return Err(result),
    };
    let error = result.err().map(|e| e.to_string()).unwrap_or_default();
    Ok(sandbox::SandboxError::Unfixable {
        details: format!(
            "{error}\n\n{}",
            sandbox::self_refusal::remediation(&program)
        ),
    }
    .into())
}

/// A failed command's result when no grantable path explains it.
///
/// A nested-sandbox refusal is [`sandbox::SandboxError::Unfixable`]: no grant
/// or setting opens it, and Seatbelt leaves no kernel record of it, so its
/// output is the only evidence (SPEC R-ESCAPE.1). Every other recognised
/// failure keeps its error and gains an explanation: a build/credential
/// diagnostic, then a capability the sandbox refused (a `kill`, R6.2.6; a GPU,
/// R6.2.7; a setuid exec; a LaunchServices launch, R6.2.10), so the agent does
/// not go looking for a directory to grant. `complete` turns a build diagnostic
/// into its text for the workspace the command ran in.
fn explain_ungrantable_failure(
    result: Result<String, anyhow::Error>,
    stderr: &str,
    stdout: &str,
    complete: &dyn Fn(&sandbox::build_diagnostics::ContaminationHint) -> String,
) -> Result<String, anyhow::Error> {
    let hint = sandbox::build_diagnostics::diagnose_streams(stderr, stdout);
    if let Some(hint) = &hint
        && hint.kind == sandbox::build_diagnostics::ContaminationKind::NestedSandbox
    {
        let error = result.err().map(|e| format!("{e}\n\n")).unwrap_or_default();
        return Err(sandbox::SandboxError::Unfixable {
            details: format!("{error}{NESTED_SANDBOX_BLOCKED}\n\n{}", hint.remediation),
        }
        .into());
    }
    let notes: Vec<String> = hint
        .map(|h| complete(&h))
        .into_iter()
        .chain(
            sandbox::signal_denial_note(stderr, stdout)
                .or_else(|| sandbox::gpu_denial_note(stderr, stdout))
                .or_else(|| sandbox::setuid_denial_note(stderr, stdout))
                .or_else(|| sandbox::launch_services_denial_note(stderr, stdout)),
        )
        .collect();
    if let Ok(output) = result {
        // A success carries only the GPU note (SPEC R6.2.7): the silent CPU
        // fallback is the one case where a run that "worked" was held back.
        return Ok(match sandbox::gpu_fallback_note(stderr, stdout) {
            Some(note) => format!("{output}\n\n{note}"),
            None => output,
        });
    }
    if notes.is_empty() {
        // Reads like a refusal, but nothing above could name it (SPEC
        // R-DENY.4): say where to look, so the agent does not guess that the
        // human must run it outside the sandbox.
        if reads_like_a_refusal(stderr) || reads_like_a_refusal(stdout) {
            return result.map_err(|e| anyhow::anyhow!("{e}\n\n{QUIET_REFUSAL_NOTE}"));
        }
        return result;
    }
    result.map_err(|e| anyhow::anyhow!("{e}\n\n{}", notes.join("\n\n")))
}

/// The line a failure that reads like a refusal gets when ahma could name
/// nothing to grant (SPEC R-DENY.4).
const QUIET_REFUSAL_NOTE: &str = "ahma: this failure reads like a refusal, but ahma's sandbox \
     recorded nothing it can grant. Before asking the human to run anything outside the \
     sandbox, run `ahma explain blocked`: a helper started elsewhere (a Gradle or Kotlin \
     daemon, sccache), a path inside the workspace, or a missing setting is the usual cause.";

/// Whether `text` complains of a refused operation in the words tools use.
fn reads_like_a_refusal(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("operation not permitted")
        || lower.contains("permission denied")
        || lower.contains("read-only file system")
        || sandbox::denial_scan::says_not_writable(&lower)
}

/// Take the "before" inventory of the trust-handoff deny tier for a command
/// running in `working_dir` (SPEC R6.1.7). The roots are the ones the Seatbelt
/// profile resolves git directories from — every writable scope plus the
/// working directory — so the watched set is the kernel-denied set on macOS.
async fn begin_handoff_watch(sandbox: &sandbox::Sandbox, working_dir: &Path) -> HandoffWatch {
    // Copied out in its own statement: the scope guard is a lock and must not
    // be held across the `.await` below.
    let mut roots: Vec<std::path::PathBuf> = sandbox.scopes().to_vec();
    if !roots.iter().any(|r| r == working_dir) {
        roots.push(working_dir.to_path_buf());
    }
    HandoffWatch::begin(&roots).await
}

/// Compare the deny tier with `watch`'s inventory and, when anything changed,
/// say so the two durable ways — a `warn` and a `handoff_write` audit record
/// per entry (SPEC R-HANDOFF.10). Returns the report for the caller to put in
/// front of the result; `None` when there is nothing to say.
async fn finish_handoff_watch(
    watch: HandoffWatch,
    op_id: &str,
    tool: &str,
) -> Option<HandoffReport> {
    let report = watch.finish().await;
    if report.is_empty() {
        return None;
    }
    for change in &report.changes {
        tracing::warn!(
            operation_id = op_id,
            path = %change.path.display(),
            change = change.kind.wire(),
            "{} {} ({}) — {}",
            sandbox::handoff_watch::ALERT_PREFIX,
            change.path.display(),
            change.kind.as_str(),
            change.trigger
        );
        audit::record_handoff_write(
            Some(op_id),
            &change.path,
            change.kind.wire(),
            change.trigger,
            tool,
        )
        .await;
    }
    for target in &report.capped {
        tracing::warn!(
            operation_id = op_id,
            "{} {} holds more than {} entries; the rest were not compared",
            sandbox::handoff_watch::INCOMPLETE_PREFIX,
            target.display(),
            sandbox::handoff_watch::MAX_INVENTORY_ENTRIES
        );
    }
    Some(report)
}

/// Where and how to look for files that changed while an operation ran
/// (SPEC R2.7.6).
#[derive(Debug, Clone)]
struct DriftProbe {
    root: std::path::PathBuf,
    lane: workspace_queue::Lane,
}

/// Only operations at least this long are probed: a sub-second command had no
/// window worth racing, and the walk is not free.
const DRIFT_MIN_RUN: Duration = Duration::from_secs(2);
/// At most this many changed files are named in a result.
const DRIFT_LIST_LIMIT: usize = 20;
/// The walk visits at most this many entries.
const DRIFT_MAX_ENTRIES: usize = 200_000;

impl DriftProbe {
    /// The `changed_during_run` result field, or `None` when nothing changed
    /// or the run was too short to probe.
    async fn report(&self, ran_for: Duration) -> Option<serde_json::Value> {
        if ran_for < DRIFT_MIN_RUN {
            return None;
        }
        let until = std::time::SystemTime::now();
        let since = until.checked_sub(ran_for)?;
        let root = self.root.clone();
        let exclude = vec![crate::utils::logging::project_log_dir()];
        let changed = tokio::task::spawn_blocking(move || {
            ahma_harness_tools::drift::files_modified_between(
                &root,
                since,
                until,
                &exclude,
                DRIFT_LIST_LIMIT,
                DRIFT_MAX_ENTRIES,
            )
        })
        .await
        .ok()?;
        if changed.is_empty() {
            return None;
        }
        Some(json!({
            "root": self.root.to_string_lossy(),
            "files": changed.paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            "more": changed.more,
            "incomplete": changed.incomplete,
            "lane": self.lane.as_str(),
        }))
    }
}

/// Acquire the command's mutex-group gate, if the command belongs to one.
///
/// The gate every async path waits for, through [`guards::RunGuards::gate`].
///
/// Returns `Ok(None)` when the command isn't gated by any group, `Ok(Some(permit))`
/// once both the in-memory and filesystem locks are held (or the semaphore was
/// unexpectedly closed — logged and treated as ungated), and `Err(message)` when
/// the wait timed out and the caller should fail the operation with `message`.
async fn acquire_mutex_gate(
    mutex_registry: &CommandMutexRegistry,
    monitor: &Arc<OperationMonitor>,
    op_id: &str,
    command: &str,
    workspace: &std::path::Path,
) -> Result<Option<mutex_groups::MutexGroupGuard>, String> {
    let Some(group) = mutex_registry.find_group(command) else {
        return Ok(None);
    };
    let group_name = group.name.clone();
    monitor.note_progress(
        op_id,
        format!(
            "Queued: waiting for '{group_name}' mutex \
             (another {group_name} command is running in this workspace)"
        ),
    );
    match mutex_registry.acquire(group, workspace).await {
        Ok(permit) => {
            tracing::debug!(op_id = %op_id, group = %group_name, "Acquired mutex group gate");
            Ok(Some(permit))
        }
        Err(mutex_groups::MutexGroupError::Timeout { secs, .. }) => Err(format!(
            "Timed out waiting for '{group_name}' exclusive lock after {secs}s. \
             Another {group_name} command is still running in this workspace. \
             Try again after it completes."
        )),
        Err(mutex_groups::MutexGroupError::Closed { .. }) => {
            // Semaphore was closed — should never happen; proceed without gate.
            tracing::warn!("mutex group '{group_name}' semaphore unexpectedly closed");
            Ok(None)
        }
    }
}

/// The ending of an operation that failed before it produced an exit status.
fn failed(why: String) -> Terminal {
    tracing::error!("{}", why);
    Terminal::Failed(why)
}

/// Give a task a 250 ms grace period then abort it if still running,
/// waiting up to 2 s for the abort to complete.
async fn drain_task_handle(id: &str, mut handle: JoinHandle<()>) {
    tracing::debug!("Waiting briefly for task {} to complete...", id);
    match tokio::time::timeout(Duration::from_millis(250), &mut handle).await {
        Ok(Ok(_)) => return,
        Ok(Err(e)) => {
            tracing::debug!("Task {} finished with join error before abort: {:?}", id, e);
            return;
        }
        Err(_) => tracing::debug!("Task {} did not complete in grace period", id),
    }
    tracing::info!("Aborting task {} during shutdown", id);
    handle.abort();
    match tokio::time::timeout(Duration::from_secs(2), &mut handle).await {
        Ok(join_res) => {
            if let Err(e) = join_res {
                tracing::debug!("Task {} aborted with: {:?}", id, e);
            }
        }
        Err(_) => tracing::warn!("Timed out waiting for aborted task {} to finish", id),
    }
}

/// Combine stdout and stderr into a single string, preferring stdout.
fn combine_stdout_stderr(stdout: String, stderr: String) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, false) => stderr,
        (false, false) => format!("{stdout}\n{stderr}"),
        _ => stdout,
    }
}

/// Run one command in a persistent shell session, writing its output to
/// `sink`, and say how it ended. Publishing the ending is the caller's.
#[allow(clippy::too_many_arguments)]
async fn run_session_operation(
    sessions: &crate::shell_session::ShellSessionManager,
    sandbox: &sandbox::Sandbox,
    session_id: &str,
    command_str: &str,
    lease: Option<&str>,
    working_dir: &std::path::Path,
    limit: Duration,
    cancellation_token: &tokio_util::sync::CancellationToken,
    sink: &mut sink::OpSink,
) -> sink::RunEnd {
    // The spill file exists from the start, as for the other paths, even if
    // the session's shell cannot be started.
    sink.started().await;
    // Warned once at 80% of the limit, as a piped command is (SPEC R2.6.6).
    // The session enforces the limit itself (`ShellSession::execute_streaming`).
    let mut near_limit = sink.near_limit(limit);

    // The session protocol streams lines through a sync callback; forward
    // them over a channel so the async side can append/spill as they arrive.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut forward = move |line: String| {
        let _ = line_tx.send(line);
    };
    let exec = sessions.execute_streaming(
        sandbox,
        session_id,
        working_dir,
        crate::shell_session::SessionCommand::new(command_str).under_lease(lease),
        limit,
        &mut forward,
    );
    tokio::pin!(exec);

    let exec_result = loop {
        tokio::select! {
            biased;

            _ = cancellation_token.cancelled() => {
                tracing::info!("Session operation {} cancelled", sink.op_id());
                // Remove the session from the map WITHOUT acquiring the per-session
                // lock.  The exec future (pinned above) holds that lock; calling
                // close_session here would deadlock.  Instead we evict the session
                // from the map so it won't be reused.  When this function returns,
                // `exec` is dropped, the MutexGuard is released, and the ShellSession
                // Arc ref-count reaches zero — at which point kill_on_drop(true) on
                // the child kills the shell and its children.
                sessions.remove_session(session_id).await;
                return sink::RunEnd::Cancelled;
            }

            _ = tokio::time::sleep_until(near_limit.at()), if near_limit.pending() => {
                sink.alert(near_limit.alert()).await;
            }

            line = line_rx.recv() => {
                // Session output is a single merged stream (per-command 2>&1).
                if let Some(line) = line {
                    sink.line(Stream::Stdout, &line).await;
                }
                // None can only happen after exec completes (sender dropped);
                // the exec branch below handles termination.
            }

            result = &mut exec => break result,
        }
    };

    // Drain any lines still buffered in the channel.
    while let Ok(line) = line_rx.try_recv() {
        sink.line(Stream::Stdout, &line).await;
    }

    session_end(exec_result)
}

/// How a session-protocol command ended, from what the session reported.
fn session_end(
    exec_result: Result<Option<i32>, crate::shell_session::SessionError>,
) -> sink::RunEnd {
    use crate::shell_session::SessionError;
    match exec_result {
        Ok(exit_code) => sink::RunEnd::Exited(exit_code),
        // The session stops the command at the limit, so the limit is how
        // long it ran. Says how to raise it, as the piped path does (SPEC
        // R2.6.6).
        Err(SessionError::TimedOut(limit)) => sink::RunEnd::TimedOut { ran_for: limit },
        Err(e @ (SessionError::ShellExited | SessionError::Failed(_))) => {
            sink::RunEnd::Failed(e.to_string())
        }
    }
}

/// The ending of a command stopped at its time limit after `ran_for`, saying
/// how to raise the limit.
fn timed_out(ran_for: Duration) -> Terminal {
    let ran_for_ms = ran_for.as_millis() as u64;
    Terminal::TimedOut(format!(
        "Operation timed out after {}ms (exceeded timeout limit). {}",
        ran_for_ms,
        time_limit::how_to_raise(ran_for_ms.div_ceil(1000))
    ))
}

/// Run a piped command with line-by-line streaming, writing each line to
/// `sink`, and say how it ended.
///
/// This is the single execution path for async operations.  Instead of
/// buffering all output, it spawns the process and reads stdout/stderr
/// concurrently via `BufReader::lines()`.  Each line goes to the sink, which
/// appends it to the operation's tail (emitting `OutputLine` on the unified
/// event stream) and, when a log monitor is configured, checks it for
/// error/warning patterns.
///
/// It has no way to end the operation: the caller adds what it knows to the
/// result, then ends it through its guards, which also write the single
/// `tool_complete` audit event that pairs with the `tool_call` written before
/// the task was spawned. `started` is when the run began, for how long a
/// timed-out command ran.
async fn execute_with_streaming(
    proc_cmd: &mut tokio::process::Command,
    limit: Duration,
    started: Instant,
    cpu_probe_after: Option<Duration>,
    cancellation_token: &tokio_util::sync::CancellationToken,
    sink: &mut sink::OpSink,
) -> sink::RunEnd {
    use tokio::io::{AsyncBufReadExt, BufReader};

    // Ensure stdin/stdout/stderr are strictly isolated (should already be set by sandbox)
    proc_cmd.stdin(std::process::Stdio::null());
    proc_cmd.stdout(std::process::Stdio::piped());
    proc_cmd.stderr(std::process::Stdio::piped());

    let mut guard = match ProcessGroupGuard::spawn(proc_cmd) {
        Ok(guard) => guard,
        Err(e) => {
            let why = format!("Failed to spawn process: {}", e);
            tracing::error!("{}", why);
            return sink::RunEnd::Failed(why);
        }
    };
    // Full-output spill file: the complete record of this operation's output,
    // queryable with file tools long after the bounded tail has rolled over.
    sink.started().await;

    let stdout = guard.child_mut().stdout.take().expect("stdout piped");
    let stderr = guard.child_mut().stderr.take().expect("stderr piped");

    // Read up to each newline as bytes, not with `lines()`: a waiting
    // caller reads what the command printed byte for byte (`OpSink::chunk`),
    // and a byte that is not UTF-8 is replaced instead of ending the stream.
    // `read_until` is cancel-safe in the `select!` below because a partial
    // read stays in its buffer until the newline (or the end) arrives.
    let mut stdout_reader = BufReader::new(stdout);
    let mut stderr_reader = BufReader::new(stderr);
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();

    let timeout_deadline = tokio::time::Instant::now() + limit;
    // Warn once at 80% of the limit (SPEC R2.6.6), so a healthy job near it
    // is not stopped without notice.
    let mut near_limit = sink.near_limit(limit);

    let mut still_running_interval = tokio::time::interval(Duration::from_secs(10));
    still_running_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the first (immediate) tick so heartbeats start at t+10s, not t+0.
    still_running_interval.tick().await;
    let mut elapsed_secs = 0;

    // ── Reap ticker ──────────────────────────────────────────────────────────
    // Once both pipes hit EOF their `next_line()` futures resolve `Ok(None)`
    // immediately and forever, so an unguarded `select!` over them spins a core
    // at 100% for as long as the child lives — a child that closes its pipes but
    // keeps working (a daemonising build, a grandchild holding the tty) does
    // exactly that. The read arms are therefore disabled at EOF, and this ticker
    // takes over as the wake source so the loop still reaches `try_wait()`
    // promptly. It is deliberately much faster than the 10s heartbeat: it bounds
    // how long a finished operation waits to be noticed.
    let mut reap_interval = tokio::time::interval(Duration::from_millis(20));
    reap_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stdout_done = false;
    let mut stderr_done = false;

    // ── Liveness watchdog input ──────────────────────────────────────────────
    // The idle watchdog kills an operation that has produced no output for
    // `idle_timeout`. Silence is not a stall: `… | tail` buffers everything until
    // EOF, a compile can be quiet for minutes. So once an operation has gone
    // quiet we ask whether its process *tree* is still burning CPU, and treat
    // that as proof of life. Sampling is deferred until the operation is actually
    // silent, so a chatty command never pays for it.
    let child_pid = guard.child_ref().id();
    let mut last_output_at = tokio::time::Instant::now();
    let mut last_cpu_ms: Option<u64> = None;

    loop {
        tokio::select! {
            // Bias stderr to prioritize error-related output
            biased;

            // Check cancellation
            _ = cancellation_token.cancelled() => {
                tracing::info!("Operation {} cancelled during streaming", sink.op_id());
                if !kill_process_tree(guard.child_mut()).await {
                    tracing::warn!("Operation {} cancelled but its process did not reap cleanly", sink.op_id());
                }
                return sink::RunEnd::Cancelled;
            }

            _ = tokio::time::sleep_until(near_limit.at()), if near_limit.pending() => {
                sink.alert(near_limit.alert()).await;
            }

            // Timeout
            _ = tokio::time::sleep_until(timeout_deadline) => {
                tracing::warn!("Operation {} timed out during streaming", sink.op_id());
                if !kill_process_tree(guard.child_mut()).await {
                    tracing::warn!("Operation {} timed out but its process did not reap cleanly", sink.op_id());
                }
                return sink::RunEnd::TimedOut { ran_for: started.elapsed() };
            }

            _ = still_running_interval.tick() => {
                elapsed_secs += 10;
                let elapsed_msg = format!("still running ({}s elapsed)", elapsed_secs);
                tracing::info!("Operation {}: {}", sink.op_id(), elapsed_msg);
                sink.progress(elapsed_msg);

                // Silent for a while? Ask whether the process tree is still working
                // before the idle watchdog is allowed to call it wedged.
                //
                // The kill decision is NOT made here. `OperationMonitor::check_timeouts`
                // owns the idle watchdog and measures it from `last_activity`; this
                // loop's only job is to feed that clock proof of life it cannot see
                // for itself. A second watchdog with its own clock would not just be
                // redundant, it would be wrong: `last_output_at` is a monotonic
                // `Instant`, so it misses the suspend forgiveness `note_monitor_tick`
                // applies, and a laptop sleep would reap a healthy operation.
                if let Some(probe_after) = cpu_probe_after
                    && last_output_at.elapsed() >= probe_after
                    && let Some(pid) = child_pid
                    // `process_tree_cpu_ms` answers `None` only when the pid is gone;
                    // where the platform cannot report CPU it answers `Some(0)`, which
                    // reads here as "no proof of life" and lets the watchdog run. That
                    // is the documented floor, not a silent kill.
                    && let Some(cpu_ms) = crate::utils::process_cpu::process_tree_cpu_ms(pid)
                {
                    // Only a *rise* counts, and the first sample is a baseline rather
                    // than evidence: a tree pinned on a lock or a denied write holds
                    // its accumulated total flat, and must still be reaped.
                    if last_cpu_ms.is_some_and(|prev| cpu_ms > prev) {
                        tracing::debug!(
                            "Operation {}: silent for {}s but its process tree consumed CPU \
                             ({}ms -> {}ms) — still working, not stalled",
                            sink.op_id(),
                            last_output_at.elapsed().as_secs(),
                            last_cpu_ms.unwrap_or(0),
                            cpu_ms
                        );
                        sink.alive().await;
                    }
                    last_cpu_ms = Some(cpu_ms);
                }
            }

            // Wake the loop while both pipes are at EOF but the child is still
            // alive, so the `try_wait()` below still runs without spinning.
            _ = reap_interval.tick(), if stdout_done && stderr_done => {}

            // Read stderr line
            result = stderr_reader.read_until(b'\n', &mut stderr_buf), if !stderr_done => {
                // EOF is not output. A child that closes its pipes but keeps
                // running yields EOF immediately and forever; treating that
                // as activity would hold the CPU probe permanently off.
                //
                // Real output also drops the CPU baseline, so the next quiet
                // stretch measures a rise from *its own* start. Carrying a
                // sample across an intervening chatty period would compare
                // against a stale total and fabricate proof of life.
                if handle_stream_chunk(result, &mut stderr_buf, Stream::Stderr, sink).await {
                    last_output_at = tokio::time::Instant::now();
                    last_cpu_ms = None;
                } else {
                    stderr_done = true;
                }
            }

            // Read stdout line
            result = stdout_reader.read_until(b'\n', &mut stdout_buf), if !stdout_done => {
                if handle_stream_chunk(result, &mut stdout_buf, Stream::Stdout, sink).await {
                    last_output_at = tokio::time::Instant::now();
                    last_cpu_ms = None;
                } else {
                    stdout_done = true;
                }
            }
        }

        // Check if the child process has exited
        // We use try_wait() to avoid blocking — if streams are closed the process may
        // have already exited.
        match guard.child_mut().try_wait() {
            Ok(Some(_status)) => {
                // What the command left running ends with it, and its result
                // says so (SPEC R-PROC.5). Before the drain: a job it left in
                // the background holds these pipes open.
                if let Some(line) = sandbox::leftovers::note(&guard.end_leftovers().await) {
                    sink.left_running(line).await;
                }
                drain_remaining_stream_lines(
                    (&mut stderr_reader, &mut stderr_buf),
                    (&mut stdout_reader, &mut stdout_buf),
                    sink,
                )
                .await;
                break;
            }
            Ok(None) => {
                // Process still running, continue loop
            }
            Err(e) => {
                tracing::warn!("Error checking child status for {}: {}", sink.op_id(), e);
                break;
            }
        }
    }

    let mut child = guard.disarm().unwrap();
    let exit_status = child.wait().await;
    tracing::debug!(
        "Operation {} process exited after {}ms",
        sink.op_id(),
        started.elapsed().as_millis()
    );
    sink::RunEnd::Exited(exit_status.ok().and_then(|s| s.code()))
}

async fn drain_remaining_stream_lines(
    stderr: (
        &mut tokio::io::BufReader<tokio::process::ChildStderr>,
        &mut Vec<u8>,
    ),
    stdout: (
        &mut tokio::io::BufReader<tokio::process::ChildStdout>,
        &mut Vec<u8>,
    ),
    sink: &mut sink::OpSink,
) {
    use tokio::io::AsyncBufReadExt;
    let (reader, buf) = stderr;
    while handle_stream_chunk(
        reader.read_until(b'\n', buf).await,
        buf,
        Stream::Stderr,
        sink,
    )
    .await
    {}
    let (reader, buf) = stdout;
    while handle_stream_chunk(
        reader.read_until(b'\n', buf).await,
        buf,
        Stream::Stdout,
        sink,
    )
    .await
    {}
}

/// Which sandbox layer to attribute a capability denial to, for the disclosure.
///
/// Uses the active confinement probe rather than bare env detection: an IDE sets
/// its markers (`CURSOR_SANDBOX`, `CLAUDECODE`, …) in the environment of the MCP
/// server it launches even though it does **not** wrap that server's executions,
/// so env presence alone would misattribute an ahma-imposed denial to the host.
/// The probe reports a host only when ahma is genuinely confined by it (a write
/// outside every scope was blocked); otherwise ahma is authoritative.
fn capability_enforcing_layer() -> sandbox::EnforcingLayer {
    if sandbox::outer_confinement().is_some() {
        sandbox::EnforcingLayer::HostSandbox
    } else {
        sandbox::EnforcingLayer::Ahma
    }
}

/// A build diagnostic's remediation, completed for the workspace the command ran in.
///
/// A plain function, not inline, because the scopes guard is not `Send` and the
/// callers hold the alert's `.await` right after: the guard must be gone first.
fn remediation_in_workspace(
    hint: &sandbox::build_diagnostics::ContaminationHint,
    sandbox: &sandbox::Sandbox,
    working_dir: Option<&Path>,
) -> String {
    // Copied out so the guard is released before the scope checks take it again.
    let workspace = working_dir
        .map(Path::to_path_buf)
        .or_else(|| sandbox.scopes().first().cloned());
    let base = workspace.clone().unwrap_or_default();
    hint.remediation_for(workspace.as_deref(), &|dir| {
        sandbox.is_path_in_scope_in_dir(dir, &base)
    })
}

/// Diagnose a failed streaming operation's stdout/stderr and surface whatever
/// remediation applies: an out-of-scope sandbox grant, a build-contamination
/// hint, or a non-path capability denial disclosure. Best-effort — each check
/// is independent and only appends an operation alert when it matches.
#[allow(clippy::too_many_arguments)]
async fn record_failure_diagnostics(
    sandbox: &Arc<sandbox::Sandbox>,
    scope_grant_notifier: Option<&Arc<dyn sandbox::ScopeGrantNotifier>>,
    op_monitor: &Arc<OperationMonitor>,
    op_id: &str,
    tool: &str,
    stdout_str: &str,
    stderr_str: &str,
    working_dir: Option<&Path>,
) {
    // The kernel's own record decides where it can be read (SPEC R-DENY):
    // its lines are the alerts; no record means the failure was not the
    // sandbox's, and no path is guessed (R-DENY.3).
    let (tag, ran) = match sandbox::kernel_denials::take_tag(op_id) {
        Some((tag, ran)) => (Some(tag), ran),
        None => (None, Duration::default()),
    };
    // A program that refuses any sandbox by its own check (SPEC R-ESCAPE.1).
    if let Some(program) = sandbox::self_refusal::refused_inside_sandbox(stderr_str, stdout_str) {
        op_monitor
            .append_alert(op_id, sandbox::self_refusal::remediation(&program))
            .await;
        return;
    }
    if let Some(report) = sandbox::grant_channel::kernel_denial_lines(
        sandbox,
        scope_grant_notifier,
        (stderr_str, stdout_str),
        tag.as_deref(),
        ran,
        tool,
        Some(op_id),
    )
    .await
    {
        let report = explain_refused_link(
            report,
            sandbox::launch_services::refused_link(stderr_str, stdout_str).as_deref(),
        );
        if let Some((path, access)) = &report.grant {
            audit::record_sandbox_denial(Some(op_id), path, access.label(), tool).await;
        }
        for line in report.lines {
            op_monitor.append_alert(op_id, line).await;
        }
        if let Some(hint) = sandbox::build_diagnostics::diagnose_streams(stderr_str, stdout_str) {
            let remediation = remediation_in_workspace(&hint, sandbox, working_dir);
            op_monitor.append_alert(op_id, remediation).await;
        }
        return;
    }
    // A refused Mach registration ended the program; any path in the same
    // output is incidental and is not offered (SPEC R6.2.13).
    if let Some(service) = sandbox::mach::refused_mach_service(stderr_str, stdout_str) {
        op_monitor
            .append_alert(op_id, sandbox::mach::remediation(&service))
            .await;
        return;
    }
    // Asked, not awaited (SPEC R-PERM.3.9): the caller still holds the
    // workspace and the operation is not yet finished.
    sandbox::grant_channel::raise_stderr_denial_question(
        sandbox,
        scope_grant_notifier,
        stderr_str,
        stdout_str,
        tool,
        working_dir,
        Some(op_id),
    );

    // An out-of-scope runtime denial cannot be returned as a typed McpError on
    // the async path (the result is delivered later as text), so attach the
    // grant -> restart -> retry remediation as an operation alert. Mirrors the
    // typed `RuntimeDenial` a waiting caller gets (`explain_failed_run`).
    let mut explained = false;
    match out_of_scope_runtime_denial(sandbox, stderr_str, stdout_str, working_dir) {
        Some((target, access)) => {
            alert_out_of_scope_denial(op_monitor, op_id, tool, &target, access).await;
            explained = true;
        }
        None => {
            if let Some(note) = non_path_denial_note(stderr_str, stdout_str) {
                op_monitor.append_alert(op_id, note).await;
                explained = true;
            }
            // The grant flow above only fires for *out-of-scope* denials. The
            // sibling failure — an in-scope EPERM from macOS provenance/sccache
            // contamination — produces no kernel event and would otherwise surface
            // as a bare `os error 1`. Diagnose it and attach the remediation as an
            // alert so the user gets an actionable line, not an errno.
            if let Some(hint) = sandbox::build_diagnostics::diagnose_streams(stderr_str, stdout_str)
            {
                let remediation = remediation_in_workspace(&hint, sandbox, working_dir);
                tracing::warn!(
                    "Operation {} failed with sandbox diagnostic ({:?}): {}",
                    op_id,
                    hint.kind,
                    remediation
                );
                op_monitor.append_alert(op_id, remediation).await;
                explained = true;
            }
        }
    }

    // A non-path capability denial (the sandbox refused the OS credential
    // store, not a filesystem path) has no path to grant, so the scans above
    // cannot help. Turn the opaque failure into the two-door disclosure.
    if let Some(cap) = sandbox::scan_capability_denial(stderr_str) {
        let disclosure = sandbox::capability_denial_disclosure(cap, capability_enforcing_layer());
        tracing::warn!(
            "Operation {} hit a capability denial ({:?}); surfacing disclosure",
            op_id,
            cap
        );
        op_monitor.append_alert(op_id, disclosure).await;
        explained = true;
    }

    // Reads like a refusal, but nothing above named one (SPEC R-DENY.4).
    if !explained && (reads_like_a_refusal(stderr_str) || reads_like_a_refusal(stdout_str)) {
        op_monitor
            .append_alert(op_id, QUIET_REFUSAL_NOTE.to_string())
            .await;
    }
}

/// The denied path and access when the streams name a path outside the sandbox
/// scope, resolved to the directory a grant should offer.
fn out_of_scope_runtime_denial(
    sandbox: &sandbox::Sandbox,
    stderr_str: &str,
    stdout_str: &str,
    working_dir: Option<&Path>,
) -> Option<(std::path::PathBuf, ahma_common::config::ScopeAccess)> {
    let hit = sandbox::scan_denial_streams(stderr_str, stdout_str)?;
    let base_scope = sandbox.scopes().first().cloned();
    let base_wd = working_dir.or(base_scope.as_deref());
    let in_scope = match base_wd {
        Some(wd) => sandbox.is_path_in_scope_in_dir(&hit.path, wd),
        None => sandbox.is_path_in_scope(&hit.path),
    };
    if in_scope {
        return None;
    }
    let target = sandbox::grant_channel::resolve_grant_target(&hit.path, base_wd, sandbox);
    Some((target, hit.access))
}

/// A denial that names no path but a signal, the GPU, a setuid binary or Launch
/// Services: nothing to grant, only something to explain.
fn non_path_denial_note(stderr_str: &str, stdout_str: &str) -> Option<String> {
    sandbox::signal_denial_note(stderr_str, stdout_str)
        .or_else(|| sandbox::gpu_denial_note(stderr_str, stdout_str))
        .or_else(|| sandbox::setuid_denial_note(stderr_str, stdout_str))
        .or_else(|| sandbox::launch_services_denial_note(stderr_str, stdout_str))
}

async fn alert_out_of_scope_denial(
    op_monitor: &OperationMonitor,
    op_id: &str,
    tool: &str,
    target: &Path,
    access: ahma_common::config::ScopeAccess,
) {
    // The alert below is transient; this line is the durable copy of the
    // same structured denial (SPEC R5.4.7).
    audit::record_sandbox_denial(Some(op_id), target, access.label(), tool).await;
    let remediation = sandbox::grant_channel::runtime_denial_remediation(target, access);
    tracing::warn!(
        "Operation {} hit an out-of-scope sandbox denial on {}: {}",
        op_id,
        target.display(),
        remediation
    );
    op_monitor.append_alert(op_id, remediation).await;
}

/// The ending of an operation cancelled before or while it ran.
fn cancelled() -> Terminal {
    Terminal::Cancelled("Operation was cancelled".to_string())
}

/// Handle one `read_until` of a stream into `buf`, inside the streaming
/// select loop or its final drain. Returns whether the stream may have more:
/// `false` at its end or on a read error, which is logged.
///
/// A chunk is written to the sink once its newline arrives, or at the end of
/// the stream for a last line without one. The end can come with bytes
/// already in `buf` from a read the loop cancelled part-way; they are that
/// last line.
async fn handle_stream_chunk(
    result: std::io::Result<usize>,
    buf: &mut Vec<u8>,
    stream: Stream,
    sink: &mut sink::OpSink,
) -> bool {
    let more = match result {
        Ok(0) => false,
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(
                "Error reading {} for {}: {}",
                stream.name(),
                sink.op_id(),
                e
            );
            false
        }
    };
    if !buf.is_empty() && (!more || buf.ends_with(b"\n")) {
        sink.chunk(stream, &std::mem::take(buf)).await;
    }
    more
}

#[cfg(test)]
mod waited_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// A session's error is read by its kind, never its text: a time limit
    /// is a timeout that ran for the limit, anything else a failure carrying
    /// the error's message, and an exit keeps its code (or lack of one).
    #[test]
    fn a_session_result_is_classified_by_its_kind() {
        use crate::shell_session::SessionError;
        use sink::RunEnd;
        let limit = Duration::from_secs(7);
        assert_eq!(session_end(Ok(Some(0))), RunEnd::Exited(Some(0)));
        assert_eq!(session_end(Ok(None)), RunEnd::Exited(None));
        assert_eq!(
            session_end(Err(SessionError::TimedOut(limit))),
            RunEnd::TimedOut { ran_for: limit }
        );
        assert_eq!(
            session_end(Err(SessionError::ShellExited)),
            RunEnd::Failed("session shell exited unexpectedly (EOF)".into())
        );
        // A failure whose text says "timed out" is still a failure.
        assert_eq!(
            session_end(Err(SessionError::Failed(anyhow::anyhow!("read timed out")))),
            RunEnd::Failed("read timed out".into())
        );
    }

    /// SPEC R6.2.7: a command that succeeded but quietly ran on the CPU still
    /// gets the GPU note, and only that note — a test suite that prints
    /// "Operation not permitted" while passing must not be told it was denied.
    #[test]
    fn a_successful_command_keeps_its_output_and_gains_only_the_gpu_note() {
        sandbox::set_allow_gpu(false);
        let out = explain_ungrantable_failure(
            Ok("12 cells ran".to_string()),
            "ggml_metal_init: error: failed to create command queue",
            "",
            &|h| h.remediation.clone(),
        )
        .unwrap();
        assert!(out.starts_with("12 cells ran"), "{out}");
        assert!(
            out.contains("ahma settings set sandbox.allow_gpu true"),
            "{out}"
        );
        let quiet = explain_ungrantable_failure(
            Ok("ok".to_string()),
            "test denies_write ... Operation not permitted (expected)",
            "",
            &|h| h.remediation.clone(),
        )
        .unwrap();
        assert_eq!(quiet, "ok");
    }

    /// Field case: headless Chrome died on a refused Mach registration, and
    /// a path from a neighbouring line (Crashpad's settings file) was offered
    /// as the grant instead. The registration decides: no path is offered.
    /// SPEC R2.7.11: a shell line of programs nothing vouches for is held
    /// only out of doubt; a known writer, a declared lane, or any other lane
    /// is held as before.
    #[test]
    fn only_a_doubtful_exclusive_line_gets_a_soft_hold() {
        let (adapter, _td) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        let line = shell_args;
        use workspace_queue::Lane;
        let doubt = line("gh release create v1");
        assert_eq!(
            adapter.soft_hold_for(Some(&doubt), None, Lane::Exclusive),
            Some(lane::SOFT_HOLD)
        );
        assert_eq!(
            adapter.soft_hold_for(Some(&doubt), None, Lane::Service),
            None
        );
        let writer = line("cargo build");
        assert_eq!(
            adapter.soft_hold_for(Some(&writer), None, Lane::Exclusive),
            None
        );
        let declared = crate::config::SubcommandConfig {
            concurrency: Some(Lane::Exclusive),
            ..Default::default()
        };
        assert_eq!(
            adapter.soft_hold_for(Some(&doubt), Some(&declared), Lane::Exclusive),
            None
        );
    }

    /// SPEC R-ESCAPE.1 (field case 2026-11-13): Apple's `log` checks for a sandbox itself and
    /// prints `log: Cannot run while sandboxed`. No grant helps, so it is unrunnable here and the
    /// escape is asked on the re-run, whatever else the run's kernel records hold.
    #[test]
    fn a_program_that_refuses_any_sandbox_is_unrunnable() {
        let err = self_refused(
            Err(anyhow::anyhow!("exit 64")),
            "log: Cannot run while sandboxed\n",
            "",
        )
        .expect("unrunnable");
        let Some(sandbox::SandboxError::Unfixable { details }) =
            err.downcast_ref::<sandbox::SandboxError>()
        else {
            panic!("expected Unfixable, got {err:?}");
        };
        assert!(details.starts_with("exit 64\n\n"), "{details}");
        assert!(
            details
                .lines()
                .any(|l| l.starts_with("Blocked: `log` refuses to run inside any sandbox")),
            "{details}"
        );
        assert!(self_refused(Ok("fine".into()), "log: Cannot run while sandboxed", "").is_err());
        assert!(self_refused(Err(anyhow::anyhow!("x")), "cat: /x: nope", "").is_err());
    }

    #[test]
    fn a_refused_mach_registration_outranks_every_path_in_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("Application Support").join("settings.dat");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, "").unwrap();
        let stderr = format!(
            "[ERROR:file_io_posix.cc:208] open {}: Operation not permitted (1)\n\
             [FATAL:mach_port_rendezvous_mac.cc:159] Check failed: kr == KERN_SUCCESS. \
             bootstrap_check_in com.google.Chrome.MachPortRendezvousServer.18339: Permission \
             denied (1100)",
            settings.display()
        );
        let err =
            fatal_unrunnable(Err(anyhow::anyhow!("exit 133")), &stderr, "").expect("unrunnable");
        let Some(sandbox::SandboxError::Unfixable { details }) =
            err.downcast_ref::<sandbox::SandboxError>()
        else {
            panic!("expected Unfixable, got {err:?}");
        };
        assert!(details.starts_with("exit 133\n\n"), "{details}");
        // The hook quotes the `Blocked: ` line in the escape dialog.
        assert!(
            details
                .lines()
                .any(|l| l.starts_with("Blocked: Google Chrome can't run")),
            "{details}"
        );
        assert!(fatal_unrunnable(Ok("fine".into()), &stderr, "").is_err());
        assert!(fatal_unrunnable(Err(anyhow::anyhow!("x")), "cat: /x: nope", "").is_err());
    }

    /// SPEC R-DENY.4: a failure that reads like a refusal ahma could not
    /// name gets one line, so the agent looks before it sends the human
    /// outside the sandbox. Field case: AGP's "not writable" came back with
    /// nothing from ahma, and the agent told the human to build in their own
    /// terminal.
    #[test]
    fn a_refusal_ahma_cannot_name_still_gets_a_line() {
        let err = explain_ungrantable_failure(
            Err(anyhow::anyhow!("exit 1")),
            "error: failed to open lockfile: Operation not permitted (os error 1)",
            "",
            &|h| h.remediation.clone(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("ahma explain blocked"), "{err}");
        assert!(err.contains("outside the sandbox"), "{err}");
        let plain = explain_ungrantable_failure(
            Err(anyhow::anyhow!("exit 1")),
            "test result: FAILED. 1 passed; 1 failed",
            "",
            &|h| h.remediation.clone(),
        )
        .unwrap_err()
        .to_string();
        assert!(!plain.contains("ahma explain"), "{plain}");
    }

    #[test]
    fn a_nested_sandbox_refusal_is_unfixable_and_keeps_the_original_error() {
        let stderr = "sandbox-exec: sandbox_apply: Operation not permitted";
        let err = explain_ungrantable_failure(Err(anyhow::anyhow!("exit 65")), stderr, "", &|h| {
            h.remediation.clone()
        })
        .unwrap_err();
        let Some(sandbox::SandboxError::Unfixable { details }) =
            err.downcast_ref::<sandbox::SandboxError>()
        else {
            panic!("expected Unfixable, got {err:?}");
        };
        assert!(details.starts_with("exit 65\n\n"), "{details}");
        // The hook quotes this line in the escape dialog (R-ESCAPE.1).
        assert!(
            details.lines().any(|l| l == NESTED_SANDBOX_BLOCKED),
            "{details}"
        );
        assert!(
            details.contains("IDEPackageSupportDisableManifestSandbox"),
            "{details}"
        );
    }

    #[test]
    fn a_launch_services_refusal_is_explained_but_not_unfixable_on_output_alone() {
        // Only the kernel's record may class `lsopen` unfixable (R-ESCAPE.1):
        // any command can print this line.
        let stderr = "LSOpenURLsWithRole() failed with error -54 for the file /x/App.app.";
        let err = explain_ungrantable_failure(Err(anyhow::anyhow!("exit 1")), stderr, "", &|h| {
            h.remediation.clone()
        })
        .unwrap_err();
        assert!(err.downcast_ref::<sandbox::SandboxError>().is_none());
        assert!(err.to_string().contains("R6.2.10"), "{err}");
    }

    /// SPEC R6.2.10 (amended), R-ESCAPE.1: a LaunchServices launch the kernel
    /// refused is unfixable — the hook then offers to run it outside the
    /// sandbox — unless it was a web link. A link has a narrower way out, so
    /// its result names the exact `ahma open-url`, carries no line offering a
    /// run outside the sandbox, and is not the unfixable error the hook
    /// records as an escape.
    #[tokio::test]
    async fn a_refused_url_open_records_no_escape() {
        use sandbox::kernel_denials::{KernelDenial, classify, one_line};
        let (adapter, _td) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        let denial = KernelDenial::parse_seatbelt("Sandbox: open(12) deny(1) lsopen").unwrap();
        let report = || sandbox::grant_channel::KernelReport {
            lines: vec![one_line(&denial, &classify(&denial))],
            grant: None,
            grant_process: None,
            unfixable: true,
        };
        let failed = || Err(anyhow::anyhow!("exit 1"));
        let stderr =
            "LSOpenURLsWithRole() failed with error -54 for the URL https://github.com/o/r/pull/1.";
        let link = sandbox::launch_services::refused_link(stderr, "");
        assert_eq!(link.as_deref(), Some("https://github.com/o/r/pull/1"));

        let err = kernel_report_result(
            &adapter.sandbox,
            report(),
            failed(),
            "op",
            "open",
            link.as_deref(),
        )
        .await
        .unwrap_err();
        assert!(
            err.downcast_ref::<sandbox::SandboxError>().is_none(),
            "not an escape: {err:?}"
        );
        let text = err.to_string();
        assert!(
            text.contains("`ahma open-url 'https://github.com/o/r/pull/1'`"),
            "{text}"
        );
        assert!(!text.contains(LSOPEN_LINE), "{text}");
        assert!(
            !text.contains("whether to run it outside the sandbox"),
            "{text}"
        );

        // An app is still unfixable: the hook asks to run it once outside.
        let app = kernel_report_result(&adapter.sandbox, report(), failed(), "op", "open", None)
            .await
            .unwrap_err();
        assert!(matches!(
            app.downcast_ref::<sandbox::SandboxError>(),
            Some(sandbox::SandboxError::Unfixable { .. })
        ));
    }

    /// SPEC R-LINK.10: with a link broker serving its commands, a whole-line
    /// `open <URL>` runs as the link helper — the one `BROWSER` names — given
    /// the checked address, and the result says so first. Any other line runs
    /// as written, and without a broker nothing is substituted. Before, the
    /// line ran `open` itself, which the sandbox refuses.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_whole_line_open_runs_as_the_helper() {
        use std::os::unix::fs::PermissionsExt;
        /// Leases nothing; its helper records the arguments it was run with.
        #[derive(Debug)]
        struct Recorded(std::path::PathBuf);
        impl crate::links::host::LinkBrokerFactory for Recorded {
            fn lease_for_command(
                &self,
                _: &std::path::Path,
                _: Option<&str>,
            ) -> Option<crate::links::host::LinkLease> {
                None
            }
            fn helper(&self) -> Option<std::path::PathBuf> {
                Some(self.0.clone())
            }
        }
        let (adapter, td) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        let ws = dunce::canonicalize(td.path()).unwrap();
        let argv = ws.join("argv");
        let helper = ws.join("fake helper");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", argv.display()),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            adapter
                .link_open_substitute("open https://example.com/z")
                .is_none(),
            "no broker, nothing substituted"
        );
        let adapter = adapter.with_link_brokers(Arc::new(Recorded(helper)));
        assert!(
            adapter
                .link_open_substitute("open -a Safari https://x/")
                .is_none()
        );
        let config = crate::AhmaMcpService::build_shell_subcommand_config(
            Some(30),
            &ExecutionMode::Synchronous,
        );
        let out = adapter
            .run_to_completion(
                None,
                crate::shell_pool::platform_shell_program(),
                Some(shell_args("open 'HTTPS://Example.com/z?a=1&b=2'")),
                &ws.to_string_lossy(),
                Some(30),
                Some(&config),
            )
            .await
            .expect("the helper ran");
        assert!(
            out.starts_with(
                "ahma: `open 'HTTPS://Example.com/z?a=1&b=2'` ran as `ahma open-url \
                 'https://example.com/z?a=1&b=2'` (SPEC R-LINK.10)."
            ),
            "{out}"
        );
        assert_eq!(
            std::fs::read_to_string(&argv).unwrap(),
            "https://example.com/z?a=1&b=2\n",
            "one argument, the address as checked"
        );
    }

    /// REGRESSION: the bare sccache failure a hooked `cargo` printed reached the
    /// agent with no explanation at all. It must carry the workspace's hint.
    #[test]
    fn an_sccache_failure_is_explained_with_the_workspace_hint() {
        let stderr = "error: process didn't exit successfully: \
            `sccache /r/bin/rustc -vV` (exit status: 2)\n--- stderr\n\
            sccache: error: Operation not permitted (os error 1)";
        let err = explain_ungrantable_failure(Err(anyhow::anyhow!("exit 101")), stderr, "", &|h| {
            format!("{} [completed]", h.remediation)
        })
        .unwrap_err()
        .to_string();
        assert!(err.starts_with("exit 101\n\n"), "{err}");
        assert!(err.contains("RUSTC_WRAPPER= cargo"), "{err}");
        assert!(err.contains("sccache --show-stats"), "{err}");
        assert!(
            err.ends_with("[completed]"),
            "the workspace completion is used: {err}"
        );
    }

    #[test]
    fn an_unrecognised_failure_passes_through_unchanged() {
        let err = explain_ungrantable_failure(Err(anyhow::anyhow!("exit 2")), "boom", "", &|h| {
            h.remediation.clone()
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "exit 2");
    }

    /// The CPU liveness probe must always fire *inside* the idle budget.
    ///
    /// A fixed 30s threshold would be a latent re-introduction of the very bug this
    /// guards against: configure a shorter idle limit and the watchdog fires before
    /// the first sample is ever taken, so a silent-but-working operation is killed
    /// exactly as before. Scaling with the limit keeps at least two samples (a rise
    /// needs two) inside the budget.
    #[test]
    fn cpu_probe_always_fits_inside_the_idle_budget() {
        // Production default: 300s limit → probe at the 30s cap, not 100s.
        assert_eq!(
            cpu_probe_threshold(Some(Duration::from_secs(300))),
            Some(Duration::from_secs(30))
        );

        // Short limits must scale DOWN, or the watchdog wins the race.
        for limit_secs in [1u64, 5, 15, 60] {
            let limit = Duration::from_secs(limit_secs);
            let probe = cpu_probe_threshold(Some(limit)).expect("watchdog enabled");
            assert!(
                probe < limit,
                "probe ({probe:?}) must fire before the idle limit ({limit:?}) or a \
                 silent-but-busy operation is killed before it is ever sampled"
            );
        }

        // Watchdog disabled: nothing can kill a quiet op, so never pay for a scan.
        assert_eq!(cpu_probe_threshold(None), None);
    }

    // ============= interpret_sync_command_output tests =============

    #[test]
    fn interpret_sync_output_appends_contamination_hint_on_provenance_failure() {
        let err = interpret_sync_command_output(
            Some(1),
            "   Compiling foo".into(),
            "error: error writing dependencies to `/w/target/debug/deps/x.d`: \
                Operation not permitted (os error 1)"
                .into(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("hint:"),
            "expected a remediation hint, got: {err}"
        );
        assert!(err.contains("com.apple.provenance"));
    }

    #[test]
    fn interpret_sync_output_no_hint_for_ordinary_failure() {
        let err = interpret_sync_command_output(
            Some(1),
            String::new(),
            "error[E0382]: borrow of moved value".into(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            !err.contains("hint:"),
            "ordinary failure should not add a hint"
        );
    }

    /// Only exit 0 succeeds: an end by a signal has no exit code, and is a
    /// failure written as exit code -1.
    #[test]
    fn interpret_sync_output_success_and_signal() {
        assert_eq!(
            interpret_sync_command_output(Some(0), "out".into(), "err".into()).unwrap(),
            "out\nerr"
        );
        let err = interpret_sync_command_output(None, String::new(), String::new())
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("Command failed with exit code -1:"),
            "{err}"
        );
    }

    // ============= generate_id tests =============

    #[test]
    fn test_generate_id_increments() {
        let id1 = generate_id("test_tool", "echo 'hello'");
        let id2 = generate_id("test_tool", "echo 'hello'");
        assert!(id1.starts_with("op_"));
        assert!(id2.starts_with("op_"));
        assert_ne!(id1, id2);
    }

    // ============= BoundedLineCollector tests =============

    #[test]
    fn test_bounded_line_collector_empty() {
        let collector = BoundedLineCollector::default();
        assert_eq!(collector.rendered_output(), "");
        assert_eq!(collector.dropped_lines(), 0);
        assert_eq!(collector.dropped_bytes(), 0);
    }

    #[test]
    fn test_bounded_line_collector_basic_push() {
        let mut collector = BoundedLineCollector::default();
        collector.push("hello".to_string());
        collector.push("world".to_string());
        assert_eq!(collector.rendered_output(), "hello\nworld");
        assert_eq!(collector.dropped_lines(), 0);
    }

    #[test]
    fn test_bounded_line_collector_evicts_when_over_line_limit() {
        let mut collector = BoundedLineCollector::default();
        for i in 0..=MAX_STREAM_COLLECTED_LINES {
            collector.push(format!("line {}", i));
        }
        // Should have evicted at least one line
        assert!(collector.dropped_lines() > 0);
        assert!(collector.lines.len() <= MAX_STREAM_COLLECTED_LINES);
    }

    #[test]
    fn test_bounded_line_collector_evicts_when_over_byte_limit() {
        let mut collector = BoundedLineCollector::default();
        // Push lines exceeding byte limit
        let big_line = "x".repeat(MAX_STREAM_COLLECTED_BYTES / 2 + 1);
        collector.push(big_line.clone());
        collector.push(big_line.clone());
        collector.push("small".to_string());
        // After exceeding byte budget, evictions happen
        assert!(collector.dropped_lines() > 0 || collector.dropped_bytes() > 0);
    }

    #[test]
    fn test_bounded_line_collector_rendered_output_with_truncation() {
        let mut collector = BoundedLineCollector::default();
        // Force eviction by exceeding line limit
        for i in 0..MAX_STREAM_COLLECTED_LINES + 10 {
            collector.push(format!("line {}", i));
        }
        let output = collector.rendered_output();
        assert!(output.contains("[output truncated:"));
        assert!(output.contains("dropped"));
    }

    // ============= Adapter construction tests =============

    #[test]
    fn test_adapter_new() {
        let monitor = Arc::new(OperationMonitor::new(
            crate::operation_monitor::MonitorConfig::with_timeout(Duration::from_secs(30)),
        ));
        let shell_pool = Arc::new(ShellPoolManager::new(
            crate::shell_pool::ShellPoolConfig::default(),
        ));
        let td = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(
            crate::sandbox::Sandbox::new(
                vec![td.path().to_path_buf()],
                crate::sandbox::SandboxMode::Test,
                Default::default(),
            )
            .unwrap(),
        );
        Adapter::new(monitor, shell_pool, sandbox).expect("an adapter builds");
    }

    fn adapter_with_mode(mode: crate::sandbox::SandboxMode) -> (Adapter, tempfile::TempDir) {
        let monitor = Arc::new(OperationMonitor::new(
            crate::operation_monitor::MonitorConfig::with_timeout(Duration::from_secs(30)),
        ));
        let shell_pool = Arc::new(ShellPoolManager::new(
            crate::shell_pool::ShellPoolConfig::default(),
        ));
        let td = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(
            crate::sandbox::Sandbox::new(vec![td.path().to_path_buf()], mode, Default::default())
                .unwrap(),
        );
        (Adapter::new(monitor, shell_pool, sandbox).unwrap(), td)
    }

    /// SPEC R2.7.1, R2.7.4: only an exclusive run takes a place in its
    /// workspace's line; a read-only or service run never holds one, and with
    /// the queue off no run does.
    #[test]
    fn only_an_exclusive_run_takes_a_place_in_line() {
        use workspace_queue::{Admission, Lane, WorkspaceQueue};
        let (adapter, td) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        let queued = adapter.with_workspace_queue(WorkspaceQueue::with_lock_dir(true, None));
        let admit = |adapter: &Adapter, lane| adapter.admit(lane, td.path(), "op", "t", None, None);
        for lane in [Lane::ReadOnly, Lane::Service] {
            let admission = admit(&queued, lane);
            assert_eq!(admission.lane(), lane);
            assert!(
                admission.into_ticket().is_none(),
                "{lane:?} takes no place in line"
            );
        }
        let admission = admit(&queued, Lane::Exclusive);
        assert!(
            matches!(admission, Admission::Exclusive(Some(_))),
            "an exclusive run takes its place: {admission:?}"
        );
        let (off, _td) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        assert!(
            matches!(admit(&off, Lane::Exclusive), Admission::Exclusive(None)),
            "with the queue off, no run has a place in line"
        );
    }

    /// A command cancelled after it was registered but before it started
    /// leaves no task handle behind: every other way an async task ends removes
    /// its own, and this one did not, so the handle lived until shutdown.
    #[tokio::test]
    async fn a_command_cancelled_before_it_starts_leaves_no_task_behind() {
        use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
        let (adapter, td) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        let id = adapter
            .execute_async_in_dir_with_options(
                "echo_tool",
                "echo",
                td.path().to_str().unwrap(),
                AsyncExecOptions {
                    id: None,
                    args: None,
                    timeout: None,
                    subcommand_config: None,
                    log_monitor_config: None,
                },
            )
            .await
            .expect("dispatch");
        // On this single-threaded test runtime the operation's task has not run
        // yet: it starts already cancelled.
        assert!(adapter.monitor.cancel_operation(&id).await);
        let deadline = tokio::time::Instant::now() + TestTimeouts::get(TimeoutCategory::Quick);
        while !adapter.task_handles.lock().await.is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the cancelled operation's task handle is still registered"
            );
            tokio::time::sleep(TestTimeouts::poll_interval() / 10).await;
        }
    }

    fn shell_args(command: &str) -> Map<String, serde_json::Value> {
        let mut args = Map::new();
        args.insert("command".into(), json!(command));
        args.insert("c_flag".into(), json!(true));
        args
    }

    /// SPEC R2.7.12: `read_only: true` puts a line the classifier cannot read
    /// (a project script that only waits on CI) in the read-only lane, where
    /// the kernel refuses any workspace write, so it never waits and never
    /// holds an edit back. Where the kernel cannot enforce the lane the request
    /// is not honoured, and the reason says why.
    #[test]
    fn asking_for_read_only_runs_an_unreadable_line_in_the_read_only_lane() {
        use workspace_queue::Lane;
        let (strict, s) = adapter_with_mode(crate::sandbox::SandboxMode::Strict);
        let mut args = shell_args("scripts/wait-for-ci.sh 387");
        assert_eq!(
            strict.resolve_lane(Some(&args), None, s.path()),
            Lane::Exclusive,
            "an unknown program takes its turn"
        );
        args.insert("read_only".into(), serde_json::Value::Bool(true));
        let (lane, reason) = strict.resolve_lane_with_reason(Some(&args), None, s.path());
        if strict.sandbox().can_enforce_read_only() {
            assert_eq!(lane, Lane::ReadOnly);
        } else {
            assert_eq!(lane, Lane::Exclusive);
            assert!(
                reason.as_deref().is_some_and(|r| r.contains("read_only")),
                "the refusal says why: {reason:?}"
            );
        }
    }

    /// SPEC R2.7.4: a reading command line is read-only only where the kernel
    /// can enforce it; without enforcement everything is exclusive.
    #[test]
    fn a_reading_command_is_read_only_only_where_the_kernel_enforces_it() {
        use workspace_queue::Lane;
        let (test_mode, t) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        assert_eq!(
            test_mode.resolve_lane(Some(&shell_args("git status")), None, t.path()),
            Lane::Exclusive,
            "no kernel enforcement, no read-only lane"
        );

        let (strict, s) = adapter_with_mode(crate::sandbox::SandboxMode::Strict);
        let expected = if strict.sandbox().can_enforce_read_only() {
            Lane::ReadOnly
        } else {
            Lane::Exclusive
        };
        assert_eq!(
            strict.resolve_lane(Some(&shell_args("git status")), None, s.path()),
            expected
        );
        assert_eq!(
            strict.resolve_lane(Some(&shell_args("cargo build")), None, s.path()),
            Lane::Exclusive
        );
    }

    /// SPEC R2.7.4: a CI watch whose output goes to a file outside every
    /// workspace takes no lease — on every platform, enforced or not — while
    /// the same watch writing into the workspace still does. The observed
    /// failure: `gh pr checks 113 --watch --interval 60 > <scratchpad>/ci.log`
    /// held the workspace for a twenty-minute CI run.
    #[test]
    fn a_watch_writing_outside_every_workspace_takes_no_lease() {
        use workspace_queue::Lane;
        let scratch = tempfile::tempdir().unwrap();
        let outside = scratch.path().join("ci.log");
        for mode in [
            crate::sandbox::SandboxMode::Test,
            crate::sandbox::SandboxMode::Strict,
        ] {
            let (adapter, ws) = adapter_with_mode(mode);
            let watch = "gh pr checks 113 --watch --interval 60";
            assert_eq!(
                adapter.resolve_lane(
                    Some(&shell_args(&format!("{watch} > '{}'", outside.display()))),
                    None,
                    ws.path(),
                ),
                Lane::Service,
                "{mode:?}"
            );
            assert_eq!(
                adapter.resolve_lane(
                    Some(&shell_args(&format!("{watch} > ci.log"))),
                    None,
                    ws.path()
                ),
                Lane::Exclusive,
                "{mode:?}: a relative file is in the workspace"
            );
            let inside = ws.path().join("ci.log");
            assert_eq!(
                adapter.resolve_lane(
                    Some(&shell_args(&format!("{watch} > '{}'", inside.display()))),
                    None,
                    ws.path()
                ),
                Lane::Exclusive,
                "{mode:?}"
            );
        }
    }

    /// SPEC R2.7.4: an MTDF declaration wins over the classifier, and a
    /// non-shell tool is exclusive unless it declares otherwise.
    #[test]
    fn a_declared_lane_wins_and_undeclared_tools_are_exclusive() {
        use workspace_queue::Lane;
        let (adapter, t) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        let mut sc = crate::mcp_service::AhmaMcpService::build_shell_subcommand_config(
            None,
            &ExecutionMode::AsyncResultPush,
        );
        sc.concurrency = Some(Lane::Service);
        assert_eq!(
            adapter.resolve_lane(Some(&shell_args("npm run dev")), Some(&sc), t.path()),
            Lane::Service
        );
        let mut not_shell = Map::new();
        not_shell.insert("command".into(), json!("git status"));
        assert_eq!(
            adapter.resolve_lane(Some(&not_shell), None, t.path()),
            Lane::Exclusive,
            "only a shell command line (c_flag) is classified"
        );
    }

    /// SPEC R-SETUP.2.2: a command stopped at its time limit keeps what it
    /// printed. The output was thrown away with the reader tasks, so the one
    /// result most likely to say what the command was stuck on said nothing.
    #[tokio::test]
    async fn a_command_stopped_at_its_limit_keeps_its_output() {
        let (adapter, td) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        // `echo` and `sleep` mean the same in sh and PowerShell.
        let line = "echo before-the-limit; sleep 30";
        let config = crate::AhmaMcpService::build_shell_subcommand_config(
            Some(1),
            &ExecutionMode::Synchronous,
        );
        let err = adapter
            .run_to_completion(
                None,
                crate::shell_pool::platform_shell_program(),
                Some(shell_args(line)),
                &td.path().to_string_lossy(),
                Some(1),
                Some(&config),
            )
            .await
            .expect_err("stopped at its one-second limit");
        let stopped = err
            .downcast_ref::<time_limit::LimitReached>()
            .unwrap_or_else(|| panic!("a typed time-limit error, got: {err:#}"));
        assert_eq!(stopped.limit_secs, 1);
        assert!(stopped.output.contains("before-the-limit"), "{stopped:?}");
        let text = err.to_string();
        assert!(text.contains("exceeded timeout limit"), "{text}");
        assert!(text.contains("before-the-limit"), "{text}");
    }

    /// SPEC R-HOOK-SHELL: a hooked Claude Code command runs under the shell
    /// Claude Code itself would use, which on macOS is usually zsh. The line
    /// below is the one macOS's `/bin/bash` 3.2 refused ("unexpected EOF while
    /// looking for matching `''"): an apostrophe in a quoted heredoc inside
    /// `$(…)`. Through the adapter, wrapper and sandbox it must run as zsh.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_heredoc_with_an_apostrophe_inside_a_substitution_runs_under_zsh() {
        let zsh = Path::new("/bin/zsh");
        if !zsh.is_file() {
            eprintln!("skipping: no /bin/zsh on this machine (a platform prerequisite)");
            return;
        }
        let (adapter, td) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        let line = "x=$(cat <<'EOF'\nit's done\nEOF\n); echo \"got: $x\"";
        let config = crate::AhmaMcpService::build_shell_subcommand_config(
            Some(30),
            &ExecutionMode::Synchronous,
        );
        let out = adapter
            .run_to_completion(
                None,
                &zsh.display().to_string(),
                Some(shell_args(line)),
                &td.path().to_string_lossy(),
                Some(30),
                Some(&config),
            )
            .await
            .expect("zsh parses it");
        assert!(out.contains("got: it's done"), "{out}");
    }

    /// SPEC R2.7.10: a command the human declared long-running takes no
    /// lease and gets its own timeout; anything chained to it is judged as
    /// usual.
    #[test]
    fn a_declared_long_running_command_runs_without_the_lease() {
        let (adapter, td) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        let adapter = adapter.with_workspace_queue(
            workspace_queue::WorkspaceQueue::with_lock_dir(true, Some(td.path().join("locks")))
                .with_long_running(vec![ahma_common::config::LongRunningCommand {
                    command: "scripts/audition ask".into(),
                    timeout_secs: Some(14400),
                }]),
        );
        let ask = shell_args("scripts/audition ask --id 3");
        assert_eq!(
            adapter.resolve_lane(Some(&ask), None, td.path()),
            workspace_queue::Lane::Service
        );
        assert_eq!(adapter.declared_timeout(Some(&ask)), Some(14400));
        let chained = shell_args("scripts/audition ask && cargo fmt");
        assert_eq!(
            adapter.resolve_lane(Some(&chained), None, td.path()),
            workspace_queue::Lane::Exclusive
        );
        assert_eq!(adapter.declared_timeout(Some(&chained)), None);
    }

    /// SPEC R-PROC.5: a command that leaves a process running (a script that
    /// starts a server and returns) has it stopped when the command ends, and
    /// its result says which process and how to keep one. The same script,
    /// declared long-running by a human (R2.7.10), keeps what it started.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_ends_what_it_left_running_unless_a_human_lets_it_stay() {
        use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
        let (adapter, td) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        let adapter = adapter.with_workspace_queue(
            workspace_queue::WorkspaceQueue::with_lock_dir(true, Some(td.path().join("locks")))
                .with_long_running(vec![ahma_common::config::LongRunningCommand {
                    command: "sh kept.sh".into(),
                    timeout_secs: None,
                }]),
        );
        // A program that waits, started in the background; the script prints
        // its pid and returns once the program is up (until then it is still
        // the shell's own image, whose environment macOS shows to no one, and
        // this test's sandbox mode has no kernel mark to find it by instead).
        let script = |ready: &str| {
            format!(
                "AHMA_TEST_LEFTOVER_WAITER=1 '{}' sandbox::leftovers::tests::waits_to_be_stopped \
                 --exact --ignored --nocapture >{ready} 2>/dev/null &\necho pid=$!\n\
                 until grep -q waiter-ready {ready}; do sleep 0.05; done\n",
                std::env::current_exe().unwrap().display()
            )
        };
        std::fs::write(td.path().join("leaves.sh"), script("left.ready")).unwrap();
        std::fs::write(td.path().join("kept.sh"), script("kept.ready")).unwrap();
        let limit = TestTimeouts::get(TimeoutCategory::ToolCall).as_secs();
        let config = crate::AhmaMcpService::build_shell_subcommand_config(
            Some(limit),
            &ExecutionMode::Synchronous,
        );
        let run = |line: &'static str| {
            let (adapter, config) = (&adapter, &config);
            let dir = td.path().to_string_lossy().into_owned();
            async move {
                adapter
                    .run_to_completion(
                        None,
                        crate::shell_pool::platform_shell_program(),
                        Some(shell_args(line)),
                        &dir,
                        Some(limit),
                        Some(config),
                    )
                    .await
                    .expect("the script succeeds")
            }
        };
        let pid_in = |out: &str| -> libc::pid_t {
            out.lines()
                .find_map(|l| l.trim().strip_prefix("pid="))
                .and_then(|p| p.trim().parse().ok())
                .unwrap_or_else(|| panic!("the script prints its pid: {out}"))
        };
        // Exited and not yet reaped by its new parent is gone too.
        // SAFETY: signal 0 only asks whether the pid exists.
        let alive = |pid: libc::pid_t| {
            let there = unsafe { libc::kill(pid, 0) == 0 };
            there && !sandbox::leftovers::is_zombie(pid as u32)
        };

        let out = run("sh leaves.sh").await;
        let left = pid_in(&out);
        assert!(out.contains("ahma: stopped"), "{out}");
        assert!(out.contains(&format!("(pid {left})")), "{out}");
        assert!(out.contains("tools.long_running"), "{out}");
        assert!(!alive(left), "what the command left running is gone");

        let out = run("sh kept.sh").await;
        let kept = pid_in(&out);
        assert!(!out.contains("ahma: stopped"), "{out}");
        assert!(alive(kept), "a declared command's process stays");
        // SAFETY: the pid is the waiter this test started.
        unsafe { libc::kill(kept, libc::SIGKILL) };
    }

    /// SPEC R2.7.3, R2.7.4: what a hooked command prints while it waits (the
    /// line a harness merges into its output) opens with that command, names
    /// the holder after it, and says why the command waits at all.
    #[tokio::test]
    async fn a_waiting_shell_line_names_itself_first_and_says_why() {
        let (adapter, td) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        let notices = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let sink = notices.clone();
        let adapter = adapter
            .with_workspace_queue(workspace_queue::WorkspaceQueue::with_lock_dir(
                true,
                Some(td.path().join("locks")),
            ))
            .with_queue_wait_notice(Arc::new(move |line: &str| {
                sink.lock().push(line.to_string())
            }));
        let key = adapter.workspace_key_for(td.path());
        let _held = adapter
            .workspace_queue()
            .enqueue(
                &key,
                workspace_queue::HolderInfo::new("op_holder", "cargo nextest run"),
            )
            .unwrap()
            .acquire(&tokio_util::sync::CancellationToken::new(), &|_| {})
            .await
            .unwrap();

        let line = "python3 -c pass";
        let _ = adapter
            .run_to_completion(
                None,
                crate::shell_pool::platform_shell_program(),
                Some(shell_args(line)),
                &td.path().to_string_lossy(),
                Some(1),
                None,
            )
            .await
            .expect_err("the workspace never frees up");
        let notices = notices.lock();
        let notice = notices.first().expect("one notice");
        assert!(
            notice.starts_with("ahma: NOT started — `"),
            "opens with its own command: {notice}"
        );
        let own = notice.find("python3").expect("names itself");
        let holder = notice.find("op_holder").expect("names the holder");
        assert!(own < holder, "{notice}");
        assert!(
            notice.contains("Queued because `python3 -c` may write"),
            "{notice}"
        );
    }

    /// SPEC R2.7.2, R2.7.4: a redirection into a scope that is not a workspace
    /// — the harness's own scratch directory, a cache or temp directory the
    /// session may write — does not make a reader a workspace writer. A
    /// `gh pr checks --watch > <scratch>/ci.txt` held the workspace for a whole
    /// CI run because the harness scratch directory is a writable scope.
    #[test]
    fn only_a_repository_is_a_workspace_for_a_redirection() {
        let td = tempfile::tempdir().unwrap();
        let ws = td.path().join("ws");
        let other = td.path().join("other");
        let scratch = td.path().join("scratch");
        for d in [&ws, &other] {
            std::fs::create_dir_all(d.join(".git")).unwrap();
        }
        std::fs::create_dir_all(&scratch).unwrap();
        let sandbox = Arc::new(
            crate::sandbox::Sandbox::new(
                vec![ws.clone(), other.clone(), scratch.clone()],
                crate::sandbox::SandboxMode::Test,
                Default::default(),
            )
            .unwrap(),
        );
        let adapter = Adapter::new(
            Arc::new(OperationMonitor::new(
                crate::operation_monitor::MonitorConfig::with_timeout(Duration::from_secs(30)),
            )),
            Arc::new(ShellPoolManager::new(
                crate::shell_pool::ShellPoolConfig::default(),
            )),
            sandbox,
        )
        .unwrap();
        let lane_of = |line: String| adapter.resolve_lane(Some(&shell_args(&line)), None, &ws);
        // Forward slashes: a backslash is a shell escape to the classifier,
        // and every shell ahma runs (PowerShell included) accepts `/`.
        let watch = |to: &std::path::Path| {
            format!(
                "gh pr checks 1 --watch > {}",
                to.join("ci.txt").to_string_lossy().replace('\\', "/")
            )
        };
        assert_eq!(lane_of(watch(&scratch)), workspace_queue::Lane::Service);
        assert_eq!(lane_of(watch(&ws)), workspace_queue::Lane::Exclusive);
        assert_eq!(
            lane_of(watch(&other)),
            workspace_queue::Lane::Exclusive,
            "another repository the session can write is a workspace too"
        );
    }

    /// SPEC R-PERM.2: a `once` write grant is spent by the next command that
    /// may write, not by a read-only one that happens to run first.
    #[test]
    fn a_read_only_command_never_spends_a_once_grant() {
        let (adapter, _td) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        let cache = tempfile::tempdir().unwrap();
        let canon = dunce::canonicalize(cache.path()).unwrap();
        adapter
            .sandbox()
            .add_once_grant(cache.path(), ahma_common::config::ScopeAccess::Rw)
            .unwrap();
        let live = || adapter.sandbox().scopes().iter().any(|s| s == &canon);
        let admit = |lane| adapter.admit(lane, cache.path(), "op", "t", None, None);
        admit(workspace_queue::Lane::ReadOnly);
        admit(workspace_queue::Lane::ReadOnly);
        assert!(live(), "`git status` twice leaves it for the build");
        admit(workspace_queue::Lane::Exclusive);
        assert!(live(), "the build gets it");
        admit(workspace_queue::Lane::Exclusive);
        assert!(!live(), "and the command after it does not");
    }

    /// A grant question that waits on a human never answers.
    #[derive(Debug, Default)]
    struct NeverAnswers {
        asked: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl sandbox::ScopeGrantNotifier for NeverAnswers {
        async fn notify_violation_with(
            &self,
            _path: &std::path::Path,
            _access: ahma_common::config::ScopeAccess,
            _reason: ahma_common::scope_grant::GrantReason,
            _tool: Option<String>,
            _context: ahma_common::scope_grant::GrantContext,
        ) -> Option<ahma_common::scope_grant::ScopeGrantRequest> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::pending().await
        }

        fn budget_exhausted(&self) -> bool {
            false
        }

        fn status(&self, _decision_id: &str) -> ahma_common::scope_grant::GrantStatus {
            ahma_common::scope_grant::GrantStatus::Pending
        }
    }

    /// SPEC R2.7.1, R-PERM.3: a grant question raised by a failed command is a
    /// question, not a wait. The command's result comes back, and the workspace
    /// is handed on, while the human has not answered yet — or a dialog nobody
    /// is looking at would stall every writer in the workspace.
    #[tokio::test]
    async fn a_grant_question_does_not_hold_the_result_or_the_workspace() {
        let (adapter, td) = adapter_with_mode(crate::sandbox::SandboxMode::Test);
        let notifier = Arc::new(NeverAnswers::default());
        let adapter = adapter
            .with_workspace_queue(workspace_queue::WorkspaceQueue::with_lock_dir(
                true,
                Some(td.path().join("locks")),
            ))
            .with_scope_grant_notifier(notifier.clone());
        let outside = crate::test_utils::path_helpers::test_out_of_scope_path();
        let line = format!("echo \"{}: Permission denied\"; exit 3", outside.display());
        let config = crate::AhmaMcpService::build_shell_subcommand_config(
            Some(30),
            &ExecutionMode::Synchronous,
        );

        let quick =
            ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick);
        let wd = td.path().to_string_lossy().into_owned();
        let run = adapter.run_to_completion(
            None,
            crate::shell_pool::platform_shell_program(),
            Some(shell_args(&line)),
            &wd,
            Some(30),
            Some(&config),
        );
        let result = tokio::time::timeout(quick, run)
            .await
            .expect("the failed command's result must not wait for the human's answer");
        assert!(result.is_err(), "the command failed: {result:?}");

        crate::test_utils::concurrency::wait_with_backoff(
            "the grant question is raised",
            quick,
            || async { notifier.asked.load(std::sync::atomic::Ordering::SeqCst) > 0 },
        )
        .await
        .expect("the question is still asked");

        let key = adapter.workspace_key_for(td.path());
        let never_cancelled = tokio_util::sync::CancellationToken::new();
        let next = adapter
            .workspace_queue()
            .enqueue(
                &key,
                workspace_queue::HolderInfo::new("op_next", "cargo build"),
            )
            .unwrap()
            .acquire(&never_cancelled, &|_| {});
        tokio::time::timeout(quick, next)
            .await
            .expect("the next writer gets the workspace while the question is open")
            .unwrap();
    }

    #[test]
    fn the_queued_message_names_who_is_ahead() {
        let one = workspace_queue::HolderInfo::new("op_1", "cargo nextest run");
        let two = workspace_queue::HolderInfo::new("op_2", "cargo build");
        assert!(queued_message(None, None, &[]).contains("another ahma process"));
        assert!(queued_message(None, None, std::slice::from_ref(&one)).contains("op_1"));
        let both = queued_message(None, None, &[one, two]);
        assert!(both.contains("op_1") && both.contains("1 more"), "{both}");
    }

    /// The waiting command comes first, so a log that merges this line into a
    /// command's output (a harness's background file, its summary) names the
    /// command it belongs to, not the holder (field report, 2026-10-31).
    #[test]
    fn the_queued_message_names_the_waiting_command_first_and_why() {
        let holder = workspace_queue::HolderInfo::new("op_9", "cargo nextest run");
        let text = queued_message(
            Some("git status"),
            Some("ahma runs inside another sandbox here"),
            std::slice::from_ref(&holder),
        );
        assert!(
            text.starts_with("NOT started — `git status` is waiting for the workspace"),
            "{text}"
        );
        let own = text.find("git status").unwrap();
        let other = text.find("cargo nextest run").unwrap();
        assert!(own < other, "{text}");
        assert!(text.contains("op `op_9`"), "{text}");
        assert!(
            text.contains("Queued because ahma runs inside another sandbox here"),
            "{text}"
        );
        let anonymous = queued_message(None, None, std::slice::from_ref(&holder));
        assert!(
            anonymous.starts_with("NOT started — waiting for the workspace"),
            "{anonymous}"
        );
        assert!(!anonymous.contains("Queued because"), "{anonymous}");
    }

    #[test]
    fn test_adapter_sandbox_accessors() {
        let monitor = Arc::new(OperationMonitor::new(
            crate::operation_monitor::MonitorConfig::with_timeout(Duration::from_secs(30)),
        ));
        let shell_pool = Arc::new(ShellPoolManager::new(
            crate::shell_pool::ShellPoolConfig::default(),
        ));
        let td = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(
            crate::sandbox::Sandbox::new(
                vec![td.path().to_path_buf()],
                crate::sandbox::SandboxMode::Test,
                Default::default(),
            )
            .unwrap(),
        );
        let adapter = Adapter::new(monitor, shell_pool, sandbox).unwrap();
        // Just verify accessors don't panic
        let _ref = adapter.sandbox();
        let _arc = adapter.sandbox_arc();
    }

    #[tokio::test]
    async fn test_adapter_shutdown_empty() {
        let monitor = Arc::new(OperationMonitor::new(
            crate::operation_monitor::MonitorConfig::with_timeout(Duration::from_secs(30)),
        ));
        let shell_pool = Arc::new(ShellPoolManager::new(
            crate::shell_pool::ShellPoolConfig::default(),
        ));
        let td = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(
            crate::sandbox::Sandbox::new(
                vec![td.path().to_path_buf()],
                crate::sandbox::SandboxMode::Test,
                Default::default(),
            )
            .unwrap(),
        );
        let adapter = Adapter::new(monitor, shell_pool, sandbox).unwrap();
        // Shutdown with no active tasks should complete without error
        adapter.shutdown().await;
    }
}
