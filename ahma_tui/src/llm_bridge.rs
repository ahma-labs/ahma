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
    Usage(ahma_llm_monitor::client::TokenUsage),
}

#[derive(Clone)]
pub struct McpChatConfig {
    pub base_url: String,
    pub workspace_root: PathBuf,
    pub session_id: Option<String>,
    pub external_http_servers: BTreeMap<String, String>,
    pub max_turns: u32,
    pub tool_approval: bool,
    /// All external MCP servers (HTTP and stdio) for agent tool routing.
    pub mcp_connections: crate::mcp_connections::McpConnectionManager,
    pub minimize_tokens: bool,
    pub small_model_harness: bool,
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

async fn call_mcp_sampling_routed(
    mcp: &McpChatConfig,
    target_label: &str,
    messages: Vec<serde_json::Value>,
    system_prompt: Option<&str>,
) -> Result<ahma_llm_monitor::client::ChatCompletionResponse, String> {
    if is_local_default_server(&mcp.base_url) {
        crate::connection::ensure_server_running(Some(&mcp.workspace_root))
            .await
            .map_err(|e| format!("Failed to ensure bridge server is running: {e}"))?;
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
    let client = builder.build().map_err(|e| e.to_string())?;
    let url = format!("{}/mcp", request_base_url);
    let session_id = get_or_create_session(&client, &url, mcp).await?;

    let mut mcp_messages = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
        mcp_messages.push(serde_json::json!({
            "role": role,
            "content": {
                "type": "text",
                "text": content
            }
        }));
    }

    let request_id = format!("route_tui_{}", uuid::Uuid::new_v4());
    let mut params = serde_json::json!({
        "messages": mcp_messages,
    });
    if let Some(sys) = system_prompt {
        params["systemPrompt"] = serde_json::json!(sys);
    }
    params["__route_target_label"] = serde_json::json!(target_label);

    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "sampling/createMessage",
        "params": params
    });

    let resp = client
        .post(&url)
        .header("mcp-session-id", &session_id)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Failed to send sampling request: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body_text}"));
    }

    let response_json = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    if let Some(error) = response_json.get("error") {
        let msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown error");
        return Err(msg.to_string());
    }

    let result = response_json
        .get("result")
        .ok_or_else(|| "Missing result in response".to_string())?;
    let content_arr = result
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| "Missing or invalid content in result".to_string())?;

    let mut completion_text = String::new();
    for item in content_arr {
        if item.get("type").and_then(|t| t.as_str()) == Some("text")
            && let Some(text) = item.get("text").and_then(|t| t.as_str())
        {
            completion_text.push_str(text);
        }
    }

    Ok(ahma_llm_monitor::client::ChatCompletionResponse {
        content: completion_text,
        tool_calls: Vec::new(),
        assistant_message: serde_json::Value::Null,
        usage: None,
    })
}

pub fn spawn_chat_task(
    client: LlmClient,
    messages: Vec<ChatMessage>,
    system_prompt: Option<String>,
    mcp: Option<McpChatConfig>,
    tx: Sender<BridgeEvent>,
) {
    tokio::spawn(async move {
        if client.base_url().starts_with("mcp://") {
            let Some(mcp_cfg) = mcp else {
                let _ = tx
                    .send(BridgeEvent::Error(
                        "MCP config missing for sampling".to_string(),
                    ))
                    .await;
                return;
            };
            let target_label = client.base_url().strip_prefix("mcp://").unwrap_or("");
            let mut msg_vals = Vec::new();
            for msg in messages {
                msg_vals.push(serde_json::json!({
                    "role": msg.role,
                    "content": msg.content
                }));
            }
            match call_mcp_sampling_routed(
                &mcp_cfg,
                target_label,
                msg_vals,
                system_prompt.as_deref(),
            )
            .await
            {
                Ok(resp) => {
                    let _ = tx.send(BridgeEvent::Token(resp.content)).await;
                    let _ = tx.send(BridgeEvent::Done).await;
                }
                Err(e) => {
                    let _ = tx.send(BridgeEvent::Error(e)).await;
                }
            }
            return;
        }

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

fn prepare_tool_definitions(
    available_tools: Vec<crate::mcp_connections::ToolInfo>,
) -> Vec<serde_json::Value> {
    available_tools
        .into_iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description.unwrap_or_else(|| "MCP tool callable from ahma".to_string()),
                    "parameters": tool.input_schema
                }
            })
        })
        .collect()
}

async fn execute_single_tool_call(
    call: ahma_llm_monitor::client::ChatToolCall,
    cfg: McpChatConfig,
    tx: Sender<BridgeEvent>,
) -> (String, String, serde_json::Value, bool) {
    let args_value = call.arguments;
    let args_str = serde_json::to_string(&args_value).unwrap_or_default();

    let approved = if needs_approval(&call.name, cfg.tool_approval) {
        let (approval_tx, approval_rx) = tokio::sync::oneshot::channel();
        let _ = tx
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
        let _ = tx
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

    let _ = tx
        .send(BridgeEvent::ToolCallStarted {
            id: call.id.clone(),
            name: call.name.clone(),
            args: args_str,
        })
        .await;

    let result = dispatch_tool_execution(&call.name, args_value, &cfg).await;
    match result {
        Ok((text, failed)) => {
            let _ = tx
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
            let _ = tx
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

async fn dispatch_tool_execution(
    name: &str,
    args_value: serde_json::Value,
    cfg: &McpChatConfig,
) -> Result<(String, bool), String> {
    if let Some((server, _tool)) = name.split_once("::") {
        let conn = cfg.mcp_connections.clone();
        let conn_has_server = conn.servers.iter().any(|s| s.name == server);
        if conn_has_server {
            match conn.call_tool(name, args_value.clone()).await {
                Ok(pair) => Ok(pair),
                Err(e) => Err(format!("MCP tool error ({name}): {e}")),
            }
        } else if let Some(base_url) = cfg.external_http_servers.get(server) {
            let base = base_url.clone();
            spawn_external_tool_call_http(&base, _tool, args_value).await
        } else {
            Err(format!("Unknown external MCP server `{server}`"))
        }
    } else {
        spawn_local_tool_call(cfg.clone(), name, args_value).await
    }
}

/// Fetch an LLM completion for the current turn, routing through either the MCP sampling
/// protocol or the standard tool-calling API depending on the client URL scheme.
///
/// Returns `None` and sends the appropriate `BridgeEvent` to `tx` on error (including the
/// "tools not supported" fallback that re-launches a plain chat task).
async fn fetch_completion(
    client: &LlmClient,
    msg_json: &[serde_json::Value],
    tool_defs: &[serde_json::Value],
    mcp: &Option<McpChatConfig>,
    tx: &Sender<BridgeEvent>,
    messages: &[ChatMessage],
    system_prompt: &Option<String>,
) -> Option<ahma_llm_monitor::client::ChatCompletionResponse> {
    if client.base_url().starts_with("mcp://") {
        let Some(mcp_cfg) = mcp else {
            let _ = tx
                .send(BridgeEvent::Error(
                    "MCP config missing for sampling".to_string(),
                ))
                .await;
            return None;
        };
        let target_label = client.base_url().strip_prefix("mcp://").unwrap_or("");
        match call_mcp_sampling_routed(
            mcp_cfg,
            target_label,
            msg_json.to_vec(),
            system_prompt.as_deref(),
        )
        .await
        {
            Ok(c) => Some(c),
            Err(e) => {
                let _ = tx.send(BridgeEvent::Error(e)).await;
                None
            }
        }
    } else {
        match client
            .chat_completion_with_tools(msg_json.to_vec(), tool_defs)
            .await
        {
            Ok(c) => Some(c),
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
                        messages.to_vec(),
                        system_prompt.clone(),
                        mcp.clone(),
                        tx.clone(),
                    );
                } else {
                    let _ = tx.send(BridgeEvent::Error(e.to_string())).await;
                }
                None
            }
        }
    }
}

/// Determine which harness hints should be injected for this batch of tool results.
/// Returns `(inject_error_hint, inject_read_hint)`.
fn plan_harness_hints(
    tool_results: &[(String, String, serde_json::Value, bool)],
    error_hinted: bool,
    read_file_hinted: bool,
) -> (bool, bool) {
    let mut inject_error_hint = false;
    let mut inject_read_hint = false;
    for (_, tool_name, _, failed) in tool_results {
        if *failed && !error_hinted {
            inject_error_hint = true;
        }
        if (*tool_name == "read_file" || *tool_name == "list_dir") && !read_file_hinted {
            inject_read_hint = true;
        }
    }
    (inject_error_hint, inject_read_hint)
}

/// Append a coaching hint to the string at `key` inside a tool-result JSON object, if present.
fn append_hint_to_field(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    hint: &str,
) {
    if let Some(serde_json::Value::String(s)) = obj.get_mut(key) {
        s.push_str(hint);
    }
}

/// Apply any applicable harness hints to `payload`, then push a `tool` message onto `msg_json`.
#[allow(clippy::too_many_arguments)]
fn push_tool_message_with_hints(
    msg_json: &mut Vec<serde_json::Value>,
    tool_call_id: String,
    tool_name: &str,
    payload: serde_json::Value,
    failed: bool,
    inject_error_hint: bool,
    inject_read_hint: bool,
    error_hinted: &mut bool,
    read_file_hinted: &mut bool,
) {
    let mut final_payload = payload;
    if let Some(obj) = final_payload.as_object_mut() {
        if failed && inject_error_hint && !*error_hinted {
            *error_hinted = true;
            let key = if obj.contains_key("error") {
                "error"
            } else {
                "output"
            };
            append_hint_to_field(
                obj,
                key,
                "\n\u{1f4a1} [Harness Hint: The previous tool call failed. Carefully read the error output above. Ensure parameter values are correct, check for typo errors, and try a different approach.]",
            );
        }
        if (tool_name == "read_file" || tool_name == "list_dir")
            && inject_read_hint
            && !*read_file_hinted
        {
            *read_file_hinted = true;
            append_hint_to_field(
                obj,
                "output",
                "\n\u{1f4a1} [Harness Hint: When modifying files that already exist, you MUST use `replace_in_file` with exact old/new string matching. Avoid using `write_file` for existing files.]",
            );
        }
    }
    msg_json.push(serde_json::json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": final_payload.to_string()
    }));
}

#[allow(clippy::too_many_arguments)]
async fn execute_agent_turn(
    client: &LlmClient,
    msg_json: &mut Vec<serde_json::Value>,
    tool_defs: &[serde_json::Value],
    mcp: &Option<McpChatConfig>,
    tx: &Sender<BridgeEvent>,
    messages: &[ChatMessage],
    system_prompt: &Option<String>,
    read_file_hinted: &mut bool,
    error_hinted: &mut bool,
) -> bool {
    let Some(completion) = fetch_completion(
        client,
        msg_json,
        tool_defs,
        mcp,
        tx,
        messages,
        system_prompt,
    )
    .await
    else {
        return false;
    };

    if let Some(usage) = &completion.usage {
        let _ = tx.send(BridgeEvent::Usage(usage.clone())).await;
    }

    msg_json.push(serde_json::json!({
        "role": "assistant",
        "content": completion.content.clone(),
        "tool_calls": completion.assistant_message.get("tool_calls").cloned().unwrap_or(serde_json::Value::Null)
    }));

    if completion.tool_calls.is_empty() {
        if !completion.content.is_empty() {
            let _ = tx.send(BridgeEvent::Token(completion.content)).await;
        }
        let _ = tx.send(BridgeEvent::Done).await;
        return false;
    }

    let Some(mcp_cfg) = mcp.clone() else {
        let _ = tx
            .send(BridgeEvent::Error(
                "Model requested tools but MCP is not configured".to_string(),
            ))
            .await;
        return false;
    };

    let call_futures = completion
        .tool_calls
        .into_iter()
        .map(|call| execute_single_tool_call(call, mcp_cfg.clone(), tx.clone()));
    let tool_results = join_all(call_futures).await;

    let (inject_error_hint, inject_read_hint) = if mcp_cfg.small_model_harness {
        plan_harness_hints(&tool_results, *error_hinted, *read_file_hinted)
    } else {
        (false, false)
    };

    for (tool_call_id, tool_name, payload, failed) in tool_results {
        push_tool_message_with_hints(
            msg_json,
            tool_call_id,
            &tool_name,
            payload,
            failed,
            inject_error_hint,
            inject_read_hint,
            error_hinted,
            read_file_hinted,
        );
    }

    true
}

pub fn spawn_agent_task(
    client: LlmClient,
    messages: Vec<ChatMessage>,
    system_prompt: Option<String>,
    mcp: Option<McpChatConfig>,
    available_tools: Vec<crate::mcp_connections::ToolInfo>,
    tx: Sender<BridgeEvent>,
) {
    tokio::spawn(async move {
        let mut sys_prompt = system_prompt.clone();
        if let Some(ref cfg) = mcp
            && cfg.minimize_tokens
        {
            let conciseness_rule = "\n\nRespond concisely. No preamble, no conversational filler. Output only the tool call, code, or bare answer.";
            match sys_prompt {
                Some(ref mut s) => s.push_str(conciseness_rule),
                None => sys_prompt = Some(conciseness_rule.trim().to_string()),
            }
        }

        let mut msg_json: Vec<serde_json::Value> = Vec::new();
        if let Some(ref system) = sys_prompt {
            msg_json.push(serde_json::json!({"role": "system", "content": system}));
        }
        for msg in &messages {
            msg_json.push(serde_json::json!({"role": msg.role, "content": msg.content}));
        }

        let tool_defs = prepare_tool_definitions(available_tools);

        let max_turns = mcp.as_ref().map(|c| c.max_turns).unwrap_or(8);
        let mut completed = false;
        let mut read_file_hinted = false;
        let mut error_hinted = false;

        for _ in 0..max_turns {
            if !execute_agent_turn(
                &client,
                &mut msg_json,
                &tool_defs,
                &mcp,
                &tx,
                &messages,
                &sys_prompt,
                &mut read_file_hinted,
                &mut error_hinted,
            )
            .await
            {
                completed = true;
                break;
            }
        }

        if !completed {
            let _ = tx
                .send(BridgeEvent::Error(
                    "Agent loop reached max turns without completion".to_string(),
                ))
                .await;
        }
    });
}

fn is_local_default_server(url: &str) -> bool {
    if url.starts_with("unix://") {
        return true;
    }
    let trimmed = url.trim_end_matches('/');
    trimmed == "http://localhost:3000" || trimmed == "http://127.0.0.1:3000"
}

async fn spawn_local_tool_call(
    mcp: McpChatConfig,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<(String, bool), String> {
    if is_local_default_server(&mcp.base_url) {
        crate::connection::ensure_server_running(Some(&mcp.workspace_root))
            .await
            .map_err(|e| format!("Failed to ensure bridge server is running: {e}"))?;
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
    let client = builder.build().map_err(|e| e.to_string())?;
    let url = format!("{}/mcp", request_base_url);
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

        let tool_result =
            call_mcp_tool_http(&client, &url, &session_id, &tool, arguments.clone()).await;
        let (result, failed) = match tool_result {
            Ok((result, failed)) => (result, failed),
            Err(ref err) if err.contains("HTTP 403") => {
                // Session may have expired — re-handshake once and retry.
                let fresh_mcp = McpChatConfig {
                    session_id: None,
                    ..mcp.clone()
                };
                match get_or_create_session(&client, &url, &fresh_mcp).await {
                    Ok(new_sid) => {
                        match call_mcp_tool_http(&client, &url, &new_sid, &tool, arguments).await {
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

async fn get_or_create_session(
    client: &reqwest::Client,
    url: &str,
    mcp: &McpChatConfig,
) -> Result<String, String> {
    if let Some(sid) = &mcp.session_id
        && !sid.is_empty()
    {
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

fn parse_mcp_response(json_resp: &serde_json::Value) -> (String, bool) {
    let result_val = json_resp.get("result");
    let is_error = json_resp.get("error").is_some()
        || (result_val
            .and_then(|r| r.get("isError"))
            .and_then(|e| e.as_bool())
            .unwrap_or(false));

    let content_str = if let Some(err) = json_resp.get("error") {
        parse_error_message(err)
    } else if let Some(res) = result_val {
        extract_content_text(res)
    } else {
        "Empty result".to_string()
    };

    (content_str, is_error)
}

fn parse_error_message(err: &serde_json::Value) -> String {
    format!(
        "Error: {}",
        err.get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
    )
}

fn extract_content_text(res: &serde_json::Value) -> String {
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

    Ok(parse_mcp_response(&json_resp))
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
        let llm_counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let llm_router = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(move || {
                let counter = llm_counter.clone();
                async move {
                    let count = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if count == 0 {
                        axum::Json(serde_json::json!({
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
                        }))
                    } else {
                        axum::Json(serde_json::json!({
                            "choices": [{
                                "message": {
                                    "role": "assistant",
                                    "content": "Tool call was successful.",
                                }
                            }]
                        }))
                    }
                }
            }),
        );

        let llm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = llm_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(llm_listener, llm_router).await.unwrap();
        });

        let mcp_router = axum::Router::new().route(
            "/mcp",
            axum::routing::post(
                |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    use axum::response::IntoResponse;
                    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    match method {
                        "initialize" => {
                            let mut headers = axum::http::HeaderMap::new();
                            headers.insert(
                                "mcp-session-id",
                                axum::http::HeaderValue::from_static("test-session-123"),
                            );
                            (headers, axum::Json(serde_json::json!({}))).into_response()
                        }
                        "tools/call" => axum::Json(serde_json::json!({
                            "result": {
                                "content": [{"type": "text", "text": "Tool executed"}]
                            }
                        }))
                        .into_response(),
                        _ => axum::http::StatusCode::OK.into_response(),
                    }
                },
            ),
        );

        let mcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mcp_addr = mcp_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(mcp_listener, mcp_router).await.unwrap();
        });

        let client = LlmClient::new(format!("http://{}", llm_addr), "test-model", None);
        let mcp = McpChatConfig {
            base_url: format!("http://{}", mcp_addr),
            workspace_root: PathBuf::from("/tmp"),
            session_id: None,
            external_http_servers: BTreeMap::new(),
            max_turns: 2,
            tool_approval: false,
            mcp_connections: crate::mcp_connections::McpConnectionManager::default(),
            minimize_tokens: false,
            small_model_harness: false,
        };

        let (tx, mut rx) = mpsc::channel(100);
        let messages = vec![ChatMessage::user("Do something")];

        spawn_agent_task(
            client,
            messages,
            None,
            Some(mcp),
            vec![crate::mcp_connections::ToolInfo {
                name: "test_tool".to_string(),
                description: None,
                input_schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": true
                }),
            }],
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
