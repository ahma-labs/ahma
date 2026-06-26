use super::super::NEXT_ID;
use super::common;
use crate::AhmaMcpService;
use crate::client_type::McpClientType;
use crate::mcp_service::progress_push;
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
        properties.insert(
            "session_id".to_string(),
            schema::string_property(
                "Run the command in a persistent shell session with this id. Commands sharing a session_id run sequentially in the SAME shell, so `cd`, exported variables, and sourced environments (e.g. virtualenvs) persist between calls. The session is created on first use.",
            ),
        );
        properties.insert(
            "pty".to_string(),
            schema::boolean_property(
                "Run the command attached to a pseudo-terminal (Unix only). Use for tools that require a TTY or change behaviour without one (colours, progress bars, interactive prompts). stdout and stderr are merged.",
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

        // Dedicated execution paths: persistent shell session / PTY.
        let session_id = common::opt_str(&args, "session_id");
        let use_pty = args.get("pty").and_then(|v| v.as_bool()).unwrap_or(false);
        if session_id.is_some() || use_pty {
            return self
                .execute_shell_special(
                    &command,
                    &working_directory,
                    timeout,
                    session_id.as_deref(),
                    use_pty,
                    &context,
                )
                .await;
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
            extra: Default::default(),
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
        let cmd_str = adapter_args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let counter_val = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let id = crate::utils::operation::generate_id_with_details(
            counter_val,
            "run_terminal_command",
            cmd_str,
        );
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

        // Sync operations never enter the OperationMonitor, so the event
        // forwarder cannot see them — push start/final progress directly.
        let push_enabled = progress_token.is_some() && client_type.supports_progress();
        if let Some(token) = progress_token.clone()
            && push_enabled
        {
            progress_push::push_progress(
                &context.peer,
                token,
                0.0,
                format!("{}: {}", platform_shell_program(), description),
                false,
            )
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

        if let Some(token) = progress_token
            && push_enabled
        {
            let (success, full_output) = match &result {
                Ok(output) => (true, output.clone()),
                Err(e) => (false, format!("Error: {}", e)),
            };
            let message = progress_push::sync_final_message(
                &id,
                platform_shell_program(),
                &description,
                working_directory,
                success,
                duration_ms,
                &full_output,
            );
            progress_push::push_progress(&context.peer, token, 100.0, message, true).await;
        }

        match result {
            Ok(output) => Ok(common::text_result(output)),
            // On an out-of-scope path access this attaches a machine-readable
            // `sandbox_denial` payload to the error's `data` field; otherwise a
            // plain internal error. (Logs the failure internally.)
            Err(e) => Err(common::execution_error(&e)),
        }
    }

    /// Execute `run_terminal_command` with a `session_id` and/or `pty: true`.
    ///
    /// Both paths run asynchronously through the standard operation lifecycle
    /// (operation id returned immediately; results via `await`/`status` and
    /// the unified event stream).  When both are requested, the session wins:
    /// PTY-in-session is not supported yet.
    async fn execute_shell_special(
        &self,
        command: &str,
        working_directory: &str,
        timeout: Option<u64>,
        session_id: Option<&str>,
        use_pty: bool,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let counter_val = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let id = crate::utils::operation::generate_id_with_details(
            counter_val,
            "run_terminal_command",
            command,
        );
        self.emit_vault_tool_call(
            &id,
            "run_terminal_command",
            &format!(
                "{{\"working_directory\":\"{}\",\"command\":\"{}\",\"session_id\":{:?},\"pty\":{}}}",
                working_directory, command, session_id, use_pty
            ),
        )
        .await;

        let progress_token = context.meta.get_progress_token();
        let client_type = McpClientType::from_peer(&context.peer);
        if let Some(token) = progress_token {
            self.progress_push
                .register(&id, context.peer.clone(), token, client_type)
                .await;
        }

        let started = match session_id {
            Some(session) => {
                if use_pty {
                    tracing::warn!(
                        "run_terminal_command: pty=true ignored — PTY inside a session is not supported"
                    );
                }
                self.adapter
                    .execute_session_async(
                        "run_terminal_command",
                        session,
                        command,
                        working_directory,
                        timeout,
                        Some(id.clone()),
                    )
                    .await
            }
            None => {
                self.adapter
                    .execute_pty_async(
                        "run_terminal_command",
                        command,
                        working_directory,
                        timeout,
                        Some(id.clone()),
                    )
                    .await
            }
        };

        match started {
            Ok(op_id) => {
                if let Some(result) =
                    common::try_automatic_async_completion(&self.operation_monitor, &op_id).await
                {
                    return Ok(result);
                }
                let hint = crate::tool_hints::preview(&op_id, "run_terminal_command");
                Ok(common::text_result(format!("AHMA ID: {}{}", op_id, hint)))
            }
            Err(e) => {
                self.progress_push.unregister(&id).await;
                self.emit_vault_tool_complete(&id, false, 0).await;
                let error_message = format!("Execution failed: {}", e);
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
        let cmd_str = adapter_args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let counter_val = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let id = crate::utils::operation::generate_id_with_details(
            counter_val,
            "run_terminal_command",
            cmd_str,
        );
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
        if let Some(token) = progress_token {
            self.progress_push
                .register(&id, context.peer.clone(), token, client_type)
                .await;
        }

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
                // The operation never started, so no terminal event will
                // arrive to clean up the push registration.
                self.progress_push.unregister(&id).await;
                self.emit_vault_tool_complete(&id, false, 0).await;
                let error_message = format!("Async execution failed: {}", e);
                tracing::error!("{}", error_message);
                Err(common::mcp_internal(error_message))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::AhmaMcpService;
    use crate::adapter::ExecutionMode;
    use crate::shell::cli::AppConfig;
    use crate::test_utils::in_process::{build_test_service, create_in_process_mcp_empty};
    use rmcp::model::{CallToolRequestParams, CallToolResult};
    use serde_json::json;
    use std::borrow::Cow;
    use std::sync::Arc;
    use std::time::Duration;

    /// Concatenate all text content of a `CallToolResult` into one String.
    fn result_text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect()
    }

    /// Call `run_terminal_command` over the in-process MCP wire with a real
    /// `RequestContext` constructed by the server, returning the result.
    async fn call_run_terminal(
        args: serde_json::Value,
    ) -> Result<CallToolResult, rmcp::ServiceError> {
        let mcp = create_in_process_mcp_empty()
            .await
            .expect("in-process MCP pair must build");
        let params = CallToolRequestParams::new(Cow::Borrowed("run_terminal_command"))
            .with_arguments(args.as_object().unwrap().clone());
        let out = tokio::time::timeout(Duration::from_secs(30), mcp.client.call_tool(params))
            .await
            .expect("call_tool must not time out");
        let _ = mcp.client.cancel().await;
        out
    }

    // ── generate_input_schema_for_run_terminal_command ───────────────────────

    #[tokio::test]
    async fn generate_input_schema_has_expected_properties_and_required() {
        let (service, _temp) = build_test_service().await.unwrap();
        let schema = service.generate_input_schema_for_run_terminal_command();

        assert_eq!(
            schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "schema type must be object"
        );
        let props = schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("schema must have properties");
        for key in [
            "command",
            "working_directory",
            "monitor_level",
            "monitor_stream",
            "session_id",
            "pty",
        ] {
            assert!(props.contains_key(key), "missing property: {key}");
        }
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("schema must have required array");
        assert!(
            required.iter().any(|v| v.as_str() == Some("command")),
            "command must be required"
        );
    }

    // ── build_shell_subcommand_config ────────────────────────────────────────

    #[test]
    fn build_shell_subcommand_config_synchronous_sets_true() {
        let cfg =
            AhmaMcpService::build_shell_subcommand_config(Some(42), &ExecutionMode::Synchronous);
        assert_eq!(cfg.name, "run_terminal_command");
        assert_eq!(cfg.synchronous, Some(true));
        assert_eq!(cfg.timeout_seconds, Some(42));
        assert_eq!(cfg.positional_args_first, Some(false));
        assert!(cfg.enabled);

        let positionals = cfg.positional_args.expect("positional args present");
        assert_eq!(positionals.len(), 1);
        assert_eq!(positionals[0].name, "command");
        assert_eq!(positionals[0].required, Some(true));

        let options = cfg.options.expect("options present");
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].name, "c_flag");
        assert_eq!(options[0].alias.as_deref(), Some("c"));
    }

    #[test]
    fn build_shell_subcommand_config_async_sets_false() {
        let cfg =
            AhmaMcpService::build_shell_subcommand_config(None, &ExecutionMode::AsyncResultPush);
        assert_eq!(cfg.synchronous, Some(false));
        assert_eq!(cfg.timeout_seconds, None);
    }

    // ── command_has_shell_metacharacters (private, in-module access) ─────────

    #[test]
    fn metacharacters_detected_for_each_special_char() {
        for cmd in [
            "echo a | b",
            "echo a; b",
            "echo a & b",
            "echo a > b",
            "echo a < b",
            "echo `whoami`",
            "echo $HOME",
            "echo a\nb",
            "echo a\rb",
        ] {
            assert!(
                AhmaMcpService::command_has_shell_metacharacters(cmd),
                "expected metachar detection for: {cmd:?}"
            );
        }
    }

    #[test]
    fn metacharacters_absent_for_plain_command() {
        assert!(!AhmaMcpService::command_has_shell_metacharacters(
            "rm foo bar"
        ));
    }

    // ── parse_rm_targets_from_command (private, in-module access) ─────────────

    #[test]
    fn parse_rm_targets_plain_two_targets() {
        let targets = AhmaMcpService::parse_rm_targets_from_command("rm foo bar");
        assert_eq!(targets, Some(vec!["foo".to_string(), "bar".to_string()]));
    }

    #[test]
    fn parse_rm_targets_filters_flags() {
        let targets = AhmaMcpService::parse_rm_targets_from_command("rm -rf foo");
        assert_eq!(targets, Some(vec!["foo".to_string()]));
    }

    #[test]
    fn parse_rm_targets_non_rm_is_none() {
        assert_eq!(
            AhmaMcpService::parse_rm_targets_from_command("ls foo"),
            None
        );
    }

    #[test]
    fn parse_rm_targets_metachar_is_none() {
        assert_eq!(
            AhmaMcpService::parse_rm_targets_from_command("rm | foo"),
            None
        );
    }

    #[test]
    fn parse_rm_targets_no_targets_is_none() {
        assert_eq!(AhmaMcpService::parse_rm_targets_from_command("rm"), None);
        assert_eq!(
            AhmaMcpService::parse_rm_targets_from_command("rm -rf"),
            None
        );
    }

    // ── maybe_stage_run_terminal_rm (private, in-module access) ───────────────

    #[tokio::test]
    async fn maybe_stage_returns_none_without_task_vault() {
        let (service, temp) = build_test_service().await.unwrap();
        let wd = temp.path().to_string_lossy().to_string();
        let result = service
            .maybe_stage_run_terminal_rm("rm foo", &wd)
            .await
            .expect("must not error");
        assert!(
            result.is_none(),
            "no task vault configured => no staging => Ok(None)"
        );
    }

    #[tokio::test]
    async fn maybe_stage_moves_target_into_vault_trash() {
        let (service, temp) = build_test_service().await.unwrap();

        // Configure a task vault so task_vault_root() becomes Some.
        let vault_dir = temp.path().join("vault");
        std::fs::create_dir_all(&vault_dir).unwrap();
        service.set_app_config(Arc::new(AppConfig {
            task_vault: Some(vault_dir.clone()),
            ..AppConfig::default()
        }));

        // Working directory with a victim file to be staged.
        let work_dir = temp.path().join("work");
        std::fs::create_dir_all(&work_dir).unwrap();
        let victim = work_dir.join("victim.txt");
        std::fs::write(&victim, b"bye").unwrap();

        let result = service
            .maybe_stage_run_terminal_rm("rm victim.txt", &work_dir.to_string_lossy())
            .await
            .expect("staging must not error")
            .expect("staging path must return Some(result)");

        let text = result_text(&result);
        assert!(
            text.contains("Staged 1 path"),
            "expected staged message, got: {text:?}"
        );
        assert!(
            !victim.exists(),
            "victim file must be moved out of work dir"
        );
        let trash = vault_dir.join("trash");
        let staged_count = std::fs::read_dir(&trash).map(|rd| rd.count()).unwrap_or(0);
        assert_eq!(staged_count, 1, "exactly one file staged into vault trash");
    }

    #[tokio::test]
    async fn maybe_stage_non_rm_command_returns_none_even_with_vault() {
        let (service, temp) = build_test_service().await.unwrap();
        let vault_dir = temp.path().join("vault");
        std::fs::create_dir_all(&vault_dir).unwrap();
        service.set_app_config(Arc::new(AppConfig {
            task_vault: Some(vault_dir),
            ..AppConfig::default()
        }));
        let wd = temp.path().to_string_lossy().to_string();
        let result = service
            .maybe_stage_run_terminal_rm("ls foo", &wd)
            .await
            .expect("must not error");
        assert!(result.is_none(), "non-rm command must not stage");
    }

    // ── handle_run_terminal_command via the in-process MCP wire ──────────────
    // These exercise the async handlers with a real server-built RequestContext.

    #[tokio::test]
    async fn handle_default_async_echo_succeeds() {
        let result = call_run_terminal(json!({"command": "echo hello_default"}))
            .await
            .expect("default async run_terminal_command must return Ok");
        assert!(
            !result.is_error.unwrap_or(false),
            "echo should not be an error result"
        );
        let text = result_text(&result);
        assert!(
            text.contains("hello_default") || text.contains("AHMA ID:"),
            "expected echoed output or an AHMA ID, got: {text:?}"
        );
    }

    #[tokio::test]
    async fn handle_synchronous_execution_mode_echo_succeeds() {
        // execution_mode = Synchronous routes through execute_shell_sync.
        let result = call_run_terminal(json!({
            "command": "echo hello_sync",
            "execution_mode": "Synchronous"
        }))
        .await
        .expect("synchronous run_terminal_command must return Ok");
        assert!(!result.is_error.unwrap_or(false));
        let text = result_text(&result);
        assert!(
            text.contains("hello_sync"),
            "synchronous path must return the command output, got: {text:?}"
        );
    }

    #[tokio::test]
    async fn handle_unknown_execution_mode_defaults_to_async() {
        // An unrecognised execution_mode falls through to AsyncResultPush.
        let result = call_run_terminal(json!({
            "command": "echo hello_bogus",
            "execution_mode": "totally-bogus"
        }))
        .await
        .expect("unknown execution_mode must still return Ok via async default");
        assert!(!result.is_error.unwrap_or(false));
        let text = result_text(&result);
        assert!(
            text.contains("hello_bogus") || text.contains("AHMA ID:"),
            "expected echoed output or an AHMA ID, got: {text:?}"
        );
    }

    #[tokio::test]
    async fn handle_with_monitor_level_succeeds() {
        // Exercises the Some(log_monitor_config) branch + monitor_stream parse.
        let result = call_run_terminal(json!({
            "command": "echo hello_monitor",
            "monitor_level": "error",
            "monitor_stream": "stdout"
        }))
        .await
        .expect("monitored run_terminal_command must return Ok");
        assert!(!result.is_error.unwrap_or(false));
        let text = result_text(&result);
        assert!(
            text.contains("hello_monitor") || text.contains("AHMA ID:"),
            "expected echoed output or an AHMA ID, got: {text:?}"
        );
    }

    #[tokio::test]
    async fn handle_with_session_id_runs_special_path() {
        // session_id routes through execute_shell_special's session branch.
        let result = call_run_terminal(json!({
            "command": "echo hello_session",
            "session_id": "unit-test-session"
        }))
        .await
        .expect("session run_terminal_command must return Ok");
        assert!(!result.is_error.unwrap_or(false));
        let text = result_text(&result);
        assert!(
            text.contains("hello_session") || text.contains("AHMA ID:"),
            "expected echoed output or an AHMA ID, got: {text:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn handle_with_pty_runs_special_path() {
        // pty=true routes through execute_shell_special's PTY branch (Unix only).
        let result = call_run_terminal(json!({
            "command": "echo hello_pty",
            "pty": true
        }))
        .await
        .expect("pty run_terminal_command must return Ok");
        // The PTY branch returns an operation id immediately (or inlines fast
        // output); either way the handler returns a non-empty result.
        let text = result_text(&result);
        assert!(
            !text.is_empty(),
            "pty path must return some content (output or AHMA ID), got empty"
        );
    }

    #[tokio::test]
    async fn handle_missing_command_is_error() {
        // require_str fails when no command is provided.
        let outcome = call_run_terminal(json!({})).await;
        assert!(
            outcome.is_err(),
            "missing required 'command' must surface an error"
        );
    }
}
