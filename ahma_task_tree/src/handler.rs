use async_trait::async_trait;
use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, CallToolResult, Content, ErrorData as McpError};
use rmcp::service::{RequestContext, RoleServer};

use ahma_mcp::Adapter;
use ahma_mcp::ExtensionToolHandler;
use ahma_mcp::config::ToolConfig;

use crate::config::TaskTreeConfig;
use crate::orchestrator::TaskTreeOrchestrator;

pub struct TaskTreeExtensionHandler;

#[async_trait]
impl ExtensionToolHandler for TaskTreeExtensionHandler {
    async fn call(
        &self,
        params: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
        config: ToolConfig,
        adapter: Arc<Adapter>,
        _operation_monitor: Arc<ahma_mcp::operation_monitor::OperationMonitor>,
    ) -> Result<CallToolResult, McpError> {
        let tree_config = if let Some(ref val) = config.task_tree {
            serde_json::from_value::<TaskTreeConfig>(val.clone()).map_err(|e| {
                McpError::invalid_params(format!("Invalid task tree config: {}", e), None)
            })?
        } else {
            TaskTreeConfig::default()
        };

        let arguments = params
            .arguments
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("Missing arguments payload", None))?;

        let goal = arguments
            .get("goal")
            .or_else(|| arguments.get("query"))
            .or_else(|| arguments.get("instructions"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                McpError::invalid_params(
                    "Missing required parameter: 'goal', 'query', or 'instructions'",
                    None,
                )
            })?;

        let orchestrator = TaskTreeOrchestrator::new(tree_config, adapter).map_err(|e| {
            McpError::internal_error(
                format!("Failed to initialize task tree orchestrator: {}", e),
                None,
            )
        })?;

        let node_result = orchestrator.execute(goal).await.map_err(|e| {
            McpError::internal_error(format!("Task tree execution failed: {}", e), None)
        })?;

        let content_text = if node_result.success {
            format!(
                "Goal: {}\nResult: SUCCESS\nSummary:\n{}",
                goal, node_result.summary
            )
        } else {
            format!(
                "Goal: {}\nResult: FAILED\nSummary:\n{}\nStdout:\n{}\nStderr:\n{}",
                goal, node_result.summary, node_result.stdout, node_result.stderr
            )
        };

        Ok(CallToolResult::success(vec![Content::text(content_text)]))
    }
}
