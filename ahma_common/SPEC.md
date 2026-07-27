# ahma_common Crate Specification

* **Status**: Approved
* **Date**: 2026-07-27

## 1. User Story / Problem Statement

*As a crate anywhere in the ahma workspace, I want one authoritative source for cross-platform behaviour, configuration, timeout scaling, and the workspace event bus, so that the same contract is not reimplemented — and silently diverged — in each crate that needs it.*

## 2. Acceptance Criteria

- **Configuration**: Workspace-wide configuration types and defaults (mutex groups, tool paths), loaded from `~/.ahma/settings.toml` and CLI flags. `AHMA_*` configuration environment variables are retired and MUST be ignored (R-CFG1.2); the hook variables `AHMA_HOOKS`, `AHMA_DISABLE_HOOKS` and `AHMA_PREFER_OWN_SANDBOX` remain live.
- **Platform-Aware Timeouts**: `timeouts::{TestTimeouts, TimeoutCategory}` provides semantic timeout categories (`ProcessSpawn`, `Handshake`, `ToolCall`, `SandboxReady`, `HttpRequest`, `SseStream`, `HealthCheck`, `Cleanup`, `Quick`) with per-platform multipliers, plus `scale_secs()` and `poll_interval()`. Callers MUST NOT hardcode durations.
- **Event Dispatch**: A broadcast event bus (`event_dispatcher`) for cross-component notification without direct coupling.
- **Sandbox State**: Shared sandbox lifecycle state (`sandbox_state`), scope grants (`scope_grant`), and workspace scope derivation (`workspace_scope`) consumed by every enforcement surface.
- **Daemon Hub**: Shared state and lifecycle for the HTTP bridge daemon, including process-group spawning so a terminated hub does not orphan its children.
- **Transport Support**: `local_tls` self-signed certificate generation, `keepalive` for long-lived HTTP/SSE connections, `file_uri` construction and parsing, and the transport-agnostic `peer_factory` abstraction.
- **Consent and Approval**: `hook_consent` and `net_approval` record user consent decisions; `elicitation` carries MCP elicitation request/response types.
- **Observability**: OpenTelemetry tracing initialisation helpers.

## 3. Non-Functional Requirements

- **Cross-Platform Parity**: Every primitive behaves identically on Linux, macOS and Windows, or documents the divergence explicitly. Windows CI runners are 3–5× slower, which the timeout multipliers exist to absorb.
- **No Upward Dependencies**: MUST NOT depend on `ahma_mcp`, `ahma_core`, or any product-surface crate; it is the foundation layer.
- **Stability**: Types here are consumed workspace-wide, so breaking changes require updating every dependent crate in the same change.

## 4. Out of Scope

- MCP protocol handling (`ahma_mcp`), HTTP transport hosting (`ahma_http_bridge`), and sandbox enforcement itself (`ahma_mcp::sandbox`) — this crate carries shared *state and contracts*, not the enforcement mechanisms.
