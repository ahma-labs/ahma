use super::common;
use crate::AhmaMcpService;
use crate::mcp_service::schema;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData as McpError};
use serde_json::{Map, Value};
use std::sync::Arc;

/// JSON schema for the `cancel` tool: cancel one operation by `id`, or **all**
/// in-flight operations with `all: true`. Exactly one of the two must be given.
pub fn cancel_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "id".to_string(),
        schema::string_property(
            "The operation_id to cancel. Omit when using `all`. Exactly one of `id` or `all` is required.",
        ),
    );
    props.insert(
        "all".to_string(),
        schema::boolean_property(
            "Cancel EVERY in-flight operation and reap each one's process tree (cargo/rustc/sccache). The clean alternative to killing and restarting the server when work wedges. Omit `id` when set.",
        ),
    );
    props.insert(
        "reason".to_string(),
        schema::string_property("Optional human-readable reason recorded with the cancellation."),
    );
    // Neither field is individually `required` — the handler enforces the
    // "exactly one of id/all" rule with a clear error.
    schema::object_input_schema(props, &[])
}

impl AhmaMcpService {
    /// Handles the 'cancel' tool call.
    pub async fn handle_cancel(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        // Optional cancellation reason to aid debugging (shared by both modes).
        let reason: Option<String> = common::opt_str(&args, "reason");

        // Bulk mode: `all: true` cancels every in-flight operation.
        if args.get("all").and_then(Value::as_bool) == Some(true) {
            return Ok(self.handle_cancel_all(reason).await);
        }

        let id = args
            .get("id")
            .ok_or_else(|| {
                McpError::invalid_params(
                    "either `id` or `all: true` is required".to_string(),
                    Some(serde_json::json!({ "missing_param": "id" })),
                )
            })?
            .as_str()
            .ok_or_else(|| {
                McpError::invalid_params(
                    "id must be a string".to_string(),
                    Some(serde_json::json!({ "id": args.get("id") })),
                )
            })?
            .to_string();

        // Attempt to cancel the operation
        let cancelled = self
            .operation_monitor
            .cancel_operation_with_reason(&id, reason.clone())
            .await;

        let result_message = if cancelled {
            let why = reason
                .as_deref()
                .unwrap_or("No reason provided (default: user-initiated)");
            format!(
                "OK Operation '{}' has been cancelled successfully.\nString: reason='{}'.\nHint: Consider restarting the operation if needed.",
                id, why
            )
        } else {
            // Check if operation exists but is already terminal
            if let Some(operation) = self.operation_monitor.get_operation(&id).await {
                format!(
                    "WARNING Operation '{}' is already {} and cannot be cancelled.",
                    id,
                    match operation.state {
                        crate::operation_monitor::OperationStatus::Completed => "completed",
                        crate::operation_monitor::OperationStatus::Failed => "failed",
                        crate::operation_monitor::OperationStatus::Cancelled => "cancelled",
                        crate::operation_monitor::OperationStatus::TimedOut => "timed out",
                        _ => "in a terminal state",
                    }
                )
            } else {
                format!(
                    "FAIL Operation '{}' not found. It may have already completed or never existed.",
                    id
                )
            }
        };

        // Add a machine-parseable suggestion block to encourage restart via tool hint
        let suggestion = serde_json::json!({
            "tool_hint": {
                "suggested_tool": "status",
                "reason": "Operation cancelled; check status and consider restarting",
                "next_steps": [
                    {"tool": "status", "args": {"id": id}},
                    {"tool": "await", "args": {"tools": "", "timeout_seconds": 360}}
                ]
            }
        });

        Ok(CallToolResult::success(vec![
            ContentBlock::text(result_message),
            ContentBlock::text(suggestion.to_string()),
        ]))
    }

    /// Cancel every in-flight operation and report what was torn down. Each
    /// cancellation reaps the operation's full process tree via the streaming
    /// executor's verified kill, so this is the clean "stop everything" exit that
    /// avoids killing and restarting the server.
    async fn handle_cancel_all(&self, reason: Option<String>) -> CallToolResult {
        let cancelled = self
            .operation_monitor
            .cancel_all_operations(reason.clone())
            .await;

        let message = if cancelled.is_empty() {
            "OK No in-flight operations to cancel — nothing was running.".to_string()
        } else {
            format!(
                "OK Cancelled {} in-flight operation(s) and reaped their process trees: {}.\nString: reason='{}'.",
                cancelled.len(),
                cancelled.join(", "),
                reason
                    .as_deref()
                    .unwrap_or("No reason provided (default: user-initiated cancel-all)"),
            )
        };

        let suggestion = serde_json::json!({
            "tool_hint": {
                "suggested_tool": "status",
                "reason": "All operations cancelled; check status before starting new work",
                "next_steps": [
                    {"tool": "status", "args": {}},
                ],
                "cancelled_ids": cancelled,
            }
        });

        CallToolResult::success(vec![
            ContentBlock::text(message),
            ContentBlock::text(suggestion.to_string()),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::cancel_schema;

    #[test]
    fn cancel_schema_exposes_id_all_and_reason_with_none_required() {
        let schema = cancel_schema();
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("properties object");
        assert!(props.contains_key("id"), "id property present");
        assert!(props.contains_key("all"), "all property present");
        assert!(props.contains_key("reason"), "reason property present");
        assert_eq!(
            props["all"]["type"], "boolean",
            "all must be a boolean flag"
        );
        // Neither id nor all is statically required — the handler enforces the
        // exactly-one rule, so clients are not forced to send both.
        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        assert_eq!(required, 0, "no field is individually required");
    }
}
