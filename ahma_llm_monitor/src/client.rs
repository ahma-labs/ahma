use std::time::Duration;

use futures::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use ahma_common::config::warn_if_looks_like_literal_secret;

use crate::anthropic;
use crate::error::LlmMonitorError;
use crate::prompt::build_messages;

/// Max time to establish a TCP/TLS connection to the LLM endpoint.
///
/// Catches an unreachable or wedged local server (e.g. Ollama not actually
/// listening) quickly instead of waiting on the OS default, which can be
/// minutes.
const LLM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Max time to wait between successive bytes from the LLM endpoint.
///
/// This is a *per-read* inactivity window, not a cap on total request
/// duration: a model that is actively producing tokens (streaming) or busy
/// generating a non-streaming completion keeps the connection warm and never
/// trips it. It exists so a silently dropped/hung connection surfaces as an
/// error rather than leaving the agent loop — and the TUI's elapsed counter —
/// spinning forever with no answer and no error.
const LLM_READ_TIMEOUT: Duration = Duration::from_secs(300);

/// Build the shared HTTP client with connect/read timeouts so a stalled or
/// unreachable LLM endpoint fails loudly instead of hanging indefinitely.
///
/// Falls back to a default client if the builder rejects the configuration
/// (should never happen with static timeouts, but we must not panic at
/// construction time).
/// The one HTTP client every [`LlmClient`] shares.
///
/// Its configuration is two static timeouts — nothing per-caller — so a client
/// per `LlmClient` bought nothing and cost a fresh rustls `ClientConfig`, root
/// store and connection pool each time. Provider discovery builds six clients
/// concurrently at TUI startup, all pointing at the same localhost endpoints;
/// sharing one lets them share connections instead of competing pools.
/// `reqwest::Client` is an `Arc` internally, so cloning is a refcount bump.
static HTTP_CLIENT: std::sync::LazyLock<Client> = std::sync::LazyLock::new(|| {
    Client::builder()
        .connect_timeout(LLM_CONNECT_TIMEOUT)
        .read_timeout(LLM_READ_TIMEOUT)
        .build()
        .unwrap_or_else(|e| {
            warn!("Failed to build HTTP client with timeouts ({e}); using default client");
            Client::new()
        })
});

/// Read window for a model served on **this machine** (Ollama, LM Studio,
/// llama-server).
///
/// A local server sends nothing at all while it reads (prefills) the prompt,
/// and on a laptop a 27B model can take minutes over a 40k-token prompt —
/// longer than [`LLM_READ_TIMEOUT`]. The short window buys nothing locally: a
/// server that is actually down refuses the connection instantly (a connect
/// error, still retried), so the only thing a timeout ever caught here was a
/// busy model, which it then made start over.
const LLM_LOCAL_READ_TIMEOUT: Duration = Duration::from_secs(30 * 60);

static LOCAL_HTTP_CLIENT: std::sync::LazyLock<Client> = std::sync::LazyLock::new(|| {
    Client::builder()
        .connect_timeout(LLM_CONNECT_TIMEOUT)
        .read_timeout(LLM_LOCAL_READ_TIMEOUT)
        .build()
        .unwrap_or_else(|e| {
            warn!("Failed to build local HTTP client with timeouts ({e}); using default client");
            Client::new()
        })
});

fn build_http_client(local: bool) -> Client {
    if local {
        LOCAL_HTTP_CLIENT.clone()
    } else {
        HTTP_CLIENT.clone()
    }
}

/// Whether `base_url` points at this machine (a loopback host).
pub fn is_loopback_url(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Maximum number of *retries* (extra attempts) on a transient LLM failure.
const LLM_MAX_RETRIES: u32 = 3;
/// Backoff before the first retry; doubles each attempt up to [`LLM_RETRY_MAX_DELAY`].
const LLM_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
const LLM_RETRY_MAX_DELAY: Duration = Duration::from_secs(8);

/// HTTP statuses worth retrying — transient overload/server conditions. Other
/// 4xx (400/401/403/404) are caller errors and must never be retried.
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504)
}

/// Transport errors worth retrying: a timed-out or momentarily unreachable
/// endpoint — not a malformed request or a decoding error.
fn is_retryable_transport_err(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect()
}

/// Exponential backoff for retry `attempt` (0-based), capped.
fn backoff_delay(attempt: u32) -> Duration {
    LLM_RETRY_BASE_DELAY
        .saturating_mul(1u32 << attempt.min(5))
        .min(LLM_RETRY_MAX_DELAY)
}

/// `Retry-After` from a response's headers, when present as (possibly
/// fractional) delta-seconds. HTTP-date form is rare on LLM endpoints and is
/// ignored rather than mis-parsed.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs: f64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

/// Convert a non-success HTTP response into a typed [`LlmMonitorError::Api`],
/// consuming the body. The raw status/body pair is logged here, once, so
/// nothing the provider said is lost even when the extracted provider message
/// is shorter than the full body.
async fn api_error_from_response(response: reqwest::Response) -> LlmMonitorError {
    let status = response.status();
    let retry_after = parse_retry_after(response.headers());
    let body_text = response.text().await.unwrap_or_default();
    warn!(status = %status, body = %body_text, "llm: HTTP error from LLM endpoint");
    LlmMonitorError::from_api_response(status.as_u16(), retry_after, &body_text)
}

/// Send an HTTP request with bounded exponential backoff on transient failures
/// (timeouts/connect errors and 429/5xx). `build` constructs a fresh request per
/// attempt. With `retry_timeouts` false a timeout is returned at once: re-sending
/// to a local model that is merely slow restarts its prompt reading from zero. A non-retryable response (success or a non-429 4xx) is returned as
/// `Ok` so the caller's own status handling runs; a non-retryable transport
/// error, or exhaustion of all retries, returns the last error/response.
async fn send_with_backoff<F>(
    build: F,
    retry_timeouts: bool,
) -> Result<reqwest::Response, LlmMonitorError>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut attempt = 0u32;
    loop {
        match build().send().await {
            Ok(resp) => {
                if is_retryable_status(resp.status()) && attempt < LLM_MAX_RETRIES {
                    let delay = backoff_delay(attempt);
                    warn!(status = %resp.status(), attempt = attempt + 1, ?delay, "llm: retryable HTTP status — backing off and retrying");
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                let retryable =
                    is_retryable_transport_err(&e) && (retry_timeouts || !e.is_timeout());
                if retryable && attempt < LLM_MAX_RETRIES {
                    let delay = backoff_delay(attempt);
                    warn!(error = %e, attempt = attempt + 1, ?delay, "llm: retryable transport error — backing off and retrying");
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue;
                }
                return Err(LlmMonitorError::from(e));
            }
        }
    }
}

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

/// Token usage metrics returned by the LLM.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// One assistant turn returned from a non-streaming chat completion.
#[derive(Debug, Clone)]
pub struct ChatCompletionResponse {
    pub content: String,
    pub tool_calls: Vec<ChatToolCall>,
    pub assistant_message: Value,
    pub usage: Option<TokenUsage>,
    /// Why the model stopped generating, normalized across providers:
    /// `"length"` means the response was cut off by a token/context limit
    /// (OpenAI/Ollama's own `"length"`, Anthropic's `"max_tokens"`) rather
    /// than the model actually finishing its turn. `None` when the provider
    /// didn't report one.
    pub finish_reason: Option<String>,
}

impl ChatCompletionResponse {
    /// Whether this response was cut off by a length/token limit rather than
    /// the model finishing its turn on its own. See [`Self::finish_reason`].
    pub fn is_length_truncated(&self) -> bool {
        matches!(self.finish_reason.as_deref(), Some("length"))
    }
}

/// A discovered local LLM provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalProvider {
    /// Display name, e.g. "Ollama" or "llama-server".
    pub name: String,
    /// Base URL, e.g. "http://localhost:11434/v1".
    pub base_url: String,
    /// Models available from this provider.
    pub models: Vec<String>,
}

/// Which provider wire format an [`LlmClient`] speaks.
///
/// ahma's internal message shape is OpenAI-compatible; the Anthropic flavor
/// translates to/from the native Messages API at the HTTP boundary (see
/// [`crate::anthropic`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApiFlavor {
    /// OpenAI-compatible `/chat/completions` (Ollama, llama.cpp, LM Studio, OpenAI…).
    #[default]
    OpenAi,
    /// Anthropic native Messages API (`/messages`, `x-api-key`).
    Anthropic,
}

/// An LLM client for issue detection and chat.
///
/// Speaks either the OpenAI-compatible API or the native Anthropic Messages
/// API depending on [`ApiFlavor`]; the public methods are identical across
/// flavors so callers need not branch.
#[derive(Debug, Clone)]
pub struct LlmClient {
    http: Client,
    /// The endpoint is on this machine: long read window, no timeout retries.
    local: bool,
    base_url: String,
    model: String,
    api_key: Option<String>,
    flavor: ApiFlavor,
    /// Anthropic only: send `thinking: {type: "adaptive"}` on chat/tool turns.
    thinking: bool,
    /// Optional context-window override (tokens). Sent to Ollama as
    /// `options.num_ctx`; ignored for non-Ollama OpenAI endpoints and Anthropic.
    num_ctx: Option<u32>,
}

impl LlmClient {
    /// Whole-request cap for a non-streaming completion: two minutes for a
    /// remote API, the local read window for a model on this machine (which is
    /// silent until it has read the entire prompt).
    fn request_timeout(&self) -> Duration {
        if self.local {
            LLM_LOCAL_READ_TIMEOUT
        } else {
            Duration::from_secs(120)
        }
    }

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
        let base_url = base_url.into().trim_end_matches('/').to_string();
        // Auto-select the Anthropic flavor for the canonical host so existing
        // call sites that only thread a base_url light up with no extra
        // plumbing. Explicit configuration overrides via `with_flavor`.
        let flavor = if anthropic::looks_like_anthropic(&base_url) {
            ApiFlavor::Anthropic
        } else {
            ApiFlavor::OpenAi
        };
        // For Anthropic, fall back to ANTHROPIC_API_KEY when no key was passed
        // (the interactive TUI constructs clients without an explicit key).
        let api_key = match (flavor, api_key) {
            (ApiFlavor::Anthropic, None) => std::env::var("ANTHROPIC_API_KEY").ok(),
            (_, key) => key,
        };
        let local = is_loopback_url(&base_url);
        Self {
            http: build_http_client(local),
            local,
            base_url,
            model: model.into(),
            api_key,
            flavor,
            // Adaptive thinking is on by default for Anthropic chat/tool turns.
            thinking: flavor == ApiFlavor::Anthropic,
            num_ctx: None,
        }
    }

    /// Set the context-window override (tokens). Only sent to Ollama endpoints;
    /// see [`ahma_common::config::endpoint_supports_num_ctx`].
    pub fn with_num_ctx(mut self, num_ctx: Option<u32>) -> Self {
        self.num_ctx = num_ctx;
        self
    }

    /// Whether this client will actually send a `num_ctx` override on requests
    /// (i.e. one is configured *and* the endpoint is Ollama).
    pub fn sends_num_ctx(&self) -> bool {
        self.num_ctx.is_some()
            && ahma_common::config::endpoint_supports_num_ctx(
                &self.base_url,
                match self.flavor {
                    ApiFlavor::OpenAi => ahma_common::config::ProviderKind::OpenAi,
                    ApiFlavor::Anthropic => ahma_common::config::ProviderKind::Anthropic,
                },
            )
    }

    /// Inject `options.num_ctx` into an OpenAI-compatible request body when a
    /// context override is configured and the endpoint is Ollama. No-op
    /// otherwise, so hosted clouds never see an unknown field.
    fn apply_num_ctx(&self, body: &mut Value) {
        if let Some(n) = self.num_ctx
            && self.sends_num_ctx()
        {
            body["options"] = json!({ "num_ctx": n });
        }
    }

    /// Override the wire-format flavor (e.g. when a `kind = "anthropic"`
    /// provider points at a proxy whose URL doesn't carry the Anthropic host).
    ///
    /// Resets `thinking` to the flavor default (on for Anthropic); call
    /// [`Self::with_thinking`] *after* this to override.
    pub fn with_flavor(mut self, flavor: ApiFlavor) -> Self {
        self.flavor = flavor;
        if flavor == ApiFlavor::Anthropic && self.api_key.is_none() {
            self.api_key = std::env::var("ANTHROPIC_API_KEY").ok();
        }
        self.thinking = flavor == ApiFlavor::Anthropic;
        self
    }

    /// Enable or disable adaptive thinking on Anthropic chat/tool turns.
    pub fn with_thinking(mut self, thinking: bool) -> Self {
        self.thinking = thinking;
        self
    }

    /// Build a client from a resolved named provider, honoring its `kind`.
    ///
    /// Unlike [`Self::new`]'s URL heuristic, this uses the explicit
    /// `kind = "anthropic"` from `~/.ahma/config.toml`, so it works for proxies
    /// and gateways whose `base_url` doesn't carry the Anthropic host.
    pub fn for_provider(provider: &ahma_common::config::ResolvedProvider) -> Self {
        let flavor = match provider.kind {
            ahma_common::config::ProviderKind::OpenAi => ApiFlavor::OpenAi,
            ahma_common::config::ProviderKind::Anthropic => ApiFlavor::Anthropic,
        };
        Self::new(
            provider.base_url.clone(),
            provider.default_model.clone(),
            provider.api_key.clone(),
        )
        .with_flavor(flavor)
        .with_num_ctx(provider.num_ctx)
    }

    /// The wire-format flavor this client speaks.
    pub fn flavor(&self) -> ApiFlavor {
        self.flavor
    }

    /// Get the base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The model this client asks for.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The models an Ollama server on this machine has loaded in memory, from
    /// its native `GET /api/ps` beside the OpenAI-compatible `/v1` root.
    ///
    /// `None` when that cannot be known — a remote endpoint (never asked), a
    /// server that is not Ollama, or any error. Short timeout: this only
    /// decides whether the status line says "loading" or "reading", and must
    /// never hold up the request it describes.
    pub async fn loaded_local_models(&self) -> Option<Vec<String>> {
        if !self.local {
            return None;
        }
        let root = self.base_url.trim_end_matches('/');
        let root = root.strip_suffix("/v1").unwrap_or(root);
        let response = self
            .http
            .get(format!("{root}/api/ps"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: Value = response.json().await.ok()?;
        let models = body.get("models")?.as_array()?;
        Some(
            models
                .iter()
                .filter_map(|m| m.get("name").or_else(|| m.get("model")))
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        )
    }

    /// Whether this client's model is among `loaded` (as reported by
    /// [`Self::loaded_local_models`]); Ollama names an untagged model
    /// `<name>:latest`.
    pub fn is_model_resident(&self, loaded: &[String]) -> bool {
        loaded.iter().any(|name| {
            name == &self.model
                || name
                    .strip_suffix(":latest")
                    .is_some_and(|base| base == self.model)
        })
    }

    /// Apply flavor-appropriate auth/version headers to a request.
    ///
    /// OpenAI uses `Authorization: Bearer`; Anthropic uses `x-api-key` plus the
    /// required `anthropic-version` header.
    fn apply_auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.flavor {
            ApiFlavor::OpenAi => match &self.api_key {
                Some(key) => request.bearer_auth(key),
                None => request,
            },
            ApiFlavor::Anthropic => {
                let request = request.header("anthropic-version", anthropic::ANTHROPIC_VERSION);
                match &self.api_key {
                    Some(key) => request.header("x-api-key", key),
                    None => request,
                }
            }
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

        // Terse classification: no adaptive thinking (the 256-token cap leaves
        // no room for a thinking budget, and CLEAN/summary needs none).
        let (url, body) = match self.flavor {
            ApiFlavor::OpenAi => (
                format!("{}/chat/completions", self.base_url),
                json!({
                    "model": self.model,
                    "messages": messages,
                    "max_tokens": 256,
                    "temperature": 0.0,
                }),
            ),
            ApiFlavor::Anthropic => {
                let (system, amsgs) = anthropic::openai_to_anthropic(&messages);
                (
                    format!("{}/messages", self.base_url),
                    anthropic::build_messages_body(
                        &self.model,
                        system,
                        amsgs,
                        &[],
                        false,
                        256,
                        false,
                    ),
                )
            }
        };

        debug!(
            "Sending chunk ({} chars) to LLM at {} for analysis",
            chunk.len(),
            self.base_url
        );

        let request = self.apply_auth(self.http.post(url).json(&body).timeout(timeout));

        let response = tokio::time::timeout(timeout, request.send())
            .await
            .map_err(|_| LlmMonitorError::Timeout(None))?
            .map_err(LlmMonitorError::from)?;

        if !response.status().is_success() {
            return Err(api_error_from_response(response).await);
        }

        let json: Value = response.json().await.map_err(LlmMonitorError::from)?;

        let pointer = match self.flavor {
            ApiFlavor::OpenAi => "/choices/0/message/content",
            ApiFlavor::Anthropic => "/content/0/text",
        };
        let text = json
            .pointer(pointer)
            .and_then(Value::as_str)
            .ok_or_else(|| LlmMonitorError::Parse(format!("missing {pointer}")))?
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

    /// Build the (url, body) pair for [`chat_stream`](Self::chat_stream),
    /// isolating the per-flavor request-shaping match (OpenAI's flat message
    /// list vs. Anthropic's system/messages split) from the stream plumbing
    /// that follows it.
    fn build_chat_stream_request(
        &self,
        messages: &[ChatMessage],
        system_prompt: Option<&str>,
    ) -> (String, Value) {
        match self.flavor {
            ApiFlavor::OpenAi => {
                let mut body = build_chat_stream_body(&self.model, messages, system_prompt);
                self.apply_num_ctx(&mut body);
                (format!("{}/chat/completions", self.base_url), body)
            }
            ApiFlavor::Anthropic => {
                let mut openai_msgs: Vec<Value> = Vec::with_capacity(messages.len() + 1);
                if let Some(sys) = system_prompt {
                    openai_msgs.push(json!({"role": "system", "content": sys}));
                }
                for message in messages {
                    openai_msgs.push(message.as_openai_message());
                }
                let (system, amsgs) = anthropic::openai_to_anthropic(&openai_msgs);
                (
                    format!("{}/messages", self.base_url),
                    anthropic::build_messages_body(
                        &self.model,
                        system,
                        amsgs,
                        &[],
                        true,
                        anthropic::DEFAULT_MAX_TOKENS,
                        self.thinking,
                    ),
                )
            }
        }
    }

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

        let (url, body) = self.build_chat_stream_request(&messages, system_prompt);
        let http = self.http.clone();
        let api_key = self.api_key.clone();
        let flavor = self.flavor;
        info!(
            model = %self.model,
            messages = messages.len(),
            url = %url,
            "llm: starting chat stream"
        );

        stream::unfold(
            ChatStreamState::Starting {
                http,
                url,
                api_key,
                body,
                flavor,
            },
            |state| async move {
                match state {
                    ChatStreamState::Starting {
                        http,
                        url,
                        api_key,
                        body,
                        flavor,
                    } => chat_stream_start(http, url, api_key, body, flavor).await,
                    ChatStreamState::Streaming {
                        stream,
                        buffer,
                        flavor,
                    } => chat_stream_poll(stream, buffer, flavor).await,
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
        messages: &[Value],
        tools: &[Value],
    ) -> Result<ChatCompletionResponse, LlmMonitorError> {
        let (url, body) = self.build_chat_completion_request_payload(messages, tools);

        let started = std::time::Instant::now();
        info!(
            model = %self.model,
            messages = messages.len(),
            tools = tools.len(),
            stream = false,
            timeout_secs = self.request_timeout().as_secs(),
            "llm: requesting chat completion with tools (non-streaming)"
        );

        let response = send_with_backoff(|| {
            self.apply_auth(
                self.http
                    .post(&url)
                    .json(&body)
                    .timeout(self.request_timeout()),
            )
        }, !self.local)
        .await
        .inspect_err(|e| {
            let elapsed_ms = started.elapsed().as_millis();
            if e.is_timeout() {
                warn!(model = %self.model, elapsed_ms, "llm: chat completion timed out after retries — model too slow, or prompt exceeds its context window");
            } else {
                warn!(model = %self.model, elapsed_ms, error = %e, "llm: chat completion request failed after retries");
            }
        })?;
        if !response.status().is_success() {
            return Err(api_error_from_response(response).await);
        }

        let json = response
            .json::<Value>()
            .await
            .map_err(LlmMonitorError::from)?;
        let parsed = match self.flavor {
            ApiFlavor::OpenAi => parse_chat_completion_response(json),
            ApiFlavor::Anthropic => anthropic::parse_messages_response(json),
        };
        if let Ok(ref resp) = parsed {
            self.log_chat_completion_metrics(resp, started, "chat completion received");
        }
        parsed
    }

    fn build_chat_completion_request_payload(
        &self,
        messages: &[Value],
        tools: &[Value],
    ) -> (String, Value) {
        match self.flavor {
            ApiFlavor::OpenAi => {
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
                self.apply_num_ctx(&mut body);
                (format!("{}/chat/completions", self.base_url), body)
            }
            ApiFlavor::Anthropic => {
                let (system, amsgs) = anthropic::openai_to_anthropic(messages);
                let atools = anthropic::openai_tools_to_anthropic(tools);
                (
                    format!("{}/messages", self.base_url),
                    anthropic::build_messages_body(
                        &self.model,
                        system,
                        amsgs,
                        &atools,
                        false,
                        anthropic::DEFAULT_MAX_TOKENS,
                        self.thinking,
                    ),
                )
            }
        }
    }

    fn log_chat_completion_metrics(
        &self,
        resp: &ChatCompletionResponse,
        started: std::time::Instant,
        label: &str,
    ) {
        let (prompt_tokens, completion_tokens) = resp
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0));
        info!(
            model = %self.model,
            elapsed_ms = started.elapsed().as_millis(),
            content_chars = resp.content.len(),
            tool_calls = resp.tool_calls.len(),
            prompt_tokens,
            completion_tokens,
            "llm: {label}"
        );
    }

    /// Streaming variant of [`Self::chat_completion_with_tools`].
    ///
    /// Sends `stream: true` and forwards visible/reasoning fragments through
    /// `deltas` as they arrive, while accumulating the full assistant turn
    /// (visible content + any tool calls) and returning it once the stream
    /// completes. This gives the TUI a live "is it actually working" signal and
    /// lets it surface reasoning tokens, instead of blocking on one opaque
    /// 120-second request.
    ///
    /// Only the OpenAI flavor streams here; Anthropic falls back to the
    /// non-streaming path (its tool-call streaming format differs) and emits its
    /// visible content as a single delta so callers still see output.
    pub async fn chat_completion_with_tools_streaming(
        &self,
        messages: &[Value],
        tools: &[Value],
        deltas: tokio::sync::mpsc::Sender<StreamDelta>,
    ) -> Result<ChatCompletionResponse, LlmMonitorError> {
        if self.flavor != ApiFlavor::OpenAi {
            // Anthropic: keep the proven non-streaming path, but still feed the
            // visible content to the UI as one delta so it isn't silent.
            let resp = self.chat_completion_with_tools(messages, tools).await?;
            if !resp.content.is_empty() {
                let _ = deltas
                    .send(StreamDelta::Content(resp.content.clone()))
                    .await;
            }
            return Ok(resp);
        }

        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": 0.2,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
            body["tool_choice"] = json!("auto");
        }
        self.apply_num_ctx(&mut body);

        let started = std::time::Instant::now();
        info!(
            model = %self.model,
            messages_len = body["messages"].as_array().map(|a| a.len()).unwrap_or(0),
            tools = tools.len(),
            stream = true,
            num_ctx = ?self.num_ctx.filter(|_| self.sends_num_ctx()),
            "llm: requesting chat completion with tools (streaming)"
        );

        let url = format!("{}/chat/completions", self.base_url);
        let response = send_with_backoff(|| {
            self.apply_auth(
                self.http
                    .post(&url)
                    .json(&body)
                    .header("Accept", "text/event-stream"),
            )
        }, !self.local)
        .await
        .inspect_err(|e| {
            warn!(model = %self.model, elapsed_ms = started.elapsed().as_millis(), error = %e, "llm: streaming chat request failed after retries");
        })?;
        if !response.status().is_success() {
            return Err(api_error_from_response(response).await);
        }

        let byte_stream = response.bytes_stream();
        let response_json =
            drain_streaming_deltas(byte_stream, &deltas, &self.model, started).await?;
        let parsed = parse_chat_completion_response(response_json);
        if let Ok(ref resp) = parsed {
            self.log_chat_completion_metrics(resp, started, "streaming chat completion assembled");
        }
        parsed
    }

    // ─── Model discovery ──────────────────────────────────────────────────────

    /// List models available from this provider via `GET /v1/models`.
    ///
    /// Returns model IDs sorted alphabetically.  Returns an empty vec (not an
    /// error) when the endpoint is unreachable — callers should treat that as
    /// "no models available right now".
    pub async fn list_model(&self) -> Vec<String> {
        let url = format!("{}/models", self.base_url);
        // Anthropic's GET /v1/models returns the same `{data:[{id}]}` shape as
        // OpenAI; only the auth headers differ.
        let req = self.apply_auth(self.http.get(&url).timeout(Duration::from_secs(3)));
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

/// A live fragment emitted while a streaming tool-calling turn is in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamDelta {
    /// Visible assistant text (shown normally).
    Content(String),
    /// Reasoning / "thinking" text (shown in lower contrast).
    Thinking(String),
}

/// Accumulates OpenAI streaming deltas into a single assistant turn.
///
/// Streaming responses split content, reasoning, and each tool call's
/// `arguments` JSON across many `choices[0].delta` chunks; tool-call fragments
/// are keyed by their `index`. This reassembles them into the same
/// `{choices:[{message}]}` shape a non-streaming response has, so the existing
/// [`parse_chat_completion_response`] can finish the job.
#[derive(Default)]
struct StreamingToolAccumulator {
    content: String,
    thinking: String,
    /// Per-index `(id, name, arguments)` accumulators.
    tools: Vec<(String, String, String)>,
}

impl StreamingToolAccumulator {
    /// Fold one streamed chunk into the accumulator, returning any
    /// `(content, thinking)` fragments to forward live.
    fn push_chunk(&mut self, chunk: &Value) -> (Option<String>, Option<String>) {
        let delta = chunk.pointer("/choices/0/delta");
        let Some(delta) = delta else {
            return (None, None);
        };

        let content = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| {
                self.content.push_str(s);
                s.to_string()
            });

        // Reasoning fragments are non-standard: Ollama/DeepSeek use
        // `reasoning_content`, some gateways use `reasoning`.
        let thinking = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| {
                self.thinking.push_str(s);
                s.to_string()
            });

        self.accumulate_tool_call_deltas(delta);

        (content, thinking)
    }

    /// Return the `(id, name, arguments)` accumulator slot for `index`,
    /// growing `self.tools` with empty slots as new indices appear.
    fn tool_slot(&mut self, index: usize) -> &mut (String, String, String) {
        while self.tools.len() <= index {
            self.tools
                .push((String::new(), String::new(), String::new()));
        }
        &mut self.tools[index]
    }

    /// Replace `*target` with `value` when `value` is present and non-empty.
    /// Used for the `id`/`name` fields, which arrive once (not accumulated)
    /// on whichever chunk first carries them.
    fn set_if_present(target: &mut String, value: Option<&str>) {
        if let Some(v) = value
            && !v.is_empty()
        {
            *target = v.to_string();
        }
    }

    /// Fold `delta.tool_calls` fragments (keyed by `index`) into `self.tools`,
    /// growing the accumulator as new indices appear.
    fn accumulate_tool_call_deltas(&mut self, delta: &Value) {
        let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) else {
            return;
        };
        for call in calls {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let slot = self.tool_slot(index);
            Self::set_if_present(&mut slot.0, call.get("id").and_then(Value::as_str));
            Self::set_if_present(
                &mut slot.1,
                call.pointer("/function/name").and_then(Value::as_str),
            );
            if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                slot.2.push_str(args);
            }
        }
    }

    /// Reassemble into a `{choices:[{message}]}` JSON value (with optional
    /// `usage`) for [`parse_chat_completion_response`].
    fn into_response_json(self, usage: Option<Value>, finish_reason: Option<String>) -> Value {
        let tool_calls: Vec<Value> = self
            .tools
            .into_iter()
            .filter(|(_, name, _)| !name.is_empty())
            .enumerate()
            .map(|(i, (id, name, args))| {
                let id = if id.is_empty() {
                    format!("call_{i}")
                } else {
                    id
                };
                json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args },
                })
            })
            .collect();

        let mut message = json!({ "role": "assistant", "content": self.content });
        if !tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(tool_calls);
        }
        let mut choice = json!({ "message": message });
        if let Some(reason) = finish_reason {
            choice["finish_reason"] = json!(reason);
        }
        let mut out = json!({ "choices": [choice] });
        if let Some(u) = usage {
            out["usage"] = u;
        }
        out
    }
}

// ─── SSE stream state machine ─────────────────────────────────────────────────

enum ChatStreamState {
    Starting {
        http: Client,
        url: String,
        api_key: Option<String>,
        body: Value,
        flavor: ApiFlavor,
    },
    Streaming {
        stream: std::pin::Pin<Box<dyn Stream<Item = Result<bytes::Bytes, LlmMonitorError>> + Send>>,
        buffer: String,
        flavor: ApiFlavor,
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
    flavor: ApiFlavor,
) -> Option<(Result<String, LlmMonitorError>, ChatStreamState)> {
    let mut req = http
        .post(&url)
        .json(&body)
        .header("Accept", "text/event-stream");
    req = match flavor {
        ApiFlavor::OpenAi => match &api_key {
            Some(key) => req.bearer_auth(key),
            None => req,
        },
        ApiFlavor::Anthropic => {
            let req = req.header("anthropic-version", anthropic::ANTHROPIC_VERSION);
            match &api_key {
                Some(key) => req.header("x-api-key", key),
                None => req,
            }
        }
    };

    let resp = match req.send().await {
        Err(e) => {
            warn!(error = %e, "llm: chat stream request failed");
            return Some((Err(LlmMonitorError::from(e)), ChatStreamState::Done));
        }
        Ok(resp) => resp,
    };

    if !resp.status().is_success() {
        return Some((
            Err(api_error_from_response(resp).await),
            ChatStreamState::Done,
        ));
    }

    debug!("llm: streaming response started");
    use futures::TryStreamExt as _;
    let byte_stream = resp.bytes_stream().map_err(LlmMonitorError::from);
    Some((
        Ok(String::new()), // empty first yield to advance state
        ChatStreamState::Streaming {
            stream: Box::pin(byte_stream),
            buffer: String::new(),
            flavor,
        },
    ))
}

fn take_sse_line(buffer: &mut String) -> Option<String> {
    let newline_pos = buffer.find('\n')?;
    let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
    // `drain` shifts the tail in place instead of reallocating the whole
    // remaining buffer for every consumed line.
    buffer.drain(..=newline_pos);
    Some(line)
}

/// Result of parsing and folding one SSE `data:` line into the in-progress
/// accumulation state, for [`drain_streaming_deltas`].
enum StreamLineOutcome {
    /// Not a `data:` line, or not valid JSON — nothing to do.
    Skip,
    /// The `[DONE]` sentinel — the caller should stop consuming.
    Done,
    /// A parsed chunk, with any content/thinking fragments to forward live.
    Chunk {
        content: Option<String>,
        thinking: Option<String>,
    },
}

/// Parse one SSE `data:` line and fold it into `acc`/`usage`/`finish_reason`.
///
/// Pure (no I/O): isolates the per-line parsing/accumulation decision from
/// the async byte-stream loop in [`drain_streaming_deltas`], which is
/// otherwise responsible for forwarding deltas and reading more bytes.
fn process_stream_line(
    line: &str,
    acc: &mut StreamingToolAccumulator,
    usage: &mut Option<Value>,
    finish_reason: &mut Option<String>,
) -> StreamLineOutcome {
    let Some(data) = line.strip_prefix("data: ") else {
        return StreamLineOutcome::Skip;
    };
    let data = data.trim();
    if data == "[DONE]" {
        return StreamLineOutcome::Done;
    }
    let Ok(chunk) = serde_json::from_str::<Value>(data) else {
        return StreamLineOutcome::Skip;
    };
    if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
        *usage = Some(u.clone());
    }
    if let Some(reason) = chunk
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
    {
        *finish_reason = Some(reason.to_string());
    }
    let (content, thinking) = acc.push_chunk(&chunk);
    StreamLineOutcome::Chunk { content, thinking }
}

/// Forward one line's content/thinking fragments to `deltas`, logging the
/// first streamed token exactly once. Isolated from
/// [`drain_streaming_deltas`]'s line-buffering loop so that loop reads as a
/// flat sequence of "parse a line, forward its deltas" steps rather than
/// nesting two more `if let`s inside it.
async fn forward_stream_deltas(
    content: Option<String>,
    thinking: Option<String>,
    deltas: &tokio::sync::mpsc::Sender<StreamDelta>,
    model: &str,
    started: std::time::Instant,
    first_token_logged: &mut bool,
) {
    if let Some(c) = content {
        if !*first_token_logged {
            *first_token_logged = true;
            info!(model = %model, elapsed_ms = started.elapsed().as_millis(), "llm: first streamed token");
        }
        let _ = deltas.send(StreamDelta::Content(c)).await;
    }
    if let Some(t) = thinking {
        let _ = deltas.send(StreamDelta::Thinking(t)).await;
    }
}

/// Drain an OpenAI-compatible SSE byte stream to completion, forwarding
/// content/thinking deltas live via `deltas`, and return the accumulated
/// `{choices:[{message}]}` JSON (with `usage`, if seen) once the stream
/// reaches `[DONE]` or ends.
///
/// Extracted from [`LlmClient::chat_completion_with_tools_streaming`] because
/// the SSE line-buffering loop (labeled `break`, nested tool-call
/// accumulation) is a self-contained responsibility distinct from request
/// setup and response parsing.
async fn drain_streaming_deltas(
    mut byte_stream: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
    deltas: &tokio::sync::mpsc::Sender<StreamDelta>,
    model: &str,
    started: std::time::Instant,
) -> Result<Value, LlmMonitorError> {
    use futures::StreamExt as _;

    let mut buffer = String::new();
    let mut acc = StreamingToolAccumulator::default();
    let mut usage: Option<Value> = None;
    let mut finish_reason: Option<String> = None;
    let mut first_token_logged = false;

    'outer: loop {
        while let Some(line) = take_sse_line(&mut buffer) {
            let outcome = process_stream_line(&line, &mut acc, &mut usage, &mut finish_reason);
            let (content_delta, thinking_delta) = match outcome {
                StreamLineOutcome::Skip => continue,
                StreamLineOutcome::Done => break 'outer,
                StreamLineOutcome::Chunk { content, thinking } => (content, thinking),
            };
            forward_stream_deltas(
                content_delta,
                thinking_delta,
                deltas,
                model,
                started,
                &mut first_token_logged,
            )
            .await;
        }
        match byte_stream.next().await {
            None => break,
            Some(Err(e)) => {
                warn!(model = %model, error = %e, "llm: streaming byte error");
                return Err(LlmMonitorError::from(e));
            }
            Some(Ok(bytes)) => buffer.push_str(&String::from_utf8_lossy(&bytes)),
        }
    }

    Ok(acc.into_response_json(usage, finish_reason))
}

/// Result of parsing one SSE line during [`chat_stream_poll`]: nothing to
/// yield yet, the stream's done, or a token to yield.
enum PollLineOutcome {
    Skip,
    Done,
    Token(String),
}

/// Parse one already-dechunked SSE line per `flavor` into a
/// [`PollLineOutcome`], isolating the flavor dispatch and `"__DONE__"`
/// sentinel check from [`chat_stream_poll`]'s read loop.
fn resolve_poll_line(flavor: ApiFlavor, line: &str) -> PollLineOutcome {
    let parsed = match flavor {
        ApiFlavor::OpenAi => parse_sse_line(line),
        ApiFlavor::Anthropic => anthropic::parse_sse_line(line),
    };
    match parsed {
        None => PollLineOutcome::Skip,
        Some(token) if token == "__DONE__" => PollLineOutcome::Done,
        Some(token) => PollLineOutcome::Token(token),
    }
}

async fn chat_stream_poll(
    mut stream: std::pin::Pin<Box<dyn Stream<Item = Result<bytes::Bytes, LlmMonitorError>> + Send>>,
    mut buffer: String,
    flavor: ApiFlavor,
) -> Option<(Result<String, LlmMonitorError>, ChatStreamState)> {
    use futures::StreamExt as _;
    loop {
        if let Some(line) = take_sse_line(&mut buffer) {
            match resolve_poll_line(flavor, &line) {
                PollLineOutcome::Skip => continue,
                PollLineOutcome::Done => return Some((Ok(String::new()), ChatStreamState::Done)),
                PollLineOutcome::Token(token) => {
                    return Some((
                        Ok(token),
                        ChatStreamState::Streaming {
                            stream,
                            buffer,
                            flavor,
                        },
                    ));
                }
            }
        }

        match stream.next().await {
            None => {
                debug!("llm: byte stream ended (no [DONE] sentinel)");
                return None;
            }
            Some(Err(e)) => {
                warn!(error = %e, "llm: byte stream error");
                return Some((Err(e), ChatStreamState::Done));
            }
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

    let usage = json.get("usage").map(|u| TokenUsage {
        prompt_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        completion_tokens: u
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        total_tokens: u.get("total_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
    });

    let finish_reason = json
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .map(String::from);

    Ok(ChatCompletionResponse {
        content,
        tool_calls,
        assistant_message,
        usage,
        finish_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_provider_constructs_client() {
        let provider = ahma_common::config::ResolvedProvider {
            name: "test".into(),
            kind: ahma_common::config::ProviderKind::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            default_model: "claude-3-5-sonnet".into(),
            api_key: Some("secret".into()),
            num_ctx: Some(8192),
        };
        let client = LlmClient::for_provider(&provider);
        assert_eq!(client.flavor(), ApiFlavor::Anthropic);
        assert_eq!(client.base_url(), "https://api.anthropic.com");
    }

    #[test]
    fn retryable_status_classification() {
        use reqwest::StatusCode;
        for s in [429u16, 500, 502, 503, 504] {
            assert!(is_retryable_status(StatusCode::from_u16(s).unwrap()), "{s}");
        }
        for s in [200u16, 400, 401, 403, 404] {
            assert!(
                !is_retryable_status(StatusCode::from_u16(s).unwrap()),
                "{s}"
            );
        }
    }

    #[test]
    fn retry_after_header_parses_integer_and_fractional_seconds() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

        let mut headers = HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);

        headers.insert(RETRY_AFTER, HeaderValue::from_static("17"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(17)));

        headers.insert(RETRY_AFTER, HeaderValue::from_static("0.5"));
        assert_eq!(
            parse_retry_after(&headers),
            Some(Duration::from_millis(500))
        );

        // HTTP-date (or garbage) is ignored rather than mis-parsed.
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Fri, 31 Dec 1999 23:59:59 GMT"),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[tokio::test]
    async fn connect_refused_classifies_as_connect_error() {
        // Bind then drop a listener so the port actively refuses connections.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let err = reqwest::get(format!("http://127.0.0.1:{port}/"))
            .await
            .expect_err("connecting to a closed port must fail");
        let typed = LlmMonitorError::from(err);
        assert!(matches!(typed, LlmMonitorError::Connect(_)), "{typed}");
        assert!(!typed.is_timeout());
    }

    #[tokio::test]
    async fn request_timeout_classifies_as_timeout_error() {
        // A listener that is never accepted from: the kernel completes the TCP
        // handshake into the backlog, the request is written, and no response
        // ever comes — so the client-side deadline is what fires. No server
        // thread is needed (and a joined one would deadlock the current-thread
        // runtime against reqwest's background pool shutdown).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // The deadline provokes the timeout under test; it fires regardless of
        // machine speed because the server never answers.
        let err = Client::new()
            .get(format!("http://127.0.0.1:{port}/"))
            .timeout(Duration::from_millis(200))
            .send()
            .await
            .expect_err("a never-responding server must time the request out");
        drop(listener);
        let typed = LlmMonitorError::from(err);
        assert!(typed.is_timeout(), "{typed}");
        assert!(typed.to_string().contains("timed out"), "{typed}");
    }

    #[test]
    fn loopback_urls_are_recognised() {
        for url in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:1234/v1",
            "http://[::1]:8080/v1",
            "http://LOCALHOST:11434",
        ] {
            assert!(is_loopback_url(url), "{url}");
        }
        for url in [
            "https://api.anthropic.com/v1",
            "http://192.168.1.5:11434/v1",
            "nonsense",
        ] {
            assert!(!is_loopback_url(url), "{url}");
        }
    }

    /// A slow local model must not be re-sent its prompt on a timeout: each
    /// re-send starts its prompt reading over (and queues behind the first).
    #[tokio::test]
    async fn timeouts_are_not_retried_when_retry_timeouts_is_off() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let started = std::time::Instant::now();
        let result = send_with_backoff(
            || {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Client::new()
                    .get(format!("http://127.0.0.1:{port}/"))
                    .timeout(Duration::from_millis(100))
            },
            false,
        )
        .await;
        drop(listener);
        assert!(result.is_err_and(|e| e.is_timeout()));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            started.elapsed() < LLM_RETRY_BASE_DELAY * 4,
            "no backoff sleeps"
        );
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_delay(0), LLM_RETRY_BASE_DELAY);
        assert!(backoff_delay(1) > backoff_delay(0));
        assert!(backoff_delay(2) > backoff_delay(1));
        // Never exceeds the cap, even for large attempt counts.
        assert!(backoff_delay(20) <= LLM_RETRY_MAX_DELAY);
    }

    #[test]
    fn streaming_accumulator_reassembles_split_tool_call_and_reasoning() {
        let mut acc = StreamingToolAccumulator::default();
        let mut content = String::new();
        let mut thinking = String::new();

        // Reasoning arrives first (DeepSeek/Ollama style), then a tool call
        // whose name+arguments are split across several chunks.
        let chunks = [
            json!({"choices":[{"delta":{"reasoning_content":"Let me "}}]}),
            json!({"choices":[{"delta":{"reasoning_content":"check."}}]}),
            json!({"choices":[{"delta":{"content":"On it. "}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read_file"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}),
        ];
        for c in &chunks {
            let (cd, td) = acc.push_chunk(c);
            if let Some(s) = cd {
                content.push_str(&s);
            }
            if let Some(s) = td {
                thinking.push_str(&s);
            }
        }
        assert_eq!(content, "On it. ");
        assert_eq!(thinking, "Let me check.");

        let resp = parse_chat_completion_response(acc.into_response_json(None, None)).unwrap();
        assert_eq!(resp.content, "On it. ");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "call_a");
        assert_eq!(resp.tool_calls[0].name, "read_file");
        assert_eq!(resp.tool_calls[0].arguments, json!({"path": "a.rs"}));
    }

    #[test]
    fn streaming_accumulator_plain_answer_has_no_tool_calls() {
        let mut acc = StreamingToolAccumulator::default();
        acc.push_chunk(&json!({"choices":[{"delta":{"content":"Hello"}}]}));
        acc.push_chunk(&json!({"choices":[{"delta":{"content":" world"}}]}));
        let resp = parse_chat_completion_response(acc.into_response_json(None, None)).unwrap();
        assert_eq!(resp.content, "Hello world");
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn num_ctx_injected_only_for_ollama_endpoints() {
        // Ollama endpoint + configured num_ctx → options.num_ctx is sent.
        let ollama = LlmClient::new("http://localhost:11434/v1", "ornith:35b", None)
            .with_num_ctx(Some(16384));
        assert!(ollama.sends_num_ctx());
        let mut body = json!({"model": "ornith:35b", "messages": []});
        ollama.apply_num_ctx(&mut body);
        assert_eq!(body["options"]["num_ctx"], json!(16384));

        // Hosted OpenAI cloud → never sends num_ctx (would 400 on unknown field).
        let cloud = LlmClient::new("https://api.openai.com/v1", "gpt-4o-mini", None)
            .with_num_ctx(Some(16384));
        assert!(!cloud.sends_num_ctx());
        let mut body = json!({"model": "gpt-4o-mini", "messages": []});
        cloud.apply_num_ctx(&mut body);
        assert!(body.get("options").is_none());

        // Ollama endpoint but no override configured → nothing sent.
        let bare = LlmClient::new("http://localhost:11434/v1", "ornith:35b", None);
        assert!(!bare.sends_num_ctx());
    }

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

    #[test]
    fn parse_chat_completion_response_extracts_usage() {
        let response = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Done."
                }
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            }
        });

        let parsed = parse_chat_completion_response(response).unwrap();
        assert_eq!(parsed.content, "Done.");
        let usage = parsed.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
    }

    #[test]
    fn parse_chat_completion_response_extracts_finish_reason() {
        let response = json!({
            "choices": [{
                "message": { "role": "assistant", "content": "cut off mid" },
                "finish_reason": "length"
            }]
        });
        let parsed = parse_chat_completion_response(response).unwrap();
        assert_eq!(parsed.finish_reason.as_deref(), Some("length"));
        assert!(parsed.is_length_truncated());
    }

    #[test]
    fn parse_chat_completion_response_normal_stop_is_not_truncated() {
        let response = json!({
            "choices": [{
                "message": { "role": "assistant", "content": "Done." },
                "finish_reason": "stop"
            }]
        });
        let parsed = parse_chat_completion_response(response).unwrap();
        assert_eq!(parsed.finish_reason.as_deref(), Some("stop"));
        assert!(!parsed.is_length_truncated());
    }

    #[test]
    fn parse_chat_completion_response_missing_finish_reason_is_not_truncated() {
        let response = json!({
            "choices": [{ "message": { "role": "assistant", "content": "Done." } }]
        });
        let parsed = parse_chat_completion_response(response).unwrap();
        assert_eq!(parsed.finish_reason, None);
        assert!(!parsed.is_length_truncated());
    }

    #[tokio::test]
    async fn drain_streaming_deltas_captures_finish_reason_from_sse() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
                   data: [DONE]\n\n";
        let stream = futures::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(sse))]);
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let json = drain_streaming_deltas(stream, &tx, "test-model", std::time::Instant::now())
            .await
            .unwrap();
        let resp = parse_chat_completion_response(json).unwrap();
        assert_eq!(resp.content, "hi");
        assert!(resp.is_length_truncated());
    }

    #[tokio::test]
    async fn drain_streaming_deltas_normal_stop_is_not_truncated() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                   data: [DONE]\n\n";
        let stream = futures::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(sse))]);
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let json = drain_streaming_deltas(stream, &tx, "test-model", std::time::Instant::now())
            .await
            .unwrap();
        let resp = parse_chat_completion_response(json).unwrap();
        assert!(!resp.is_length_truncated());
    }
}
