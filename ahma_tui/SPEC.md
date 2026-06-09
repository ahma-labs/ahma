# ahma_tui Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As an interactive user, I want a unified terminal UI to monitor running background operations, chat with the local/remote LLM, and review/resolve security approval gates easily.*

## 2. Acceptance Criteria

- **Terminal Dashboard**: Implements a full-screen ratatui terminal user interface with mouse support and Unicode detection.
- **Operations Monitor**: Displays all active, pending, and completed background operations with detailed status views.
- **Interactive Controls**: Allows pin/unpin and cancellation of running background operations via interactive hotkeys or mouse clicks.
- **Chat Interface**: Connects to the local LLM and streams chat responses, incorporating animated thinking indicators.
- **Approval Gate Prompts**: Intercepts and renders prompts for security checkpoints (e.g. renewal gates, elevation requests).
- **Log Monitor integration**: Displays real-time tailing of log files and LLM-powered alert notifications.

## 3. Non-Functional Requirements

- **Resource Usage**: Must remain idle (low CPU) when no active rendering or updates are happening.
- **Terminal Recovery**: Must reliably restore terminal raw mode and clear alternate screens on exit, even upon panic.

## 4. Out of Scope

- Implementing the LLM or MCP protocol directly (delegated to the HTTP bridge or stdio endpoints).
