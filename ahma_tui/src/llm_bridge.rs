use ahma_llm_monitor::ChatMessage;
use ahma_llm_monitor::LocalProvider;
use ahma_llm_monitor::client::LlmClient;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

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
        steps: Vec<ahma_task_tree::parser::ParsedStep>,
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
        let id = format!(
            "call_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );
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

        let builder = reqwest::Client::builder();
        let (request_base_url, builder) = if let Some(path) = mcp.base_url.strip_prefix("unix://") {
            #[cfg(unix)]
            {
                ("http://localhost".to_string(), builder.unix_socket(path))
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                (mcp.base_url.clone(), builder)
            }
        } else {
            (mcp.base_url.clone(), builder)
        };
        let client = match builder.build() {
            Ok(c) => c,
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
                let fresh_mcp = McpChatConfig {
                    session_id: None,
                    ..mcp.clone()
                };
                match ahma_core::agent::get_or_create_session(&client, &url, &fresh_mcp).await {
                    Ok(new_sid) => {
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

pub fn spawn_decompose_task(client: LlmClient, goal: String, tx: Sender<BridgeEvent>) {
    tokio::spawn(async move {
        let prompt = ahma_task_tree::prompt::build_planning_prompt(
            &goal,
            &goal,
            "No prior task context available.",
            5,
        );
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
                let steps_res = ahma_task_tree::parser::parse_steps(&completion.content);
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
        let mut cmd = if cfg!(target_os = "windows") {
            let mut c = tokio::process::Command::new("powershell");
            c.arg("-NoProfile").arg("-Command").arg(&command_str);
            c
        } else {
            let mut c = tokio::process::Command::new("bash");
            c.arg("-c").arg(&command_str);
            c
        };
        cmd.current_dir(&working_dir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

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
                let _ = child.kill().await;
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
    num_ctx: Option<u32>,
    instructions: String,
    mcp: Option<McpChatConfig>,
    mut abort_rx: tokio::sync::oneshot::Receiver<()>,
    tx: Sender<BridgeEvent>,
) {
    tokio::spawn(async move {
        let client = LlmClient::new(base_url, model, None).with_num_ctx(num_ctx);
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
