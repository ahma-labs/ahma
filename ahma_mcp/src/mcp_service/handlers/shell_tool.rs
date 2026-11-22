use super::common;
use super::working_directory;
use crate::AhmaMcpService;
use crate::client_type::McpClientType;
use crate::mcp_service::schema;
use crate::shell_pool::platform_shell_program;
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, ErrorData as McpError},
    service::{RequestContext, RoleServer},
};
use serde_json::{Map, Value};
use std::sync::Arc;
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
                "Run the command attached to a pseudo-terminal (Unix only). Use for tools that require a TTY or change behaviour without one (colours, progress bars, interactive prompts). stdout and stderr are merged. Cannot be combined with session_id.",
            ),
        );
        properties.insert(
            "timeout_seconds".to_string(),
            schema::integer_property(
                "Kill the command if it is still running after this many seconds. Not a wait: the call still returns as soon as the command finishes, or hands back an operation id if it is slow (SPEC R2.6).",
            ),
        );
        // Deliberately NOT advertised: any parameter that lets the model choose
        // synchronous execution (SPEC R2.6.3). Blocking a single MCP request for
        // the length of a `cargo` build exceeds what several clients tolerate,
        // and models reach for such a flag by default rather than selectively.
        // The inline/async decision is ahma's (R2.6.1), not the model's.
        schema::object_input_schema(properties, &["command"])
    }

    /// Parameters `run_terminal_command` understands. Anything else a client
    /// sends is ignored — and disclosed in the result (SPEC R2.6.4) so a model
    /// that invented a parameter learns it did nothing, instead of assuming it
    /// took effect. `execution_mode` is honoured but unadvertised: it is the
    /// CLI/test escape hatch, not a knob for models.
    const KNOWN_ARGS: &'static [&'static str] = &[
        "command",
        "working_directory",
        "monitor_level",
        "monitor_stream",
        "session_id",
        "pty",
        "timeout_seconds",
        "execution_mode",
    ];

    /// Names the client sent that ahma does not act on, in a stable order.
    fn unknown_args(args: &Map<String, Value>) -> Vec<String> {
        let mut unknown: Vec<String> = args
            .keys()
            .filter(|k| !Self::KNOWN_ARGS.contains(&k.as_str()))
            .cloned()
            .collect();
        unknown.sort();
        unknown
    }

    /// The disclosure appended to a result when arguments were ignored.
    fn unknown_args_notice(unknown: &[String]) -> String {
        format!(
            "\n\nNote: ignored unknown argument(s): {}. `run_terminal_command` is \
             async-first: fast commands return their output inline, slower ones return \
             an operation id to `await`. There is no caller-selectable synchronous mode.",
            unknown.join(", ")
        )
    }

    /// Handles the 'run_terminal_command' built-in tool call, disclosing any
    /// arguments it ignored (SPEC R2.6.4).
    pub async fn handle_run_terminal_command(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let unknown = Self::unknown_args(params.arguments.as_ref().unwrap_or(&Map::new()));
        let result = self.dispatch_run_terminal_command(params, context).await?;
        if unknown.is_empty() {
            return Ok(result);
        }
        tracing::warn!(
            ignored = ?unknown,
            "run_terminal_command called with unknown argument(s); ignored and disclosed to the client"
        );
        Ok(common::append_note(
            result,
            &Self::unknown_args_notice(&unknown),
        ))
    }

    async fn dispatch_run_terminal_command(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args = params.arguments.unwrap_or_default();

        // No sandbox-readiness check here: `tools/call` dispatch applies it to
        // every tool, once (SPEC R5.1.2). A copy in this handler is what the
        // invariant used to be — and being written per-handler is precisely why
        // the six built-in file tools were added without it. Keeping a
        // now-unreachable duplicate would advertise the wrong pattern.

        // Extract command (required)
        let command = common::require_str(&args, "command", "command parameter is required")?;

        // Where the command runs, and who decided that (SPEC R5.2.8 / R5.4).
        let working_directory =
            working_directory::resolve(self.adapter.sandbox(), "run_terminal_command", &args)?;

        match self
            .run_terminal_command_in(command, args, &working_directory.path, context)
            .await
        {
            Ok(result) => Ok(working_directory.disclose(result)),
            Err(e) => Err(working_directory.disclose_error(e)),
        }
    }

    /// Run the command in an already-decided `working_directory`.
    ///
    /// Split from [`dispatch_run_terminal_command`](Self::dispatch_run_terminal_command)
    /// so every exit path — vault staging, session/PTY, sync, async — flows back
    /// through one place that can attach the working-directory disclosure.
    async fn run_terminal_command_in(
        &self,
        command: String,
        args: Map<String, Value>,
        working_directory: &str,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = args.get("timeout_seconds").and_then(|v| v.as_u64());

        if let Some(staged) = self
            .maybe_stage_run_terminal_rm(&command, working_directory)
            .await?
        {
            return Ok(staged);
        }

        // Dedicated execution paths: persistent shell session / PTY.
        if let Some(runner) = ShellRunner::from_args(&args)? {
            return self
                .execute_shell_special(&command, working_directory, timeout, runner, &context)
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

        // Determine execution mode. `execution_mode` is the CLI/test escape
        // hatch; models get no say (SPEC R2.6.3).
        let execution_mode = if self.force_synchronous {
            crate::adapter::ExecutionMode::Synchronous
        } else if let Some(mode_str) = common::opt_str(&args, "execution_mode") {
            match mode_str.as_str() {
                "Synchronous" => crate::adapter::ExecutionMode::Synchronous,
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
                    working_directory,
                    timeout,
                    &subcommand_config,
                    &context,
                )
                .await
            }
            crate::adapter::ExecutionMode::AsyncResultPush => {
                self.execute_shell_async(
                    adapter_args,
                    working_directory,
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
            // run_terminal_command executes an arbitrary shell command — the
            // least contained builtin there is, matching
            // BuiltinTool::RunTerminalCommand.is_mutating() == true.
            mutates: Some(true),
            // Undeclared on purpose: the adapter classifies each command line
            // (SPEC R2.7.4) — `git status` reads, `cargo build` writes.
            concurrency: None,
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
        let id = crate::utils::operation::mint_id("run_terminal_command", cmd_str);
        let started_at = std::time::Instant::now();
        self.emit_vault_tool_call(
            &id,
            "run_terminal_command",
            &VaultArgs::new(working_directory, cmd_str).to_json(),
        )
        .await;

        let progress_token = context.meta.get_progress_token();
        let client_type = McpClientType::from_peer(&context.peer);

        // Sync operations never enter the OperationMonitor, so the event
        // forwarder cannot see them — push start/final progress directly via
        // the shared sync-progress helpers (the shell's "base command" is the
        // platform shell program).
        let push_token = progress_token.filter(|_| self.effective_supports_progress(client_type));
        self.push_sync_start_progress(
            push_token.clone(),
            &context.peer,
            platform_shell_program(),
            working_directory,
        )
        .await;

        let result = self
            .adapter
            .execute_sync_in_dir_as(
                Some(&id),
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

        self.push_sync_final_progress(
            push_token,
            &context.peer,
            &id,
            platform_shell_program(),
            working_directory,
            duration_ms,
            &result,
        )
        .await;

        match result {
            Ok(output) => Ok(common::text_result(output)),
            // On an out-of-scope path access this attaches a machine-readable
            // `sandbox_denial` payload to the error's `data` field; otherwise a
            // plain internal error. (Logs the failure internally.)
            Err(e) => Err(common::execution_error(&e)),
        }
    }

    /// Execute `run_terminal_command` with a `session_id` or `pty: true`.
    ///
    /// Both paths run asynchronously through the standard operation lifecycle
    /// (operation id returned immediately; results via `await`/`status` and
    /// the unified event stream).
    async fn execute_shell_special(
        &self,
        command: &str,
        working_directory: &str,
        timeout: Option<u64>,
        runner: ShellRunner,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let id = crate::utils::operation::mint_id("run_terminal_command", command);
        self.emit_vault_tool_call(
            &id,
            "run_terminal_command",
            &VaultArgs::new(working_directory, command)
                .special(&runner)
                .to_json(),
        )
        .await;

        let progress_token = context.meta.get_progress_token();
        let client_type = McpClientType::from_peer(&context.peer);
        self.register_progress_if_requested(&id, context.peer.clone(), progress_token, client_type)
            .await;

        let started = match &runner {
            ShellRunner::Session(session) => {
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
            ShellRunner::Pty => {
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
                if let Some(result) = common::try_automatic_async_completion(
                    &self.operation_monitor,
                    &op_id,
                    self.effective_request_budget(client_type),
                )
                .await
                {
                    return Ok(result);
                }
                let hint = crate::tool_hints::preview(&op_id, "run_terminal_command");
                Ok(common::text_result(format!(
                    "{}{op_id}{hint}",
                    crate::utils::operation::STARTED_PREFIX
                )))
            }
            Err(e) => {
                self.progress_push.unregister(&id).await;
                self.emit_vault_tool_complete(&id, false, 0).await;
                // Session/PTY starts validate the working directory too, so this
                // is the same denial the async path can hit — it gets the same
                // machine-readable payload (logging included).
                Err(common::async_execution_error(&e))
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
        let id = crate::utils::operation::mint_id("run_terminal_command", cmd_str);
        let vault_args_summary = VaultArgs::new(working_directory, cmd_str).to_json();

        let progress_token = context.meta.get_progress_token();
        let client_type = McpClientType::from_peer(&context.peer);

        self.call_async_tool(
            "run_terminal_command",
            id,
            platform_shell_program(),
            working_directory,
            adapter_args,
            timeout,
            subcommand_config,
            log_monitor_config,
            &vault_args_summary,
            progress_token,
            client_type,
            context.peer.clone(),
            // The operator's `tools.execution_mode` decides; the caller has
            // no say (SPEC R2.6.3).
            self.call_wait(false),
            // Async is the *default* path, so this is the error agents
            // actually see: it must carry the same structured
            // `sandbox_denial` + remediation the sync path carries, or a
            // scope violation reaches the model as unactionable prose.
            common::async_execution_error,
        )
        .await
    }
}

/// The dedicated runner a `run_terminal_command` call asked for: a terminal,
/// or a persistent shell session. Read once from the arguments, so a call can
/// ask for one or the other and nothing in between.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShellRunner {
    Pty,
    Session(String),
}

impl ShellRunner {
    /// `None` for a plain call. A call that asks for both is refused, saying
    /// why: a PTY inside a session is not supported, and running it without
    /// the PTY it asked for would leave the model thinking it had one.
    fn from_args(args: &Map<String, Value>) -> Result<Option<Self>, McpError> {
        let session_id = common::opt_str(args, "session_id");
        let pty = args.get("pty").and_then(Value::as_bool).unwrap_or(false);
        match (session_id, pty) {
            (Some(_), true) => Err(common::mcp_invalid_params(
                "`pty: true` and `session_id` cannot be combined: a terminal inside a persistent \
                 session is not supported. Drop `pty` to keep the session's state between \
                 calls, or drop `session_id` to get a terminal.",
            )),
            (Some(id), false) => Ok(Some(Self::Session(id))),
            (None, true) => Ok(Some(Self::Pty)),
            (None, false) => Ok(None),
        }
    }
}

/// The arguments a `run_terminal_command` call records in the task vault's
/// audit log (`args_summary`), serialized rather than formatted: a command
/// holds quotes and backslashes, and the line must stay JSON whatever it holds.
#[derive(serde::Serialize)]
struct VaultArgs<'a> {
    working_directory: &'a str,
    command: &'a str,
    /// Written for a terminal or session call (as `null` when it has none),
    /// left out of a plain one, as before.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<Option<&'a str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pty: Option<bool>,
}

impl<'a> VaultArgs<'a> {
    fn new(working_directory: &'a str, command: &'a str) -> Self {
        Self {
            working_directory,
            command,
            session_id: None,
            pty: None,
        }
    }

    /// The fields a terminal or session call adds.
    fn special(self, runner: &'a ShellRunner) -> Self {
        let (session_id, pty) = match runner {
            ShellRunner::Session(id) => (Some(id.as_str()), false),
            ShellRunner::Pty => (None, true),
        };
        Self {
            session_id: Some(session_id),
            pty: Some(pty),
            ..self
        }
    }

    fn to_json(&self) -> String {
        // Serializing borrowed strings and a bool cannot fail.
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::working_directory;
    use crate::AhmaMcpService;
    use crate::adapter::{Adapter, ExecutionMode};
    use crate::mcp_service::GuidanceConfig;
    use crate::operation_monitor::{MonitorConfig, OperationMonitor};
    use crate::sandbox::{Sandbox, SandboxMode};
    use crate::shell::cli::AppConfig;
    use crate::shell_pool::{ShellPoolConfig, ShellPoolManager};
    use crate::test_utils::in_process::{
        build_test_service, create_in_process_mcp_empty, create_in_process_mcp_with_scope,
    };
    use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
    use rmcp::model::{CallToolRequestParams, CallToolResult, ErrorCode, ErrorData as McpError};
    use serde_json::{Map, Value, json};
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    /// The vault's audit line holds the arguments as JSON (`args_summary`), and
    /// a reader parses it. A command with a quote or a backslash, and every
    /// terminal or session call (`session_id` was written with Rust's `{:?}`,
    /// `Some("s")`), wrote text no JSON reader accepts.
    #[test]
    fn vault_args_are_json_whatever_the_command_holds() {
        use super::VaultArgs;
        let dir = r#"C:\Users\me\repo "x""#;
        let command = r#"grep -rn "a\b" src | head"#;
        let plain: Value = serde_json::from_str(&VaultArgs::new(dir, command).to_json())
            .expect("a plain call is JSON");
        assert_eq!(plain, json!({"working_directory": dir, "command": command}));

        for (runner, session_id, pty) in [
            (super::ShellRunner::Session("s1".into()), Some("s1"), false),
            (super::ShellRunner::Pty, None, true),
        ] {
            let text = VaultArgs::new(dir, command).special(&runner).to_json();
            let parsed: Value = serde_json::from_str(&text).expect("a session or PTY call is JSON");
            assert_eq!(
                parsed,
                json!({"working_directory": dir, "command": command, "session_id": session_id, "pty": pty}),
                "{text}"
            );
        }
    }

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
        let out = tokio::time::timeout(
            TestTimeouts::get(TimeoutCategory::ToolCall),
            mcp.client.call_tool(params),
        )
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

    /// A call that asks for a PTY inside a session is refused and told why.
    /// The session ran it and the PTY request was dropped with a line in the
    /// server's log, so the model never learned it had no terminal.
    #[tokio::test]
    async fn a_pty_inside_a_session_is_refused_and_says_why() {
        let err = call_run_terminal(json!({
            "command": "echo hello_both",
            "session_id": "unit-test-session",
            "pty": true
        }))
        .await
        .expect_err("asking for both is refused");
        let text = err.to_string();
        assert!(
            text.contains("pty") && text.contains("session_id"),
            "the refusal names both: {text}"
        );
    }

    /// The task vault and the execution audit log name a command by one id,
    /// so a reader can join the two logs, on the synchronous path as on the
    /// others. The synchronous path minted a second id for the execution log.
    #[tokio::test]
    async fn the_vault_and_the_execution_log_name_a_command_by_one_id() {
        let audit_dir = tempfile::tempdir().unwrap();
        // First caller wins; read back whichever file this process writes.
        let _ = crate::adapter::audit::set_audit_log_path(audit_dir.path().join("audit.jsonl"));
        let exec_log = crate::adapter::audit::audit_log().path().to_path_buf();
        let vault = tempfile::tempdir().unwrap();
        let mcp = create_in_process_mcp_empty()
            .await
            .expect("in-process MCP pair must build");
        mcp.service.set_app_config(Arc::new(AppConfig {
            task_vault: Some(vault.path().to_path_buf()),
            ..AppConfig::default()
        }));

        /// The `operation_id` of the `tool_call` in `log` that mentions `marker`.
        async fn call_id(log: &Path, marker: &str) -> Option<String> {
            let text = tokio::fs::read_to_string(log).await.unwrap_or_default();
            text.lines()
                .filter(|line| line.contains(marker))
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find(|event| event["type"] == "tool_call")
                .and_then(|event| event["operation_id"].as_str().map(str::to_string))
        }

        for (marker, mode) in [
            ("one_id_sync_marker", "Synchronous"),
            ("one_id_async_marker", "AsyncResultPush"),
        ] {
            let params = CallToolRequestParams::new(Cow::Borrowed("run_terminal_command"))
                .with_arguments(args_of(json!({
                    "command": format!("echo {marker}"),
                    "execution_mode": mode,
                })));
            let result = tokio::time::timeout(
                TestTimeouts::get(TimeoutCategory::ToolCall),
                mcp.client.call_tool(params),
            )
            .await
            .expect("call_tool must not time out")
            .expect("the call runs");
            assert!(!result.is_error.unwrap_or(false), "{mode}: {result:?}");

            let in_vault = call_id(&vault.path().join("audit.jsonl"), marker)
                .await
                .unwrap_or_else(|| panic!("{mode}: the vault records the call"));
            let in_exec_log = call_id(&exec_log, marker)
                .await
                .unwrap_or_else(|| panic!("{mode}: the execution log records the call"));
            assert_eq!(in_vault, in_exec_log, "{mode}: one id for one command");
        }
        let _ = mcp.client.cancel().await;
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

    // ── working-directory substitution (SPEC R5.2 provenance, R5.4 disclosure) ─
    //
    // REGRESSION: with the scope at the invented default `~/sandbox`, a call
    // that omitted `working_directory` silently ran there and returned
    // `fatal: not a git repository` / `bash: ./gradlew: No such file or
    // directory`. Those read as project errors, so the model could not diagnose
    // the directory and abandoned ahma for its own unsandboxed terminal.

    /// A service whose scope provenance is exactly the flag pair
    /// `scope_source()` reads (explicit / roots-received). When neither is set,
    /// the scope is armed as the user's **container root** so the provenance is
    /// [`ScopeSource::Container`] (a fresh sandbox with no container would now
    /// honestly report `pending`).
    ///
    /// `SandboxMode::Strict` on purpose: `SandboxMode::Test` resolves scope but
    /// never substitutes it, so it cannot exercise this decision at all.
    async fn service_with_provenance(
        scope: &Path,
        explicit: bool,
        roots_received: bool,
    ) -> AhmaMcpService {
        let container_root = (!explicit && !roots_received)
            .then(|| dunce::canonicalize(scope).expect("tempdir canonicalizes"));
        let sandbox = Sandbox::new(
            vec![scope.to_path_buf()],
            SandboxMode::Strict,
            Default::default(),
        )
        .expect("sandbox must build")
        .with_explicit_scopes(explicit)
        .with_container_root(container_root);

        let operation_monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::get(TimeoutCategory::ToolCall),
        )));
        let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
        let adapter = Arc::new(
            Adapter::new(
                Arc::clone(&operation_monitor),
                shell_pool,
                Arc::new(sandbox),
            )
            .expect("adapter must build"),
        );
        let service = AhmaMcpService::new(
            adapter,
            operation_monitor,
            Arc::new(HashMap::new()),
            Arc::new(None::<GuidanceConfig>),
            false, // force_synchronous
            false, // defer_sandbox
        )
        .await
        .expect("service must build");
        if roots_received {
            service.adapter.sandbox().mark_roots_received();
        }
        service
    }

    /// The scope as the sandbox canonicalized it (macOS rewrites `/var` to
    /// `/private/var`), which is what the handler substitutes.
    fn locked_scope(service: &AhmaMcpService) -> String {
        service
            .adapter
            .sandbox()
            .scopes()
            .first()
            .expect("test sandbox has one scope")
            .to_string_lossy()
            .to_string()
    }

    fn args_of(value: Value) -> Map<String, Value> {
        value.as_object().expect("object args").clone()
    }

    /// Resolve as `run_terminal_command` does, through the surface-independent
    /// resolver both it and the MTDF dispatcher share (SPEC R5.2.8).
    fn resolve_for(
        service: &AhmaMcpService,
        args: Value,
    ) -> Result<working_directory::WorkingDirectory, McpError> {
        working_directory::resolve(
            service.adapter.sandbox(),
            "run_terminal_command",
            &args_of(args),
        )
    }

    #[tokio::test]
    async fn supplied_working_directory_is_used_verbatim_and_not_disclosed() {
        // Even with the provenance that refuses a substitution, an explicitly
        // supplied directory is honoured untouched and disclosed to nobody.
        let temp = tempfile::tempdir().unwrap();
        let service = service_with_provenance(temp.path(), false, false).await;
        let supplied = temp.path().to_string_lossy().to_string();

        let resolved = resolve_for(
            &service,
            json!({
                "command": "true",
                "working_directory": supplied,
            }),
        )
        .expect("a supplied working_directory must never be refused");

        assert_eq!(resolved.path, supplied, "supplied path must pass through");
        assert!(
            !resolved.was_substituted(),
            "nothing was substituted, so nothing may be disclosed"
        );
    }

    #[tokio::test]
    async fn omitted_working_directory_with_container_scope_is_refused_actionably() {
        let temp = tempfile::tempdir().unwrap();
        // Neither explicit nor roots-derived => the user's container root (R5.2.3).
        let service = service_with_provenance(temp.path(), false, false).await;
        let scope = locked_scope(&service);

        let err = resolve_for(&service, json!({"command": "git status"}))
            .expect_err("a container-provenance substitution must be refused, not run");

        assert_eq!(
            err.code,
            ErrorCode::INVALID_PARAMS,
            "the caller can fix this by passing a parameter: {err:?}"
        );
        let message = err.message.to_string();
        for expected in [
            "working_directory", // what the call was missing
            "source: container", // the provenance that made it unsafe
            scope.as_str(),      // the directory it would have used
            "--sandbox-scope",   // how to fix it at the session level
        ] {
            assert!(
                message.contains(expected),
                "error must name {expected:?}; got: {message}"
            );
        }
        let data = err.data.expect("machine-readable payload");
        assert_eq!(
            data.get("kind").and_then(|v| v.as_str()),
            Some("working_directory_required")
        );
        assert_eq!(
            data.get("scope_source").and_then(|v| v.as_str()),
            Some("container")
        );
    }

    #[tokio::test]
    async fn omitted_working_directory_with_roots_scope_runs_and_marks_substitution() {
        let temp = tempfile::tempdir().unwrap();
        let service = service_with_provenance(temp.path(), false, true).await;
        let scope = locked_scope(&service);

        let resolved = resolve_for(&service, json!({"command": "git status"}))
            .expect("a client-chosen scope is a legitimate substitution");

        assert_eq!(resolved.path, scope, "must run in the locked scope");
        let notice = resolved
            .substitution_notice()
            .expect("a substituted directory must be disclosed (R5.4)");
        assert!(notice.contains(&scope), "notice must name it: {notice}");
        assert!(
            notice.contains("roots/list"),
            "notice must state the provenance: {notice}"
        );
    }

    #[tokio::test]
    async fn omitted_working_directory_with_explicit_scope_runs_and_marks_substitution() {
        let temp = tempfile::tempdir().unwrap();
        let service = service_with_provenance(temp.path(), true, false).await;
        let scope = locked_scope(&service);

        let resolved = resolve_for(&service, json!({"command": "git status"}))
            .expect("an operator-chosen scope is a legitimate substitution");

        assert_eq!(resolved.path, scope);
        let notice = resolved
            .substitution_notice()
            .expect("a substituted directory must be disclosed (R5.4)");
        assert!(
            notice.contains("explicit"),
            "notice must state the provenance: {notice}"
        );
    }

    #[tokio::test]
    async fn substitution_notice_reaches_the_client_in_the_result() {
        // The disclosure is worthless unless the model actually receives it, so
        // assert on what comes back over the wire, not on the resolver.
        let result = call_run_terminal(json!({"command": "echo hello_substituted"}))
            .await
            .expect("run_terminal_command must return Ok");
        let text = result_text(&result);
        assert!(
            text.contains("no `working_directory` was given"),
            "an omitted working_directory must be disclosed in the result: {text:?}"
        );
    }

    #[tokio::test]
    async fn supplied_working_directory_adds_no_notice_over_the_wire() {
        // The in-process sandbox scope is the process CWD, so this is in scope.
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let result = call_run_terminal(json!({
            "command": "echo hello_supplied",
            "working_directory": cwd,
        }))
        .await
        .expect("run_terminal_command must return Ok");
        let text = result_text(&result);
        assert!(
            !text.contains("no `working_directory` was given"),
            "the caller chose the directory; there is nothing to disclose: {text:?}"
        );
    }

    // ── async-path sandbox denials carry actionable data (SPEC R5.4.7) ───────

    #[tokio::test]
    async fn async_path_scope_denial_carries_remediation_naming_the_grant_command() {
        // REGRESSION: the async path built its error with `mcp_internal`, whose
        // `data` is `None`. Async is the *default* for run_terminal_command, so
        // in practice every scope violation reached the agent as bare prose —
        // observed on the wire in an Antigravity session. Only the rarely-taken
        // synchronous path carried the `sandbox_denial` payload.
        let tools_dir = tempfile::tempdir().unwrap();
        let scope = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // `create_in_process_mcp_with_scope` is always Strict, so `validate_path`
        // really runs (SandboxMode::Test would skip it and make this vacuous).
        let mcp =
            create_in_process_mcp_with_scope(tools_dir.path(), vec![scope.path().to_path_buf()])
                .await
                .expect("in-process MCP pair must build");
        let params = CallToolRequestParams::new(Cow::Borrowed("run_terminal_command"))
            .with_arguments(args_of(json!({
                "command": "echo denied",
                "working_directory": outside.path().to_string_lossy(),
            })));
        let outcome = tokio::time::timeout(
            TestTimeouts::get(TimeoutCategory::ToolCall),
            mcp.client.call_tool(params),
        )
        .await
        .expect("call_tool must not time out");
        let _ = mcp.client.cancel().await;

        let err = match outcome.expect_err("an out-of-scope working directory must fail") {
            rmcp::ServiceError::McpError(e) => e,
            other => panic!("expected an MCP error, got: {other:?}"),
        };
        let data = err
            .data
            .expect("an async scope denial must carry machine-readable data");
        assert_eq!(
            data.get("kind").and_then(|v| v.as_str()),
            Some("sandbox_denial"),
            "async denials must use the same payload shape as sync: {data}"
        );
        let remediation = data
            .get("remediation")
            .and_then(|v| v.as_str())
            .expect("a denial without remediation is not actionable");
        assert!(
            remediation.contains("ahma sandbox grant"),
            "remediation must name the escape hatch concretely: {remediation}"
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

#[cfg(test)]
mod arg_disclosure_tests {
    use crate::AhmaMcpService;
    use serde_json::{Map, json};

    fn args(pairs: &[(&str, serde_json::Value)]) -> Map<String, serde_json::Value> {
        pairs
            .iter()
            .cloned()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    #[tokio::test]
    async fn every_advertised_property_is_a_known_argument() {
        // The schema and the handler must not drift: a property we advertise but
        // do not list as known would be "disclosed as ignored" on every call.
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let schema = service.generate_input_schema_for_run_terminal_command();
        let props = schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("schema has properties");
        for name in props.keys() {
            assert!(
                AhmaMcpService::KNOWN_ARGS.contains(&name.as_str()),
                "advertised property {name:?} is missing from KNOWN_ARGS"
            );
        }
    }

    #[test]
    fn known_arguments_are_not_reported() {
        let a = args(&[
            ("command", json!("ls")),
            ("working_directory", json!("/tmp")),
            ("pty", json!(true)),
            ("timeout_seconds", json!(30)),
        ]);
        assert!(AhmaMcpService::unknown_args(&a).is_empty());
    }

    #[test]
    fn an_invented_argument_is_reported_in_a_stable_order() {
        // REGRESSION: a captured session shows Gemini sending `"sync": true`,
        // which was not in the schema. ahma dropped it silently, so the model had
        // no way to learn the parameter did nothing — and concluded ahma was
        // broken rather than that its parameter was imaginary.
        let a = args(&[
            ("command", json!("cargo fmt --check")),
            ("sync", json!(true)),
            ("blocking", json!(true)),
        ]);
        assert_eq!(
            AhmaMcpService::unknown_args(&a),
            vec!["blocking".to_string(), "sync".to_string()]
        );

        let notice = AhmaMcpService::unknown_args_notice(&AhmaMcpService::unknown_args(&a));
        assert!(notice.contains("sync"), "{notice}");
        assert!(
            notice.contains("no caller-selectable synchronous mode"),
            "the notice must say why, or the model will just try again: {notice}"
        );
    }
}
