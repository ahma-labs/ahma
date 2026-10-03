//! Ollama's own chat API (`POST /api/chat`), spoken for Ollama endpoints
//! instead of its OpenAI-compatible `/v1` (ahma_llm_monitor SPEC, "Ollama's
//! own API").
//!
//! Why: Ollama's `/v1/chat/completions` ignores `options`, so a context size
//! sent there does nothing. Checked against Ollama 0.35: `num_ctx` 5120 and
//! 12288 over `/v1` both loaded the model at the server default (131072, 13
//! GB); the same 12288 over `/api/chat` loaded it at 12288 (3.4 GB). A model
//! loaded once at a chosen size is reloaded at the default by the next `/v1`
//! request, so there is no way to choose the size over `/v1` at all.
//!
//! The rest of the client speaks OpenAI shapes. This module translates at the
//! edge, as [`crate::anthropic`] does: requests out of the OpenAI message
//! list, and responses (whole, or streamed line by line) back into OpenAI
//! JSON, so parsing, tool-call assembly and the agent loop are shared.

use bytes::Bytes;
use futures::Stream;
use serde_json::{Value, json};
use tracing::warn;

/// Whether a base URL is an Ollama server: its default port, or a host that
/// says so. Used to pick this API without configuration; a server on another
/// port is recognised later by what it answers
/// ([`crate::LlmClient::server_context`]).
pub fn looks_like_ollama(base_url: &str) -> bool {
    base_url.contains(":11434") || base_url.to_ascii_lowercase().contains("ollama")
}

/// The server root of an Ollama base URL (`http://host:11434/v1` → `http://host:11434`).
pub fn root(base_url: &str) -> &str {
    let trimmed = base_url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed)
}

/// Translate an OpenAI message list into Ollama's.
///
/// * Content given as parts is joined into one string; `data:` image URLs
///   become Ollama's `images` (base64 without the prefix).
/// * An assistant's `tool_calls` carry arguments as an object, not a JSON
///   string. Arguments that do not parse are sent as `{}`: Ollama rejects
///   anything but an object, and the model already saw the call fail.
/// * A tool result names its tool (`tool_name`), found from the call it
///   answers. Ollama (0.35) also gives each call an `id` and accepts
///   `tool_call_id` back, but neither is sent yet, so two calls to one tool in
///   a turn are matched by their order.
/// * Reasoning text from earlier turns is not sent back.
pub fn to_ollama_messages(messages: &[Value]) -> Vec<Value> {
    let mut names_by_id = std::collections::HashMap::<String, String>::new();
    messages
        .iter()
        .map(|m| {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
            let (content, images) = content_and_images(m.get("content"));
            let mut out = json!({"role": role, "content": content});
            if !images.is_empty() {
                out["images"] = json!(images);
            }
            if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                let calls: Vec<Value> = calls
                    .iter()
                    .map(|c| {
                        let name = c
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if let Some(id) = c.get("id").and_then(Value::as_str) {
                            names_by_id.insert(id.to_string(), name.to_string());
                        }
                        let arguments = match c.pointer("/function/arguments") {
                            Some(Value::String(s)) => serde_json::from_str::<Value>(s)
                                .ok()
                                .filter(Value::is_object)
                                .unwrap_or_else(|| json!({})),
                            Some(v @ Value::Object(_)) => v.clone(),
                            _ => json!({}),
                        };
                        json!({"function": {"name": name, "arguments": arguments}})
                    })
                    .collect();
                out["tool_calls"] = json!(calls);
            }
            if role == "tool"
                && let Some(name) = m
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .and_then(|id| names_by_id.get(id))
            {
                out["tool_name"] = json!(name);
            }
            out
        })
        .collect()
}

fn content_and_images(content: Option<&Value>) -> (String, Vec<String>) {
    match content {
        Some(Value::String(s)) => (s.clone(), Vec::new()),
        Some(Value::Array(parts)) => {
            let mut text = Vec::new();
            let mut images = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            text.push(t.to_string());
                        }
                    }
                    Some("image_url") => {
                        if let Some(b64) = part
                            .pointer("/image_url/url")
                            .and_then(Value::as_str)
                            .and_then(|u| u.split_once(";base64,"))
                            .map(|(_, data)| data.to_string())
                        {
                            images.push(b64);
                        }
                    }
                    _ => {}
                }
            }
            (text.join("\n"), images)
        }
        _ => (String::new(), Vec::new()),
    }
}

/// An `/api/chat` request body. `options` carries `num_ctx`, `num_predict`
/// and `temperature`; OpenAI-shaped tool definitions are accepted as they are.
pub fn chat_body(
    model: &str,
    messages: &[Value],
    tools: &[Value],
    stream: bool,
    options: Value,
) -> Value {
    let mut body = json!({
        "model": model,
        "messages": to_ollama_messages(messages),
        "stream": stream,
        "options": options,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
    }
    body
}

/// Translate one Ollama response object (a whole non-streamed reply, or one
/// streamed line) into OpenAI shape: `{"choices":[{"message"|"delta":…,
/// "finish_reason"}],"usage"}`. Tool calls get `index`/`id` numbered from
/// `next_call`, which carries across the lines of one stream.
fn to_openai(obj: &Value, key: &str, next_call: &mut usize) -> Value {
    let message = obj.get("message").cloned().unwrap_or_else(|| json!({}));
    let mut out = json!({"role": "assistant"});
    if let Some(c) = message.get("content").and_then(Value::as_str)
        && (!c.is_empty() || key == "message")
    {
        out["content"] = json!(c);
    }
    if let Some(t) = message
        .get("thinking")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        out["reasoning_content"] = json!(t);
    }
    let mut had_calls = false;
    if let Some(calls) = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .filter(|c| !c.is_empty())
    {
        had_calls = true;
        let calls: Vec<Value> = calls
            .iter()
            .map(|c| {
                let index = *next_call;
                *next_call += 1;
                let id = c
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{index}"));
                let arguments = match c.pointer("/function/arguments") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => "{}".to_string(),
                };
                json!({
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": c.pointer("/function/name").and_then(Value::as_str).unwrap_or(""),
                        "arguments": arguments,
                    }
                })
            })
            .collect();
        out["tool_calls"] = json!(calls);
    }
    let mut choice = json!({ key: out });
    let mut result = json!({});
    if obj.get("done").and_then(Value::as_bool) == Some(true) {
        let finish = match obj.get("done_reason").and_then(Value::as_str) {
            Some("length") => "length",
            _ if had_calls || *next_call > 0 => "tool_calls",
            _ => "stop",
        };
        choice["finish_reason"] = json!(finish);
        let prompt = obj
            .get("prompt_eval_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let completion = obj.get("eval_count").and_then(Value::as_u64).unwrap_or(0);
        result["usage"] = json!({
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
        });
    }
    result["choices"] = json!([choice]);
    result
}

/// A whole (non-streamed) `/api/chat` reply in OpenAI shape.
pub fn response_to_openai(obj: &Value) -> Value {
    to_openai(obj, "message", &mut 0)
}

/// Rewrites an Ollama NDJSON byte stream (one JSON object per line) into the
/// OpenAI server-sent-events stream the rest of the client reads: each line
/// becomes `data: <chunk>`, and the final one is followed by `data: [DONE]`.
/// An in-stream `{"error": …}` line is logged and ends the stream.
pub fn ndjson_as_sse<E>(
    inner: impl Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
) -> impl Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static
where
    E: Send + 'static,
{
    use futures::StreamExt as _;
    struct State<S> {
        inner: S,
        buffer: Vec<u8>,
        next_call: usize,
        finished: bool,
    }
    let state = State {
        inner,
        buffer: Vec::new(),
        next_call: 0,
        finished: false,
    };
    Box::pin(futures::stream::unfold(state, |mut st| async move {
        loop {
            if st.finished {
                return None;
            }
            if let Some(pos) = st.buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = st.buffer.drain(..=pos).collect();
                if let Some(out) = convert_line(&line, &mut st.next_call, &mut st.finished) {
                    return Some((Ok(Bytes::from(out)), st));
                }
                continue;
            }
            match st.inner.next().await {
                Some(Ok(bytes)) => st.buffer.extend_from_slice(&bytes),
                Some(Err(e)) => {
                    st.finished = true;
                    return Some((Err(e), st));
                }
                None => {
                    // A last line without a newline still counts.
                    if st.buffer.is_empty() {
                        return None;
                    }
                    let line = std::mem::take(&mut st.buffer);
                    let out = convert_line(&line, &mut st.next_call, &mut st.finished);
                    st.finished = true;
                    return out.map(|o| (Ok(Bytes::from(o)), st));
                }
            }
        }
    }))
}

fn convert_line(line: &[u8], next_call: &mut usize, finished: &mut bool) -> Option<String> {
    let text = String::from_utf8_lossy(line);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let Ok(obj) = serde_json::from_str::<Value>(text) else {
        return None;
    };
    if let Some(err) = obj.get("error") {
        warn!(error = %err, "llm: Ollama reported an error mid-stream");
        *finished = true;
        return Some("data: [DONE]\n".to_string());
    }
    let chunk = to_openai(&obj, "delta", next_call);
    let mut out = format!("data: {chunk}\n");
    if obj.get("done").and_then(Value::as_bool) == Some(true) {
        out.push_str("data: [DONE]\n");
        *finished = true;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    #[test]
    fn ollama_is_recognised_by_port_or_name() {
        assert!(looks_like_ollama("http://localhost:11434/v1"));
        assert!(looks_like_ollama("http://ollama.lan:8080/v1"));
        assert!(!looks_like_ollama("http://localhost:1234/v1"));
        assert_eq!(root("http://localhost:11434/v1/"), "http://localhost:11434");
    }

    #[test]
    fn messages_carry_tool_calls_as_objects_and_results_by_name() {
        let out = to_ollama_messages(&[
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]}),
            json!({"role": "assistant", "content": null, "reasoning_content": "hmm", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"}},
                {"id": "c2", "type": "function", "function": {"name": "ls", "arguments": "not json"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "file body"}),
        ]);
        assert_eq!(out[0], json!({"role": "system", "content": "sys"}));
        assert_eq!(
            out[1],
            json!({"role": "user", "content": "look", "images": ["AAAA"]})
        );
        assert_eq!(out[2]["content"], json!(""));
        assert!(out[2].get("reasoning_content").is_none());
        assert_eq!(
            out[2]["tool_calls"][0]["function"]["arguments"],
            json!({"path": "a"})
        );
        assert_eq!(out[2]["tool_calls"][1]["function"]["arguments"], json!({}));
        assert_eq!(
            out[3],
            json!({"role": "tool", "content": "file body", "tool_name": "read_file"})
        );
    }

    #[test]
    fn a_whole_reply_reads_as_an_openai_completion() {
        let reply = json!({
            "message": {"role": "assistant", "content": "", "thinking": "plan",
                        "tool_calls": [{"function": {"name": "ls", "arguments": {"dir": "."}}}]},
            "done": true, "done_reason": "stop", "prompt_eval_count": 10, "eval_count": 5
        });
        let out = response_to_openai(&reply);
        let msg = &out["choices"][0]["message"];
        assert_eq!(msg["reasoning_content"], json!("plan"));
        assert_eq!(msg["tool_calls"][0]["id"], json!("call_0"));
        assert_eq!(
            msg["tool_calls"][0]["function"]["arguments"],
            json!("{\"dir\":\".\"}")
        );
        assert_eq!(out["choices"][0]["finish_reason"], json!("tool_calls"));
        assert_eq!(out["usage"]["total_tokens"], json!(15));

        let cut = response_to_openai(
            &json!({"message": {"content": "abc"}, "done": true, "done_reason": "length"}),
        );
        assert_eq!(cut["choices"][0]["finish_reason"], json!("length"));
        assert_eq!(cut["choices"][0]["message"]["content"], json!("abc"));
    }

    #[tokio::test]
    async fn a_streamed_reply_becomes_server_sent_events() {
        let lines = concat!(
            "{\"message\":{\"content\":\"\",\"thinking\":\"th\"},\"done\":false}\n",
            "{\"message\":{\"content\":\"Hel\"},\"done\":false}\n{\"message\":{\"content\":\"lo\"},\"done\":false}\n",
            "{\"message\":{\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"a\",\"arguments\":{}}}]},\"done\":false}\n",
            "{\"message\":{\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"b\",\"arguments\":{\"x\":1}}}]},\"done\":false}\n",
            "{\"message\":{\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":3,\"eval_count\":4}"
        );
        // Split mid-line to prove lines are reassembled across reads.
        let (a, b) = lines.split_at(37);
        let inner = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from(a.to_string())),
            Ok(Bytes::from(b.to_string())),
        ]);
        let out: Vec<String> = ndjson_as_sse(inner)
            .map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
            .collect()
            .await;
        let all = out.concat();
        let chunks: Vec<Value> = all
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|d| *d != "[DONE]")
            .map(|d| serde_json::from_str(d).unwrap())
            .collect();
        assert_eq!(
            chunks[0]["choices"][0]["delta"]["reasoning_content"],
            json!("th")
        );
        assert_eq!(chunks[1]["choices"][0]["delta"]["content"], json!("Hel"));
        assert_eq!(
            chunks[3]["choices"][0]["delta"]["tool_calls"][0]["index"],
            json!(0)
        );
        assert_eq!(
            chunks[4]["choices"][0]["delta"]["tool_calls"][0]["index"],
            json!(1)
        );
        assert_eq!(
            chunks[4]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            json!("{\"x\":1}")
        );
        assert_eq!(
            chunks[5]["choices"][0]["finish_reason"],
            json!("tool_calls")
        );
        assert_eq!(chunks[5]["usage"]["prompt_tokens"], json!(3));
        assert!(all.ends_with("data: [DONE]\n"));
    }

    #[tokio::test]
    async fn an_error_line_ends_the_stream() {
        let inner = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(
            "{\"message\":{\"content\":\"x\"},\"done\":false}\n{\"error\":\"model ran out of memory\"}\n{\"message\":{\"content\":\"never\"}}\n",
        ))]);
        let all: String = ndjson_as_sse(inner)
            .map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
            .collect::<Vec<_>>()
            .await
            .concat();
        assert!(all.ends_with("data: [DONE]\n"), "{all}");
        assert!(!all.contains("never"));
    }
}
