use ahma_llm_monitor::ChatMessage;
use ahma_llm_monitor::LocalProvider;
use ahma_llm_monitor::client::LlmClient;
use std::sync::Arc;
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
    let client = LlmClient::new(base_url.clone(), model.into(), None);
    let client = match config.kind_for_base_url(&base_url) {
        Some(ProviderKind::Anthropic) => client.with_flavor(ahma_llm_monitor::ApiFlavor::Anthropic),
        Some(ProviderKind::OpenAi) => client.with_flavor(ahma_llm_monitor::ApiFlavor::OpenAi),
        None => client,
    };
    client.with_num_ctx(config.num_ctx_for_base_url(&base_url))
}

pub enum BridgeEvent {
    Token(String),
    /// Reasoning / "thinking" fragment, rendered in lower contrast.
    Thinking(String),
    Done,
    Error(String),
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
    RequestApproval {
        id: String,
        tool: String,
        args: String,
        tx: tokio::sync::oneshot::Sender<bool>,
    },
    Usage(ahma_llm_monitor::client::TokenUsage),
    /// The model's response was cut off by a length/context limit; the agent
    /// is requesting a continuation. See [`ahma_core::agent::AgentEvent::Truncated`].
    Truncated {
        reason: String,
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

struct TuiApprovalGate {
    tx: Sender<BridgeEvent>,
}

#[async_trait::async_trait]
impl ahma_core::agent::AgentApprovalGate for TuiApprovalGate {
    async fn request_approval(&self, id: &str, tool: &str, args: &str) -> bool {
        let (approval_tx, approval_rx) = tokio::sync::oneshot::channel();
        let _ = self
            .tx
            .send(BridgeEvent::RequestApproval {
                id: id.to_string(),
                tool: tool.to_string(),
                args: args.to_string(),
                tx: approval_tx,
            })
            .await;
        approval_rx.await.unwrap_or(false)
    }
}

pub fn spawn_agent_task(
    client: LlmClient,
    messages: Vec<ChatMessage>,
    system_prompt: Option<String>,
    mcp: Option<McpChatConfig>,
    available_tools: Vec<crate::mcp_connections::ToolInfo>,
    tx: Sender<BridgeEvent>,
) {
    let (core_tx, mut core_rx) = tokio::sync::mpsc::channel(100);
    let gate = Arc::new(TuiApprovalGate { tx: tx.clone() });

    ahma_core::agent::spawn_agent_task(
        client,
        messages,
        system_prompt,
        mcp,
        available_tools,
        core_tx,
        gate,
    );

    tokio::spawn(async move {
        while let Some(evt) = core_rx.recv().await {
            let bridge_evt = match evt {
                ahma_core::agent::AgentEvent::Token(t) => BridgeEvent::Token(t),
                ahma_core::agent::AgentEvent::Thinking(t) => BridgeEvent::Thinking(t),
                ahma_core::agent::AgentEvent::Done => BridgeEvent::Done,
                ahma_core::agent::AgentEvent::Error(e) => BridgeEvent::Error(e),
                ahma_core::agent::AgentEvent::ToolCallStarted { id, name, args } => {
                    BridgeEvent::ToolCallStarted { id, name, args }
                }
                ahma_core::agent::AgentEvent::ToolCallFinished { id, result, failed } => {
                    BridgeEvent::ToolCallFinished { id, result, failed }
                }
                ahma_core::agent::AgentEvent::Usage(u) => BridgeEvent::Usage(u),
                ahma_core::agent::AgentEvent::Truncated { reason } => {
                    BridgeEvent::Truncated { reason }
                }
            };
            let _ = tx.send(bridge_evt).await;
        }
    });
}

pub fn spawn_chat_task(
    client: LlmClient,
    messages: Vec<ChatMessage>,
    system_prompt: Option<String>,
    mcp: Option<McpChatConfig>,
    tx: Sender<BridgeEvent>,
) {
    let (core_tx, mut core_rx) = tokio::sync::mpsc::channel(100);
    ahma_core::agent::spawn_chat_task(client, messages, system_prompt, mcp, core_tx);
    tokio::spawn(async move {
        while let Some(evt) = core_rx.recv().await {
            let bridge_evt = match evt {
                ahma_core::agent::AgentEvent::Token(t) => BridgeEvent::Token(t),
                ahma_core::agent::AgentEvent::Thinking(t) => BridgeEvent::Thinking(t),
                ahma_core::agent::AgentEvent::Done => BridgeEvent::Done,
                ahma_core::agent::AgentEvent::Error(e) => BridgeEvent::Error(e),
                ahma_core::agent::AgentEvent::ToolCallStarted { id, name, args } => {
                    BridgeEvent::ToolCallStarted { id, name, args }
                }
                ahma_core::agent::AgentEvent::ToolCallFinished { id, result, failed } => {
                    BridgeEvent::ToolCallFinished { id, result, failed }
                }
                ahma_core::agent::AgentEvent::Usage(u) => BridgeEvent::Usage(u),
                ahma_core::agent::AgentEvent::Truncated { reason } => {
                    BridgeEvent::Truncated { reason }
                }
            };
            let _ = tx.send(bridge_evt).await;
        }
    });
}

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
        let client = LlmClient::new(base_url.clone(), "", None);
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
            let res = crate::connection::ensure_server_running(Some(&mcp.workspace_root)).await;
            if let Err(e) = res {
                let _ = tx
                    .send(BridgeEvent::ToolCallFinished {
                        id: id.clone(),
                        result: format!("Error ensuring bridge server is running: {e}"),
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
                // The cached/reused session was rejected — drop it from the
                // process-wide cache too, or get_or_create_session would just
                // hand back the same dead id for `fresh_mcp` below.
                ahma_core::agent::invalidate_cached_session(&url, &mcp.workspace_root);
                let fresh_mcp = McpChatConfig {
                    session_id: None,
                    ..mcp.clone()
                };
                match ahma_core::agent::get_or_create_session(&client, &url, &fresh_mcp).await {
                    Ok(new_sid) => {
                        // The old session_id was rejected (403); replace it in
                        // app state so subsequent calls don't keep retrying a
                        // dead session before falling back here every time.
                        let _ = tx
                            .send(BridgeEvent::SessionEstablished {
                                session_id: new_sid.clone(),
                            })
                            .await;
                        match ahma_core::agent::call_mcp_tool_http(
                            &client, &url, &new_sid, &tool, arguments,
                        )
                        .await
                        {
                            Ok((result, failed)) => (result, failed),
                            Err(e) => (format!("Error: {e}"), true),
                        }
                    }
                    Err(e) => (format!("Error: {e}"), true),
                }
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
            .chat_completion_with_tools(vec![system_msg, user_msg], &[])
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

pub fn spawn_window_cli_task(
    window_id: usize,
    command_str: String,
    working_dir: String,
    mut abort_rx: tokio::sync::oneshot::Receiver<()>,
    tx: Sender<BridgeEvent>,
) {
    tokio::spawn(async move {
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
                let _ = tx
                    .send(BridgeEvent::WindowFinished {
                        window_id,
                        success: false,
                        summary: format!("Failed to spawn: {e}"),
                    })
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
        let stdout_handle = tokio::spawn(async move {
            while let Ok(Some(line)) = stdout_reader.next_line().await {
                let _ = tx_clone
                    .send(BridgeEvent::WindowOutput { window_id, line })
                    .await;
            }
        });

        let tx_clone2 = tx.clone();
        let stderr_handle = tokio::spawn(async move {
            while let Ok(Some(line)) = stderr_reader.next_line().await {
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
                let _ = tx.send(BridgeEvent::WindowFinished {
                    window_id,
                    success: false,
                    summary: "Cancelled".to_string(),
                }).await;
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
                        let _ = tx.send(BridgeEvent::WindowFinished {
                            window_id,
                            success,
                            summary,
                        }).await;
                    }
                    Err(e) => {
                        let _ = tx.send(BridgeEvent::WindowFinished {
                            window_id,
                            success: false,
                            summary: format!("Execution error: {e}"),
                        }).await;
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
                                summary: format!("LLM error: {e}"),
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
