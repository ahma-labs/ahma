use super::super::NEXT_ID;
use super::common;
use crate::AhmaMcpService;
use crate::callback_system::CallbackSender;
use crate::client_type::McpClientType;
use crate::mcp_callback::McpCallbackSender;
use crate::mcp_service::schema;
use crate::shell_pool::platform_shell_program;
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, ErrorData as McpError},
    service::{RequestContext, RoleServer},
};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing;

impl AhmaMcpService {
    fn command_has_shell_metacharacters(command: &str) -> bool {
        command
            .chars()
            .any(|c| matches!(c, '|' | ';' | '&' | '>' | '<' | '`' | '$' | '\n' | '\r'))
    }

    fn parse_rm_targets_from_command(command: &str) -> Option<Vec<String>> {
        if Self::command_has_shell_metacharacters(command) {
            return None;
        }

        let tokens: Vec<&str> = command.split_whitespace().collect();
        let first = tokens.first().copied().unwrap_or_default();
        if first != "rm" {
            return None;
        }

        let targets: Vec<String> = tokens
            .iter()
            .skip(1)
            .filter(|t| !t.starts_with('-'))
            .map(|t| (*t).to_string())
            .collect();

        (!targets.is_empty()).then_some(targets)
    }

    async fn maybe_stage_run_terminal_rm(
        &self,
        command: &str,
        working_directory: &str,
    ) -> Result<Option<CallToolResult>, McpError> {
        if self.task_vault_root().is_none() {
            return Ok(None);
        }

        let Some(targets) = Self::parse_rm_targets_from_command(command) else {
            return Ok(None);
        };

        let trash_dir = self
            .task_vault_trash_dir()
            .ok_or_else(|| common::mcp_internal("Task vault trash directory not configured"))?;

        let staged = crate::vault::rm_interceptor::RmInterceptor::stage_paths_into_vault_trash(
            &trash_dir,
            working_directory,
            &targets,
        )
        .map_err(|e| {
            common::mcp_internal(format!("Failed to stage deletion to vault trash: {}", e))
        })?;

        for (original_path, trash_path) in &staged {
            self.emit_vault_file_staged(original_path, trash_path).await;
        }

        Ok(Some(common::text_result(format!(
            "Staged {} path(s) into vault trash instead of permanent delete.",
            staged.len()
        ))))
    }

    /// Generates the specific input schema for the `run_terminal_command` tool.
    pub fn generate_input_schema_for_run_terminal_command(&self) -> Arc<Map<String, Value>> {
        let mut properties = Map::new();
        properties.insert(
            "command".to_string(),
            schema::string_property(
                "The shell command to execute (supports pipes, redirects, variables, etc.)",
            ),
        );
        properties.insert(
            "working_directory".to_string(),
            schema::path_property("Working directory for command execution"),
        );
        properties.insert(
            "monitor_level".to_string(),
            schema::enum_string_property(
                "Enable live log monitoring at this severity level. When set, stderr/stdout is streamed line-by-line and alerts are pushed when error/warning patterns are detected. Values: error, warn, info, debug, trace",
                &["error", "warn", "info", "debug", "trace"],
            ),
        );
        properties.insert(
            "monitor_stream".to_string(),
            schema::enum_string_property_with_default(
                "Which stream to monitor for log patterns (default: stderr). Use 'stdout' for tools like adb logcat that write logs to stdout.",
                &["stderr", "stdout", "both"],
                "stderr",
            ),
        );
        schema::object_input_schema(properties, &["command"])
    }

    /// Handles the 'run_terminal_command' built-in tool call.
    pub async fn handle_run_terminal_command(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args = params.arguments.unwrap_or_default();

        // Delay tool execution until sandbox is initialized from roots/list.
        // This is critical in HTTP bridge mode with deferred sandbox initialization.
        if !self.adapter.sandbox().is_ready_for_tool_calls() {
            let error_message = "Sandbox initializing from client roots - retry tools/call after roots/list completes".to_string();
            tracing::warn!("{}", error_message);
            return Err(common::mcp_internal(error_message));
        }

        // Extract command (required)
        let command = common::require_str(&args, "command", "command parameter is required")?;

        // Extract working_directory (optional)
        let working_directory = common::opt_str(&args, "working_directory")
            .or_else(|| {
                if self.adapter.sandbox().is_test_mode() {
                    None
                } else {
                    self.adapter
                        .sandbox()
                        .scopes()
                        .first()
                        .map(|p: &std::path::PathBuf| p.to_string_lossy().to_string())
                }
            })
            .unwrap_or_else(|| ".".to_string());

        let timeout = args.get("timeout_seconds").and_then(|v| v.as_u64());

        if let Some(staged) = self
            .maybe_stage_run_terminal_rm(&command, &working_directory)
            .await?
        {
            return Ok(staged);
        }

        // Extract optional log monitoring parameters
        let log_monitor_config = common::opt_str(&args, "monitor_level").map(|level_str| {
            let monitor_level: crate::log_monitor::LogLevel = level_str
                .parse()
                .unwrap_or(crate::log_monitor::LogLevel::Error);
            let monitor_stream: crate::log_monitor::MonitorStream =
                common::opt_str(&args, "monitor_stream")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_default();
            crate::log_monitor::LogMonitorConfig {
                monitor_level,
                monitor_stream,
                rate_limit_seconds: self.monitor_rate_limit_seconds,
            }
        });

        // Determine execution mode
        let execution_mode = if self.force_synchronous {
            crate::adapter::ExecutionMode::Synchronous
        } else if let Some(mode_str) = common::opt_str(&args, "execution_mode") {
            match mode_str.as_str() {
                "Synchronous" => crate::adapter::ExecutionMode::Synchronous,
                "AsyncResultPush" => crate::adapter::ExecutionMode::AsyncResultPush,
                _ => crate::adapter::ExecutionMode::AsyncResultPush,
            }
        } else {
            crate::adapter::ExecutionMode::AsyncResultPush
        };

        // Build arguments map for adapter
        let mut adapter_args = Map::new();
        adapter_args.insert("command".to_string(), serde_json::Value::String(command));
        if let Some(wd) = args.get("working_directory") {
            adapter_args.insert("working_directory".to_string(), wd.clone());
        }

        let subcommand_config = Self::build_shell_subcommand_config(timeout, &execution_mode);

        adapter_args.insert("c_flag".to_string(), serde_json::Value::Bool(true));

        match execution_mode {
            crate::adapter::ExecutionMode::Synchronous => {
                self.execute_shell_sync(
                    adapter_args,
                    &working_directory,
                    timeout,
                    &subcommand_config,
                    &context,
                )
                .await
            }
            crate::adapter::ExecutionMode::AsyncResultPush => {
                self.execute_shell_async(
                    adapter_args,
                    &working_directory,
                    timeout,
                    &subcommand_config,
                    &context,
                    log_monitor_config,
                )
                .await
            }
        }
    }

    #[allow(deprecated)]
    pub fn build_shell_subcommand_config(
        timeout: Option<u64>,
        execution_mode: &crate::adapter::ExecutionMode,
    ) -> crate::config::SubcommandConfig {
        crate::config::SubcommandConfig {
            name: "run_terminal_command".to_string(),
            description: "Execute shell commands".to_string(),
            subcommand: None,
            options: Some(vec![crate::config::CommandOption {
                name: "c_flag".to_string(),
                option_type: "boolean".to_string(),
                description: Some("Execute command string".to_string()),
                required: Some(false),
                format: None,
                items: None,
                file_arg: None,
                file_flag: None,
                alias: Some("c".to_string()),
            }]),
            positional_args: Some(vec![crate::config::CommandOption {
                name: "command".to_string(),
                option_type: "string".to_string(),
                description: Some("Shell command to execute".to_string()),
                required: Some(true),
                format: None,
                items: None,
                file_arg: None,
                file_flag: None,
                alias: None,
            }]),
            positional_args_first: Some(false),
            timeout_seconds: timeout,
            synchronous: Some(matches!(
                execution_mode,
                crate::adapter::ExecutionMode::Synchronous
            )),
            enabled: true,
            guidance_key: None,
            sequence: None,
            step_delay_ms: None,
            availability_check: None,
            install_instructions: None,
        }
    }

    pub async fn execute_shell_sync(
        &self,
        adapter_args: Map<String, Value>,
        working_directory: &str,
        timeout: Option<u64>,
        subcommand_config: &crate::config::SubcommandConfig,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let cmd_str = adapter_args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let counter_val = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let id = crate::utils::operation::generate_id_with_details(counter_val, "run_terminal_command", cmd_str);
        let started_at = std::time::Instant::now();
        self.emit_vault_tool_call(
            &id,
            "run_terminal_command",
            &format!(
                "{{\"working_directory\":\"{}\",\"command\":\"{}\"}}",
                working_directory,
                adapter_args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
        )
        .await;

        let progress_token = context.meta.get_progress_token();
        let client_type = McpClientType::from_peer(&context.peer);
        let description = format!(
            "Execute {} in {}",
            platform_shell_program(),
            working_directory
        );

        if let Some(token) = progress_token.clone() {
            let callback =
                McpCallbackSender::new(context.peer.clone(), id.clone(), Some(token), client_type);
            let _ = callback
                .send_progress(crate::callback_system::ProgressUpdate::Started {
                    id: id.clone(),
                    command: platform_shell_program().to_string(),
                    description: description.clone(),
                })
                .await;
        }

        let result = self
            .adapter
            .execute_sync_in_dir(
                platform_shell_program(),
                Some(adapter_args),
                working_directory,
                timeout,
                Some(subcommand_config),
            )
            .await;

        let duration_ms = started_at.elapsed().as_millis() as u64;
        self.emit_vault_tool_complete(&id, result.is_ok(), duration_ms)
            .await;

        if let Some(token) = progress_token {
            let callback =
                McpCallbackSender::new(context.peer.clone(), id.clone(), Some(token), client_type);
            let (success, full_output) = match &result {
                Ok(output) => (true, output.clone()),
                Err(e) => (false, format!("Error: {}", e)),
            };
            let _ = callback
                .send_progress(crate::callback_system::ProgressUpdate::FinalResult {
                    id: id.clone(),
                    command: platform_shell_program().to_string(),
                    description,
                    working_directory: working_directory.to_string(),
                    success,
                    duration_ms: 0,
                    full_output,
                })
                .await;
        }

        match result {
            Ok(output) => Ok(common::text_result(output)),
            Err(e) => {
                let error_message = format!("Synchronous execution failed: {}", e);
                tracing::error!("{}", error_message);
                Err(common::mcp_internal(error_message))
            }
        }
    }

    pub async fn execute_shell_async(
        &self,
        adapter_args: Map<String, Value>,
        working_directory: &str,
        timeout: Option<u64>,
        subcommand_config: &crate::config::SubcommandConfig,
        context: &RequestContext<RoleServer>,
        log_monitor_config: Option<crate::log_monitor::LogMonitorConfig>,
    ) -> Result<CallToolResult, McpError> {
        let cmd_str = adapter_args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let counter_val = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let id = crate::utils::operation::generate_id_with_details(counter_val, "run_terminal_command", cmd_str);
        self.emit_vault_tool_call(
            &id,
            "run_terminal_command",
            &format!(
                "{{\"working_directory\":\"{}\",\"command\":\"{}\"}}",
                working_directory,
                adapter_args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            ),
        )
        .await;

        let progress_token = context.meta.get_progress_token();
        let client_type = McpClientType::from_peer(&context.peer);
        let callback: Option<Box<dyn CallbackSender>> = progress_token.map(|token| {
            Box::new(McpCallbackSender::new(
                context.peer.clone(),
                id.clone(),
                Some(token),
                client_type,
            )) as Box<dyn CallbackSender>
        });
        let callback = self.wrap_callback_with_vault_audit(callback, &id);

        let job_id = self
            .adapter
            .execute_async_in_dir_with_options(
                "run_terminal_command",
                platform_shell_program(),
                working_directory,
                crate::adapter::AsyncExecOptions {
                    id: Some(id.clone()),
                    args: Some(adapter_args),
                    timeout,
                    callback,
                    subcommand_config: Some(subcommand_config),
                    log_monitor_config,
                },
            )
            .await;

        match job_id {
            Ok(id) => {
                // Automatic async: wait briefly for fast commands to complete
                if let Some(result) =
                    common::try_automatic_async_completion(&self.operation_monitor, &id).await
                {
                    return Ok(result);
                }

                let hint = crate::tool_hints::preview(&id, "run_terminal_command");
                let message = format!("AHMA ID: {}{}", id, hint);
                Ok(common::text_result(message))
            }
            Err(e) => {
                self.emit_vault_tool_complete(&id, false, 0).await;
                let error_message = format!("Async execution failed: {}", e);
                tracing::error!("{}", error_message);
                Err(common::mcp_internal(error_message))
            }
        }
    }
}
