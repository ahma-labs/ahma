use ahma_llm_monitor::ChatMessage;
use ahma_llm_monitor::LocalProvider;
use ahma_llm_monitor::client::LlmClient;
use futures::future::join_all;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::sync::mpsc::Sender;

pub enum BridgeEvent {
    Token(String),
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
}

#[derive(Clone)]
pub struct McpChatConfig {
    pub base_url: String,
    pub workspace_root: PathBuf,
    pub session_id: Option<String>,
    pub external_http_servers: BTreeMap<String, String>,
    pub max_turns: u32,
    pub tool_approval: bool,
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
        let models = client.list_models().await;
        let _ = tx
            .send(BridgeEvent::ModelsRefreshed { base_url, models })
            .await;
    });
}

pub fn spawn_chat_task(
    client: LlmClient,
    messages: Vec<ChatMessage>,
    system_prompt: Option<String>,
    _mcp: Option<McpChatConfig>,
    tx: Sender<BridgeEvent>,
) {
    tokio::spawn(async move {
        let stream = client.chat_stream(messages, system_prompt.as_deref());
        tokio::pin!(stream);
        use futures::StreamExt;
        while let Some(res) = stream.next().await {
            match res {
                Ok(token) => {
                    if !token.is_empty() {
                        let _ = tx.send(BridgeEvent::Token(token)).await;
                    }
                }
                Err(e) => {
                    let _ = tx.send(BridgeEvent::Error(e.to_string())).await;
                    return;
                }
            }
        }
        let _ = tx.send(BridgeEvent::Done).await;
    });
}

pub fn spawn_agent_task(
    client: LlmClient,
    messages: Vec<ChatMessage>,
    system_prompt: Option<String>,
    mcp: Option<McpChatConfig>,
    available_tools: Vec<String>,
    tx: Sender<BridgeEvent>,
) {
    tokio::spawn(async move {
        let mut msg_json: Vec<serde_json::Value> = Vec::new();
        if let Some(ref system) = system_prompt {
            msg_json.push(serde_json::json!({"role": "system", "content": system}));
        }
        for msg in &messages {
            msg_json.push(serde_json::json!({"role": msg.role, "content": msg.content}));
        }

        let tool_defs: Vec<serde_json::Value> = available_tools
            .into_iter()
            .map(|name| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": "MCP tool callable from ahma",
                        "parameters": {
                            "type": "object",
                            "additionalProperties": true
                        }
                    }
                })
            })
            .collect();

        let max_turns = mcp.as_ref().map(|c| c.max_turns).unwrap_or(8);
        for _ in 0..max_turns {
            let completion = match client
                .chat_completion_with_tools(msg_json.clone(), &tool_defs)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    let err_msg = e.to_string().to_lowercase();
                    if err_msg.contains("400")
                        || err_msg.contains("tool")
                        || err_msg.contains("not supported")
                    {
                        let _ = tx
                            .send(BridgeEvent::Error(
                                "Model does not support tools. Falling back to standard chat."
                                    .to_string(),
                            ))
                            .await;
                        spawn_chat_task(
                            client.clone(),
                            messages.clone(),
                            system_prompt.clone(),
                            mcp.clone(),
                            tx.clone(),
                        );
                        return;
                    }
                    let _ = tx.send(BridgeEvent::Error(e.to_string())).await;
                    return;
                }
            };

            let assistant_content = completion.content.clone();
            msg_json.push(serde_json::json!({
                "role": "assistant",
                "content": assistant_content,
                "tool_calls": completion.assistant_message.get("tool_calls").cloned().unwrap_or(serde_json::Value::Null)
            }));

            if completion.tool_calls.is_empty() {
                if !completion.content.is_empty() {
                    let _ = tx.send(BridgeEvent::Token(completion.content)).await;
                }
                let _ = tx.send(BridgeEvent::Done).await;
                return;
            }

            let Some(mcp_cfg) = mcp.clone() else {
                let _ = tx
                    .send(BridgeEvent::Error(
                        "Model requested tools but MCP is not configured".to_string(),
                    ))
                    .await;
                return;
            };

            let calls = completion.tool_calls;
            let call_futures = calls.into_iter().map(|call| {
                let tx_clone = tx.clone();
                let cfg = mcp_cfg.clone();
                async move {
                    let args_value = call.arguments;
                    let args_str = serde_json::to_string(&args_value).unwrap_or_default();

                    let approved = if needs_approval(&call.name, cfg.tool_approval) {
                        let (approval_tx, approval_rx) = tokio::sync::oneshot::channel();
                        let _ = tx_clone
                            .send(BridgeEvent::RequestApproval {
                                id: call.id.clone(),
                                tool: call.name.clone(),
                                args: args_str.clone(),
                                tx: approval_tx,
                            })
                            .await;
                        approval_rx.await.unwrap_or(false)
                    } else {
                        true
                    };

                    if !approved {
                        let err_text = "Error: Tool execution rejected by user".to_string();
                        let _ = tx_clone
                            .send(BridgeEvent::ToolCallFinished {
                                id: call.id.clone(),
                                result: err_text.clone(),
                                failed: true,
                            })
                            .await;
                        return (
                            call.id,
                            call.name,
                            serde_json::json!({"error": err_text}),
                            true,
                        );
                    }

                    let _ = tx_clone
                        .send(BridgeEvent::ToolCallStarted {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            args: args_str,
                        })
                        .await;

                    let result = if let Some((server, tool)) = call.name.split_once("::") {
                        if let Some(base_url) = cfg.external_http_servers.get(server) {
                            spawn_external_tool_call_http(base_url, tool, args_value).await
                        } else {
                            Err(format!("Unknown external MCP server `{server}`"))
                        }
                    } else {
                        spawn_local_tool_call(cfg, &call.name, args_value).await
                    };
                    match result {
                        Ok((text, failed)) => {
                            let _ = tx_clone
                                .send(BridgeEvent::ToolCallFinished {
                                    id: call.id.clone(),
                                    result: text.clone(),
                                    failed,
                                })
                                .await;
                            (
                                call.id,
                                call.name,
                                serde_json::json!({"output": text}),
                                failed,
                            )
                        }
                        Err(e) => {
                            let err_text = format!("Error: {e}");
                            let _ = tx_clone
                                .send(BridgeEvent::ToolCallFinished {
                                    id: call.id.clone(),
                                    result: err_text.clone(),
                                    failed: true,
                                })
                                .await;
                            (
                                call.id,
                                call.name,
                                serde_json::json!({"error": err_text}),
                                true,
                            )
                        }
                    }
                }
            });

            let tool_results = join_all(call_futures).await;
            for (tool_call_id, _tool_name, payload, _failed) in tool_results {
                msg_json.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": payload.to_string()
                }));
            }
        }

        let _ = tx
            .send(BridgeEvent::Error(
                "Agent loop reached max turns without completion".to_string(),
            ))
            .await;
    });
}

async fn spawn_local_tool_call(
    mcp: McpChatConfig,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<(String, bool), String> {
    let client = reqwest::Client::new();
    let url = format!("{}/mcp", mcp.base_url);
    let session_id = get_or_create_session(&client, &url, &mcp).await?;
    call_mcp_tool_http(&client, &url, &session_id, tool, arguments).await
}

async fn spawn_external_tool_call_http(
    base_url: &str,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<(String, bool), String> {
    let client = reqwest::Client::new();
    let url = format!("{}/mcp", base_url.trim_end_matches('/'));
    let sid = get_or_create_external_session(&client, &url).await?;
    call_mcp_tool_http(&client, &url, &sid, tool, arguments).await
}

async fn get_or_create_external_session(
    client: &reqwest::Client,
    url: &str,
) -> Result<String, String> {
    let init_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "roots": { "listChanged": false } },
            "clientInfo": { "name": "ahma-tui-external-tool", "version": env!("CARGO_PKG_VERSION") }
        }
    });

    let resp = client
        .post(url)
        .json(&init_body)
        .send()
        .await
        .map_err(|e| format!("Failed to initialize external session: {e}"))?;

    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "No mcp-session-id header in external initialize response".to_string())?
        .to_string();

    let initialized_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });

    let _ = client
        .post(url)
        .header("mcp-session-id", &sid)
        .json(&initialized_body)
        .send()
        .await;

    Ok(sid)
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

        let client = reqwest::Client::new();
        let url = format!("{}/mcp", mcp.base_url);

        let session_id = match get_or_create_session(&client, &url, &mcp).await {
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

        match call_mcp_tool_http(&client, &url, &session_id, &tool, arguments).await {
            Ok((result, failed)) => {
                let _ = tx
                    .send(BridgeEvent::ToolCallFinished { id, result, failed })
                    .await;
            }
            Err(err) => {
                let _ = tx
                    .send(BridgeEvent::ToolCallFinished {
                        id,
                        result: format!("Error: {err}"),
                        failed: true,
                    })
                    .await;
            }
        }
    });
}

async fn get_or_create_session(
    client: &reqwest::Client,
    url: &str,
    mcp: &McpChatConfig,
) -> Result<String, String> {
    if let Some(sid) = &mcp.session_id {
        return Ok(sid.clone());
    }

    // Try to initialize a new session
    let init_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "roots": { "listChanged": false } },
            "clientInfo": { "name": "ahma-tui-tool", "version": env!("CARGO_PKG_VERSION") }
        }
    });

    let resp = client
        .post(url)
        .json(&init_body)
        .send()
        .await
        .map_err(|e| format!("Failed to initialize session: {e}"))?;

    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "No mcp-session-id header in response".to_string())?
        .to_string();

    let initialized_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });

    let _ = client
        .post(url)
        .header("mcp-session-id", &sid)
        .json(&initialized_body)
        .send()
        .await;

    Ok(sid)
}

async fn call_mcp_tool_http(
    client: &reqwest::Client,
    url: &str,
    session_id: &str,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<(String, bool), String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": arguments
        }
    });

    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("mcp-session-id", session_id)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {text}"));
    }

    let json_resp = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("Failed to parse tool response JSON: {e}"))?;

    let result_val = json_resp.get("result");
    let is_error = json_resp.get("error").is_some()
        || (result_val
            .and_then(|r| r.get("isError"))
            .and_then(|e| e.as_bool())
            .unwrap_or(false));

    let content_str = if let Some(err) = json_resp.get("error") {
        format!(
            "Error: {}",
            err.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
        )
    } else if let Some(res) = result_val {
        if let Some(content_array) = res.get("content").and_then(|c| c.as_array()) {
            let mut texts = Vec::new();
            for item in content_array {
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    texts.push(t.to_string());
                }
            }
            texts.join("\n")
        } else {
            serde_json::to_string_pretty(res).unwrap_or_default()
        }
    } else {
        "Empty result".to_string()
    };

    Ok((content_str, is_error))
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
        let stdout_loop = async {
            while let Ok(Some(line)) = stdout_reader.next_line().await {
                let _ = tx_clone
                    .send(BridgeEvent::WindowOutput { window_id, line })
                    .await;
            }
        };

        let tx_clone2 = tx.clone();
        let stderr_loop = async {
            while let Ok(Some(line)) = stderr_reader.next_line().await {
                let _ = tx_clone2
                    .send(BridgeEvent::WindowOutput { window_id, line })
                    .await;
            }
        };

        let wait_loop = child.wait();

        tokio::select! {
            biased;
            _ = &mut abort_rx => {
                let _ = child.kill().await;
                let _ = tx.send(BridgeEvent::WindowFinished {
                    window_id,
                    success: false,
                    summary: "Cancelled".to_string(),
                }).await;
            }
            res = wait_loop => {
                let _ = tokio::join!(stdout_loop, stderr_loop);
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

pub fn spawn_window_llm_task(
    window_id: usize,
    base_url: String,
    model: String,
    instructions: String,
    mut abort_rx: tokio::sync::oneshot::Receiver<()>,
    tx: Sender<BridgeEvent>,
) {
    tokio::spawn(async move {
        let client = LlmClient::new(base_url, model, None);
        let system_msg = "You are a reasoning agent performing a subtask. Follow the instructions carefully and output the results.";
        let messages = vec![ChatMessage::user(instructions)];

        let stream = client.chat_stream(messages, Some(system_msg));
        tokio::pin!(stream);

        use futures::StreamExt;

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
                res = stream.next() => {
                    match res {
                        Some(Ok(token)) => {
                            if !token.is_empty() {
                                let _ = tx.send(BridgeEvent::WindowOutput {
                                    window_id,
                                    line: token,
                                }).await;
                            }
                        }
                        Some(Err(e)) => {
                            let _ = tx.send(BridgeEvent::WindowFinished {
                                window_id,
                                success: false,
                                summary: format!("LLM error: {e}"),
                            }).await;
                            return;
                        }
                        None => {
                            let _ = tx.send(BridgeEvent::WindowFinished {
                                window_id,
                                success: true,
                                summary: "Done".to_string(),
                            }).await;
                            return;
                        }
                    }
                }
            }
        }
    });
}

pub(crate) fn needs_approval(tool_name: &str, tool_approval_enabled: bool) -> bool {
    if tool_approval_enabled {
        return true;
    }
    tool_name == "write_file"
        || tool_name == "replace_in_file"
        || tool_name.ends_with("::write_file")
        || tool_name.ends_with("::replace_in_file")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_agent_task_tool_call_loop() {
        let llm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = llm_listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = llm_listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                let response = serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "",
                            "tool_calls": [{
                                "id": "call_123",
                                "type": "function",
                                "function": {
                                    "name": "test_tool",
                                    "arguments": "{\"arg\":\"value\"}"
                                }
                            }]
                        }
                    }]
                });
                let resp_str = serde_json::to_string(&response).unwrap();
                let http_resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
                    resp_str.len(),
                    resp_str
                );
                let _ =
                    tokio::io::AsyncWriteExt::write_all(&mut socket, http_resp.as_bytes()).await;
            }

            if let Ok((mut socket, _)) = llm_listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                let response = serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "Tool call was successful.",
                        }
                    }]
                });
                let resp_str = serde_json::to_string(&response).unwrap();
                let http_resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
                    resp_str.len(),
                    resp_str
                );
                let _ =
                    tokio::io::AsyncWriteExt::write_all(&mut socket, http_resp.as_bytes()).await;
            }
        });

        let mcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mcp_addr = mcp_listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = mcp_listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                let http_resp = "HTTP/1.1 200 OK\r\nmcp-session-id: test-session-123\r\nContent-Length: 2\r\nContent-Type: application/json\r\n\r\n{}";
                let _ =
                    tokio::io::AsyncWriteExt::write_all(&mut socket, http_resp.as_bytes()).await;
            }

            if let Ok((mut socket, _)) = mcp_listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                let http_resp = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                let _ =
                    tokio::io::AsyncWriteExt::write_all(&mut socket, http_resp.as_bytes()).await;
            }

            if let Ok((mut socket, _)) = mcp_listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                let response = serde_json::json!({
                    "result": {
                        "content": [{"type": "text", "text": "Tool executed"}]
                    }
                });
                let resp_str = serde_json::to_string(&response).unwrap();
                let http_resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
                    resp_str.len(),
                    resp_str
                );
                let _ =
                    tokio::io::AsyncWriteExt::write_all(&mut socket, http_resp.as_bytes()).await;
            }
        });

        let client = LlmClient::new(format!("http://{}", llm_addr), "test-model", None);
        let mcp = McpChatConfig {
            base_url: format!("http://{}", mcp_addr),
            workspace_root: PathBuf::from("/tmp"),
            session_id: None,
            external_http_servers: BTreeMap::new(),
            max_turns: 2,
            tool_approval: false,
        };

        let (tx, mut rx) = mpsc::channel(100);
        let messages = vec![ChatMessage::user("Do something")];

        spawn_agent_task(
            client,
            messages,
            None,
            Some(mcp),
            vec!["test_tool".to_string()],
            tx,
        );

        let mut events = Vec::new();
        while let Some(evt) = rx.recv().await {
            events.push(evt);
            if events.len() > 10 {
                break;
            }
        }

        let mut got_start = false;
        let mut got_finish = false;
        let mut got_token = false;
        let mut got_done = false;

        for evt in events {
            match evt {
                BridgeEvent::ToolCallStarted { name, .. } => {
                    assert_eq!(name, "test_tool");
                    got_start = true;
                }
                BridgeEvent::ToolCallFinished { result, failed, .. } => {
                    assert_eq!(result, "Tool executed");
                    assert!(!failed);
                    got_finish = true;
                }
                BridgeEvent::Token(t) => {
                    assert_eq!(t, "Tool call was successful.");
                    got_token = true;
                }
                BridgeEvent::Done => {
                    got_done = true;
                }
                BridgeEvent::Error(e) => panic!("Unexpected error: {e}"),
                _ => {}
            }
        }

        assert!(got_start, "Missing ToolCallStarted");
        assert!(got_finish, "Missing ToolCallFinished");
        assert!(got_token, "Missing Token");
        assert!(got_done, "Missing Done");
    }
}
