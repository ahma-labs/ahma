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
//! 1. **Performance Path (Async + Shell Pooling)**:
//!    By default, tools execute asynchronously. The adapter requests a pre-warmed
//!    shell process from the [`ShellPoolManager`](crate::shell_pool::ShellPoolManager).
//!    This avoids the 200ms-500ms latency typically associated with spawning a new
//!    shell and loading environment profiles. Results are tracked via the
//!    [`OperationMonitor`](crate::operation_monitor::OperationMonitor) and pushed
//!    back via notifications.
//!
//! 2. **Correctness Path (Synchronous / Direct Spawn)**:
//!    Some operations (like `cargo add` or configuration changes) require immediate
//!    completion to prevent race conditions. When a tool is marked as `synchronous`
//!    or when the `--sync` flag is active, the adapter bypasses the shell pool and
//!    spawns a direct process, waiting for it to exit before returning.
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

pub mod executor;
pub mod mutex_groups;
mod preparer;
mod pty_exec;
pub mod spill;
mod types;

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
use crate::shell_pool::ShellPoolManager;
use ahma_common::event_dispatcher::{EventDispatcher, OperationEvent};
use anyhow::Result;
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, VecDeque},
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
/// *   **Command Execution**: Executes tools using either a pre-warmed shell pool (for async
///     performance) or standard process spawning.
/// *   **Resource Management**: Manages temporary files created for complex arguments and
///     ensures they are cleaned up.
/// *   **Shell Pooling**: Integrates with `ShellPoolManager` to reuse shell processes,
///     reducing latency for frequent commands.
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
    /// Pre-warmed shell pool manager for async execution.
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
    /// Token minimization and output optimizer context.
    pub output_optimizer: Arc<tokio::sync::Mutex<crate::output_optimizer::OutputOptimizer>>,
    /// Persistent stateful shell sessions (`session_id` parameter).
    pub shell_sessions: Arc<crate::shell_session::ShellSessionManager>,
    /// Configurable per-(group, directory) command serialisation registry.
    pub mutex_registry: Arc<CommandMutexRegistry>,
    /// Optional sink for auto-detected sandbox scope violations. When set, an
    /// out-of-scope path (rejected up front, or surfaced by a stderr denial) raises
    /// a "grant access to X?" prompt through this notifier. `None` disables
    /// detection (the default). The notifier only *persists* an approved grant — it
    /// never widens the live session (SPEC R5).
    scope_grant_notifier: Option<Arc<dyn sandbox::ScopeGrantNotifier>>,
}

impl Adapter {
    /// Creates a new `Adapter` instance.
    ///
    /// The adapter requires an `OperationMonitor` for tracking async tasks, a `ShellPoolManager`
    /// for efficient shell execution, and a `Sandbox` for security context.
    ///
    /// # Arguments
    ///
    /// * `monitor` - Shared reference to the operation monitor
    /// * `shell_pool` - Shared reference to the shell pool manager
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
            output_optimizer: Arc::new(tokio::sync::Mutex::new(
                crate::output_optimizer::OutputOptimizer::new(false, None),
            )),
            shell_sessions: crate::shell_session::ShellSessionManager::new(),
            mutex_registry,
            scope_grant_notifier: None,
        })
    }

    /// Convenience factory: build a [`CommandMutexRegistry`] from a slice of group configs.
    ///
    /// This is the same as calling `CommandMutexRegistry::from_config(groups)` directly.
    pub fn mutex_registry_from(
        groups: &[ahma_common::config::MutexGroupConfig],
    ) -> CommandMutexRegistry {
        CommandMutexRegistry::from_config(groups)
    }

    /// Subscribe to the unified operation event stream.
    ///
    /// Returns a `broadcast::Receiver` that yields every [`OperationEvent`] emitted
    /// by this adapter.  Events are wrapped in `Arc` so cloning is cheap.
    ///
    /// # Note
    ///
    /// Events emitted before this call are not replayed.  Subscribe before triggering
    /// the operation you want to observe.
    pub fn subscribe_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<std::sync::Arc<OperationEvent>> {
        self.event_dispatcher.subscribe()
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

    /// Validate a working directory against the sandbox scope, raising a scope-grant
    /// prompt (best-effort, never blocking the error) when it is rejected as
    /// out-of-scope. Returns the same `Result` as [`Sandbox::validate_path`] so
    /// callers keep failing closed.
    async fn validate_working_dir(
        &self,
        working_dir: &str,
        tool: &str,
    ) -> Result<std::path::PathBuf> {
        match self
            .sandbox
            .validate_path(std::path::Path::new(working_dir))
        {
            Ok(p) => Ok(p),
            Err(e) => {
                sandbox::grant_channel::notify_pre_exec(
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

        // 3) Shut down all shell pools (kills any lingering shell processes)
        tracing::info!("Shutting down shell pools");
        self.shell_pool.shutdown_all().await;

        // 4) Kill persistent session shells
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

        let timeout = timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or_else(|| self.shell_pool.config().command_timeout);

        // Create sandboxed command.
        // `create_shell_command` is only needed for raw /bin/sh invocations where
        // the caller has NOT already added the -c flag via a subcommand config.
        // For bash/powershell the preparer already embeds -c/-Command; always
        // use create_command so the sandbox wrapper is applied without double-wrapping.
        let mut cmd = build_sandboxed_command(
            self.command_executor.as_ref(),
            &self.sandbox,
            &program,
            &args_vec,
            &safe_wd,
        )?;

        // Spawn manually (rather than `cmd.output()`) so a timeout can take down
        // the whole process group — `cmd.output()` drops the future on timeout,
        // and `kill_on_drop` then kills only the direct `sandbox-exec` child,
        // orphaning `sh`/`cargo`/`rustc` descendants. `base_command` pipes
        // stdout/stderr and makes the child a process-group leader.
        // Match `cmd.output()`'s guarantee that stdout/stderr are captured.
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Command execution failed: {}", e))?;
        // Capture the pid before `wait_with_output` consumes `child`.
        #[cfg(unix)]
        let child_pid = child.id();

        let output_res = tokio::time::timeout(timeout, child.wait_with_output()).await;

        let output = match output_res {
            Err(_) => {
                // Kill the entire process group so build descendants don't orphan.
                #[cfg(unix)]
                if let Some(pid) = child_pid {
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                }
                return Err(anyhow::anyhow!(
                    "Operation timed out (exceeded timeout limit): {} seconds",
                    timeout.as_secs()
                ));
            }
            Ok(Err(e)) => return Err(anyhow::anyhow!("Command execution failed: {}", e)),
            Ok(Ok(output)) => output,
        };

        // A non-zero exit may be a runtime sandbox denial the kernel did not name;
        // scan stderr and (best-effort, never blocking the result) offer to grant
        // an out-of-scope path it references.
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let result = interpret_sync_command_output(output);
        if result.is_err() {
            sandbox::grant_channel::notify_stderr_denial(
                &self.sandbox,
                self.scope_grant_notifier.as_ref(),
                &stderr,
                command,
            )
            .await;
            // When the failure was a kernel denial on an out-of-scope path, return
            // it as a typed error so the MCP boundary attaches a structured
            // `sandbox_denial` payload (path + grant->restart->retry remediation)
            // instead of leaving the agent with a raw `os error 1`.
            if let Some(hit) = sandbox::scan_denial(&stderr)
                && !self.sandbox.is_path_in_scope(&hit.path)
            {
                let details = result.err().map(|e| e.to_string()).unwrap_or_default();
                return Err(sandbox::SandboxError::RuntimeDenial {
                    path: hit.path,
                    access: hit.access,
                    scopes: self.sandbox.scopes().to_vec(),
                    details,
                }
                .into());
            }
        }
        result
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
    /// This method queues the command for execution in a background task, potentially using a
    /// pre-warmed shell from the pool. The result will be reported via the `OperationMonitor`
    /// and any registered callbacks.
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
        self.monitor.add_operation(operation).await;

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
            output_optimizer: self.output_optimizer.clone(),
            mutex_registry: self.mutex_registry.clone(),
            scope_grant_notifier: self.scope_grant_notifier.clone(),
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

        let handle = tokio::spawn(async move {
            pty_exec::run_pty_operation(
                &sandbox,
                &command_str,
                &safe_wd,
                timeout_ms,
                &cancellation_token,
                &op_id_task,
                &monitor,
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

        let handle = tokio::spawn(async move {
            run_session_operation(
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
    output_optimizer: Arc<tokio::sync::Mutex<crate::output_optimizer::OutputOptimizer>>,
    mutex_registry: Arc<CommandMutexRegistry>,
    scope_grant_notifier: Option<Arc<dyn sandbox::ScopeGrantNotifier>>,
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
        output_optimizer,
        mutex_registry,
        scope_grant_notifier,
    } = ctx;

    let cancellation_token = match monitor.get_operation(&op_id).await {
        Some(operation) => operation.cancellation_token.clone(),
        None => {
            tracing::error!("Could not find operation {} for cancellation token", op_id);
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
        return;
    }

    // ── Command mutex group gate ────────────────────────────────────────────────────
    // Serialise commands in the same mutex group within the same working
    // directory.  The permit is held for the entire execution and released
    // automatically when it drops at the end of this function.
    let _exclusive_permit = if let Some(group) = mutex_registry.find_group(&command) {
        let group_name = group.name.clone();
        monitor.note_progress(
            &op_id,
            format!(
                "Queued: waiting for '{group_name}' mutex \
                 (another {group_name} command is running in this directory)"
            ),
        );
        let wd_path = std::path::Path::new(&working_dir);
        match mutex_registry.acquire(group, wd_path).await {
            Ok(permit) => {
                tracing::debug!(op_id = %op_id, group = %group_name, "Acquired mutex group gate");
                Some(permit)
            }
            Err(mutex_groups::MutexGroupError::Timeout { secs, .. }) => {
                let err = format!(
                    "Timed out waiting for '{group_name}' exclusive lock after {secs}s. \
                     Another {group_name} command is still running in this directory. \
                     Try again after it completes."
                );
                fail_operation_with_error(&monitor, &op_id, err).await;
                task_handles.lock().await.remove(&op_id);
                return;
            }
            Err(mutex_groups::MutexGroupError::Closed { .. }) => {
                // Semaphore was closed — should never happen; proceed without gate.
                tracing::warn!("mutex group '{group_name}' semaphore unexpectedly closed");
                None
            }
        }
    } else {
        None
    };

    let start_time = Instant::now();
    let wd_path = std::path::PathBuf::from(&working_dir);
    let mut proc_cmd = match build_sandboxed_command(
        command_executor.as_ref(),
        &sandbox,
        &program,
        &args_vec,
        &wd_path,
    ) {
        Ok(cmd) => cmd,
        Err(e) => {
            let err_msg = format!("Failed to create sandboxed command: {}", e);
            fail_operation_with_error(&monitor, &op_id, err_msg).await;
            task_handles.lock().await.remove(&op_id);
            return;
        }
    };

    let timeout_ms = timeout_secs
        .map(|t| t * 1000)
        .unwrap_or_else(|| shell_pool.config().command_timeout.as_millis() as u64);

    // Single execution path: stream stdout/stderr line-by-line for every
    // operation (SPEC R15.2).  Lines flow through the OperationMonitor, which
    // appends to the tail buffer and emits `OutputLine` on the unified event
    // stream — so the TUI and hub subscribers see output as it is produced.
    // Log-monitor alerting only runs when a config was provided.
    execute_with_streaming(
        &mut proc_cmd,
        timeout_ms,
        log_monitor_config,
        &cancellation_token,
        &op_id,
        start_time,
        &monitor,
        &output_optimizer,
        &sandbox,
        scope_grant_notifier.as_ref(),
        &command,
    )
    .await;

    task_handles.lock().await.remove(&op_id);
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
) {
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
                return;
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
            let status = if exit_code == 0 {
                OperationStatus::Completed
            } else {
                OperationStatus::Failed
            };
            monitor
                .update_status(op_id, status, Some(final_output))
                .await;
        }
        Err(e) => {
            let timed_out = e.to_string().contains("timed out");
            let status = if timed_out {
                OperationStatus::TimedOut
            } else {
                OperationStatus::Failed
            };
            monitor
                .update_status(op_id, status, Some(Value::String(e.to_string())))
                .await;
        }
    }
}

async fn cancel_operation_timed_out(
    monitor: &Arc<OperationMonitor>,
    op_id: &str,
    duration_ms: u64,
) {
    let timeout_reason = format!(
        "Operation timed out after {}ms (exceeded timeout limit)",
        duration_ms
    );
    monitor
        .update_status(
            op_id,
            OperationStatus::TimedOut,
            Some(Value::String(timeout_reason)),
        )
        .await;
}

/// Grace period for [`kill_process_tree`] to confirm the direct child was reaped
/// after SIGKILL. A child stuck in an uninterruptible kernel wait (D-state: a
/// denied write being retried, a held file lock) will not die promptly even on
/// SIGKILL; bounding the reap keeps the executor from blocking forever on it.
const KILL_REAP_GRACE: Duration = Duration::from_secs(5);

/// Kill a spawned command and its entire process group, then **verify** the
/// direct child was reaped within [`KILL_REAP_GRACE`].
///
/// Commands are spawned as process-group leaders (see `Sandbox::base_command`),
/// so on Unix `kill(-pgid)` takes down the whole descendant tree — e.g.
/// `sandbox-exec → sh → cargo → rustc` — instead of orphaning the grandchildren
/// when only the direct child is signalled. On non-Unix it falls back to killing
/// the direct child.
///
/// Returns `true` if the child was confirmed dead, `false` if it did not reap
/// within the grace window (it is likely suspended or wedged in the kernel, and
/// may linger as an orphan). The boolean lets callers log the difference instead
/// of silently assuming the kill worked.
async fn kill_process_tree(child: &mut tokio::process::Child) -> bool {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Negative pid targets the process group led by the child.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    // `start_kill` sends SIGKILL to the direct child (idempotent on Unix after the
    // group kill); the bounded `wait` confirms the reap rather than blocking
    // unboundedly on an unresponsive child.
    let _ = child.start_kill();
    match tokio::time::timeout(KILL_REAP_GRACE, child.wait()).await {
        Ok(Ok(_status)) => true,
        Ok(Err(e)) => {
            tracing::warn!("kill_process_tree: error reaping child: {}", e);
            false
        }
        Err(_) => {
            tracing::error!(
                "kill_process_tree: child did not exit within {:.0}s of SIGKILL — \
                 it is likely suspended or wedged in the kernel and may orphan",
                KILL_REAP_GRACE.as_secs_f64()
            );
            false
        }
    }
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
#[allow(clippy::too_many_arguments)]
async fn execute_with_streaming(
    proc_cmd: &mut tokio::process::Command,
    timeout_ms: u64,
    monitor_config: Option<crate::log_monitor::LogMonitorConfig>,
    cancellation_token: &tokio_util::sync::CancellationToken,
    op_id: &str,
    start_time: Instant,
    op_monitor: &Arc<OperationMonitor>,
    output_optimizer: &Arc<tokio::sync::Mutex<crate::output_optimizer::OutputOptimizer>>,
    sandbox: &Arc<sandbox::Sandbox>,
    scope_grant_notifier: Option<&Arc<dyn sandbox::ScopeGrantNotifier>>,
    tool: &str,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};

    // Ensure stdout/stderr are piped (should already be set by sandbox)
    proc_cmd.stdout(std::process::Stdio::piped());
    proc_cmd.stderr(std::process::Stdio::piped());

    let mut child = match proc_cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            fail_operation_with_error(op_monitor, op_id, format!("Failed to spawn process: {}", e))
                .await;
            return;
        }
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

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

    let mut still_running_interval = tokio::time::interval(Duration::from_secs(10));
    still_running_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the first (immediate) tick so heartbeats start at t+10s, not t+0.
    still_running_interval.tick().await;
    let mut elapsed_secs = 0;

    // ── Liveness watchdog input ──────────────────────────────────────────────
    // The idle watchdog kills an operation that has produced no output for
    // `idle_timeout`. Silence is not a stall: `… | tail` buffers everything until
    // EOF, a compile can be quiet for minutes. So once an operation has gone
    // quiet we ask whether its process *tree* is still burning CPU, and treat
    // that as proof of life. Sampling is deferred until the operation is actually
    // silent, so a chatty command never pays for it.
    let child_pid = child.id();
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
                if !kill_process_tree(&mut child).await {
                    tracing::warn!("Operation {} cancelled but its process did not reap cleanly", op_id);
                }
                spill.finish().await;
                handle_cancellation(op_monitor, op_id).await;
                return;
            }

            // Timeout
            _ = tokio::time::sleep_until(timeout_deadline) => {
                tracing::warn!("Operation {} timed out during streaming", op_id);
                if !kill_process_tree(&mut child).await {
                    tracing::warn!("Operation {} timed out but its process did not reap cleanly", op_id);
                }
                spill.finish().await;
                let duration_ms = start_time.elapsed().as_millis() as u64;
                cancel_operation_timed_out(op_monitor, op_id, duration_ms).await;
                return;
            }

            _ = still_running_interval.tick() => {
                elapsed_secs += 10;
                let elapsed_msg = format!("still running ({}s elapsed)", elapsed_secs);
                tracing::info!("Operation {}: {}", op_id, elapsed_msg);
                op_monitor.note_progress(op_id, elapsed_msg);

                // Silent for a while? Ask whether the process tree is still working
                // before the idle watchdog is allowed to call it wedged.
                if let Some(probe_after) = cpu_probe_after
                    && last_output_at.elapsed() >= probe_after
                    && let Some(pid) = child_pid
                    && let Some(cpu_ms) = crate::utils::process_cpu::process_tree_cpu_ms(pid)
                {
                    // Only a *rise* counts. A tree pinned on a lock or a denied write
                    // holds its CPU total flat, and must still be reaped.
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

            // Read stderr line
            result = stderr_reader.next_line() => {
                last_output_at = tokio::time::Instant::now();
                handle_stream_line(result, true, &mut collected_stderr, &mut log_monitor, op_id, op_monitor, output_optimizer, &mut spill).await;
            }

            // Read stdout line
            result = stdout_reader.next_line() => {
                last_output_at = tokio::time::Instant::now();
                handle_stream_line(result, false, &mut collected_stdout, &mut log_monitor, op_id, op_monitor, output_optimizer, &mut spill).await;
            }
        }

        // Check if the child process has exited
        // We use try_wait() to avoid blocking — if streams are closed the process may
        // have already exited.
        match child.try_wait() {
            Ok(Some(_status)) => {
                drain_remaining_stream_lines(
                    &mut stderr_reader,
                    &mut stdout_reader,
                    &mut collected_stdout,
                    &mut collected_stderr,
                    &mut log_monitor,
                    op_id,
                    op_monitor,
                    output_optimizer,
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
    )
    .await;
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
    output_optimizer: &Arc<tokio::sync::Mutex<crate::output_optimizer::OutputOptimizer>>,
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
            output_optimizer,
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
            output_optimizer,
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
) {
    let exit_status = child.wait().await;
    let duration_ms = start_time.elapsed().as_millis() as u64;
    tracing::debug!("Operation {} process exited after {}ms", op_id, duration_ms);
    let exit_code = exit_status
        .as_ref()
        .ok()
        .and_then(|s| s.code())
        .unwrap_or(-1);
    let success = exit_status.as_ref().is_ok_and(|s| s.success());

    let stdout_str = collected_stdout.rendered_output();
    let stderr_str = collected_stderr.rendered_output();

    // A failed async command may have hit a runtime sandbox denial the kernel did
    // not name; scan stderr and (best-effort) offer to grant an out-of-scope path.
    // This is the async-path twin of the scan in `execute_sync_in_dir`.
    if !success {
        sandbox::grant_channel::notify_stderr_denial(
            sandbox,
            scope_grant_notifier,
            &stderr_str,
            tool,
        )
        .await;

        // An out-of-scope runtime denial cannot be returned as a typed McpError on
        // the async path (the result is delivered later as text), so attach the
        // grant -> restart -> retry remediation as an operation alert. Mirrors the
        // typed `RuntimeDenial` the sync path returns.
        if let Some(hit) = sandbox::scan_denial(&stderr_str)
            && !sandbox.is_path_in_scope(&hit.path)
        {
            let remediation =
                sandbox::grant_channel::runtime_denial_remediation(&hit.path, hit.access);
            tracing::warn!(
                "Operation {} hit an out-of-scope sandbox denial on {}: {}",
                op_id,
                hit.path.display(),
                remediation
            );
            op_monitor.append_alert(op_id, remediation).await;
        }

        // The grant flow above only fires for *out-of-scope* denials. The
        // sibling failure — an in-scope EPERM from macOS provenance/sccache
        // contamination — produces no kernel event and would otherwise surface
        // as a bare `os error 1`. Diagnose it and attach the remediation as an
        // alert so the user gets an actionable line, not an errno.
        if let Some(hint) = sandbox::build_diagnostics::diagnose(&stderr_str) {
            tracing::warn!(
                "Operation {} failed with sandbox build contamination ({:?}): {}",
                op_id,
                hint.kind,
                hint.remediation
            );
            op_monitor.append_alert(op_id, hint.remediation).await;
        }

        // A non-path capability denial (the sandbox refused the OS credential
        // store, not a filesystem path) has no path to grant, so the scans above
        // cannot help. Turn the opaque failure into the two-door disclosure.
        if let Some(cap) = sandbox::scan_capability_denial(&stderr_str) {
            let disclosure =
                sandbox::capability_denial_disclosure(cap, capability_enforcing_layer());
            tracing::warn!(
                "Operation {} hit a capability denial ({:?}); surfacing disclosure",
                op_id,
                cap
            );
            op_monitor.append_alert(op_id, disclosure).await;
        }
    }

    let final_output = json!({
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

    let status = if success {
        OperationStatus::Completed
    } else {
        OperationStatus::Failed
    };
    // The terminal event emitted by this transition carries the result to all
    // subscribers (MCP progress push, daemon hub, TUI).
    op_monitor
        .update_status(op_id, status, Some(final_output))
        .await;
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
#[allow(clippy::too_many_arguments)]
async fn handle_stream_line(
    result: Result<Option<String>, std::io::Error>,
    is_stderr: bool,
    collector: &mut BoundedLineCollector,
    log_monitor: &mut Option<crate::log_monitor::LogMonitor>,
    op_id: &str,
    op_monitor: &Arc<OperationMonitor>,
    output_optimizer: &Arc<tokio::sync::Mutex<crate::output_optimizer::OutputOptimizer>>,
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
                output_optimizer,
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
#[allow(clippy::too_many_arguments)]
async fn process_streaming_line(
    line: &str,
    is_stderr: bool,
    collector: &mut BoundedLineCollector,
    log_monitor: &mut Option<crate::log_monitor::LogMonitor>,
    op_id: &str,
    op_monitor: &Arc<OperationMonitor>,
    output_optimizer: &Arc<tokio::sync::Mutex<crate::output_optimizer::OutputOptimizer>>,
    spill: &mut spill::SpillWriter,
) {
    let safe_line = crate::log_monitor::redact_sensitive_line(line);

    // The spill file records the redacted-but-unminimised line — the faithful
    // full record, independent of the bounded tail and token optimisation.
    spill.write_line(&safe_line, is_stderr).await;

    let opt_lines = if let Ok(mut opt) = output_optimizer.try_lock() {
        opt.process_streaming_line(&safe_line)
    } else {
        vec![safe_line.clone()]
    };

    for opt_line in opt_lines {
        collector.push(opt_line.clone());
        op_monitor
            .append_output_line(op_id, opt_line, is_stderr)
            .await;
    }

    if let Some(log_monitor) = log_monitor
        && let Some(snapshot) = log_monitor.process_line(line, is_stderr)
    {
        // The full notification snapshot (trigger + recent context) is stored
        // and emitted as a single `Alert` event so every subscriber — MCP
        // progress push, daemon hub, TUI — gets the same rich content.
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

    /// Regression: a timed-out/cancelled command must take down its whole process
    /// group, not just the direct child. Spawn `sh` (direct child) as a
    /// process-group leader; it backgrounds `sleep 60` (grandchild) and records
    /// the grandchild pid. `kill_process_tree` must kill the grandchild too —
    /// otherwise interrupted builds orphan `cargo`/`rustc` and leak processes.
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_process_tree_takes_down_grandchildren() {
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("sleep 60 & echo $! > {}; wait", pidfile.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            // Same flag base_command sets in production — makes the child a
            // process-group leader so the group kill reaches the grandchild.
            .process_group(0);
        let mut child = cmd.spawn().expect("spawn sh");

        // Wait for the grandchild pid to be recorded.
        let mut gpid = None;
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&pidfile)
                && let Ok(p) = s.trim().parse::<i32>()
            {
                gpid = Some(p);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let gpid = gpid.expect("grandchild pid file should be written");

        // Grandchild is alive (signal 0 only probes existence).
        assert_eq!(
            unsafe { libc::kill(gpid, 0) },
            0,
            "grandchild should be alive before kill"
        );

        let reaped = kill_process_tree(&mut child).await;
        assert!(
            reaped,
            "kill_process_tree should confirm the direct child was reaped"
        );

        // The grandchild should die (and be reaped) shortly after the group kill.
        let mut dead = false;
        for _ in 0..100 {
            if unsafe { libc::kill(gpid, 0) } != 0 {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            dead,
            "grandchild (pid {gpid}) must be killed via process-group kill, not orphaned"
        );
    }

    /// The verified-kill contract: `kill_process_tree` returns `true` once the
    /// direct child is confirmed reaped, and does so well within the grace window.
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_process_tree_confirms_reap_of_direct_child() {
        use std::time::Duration;

        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("300")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = cmd.spawn().expect("spawn sleep");

        let start = std::time::Instant::now();
        let reaped = kill_process_tree(&mut child).await;
        let elapsed = start.elapsed();

        assert!(reaped, "a plain killable child must be confirmed reaped");
        assert!(
            elapsed < Duration::from_secs(KILL_REAP_GRACE.as_secs()),
            "reap should be near-instant for a killable child, took {elapsed:?}"
        );
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
