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

        // SPEC R2.7.9: who holds a workspace, and whether anything waits,
        // in the same words `ahma queue` prints.
        // `queue_report` reads the lock directory with blocking calls (it is
        // shared with the CLI), so it runs off the async workers.
        let queue = tokio::task::spawn_blocking(|| {
            let lock_dir = crate::adapter::workspace_queue::default_lock_dir();
            crate::adapter::workspace_queue::queue_report(
                lock_dir.as_deref(),
                &crate::sandbox::session_tier::pid_alive,
                ahma_common::session_grants::now_secs(),
            )
            .unwrap_or_else(|cannot_tell| cannot_tell)
        })
        .await
        .unwrap_or_else(|e| format!("Cannot tell who holds a workspace: {e}"));
        contents.push(ContentBlock::text(format!(
            "\n=== WORKSPACE QUEUE ===\n{queue}"
        )));

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
        out.push_str(&lease_forecast(
            &sandbox.persistent_grants_in_effect(),
            ahma_common::config::unix_now(),
        ));
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

/// How far ahead `status` warns about leases (SPEC R-PERM.2.3): long enough
/// to cover an overnight run.
const LEASE_FORECAST_SECS: u64 = 12 * 3_600;

/// The leases in force that end within [`LEASE_FORECAST_SECS`], so an agent
/// about to start a long unattended run can ask the human to renew them once,
/// before it starts, instead of losing access partway through.
fn lease_forecast(grants: &[ahma_common::config::PersistentScope], now: u64) -> String {
    let mut due: Vec<_> = grants
        .iter()
        .filter_map(|g| {
            let at = g.expires_at?;
            (at > now && at - now <= LEASE_FORECAST_SECS).then_some((at - now, g))
        })
        .collect();
    if due.is_empty() {
        return String::new();
    }
    due.sort_by_key(|(left, _)| *left);
    let mut out = String::from(
        "  expiring within 12h (before a long run, ask the human to renew what it needs):\n",
    );
    for (left, g) in due {
        out.push_str(&format!(
            "    {} ({}) ends in {} — ahma sandbox renew {} --for 24h\n",
            g.path.display(),
            g.access.label(),
            ahma_common::config::fmt_lease_duration(left),
            g.path.display()
        ));
    }
    out
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
        // SPEC R2.7.9: the queue's facts reach the agent here too.
        assert!(text.contains("=== WORKSPACE QUEUE ==="), "{text}");
    }

    #[test]
    fn status_forecasts_leases_that_end_within_twelve_hours() {
        use ahma_common::config::{PersistentScope, ScopeAccess};
        let g = |path: &str, expires_at: Option<u64>| PersistentScope {
            path: path.into(),
            access: ScopeAccess::Rw,
            workspace: None,
            granted_by: None,
            granted_at: None,
            note: None,
            expires_at,
        };
        let now = 1_000_000;
        let text = super::lease_forecast(
            &[
                g("/soon", Some(now + 3 * 3_600)),
                g("/later", Some(now + 30 * 3_600)),
                g("/forever", None),
            ],
            now,
        );
        assert!(text.contains("/soon (read+write) ends in 3h"), "{text}");
        assert!(
            text.contains("ahma sandbox renew /soon --for 24h"),
            "{text}"
        );
        assert!(
            !text.contains("/later") && !text.contains("/forever"),
            "{text}"
        );
        assert!(super::lease_forecast(&[g("/forever", None)], now).is_empty());
    }
}
