//! Type definitions for the MCP service.
//!
//! Contains configuration structs and enums used throughout the MCP service.

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RequestContext, RoleServer};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

use ahma_common::daemon_hub::{ClientMsg, DaemonChatMessage};

/// How long a tool call waits for the operation it started before answering
/// (SPEC R2.1). Either way the call is a tracked operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallWait {
    /// The adaptive inline window (SPEC R2.6.1), then an operation id.
    Adaptive,
    /// Until it finishes, bounded by what the client tolerates (the bound a
    /// default `await` gets); past that, an id and a note to `await` it.
    UntilDone,
}

/// Tracks approval sender for the active agent turn.
#[derive(Default)]
pub struct ActiveAgentSession {
    pub approval_tx: Option<tokio::sync::oneshot::Sender<bool>>,
    pub approvals: std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>,
    /// The running turn's task, so a `CancelPrompt` can stop it.
    pub turn: Option<tokio::task::AbortHandle>,
}

/// A trait for executing prompts via the agent loop (implemented in ahma_core).
#[async_trait::async_trait]
pub trait PromptRunner: Send + Sync {
    async fn run_prompt(
        &self,
        messages: Vec<DaemonChatMessage>,
        system_prompt: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        hub_tx: tokio::sync::mpsc::Sender<ClientMsg>,
        session: Arc<tokio::sync::Mutex<ActiveAgentSession>>,
    ) -> Result<(), String>;

    /// Run the agent loop to completion for a **delegated sub-task** (the MCP
    /// `agent` tool) and return the final assistant text. Unlike [`PromptRunner::run_prompt`]
    /// this does not stream to a hub and auto-approves tool calls — a sub-agent
    /// has no interactive surface to ask. `provider`/`model` default to the
    /// model last selected in `ahma tui` (`settings.agent`); `max_turns`
    /// overrides the configured default when set.
    async fn run_prompt_to_completion(
        &self,
        messages: Vec<DaemonChatMessage>,
        system_prompt: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        max_turns: Option<u32>,
    ) -> Result<String, String>;
}

static GLOBAL_PROMPT_RUNNER: std::sync::OnceLock<Arc<dyn PromptRunner>> =
    std::sync::OnceLock::new();

/// Register the global prompt runner.
pub fn register_global_prompt_runner(runner: Arc<dyn PromptRunner>) {
    let _ = GLOBAL_PROMPT_RUNNER.set(runner);
}

/// Retrieve the global prompt runner.
pub fn get_global_prompt_runner() -> Option<Arc<dyn PromptRunner>> {
    GLOBAL_PROMPT_RUNNER.get().cloned()
}

use crate::config::ToolConfig;

/// Distinguishes between top-level sequence tools and subcommand sequences.
/// Used by the unified sequence execution logic to handle differences in
/// tool/config lookup and message formatting.
#[derive(Clone)]
pub enum SequenceKind {
    /// Top-level sequence: each step specifies a different tool (e.g., `test_sequence`)
    TopLevel,
    /// Subcommand sequence: all steps use the same base tool config (e.g., `cargo qualitycheck`)
    Subcommand,
}

/// Represents the structure of the guidance JSON file.
///
/// Only `guidance_blocks` is consumed; unknown keys in a guidance file
/// (e.g. the retired `templates`/`legacy_guidance` sections) are ignored by
/// serde on deserialize.
#[derive(Deserialize, Debug, Clone)]
pub struct GuidanceConfig {
    pub guidance_blocks: HashMap<String, String>,
}

impl Default for GuidanceConfig {
    fn default() -> Self {
        let mut guidance_blocks = HashMap::new();
        guidance_blocks.insert(
            "async_behavior".to_string(),
            "**IMPORTANT:** This tool operates asynchronously\n1. **Immediate Response:** Returns id and status 'started'. This is NOT YET success\n2. **Final Result:** Result pushed automatically via MCP notification when complete\n\n**Your Instructions:**\n- DO NOT await for the final result unless you are at end of all tasks and have already updated the user with 'assume success but verify' results.\n- **DO** continue with other tasks that don't depend on this operation\n- You **MUST** process the future result notification to know if operation succeeded".to_string(),
        );
        guidance_blocks.insert(
            "sync_behavior".to_string(),
            "This tool runs synchronously and returns results immediately".to_string(),
        );
        guidance_blocks.insert(
            "coordination_tool".to_string(),
            "**WARNING:** This is a blocking coordination tool. Use ONLY for final project validation when no other productive work remains.".to_string(),
        );
        guidance_blocks.insert(
            "python_async".to_string(),
            "**IMPORTANT:** This tool operates asynchronously.\n1. **Immediate Response:** Returns id and status 'started'. NOT success.\n2. **Final Result:** Result pushed automatically via MCP notification when complete.\n\n**Your Instructions:**\n- DO NOT await for the final result.\n- **DO** continue with other tasks that don't depend on this operation.\n- You **MUST** process the future result notification to know if operation succeeded.".to_string(),
        );
        guidance_blocks.insert(
            "python_sync".to_string(),
            "This tool runs synchronously and returns results immediately.".to_string(),
        );
        guidance_blocks.insert(
            "git_operations".to_string(),
            "This tool runs synchronously and returns results immediately.".to_string(),
        );
        guidance_blocks.insert(
            "cancellation_restart_hint".to_string(),
            "An operation was cancelled. Include the cancellation reason back to the user, and suggest a tool hint to restart or check status: 1) Call 'status' with the id to confirm state; 2) If appropriate, restart the tool with the same parameters; 3) Consider 'await' only when results are actually needed.".to_string(),
        );

        Self { guidance_blocks }
    }
}

/// Meta-parameters that control execution environment but should not be passed as CLI args
pub const META_PARAMS: &[&str] = &["working_directory", "execution_mode", "timeout_seconds"];

/// A trait for executing extension tool types implemented in separate sibling crates.
#[async_trait::async_trait]
pub trait ExtensionToolHandler: Send + Sync {
    async fn call(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
        config: ToolConfig,
        adapter: Arc<crate::adapter::Adapter>,
        operation_monitor: Arc<crate::operation_monitor::OperationMonitor>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData>;
}

static GLOBAL_EXTENSION_HANDLERS: std::sync::LazyLock<
    parking_lot::RwLock<HashMap<String, Arc<dyn ExtensionToolHandler>>>,
> = std::sync::LazyLock::new(|| parking_lot::RwLock::new(HashMap::new()));

/// Register an extension handler globally so that new MCP service instances can retrieve it.
pub fn register_global_extension_handler(name: String, handler: Arc<dyn ExtensionToolHandler>) {
    GLOBAL_EXTENSION_HANDLERS.write().insert(name, handler);
}

/// Retrieve the global map of extension handlers.
pub fn get_global_extension_handlers()
-> &'static parking_lot::RwLock<HashMap<String, Arc<dyn ExtensionToolHandler>>> {
    &GLOBAL_EXTENSION_HANDLERS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guidance_config_default_contains_expected_blocks() {
        let cfg = GuidanceConfig::default();
        assert!(cfg.guidance_blocks.contains_key("async_behavior"));
        assert!(cfg.guidance_blocks.contains_key("sync_behavior"));
        assert!(cfg.guidance_blocks.contains_key("coordination_tool"));
    }

    #[test]
    fn test_meta_params_contains_expected_values() {
        assert!(META_PARAMS.contains(&"working_directory"));
        assert!(META_PARAMS.contains(&"execution_mode"));
        assert!(META_PARAMS.contains(&"timeout_seconds"));
        assert_eq!(META_PARAMS.len(), 3);
    }

    #[test]
    fn test_sequence_kind_toplevel() {
        let kind = SequenceKind::TopLevel;
        match kind {
            SequenceKind::TopLevel => {} // Expected
            _ => panic!("Expected TopLevel variant"),
        }
    }

    #[test]
    fn test_guidance_config_deserialize_minimal() {
        let json = r#"{"guidance_blocks": {"test": "value"}}"#;
        let config: GuidanceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.guidance_blocks.get("test"),
            Some(&"value".to_string())
        );
    }

    /// Guidance files written for older versions may still carry the retired
    /// `templates`/`legacy_guidance` sections — serde must ignore them.
    #[test]
    fn test_guidance_config_deserialize_ignores_retired_sections() {
        let json = r#"{
            "guidance_blocks": {"tool1": "guidance1"},
            "templates": {"tmpl1": "template_value"},
            "legacy_guidance": {
                "general_guidance": {"key1": "val1"},
                "tool_specific_guidance": {"tool1": {"key2": "val2"}}
            }
        }"#;
        let config: GuidanceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.guidance_blocks.len(), 1);
    }
}
