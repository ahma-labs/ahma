//! Integration tests for the livelog pipeline.
//!
//! Tests `run_livelog_pipeline()` end-to-end using:
//! - A real mock source command (`echo` / `printf`) that produces known output.
//! - A wiremock server standing in for the LLM endpoint.
//! - The `OperationMonitor` as the store of record: detected issues are
//!   appended to `Operation::alerts` (and emitted as `Alert` events).

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

use ahma_mcp::config::{LivelogConfig, LlmProviderConfig};
use ahma_mcp::livelog::run_livelog_pipeline;
use ahma_mcp::operation_monitor::{MonitorConfig, Operation, OperationMonitor};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_config(
    source_command: &str,
    source_args: Vec<String>,
    llm_base_url: &str,
    detection_prompt: &str,
) -> LivelogConfig {
    LivelogConfig {
        source_command: source_command.to_string(),
        source_args,
        detection_prompt: detection_prompt.to_string(),
        llm_provider: LlmProviderConfig {
            base_url: llm_base_url.to_string(),
            model: "test-model".to_string(),
            api_key: None,
        },
        parameters: Vec::new(),
        env: std::collections::BTreeMap::new(),
        clear_command: None,
        prefilter_regex: None,
        // Use a tiny chunk so the single echo output is flushed quickly.
        chunk_max_lines: 1,
        chunk_max_seconds: 5,
        cooldown_seconds: 0, // no cooldown between alerts in tests
        llm_timeout_seconds: 10,
    }
}

/// Build a default runtime (no caller parameters) for tests that exercise the
/// pipeline directly. Mirrors what `handle_livelog_start` does for a call with
/// no extra arguments.
fn default_runtime(config: &LivelogConfig) -> ahma_mcp::config::LivelogRuntime {
    config
        .resolve_runtime(&serde_json::Map::new())
        .expect("default runtime resolves")
}

fn make_llm_response(content: &str) -> serde_json::Value {
    json!({
        "choices": [{"message": {"content": content, "role": "assistant"}}]
    })
}

fn make_monitor() -> Arc<OperationMonitor> {
    Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        TestTimeouts::get(TimeoutCategory::ToolCall),
    )))
}

/// Register the livelog operation so `append_alert` has a target — mirrors
/// what `handle_livelog_start` does in production.
async fn register_op(monitor: &OperationMonitor, op_id: &str) {
    monitor
        .add_operation(Operation::new_with_timeout(
            op_id.to_string(),
            "livelog".to_string(),
            format!("livelog test op {op_id}"),
            None,
            None,
        ))
        .await;
}

async fn alerts_for(monitor: &OperationMonitor, op_id: &str) -> Vec<String> {
    monitor
        .get_operation(op_id)
        .await
        .map(|op| op.alerts)
        .unwrap_or_default()
}

fn make_sandbox(temp_dir: &std::path::Path) -> Arc<Sandbox> {
    Arc::new(
        Sandbox::new(
            vec![temp_dir.to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// When the LLM returns "CLEAN", no alert should be recorded.
#[tokio::test]
async fn test_livelog_pipeline_clean_response_no_alert() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_llm_response("CLEAN")))
        .mount(&server)
        .await;

    let temp_dir = tempdir().unwrap();
    let sandbox = make_sandbox(temp_dir.path());

    let config = make_config(
        "echo",
        vec!["INFO everything is fine".to_string()],
        &server.uri(),
        "look for crashes",
    );

    let token = CancellationToken::new();
    let monitor = make_monitor();
    register_op(&monitor, "test-op-clean").await;

    run_livelog_pipeline(
        "test-op-clean",
        &config,
        &default_runtime(&config),
        &sandbox,
        temp_dir.path(),
        token,
        monitor.clone(),
        Arc::new(ahma_mcp::llm_service::DefaultLlmCompletionService),
    )
    .await;

    let alerts = alerts_for(&monitor, "test-op-clean").await;
    assert!(
        alerts.is_empty(),
        "expected no alerts for CLEAN response, got: {:?}",
        alerts
    );
}

/// When the LLM returns an issue summary, an alert is recorded with both the
/// summary and the raw log context.
#[tokio::test]
async fn test_livelog_pipeline_issue_detected_sends_alert() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_llm_response(
            "NullPointerException at MainActivity line 42",
        )))
        .mount(&server)
        .await;

    let temp_dir = tempdir().unwrap();
    let sandbox = make_sandbox(temp_dir.path());

    let config = make_config(
        "echo",
        vec!["FATAL EXCEPTION: NullPointerException".to_string()],
        &server.uri(),
        "look for crashes",
    );

    let token = CancellationToken::new();
    let monitor = make_monitor();
    register_op(&monitor, "test-op-issue").await;

    run_livelog_pipeline(
        "test-op-issue",
        &config,
        &default_runtime(&config),
        &sandbox,
        temp_dir.path(),
        token,
        monitor.clone(),
        Arc::new(ahma_mcp::llm_service::DefaultLlmCompletionService),
    )
    .await;

    let alerts = alerts_for(&monitor, "test-op-issue").await;
    assert_eq!(alerts.len(), 1, "expected exactly one alert: {:?}", alerts);
    let alert = &alerts[0];
    assert!(
        alert.contains("NullPointerException at MainActivity line 42"),
        "alert should contain the LLM summary: {alert}"
    );
    assert!(
        alert.contains("FATAL EXCEPTION"),
        "alert should contain the raw log context: {alert}"
    );
}

/// The cooldown window suppresses a second alert fired within cooldown_seconds.
#[tokio::test]
async fn test_livelog_pipeline_cooldown_suppresses_second_alert() {
    let server = MockServer::start().await;
    // Always return an "issue detected" response.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(make_llm_response("Error: crash detected")),
        )
        .mount(&server)
        .await;

    let temp_dir = tempdir().unwrap();
    let sandbox = make_sandbox(temp_dir.path());

    // Two lines = two chunks (chunk_max_lines = 1), but cooldown = 300s.
    let mut config = make_config(
        "printf",
        vec!["line1\\nline2\\n".to_string()],
        &server.uri(),
        "look for errors",
    );
    config.cooldown_seconds = 300; // very long cooldown

    let token = CancellationToken::new();
    let monitor = make_monitor();
    register_op(&monitor, "test-op-cooldown").await;

    run_livelog_pipeline(
        "test-op-cooldown",
        &config,
        &default_runtime(&config),
        &sandbox,
        temp_dir.path(),
        token,
        monitor.clone(),
        Arc::new(ahma_mcp::llm_service::DefaultLlmCompletionService),
    )
    .await;

    let alerts = alerts_for(&monitor, "test-op-cooldown").await;
    // First chunk triggers alert; second chunk should be silenced by cooldown.
    assert_eq!(
        alerts.len(),
        1,
        "cooldown should suppress second alert, got {} alerts",
        alerts.len()
    );
}

/// When the pipeline is cancelled, it terminates promptly.
#[tokio::test]
async fn test_livelog_pipeline_cancellation_stops_pipeline() {
    // Use `sleep` as an "infinite" source that never exits on its own.
    // We cancel immediately after spawning.
    let server = MockServer::start().await;
    // LLM is never reached because we cancel before any chunk is produced.

    let temp_dir = tempdir().unwrap();
    let sandbox = make_sandbox(temp_dir.path());

    let config = make_config(
        "sleep",
        vec!["60".to_string()],
        &server.uri(),
        "look for crashes",
    );

    let token = CancellationToken::new();
    let monitor = make_monitor();
    register_op(&monitor, "test-op-cancel").await;

    // Cancel after a short delay so the pipeline can actually start.
    let token_clone = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(TestTimeouts::poll_interval()).await;
        token_clone.cancel();
    });

    let start = std::time::Instant::now();
    run_livelog_pipeline(
        "test-op-cancel",
        &config,
        &default_runtime(&config),
        &sandbox,
        temp_dir.path(),
        token,
        monitor.clone(),
        Arc::new(ahma_mcp::llm_service::DefaultLlmCompletionService),
    )
    .await;
    let elapsed = start.elapsed();

    // Should finish well before the 60s sleep timeout.
    assert!(
        elapsed < Duration::from_secs(5),
        "pipeline should stop promptly after cancellation, took {:.2?}",
        elapsed
    );
    assert!(
        alerts_for(&monitor, "test-op-cancel").await.is_empty(),
        "no alerts expected after immediate cancellation"
    );
}

/// LLM returning HTTP 500 should not crash the pipeline — it logs a warning
/// and continues to the next chunk.
#[tokio::test]
async fn test_livelog_pipeline_llm_http_500_graceful() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    let temp_dir = tempdir().unwrap();
    let sandbox = make_sandbox(temp_dir.path());

    let config = make_config(
        "echo",
        vec!["ERROR something broke".to_string()],
        &server.uri(),
        "look for errors",
    );

    let token = CancellationToken::new();
    let monitor = make_monitor();
    register_op(&monitor, "test-op-500").await;

    // Pipeline should complete without panic/hang despite LLM 500.
    run_livelog_pipeline(
        "test-op-500",
        &config,
        &default_runtime(&config),
        &sandbox,
        temp_dir.path(),
        token,
        monitor.clone(),
        Arc::new(ahma_mcp::llm_service::DefaultLlmCompletionService),
    )
    .await;

    // The LLM error means no alert is generated (the error is logged, not propagated).
    assert!(
        alerts_for(&monitor, "test-op-500").await.is_empty(),
        "LLM 500 should not produce an alert"
    );
}

/// LLM returning invalid JSON should be handled gracefully — no crash, no alert.
#[tokio::test]
async fn test_livelog_pipeline_llm_malformed_json_graceful() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not valid json {{{"))
        .mount(&server)
        .await;

    let temp_dir = tempdir().unwrap();
    let sandbox = make_sandbox(temp_dir.path());

    let config = make_config(
        "echo",
        vec!["WARN something fishy".to_string()],
        &server.uri(),
        "look for warnings",
    );

    let token = CancellationToken::new();
    let monitor = make_monitor();
    register_op(&monitor, "test-op-bad-json").await;

    run_livelog_pipeline(
        "test-op-bad-json",
        &config,
        &default_runtime(&config),
        &sandbox,
        temp_dir.path(),
        token,
        monitor.clone(),
        Arc::new(ahma_mcp::llm_service::DefaultLlmCompletionService),
    )
    .await;

    assert!(
        alerts_for(&monitor, "test-op-bad-json").await.is_empty(),
        "malformed LLM JSON should not produce an alert"
    );
}

/// With cooldown=0, every chunk that triggers an LLM issue should fire an alert.
#[tokio::test]
async fn test_livelog_pipeline_zero_cooldown_fires_all_alerts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_llm_response("Bug detected!")))
        .expect(2) // two chunks → two LLM calls
        .mount(&server)
        .await;

    let temp_dir = tempdir().unwrap();
    let sandbox = make_sandbox(temp_dir.path());

    // Two lines with chunk_max_lines=1 → two chunks.
    let mut config = make_config(
        "printf",
        vec!["line1\\nline2\\n".to_string()],
        &server.uri(),
        "look for bugs",
    );
    config.cooldown_seconds = 0; // no suppression

    let token = CancellationToken::new();
    let monitor = make_monitor();
    register_op(&monitor, "test-op-zero-cd").await;

    run_livelog_pipeline(
        "test-op-zero-cd",
        &config,
        &default_runtime(&config),
        &sandbox,
        temp_dir.path(),
        token,
        monitor.clone(),
        Arc::new(ahma_mcp::llm_service::DefaultLlmCompletionService),
    )
    .await;

    let alerts = alerts_for(&monitor, "test-op-zero-cd").await;
    assert_eq!(
        alerts.len(),
        2,
        "both chunks should produce alerts with cooldown=0, got {}",
        alerts.len()
    );
}

/// A source command that does not exist should not crash the pipeline.
#[tokio::test]
async fn test_livelog_pipeline_source_not_found_graceful() {
    let server = MockServer::start().await;
    // LLM mock is set up but should never be reached.

    let temp_dir = tempdir().unwrap();
    let sandbox = make_sandbox(temp_dir.path());

    let config = make_config(
        "this_command_definitely_does_not_exist_12345",
        vec![],
        &server.uri(),
        "unused",
    );

    let token = CancellationToken::new();
    let monitor = make_monitor();
    register_op(&monitor, "test-op-not-found").await;

    // Should complete without panicking.
    run_livelog_pipeline(
        "test-op-not-found",
        &config,
        &default_runtime(&config),
        &sandbox,
        temp_dir.path(),
        token,
        monitor.clone(),
        Arc::new(ahma_mcp::llm_service::DefaultLlmCompletionService),
    )
    .await;

    assert!(
        alerts_for(&monitor, "test-op-not-found").await.is_empty(),
        "no alerts expected when source command not found"
    );
}

/// A pre-filter that excludes every emitted line means the LLM is never asked,
/// so no alert fires even though the mock would flag anything it received.
#[tokio::test]
async fn test_livelog_prefilter_excludes_all_lines_no_llm_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(make_llm_response("ISSUE: would-be crash detected")),
        )
        .expect(0) // the LLM must NOT be called when nothing passes the filter
        .mount(&server)
        .await;

    let temp_dir = tempdir().unwrap();
    let sandbox = make_sandbox(temp_dir.path());

    let mut config = make_config(
        "echo",
        vec!["INFO perfectly benign line".to_string()],
        &server.uri(),
        "look for crashes",
    );
    // Only forward lines that look like errors; the benign INFO line is dropped.
    config.prefilter_regex = Some(r"(?i)\b(FATAL|error|exception)\b".to_string());

    let monitor = make_monitor();
    register_op(&monitor, "test-op-prefilter-none").await;

    run_livelog_pipeline(
        "test-op-prefilter-none",
        &config,
        &default_runtime(&config),
        &sandbox,
        temp_dir.path(),
        CancellationToken::new(),
        monitor.clone(),
        Arc::new(ahma_mcp::llm_service::DefaultLlmCompletionService),
    )
    .await;

    assert!(
        alerts_for(&monitor, "test-op-prefilter-none")
            .await
            .is_empty(),
        "filtered-out lines must not reach the LLM"
    );
}

/// A line that matches the pre-filter still reaches the LLM and produces an alert.
#[tokio::test]
async fn test_livelog_prefilter_passes_matching_line() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(make_llm_response("Crash: NPE in onCreate")),
        )
        .mount(&server)
        .await;

    let temp_dir = tempdir().unwrap();
    let sandbox = make_sandbox(temp_dir.path());

    let mut config = make_config(
        "echo",
        vec!["FATAL EXCEPTION: boom".to_string()],
        &server.uri(),
        "look for crashes",
    );
    config.prefilter_regex = Some(r"(?i)\bFATAL\b".to_string());

    let monitor = make_monitor();
    register_op(&monitor, "test-op-prefilter-pass").await;

    run_livelog_pipeline(
        "test-op-prefilter-pass",
        &config,
        &default_runtime(&config),
        &sandbox,
        temp_dir.path(),
        CancellationToken::new(),
        monitor.clone(),
        Arc::new(ahma_mcp::llm_service::DefaultLlmCompletionService),
    )
    .await;

    let alerts = alerts_for(&monitor, "test-op-prefilter-pass").await;
    assert_eq!(
        alerts.len(),
        1,
        "matching line should produce an alert: {alerts:?}"
    );
    assert!(alerts[0].contains("NPE in onCreate"));
}

/// An invalid pre-filter pattern is non-fatal: monitoring proceeds with every
/// line forwarded (the bad regex is ignored, not a hard error).
#[tokio::test]
async fn test_livelog_invalid_prefilter_falls_back_to_pass_all() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_llm_response("Issue: boom")))
        .mount(&server)
        .await;

    let temp_dir = tempdir().unwrap();
    let sandbox = make_sandbox(temp_dir.path());

    let mut config = make_config(
        "echo",
        vec!["FATAL boom".to_string()],
        &server.uri(),
        "look for crashes",
    );
    config.prefilter_regex = Some("(unclosed".to_string()); // invalid regex

    let monitor = make_monitor();
    register_op(&monitor, "test-op-prefilter-bad").await;

    run_livelog_pipeline(
        "test-op-prefilter-bad",
        &config,
        &default_runtime(&config),
        &sandbox,
        temp_dir.path(),
        CancellationToken::new(),
        monitor.clone(),
        Arc::new(ahma_mcp::llm_service::DefaultLlmCompletionService),
    )
    .await;

    assert_eq!(
        alerts_for(&monitor, "test-op-prefilter-bad").await.len(),
        1,
        "invalid regex should not suppress monitoring"
    );
}
