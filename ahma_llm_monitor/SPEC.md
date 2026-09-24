# ahma_llm_monitor Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0
* **Depends on**: `ahma_common`
* **Used by**: `ahma_mcp` (livelog, log monitor), `ahma_core` (chat agent), `ahma_tui`

## 1. User Story / Problem Statement

*As the live-log monitor and the TUI chat interface, I want an OpenAI-compatible LLM client that can analyse log chunks against a plain-English detection prompt and stream chat responses, so that issues are surfaced as they appear without the user reading every line — and so that a locally hosted model can be used without cloud egress.*

## 2. Acceptance Criteria

- **OpenAI-Compatible Client**: `LlmClient` issues chat-completion requests against any OpenAI-compatible `base_url`, selected per tool definition or settings.
- **Anthropic Flavor**: `ApiFlavor` distinguishes Anthropic's message API from the OpenAI shape, so both providers are reachable through one client type.
- **Log Analysis**: Accepts a log chunk plus a natural-language `detection_prompt` and returns whether the chunk warrants an alert, backing the `livelog` tool type and `--log-monitor`.
- **Streaming Chat**: `chat_stream` yields incremental tokens for the TUI chat interface rather than blocking to completion.
- **Tool Calls**: `ChatToolCall` carries provider tool-invocation requests back to the caller.
- **Local Model Residency**: `loaded_local_models` / `is_model_resident` report which models a local server has loaded, so the TUI can show a "loading model" phase instead of an unexplained wait; `num_ctx` is sent only to endpoints that accept it (Ollama).
- **Loopback Detection**: `is_loopback_url` is the one test for "this provider runs on this machine".
- **Local Provider Discovery**: `discover_local_providers` probes well-known local endpoints (e.g. Ollama, LM Studio) and reports which are reachable, so a local model can be chosen without manual configuration.
- **Typed Errors**: Failures surface as `LlmMonitorError` variants — `Api` (with an `ApiErrorKind` classification: rate-limited, auth, context-length exceeded, invalid request, server; plus the provider-reported message and any `Retry-After`), `Connect`, `Timeout`, `Http`, `Parse`. Classification of a provider error response happens once, in this crate; consumers MUST branch on the typed variants/kinds (e.g. `is_tools_rejected`, `is_timeout`), never by substring-matching rendered error text. The provider's raw message stays available on the error for display and logging.
- **Retry and failure wording (root SPEC R-HTTP)**: every request — completions, streamed or not, and log-chunk analysis — is sent through `ahma_common::http_retry::send_with_retry`: transient failures are retried with backoff, `Retry-After` is honoured, and a model on this machine gets no timeout retries. `LlmMonitorError::into_service_error(base_url)` renders a failure summary-first, naming the endpoint in plain words (`llm_service_name`: "your local model server at localhost:11434" or "the model provider at api.example.com") with a hint by kind; `failure()` gives its retry classification. A stream is retried only until it opens — once tokens flow, a re-send would duplicate text already shown.

## 3. Non-Functional Requirements

- **Failure Isolation**: An unreachable or erroring LLM provider MUST degrade to "no alert" and log the failure. It MUST NOT fail the monitored operation, which is unrelated to the analysis.
- **No Cloud Egress By Default**: Discovery prefers local providers; a remote provider is used only when explicitly configured.

## 4. Out of Scope

- Log file tailing, chunking and rotation (the live-log pipeline in `ahma_mcp::livelog`).
- Model hosting or inference — this crate is a client only.

