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
mod preparer;
mod types;

pub use preparer::{
    TempFileManager, escape_shell_argument, format_option_flag, needs_file_handling,
    prepare_command_and_args,
};
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
    ///
    /// All operation lifecycle events are broadcast here.  Subscribers receive every event
    /// across all concurrent operations from this adapter.  Filter by `operation_id` if
    /// you care about a specific operation.
    ///
    /// In a future pass (P5) the legacy `callback` / direct-monitor-update paths will be
    /// removed and all notifications will flow exclusively through this dispatcher.
    pub event_dispatcher: EventDispatcher,
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
        Ok(Self {
            monitor,
            shell_pool,
            sandbox,
            task_handles: Arc::new(Mutex::new(HashMap::new())),
            temp_file_manager: preparer::TempFileManager::new(),
            retry_config: None,
            command_executor: Arc::new(executor::DefaultCommandExecutor),
            event_dispatcher: EventDispatcher::default(),
        })
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
        tracing::error!(
            "execute_sync_in_dir START: command='{}', working_dir='{}', args={:?}",
            command,
            working_dir,
            args
        );

        // Validate working directory against sandbox scope.
        let safe_wd = self
            .sandbox
            .validate_path(std::path::Path::new(working_dir))?;

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

        let output_res = tokio::time::timeout(timeout, cmd.output()).await;

        let output = match output_res {
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "Operation timed out (exceeded timeout limit): {} seconds",
                    timeout.as_secs()
                ));
            }
            Ok(Err(e)) => return Err(anyhow::anyhow!("Command execution failed: {}", e)),
            Ok(Ok(output)) => output,
        };

        interpret_sync_command_output(output)
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
        callback: Option<Box<dyn crate::callback_system::CallbackSender>>,
    ) -> Result<String> {
        self.execute_async_in_dir_with_options(
            tool_name,
            command,
            working_directory,
            AsyncExecOptions {
                id: None,
                args,
                timeout,
                callback,
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
            callback,
            subcommand_config,
            log_monitor_config,
        } = options;

        // Validate working directory against sandbox scope.
        let safe_wd = self
            .sandbox
            .validate_path(std::path::Path::new(working_dir))?;
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

        let operation = Operation::new_with_timeout(
            op_id.clone(),
            tool_name.to_string(),
            format!("{} {:?}", command, args),
            None,
            timeout_duration,
        );
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
            callback,
            log_monitor_config,
            monitor,
            shell_pool,
            sandbox,
            task_handles,
            command_executor: self.command_executor.clone(),
            event_dispatcher: self.event_dispatcher.clone(),
        }));

        // Store the handle for graceful shutdown
        self.task_handles
            .lock()
            .await
            .insert(op_id_clone.clone(), handle);

        Ok(op_id_clone)
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
        return Err(anyhow::anyhow!(
            "Command failed with exit code {}: stderr: {}, stdout: {}",
            output.status.code().unwrap_or(-1),
            stderr,
            stdout
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
    callback: Option<Box<dyn crate::callback_system::CallbackSender>>,
    log_monitor_config: Option<crate::log_monitor::LogMonitorConfig>,
    monitor: Arc<OperationMonitor>,
    shell_pool: Arc<ShellPoolManager>,
    sandbox: Arc<sandbox::Sandbox>,
    task_handles: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    command_executor: Arc<dyn executor::CommandExecutor>,
    /// Unified event dispatcher for P2 event stream.
    event_dispatcher: EventDispatcher,
}

async fn run_async_operation(ctx: AsyncOperationRun) {
    let AsyncOperationRun {
        op_id,
        command,
        program,
        args_vec,
        working_dir,
        timeout_secs,
        callback,
        log_monitor_config,
        monitor,
        shell_pool,
        sandbox,
        task_handles,
        command_executor,
        event_dispatcher,
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

    // Emit Started event to the unified dispatcher (P2).
    let tool_name = monitor
        .get_operation(&op_id)
        .await
        .map(|op| op.tool_name.clone())
        .unwrap_or_else(|| command.clone());
    event_dispatcher.emit(OperationEvent::Started {
        operation_id: op_id.clone(),
        tool_name,
        description: execution_description(&command, &working_dir),
    });

    if let Some(callback) = &callback {
        let _ = callback
            .send_progress(crate::callback_system::ProgressUpdate::Started {
                id: op_id.clone(),
                command: command.clone(),
                description: execution_description(&command, &working_dir),
            })
            .await;
    }

    if cancellation_token.is_cancelled() {
        tracing::info!("Operation {} was cancelled before execution started", op_id);
        handle_cancellation(&monitor, &callback, &op_id, 0).await;
        event_dispatcher.emit(OperationEvent::Cancelled {
            operation_id: op_id.clone(),
            reason: "cancelled before execution started".to_string(),
            duration_ms: 0,
        });
        return;
    }

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
            fail_operation_with_error(
                &monitor,
                &callback,
                &op_id,
                &program,
                &working_dir,
                0,
                err_msg.clone(),
            )
            .await;
            event_dispatcher.emit(OperationEvent::Failed {
                operation_id: op_id.clone(),
                error: err_msg,
                duration_ms: 0,
            });
            task_handles.lock().await.remove(&op_id);
            return;
        }
    };

    let timeout_ms = timeout_secs
        .map(|t| t * 1000)
        .unwrap_or_else(|| shell_pool.config().command_timeout.as_millis() as u64);

    if let Some(monitor_config) = log_monitor_config {
        execute_with_streaming(
            &mut proc_cmd,
            timeout_ms,
            monitor_config,
            &cancellation_token,
            &callback,
            &op_id,
            &program,
            &working_dir,
            start_time,
            &monitor,
            &event_dispatcher,
        )
        .await;
    } else {
        execute_batch(
            &mut proc_cmd,
            timeout_ms,
            &cancellation_token,
            &callback,
            &op_id,
            &program,
            &working_dir,
            start_time,
            &monitor,
        )
        .await;
    }

    // Emit terminal event to the unified dispatcher (P2).
    // Terminal states move the operation from active map to completion_history inside
    // update_status, so we must look in history — get_operation only checks active ops.
    let elapsed_ms = start_time.elapsed().as_millis() as u64;
    if let Some(op) = monitor.check_completion_history_pub(&op_id).await {
        let terminal_event = match op.state {
            OperationStatus::Completed => OperationEvent::Completed {
                operation_id: op_id.clone(),
                result: op.result.clone().unwrap_or(Value::Null),
                duration_ms: op
                    .end_time
                    .and_then(|e| e.duration_since(op.start_time).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(elapsed_ms),
            },
            OperationStatus::Failed => OperationEvent::Failed {
                operation_id: op_id.clone(),
                error: op
                    .result
                    .as_ref()
                    .and_then(|v| {
                        v.as_str()
                            .map(|s| s.to_string())
                            .or_else(|| serde_json::to_string(v).ok())
                    })
                    .unwrap_or_else(|| "unknown error".to_string()),
                duration_ms: elapsed_ms,
            },
            OperationStatus::Cancelled => OperationEvent::Cancelled {
                operation_id: op_id.clone(),
                reason: op
                    .result
                    .as_ref()
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| "cancelled".to_string()),
                duration_ms: elapsed_ms,
            },
            OperationStatus::TimedOut => OperationEvent::TimedOut {
                operation_id: op_id.clone(),
                duration_ms: elapsed_ms,
            },
            _ => OperationEvent::Failed {
                operation_id: op_id.clone(),
                error: "operation ended in unexpected state".to_string(),
                duration_ms: elapsed_ms,
            },
        };
        event_dispatcher.emit(terminal_event);
    } else {
        tracing::warn!(
            op_id = %op_id,
            "run_async_operation: operation not in completion history after execution; \
             terminal event will not be emitted",
        );
    }

    task_handles.lock().await.remove(&op_id);
}

async fn fail_operation_with_error(
    monitor: &Arc<OperationMonitor>,
    callback: &Option<Box<dyn crate::callback_system::CallbackSender>>,
    op_id: &str,
    program: &str,
    working_dir: &str,
    duration_ms: u64,
    error_message: String,
) {
    tracing::error!("{}", error_message);
    monitor
        .update_status(
            op_id,
            OperationStatus::Failed,
            Some(Value::String(error_message.clone())),
        )
        .await;
    send_progress_best_effort(
        callback,
        final_result_progress_update(
            op_id,
            program,
            working_dir,
            false,
            duration_ms,
            format!("Error: {}", error_message),
        ),
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

fn execution_description(program: &str, working_dir: &str) -> String {
    format!("Execute {} in {}", program, working_dir)
}

fn final_result_progress_update(
    op_id: &str,
    program: &str,
    working_dir: &str,
    success: bool,
    duration_ms: u64,
    full_output: String,
) -> crate::callback_system::ProgressUpdate {
    crate::callback_system::ProgressUpdate::FinalResult {
        id: op_id.to_string(),
        command: program.to_string(),
        description: execution_description(program, working_dir),
        working_directory: working_dir.to_string(),
        success,
        duration_ms,
        full_output,
    }
}

fn cancelled_progress_update(
    op_id: &str,
    message: String,
    duration_ms: u64,
) -> crate::callback_system::ProgressUpdate {
    crate::callback_system::ProgressUpdate::Cancelled {
        id: op_id.to_string(),
        message,
        duration_ms,
    }
}

async fn send_progress_best_effort(
    callback: &Option<Box<dyn crate::callback_system::CallbackSender>>,
    update: crate::callback_system::ProgressUpdate,
) {
    if let Some(callback) = callback.as_ref() {
        let _ = callback.send_progress(update).await;
    }
}

async fn send_final_result_progress(
    callback: &Option<Box<dyn crate::callback_system::CallbackSender>>,
    update: crate::callback_system::ProgressUpdate,
    op_id: &str,
) {
    let Some(callback) = callback.as_ref() else {
        return;
    };

    if let Err(e) = callback.send_progress(update).await {
        tracing::error!("Failed to send completion notification: {:?}", e);
    } else {
        tracing::info!("Sent completion notification for operation: {}", op_id);
    }
}

/// Identity fields for a running operation, passed as a bundle to avoid argument-count warnings.
struct StreamingOpContext<'a> {
    op_id: &'a str,
    program: &'a str,
    working_dir: &'a str,
}

/// Execute a command in batch mode (existing behavior): collect all output at once.
#[allow(clippy::too_many_arguments)]
async fn execute_batch(
    proc_cmd: &mut tokio::process::Command,
    timeout_ms: u64,
    cancellation_token: &tokio_util::sync::CancellationToken,
    callback: &Option<Box<dyn crate::callback_system::CallbackSender>>,
    op_id: &str,
    program: &str,
    working_dir: &str,
    start_time: Instant,
    monitor: &Arc<OperationMonitor>,
) {
    let proc_result =
        tokio::time::timeout(Duration::from_millis(timeout_ms), proc_cmd.output()).await;

    let duration_ms = start_time.elapsed().as_millis() as u64;

    // Check for cancellation after command execution
    if cancellation_token.is_cancelled() {
        tracing::info!("Operation {} was cancelled after shell execution", op_id);
        handle_cancellation(monitor, callback, op_id, duration_ms).await;
        return;
    }

    match proc_result {
        Ok(Ok(output)) => {
            complete_operation_with_output(
                monitor,
                callback,
                op_id,
                program,
                working_dir,
                duration_ms,
                output,
            )
            .await;
        }
        Ok(Err(e)) => {
            fail_operation_with_error(
                monitor,
                callback,
                op_id,
                program,
                working_dir,
                duration_ms,
                e.to_string(),
            )
            .await;
        }
        Err(_) => {
            cancel_operation_timed_out(monitor, callback, op_id, duration_ms).await;
        }
    }
}

async fn complete_operation_with_output(
    monitor: &Arc<OperationMonitor>,
    callback: &Option<Box<dyn crate::callback_system::CallbackSender>>,
    op_id: &str,
    program: &str,
    working_dir: &str,
    duration_ms: u64,
    output: std::process::Output,
) {
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);
    let success = output.status.success();

    let tail_source = if stdout.is_empty() { &stderr } else { &stdout };
    let tail_lines: Vec<String> = tail_source
        .lines()
        .rev()
        .take(100)
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    if let Some(mut op) = monitor.get_operation(op_id).await {
        op.stdout_tail = tail_lines;
        monitor.add_operation(op).await;
    }

    let final_output = json!({
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": exit_code,
    });
    let status = if success {
        OperationStatus::Completed
    } else {
        OperationStatus::Failed
    };
    monitor
        .update_status(op_id, status, Some(final_output))
        .await;

    send_final_result_progress(
        callback,
        final_result_progress_update(
            op_id,
            program,
            working_dir,
            success,
            duration_ms,
            format!("Exit code: {exit_code}\nStdout:\n{stdout}\nStderr:\n{stderr}"),
        ),
        op_id,
    )
    .await;
}

async fn cancel_operation_timed_out(
    monitor: &Arc<OperationMonitor>,
    callback: &Option<Box<dyn crate::callback_system::CallbackSender>>,
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
            Some(Value::String(timeout_reason.clone())),
        )
        .await;
    send_progress_best_effort(
        callback,
        cancelled_progress_update(op_id, timeout_reason, duration_ms),
    )
    .await;
}

/// Execute a command with line-by-line streaming and log monitoring.
///
/// Instead of buffering all output, this spawns the process and reads stdout/stderr
/// concurrently via `BufReader::lines()`. Each line is fed through a `LogMonitor`
/// which checks for error/warning patterns and fires alerts via the callback.
#[allow(clippy::too_many_arguments)]
async fn execute_with_streaming(
    proc_cmd: &mut tokio::process::Command,
    timeout_ms: u64,
    monitor_config: crate::log_monitor::LogMonitorConfig,
    cancellation_token: &tokio_util::sync::CancellationToken,
    callback: &Option<Box<dyn crate::callback_system::CallbackSender>>,
    op_id: &str,
    program: &str,
    working_dir: &str,
    start_time: Instant,
    op_monitor: &Arc<OperationMonitor>,
    event_dispatcher: &EventDispatcher,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};

    // Ensure stdout/stderr are piped (should already be set by sandbox)
    proc_cmd.stdout(std::process::Stdio::piped());
    proc_cmd.stderr(std::process::Stdio::piped());

    let mut child = match proc_cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            fail_operation_with_error(
                op_monitor,
                callback,
                op_id,
                program,
                working_dir,
                0,
                format!("Failed to spawn process: {}", e),
            )
            .await;
            return;
        }
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut log_monitor = crate::log_monitor::LogMonitor::new(monitor_config);

    // Collected output for the final result (bounded to prevent unbounded memory growth).
    let mut collected_stdout = BoundedLineCollector::default();
    let mut collected_stderr = BoundedLineCollector::default();

    let timeout_deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        tokio::select! {
            // Bias stderr to prioritize error-related output
            biased;

            // Check cancellation
            _ = cancellation_token.cancelled() => {
                tracing::info!("Operation {} cancelled during streaming", op_id);
                let _ = child.kill().await;
                let duration_ms = start_time.elapsed().as_millis() as u64;
                handle_cancellation(op_monitor, callback, op_id, duration_ms).await;
                return;
            }

            // Timeout
            _ = tokio::time::sleep_until(timeout_deadline) => {
                tracing::warn!("Operation {} timed out during streaming", op_id);
                let _ = child.kill().await;
                let duration_ms = start_time.elapsed().as_millis() as u64;
                cancel_operation_timed_out(op_monitor, callback, op_id, duration_ms).await;
                return;
            }

            // Read stderr line
            result = stderr_reader.next_line() => {
                handle_stream_line(result, true, &mut collected_stderr, &mut log_monitor, callback, op_id, op_monitor, event_dispatcher).await;
            }

            // Read stdout line
            result = stdout_reader.next_line() => {
                handle_stream_line(result, false, &mut collected_stdout, &mut log_monitor, callback, op_id, op_monitor, event_dispatcher).await;
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
                    callback,
                    op_id,
                    op_monitor,
                    event_dispatcher,
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

    finalize_streaming_operation(
        &mut child,
        start_time,
        &collected_stdout,
        &collected_stderr,
        op_monitor,
        callback,
        &StreamingOpContext {
            op_id,
            program,
            working_dir,
        },
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn drain_remaining_stream_lines(
    stderr_reader: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStderr>>,
    stdout_reader: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    collected_stdout: &mut BoundedLineCollector,
    collected_stderr: &mut BoundedLineCollector,
    log_monitor: &mut crate::log_monitor::LogMonitor,
    callback: &Option<Box<dyn crate::callback_system::CallbackSender>>,
    op_id: &str,
    op_monitor: &Arc<OperationMonitor>,
    event_dispatcher: &EventDispatcher,
) {
    while let Ok(Some(line)) = stderr_reader.next_line().await {
        process_streaming_line(
            &line,
            true,
            collected_stderr,
            log_monitor,
            callback,
            op_id,
            op_monitor,
            event_dispatcher,
        )
        .await;
    }
    while let Ok(Some(line)) = stdout_reader.next_line().await {
        process_streaming_line(
            &line,
            false,
            collected_stdout,
            log_monitor,
            callback,
            op_id,
            op_monitor,
            event_dispatcher,
        )
        .await;
    }
}

async fn finalize_streaming_operation(
    child: &mut tokio::process::Child,
    start_time: Instant,
    collected_stdout: &BoundedLineCollector,
    collected_stderr: &BoundedLineCollector,
    op_monitor: &Arc<OperationMonitor>,
    callback: &Option<Box<dyn crate::callback_system::CallbackSender>>,
    ctx: &StreamingOpContext<'_>,
) {
    let op_id = ctx.op_id;
    let program = ctx.program;
    let working_dir = ctx.working_dir;
    let exit_status = child.wait().await;
    let duration_ms = start_time.elapsed().as_millis() as u64;
    let exit_code = exit_status
        .as_ref()
        .ok()
        .and_then(|s| s.code())
        .unwrap_or(-1);
    let success = exit_status.as_ref().is_ok_and(|s| s.success());

    let stdout_str = collected_stdout.rendered_output();
    let stderr_str = collected_stderr.rendered_output();

    let final_output = json!({
        "stdout": stdout_str,
        "stderr": stderr_str,
        "exit_code": exit_code,
        "stdout_truncated_lines": collected_stdout.dropped_lines(),
        "stderr_truncated_lines": collected_stderr.dropped_lines(),
        "stdout_truncated_bytes": collected_stdout.dropped_bytes(),
        "stderr_truncated_bytes": collected_stderr.dropped_bytes(),
    });

    let status = if success {
        OperationStatus::Completed
    } else {
        OperationStatus::Failed
    };
    op_monitor
        .update_status(op_id, status, Some(final_output))
        .await;

    send_final_result_progress(
        callback,
        final_result_progress_update(
            op_id,
            program,
            working_dir,
            success,
            duration_ms,
            format!("Exit code: {exit_code}\nStdout:\n{stdout_str}\nStderr:\n{stderr_str}"),
        ),
        op_id,
    )
    .await;
}

/// Handle cancellation of an operation — shared logic for both execution paths.
async fn handle_cancellation(
    monitor: &Arc<OperationMonitor>,
    callback: &Option<Box<dyn crate::callback_system::CallbackSender>>,
    op_id: &str,
    duration_ms: u64,
) {
    monitor
        .update_status(
            op_id,
            OperationStatus::Cancelled,
            Some(Value::String("Operation was cancelled".to_string())),
        )
        .await;
    if callback.is_some() {
        let reason_owned = match monitor.get_operation(op_id).await {
            Some(op) => {
                let val = op.result.clone();
                val.and_then(|v| v.get("reason").cloned())
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| "Operation was cancelled".to_string())
            }
            None => "Operation was cancelled".to_string(),
        };
        send_progress_best_effort(
            callback,
            cancelled_progress_update(op_id, reason_owned, duration_ms),
        )
        .await;
    }
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
    log_monitor: &mut crate::log_monitor::LogMonitor,
    callback: &Option<Box<dyn crate::callback_system::CallbackSender>>,
    op_id: &str,
    op_monitor: &Arc<OperationMonitor>,
    event_dispatcher: &EventDispatcher,
) {
    match result {
        Ok(Some(line)) => {
            process_streaming_line(
                &line,
                is_stderr,
                collector,
                log_monitor,
                callback,
                op_id,
                op_monitor,
                event_dispatcher,
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

/// Redact, collect, and optionally send a log-monitor alert for a single streamed line.
#[allow(clippy::too_many_arguments)]
async fn process_streaming_line(
    line: &str,
    is_stderr: bool,
    collector: &mut BoundedLineCollector,
    log_monitor: &mut crate::log_monitor::LogMonitor,
    callback: &Option<Box<dyn crate::callback_system::CallbackSender>>,
    op_id: &str,
    op_monitor: &Arc<OperationMonitor>,
    event_dispatcher: &EventDispatcher,
) {
    let safe_line = crate::log_monitor::redact_sensitive_line(line);
    collector.push(safe_line.clone());
    op_monitor
        .append_stdout_line(op_id, safe_line.clone())
        .await;

    // Emit OutputLine to the unified dispatcher (P2).
    event_dispatcher.emit(OperationEvent::OutputLine {
        operation_id: op_id.to_string(),
        line: safe_line,
        is_stderr,
    });

    if let Some(snapshot) = log_monitor.process_line(line, is_stderr) {
        let alert_summary = format!("[{}] {}", snapshot.trigger_level, snapshot.trigger_line);
        op_monitor.append_alert(op_id, alert_summary.clone()).await;

        // Emit Alert to the unified dispatcher (P2).
        event_dispatcher.emit(OperationEvent::Alert {
            operation_id: op_id.to_string(),
            message: alert_summary,
        });

        if let Some(callback) = callback {
            let alert = crate::callback_system::ProgressUpdate::LogAlert {
                id: op_id.to_string(),
                trigger_level: snapshot.trigger_level.to_string(),
                context_snapshot: snapshot.format_for_notification(),
                llm_summary: None,
                trigger_lines: None,
            };
            let _ = callback.send_progress(alert).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
