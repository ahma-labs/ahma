# ahma_llm_monitor Crate Specification

* **Status**: Approved
* **Date**: 2026-07-27

## 1. User Story / Problem Statement

*As the live-log monitor and the TUI chat interface, I want an OpenAI-compatible LLM client that can analyse log chunks against a plain-English detection prompt and stream chat responses, so that issues are surfaced as they appear without the user reading every line — and so that a locally hosted model can be used without cloud egress.*

## 2. Acceptance Criteria

- **OpenAI-Compatible Client**: `LlmClient` issues chat-completion requests against any OpenAI-compatible `base_url`, selected per tool definition or settings.
- **Anthropic Flavor**: `ApiFlavor` distinguishes Anthropic's message API from the OpenAI shape, so both providers are reachable through one client type.
- **Log Analysis**: Accepts a log chunk plus a natural-language `detection_prompt` and returns whether the chunk warrants an alert, backing the `livelog` tool type and `--log-monitor`.
- **Streaming Chat**: `chat_stream` yields incremental tokens for the TUI chat interface rather than blocking to completion.
- **Tool Calls**: `ChatToolCall` carries provider tool-invocation requests back to the caller.
- **Local Provider Discovery**: `discover_local_providers` probes well-known local endpoints (e.g. Ollama, LM Studio) and reports which are reachable, so a local model can be chosen without manual configuration.

## 3. Non-Functional Requirements

- **Rate Limiting**: Alert emission is rate-limited by the caller (`--monitor-rate-limit`, default 60s) so a noisy log cannot flood the agent with notifications.
- **Failure Isolation**: An unreachable or erroring LLM provider MUST degrade to "no alert" and log the failure. It MUST NOT fail the monitored operation, which is unrelated to the analysis.
- **No Cloud Egress By Default**: Discovery prefers local providers; a remote provider is used only when explicitly configured.

## 4. Out of Scope

- Log file tailing, chunking and rotation (the live-log pipeline in `ahma_mcp::livelog`).
- Model hosting or inference — this crate is a client only.

