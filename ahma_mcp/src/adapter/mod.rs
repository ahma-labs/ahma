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
pub mod lane;
pub mod mutex_groups;
mod preparer;
mod pty_exec;
pub mod spill;
mod time_limit;
mod types;
pub mod workspace_queue;

pub use mutex_groups::CommandMutexRegistry;
pub use preparer::{
    TempFileManager, escape_shell_argument, format_option_flag, needs_file_handling,
    prepare_command_and_args,
};
pub use pty_exec::pty_available;
pub use types::{AsyncExecOptions, ExecutionMode};

use crate::operation_monitor::{Operation, OperationMonitor, OperationStatus};
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
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::Mutex, task::JoinHandle};

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn generate_id(tool_name: &str, command: &str) -> String {
    let id = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    crate::utils::operation::generate_id_with_details(id, tool_name, command)
}

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
        use workspace_queue::Lane;
        let lane = subcommand_config
            .and_then(|s| s.concurrency)
            .unwrap_or_else(|| match shell_command_line(args) {
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
                    lane::classify_shell_command_at(
                        line,
                        &lane::RunSite {
                            cwd: working_dir,
                            read_only_enforced: self.sandbox.can_enforce_read_only(),
                            writes_workspace: &writes_workspace,
                        },
                    )
                }
                None => Lane::Exclusive,
            });
        if lane == Lane::ReadOnly && !self.sandbox.can_enforce_read_only() {
            Lane::Exclusive
        } else {
            lane
        }
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
    ) -> Option<crate::credentials::ssh_agent::host::BrokerLease> {
        let lease = self.credential_brokers.as_ref()?.lease(working_dir)?;
        cmd.env("SSH_AUTH_SOCK", lease.socket());
        Some(lease)
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

    /// Gracefully shuts down the adapter by cancelling active operations, aborting tasks,
    /// and shutting down shell pools. Uses timeouts to avoid hanging indefinitely.
    pub async fn shutdown(&self) {
        tracing::info!("Adapter shutdown initiated: cancelling operations and aborting tasks");

        // 1) Cancel all known operations tracked by this adapter (best-effort)
        {
            let handles = self.task_handles.lock().await;
            for op_id in handles.keys() {
                let reason = Some("Adapter shutdown".to_string());
                let _ = self
                    .monitor
                    .cancel_operation_with_reason(op_id, reason)
                    .await;
            }
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
        let lane = self.resolve_lane(args.as_ref(), subcommand_config, &safe_wd);
        self.command_starts(lane);
        let title = shell_command_line(args.as_ref())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{program} {}", args_vec.join(" ")));
        let timeout = self.sync_timeout(timeout_seconds);
        let lease = match self.enqueue_exclusive(lane, &safe_wd, &op_id, &title) {
            Some(ticket) => Some(self.acquire_for_sync(ticket, timeout).await?),
            None => None,
        };

        audit::record_tool_call(
            &op_id,
            command,
            args.as_ref(),
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
        run.result
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
        let observe = |ahead: &[workspace_queue::HolderInfo]| {
            *last_ahead.lock() = ahead.to_vec();
            if !announced.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let line = format!("ahma: {}", queued_message(ahead));
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
                    queued_message(&ahead)
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
    ) -> SyncRun {
        // Create sandboxed command.
        // `create_shell_command` is only needed for raw /bin/sh invocations where
        // the caller has NOT already added the -c flag via a subcommand config.
        // For bash/powershell the preparer already embeds -c/-Command; always
        // use create_command so the sandbox wrapper is applied without double-wrapping.
        let built = if lane == workspace_queue::Lane::ReadOnly {
            self.command_executor
                .build_read_only_command(&self.sandbox, program, args_vec, safe_wd)
        } else {
            build_sandboxed_command(
                self.command_executor.as_ref(),
                &self.sandbox,
                program,
                args_vec,
                safe_wd,
            )
        };
        let mut cmd = match built {
            Ok(cmd) => cmd,
            Err(e) => return SyncRun::failed(e),
        };
        stamp_lease(&mut cmd, lease);
        #[cfg(unix)]
        let broker = self.lease_credential_broker(&mut cmd, safe_wd);
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
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                return SyncRun::failed(anyhow::anyhow!("Command execution failed: {}", e));
            }
        };
        let mut guard = ProcessGroupGuard::new(child);

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
                stdout_task.abort();
                stderr_task.abort();
                return SyncRun {
                    outcome: audit::Outcome::TimedOut,
                    exit_code: None,
                    result: Err(anyhow::anyhow!(time_limit::limit_reached(
                        timeout.as_secs()
                    ))),
                };
            }
            Ok(Err(e)) => {
                return SyncRun::failed(anyhow::anyhow!("Command execution failed: {}", e));
            }
            Ok(Ok(status)) => {
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
        if result.is_err() {
            return self
                .finalize_sync_denial(
                    command,
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
                result: kernel_report_result(&self.sandbox, report, result, op_id, command).await,
            };
        }
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
        let Some(hit) = sandbox::scan_denial_streams(stderr, stdout) else {
            // A refused `kill` is a boundary too (SPEC R6.2.6): say so, or the
            // agent reads "Operation not permitted" as a dead pid and escalates.
            // Likewise a GPU the sandbox would not open (SPEC R6.2.7): a capability,
            // not a path, so the agent must not go looking for a directory to grant.
            let result = match sandbox::signal_denial_note(stderr, stdout)
                .or_else(|| sandbox::gpu_denial_note(stderr, stdout))
                .or_else(|| sandbox::setuid_denial_note(stderr, stdout))
                .or_else(|| sandbox::launch_services_denial_note(stderr, stdout))
            {
                Some(note) => result.map_err(|e| anyhow::anyhow!("{e}\n\n{note}")),
                None => result,
            };
            return SyncRun {
                outcome: audit::Outcome::Failed,
                exit_code,
                result,
            };
        };
        if !self.sandbox.is_path_in_scope_in_dir(&hit.path, safe_wd) {
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
        SyncRun {
            outcome: audit::Outcome::Failed,
            exit_code,
            result,
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

        let op_id = id
            .map(|s| s.to_string())
            .unwrap_or_else(|| generate_id(tool_name, command));
        let op_id_clone = op_id.clone();
        let wd = safe_wd_str.clone();

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
        let lane = self.resolve_lane(args.as_ref(), subcommand_config, &safe_wd);
        self.command_starts(lane);
        let workspace = self.workspace_key_for(&safe_wd);
        let drift_root = workspace_queue::drift_root(&workspace, &self.sandbox.scopes(), &safe_wd);
        let holder_title = operation
            .command
            .clone()
            .or_else(|| operation.title.clone())
            .unwrap_or_else(|| command.to_string());
        let ticket = self.enqueue_exclusive(lane, &safe_wd, &op_id, &holder_title);
        let display_command = shell_command_line(args.as_ref())
            .map(str::to_string)
            .unwrap_or_else(|| command.to_string());
        self.monitor.add_operation(operation).await;

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

        let task_handles = self.task_handles.clone();

        let handle = tokio::spawn(run_async_operation(AsyncOperationRun {
            op_id,
            command,
            program: program_with_subcommand,
            args_vec,
            working_dir: wd_clone,
            timeout_secs: timeout,
            log_monitor_config,
            monitor,
            shell_pool,
            sandbox,
            task_handles,
            command_executor: self.command_executor.clone(),
            mutex_registry: self.mutex_registry.clone(),
            scope_grant_notifier: self.scope_grant_notifier.clone(),
            lane,
            ticket,
            workspace,
            drift_root,
            display_command,
            watch_handoff: self.watches_handoff(lane),
            #[cfg(unix)]
            credential_brokers: self.credential_brokers.clone(),
        }));

        // Store the handle for graceful shutdown
        self.task_handles
            .lock()
            .await
            .insert(op_id_clone.clone(), handle);

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
        self.monitor.add_operation(operation).await;

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

        let cancellation_token = match self.monitor.get_operation(&op_id).await {
            Some(op) => op.cancellation_token.clone(),
            None => anyhow::bail!("operation {op_id} vanished before start"),
        };
        self.monitor
            .update_status(&op_id, OperationStatus::InProgress, None)
            .await;

        let timeout_ms = timeout
            .map(|t| t * 1000)
            .unwrap_or_else(|| self.shell_pool.config().command_timeout.as_millis() as u64);
        let monitor = self.monitor.clone();
        let sandbox = self.sandbox.clone();
        let command_str = command_str.to_string();
        let task_handles = self.task_handles.clone();
        let op_id_task = op_id.clone();
        let tool_task = tool_name.to_string();
        #[cfg(unix)]
        let credential_brokers = self.credential_brokers.clone();
        let watch_handoff = self.watches_handoff(workspace_queue::Lane::Exclusive);
        // A PTY command is an arbitrary shell command: exclusive (SPEC R2.7).
        let ticket = self.enqueue_exclusive(
            workspace_queue::Lane::Exclusive,
            &safe_wd,
            &op_id,
            &command_str,
        );

        let handle = tokio::spawn(async move {
            let started = Instant::now();
            // Held until the PTY process is gone.
            let Ok(lease) =
                wait_for_lease(ticket, &monitor, &op_id_task, &cancellation_token).await
            else {
                handle_cancellation(&monitor, &op_id_task).await;
                audit::record_tool_complete(
                    &op_id_task,
                    audit::Outcome::Cancelled,
                    started.elapsed().as_millis() as u64,
                    None,
                )
                .await;
                task_handles.lock().await.remove(&op_id_task);
                return;
            };
            let handoff = if watch_handoff {
                Some(begin_handoff_watch(&sandbox, &safe_wd).await)
            } else {
                None
            };
            // Its SSH key broker (SPEC R-CRED.1), as for any other command: a
            // refused signature is an alert on this operation (R-CRED.10). Held
            // until the PTY process is gone.
            #[cfg(unix)]
            let broker = credential_brokers
                .as_ref()
                .and_then(|f| f.lease(&safe_wd))
                .inspect(|broker| alert_broker_refusals(broker, &monitor, &op_id_task));
            #[cfg(unix)]
            let broker_socket = broker.as_ref().map(|b| b.socket().to_path_buf());
            #[cfg(not(unix))]
            let broker_socket: Option<std::path::PathBuf> = None;
            let (outcome, exit_code) = pty_exec::run_pty_operation(
                &sandbox,
                &command_str,
                &safe_wd,
                timeout_ms,
                &cancellation_token,
                &op_id_task,
                &monitor,
                broker_socket.as_deref(),
            )
            .await;
            #[cfg(unix)]
            if let Some(broker) = broker.as_ref() {
                for refusal in broker_refusal_lines(broker) {
                    monitor.append_alert_unique(&op_id_task, refusal).await;
                }
            }
            // The PTY path publishes its own result, so a deny-tier write here
            // reaches the log and the audit trail, not the result text.
            if let Some(watch) = handoff {
                let _ = finish_handoff_watch(watch, &op_id_task, &tool_task).await;
            }
            #[cfg(unix)]
            drop(broker);
            drop(lease);
            audit::record_tool_complete(
                &op_id_task,
                outcome,
                started.elapsed().as_millis() as u64,
                exit_code,
            )
            .await;
            task_handles.lock().await.remove(&op_id_task);
        });
        self.task_handles.lock().await.insert(op_id.clone(), handle);

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
        self.monitor.add_operation(operation).await;

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

        let cancellation_token = match self.monitor.get_operation(&op_id).await {
            Some(op) => op.cancellation_token.clone(),
            None => anyhow::bail!("operation {op_id} vanished before start"),
        };
        self.monitor
            .update_status(&op_id, OperationStatus::InProgress, None)
            .await;

        let timeout_duration = timeout
            .map(Duration::from_secs)
            .unwrap_or_else(|| self.shell_pool.config().command_timeout);
        let monitor = self.monitor.clone();
        let sandbox = self.sandbox.clone();
        let sessions = self.shell_sessions.clone();
        let session_id = session_id.to_string();
        let command_str = command_str.to_string();
        let task_handles = self.task_handles.clone();
        let op_id_task = op_id.clone();
        let tool_task = tool_name.to_string();
        let watch_handoff = self.watches_handoff(workspace_queue::Lane::Exclusive);
        // A session command is an arbitrary shell command: exclusive (SPEC R2.7).
        let ticket = self.enqueue_exclusive(
            workspace_queue::Lane::Exclusive,
            &safe_wd,
            &op_id,
            &command_str,
        );

        let handle = tokio::spawn(async move {
            let started = Instant::now();
            // Held until the session command is done.
            let Ok(lease) =
                wait_for_lease(ticket, &monitor, &op_id_task, &cancellation_token).await
            else {
                handle_cancellation(&monitor, &op_id_task).await;
                audit::record_tool_complete(
                    &op_id_task,
                    audit::Outcome::Cancelled,
                    started.elapsed().as_millis() as u64,
                    None,
                )
                .await;
                task_handles.lock().await.remove(&op_id_task);
                return;
            };
            let handoff = if watch_handoff {
                Some(begin_handoff_watch(&sandbox, &safe_wd).await)
            } else {
                None
            };
            let (outcome, exit_code) = run_session_operation(
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
            // As for PTY: the session path publishes its own result.
            if let Some(watch) = handoff {
                let _ = finish_handoff_watch(watch, &op_id_task, &tool_task).await;
            }
            drop(lease);
            audit::record_tool_complete(
                &op_id_task,
                outcome,
                started.elapsed().as_millis() as u64,
                exit_code,
            )
            .await;
            task_handles.lock().await.remove(&op_id_task);
        });
        self.task_handles.lock().await.insert(op_id.clone(), handle);

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

/// Context for a single background async operation task.
struct AsyncOperationRun {
    op_id: String,
    command: String,
    program: String,
    args_vec: Vec<String>,
    working_dir: String,
    timeout_secs: Option<u64>,
    log_monitor_config: Option<crate::log_monitor::LogMonitorConfig>,
    monitor: Arc<OperationMonitor>,
    shell_pool: Arc<ShellPoolManager>,
    sandbox: Arc<sandbox::Sandbox>,
    task_handles: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
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
        command,
        program,
        args_vec,
        working_dir,
        timeout_secs,
        log_monitor_config,
        monitor,
        shell_pool,
        sandbox,
        task_handles,
        command_executor,
        mutex_registry,
        scope_grant_notifier,
        lane,
        ticket,
        workspace,
        drift_root,
        display_command,
        watch_handoff,
        #[cfg(unix)]
        credential_brokers,
    } = ctx;

    // Timed from the moment the task starts, so queueing behind a command mutex
    // group is visible in the audit record rather than silently excluded.
    let task_start = Instant::now();
    let audit_complete = |outcome: audit::Outcome, exit_code: Option<i32>| {
        let op_id = op_id.clone();
        async move {
            audit::record_tool_complete(
                &op_id,
                outcome,
                task_start.elapsed().as_millis() as u64,
                exit_code,
            )
            .await;
        }
    };

    let cancellation_token = match monitor.get_operation(&op_id).await {
        Some(operation) => operation.cancellation_token.clone(),
        None => {
            tracing::error!("Could not find operation {} for cancellation token", op_id);
            audit_complete(audit::Outcome::Failed, None).await;
            return;
        }
    };

    // ── Workspace write queue (SPEC R2.7) ──────────────────────────────────
    // Wait for every exclusive operation that arrived earlier in this
    // workspace — in this process or any other — before starting. The lease is
    // held until this function returns, i.e. until the process tree is gone.
    let mut lease = match wait_for_lease(ticket, &monitor, &op_id, &cancellation_token).await {
        Ok(lease) => lease,
        Err(()) => {
            handle_cancellation(&monitor, &op_id).await;
            audit_complete(audit::Outcome::Cancelled, None).await;
            task_handles.lock().await.remove(&op_id);
            return;
        }
    };

    monitor
        .update_status(&op_id, OperationStatus::InProgress, None)
        .await;

    // Lifecycle events (Started / terminal) are emitted by the OperationMonitor
    // at each state transition — the single emission point for the unified
    // event stream (SPEC R15).  MCP progress notifications are pushed by the
    // `progress_push` forwarder subscribed to that stream.

    if cancellation_token.is_cancelled() {
        tracing::info!("Operation {} was cancelled before execution started", op_id);
        handle_cancellation(&monitor, &op_id).await;
        audit_complete(audit::Outcome::Cancelled, None).await;
        return;
    }

    // ── Command mutex group gate ────────────────────────────────────────────────────
    // Serialise commands in the same mutex group within the same working
    // directory.  The permit is held for the entire execution and released
    // automatically when it drops at the end of this function.
    let _exclusive_permit = match acquire_mutex_gate(
        &mutex_registry,
        &monitor,
        &op_id,
        &display_command,
        &workspace,
    )
    .await
    {
        Ok(permit) => permit,
        Err(err) => {
            drop(lease.take());
            fail_operation_with_error(&monitor, &op_id, err).await;
            audit_complete(audit::Outcome::TimedOut, None).await;
            task_handles.lock().await.remove(&op_id);
            return;
        }
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
            let err_msg = format!("Failed to create sandboxed command: {}", e);
            drop(lease.take());
            fail_operation_with_error(&monitor, &op_id, err_msg).await;
            audit_complete(audit::Outcome::Failed, None).await;
            task_handles.lock().await.remove(&op_id);
            return;
        }
    };

    stamp_lease(&mut proc_cmd, lease.as_ref());
    // The command's SSH key broker (SPEC R-CRED.1): a refused signature is an
    // alert on this operation while it still runs (R-CRED.10). Held until the
    // command has finished.
    #[cfg(unix)]
    let broker = credential_brokers
        .as_ref()
        .and_then(|f| f.lease(&wd_path))
        .inspect(|broker| {
            proc_cmd.env("SSH_AUTH_SOCK", broker.socket());
            alert_broker_refusals(broker, &monitor, &op_id);
        });
    // The tag the kernel's records of this operation's denials carry
    // (SPEC R-DENY.1), for the failure path to read them back.
    if let Some(tag) = sandbox::kernel_denials::tag_of(proc_cmd.as_std().get_args()) {
        sandbox::kernel_denials::remember_tag(&op_id, tag);
    }

    let timeout_ms = timeout_secs
        .map(|t| t * 1000)
        .unwrap_or_else(|| shell_pool.config().command_timeout.as_millis() as u64);

    let drift = (lane != workspace_queue::Lane::Service).then_some(DriftProbe {
        root: drift_root,
        lane,
    });

    // SPEC R6.1.7: the "before" inventory of the deny tier, taken as late as
    // possible — after the queue and the mutex gate, just before the spawn.
    let mut handoff = if watch_handoff {
        Some(begin_handoff_watch(&sandbox, &wd_path).await)
    } else {
        None
    };

    // Single execution path: stream stdout/stderr line-by-line for every
    // operation (SPEC R15.2).  Lines flow through the OperationMonitor, which
    // appends to the tail buffer and emits `OutputLine` on the unified event
    // stream — so the TUI and hub subscribers see output as it is produced.
    // Log-monitor alerting only runs when a config was provided.
    let (outcome, exit_code) = execute_with_streaming(
        &mut proc_cmd,
        timeout_ms,
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
        &mut handoff,
        &mut lease,
        #[cfg(unix)]
        broker.as_ref(),
    )
    .await;
    // Normally compared inside, where the alert can lead the result. Still
    // here only when the run ended another way — timeout, cancellation, spawn
    // failure — and its result is already published: a partial run can still
    // have written a hook, so the log and the audit trail hear about it.
    if let Some(watch) = handoff.take() {
        let _ = finish_handoff_watch(watch, &op_id, &command).await;
    }
    // Normally already released inside, just before the terminal transition.
    drop(lease);
    audit_complete(outcome, exit_code).await;

    task_handles.lock().await.remove(&op_id);
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
pub(crate) fn queued_message(ahead: &[workspace_queue::HolderInfo]) -> String {
    match ahead {
        [] => "Queued: waiting for the workspace, held by another ahma process".to_string(),
        [only] => format!(
            "Queued: waiting for the workspace behind {}",
            only.describe()
        ),
        [first, rest @ ..] => format!(
            "Queued: waiting for the workspace behind {} and {} more",
            first.describe(),
            rest.len()
        ),
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
    let observe = |ahead: &[workspace_queue::HolderInfo]| {
        monitor.try_note_liveness(op_id);
        monitor.note_progress(op_id, queued_message(ahead));
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

/// Stamp a child spawned under a lease with it (SPEC R2.7.7), so an ahma the
/// command itself starts does not wait for the lease its ancestor holds.
/// A failed command's result, as the kernel's records describe it (SPEC
/// R-DENY): a grantable denial is the typed [`sandbox::SandboxError::RuntimeDenial`]
/// the MCP boundary turns into a structured `sandbox_denial` payload, with
/// every record's line in its details; other denials are their lines; no
/// records leave the failure as it was, with nothing suggested (R-DENY.3).
async fn kernel_report_result(
    sandbox: &sandbox::Sandbox,
    report: sandbox::grant_channel::KernelReport,
    result: Result<String, anyhow::Error>,
    op_id: &str,
    command: &str,
) -> Result<String, anyhow::Error> {
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
/// Split out of [`run_async_operation`] so its early-return control flow
/// doesn't nest a match inside an `if let` inside that function's straight
/// line of guard clauses.
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

async fn fail_operation_with_error(
    monitor: &Arc<OperationMonitor>,
    op_id: &str,
    error_message: String,
) {
    tracing::error!("{}", error_message);
    monitor
        .update_status(
            op_id,
            OperationStatus::Failed,
            Some(Value::String(error_message)),
        )
        .await;
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
) -> (audit::Outcome, Option<i32>) {
    let mut spill_writer = spill::SpillWriter::create(op_id).await;
    let mut collected = BoundedLineCollector::default();

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
                monitor
                    .update_status(
                        op_id,
                        OperationStatus::Cancelled,
                        Some(Value::String("Operation was cancelled".to_string())),
                    )
                    .await;
                return (audit::Outcome::Cancelled, None);
            }

            line = line_rx.recv() => {
                if let Some(line) = line {
                    let safe = crate::log_monitor::redact_sensitive_line(&line);
                    spill_writer.write_line(&safe, false).await;
                    collected.push(safe.clone());
                    monitor.append_output_line(op_id, safe, false).await;
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
        spill_writer.write_line(&safe, false).await;
        collected.push(safe.clone());
        monitor.append_output_line(op_id, safe, false).await;
    }
    spill_writer.finish().await;

    finalize_session_result(exec_result, &collected, session_id, op_id, monitor).await
}

/// Turn the terminal result of a session-protocol command into the final
/// monitor status update and the `(outcome, exit_code)` pair the caller
/// reports to the audit log.
async fn finalize_session_result(
    exec_result: anyhow::Result<i32>,
    collected: &BoundedLineCollector,
    session_id: &str,
    op_id: &str,
    monitor: &Arc<OperationMonitor>,
) -> (audit::Outcome, Option<i32>) {
    match exec_result {
        Ok(exit_code) => {
            let final_output = json!({
                // Session output is a single merged stream (per-command 2>&1).
                "stdout": collected.rendered_output(),
                "stderr": "",
                "exit_code": exit_code,
                "session_id": session_id,
                "stdout_truncated_lines": collected.dropped_lines(),
                "stdout_truncated_bytes": collected.dropped_bytes(),
                "output_file": spill::operation_spill_path(op_id).to_string_lossy(),
            });
            // Monitor status and audit outcome are two views of one verdict —
            // decided together so they cannot drift apart.
            let (status, outcome) = if exit_code == 0 {
                (OperationStatus::Completed, audit::Outcome::Completed)
            } else {
                (OperationStatus::Failed, audit::Outcome::Failed)
            };
            monitor
                .update_status(op_id, status, Some(final_output))
                .await;
            (outcome, Some(exit_code))
        }
        Err(e) => {
            let (status, outcome) = if e.to_string().contains("timed out") {
                (OperationStatus::TimedOut, audit::Outcome::TimedOut)
            } else {
                (OperationStatus::Failed, audit::Outcome::Failed)
            };
            monitor
                .update_status(op_id, status, Some(Value::String(e.to_string())))
                .await;
            (outcome, None)
        }
    }
}

async fn cancel_operation_timed_out(
    monitor: &Arc<OperationMonitor>,
    op_id: &str,
    duration_ms: u64,
) {
    let timeout_reason = format!(
        "Operation timed out after {}ms (exceeded timeout limit). {}",
        duration_ms,
        time_limit::how_to_raise(duration_ms.div_ceil(1000))
    );
    monitor
        .update_status(
            op_id,
            OperationStatus::TimedOut,
            Some(Value::String(timeout_reason)),
        )
        .await;
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
/// Returns how the operation ended and, where one exists, its exit code — the
/// caller turns that into the single `tool_complete` audit event that pairs with
/// the `tool_call` written before the task was spawned.
#[allow(clippy::too_many_arguments)]
async fn execute_with_streaming(
    proc_cmd: &mut tokio::process::Command,
    timeout_ms: u64,
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
    handoff: &mut Option<HandoffWatch>,
    lease: &mut Option<workspace_queue::Lease>,
    #[cfg(unix)] broker: Option<&crate::credentials::ssh_agent::host::BrokerLease>,
) -> (audit::Outcome, Option<i32>) {
    use tokio::io::{AsyncBufReadExt, BufReader};

    // Ensure stdin/stdout/stderr are strictly isolated (should already be set by sandbox)
    proc_cmd.stdin(std::process::Stdio::null());
    proc_cmd.stdout(std::process::Stdio::piped());
    proc_cmd.stderr(std::process::Stdio::piped());

    let child = match proc_cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            drop(lease.take());
            fail_operation_with_error(op_monitor, op_id, format!("Failed to spawn process: {}", e))
                .await;
            return (audit::Outcome::Failed, None);
        }
    };
    let mut guard = ProcessGroupGuard::new(child);

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

    let timeout_deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    // Warn once at 80% of the limit (SPEC R2.6.6), so a healthy job near it
    // is not stopped without notice.
    let near_limit_at = tokio::time::Instant::now() + Duration::from_millis(timeout_ms * 8 / 10);
    let mut warned_near_limit = false;

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
                drop(lease.take());
                handle_cancellation(op_monitor, op_id).await;
                return (audit::Outcome::Cancelled, None);
            }

            _ = tokio::time::sleep_until(near_limit_at), if !warned_near_limit => {
                warned_near_limit = true;
                op_monitor
                    .append_alert(op_id, time_limit::near_limit(timeout_ms.div_ceil(1000)))
                    .await;
            }

            // Timeout
            _ = tokio::time::sleep_until(timeout_deadline) => {
                tracing::warn!("Operation {} timed out during streaming", op_id);
                if !kill_process_tree(guard.child_mut()).await {
                    tracing::warn!("Operation {} timed out but its process did not reap cleanly", op_id);
                }
                spill.finish().await;
                let duration_ms = start_time.elapsed().as_millis() as u64;
                drop(lease.take());
                cancel_operation_timed_out(op_monitor, op_id, duration_ms).await;
                return (audit::Outcome::TimedOut, None);
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
                handle_stream_line(result, true, &mut collected_stderr, &mut log_monitor, op_id, op_monitor, &mut spill).await;
            }

            // Read stdout line
            result = stdout_reader.next_line(), if !stdout_done => {
                if matches!(result, Ok(Some(_))) {
                    last_output_at = tokio::time::Instant::now();
                    last_cpu_ms = None;
                } else {
                    stdout_done = true;
                }
                handle_stream_line(result, false, &mut collected_stdout, &mut log_monitor, op_id, op_monitor, &mut spill).await;
            }
        }

        // Check if the child process has exited
        // We use try_wait() to avoid blocking — if streams are closed the process may
        // have already exited.
        match guard.child_mut().try_wait() {
            Ok(Some(_status)) => {
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
        handoff,
        lease,
        #[cfg(unix)]
        broker,
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
            true,
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
            false,
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
        if let Some((path, access)) = &report.grant {
            audit::record_sandbox_denial(Some(op_id), path, access.label(), tool).await;
        }
        for line in report.lines {
            op_monitor.append_alert(op_id, line).await;
        }
        if let Some(hint) = sandbox::build_diagnostics::diagnose_streams(stderr_str, stdout_str) {
            op_monitor.append_alert(op_id, hint.remediation).await;
        }
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
    let target_and_access = if let Some(hit) = sandbox::scan_denial_streams(stderr_str, stdout_str)
    {
        let base_scope = sandbox.scopes().first().cloned();
        let base_wd = working_dir.or(base_scope.as_deref());
        let in_scope = if let Some(wd) = base_wd {
            sandbox.is_path_in_scope_in_dir(&hit.path, wd)
        } else {
            sandbox.is_path_in_scope(&hit.path)
        };
        if !in_scope {
            let target = sandbox::grant_channel::resolve_grant_target(&hit.path, base_wd, sandbox);
            Some((target, hit.access))
        } else {
            None
        }
    } else {
        None
    };

    if target_and_access.is_none()
        && let Some(note) = sandbox::signal_denial_note(stderr_str, stdout_str)
            .or_else(|| sandbox::gpu_denial_note(stderr_str, stdout_str))
            .or_else(|| sandbox::setuid_denial_note(stderr_str, stdout_str))
            .or_else(|| sandbox::launch_services_denial_note(stderr_str, stdout_str))
    {
        op_monitor.append_alert(op_id, note).await;
    }

    if let Some((target, access)) = target_and_access {
        // The alert below is transient; this line is the durable copy of the
        // same structured denial (SPEC R5.4.7).
        audit::record_sandbox_denial(Some(op_id), &target, access.label(), tool).await;
        let remediation = sandbox::grant_channel::runtime_denial_remediation(&target, access);
        tracing::warn!(
            "Operation {} hit an out-of-scope sandbox denial on {}: {}",
            op_id,
            target.display(),
            remediation
        );
        op_monitor.append_alert(op_id, remediation).await;
    } else {
        // The grant flow above only fires for *out-of-scope* denials. The
        // sibling failure — an in-scope EPERM from macOS provenance/sccache
        // contamination — produces no kernel event and would otherwise surface
        // as a bare `os error 1`. Diagnose it and attach the remediation as an
        // alert so the user gets an actionable line, not an errno.
        if let Some(hint) = sandbox::build_diagnostics::diagnose_streams(stderr_str, stdout_str) {
            tracing::warn!(
                "Operation {} failed with sandbox diagnostic ({:?}): {}",
                op_id,
                hint.kind,
                hint.remediation
            );
            op_monitor.append_alert(op_id, hint.remediation).await;
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
    }
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
    handoff: &mut Option<HandoffWatch>,
    lease: &mut Option<workspace_queue::Lease>,
    #[cfg(unix)] broker: Option<&crate::credentials::ssh_agent::host::BrokerLease>,
) -> (audit::Outcome, Option<i32>) {
    let exit_status = child.wait().await;
    let duration_ms = start_time.elapsed().as_millis() as u64;
    tracing::debug!("Operation {} process exited after {}ms", op_id, duration_ms);
    let exit_code = exit_status
        .as_ref()
        .ok()
        .and_then(|s| s.code())
        .unwrap_or(-1);
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
    }

    let changed_during_run = match drift {
        Some(probe) => probe.report(start_time.elapsed()).await,
        None => None,
    };
    // The process tree is gone: compare the deny tier with its inventory from
    // before the spawn (SPEC R6.1.7).
    let handoff_report = match handoff.take() {
        Some(watch) => finish_handoff_watch(watch, op_id, tool).await,
        None => None,
    };

    let mut final_output = json!({
        "stdout": stdout_str,
        "stderr": stderr_str,
        "exit_code": exit_code,
        "stdout_truncated_lines": collected_stdout.dropped_lines(),
        "stderr_truncated_lines": collected_stderr.dropped_lines(),
        "stdout_truncated_bytes": collected_stdout.dropped_bytes(),
        "stderr_truncated_bytes": collected_stderr.dropped_bytes(),
        // Complete output record — query with file tools (tail/grep) when the
        // inline stdout/stderr above was truncated.
        "output_file": spill::operation_spill_path(op_id).to_string_lossy(),
    });
    if let Some(changed) = changed_during_run {
        final_output["changed_during_run"] = changed;
    }
    if let Some(report) = handoff_report {
        let alert = report.render_alert();
        final_output["handoff_writes"] = report.to_json();
        final_output["handoff_alert"] = json!(alert);
        // Before the terminal transition, while the operation is still active:
        // the `Alert` event is what the hub and the TUI show.
        op_monitor.append_alert(op_id, alert).await;
    }
    // What the SSH key broker refused this command, one line each (SPEC R-CRED.10):
    // appended before the terminal transition so a waiter (or fast exit) observes it
    // deterministically without racing the broker thread's background alert.
    #[cfg(unix)]
    if let Some(broker) = broker {
        for refusal in broker_refusal_lines(broker) {
            op_monitor.append_alert_unique(op_id, refusal).await;
        }
    }
    // The process tree is gone and the drift probe has looked: hand the
    // workspace on *before* anyone can observe this operation as finished, so
    // a caller woken by the terminal transition never finds it still held
    // (SPEC R2.7.1).
    drop(lease.take());

    let status = if success {
        OperationStatus::Completed
    } else {
        OperationStatus::Failed
    };
    // The terminal event emitted by this transition carries the result to all
    // subscribers (MCP progress push, hub, TUI).
    op_monitor
        .update_status(op_id, status, Some(final_output))
        .await;

    let outcome = if success {
        audit::Outcome::Completed
    } else {
        audit::Outcome::Failed
    };
    (outcome, exit_status.as_ref().ok().and_then(|s| s.code()))
}

/// Handle cancellation of an operation — shared logic for both execution paths.
///
/// The `Cancelled` event emitted by the status transition carries the reason
/// to all subscribers.
async fn handle_cancellation(monitor: &Arc<OperationMonitor>, op_id: &str) {
    monitor
        .update_status(
            op_id,
            OperationStatus::Cancelled,
            Some(Value::String("Operation was cancelled".to_string())),
        )
        .await;
}

/// Handle one result from a `next_line()` call inside the streaming select loop.
///
/// Dispatches to `process_streaming_line` on success, ignores closed-stream
/// signals (`Ok(None)`), and logs a warning on read errors.
async fn handle_stream_line(
    result: Result<Option<String>, std::io::Error>,
    is_stderr: bool,
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
                is_stderr,
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
            let stream = if is_stderr { "stderr" } else { "stdout" };
            tracing::warn!("Error reading {} for {}: {}", stream, op_id, e);
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
    is_stderr: bool,
    collector: &mut BoundedLineCollector,
    log_monitor: &mut Option<crate::log_monitor::LogMonitor>,
    op_id: &str,
    op_monitor: &Arc<OperationMonitor>,
    spill: &mut spill::SpillWriter,
) {
    let safe_line = crate::log_monitor::redact_sensitive_line(line);

    // The spill file records every redacted line — the faithful full record,
    // independent of the bounded tail.
    spill.write_line(&safe_line, is_stderr).await;

    collector.push(safe_line.clone());
    op_monitor
        .append_output_line(op_id, safe_line, is_stderr)
        .await;

    if let Some(log_monitor) = log_monitor
        && let Some(snapshot) = log_monitor.process_line(line, is_stderr)
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
                false,
                false,
                false,
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
            crate::sandbox::Sandbox::new(vec![td.path().to_path_buf()], mode, false, false, false)
                .unwrap(),
        );
        (Adapter::new(monitor, shell_pool, sandbox).unwrap(), td)
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
                false,
                false,
                false,
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
        assert!(queued_message(&[]).contains("another ahma process"));
        assert!(queued_message(std::slice::from_ref(&one)).contains("op_1"));
        let both = queued_message(&[one, two]);
        assert!(both.contains("op_1") && both.contains("1 more"), "{both}");
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
                false,
                false,
                false,
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
                false,
                false,
                false,
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
                false,
                false,
                false,
            )
            .unwrap(),
        );
        let adapter = Adapter::new(monitor, shell_pool, sandbox).unwrap();
        // Shutdown with no active tasks should complete without error
        adapter.shutdown().await;
    }
}
