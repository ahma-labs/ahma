//! MCP protocol-version negotiation shared by every in-house HTTP client.
//!
//! The 2025-06-18 Streamable HTTP revision requires the client to send
//! `MCP-Protocol-Version: <negotiated-version>` on every HTTP request after
//! `initialize`, where *negotiated* means the version the **server returned**
//! in its `initialize` result. ahma has several hand-rolled HTTP clients (the
//! stdio proxy, the TUI source, the agent, the external-tool client); this
//! module is the one place they all get the header name, the request/default
//! versions, and the extraction of the negotiated value — so the clients
//! cannot drift apart on protocol currency.

use serde_json::Value;

/// Canonical (lowercase) name of the protocol-version header.
pub const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

/// Modern stateless MCP protocol revision (SEP-2575).
pub const MCP_PROTOCOL_VERSION_2026_07_28: &str = "2026-07-28";

/// Stateful MCP protocol revision with tools/call notifications.
pub const MCP_PROTOCOL_VERSION_2025_11_25: &str = "2025-11-25";

/// The protocol revision ahma's own clients request at `initialize`.
pub const REQUESTED_PROTOCOL_VERSION: &str = "2025-06-18";

/// The version to assume when an `initialize` result carries no
/// `protocolVersion` — the same default the spec assigns to a request that
/// carries no header at all.
pub const DEFAULT_NEGOTIATED_PROTOCOL_VERSION: &str = "2025-03-26";

/// Extract the negotiated protocol version from an `initialize` response.
///
/// Accepts either the full JSON-RPC envelope (`{"result": {"protocolVersion":
/// …}}`) or a bare result object, falling back to
/// [`DEFAULT_NEGOTIATED_PROTOCOL_VERSION`].
pub fn negotiated_protocol_version(init_response: &Value) -> String {
    init_response
        .get("result")
        .unwrap_or(init_response)
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_NEGOTIATED_PROTOCOL_VERSION)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_from_envelope_and_bare_result() {
        let envelope = json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25"}});
        assert_eq!(negotiated_protocol_version(&envelope), "2025-11-25");
        let bare = json!({"protocolVersion":"2024-11-05"});
        assert_eq!(negotiated_protocol_version(&bare), "2024-11-05");
    }

    #[test]
    fn falls_back_to_spec_default() {
        assert_eq!(
            negotiated_protocol_version(&json!({"result": {}})),
            DEFAULT_NEGOTIATED_PROTOCOL_VERSION
        );
        assert_eq!(
            negotiated_protocol_version(&json!(null)),
            DEFAULT_NEGOTIATED_PROTOCOL_VERSION
        );
    }
}
