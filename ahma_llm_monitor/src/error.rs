use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

/// Classification of a non-success HTTP response from an LLM provider.
///
/// Derived once, here, from the HTTP status plus the provider's structured
/// error body (OpenAI `error.code`/`error.type`, Anthropic `error.type`,
/// Ollama's bare `error` string). Consumers branch on this instead of
/// substring-matching rendered error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorKind {
    /// HTTP 429 — the provider throttled the request.
    RateLimited,
    /// HTTP 401/403 — missing or invalid API key, or insufficient permissions.
    Auth,
    /// The prompt exceeded the model's context window (OpenAI
    /// `context_length_exceeded`, Anthropic "prompt is too long", …).
    ContextLengthExceeded,
    /// Any other 4xx — the request itself was rejected; retrying the same
    /// request will not help.
    InvalidRequest,
    /// 5xx — transient server-side failure (including Anthropic's 529
    /// "overloaded").
    Server,
    /// A status outside the 4xx/5xx classes (should not normally occur).
    Other,
}

/// Errors that can occur during LLM monitor operations.
#[derive(Debug, Error)]
pub enum LlmMonitorError {
    /// The provider answered with a non-success HTTP status.
    ///
    /// `message` keeps the provider-reported error text (the extracted
    /// `error.message` when the body was structured JSON, otherwise the raw
    /// body) so nothing is lost for display/logging; `kind` carries the
    /// classification consumers branch on.
    #[error("LLM API error (HTTP {status}): {message}")]
    Api {
        /// HTTP status code of the response.
        status: u16,
        /// Classification of the failure.
        kind: ApiErrorKind,
        /// Provider-reported error message (or the raw response body).
        message: String,
        /// `Retry-After` from the response headers, when present.
        retry_after: Option<Duration>,
    },

    /// The endpoint could not be reached (DNS/TCP/TLS connect failure).
    #[error("cannot connect to LLM endpoint: {0}")]
    Connect(#[source] reqwest::Error),

    /// The request (or a read on its response) timed out. Carries the
    /// transport error when the HTTP layer reported the timeout (its text
    /// names the URL); `None` when the caller's own deadline elapsed.
    #[error("LLM request timed out{}", .0.as_ref().map(|e| format!(": {e}")).unwrap_or_default())]
    Timeout(Option<reqwest::Error>),

    /// Any other transport-level failure (broken stream, decode error, …).
    #[error("HTTP request failed: {0}")]
    Http(#[source] reqwest::Error),

    /// The LLM API returned a response that couldn't be parsed.
    #[error("LLM returned unexpected response: {0}")]
    Parse(String),
}

/// Classify transport errors at the conversion boundary so consumers can
/// distinguish "endpoint unreachable" from "endpoint too slow" without
/// inspecting error text.
impl From<reqwest::Error> for LlmMonitorError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            Self::Timeout(Some(e))
        } else if e.is_connect() {
            Self::Connect(e)
        } else {
            Self::Http(e)
        }
    }
}

impl LlmMonitorError {
    /// Build a typed [`LlmMonitorError::Api`] from a non-success HTTP
    /// response's status, `Retry-After` header, and body.
    pub fn from_api_response(status: u16, retry_after: Option<Duration>, body: &str) -> Self {
        let (extracted, code) = extract_provider_error(body);
        let message = extracted.unwrap_or_else(|| body.trim().to_string());
        let kind = classify_api_error(status, code.as_deref(), &message);
        Self::Api {
            status,
            kind,
            message,
            retry_after,
        }
    }

    /// The API error classification, when this is an [`Self::Api`] error.
    pub fn api_kind(&self) -> Option<ApiErrorKind> {
        match self {
            Self::Api { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// The provider-reported error message, when one was captured.
    pub fn provider_message(&self) -> Option<&str> {
        match self {
            Self::Api { message, .. } => Some(message),
            _ => None,
        }
    }

    /// The `Retry-After` duration the provider asked for, when present.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// True when the request (or the caller's own deadline) timed out.
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout(_))
    }

    /// True when the provider rejected the request in a way that suggests the
    /// tool definitions were the problem (the model or endpoint does not
    /// support tools), so retrying the turn *without* tools may succeed.
    ///
    /// Preserves the pre-typed heuristic exactly: any HTTP 400, or a provider
    /// message mentioning "tool" / "not supported". Transport and parse
    /// failures never qualify.
    pub fn is_tools_rejected(&self) -> bool {
        match self {
            Self::Api {
                status, message, ..
            } => {
                *status == 400 || {
                    let m = message.to_ascii_lowercase();
                    m.contains("tool") || m.contains("not supported")
                }
            }
            _ => false,
        }
    }
}

/// Extract `(message, machine_readable_code)` from a provider error body.
///
/// Understood shapes:
/// * OpenAI: `{"error": {"message": …, "type": …, "code": …}}`
/// * Anthropic: `{"type": "error", "error": {"type": …, "message": …}}`
/// * Ollama: `{"error": "…"}`
///
/// Returns `(None, None)` for anything else so the raw body is kept.
fn extract_provider_error(body: &str) -> (Option<String>, Option<String>) {
    let Ok(v) = serde_json::from_str::<Value>(body.trim()) else {
        return (None, None);
    };
    match v.get("error") {
        Some(Value::String(s)) => (Some(s.clone()), None),
        Some(Value::Object(o)) => {
            let message = o.get("message").and_then(Value::as_str).map(String::from);
            // OpenAI's `code` is the most specific signal; both flavors carry
            // a `type` as fallback.
            let code = o
                .get("code")
                .and_then(Value::as_str)
                .or_else(|| o.get("type").and_then(Value::as_str))
                .map(String::from);
            (message, code)
        }
        _ => (None, None),
    }
}

/// Map an HTTP status plus the provider's machine-readable code/message to an
/// [`ApiErrorKind`]. Context overflow wins over the status class because
/// providers report it as a generic 400/413.
fn classify_api_error(status: u16, code: Option<&str>, message: &str) -> ApiErrorKind {
    if code == Some("context_length_exceeded") || looks_like_context_overflow(message) {
        return ApiErrorKind::ContextLengthExceeded;
    }
    match status {
        401 | 403 => ApiErrorKind::Auth,
        429 => ApiErrorKind::RateLimited,
        400..=499 => ApiErrorKind::InvalidRequest,
        500..=599 => ApiErrorKind::Server,
        _ => ApiErrorKind::Other,
    }
}

/// Provider phrasings for "the prompt does not fit the context window":
/// OpenAI/compatible ("maximum context length"), Anthropic ("prompt is too
/// long"), llama.cpp/Ollama variants.
fn looks_like_context_overflow(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "context length",
        "context window",
        "prompt is too long",
        "maximum context",
        "too many tokens",
    ]
    .iter()
    .any(|p| m.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16, body: &str) -> LlmMonitorError {
        LlmMonitorError::from_api_response(status, None, body)
    }

    #[test]
    fn openai_429_classifies_rate_limited_and_carries_retry_after() {
        let body = r#"{"error":{"message":"Rate limit reached for gpt-4o","type":"tokens","code":"rate_limit_exceeded"}}"#;
        let err = LlmMonitorError::from_api_response(429, Some(Duration::from_secs(17)), body);
        assert_eq!(err.api_kind(), Some(ApiErrorKind::RateLimited));
        assert_eq!(err.retry_after(), Some(Duration::from_secs(17)));
        assert_eq!(
            err.provider_message(),
            Some("Rate limit reached for gpt-4o")
        );
    }

    #[test]
    fn auth_statuses_classify_as_auth() {
        let openai_401 = api(
            401,
            r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#,
        );
        assert_eq!(openai_401.api_kind(), Some(ApiErrorKind::Auth));
        assert_eq!(
            openai_401.provider_message(),
            Some("Incorrect API key provided")
        );

        let anthropic_403 = api(
            403,
            r#"{"type":"error","error":{"type":"permission_error","message":"Your API key does not have permission"}}"#,
        );
        assert_eq!(anthropic_403.api_kind(), Some(ApiErrorKind::Auth));
    }

    #[test]
    fn openai_context_length_code_classifies_as_context_overflow() {
        let err = api(
            400,
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#,
        );
        assert_eq!(err.api_kind(), Some(ApiErrorKind::ContextLengthExceeded));
    }

    #[test]
    fn anthropic_prompt_too_long_classifies_as_context_overflow() {
        let err = api(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210522 tokens > 200000 maximum"}}"#,
        );
        assert_eq!(err.api_kind(), Some(ApiErrorKind::ContextLengthExceeded));
    }

    #[test]
    fn plain_500_is_server_and_keeps_raw_body() {
        let err = api(500, "server exploded");
        assert_eq!(err.api_kind(), Some(ApiErrorKind::Server));
        assert_eq!(err.provider_message(), Some("server exploded"));
        // Anthropic's 529 "overloaded" is a transient server failure too.
        let overloaded = api(
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        assert_eq!(overloaded.api_kind(), Some(ApiErrorKind::Server));
    }

    #[test]
    fn ollama_string_error_body_message_extracted() {
        let err = api(404, r#"{"error":"model 'nope' not found"}"#);
        assert_eq!(err.api_kind(), Some(ApiErrorKind::InvalidRequest));
        assert_eq!(err.provider_message(), Some("model 'nope' not found"));
    }

    #[test]
    fn tools_rejection_heuristic_preserved() {
        // Any 400 qualifies (pre-typed behavior: message contained "400").
        assert!(api(400, "bad request").is_tools_rejected());
        // A non-400 whose message names tools qualifies.
        assert!(api(500, "tools are not supported by this model").is_tools_rejected());
        // A plain 500 does not.
        assert!(!api(500, "server exploded").is_tools_rejected());
        // Transport/parse failures never qualify.
        assert!(!LlmMonitorError::Parse("missing choices[0].message".into()).is_tools_rejected());
        assert!(!LlmMonitorError::Timeout(None).is_tools_rejected());
    }

    #[test]
    fn display_keeps_status_and_provider_message() {
        let err = api(503, r#"{"error":{"message":"upstream busy"}}"#);
        let text = err.to_string();
        assert!(text.contains("503"), "{text}");
        assert!(text.contains("upstream busy"), "{text}");
    }

    #[test]
    fn non_json_body_is_preserved_verbatim() {
        let err = api(502, "<html>Bad Gateway</html>");
        assert_eq!(err.provider_message(), Some("<html>Bad Gateway</html>"));
        assert_eq!(err.api_kind(), Some(ApiErrorKind::Server));
    }

    #[test]
    fn caller_deadline_timeout_displays_without_source() {
        let err = LlmMonitorError::Timeout(None);
        assert_eq!(err.to_string(), "LLM request timed out");
        assert!(err.is_timeout());
    }
}
