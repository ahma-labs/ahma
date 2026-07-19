//! Session-health event envelopes (issue #485, `docs/session-health-notifications.md`).
//!
//! One internal event type, fanned out over two wire forms:
//!
//! 1. **`notifications/ahma/session_event`** — the canonical structured
//!    notification for ahma-aware peers (the ahma TUI, the stdio proxy, future
//!    ahma-aware clients). Unknown JSON-RPC notification methods are ignored by
//!    compliant clients, so this degrades safely everywhere else.
//! 2. **`notifications/message`** — the standard MCP logging notification,
//!    mirroring the same payload in `data` so foreign clients (Claude Code,
//!    Cursor, …) surface the disclosure with zero ahma-specific code.
//!
//! Events are **information only**: they never demand a response and never gate
//! server progress. Emission failure must never fail the operation that
//! triggered the event. This module builds the envelopes; each emitter (the MCP
//! service, the stdio proxy) owns its own transport and monotonic `seq`.

use serde::{Deserialize, Serialize};

/// The JSON-RPC method of the canonical structured event notification.
pub const SESSION_EVENT_METHOD: &str = "notifications/ahma/session_event";

/// The JSON-RPC method of the standard MCP logging mirror.
pub const MESSAGE_METHOD: &str = "notifications/message";

/// What happened to the session. `detail` schemas are documented in
/// `docs/session-health-notifications.md` §3.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEventKind {
    /// The stdio proxy rebuilt the bridge connection and replayed the cached
    /// handshake; the session resumed transparently (#479).
    Reconnected,
    /// Reconnection exhausted its attempts; the proxy is about to exit and the
    /// pipe will go dead. Emitted so the death is at least explained.
    ReconnectFailed,
    /// A sandbox scope grant is awaiting a human decision at some surface.
    GrantPending,
    /// A previously pending grant was decided (granted / declined).
    GrantDecided,
    /// General session-health telemetry.
    Health,
}

impl SessionEventKind {
    /// The wire value of `kind` (matches the serde snake_case rename).
    pub fn as_str(self) -> &'static str {
        match self {
            SessionEventKind::Reconnected => "reconnected",
            SessionEventKind::ReconnectFailed => "reconnect_failed",
            SessionEventKind::GrantPending => "grant_pending",
            SessionEventKind::GrantDecided => "grant_decided",
            SessionEventKind::Health => "health",
        }
    }

    /// The MCP logging level used for the `notifications/message` mirror.
    /// `reconnect_failed` is terminal (the pipe dies next) → `error`; pending
    /// grants and transparent reconnects deserve attention → `warning`; plain
    /// health telemetry is `info`.
    pub fn mirror_level(self) -> &'static str {
        match self {
            SessionEventKind::ReconnectFailed => "error",
            SessionEventKind::Reconnected
            | SessionEventKind::GrantPending
            | SessionEventKind::GrantDecided => "warning",
            SessionEventKind::Health => "info",
        }
    }
}

/// The `params` of a `notifications/ahma/session_event`, also embedded as the
/// `data` of its `notifications/message` mirror.
pub fn event_params(
    kind: SessionEventKind,
    seq: u64,
    timestamp_ms: u64,
    detail: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "kind": kind.as_str(),
        "timestamp": timestamp_ms,
        "seq": seq,
        "detail": detail,
    })
}

/// The complete `notifications/ahma/session_event` JSON-RPC notification.
pub fn event_notification(
    kind: SessionEventKind,
    seq: u64,
    timestamp_ms: u64,
    detail: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": SESSION_EVENT_METHOD,
        "params": event_params(kind, seq, timestamp_ms, detail),
    })
}

/// The complete `notifications/message` mirror JSON-RPC notification: the same
/// event as a standard MCP logging notification so foreign clients surface it
/// without knowing anything about ahma.
pub fn message_mirror_notification(
    kind: SessionEventKind,
    seq: u64,
    timestamp_ms: u64,
    detail: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": MESSAGE_METHOD,
        "params": {
            "level": kind.mirror_level(),
            "logger": "ahma.session",
            "data": event_params(kind, seq, timestamp_ms, detail),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_wire_values_match_serde_rename() {
        for kind in [
            SessionEventKind::Reconnected,
            SessionEventKind::ReconnectFailed,
            SessionEventKind::GrantPending,
            SessionEventKind::GrantDecided,
            SessionEventKind::Health,
        ] {
            let via_serde = serde_json::to_value(kind).unwrap();
            assert_eq!(via_serde, serde_json::json!(kind.as_str()));
        }
    }

    #[test]
    fn event_notification_has_canonical_envelope() {
        let n = event_notification(
            SessionEventKind::Reconnected,
            3,
            1_789_000_000_000,
            serde_json::json!({"cause": "transport_failure"}),
        );
        assert_eq!(n["jsonrpc"], "2.0");
        assert_eq!(n["method"], SESSION_EVENT_METHOD);
        assert_eq!(n["params"]["kind"], "reconnected");
        assert_eq!(n["params"]["seq"], 3);
        assert_eq!(n["params"]["timestamp"], 1_789_000_000_000u64);
        assert_eq!(n["params"]["detail"]["cause"], "transport_failure");
        assert!(n.get("id").is_none(), "a notification must not carry an id");
    }

    #[test]
    fn mirror_levels_reflect_severity() {
        assert_eq!(SessionEventKind::ReconnectFailed.mirror_level(), "error");
        assert_eq!(SessionEventKind::Reconnected.mirror_level(), "warning");
        assert_eq!(SessionEventKind::GrantPending.mirror_level(), "warning");
        assert_eq!(SessionEventKind::GrantDecided.mirror_level(), "warning");
        assert_eq!(SessionEventKind::Health.mirror_level(), "info");
    }

    #[test]
    fn mirror_embeds_the_same_params_as_data() {
        let detail = serde_json::json!({"grant_id": "abc", "path": "/x"});
        let mirror =
            message_mirror_notification(SessionEventKind::GrantPending, 7, 42, detail.clone());
        assert_eq!(mirror["method"], MESSAGE_METHOD);
        assert_eq!(mirror["params"]["level"], "warning");
        assert_eq!(mirror["params"]["logger"], "ahma.session");
        assert_eq!(
            mirror["params"]["data"],
            event_params(SessionEventKind::GrantPending, 7, 42, detail)
        );
    }
}
