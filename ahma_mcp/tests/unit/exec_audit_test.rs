//! Execution audit log (`<project log dir>/audit.jsonl`) — in-process coverage.
//!
//! The claim under test is provenance, not logging: after a session, can you
//! answer *what ran, when, where, and what did it write that something else will
//! later execute?* Every test here reads the JSONL back and asserts on the
//! recorded facts, never on internal bookkeeping.
//!
//! All tests share one audit file. The process-wide log resolves exactly once
//! ([`ahma_mcp::adapter::audit::set_audit_log_path`]), so a per-test file is not
//! possible in a shared binary; instead each test filters the log by its own
//! operation id or file path. That is also a stronger test — it exercises the
//! append-only file exactly as production uses it, with unrelated operations
//! interleaved.

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::adapter::{Adapter, AsyncExecOptions};
use ahma_mcp::file_ops::{DefaultFileOpsProvider, FileOpsProvider};
use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────────────
// Harness
// ─────────────────────────────────────────────────────────────────────────────

/// The tempdir holding this test binary's audit log. Leaked for the process
/// lifetime on purpose: the log path is resolved once and every test in the
/// binary writes to it, so it must outlive any individual test.
static AUDIT_DIR: OnceLock<TempDir> = OnceLock::new();

/// Redirect the process-wide audit log into a tempdir and return its path.
///
/// Idempotent: the first caller wins and every later caller gets the same file.
fn audit_log_path() -> PathBuf {
    let dir = AUDIT_DIR.get_or_init(|| tempfile::tempdir().expect("audit tempdir"));
    let path = dir.path().join("audit.jsonl");
    // Ignore the return value: another test in this binary may have set it first,
    // which is exactly the shared-file situation these tests are written for.
    let _ = ahma_mcp::adapter::audit::set_audit_log_path(path.clone());
    path
}

/// Read every complete event currently in the audit log.
///
/// Parsing is strict on purpose: a partially written or interleaved line fails
/// here, which is the JSONL invariant the concurrency test relies on.
async fn read_events() -> Vec<Value> {
    let path = audit_log_path();
    let contents = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("audit line must be complete JSON ({e}): {l}"))
        })
        .collect()
}

/// Events belonging to one operation id, in file order.
async fn events_for(op_id: &str) -> Vec<Value> {
    read_events()
        .await
        .into_iter()
        .filter(|e| e["operation_id"] == op_id)
        .collect()
}

/// Poll the audit log until `pred` holds or the tool-call budget expires.
async fn wait_until<F>(mut pred: F) -> bool
where
    F: FnMut(&[Value]) -> bool,
{
    let deadline = tokio::time::Instant::now() + TestTimeouts::get(TimeoutCategory::ToolCall);
    loop {
        let events = read_events().await;
        if pred(&events) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(TestTimeouts::poll_interval()).await;
    }
}

fn test_adapter(scope: &Path) -> Adapter {
    audit_log_path();
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        TestTimeouts::get(TimeoutCategory::ToolCall),
    )));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let sandbox = Arc::new(
        Sandbox::new(
            vec![scope.to_path_buf(), std::env::temp_dir()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .expect("test sandbox"),
    );
    Adapter::new(monitor, shell_pool, sandbox).expect("adapter")
}

/// Canonicalized scope directory — matches what the sandbox and the harness
/// tools produce after canonicalization (`/var` -> `/private/var` on macOS,
/// verbatim-prefix normalization on Windows).
fn scope_dir() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let canonical = dunce::canonicalize(dir.path()).expect("canonicalize");
    (dir, canonical)
}

async fn dispatch_echo_with_args(adapter: &Adapter, scope: &Path, args_list: Vec<Value>) -> String {
    let mut args = Map::new();
    args.insert("args".to_string(), Value::Array(args_list));
    adapter
        .execute_async_in_dir_with_options(
            "echo_tool",
            "echo",
            scope.to_str().unwrap(),
            AsyncExecOptions {
                id: None,
                args: Some(args),
                timeout: Some(TestTimeouts::get(TimeoutCategory::ToolCall).as_secs()),
                subcommand_config: None,
                log_monitor_config: None,
            },
        )
        .await
        .expect("dispatch")
}

// ─────────────────────────────────────────────────────────────────────────────
// tool_call is written before execution
// ─────────────────────────────────────────────────────────────────────────────

/// The central claim: the record of *what was asked for* exists before the
/// process does. A command that is killed, times out, or hangs forever must
/// still be in the log.
#[tokio::test]
async fn tool_call_is_recorded_before_the_command_can_complete() {
    let (_dir, scope) = scope_dir();
    let adapter = test_adapter(&scope);

    // A command that far outlives its own timeout: the operation is guaranteed to
    // end by being killed, never by finishing.
    let op_id = adapter
        .execute_async_in_dir(
            "sleeper",
            "sleep 300",
            None,
            scope.to_str().unwrap(),
            Some(TestTimeouts::scale_secs(2).as_secs()),
        )
        .await
        .expect("dispatch");

    // The command is still running — `execute_async_in_dir` returns as soon as
    // the task is spawned — yet the call is already on disk.
    let events = events_for(&op_id).await;
    assert_eq!(
        events.len(),
        1,
        "exactly the tool_call, no completion yet: {events:?}"
    );
    assert_eq!(events[0]["type"], "tool_call");
    assert_eq!(events[0]["tool_name"], "sleeper");
    assert_eq!(
        events[0]["working_dir"],
        scope.to_string_lossy().as_ref(),
        "the resolved working directory is part of the record"
    );
    assert!(
        events[0]["command"]
            .as_str()
            .is_some_and(|c| c.contains("sleep")),
        "the command as it will actually run must be recorded: {events:?}"
    );

    // Being killed must still close the record — the log distinguishes "was
    // killed" from "we lost track of it".
    assert!(
        wait_until(|events| {
            events
                .iter()
                .any(|e| e["operation_id"] == op_id.as_str() && e["type"] == "tool_complete")
        })
        .await,
        "a killed operation must still be closed out in the audit log"
    );

    let events = events_for(&op_id).await;
    let complete = events
        .iter()
        .find(|e| e["type"] == "tool_complete")
        .expect("completion present");
    assert_eq!(complete["success"], false);
    assert_eq!(
        complete["outcome"], "timed_out",
        "the reason it ended is part of the record: {complete:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// tool_complete pairs correctly
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tool_call_and_tool_complete_pair_for_a_successful_command() {
    let (_dir, scope) = scope_dir();
    let adapter = test_adapter(&scope);

    let op_id = dispatch_echo_with_args(&adapter, &scope, vec![json!("audit-pairing")]).await;

    assert!(
        wait_until(|events| {
            events
                .iter()
                .any(|e| e["operation_id"] == op_id.as_str() && e["type"] == "tool_complete")
        })
        .await,
        "completion must be recorded"
    );

    let events = events_for(&op_id).await;
    assert_eq!(events.len(), 2, "exactly one pair: {events:?}");
    assert_eq!(events[0]["type"], "tool_call", "the call comes first");
    assert_eq!(events[1]["type"], "tool_complete");
    assert_eq!(events[1]["success"], true);
    assert_eq!(events[1]["exit_code"], 0);
    assert_eq!(events[1]["outcome"], "completed");
    assert!(
        events[1]["duration_ms"].is_u64(),
        "a duration is part of the record: {events:?}"
    );
    assert!(
        events[0]["args_summary"]
            .as_str()
            .is_some_and(|s| s.contains("audit-pairing")),
        "arguments are summarised: {events:?}"
    );
}

/// The synchronous path mints its own id so its call and completion can still be
/// correlated; a failure must be recorded as one, not omitted.
#[tokio::test]
async fn the_synchronous_path_records_a_failed_command() {
    let (_dir, scope) = scope_dir();
    let adapter = test_adapter(&scope);

    let before = read_events().await.len();
    let result = adapter
        .execute_sync_in_dir(
            "ahma_audit_nonexistent_command",
            None,
            scope.to_str().unwrap(),
            Some(TestTimeouts::get(TimeoutCategory::Quick).as_secs()),
            None,
        )
        .await;
    assert!(result.is_err(), "the command does not exist");

    let events = read_events().await;
    let new: Vec<&Value> = events[before..].iter().collect();
    let call = new
        .iter()
        .find(|e| e["type"] == "tool_call" && e["tool_name"] == "ahma_audit_nonexistent_command")
        .expect("the synchronous path records a tool_call");
    let op_id = call["operation_id"].as_str().expect("an operation id");

    let complete = new
        .iter()
        .find(|e| e["type"] == "tool_complete" && e["operation_id"] == op_id)
        .expect("…and pairs it with a tool_complete");
    assert_eq!(complete["success"], false);
}

// ─────────────────────────────────────────────────────────────────────────────
// Redaction
// ─────────────────────────────────────────────────────────────────────────────

/// An audit log that captures secrets is a liability, not a control. The
/// argument summary and the command string go through the same redaction path
/// operation output already uses before it is spilled.
#[tokio::test]
async fn secrets_are_redacted_out_of_the_arguments_and_the_command() {
    let (_dir, scope) = scope_dir();
    let adapter = test_adapter(&scope);

    const GITHUB_TOKEN: &str = "ghp_auditshouldnevercapturethis01234";
    const PROVIDER_KEY: &str = "sk-ant-api03-auditshouldnevercapturethis";

    let op_id = dispatch_echo_with_args(
        &adapter,
        &scope,
        vec![
            json!(format!("GITHUB_TOKEN={GITHUB_TOKEN}")),
            json!(PROVIDER_KEY),
        ],
    )
    .await;

    let events = events_for(&op_id).await;
    let call = &events[0];
    let summary = call["args_summary"].as_str().unwrap_or_default();
    let command = call["command"].as_str().unwrap_or_default();

    assert!(
        !summary.contains(GITHUB_TOKEN),
        "the token must not reach the audit log via args: {summary}"
    );
    assert!(
        !command.contains(GITHUB_TOKEN),
        "…nor via the command string: {command}"
    );
    assert!(
        !summary.contains(PROVIDER_KEY) && !command.contains(PROVIDER_KEY),
        "provider keys must be redacted too: {summary} / {command}"
    );
    assert!(
        summary.contains("[REDACTED]"),
        "redaction must be visible in the record rather than silently dropping it: {summary}"
    );

    // Belt and braces: the raw secret must not appear anywhere in the file.
    let raw = tokio::fs::read_to_string(audit_log_path())
        .await
        .unwrap_or_default();
    assert!(!raw.contains(GITHUB_TOKEN), "token leaked into audit.jsonl");
    assert!(
        !raw.contains(PROVIDER_KEY),
        "provider key leaked into audit.jsonl"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Trust-handoff disclosure (SPEC R-HANDOFF)
// ─────────────────────────────────────────────────────────────────────────────

/// The highest-value entry in the log. The `Disclose` tier lets the write
/// through and warns — but the warning is transient and the execution happens
/// later, so the durable record is the actual control.
#[tokio::test]
async fn a_disclose_tier_write_records_the_file_and_the_trigger() {
    audit_log_path();
    let (_dir, scope) = scope_dir();
    let scopes = vec![scope.clone()];
    let target = scope.join(".vscode").join("tasks.json");
    tokio::fs::create_dir_all(scope.join(".vscode"))
        .await
        .expect("create .vscode");

    DefaultFileOpsProvider
        .write_file(&scopes, &target, "{}")
        .await
        .expect("a disclosed write still succeeds");

    let events = read_events().await;
    let handoff = events
        .iter()
        .rev()
        .find(|e| {
            e["type"] == "trust_handoff_disclosure"
                && e["path"].as_str().is_some_and(|p| p.contains("tasks.json"))
        })
        .expect("the disclosed write must be in the audit log");

    assert!(
        handoff["path"]
            .as_str()
            .is_some_and(|p| p.contains(".vscode")),
        "the record names the file: {handoff:?}"
    );
    assert!(
        handoff["trigger"]
            .as_str()
            .is_some_and(|t| t.contains("folderOpen")),
        "…and what will execute it: {handoff:?}"
    );
    assert_eq!(
        handoff["tool_name"], "write_file",
        "…and which tool wrote it"
    );
}

/// A refused write is not a handoff. Nothing may be recorded for the `DenyWrite`
/// tier, or the log would report handoffs that never happened.
#[tokio::test]
async fn a_refused_write_records_no_handoff() {
    audit_log_path();
    let (_dir, scope) = scope_dir();
    let scopes = vec![scope.clone()];
    tokio::fs::create_dir_all(scope.join(".git").join("hooks"))
        .await
        .expect("create hooks dir");
    let hook = scope.join(".git").join("hooks").join("pre-commit");

    let before = read_events().await.len();
    let result = DefaultFileOpsProvider
        .write_file(&scopes, &hook, "exfiltrate\n")
        .await;
    assert!(result.is_err(), "a git hook write is refused");

    let after = read_events().await;
    assert!(
        !after[before..]
            .iter()
            .any(|e| e["type"] == "trust_handoff_disclosure"),
        "a refused write is not a trust handoff: {:?}",
        &after[before..]
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Append-only under concurrency
// ─────────────────────────────────────────────────────────────────────────────

/// Several operations running at once append to one file with no lock between
/// them. JSONL means one complete line per event; a torn or interleaved line
/// makes `read_events` panic, which is the assertion.
#[tokio::test]
async fn concurrent_operations_do_not_corrupt_the_log() {
    let (_dir, scope) = scope_dir();
    let adapter = Arc::new(test_adapter(&scope));

    let mut op_ids = Vec::new();
    for i in 0..8u32 {
        let op_id =
            dispatch_echo_with_args(&adapter, &scope, vec![json!(format!("concurrent-{i}"))]).await;
        op_ids.push(op_id);
    }

    let wanted: Vec<String> = op_ids.clone();
    assert!(
        wait_until(move |events| {
            wanted.iter().all(|id| {
                events
                    .iter()
                    .any(|e| e["operation_id"] == id.as_str() && e["type"] == "tool_complete")
            })
        })
        .await,
        "every concurrent operation must be closed out"
    );

    // read_events() would have panicked on any partial line; now check that no
    // operation lost or duplicated an event under the interleaving.
    let events = read_events().await;
    for id in &op_ids {
        let mine: Vec<&Value> = events
            .iter()
            .filter(|e| e["operation_id"] == id.as_str())
            .collect();
        assert_eq!(mine.len(), 2, "exactly one call + one completion for {id}");
        assert_eq!(mine[0]["type"], "tool_call");
        assert_eq!(mine[1]["type"], "tool_complete");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Graceful degradation
// ─────────────────────────────────────────────────────────────────────────────

/// An unwritable audit destination must cost the operation nothing. This
/// exercises the real writer against a path that cannot be created, on every
/// platform, and asserts the command still succeeds.
#[tokio::test]
async fn an_unwritable_audit_log_does_not_fail_the_operation() {
    use ahma_mcp::adapter::audit::{AuditEventKind, AuditLog};

    let tmp = tempfile::tempdir().expect("tempdir");
    // A regular file standing where the parent directory would have to be.
    let blocker = tmp.path().join("not-a-directory");
    tokio::fs::write(&blocker, b"x")
        .await
        .expect("write blocker");
    let log = AuditLog::at(blocker.join("logs").join("audit.jsonl"));

    let event = AuditEventKind::ToolComplete {
        operation_id: "op_degraded".into(),
        success: true,
        duration_ms: 1,
        exit_code: Some(0),
        outcome: Some("completed".into()),
    };
    assert!(
        log.emit(event.clone()).await.is_err(),
        "emit reports the failure"
    );
    // record swallows it — returning normally is the whole assertion.
    log.record(event).await;

    // …and a real command through the adapter is unaffected either way.
    let (_dir, scope) = scope_dir();
    let adapter = test_adapter(&scope);
    let result = adapter
        .execute_sync_in_dir(
            "echo",
            None,
            scope.to_str().unwrap(),
            Some(TestTimeouts::get(TimeoutCategory::Quick).as_secs()),
            None,
        )
        .await;
    assert!(result.is_ok(), "audit trouble never fails a command");
}
