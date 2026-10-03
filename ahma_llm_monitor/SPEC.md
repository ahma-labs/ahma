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
  - Arguments that are not valid JSON arrive as `null` beside `arguments_raw`, never as `{}`. The chat agent repairs trailing commas and otherwise answers the call with an error asking the model to resend, so no tool runs with arguments the model did not send.
  - History trimming and compaction never leave a tool result without the assistant call it answers.
  - Streamed tool-call fragments without an `index` (optional in the OpenAI format) become one call per tool: a fragment with a new `id`, or with a new name after complete arguments, starts the next call.
- **Tool Names on the Wire**: providers accept a function name only as `^[a-zA-Z0-9_-]{1,64}$`, while ahma names an external MCP tool `server::tool`. Each request renames, for that request only, every tool name a provider would reject — in the tool definitions and in earlier assistant tool calls — and maps the provider's tool calls back before returning them (`tool_names`); a valid name is sent unchanged.
- **Tools Rejected Means Tools Unsupported**: `is_tools_rejected` is true only when the provider's message names tools or function calling *and* says they are unsupported or unrecognised; never for a bare 400, a throttle, an auth failure or a context overflow. The chat agent falls back to plain chat only before any tool has run in the turn's conversation, since the fallback rebuilds it from the original messages.
- **Request Shape Fits the Endpoint**: every OpenAI-flavor body sets its output limit and temperature through one helper (`apply_sampling`), because a rejected parameter is a 400 that costs the user the turn.
  - OpenAI's own API and Azure OpenAI get `max_completion_tokens`, which reasoning models require and the rest accept; every other compatible server gets `max_tokens`.
  - Reasoning models (o1, o3, o4, gpt-5 families) are sent no `temperature`, since they reject any value but the default.
- **Local Model Residency**: `loaded_local_models` / `is_model_resident` report which models a local server has loaded, so the TUI can show a "loading model" phase instead of an unexplained wait; `num_ctx` is sent only to endpoints that accept it (Ollama).
- **Ollama's own API**: an Ollama endpoint is spoken to through `/api/chat` (`ApiFlavor::Ollama`, module `ollama`), never its OpenAI-compatible `/v1`, which ignores `options` (checked against Ollama 0.35: `num_ctx` 5120 and 12288 both loaded at the server default; the same 12288 over `/api/chat` loaded at 12288, and a later plain `/v1` request reloads at the default). It is chosen without configuration: by URL (`:11434` or an `ollama` host), by a provider `kind = "openai"` at such a URL (OpenAI-compatible describes the server, not which of its APIs to prefer), and by the agent when `server_context` finds Ollama on another port. Requests and responses are translated at the edge as for Anthropic: tool-call arguments as objects, tool results by `tool_name`, `thinking` as reasoning, and the NDJSON stream rewritten into OpenAI chunks so parsing is shared. `num_ctx`, `num_predict` and temperature go in `options`; `num_ctx` is sent only when configured, so an unset size stays the server's choice. An in-stream `error` line ends the stream.
- **Context window**: `LlmClient::server_context` asks a server on this machine, through its own API, which program it is (`ModelServer`) and how much context it gives the model: Ollama `/api/ps` (the loaded size; none before the model loads), LM Studio `/api/v0/models/<model>` (`loaded_context_length`), llama.cpp `/props` (`n_ctx`), vLLM `/v1/models` (`max_model_len`), LiteLLM `/model/info` (`max_input_tokens`). All are asked at once with a short timeout; remote endpoints and the Anthropic flavor are never asked. The chat agent uses it only when no size is configured: a configured size always wins, and the size is never guessed by hostname. A model the server runs here (anything but LiteLLM) gets the small-model budgets and, while no size is stated, `DEFAULT_OLLAMA_NUM_CTX`; the agent asks again each turn until one is, and reports it (`Status` phase `context`) for the TUI meter. A guessed size is never sent to a server, since that would override the server's own choice.
- **Loopback Detection**: `is_loopback_url` is the one test for "this provider runs on this machine".
- **Local Provider Discovery**: `discover_local_providers` probes well-known local endpoints (e.g. Ollama, LM Studio) and reports which are reachable, so a local model can be chosen without manual configuration.
- **Typed Errors**: Failures surface as `LlmMonitorError` variants — `Api` (with an `ApiErrorKind` classification: rate-limited, auth, context-length exceeded, invalid request, server; plus the provider-reported message and any `Retry-After`), `Connect`, `Timeout`, `Http`, `Parse`. Classification of a provider error response happens once, in this crate; consumers MUST branch on the typed variants/kinds (e.g. `is_tools_rejected`, `is_timeout`), never by substring-matching rendered error text. The provider's raw message stays available on the error for display and logging.
- **Retry and failure wording**: every request — completions, streamed or not, and log-chunk analysis — is sent through `ahma_common::http_retry::send_with_retry` and follows root SPEC R-HTTP (R-HTTP.1–R-HTTP.3). Specific to this crate:
  - `LlmMonitorError::into_service_error(base_url)` renders the R-HTTP.3 summary, naming the endpoint via `llm_service_name` ("your local model server at localhost:11434" or "the model provider at api.example.com") with a hint by kind; `failure()` gives its retry classification.
  - A stream is retried only until it opens — once tokens flow, a re-send would duplicate text already shown.
  - A non-streaming completion is never retried on a timeout, local or remote: the provider most likely received, worked on and billed it, and the same work usually times out again.
  - A non-streaming completion's timeout is ten minutes for a remote API (the window provider SDKs allow a long tool turn) and the local read window for a model on this machine.

## 3. Non-Functional Requirements

- **Failure Isolation**: An unreachable or erroring LLM provider MUST degrade to "no alert" and log the failure. It MUST NOT fail the monitored operation, which is unrelated to the analysis.
- **No Cloud Egress By Default**: Discovery prefers local providers; a remote provider is used only when explicitly configured.

## 4. Out of Scope

- Log file tailing, chunking and rotation (the live-log pipeline in `ahma_mcp::livelog`).
- Model hosting or inference — this crate is a client only.
