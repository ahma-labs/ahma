# ahma_http_bridge Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As a web-based client or remote integration, I want to communicate with Ahma over HTTP and SSE so that I can invoke MCP tools and receive live notifications using standard web protocols.*

## 2. Acceptance Criteria

- **HTTP/SSE Transport**: Exposes MCP protocol over POST `/mcp` (JSON-RPC) and GET `/mcp` (Server-Sent Events stream for notifications).
- **Streamable HTTP**: Supports multiplexed response streaming and Event ID ordering on POST requests with `Accept: text/event-stream`.
- **Reconnection Resilience**: Implements an event history buffer to replay missed events using the `Last-Event-Id` header.
- **Session Isolation**: Spawns a dedicated, isolated `ahma serve stdio` subprocess per `Mcp-Session-Id`.
- **Sandbox Derivation**: Derives the sandbox scope from the client's first `roots/list` response and locks it. Rejects any scope widening attempts with HTTP 403.
- **Auto-Restart**: Automatically restarts the stdio subprocess if it crashes.
- **Health check**: Exposes a `/health` endpoint to monitor server status.

## 3. Non-Functional Requirements

- **Protocol Parity**: Must fully implement all JSON-RPC methods defined by the Model Context Protocol.
- **HTTP/3 Preference**: Prefers HTTP/3 (QUIC) transport when the client supports it, falling back to HTTP/2 and HTTP/1.1 transparently.

## 4. Out of Scope

- Implementing the command execution or sandboxing logic directly (delegated to the `ahma serve stdio` subprocess).
