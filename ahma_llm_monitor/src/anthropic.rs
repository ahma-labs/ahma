//! Native Anthropic Messages API (`POST /v1/messages`) support for [`LlmClient`].
//!
//! ahma speaks the OpenAI chat-completion message shape internally as a lingua
//! franca (see [`crate::client::ChatMessage::as_openai_message`]).  The
//! Anthropic Messages API is **not** OpenAI-compatible — different endpoint,
//! headers (`x-api-key` + `anthropic-version`), request/response shape, and
//! SSE event format.  This module is the boundary translator: it converts
//! OpenAI-shaped requests into Messages-API bodies and parses Messages-API
//! responses back into ahma's [`crate::client::ChatCompletionResponse`], so the
//! rest of the codebase (and its agentic tool loop) is unchanged regardless of
//! which provider flavor is in use.
//!
//! There is no official Anthropic Rust SDK, so the wire format is built and
//! parsed by hand against the public Messages API documentation.

use serde_json::{Value, json};

use crate::client::{ChatCompletionResponse, ChatToolCall, TokenUsage};
use crate::error::LlmMonitorError;

/// `anthropic-version` header value pinned by this client.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Default `max_tokens` for chat / tool turns.  The Messages API *requires*
/// `max_tokens`; this is a generous-but-safe ceiling for interactive use.
pub const DEFAULT_MAX_TOKENS: u64 = 8192;

/// Heuristic: does this base URL point at the Anthropic Messages API?
///
/// Used by [`crate::client::LlmClient::new`] to auto-select the Anthropic
/// flavor so existing call sites that only thread a `base_url` light up without
/// extra plumbing.  Matches the canonical host; explicit configuration (a
/// `kind = "anthropic"` provider, or [`crate::client::LlmClient::with_flavor`])
/// covers proxies and gateways that don't carry the host in the URL.
pub fn looks_like_anthropic(base_url: &str) -> bool {
    base_url.contains("anthropic.com")
}

/// Translate OpenAI-style chat messages into an Anthropic `system` prompt plus
/// a `messages` array.
///
/// * `system` messages are concatenated into the top-level system prompt
///   (Anthropic carries the system prompt out-of-band, not as a message role).
/// * `assistant` messages carrying OpenAI `tool_calls` become `tool_use`
///   content blocks; plain assistant text becomes a `text` block.
/// * `tool` results (`role: "tool"`, `tool_call_id`) become `tool_result`
///   blocks inside a `user` turn, merged with any immediately-preceding
///   tool-result turn (the API expects parallel results grouped together).
pub fn openai_to_anthropic(messages: &[Value]) -> (Option<String>, Vec<Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        match role {
            "system" => {
                if let Some(text) = msg.get("content").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    system_parts.push(text.to_string());
                }
            }
            "tool" => {
                let tool_use_id = msg
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let content = msg
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                push_tool_result(
                    &mut out,
                    json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": content,
                    }),
                );
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(text) = msg.get("content").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
                        let name = call
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let args_raw = call
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        let input: Value =
                            serde_json::from_str(args_raw).unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }));
                    }
                }
                // Anthropic rejects an empty content array; keep an empty text block.
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": ""}));
                }
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            _ => {
                let content = msg
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                out.push(json!({"role": "user", "content": content}));
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (system, out)
}

/// Append a `tool_result` block, merging into a preceding tool-result `user`
/// turn (whose `content` is already an array) rather than starting a new turn.
fn push_tool_result(out: &mut Vec<Value>, block: Value) {
    if let Some(last) = out.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("user")
        && let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        arr.push(block);
        return;
    }
    out.push(json!({"role": "user", "content": [block]}));
}

/// Translate OpenAI-style tool definitions (`{type, function: {name,
/// description, parameters}}`) into Anthropic tool definitions (`{name,
/// description, input_schema}`).  Also accepts already-flattened definitions.
pub fn openai_tools_to_anthropic(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let spec = tool.get("function").unwrap_or(tool);
            let name = spec.get("name").and_then(Value::as_str)?;
            let description = spec
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            let schema = spec
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(json!({
                "name": name,
                "description": description,
                "input_schema": schema,
            }))
        })
        .collect()
}

/// Build a Messages API request body.
///
/// `temperature` is deliberately omitted: with adaptive thinking the current
/// Opus/Sonnet models reject sampling parameters, and omitting it is valid
/// across every model, so the one body shape works regardless of `thinking`.
pub fn build_messages_body(
    model: &str,
    system: Option<String>,
    messages: Vec<Value>,
    tools: &[Value],
    stream: bool,
    max_tokens: u64,
    thinking: bool,
) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
    });
    if let Some(system) = system {
        body["system"] = json!(system);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
    }
    if stream {
        body["stream"] = json!(true);
    }
    if thinking {
        // Adaptive thinking: the model decides when and how much to think.
        body["thinking"] = json!({"type": "adaptive"});
    }
    body
}

/// Parse a Messages API response into a [`ChatCompletionResponse`].
///
/// `tool_use` blocks become both [`ChatToolCall`]s and an OpenAI-shaped
/// `assistant_message` (with `tool_calls`), so the existing agentic loop can
/// append it to history and have [`openai_to_anthropic`] round-trip it on the
/// next turn.  `thinking` blocks are ignored for history purposes.
pub fn parse_messages_response(json: Value) -> Result<ChatCompletionResponse, LlmMonitorError> {
    let blocks = json
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmMonitorError::Parse("missing content array".into()))?;

    let mut text = String::new();
    let mut tool_calls: Vec<ChatToolCall> = Vec::new();
    let mut openai_tool_calls: Vec<Value> = Vec::new();

    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let arguments_raw =
                    serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                openai_tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments_raw},
                }));
                tool_calls.push(ChatToolCall {
                    id,
                    name,
                    arguments: input,
                    arguments_raw,
                });
            }
            // thinking / redacted_thinking / other block types: ignored here.
            _ => {}
        }
    }

    let mut assistant_message = json!({
        "role": "assistant",
        "content": if text.is_empty() { Value::Null } else { json!(text) },
    });
    if !openai_tool_calls.is_empty() {
        assistant_message["tool_calls"] = Value::Array(openai_tool_calls);
    }

    let usage = json.get("usage").map(|u| {
        let input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let output = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        TokenUsage {
            prompt_tokens: input as u32,
            completion_tokens: output as u32,
            total_tokens: (input + output) as u32,
        }
    });

    // Normalize Anthropic's stop_reason ("end_turn" | "max_tokens" |
    // "stop_sequence" | "tool_use") to the same "length" convention the
    // OpenAI-flavored parser uses, so callers have one truncation signal.
    let finish_reason = json
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(|r| if r == "max_tokens" { "length" } else { r }.to_string());

    Ok(ChatCompletionResponse {
        content: text,
        tool_calls,
        assistant_message,
        usage,
        finish_reason,
    })
}

/// Parse one Messages API SSE `data:` line into a streamed token.
///
/// Returns `Some(text)` for a `text_delta`, `Some("__DONE__")` for
/// `message_stop`, and `None` for everything else (`thinking_delta`,
/// `input_json_delta`, `ping`, block start/stop, …).  The sentinel matches the
/// OpenAI parser so the shared stream state machine handles both flavors.
pub fn parse_sse_line(line: &str) -> Option<String> {
    let data = line.strip_prefix("data: ")?;
    let data = data.trim();
    let json: Value = serde_json::from_str(data).ok()?;
    match json.get("type").and_then(Value::as_str) {
        Some("content_block_delta") => {
            let delta = json.get("delta")?;
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => delta
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .filter(|s| !s.is_empty()),
                _ => None,
            }
        }
        Some("message_stop") => Some("__DONE__".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_anthropic_matches_host() {
        assert!(looks_like_anthropic("https://api.anthropic.com/v1"));
        assert!(!looks_like_anthropic("http://localhost:11434/v1"));
    }

    #[test]
    fn system_messages_lift_to_top_level() {
        let (system, msgs) = openai_to_anthropic(&[
            json!({"role": "system", "content": "be terse"}),
            json!({"role": "user", "content": "hi"}),
        ]);
        assert_eq!(system.as_deref(), Some("be terse"));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "hi");
    }

    #[test]
    fn assistant_tool_calls_become_tool_use_blocks() {
        let (_system, msgs) = openai_to_anthropic(&[json!({
            "role": "assistant",
            "content": "let me check",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "status", "arguments": "{\"verbose\":true}"}
            }]
        })]);
        let blocks = msgs[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "call_1");
        assert_eq!(blocks[1]["name"], "status");
        assert_eq!(blocks[1]["input"], json!({"verbose": true}));
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_turn() {
        let (_system, msgs) = openai_to_anthropic(&[
            json!({"role": "tool", "tool_call_id": "a", "content": "ra"}),
            json!({"role": "tool", "tool_call_id": "b", "content": "rb"}),
        ]);
        assert_eq!(msgs.len(), 1);
        let blocks = msgs[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["tool_use_id"], "a");
        assert_eq!(blocks[1]["tool_use_id"], "b");
    }

    #[test]
    fn openai_tools_translate_to_input_schema() {
        let tools = openai_tools_to_anthropic(&[json!({
            "type": "function",
            "function": {
                "name": "status",
                "description": "show status",
                "parameters": {"type": "object", "properties": {"verbose": {"type": "boolean"}}}
            }
        })]);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "status");
        assert_eq!(tools[0]["description"], "show status");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
    }

    #[test]
    fn build_body_adds_thinking_and_system_but_no_temperature() {
        let body = build_messages_body(
            "claude-opus-4-8",
            Some("sys".into()),
            vec![json!({"role": "user", "content": "hi"})],
            &[],
            true,
            DEFAULT_MAX_TOKENS,
            true,
        );
        assert_eq!(body["model"], "claude-opus-4-8");
        assert_eq!(body["system"], "sys");
        assert_eq!(body["stream"], true);
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert!(body.get("temperature").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn parse_response_extracts_text_tool_calls_and_usage() {
        let resp = parse_messages_response(json!({
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "I'll inspect that."},
                {"type": "tool_use", "id": "tu_1", "name": "status", "input": {"verbose": true}}
            ],
            "usage": {"input_tokens": 100, "output_tokens": 50}
        }))
        .unwrap();

        assert_eq!(resp.content, "I'll inspect that.");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "status");
        assert_eq!(resp.tool_calls[0].arguments, json!({"verbose": true}));
        // assistant_message is OpenAI-shaped so the agentic loop round-trips it.
        let oa_call = &resp.assistant_message["tool_calls"][0];
        assert_eq!(oa_call["id"], "tu_1");
        assert_eq!(oa_call["function"]["name"], "status");
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
    }

    #[test]
    fn parse_response_normalizes_max_tokens_stop_reason_to_length() {
        let resp = parse_messages_response(json!({
            "content": [{"type": "text", "text": "cut off"}],
            "stop_reason": "max_tokens"
        }))
        .unwrap();
        assert_eq!(resp.finish_reason.as_deref(), Some("length"));
        assert!(resp.is_length_truncated());
    }

    #[test]
    fn parse_response_passes_through_other_stop_reasons() {
        let resp = parse_messages_response(json!({
            "content": [{"type": "text", "text": "Done."}],
            "stop_reason": "end_turn"
        }))
        .unwrap();
        assert_eq!(resp.finish_reason.as_deref(), Some("end_turn"));
        assert!(!resp.is_length_truncated());
    }

    #[test]
    fn sse_parser_handles_text_delta_and_stop() {
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#;
        assert_eq!(parse_sse_line(line).as_deref(), Some("Hel"));

        let thinking = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"x"}}"#;
        assert_eq!(parse_sse_line(thinking), None);

        assert_eq!(
            parse_sse_line(r#"data: {"type":"message_stop"}"#).as_deref(),
            Some("__DONE__")
        );
        assert_eq!(parse_sse_line("event: message_stop"), None);
    }
}
