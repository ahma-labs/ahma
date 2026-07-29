use super::*;
use crate::operation_monitor::{Operation, OperationStatus};
use serde_json::json;

fn make_map(pairs: &[(&str, &str)]) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), json!(*v));
    }
    m
}

fn make_op(id: &str, tool: &str, status: OperationStatus) -> Operation {
    let mut op = Operation::new(id.to_string(), tool.to_string(), String::new(), None);
    op.state = status;
    op
}

#[test]
fn execution_error_attaches_structured_sandbox_denial() {
    use crate::sandbox::SandboxError;
    use std::path::PathBuf;

    let err: anyhow::Error = SandboxError::PathOutsideSandbox {
        path: PathBuf::from("/out/of/scope"),
        scopes: vec![PathBuf::from("/work/space")],
    }
    .into();

    let mcp = execution_error(&err);
    let data = mcp.data.expect("sandbox denial must carry a data payload");
    assert_eq!(data["kind"], "sandbox_denial");
    assert_eq!(data["path"], "/out/of/scope");
    assert_eq!(data["access"], "write");
    assert_eq!(data["reason"], "path_outside_sandbox");
    assert_eq!(data["current_scopes"][0], "/work/space");
    assert!(
        data["remediation"]
            .as_str()
            .unwrap()
            .contains("ahma sandbox grant /out/of/scope"),
        "remediation should name the grant command: {data}"
    );
    // The human-readable message is preserved alongside the structured data.
    assert!(mcp.message.contains("Synchronous execution failed"));
}

#[test]
fn execution_error_attaches_runtime_denial_payload() {
    use crate::sandbox::SandboxError;
    use ahma_common::config::ScopeAccess;
    use std::path::PathBuf;

    // A runtime kernel denial carries the original command output in `details`
    // and the offending path/access so the agent can drive grant -> restart -> retry.
    let err: anyhow::Error = SandboxError::RuntimeDenial {
        path: PathBuf::from("/Users/me/.cargo/.crates.toml"),
        access: ScopeAccess::Rw,
        scopes: vec![PathBuf::from("/work/space")],
        details: "Command failed with exit code 101: stderr: Operation not permitted (os error 1)"
            .to_string(),
    }
    .into();

    let mcp = execution_error(&err);
    let data = mcp.data.expect("runtime denial must carry a data payload");
    assert_eq!(data["kind"], "sandbox_denial");
    assert_eq!(data["path"], "/Users/me/.cargo/.crates.toml");
    assert_eq!(data["access"], "write");
    assert_eq!(data["reason"], "runtime_kernel_denial");
    assert_eq!(data["current_scopes"][0], "/work/space");
    let remediation = data["remediation"].as_str().unwrap();
    assert!(
        remediation.contains("sandbox_grant") && remediation.contains("restart"),
        "remediation should describe the grant -> restart -> retry loop: {remediation}"
    );
    // The original command output is preserved in the human-readable message.
    assert!(mcp.message.contains("os error 1"));
}

#[test]
fn execution_error_without_sandbox_cause_has_no_data() {
    let err = anyhow::anyhow!("compilation failed: missing semicolon");
    let mcp = execution_error(&err);
    assert!(
        mcp.data.is_none(),
        "non-sandbox failures must not carry a denial payload"
    );
    assert!(mcp.message.contains("compilation failed"));
}

#[test]
fn test_parse_comma_separated_filter_basic() {
    let args = make_map(&[("tools", "cargo,clippy,nextest")]);
    let result = parse_comma_separated_filter(&args, "tools");
    assert_eq!(result, vec!["cargo", "clippy", "nextest"]);
}

#[test]
fn test_parse_comma_separated_filter_trims_whitespace() {
    let args = make_map(&[("tools", "  cargo , clippy , nextest  ")]);
    let result = parse_comma_separated_filter(&args, "tools");
    assert_eq!(result, vec!["cargo", "clippy", "nextest"]);
}

#[test]
fn test_parse_comma_separated_filter_filters_empty_segments() {
    let args = make_map(&[("tools", "cargo,,nextest,")]);
    let result = parse_comma_separated_filter(&args, "tools");
    assert_eq!(result, vec!["cargo", "nextest"]);
}

#[test]
fn test_parse_comma_separated_filter_missing_key() {
    let args = make_map(&[]);
    let result = parse_comma_separated_filter(&args, "tools");
    assert!(result.is_empty());
}

#[test]
fn test_parse_comma_separated_filter_single_value() {
    let args = make_map(&[("tools", "cargo")]);
    let result = parse_comma_separated_filter(&args, "tools");
    assert_eq!(result, vec!["cargo"]);
}

#[test]
fn test_parse_comma_separated_filter_only_commas() {
    let args = make_map(&[("tools", ",,,")]);
    let result = parse_comma_separated_filter(&args, "tools");
    assert!(result.is_empty());
}

#[test]
fn test_parse_tool_filters_delegates_to_tools_key() {
    let args = make_map(&[("tools", "cargo,clippy")]);
    let result = parse_tool_filters(&args);
    assert_eq!(result, vec!["cargo", "clippy"]);
}

#[test]
fn test_parse_tool_filters_empty_args() {
    let args = make_map(&[]);
    assert!(parse_tool_filters(&args).is_empty());
}

#[test]
fn test_parse_id_present() {
    let args = make_map(&[("id", "op-1234")]);
    assert_eq!(parse_id(&args), Some("op-1234".to_string()));
}

#[test]
fn test_parse_id_absent() {
    let args = make_map(&[]);
    assert_eq!(parse_id(&args), None);
}

#[test]
fn test_operation_matches_filters_no_filters_no_id() {
    let op = make_op("op-1", "cargo_build", OperationStatus::Completed);
    assert!(operation_matches_filters(&op, &[], None));
}

#[test]
fn test_operation_matches_filters_matching_tool_prefix() {
    let op = make_op("op-1", "cargo_build", OperationStatus::Completed);
    let filters = vec!["cargo".to_string()];
    assert!(operation_matches_filters(&op, &filters, None));
}

#[test]
fn test_operation_matches_filters_non_matching_tool_prefix() {
    let op = make_op("op-1", "cargo_build", OperationStatus::Completed);
    let filters = vec!["npm".to_string()];
    assert!(!operation_matches_filters(&op, &filters, None));
}

#[test]
fn test_operation_matches_filters_matching_id() {
    let op = make_op("op-42", "cargo_build", OperationStatus::Completed);
    assert!(operation_matches_filters(&op, &[], Some("op-42")));
}

#[test]
fn test_operation_matches_filters_non_matching_id() {
    let op = make_op("op-42", "cargo_build", OperationStatus::Completed);
    assert!(!operation_matches_filters(&op, &[], Some("op-99")));
}

#[test]
fn test_operation_matches_filters_tool_and_id_both_match() {
    let op = make_op("op-42", "cargo_build", OperationStatus::Completed);
    let filters = vec!["cargo".to_string()];
    assert!(operation_matches_filters(&op, &filters, Some("op-42")));
}

#[test]
fn test_operation_matches_filters_tool_matches_but_id_mismatch() {
    let op = make_op("op-42", "cargo_build", OperationStatus::Completed);
    let filters = vec!["cargo".to_string()];
    assert!(!operation_matches_filters(&op, &filters, Some("op-99")));
}

#[test]
fn test_serialize_operations_to_content_empty() {
    let ops: Vec<Operation> = vec![];
    let result = serialize_operations_to_content(&ops);
    assert!(result.is_empty());
}

#[test]
fn test_serialize_operations_to_content_single_op() {
    let op = make_op("op-1", "cargo_build", OperationStatus::Completed);
    let result = serialize_operations_to_content(&[op]);
    assert_eq!(result.len(), 1);
    let text = result[0].as_text().map(|t| t.text.as_str()).unwrap_or("");
    assert!(
        text.contains("op-1"),
        "Serialized content should contain op id: {text}"
    );
}

#[test]
fn test_serialize_operations_to_content_multiple_ops() {
    let ops = vec![
        make_op("op-1", "cargo_build", OperationStatus::Completed),
        make_op("op-2", "cargo_test", OperationStatus::Failed),
    ];
    let result = serialize_operations_to_content(&ops);
    assert_eq!(result.len(), 2);
}

#[test]
fn test_extract_output_none_result() {
    assert_eq!(extract_output_from_result(&None), "");
}

#[test]
fn test_extract_output_string_result() {
    let result = Some(json!("error: compilation failed"));
    assert_eq!(
        extract_output_from_result(&result),
        "error: compilation failed"
    );
}

#[test]
fn test_extract_output_stdout_only_exit_zero() {
    let result = Some(json!({
        "stdout": "hello world",
        "stderr": "",
        "exit_code": 0
    }));
    assert_eq!(extract_output_from_result(&result), "hello world");
}

#[test]
fn test_extract_output_stderr_only_exit_zero() {
    let result = Some(json!({
        "stdout": "",
        "stderr": "warning: unused variable",
        "exit_code": 0
    }));
    assert_eq!(
        extract_output_from_result(&result),
        "warning: unused variable"
    );
}

#[test]
fn test_extract_output_both_stdout_and_stderr_exit_zero() {
    let result = Some(json!({
        "stdout": "output",
        "stderr": "warning",
        "exit_code": 0
    }));
    let out = extract_output_from_result(&result);
    assert!(out.contains("output"));
    assert!(out.contains("warning"));
}

#[test]
fn test_extract_output_nonzero_exit_code() {
    let result = Some(json!({
        "stdout": "some output",
        "stderr": "error text",
        "exit_code": 1
    }));
    let out = extract_output_from_result(&result);
    assert!(
        out.contains("Exit code: 1"),
        "Non-zero exit should show exit code: {out}"
    );
    assert!(out.contains("some output"));
    assert!(out.contains("error text"));
}

#[test]
fn test_extract_output_arbitrary_json_fallback() {
    let result = Some(json!({"nested": {"key": "value"}}));
    let out = extract_output_from_result(&result);
    assert!(
        out.contains("nested"),
        "Fallback should serialize JSON: {out}"
    );
}

// ─── Inline result: the outcome is always stated (SPEC R2.6.2) ───────────────

/// A completed operation carrying stdout/stderr/exit_code, as the adapter
/// stores it.
fn completed_op(id: &str, title: &str, stdout: &str, stderr: &str, exit_code: i64) -> Operation {
    let mut op = make_op(id, "run_terminal_command", OperationStatus::Completed);
    op.title = Some(title.to_string());
    op.end_time = Some(op.start_time + std::time::Duration::from_millis(867));
    op.result = Some(json!({
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": exit_code,
    }));
    op
}

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect()
}

#[test]
fn silent_success_still_reports_the_outcome() {
    // REGRESSION: `cargo fmt --check` on clean code writes nothing and exits 0.
    // The inline result used to be the empty string, so the model could not
    // tell "passed" from "the tool is broken" — and a captured Antigravity
    // session did exactly that, then invented a `sync` parameter to fix it.
    let op = completed_op("op_1_cargo_fmt", "cargo fmt --check", "", "", 0);
    let text = text_of(&format_completed_operation(&op));

    assert!(!text.trim().is_empty(), "result must never be empty");
    assert!(
        text.contains("cargo fmt --check"),
        "must name what ran: {text:?}"
    );
    assert!(
        text.contains("exit 0"),
        "must state the exit code: {text:?}"
    );
    assert!(
        text.contains("(no output)"),
        "must say the command was silent rather than say nothing: {text:?}"
    );
}

#[test]
fn successful_output_is_preceded_by_the_identity_line() {
    let op = completed_op("op_2_echo", "echo hi", "hi\n", "", 0);
    let text = text_of(&format_completed_operation(&op));

    let (first, rest) = text.split_once('\n').expect("identity line then body");
    assert!(
        first.contains("echo hi") && first.contains("exit 0"),
        "{first:?}"
    );
    assert_eq!(rest, "hi", "the command's own output follows verbatim");
}

#[test]
fn failure_states_the_nonzero_exit_code() {
    let op = completed_op("op_3_build", "cargo build", "", "error[E0433]", 1);
    let text = text_of(&format_completed_operation(&op));

    assert!(
        text.contains("exit 1"),
        "must state the exit code: {text:?}"
    );
    assert!(text.contains("error[E0433]"), "must keep stderr: {text:?}");
}

// ─── Ignored-argument disclosure (SPEC R2.6.4) ───────────────────────────────

#[test]
fn append_note_extends_the_trailing_text_block() {
    let result = append_note(text_result("payload"), "\n\nNote: something");
    assert_eq!(text_of(&result), "payload\n\nNote: something");
}

#[test]
fn append_note_adds_a_block_when_there_is_no_text() {
    let empty = rmcp::model::CallToolResult::success(vec![]);
    let result = append_note(empty, "\n\nNote: something");
    assert_eq!(text_of(&result), "Note: something");
}

// ─── Adaptive inline window (SPEC R2.6.1, R2.6.5) ────────────────────────────

use crate::client_type::McpClientType;
use crate::constants::{INLINE_WINDOW_BUSY_SECS, INLINE_WINDOW_IDLE_SECS};
use crate::operation_monitor::{MonitorConfig, OperationMonitor};
use std::time::Duration;

async fn monitor_with_running(ids: &[&str]) -> OperationMonitor {
    let monitor = OperationMonitor::new(MonitorConfig::with_timeout(Duration::from_secs(60)));
    for id in ids {
        let mut op = make_op(id, "run_terminal_command", OperationStatus::InProgress);
        op.end_time = None;
        monitor.add_operation(op).await;
    }
    monitor
}

#[tokio::test]
async fn idle_session_waits_the_long_window() {
    // Nothing to overlap with: the model's next move would be `await` anyway,
    // so waiting is free and may save a whole round-trip.
    let monitor = monitor_with_running(&["op_1"]).await;
    let window = inline_window(&monitor, "op_1", McpClientType::ClaudeDesktop).await;
    assert_eq!(window, Duration::from_secs(INLINE_WINDOW_IDLE_SECS));
}

#[tokio::test]
async fn fanning_out_gets_the_short_window() {
    // Something else is already running, so holding this response delays the
    // next command in a fan-out. Hand the id back promptly instead.
    let monitor = monitor_with_running(&["op_1", "op_2"]).await;
    let window = inline_window(&monitor, "op_2", McpClientType::ClaudeDesktop).await;
    assert_eq!(window, Duration::from_secs(INLINE_WINDOW_BUSY_SECS));
}

#[tokio::test]
async fn a_tight_client_budget_clamps_the_window() {
    // Antigravity abandons the transport partway through a long request, so the
    // window can never approach its budget however idle the session is.
    let monitor = monitor_with_running(&["op_1"]).await;
    let window = inline_window(&monitor, "op_1", McpClientType::Antigravity).await;
    assert!(
        window <= McpClientType::Antigravity.request_budget() / 2,
        "window {window:?} must leave the client margin to receive the response"
    );
    assert!(window >= Duration::from_secs(INLINE_WINDOW_BUSY_SECS));
}
