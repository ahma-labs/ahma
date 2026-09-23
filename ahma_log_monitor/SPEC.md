# ahma_log_monitor Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0
* **Depends on**: no workspace crate
* **Used by**: `ahma_mcp` (the `monitor_level` option of `run_terminal_command` and MTDF tools;
  redaction on every streamed line), re-exported as `ahma_mcp::log_monitor`

## 1. User Story / Problem Statement

*As an agent running a long command, I want to be told when its output shows an error,
with enough context to act, without reading the whole stream myself.*

## 2. Acceptance Criteria

- `LogLevelDetector` classifies a line's level (error, warn, info, debug, trace) by regex.
- `LogRingBuffer` keeps the last `LOG_CONTEXT_LINES` (100) stdout and stderr lines.
- `LogMonitor` fires on a line at or above `monitor_level` on the chosen stream(s), returns
  a `LogSnapshot` of the recent context, and rate-limits itself (60 s by default).
- `redact_sensitive_line` / `redact_sensitive_text` remove authorization headers, tokens and
  similar secrets. Every streamed output line passes through them before it is stored,
  spilled or sent anywhere.

## 3. Non-Functional Requirements

- Pure and synchronous: no I/O. Delivering an alert (an `Alert` event on the operation,
  pushed as a progress notification) is the caller's job.

## 4. Out of Scope

- LLM-based analysis — that is the `livelog` tool type (`ahma_llm_monitor`).
