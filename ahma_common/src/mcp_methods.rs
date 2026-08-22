//! Shared MCP JSON-RPC method name constants.
//!
//! These methods are spoken by multiple surfaces (the MCP server, the HTTP
//! bridge, the core agent loop, the TUI's MCP source). Naming them once here
//! keeps the wire strings from drifting between the crates that send them and
//! the crates that match on them. Session-health notification methods live in
//! [`crate::session_event`] (`SESSION_EVENT_METHOD` / `MESSAGE_METHOD`).

/// Client → server notification completing the MCP initialize handshake.
pub const INITIALIZED_METHOD: &str = "notifications/initialized";

/// Server → client request asking for the client's workspace roots.
pub const ROOTS_LIST_METHOD: &str = "roots/list";

/// Server → client notification: the sandbox is configured and locked for the
/// session.
pub const SANDBOX_CONFIGURED_METHOD: &str = "notifications/sandbox/configured";

/// Server → client notification: sandbox configuration failed.
pub const SANDBOX_FAILED_METHOD: &str = "notifications/sandbox/failed";
