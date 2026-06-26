//! Patched transport for MCP stdio connections.
//!
//! This module adapts the `rmcp` transport to support both line-delimited JSON
//! and Content-Length framed messages on stdin/stdout. It is primarily used by
//! the HTTP bridge and testing utilities.
//!
//! ## Security
//! This transport only handles framing. It does not perform authentication or
//! validation; callers should enforce sandboxing and schema checks elsewhere.

use rmcp::{
    service::{RoleServer, RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};
use serde_json::Value;
use std::io::Error;
use std::sync::Arc;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::sync::Mutex;
use tracing;

/// Summarize a JSON-RPC payload for debug logging (full body is logged at TRACE only).
fn summarize_jsonrpc_payload(json: &str) -> String {
    let bytes = json.trim_end().len();
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return format!("bytes={bytes} parse=invalid");
    };
    let id = value
        .get("id")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_string());
    let method = value.get("method").and_then(Value::as_str).unwrap_or("-");
    format!("bytes={bytes} id={id} method={method}")
}

/// Transport wrapper supporting mixed MCP framing styles.
#[derive(Clone)]
pub struct PatchedTransport<R, W> {
    reader: Arc<Mutex<R>>,
    writer: Arc<Mutex<W>>,
}

impl<R, W> PatchedTransport<R, W>
where
    R: AsyncBufRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    /// Create a patched transport from an async reader and writer.
    ///
    /// # Arguments
    /// * `reader` - Async buffered reader (stdin or TCP stream).
    /// * `writer` - Async writer (stdout or TCP stream).
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: Arc::new(Mutex::new(reader)),
            writer: Arc::new(Mutex::new(writer)),
        }
    }
}

/// Convenience alias for stdio-based patched transport.
pub type PatchedStdioTransport = PatchedTransport<
    BufReader<tokio::io::Stdin>,
    tokio_util::either::Either<tokio::io::Stdout, tokio::fs::File>,
>;

impl PatchedStdioTransport {
    /// Create a patched transport bound to process stdin/stdout.
    pub fn new_stdio() -> Self {
        let writer = if let Some(saved_stdout) = crate::utils::stdio_redirect::get_saved_stdout() {
            tokio_util::either::Either::Right(tokio::fs::File::from_std(saved_stdout))
        } else {
            tokio_util::either::Either::Left(tokio::io::stdout())
        };
        Self::new(BufReader::new(tokio::io::stdin()), writer)
    }
}

impl<R, W> Transport<RoleServer> for PatchedTransport<R, W>
where
    R: AsyncBufRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    type Error = Error;

    fn send(
        &mut self,
        msg: TxJsonRpcMessage<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let writer = self.writer.clone();
        let json_res =
            serde_json::to_string(&msg).map_err(|e| Error::new(std::io::ErrorKind::InvalidData, e));

        async move {
            let mut json = json_res?;
            json.push('\n');
            let mut w = writer.lock().await;
            tracing::trace!("[AhmaTransport] SEND full: {}", json.trim_end());
            tracing::debug!("[AhmaTransport] SEND {}", summarize_jsonrpc_payload(&json));
            w.write_all(json.as_bytes()).await?;
            w.flush().await?;
            Ok(())
        }
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        let reader = self.reader.clone();

        async move {
            let mut r = reader.lock().await;
            loop {
                // Peek/Read logic to handle both Line-Delimited and Content-Length framed messages
                let mut first_line = String::new();
                match r.read_line(&mut first_line).await {
                    Ok(0) => return None, // EOF
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!("[AhmaTransport] Read Error: {}", e);
                        return None;
                    }
                }

                let message_body = if first_line.starts_with("Content-Length:") {
                    // Header Mode
                    let len_str = first_line
                        .trim()
                        .strip_prefix("Content-Length:")
                        .unwrap_or("0")
                        .trim();
                    let content_len: usize = len_str.parse().unwrap_or(0);
                    tracing::debug!(
                        "[AhmaTransport] Detected Header Framing. Content-Length: {}",
                        content_len
                    );

                    // Skip remaining headers until empty line
                    loop {
                        let mut h = String::new();
                        if let Ok(n) = r.read_line(&mut h).await {
                            if n == 0 || h.trim().is_empty() {
                                break;
                            }
                        } else {
                            break;
                        }
                    }

                    // Read exact bytes
                    let mut buf = vec![0u8; content_len];
                    if let Err(e) = r.read_exact(&mut buf).await {
                        tracing::debug!("[AhmaTransport] Failed to read body: {}", e);
                        continue;
                    }
                    match String::from_utf8(buf) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::debug!("[AhmaTransport] Invalid UTF-8 body: {}", e);
                            continue;
                        }
                    }
                } else {
                    // Line Mode
                    first_line
                };

                // Try to parse as Value to inspect and patch
                let mut value: Value = match serde_json::from_str(&message_body) {
                    Ok(v) => {
                        tracing::trace!("[AhmaTransport] RECV full: {}", message_body.trim());
                        tracing::debug!(
                            "[AhmaTransport] RECV {}",
                            summarize_jsonrpc_payload(&message_body)
                        );
                        v
                    }
                    Err(e) => {
                        if !message_body.trim().is_empty() {
                            tracing::debug!(
                                "[AhmaTransport] Invalid JSON: {} | Content: {}",
                                e,
                                message_body
                            );
                        }
                        // Ignore invalid JSON lines and continue loop
                        continue;
                    }
                };

                // --- PATCHING LOGIC ---
                if let Some(method) = value.get("method").and_then(|v| v.as_str())
                    && method == "initialize"
                {
                    tracing::debug!(
                        "[AhmaTransport] Detected 'initialize' request. Checking capabilities..."
                    );
                    if let Some(params) = value.get_mut("params")
                        && let Some(caps) = params.get_mut("capabilities")
                        && let Some(tasks) = caps.get("tasks")
                    {
                        if tasks.is_object() {
                            tracing::debug!(
                                "[AhmaTransport] Patching: Removing 'tasks' capability object"
                            );
                            if let Some(caps_obj) = caps.as_object_mut() {
                                caps_obj.remove("tasks");
                            }
                        } else {
                            tracing::debug!(
                                "[AhmaTransport] 'tasks' capability found but not an object: {:?}",
                                tasks
                            );
                        }
                    }
                }
                // -----------------------

                match serde_json::from_value(value) {
                    Ok(msg) => return Some(msg),
                    Err(e) => {
                        tracing::debug!("[AhmaTransport] Deserialization failed: {}", e);
                        continue;
                    }
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        // Ensure any buffered data is flushed and the writer is cleanly shutdown
        // so that outgoing notifications are delivered to the peer before
        // the transport is considered closed.
        let writer = self.writer.clone();
        let mut w = writer.lock().await;
        // Attempt to flush any buffered bytes first
        if let Err(e) = w.flush().await {
            tracing::debug!("[AhmaTransport] Flush on close failed: {}", e);
            return Err(e);
        }
        // Then try a graceful shutdown of the writer (if supported)
        if let Err(e) = AsyncWriteExt::shutdown(&mut *w).await {
            tracing::debug!("[AhmaTransport] Shutdown on close failed: {}", e);
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::summarize_jsonrpc_payload;

    #[test]
    fn summarize_jsonrpc_includes_method_and_id() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let summary = summarize_jsonrpc_payload(json);
        assert!(summary.contains("method=tools/list"));
        assert!(summary.contains("id=1"));
        assert!(summary.contains("bytes="));
    }

    #[test]
    fn summarize_jsonrpc_invalid_json() {
        let summary = summarize_jsonrpc_payload("not-json");
        assert!(summary.contains("parse=invalid"));
    }

    #[test]
    fn summarize_jsonrpc_notification_without_id() {
        // Notification: method present but no `id` -> id falls back to "-".
        let json = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let summary = summarize_jsonrpc_payload(json);
        assert!(summary.contains("id=-"), "summary was: {summary}");
        assert!(summary.contains("method=notifications/initialized"));
    }

    #[test]
    fn summarize_jsonrpc_valid_json_without_method() {
        // Response-shaped payload: id present, method absent -> method "-".
        let json = r#"{"jsonrpc":"2.0","id":5,"result":{}}"#;
        let summary = summarize_jsonrpc_payload(json);
        assert!(summary.contains("id=5"), "summary was: {summary}");
        assert!(summary.contains("method=-"), "summary was: {summary}");
    }

    // ---- Transport<RoleServer> tests using in-memory buffers (no real stdio) ----

    use super::PatchedTransport;
    use rmcp::service::{RoleServer, TxJsonRpcMessage};
    use rmcp::transport::Transport;
    use serde_json::json;
    use std::io::Cursor;
    use tokio::io::{AsyncReadExt, BufReader};

    /// Build an in-memory AsyncBufRead from a string.
    fn reader_from(input: &str) -> BufReader<Cursor<Vec<u8>>> {
        BufReader::new(Cursor::new(input.as_bytes().to_vec()))
    }

    /// Build a transport whose reader replays `input` and whose writer is an
    /// in-memory `Vec<u8>` (used when the test only exercises `receive`).
    fn transport_from(input: &str) -> PatchedTransport<BufReader<Cursor<Vec<u8>>>, Vec<u8>> {
        PatchedTransport::new(reader_from(input), Vec::<u8>::new())
    }

    #[tokio::test]
    async fn receive_line_mode_returns_message() {
        let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}\n";
        let mut t = transport_from(input);
        let msg = t.receive().await;
        assert!(msg.is_some(), "line-mode message should parse");
    }

    #[tokio::test]
    async fn receive_content_length_header_mode_returns_message() {
        let body = "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}";
        let input = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut t = transport_from(&input);
        let msg = t.receive().await;
        assert!(msg.is_some(), "Content-Length framed body should parse");
    }

    #[tokio::test]
    async fn receive_content_length_with_extra_headers() {
        // Exercises the header-skip loop with a non-empty extra header line.
        let body = "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/list\",\"params\":{}}";
        let input = format!(
            "Content-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            body
        );
        let mut t = transport_from(&input);
        let msg = t.receive().await;
        assert!(msg.is_some(), "framed body with extra headers should parse");
    }

    #[tokio::test]
    async fn receive_content_length_invalid_length_falls_back_to_zero() {
        // Non-numeric Content-Length -> parse().unwrap_or(0) -> empty body ->
        // empty parse is skipped (no error log) -> loop continues to next message.
        let body = "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/list\",\"params\":{}}";
        let input = format!("Content-Length: not-a-number\r\n\r\n{}\n", body);
        let mut t = transport_from(&input);
        let msg = t.receive().await;
        assert!(msg.is_some(), "should recover and parse the following line");
    }

    #[tokio::test]
    async fn receive_initialize_strips_tasks_capability_object() {
        // `params.capabilities.tasks` is an object -> patch removes it, then the
        // message deserializes successfully.
        let req = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tasks": { "listChanged": true } },
                "clientInfo": { "name": "test-client", "version": "1.0.0" }
            }
        });
        let input = format!("{}\n", serde_json::to_string(&req).unwrap());
        let mut t = transport_from(&input);
        let msg = t.receive().await;
        assert!(
            msg.is_some(),
            "initialize should parse after tasks capability is stripped"
        );
    }

    #[tokio::test]
    async fn receive_initialize_tasks_not_object_hits_else_branch() {
        // `tasks` is a string (not an object) -> else branch logs and leaves it,
        // so deserialization of capabilities fails -> the message is skipped.
        // A following valid line is then returned.
        let bad = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tasks": "not-an-object" },
                "clientInfo": { "name": "test-client", "version": "1.0.0" }
            }
        });
        let good = json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/list",
            "params": {}
        });
        let input = format!(
            "{}\n{}\n",
            serde_json::to_string(&bad).unwrap(),
            serde_json::to_string(&good).unwrap()
        );
        let mut t = transport_from(&input);
        let msg = t.receive().await;
        assert!(
            msg.is_some(),
            "should skip the un-deserializable initialize and return the next message"
        );
    }

    #[tokio::test]
    async fn receive_skips_invalid_json_then_returns_valid() {
        // First line is garbage (invalid JSON) -> `continue`; second is valid.
        let input = "this is not json\n{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/list\",\"params\":{}}\n";
        let mut t = transport_from(input);
        let msg = t.receive().await;
        assert!(msg.is_some(), "invalid JSON line should be skipped");
    }

    #[tokio::test]
    async fn receive_eof_returns_none() {
        let mut t = transport_from("");
        let msg = t.receive().await;
        assert!(msg.is_none(), "EOF should yield None");
    }

    #[tokio::test]
    async fn send_writes_json_followed_by_newline() {
        // A JSON-RPC error response is a valid TxJsonRpcMessage<RoleServer>
        // regardless of the request/result/notification generics.
        let msg: TxJsonRpcMessage<RoleServer> = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32600, "message": "boom" }
        }))
        .expect("error response should deserialize as a server Tx message");

        let (mut client_end, server_end) = tokio::io::duplex(64 * 1024);
        let mut t = PatchedTransport::new(reader_from(""), server_end);

        t.send(msg).await.expect("send should succeed");

        let mut buf = vec![0u8; 4096];
        let n = client_end.read(&mut buf).await.expect("read framed bytes");
        let written = String::from_utf8(buf[..n].to_vec()).expect("utf8 output");
        assert!(written.ends_with('\n'), "output must end with newline");
        assert!(written.contains("\"error\""), "output: {written}");
        assert!(written.contains("boom"), "output: {written}");
        // The framed payload itself (minus the trailing newline) must be valid JSON.
        let parsed: serde_json::Value = serde_json::from_str(written.trim_end()).unwrap();
        assert_eq!(parsed["error"]["message"], "boom");
    }

    #[tokio::test]
    async fn close_flushes_and_shuts_down_ok() {
        let (_client_end, server_end) = tokio::io::duplex(1024);
        let mut t = PatchedTransport::new(reader_from(""), server_end);
        let result = t.close().await;
        assert!(result.is_ok(), "close on in-memory writer should succeed");
    }
}
