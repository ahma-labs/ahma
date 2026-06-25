//! Integration tests for `run_file_monitor_pipeline`.
//!
//! Covers every early-exit path and the core monitoring loop:
//!
//! * LLM provider resolution error → early return (no panic, no alert)
//! * File-not-found → early return
//! * Pre-cancelled token → setup runs fully, loop exits at the first cancellation
//!   check (covers all of the `AnalysisCtx`/variable-init section)
//! * File grows by 50 lines → `chunk_max_lines` hit → LLM called → alert recorded
//! * LLM returns "CLEAN" → no alert
//! * LLM returns HTTP 500 → warn + no alert, no panic
//!
//! The "no-change → 500 ms sleep → cancel" branch is covered implicitly by the
//! file-growth test: after the initial chunk is flushed the file stops changing,
//! so the pipeline enters the `else` sleep branch before the test cancels it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tempfile::tempdir;
use tokio::io::AsyncWriteExt as _;
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

use ahma_mcp::config::LlmProviderConfig;
use ahma_mcp::livelog::run_file_monitor_pipeline;
use ahma_mcp::llm_service::DefaultLlmCompletionService;
use ahma_mcp::operation_monitor::{MonitorConfig, Operation, OperationMonitor};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_monitor() -> Arc<OperationMonitor> {
    Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(30),
    )))
}

async fn register_op(monitor: &OperationMonitor, op_id: &str) {
    monitor
        .add_operation(Operation::new_with_timeout(
            op_id.to_string(),
            "file_monitor".to_string(),
            format!("file monitor test op {op_id}"),
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

fn make_llm_response(content: &str) -> serde_json::Value {
    json!({
        "choices": [{"message": {"content": content, "role": "assistant"}}]
    })
}

/// Build a `LlmProviderConfig` pointing at `base_url` with no API key.
fn valid_provider(base_url: &str) -> LlmProviderConfig {
    LlmProviderConfig {
        base_url: base_url.to_string(),
        model: "test-model".to_string(),
        api_key: None,
    }
}

// ---------------------------------------------------------------------------
// Early-exit paths
// ---------------------------------------------------------------------------

/// When the `api_key` contains an unresolvable `${VAR}` reference, `resolve()`
/// returns `Err` and the pipeline exits immediately without touching the file or
/// the monitor.
#[tokio::test]
async fn test_file_monitor_llm_resolve_error_exits_early() {
    let temp_dir = tempdir().unwrap();
    let log_path = temp_dir.path().join("test.log");
    tokio::fs::write(&log_path, b"").await.unwrap();

    let monitor = make_monitor();
    register_op(&monitor, "op-llm-err").await;

    // Inject an api_key that references an env var guaranteed not to be set.
    // `interpolate_env_vars` returns `Err` for unresolvable `${VAR}` references.
    let bad_provider = LlmProviderConfig {
        base_url: "http://localhost:1234".to_string(),
        model: "test-model".to_string(),
        api_key: Some("${_AHMA_FILE_MON_TEST_MISSING_KEY_XYZ_}".to_string()),
    };

    let cancel = CancellationToken::new();
    run_file_monitor_pipeline(
        "op-llm-err",
        log_path,
        "detect errors".to_string(),
        bad_provider,
        cancel,
        monitor.clone(),
        Arc::new(DefaultLlmCompletionService),
    )
    .await;

    // No alert should be recorded because the pipeline returned before the loop.
    assert!(
        alerts_for(&monitor, "op-llm-err").await.is_empty(),
        "LLM resolve error should produce no alerts"
    );
}

/// A non-existent file path causes `File::open` to fail; the pipeline exits
/// immediately without panicking.
#[tokio::test]
async fn test_file_monitor_file_not_found_exits_early() {
    let temp_dir = tempdir().unwrap();
    let nonexistent = temp_dir.path().join("does_not_exist.log");

    let monitor = make_monitor();
    register_op(&monitor, "op-no-file").await;

    let cancel = CancellationToken::new();
    run_file_monitor_pipeline(
        "op-no-file",
        nonexistent,
        "detect errors".to_string(),
        valid_provider("http://localhost:1234"),
        cancel,
        monitor.clone(),
        Arc::new(DefaultLlmCompletionService),
    )
    .await;

    assert!(
        alerts_for(&monitor, "op-no-file").await.is_empty(),
        "file-not-found should produce no alerts"
    );
}

/// A pre-cancelled token exercises the full setup section (LLM resolve, file
/// open, metadata, seek, `AnalysisCtx` construction) and then breaks at the
/// very first cancellation check at the top of the loop.
#[tokio::test]
async fn test_file_monitor_pre_cancelled_token_completes_setup_then_exits() {
    let temp_dir = tempdir().unwrap();
    let log_path = temp_dir.path().join("test.log");
    tokio::fs::write(&log_path, b"initial content\n")
        .await
        .unwrap();

    let monitor = make_monitor();
    register_op(&monitor, "op-pre-cancel").await;

    // Cancel the token before the pipeline even starts.
    let cancel = CancellationToken::new();
    cancel.cancel();

    let start = std::time::Instant::now();
    run_file_monitor_pipeline(
        "op-pre-cancel",
        log_path,
        "detect errors".to_string(),
        valid_provider("http://localhost:1234"),
        cancel,
        monitor.clone(),
        Arc::new(DefaultLlmCompletionService),
    )
    .await;
    let elapsed = start.elapsed();

    // Should finish almost instantly (no sleeps, exits at first cancellation check).
    assert!(
        elapsed < Duration::from_secs(2),
        "pre-cancelled pipeline should finish quickly, took {elapsed:.2?}"
    );
    assert!(
        alerts_for(&monitor, "op-pre-cancel").await.is_empty(),
        "no alerts expected after immediate cancellation"
    );
}

// ---------------------------------------------------------------------------
// File-growth path — chunk_max_lines = 50 (hardcoded in the pipeline)
// ---------------------------------------------------------------------------

/// Writing exactly 50 lines to the log file while the pipeline is polling
/// triggers `chunk_max_lines`, which flushes to the LLM and records an alert.
///
/// The "no-change → 500 ms sleep" branch is also exercised here: the pipeline
/// enters it once (while the file is still empty) and again after the chunk is
/// flushed (file unchanged while the test cancels).
#[tokio::test]
async fn test_file_monitor_file_growth_triggers_llm_and_records_alert() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(make_llm_response("Crash: NullPointerException at line 42")),
        )
        .mount(&server)
        .await;

    let temp_dir = tempdir().unwrap();
    let log_path = temp_dir.path().join("growing.log");
    // Start with an empty file so the pipeline positions at offset 0.
    tokio::fs::write(&log_path, b"").await.unwrap();

    let monitor = make_monitor();
    register_op(&monitor, "op-grow").await;

    let cancel = CancellationToken::new();
    let cancel_bg = cancel.clone();
    let monitor_bg = monitor.clone();
    let log_path_bg = log_path.clone();
    // Capture the URI before moving `server` into the background task.
    let server_uri = server.uri();

    tokio::spawn(async move {
        run_file_monitor_pipeline(
            "op-grow",
            log_path_bg,
            "look for crashes".to_string(),
            valid_provider(&server_uri),
            cancel_bg,
            monitor_bg,
            Arc::new(DefaultLlmCompletionService),
        )
        .await;
    });

    // Give the pipeline time to open the file, get metadata (0 bytes), and
    // enter the 500 ms "no new data" sleep.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Write exactly 50 lines — enough to hit `chunk_max_lines` in one read.
    let mut content = String::with_capacity(50 * 30);
    for i in 0..50 {
        content.push_str(&format!("Error: crash iteration {i}\n"));
    }
    // Append to the file (pipeline's pos is 0 so growth = full content).
    {
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .await
            .unwrap();
        f.write_all(content.as_bytes()).await.unwrap();
        f.flush().await.unwrap();
    }

    // Poll until the mock receives a request or we time out.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let received = server.received_requests().await.unwrap_or_default().len();
        if received > 0 || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    cancel.cancel();

    // Give the background task a moment to shut down before asserting.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests.is_empty(),
        "LLM should have been called after 50 lines were appended"
    );

    let alerts = alerts_for(&monitor, "op-grow").await;
    assert_eq!(
        alerts.len(),
        1,
        "exactly one alert expected (one chunk hit the size limit): {alerts:?}"
    );
    assert!(
        alerts[0].contains("NullPointerException"),
        "alert should contain the LLM summary: {}",
        alerts[0]
    );
    assert!(
        alerts[0].contains("Error: crash"),
        "alert should contain the raw log context: {}",
        alerts[0]
    );
}

/// When the LLM responds with "CLEAN" the pipeline records no alert, even
/// though 50 lines were forwarded.
#[tokio::test]
async fn test_file_monitor_clean_llm_response_no_alert() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_llm_response("CLEAN")))
        .mount(&server)
        .await;

    let temp_dir = tempdir().unwrap();
    let log_path = temp_dir.path().join("clean.log");
    tokio::fs::write(&log_path, b"").await.unwrap();

    let monitor = make_monitor();
    register_op(&monitor, "op-clean").await;

    let cancel = CancellationToken::new();
    let cancel_bg = cancel.clone();
    let monitor_bg = monitor.clone();
    let log_path_bg = log_path.clone();
    let server_uri = server.uri();

    tokio::spawn(async move {
        run_file_monitor_pipeline(
            "op-clean",
            log_path_bg,
            "look for crashes".to_string(),
            valid_provider(&server_uri),
            cancel_bg,
            monitor_bg,
            Arc::new(DefaultLlmCompletionService),
        )
        .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut content = String::new();
    for i in 0..50 {
        content.push_str(&format!("INFO: boring log line {i}\n"));
    }
    {
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .await
            .unwrap();
        f.write_all(content.as_bytes()).await.unwrap();
        f.flush().await.unwrap();
    }

    // Wait for the LLM to be called.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let received = server.received_requests().await.unwrap_or_default().len();
        if received > 0 || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    cancel.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // LLM was called but returned "CLEAN".
    assert!(
        !server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "LLM should be called even when the response is CLEAN"
    );
    assert!(
        alerts_for(&monitor, "op-clean").await.is_empty(),
        "CLEAN response must produce no alert"
    );
}

/// An HTTP 500 from the LLM is handled gracefully: a warning is logged but the
/// pipeline does not panic and records no alert.
#[tokio::test]
async fn test_file_monitor_llm_http_error_no_panic_no_alert() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    let temp_dir = tempdir().unwrap();
    let log_path = temp_dir.path().join("llm_err.log");
    tokio::fs::write(&log_path, b"").await.unwrap();

    let monitor = make_monitor();
    register_op(&monitor, "op-llm-500").await;

    let cancel = CancellationToken::new();
    let cancel_bg = cancel.clone();
    let monitor_bg = monitor.clone();
    let log_path_bg = log_path.clone();
    let server_uri = server.uri();

    tokio::spawn(async move {
        run_file_monitor_pipeline(
            "op-llm-500",
            log_path_bg,
            "look for crashes".to_string(),
            valid_provider(&server_uri),
            cancel_bg,
            monitor_bg,
            Arc::new(DefaultLlmCompletionService),
        )
        .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut content = String::new();
    for i in 0..50 {
        content.push_str(&format!("Error: line {i}\n"));
    }
    {
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .await
            .unwrap();
        f.write_all(content.as_bytes()).await.unwrap();
        f.flush().await.unwrap();
    }

    // Wait for the LLM mock to be hit.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let received = server.received_requests().await.unwrap_or_default().len();
        if received > 0 || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    cancel.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // LLM was called but errored — no alert, no panic.
    assert!(
        !server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "LLM endpoint must have been reached even though it returned 500"
    );
    assert!(
        alerts_for(&monitor, "op-llm-500").await.is_empty(),
        "HTTP 500 from LLM must not produce an alert"
    );
}
