//! Live log monitoring pipeline.
//!
//! The livelog pipeline spawns an external command (e.g. `adb logcat`, `ssh … tail -f …`),
//! reads its output line-by-line, accumulates lines into time/size-bounded chunks, and
//! periodically sends each chunk to an OpenAI-compatible LLM for issue detection.
//!
//! TODO: Deprecate direct LLM orchestration in the runner/adapter and extract this
//! logic to a standalone worker process or companion MCP server to achieve a cleaner
//! segregation of duties and allow better sandboxing boundaries.
//!
//! When the LLM reports an issue, an `Alert` event is recorded on the
//! operation (via `OperationMonitor::append_alert`), which the unified event
//! stream forwards to all subscribers — including the MCP progress push.  A
//! cooldown window prevents alert storms when many problematic lines arrive
//! in rapid succession.
//!
//! The pipeline runs inside a `tokio::spawn` task and can be stopped at any time by calling
//! [`tokio_util::sync::CancellationToken::cancel`] on the token that was obtained from the
//! [`crate::operation_monitor::OperationMonitor`] for the corresponding operation.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    config::{LivelogConfig, LivelogRuntime, LlmProviderConfig},
    sandbox::Sandbox,
};

use crate::operation_monitor::OperationMonitor;

/// Apply caller-resolved environment variables to a sandboxed command.
fn apply_env(cmd: &mut tokio::process::Command, env: &[(String, String)]) {
    for (key, value) in env {
        cmd.env(key, value);
    }
}

/// Push `line` into the LLM analysis `chunk` only when it passes the optional
/// pre-filter. The full output is recorded on the operation separately; this
/// gates only what is forwarded to the (comparatively expensive) LLM.
fn push_if_match(chunk: &mut Vec<String>, line: String, prefilter: &Option<regex::Regex>) {
    if prefilter.as_ref().is_none_or(|re| re.is_match(&line)) {
        chunk.push(line);
    }
}

/// Run the live-log pipeline until cancelled or the source process exits.
///
/// # Arguments
///
/// * `op_id`              — MCP operation ID used in progress notifications.
/// * `config`             — Livelog tool configuration (source command, LLM settings, etc.)
/// * `sandbox`            — Validated sandbox used to spawn the source process.
/// * `working_dir`        — Working directory for the source process.
/// * `cancellation_token` — Token to stop the pipeline on demand.
/// * `monitor`            — Operation monitor to update logs and alerts.
#[allow(clippy::too_many_arguments)]
pub async fn run_livelog_pipeline(
    op_id: &str,
    config: &LivelogConfig,
    runtime: &LivelogRuntime,
    sandbox: &Arc<Sandbox>,
    working_dir: &std::path::Path,
    cancellation_token: CancellationToken,
    monitor: Arc<OperationMonitor>,
    llm_service: Arc<dyn crate::llm_service::LlmCompletionService>,
) {
    let provider = match config.llm_provider.resolve() {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "livelog[{}]: failed to resolve LLM provider config: {}",
                op_id, e
            );
            return;
        }
    };

    // Compile the optional pre-filter once. An invalid pattern is non-fatal:
    // log it and let every line through rather than aborting monitoring.
    let prefilter = config.prefilter_regex.as_deref().and_then(|pat| {
        match regex::Regex::new(pat) {
            Ok(re) => Some(re),
            Err(e) => {
                warn!(
                    "livelog[{}]: invalid prefilter_regex {:?}: {} — sending all lines to the LLM",
                    op_id, pat, e
                );
                None
            }
        }
    });

    // Best-effort buffer clear (e.g. `adb logcat -c`) so stale crashes from a
    // previous run are not replayed as fresh alerts. Skipped when the caller
    // passed `clear: false` or the tool defines no `clear_command`.
    if runtime.clear
        && let Some(clear_args) = config.clear_command.as_deref()
    {
        match sandbox.create_command(&config.source_command, clear_args, working_dir) {
            Ok(mut cmd) => {
                apply_env(&mut cmd, &runtime.env);
                cmd.stdout(std::process::Stdio::null());
                cmd.stderr(std::process::Stdio::null());
                match cmd.status().await {
                    Ok(status) => {
                        debug!("livelog[{}]: clear command exited with {}", op_id, status)
                    }
                    Err(e) => warn!("livelog[{}]: clear command failed: {}", op_id, e),
                }
            }
            Err(e) => warn!("livelog[{}]: failed to create clear command: {}", op_id, e),
        }
    }

    let cmd_result =
        sandbox.create_command(&config.source_command, &runtime.source_args, working_dir);

    let mut child = match cmd_result {
        Ok(mut cmd) => {
            apply_env(&mut cmd, &runtime.env);
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            match cmd.spawn() {
                Ok(child) => child,
                Err(e) => {
                    warn!("livelog[{}]: failed to spawn source process: {}", op_id, e);
                    return;
                }
            }
        }
        Err(e) => {
            warn!("livelog[{}]: failed to create command: {}", op_id, e);
            return;
        }
    };

    info!(
        "livelog[{}]: source process started ({} {:?})",
        op_id, config.source_command, runtime.source_args
    );

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();

    let chunk_max_lines = config.chunk_max_lines;
    let chunk_max_duration = Duration::from_secs(config.chunk_max_seconds);
    let ctx = AnalysisCtx {
        llm_service: llm_service.as_ref(),
        base_url: provider.base_url,
        model: provider.model,
        api_key: provider.api_key,
        detection_prompt: &config.detection_prompt,
        llm_timeout: Duration::from_secs(config.llm_timeout_seconds),
        cooldown: Duration::from_secs(config.cooldown_seconds),
        monitor: &monitor,
    };

    let mut chunk: Vec<String> = Vec::new();
    let mut chunk_start = Instant::now();
    let mut last_alert: Option<Instant> = None;
    let mut stdout_closed = false;
    let mut stderr_closed = false;

    loop {
        if stdout_closed && stderr_closed {
            info!(
                "livelog[{}]: both streams closed, draining final chunk",
                op_id
            );
            if !chunk.is_empty() {
                maybe_analyze(op_id, &ctx, &mut chunk, &mut last_alert).await;
            }
            break;
        }

        let time_remaining = chunk_max_duration.saturating_sub(chunk_start.elapsed());

        tokio::select! {
            biased;

            // Cancellation has priority over everything else.
            _ = cancellation_token.cancelled() => {
                info!("livelog[{}]: cancelled, killing source process", op_id);
                let _ = child.kill().await;
                break;
            }

            // Read a stderr line (biased first so error output is processed promptly).
            result = stderr_lines.next_line(), if !stderr_closed => {
                match result {
                    Ok(Some(line)) => {
                        debug!("livelog[{}] stderr: {}", op_id, line);
                        monitor.append_stdout_line(op_id, line.clone()).await;
                        push_if_match(&mut chunk, line, &prefilter);
                    }
                    Ok(None) => {
                        debug!("livelog[{}]: stderr closed", op_id);
                        stderr_closed = true;
                    }
                    Err(e) => {
                        warn!("livelog[{}]: stderr read error: {}", op_id, e);
                        stderr_closed = true;
                    }
                }
            }

            // Read a stdout line.
            result = stdout_lines.next_line(), if !stdout_closed => {
                match result {
                    Ok(Some(line)) => {
                        debug!("livelog[{}] stdout: {}", op_id, line);
                        monitor.append_stdout_line(op_id, line.clone()).await;
                        push_if_match(&mut chunk, line, &prefilter);
                    }
                    Ok(None) => {
                        debug!("livelog[{}]: stdout closed", op_id);
                        stdout_closed = true;
                    }
                    Err(e) => {
                        warn!("livelog[{}]: stdout read error: {}", op_id, e);
                        stdout_closed = true;
                    }
                }
            }

            // Time-window expiry — flush whatever we have even if the chunk is not full yet.
            _ = tokio::time::sleep(time_remaining) => {
                debug!("livelog[{}]: chunk time window expired ({} lines)", op_id, chunk.len());
            }
        }

        // Flush the chunk when it hits the size limit or the time window.
        let chunk_full = chunk.len() >= chunk_max_lines;
        let chunk_timed_out = chunk_start.elapsed() >= chunk_max_duration;

        if (chunk_full || chunk_timed_out) && !chunk.is_empty() {
            maybe_analyze(op_id, &ctx, &mut chunk, &mut last_alert).await;
            chunk_start = Instant::now();
        }
    }

    let _ = child.wait().await;
    info!("livelog[{}]: pipeline finished", op_id);
}

/// Immutable per-pipeline configuration threaded into [`maybe_analyze`].
struct AnalysisCtx<'a> {
    llm_service: &'a dyn crate::llm_service::LlmCompletionService,
    base_url: String,
    model: String,
    api_key: Option<String>,
    detection_prompt: &'a str,
    llm_timeout: Duration,
    cooldown: Duration,
    monitor: &'a Arc<OperationMonitor>,
}

/// Send `chunk` to the LLM for analysis; record an `Alert` if issues are found.
///
/// Respects the cooldown window — if an alert was sent recently the chunk is
/// discarded without calling the LLM.
async fn maybe_analyze(
    op_id: &str,
    ctx: &AnalysisCtx<'_>,
    chunk: &mut Vec<String>,
    last_alert: &mut Option<Instant>,
) {
    let (detection_prompt, llm_timeout, cooldown) =
        (ctx.detection_prompt, ctx.llm_timeout, ctx.cooldown);
    // Enforce cooldown before hitting the LLM.
    if let Some(last) = last_alert
        && last.elapsed() < cooldown
    {
        debug!(
            "livelog[{}]: cooldown active ({:.1}s remaining), skipping LLM check",
            op_id,
            (cooldown - last.elapsed()).as_secs_f32()
        );
        chunk.clear();
        return;
    }

    let chunk_text = chunk.join("\n");
    chunk.clear();

    match ctx
        .llm_service
        .detect_issues(
            &ctx.base_url,
            &ctx.model,
            ctx.api_key.clone(),
            detection_prompt,
            &chunk_text,
            llm_timeout,
        )
        .await
    {
        Ok(Some(summary)) => {
            info!("livelog[{}]: LLM detected issue: {}", op_id, summary);
            *last_alert = Some(Instant::now());

            // One rich alert: human-readable diagnosis first, raw context
            // after.  Recorded on the operation and emitted as an `Alert`
            // event, which the progress push forwards to the MCP client.
            ctx.monitor
                .append_alert(
                    op_id,
                    format!("**Issue detected**: {summary}\n\n---\n\n{chunk_text}"),
                )
                .await;
        }
        Ok(None) => {
            debug!("livelog[{}]: LLM response: clean", op_id);
        }
        Err(e) => {
            warn!("livelog[{}]: LLM error: {}", op_id, e);
        }
    }
}

async fn process_new_bytes(
    buffer: &[u8],
    n: usize,
    remainder: &mut String,
    op_id: &str,
    monitor: &OperationMonitor,
) -> Vec<String> {
    let text = format!("{}{}", remainder, String::from_utf8_lossy(&buffer[..n]));
    let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    if let Some(last) = lines.pop() {
        *remainder = last;
    } else {
        remainder.clear();
    }

    let mut cleaned = Vec::new();
    for line in lines {
        let line_clean = line.trim_end_matches('\r').to_string();
        monitor.append_stdout_line(op_id, line_clean.clone()).await;
        cleaned.push(line_clean);
    }
    cleaned
}

/// Run the file-log tailing pipeline, reading from the file as it grows.
#[allow(clippy::too_many_arguments)]
pub async fn run_file_monitor_pipeline(
    op_id: &str,
    file_path: std::path::PathBuf,
    detection_prompt: String,
    llm_provider: LlmProviderConfig,
    cancellation_token: CancellationToken,
    monitor: Arc<OperationMonitor>,
    llm_service: Arc<dyn crate::llm_service::LlmCompletionService>,
) {
    let provider = match llm_provider.resolve() {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "file_monitor[{}]: failed to resolve LLM provider config: {}",
                op_id, e
            );
            return;
        }
    };

    info!(
        "file_monitor[{}]: starting file monitor on {:?}",
        op_id, file_path
    );

    // Open the file and seek to the end
    let mut file = match tokio::fs::File::open(&file_path).await {
        Ok(f) => f,
        Err(e) => {
            warn!(
                "file_monitor[{}]: failed to open file {:?}: {}",
                op_id, file_path, e
            );
            return;
        }
    };

    let mut pos = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            warn!(
                "file_monitor[{}]: failed to read metadata for {:?}: {}",
                op_id, file_path, e
            );
            return;
        }
    };

    if let Err(e) = file.seek(std::io::SeekFrom::Start(pos)).await {
        warn!(
            "file_monitor[{}]: failed to seek file {:?}: {}",
            op_id, file_path, e
        );
        return;
    }

    let chunk_max_lines = 50;
    let chunk_max_duration = Duration::from_secs(30);
    let ctx = AnalysisCtx {
        llm_service: llm_service.as_ref(),
        base_url: provider.base_url,
        model: provider.model,
        api_key: provider.api_key,
        detection_prompt: &detection_prompt,
        llm_timeout: Duration::from_secs(30),
        cooldown: Duration::from_secs(60),
        monitor: &monitor,
    };

    let mut chunk: Vec<String> = Vec::new();
    let mut chunk_start = Instant::now();
    let mut last_alert: Option<Instant> = None;
    let mut remainder = String::new();
    let mut buffer = vec![0u8; 8192];

    loop {
        if cancellation_token.is_cancelled() {
            info!("file_monitor[{}]: cancelled, exiting", op_id);
            break;
        }

        // Flush chunk if time window expires
        let chunk_timed_out = chunk_start.elapsed() >= chunk_max_duration;
        if chunk_timed_out && !chunk.is_empty() {
            maybe_analyze(op_id, &ctx, &mut chunk, &mut last_alert).await;
            chunk_start = Instant::now();
        }

        // Check file metadata / size
        let metadata = match file.metadata().await {
            Ok(m) => m,
            Err(e) => {
                warn!("file_monitor[{}]: failed to read metadata: {}", op_id, e);
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };

        let new_len = metadata.len();
        if new_len < pos {
            // File truncated / rotated
            info!("file_monitor[{}]: file truncated/rotated", op_id);
            pos = 0;
            if let Err(e) = file.seek(std::io::SeekFrom::Start(0)).await {
                warn!("file_monitor[{}]: seek failed: {}", op_id, e);
            }
            remainder.clear();
        } else if new_len > pos {
            // Read new bytes
            let to_read = (new_len - pos).min(buffer.len() as u64) as usize;
            match file.read(&mut buffer[..to_read]).await {
                Ok(0) => {}
                Ok(n) => {
                    pos += n as u64;
                    let new_lines =
                        process_new_bytes(&buffer, n, &mut remainder, op_id, monitor.as_ref())
                            .await;
                    for line_clean in new_lines {
                        chunk.push(line_clean);

                        if chunk.len() >= chunk_max_lines {
                            maybe_analyze(op_id, &ctx, &mut chunk, &mut last_alert).await;
                            chunk_start = Instant::now();
                        }
                    }
                }
                Err(e) => {
                    warn!("file_monitor[{}]: read error: {}", op_id, e);
                }
            }
        } else {
            // Sleep briefly before polling again
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    info!("file_monitor[{}]: pipeline finished", op_id);
}
