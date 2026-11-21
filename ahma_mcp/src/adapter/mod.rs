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
//! To achieve high performance without sacrificing correctness, the adapter chooses
//! between two primary execution paths:
//!
//! 1. **Performance Path (Async)**:
//!    By default, tools execute asynchronously in a spawned task. Results are
//!    tracked via the [`OperationMonitor`](crate::operation_monitor::OperationMonitor)
//!    and pushed back via notifications.
//!
//! 2. **Correctness Path (Synchronous / Direct Spawn)**:
//!    Some operations (like `cargo add` or configuration changes) require immediate
//!    completion to prevent race conditions. When a tool is marked as `synchronous`
//!    or when the `--sync` flag is active, the adapter spawns a direct process,
//!    waiting for it to exit before returning.
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
pub mod executor;
mod guards;
pub mod lane;
pub mod mutex_groups;
mod preparer;
mod pty_exec;
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

use crate::operation_monitor::{
    CommandOutput, Operation, OperationMonitor, OperationStatus, Stream, Terminal,
};
use crate::retry::{self, RetryConfig};
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

/// Result of one prepared synchronous run: what the caller gets back, plus the
/// two facts the audit log needs (how it ended, and with which exit code).
struct SyncRun {
    outcome: audit::Outcome,
    exit_code: Option<i32>,
    result: Result<String, anyhow::Error>,
}

impl SyncRun {
    /// Lead the result with a trust-handoff alert (SPEC R6.1.7). A typed
    /// sandbox denial keeps its type, because the MCP boundary turns it into a
    /// structured `sandbox_denial` payload; the alert then reaches the log and
    /// the audit trail only.
    fn with_handoff_alert(mut self, alert: &str) -> Self {
        self.result = match self.result {
            Ok(output) => Ok(format!("{alert}\n\n{output}")),
            Err(e) if e.downcast_ref::<sandbox::SandboxError>().is_some() => Err(e),
            Err(e) => Err(anyhow::anyhow!("{alert}\n\n{e}")),
        };
        self
    }

    /// A run that never produced an exit status (spawn or sandbox-wrap failure).
    fn failed(err: anyhow::Error) -> Self {
        Self {
            outcome: audit::Outcome::Failed,
            exit_code: None,
            result: Err(err),
        }
    }
}

type OutputReadTask = JoinHandle<std::io::Result<Vec<u8>>>;

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
/// *   **Command Execution**: Executes tools via standard process spawning (async tasks or
///     synchronous direct spawns).
/// *   **Resource Management**: Manages temporary files created for complex arguments and
///     ensures they are cleaned up.
/// *   **Operation Tracking**: Uses `OperationMonitor` to track the status (running, completed,
///     failed) of asynchronous operations.
/// *   **Sandboxing**: Enforces path security by validating operations against a root directory.
/// *   **Retry Logic**: Applies configured retry policies for transient failures.
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
    /// Optional retry configuration for transient error handling.
    retry_config: Option<RetryConfig>,
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
    /// Told once when a synchronous call has to wait for its workspace (SPEC
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
    /// command's `BROWSER` alone. Served on the synchronous path (the
    /// terminal hook's) today.
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
            retry_config: None,
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

    /// Where a synchronous call says that it is waiting for its workspace
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

    /// A command in `lane` is about to run: one that may write spends the
    /// session's once-grants (SPEC R-PERM.2); a read-only one cannot use a
    /// write grant, so it leaves them for the command that can.
    fn command_starts(&self, lane: workspace_queue::Lane) {
        if lane != workspace_queue::Lane::ReadOnly {
            self.sandbox.spend_once_grants();
        }
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

    /// Take a place in the workspace line for an exclusive operation, now —
    /// at arrival — so arrival order is execution order (SPEC R2.7.1).
    fn enqueue_exclusive(
        &self,
        lane: workspace_queue::Lane,
        working_dir: &std::path::Path,
        op_id: &str,
        title: &str,
    ) -> Option<workspace_queue::Ticket> {
        if lane != workspace_queue::Lane::Exclusive {
            return None;
        }
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

    /// Sets retry configuration for transient error handling.
    pub fn with_retry_config(mut self, config: RetryConfig) -> Self {
        self.retry_config = Some(config);
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

    /// The broker lease for a command about to run in `working_dir`, with the
    /// command pointed at it. Held until the command has finished.
    #[cfg(unix)]
    fn lease_credential_broker(
        &self,
        cmd: &mut tokio::process::Command,
        working_dir: &std::path::Path,
        command: Option<&str>,
    ) -> Option<crate::credentials::ssh_agent::host::BrokerLease> {
        let lease = self
            .credential_brokers
            .as_ref()?
            .lease_for_command(working_dir, command)?;
        cmd.env("SSH_AUTH_SOCK", lease.socket());
        Some(lease)
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

    /// The link lease for a command about to run in `working_dir`, taken
    /// before its sandbox is built: whether it has one decides whether its
    /// profile lets it reach the broker directory. Held until the command
    /// has finished.
    #[cfg(unix)]
    fn lease_link_broker(
        &self,
        working_dir: &std::path::Path,
        command: Option<&str>,
    ) -> Option<crate::links::host::LinkLease> {
        self.link_brokers
            .as_ref()?
            .lease_for_command(working_dir, command)
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
        // when the command's lane is known ([`Self::command_starts`]).
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

    /// Returns the retry configuration, if set.
    pub fn retry_config(&self) -> Option<&RetryConfig> {
        self.retry_config.as_ref()
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
    /// path — the `--tmp` request (SPEC R5.2.5, R5.3) — so it goes through the
    /// same broker, dedup and budget as every other, never a second door.
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

    /// Synchronously executes a command and returns the result directly.
    ///
    /// This method bypasses the async operation queue and runs the command directly, waiting for it to complete.
    /// It captures and returns the combined stdout and stderr.
    ///
    /// # Arguments
    ///
    /// * `command` - The command to execute (e.g., "ls", "grep").
    /// * `args` - Optional map of arguments (see `prepare_command_and_args`).
    /// * `working_dir` - Directory to execute the command in. Must be within allowed sandbox scopes.
    /// * `timeout_seconds` - Optional timeout in seconds (overrides default).
    /// * `subcommand_config` - Optional configuration for dealing with subcommands and aliases.
    ///
    /// # Returns
    ///
    /// Returns the command output (stdout + stderr) as a `String` if successful.
    /// Returns an error if execution fails or times out.
    #[tracing::instrument(skip(self, args, subcommand_config), fields(working_dir))]
    pub async fn execute_sync_in_dir(
        &self,
        command: &str,
        args: Option<Map<String, serde_json::Value>>,
        working_dir: &str,
        timeout_seconds: Option<u64>,
        subcommand_config: Option<&crate::config::SubcommandConfig>,
    ) -> Result<String, anyhow::Error> {
        tracing::debug!(
            "execute_sync_in_dir: command='{}', working_dir='{}'",
            command,
            working_dir,
        );

        // Validate working directory against sandbox scope.
        let safe_wd = self.validate_working_dir(working_dir, command).await?;

        // A whole-line `open <URL>` runs as the link helper (SPEC R-LINK.10).
        // The audit keeps the line as written; everything else runs the
        // helper's line.
        #[cfg(unix)]
        let substitute =
            shell_command_line(args.as_ref()).and_then(|line| self.link_open_substitute(line));
        #[cfg(unix)]
        let (args, written, link_note) = match substitute {
            Some((runs, said)) => {
                let written = args.clone();
                let mut args = args;
                if let Some(map) = args.as_mut() {
                    map.insert("command".to_string(), serde_json::Value::String(runs));
                }
                (args, written, Some(said))
            }
            None => (args, None, None),
        };
        #[cfg(not(unix))]
        let (written, link_note): (Option<Map<String, serde_json::Value>>, Option<String>) =
            (None, None);

        let (program, args_vec) = self
            .prepare_command_and_args(command, args.as_ref(), subcommand_config, &safe_wd)
            .await?;

        tracing::info!(
            "Prepared command: program='{}', args={:?}",
            program,
            args_vec
        );

        // Provenance before execution (SPEC R-HANDOFF / audit): the synchronous
        // path has no operation id of its own, so mint one purely so the
        // `tool_call` and its `tool_complete` can be correlated in the log.
        let op_id = generate_id(command, command);
        let safe_wd_str = safe_wd.to_string_lossy().into_owned();

        // Workspace write queue (SPEC R2.7): the synchronous path — terminal
        // hooks, CLI one-shots, `synchronous: true` tools — queues exactly like
        // an async operation, so a hooked `sed -i` never lands in the middle of
        // an MCP `cargo nextest run`.
        let (lane, queue_reason) =
            self.resolve_lane_with_reason(args.as_ref(), subcommand_config, &safe_wd);
        self.command_starts(lane);
        let title = shell_command_line(written.as_ref().or(args.as_ref()))
            .map(str::to_string)
            .unwrap_or_else(|| format!("{program} {}", args_vec.join(" ")));
        let timeout =
            self.sync_timeout(timeout_seconds.or_else(|| self.declared_timeout(args.as_ref())));
        let soft_hold = self.soft_hold_for(args.as_ref(), subcommand_config, lane);
        let lease = match self
            .enqueue_exclusive(lane, &safe_wd, &op_id, &title)
            .map(|t| t.with_reason(queue_reason).with_soft_hold(soft_hold))
        {
            Some(ticket) => Some(self.acquire_for_sync(ticket, timeout).await?),
            None => None,
        };

        audit::record_tool_call(
            &op_id,
            command,
            written.as_ref().or(args.as_ref()),
            &safe_wd_str,
            &program,
            &args_vec,
        )
        .await;

        // SPEC R6.1.7: where the kernel does not hold the deny tier, take its
        // inventory now and compare after the process is gone — before the
        // lease is released, so the next command's writes are not this one's.
        let handoff = if self.watches_handoff(lane) {
            Some(begin_handoff_watch(&self.sandbox, &safe_wd).await)
        } else {
            None
        };

        let start_time = Instant::now();
        let mut run = self
            .run_sync_prepared(
                command,
                &op_id,
                &program,
                &args_vec,
                &safe_wd,
                timeout,
                lane,
                lease.as_ref(),
                shell_command_line(args.as_ref()),
            )
            .await;
        if let Some(watch) = handoff
            && let Some(report) = finish_handoff_watch(watch, &op_id, command).await
        {
            run = run.with_handoff_alert(&report.render_alert());
        }
        drop(lease);
        audit::record_tool_complete(
            &op_id,
            run.outcome,
            start_time.elapsed().as_millis() as u64,
            run.exit_code,
        )
        .await;
        match link_note {
            Some(line) => said_first(run.result, &line),
            None => run.result,
        }
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

    /// The synchronous path's timeout: the caller's, else the pool default.
    fn sync_timeout(&self, timeout_seconds: Option<u64>) -> Duration {
        timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or_else(|| self.shell_pool.config().command_timeout)
    }

    /// Wait for a synchronous call's turn in its workspace (SPEC R2.7.1). The
    /// synchronous path has no operation to show as queued, so the wait is
    /// bounded by the command's own timeout and announced once; a turn that
    /// never comes is an error saying the command did **not** run and who held
    /// the workspace, never a silent hang until the caller gives up.
    async fn acquire_for_sync(
        &self,
        ticket: workspace_queue::Ticket,
        timeout: Duration,
    ) -> Result<workspace_queue::Lease, anyhow::Error> {
        let last_ahead = parking_lot::Mutex::new(Vec::new());
        let announced = std::sync::atomic::AtomicBool::new(false);
        let waiting = ticket.title().to_string();
        let reason = ticket.reason().map(str::to_string);
        let observe = |ahead: &[workspace_queue::HolderInfo]| {
            *last_ahead.lock() = ahead.to_vec();
            if !announced.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let line = format!(
                    "ahma: {}",
                    queued_message(Some(&waiting), reason.as_deref(), ahead)
                );
                match &self.queue_wait_notice {
                    Some(notice) => (notice.0)(&line),
                    None => tracing::info!("{line}"),
                }
            }
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        match tokio::time::timeout(timeout, ticket.acquire(&cancel, &observe)).await {
            Ok(Ok(lease)) => Ok(lease),
            Ok(Err(e)) => Err(anyhow::anyhow!("Not run: {e}")),
            Err(_) => {
                let ahead = last_ahead.lock().clone();
                Err(anyhow::anyhow!(
                    "Not run: the workspace stayed busy for its whole {}s timeout. {}. \
                     Retry once that finishes, or cancel it.",
                    timeout.as_secs(),
                    queued_message(Some(&waiting), reason.as_deref(), &ahead)
                ))
            }
        }
    }

    /// Spawn, wait for, and interpret one prepared synchronous command.
    ///
    /// Split out of [`Self::execute_sync_in_dir`] so that every terminal path —
    /// spawn failure, timeout, non-zero exit, sandbox denial — funnels through a
    /// single return value the caller can turn into exactly one `tool_complete`
    /// audit event.  Recording completion at each `return` instead would be one
    /// forgotten branch away from a `tool_call` that never closes.
    #[allow(clippy::too_many_arguments)]
    async fn run_sync_prepared(
        &self,
        command: &str,
        op_id: &str,
        program: &str,
        args_vec: &[String],
        safe_wd: &std::path::Path,
        timeout: Duration,
        lane: workspace_queue::Lane,
        lease: Option<&workspace_queue::Lease>,
        shell_command: Option<&str>,
    ) -> SyncRun {
        let effective_command = shell_command.unwrap_or(command);
        // Its link broker (SPEC R-LINK.1), held until the command has ended,
        // and leased before its sandbox is built: having one is what lets
        // this command, and no other, reach the broker directory.
        #[cfg(unix)]
        let links = self.lease_link_broker(safe_wd, Some(effective_command));
        #[cfg(unix)]
        let served_links = links.is_some();
        #[cfg(not(unix))]
        let served_links = false;
        // Create sandboxed command.
        // `create_shell_command` is only needed for raw /bin/sh invocations where
        // the caller has NOT already added the -c flag via a subcommand config.
        // For bash/powershell the preparer already embeds -c/-Command; always
        // use create_command so the sandbox wrapper is applied without double-wrapping.
        let built = build_for_this_command(served_links, || {
            if lane == workspace_queue::Lane::ReadOnly {
                self.command_executor.build_read_only_command(
                    &self.sandbox,
                    program,
                    args_vec,
                    safe_wd,
                )
            } else {
                build_sandboxed_command(
                    self.command_executor.as_ref(),
                    &self.sandbox,
                    program,
                    args_vec,
                    safe_wd,
                )
            }
        });
        let mut cmd = match built {
            Ok(cmd) => cmd,
            Err(e) => return SyncRun::failed(e),
        };
        stamp_lease(&mut cmd, lease);
        if shell_command.is_some_and(|line| self.is_declared_long_running(line)) {
            sandbox::leftovers::keep(&mut cmd);
        }
        #[cfg(unix)]
        let broker = self.lease_credential_broker(&mut cmd, safe_wd, Some(effective_command));
        #[cfg(unix)]
        if let Some(links) = &links {
            for (key, value) in links.env() {
                cmd.env(key, value);
            }
        }
        // The tag the kernel's records of this command's denials carry
        // (SPEC R-DENY.1).
        let denial_tag = sandbox::kernel_denials::tag_of(cmd.as_std().get_args());
        let spawned_at = Instant::now();

        // Spawn manually (rather than `cmd.output()`) so a timeout can take down
        // the whole process group — `cmd.output()` drops the future on timeout,
        // and `kill_on_drop` then kills only the direct `sandbox-exec` child,
        // orphaning `sh`/`cargo`/`rustc` descendants. `base_command` pipes
        // stdout/stderr and makes the child a process-group leader.
        // Match `cmd.output()`'s guarantee that stdout/stderr are captured.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut guard = match ProcessGroupGuard::spawn(&mut cmd) {
            Ok(guard) => guard,
            Err(e) => {
                return SyncRun::failed(anyhow::anyhow!("Command execution failed: {}", e));
            }
        };

        // Read stdout/stderr concurrently with `wait()` (not after `wait()`
        // returns) so a chatty child can't fill the pipe buffer and deadlock —
        // the same guarantee `wait_with_output()` gave us. We can no longer use
        // `wait_with_output()` itself, though: it consumes `child` by value, and
        // on timeout the cancelled future would drop (and silently orphan) the
        // child instead of letting us route the kill through the shared
        // `kill_process_tree` chokepoint below, so `child` has to stay owned
        // here.
        let (stdout_task, stderr_task) = Self::spawn_output_reader_tasks(guard.child_mut());

        let wait_res = tokio::time::timeout(timeout, guard.child_mut().wait()).await;

        let left_running;
        let output = match wait_res {
            Err(_) => {
                // Kill the entire process group so build descendants don't
                // orphan — the same cross-crate chokepoint
                // (`shell_pool::kill_process_tree`) the async streaming path
                // uses, which additionally confirms the reap and supports
                // Windows Job Objects.
                if !kill_process_tree(guard.child_mut()).await {
                    tracing::warn!(
                        "run_sync_prepared: operation {} timed out but its process did not reap cleanly",
                        op_id
                    );
                }
                // Keep what it printed: a command stopped at its limit is the
                // one whose last lines say what it was stuck on.
                let output = Self::output_after_kill(stdout_task, stderr_task).await;
                return SyncRun {
                    outcome: audit::Outcome::TimedOut,
                    exit_code: None,
                    result: Err(time_limit::LimitReached {
                        limit_secs: timeout.as_secs(),
                        output,
                    }
                    .into()),
                };
            }
            Ok(Err(e)) => {
                return SyncRun::failed(anyhow::anyhow!("Command execution failed: {}", e));
            }
            Ok(Ok(status)) => {
                // What the command left running ends with it (SPEC R-PROC.5).
                left_running = sandbox::leftovers::note(&guard.end_leftovers().await);
                let _ = guard.disarm();
                // The child has exited, so its pipes are at (or imminently
                // reaching) EOF; these joins are not a second wait for the
                // process itself.
                let stdout = stdout_task
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default();
                let stderr = stderr_task
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default();
                std::process::Output {
                    status,
                    stdout,
                    stderr,
                }
            }
        };

        // A non-zero exit may be a runtime sandbox denial the kernel did not name;
        // scan stderr and (best-effort, never blocking the result) offer to grant
        // an out-of-scope path it references.
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let exit_code = output.status.code();
        let result = interpret_sync_command_output(output);
        // What the SSH key broker refused this command, one line each
        // (SPEC R-CRED.10), on whichever side the result is.
        #[cfg(unix)]
        let result = match broker.as_ref().and_then(broker_refusals) {
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
        let result = match links
            .as_ref()
            .and_then(|l| link_refusals(l, &stdout, &stderr))
        {
            Some(lines) => match result {
                Ok(out) => Ok(format!("{out}\n{lines}")),
                Err(e) => Err(anyhow::anyhow!("{e}\n\n{lines}")),
            },
            None => result,
        };
        let result = match left_running {
            Some(line) => match result {
                Ok(out) => Ok(format!("{out}\n{line}")),
                Err(e) => Err(anyhow::anyhow!("{e}\n\n{line}")),
            },
            None => result,
        };
        if result.is_err() {
            return self
                .finalize_sync_denial(
                    effective_command,
                    op_id,
                    exit_code,
                    &stdout,
                    &stderr,
                    safe_wd,
                    result,
                    denial_tag.as_deref(),
                    spawned_at.elapsed(),
                )
                .await;
        }
        SyncRun {
            outcome: audit::Outcome::Completed,
            exit_code,
            result,
        }
    }

    /// Turn a failed sync command's output into the final [`SyncRun`], scanning
    /// stderr/stdout for a sandbox denial the kernel did not name.
    ///
    /// Split out of [`Self::run_sync_prepared`] so the happy path there stays a
    /// straight line; this is the async-path twin of `record_failure_diagnostics`.
    #[allow(clippy::too_many_arguments)]
    async fn finalize_sync_denial(
        &self,
        command: &str,
        op_id: &str,
        exit_code: Option<i32>,
        stdout: &str,
        stderr: &str,
        safe_wd: &std::path::Path,
        result: Result<String, anyhow::Error>,
        denial_tag: Option<&str>,
        ran_for: Duration,
    ) -> SyncRun {
        // A program that refuses any sandbox by its own check outranks every record (SPEC
        // R-ESCAPE.1): the kernel refused nothing that mattered.
        let result = match self_refused(result, stderr, stdout) {
            Ok(fatal) => {
                return SyncRun {
                    outcome: audit::Outcome::Failed,
                    exit_code,
                    result: Err(fatal),
                };
            }
            Err(result) => result,
        };
        // The kernel's own record decides where it can be read (SPEC R-DENY).
        if let Some(report) = sandbox::grant_channel::kernel_denial_lines(
            &self.sandbox,
            self.scope_grant_notifier.as_ref(),
            (stderr, stdout),
            denial_tag,
            ran_for,
            command,
            Some(op_id),
        )
        .await
        {
            return SyncRun {
                outcome: audit::Outcome::Failed,
                exit_code,
                result: kernel_report_result(
                    &self.sandbox,
                    report,
                    result,
                    op_id,
                    command,
                    sandbox::launch_services::refused_link(stderr, stdout).as_deref(),
                )
                .await,
            };
        }
        let result = match fatal_unrunnable(result, stderr, stdout) {
            Ok(fatal) => {
                return SyncRun {
                    outcome: audit::Outcome::Failed,
                    exit_code,
                    result: Err(fatal),
                };
            }
            Err(result) => result,
        };
        sandbox::grant_channel::raise_stderr_denial_question(
            &self.sandbox,
            self.scope_grant_notifier.as_ref(),
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
            .filter(|hit| !self.sandbox.is_path_in_scope_in_dir(&hit.path, safe_wd));
        if let Some(hit) = hit {
            let target = sandbox::grant_channel::resolve_grant_target(
                &hit.path,
                Some(safe_wd),
                &self.sandbox,
            );
            // Same payload, durable copy: the structured error reaches the
            // agent now, the audit line survives the session (SPEC R5.4.7).
            audit::record_sandbox_denial(Some(op_id), &target, hit.access.label(), command).await;
            let details = result.err().map(|e| e.to_string()).unwrap_or_default();
            return SyncRun {
                outcome: audit::Outcome::Failed,
                exit_code,
                result: Err(sandbox::SandboxError::RuntimeDenial {
                    path: target,
                    access: hit.access,
                    scopes: self.sandbox.scopes().to_vec(),
                    details,
                }
                .into()),
            };
        }
        // sccache prints a bare `Operation not permitted` with no path when the
        // cache the user set lies outside the scope; name that directory as the
        // denial, so every harness offers its grant (SPEC R-DOCTOR.9).
        if let Some(dir) = sandbox::build_diagnostics::sccache_cache_outside_scope(
            stderr,
            stdout,
            std::env::var("SCCACHE_DIR").ok(),
            &|dir| self.sandbox.is_path_in_scope_in_dir(dir, safe_wd),
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
            return SyncRun {
                outcome: audit::Outcome::Failed,
                exit_code,
                result: Err(sandbox::SandboxError::RuntimeDenial {
                    path: dir,
                    access: ahma_common::config::ScopeAccess::Rw,
                    scopes: self.sandbox.scopes().to_vec(),
                    details: format!("{error}\n\n{note}"),
                }
                .into()),
            };
        }
        SyncRun {
            outcome: audit::Outcome::Failed,
            exit_code,
            result: explain_ungrantable_failure(result, stderr, stdout, &|hint| {
                remediation_in_workspace(hint, &self.sandbox, Some(safe_wd))
            }),
        }
    }

    fn spawn_output_reader_tasks(
        child: &mut tokio::process::Child,
    ) -> (OutputReadTask, OutputReadTask) {
        let stdout_pipe = child.stdout.take().expect("stdout piped");
        let stderr_pipe = child.stderr.take().expect("stderr piped");
        let stdout_task: JoinHandle<std::io::Result<Vec<u8>>> = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut stdout_pipe = stdout_pipe;
            tokio::io::AsyncReadExt::read_to_end(&mut stdout_pipe, &mut buf).await?;
            Ok(buf)
        });
        let stderr_task: JoinHandle<std::io::Result<Vec<u8>>> = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut stderr_pipe = stderr_pipe;
            tokio::io::AsyncReadExt::read_to_end(&mut stderr_pipe, &mut buf).await?;
            Ok(buf)
        });
        (stdout_task, stderr_task)
    }

    /// What a killed command had printed, stdout then stderr. Its pipes close
    /// once the process tree is gone; a descendant that escaped the kill and
    /// still holds one is not waited for.
    async fn output_after_kill(stdout: OutputReadTask, stderr: OutputReadTask) -> String {
        async fn drain(mut task: OutputReadTask) -> String {
            match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
                Ok(Ok(Ok(bytes))) => String::from_utf8_lossy(&bytes).into_owned(),
                Ok(_) => String::new(),
                Err(_) => {
                    task.abort();
                    String::new()
                }
            }
        }
        let (out, err) = tokio::join!(drain(stdout), drain(stderr));
        [out, err]
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Synchronously executes a command with optional retry logic for transient errors.
    ///
    /// If a `RetryConfig` is set on the adapter, transient errors (timeouts, resource
    /// exhaustion, network issues) will be retried with exponential backoff.
    /// Permanent errors (command not found, permission denied) fail immediately.
    ///
    /// If no retry config is set, this behaves identically to `execute_sync_in_dir`.
    pub async fn execute_sync_with_retry(
        &self,
        command: &str,
        args: Option<Map<String, serde_json::Value>>,
        working_dir: &str,
        timeout_seconds: Option<u64>,
        subcommand_config: Option<&crate::config::SubcommandConfig>,
    ) -> Result<String, anyhow::Error> {
        match &self.retry_config {
            Some(config) => {
                // Clone args for each retry attempt since the closure needs ownership
                let args_clone = args.clone();
                retry::execute_with_retry(config, || {
                    let args_inner = args_clone.clone();
                    async move {
                        self.execute_sync_in_dir(
                            command,
                            args_inner,
                            working_dir,
                            timeout_seconds,
                            subcommand_config,
                        )
                        .await
                    }
                })
                .await
            }
            None => {
                // No retry config, execute directly
                self.execute_sync_in_dir(
                    command,
                    args,
                    working_dir,
                    timeout_seconds,
                    subcommand_config,
                )
                .await
            }
        }
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

        // Validate command arguments
        let (program_with_subcommand, args_vec) = self
            .prepare_command_and_args(command, args.as_ref(), subcommand_config, &safe_wd)
            .await?;
        let keep_leftovers = shell_command_line(args.as_ref())
            .is_some_and(|line| self.is_declared_long_running(line));

        let op_id = id
            .map(|s| s.to_string())
            .unwrap_or_else(|| generate_id(tool_name, command));
        let op_id_clone = op_id.clone();
        let wd = safe_wd_str.clone();

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
            self.resolve_lane_with_reason(args.as_ref(), subcommand_config, &safe_wd);
        self.command_starts(lane);
        let workspace = self.workspace_key_for(&safe_wd);
        let drift_root = workspace_queue::drift_root(&workspace, &self.sandbox.scopes(), &safe_wd);
        let holder_title = operation
            .command
            .clone()
            .or_else(|| operation.title.clone())
            .unwrap_or_else(|| command.to_string());
        let soft_hold = self.soft_hold_for(args.as_ref(), subcommand_config, lane);
        let ticket = self
            .enqueue_exclusive(lane, &safe_wd, &op_id, &holder_title)
            .map(|t| t.with_reason(queue_reason).with_soft_hold(soft_hold));
        let display_command = shell_command_line(args.as_ref())
            .map(str::to_string)
            .unwrap_or_else(|| command.to_string());
        let cancellation_token = self.monitor.register(operation).await;

        // Durable provenance, written *before* the task is spawned. Doing it here
        // rather than inside the task is the point: nothing between this line and
        // the process exiting — a panic, a SIGKILL, a hang, a machine losing
        // power — can leave the log without a record of what was asked for.
        audit::record_tool_call(
            &op_id,
            tool_name,
            args.as_ref(),
            &safe_wd_str,
            &program_with_subcommand,
            &args_vec,
        )
        .await;

        let monitor = self.monitor.clone();
        let shell_pool = self.shell_pool.clone();
        let sandbox = self.sandbox.clone(); // Clone ARC to pass to task
        let command = command.to_string();
        let wd_clone = wd.clone();

        let run = run_async_operation(AsyncOperationRun {
            op_id,
            cancellation_token,
            command,
            program: program_with_subcommand,
            args_vec,
            working_dir: wd_clone,
            timeout_secs: timeout,
            log_monitor_config,
            monitor,
            shell_pool,
            sandbox,
            command_executor: self.command_executor.clone(),
            mutex_registry: self.mutex_registry.clone(),
            scope_grant_notifier: self.scope_grant_notifier.clone(),
            lane,
            ticket,
            workspace,
            drift_root,
            display_command,
            watch_handoff: self.watches_handoff(lane),
            keep_leftovers,
            #[cfg(unix)]
            credential_brokers: self.credential_brokers.clone(),
        });
        self.spawn_operation_task(&op_id_clone, run).await;

        Ok(op_id_clone)
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
        // No lane of its own: it may write.
        self.command_starts(workspace_queue::Lane::Exclusive);
        let op_id = id.unwrap_or_else(|| generate_id(tool_name, command_str));

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
        let cancellation_token = self.monitor.register(operation).await;

        let audit_argv = [command_str.to_string()];
        audit::record_tool_call(
            &op_id,
            tool_name,
            None,
            &safe_wd.to_string_lossy(),
            "[pty]",
            &audit_argv,
        )
        .await;

        let limit = command_limit(&self.shell_pool, timeout);
        let monitor = self.monitor.clone();
        let sandbox = self.sandbox.clone();
        let command_str = command_str.to_string();
        let op_id_task = op_id.clone();
        let tool_task = tool_name.to_string();
        #[cfg(unix)]
        let credential_brokers = self.credential_brokers.clone();
        let watch_handoff = self.watches_handoff(workspace_queue::Lane::Exclusive);
        let keep_leftovers = self.is_declared_long_running(&command_str);
        // Its mutex group (SPEC R2.7.2), keyed like the lease and matched on
        // the line the caller wrote, as for a piped command.
        let mutex_registry = self.mutex_registry.clone();
        let workspace = self.workspace_key_for(&safe_wd);
        // Drift (SPEC R2.7.6): this command is exclusive, never a service.
        let drift = DriftProbe {
            root: workspace_queue::drift_root(&workspace, &self.sandbox.scopes(), &safe_wd),
            lane: workspace_queue::Lane::Exclusive,
        };
        // A PTY command is an arbitrary shell command: exclusive (SPEC R2.7).
        let ticket = self.enqueue_exclusive(
            workspace_queue::Lane::Exclusive,
            &safe_wd,
            &op_id,
            &command_str,
        );

        let task = async move {
            let Some(guards) = guards::RunGuards::begin(
                &monitor,
                &op_id_task,
                &tool_task,
                ticket,
                &cancellation_token,
            )
            .await
            else {
                return;
            };
            // Its mutex group (SPEC R2.7.2), as for a piped command.
            let Some(mut guards) = guards
                .gate(&monitor, &mutex_registry, &command_str, &workspace)
                .await
            else {
                return;
            };
            // As on the piped path: after the queue and the gate, before the spawn.
            let started = Instant::now();
            if watch_handoff {
                guards.watch_handoff(Some(begin_handoff_watch(&sandbox, &safe_wd).await));
            }
            // Its SSH key broker (SPEC R-CRED.1), as for any other command: a
            // refused signature is an alert on this operation (R-CRED.10). Held
            // until the PTY process is gone.
            #[cfg(unix)]
            guards.hold_broker(
                credential_brokers
                    .as_ref()
                    .and_then(|f| f.lease_for_command(&safe_wd, Some(&command_str)))
                    .inspect(|broker| alert_broker_refusals(broker, &monitor, &op_id_task)),
            );
            #[cfg(unix)]
            let broker_socket = guards.broker().map(|b| b.socket().to_path_buf());
            #[cfg(not(unix))]
            let broker_socket: Option<std::path::PathBuf> = None;
            let finished = pty_exec::run_pty_operation(
                &sandbox,
                &command_str,
                &safe_wd,
                limit,
                &cancellation_token,
                &op_id_task,
                &monitor,
                broker_socket.as_deref(),
                keep_leftovers,
            )
            .await;
            // The PTY process is gone: the result carries the drift and
            // deny-tier reports, as a piped command's does.
            let finished =
                carry_run_reports(finished, &drift, started, &mut guards, &monitor).await;
            guards.end(&monitor, finished).await;
        };
        self.spawn_operation_task(&op_id, task).await;

        Ok(op_id)
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
        // No lane of its own: it may write.
        self.command_starts(workspace_queue::Lane::Exclusive);
        let op_id = id.unwrap_or_else(|| generate_id(tool_name, command_str));

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
        let cancellation_token = self.monitor.register(operation).await;

        let audit_argv = [command_str.to_string()];
        audit::record_tool_call(
            &op_id,
            tool_name,
            None,
            &safe_wd.to_string_lossy(),
            &format!("[session {session_id}]"),
            &audit_argv,
        )
        .await;

        let timeout_duration = command_limit(&self.shell_pool, timeout);
        let monitor = self.monitor.clone();
        let sandbox = self.sandbox.clone();
        let sessions = self.shell_sessions.clone();
        let session_id = session_id.to_string();
        let command_str = command_str.to_string();
        let op_id_task = op_id.clone();
        let tool_task = tool_name.to_string();
        let watch_handoff = self.watches_handoff(workspace_queue::Lane::Exclusive);
        let mutex_registry = self.mutex_registry.clone();
        let workspace = self.workspace_key_for(&safe_wd);
        // Drift (SPEC R2.7.6): this command is exclusive, never a service.
        let drift = DriftProbe {
            root: workspace_queue::drift_root(&workspace, &self.sandbox.scopes(), &safe_wd),
            lane: workspace_queue::Lane::Exclusive,
        };
        // A session command is an arbitrary shell command: exclusive (SPEC R2.7).
        let ticket = self.enqueue_exclusive(
            workspace_queue::Lane::Exclusive,
            &safe_wd,
            &op_id,
            &command_str,
        );

        let task = async move {
            let Some(guards) = guards::RunGuards::begin(
                &monitor,
                &op_id_task,
                &tool_task,
                ticket,
                &cancellation_token,
            )
            .await
            else {
                return;
            };
            // Its mutex group (SPEC R2.7.2), as for a piped command.
            let Some(mut guards) = guards
                .gate(&monitor, &mutex_registry, &command_str, &workspace)
                .await
            else {
                return;
            };
            // As on the piped path: after the queue and the gate, before the spawn.
            let started = Instant::now();
            if watch_handoff {
                guards.watch_handoff(Some(begin_handoff_watch(&sandbox, &safe_wd).await));
            }
            let finished = run_session_operation(
                &sessions,
                &sandbox,
                &session_id,
                &command_str,
                &safe_wd,
                timeout_duration,
                &cancellation_token,
                &op_id_task,
                &monitor,
            )
            .await;
            // The command has ended (the session's shell lives on): as for PTY.
            let finished =
                carry_run_reports(finished, &drift, started, &mut guards, &monitor).await;
            guards.end(&monitor, finished).await;
        };
        self.spawn_operation_task(&op_id, task).await;

        Ok(op_id)
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

/// Build a sandbox-wrapped `tokio::process::Command` for the given program and args.
///
/// Raw `/bin/sh` invocations use `create_shell_command` (injects `-c`); all other
/// programs use `create_command` so the preparer's shell flags are not doubled.
fn build_sandboxed_command(
    executor: &dyn executor::CommandExecutor,
    sandbox: &sandbox::Sandbox,
    program: &str,
    args_vec: &[String],
    working_dir: &std::path::Path,
) -> Result<tokio::process::Command> {
    executor.build_command(sandbox, program, args_vec, working_dir)
}

/// Interpret a completed synchronous process output as success text or an error.
fn interpret_sync_command_output(output: std::process::Output) -> Result<String, anyhow::Error> {
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        // Append a remediation line when the failure is sandbox build
        // contamination (in-scope EPERM / sccache), so the CLI user sees what to
        // do instead of a bare `os error 1`. See `sandbox::build_diagnostics`.
        let remediation = sandbox::build_diagnostics::diagnose(&stderr)
            .map(|h| format!("\n\nhint: {}", h.remediation))
            .unwrap_or_default();
        return Err(anyhow::anyhow!(
            "Command failed with exit code {}: stderr: {}, stdout: {}{}",
            output.status.code().unwrap_or(-1),
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

/// Publishing how a command ended. A PTY or session runner returns its
/// [`Terminal`] without publishing it: the caller first gives up the workspace
/// lease and the guards around the run, then publishes, so a caller woken by
/// the terminal state never finds the workspace still held (SPEC R2.7.1).
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

impl BoundedLineCollector {
    /// The result of a command whose output is one merged stream (a PTY, a
    /// session): `stderr` is empty and has no counts of its own.
    fn merged_output(&self, exit_code: Option<i32>, op_id: &str) -> CommandOutput {
        CommandOutput {
            stdout: self.rendered_output(),
            exit_code,
            stdout_truncated_lines: self.dropped_lines(),
            stdout_truncated_bytes: self.dropped_bytes(),
            output_file: spill::operation_spill_path(op_id)
                .to_string_lossy()
                .into_owned(),
            ..CommandOutput::default()
        }
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

/// Context for a single background async operation task.
struct AsyncOperationRun {
    op_id: String,
    cancellation_token: tokio_util::sync::CancellationToken,
    command: String,
    program: String,
    args_vec: Vec<String>,
    working_dir: String,
    timeout_secs: Option<u64>,
    log_monitor_config: Option<crate::log_monitor::LogMonitorConfig>,
    monitor: Arc<OperationMonitor>,
    shell_pool: Arc<ShellPoolManager>,
    sandbox: Arc<sandbox::Sandbox>,
    command_executor: Arc<dyn executor::CommandExecutor>,
    mutex_registry: Arc<CommandMutexRegistry>,
    scope_grant_notifier: Option<Arc<dyn sandbox::ScopeGrantNotifier>>,
    /// Workspace-queue lane (SPEC R2.7.4).
    lane: workspace_queue::Lane,
    /// Place in the workspace line, for an exclusive operation.
    ticket: Option<workspace_queue::Ticket>,
    /// The workspace key (SPEC R2.7.2): the mutex groups' key.
    workspace: std::path::PathBuf,
    /// Where the drift probe walks: the workspace, clipped to the sandbox
    /// scope (SPEC R2.7.6).
    drift_root: std::path::PathBuf,
    /// The command line as the caller wrote it (for a shell tool, the shell
    /// string — `command` is then just the shell program).
    display_command: String,
    /// Inventory the trust-handoff deny tier around the run (SPEC R6.1.7).
    watch_handoff: bool,
    /// A human declared this command long-running (SPEC R2.7.10), so what it
    /// starts may stay running after it (R-PROC.5).
    keep_leftovers: bool,
    /// Gives the command an SSH key broker (SPEC R-CRED.1).
    #[cfg(unix)]
    credential_brokers: Option<Arc<dyn crate::credentials::ssh_agent::consent::BrokerFactory>>,
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

async fn run_async_operation(ctx: AsyncOperationRun) {
    let AsyncOperationRun {
        op_id,
        cancellation_token,
        command,
        program,
        args_vec,
        working_dir,
        timeout_secs,
        log_monitor_config,
        monitor,
        shell_pool,
        sandbox,
        command_executor,
        mutex_registry,
        scope_grant_notifier,
        lane,
        ticket,
        workspace,
        drift_root,
        display_command,
        watch_handoff,
        keep_leftovers,
        #[cfg(unix)]
        credential_brokers,
    } = ctx;

    // Timed from the moment the task starts, so queueing behind a command mutex
    // group is visible in the audit record rather than silently excluded.
    // ── Workspace write queue (SPEC R2.7) ──────────────────────────────────
    // Wait for every exclusive operation that arrived earlier in this
    // workspace — in this process or any other — before starting. The lease is
    // held until the operation ends, i.e. until the process tree is gone.
    let Some(guards) =
        guards::RunGuards::begin(&monitor, &op_id, &command, ticket, &cancellation_token).await
    else {
        return;
    };

    // Lifecycle events (Started / terminal) are emitted by the OperationMonitor
    // at each state transition — the single emission point for the unified
    // event stream (SPEC R15).  MCP progress notifications are pushed by the
    // `progress_push` forwarder subscribed to that stream.

    // ── Command mutex group gate (SPEC R2.7.2) ─────────────────────────────
    // Held by the guards for the whole run and handed on with the lease.
    let Some(mut guards) = guards
        .gate(&monitor, &mutex_registry, &display_command, &workspace)
        .await
    else {
        return;
    };

    let start_time = Instant::now();
    let wd_path = std::path::PathBuf::from(&working_dir);
    let built = if lane == workspace_queue::Lane::ReadOnly {
        command_executor.build_read_only_command(&sandbox, &program, &args_vec, &wd_path)
    } else {
        build_sandboxed_command(
            command_executor.as_ref(),
            &sandbox,
            &program,
            &args_vec,
            &wd_path,
        )
    };
    let mut proc_cmd = match built {
        Ok(cmd) => cmd,
        Err(e) => {
            let why = format!("Failed to create sandboxed command: {}", e);
            guards.end(&monitor, failed(why)).await;
            return;
        }
    };

    stamp_lease(&mut proc_cmd, guards.lease());
    if keep_leftovers {
        sandbox::leftovers::keep(&mut proc_cmd);
    }
    // The command's SSH key broker (SPEC R-CRED.1): a refused signature is an
    // alert on this operation while it still runs (R-CRED.10). Held until the
    // command has finished.
    #[cfg(unix)]
    guards.hold_broker(
        credential_brokers
            .as_ref()
            .and_then(|f| f.lease_for_command(&wd_path, Some(&display_command)))
            .inspect(|broker| {
                proc_cmd.env("SSH_AUTH_SOCK", broker.socket());
                alert_broker_refusals(broker, &monitor, &op_id);
            }),
    );
    // The tag the kernel's records of this operation's denials carry
    // (SPEC R-DENY.1), for the failure path to read them back.
    if let Some(tag) = sandbox::kernel_denials::tag_of(proc_cmd.as_std().get_args()) {
        sandbox::kernel_denials::remember_tag(&op_id, tag);
    }

    let limit = command_limit(&shell_pool, timeout_secs);

    let drift = (lane != workspace_queue::Lane::Service).then_some(DriftProbe {
        root: drift_root,
        lane,
    });

    // SPEC R6.1.7: the "before" inventory of the deny tier, taken as late as
    // possible — after the queue and the mutex gate, just before the spawn.
    if watch_handoff {
        guards.watch_handoff(Some(begin_handoff_watch(&sandbox, &wd_path).await));
    }

    // Single execution path: stream stdout/stderr line-by-line for every
    // operation (SPEC R15.2).  Lines flow through the OperationMonitor, which
    // appends to the tail buffer and emits `OutputLine` on the unified event
    // stream — so the TUI and hub subscribers see output as it is produced.
    // Log-monitor alerting only runs when a config was provided.
    execute_with_streaming(
        &mut proc_cmd,
        limit,
        log_monitor_config,
        &cancellation_token,
        &op_id,
        start_time,
        &monitor,
        &sandbox,
        scope_grant_notifier.as_ref(),
        &command,
        &wd_path,
        drift.as_ref(),
        guards,
    )
    .await;
}

/// The shell command line of a `run_terminal_command`-style call (`command`
/// plus `c_flag`), which is what the lane classifier and the mutex groups must
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

/// Wait for a ticket's turn (SPEC R2.7.1), keeping the operation visibly alive
/// meanwhile: queued is not stalled, so the idle-output watchdog must not kill
/// it, and observers see who it waits for. `Err` = cancelled while queued.
async fn wait_for_lease(
    ticket: Option<workspace_queue::Ticket>,
    monitor: &Arc<OperationMonitor>,
    op_id: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<workspace_queue::Lease>, ()> {
    let Some(ticket) = ticket else {
        return Ok(None);
    };
    let started = Instant::now();
    let waiting = ticket.title().to_string();
    let reason = ticket.reason().map(str::to_string);
    let observe = |ahead: &[workspace_queue::HolderInfo]| {
        monitor.try_note_liveness(op_id);
        monitor.note_progress(
            op_id,
            queued_message(Some(&waiting), reason.as_deref(), ahead),
        );
    };
    let lease = ticket.acquire(cancel, &observe).await.map_err(|_| ())?;
    let waited = started.elapsed();
    if waited >= QUEUE_WAIT_WORTH_REPORTING {
        monitor.left_queue(op_id, waited).await;
    }
    Ok(Some(lease))
}

/// Where a synchronous call announces a wait for its workspace; see
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

/// Stamp a child spawned under a lease with it (SPEC R2.7.7), so an ahma the
/// command itself starts does not wait for the lease its ancestor holds.
fn stamp_lease(cmd: &mut tokio::process::Command, lease: Option<&workspace_queue::Lease>) {
    if let Some(lease) = lease {
        cmd.env(
            workspace_queue::HELD_LEASE_ENV,
            workspace_queue::child_lease_env(lease.key()),
        );
    }
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
/// A PTY or session command's ending with what its result owes beyond the
/// output, as a piped command's result has it: the files that changed in its
/// workspace while it ran (SPEC R2.7.6). Only an ending that carries output
/// (`Exited`) has a place for it.
/// A PTY or session command's ending with what its result owes beyond the
/// output, as a piped command's result has it, in the same order: the files
/// that changed in its workspace while it ran (SPEC R2.7.6), then the
/// deny-tier report and its alert (R6.1.7). Only an ending that carries output
/// (`Exited`) has a place for them; any other leaves the watch for
/// `RunGuards::end`, which reports it to the log and the audit trail.
async fn carry_run_reports(
    ending: Terminal,
    drift: &DriftProbe,
    started: Instant,
    guards: &mut guards::RunGuards,
    monitor: &OperationMonitor,
) -> Terminal {
    let mut output = match ending {
        Terminal::Exited(output) => output,
        other => return other,
    };
    output.changed_during_run = drift.report(started.elapsed()).await;
    guards.report_handoff_in(&mut output, monitor).await;
    Terminal::Exited(output)
}

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

/// Run one command in a persistent shell session through the standard
/// operation lifecycle (streamed lines, spill file, single terminal event).
#[allow(clippy::too_many_arguments)]
async fn run_session_operation(
    sessions: &crate::shell_session::ShellSessionManager,
    sandbox: &sandbox::Sandbox,
    session_id: &str,
    command_str: &str,
    working_dir: &std::path::Path,
    timeout: Duration,
    cancellation_token: &tokio_util::sync::CancellationToken,
    op_id: &str,
    monitor: &Arc<OperationMonitor>,
) -> Terminal {
    let mut spill_writer = spill::SpillWriter::create(op_id).await;
    let mut collected = BoundedLineCollector::default();
    // Warned once at 80% of the limit, as a piped command is (SPEC R2.6.6).
    // The session enforces the limit itself (`ShellSession::execute_streaming`).
    let mut near_limit = time_limit::NearLimit::starting_now(timeout);

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
        command_str,
        timeout,
        &mut forward,
    );
    tokio::pin!(exec);

    let exec_result = loop {
        tokio::select! {
            biased;

            _ = cancellation_token.cancelled() => {
                tracing::info!("Session operation {} cancelled", op_id);
                // Remove the session from the map WITHOUT acquiring the per-session
                // lock.  The exec future (pinned above) holds that lock; calling
                // close_session here would deadlock.  Instead we evict the session
                // from the map so it won't be reused.  When this function returns,
                // `exec` is dropped, the MutexGuard is released, and the ShellSession
                // Arc ref-count reaches zero — at which point kill_on_drop(true) on
                // the child kills the shell and its children.
                sessions.remove_session(session_id).await;
                spill_writer.finish().await;
                return Terminal::Cancelled("Operation was cancelled".to_string());
            }

            _ = tokio::time::sleep_until(near_limit.at()), if near_limit.pending() => {
                monitor.append_alert(op_id, near_limit.alert()).await;
            }

            line = line_rx.recv() => {
                if let Some(line) = line {
                    let safe = crate::log_monitor::redact_sensitive_line(&line);
                    spill_writer.write_line(&safe, Stream::Stdout).await;
                    collected.push(safe.clone());
                    monitor.append_output_line(op_id, safe, Stream::Stdout).await;
                }
                // None can only happen after exec completes (sender dropped);
                // the exec branch below handles termination.
            }

            result = &mut exec => break result,
        }
    };

    // Drain any lines still buffered in the channel.
    while let Ok(line) = line_rx.try_recv() {
        let safe = crate::log_monitor::redact_sensitive_line(&line);
        spill_writer.write_line(&safe, Stream::Stdout).await;
        collected.push(safe.clone());
        monitor
            .append_output_line(op_id, safe, Stream::Stdout)
            .await;
    }
    spill_writer.finish().await;

    finalize_session_result(exec_result, &collected, session_id, op_id)
}

/// How a session-protocol command ended, from its terminal result.
fn finalize_session_result(
    exec_result: Result<Option<i32>, crate::shell_session::SessionError>,
    collected: &BoundedLineCollector,
    session_id: &str,
    op_id: &str,
) -> Terminal {
    match exec_result {
        // Session output is a single merged stream (per-command 2>&1).
        Ok(exit_code) => Terminal::Exited(Box::new(CommandOutput {
            session_id: Some(session_id.to_string()),
            ..collected.merged_output(exit_code, op_id)
        })),
        // Says how to raise the limit, as the piped path does (SPEC R2.6.6).
        Err(crate::shell_session::SessionError::TimedOut(limit)) => {
            timed_out(limit.as_millis() as u64)
        }
        Err(e) => Terminal::Failed(e.to_string()),
    }
}

/// The ending of a command stopped at its time limit, saying how to raise it.
fn timed_out(duration_ms: u64) -> Terminal {
    Terminal::TimedOut(format!(
        "Operation timed out after {}ms (exceeded timeout limit). {}",
        duration_ms,
        time_limit::how_to_raise(duration_ms.div_ceil(1000))
    ))
}

/// Execute a command with line-by-line streaming and optional log monitoring.
///
/// This is the single execution path for async operations.  Instead of
/// buffering all output, it spawns the process and reads stdout/stderr
/// concurrently via `BufReader::lines()`.  Each line is appended to the
/// operation's tail buffer (which emits `OutputLine` on the unified event
/// stream).  When `monitor_config` is `Some`, each line is additionally fed
/// through a `LogMonitor` which checks for error/warning patterns and emits
/// `Alert` events.
///
/// Ends the operation through `guards`, which also writes the single
/// `tool_complete` audit event that pairs with the `tool_call` written before
/// the task was spawned.
#[allow(clippy::too_many_arguments)]
async fn execute_with_streaming(
    proc_cmd: &mut tokio::process::Command,
    limit: Duration,
    monitor_config: Option<crate::log_monitor::LogMonitorConfig>,
    cancellation_token: &tokio_util::sync::CancellationToken,
    op_id: &str,
    start_time: Instant,
    op_monitor: &Arc<OperationMonitor>,
    sandbox: &Arc<sandbox::Sandbox>,
    scope_grant_notifier: Option<&Arc<dyn sandbox::ScopeGrantNotifier>>,
    tool: &str,
    working_dir: &Path,
    drift: Option<&DriftProbe>,
    guards: guards::RunGuards,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};

    // Ensure stdin/stdout/stderr are strictly isolated (should already be set by sandbox)
    proc_cmd.stdin(std::process::Stdio::null());
    proc_cmd.stdout(std::process::Stdio::piped());
    proc_cmd.stderr(std::process::Stdio::piped());

    let mut guard = match ProcessGroupGuard::spawn(proc_cmd) {
        Ok(guard) => guard,
        Err(e) => {
            guards
                .end(
                    op_monitor,
                    failed(format!("Failed to spawn process: {}", e)),
                )
                .await;
            return;
        }
    };

    let stdout = guard.child_mut().stdout.take().expect("stdout piped");
    let stderr = guard.child_mut().stderr.take().expect("stderr piped");

    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut log_monitor = monitor_config.map(crate::log_monitor::LogMonitor::new);

    // Full-output spill file: the complete record of this operation's output,
    // queryable with file tools long after the bounded tail has rolled over.
    let mut spill = spill::SpillWriter::create(op_id).await;

    // Collected output for the final result (bounded to prevent unbounded memory growth).
    let mut collected_stdout = BoundedLineCollector::default();
    let mut collected_stderr = BoundedLineCollector::default();

    let timeout_deadline = tokio::time::Instant::now() + limit;
    // Warn once at 80% of the limit (SPEC R2.6.6), so a healthy job near it
    // is not stopped without notice.
    let mut near_limit = time_limit::NearLimit::starting_now(limit);

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
    let cpu_probe_after = cpu_probe_threshold(op_monitor.idle_timeout());

    loop {
        tokio::select! {
            // Bias stderr to prioritize error-related output
            biased;

            // Check cancellation
            _ = cancellation_token.cancelled() => {
                tracing::info!("Operation {} cancelled during streaming", op_id);
                if !kill_process_tree(guard.child_mut()).await {
                    tracing::warn!("Operation {} cancelled but its process did not reap cleanly", op_id);
                }
                spill.finish().await;
                guards.end(op_monitor, cancelled()).await;
                return;
            }

            _ = tokio::time::sleep_until(near_limit.at()), if near_limit.pending() => {
                op_monitor.append_alert(op_id, near_limit.alert()).await;
            }

            // Timeout
            _ = tokio::time::sleep_until(timeout_deadline) => {
                tracing::warn!("Operation {} timed out during streaming", op_id);
                if !kill_process_tree(guard.child_mut()).await {
                    tracing::warn!("Operation {} timed out but its process did not reap cleanly", op_id);
                }
                spill.finish().await;
                let duration_ms = start_time.elapsed().as_millis() as u64;
                guards.end(op_monitor, timed_out(duration_ms)).await;
                return;
            }

            _ = still_running_interval.tick() => {
                elapsed_secs += 10;
                let elapsed_msg = format!("still running ({}s elapsed)", elapsed_secs);
                tracing::info!("Operation {}: {}", op_id, elapsed_msg);
                op_monitor.note_progress(op_id, elapsed_msg);

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
                            op_id,
                            last_output_at.elapsed().as_secs(),
                            last_cpu_ms.unwrap_or(0),
                            cpu_ms
                        );
                        op_monitor.note_liveness(op_id).await;
                    }
                    last_cpu_ms = Some(cpu_ms);
                }
            }

            // Wake the loop while both pipes are at EOF but the child is still
            // alive, so the `try_wait()` below still runs without spinning.
            _ = reap_interval.tick(), if stdout_done && stderr_done => {}

            // Read stderr line
            result = stderr_reader.next_line(), if !stderr_done => {
                // EOF is not output. A child that closes its pipes but keeps
                // running yields `Ok(None)` immediately and forever; treating
                // that as activity would hold the CPU probe permanently off.
                //
                // Real output also drops the CPU baseline, so the next quiet
                // stretch measures a rise from *its own* start. Carrying a
                // sample across an intervening chatty period would compare
                // against a stale total and fabricate proof of life.
                if matches!(result, Ok(Some(_))) {
                    last_output_at = tokio::time::Instant::now();
                    last_cpu_ms = None;
                } else {
                    stderr_done = true;
                }
                handle_stream_line(result, Stream::Stderr, &mut collected_stderr, &mut log_monitor, op_id, op_monitor, &mut spill).await;
            }

            // Read stdout line
            result = stdout_reader.next_line(), if !stdout_done => {
                if matches!(result, Ok(Some(_))) {
                    last_output_at = tokio::time::Instant::now();
                    last_cpu_ms = None;
                } else {
                    stdout_done = true;
                }
                handle_stream_line(result, Stream::Stdout, &mut collected_stdout, &mut log_monitor, op_id, op_monitor, &mut spill).await;
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
                    op_monitor.append_alert(op_id, line).await;
                }
                drain_remaining_stream_lines(
                    &mut stderr_reader,
                    &mut stdout_reader,
                    &mut collected_stdout,
                    &mut collected_stderr,
                    &mut log_monitor,
                    op_id,
                    op_monitor,
                    &mut spill,
                )
                .await;
                break;
            }
            Ok(None) => {
                // Process still running, continue loop
            }
            Err(e) => {
                tracing::warn!("Error checking child status for {}: {}", op_id, e);
                break;
            }
        }
    }

    spill.finish().await;
    let mut child = guard.disarm().unwrap();

    finalize_streaming_operation(
        &mut child,
        start_time,
        &collected_stdout,
        &collected_stderr,
        op_monitor,
        op_id,
        sandbox,
        scope_grant_notifier,
        tool,
        working_dir,
        drift,
        guards,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drain_remaining_stream_lines(
    stderr_reader: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStderr>>,
    stdout_reader: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    collected_stdout: &mut BoundedLineCollector,
    collected_stderr: &mut BoundedLineCollector,
    log_monitor: &mut Option<crate::log_monitor::LogMonitor>,
    op_id: &str,
    op_monitor: &Arc<OperationMonitor>,
    spill: &mut spill::SpillWriter,
) {
    while let Ok(Some(line)) = stderr_reader.next_line().await {
        process_streaming_line(
            &line,
            Stream::Stderr,
            collected_stderr,
            log_monitor,
            op_id,
            op_monitor,
            spill,
        )
        .await;
    }
    while let Ok(Some(line)) = stdout_reader.next_line().await {
        process_streaming_line(
            &line,
            Stream::Stdout,
            collected_stdout,
            log_monitor,
            op_id,
            op_monitor,
            spill,
        )
        .await;
    }
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
    // typed `RuntimeDenial` the sync path returns.
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

#[allow(clippy::too_many_arguments)]
async fn finalize_streaming_operation(
    child: &mut tokio::process::Child,
    start_time: Instant,
    collected_stdout: &BoundedLineCollector,
    collected_stderr: &BoundedLineCollector,
    op_monitor: &Arc<OperationMonitor>,
    op_id: &str,
    sandbox: &Arc<sandbox::Sandbox>,
    scope_grant_notifier: Option<&Arc<dyn sandbox::ScopeGrantNotifier>>,
    tool: &str,
    working_dir: &Path,
    drift: Option<&DriftProbe>,
    mut guards: guards::RunGuards,
) {
    let exit_status = child.wait().await;
    let duration_ms = start_time.elapsed().as_millis() as u64;
    tracing::debug!("Operation {} process exited after {}ms", op_id, duration_ms);
    let exit_code = exit_status.as_ref().ok().and_then(|s| s.code());
    let success = exit_status.as_ref().is_ok_and(|s| s.success());
    if success {
        // Nothing to read back for an operation that succeeded.
        let _ = sandbox::kernel_denials::take_tag(op_id);
    }

    let stdout_str = collected_stdout.rendered_output();
    let stderr_str = collected_stderr.rendered_output();

    // A failed async command may have hit a runtime sandbox denial the kernel did
    // not name; scan stderr and (best-effort) offer to grant an out-of-scope path.
    // This is the async-path twin of the scan in `execute_sync_in_dir`.
    if !success {
        record_failure_diagnostics(
            sandbox,
            scope_grant_notifier,
            op_monitor,
            op_id,
            tool,
            &stdout_str,
            &stderr_str,
            Some(working_dir),
        )
        .await;
    } else if let Some(note) = sandbox::gpu_fallback_note(&stderr_str, &stdout_str) {
        // The async twin of the success note in `explain_ungrantable_failure`.
        op_monitor.append_alert(op_id, note).await;
    }

    let changed_during_run = match drift {
        Some(probe) => probe.report(start_time.elapsed()).await,
        None => None,
    };

    let mut output = CommandOutput {
        stdout: stdout_str,
        stderr: stderr_str,
        exit_code,
        stdout_truncated_lines: collected_stdout.dropped_lines(),
        stderr_truncated_lines: Some(collected_stderr.dropped_lines()),
        stdout_truncated_bytes: collected_stdout.dropped_bytes(),
        stderr_truncated_bytes: Some(collected_stderr.dropped_bytes()),
        // Complete output record — query with file tools (tail/grep) when the
        // inline stdout/stderr above was truncated.
        output_file: spill::operation_spill_path(op_id)
            .to_string_lossy()
            .into_owned(),
        changed_during_run,
        ..CommandOutput::default()
    };
    // The process tree is gone: the deny-tier report (SPEC R6.1.7) goes in
    // the result, and its alert out before the terminal transition.
    guards.report_handoff_in(&mut output, op_monitor).await;
    // The process tree is gone and the drift probe has looked: the guards
    // hand the workspace on, then publish. The terminal event carries the
    // result to all subscribers (MCP progress push, hub, TUI).
    guards
        .end(op_monitor, Terminal::Exited(Box::new(output)))
        .await;
}

/// The ending of an operation cancelled before or while it ran.
fn cancelled() -> Terminal {
    Terminal::Cancelled("Operation was cancelled".to_string())
}

/// Handle one result from a `next_line()` call inside the streaming select loop.
///
/// Dispatches to `process_streaming_line` on success, ignores closed-stream
/// signals (`Ok(None)`), and logs a warning on read errors.
async fn handle_stream_line(
    result: Result<Option<String>, std::io::Error>,
    stream: Stream,
    collector: &mut BoundedLineCollector,
    log_monitor: &mut Option<crate::log_monitor::LogMonitor>,
    op_id: &str,
    op_monitor: &Arc<OperationMonitor>,
    spill: &mut spill::SpillWriter,
) {
    match result {
        Ok(Some(line)) => {
            process_streaming_line(
                &line,
                stream,
                collector,
                log_monitor,
                op_id,
                op_monitor,
                spill,
            )
            .await;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!("Error reading {} for {}: {}", stream.name(), op_id, e);
        }
    }
}

/// Redact, collect, and optionally record a log-monitor alert for a single streamed line.
///
/// `op_monitor.append_output_line` is the single emission point for
/// `OutputLine` events on the unified stream; `append_alert` likewise emits
/// `Alert` — no direct dispatcher access is needed here.
async fn process_streaming_line(
    line: &str,
    stream: Stream,
    collector: &mut BoundedLineCollector,
    log_monitor: &mut Option<crate::log_monitor::LogMonitor>,
    op_id: &str,
    op_monitor: &Arc<OperationMonitor>,
    spill: &mut spill::SpillWriter,
) {
    let safe_line = crate::log_monitor::redact_sensitive_line(line);

    // The spill file records every redacted line — the faithful full record,
    // independent of the bounded tail.
    spill.write_line(&safe_line, stream).await;

    collector.push(safe_line.clone());
    op_monitor
        .append_output_line(op_id, safe_line, stream)
        .await;

    if let Some(log_monitor) = log_monitor
        && let Some(snapshot) = log_monitor.process_line(line, stream.is_stderr())
    {
        // The full notification snapshot (trigger + recent context) is stored
        // and emitted as a single `Alert` event so every subscriber — MCP
        // progress push, hub, TUI — gets the same rich content.
        op_monitor
            .append_alert(op_id, snapshot.format_for_notification())
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            .execute_sync_in_dir(
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
    // Gated to Unix: these synthesise an `ExitStatus` via the Unix-only
    // `ExitStatusExt::from_raw` (Windows uses an incompatible u32 encoding). The
    // remediation logic under test is itself platform-agnostic.

    #[cfg(unix)]
    #[test]
    fn interpret_sync_output_appends_contamination_hint_on_provenance_failure() {
        use std::os::unix::process::ExitStatusExt;
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(256), // exit code 1
            stdout: b"   Compiling foo".to_vec(),
            stderr: b"error: error writing dependencies to `/w/target/debug/deps/x.d`: \
                Operation not permitted (os error 1)"
                .to_vec(),
        };
        let err = interpret_sync_command_output(output)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("hint:"),
            "expected a remediation hint, got: {err}"
        );
        assert!(err.contains("com.apple.provenance"));
    }

    #[cfg(unix)]
    #[test]
    fn interpret_sync_output_no_hint_for_ordinary_failure() {
        use std::os::unix::process::ExitStatusExt;
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: Vec::new(),
            stderr: b"error[E0382]: borrow of moved value".to_vec(),
        };
        let err = interpret_sync_command_output(output)
            .unwrap_err()
            .to_string();
        assert!(
            !err.contains("hint:"),
            "ordinary failure should not add a hint"
        );
    }

    #[cfg(unix)]
    #[test]
    fn interpret_sync_output_ok_on_success() {
        use std::os::unix::process::ExitStatusExt;
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"done".to_vec(),
            stderr: Vec::new(),
        };
        assert!(interpret_sync_command_output(output).is_ok());
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
        let adapter = Adapter::new(monitor, shell_pool, sandbox).unwrap();
        assert!(adapter.retry_config().is_none());
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
            .execute_sync_in_dir(
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

    /// SPEC R2.7.1: the synchronous path (terminal hooks, CLI one-shots,
    /// `synchronous: true` tools) waits for the workspace no longer than its
    /// own timeout, says it is waiting, and — if the turn never comes — reports
    /// that the command did not run and who held the workspace.
    #[tokio::test]
    async fn a_sync_call_waits_for_the_workspace_no_longer_than_its_timeout() {
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

        let started = Instant::now();
        let err = adapter
            .execute_sync_in_dir("echo", None, &td.path().to_string_lossy(), Some(1), None)
            .await
            .expect_err("the workspace never frees up, so the command must not run");
        let text = err.to_string();
        assert!(text.contains("Not run"), "{text}");
        assert!(text.contains("op_holder"), "names the holder: {text}");
        assert!(
            started.elapsed()
                < ahma_common::timeouts::TestTimeouts::get(
                    ahma_common::timeouts::TimeoutCategory::Quick
                ),
            "bounded by its own one-second timeout"
        );
        let notices = notices.lock();
        assert_eq!(notices.len(), 1, "one notice per wait: {notices:?}");
        assert!(notices[0].contains("op_holder"), "{notices:?}");
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
                    .execute_sync_in_dir(
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
            .execute_sync_in_dir(
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
        adapter.command_starts(workspace_queue::Lane::ReadOnly);
        adapter.command_starts(workspace_queue::Lane::ReadOnly);
        assert!(live(), "`git status` twice leaves it for the build");
        adapter.command_starts(workspace_queue::Lane::Exclusive);
        assert!(live(), "the build gets it");
        adapter.command_starts(workspace_queue::Lane::Exclusive);
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
        let run = adapter.execute_sync_in_dir(
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
    fn test_adapter_with_retry_config() {
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
        let adapter = Adapter::new(monitor, shell_pool, sandbox)
            .unwrap()
            .with_retry_config(RetryConfig::default());
        assert!(adapter.retry_config().is_some());
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
