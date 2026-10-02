use super::common;
use crate::AhmaMcpService;
use crate::mcp_service::schema;
use crate::operation_monitor::Operation;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData as McpError};
use serde_json::{Map, Value};
use std::sync::Arc;

impl AhmaMcpService {
    /// Generates the specific input schema for the `status` tool.
    pub fn generate_input_schema_for_status(&self) -> Arc<Map<String, Value>> {
        let mut properties = Map::new();
        properties.insert(
            "tools".to_string(),
            schema::string_property(
                "Comma-separated tool name prefixes to filter by (optional; shows all if omitted)",
            ),
        );
        properties.insert(
            "id".to_string(),
            schema::string_property(
                "Specific operation ID to query (optional; shows all if omitted)",
            ),
        );
        schema::object_input_schema(properties, &[])
    }

    /// Handles the 'status' tool call.
    pub async fn handle_status(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let tool_filters = common::parse_tool_filters(&args);
        let specific_id = common::parse_id(&args);

        let mut contents = Vec::new();

        let op_id_ref = specific_id.as_deref();

        let active_ops: Vec<Operation> = self
            .operation_monitor
            .get_all_active_operations()
            .await
            .into_iter()
            .filter(|op| common::operation_matches_filters(op, &tool_filters, op_id_ref))
            .collect();

        let completed_ops: Vec<Operation> = self
            .operation_monitor
            .get_completed_operations()
            .await
            .into_iter()
            .filter(|op| common::operation_matches_filters(op, &tool_filters, op_id_ref))
            .collect();

        // Create summary with timing information
        let active_count = active_ops.len();
        let completed_count = completed_ops.len();
        let total_count = active_count + completed_count;

        let summary = if let Some(ref id) = specific_id {
            if total_count == 0 {
                common::unknown_operation_report(id).await
            } else {
                format!("Operation '{}' found", id)
            }
        } else if tool_filters.is_empty() {
            format!(
                "Operations status: {} active, {} completed (total: {})",
                active_count, completed_count, total_count
            )
        } else {
            format!(
                "Operations status for '{}': {} active, {} completed (total: {})",
                tool_filters.join(", "),
                active_count,
                completed_count,
                total_count
            )
        };

        contents.push(ContentBlock::text(summary));

        // SPEC R5.4: the scope is always visible with provenance — here too,
        // the one surface an agent (or a human reading its transcript) reaches
        // without a TUI. Which sandbox is enforcing, what it confines, and
        // every grant in force with who asked for it and for which workspace.
        contents.push(ContentBlock::text(self.sandbox_report()));

        // Add concurrency efficiency analysis
        if !completed_ops.is_empty()
            && let Some(efficiency_analysis) = Self::run_concurrency_analysis(&completed_ops)
        {
            contents.push(ContentBlock::text(format!(
                "\nConcurrency Analysis:\n{}",
                efficiency_analysis
            )));
        }

        if !active_ops.is_empty() {
            contents.push(ContentBlock::text(
                "\n=== ACTIVE OPERATIONS ===".to_string(),
            ));
            contents.extend(common::serialize_operations_to_content(&active_ops));
        }

        if !completed_ops.is_empty() {
            contents.push(ContentBlock::text(
                "\n=== COMPLETED OPERATIONS ===".to_string(),
            ));
            contents.extend(common::serialize_operations_to_content(&completed_ops));
        }

        Ok(CallToolResult::success(contents))
    }

    /// The sandbox section of `status` (SPEC R5.4): enforcement, scope, grants.
    pub fn sandbox_report(&self) -> String {
        let sandbox = self.adapter.sandbox();
        let mut out = String::from("\n=== SANDBOX ===\n");
        out.push_str(&sandbox.active_sandbox().disclosure_line());
        out.push('\n');
        out.push_str(&sandbox.scope_text(sandbox.scope_source()));
        let grants = sandbox.persistent_grants_in_effect();
        if grants.is_empty() {
            out.push_str("  grants: (none in force for this session)\n");
        } else {
            out.push_str("  grants (persistent, in force for this session):\n");
            for g in grants {
                out.push_str(&format!(
                    "    {} ({}) workspace={} by={}\n",
                    g.path.display(),
                    g.access.label(),
                    g.workspace
                        .as_deref()
                        .map(|w| w.display().to_string())
                        .unwrap_or_else(|| "GLOBAL (legacy)".to_string()),
                    g.granted_by.as_deref().unwrap_or("user"),
                ));
            }
        }
        out.push_str(
            "  Only a human can widen this: they approve a prompt in the ahma TUI or run \
             `ahma sandbox grant <dir>`; `sandbox_grant` only asks.\n",
        );
        out
    }

    fn run_concurrency_analysis(completed_ops: &[Operation]) -> Option<String> {
        let mut total_execution_time = 0.0;
        let mut total_wait_time = 0.0;
        let mut operations_with_waits = 0;

        for op in completed_ops {
            if let Some(end_time) = op.end_time
                && let Ok(execution_duration) = end_time.duration_since(op.start_time)
            {
                total_execution_time += execution_duration.as_secs_f64();

                if let Some(first_wait_time) = op.first_wait_time
                    && let Ok(wait_duration) = first_wait_time.duration_since(op.start_time)
                {
                    total_wait_time += wait_duration.as_secs_f64();
                    operations_with_waits += 1;
                }
            }
        }

        if total_execution_time > 0.0 {
            if operations_with_waits > 0 {
                let avg_wait_ratio = (total_wait_time / total_execution_time) * 100.0;
                if avg_wait_ratio < 10.0 {
                    Some(format!(
                        "OK Good concurrency efficiency: {:.1}% of execution time spent waiting",
                        avg_wait_ratio
                    ))
                } else if avg_wait_ratio < 50.0 {
                    Some(format!(
                        "WARNING Moderate concurrency efficiency: {:.1}% of execution time spent waiting",
                        avg_wait_ratio
                    ))
                } else {
                    Some(format!(
                        "WARNING Low concurrency efficiency: {:.1}% of execution time spent waiting. Consider using status tool instead of frequent waits.",
                        avg_wait_ratio
                    ))
                }
            } else {
                Some("OK Excellent concurrency: No blocking waits detected".to_string())
            }
        } else {
            None
        }
    }
}

#[cfg(test)]
mod sandbox_report_tests {
    use crate::test_utils::in_process::build_test_service;

    /// SPEC R5.4: `status` tells the agent which sandbox protects it, what it
    /// confines, and the grants in force — the one scope surface that needs no
    /// TUI.
    #[tokio::test]
    async fn status_tool_reports_sandbox_scopes_and_grants() {
        let (service, scope) = build_test_service().await.unwrap();
        let report = service.sandbox_report();
        assert!(report.contains("=== SANDBOX ==="), "{report}");
        assert!(report.contains("Sandbox:"), "{report}");
        let canon = dunce::canonicalize(scope.path()).unwrap();
        assert!(
            report.contains(&canon.display().to_string()),
            "names the writable scope: {report}"
        );
        assert!(report.contains("grants"), "{report}");
        assert!(report.contains("Only a human can widen this"), "{report}");

        let result = service
            .handle_status(serde_json::Map::new())
            .await
            .expect("status succeeds");
        let text = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<String>();
        assert!(text.contains("=== SANDBOX ==="), "{text}");
    }
}
