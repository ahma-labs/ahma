# ahma_http_mcp_client Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As the Ahma server, I want to connect to remote external HTTP/SSE MCP servers with authentication so that I can dispatch subtasks to third-party endpoints securely.*

## 2. Acceptance Criteria

- **HTTP Client Transport**: Implements outbound HTTP POST requests with support for bearer token authentication.
- **SSE Stream Listener**: Listens to server-sent events from the remote MCP server in a background task.
- **OAuth 2.0 + PKCE**: Implements secure PKCE auth flow for remote HTTP MCP servers requiring user authorization.
- **Token Storage**: Securely persists tokens locally.
- **Token Refresh**: (Planned) Automatically refreshes expired tokens using saved refresh tokens.

## 3. Non-Functional Requirements

- **Transport Safety**: Outbound calls must respect HTTP/3 client preference where available.
- **Robustness**: Handles network disconnections and connection drops gracefully.

## 4. Out of Scope

- Hosting an MCP server endpoint (handled by `ahma_mcp` / `ahma_http_bridge`).
