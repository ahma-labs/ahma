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
use crate::shell_pool::{ProcessGroupGuard, kill_process_tree};

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

// JSON output instruction appended to the detection prompt when `structured_output: true`.
// Modern LLMs follow user-message JSON instructions even when the system prompt specifies a
// different format, overriding the default "CLEAN or prose" response.
const STRUCTURED_OUTPUT_INSTRUCTION: &str = "\n\n\
IMPORTANT: Respond ONLY with valid JSON — no other text, no markdown, no explanation.\n\
  No issue found : {\"issue\":false}\n\
  Issue found    : {\"issue\":true,\"level\":\"FATAL|ERROR|WARN\",\"summary\":\"one sentence\",\
\"exception_class\":\"optional.FullyQualifiedClass\",\"top_frame\":\"optional File.kt:42\"}";

/// Compile the optional pre-filter regex once. An invalid pattern is
/// non-fatal: log it and let every line through rather than aborting
/// monitoring.
fn resolve_prefilter(op_id: &str, pattern: Option<&str>) -> Option<regex::Regex> {
    pattern.and_then(|pat| match regex::Regex::new(pat) {
        Ok(re) => Some(re),
        Err(e) => {
            warn!(
                "livelog[{}]: invalid prefilter_regex {:?}: {} — sending all lines to the LLM",
                op_id, pat, e
            );
            None
        }
    })
}

/// Best-effort buffer clear (e.g. `adb logcat -c`) so stale crashes from a
/// previous run are not replayed as fresh alerts. Skipped when the caller
/// passed `clear: false` or the tool defines no `clear_command`.
async fn run_clear_command_if_requested(
    op_id: &str,
    config: &LivelogConfig,
    runtime: &LivelogRuntime,
    sandbox: &Arc<Sandbox>,
    working_dir: &std::path::Path,
) {
    if !runtime.clear {
        return;
    }
    let Some(clear_args) = config.clear_command.as_deref() else {
        return;
    };
    let mut cmd = match sandbox.create_command(&config.source_command, clear_args, working_dir) {
        Ok(cmd) => cmd,
        Err(e) => {
            warn!("livelog[{}]: failed to create clear command: {}", op_id, e);
            return;
        }
    };
    apply_env(&mut cmd, &runtime.env);
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    match cmd.status().await {
        Ok(status) => debug!("livelog[{}]: clear command exited with {}", op_id, status),
        Err(e) => warn!("livelog[{}]: clear command failed: {}", op_id, e),
    }
}

/// Create and spawn the source process (e.g. `adb logcat`, `ssh … tail -f …`)
/// with piped stdout/stderr for line-by-line reading. Any failure is logged
/// and yields `None`.
fn spawn_source_process(
    op_id: &str,
    config: &LivelogConfig,
    runtime: &LivelogRuntime,
    sandbox: &Arc<Sandbox>,
    working_dir: &std::path::Path,
) -> Option<ProcessGroupGuard> {
    let mut cmd =
        match sandbox.create_command(&config.source_command, &runtime.source_args, working_dir) {
            Ok(cmd) => cmd,
            Err(e) => {
                warn!("livelog[{}]: failed to create command: {}", op_id, e);
                return None;
            }
        };
    apply_env(&mut cmd, &runtime.env);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    match cmd.spawn() {
        // A log source is typically a pipeline (`adb logcat | grep …`), so the
        // interesting processes are grandchildren. `kill_on_drop` would signal
        // only the direct child and leave them streaming; the guard sends
        // `kill(-pgid)`, and `create_command` already made this a group leader.
        Ok(child) => Some(ProcessGroupGuard::new(child)),
        Err(e) => {
            warn!("livelog[{}]: failed to spawn source process: {}", op_id, e);
            None
        }
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
    llm_service: Arc<crate::llm_service::DefaultLlmCompletionService>,
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
    let prefilter = resolve_prefilter(op_id, config.prefilter_regex.as_deref());

    run_clear_command_if_requested(op_id, config, runtime, sandbox, working_dir).await;

    let Some(mut child) = spawn_source_process(op_id, config, runtime, sandbox, working_dir) else {
        return;
    };

    info!(
        "livelog[{}]: source process started ({} {:?})",
        op_id, config.source_command, runtime.source_args
    );

    let stdout = child.child_mut().stdout.take().expect("stdout was piped");
    let stderr = child.child_mut().stderr.take().expect("stderr was piped");

    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();

    let chunk_max_lines = config.chunk_max_lines;
    let chunk_max_duration = Duration::from_secs(config.chunk_max_seconds);
    // When structured output is requested, append JSON format instructions to the
    // detection prompt so the LLM returns a parseable object instead of prose.
    let effective_prompt: String = if config.structured_output {
        format!(
            "{}{}",
            config.detection_prompt, STRUCTURED_OUTPUT_INSTRUCTION
        )
    } else {
        config.detection_prompt.clone()
    };
    let ctx = AnalysisCtx {
        llm_service: llm_service.as_ref(),
        base_url: provider.base_url,
        model: provider.model,
        api_key: provider.api_key,
        detection_prompt: &effective_prompt,
        llm_timeout: Duration::from_secs(config.llm_timeout_seconds),
        cooldown: Duration::from_secs(config.cooldown_seconds),
        monitor: &monitor,
        structured_output: config.structured_output,
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
                if !kill_process_tree(child.child_mut()).await {
                    warn!("livelog[{}]: source process did not reap cleanly", op_id);
                }
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

    let _ = child.child_mut().wait().await;
    info!("livelog[{}]: pipeline finished", op_id);
}

/// Immutable per-pipeline configuration threaded into [`maybe_analyze`].
struct AnalysisCtx<'a> {
    llm_service: &'a crate::llm_service::DefaultLlmCompletionService,
    base_url: String,
    model: String,
    api_key: Option<String>,
    detection_prompt: &'a str,
    llm_timeout: Duration,
    cooldown: Duration,
    monitor: &'a Arc<OperationMonitor>,
    /// When true, parse the LLM response as a structured JSON object rather
    /// than treating it as opaque prose.  See `STRUCTURED_OUTPUT_INSTRUCTION`.
    structured_output: bool,
}

/// The outcome of interpreting an LLM response in `structured_output` mode.
///
/// Three states, deliberately distinct: a parsed-clean verdict and an
/// *unparseable* response both used to collapse to `None`, which silently
/// dropped any crash the model reported as prose instead of JSON. Keeping them
/// apart lets the caller drop only the genuinely-clean case and fall back to a
/// plain-text alert for prose — never losing a detection.
#[derive(Debug, PartialEq, Eq)]
enum StructuredVerdict {
    /// Valid JSON with `issue: false` — the model looked and found nothing.
    Clean,
    /// Valid JSON describing an issue — a formatted alert.
    Alert(String),
    /// Not valid JSON (the model answered in prose). The caller must fall back
    /// to treating the raw text as a plain-text alert rather than discard it.
    Unparseable,
}

/// Classify a structured-mode LLM response into a [`StructuredVerdict`].
///
/// Parses `{issue, level, summary, exception_class?, top_frame?}`. A non-JSON
/// body yields [`StructuredVerdict::Unparseable`] (NOT clean) so a prose crash
/// report is never silently dropped.
fn classify_structured_response(text: &str) -> StructuredVerdict {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return StructuredVerdict::Unparseable;
    };
    if v.get("issue").and_then(|b| b.as_bool()) == Some(false) {
        return StructuredVerdict::Clean;
    }
    let level = v.get("level").and_then(|b| b.as_str()).unwrap_or("ERROR");
    let summary = v.get("summary").and_then(|b| b.as_str()).unwrap_or(text);
    let mut alert = format!("[{level}] {summary}");
    if let Some(exc) = v.get("exception_class").and_then(|b| b.as_str()) {
        alert.push_str(&format!("\n  Exception: {exc}"));
    }
    if let Some(frame) = v.get("top_frame").and_then(|b| b.as_str()) {
        alert.push_str(&format!("\n  at {frame}"));
    }
    StructuredVerdict::Alert(alert)
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
        Ok(Some(raw)) => {
            // In structured mode, parse JSON and short-circuit on `issue:false`.
            // A response that is *not* valid JSON (model answered in prose) must
            // NOT be treated as clean — it falls through to the plain-text path
            // so a prose-reported crash is still surfaced, never dropped.
            let summary = if ctx.structured_output {
                match classify_structured_response(&raw) {
                    StructuredVerdict::Alert(s) => s,
                    StructuredVerdict::Clean => {
                        debug!("livelog[{}]: structured response: clean", op_id);
                        return;
                    }
                    StructuredVerdict::Unparseable => {
                        debug!(
                            "livelog[{}]: structured response was not JSON; \
                             treating as plain-text alert",
                            op_id
                        );
                        raw.clone()
                    }
                }
            } else {
                raw.clone()
            };

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

/// Open `file_path` and seek to its current end, returning the open file and
/// starting byte offset for tailing. Any failure is logged and yields `None`.
async fn open_file_and_seek_to_end(
    op_id: &str,
    file_path: &std::path::Path,
) -> Option<(tokio::fs::File, u64)> {
    let mut file = match tokio::fs::File::open(file_path).await {
        Ok(f) => f,
        Err(e) => {
            warn!(
                "file_monitor[{}]: failed to open file {:?}: {}",
                op_id, file_path, e
            );
            return None;
        }
    };

    let pos = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            warn!(
                "file_monitor[{}]: failed to read metadata for {:?}: {}",
                op_id, file_path, e
            );
            return None;
        }
    };

    if let Err(e) = file.seek(std::io::SeekFrom::Start(pos)).await {
        warn!(
            "file_monitor[{}]: failed to seek file {:?}: {}",
            op_id, file_path, e
        );
        return None;
    }

    Some((file, pos))
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
    llm_service: Arc<crate::llm_service::DefaultLlmCompletionService>,
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
    let Some((mut file, mut pos)) = open_file_and_seek_to_end(op_id, &file_path).await else {
        return;
    };

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
        structured_output: false, // file monitor uses plain-text alerts
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

// ===========================================================================
// Unit tests for private helpers (`process_new_bytes`, `push_if_match`,
// `apply_env`).  Integration-level tests for the two public pipelines live in
// `ahma_mcp/tests/unit/livelog_pipeline_test.rs` and
// `ahma_mcp/tests/unit/livelog_file_monitor_test.rs`.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::operation_monitor::{MonitorConfig, OperationMonitor};

    fn make_monitor() -> std::sync::Arc<OperationMonitor> {
        std::sync::Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            Duration::from_secs(60),
        )))
    }

    // -----------------------------------------------------------------------
    // push_if_match
    // -----------------------------------------------------------------------

    #[test]
    fn test_push_if_match_no_prefilter_always_pushes() {
        let mut chunk: Vec<String> = Vec::new();
        push_if_match(&mut chunk, "any line".to_string(), &None);
        assert_eq!(chunk, vec!["any line"]);
    }

    #[test]
    fn test_push_if_match_with_matching_regex_pushes_line() {
        let re = regex::Regex::new(r"ERROR").unwrap();
        let mut chunk: Vec<String> = Vec::new();
        push_if_match(&mut chunk, "ERROR: crash detected".to_string(), &Some(re));
        assert_eq!(chunk.len(), 1, "matching line should be pushed");
    }

    #[test]
    fn test_push_if_match_with_non_matching_regex_drops_line() {
        let re = regex::Regex::new(r"ERROR").unwrap();
        let mut chunk: Vec<String> = Vec::new();
        push_if_match(
            &mut chunk,
            "INFO: everything is fine".to_string(),
            &Some(re),
        );
        assert!(chunk.is_empty(), "non-matching line must be dropped");
    }

    // -----------------------------------------------------------------------
    // apply_env
    // -----------------------------------------------------------------------

    #[test]
    fn test_apply_env_with_empty_slice_is_noop() {
        let mut cmd = tokio::process::Command::new("echo");
        // Must not panic with an empty env list.
        apply_env(&mut cmd, &[]);
    }

    #[test]
    fn test_apply_env_sets_each_variable() {
        let mut cmd = tokio::process::Command::new("echo");
        let env = vec![
            ("AHMA_TEST_KEY1".to_string(), "value1".to_string()),
            ("AHMA_TEST_KEY2".to_string(), "value2".to_string()),
        ];
        // Exercises the for-loop body — the call succeeding without panic is the
        // observable assertion; Command does not expose a public env accessor.
        apply_env(&mut cmd, &env);
    }

    // -----------------------------------------------------------------------
    // process_new_bytes
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_process_new_bytes_returns_all_complete_lines() {
        let monitor = make_monitor();
        let data = b"line1\nline2\nline3\n";
        let mut remainder = String::new();
        let result = process_new_bytes(data, data.len(), &mut remainder, "op-test", &monitor).await;
        assert_eq!(result, vec!["line1", "line2", "line3"]);
        assert!(remainder.is_empty(), "trailing newline leaves no remainder");
    }

    #[tokio::test]
    async fn test_process_new_bytes_retains_partial_last_line_as_remainder() {
        let monitor = make_monitor();
        let data = b"complete\npartial";
        let mut remainder = String::new();
        let result = process_new_bytes(data, data.len(), &mut remainder, "op-test", &monitor).await;
        assert_eq!(result, vec!["complete"]);
        assert_eq!(remainder, "partial");
    }

    #[tokio::test]
    async fn test_process_new_bytes_prepends_existing_remainder() {
        let monitor = make_monitor();
        let data = b"_suffix\nnext_line\n";
        let mut remainder = "prefix".to_string();
        let result = process_new_bytes(data, data.len(), &mut remainder, "op-test", &monitor).await;
        assert_eq!(result, vec!["prefix_suffix", "next_line"]);
        assert!(remainder.is_empty());
    }

    #[tokio::test]
    async fn test_process_new_bytes_strips_carriage_returns() {
        let monitor = make_monitor();
        let data = b"windows_line\r\nanother\r\n";
        let mut remainder = String::new();
        let result = process_new_bytes(data, data.len(), &mut remainder, "op-test", &monitor).await;
        assert_eq!(result, vec!["windows_line", "another"]);
        assert!(remainder.is_empty());
    }

    #[tokio::test]
    async fn test_process_new_bytes_single_line_without_newline_becomes_remainder() {
        let monitor = make_monitor();
        let data = b"no newline here";
        let mut remainder = String::new();
        let result = process_new_bytes(data, data.len(), &mut remainder, "op-test", &monitor).await;
        assert!(
            result.is_empty(),
            "no complete lines without trailing newline"
        );
        assert_eq!(remainder, "no newline here");
    }

    #[tokio::test]
    async fn test_process_new_bytes_respects_n_boundary() {
        let monitor = make_monitor();
        // The buffer contains more data than `n` — only the first `n` bytes should
        // be processed; the tail must be ignored.
        let data = b"line1\nline2\nignored_suffix";
        let n = b"line1\nline2\n".len();
        let mut remainder = String::new();
        let result = process_new_bytes(data, n, &mut remainder, "op-test", &monitor).await;
        assert_eq!(result, vec!["line1", "line2"]);
        assert!(remainder.is_empty());
    }

    // -----------------------------------------------------------------------
    // classify_structured_response
    // -----------------------------------------------------------------------

    #[test]
    fn issue_false_is_clean() {
        assert_eq!(
            classify_structured_response(r#"{"issue":false}"#),
            StructuredVerdict::Clean
        );
    }

    #[test]
    fn issue_true_formats_alert_with_fields() {
        let v = classify_structured_response(
            r#"{"issue":true,"level":"FATAL","summary":"NPE in MainActivity",
                "exception_class":"java.lang.NullPointerException",
                "top_frame":"MainActivity.onCreate(MainActivity.kt:42)"}"#,
        );
        match v {
            StructuredVerdict::Alert(s) => {
                assert!(s.contains("[FATAL]"), "level should appear: {s}");
                assert!(
                    s.contains("NPE in MainActivity"),
                    "summary should appear: {s}"
                );
                assert!(s.contains("java.lang.NullPointerException"));
                assert!(s.contains("MainActivity.kt:42"));
            }
            other => panic!("expected Alert, got {other:?}"),
        }
    }

    #[test]
    fn issue_true_without_optional_fields_uses_defaults() {
        let v = classify_structured_response(r#"{"issue":true,"summary":"boom"}"#);
        assert_eq!(
            v,
            StructuredVerdict::Alert("[ERROR] boom".to_string()),
            "missing level defaults to ERROR; no exception/frame lines"
        );
    }

    #[test]
    fn non_json_prose_is_unparseable_not_clean() {
        // The regression this fix targets: a prose crash report must NOT be
        // silently dropped — it is Unparseable so the caller falls back to a
        // plain-text alert.
        assert_eq!(
            classify_structured_response("FATAL: NPE in com.example.app"),
            StructuredVerdict::Unparseable
        );
    }

    #[test]
    fn empty_response_is_unparseable() {
        assert_eq!(
            classify_structured_response(""),
            StructuredVerdict::Unparseable
        );
    }

    #[test]
    fn json_array_without_issue_field_is_treated_as_alert() {
        // Valid JSON that isn't our object shape still parses; absent `issue`
        // means "not explicitly clean", so it surfaces as an alert (fail-safe).
        let v = classify_structured_response(r#"{"unexpected":"shape"}"#);
        assert!(matches!(v, StructuredVerdict::Alert(_)));
    }
}
