use ahma_llm_monitor::ChatMessage;
use ahma_llm_monitor::LocalProvider;
use ahma_llm_monitor::client::LlmClient;
use tokio::sync::mpsc::Sender;

/// Build an [`LlmClient`] for `base_url`, honoring everything the matching
/// `~/.ahma/config.toml` entry declares.
///
/// The TUI addresses providers by URL, so a client built straight from
/// [`LlmClient::new`] silently discards the entry's `kind` and `num_ctx`: the
/// lost `num_ctx` makes proactive compaction inert (no denominator), and the
/// lost `kind` lets the host heuristic override an explicit `kind =
/// "anthropic"`, sending OpenAI-shaped requests to a proxied Anthropic
/// provider. Issue #484.
///
/// When no entry claims this URL, the heuristic is left in charge — that is
/// the correct behavior for ad-hoc and auto-discovered endpoints.
pub fn build_configured_client(base_url: impl Into<String>, model: impl Into<String>) -> LlmClient {
    use ahma_common::config::ProviderKind;

    let base_url = base_url.into();
    let config = ahma_common::config::AhmaConfig::load();
    // The entry's key too, or every call to a keyed cloud provider is a 401.
    let api_key = config
        .provider_for_base_url(&base_url)
        .and_then(|e| e.resolve().ok())
        .and_then(|r| r.api_key);
    let client = LlmClient::new(base_url.clone(), model.into(), api_key);
    let client = match config.kind_for_base_url(&base_url) {
        Some(ProviderKind::Anthropic) => client.with_flavor(ahma_llm_monitor::ApiFlavor::Anthropic),
        Some(ProviderKind::OpenAi) => client.with_flavor(ahma_llm_monitor::ApiFlavor::OpenAi),
        None => client,
    };
    client.with_num_ctx(config.num_ctx_for_base_url(&base_url))
}

pub enum BridgeEvent {
    Error(String),
    /// A chat turn's message could not be delivered to the daemon; the turn
    /// is ended with this reason rather than left spinning.
    TurnSendFailed(String),
    ToolCallStarted {
        id: String,
        name: String,
        args: String,
    },
    ToolCallFinished {
        id: String,
        result: String,
        failed: bool,
    },
    ProvidersDiscovered(Vec<LocalProvider>),
    ModelsRefreshed {
        base_url: String,
        models: Vec<String>,
    },
    Decomposed {
        steps: Vec<ParsedStep>,
    },
    WindowOutput {
        window_id: usize,
        line: String,
    },
    WindowFinished {
        window_id: usize,
        success: bool,
        summary: String,
    },
    ExternalToolsRefreshed {
        manager: crate::mcp_connections::McpConnectionManager,
    },
    /// A tool call just negotiated (or reused) an MCP session. The app stores
    /// this in `state.session_id` so the *next* tool call's `McpChatConfig`
    /// carries it and `get_or_create_session` takes its reuse fast-path
    /// instead of spawning a brand-new bridge subprocess and handshake per
    /// call (SPEC R25).
    SessionEstablished {
        session_id: String,
    },
}

pub type McpChatConfig = ahma_core::agent::McpChatConfig;

pub fn spawn_external_tools_refresh(
    mut manager: crate::mcp_connections::McpConnectionManager,
    tx: Sender<BridgeEvent>,
) {
    tokio::spawn(async move {
        manager.refresh_tools().await;
        let _ = tx
            .send(BridgeEvent::ExternalToolsRefreshed { manager })
            .await;
    });
}

pub fn spawn_discovery_task(tx: Sender<BridgeEvent>) {
    tokio::spawn(async move {
        if let Ok(providers) = ahma_llm_monitor::discovery::discover_local_providers().await {
            let _ = tx.send(BridgeEvent::ProvidersDiscovered(providers)).await;
        }
    });
}

pub fn spawn_model_refresh(base_url: String, tx: Sender<BridgeEvent>) {
    tokio::spawn(async move {
        let client = build_configured_client(base_url.clone(), "");
        let models = client.list_model().await;
        let _ = tx
            .send(BridgeEvent::ModelsRefreshed { base_url, models })
            .await;
    });
}

/// As [`spawn_model_refresh`], with an explicit key — the setup wizard checks
/// a cloud provider before it is registered, so no config entry holds it yet.
pub fn spawn_model_refresh_with_key(
    base_url: String,
    api_key: Option<String>,
    tx: Sender<BridgeEvent>,
) {
    tokio::spawn(async move {
        let client = match api_key {
            Some(key) => LlmClient::new(base_url.clone(), "", Some(key)),
            None => build_configured_client(base_url.clone(), ""),
        };
        let models = client.list_model().await;
        let _ = tx
            .send(BridgeEvent::ModelsRefreshed { base_url, models })
            .await;
    });
}

fn is_local_default_server(url: &str) -> bool {
    if url.starts_with("unix://") {
        return true;
    }
    let trimmed = url.trim_end_matches('/');
    trimmed == "http://localhost:3000" || trimmed == "http://127.0.0.1:3000"
}

/// Recovers from a `403` on the (possibly cached/reused) MCP session: drops
/// it from the process-wide cache, negotiates a fresh one, tells `tx` about
/// it so subsequent calls reuse it instead of retrying a dead session, and
/// retries the tool call exactly once.
async fn retry_tool_call_after_403(
    client: &reqwest::Client,
    url: &str,
    mcp: &McpChatConfig,
    tool: &str,
    arguments: serde_json::Value,
    tx: &Sender<BridgeEvent>,
) -> (String, bool) {
    ahma_core::agent::invalidate_cached_session(url, &mcp.workspace_root);
    let fresh_mcp = McpChatConfig {
        session_id: None,
        ..mcp.clone()
    };
    let new_sid = match ahma_core::agent::get_or_create_session(client, url, &fresh_mcp).await {
        Ok(sid) => sid,
        Err(e) => return (format!("Error: {e}"), true),
    };
    // The old session_id was rejected (403); replace it in app state so
    // subsequent calls don't keep retrying a dead session before falling
    // back here every time.
    let _ = tx
        .send(BridgeEvent::SessionEstablished {
            session_id: new_sid.clone(),
        })
        .await;
    match ahma_core::agent::call_mcp_tool_http(client, url, &new_sid, tool, arguments).await {
        Ok((result, failed)) => (result, failed),
        Err(e) => (format!("Error: {e}"), true),
    }
}

pub fn spawn_tool_call_task(
    tool: String,
    arguments: serde_json::Value,
    mcp: McpChatConfig,
    tx: Sender<BridgeEvent>,
) {
    tokio::spawn(async move {
        let id = format!("call_{}", ahma_common::keepalive::current_timestamp_ms());
        let args_str = serde_json::to_string(&arguments).unwrap_or_default();
        let _ = tx
            .send(BridgeEvent::ToolCallStarted {
                id: id.clone(),
                name: tool.clone(),
                args: args_str,
            })
            .await;

        if is_local_default_server(&mcp.base_url) {
            // The chat's tool calls run in an ordinary MCP session on the
            // per-user daemon, scoped by this TUI's own `roots/list` answer —
            // not by a server started for this directory (SPEC R-DAEMON.9).
            let socket = ahma_common::daemon_hub::mcp_socket_path(None);
            let res =
                ahma_mcp::shell::modes::daemon_client::ensure_daemon(Some(&socket), None, None)
                    .await;
            if let Err(e) = res {
                let _ = tx
                    .send(BridgeEvent::ToolCallFinished {
                        id: id.clone(),
                        result: format!("Error reaching the ahma daemon: {e}"),
                        failed: true,
                    })
                    .await;
                return;
            }
        }

        // Shared, process-wide client (keyed by base URL): building a fresh
        // reqwest client per tool call re-does TLS/pool setup and loses
        // keep-alive connection reuse.
        let (request_base_url, client) = match ahma_core::agent::cached_http_client(&mcp.base_url) {
            Ok(pair) => pair,
            Err(e) => {
                let _ = tx
                    .send(BridgeEvent::ToolCallFinished {
                        id: id.clone(),
                        result: format!("Error: {e}"),
                        failed: true,
                    })
                    .await;
                return;
            }
        };
        let url = format!("{}/mcp", request_base_url);

        let session_id = match ahma_core::agent::get_or_create_session(&client, &url, &mcp).await {
            Ok(sid) => sid,
            Err(err) => {
                let _ = tx
                    .send(BridgeEvent::ToolCallFinished {
                        id: id.clone(),
                        result: format!("Error: {err}"),
                        failed: true,
                    })
                    .await;
                return;
            }
        };
        // Feed the (possibly freshly negotiated) session id back to the app so
        // the *next* tool call's McpChatConfig carries it and reuses this
        // session instead of handshaking a brand-new one (SPEC R25).
        let _ = tx
            .send(BridgeEvent::SessionEstablished {
                session_id: session_id.clone(),
            })
            .await;

        let tool_result = ahma_core::agent::call_mcp_tool_http(
            &client,
            &url,
            &session_id,
            &tool,
            arguments.clone(),
        )
        .await;
        let (result, failed) = match tool_result {
            Ok((result, failed)) => (result, failed),
            Err(ref err) if err.contains("HTTP 403") => {
                retry_tool_call_after_403(&client, &url, &mcp, &tool, arguments, &tx).await
            }
            Err(err) => (format!("Error: {err}"), true),
        };
        let _ = tx
            .send(BridgeEvent::ToolCallFinished { id, result, failed })
            .await;
    });
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct ParsedStep {
    pub task: String,
    pub r#type: String, // "shell_command", "llm_call", "planning"
    pub command: Option<String>,
    pub instructions: Option<String>,
    pub subgoal: Option<String>,
    pub sandbox_scopes: Option<Vec<String>>,
    pub allowed_tools: Option<Vec<String>>,
    pub allowed_domains: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize)]
struct ParsedPlan {
    pub steps: Vec<ParsedStep>,
}

fn clean_json_response(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some(stripped) = s.strip_prefix("```json") {
        s = stripped;
    } else if let Some(stripped) = s.strip_prefix("```") {
        s = stripped;
    }
    if let Some(stripped) = s.strip_suffix("```") {
        s = stripped;
    }
    s.trim().to_string()
}

pub fn parse_steps(response: &str) -> anyhow::Result<Vec<ParsedStep>> {
    let cleaned = clean_json_response(response);
    let plan: ParsedPlan = serde_json::from_str(&cleaned).map_err(|e| {
        anyhow::anyhow!("Failed to parse JSON plan from LLM response: {e} ({cleaned})")
    })?;
    Ok(plan.steps)
}

pub fn build_planning_prompt(
    goal: &str,
    task_desc: &str,
    branch_context: &str,
    max_subtasks: usize,
) -> String {
    format!(
        r#"You are an expert task planner and knowledge worker. Your job is to decompose the current task into at most {max_subtasks} sequential steps.

System Instructions:
1. Break the task down into distinct, logical, self-contained sub-tasks.
2. Each step must be one of the following types:
   - "shell_command": Running a sandboxed terminal command.
   - "llm_call": A subtask that requires reasoning or parsing text.
   - "planning": A subtask that requires further planning/decomposition.
3. Security Scoping Down Rules:
   - If a step operates on a subdirectory (e.g. "src"), you may narrow down filesystem access by setting `sandbox_scopes` (e.g. `["src"]`).
   - You may restrict the allowed command prefixes in `allowed_tools` (e.g. `["cargo build", "cargo clippy"]` or `["git diff"]`).
   - You may restrict outbound network access by setting `allowed_domains` (e.g. `["crates.io", "api.github.com"]`).
4. Output format: You must output ONLY a valid JSON object matching the schema below. No other markdown formatting except optional json code blocks.

Expected JSON Schema:
```json
{{
  "steps": [
    {{
      "task": "description of this step",
      "type": "shell_command",
      "command": "cargo check --workspace",
      "sandbox_scopes": ["optional_subdir"],
      "allowed_tools": ["cargo"],
      "allowed_domains": []
    }},
    {{
      "task": "evaluate errors",
      "type": "llm_call",
      "instructions": "Look at the check output and identify compile errors."
    }}
  ]
}}
```

Overall Goal: {goal}

Current Task: {task_desc}

Context (Previous branch outcomes):
{branch_context}

Decompose this task and output the JSON steps now:"#
    )
}

pub fn spawn_decompose_task(client: LlmClient, goal: String, tx: Sender<BridgeEvent>) {
    tokio::spawn(async move {
        let prompt = build_planning_prompt(&goal, &goal, "No prior task context available.", 5);
        let system_msg = serde_json::json!({
            "role": "system",
            "content": "You are a precise task orchestrator. You decompose goals into subtasks and output strictly valid JSON according to the schema provided."
        });
        let user_msg = serde_json::json!({
            "role": "user",
            "content": prompt
        });

        let completion_res = client
            .chat_completion_with_tools(&[system_msg, user_msg], &[])
            .await;
        match completion_res {
            Ok(completion) => {
                let steps_res = parse_steps(&completion.content);
                match steps_res {
                    Ok(steps) => {
                        let _ = tx.send(BridgeEvent::Decomposed { steps }).await;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(BridgeEvent::Error(format!(
                                "Failed to parse decomposition JSON: {e}"
                            )))
                            .await;
                    }
                }
            }
            Err(e) => {
                let _ = tx
                    .send(BridgeEvent::Error(format!(
                        "Decomposition call failed: {e}"
                    )))
                    .await;
            }
        }
    });
}

/// Where a `!` command's progress is reported besides the local window.
///
/// `Option`-wrapped at the call site rather than baked in, because the same
/// runner is used by tests that have no daemon and no hub to talk to.
#[derive(Debug, Clone)]
pub struct BangReport {
    pub reporter: crate::tui_reporter::TuiReporter,
    pub op_id: String,
}

impl BangReport {
    fn output(&self, line: &str, is_stderr: bool) {
        self.reporter
            .report(ahma_common::daemon_hub::DaemonEvent::OpOutput {
                id: self.op_id.clone(),
                line: line.to_string(),
                is_stderr,
            });
    }

    fn finished(
        &self,
        status: ahma_common::daemon_hub::OpStatus,
        summary: &str,
        exit_code: Option<i64>,
        started: std::time::Instant,
    ) {
        self.reporter
            .report(ahma_common::daemon_hub::DaemonEvent::OpFinished {
                id: self.op_id.clone(),
                status,
                result_summary: Some(summary.to_string()),
                duration_ms: started.elapsed().as_millis() as u64,
                ended_epoch_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_millis() as u64),
                exit_code,
                denial: None,
                interrupted: false,
            });
    }
}

/// The outcome fields shared by every exit path of `spawn_window_cli_task`,
/// bundled together so `finish_window` takes one value instead of six.
struct WindowOutcome {
    window_id: usize,
    op_status: ahma_common::daemon_hub::OpStatus,
    success: bool,
    summary: String,
    exit_code: Option<i64>,
    started: std::time::Instant,
}

/// Emits both the daemon-hub completion record (if this window is a
/// reported `!` command) and the TUI's own `WindowFinished` event, so every
/// exit path of `spawn_window_cli_task` updates both places the same way.
async fn finish_window(
    tx: &Sender<BridgeEvent>,
    report: &Option<BangReport>,
    outcome: WindowOutcome,
) {
    let WindowOutcome {
        window_id,
        op_status,
        success,
        summary,
        exit_code,
        started,
    } = outcome;
    if let Some(r) = report {
        r.finished(op_status, &summary, exit_code, started);
    }
    let _ = tx
        .send(BridgeEvent::WindowFinished {
            window_id,
            success,
            summary,
        })
        .await;
}

pub fn spawn_window_cli_task(
    window_id: usize,
    command_str: String,
    working_dir: String,
    mut abort_rx: tokio::sync::oneshot::Receiver<()>,
    tx: Sender<BridgeEvent>,
    report: Option<BangReport>,
) {
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        // Shell selection goes through the cross-crate chokepoint
        // (`platform_shell_program`, see AGENTS.md); only the one-shot flags
        // are chosen here.
        let shell = ahma_mcp::shell_pool::platform_shell_program();
        let mut cmd = tokio::process::Command::new(shell);
        if cfg!(target_os = "windows") {
            cmd.arg("-NoProfile").arg("-Command").arg(&command_str);
        } else {
            cmd.arg("-c").arg(&command_str);
        }
        cmd.current_dir(&working_dir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // This child is *owned* by the window, so it dies with it (SPEC R-PROC.1):
        // kill_on_drop covers the task being dropped, and the process group lets
        // an explicit cancel reap the shell's descendants too, instead of
        // orphaning whatever it started (SPEC R-PROC.2).
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let summary = format!("Failed to spawn: {e}");
                finish_window(
                    &tx,
                    &report,
                    WindowOutcome {
                        window_id,
                        op_status: ahma_common::daemon_hub::OpStatus::Failed,
                        success: false,
                        summary,
                        exit_code: None,
                        started,
                    },
                )
                .await;
                return;
            }
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        use tokio::io::AsyncBufReadExt;
        let mut stdout_reader = tokio::io::BufReader::new(stdout).lines();
        let mut stderr_reader = tokio::io::BufReader::new(stderr).lines();

        let tx_clone = tx.clone();
        let report_out = report.clone();
        let stdout_handle = tokio::spawn(async move {
            while let Ok(Some(line)) = stdout_reader.next_line().await {
                if let Some(r) = &report_out {
                    r.output(&line, false);
                }
                let _ = tx_clone
                    .send(BridgeEvent::WindowOutput { window_id, line })
                    .await;
            }
        });

        let tx_clone2 = tx.clone();
        let report_err = report.clone();
        let stderr_handle = tokio::spawn(async move {
            while let Ok(Some(line)) = stderr_reader.next_line().await {
                if let Some(r) = &report_err {
                    r.output(&line, true);
                }
                let _ = tx_clone2
                    .send(BridgeEvent::WindowOutput { window_id, line })
                    .await;
            }
        });

        let wait_loop = child.wait();

        tokio::select! {
            biased;
            _ = &mut abort_rx => {
                let _ = ahma_mcp::shell_pool::kill_process_tree(&mut child).await;
                let _ = stdout_handle.await;
                let _ = stderr_handle.await;
                finish_window(
                    &tx,
                    &report,
                    WindowOutcome {
                        window_id,
                        op_status: ahma_common::daemon_hub::OpStatus::Cancelled,
                        success: false,
                        summary: "Cancelled".to_string(),
                        exit_code: None,
                        started,
                    },
                )
                .await;
            }
            res = wait_loop => {
                let _ = stdout_handle.await;
                let _ = stderr_handle.await;
                match res {
                    Ok(status) => {
                        let success = status.success();
                        let summary = if success {
                            "Completed".to_string()
                        } else {
                            format!("Failed with exit code {:?}", status.code())
                        };
                        let op_status = if success {
                            ahma_common::daemon_hub::OpStatus::Completed
                        } else {
                            ahma_common::daemon_hub::OpStatus::Failed
                        };
                        finish_window(
                            &tx,
                            &report,
                            WindowOutcome {
                                window_id,
                                op_status,
                                success,
                                summary,
                                exit_code: status.code().map(i64::from),
                                started,
                            },
                        )
                        .await;
                    }
                    Err(e) => {
                        let summary = format!("Execution error: {e}");
                        finish_window(
                            &tx,
                            &report,
                            WindowOutcome {
                                window_id,
                                op_status: ahma_common::daemon_hub::OpStatus::Failed,
                                success: false,
                                summary,
                                exit_code: None,
                                started,
                            },
                        )
                        .await;
                    }
                }
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_window_llm_task(
    window_id: usize,
    base_url: String,
    model: String,
    instructions: String,
    mcp: Option<McpChatConfig>,
    mut abort_rx: tokio::sync::oneshot::Receiver<()>,
    tx: Sender<BridgeEvent>,
) {
    tokio::spawn(async move {
        let client = build_configured_client(base_url, model);
        let system_msg = "You are a reasoning agent performing a subtask. Follow the instructions carefully and output the results.";
        let messages = vec![ChatMessage::user(instructions)];

        let (core_tx, mut core_rx) = tokio::sync::mpsc::channel(100);
        ahma_core::agent::spawn_chat_task(
            client,
            messages,
            Some(system_msg.to_string()),
            mcp,
            core_tx,
        );

        loop {
            tokio::select! {
                biased;
                _ = &mut abort_rx => {
                    let _ = tx.send(BridgeEvent::WindowFinished {
                        window_id,
                        success: false,
                        summary: "Cancelled".to_string(),
                    }).await;
                    return;
                }
                res = core_rx.recv() => {
                    match res {
                        Some(ahma_core::agent::AgentEvent::Token(token)) if !token.is_empty() => {
                            let _ = tx.send(BridgeEvent::WindowOutput {
                                window_id,
                                line: token,
                            }).await;
                        }
                        Some(ahma_core::agent::AgentEvent::Error(e)) => {
                            let _ = tx.send(BridgeEvent::WindowFinished {
                                window_id,
                                success: false,
                                summary: e.to_string(),
                            }).await;
                            return;
                        }
                        Some(ahma_core::agent::AgentEvent::Done) | None => {
                            let _ = tx.send(BridgeEvent::WindowFinished {
                                window_id,
                                success: true,
                                summary: "Done".to_string(),
                            }).await;
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `!` command's whole life reaches the daemon: the lines it printed and
    /// the code it exited with, not just the fact that it happened.
    ///
    /// Without this the unified view could show that the user ran something
    /// unconfined and nothing about how it went — which is the half that
    /// matters after the window has scrolled away (SPEC R-DAEMON.9).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_reported_window_command_streams_its_output_and_exit_code() {
        use ahma_common::daemon_hub::{ClientMsg, DaemonEvent, DaemonMsg, HubServer, recv_msg};
        use ahma_common::timeouts::TestTimeouts;

        let dir = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process (SPEC R-ISO.1).
        unsafe { std::env::set_var("AHMA_DAEMON_SOCK", dir.path().join("d.sock")) };
        if let Ok(l) = std::net::TcpListener::bind("127.0.0.1:0")
            && let Ok(addr) = l.local_addr()
        {
            // SAFETY: as above.
            unsafe { std::env::set_var("AHMA_DAEMON_PORT", addr.port().to_string()) };
        }

        let hub = HubServer::bind_at(ahma_common::daemon_hub::default_socket_path())
            .await
            .expect("this test owns a freshly isolated socket");
        let hub_task = tokio::spawn(hub.serve());

        let sub = ahma_common::daemon_hub::connect_to_daemon().await.unwrap();
        let (sr, mut sw) = tokio::io::split(sub);
        let mut sub_reader = tokio::io::BufReader::new(sr);
        ahma_common::daemon_hub::send_msg(&mut sw, &ClientMsg::Subscribe)
            .await
            .unwrap();
        let _snapshot = recv_msg::<_, DaemonMsg>(&mut sub_reader).await.unwrap();

        let reporter = crate::tui_reporter::spawn_tui_reporter(dir.path().display().to_string());
        let op_id = crate::tui_reporter::next_bang_op_id(reporter.session_id());
        let report = BangReport {
            reporter: reporter.clone(),
            op_id: op_id.clone(),
        };
        reporter.report(crate::tui_reporter::bang_started(
            &op_id,
            "echo hello; exit 3",
            &dir.path().display().to_string(),
        ));

        let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        spawn_window_cli_task(
            7,
            "echo hello; exit 3".to_string(),
            dir.path().display().to_string(),
            abort_rx,
            tx,
            Some(report),
        );

        let mut line = None;
        let mut exit = None;
        let deadline = tokio::time::Instant::now() + TestTimeouts::scale_secs(15);
        while tokio::time::Instant::now() < deadline && (line.is_none() || exit.is_none()) {
            let Ok(Ok(msg)) = tokio::time::timeout(
                TestTimeouts::scale_secs(5),
                recv_msg::<_, DaemonMsg>(&mut sub_reader),
            )
            .await
            else {
                break;
            };
            if let DaemonMsg::Event { payload, .. } = msg {
                match payload {
                    DaemonEvent::OpOutput { id, line: l, .. } if id == op_id => line = Some(l),
                    DaemonEvent::OpFinished { id, exit_code, .. } if id == op_id => {
                        exit = exit_code
                    }
                    _ => {}
                }
            }
        }

        rx.close();
        hub_task.abort();

        assert_eq!(
            line.as_deref(),
            Some("hello"),
            "the output must reach the hub"
        );
        assert_eq!(exit, Some(3), "and so must the exit code");
    }

    /// Regression test (SPEC R-PROC.2): cancelling a window command must reap the
    /// shell's **descendants**, not just the shell.
    ///
    /// `child.kill()` signals one pid, so cancelling a window running
    /// `bash -c "cargo build"` used to reap `bash` and leave `cargo`/`rustc`
    /// running — detached from any surface that could show or stop them.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_a_window_reaps_the_grandchild_too() {
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");

        let (abort_tx, abort_rx) = tokio::sync::oneshot::channel();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

        // The shell backgrounds a grandchild, records its pid, then waits.
        spawn_window_cli_task(
            1,
            format!("sleep 300 & echo $! > {}; wait", pidfile.display()),
            dir.path().display().to_string(),
            abort_rx,
            tx,
            None,
        );

        let mut gpid = None;
        for _ in 0..200 {
            if let Ok(s) = std::fs::read_to_string(&pidfile)
                && let Ok(p) = s.trim().parse::<i32>()
            {
                gpid = Some(p);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let gpid = gpid.expect("grandchild pid file should be written");

        // Signal 0 only probes for existence.
        assert_eq!(
            unsafe { libc::kill(gpid, 0) },
            0,
            "grandchild should be alive before the cancel"
        );

        abort_tx.send(()).expect("abort receiver should be live");

        let mut dead = false;
        for _ in 0..200 {
            if unsafe { libc::kill(gpid, 0) } != 0 {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        rx.close();

        assert!(
            dead,
            "grandchild (pid {gpid}) must be reaped via the process-group kill, \
             not orphaned to outlive the cancelled window"
        );
    }
}
