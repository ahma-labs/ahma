//! Shared MCP JSON-RPC method name constants and typed notification params.
//!
//! These methods are spoken by multiple surfaces (the MCP server, the HTTP
//! bridge, the core agent loop, the TUI's MCP source). Naming them once here
//! keeps the wire strings from drifting between the crates that send them and
//! the crates that match on them. Session-health notification methods live in
//! [`crate::session_event`] (`SESSION_EVENT_METHOD` / `MESSAGE_METHOD`).
//!
//! The `*Params` structs below are the single source of truth for each ahma
//! custom notification payload. Emitters serialize them; parsers deserialize
//! them **leniently** — a malformed or missing field falls back to its default
//! instead of erroring, because a lifecycle notification must never be dropped
//! over a payload quirk (the state transition it announces already happened).

use serde::{Deserialize, Deserializer, Serialize};

/// Client → server notification completing the MCP initialize handshake.
pub const INITIALIZED_METHOD: &str = "notifications/initialized";

/// Server → client request asking for the client's workspace roots.
pub const ROOTS_LIST_METHOD: &str = "roots/list";

// ── Active-sandbox tokens (SPEC R5.4) ────────────────────────────────────────
//
// Emitted by `ahma_mcp::sandbox::display` on the `sandbox/configured`
// notification and matched by the TUI to decide who is actually enforcing.
// Named here because that producer and that consumer are in different crates.

/// ahma's own kernel sandbox is the sole authority.
pub const ACTIVE_SANDBOX_AHMA: &str = "ahma";
/// ahma is enforcing, nested inside a host sandbox that is also enforcing.
pub const ACTIVE_SANDBOX_NESTED_IN_HOST: &str = "ahma_nested_in_host";
/// ahma deferred to the host's sandbox and is NOT enforcing.
pub const ACTIVE_SANDBOX_DEFERRED_TO_HOST: &str = "deferred_to_host";
/// No kernel confinement is in effect.
pub const ACTIVE_SANDBOX_DISABLED: &str = "disabled";

/// Client → server request opening the MCP handshake.
pub const INITIALIZE_METHOD: &str = "initialize";

/// Modern MCP (2026-07-28 / SEP-2575) stateless discovery probe.
pub const SERVER_DISCOVER_METHOD: &str = "server/discover";

/// Modern MCP (2026-07-28 / SEP-2575) subscriptions listen method.
pub const SUBSCRIPTIONS_LISTEN_METHOD: &str = "subscriptions/listen";

// The server instructions are one text with a mode-specific middle; the
// shared parts are macros so both variants are compile-time constants that
// cannot drift apart.
macro_rules! instructions_head {
    () => {
        "\
Ahma exposes shell, build, test, and log-monitoring tools that run inside a \
kernel-enforced workspace sandbox (Landlock on Linux, Seatbelt on macOS, \
Job Objects on Windows). Prefer `run_terminal_command` over the native terminal when: \
(1) the command writes to disk — the sandbox guarantees the write stays inside the workspace; "
    };
}

macro_rules! instructions_tail {
    () => {
        "\
A soft `await` timeout is not completion — before declaring a task done, `status` \
or `await` every operation_id you started and confirm each reached a terminal state, \
not \"still running\". \
Results include a bounded stdout/stderr window plus an `output_file` path holding the \
COMPLETE output of the operation; when the inline output is marked truncated, read or \
grep that file instead of re-running the command. \
For reading, searching, and editing files (read, grep, glob, edit) keep using the \
IDE's native file tools — that is what they are for; ahma withholds its own \
read_file/write_file/replace_in_file/list_dir/file_search/grep_search from clients \
that already have native equivalents."
    };
}

/// Canonical server instructions for an **async-mode** server
/// (`tools.execution_mode = "async"`), and for stateless discovery, which
/// cannot know a session's mode. See [`server_instructions`].
pub const SERVER_INSTRUCTIONS: &str = concat!(
    instructions_head!(),
    "\
(2) the command is long-running — `run_terminal_command` returns an operation_id immediately \
and you can `status`, `await`, or `cancel` it without blocking; \
(3) the command's output should be watched for errors — set `monitor_level` and ahma \
streams alerts when matching lines appear; \
(4) multiple commands should run concurrently — each call gets its own operation_id. \
Workflow: start operations, do other useful work, then `await` the ids you need — \
completion is also pushed via notifications, so avoid polling `status` in a loop. \
Push notifications only arrive over a live, actively-listening connection — if you \
might stop generating before an operation finishes (ending your turn, handing off, \
or exiting), call `await` and let it block rather than counting on a notification to \
resume you; a push sent while you are not listening is not queued or replayed. ",
    instructions_tail!()
);

/// Server instructions for a **sync-mode** server (`tools.execution_mode =
/// "sync"`, the default): calls return their result, and an operation id is
/// the exception a model must know how to handle, not the workflow.
pub const SERVER_INSTRUCTIONS_SYNC: &str = concat!(
    instructions_head!(),
    "\
(2) the command's output should be watched for errors — set `monitor_level` and ahma \
streams alerts when matching lines appear. \
Each call waits for the command to finish and returns its result, like a terminal. \
If a command outlasts what your client can wait for on one request, the call returns an \
operation_id and says it is still running: call `await` with that id to collect the \
result (it blocks; do not poll `status` in a loop), or `cancel` it. ",
    instructions_tail!()
);

/// The server instructions for a server in this mode.
pub fn server_instructions(policy: crate::config::ExecutionPolicy) -> &'static str {
    match policy {
        crate::config::ExecutionPolicy::Sync => SERVER_INSTRUCTIONS_SYNC,
        crate::config::ExecutionPolicy::Async => SERVER_INSTRUCTIONS,
    }
}

/// Client → server request invoking a tool.
pub const TOOLS_CALL_METHOD: &str = "tools/call";

/// Server → client request asking the client's model to complete a prompt.
pub const SAMPLING_CREATE_MESSAGE_METHOD: &str = "sampling/createMessage";

/// Client → server notification that the client's roots changed. The bridge
/// both emits this to its subprocess and matches it from the real client, in
/// two different files — the exact drift this module exists to prevent.
pub const ROOTS_LIST_CHANGED_METHOD: &str = "notifications/roots/list_changed";

/// Server → client notification: the sandbox is configured and locked for the
/// session. Params: [`SandboxLifecycleParams`] (SPEC R5.4 / R5.6).
pub const SANDBOX_CONFIGURED_METHOD: &str = "notifications/sandbox/configured";

/// Server → client notification: sandbox configuration failed.
/// Params: [`SandboxLifecycleParams`] with `error` set (SPEC R5.6).
pub const SANDBOX_FAILED_METHOD: &str = "notifications/sandbox/failed";

/// Server → client notification: the sandboxed session ended.
/// Params: [`SandboxTerminatedParams`] (SPEC R5.6).
pub const SANDBOX_TERMINATED_METHOD: &str = "notifications/sandbox/terminated";

/// Bridge → subprocess notification: whether the session currently has a live
/// push channel (an open SSE stream) to the real client.
/// Params: [`PushChannelChangedParams`] (SPEC R2.6.5.3).
pub const PUSH_CHANNEL_CHANGED_METHOD: &str = "notifications/ahma/pushChannelChanged";

/// Bidirectional ahma-peer keepalive notification. Params:
/// [`crate::keepalive::HeartbeatPayload`] (SPEC R8.8.4).
pub const HEARTBEAT_METHOD: &str = "notifications/ahma/heartbeat";

// ── JSON-RPC error codes ─────────────────────────────────────────────────────
//
// These live here for the same reason the method names do: each is written by
// one surface (the HTTP bridge) and matched by another (the stdio proxy, which
// recovers them from a rendered transport error), in a different crate. Spelled
// as bare literals at both ends, a change on either side type-checks.

/// `tools/call` arrived before the sandbox scope was locked — the client must
/// complete the roots exchange first. Paired with HTTP 409; the pair is a SPEC
/// RB.1.1 hard invariant and tests assert it directly.
pub const JSONRPC_SANDBOX_NOT_READY: i32 = -32001;

/// The request timed out waiting on the session subprocess or the client.
/// Paired with HTTP 504 (or 500 on the proxy's generic forward failure).
pub const JSONRPC_REQUEST_TIMEOUT: i32 = -32002;

/// Sandbox configuration failed, or the session is terminated — the session
/// will never become usable. Paired with HTTP 403.
pub const JSONRPC_SANDBOX_FAILED: i32 = -32000;

/// Standard JSON-RPC 2.0 Method Not Found error code.
pub const JSONRPC_METHOD_NOT_FOUND: i32 = -32601;

/// Deserialize a field to its `Default` when the value is missing **or**
/// malformed, instead of failing the whole payload (lenient parsing — see the
/// module docs).
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// Lenient display-path list: a non-array becomes empty, and non-string
/// elements are skipped individually (matching the historical
/// `filter_map(Value::as_str)` extraction in the HTTP bridge).
fn lenient_display_paths<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    })
}

/// Params of [`SANDBOX_CONFIGURED_METHOD`] and [`SANDBOX_FAILED_METHOD`]
/// (SPEC R5.6): one shape, two methods — `configured` carries `scope` (SPEC
/// R5.4: the complete locked scope with provenance), `failed` carries `error`.
/// Both fields are omitted from the wire when absent, so a plain `{}` payload
/// (the raw Linux fatal-exit emitter, older peers) stays valid.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SandboxLifecycleParams {
    /// Why sandbox configuration failed (`failed` only). Parsers fall back to
    /// a generic message when absent.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub error: Option<String>,
    /// The canonical scope summary (`configured` only; SPEC R5.4).
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub scope: Option<SandboxScopeSummary>,
}

/// Leniently parse a notification's `params`: missing or malformed input
/// yields `T::default()` rather than an error, mirroring the historical
/// `.get()` chains these typed params replaced — a lifecycle notification is
/// never rejected over its payload. Shared by every `*Params::from_params`.
fn lenient_from_params<T: serde::de::DeserializeOwned + Default>(
    params: Option<&serde_json::Value>,
) -> T {
    params
        .map(|p| serde_json::from_value(p.clone()).unwrap_or_default())
        .unwrap_or_default()
}

impl SandboxLifecycleParams {
    /// Leniently parse the params of a sandbox lifecycle notification: missing
    /// or malformed params yield the default (both fields `None`), mirroring
    /// the historical `.get()` chains — a lifecycle notification is never
    /// rejected over its payload.
    pub fn from_params(params: Option<&serde_json::Value>) -> Self {
        lenient_from_params(params)
    }

    /// As [`Self::from_params`], but starting from a full JSON-RPC
    /// notification value (reads its `params` member).
    pub fn from_notification(notification: &serde_json::Value) -> Self {
        Self::from_params(notification.get("params"))
    }
}

/// The structured sandbox scope summary carried by
/// [`SANDBOX_CONFIGURED_METHOD`] (SPEC R5.4: scope is always visible with
/// provenance). Produced by `ScopeView::to_json` / `Sandbox::scope_json` in
/// `ahma_mcp`; consumed by the HTTP bridge (write scopes) and the TUI scope
/// panel.
///
/// Unknown fields are preserved in [`extra`](Self::extra) so a field added by
/// the producer is never silently dropped on the wire by an intermediary that
/// deserializes and re-serializes this struct.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SandboxScopeSummary {
    /// Whether kernel enforcement is active (`false` == `--no-sandbox`).
    #[serde(default, deserialize_with = "lenient")]
    pub enforced: bool,
    /// Directories the AI may write to (display form).
    #[serde(default, deserialize_with = "lenient_display_paths")]
    pub write: Vec<String>,
    /// Read-only directories granted beyond the write roots (display form).
    #[serde(default, deserialize_with = "lenient_display_paths")]
    pub read: Vec<String>,
    /// Whether the system temp directory was added via `--tmp`.
    #[serde(default, deserialize_with = "lenient")]
    pub tmp: bool,
    /// Provenance of the scope (`explicit` | `roots/list` | `elicited` |
    /// `container` | `pending`; SPEC R5.4).
    #[serde(default, deserialize_with = "lenient")]
    pub source: String,
    /// Present (`true`) on platforms where reads are not kernel-scoped
    /// (macOS, R6.2.2; Windows while AppContainer is off, R6.3.9).
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub reads_unrestricted: Option<bool>,
    /// Present (`true`) on platforms where *writes* are not kernel-scoped either
    /// — currently only Windows, where the Job Object bounds process lifetime
    /// and nothing about paths (SPEC R6.3.9).
    ///
    /// Added alongside `reads_unrestricted` rather than folded into it: a client
    /// that renders "reads are open" very differently from "there is no boundary
    /// at all" needs to tell those apart, and on macOS only the first is true.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub writes_unrestricted: Option<bool>,
    /// Human-readable disclosure accompanying the two flags above — every note
    /// for this platform, joined. Kept for readers that predate
    /// [`platform_notes`](Self::platform_notes).
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub platform_note: Option<String>,
    /// The same disclosures, one per gap, so a renderer can list them rather
    /// than showing one run-on paragraph. Add-only per R24.5: a pre-R-PERM.5.1
    /// reader ignores it and still gets `platform_note`.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub platform_notes: Option<Vec<String>>,
    /// Which sandbox is actually protecting the user (`ahma` |
    /// `ahma_nested_in_host` | `deferred_to_host` | `disabled`; SPEC R5.4).
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub active: Option<String>,
    /// The loud one-line disclosure for [`active`](Self::active).
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub active_disclosure: Option<String>,
    /// The host sandbox label when nested in / deferring to one.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub host: Option<String>,
    /// Any fields this version does not know about, preserved verbatim.
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Params of [`SANDBOX_TERMINATED_METHOD`]: `{"reason": "..."}` (SPEC R5.6).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SandboxTerminatedParams {
    /// Why the session ended.
    #[serde(default, deserialize_with = "lenient")]
    pub reason: String,
}

/// Params of [`PUSH_CHANNEL_CHANGED_METHOD`]: `{"connected": <bool>}`
/// (SPEC R2.6.5.3). The receiver defaults to `false` — "no live channel" is
/// the safe, conservative assumption — so lenient parsing of a malformed
/// payload lands on exactly that default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushChannelChangedParams {
    /// Whether an SSE push channel to the real client is currently open.
    #[serde(default, deserialize_with = "lenient")]
    pub connected: bool,
}

impl PushChannelChangedParams {
    /// Leniently parse the params: missing or malformed input yields the safe
    /// default (`connected: false`), mirroring the historical `.get()` chain.
    pub fn from_params(params: Option<&serde_json::Value>) -> Self {
        lenient_from_params(params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wire(v: &impl Serialize) -> String {
        serde_json::to_string(v).unwrap()
    }

    // ── Byte-for-byte wire compatibility with the legacy `json!` emitters ────
    //
    // Each test rebuilds the payload exactly the way the emitter built it
    // before the typed structs existed, and asserts the serialized bytes are
    // identical. These pin the wire format: field names, optionality, and
    // (via serde_json's `preserve_order`) field order.

    #[test]
    fn configured_with_scope_matches_legacy_emitter_bytes() {
        // Legacy: ahma_mcp/src/mcp_service/sandbox_config.rs
        // emit_sandbox_notification_via_peer_with_scope — starts from `{}`
        // (error is None for `configured`) and inserts "scope".
        let scope = json!({
            "enforced": true,
            "write": ["/work/project", "/work/extra"],
            "read": ["/etc"],
            "tmp": false,
            "source": "roots/list",
            "active": "ahma",
            "active_disclosure": "Sandbox: ahma kernel sandbox is ENFORCING.",
        });
        let mut legacy = json!({});
        legacy
            .as_object_mut()
            .unwrap()
            .insert("scope".to_string(), scope.clone());

        let typed = SandboxLifecycleParams {
            error: None,
            scope: Some(serde_json::from_value(scope).unwrap()),
        };
        assert_eq!(wire(&typed), wire(&legacy));
    }

    #[test]
    fn configured_scope_with_platform_note_matches_legacy_bytes() {
        // Legacy scope with the macOS read disclosure appended (ScopeView::to_json).
        let scope = json!({
            "enforced": true,
            "write": ["/w"],
            "read": [],
            "tmp": true,
            "source": "explicit",
            "reads_unrestricted": true,
            "platform_note": "macOS reads are not kernel-scoped",
            "active": "ahma_nested_in_host",
            "active_disclosure": "nested",
            "host": "Cursor",
        });
        let typed: SandboxScopeSummary = serde_json::from_value(scope.clone()).unwrap();
        assert_eq!(wire(&typed), wire(&scope));
    }

    #[test]
    fn failed_matches_legacy_emitter_bytes() {
        // Legacy: `json!({ "error": err })`.
        let legacy = json!({ "error": "Landlock unavailable" });
        let typed = SandboxLifecycleParams {
            error: Some("Landlock unavailable".to_string()),
            scope: None,
        };
        assert_eq!(wire(&typed), wire(&legacy));
    }

    #[test]
    fn empty_lifecycle_params_serialize_as_empty_object() {
        // Legacy raw-stdout path: `json!({})` when no error is attached.
        assert_eq!(wire(&SandboxLifecycleParams::default()), "{}");
    }

    #[test]
    fn terminated_matches_legacy_emitter_bytes() {
        // Legacy: ahma_mcp/src/shell/modes/server.rs emit_sandbox_terminated.
        let legacy = json!({ "reason": "shutdown signal received" });
        let typed = SandboxTerminatedParams {
            reason: "shutdown signal received".to_string(),
        };
        assert_eq!(wire(&typed), wire(&legacy));
    }

    #[test]
    fn push_channel_changed_matches_legacy_emitter_bytes() {
        // Legacy: ahma_http_bridge/src/session.rs send_push_channel_changed.
        for connected in [true, false] {
            let legacy = json!({ "connected": connected });
            let typed = PushChannelChangedParams { connected };
            assert_eq!(wire(&typed), wire(&legacy));
        }
    }

    // ── Round trips ──────────────────────────────────────────────────────────

    #[test]
    fn lifecycle_params_round_trip() {
        let original = SandboxLifecycleParams {
            error: None,
            scope: Some(SandboxScopeSummary {
                enforced: true,
                write: vec!["/a".into(), "/b".into()],
                read: vec!["/r".into()],
                tmp: true,
                source: "roots/list".into(),
                reads_unrestricted: Some(true),
                writes_unrestricted: None,
                platform_note: Some("note".into()),
                platform_notes: Some(vec!["note".into()]),
                active: Some("ahma".into()),
                active_disclosure: Some("disclosure".into()),
                host: None,
                extra: serde_json::Map::new(),
            }),
        };
        let value = serde_json::to_value(&original).unwrap();
        let back: SandboxLifecycleParams = serde_json::from_value(value).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn scope_summary_preserves_unknown_fields_through_round_trip() {
        // A field added by a newer producer must survive deserialize +
        // re-serialize (the `extra` flatten map), not vanish from the wire.
        let scope = json!({
            "enforced": false,
            "write": ["/w"],
            "read": [],
            "tmp": false,
            "source": "explicit",
            "future_field": {"nested": 7},
        });
        let typed: SandboxScopeSummary = serde_json::from_value(scope.clone()).unwrap();
        assert_eq!(typed.extra.get("future_field"), Some(&json!({"nested": 7})));
        assert_eq!(serde_json::to_value(&typed).unwrap(), scope);
    }

    #[test]
    fn terminated_and_push_channel_round_trip() {
        let t = SandboxTerminatedParams { reason: "r".into() };
        let back: SandboxTerminatedParams =
            serde_json::from_value(serde_json::to_value(&t).unwrap()).unwrap();
        assert_eq!(back, t);

        let p = PushChannelChangedParams { connected: true };
        let back: PushChannelChangedParams =
            serde_json::from_value(serde_json::to_value(p).unwrap()).unwrap();
        assert_eq!(back, p);
    }

    // ── Lenient parsing (pins the historical fallback behaviors) ─────────────

    #[test]
    fn lenient_missing_params_yield_defaults() {
        // No `params` member at all → both fields default (the bridge then
        // preserves in-flight Configuring scopes / reports "Unknown error").
        let notif = json!({"jsonrpc": "2.0", "method": SANDBOX_CONFIGURED_METHOD});
        assert_eq!(
            SandboxLifecycleParams::from_notification(&notif),
            SandboxLifecycleParams::default()
        );
        assert_eq!(
            PushChannelChangedParams::from_params(None),
            PushChannelChangedParams::default()
        );
    }

    #[test]
    fn lenient_non_object_params_yield_defaults() {
        for params in [json!(null), json!(5), json!("x"), json!([1, 2])] {
            assert_eq!(
                SandboxLifecycleParams::from_params(Some(&params)),
                SandboxLifecycleParams::default()
            );
            assert_eq!(
                PushChannelChangedParams::from_params(Some(&params)),
                PushChannelChangedParams::default()
            );
        }
    }

    #[test]
    fn lenient_malformed_error_field_falls_back_to_none() {
        // Historically `params.error` was read with `.as_str()`: a non-string
        // error meant the generic fallback message, never a parse failure.
        let params = json!({"error": 42});
        assert_eq!(
            SandboxLifecycleParams::from_params(Some(&params)).error,
            None
        );
    }

    #[test]
    fn lenient_scope_write_skips_non_string_entries() {
        // Historically `scope.write` was extracted with
        // `filter_map(Value::as_str)`: mixed-type arrays keep their strings.
        let params = json!({"scope": {"write": ["/keep", 1, null, "/also"]}});
        let parsed = SandboxLifecycleParams::from_params(Some(&params));
        assert_eq!(
            parsed.scope.unwrap().write,
            vec!["/keep".to_string(), "/also".to_string()]
        );
    }

    #[test]
    fn lenient_scope_write_non_array_becomes_empty() {
        let params = json!({"scope": {"write": "not-an-array"}});
        let parsed = SandboxLifecycleParams::from_params(Some(&params));
        assert_eq!(parsed.scope.unwrap().write, Vec::<String>::new());
    }

    #[test]
    fn lenient_non_object_scope_becomes_none() {
        let params = json!({"scope": "oops"});
        assert_eq!(
            SandboxLifecycleParams::from_params(Some(&params)).scope,
            None
        );
    }

    #[test]
    fn lenient_malformed_connected_falls_back_to_false() {
        // The safe default is "no live channel" (SPEC R2.6.5.3).
        for params in [json!({"connected": "yes"}), json!({}), json!({"other": 1})] {
            assert!(!PushChannelChangedParams::from_params(Some(&params)).connected);
        }
    }

    #[test]
    fn lenient_malformed_reason_falls_back_to_empty() {
        let parsed: SandboxTerminatedParams =
            serde_json::from_value(json!({"reason": ["not", "a", "string"]})).unwrap();
        assert_eq!(parsed.reason, "");
    }

    #[test]
    fn method_constants_are_the_wire_strings() {
        // The literals are load-bearing wire strings; pin them so a rename in
        // the constant can never silently change the protocol.
        assert_eq!(
            SANDBOX_CONFIGURED_METHOD,
            "notifications/sandbox/configured"
        );
        assert_eq!(SANDBOX_FAILED_METHOD, "notifications/sandbox/failed");
        assert_eq!(
            SANDBOX_TERMINATED_METHOD,
            "notifications/sandbox/terminated"
        );
        assert_eq!(
            PUSH_CHANNEL_CHANGED_METHOD,
            "notifications/ahma/pushChannelChanged"
        );
        assert_eq!(HEARTBEAT_METHOD, "notifications/ahma/heartbeat");
        assert_eq!(SERVER_DISCOVER_METHOD, "server/discover");
        assert_eq!(SUBSCRIPTIONS_LISTEN_METHOD, "subscriptions/listen");
    }
}
