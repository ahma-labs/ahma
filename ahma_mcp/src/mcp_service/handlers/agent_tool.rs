//! The `agent` MCP tool: delegate a task to ahma's own agent loop as a
//! sub-agent. Another model (or an MCP client) calls this to hand ahma a
//! self-contained task; ahma runs its full tool-using loop with the model the
//! user last selected in `ahma tui` and returns the final answer.

use super::common::{mcp_internal, mcp_invalid_params, text_result};
use crate::AhmaMcpService;
use crate::mcp_service::schema;
use ahma_common::daemon_hub::DaemonChatMessage;
use rmcp::model::{CallToolResult, Content, ErrorData as McpError};
use serde_json::{Map, Value, json};
use std::sync::Arc;

impl AhmaMcpService {
    pub async fn handle_agent(&self, args: Map<String, Value>) -> Result<CallToolResult, McpError> {
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| mcp_invalid_params("'prompt' (non-empty string) is required"))?;

        let system_prompt = args
            .get("system_prompt")
            .and_then(Value::as_str)
            .map(str::to_string);
        let model = args
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        let provider = args
            .get("provider")
            .and_then(Value::as_str)
            .map(str::to_string);
        let max_turns = args
            .get("max_turns")
            .and_then(Value::as_u64)
            .map(|n| n as u32);

        let runner = crate::get_global_prompt_runner().ok_or_else(|| {
            mcp_internal("no agent runtime is registered in this process; the `agent` tool is unavailable here")
        })?;

        let messages = vec![DaemonChatMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
        }];

        match runner
            .run_prompt_to_completion(messages, system_prompt, provider, model, max_turns)
            .await
        {
            Ok(text) if text.trim().is_empty() => Ok(text_result(
                "The sub-agent finished without producing a textual answer.",
            )),
            Ok(text) => Ok(text_result(text)),
            // A failed delegation is a tool-level error, not a protocol error, so
            // the calling model can read it and adapt.
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "agent sub-task failed: {e}"
            ))])),
        }
    }
}

/// Input schema for the `agent` tool.
pub fn agent_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "prompt".to_string(),
        json!({
            "type": "string",
            "description": "The self-contained task or question to delegate to the ahma sub-agent."
        }),
    );
    props.insert(
        "system_prompt".to_string(),
        json!({
            "type": "string",
            "description": "Optional system-prompt override. Defaults to ahma's editable agent prompt."
        }),
    );
    props.insert(
        "model".to_string(),
        json!({
            "type": "string",
            "description": "Optional model id. Defaults to the model last selected in ahma tui."
        }),
    );
    props.insert(
        "provider".to_string(),
        json!({
            "type": "string",
            "description": "Optional provider name or base URL. Defaults to the last-selected provider."
        }),
    );
    props.insert(
        "max_turns".to_string(),
        json!({
            "type": "integer",
            "minimum": 1,
            "description": "Optional cap on the sub-agent's tool-call turns. Defaults to tools.max_turns."
        }),
    );
    schema::object_input_schema(props, &["prompt"])
}
