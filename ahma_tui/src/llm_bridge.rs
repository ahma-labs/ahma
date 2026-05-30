use ahma_llm_monitor::ChatMessage;
use ahma_llm_monitor::LocalProvider;
use ahma_llm_monitor::client::LlmClient;
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
}

#[derive(Clone)]
pub struct McpChatConfig {
    pub base_url: String,
    pub workspace_root: PathBuf,
    pub session_id: Option<String>,
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
