//! Handler for `tool_type: livelog` tools.
//!
//! A livelog tool spawns a long-running source command (e.g. `adb logcat`),
//! pipes its output through an LLM for issue detection, and records `Alert`
//! events on the operation whenever the LLM finds problems matching the
//! `detection_prompt`.  The unified event stream forwards each alert to all
//! subscribers, including the MCP progress push.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use serde_json::{Map, Value};
use tracing::info;

use crate::{
    config::ToolConfig,
    livelog::run_livelog_pipeline,
    operation_monitor::{Operation, OperationMonitor, OperationStatus},
    sandbox::Sandbox,
};

/// Start a live-log monitoring session and return the operation ID immediately.
///
/// The source process is spawned inside a background `tokio` task.  Log chunks
/// are forwarded to the configured LLM; detected issues are recorded as
/// `Alert` events on the operation.
///
/// # Arguments
///
/// * `op_id`    — Pre-generated operation ID.
/// * `config`   — Tool configuration (must have `livelog` field populated).
/// * `params`   — MCP call params (used for optional `working_directory` override).
/// * `monitor`  — Operation monitor for lifecycle tracking.
/// * `sandbox`  — Sandbox used to spawn the source process.
pub async fn handle_livelog_start(
    op_id: String,
    config: &ToolConfig,
    params: &Map<String, Value>,
    monitor: Arc<OperationMonitor>,
    sandbox: Arc<Sandbox>,
    llm_service: Arc<dyn crate::llm_service::LlmCompletionService>,
) -> Result<String> {
    let livelog = config.livelog.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "tool '{}' has tool_type=livelog but no 'livelog' config block",
            config.name
        )
    })?;

    let working_dir = params
        .get("working_directory")
        .and_then(Value::as_str)
        .unwrap_or(".");

    // Validate the working directory against the sandbox scope up front so we
    // can return an error to the caller before spawning anything.
    let safe_wd = sandbox
        .validate_path(std::path::Path::new(working_dir))
        .map_err(|e| anyhow::anyhow!("Invalid working directory '{}': {}", working_dir, e))?;

    // Resolve caller-supplied runtime parameters (device serial, pid, clear, …)
    // into concrete source args + environment. A missing *required* parameter is
    // surfaced to the caller here, before any process is spawned.
    let runtime = livelog
        .resolve_runtime(params)
        .map_err(|e| anyhow::anyhow!("Invalid parameters for tool '{}': {}", config.name, e))?;

    let timeout = config.timeout_seconds.map(Duration::from_secs);

    let operation = Operation::new_with_timeout(
        op_id.clone(),
        config.name.clone(),
        format!(
            "Livelog: {} {:?} (detection: {})",
            livelog.source_command,
            livelog.source_args,
            &livelog
                .detection_prompt
                .chars()
                .take(60)
                .collect::<String>()
        ),
        None,
        timeout,
    );
    monitor.add_operation(operation).await;

    info!(
        "livelog_tool: registered operation '{}' for tool '{}'",
        op_id, config.name
    );

    // Clone everything that needs to move into the background task.
    let op_id_task = op_id.clone();
    let livelog_config = livelog.clone();
    let runtime_task = runtime;
    let monitor_task = monitor.clone();
    let sandbox_task = sandbox.clone();
    let llm_service_task = llm_service.clone();

    tokio::spawn(async move {
        // Retrieve the cancellation token from the monitor (set when the operation
        // was registered so callers can cancel via `cancel_tool`).
        let cancellation_token = match monitor_task.get_operation(&op_id_task).await {
            Some(op) => op.cancellation_token.clone(),
            None => {
                tracing::error!(
                    "livelog_tool: operation '{}' disappeared from monitor before task started",
                    op_id_task
                );
                return;
            }
        };

        monitor_task
            .update_status(&op_id_task, OperationStatus::InProgress, None)
            .await;

        run_livelog_pipeline(
            &op_id_task,
            &livelog_config,
            &runtime_task,
            &sandbox_task,
            &safe_wd,
            cancellation_token,
            monitor_task.clone(),
            llm_service_task,
        )
        .await;

        monitor_task
            .update_status(&op_id_task, OperationStatus::Completed, None)
            .await;

        info!("livelog_tool: operation '{}' completed", op_id_task);
    });

    Ok(op_id)
}
