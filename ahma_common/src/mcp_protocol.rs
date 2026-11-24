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

/// The revisions ahma advertises in a `server/discover` answer, newest first.
pub const ADVERTISED_PROTOCOL_VERSIONS: &[&str] = &[
    MCP_PROTOCOL_VERSION_2026_07_28,
    MCP_PROTOCOL_VERSION_2025_11_25,
];

/// The revisions an HTTP transport accepts in the `MCP-Protocol-Version`
/// header: every advertised one first, then the older revisions a client may
/// still be pinned to. Kept beside [`ADVERTISED_PROTOCOL_VERSIONS`] so the two
/// cannot disagree: a bridge once advertised a revision it then refused.
pub const ACCEPTED_PROTOCOL_VERSIONS: &[&str] = &[
    MCP_PROTOCOL_VERSION_2026_07_28,
    MCP_PROTOCOL_VERSION_2025_11_25,
    REQUESTED_PROTOCOL_VERSION,
    DEFAULT_NEGOTIATED_PROTOCOL_VERSION,
    "2024-11-05",
];

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

/// How long a client may cache a `DiscoverResult`, in milliseconds: none.
/// The hub behind a session can be replaced by a newer build at any time
/// (R-HUB.5), and with it the capabilities and instructions.
pub const DISCOVER_TTL_MS: u64 = 0;

/// The 2026-07-28 `DiscoverResult` (SPEC R-HUB.4.4), built in one place for
/// every surface that answers `server/discover`, so none of them drifts from
/// the required fields (`resultType`, `ttlMs`, `cacheScope`). `private`: the
/// instructions and tool set are this user's.
pub fn discover_result(capabilities: Value, instructions: &str, server_info: Value) -> Value {
    serde_json::json!({
        "resultType": "complete",
        "supportedVersions": ADVERTISED_PROTOCOL_VERSIONS,
        "capabilities": capabilities,
        "instructions": instructions,
        "ttlMs": DISCOVER_TTL_MS,
        "cacheScope": "private",
        "serverInfo": server_info.clone(),
        "_meta": { "io.modelcontextprotocol/serverInfo": server_info },
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_discover_result_carries_every_required_field() {
        let result = discover_result(json!({"tools": {}}), "i", json!({"name": "ahma"}));
        for field in [
            "resultType",
            "supportedVersions",
            "capabilities",
            "ttlMs",
            "cacheScope",
        ] {
            assert!(result.get(field).is_some(), "{field}: {result:#?}");
        }
        assert_eq!(result["resultType"], "complete");
        assert_eq!(
            result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "ahma"
        );
    }

    #[test]
    fn every_advertised_revision_is_accepted() {
        for v in ADVERTISED_PROTOCOL_VERSIONS {
            assert!(ACCEPTED_PROTOCOL_VERSIONS.contains(v), "{v}");
        }
    }

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
