//! Minimal Server-Sent Events (SSE) framing helpers.
//!
//! The MCP Streamable-HTTP transport delivers server → client traffic as an
//! SSE stream. Both the core agent loop (`ahma_core::agent`) and the TUI's
//! MCP source (`ahma_tui::mcp_source`) consume such streams; this module is
//! the single shared implementation of the event framing they both need:
//! splitting a byte buffer on event boundaries and extracting the JSON payload
//! from an event's `data:` lines.

/// Find the first SSE event boundary (`\n\n` or `\r\n\r\n`) in `buffer`.
///
/// Returns `(index, delimiter_len)` of the earliest boundary, or `None` when
/// the buffer holds no complete event yet.
pub fn first_sse_event_boundary(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n").map(|idx| (idx, 2));
    let crlf = buffer.find("\r\n\r\n").map(|idx| (idx, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Pop the next complete SSE event from `buffer`, leaving the remainder.
pub fn pop_next_sse_event(buffer: &mut String) -> Option<String> {
    let (idx, delimiter_len) = first_sse_event_boundary(buffer)?;
    let raw_event = buffer[..idx].to_string();
    buffer.drain(..idx + delimiter_len);
    Some(raw_event)
}

/// Parse the `data:` lines of a raw SSE event into a JSON value.
///
/// Multiple `data:` lines are joined with `\n` (per the SSE spec) before
/// parsing. Returns `None` when the event carries no `data:` lines or the
/// joined payload is not valid JSON.
pub fn event_data_to_json(raw_event: &str) -> Option<serde_json::Value> {
    let data: Vec<&str> = raw_event
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("data:"))
        .map(str::trim)
        .collect();
    if data.is_empty() {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(&data.join("\n")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── first_sse_event_boundary ────────────────────────────────────────────

    #[test]
    fn boundary_lf_crlf_and_none() {
        assert_eq!(first_sse_event_boundary("ab\n\ncd"), Some((2, 2)));
        assert_eq!(first_sse_event_boundary("ab\r\n\r\ncd"), Some((2, 4)));
        assert_eq!(first_sse_event_boundary("no boundary here"), None);
    }

    #[test]
    fn boundary_prefers_earliest() {
        // CRLF boundary precedes a later LF boundary.
        assert_eq!(first_sse_event_boundary("a\r\n\r\nb\n\nc"), Some((1, 4)));
        // LF boundary precedes a later CRLF boundary.
        assert_eq!(first_sse_event_boundary("ab\n\nx\r\n\r\ny"), Some((2, 2)));
        assert_eq!(first_sse_event_boundary("a\n\nb\r\n\r\n"), Some((1, 2)));
        assert_eq!(first_sse_event_boundary("ab\r\n\r\nc\n\nd"), Some((2, 4)));
    }

    // ── pop_next_sse_event ──────────────────────────────────────────────────

    #[test]
    fn pop_lf_delimiter() {
        let mut buf = "event: ping\ndata: {}\n\nmore".to_string();
        let event = pop_next_sse_event(&mut buf);
        assert_eq!(event.as_deref(), Some("event: ping\ndata: {}"));
        assert_eq!(buf, "more");
    }

    #[test]
    fn pop_crlf_delimiter() {
        let mut buf = "data: hello\r\n\r\nremainder".to_string();
        let event = pop_next_sse_event(&mut buf);
        assert_eq!(event.as_deref(), Some("data: hello"));
        assert_eq!(buf, "remainder");
    }

    #[test]
    fn pop_no_delimiter_returns_none() {
        let mut buf = "data: incomplete".to_string();
        assert!(pop_next_sse_event(&mut buf).is_none());
        assert_eq!(buf, "data: incomplete");
    }

    #[test]
    fn pop_empty_buf() {
        let mut buf = String::new();
        assert!(pop_next_sse_event(&mut buf).is_none());
    }

    #[test]
    fn pop_multiple_events() {
        let mut buf = "data: 1\n\ndata: 2\n\n".to_string();
        assert_eq!(pop_next_sse_event(&mut buf).as_deref(), Some("data: 1"));
        assert_eq!(pop_next_sse_event(&mut buf).as_deref(), Some("data: 2"));
        assert!(pop_next_sse_event(&mut buf).is_none());
        assert_eq!(buf, "");
    }

    // ── event_data_to_json ──────────────────────────────────────────────────

    #[test]
    fn data_single_line() {
        let raw = "data: {\"method\":\"roots/list\",\"id\":1}";
        let v = event_data_to_json(raw).expect("should parse");
        assert_eq!(v["method"].as_str(), Some("roots/list"));
    }

    #[test]
    fn data_no_prefix_returns_none() {
        assert!(event_data_to_json("event: ping\n: comment").is_none());
        assert!(event_data_to_json("event: ping\nid: 1").is_none());
        assert!(event_data_to_json("").is_none());
    }

    #[test]
    fn data_invalid_json_returns_none() {
        assert!(event_data_to_json("data: not-valid-json").is_none());
        assert!(event_data_to_json("data: not json {").is_none());
    }

    #[test]
    fn data_multiline_join() {
        // Multiple `data:` lines are concatenated with '\n' before JSON parsing.
        let raw = "data: {\"x\":\ndata: 5}";
        let v = event_data_to_json(raw).expect("multiline data joins into valid json");
        assert_eq!(v["x"].as_i64(), Some(5));
        // Multi-line data with CRLF endings is joined and parsed as one value.
        let raw = "data: {\r\ndata: \"k\": 1\r\ndata: }";
        let v2 = event_data_to_json(raw).unwrap();
        assert_eq!(v2["k"], 1);
    }
}
