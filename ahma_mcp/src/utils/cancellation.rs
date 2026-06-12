//! User-facing cancellation message formatting.
//!
//! Converts cryptic transport-level messages like `"Canceled: canceled"` into
//! actionable text that explains what happened and what to do next.  Used by
//! the MCP progress push and the `await`/`status` result paths.

const CANCELLATION_SUGGESTIONS: &str = "Suggestions: retry the command; capture server logs with `2>&1`; if this happened immediately, verify MCP roots/list handshake completed before tools/call";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancellationKind {
    UnknownSource,
    McpCancellation,
    Timeout,
    UserInitiated,
    Generic,
}

impl CancellationKind {
    fn as_message(self) -> &'static str {
        match self {
            CancellationKind::UnknownSource => "Operation was cancelled (source: unknown)",
            CancellationKind::McpCancellation => "MCP cancellation received",
            CancellationKind::Timeout => "Operation timed out",
            CancellationKind::UserInitiated => "User-initiated cancellation",
            CancellationKind::Generic => "Operation was cancelled",
        }
    }
}

fn detect_cancellation_kind(raw_message: &str) -> Option<CancellationKind> {
    let lower = raw_message.to_lowercase();

    if lower == "canceled" {
        return Some(CancellationKind::UnknownSource);
    }

    // Order matters: more specific patterns first, then broader patterns
    const ORDERED_PATTERNS: &[(&[&str], CancellationKind)] = &[
        (&["canceled: canceled"], CancellationKind::UnknownSource),
        (
            &["task cancelled for reason"],
            CancellationKind::McpCancellation,
        ),
        (&["timeout"], CancellationKind::Timeout),
        (&["user", "request"], CancellationKind::UserInitiated),
        (&["cancel"], CancellationKind::Generic),
    ];

    ORDERED_PATTERNS
        .iter()
        .find_map(|(patterns, kind)| patterns.iter().any(|p| lower.contains(p)).then_some(*kind))
}

fn format_cancellation_context(tool_name: Option<&str>, id: Option<&str>) -> Vec<String> {
    [
        tool_name.map(|t| format!("Tool: {t}")),
        id.map(|o| format!("Operation: {o}")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Format a cancellation error message to be more informative.
///
/// Non-cancellation messages are passed through unchanged.
///
/// # Arguments
/// * `raw_message` - The raw error/cancellation message from rmcp or the MCP client
/// * `tool_name` - Optional tool name that was being executed
/// * `id` - Optional operation ID for reference
pub fn format_cancellation_message(
    raw_message: &str,
    tool_name: Option<&str>,
    id: Option<&str>,
) -> String {
    let Some(kind) = detect_cancellation_kind(raw_message) else {
        return raw_message.to_string();
    };

    let mut parts = vec![kind.as_message().to_string()];
    parts.extend(format_cancellation_context(tool_name, id));
    parts.push(format!("Raw: {raw_message}"));
    parts.push(CANCELLATION_SUGGESTIONS.to_string());
    parts.join(". ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_cancellation_kind_variants() {
        assert_eq!(
            detect_cancellation_kind("Canceled: canceled"),
            Some(CancellationKind::UnknownSource)
        );
        assert_eq!(
            detect_cancellation_kind("task cancelled for reason: client disconnected"),
            Some(CancellationKind::McpCancellation)
        );
        assert_eq!(
            detect_cancellation_kind("operation timeout waiting for subprocess"),
            Some(CancellationKind::Timeout)
        );
        assert_eq!(
            detect_cancellation_kind("cancelled by user request"),
            Some(CancellationKind::UserInitiated)
        );
        assert_eq!(
            detect_cancellation_kind("cancelled"),
            Some(CancellationKind::Generic)
        );
        assert_eq!(detect_cancellation_kind("some unrelated error"), None);
    }

    #[test]
    fn test_format_cancellation_message_passthrough_for_non_cancellation() {
        let raw = "failed to parse json";
        assert_eq!(format_cancellation_message(raw, None, None), raw);
    }

    #[test]
    fn test_format_cancellation_message_includes_context_and_suggestions() {
        let message =
            format_cancellation_message("Canceled: canceled", Some("cargo_build"), Some("op-123"));

        assert!(message.contains("Operation was cancelled (source: unknown)"));
        assert!(message.contains("Tool: cargo_build"));
        assert!(message.contains("Operation: op-123"));
        assert!(message.contains("Raw: Canceled: canceled"));
        assert!(message.contains("Suggestions: retry the command"));
    }
}
