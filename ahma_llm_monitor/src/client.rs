use std::time::Duration;

use futures::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, warn};

use ahma_common::config::warn_if_looks_like_literal_secret;

use crate::error::LlmMonitorError;
use crate::prompt::build_messages;

// ─── Chat types ───────────────────────────────────────────────────────────────

/// Role in a chat conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

/// A single message in a chat conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            tool_call_id: None,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
            tool_call_id: None,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    pub fn as_openai_message(&self) -> Value {
        let mut message = json!({
            "role": self.role,
            "content": self.content,
        });
        if let Some(tool_call_id) = &self.tool_call_id {
            message["tool_call_id"] = json!(tool_call_id);
        }
        message
    }
}

/// One OpenAI-compatible tool call emitted by the model.
#[derive(Debug, Clone)]
pub struct ChatToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub arguments_raw: String,
}

/// One assistant turn returned from a non-streaming chat completion.
#[derive(Debug, Clone)]
pub struct ChatCompletionResponse {
    pub content: String,
    pub tool_calls: Vec<ChatToolCall>,
    pub assistant_message: Value,
}

/// A discovered local LLM provider.
#[derive(Debug, Clone)]
pub struct LocalProvider {
    /// Display name, e.g. "Ollama" or "llama-server".
    pub name: String,
    /// Base URL, e.g. "http://localhost:11434/v1".
    pub base_url: String,
    /// Models available from this provider.
    pub models: Vec<String>,
}

/// An OpenAI-compatible LLM client for issue detection in log chunks.
#[derive(Debug, Clone)]
pub struct LlmClient {
    http: Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
}

impl LlmClient {
    /// Create a new LLM client.
    ///
    /// * `base_url` — Base URL of the OpenAI-compatible API (e.g. `http://localhost:11434/v1`)
    /// * `model` — Model name (e.g. `llama3.2`, `gpt-4o-mini`)
    /// * `api_key` — Optional bearer token; pass `None` for local models (Ollama etc.)
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        if let Some(key) = &api_key {
            warn_if_looks_like_literal_secret(key);
        }
        Self {
            http: Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            api_key,
        }
    }

    /// Analyse a chunk of log lines against the detection prompt.
    ///
    /// Returns `Ok(Some(summary))` if the LLM detected an issue, or `Ok(None)` if clean.
    /// The `timeout` controls the maximum time to wait for the API response.
    pub async fn detect_issues(
        &self,
        detection_prompt: &str,
        chunk: &str,
        timeout: Duration,
    ) -> Result<Option<String>, LlmMonitorError> {
        let messages = build_messages(detection_prompt, chunk);

        let body = json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": 256,
            "temperature": 0.0,
        });

        debug!(
            "Sending chunk ({} chars) to LLM at {} for analysis",
            chunk.len(),
            self.base_url
        );

        let mut request = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body)
            .timeout(timeout);

        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }

        let response = tokio::time::timeout(timeout, request.send())
            .await
            .map_err(|_| LlmMonitorError::Timeout)?
            .map_err(LlmMonitorError::Http)?;

        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_default();
            warn!("LLM API error {}: {}", status, body_text);
            return Err(LlmMonitorError::Parse(format!(
                "HTTP {status}: {body_text}"
            )));
        }

        let json: Value = response.json().await.map_err(LlmMonitorError::Http)?;

        let text = json
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| LlmMonitorError::Parse("missing choices[0].message.content".into()))?
            .trim()
            .to_string();

        // The LLM is instructed to respond with "CLEAN" when no issues are found.
        if text.eq_ignore_ascii_case("clean") || text.to_ascii_uppercase().starts_with("CLEAN") {
            debug!("LLM response: CLEAN");
            Ok(None)
        } else {
            debug!("LLM detected issue: {}", text);
            Ok(Some(text))
        }
    }

    // ─── Chat ─────────────────────────────────────────────────────────────────

    /// Send a chat conversation and stream back token chunks.
    ///
    /// Returns a `Stream` of `Result<String>` where each `Ok` item is a text
    /// delta from the server-sent-events stream.  The stream ends when the
    /// server sends `data: [DONE]`.
    pub fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
        system_prompt: Option<&str>,
    ) -> impl Stream<Item = Result<String, LlmMonitorError>> + '_ {
        use futures::StreamExt as _;
        use futures::stream;

        let body = build_chat_stream_body(&self.model, &messages, system_prompt);
        let http = self.http.clone();
        let url = format!("{}/chat/completions", self.base_url);
        let api_key = self.api_key.clone();

        stream::unfold(
            ChatStreamState::Starting {
                http,
                url,
                api_key,
                body,
            },
            |state| async move {
                match state {
                    ChatStreamState::Starting {
                        http,
                        url,
                        api_key,
                        body,
                    } => chat_stream_start(http, url, api_key, body).await,
                    ChatStreamState::Streaming { stream, buffer } => {
                        chat_stream_poll(stream, buffer).await
                    }
                    ChatStreamState::Done => None,
                }
            },
        )
        // Skip the empty first yield from Starting state.
        .filter(|item| {
            let keep = match item {
                Ok(s) => !s.is_empty(),
                Err(_) => true,
            };
            futures::future::ready(keep)
        })
    }

    /// Send a non-streaming chat completion with OpenAI-compatible tools.
    pub async fn chat_completion_with_tools(
        &self,
        messages: Vec<Value>,
        tools: &[Value],
    ) -> Result<ChatCompletionResponse, LlmMonitorError> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": 0.2,
            "stream": false,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
            body["tool_choice"] = json!("auto");
        }

        let mut request = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body)
            .timeout(Duration::from_secs(30));

        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }

        let response = request.send().await.map_err(LlmMonitorError::Http)?;
        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_default();
            return Err(LlmMonitorError::Parse(format!(
                "HTTP {status}: {body_text}"
            )));
        }

        let json = response
            .json::<Value>()
            .await
            .map_err(LlmMonitorError::Http)?;
        parse_chat_completion_response(json)
    }

    // ─── Model discovery ──────────────────────────────────────────────────────

    /// List models available from this provider via `GET /v1/models`.
    ///
    /// Returns model IDs sorted alphabetically.  Returns an empty vec (not an
    /// error) when the endpoint is unreachable — callers should treat that as
    /// "no models available right now".
    pub async fn list_models(&self) -> Vec<String> {
        let url = format!("{}/models", self.base_url);
        let mut req = self.http.get(&url).timeout(Duration::from_secs(3));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let Ok(resp) = req.send().await else {
            return vec![];
        };
        if !resp.status().is_success() {
            return vec![];
        }
        let Ok(json) = resp.json::<Value>().await else {
            return vec![];
        };
        let mut ids: Vec<String> = json
            .pointer("/data")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.pointer("/id").and_then(Value::as_str))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    }
}

// ─── SSE stream state machine ─────────────────────────────────────────────────

enum ChatStreamState {
    Starting {
        http: Client,
        url: String,
        api_key: Option<String>,
        body: Value,
    },
    Streaming {
        stream: std::pin::Pin<Box<dyn Stream<Item = Result<bytes::Bytes, LlmMonitorError>> + Send>>,
        buffer: String,
    },
    Done,
}

fn build_chat_stream_body(
    model: &str,
    messages: &[ChatMessage],
    system_prompt: Option<&str>,
) -> Value {
    let mut all_messages: Vec<Value> = Vec::with_capacity(messages.len() + 1);
    if let Some(sys) = system_prompt {
        all_messages.push(json!({"role": "system", "content": sys}));
    }
    for message in messages {
        all_messages.push(message.as_openai_message());
    }
    json!({
        "model": model,
        "messages": all_messages,
        "stream": true,
        "temperature": 0.7,
    })
}

async fn chat_stream_start(
    http: Client,
    url: String,
    api_key: Option<String>,
    body: Value,
) -> Option<(Result<String, LlmMonitorError>, ChatStreamState)> {
    let mut req = http
        .post(&url)
        .json(&body)
        .header("Accept", "text/event-stream");
    if let Some(key) = &api_key {
        req = req.bearer_auth(key);
    }

    let resp = match req.send().await {
        Err(e) => return Some((Err(LlmMonitorError::Http(e)), ChatStreamState::Done)),
        Ok(resp) => resp,
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Some((
            Err(LlmMonitorError::Parse(format!("HTTP {status}: {body}"))),
            ChatStreamState::Done,
        ));
    }

    use futures::TryStreamExt as _;
    let byte_stream = resp.bytes_stream().map_err(LlmMonitorError::Http);
    Some((
        Ok(String::new()), // empty first yield to advance state
        ChatStreamState::Streaming {
            stream: Box::pin(byte_stream),
            buffer: String::new(),
        },
    ))
}

fn take_sse_line(buffer: &mut String) -> Option<String> {
    let newline_pos = buffer.find('\n')?;
    let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
    *buffer = buffer[newline_pos + 1..].to_string();
    Some(line)
}

async fn chat_stream_poll(
    mut stream: std::pin::Pin<Box<dyn Stream<Item = Result<bytes::Bytes, LlmMonitorError>> + Send>>,
    mut buffer: String,
) -> Option<(Result<String, LlmMonitorError>, ChatStreamState)> {
    use futures::StreamExt as _;
    loop {
        if let Some(line) = take_sse_line(&mut buffer) {
            if let Some(token) = parse_sse_line(&line) {
                if token == "__DONE__" {
                    return Some((Ok(String::new()), ChatStreamState::Done));
                }
                return Some((Ok(token), ChatStreamState::Streaming { stream, buffer }));
            }
            continue;
        }

        match stream.next().await {
            None => return None,
            Some(Err(e)) => return Some((Err(e), ChatStreamState::Done)),
            Some(Ok(bytes)) => buffer.push_str(&String::from_utf8_lossy(&bytes)),
        }
    }
}

/// Parse one SSE `data:` line into a token string.
/// Returns `None` for blank/comment lines, `Some("__DONE__")` for `[DONE]`.
fn parse_sse_line(line: &str) -> Option<String> {
    let data = line.strip_prefix("data: ")?;
    let data = data.trim();
    if data == "[DONE]" {
        return Some("__DONE__".to_string());
    }
    let json: Value = serde_json::from_str(data).ok()?;
    let token = json
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)?
        .to_string();
    if token.is_empty() { None } else { Some(token) }
}

fn parse_chat_completion_response(json: Value) -> Result<ChatCompletionResponse, LlmMonitorError> {
    let assistant_message = json
        .pointer("/choices/0/message")
        .cloned()
        .ok_or_else(|| LlmMonitorError::Parse("missing choices[0].message".into()))?;

    let content = assistant_message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let tool_calls = assistant_message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| {
                    let id = call.get("id")?.as_str()?.to_string();
                    let name = call.pointer("/function/name")?.as_str()?.to_string();
                    let arguments_raw = call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}")
                        .to_string();
                    let arguments =
                        serde_json::from_str(&arguments_raw).unwrap_or_else(|_| json!({}));
                    Some(ChatToolCall {
                        id,
                        name,
                        arguments,
                        arguments_raw,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(ChatCompletionResponse {
        content,
        tool_calls,
        assistant_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_chat_completion_response_extracts_tool_calls() {
        let response = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "I'll inspect that.",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "status",
                            "arguments": "{\"verbose\":true}"
                        }
                    }]
                }
            }]
        });

        let parsed = parse_chat_completion_response(response).unwrap();
        assert_eq!(parsed.content, "I'll inspect that.");
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "status");
        assert_eq!(parsed.tool_calls[0].arguments, json!({"verbose": true}));
    }
}
