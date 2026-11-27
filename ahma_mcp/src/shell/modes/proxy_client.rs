//! # Stdio MCP Proxy Client
//!
//! When the localhost bridge server (UDS or HTTP) is already running,
//! this module acts as a transparent proxy that forwards all stdio
//! JSON-RPC traffic to the running server.

use super::modern_input::{self, CallRoute, CallStep, Flush, ModernSession};
use crate::transport_patch::PatchedStdioTransport;
use ahma_common::mcp_methods::ROOTS_LIST_METHOD;
use ahma_common::mcp_methods::{
    INITIALIZE_METHOD, INITIALIZED_METHOD, SERVER_DISCOVER_METHOD, SERVER_INSTRUCTIONS,
    SUBSCRIPTIONS_LISTEN_METHOD, TOOLS_CALL_METHOD,
};
use ahma_common::mcp_protocol::MCP_PROTOCOL_VERSION_2025_11_25;
use anyhow::{Context, Result, anyhow};
use rmcp::model::{CustomResult, RequestId, ServerResult};
use rmcp::service::RoleClient;
use rmcp::service::{RoleServer, TxJsonRpcMessage};
use rmcp::transport::Transport;
use std::time::Duration;

/// How many *consecutive* forward failures the proxy tolerates before treating
/// the bridge transport as genuinely dead and attempting to reconnect. A single
/// failure (e.g. a per-request timeout or a sandbox-initializing 409) is relayed
/// to the client and the session is preserved; only a sustained run of failures
/// — meaning the transport itself is broken, not one request — triggers a
/// reconnect (or, failing that, exit).
///
/// This threshold applies to failures on a *live* endpoint only. A failure that
/// proves the endpoint itself is gone ([`ForwardFailure::EndpointGone`]) cannot
/// improve by being retried, so it bypasses the count entirely and recovers on
/// the first occurrence — see the gone-endpoint branch in `run_transport_proxy`.
const MAX_CONSECUTIVE_FORWARD_FAILURES: u32 = 3;

/// How many reconnect attempts make up one recovery burst. Each attempt
/// rebuilds the bridge connection and replays the cached handshake; a transient
/// bridge restart typically recovers within one or two attempts.
///
/// A failed burst does **not** end the session (SPEC R-LIFECYCLE.3): while the editor
/// is attached the proxy stays attached, answers requests with
/// [`BRIDGE_UNREACHABLE_MESSAGE`] and runs another burst on the next request.
/// Exiting instead left the editor showing ahma as dead until the user
/// reconnected by hand, although the next hub was one respawn away.
const MAX_RECONNECT_ATTEMPTS: u32 = 3;

/// What a request is answered with while the bridge cannot be reached.
const BRIDGE_UNREACHABLE_MESSAGE: &str = "ahma is restarting or unreachable. This connection \
     stays open and reconnects automatically on your next request, so retry shortly. \
     Operations started before the restart may have finished; check their effect (or \
     `status`) before re-running them.";

/// The handshake messages captured the first time they flow through the proxy,
/// so a dead bridge transport can be reconnected *invisibly to the downstream
/// client*: the client (Claude Code, Cursor, …) already sent `initialize` and
/// answered `roots/list` once and does not expect — and in practice will not
/// resend — either on a mid-session reconnect. The proxy replays its own cached
/// copies against a freshly built bridge connection instead.
#[derive(Default, Clone)]
struct CachedHandshake {
    /// The raw `initialize` request the client sent, if seen yet.
    init_request: Option<serde_json::Value>,
    /// The raw `notifications/initialized` notification, if seen yet.
    notif_initialized: Option<serde_json::Value>,
    /// The `id` of an in-flight bridge→client `roots/list` request whose answer
    /// has not yet been observed flowing back from the client.
    pending_roots_list_id: Option<serde_json::Value>,
    /// The client's answer to the most recent `roots/list` request, if seen yet.
    /// The sandbox scope cannot change during a session (SPEC security
    /// invariant), so replaying this cached answer on reconnect is correct, not
    /// just convenient.
    roots_response: Option<serde_json::Value>,
    /// The `serverInfo.version` the client last saw: from the first
    /// `initialize` answer, then from each reconnect that announced a change.
    server_version: Option<String>,
    /// The `serverInfo.version` the most recent replay was answered with,
    /// left for the reconnect site to compare. Shared rather than returned so
    /// every reconnect path reports it the same way.
    replayed_version: std::sync::Arc<parking_lot::Mutex<Option<String>>>,
}

/// `result.serverInfo.version` of an `initialize` answer.
fn server_version_of(response: &serde_json::Value) -> Option<String> {
    response
        .get("result")?
        .get("serverInfo")?
        .get("version")?
        .as_str()
        .map(String::from)
}

impl CachedHandshake {
    /// Observe a message forwarded from the client (stdio) to the bridge, and
    /// cache it if the reconnect replay will need it.
    fn observe_client_to_bridge(&mut self, val: &serde_json::Value) {
        let method = val.get("method").and_then(|m| m.as_str());
        if method == Some("initialize") && self.init_request.is_none() {
            self.init_request = Some(val.clone());
        }
        if method == Some(INITIALIZED_METHOD) && self.notif_initialized.is_none() {
            self.notif_initialized = Some(val.clone());
        }
        if self.pending_roots_list_id.is_some()
            && self.pending_roots_list_id == val.get("id").cloned()
        {
            self.roots_response = Some(val.clone());
            self.pending_roots_list_id = None;
        }
    }

    /// Observe a message forwarded from the bridge to the client, and note when
    /// it is a `roots/list` request whose answer we need to watch for.
    fn observe_bridge_to_client(&mut self, val: &serde_json::Value) {
        if self.server_version.is_none()
            && val.get("id").is_some()
            && self.init_request.as_ref().and_then(|r| r.get("id")) == val.get("id")
        {
            self.server_version = server_version_of(val);
        }
        if val.get("method").and_then(|m| m.as_str()) == Some(ROOTS_LIST_METHOD) {
            self.pending_roots_list_id = val.get("id").cloned();
        }
    }
}

/// Replay the cached handshake against a freshly built bridge connection:
/// resend `initialize`, resend `notifications/initialized`, then answer the
/// bridge's `roots/list` request from the cached response. None of this reaches
/// `stdio` — the downstream client already completed this handshake once and
/// must not see it repeated.
async fn replay_handshake<C>(client: &mut C, handshake: &CachedHandshake) -> Result<()>
where
    C: Transport<RoleClient>,
    C::Error: std::fmt::Debug,
{
    let Some(init_request) = &handshake.init_request else {
        // No handshake was ever observed (the failure happened before
        // `initialize`); nothing to replay.
        return Ok(());
    };
    let init_msg: TxJsonRpcMessage<RoleClient> = serde_json::from_value(init_request.clone())
        .context("reconnect: cached initialize request no longer deserializes")?;
    client
        .send(init_msg)
        .await
        .map_err(|e| anyhow!("reconnect: failed to resend initialize: {e:?}"))?;
    let answer = client
        .receive()
        .await
        .ok_or_else(|| anyhow!("reconnect: bridge closed before answering initialize"))?;
    *handshake.replayed_version.lock() = serde_json::to_value(&answer)
        .ok()
        .as_ref()
        .and_then(server_version_of);

    if let Some(notif) = &handshake.notif_initialized {
        let notif_msg: TxJsonRpcMessage<RoleClient> = serde_json::from_value(notif.clone())
            .context("reconnect: cached notifications/initialized no longer deserializes")?;
        client
            .send(notif_msg)
            .await
            .map_err(|e| anyhow!("reconnect: failed to resend notifications/initialized: {e:?}"))?;
    }

    if let Some(roots_response) = &handshake.roots_response {
        // A deferred-roots session (SPEC R-HUB.4.3) is not asked for roots at
        // handshake time; tell it they changed, as the first call did.
        if modern_input::defers_roots(init_request) {
            let changed: TxJsonRpcMessage<RoleClient> = serde_json::from_value(serde_json::json!({
                "jsonrpc": "2.0",
                "method": ahma_common::mcp_methods::ROOTS_LIST_CHANGED_METHOD,
            }))
            .context("reconnect: roots/list_changed does not deserialize")?;
            client
                .send(changed)
                .await
                .map_err(|e| anyhow!("reconnect: failed to send roots/list_changed: {e:?}"))?;
        }
        tokio::time::timeout(
            Duration::from_secs(15),
            wait_and_answer_roots_list(client, roots_response),
        )
        .await
        .map_err(|_| anyhow!("reconnect: timed out waiting for roots/list"))??;
    }
    Ok(())
}

async fn wait_and_answer_roots_list<C>(
    client: &mut C,
    roots_response: &serde_json::Value,
) -> Result<()>
where
    C: Transport<RoleClient>,
    C::Error: std::fmt::Debug,
{
    loop {
        let Some(msg) = client.receive().await else {
            return Err(anyhow!(
                "reconnect: bridge closed before requesting roots/list"
            ));
        };
        let val = serde_json::to_value(&msg).unwrap_or_default();
        if val.get("method").and_then(|m| m.as_str()) != Some(ROOTS_LIST_METHOD) {
            continue;
        }
        let mut resp = roots_response.clone();
        if let Some(id) = val.get("id") {
            resp["id"] = id.clone();
        }
        let resp_msg: TxJsonRpcMessage<RoleClient> = serde_json::from_value(resp)
            .context("reconnect: cached roots/list response no longer deserializes")?;
        return client
            .send(resp_msg)
            .await
            .map_err(|e| anyhow!("reconnect: failed to answer roots/list: {e:?}"));
    }
}

/// Session-health disclosure (#485): the proxy is the *only* party that knows
/// a transparent reconnect happened — the rebuilt bridge session is brand-new
/// and the downstream client was deliberately kept unaware of the replay. So
/// the proxy synthesizes the disclosure itself, downstream only: a canonical
/// `notifications/ahma/session_event` plus its `notifications/message` mirror
/// for foreign clients. Best-effort — a failed send must never affect the
/// session that was just saved.
async fn emit_session_event_downstream<S>(
    stdio: &mut S,
    seq: &mut u64,
    kind: ahma_common::session_event::SessionEventKind,
    detail: serde_json::Value,
) where
    S: Transport<RoleServer>,
    S::Error: std::fmt::Debug,
{
    use ahma_common::session_event::{event_notification, message_mirror_notification};
    *seq += 1;
    let now = ahma_common::keepalive::current_timestamp_ms();
    for val in [
        event_notification(kind, *seq, now, detail.clone()),
        message_mirror_notification(kind, *seq, now, detail),
    ] {
        send_session_event_notification(stdio, kind, val).await;
    }
}

/// Did the reconnect just replayed land on a different ahma version?
///
/// Consumes the replayed version, and adopts it as the one the client has
/// now been told about, so one change is announced once.
fn version_changed_on_reconnect(handshake: &mut CachedHandshake) -> bool {
    let replayed = handshake.replayed_version.lock().take();
    match replayed {
        Some(new) if handshake.server_version.as_deref() != Some(new.as_str()) => {
            handshake.server_version = Some(new);
            true
        }
        _ => false,
    }
}

/// Disclose a transparent reconnect downstream, and — when it landed on a
/// different ahma version — tell the client its tool list may have changed
/// (SPEC R-HUB.5). The client cached `tools/list` from the old build; without
/// this it would keep calling tools by the old build's schemas until it
/// restarted.
async fn announce_reconnect<S>(
    stdio: &mut S,
    seq: &mut u64,
    handshake: &mut CachedHandshake,
    detail: serde_json::Value,
) where
    S: Transport<RoleServer>,
    S::Error: std::fmt::Debug,
{
    emit_session_event_downstream(
        stdio,
        seq,
        ahma_common::session_event::SessionEventKind::Reconnected,
        detail,
    )
    .await;
    if version_changed_on_reconnect(handshake) {
        let notice = serde_json::json!({
            "jsonrpc": "2.0",
            "method": TOOLS_LIST_CHANGED_METHOD,
        });
        match serde_json::from_value::<TxJsonRpcMessage<RoleServer>>(notice) {
            Ok(msg) => {
                if let Err(e) = stdio.send(msg).await {
                    tracing::debug!(error = ?e, "tools/list_changed to stdio failed (non-fatal)");
                }
            }
            Err(e) => tracing::debug!(error = %e, "tools/list_changed did not serialize"),
        }
    }
}

/// The notification that tells a client to fetch `tools/list` again.
const TOOLS_LIST_CHANGED_METHOD: &str = "notifications/tools/list_changed";

/// Deserialize one session-event notification and send it downstream,
/// logging (never propagating) either failure: a malformed notification or a
/// failed send must not affect the session that was just saved.
async fn send_session_event_notification<S>(
    stdio: &mut S,
    kind: ahma_common::session_event::SessionEventKind,
    val: serde_json::Value,
) where
    S: Transport<RoleServer>,
    S::Error: std::fmt::Debug,
{
    match serde_json::from_value::<TxJsonRpcMessage<RoleServer>>(val) {
        Ok(msg) => {
            if let Err(e) = stdio.send(msg).await {
                tracing::debug!(
                    kind = kind.as_str(),
                    error = ?e,
                    "session event emission to stdio failed (non-fatal)"
                );
            }
        }
        Err(e) => tracing::debug!(
            kind = kind.as_str(),
            error = %e,
            "session event did not serialize as a notification (non-fatal)"
        ),
    }
}

/// Overlay the proxy's reconnect count onto a forwarded
/// `notifications/ahma/heartbeat` (#485): the server behind the proxy cannot
/// know how many times its transport was rebuilt, so the proxy owns this field.
/// Returns whether the value was mutated, so the caller only rebuilds the typed
/// message from JSON when the overlay actually applied.
fn overlay_heartbeat_reconnects(val: &mut serde_json::Value, reconnects: u32) -> bool {
    if reconnects > 0
        && val.get("method").and_then(|m| m.as_str()) == Some("notifications/ahma/heartbeat")
        && let Some(params) = val.get_mut("params").and_then(|p| p.as_object_mut())
    {
        params.insert("reconnects".to_string(), serde_json::json!(reconnects));
        return true;
    }
    false
}

/// Async hook that respawns the background bridge. Provided by the frontend
/// path (which owns the `AppConfig` needed to spawn); `None` elsewhere.
///
/// Not `#[cfg(unix)]`: the alias appears in cross-platform signatures
/// (`run_proxy_client`, the server frontend). Only the *consumer* — the
/// Unix-transport reconnect loop — is platform-gated; on Windows the hook is
/// accepted and unused.
pub type BridgeRespawnFn = Box<
    dyn FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> + Send,
>;

/// True when a *rendered* failure (Display or Debug) carries the signature of a
/// gone bridge endpoint — the socket file was unlinked, or nothing is listening
/// on it. Factored out of [`reconnect_failure_wants_respawn`] so the same
/// judgement can be applied to a transport `send` error, which is not an
/// `anyhow::Error` and has no error chain to walk.
///
/// Re-dialing a gone endpoint can never succeed on its own; only respawning the
/// bridge restores service.
fn error_text_indicates_gone_endpoint(text: &str) -> bool {
    text.contains("No such file or directory")
        || text.contains("Connection refused")
        || text.contains("(os error 2)")
        || text.contains("(os error 61)")
        // Windows: WSAECONNREFUSED, what an `AF_UNIX` connect to a socket file
        // with no listener fails with there.
        || text.contains("(os error 10061)")
        // Debug renderings of a nested io error keep the kind, not the message.
        || text.contains("kind: NotFound")
        || text.contains("kind: ConnectionRefused")
}

/// True when a reconnect failure indicates the bridge endpoint itself is gone
/// (socket file unlinked, nothing listening) rather than a transient error on
/// a live endpoint.
fn reconnect_failure_wants_respawn(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        {
            return true;
        }
    }
    // Some transport layers stringify the underlying io error instead of
    // preserving it in the error chain.
    error_text_indicates_gone_endpoint(&format!("{err:#}"))
}

/// The marker written ahead of a folded HTTP response body:
/// `StreamableHttpError::UnexpectedServerResponse(format!("HTTP {status}: {body}"))`
/// in `LocalSocketHttpClient::post_message`.
const HTTP_BODY_MARKER: &str = "HTTP ";

/// Recover the JSON-RPC `error` object the bridge actually sent, from the
/// rendered form of a transport `send` error.
///
/// rmcp folds **every** non-2xx bridge response into a transport error whose
/// `Display` is `unexpected server response: HTTP <status>: <body>`, where
/// `<body>` is the bridge's full JSON-RPC error body. Without this recovery the
/// proxy would throw away every actionable answer the bridge gives — the 409 /
/// `-32001` "sandbox initializing from client roots" instruction, the 504 /
/// `-32002` handshake-timeout checklist, 403 / `-32000` sandbox-configuration
/// failures — and replace them with a generic "retry or await" sentence that
/// sends the model chasing an operation that never existed.
///
/// Returns `None` when the rendered error carries no HTTP body at all (a
/// genuinely dead socket: ENOENT / ECONNREFUSED), when the body is not JSON, or
/// when the body has no `error` object the downstream client could decode.
fn recover_bridge_jsonrpc_error(rendered: &str) -> Option<serde_json::Value> {
    // Find where the body's JSON *starts* rather than splitting on ':' — the
    // status line, the JSON structure and the bridge's own message all contain
    // colons, and the message contains braces and quotes too.
    let after_marker = rendered.find(HTTP_BODY_MARKER)? + HTTP_BODY_MARKER.len();
    let body_start = after_marker + rendered[after_marker..].find('{')?;
    // Parse only the first JSON value: anything a wrapping error appended after
    // the body is ignored instead of failing the whole parse.
    let body = serde_json::Deserializer::from_str(&rendered[body_start..])
        .into_iter::<serde_json::Value>()
        .next()?
        .ok()?;
    let error = body.get("error")?;
    // Only relay something that is actually a JSON-RPC error object; relaying a
    // half-formed one would fail to deserialize downstream and the client would
    // get nothing at all for this request id.
    let usable = error.get("code").is_some_and(|c| c.is_i64())
        && error.get("message").is_some_and(|m| m.is_string());
    usable.then(|| error.clone())
}

/// What a failed forward to the bridge actually means. Distinguishing these is
/// the difference between telling the model something true and something
/// invented, and between recovering now and burning three requests first.
#[derive(Debug, PartialEq, Eq)]
enum ForwardFailure {
    /// The bridge answered — with a non-2xx HTTP status whose body carried a
    /// JSON-RPC `error`. The endpoint is manifestly alive; relay the error.
    BridgeError(serde_json::Value),
    /// Nothing is listening on the bridge endpoint any more (auto-spawned
    /// bridges unlink their socket and exit after `--idle-timeout`). The
    /// request never reached the bridge, so it can be resent after a reconnect.
    EndpointGone,
    /// The hub answered 404 for this proxy's session: it does not know it
    /// (it handed over to a successor, or restarted). The request never ran.
    SessionGone,
    /// Anything else: a transient error on an endpoint that is still there.
    Transient,
}

/// Classify a transport `send` error from both its `Display` and `Debug`
/// renderings. `Display` carries the embedded HTTP body (`Debug` escapes its
/// quotes, which is why the caller needs a `Display` bound); `Debug` carries
/// the io `kind` when an intermediate layer dropped the message.
///
/// Order matters: a recoverable bridge body proves the endpoint answered, so it
/// is never mistaken for a gone endpoint even if the bridge's own message
/// happens to mention a missing file.
fn classify_forward_failure(display: &str, debug: &str) -> ForwardFailure {
    if let Some(error) = recover_bridge_jsonrpc_error(display) {
        return ForwardFailure::BridgeError(error);
    }
    if display.contains(SESSION_EXPIRED_DISPLAY) || debug.contains("SessionExpired") {
        return ForwardFailure::SessionGone;
    }
    if error_text_indicates_gone_endpoint(display) || error_text_indicates_gone_endpoint(debug) {
        return ForwardFailure::EndpointGone;
    }
    ForwardFailure::Transient
}

/// `Display` of rmcp's `StreamableHttpError::SessionExpired`: the hub answered
/// 404 for the session id this proxy holds.
const SESSION_EXPIRED_DISPLAY: &str = "Session expired (HTTP 404)";

/// Whether a failed forward means the hub's session is not the one this client
/// set up, so the proxy must rebuild it from its cached handshake and resend
/// the request (SPEC R-HUB.5). Either the hub does not know the session (404),
/// or it answers "sandbox initializing" (409, `-32001`) although the client
/// already gave its roots, so the session it holds lost them. In both cases
/// the hub never ran the request.
fn failure_rebuilds_session(failure: &ForwardFailure, handshake: &CachedHandshake) -> bool {
    match failure {
        ForwardFailure::SessionGone => true,
        ForwardFailure::BridgeError(error) => {
            handshake.roots_response.is_some()
                && error.get("code").and_then(serde_json::Value::as_i64)
                    == Some(i64::from(
                        ahma_common::mcp_methods::RpcErrorCode::SandboxNotReady.code(),
                    ))
        }
        ForwardFailure::EndpointGone | ForwardFailure::Transient => false,
    }
}

/// The message the proxy invents when a forward failed and *nothing* better
/// could be recovered from it. Deliberately vague, because in that case the
/// proxy genuinely does not know whether the bridge saw the request.
const GENERIC_FORWARD_FAILURE_MESSAGE: &str = "Bridge could not service this request; it may \
                                               still be running. Retry, or await the completion \
                                               notification.";

/// Answer a single request id downstream after its forward failed: the bridge's
/// own JSON-RPC error when one could be recovered, otherwise the generic
/// fallback. Best-effort — a failed relay must not end the session.
async fn relay_forward_failure_to_client<S>(
    stdio: &mut S,
    request_id: serde_json::Value,
    recovered: Option<serde_json::Value>,
) where
    S: Transport<RoleServer>,
    S::Error: std::fmt::Debug,
{
    let error = recovered.unwrap_or_else(|| {
        serde_json::json!({
            "code": ahma_common::mcp_methods::RpcErrorCode::RequestTimeout,
            "message": GENERIC_FORWARD_FAILURE_MESSAGE,
        })
    });
    let err_val = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "error": error,
    });
    if let Ok(err_msg) = serde_json::from_value::<TxJsonRpcMessage<RoleServer>>(err_val) {
        let _ = stdio.send(err_msg).await;
    }
}

/// True for the messages [`replay_handshake`] resends on a rebuilt connection.
/// Such a message must never *also* be resent by the gone-endpoint recovery
/// path: the replay already delivered it, and a second `initialize` on the
/// fresh session is a protocol error.
fn message_is_replayed_by_handshake(val: &serde_json::Value) -> bool {
    matches!(
        val.get("method").and_then(|m| m.as_str()),
        Some("initialize") | Some(INITIALIZED_METHOD)
    )
}

/// Transparently resend, on a freshly reconnected transport, the one request
/// whose forward failed because the bridge endpoint or its session was gone.
///
/// Resending is safe and is *not* a duplicate execution: this path only runs
/// when the failure proved the bridge never ran the request (the endpoint was
/// gone, or the hub refused it for a session it does not know or that has no
/// roots).
///
/// `Err` when the request did not go out, carrying the bridge's own JSON-RPC
/// error when it answered one; the caller must answer the request with it, or
/// with the generic error.
async fn resend_pending<C>(
    fresh: &mut C,
    pending: &serde_json::Value,
    transport: &str,
) -> Result<(), Option<serde_json::Value>>
where
    C: Transport<RoleClient>,
    C::Error: std::fmt::Debug + std::fmt::Display,
{
    match serde_json::from_value::<TxJsonRpcMessage<RoleClient>>(pending.clone()) {
        Ok(msg) => match fresh.send(msg).await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(
                    transport,
                    error = ?e,
                    "Resend on the freshly reconnected bridge failed; \
                     answering the request with an error instead"
                );
                Err(recover_bridge_jsonrpc_error(&e.to_string()))
            }
        },
        Err(e) => {
            tracing::warn!(
                transport,
                error = %e,
                "Pending request no longer deserializes; cannot resend it"
            );
            Err(None)
        }
    }
}

/// Why the proxy rebuilt its bridge transport: the `cause` of the
/// `reconnected` disclosure (#485), and what happened to the request that
/// triggered the rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReconnectCause {
    /// A request arrived while the bridge was unreachable. Nothing was in
    /// flight; the triggering request is forwarded on the fresh transport.
    BridgeUnreachable,
    /// A forward failed because the bridge endpoint was gone. The request
    /// never reached the bridge, so it was resent on the fresh transport;
    /// `resent` says whether that worked.
    EndpointGone { resent: bool },
    /// The hub's session was not the one this client set up: it did not know
    /// it (after a handover or restart), or it had lost the client's roots
    /// (SPEC R-HUB.5). The request never ran, so it was resent on the rebuilt
    /// session; `resent` says whether that worked.
    SessionGone { resent: bool },
    /// Forwards kept failing until the threshold tripped. The request that
    /// tripped it was already answered with an error.
    TransportFailure,
}

impl ReconnectCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::BridgeUnreachable => "bridge_unreachable",
            Self::EndpointGone { .. } => "endpoint_gone",
            Self::SessionGone { .. } => "session_gone",
            Self::TransportFailure => "transport_failure",
        }
    }

    /// Whether the client's request reached the fresh transport, so it has
    /// nothing to retry.
    fn in_flight_resent(self) -> bool {
        match self {
            Self::BridgeUnreachable => true,
            Self::EndpointGone { resent } | Self::SessionGone { resent } => resent,
            Self::TransportFailure => false,
        }
    }
}

/// The proxy's bridge transport, and the per-session state that describes it.
///
/// Every transport swap goes through [`BridgeConnection::adopt`], so what a
/// successful reconnect resets is written once (R-LIFECYCLE.3, R8.8.3). Three
/// reconnect paths used to reset it by hand and drifted apart: one that skipped
/// `forwarded_any` left a session on the fresh bridge that the teardown never
/// closed and the handshake deadline could exit under.
struct BridgeConnection<C> {
    /// The bridge transport: live, or dead while `disconnected`.
    client: C,
    /// Whether a bridge session could exist. Until the client's `initialize`
    /// reaches a bridge, the bridge never creates a session for this proxy (and
    /// the underlying rmcp worker is parked awaiting the first message without
    /// observing its cancellation token). In that state `client.close()` would
    /// block forever, so the teardown only closes when this is set. It also
    /// disarms the handshake deadline (R-LIFECYCLE.2).
    forwarded_any: bool,
    /// Whether the bridge ever responded. This is the signal
    /// `handle_version_checks` uses to detect a stale bridge: a healthy bridge
    /// always replies to `initialize`; a stale one closes silently.
    bridge_responded: bool,
    /// Consecutive failures to forward a request on the current transport.
    /// Reset on any success, and on adopting a fresh transport. A single
    /// failure does not tear the session down (see
    /// [`MAX_CONSECUTIVE_FORWARD_FAILURES`]).
    consecutive_forward_failures: u32,
    /// Transparent reconnects this session (#485), overlaid on heartbeats.
    reconnects: u32,
    /// The monotonic seq of the session events that disclose them (R8.8.1).
    event_seq: u64,
    /// The bridge could not be reached (or closed a handshaken session). The
    /// proxy stays attached and reconnects when the next request arrives.
    disconnected: bool,
    /// Whether this outage has been disclosed; one `reconnect_failed` per outage.
    outage_disclosed: bool,
}

impl<C> BridgeConnection<C>
where
    C: Transport<RoleClient>,
{
    fn new(client: C) -> Self {
        Self {
            client,
            forwarded_any: false,
            bridge_responded: false,
            consecutive_forward_failures: 0,
            reconnects: 0,
            event_seq: 0,
            disconnected: false,
            outage_disclosed: false,
        }
    }

    /// Swap in a freshly reconnected transport whose handshake replay
    /// succeeded, reset the session state for it, and disclose the rebuild
    /// downstream. Every reconnect path ends here, whatever triggered it.
    async fn adopt<S>(
        &mut self,
        fresh: C,
        cause: ReconnectCause,
        stdio: &mut S,
        handshake: &mut CachedHandshake,
    ) where
        S: Transport<RoleServer>,
        S::Error: std::fmt::Debug,
    {
        // Best-effort: release the dead connection's resources. Bounded so a
        // broken close() cannot stall the now-healthy session waiting on it.
        let _ = tokio::time::timeout(Duration::from_secs(2), self.client.close()).await;
        self.client = fresh;
        // The replay opened a session on the fresh transport: the teardown
        // must close it, and the handshake deadline must not end the proxy
        // under it — even if the request that follows fails to forward.
        self.forwarded_any = true;
        // The replay's `initialize` answer proved the fresh bridge is alive.
        self.bridge_responded = true;
        // Failures on the dead transport say nothing about this one.
        self.consecutive_forward_failures = 0;
        self.disconnected = false;
        self.outage_disclosed = false;
        self.reconnects += 1;
        let detail = reconnected_detail(self.reconnects, cause);
        announce_reconnect(stdio, &mut self.event_seq, handshake, detail).await;
    }

    /// Record that a reconnect burst failed: the proxy stays attached and
    /// retries when the next request arrives. The outage is disclosed once,
    /// however many requests arrive during it (R-LIFECYCLE.3).
    async fn mark_unreachable<S>(&mut self, stdio: &mut S, err: &anyhow::Error)
    where
        S: Transport<RoleServer>,
        S::Error: std::fmt::Debug,
    {
        self.disconnected = true;
        if !self.outage_disclosed {
            self.outage_disclosed = true;
            emit_session_event_downstream(
                stdio,
                &mut self.event_seq,
                ahma_common::session_event::SessionEventKind::ReconnectFailed,
                reconnect_failed_detail(err),
            )
            .await;
        }
    }
}

/// Build the `detail` payload for the transparent-reconnect disclosure (#485).
///
/// The two cases must not be conflated: when the failed request was resent on
/// the fresh transport the client has nothing to do, and telling it the request
/// "was answered with an error and can be retried" would be a lie that invites
/// a duplicate call.
fn reconnected_detail(reconnects: u32, cause: ReconnectCause) -> serde_json::Value {
    let (in_flight_request, message) = if cause.in_flight_resent() {
        (
            "resent",
            "ahma bridge session was rebuilt transparently and the in-flight request was \
             resent on the new connection; no client action is needed",
        )
    } else {
        (
            "answered_with_error",
            "ahma bridge session was rebuilt transparently; in-flight requests were \
             answered with an error and can be retried",
        )
    };
    serde_json::json!({
        "cause": cause.as_str(),
        "reconnects": reconnects,
        "in_flight_request": in_flight_request,
        "message": message,
    })
}

/// The `detail` of a `reconnect_failed` event: the bridge is unreachable, but
/// the session is not over — the proxy retries on the next request.
fn reconnect_failed_detail(err: &anyhow::Error) -> serde_json::Value {
    serde_json::json!({
        "cause": err.to_string(),
        "attempts": MAX_RECONNECT_ATTEMPTS,
        "message": "ahma bridge is unreachable; this connection stays open and \
                    reconnects automatically on the next request",
    })
}

/// The JSON-RPC error a request gets while the bridge is unreachable.
fn bridge_unreachable_error() -> serde_json::Value {
    serde_json::json!({
        "code": ahma_common::mcp_methods::RpcErrorCode::RequestTimeout,
        "message": BRIDGE_UNREACHABLE_MESSAGE,
    })
}

/// Rebuild the bridge connection (via `reconnect`) and replay the cached
/// handshake, retrying up to [`MAX_RECONNECT_ATTEMPTS`] times with a short
/// backoff. Returns the freshly reconnected client on success.
///
/// When an attempt fails because the bridge endpoint is *gone* (not merely
/// glitching) and a `respawn` hook is available, the hook is invoked before
/// the next attempt so the retry has a live bridge to dial — without this the
/// proxy could only re-dial a socket that no longer exists until the attempts
/// were exhausted, and the client saw the server as dead.
async fn reconnect_with_retries<C>(
    reconnect: &mut dyn FnMut() -> Result<C>,
    handshake: &CachedHandshake,
    transport: &str,
    mut respawn: Option<&mut BridgeRespawnFn>,
) -> Result<C>
where
    C: Transport<RoleClient>,
    C::Error: std::fmt::Debug,
{
    let mut last_err = None;
    for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
        let outcome = async {
            let mut fresh = reconnect()?;
            replay_handshake(&mut fresh, handshake).await?;
            Ok(fresh)
        }
        .await;
        match outcome {
            Ok(fresh) => return Ok(fresh),
            Err(e) => {
                tracing::warn!(
                    transport,
                    attempt,
                    max_attempts = MAX_RECONNECT_ATTEMPTS,
                    error = %e,
                    "Reconnect attempt failed"
                );
                respawn_after_reconnect_failure(&e, respawn.as_deref_mut(), transport).await;
                last_err = Some(e);
            }
        }
        if attempt < MAX_RECONNECT_ATTEMPTS {
            tokio::time::sleep(reconnect_backoff_base() * attempt).await;
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("reconnect failed for an unknown reason")))
}

/// After a failed reconnect attempt, respawn the background bridge when the
/// failure proves the endpoint itself is gone (not merely glitching) and a
/// respawn hook is available. This includes the last attempt of a burst: the
/// session outlives the burst, so the next request finds a live bridge.
/// Best-effort: a failed respawn only logs.
async fn respawn_after_reconnect_failure(
    err: &anyhow::Error,
    respawn: Option<&mut BridgeRespawnFn>,
    transport: &str,
) {
    let Some(respawn) = respawn else {
        return;
    };
    if !reconnect_failure_wants_respawn(err) {
        return;
    }
    tracing::warn!(
        transport,
        "Bridge endpoint is gone; respawning background bridge before \
         the next reconnect attempt"
    );
    if let Err(spawn_err) = respawn().await {
        tracing::warn!(
            transport,
            error = %spawn_err,
            "Background bridge respawn failed"
        );
    }
}

/// Resolve the frontend handshake deadline: in debug builds the test override
/// `AHMA_FRONTEND_HANDSHAKE_DEADLINE_SECS` if set (SPEC R-CFG9; release builds
/// never read it), otherwise
/// [`FRONTEND_HANDSHAKE_DEADLINE_SECS`](ahma_common::timeouts::FRONTEND_HANDSHAKE_DEADLINE_SECS). A value of `0` disables the deadline
/// (returns `None`).
fn frontend_handshake_deadline() -> Option<Duration> {
    let secs = cfg!(debug_assertions)
        .then(|| std::env::var("AHMA_FRONTEND_HANDSHAKE_DEADLINE_SECS").ok())
        .flatten()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(ahma_common::timeouts::FRONTEND_HANDSHAKE_DEADLINE_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Base backoff between reconnect attempts (multiplied by the attempt number).
/// Debug builds let tests override it with `AHMA_RECONNECT_BACKOFF_MS`, so the
/// retry path stays fast and deterministic; release builds never read it (R-CFG9).
fn reconnect_backoff_base() -> Duration {
    let ms = cfg!(debug_assertions)
        .then(|| std::env::var("AHMA_RECONNECT_BACKOFF_MS").ok())
        .flatten()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(500);
    Duration::from_millis(ms)
}

/// Run the stdio proxy against the hub's MCP endpoint on its local socket.
///
/// Returns `Ok(true)` when the bridge successfully responded to at least one
/// message (normal session end).  Returns `Ok(false)` or `Err` when the bridge
/// closed the connection before sending any response back to the client, which
/// typically indicates a stale or incompatible bridge hub.
pub async fn run_proxy_client(
    socket_path: &str,
    respawn_bridge: Option<BridgeRespawnFn>,
) -> Result<bool> {
    run_proxy_client_with_options(socket_path, respawn_bridge, "").await
}

/// As [`run_proxy_client`], carrying this client's per-session options in the
/// MCP URL's query so the hub can apply them to this session's worker and no
/// other (SPEC R-HUB.4).
pub async fn run_proxy_client_with_options(
    socket_path: &str,
    respawn_bridge: Option<BridgeRespawnFn>,
    session_query: &str,
) -> Result<bool> {
    let handshake_deadline = frontend_handshake_deadline();
    let mcp_uri = append_session_query("http://localhost/mcp", session_query);
    tracing::info!(
        socket = socket_path,
        "Proxying stdio to the hub's local socket"
    );
    run_proxy_client_socket(socket_path, &mcp_uri, handshake_deadline, respawn_bridge).await
}

/// Append `?session_query` to `base` (trimming any trailing `/` first so the
/// query never lands after a doubled slash), or return `base` unchanged when
/// there is no query to carry.
fn append_session_query(base: &str, session_query: &str) -> String {
    if session_query.is_empty() {
        base.to_string()
    } else {
        format!("{}?{session_query}", base.trim_end_matches('/'))
    }
}

/// Detects a specific client name for modern MCP clients that probe via `server/discover`.
fn detect_modern_client_name() -> &'static str {
    // 1. Check known environment variables passed by Antigravity IDE
    if std::env::var("ANTIGRAVITY_EDITOR_APP_ROOT").is_ok()
        || std::env::var("__CFBundleIdentifier").as_deref() == Ok("com.google.antigravity-ide")
        || std::env::var("ANTIGRAVITY_AGENT").is_ok()
        || std::env::var("VSCODE_IPC_HOOK")
            .map(|s| s.contains("Antigravity"))
            .unwrap_or(false)
    {
        return "Antigravity IDE";
    }

    // 2. Check known environment variables for Antigravity CLI (agy)
    if std::env::var("GEMINI_CLI").is_ok() || std::env::var("ANTIGRAVITY_CLI").is_ok() {
        return "agy";
    }

    // 3. Inspect parent process on Unix
    #[cfg(unix)]
    {
        let ppid = unsafe { libc::getppid() };
        if ppid > 1 {
            use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
            let mut sys = System::new();
            sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing().with_cmd(UpdateKind::OnlyIfNotSet),
            );
            let mut cur_pid = Some(Pid::from_u32(ppid as u32));
            for _ in 0..3 {
                let Some(pid) = cur_pid else { break };
                let Some(proc) = sys.process(pid) else { break };
                let name = proc.name().to_string_lossy().to_lowercase();
                let cmd: String = proc
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase();
                if name == "agy" || cmd.contains("agy") || cmd.contains("antigravity-cli") {
                    return "agy";
                }
                if name.contains("antigravity")
                    || cmd.contains("antigravity-ide")
                    || cmd.contains("com.google.antigravity-ide")
                {
                    return "Antigravity IDE";
                }
                cur_pid = proc.parent();
            }
        }
    }

    "Antigravity"
}

/// One client field of a `server/discover` probe. The 2026-07-28 protocol puts
/// it in `_meta` under a namespaced key (`io.modelcontextprotocol/clientInfo`,
/// as Claude Code sends it); the plain `_meta` key and a top-level `params`
/// field are accepted too (SPEC R-HUB.4.1).
fn discover_client_field<'a>(
    val: &'a serde_json::Value,
    meta_key: &str,
    params_key: &str,
) -> Option<&'a serde_json::Value> {
    let params = val.get("params")?;
    let meta = params.get("_meta");
    meta.and_then(|m| m.get(format!("io.modelcontextprotocol/{meta_key}")))
        .or_else(|| meta.and_then(|m| m.get(meta_key)))
        .or_else(|| params.get(params_key))
}

/// Synthesizes a standard `initialize` JSON-RPC message from a `server/discover` probe.
fn synthesize_initialize_request(val: &serde_json::Value) -> serde_json::Value {
    let client_info = discover_client_field(val, "clientInfo", "clientInfo")
        .cloned()
        .unwrap_or_else(|| {
            serde_json::json!({
                "name": detect_modern_client_name(),
                "version": "1.0.0"
            })
        });

    let client_capabilities = discover_client_field(val, "clientCapabilities", "capabilities")
        .cloned()
        .unwrap_or_else(|| {
            serde_json::json!({
                "roots": { "listChanged": true }
            })
        });

    let mut initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": INITIALIZE_METHOD,
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION_2025_11_25,
            "capabilities": client_capabilities,
            "clientInfo": client_info
        }
    });
    // SPEC R-HUB.4.3: this client can return roots only with its first tool
    // call, so the bridge must not ask for them at handshake time.
    modern_input::mark_defers_roots(&mut initialize);
    initialize
}

/// Answer a client request on stdio with a result built here, not by the
/// bridge (`server/discover`, an `InputRequiredResult`).
async fn answer_on_stdio<S>(stdio: &mut S, id: serde_json::Value, result: serde_json::Value) -> bool
where
    S: Transport<RoleServer>,
    S::Error: std::fmt::Debug,
{
    let req_id: RequestId = serde_json::from_value(id).unwrap_or(RequestId::Number(1));
    let message = TxJsonRpcMessage::<RoleServer>::response(
        ServerResult::CustomResult(CustomResult(result)),
        req_id,
    );
    match stdio.send(message).await {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(error = ?e, "Failed to answer a client request on stdio");
            false
        }
    }
}

/// Write a message built here (a listen acknowledgement or completion) to
/// stdio. Best-effort: a client that went away is noticed at its EOF.
async fn send_value_on_stdio<S>(stdio: &mut S, message: serde_json::Value)
where
    S: Transport<RoleServer>,
    S::Error: std::fmt::Debug,
{
    match serde_json::from_value::<TxJsonRpcMessage<RoleServer>>(message) {
        Ok(message) => {
            if let Err(e) = stdio.send(message).await {
                tracing::debug!(error = ?e, "Writing to stdio failed (non-fatal)");
            }
        }
        Err(e) => tracing::error!(error = %e, "A message for stdio does not deserialize"),
    }
}

/// Send what a [`Flush`] holds to the bridge: answers and notifications, then
/// the calls they gated. A call that fails to forward is answered on stdio
/// with the bridge's own error when there is one (SPEC R-HUB.4.3).
async fn deliver_modern_flush<S, C>(
    flush: Flush,
    conn: &mut BridgeConnection<C>,
    stdio: &mut S,
    transport: &str,
) where
    S: Transport<RoleServer>,
    C: Transport<RoleClient>,
    S::Error: std::fmt::Debug,
    C::Error: std::fmt::Debug + std::fmt::Display,
{
    for message in flush.to_bridge.into_iter().chain(flush.calls) {
        let id = message.get("id").cloned();
        let is_call = message.get("method").and_then(|m| m.as_str()) == Some(TOOLS_CALL_METHOD);
        let typed: TxJsonRpcMessage<RoleClient> = match serde_json::from_value(message) {
            Ok(typed) => typed,
            Err(e) => {
                tracing::error!(transport, error = %e, "A message for the bridge does not deserialize");
                continue;
            }
        };
        if let Err(e) = conn.client.send(typed).await {
            tracing::warn!(transport, error = %e, "Failed to forward a modern-session message to the bridge");
            if is_call && let Some(id) = id {
                let recovered = recover_bridge_jsonrpc_error(&format!("{e}"));
                relay_forward_failure_to_client(stdio, id, recovered).await;
            }
            continue;
        }
        conn.forwarded_any = true;
    }
}

/// Proxy stdio to the MCP endpoint on `socket_path` — an `AF_UNIX` socket on
/// every OS (SPEC R-HUB.2), so the reconnect and respawn below that keep a
/// session alive across a hub restart (SPEC R-LIFECYCLE.3) work on Windows
/// too.
async fn run_proxy_client_socket(
    socket_path: &str,
    mcp_uri: &str,
    handshake_deadline: Option<Duration>,
    respawn_bridge: Option<BridgeRespawnFn>,
) -> Result<bool> {
    use ahma_http_mcp_client::local_socket_client::local_socket_transport;

    let client_transport = local_socket_transport(socket_path, mcp_uri);
    let stdio_transport = PatchedStdioTransport::new_stdio();

    let socket_path_owned = socket_path.to_string();
    let mcp_uri_owned = mcp_uri.to_string();
    let mut reconnect = move || Ok(local_socket_transport(&socket_path_owned, &mcp_uri_owned));
    let result = run_transport_proxy(
        stdio_transport,
        client_transport,
        "socket",
        handshake_deadline,
        &mut reconnect,
        respawn_bridge,
    )
    .await;
    if let Err(ref e) = result {
        tracing::error!(socket = socket_path, error = %e, "Proxy session ended with error");
    }
    result
}

async fn run_transport_proxy<S, C>(
    stdio: S,
    client: C,
    transport: &str,
    handshake_deadline: Option<Duration>,
    reconnect: &mut dyn FnMut() -> Result<C>,
    mut respawn_bridge: Option<BridgeRespawnFn>,
) -> Result<bool>
where
    S: Transport<RoleServer> + Send + 'static,
    C: Transport<RoleClient> + Send + 'static,
    S::Error: std::fmt::Debug + Send,
    // `Display` (not just `Debug`) is required: rmcp embeds the bridge's JSON
    // response body in the transport error's `Display`, and `Debug` escapes its
    // quotes into something no JSON parser will accept. See
    // `recover_bridge_jsonrpc_error`.
    C::Error: std::fmt::Debug + std::fmt::Display + Send,
{
    // Every write to stdout goes through the R-HUB.4.4 filter, which stays a
    // pass-through unless the client opens with `server/discover`.
    let mut stdio = modern_input::ModernStdio::new(stdio);
    // The `DiscoverResult` this connection was given, for a repeated probe.
    let mut discover_answer: Option<serde_json::Value> = None;
    let mut handshake = CachedHandshake::default();
    // The bridge transport and the session state that describes it. A
    // reconnect swaps the transport only through `BridgeConnection::adopt`.
    let mut conn = BridgeConnection::new(client);
    // Set once a 2026-07-28 client opened with `server/discover` (SPEC
    // R-HUB.4.2, R-HUB.4.3).
    let mut modern: Option<ModernSession> = None;

    // Handshake deadline: if the client never sends its first message (the
    // `initialize` handshake) within this window, the connection was spawned
    // and abandoned — exit so abandoned `serve stdio` spawns cannot accumulate.
    // Disarmed once the first message is forwarded; a live idle session is never
    // killed by this. A far-future sleep stands in for "no deadline".
    let deadline = handshake_deadline.unwrap_or(Duration::from_secs(u64::MAX / 2));
    let handshake_timer = tokio::time::sleep(deadline);
    tokio::pin!(handshake_timer);

    loop {
        let held_deadline = modern
            .as_ref()
            .and_then(ModernSession::held_deadline)
            .map(tokio::time::Instant::from_std);
        tokio::select! {
            biased;
            _ = &mut handshake_timer, if !conn.forwarded_any => {
                tracing::warn!(
                    transport,
                    ?deadline,
                    "Proxy exiting: no MCP handshake within deadline (connection spawned but abandoned)"
                );
                // Exit the process directly rather than returning up the stack:
                // the stdin reader thread (tokio::io::stdin) is still blocked in a
                // read() on the held-open pipe, so a normal return would hang on
                // runtime shutdown waiting for that thread. The frontend proxy
                // holds no state worth draining.
                std::process::exit(0);
            }
            stdio_msg = stdio.receive() => {
                let Some(msg) = stdio_msg else {
                    tracing::info!(transport, "Proxy exiting: stdio EOF (Cursor client disconnected)");
                    break;
                };
                let val = serde_json::to_value(&msg).unwrap();
                let request_id = val.get("id").filter(|id| !id.is_null()).cloned();
                if val.get("method").and_then(|m| m.as_str()) == Some(SERVER_DISCOVER_METHOD) {
                    tracing::info!(
                        transport,
                        "Received server/discover probe from modern MCP client (2026-07-28); establishing first-class modern stateless session"
                    );
                    let discover_id = request_id.clone().unwrap_or_else(|| serde_json::json!(1));
                    // The session this connection opened already exists; a
                    // second `initialize` would replace it.
                    if let Some(cached) = &discover_answer {
                        answer_on_stdio(&mut stdio, discover_id, cached.clone()).await;
                        continue;
                    }

                    let synth_init = synthesize_initialize_request(&val);
                    modern = Some(ModernSession::new(
                        synth_init["params"]["capabilities"].clone(),
                    ));

                    handshake.observe_client_to_bridge(&synth_init);

                    let init_msg: TxJsonRpcMessage<RoleClient> = match serde_json::from_value(synth_init) {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::error!(transport, error = %e, "Failed to serialize synthesized initialize");
                            continue;
                        }
                    };

                    if let Err(e) = conn.client.send(init_msg).await {
                        tracing::error!(transport, error = %e, "Failed to send synthesized initialize to bridge");
                        continue;
                    }

                    let init_resp = match conn.client.receive().await {
                        Some(resp) => resp,
                        None => {
                            tracing::error!(transport, "Bridge closed connection while waiting for initialize response");
                            break;
                        }
                    };

                    let init_resp_val = serde_json::to_value(&init_resp).unwrap_or_default();
                    let init_result = init_resp_val.get("result").cloned().unwrap_or_default();

                    let bridge_capabilities = init_result.get("capabilities").cloned().unwrap_or_else(|| {
                        serde_json::json!({
                            "tools": { "listChanged": true }
                        })
                    });
                    let bridge_server_info = init_result.get("serverInfo").cloned().unwrap_or_else(|| {
                        serde_json::json!({
                            "name": env!("CARGO_PKG_NAME"),
                            "version": env!("CARGO_PKG_VERSION")
                        })
                    });
                    let bridge_instructions = init_result
                        .get("instructions")
                        .and_then(|i| i.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| SERVER_INSTRUCTIONS.to_string());

                    let discover_result = ahma_common::mcp_protocol::discover_result(
                        bridge_capabilities,
                        &bridge_instructions,
                        bridge_server_info,
                    );
                    discover_answer = Some(discover_result.clone());
                    // SPEC R-HUB.4.4: from here on, what reaches stdout is filtered.
                    stdio.go_modern();

                    if !answer_on_stdio(&mut stdio, discover_id, discover_result).await {
                        break;
                    }

                    let notif_init = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": INITIALIZED_METHOD,
                        "params": {}
                    });
                    handshake.observe_client_to_bridge(&notif_init);
                    if let Ok(notif_msg) = serde_json::from_value::<TxJsonRpcMessage<RoleClient>>(notif_init) {
                        let _ = conn.client.send(notif_msg).await;
                    }

                    conn.consecutive_forward_failures = 0;
                    conn.forwarded_any = true;
                    conn.bridge_responded = true;
                    continue;
                }

                // SPEC R-HUB.4.4: a listen stream is the proxy's to keep. It is
                // acknowledged, held open, and completed when the client
                // cancels it; the bridge never sees it.
                if val.get("method").and_then(|m| m.as_str()) == Some(SUBSCRIPTIONS_LISTEN_METHOD) {
                    tracing::debug!(transport, "Opening a subscriptions/listen stream");
                    match stdio.listens() {
                        Some(listens) => {
                            let ack = listens.open(&val);
                            send_value_on_stdio(&mut stdio, ack).await;
                        }
                        // A legacy client has no such method; answer as before.
                        None => {
                            if let Some(id) = request_id {
                                answer_on_stdio(&mut stdio, id, serde_json::json!({})).await;
                            }
                        }
                    }
                    continue;
                }
                if let Some(done) = stdio.listens().and_then(|listens| listens.close(&val)) {
                    send_value_on_stdio(&mut stdio, done).await;
                    continue;
                }
                let (msg, val) = match modern.as_mut() {
                    Some(session)
                        if val.get("method").and_then(|m| m.as_str())
                            == Some(TOOLS_CALL_METHOD) =>
                    {
                        match session.on_tools_call(val, std::time::Instant::now()) {
                            CallStep::AskClient(result) => {
                                if let Some(id) = request_id {
                                    answer_on_stdio(&mut stdio, id, result).await;
                                }
                                continue;
                            }
                            CallStep::Held => {
                                let flush = session.flush();
                                deliver_modern_flush(flush, &mut conn, &mut stdio, transport).await;
                                if handshake.roots_response.is_none() {
                                    handshake.roots_response = session.roots_response();
                                }
                                continue;
                            }
                            CallStep::Forward(call) => match session.prompts.on_call(call) {
                                CallRoute::Forward(call) => match serde_json::from_value(call.clone()) {
                                    Ok(typed) => (typed, call),
                                    Err(e) => {
                                        tracing::error!(transport, error = %e, "A stripped tools/call does not deserialize");
                                        continue;
                                    }
                                },
                                // SPEC R-HUB.4.5: an answered prompt, or a
                                // prompt riding this call.
                                CallRoute::Answer { to_bridge, to_client } => {
                                    let flush = Flush { to_bridge, calls: Vec::new() };
                                    deliver_modern_flush(flush, &mut conn, &mut stdio, transport).await;
                                    for message in to_client {
                                        send_value_on_stdio(&mut stdio, message).await;
                                    }
                                    continue;
                                }
                            },
                        }
                    }
                    // A cancel for a call the bridge knows by its original id.
                    Some(session)
                        if val.get("method").and_then(|m| m.as_str())
                            == Some("notifications/cancelled") =>
                    {
                        let val = modern_input::legacy_request(
                            session.prompts.rewrite_client_cancel(val),
                        );
                        match serde_json::from_value(val.clone()) {
                            Ok(typed) => (typed, val),
                            Err(_) => continue,
                        }
                    }
                    // SPEC R-HUB.4.6: everything else a modern client sends
                    // reaches the bridge without its 2026-07-28 envelope.
                    Some(_) => {
                        let val = modern_input::legacy_request(val);
                        match serde_json::from_value(val.clone()) {
                            Ok(typed) => (typed, val),
                            Err(e) => {
                                tracing::error!(transport, error = %e, "A modern request does not deserialize without its envelope");
                                continue;
                            }
                        }
                    }
                    None => (msg, val),
                };
                handshake.observe_client_to_bridge(&val);
                if conn.disconnected {
                    match reconnect_with_retries(
                        reconnect,
                        &handshake,
                        transport,
                        respawn_bridge.as_mut(),
                    )
                    .await
                    {
                        Ok(fresh) => {
                            tracing::warn!(
                                transport,
                                "Proxy reconnected to a live bridge; session resumed"
                            );
                            conn.adopt(
                                fresh,
                                ReconnectCause::BridgeUnreachable,
                                &mut stdio,
                                &mut handshake,
                            )
                            .await;
                            // The replay just delivered the client's
                            // `notifications/initialized` (cached above); its
                            // own copy must not follow it onto the fresh session.
                            if val.get("method").and_then(|m| m.as_str())
                                == Some(INITIALIZED_METHOD)
                            {
                                continue;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                transport,
                                error = %e,
                                "Bridge still unreachable; answering the request with an error"
                            );
                            conn.mark_unreachable(&mut stdio, &e).await;
                            if let Some(id) = request_id {
                                relay_forward_failure_to_client(
                                    &mut stdio,
                                    id,
                                    Some(bridge_unreachable_error()),
                                )
                                .await;
                            }
                            continue;
                        }
                    }
                }
                // `val` is kept alive past the send: the gone-endpoint recovery
                // path below resends this exact message on a fresh transport.
                // The typed message itself is forwarded — the client-to-server
                // message type is identical on both transports, so no
                // round-trip back through `serde_json::from_value` is needed.
                // No inner retry here: an rmcp transport `send` error means the
                // worker behind this transport is gone, not that the channel is
                // momentarily busy (`send` awaits capacity). Re-sending the same
                // message down the same dead transport only multiplies latency
                // and inflates the failure count below. Recovery is the
                // reconnect path, which rebuilds the transport and replays the
                // handshake.
                if let Err(e) = conn.client.send(msg).await {
                    // A single forward failure must NOT tear down the whole
                    // multiplexed session. Only the per-request timeout is
                    // returned by the bridge as an ordinary HTTP 200 response;
                    // *every other* non-2xx answer — the sandbox-initializing
                    // 409, the handshake-timeout 504, sandbox-configuration
                    // 403s — is folded by rmcp into a transport `send` error
                    // with the bridge's JSON body embedded in its `Display`. So
                    // classify first: relay the bridge's own error when there is
                    // one, and keep serving. Only a sustained run of failures
                    // (the transport is genuinely dead) exits the proxy.
                    let rendered = format!("{e}");
                    let rendered_debug = format!("{e:?}");
                    let failure = classify_forward_failure(&rendered, &rendered_debug);

                    // The bridge auto-spawns with `--idle-timeout`: at zero
                    // active sessions it unlinks its socket and exits, leaving a
                    // frontend proxy that outlives it (routine with clients that
                    // abandon the transport). Those failures cannot improve by
                    // being retried, so recover on the FIRST one instead of
                    // answering two requests with errors first — and because the
                    // request never reached the bridge, resending it on the
                    // fresh transport is a retry, not a duplicate execution.
                    //
                    // The same holds when the hub's session is not this
                    // client's (SPEC R-HUB.5): a successor that does not know
                    // it answers 404, and one that lost its roots answers 409.
                    // The hub ran nothing, and only the proxy's replay carries
                    // the roots, so rebuild on the first failure and resend.
                    //
                    // Handshake messages are excluded: `replay_handshake` already
                    // resends those, so a resend would duplicate them.
                    let session_gone = failure_rebuilds_session(&failure, &handshake);
                    if (failure == ForwardFailure::EndpointGone || session_gone)
                        && handshake.init_request.is_some()
                        && !message_is_replayed_by_handshake(&val)
                    {
                        tracing::warn!(
                            transport,
                            error = %rendered,
                            session_gone,
                            "Bridge endpoint or its session is gone; reconnecting immediately \
                             and resending the request rather than waiting for repeated failures"
                        );
                        match reconnect_with_retries(
                            reconnect,
                            &handshake,
                            transport,
                            respawn_bridge.as_mut(),
                        )
                        .await
                        {
                            Ok(mut fresh) => {
                                // Resend before adopting, so the disclosure can
                                // say whether the request reached the fresh
                                // bridge; the error relay still precedes it.
                                let resend = resend_pending(&mut fresh, &val, transport).await;
                                let resent = resend.is_ok();
                                // Once per request: a refusal on the rebuilt
                                // session is relayed, never rebuilt again.
                                if let (Err(recovered), Some(id)) = (resend, request_id.clone()) {
                                    relay_forward_failure_to_client(&mut stdio, id, recovered)
                                        .await;
                                }
                                let cause = if session_gone {
                                    ReconnectCause::SessionGone { resent }
                                } else {
                                    ReconnectCause::EndpointGone { resent }
                                };
                                conn.adopt(fresh, cause, &mut stdio, &mut handshake).await;
                                if !resent {
                                    // The failed resend is the fresh transport's
                                    // first forward failure.
                                    conn.consecutive_forward_failures += 1;
                                }
                                continue;
                            }
                            Err(reconnect_err) => {
                                tracing::warn!(
                                    transport,
                                    error = %reconnect_err,
                                    "Immediate reconnect after a gone bridge endpoint failed; \
                                     falling back to the failure-count path"
                                );
                            }
                        }
                    }

                    conn.consecutive_forward_failures += 1;
                    tracing::warn!(
                        transport,
                        error = %rendered,
                        failures = conn.consecutive_forward_failures,
                        "Failed to forward request to bridge; relaying error to client, session preserved"
                    );
                    if let Some(id) = request_id {
                        // Relay the bridge's own actionable error when it sent
                        // one; the generic fallback is for a dead socket, where
                        // there is genuinely nothing better to say.
                        let recovered = match failure {
                            ForwardFailure::BridgeError(error) => Some(error),
                            _ => None,
                        };
                        relay_forward_failure_to_client(&mut stdio, id, recovered).await;
                    }
                    if conn.consecutive_forward_failures >= MAX_CONSECUTIVE_FORWARD_FAILURES {
                        // Only worth reconnecting once a real handshake has been
                        // observed — otherwise there is no session to preserve
                        // (mirrors the handshake-deadline exit above: an
                        // unhandshaked connection is abandoned, not resumed).
                        if handshake.init_request.is_some() {
                            match reconnect_with_retries(
                                reconnect,
                                &handshake,
                                transport,
                                respawn_bridge.as_mut(),
                            )
                            .await
                            {
                                Ok(fresh) => {
                                    tracing::warn!(
                                        transport,
                                        "Proxy reconnected to bridge after transport failure; \
                                         session resumed transparently"
                                    );
                                    // The request that tripped the failure was
                                    // answered with an error above; the
                                    // disclosure names it so the client can
                                    // re-issue.
                                    conn.adopt(
                                        fresh,
                                        ReconnectCause::TransportFailure,
                                        &mut stdio,
                                        &mut handshake,
                                    )
                                    .await;
                                    continue;
                                }
                                Err(e) => {
                                    tracing::error!(
                                        transport,
                                        failures = conn.consecutive_forward_failures,
                                        error = %e,
                                        "Bridge transport failed repeatedly and reconnect also \
                                         failed; staying attached and retrying on the next request"
                                    );
                                    conn.mark_unreachable(&mut stdio, &e).await;
                                    continue;
                                }
                            }
                        }
                        tracing::error!(
                            transport,
                            failures = conn.consecutive_forward_failures,
                            "Proxy exiting: bridge transport failed repeatedly (genuinely dead)"
                        );
                        break;
                    }
                    continue;
                }
                conn.consecutive_forward_failures = 0;
                conn.forwarded_any = true;
            }
            client_msg = conn.client.receive(), if !conn.disconnected => {
                let Some(msg) = client_msg else {
                    if handshake.init_request.is_some() {
                        // The bridge exited on idle, crashed, or handed over to
                        // a newer version. The editor is still attached, so stay
                        // attached and reconnect when the next request arrives.
                        tracing::warn!(
                            transport,
                            "Bridge connection closed; will reconnect on the next request"
                        );
                        conn.disconnected = true;
                        continue;
                    }
                    tracing::info!(
                        transport,
                        "Proxy exiting: bridge connection closed"
                    );
                    break;
                };
                let mut val = serde_json::to_value(&msg).unwrap();
                // SPEC R-HUB.4.2: a 2026-07-28 client's stdout carries no
                // server request. Each is answered here; a `roots/list` waits
                // for the roots the client's first tool call brings.
                // SPEC R-HUB.4.5: a prompt becomes a running call's
                // `input_required`, or waits for the next call.
                if let Some(session) = modern.as_mut()
                    && modern_input::is_server_request(&val)
                    && val.get("method").and_then(|m| m.as_str()) == Some("elicitation/create")
                {
                    for message in session.prompts.on_elicitation(&val) {
                        send_value_on_stdio(&mut stdio, message).await;
                    }
                    conn.bridge_responded = true;
                    continue;
                }
                // A call's result goes to the id the client now awaits.
                if let Some(session) = modern.as_mut()
                    && val.get("method").is_none()
                    && val.get("id").is_some()
                {
                    for message in session.prompts.on_bridge_response(val) {
                        send_value_on_stdio(&mut stdio, message).await;
                    }
                    conn.bridge_responded = true;
                    continue;
                }
                if let Some(session) = modern.as_mut()
                    && val.get("method").and_then(|m| m.as_str()) == Some("notifications/cancelled")
                {
                    session.prompts.on_bridge_cancelled(&val);
                }
                if let Some(session) = modern.as_mut()
                    && modern_input::is_server_request(&val)
                {
                    let answer = session.on_server_request(&val);
                    let mut flush = session.flush();
                    flush.to_bridge.splice(0..0, answer);
                    deliver_modern_flush(flush, &mut conn, &mut stdio, transport).await;
                    if handshake.roots_response.is_none() {
                        handshake.roots_response = session.roots_response();
                    }
                    continue;
                }
                handshake.observe_bridge_to_client(&val);
                // The server cannot know its transport was rebuilt; the proxy
                // owns the heartbeat's `reconnects` field (#485). Only a
                // heartbeat the overlay actually mutated is rebuilt from JSON;
                // every other message forwards as the typed value it arrived as
                // (the server-to-client message type is identical on both
                // transports).
                let tx_msg = if overlay_heartbeat_reconnects(&mut val, conn.reconnects) {
                    serde_json::from_value(val).unwrap()
                } else {
                    msg
                };
                if let Err(e) = stdio.send(tx_msg).await {
                    tracing::error!(
                        transport,
                        error = ?e,
                        "Proxy exiting: failed to forward message to stdio"
                    );
                    break;
                }
                conn.bridge_responded = true;
            }
            _ = tokio::time::sleep_until(held_deadline.unwrap_or_else(tokio::time::Instant::now)),
                if held_deadline.is_some() =>
            {
                // The bridge never asked for the roots it was told about: its
                // scope did not come from the client. It decides what the
                // held calls get.
                if let Some(session) = modern.as_mut() {
                    tracing::info!(transport, "Bridge did not ask for roots; releasing held tool calls");
                    let calls = session.release_held();
                    let flush = Flush { to_bridge: Vec::new(), calls };
                    deliver_modern_flush(flush, &mut conn, &mut stdio, transport).await;
                }
            }
        }
    }
    // Notify the bridge that this session is terminating.  For HTTP-backed transports
    // this sends DELETE /mcp so the bridge decrements active_sessions immediately rather
    // than waiting for the 5-second SSE-drop grace period.
    //
    // Only attempt this when at least one message was forwarded: otherwise no session was
    // ever established and `client.close()` would block indefinitely (the rmcp worker is
    // parked before its cancellation-aware main loop).  Even then we bound the call with a
    // timeout as a backstop in case the worker is mid-handshake; the bridge's SSE-drop
    // grace period guarantees eventual cleanup regardless.
    if conn.forwarded_any {
        match tokio::time::timeout(Duration::from_secs(6), conn.client.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(transport, error = ?e, "Proxy close error (non-fatal)")
            }
            Err(_) => {
                tracing::debug!(transport, "Proxy close timed out (non-fatal)")
            }
        }
    }
    Ok(conn.bridge_responded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use rmcp::service::{RoleClient, RxJsonRpcMessage};
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Serializes access to the process-wide `AHMA_FRONTEND_HANDSHAKE_DEADLINE_SECS`
    /// env var so the deadline-resolution tests do not race each other.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());
    const DEADLINE_ENV_KEY: &str = "AHMA_FRONTEND_HANDSHAKE_DEADLINE_SECS";

    /// Run `frontend_handshake_deadline()` with the env var forced to `value`
    /// (or unset when `None`), restoring the prior value afterward. Returns the
    /// resolved deadline so assertions run *outside* the lock (avoids poisoning
    /// the mutex on assertion failure).
    fn deadline_with_env(value: Option<&str>) -> Option<Duration> {
        let _guard = ENV_MUTEX.lock();
        let saved = std::env::var_os(DEADLINE_ENV_KEY);
        // SAFETY: test-only; ENV_MUTEX serializes env access in this module.
        unsafe {
            match value {
                Some(v) => std::env::set_var(DEADLINE_ENV_KEY, v),
                None => std::env::remove_var(DEADLINE_ENV_KEY),
            }
        }
        let resolved = frontend_handshake_deadline();
        // SAFETY: test-only; ENV_MUTEX held for the duration of this function.
        unsafe {
            match saved {
                Some(v) => std::env::set_var(DEADLINE_ENV_KEY, v),
                None => std::env::remove_var(DEADLINE_ENV_KEY),
            }
        }
        resolved
    }

    /// Placeholder reconnect closure for tests where no handshake is ever
    /// observed, so `run_transport_proxy` must never invoke it at all.
    fn no_reconnect() -> Result<MockClient> {
        panic!("must not attempt reconnect in this test")
    }

    fn client_request(id: i64) -> RxJsonRpcMessage<RoleServer> {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": "run_terminal_command", "arguments": {}}
        }))
        .expect("valid client request")
    }

    /// A bridge→client (server→client) response message the proxy should forward
    /// back to stdio.
    fn bridge_response(id: i64) -> RxJsonRpcMessage<RoleClient> {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {}
        }))
        .expect("valid bridge response")
    }

    fn bridge_initialize_response(id: i64) -> RxJsonRpcMessage<RoleClient> {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2025-11-25",
                "capabilities": {
                    "tools": { "listChanged": true }
                },
                "serverInfo": {
                    "name": "ahma",
                    "version": "0.1.0"
                },
                "instructions": "Ahma tools"
            }
        }))
        .expect("valid bridge initialize response")
    }

    /// The client's `initialize` request — the first message of any real
    /// session, and the one the reconnect path replays from cache.
    fn client_initialize(id: i64) -> RxJsonRpcMessage<RoleServer> {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}
        }))
        .expect("valid initialize request")
    }

    fn client_discover_request(id: i64) -> RxJsonRpcMessage<RoleServer> {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "server/discover",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28"
                }
            }
        }))
        .expect("valid discover request")
    }

    /// The client's `notifications/initialized` — sent once, no response expected.
    fn client_notifications_initialized() -> RxJsonRpcMessage<RoleServer> {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .expect("valid notifications/initialized")
    }

    /// A bridge→client `roots/list` request (the bridge asking for the sandbox scope).
    fn bridge_roots_list_request(id: i64) -> RxJsonRpcMessage<RoleClient> {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "roots/list"
        }))
        .expect("valid roots/list request")
    }

    /// The client's answer to a `roots/list` request.
    fn client_roots_list_response(id: i64) -> RxJsonRpcMessage<RoleServer> {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"roots": [{"uri": "file:///workspace", "name": "workspace"}]}
        }))
        .expect("valid roots/list response")
    }

    /// Serializes access to the process-wide `AHMA_RECONNECT_BACKOFF_MS` env var
    /// across the (async) duration of a `run_transport_proxy` call, so reconnect
    /// tests stay fast and deterministic without racing each other. A
    /// `tokio::sync::Mutex` is required (not `ENV_MUTEX`) because the guard must
    /// be held across `.await`.
    static BACKOFF_ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    const BACKOFF_ENV_KEY: &str = "AHMA_RECONNECT_BACKOFF_MS";

    /// Run `body` with `AHMA_RECONNECT_BACKOFF_MS=0` for its whole (async)
    /// duration, restoring the prior value afterward.
    async fn with_zero_backoff<F, T>(body: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let _guard = BACKOFF_ENV_MUTEX.lock().await;
        let saved = std::env::var_os(BACKOFF_ENV_KEY);
        // SAFETY: test-only; BACKOFF_ENV_MUTEX serializes env access for the
        // duration of this async block, including across the awaited body.
        unsafe {
            std::env::set_var(BACKOFF_ENV_KEY, "0");
        }
        let result = body.await;
        // SAFETY: see above.
        unsafe {
            match saved {
                Some(v) => std::env::set_var(BACKOFF_ENV_KEY, v),
                None => std::env::remove_var(BACKOFF_ENV_KEY),
            }
        }
        result
    }

    /// Stdio side: hands the proxy a fixed queue of client requests, then EOF
    /// (`None`), and records every server→client message the proxy sends back.
    struct MockStdio {
        inbound: VecDeque<RxJsonRpcMessage<RoleServer>>,
        sent: Arc<Mutex<Vec<serde_json::Value>>>,
        /// Optional delay applied *before* yielding EOF once `inbound` is drained.
        /// Used to deterministically order a ready bridge→client message ahead of
        /// stdio EOF inside the proxy's `tokio::select!` loop.
        eof_delay: Option<Duration>,
        /// When true, `send()` always fails (simulates a broken pipe writing back
        /// to the client), instead of recording the message.
        fail_send: bool,
        /// Optional delay before yielding every inbound message after the first,
        /// so bridge-side events can be ordered between client requests.
        inbound_delay: Option<Duration>,
        yielded: usize,
    }
    impl MockStdio {
        fn new(
            inbound: VecDeque<RxJsonRpcMessage<RoleServer>>,
            sent: Arc<Mutex<Vec<serde_json::Value>>>,
        ) -> Self {
            Self {
                inbound,
                sent,
                eof_delay: None,
                fail_send: false,
                inbound_delay: None,
                yielded: 0,
            }
        }
    }
    impl Transport<RoleServer> for MockStdio {
        type Error = std::io::Error;
        // The trait requires `send` to return a `'static` future, so it cannot
        // borrow `&mut self`; clone the shared recorder into an owned future.
        fn send(
            &mut self,
            item: TxJsonRpcMessage<RoleServer>,
        ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
            let sent = self.sent.clone();
            let fail_send = self.fail_send;
            async move {
                if fail_send {
                    return Err(std::io::Error::other("simulated stdio write failure"));
                }
                sent.lock().push(serde_json::to_value(item).unwrap());
                Ok(())
            }
        }
        async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
            if let Some(delay) = self.inbound_delay
                && self.yielded > 0
                && !self.inbound.is_empty()
            {
                tokio::time::sleep(delay).await;
            }
            if let Some(msg) = self.inbound.pop_front() {
                self.yielded += 1;
                return Some(msg);
            }
            if let Some(delay) = self.eof_delay {
                tokio::time::sleep(delay).await;
            }
            None
        }
        async fn close(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// Controls what `MockClient::close()` does when invoked by the proxy's
    /// teardown path.
    enum CloseBehavior {
        Ok,
        Err,
        /// Sleeps longer than the proxy's internal close timeout before
        /// succeeding, so the *timeout* branch (not the mock) fires.
        Hang(Duration),
    }

    /// The default text of a simulated bridge send failure: a transient error
    /// on a *live* endpoint (no HTTP body, no io "gone" signature), which the
    /// proxy must handle via the failure-count path.
    const TRANSIENT_SEND_ERROR: &str = "simulated bridge forward failure";

    /// A send failure that carries the signature of a bridge whose socket was
    /// unlinked out from under it — what an idle-timed-out bridge produces.
    const GONE_ENDPOINT_SEND_ERROR: &str = "Client error: No such file or directory (os error 2)";

    /// Bridge side: fails the first `fail_first_n` sends, then succeeds (or,
    /// with `fail_sends_from`, succeeds first and fails from the Nth send
    /// onward). Delivers any queued `inbound` bridge→client messages once (then
    /// `receive` parks forever via `pending`, unless `receive_none_after` is set
    /// — see below). Records how many times `close()` is invoked so tests can
    /// assert teardown behaviour.
    struct MockClient {
        fail_first_n: usize,
        /// 1-indexed send number from which every send fails. `usize::MAX`
        /// (the default) disables this, leaving `fail_first_n` in charge.
        fail_sends_from: usize,
        /// Message of the `io::Error` a failing send returns. Drives the
        /// proxy's failure classification (transient vs gone endpoint vs a
        /// recoverable bridge JSON-RPC body).
        send_error_text: &'static str,
        attempts: Arc<AtomicUsize>,
        inbound: VecDeque<RxJsonRpcMessage<RoleClient>>,
        closed: Arc<AtomicUsize>,
        /// Once `inbound` is drained, `receive()` calls are counted; from the
        /// Nth call onward (1-indexed) it returns `None` (simulating the bridge
        /// closing the connection) instead of pending forever. `None` disables
        /// this (the default: always pend once drained).
        receive_none_after: Option<usize>,
        receive_call_count: Arc<AtomicUsize>,
        close_behavior: CloseBehavior,
        /// When set, every successfully-sent message is recorded here (used to
        /// verify *what* was sent during handshake replay, not just how many
        /// times `send()` was called).
        sent: Option<Arc<Mutex<Vec<serde_json::Value>>>>,
    }
    impl Transport<RoleClient> for MockClient {
        type Error = std::io::Error;
        fn send(
            &mut self,
            item: TxJsonRpcMessage<RoleClient>,
        ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
            let attempts = self.attempts.clone();
            let fail_first_n = self.fail_first_n;
            let fail_sends_from = self.fail_sends_from;
            let send_error_text = self.send_error_text;
            let sent = self.sent.clone();
            async move {
                let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= fail_first_n || n >= fail_sends_from {
                    Err(std::io::Error::other(send_error_text))
                } else {
                    if let Some(sent) = sent {
                        sent.lock().push(serde_json::to_value(item).unwrap());
                    }
                    Ok(())
                }
            }
        }
        async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
            if let Some(msg) = self.inbound.pop_front() {
                return Some(msg);
            }
            if let Some(threshold) = self.receive_none_after {
                let n = self.receive_call_count.fetch_add(1, Ordering::SeqCst) + 1;
                if n >= threshold {
                    return None;
                }
            }
            std::future::pending().await
        }
        async fn close(&mut self) -> Result<(), Self::Error> {
            self.closed.fetch_add(1, Ordering::SeqCst);
            match &self.close_behavior {
                CloseBehavior::Ok => Ok(()),
                CloseBehavior::Err => Err(std::io::Error::other("simulated bridge close failure")),
                CloseBehavior::Hang(d) => {
                    tokio::time::sleep(*d).await;
                    Ok(())
                }
            }
        }
    }

    struct TestState {
        sent: Arc<Mutex<Vec<serde_json::Value>>>,
        attempts: Arc<AtomicUsize>,
        closed: Arc<AtomicUsize>,
    }

    impl TestState {
        fn new() -> Self {
            Self {
                sent: Arc::new(Mutex::new(Vec::new())),
                attempts: Arc::new(AtomicUsize::new(0)),
                closed: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn stdio(&self, inbound: VecDeque<RxJsonRpcMessage<RoleServer>>) -> MockStdio {
            MockStdio::new(inbound, self.sent.clone())
        }

        fn client(&self, fail_first_n: usize) -> MockClient {
            self.client_failing_with(fail_first_n, TRANSIENT_SEND_ERROR)
        }

        /// Like [`Self::client`] but with a chosen send-error text, so tests can
        /// drive the proxy's failure classification (bridge JSON-RPC body,
        /// gone endpoint, transient).
        fn client_failing_with(
            &self,
            fail_first_n: usize,
            send_error_text: &'static str,
        ) -> MockClient {
            MockClient {
                fail_first_n,
                fail_sends_from: usize::MAX,
                send_error_text,
                attempts: self.attempts.clone(),
                inbound: VecDeque::new(),
                closed: self.closed.clone(),
                receive_none_after: None,
                receive_call_count: Arc::new(AtomicUsize::new(0)),
                sent: None,
                close_behavior: CloseBehavior::Ok,
            }
        }
    }

    #[tokio::test]
    async fn single_forward_failure_relays_error_and_keeps_session_alive() {
        // REGRESSION: one failed forward (e.g. a per-request timeout surfaced as a
        // transport error) must NOT tear the proxy down. The proxy relays a
        // JSON-RPC error for that request id and keeps serving the next request.
        let state = TestState::new();
        let stdio = state.stdio(VecDeque::from(vec![client_request(1), client_request(2)]));
        let client = state.client(1);
        let mut reconnect = || -> Result<MockClient> { Err(anyhow!("no reconnect in this test")) };

        let result = run_transport_proxy(stdio, client, "test", None, &mut reconnect, None).await;
        assert!(
            result.is_ok(),
            "proxy must survive a single forward failure"
        );
        // Both requests were attempted → the session survived the first failure.
        assert_eq!(state.attempts.load(Ordering::SeqCst), 2);
        let sent = state.sent.lock();
        assert_eq!(sent.len(), 1, "expected one relayed error, got {sent:?}");
        assert_eq!(sent[0]["id"], serde_json::json!(1));
        assert_eq!(sent[0]["error"]["code"], serde_json::json!(-32002));
    }

    #[tokio::test]
    async fn sustained_forward_failures_tear_down_the_session() {
        // A genuinely dead transport still exits — but only after a sustained run
        // of failures, not on the first one. No `initialize` was ever observed
        // (these are bare `tools/call` requests), so there is no handshake to
        // replay and the proxy must not attempt to reconnect at all.
        let state = TestState::new();
        let stdio = state.stdio(VecDeque::from(vec![
            client_request(1),
            client_request(2),
            client_request(3),
            client_request(4),
        ]));
        let client = state.client(usize::MAX);
        let mut reconnect = || -> Result<MockClient> {
            panic!("must not attempt reconnect without an observed handshake")
        };

        let result = run_transport_proxy(stdio, client, "test", None, &mut reconnect, None).await;
        assert!(result.is_ok());
        // Gives up after MAX_CONSECUTIVE_FORWARD_FAILURES; request 4 is never tried.
        assert_eq!(
            state.attempts.load(Ordering::SeqCst),
            MAX_CONSECUTIVE_FORWARD_FAILURES as usize
        );
        assert_eq!(
            state.sent.lock().len(),
            MAX_CONSECUTIVE_FORWARD_FAILURES as usize
        );
    }

    #[tokio::test]
    async fn reconnect_after_dead_transport_resumes_session_transparently() {
        // REGRESSION: once a handshake has been observed, a genuinely dead
        // bridge transport must not kill the proxy. It should transparently
        // reconnect (replaying the cached handshake) and keep serving the same
        // stdio session, invisibly to the downstream client.
        with_zero_backoff(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let stdio = MockStdio::new(
                VecDeque::from(vec![
                    client_initialize(0),
                    client_request(1),
                    client_request(2),
                    client_request(3),
                    client_request(4),
                ]),
                sent.clone(),
            );

            let dead_attempts = Arc::new(AtomicUsize::new(0));
            let dead_client = MockClient {
                fail_first_n: usize::MAX,
                fail_sends_from: usize::MAX,
                send_error_text: TRANSIENT_SEND_ERROR,
                attempts: dead_attempts.clone(),
                inbound: VecDeque::new(),
                closed: Arc::new(AtomicUsize::new(0)),
                receive_none_after: None,
                receive_call_count: Arc::new(AtomicUsize::new(0)),
                close_behavior: CloseBehavior::Ok,
                sent: None,
            };

            let fresh_attempts = Arc::new(AtomicUsize::new(0));
            let reconnect_calls = Arc::new(AtomicUsize::new(0));
            let reconnect_calls_inner = reconnect_calls.clone();
            let fresh_attempts_inner = fresh_attempts.clone();
            let mut reconnect = move || -> Result<MockClient> {
                reconnect_calls_inner.fetch_add(1, Ordering::SeqCst);
                Ok(MockClient {
                    fail_first_n: 0,
                    fail_sends_from: usize::MAX,
                    send_error_text: TRANSIENT_SEND_ERROR,
                    attempts: fresh_attempts_inner.clone(),
                    inbound: VecDeque::from(vec![bridge_response(0)]),
                    closed: Arc::new(AtomicUsize::new(0)),
                    receive_none_after: None,
                    receive_call_count: Arc::new(AtomicUsize::new(0)),
                    close_behavior: CloseBehavior::Ok,
                    sent: None,
                })
            };

            let result =
                run_transport_proxy(stdio, dead_client, "test", None, &mut reconnect, None).await;
            assert!(
                result.is_ok_and(|responded| responded),
                "reconnect must mark the bridge as having responded"
            );
            assert_eq!(
                reconnect_calls.load(Ordering::SeqCst),
                1,
                "must reconnect exactly once"
            );
            // initialize(0), request(1), request(2) each failed against the dead
            // client before the 3rd consecutive failure triggered a reconnect.
            assert_eq!(
                dead_attempts.load(Ordering::SeqCst),
                MAX_CONSECUTIVE_FORWARD_FAILURES as usize
            );
            // The fresh client sees: the replayed initialize, then request(3)
            // and request(4) forwarded normally after reconnect — and nothing
            // else: session events must never leak upstream to the bridge.
            assert_eq!(fresh_attempts.load(Ordering::SeqCst), 3);
            let sent = sent.lock();
            let error_count = sent.iter().filter(|m| m.get("error").is_some()).count();
            assert_eq!(error_count, 3, "expected 3 relayed errors, got {sent:?}");
            // Session-health disclosure (#485): exactly one `reconnected`
            // event reached the client, with its `notifications/message`
            // mirror for foreign clients.
            let events: Vec<_> = sent
                .iter()
                .filter(|m| {
                    m.get("method").and_then(|m| m.as_str())
                        == Some(ahma_common::session_event::SESSION_EVENT_METHOD)
                })
                .collect();
            assert_eq!(events.len(), 1, "expected 1 session event, got {sent:?}");
            assert_eq!(events[0]["params"]["kind"], "reconnected");
            assert_eq!(events[0]["params"]["detail"]["reconnects"], 1);
            let mirrors: Vec<_> = sent
                .iter()
                .filter(|m| {
                    m.get("method").and_then(|m| m.as_str()) == Some("notifications/message")
                })
                .collect();
            assert_eq!(mirrors.len(), 1, "expected 1 logging mirror, got {sent:?}");
            assert_eq!(mirrors[0]["params"]["level"], "warning");
            assert_eq!(mirrors[0]["params"]["data"]["kind"], "reconnected");
        })
        .await;
    }

    /// A client whose every `send` fails with a transient error: a transport
    /// that is genuinely dead but not provably gone.
    fn dead_client() -> MockClient {
        MockClient {
            fail_first_n: usize::MAX,
            fail_sends_from: usize::MAX,
            send_error_text: TRANSIENT_SEND_ERROR,
            attempts: Arc::new(AtomicUsize::new(0)),
            inbound: VecDeque::new(),
            closed: Arc::new(AtomicUsize::new(0)),
            receive_none_after: None,
            receive_call_count: Arc::new(AtomicUsize::new(0)),
            close_behavior: CloseBehavior::Ok,
            sent: None,
        }
    }

    /// A freshly reconnected client that answers the replayed `initialize` and
    /// accepts every later send, counting them in `attempts`.
    fn healthy_client(attempts: Arc<AtomicUsize>) -> MockClient {
        MockClient {
            fail_first_n: 0,
            fail_sends_from: usize::MAX,
            send_error_text: TRANSIENT_SEND_ERROR,
            attempts,
            inbound: VecDeque::from(vec![bridge_response(0)]),
            closed: Arc::new(AtomicUsize::new(0)),
            receive_none_after: None,
            receive_call_count: Arc::new(AtomicUsize::new(0)),
            close_behavior: CloseBehavior::Ok,
            sent: None,
        }
    }

    fn session_event_kinds(sent: &[serde_json::Value]) -> Vec<String> {
        sent.iter()
            .filter(|m| {
                m.get("method").and_then(|m| m.as_str())
                    == Some(ahma_common::session_event::SESSION_EVENT_METHOD)
            })
            .map(|m| m["params"]["kind"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[tokio::test]
    async fn unreachable_bridge_never_ends_the_session() {
        // REGRESSION: the proxy used to exit for good once one burst of
        // reconnect attempts failed, and the editor then showed ahma as dead
        // until the user reconnected by hand. While the editor is attached the
        // proxy must stay attached: every request is answered (with an error
        // while the bridge is down) and each new request tries to reconnect.
        with_zero_backoff(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let stdio = MockStdio::new(
                VecDeque::from(vec![
                    client_initialize(0),
                    client_request(1),
                    client_request(2),
                    client_request(3),
                    client_request(4),
                ]),
                sent.clone(),
            );
            let reconnect_calls = Arc::new(AtomicUsize::new(0));
            let reconnect_calls_inner = reconnect_calls.clone();
            let mut reconnect = move || -> Result<MockClient> {
                reconnect_calls_inner.fetch_add(1, Ordering::SeqCst);
                Err(anyhow!("simulated: bridge process is gone"))
            };

            let result =
                run_transport_proxy(stdio, dead_client(), "test", None, &mut reconnect, None).await;
            assert!(result.is_ok(), "proxy must end cleanly at stdio EOF");
            // One burst when the transport died (after request 2), then one
            // burst per request that arrived while disconnected (3 and 4).
            assert_eq!(
                reconnect_calls.load(Ordering::SeqCst),
                3 * MAX_RECONNECT_ATTEMPTS as usize,
                "each request while disconnected must retry the reconnect"
            );
            let sent = sent.lock();
            for id in 0..=4 {
                assert!(
                    sent.iter()
                        .any(|m| m["id"] == serde_json::json!(id) && m.get("error").is_some()),
                    "request {id} must be answered with an error, got {sent:?}"
                );
            }
        })
        .await;
    }

    #[tokio::test]
    async fn unreachable_bridge_is_disclosed_once_without_claiming_the_session_closed() {
        // Session-health disclosure (#485): losing the bridge is announced once
        // per outage, not once per request, and says the proxy is still here.
        with_zero_backoff(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let stdio = MockStdio::new(
                VecDeque::from(vec![
                    client_initialize(0),
                    client_request(1),
                    client_request(2),
                    client_request(3),
                ]),
                sent.clone(),
            );
            let mut reconnect =
                move || -> Result<MockClient> { Err(anyhow!("simulated: bridge gone")) };

            let result =
                run_transport_proxy(stdio, dead_client(), "test", None, &mut reconnect, None).await;
            assert!(result.is_ok());
            let sent = sent.lock();
            assert_eq!(
                session_event_kinds(&sent),
                vec!["reconnect_failed".to_string()],
                "got {sent:?}"
            );
            let event = sent
                .iter()
                .find(|m| {
                    m.get("method").and_then(|m| m.as_str())
                        == Some(ahma_common::session_event::SESSION_EVENT_METHOD)
                })
                .unwrap();
            let message = event["params"]["detail"]["message"].as_str().unwrap();
            assert!(
                !message.contains("closing"),
                "the connection is not closing: {message}"
            );
            let mirror = sent
                .iter()
                .find(|m| m.get("method").and_then(|m| m.as_str()) == Some("notifications/message"))
                .expect("logging mirror present");
            assert_eq!(mirror["params"]["level"], "error");
        })
        .await;
    }

    #[tokio::test]
    async fn unreachable_bridge_recovers_on_a_later_request() {
        // The bridge comes back (restarted, or a newer version took over): the
        // next request reconnects, replays the handshake, is forwarded, and the
        // recovery is disclosed.
        with_zero_backoff(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let stdio = MockStdio::new(
                VecDeque::from(vec![
                    client_initialize(0),
                    client_request(1),
                    client_request(2),
                    client_request(3),
                ]),
                sent.clone(),
            );
            let reconnect_calls = Arc::new(AtomicUsize::new(0));
            let reconnect_calls_inner = reconnect_calls.clone();
            let fresh_attempts = Arc::new(AtomicUsize::new(0));
            let fresh_attempts_inner = fresh_attempts.clone();
            let mut reconnect = move || -> Result<MockClient> {
                let n = reconnect_calls_inner.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= MAX_RECONNECT_ATTEMPTS as usize {
                    Err(anyhow!("simulated: bridge restarting"))
                } else {
                    Ok(healthy_client(fresh_attempts_inner.clone()))
                }
            };

            let result =
                run_transport_proxy(stdio, dead_client(), "test", None, &mut reconnect, None).await;
            assert!(result.is_ok_and(|responded| responded));
            // The fresh client saw the replayed initialize and request 3.
            assert_eq!(fresh_attempts.load(Ordering::SeqCst), 2);
            let sent = sent.lock();
            assert!(
                !sent.iter().any(|m| m["id"] == serde_json::json!(3)),
                "request 3 was forwarded, not answered with an error: {sent:?}"
            );
            assert_eq!(
                session_event_kinds(&sent),
                vec!["reconnect_failed".to_string(), "reconnected".to_string()],
                "got {sent:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn bridge_closing_a_handshaken_session_does_not_end_the_proxy() {
        // The bridge closes its side (it exited on idle, crashed, or handed over
        // to a newer version). With a handshake to replay, the proxy keeps the
        // editor attached and reconnects when the next request arrives.
        with_zero_backoff(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let mut stdio = MockStdio::new(
                VecDeque::from(vec![client_initialize(0), client_request(1)]),
                sent.clone(),
            );
            stdio.inbound_delay = Some(Duration::from_millis(50));
            let first = MockClient {
                inbound: VecDeque::from(vec![bridge_response(0)]),
                receive_none_after: Some(1),
                ..healthy_client(Arc::new(AtomicUsize::new(0)))
            };
            let fresh_attempts = Arc::new(AtomicUsize::new(0));
            let fresh_attempts_inner = fresh_attempts.clone();
            let mut reconnect =
                move || -> Result<MockClient> { Ok(healthy_client(fresh_attempts_inner.clone())) };

            let result =
                run_transport_proxy(stdio, first, "test", None, &mut reconnect, None).await;
            assert!(result.is_ok());
            // Replayed initialize + request 1 on the fresh connection.
            assert_eq!(fresh_attempts.load(Ordering::SeqCst), 2);
            assert!(
                session_event_kinds(&sent.lock()).contains(&"reconnected".to_string()),
                "got {:?}",
                sent.lock()
            );
        })
        .await;
    }

    #[test]
    fn heartbeat_overlay_injects_reconnects_only_when_nonzero() {
        let mut hb = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/ahma/heartbeat",
            "params": {"version": "1", "hash": "h", "timestamp": 1}
        });
        overlay_heartbeat_reconnects(&mut hb, 0);
        assert!(hb["params"].get("reconnects").is_none(), "0 → untouched");
        overlay_heartbeat_reconnects(&mut hb, 2);
        assert_eq!(hb["params"]["reconnects"], 2);

        // Non-heartbeat messages are never touched.
        let mut other = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "result": {}
        });
        let before = other.clone();
        overlay_heartbeat_reconnects(&mut other, 5);
        assert_eq!(other, before);
    }

    /// Endpoint-gone errors (socket unlinked, nothing listening) call for a
    /// bridge respawn; other reconnect failures do not.
    #[test]
    fn reconnect_failure_wants_respawn_classifies_errors() {
        let not_found = anyhow::Error::from(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory",
        ))
        .context("Failed to reconnect proxy to UDS /tmp/ahma.sock");
        assert!(reconnect_failure_wants_respawn(&not_found));

        let refused = anyhow::Error::from(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "Connection refused",
        ));
        assert!(reconnect_failure_wants_respawn(&refused));

        // Stringified io error (chain lost by an intermediate layer).
        let stringified = anyhow!(
            "UnexpectedServerResponse: Client(Io(Os {{ code: 2, kind: NotFound, \
             message: \"No such file or directory\" }}))"
        );
        assert!(reconnect_failure_wants_respawn(&stringified));

        let unrelated = anyhow!("bridge answered with a malformed initialize response");
        assert!(!reconnect_failure_wants_respawn(&unrelated));
    }

    /// A realistic rmcp rendering of the bridge's sandbox-initializing answer:
    /// HTTP 409 folded into `StreamableHttpError::UnexpectedServerResponse`.
    const RENDERED_409_SANDBOX_INITIALIZING: &str = concat!(
        "unexpected server response: HTTP 409 Conflict: ",
        r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32001,"message":"#,
        r#""Sandbox initializing from client roots. This server requires roots/list "#,
        r#"from client; configure --sandbox-scope for clients without roots support."}}"#,
    );

    /// The bridge's handshake-timeout answer: HTTP 504, and a message that is
    /// itself full of colons, digits and punctuation — exactly what a naive
    /// "split on the first colon" recovery would mangle.
    const RENDERED_504_HANDSHAKE_TIMEOUT: &str = concat!(
        "unexpected server response: HTTP 504 Gateway Timeout: ",
        r#"{"jsonrpc":"2.0","id":9,"error":{"code":-32002,"message":"#,
        r#""Handshake timed out after 30s: 1) send initialize with no session header; "#,
        r#"2) open the SSE stream before notifications/initialized; "#,
        r#"3) answer roots/list over SSE with the same id; 4) only then tools/call"}}"#,
    );

    #[test]
    fn recovers_the_bridges_409_sandbox_error_from_the_transport_error() {
        // REGRESSION: rmcp turns every non-2xx bridge response into a transport
        // error, so the bridge's own actionable instruction was being replaced
        // by a generic "retry or await" sentence that sent the model chasing an
        // operation that never existed.
        let recovered = recover_bridge_jsonrpc_error(RENDERED_409_SANDBOX_INITIALIZING)
            .expect("the 409 body must be recoverable");
        assert_eq!(recovered["code"], serde_json::json!(-32001));
        assert_eq!(
            recovered["message"],
            serde_json::json!(
                "Sandbox initializing from client roots. This server requires roots/list \
                 from client; configure --sandbox-scope for clients without roots support."
            )
        );
    }

    #[test]
    fn recovers_a_body_whose_message_contains_colons_and_digits() {
        let recovered = recover_bridge_jsonrpc_error(RENDERED_504_HANDSHAKE_TIMEOUT)
            .expect("the 504 body must be recoverable");
        assert_eq!(recovered["code"], serde_json::json!(-32002));
        let message = recovered["message"].as_str().expect("message is a string");
        assert!(
            message.starts_with("Handshake timed out after 30s: 1) send initialize"),
            "remediation checklist truncated: {message}"
        );
        assert!(
            message.ends_with("4) only then tools/call"),
            "remediation checklist truncated: {message}"
        );
    }

    #[test]
    fn recovers_nothing_from_a_dead_socket_or_a_non_json_body() {
        // A genuinely dead socket carries no HTTP response at all.
        assert!(recover_bridge_jsonrpc_error(GONE_ENDPOINT_SEND_ERROR).is_none());
        assert!(recover_bridge_jsonrpc_error(TRANSIENT_SEND_ERROR).is_none());
        // An HTTP status with a body that is not JSON.
        assert!(
            recover_bridge_jsonrpc_error(
                "unexpected server response: HTTP 502 Bad Gateway: <html>{oops}</html>"
            )
            .is_none()
        );
        // Valid JSON, but no `error` object to relay.
        assert!(
            recover_bridge_jsonrpc_error(
                r#"unexpected server response: HTTP 500: {"jsonrpc":"2.0","id":1,"result":{}}"#
            )
            .is_none()
        );
        // An `error` that a downstream client could not decode as JSON-RPC.
        assert!(
            recover_bridge_jsonrpc_error(
                r#"unexpected server response: HTTP 500: {"error":{"code":"nope"}}"#
            )
            .is_none()
        );
    }

    #[test]
    fn classification_prefers_a_recovered_body_over_the_gone_endpoint_signature() {
        // A live bridge can legitimately answer with a message mentioning a
        // missing file; that must never be mistaken for a gone endpoint, or the
        // proxy would tear down and rebuild a perfectly healthy connection.
        let rendered = concat!(
            "unexpected server response: HTTP 500 Internal Server Error: ",
            r#"{"error":{"code":-32603,"message":"Failed to send request: "#,
            r#"No such file or directory (os error 2)"}}"#,
        );
        assert!(matches!(
            classify_forward_failure(rendered, "irrelevant"),
            ForwardFailure::BridgeError(_)
        ));
        assert_eq!(
            classify_forward_failure(GONE_ENDPOINT_SEND_ERROR, "irrelevant"),
            ForwardFailure::EndpointGone
        );
        assert_eq!(
            classify_forward_failure(TRANSIENT_SEND_ERROR, "irrelevant"),
            ForwardFailure::Transient
        );
        // The io kind survives only in the Debug rendering when an intermediate
        // layer dropped the message.
        assert_eq!(
            classify_forward_failure("Client error", "Client(Io(Os { code: 2, kind: NotFound }))"),
            ForwardFailure::EndpointGone
        );
    }

    #[tokio::test]
    async fn bridge_error_body_is_relayed_downstream_instead_of_the_generic_message() {
        // REGRESSION (live incident): the client used to receive only
        // "Bridge could not service this request; it may still be running.
        // Retry, or await the completion notification." for a 409 that actually
        // said how to fix the problem. The agent retried, got nowhere, and
        // abandoned ahma for its own unsandboxed terminal.
        let state = TestState::new();
        let stdio = state.stdio(VecDeque::from(vec![client_request(1), client_request(2)]));
        let client = state.client_failing_with(1, RENDERED_409_SANDBOX_INITIALIZING);
        let mut reconnect = || -> Result<MockClient> { Err(anyhow!("no reconnect in this test")) };

        let result = run_transport_proxy(stdio, client, "test", None, &mut reconnect, None).await;
        assert!(result.is_ok(), "the session must survive the relayed error");

        let sent = state.sent.lock();
        assert_eq!(sent.len(), 1, "expected one relayed error, got {sent:?}");
        assert_eq!(
            sent[0]["id"],
            serde_json::json!(1),
            "the bridge's error must be relayed under *this* request's id"
        );
        assert_eq!(
            sent[0]["error"]["code"],
            serde_json::json!(-32001),
            "expected the bridge's own code, not the generic -32002: {sent:?}"
        );
        let message = sent[0]["error"]["message"]
            .as_str()
            .expect("message is a string");
        assert!(
            message.contains("Sandbox initializing from client roots")
                && message.contains("--sandbox-scope"),
            "the bridge's remediation text must survive verbatim: {message}"
        );
    }

    #[tokio::test]
    async fn gone_endpoint_reconnects_on_the_first_failure_and_resends_the_request() {
        // REGRESSION: an auto-spawned bridge unlinks its socket and exits after
        // `--idle-timeout`, so a frontend proxy that outlives it fails to POST
        // with ENOENT. Re-dialing a gone endpoint can never succeed, so waiting
        // for MAX_CONSECUTIVE_FORWARD_FAILURES just burns two more requests on
        // a socket that is not coming back. Recover on the first failure, and
        // resend the request that never reached the bridge.
        with_zero_backoff(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let stdio = MockStdio::new(
                VecDeque::from(vec![
                    client_initialize(0),
                    client_request(1),
                    client_request(2),
                ]),
                sent.clone(),
            );

            let dead_attempts = Arc::new(AtomicUsize::new(0));
            let dead_client = MockClient {
                fail_first_n: 0,
                // `initialize` gets through; the bridge then idles out and the
                // next forward hits an unlinked socket.
                fail_sends_from: 2,
                send_error_text: GONE_ENDPOINT_SEND_ERROR,
                attempts: dead_attempts.clone(),
                inbound: VecDeque::new(),
                closed: Arc::new(AtomicUsize::new(0)),
                receive_none_after: None,
                receive_call_count: Arc::new(AtomicUsize::new(0)),
                close_behavior: CloseBehavior::Ok,
                sent: None,
            };

            let fresh_attempts = Arc::new(AtomicUsize::new(0));
            let fresh_sent = Arc::new(Mutex::new(Vec::new()));
            let reconnect_calls = Arc::new(AtomicUsize::new(0));
            let reconnect_calls_inner = reconnect_calls.clone();
            let fresh_attempts_inner = fresh_attempts.clone();
            let fresh_sent_inner = fresh_sent.clone();
            let mut reconnect = move || -> Result<MockClient> {
                reconnect_calls_inner.fetch_add(1, Ordering::SeqCst);
                Ok(MockClient {
                    fail_first_n: 0,
                    fail_sends_from: usize::MAX,
                    send_error_text: TRANSIENT_SEND_ERROR,
                    attempts: fresh_attempts_inner.clone(),
                    inbound: VecDeque::from(vec![bridge_response(0)]),
                    closed: Arc::new(AtomicUsize::new(0)),
                    receive_none_after: None,
                    receive_call_count: Arc::new(AtomicUsize::new(0)),
                    close_behavior: CloseBehavior::Ok,
                    sent: Some(fresh_sent_inner.clone()),
                })
            };

            let result =
                run_transport_proxy(stdio, dead_client, "test", None, &mut reconnect, None).await;
            assert!(
                result.is_ok_and(|responded| responded),
                "the session must resume against the reconnected bridge"
            );
            assert_eq!(
                reconnect_calls.load(Ordering::SeqCst),
                1,
                "must reconnect exactly once"
            );
            assert_eq!(
                dead_attempts.load(Ordering::SeqCst),
                2,
                "the gone endpoint must be abandoned on the FIRST failure, not the third"
            );

            // Replayed initialize, the resent request(1), then request(2).
            let fresh_sent = fresh_sent.lock();
            assert_eq!(fresh_sent.len(), 3, "expected 3 sends, got {fresh_sent:?}");
            assert_eq!(fresh_sent[0]["method"], serde_json::json!("initialize"));
            assert_eq!(
                fresh_sent[1]["id"],
                serde_json::json!(1),
                "the failed request must be resent transparently: {fresh_sent:?}"
            );
            assert_eq!(fresh_sent[2]["id"], serde_json::json!(2));

            let sent = sent.lock();
            assert_eq!(
                sent.iter().filter(|m| m.get("error").is_some()).count(),
                0,
                "a resent request must NOT also be answered with an error: {sent:?}"
            );
            // The reconnect disclosure (#485) must say the request was resent,
            // not invite the client to retry something already in flight.
            let events: Vec<_> = sent
                .iter()
                .filter(|m| {
                    m.get("method").and_then(|m| m.as_str())
                        == Some(ahma_common::session_event::SESSION_EVENT_METHOD)
                })
                .collect();
            assert_eq!(events.len(), 1, "expected 1 session event, got {sent:?}");
            assert_eq!(events[0]["params"]["kind"], "reconnected");
            assert_eq!(events[0]["params"]["detail"]["cause"], "endpoint_gone");
            assert_eq!(events[0]["params"]["detail"]["in_flight_request"], "resent");
            let message = events[0]["params"]["detail"]["message"]
                .as_str()
                .expect("detail message is a string");
            assert!(
                message.contains("resent") && !message.contains("can be retried"),
                "disclosure must not claim the request needs retrying: {message}"
            );
        })
        .await;
    }

    /// rmcp's rendering of a POST the hub answered 404: the session id the
    /// proxy holds is one the hub does not know (it handed over to a successor,
    /// or restarted), with rmcp's own re-initialization turned off.
    const SESSION_EXPIRED_SEND_ERROR: &str = "Session expired (HTTP 404)";

    /// A bridge transport whose sends fail from the `fail_sends_from`th one
    /// (1-indexed) with `error`, which answers its receives from `inbound`,
    /// and which records every send that went through in `sent`.
    fn scripted_client(
        fail_sends_from: usize,
        error: &'static str,
        inbound: Vec<RxJsonRpcMessage<RoleClient>>,
        sent: &Arc<Mutex<Vec<serde_json::Value>>>,
    ) -> MockClient {
        MockClient {
            fail_first_n: 0,
            fail_sends_from,
            send_error_text: error,
            attempts: Arc::new(AtomicUsize::new(0)),
            inbound: VecDeque::from(inbound),
            closed: Arc::new(AtomicUsize::new(0)),
            receive_none_after: None,
            receive_call_count: Arc::new(AtomicUsize::new(0)),
            close_behavior: CloseBehavior::Ok,
            sent: Some(sent.clone()),
        }
    }

    /// The session events the proxy sent downstream.
    fn session_events(sent: &[serde_json::Value]) -> Vec<serde_json::Value> {
        sent.iter()
            .filter(|m| {
                m.get("method").and_then(|m| m.as_str())
                    == Some(ahma_common::session_event::SESSION_EVENT_METHOD)
            })
            .cloned()
            .collect()
    }

    /// A client that gave its roots: `initialize`, then (after the bridge has
    /// asked) its `roots/list` answer, then one tool call. The delay lets the
    /// bridge's `initialize` answer and `roots/list` request reach the proxy
    /// between the client's messages.
    fn stdio_with_roots_then_a_call(sent: &Arc<Mutex<Vec<serde_json::Value>>>) -> MockStdio {
        let mut stdio = MockStdio::new(
            VecDeque::from(vec![
                client_initialize(0),
                client_roots_list_response(50),
                client_request(1),
            ]),
            sent.clone(),
        );
        stdio.inbound_delay = Some(Duration::from_millis(50));
        stdio
    }

    /// SPEC R-HUB.5: after a hub hands over to its successor, the successor
    /// does not know the proxy's session and answers 404. The proxy rebuilds
    /// the session itself (only its replay carries roots) on that FIRST
    /// failure, and resends the request the hub never ran. Field case
    /// 2026-11-27: rmcp re-initialized behind the proxy's back instead, the new
    /// session never got roots, and three tool calls in a row were answered
    /// "Sandbox initializing from client roots" before the proxy noticed.
    #[tokio::test]
    async fn an_expired_session_is_rebuilt_with_its_roots_and_the_request_resent() {
        with_zero_backoff(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let stdio = stdio_with_roots_then_a_call(&sent);
            let old_sent = Arc::new(Mutex::new(Vec::new()));
            // `initialize` and the roots answer get through; then the hub hands over.
            let old = scripted_client(
                3,
                SESSION_EXPIRED_SEND_ERROR,
                vec![bridge_initialize_response(0), bridge_roots_list_request(50)],
                &old_sent,
            );
            let fresh_sent = Arc::new(Mutex::new(Vec::new()));
            let fresh_sent_inner = fresh_sent.clone();
            let reconnects = Arc::new(AtomicUsize::new(0));
            let reconnects_inner = reconnects.clone();
            let mut reconnect = move || -> Result<MockClient> {
                reconnects_inner.fetch_add(1, Ordering::SeqCst);
                Ok(scripted_client(
                    usize::MAX,
                    TRANSIENT_SEND_ERROR,
                    vec![bridge_initialize_response(0), bridge_roots_list_request(51)],
                    &fresh_sent_inner,
                ))
            };

            let result = run_transport_proxy(stdio, old, "test", None, &mut reconnect, None).await;
            assert!(result.is_ok_and(|responded| responded));
            assert_eq!(
                reconnects.load(Ordering::SeqCst),
                1,
                "rebuilt on the first 404"
            );

            let fresh_sent = fresh_sent.lock();
            assert_eq!(
                fresh_sent.len(),
                3,
                "replay, roots, resend: {fresh_sent:#?}"
            );
            assert_eq!(fresh_sent[0]["method"], INITIALIZE_METHOD);
            assert_eq!(
                fresh_sent[1]["id"], 51,
                "the new session's roots/list is answered"
            );
            assert_eq!(
                fresh_sent[1]["result"]["roots"][0]["uri"],
                "file:///workspace"
            );
            assert_eq!(
                fresh_sent[2]["id"], 1,
                "the request is resent: {fresh_sent:#?}"
            );

            let sent = sent.lock();
            assert!(
                sent.iter().all(|m| m.get("error").is_none()),
                "the client sees no error: {sent:#?}"
            );
            let events = session_events(&sent);
            assert_eq!(events.len(), 1, "one disclosure: {sent:#?}");
            assert_eq!(events[0]["params"]["kind"], "reconnected");
            assert_eq!(events[0]["params"]["detail"]["cause"], "session_gone");
            assert_eq!(events[0]["params"]["detail"]["in_flight_request"], "resent");
        })
        .await;
    }

    /// The backstop: a hub session that answers "sandbox initializing" after
    /// this client already gave its roots has lost them, whatever lost them.
    /// The proxy rebuilds it and resends the call, which the hub refused
    /// without running it.
    #[tokio::test]
    async fn sandbox_initializing_after_roots_were_given_rebuilds_the_session() {
        with_zero_backoff(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let stdio = stdio_with_roots_then_a_call(&sent);
            let old_sent = Arc::new(Mutex::new(Vec::new()));
            let old = scripted_client(
                3,
                RENDERED_409_SANDBOX_INITIALIZING,
                vec![bridge_initialize_response(0), bridge_roots_list_request(50)],
                &old_sent,
            );
            let fresh_sent = Arc::new(Mutex::new(Vec::new()));
            let fresh_sent_inner = fresh_sent.clone();
            let reconnects = Arc::new(AtomicUsize::new(0));
            let reconnects_inner = reconnects.clone();
            let mut reconnect = move || -> Result<MockClient> {
                reconnects_inner.fetch_add(1, Ordering::SeqCst);
                Ok(scripted_client(
                    usize::MAX,
                    TRANSIENT_SEND_ERROR,
                    vec![bridge_initialize_response(0), bridge_roots_list_request(51)],
                    &fresh_sent_inner,
                ))
            };

            let result = run_transport_proxy(stdio, old, "test", None, &mut reconnect, None).await;
            assert!(result.is_ok());
            assert_eq!(reconnects.load(Ordering::SeqCst), 1);
            let fresh_sent = fresh_sent.lock();
            assert_eq!(
                fresh_sent.last().map(|m| m["id"].clone()),
                Some(serde_json::json!(1))
            );
            let sent = sent.lock();
            assert!(sent.iter().all(|m| m.get("error").is_none()), "{sent:#?}");
            assert_eq!(
                session_events(&sent)[0]["params"]["detail"]["cause"],
                "session_gone"
            );
        })
        .await;
    }

    /// Before the client gave roots, "sandbox initializing" is the truth, not
    /// a lost session: it is relayed, and nothing is rebuilt.
    #[tokio::test]
    async fn sandbox_initializing_before_any_roots_is_relayed() {
        let state = TestState::new();
        let stdio = state.stdio(VecDeque::from(vec![
            client_initialize(0),
            client_request(1),
        ]));
        let mut client = state.client(0);
        client.fail_sends_from = 2;
        client.send_error_text = RENDERED_409_SANDBOX_INITIALIZING;
        let mut reconnect =
            || -> Result<MockClient> { panic!("a session that never had roots is not rebuilt") };

        let result = run_transport_proxy(stdio, client, "test", None, &mut reconnect, None).await;
        assert!(result.is_ok());
        let sent = state.sent.lock();
        assert_eq!(sent.len(), 1, "{sent:#?}");
        assert_eq!(sent[0]["id"], 1);
        assert_eq!(sent[0]["error"]["code"], -32001);
    }

    /// Once per request: when the rebuilt session refuses the resent call too,
    /// the hub's own answer is relayed and the proxy does not rebuild again.
    #[tokio::test]
    async fn a_rebuilt_session_that_still_refuses_is_relayed_without_a_second_rebuild() {
        with_zero_backoff(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let stdio = stdio_with_roots_then_a_call(&sent);
            let old_sent = Arc::new(Mutex::new(Vec::new()));
            let old = scripted_client(
                3,
                RENDERED_409_SANDBOX_INITIALIZING,
                vec![bridge_initialize_response(0), bridge_roots_list_request(50)],
                &old_sent,
            );
            let fresh_sent = Arc::new(Mutex::new(Vec::new()));
            let fresh_sent_inner = fresh_sent.clone();
            let reconnects = Arc::new(AtomicUsize::new(0));
            let reconnects_inner = reconnects.clone();
            let mut reconnect = move || -> Result<MockClient> {
                reconnects_inner.fetch_add(1, Ordering::SeqCst);
                // The replay's two sends go through; the resend is refused.
                Ok(scripted_client(
                    3,
                    RENDERED_409_SANDBOX_INITIALIZING,
                    vec![bridge_initialize_response(0), bridge_roots_list_request(51)],
                    &fresh_sent_inner,
                ))
            };

            let result = run_transport_proxy(stdio, old, "test", None, &mut reconnect, None).await;
            assert!(result.is_ok());
            assert_eq!(reconnects.load(Ordering::SeqCst), 1, "no second rebuild");
            let sent = sent.lock();
            let errors: Vec<_> = sent.iter().filter(|m| m.get("error").is_some()).collect();
            assert_eq!(errors.len(), 1, "{sent:#?}");
            assert_eq!(errors[0]["id"], 1);
            assert_eq!(
                errors[0]["error"]["code"], -32001,
                "the hub's own answer, not the generic one"
            );
        })
        .await;
    }

    #[test]
    fn an_expired_session_is_classified_as_gone_not_transient() {
        assert_eq!(
            classify_forward_failure(SESSION_EXPIRED_SEND_ERROR, "SessionExpired"),
            ForwardFailure::SessionGone
        );
    }

    #[tokio::test]
    async fn transient_failures_still_wait_for_the_threshold_before_reconnecting() {
        // The gone-endpoint fast path must not swallow the count-based path:
        // a transient failure on a *live* endpoint is still tolerated up to
        // MAX_CONSECUTIVE_FORWARD_FAILURES. (That the third failure does then
        // reconnect is covered by
        // `reconnect_after_dead_transport_resumes_session_transparently`.)
        let state = TestState::new();
        let stdio = state.stdio(VecDeque::from(vec![
            client_initialize(0),
            client_request(1),
        ]));
        let client = state.client(usize::MAX);
        let mut reconnect = || -> Result<MockClient> {
            panic!("must not reconnect before MAX_CONSECUTIVE_FORWARD_FAILURES on a live endpoint")
        };

        let result = run_transport_proxy(stdio, client, "test", None, &mut reconnect, None).await;
        assert!(result.is_ok());
        assert_eq!(
            state.attempts.load(Ordering::SeqCst),
            2,
            "both messages must be attempted without an early reconnect"
        );
        let sent = state.sent.lock();
        assert_eq!(sent.len(), 2, "expected 2 relayed errors, got {sent:?}");
        assert!(
            sent.iter()
                .all(|m| m["error"]["code"] == serde_json::json!(-32002)),
            "a transient failure has no recoverable body, so the generic error stands: {sent:?}"
        );
    }

    /// REGRESSION (2026-07-14 live incident): when the bridge endpoint is gone
    /// — its socket was unlinked out from under it — re-dialing can never
    /// succeed. The proxy must invoke the respawn hook and then reconnect to
    /// the freshly spawned bridge, resuming the session instead of exhausting
    /// retries and presenting a dead server to the client.
    #[tokio::test]
    async fn respawn_hook_revives_a_gone_bridge_endpoint() {
        with_zero_backoff(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let stdio = MockStdio::new(
                VecDeque::from(vec![
                    client_initialize(0),
                    client_request(1),
                    client_request(2),
                    client_request(3),
                ]),
                sent.clone(),
            );

            let dead_client = MockClient {
                fail_first_n: usize::MAX,
                fail_sends_from: usize::MAX,
                send_error_text: TRANSIENT_SEND_ERROR,
                attempts: Arc::new(AtomicUsize::new(0)),
                inbound: VecDeque::new(),
                closed: Arc::new(AtomicUsize::new(0)),
                receive_none_after: None,
                receive_call_count: Arc::new(AtomicUsize::new(0)),
                close_behavior: CloseBehavior::Ok,
                sent: None,
            };

            // Until the respawn hook has run, reconnecting fails exactly the
            // way a gone endpoint does (io NotFound). After respawn, dialing
            // succeeds.
            let respawned = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let respawned_for_reconnect = respawned.clone();
            let fresh_attempts = Arc::new(AtomicUsize::new(0));
            let fresh_attempts_inner = fresh_attempts.clone();
            let mut reconnect = move || -> Result<MockClient> {
                if !respawned_for_reconnect.load(Ordering::SeqCst) {
                    return Err(anyhow::Error::from(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "No such file or directory",
                    ))
                    .context("Failed to reconnect proxy to UDS /tmp/test.sock"));
                }
                Ok(MockClient {
                    fail_first_n: 0,
                    fail_sends_from: usize::MAX,
                    send_error_text: TRANSIENT_SEND_ERROR,
                    attempts: fresh_attempts_inner.clone(),
                    inbound: VecDeque::from(vec![bridge_response(0)]),
                    closed: Arc::new(AtomicUsize::new(0)),
                    receive_none_after: None,
                    receive_call_count: Arc::new(AtomicUsize::new(0)),
                    close_behavior: CloseBehavior::Ok,
                    sent: None,
                })
            };

            let respawn_calls = Arc::new(AtomicUsize::new(0));
            let respawn_calls_inner = respawn_calls.clone();
            let respawned_inner = respawned.clone();
            let respawn: BridgeRespawnFn = Box::new(move || {
                let respawn_calls = respawn_calls_inner.clone();
                let respawned = respawned_inner.clone();
                Box::pin(async move {
                    respawn_calls.fetch_add(1, Ordering::SeqCst);
                    respawned.store(true, Ordering::SeqCst);
                    Ok(())
                })
            });

            let result = run_transport_proxy(
                stdio,
                dead_client,
                "test",
                None,
                &mut reconnect,
                Some(respawn),
            )
            .await;
            assert!(
                result.is_ok_and(|responded| responded),
                "session must resume against the respawned bridge"
            );
            assert_eq!(
                respawn_calls.load(Ordering::SeqCst),
                1,
                "the respawn hook must run exactly once"
            );
            assert!(
                fresh_attempts.load(Ordering::SeqCst) > 0,
                "traffic must flow to the respawned bridge"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn replay_handshake_answers_roots_list_from_cache_with_the_new_request_id() {
        // The sandbox scope cannot change during a session (security
        // invariant), so replaying the cached roots/list answer on reconnect —
        // rather than re-asking the downstream client — is correct. The id
        // must be substituted to match the fresh session's own request.
        let handshake = CachedHandshake {
            init_request: Some(serde_json::to_value(client_initialize(0)).unwrap()),
            notif_initialized: Some(
                serde_json::to_value(client_notifications_initialized()).unwrap(),
            ),
            pending_roots_list_id: None,
            roots_response: Some(serde_json::to_value(client_roots_list_response(1)).unwrap()),
            ..Default::default()
        };

        let attempts = Arc::new(AtomicUsize::new(0));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut client = MockClient {
            fail_first_n: 0,
            fail_sends_from: usize::MAX,
            send_error_text: TRANSIENT_SEND_ERROR,
            attempts: attempts.clone(),
            inbound: VecDeque::from(vec![
                bridge_initialize_response(0), // answers the replayed initialize
                bridge_roots_list_request(7),  // the fresh session's own roots/list
            ]),
            closed: Arc::new(AtomicUsize::new(0)),
            receive_none_after: None,
            receive_call_count: Arc::new(AtomicUsize::new(0)),
            close_behavior: CloseBehavior::Ok,
            sent: Some(sent.clone()),
        };

        replay_handshake(&mut client, &handshake)
            .await
            .expect("replay must succeed");
        assert_eq!(
            handshake.replayed_version.lock().as_deref(),
            Some("0.1.0"),
            "the replay records which version answered, for the reconnect site"
        );

        // initialize + notifications/initialized + the roots/list answer.
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        let sent = sent.lock();
        assert_eq!(sent.len(), 3, "expected 3 sends, got {sent:?}");
        assert_eq!(sent[0]["method"], serde_json::json!("initialize"));
        assert_eq!(
            sent[1]["method"],
            serde_json::json!("notifications/initialized")
        );
        // The cached response was for the *original* roots/list (id 1); the
        // fresh bridge session asked with id 7, so the replayed answer must
        // carry id 7, not the stale cached id.
        assert_eq!(sent[2]["id"], serde_json::json!(7));
        assert_eq!(
            sent[2]["result"]["roots"][0]["uri"],
            serde_json::json!("file:///workspace")
        );
    }

    /// A reconnect that lands on a different ahma version tells the client
    /// its tool list may have changed (SPEC R-HUB.5): it cached `tools/list`
    /// from the old build and would otherwise keep using that build's
    /// schemas until it restarted. Announced once per change, and never for
    /// a reconnect to the same version.
    #[tokio::test]
    async fn a_reconnect_to_a_new_version_announces_changed_tools() {
        let mut handshake = CachedHandshake {
            init_request: Some(serde_json::to_value(client_initialize(0)).unwrap()),
            ..Default::default()
        };
        handshake.observe_bridge_to_client(
            &serde_json::to_value(bridge_initialize_response(0)).unwrap(),
        );
        assert_eq!(handshake.server_version.as_deref(), Some("0.1.0"));

        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut stdio = MockStdio::new(VecDeque::new(), sent.clone());
        let mut seq = 0;
        let announcements = |sent: &Arc<Mutex<Vec<serde_json::Value>>>| {
            sent.lock()
                .iter()
                .filter(|m| m["method"] == TOOLS_LIST_CHANGED_METHOD)
                .count()
        };

        *handshake.replayed_version.lock() = Some("0.1.0".into());
        announce_reconnect(&mut stdio, &mut seq, &mut handshake, serde_json::json!({})).await;
        assert_eq!(announcements(&sent), 0, "same version, same tools");

        *handshake.replayed_version.lock() = Some("0.2.0".into());
        announce_reconnect(&mut stdio, &mut seq, &mut handshake, serde_json::json!({})).await;
        announce_reconnect(&mut stdio, &mut seq, &mut handshake, serde_json::json!({})).await;
        assert_eq!(announcements(&sent), 1, "one change, announced once");
        assert_eq!(handshake.server_version.as_deref(), Some("0.2.0"));
    }

    #[tokio::test]
    async fn replay_handshake_is_a_noop_without_a_cached_initialize() {
        // No handshake was ever observed — nothing to replay, and no sends
        // should happen (this path is only reachable pre-handshake if a caller
        // invokes replay_handshake directly; the main loop already gates on
        // `init_request.is_some()` before calling it at all).
        let handshake = CachedHandshake::default();
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut client = MockClient {
            fail_first_n: 0,
            fail_sends_from: usize::MAX,
            send_error_text: TRANSIENT_SEND_ERROR,
            attempts: attempts.clone(),
            inbound: VecDeque::new(),
            closed: Arc::new(AtomicUsize::new(0)),
            receive_none_after: None,
            receive_call_count: Arc::new(AtomicUsize::new(0)),
            close_behavior: CloseBehavior::Ok,
            sent: None,
        };

        replay_handshake(&mut client, &handshake)
            .await
            .expect("no-op replay must succeed");
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn handshake_deadline_unset_returns_default() {
        let resolved = deadline_with_env(None);
        assert_eq!(
            resolved,
            Some(Duration::from_secs(
                ahma_common::timeouts::FRONTEND_HANDSHAKE_DEADLINE_SECS
            )),
            "unset env var must fall back to the default constant"
        );
    }

    #[test]
    fn handshake_deadline_zero_disables() {
        let resolved = deadline_with_env(Some("0"));
        assert_eq!(resolved, None, "value of 0 must disable the deadline");
    }

    #[test]
    fn handshake_deadline_positive_value_is_used() {
        let resolved = deadline_with_env(Some("5"));
        assert_eq!(
            resolved,
            Some(Duration::from_secs(5)),
            "positive value must be parsed and used verbatim"
        );
    }

    #[test]
    fn handshake_deadline_garbage_falls_back_to_default() {
        let resolved = deadline_with_env(Some("abc"));
        assert_eq!(
            resolved,
            Some(Duration::from_secs(
                ahma_common::timeouts::FRONTEND_HANDSHAKE_DEADLINE_SECS
            )),
            "unparseable value must fall back to the default constant"
        );
    }

    #[tokio::test]
    async fn bridge_response_forwarded_then_stdio_eof_returns_true() {
        // Bridge delivers one message which the proxy must forward to stdio; the
        // stdio side then EOFs. Because nothing was ever forwarded *to* the bridge
        // (forwarded_any == false), the teardown `client.close()` must be skipped.
        let state = TestState::new();
        let mut stdio = state.stdio(VecDeque::new());
        // Delay EOF so the immediately-ready bridge message wins the first
        // `select!` poll and is forwarded before the loop breaks on EOF.
        stdio.eof_delay = Some(Duration::from_millis(50));

        let mut client = state.client(0);
        client.inbound = VecDeque::from(vec![bridge_response(1)]);

        let mut reconnect = no_reconnect;
        let result = run_transport_proxy(stdio, client, "test", None, &mut reconnect, None).await;
        assert!(
            matches!(result, Ok(true)),
            "bridge responded → Ok(true), got {result:?}"
        );

        let sent = state.sent.lock();
        assert_eq!(sent.len(), 1, "bridge message must be forwarded to stdio");
        assert_eq!(sent[0]["id"], serde_json::json!(1));
        assert_eq!(sent[0]["result"], serde_json::json!({}));

        // Nothing forwarded to the bridge → no session → close() must be skipped.
        assert_eq!(
            state.attempts.load(Ordering::SeqCst),
            0,
            "no client.send should have occurred"
        );
        assert_eq!(
            state.closed.load(Ordering::SeqCst),
            0,
            "close() must not run on the forwarded_any == false path"
        );
    }

    #[tokio::test]
    async fn clean_stdio_eof_with_no_traffic_returns_false() {
        // Empty inbound on both sides: stdio EOFs immediately, the bridge never
        // responds. The loop breaks at once and the proxy reports bridge_responded
        // == false without attempting any teardown.
        let state = TestState::new();
        let stdio = state.stdio(VecDeque::new());
        let client = state.client(0);

        let mut reconnect = no_reconnect;
        let result = run_transport_proxy(stdio, client, "test", None, &mut reconnect, None).await;
        assert!(
            matches!(result, Ok(false)),
            "no bridge response → Ok(false), got {result:?}"
        );
        assert_eq!(state.sent.lock().len(), 0, "no messages should be sent");
        assert_eq!(
            state.closed.load(Ordering::SeqCst),
            0,
            "close() must be skipped"
        );
    }

    /// A client->server notification (no `id`), e.g. the standard
    /// `notifications/initialized` message. Used to exercise the forward-failure
    /// path when there is no request id to relay an error against.
    fn client_notification() -> RxJsonRpcMessage<RoleServer> {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .expect("valid client notification")
    }

    #[tokio::test]
    async fn stdio_send_failure_on_bridge_message_breaks_loop_without_marking_responded() {
        // The bridge delivers a response, but writing it back to stdio fails
        // (e.g. broken pipe). The proxy must exit the loop without ever setting
        // bridge_responded, and — since nothing was ever forwarded to the bridge
        // — must skip the client.close() teardown.
        let state = TestState::new();
        let mut stdio = state.stdio(VecDeque::new());
        stdio.eof_delay = Some(Duration::from_millis(50));
        stdio.fail_send = true;

        let mut client = state.client(0);
        client.inbound = VecDeque::from(vec![bridge_response(1)]);

        let mut reconnect = no_reconnect;
        let result = run_transport_proxy(stdio, client, "test", None, &mut reconnect, None).await;
        assert!(
            matches!(result, Ok(false)),
            "stdio write failure must not mark bridge_responded, got {result:?}"
        );
        assert_eq!(
            state.sent.lock().len(),
            0,
            "failed send must not be recorded"
        );
        assert_eq!(
            state.closed.load(Ordering::SeqCst),
            0,
            "nothing forwarded to bridge -> no session -> close() skipped"
        );
    }

    #[tokio::test]
    async fn client_close_err_is_non_fatal_after_forwarded_message() {
        // Once a message has been forwarded to the bridge (forwarded_any), the
        // proxy attempts a teardown close(). If close() itself errors, that must
        // be logged and swallowed (non-fatal) rather than surfaced as an Err.
        let state = TestState::new();
        let stdio = state.stdio(VecDeque::from(vec![client_request(1)]));
        let mut client = state.client(0);
        client.close_behavior = CloseBehavior::Err;

        let mut reconnect = no_reconnect;
        let result = run_transport_proxy(stdio, client, "test", None, &mut reconnect, None).await;
        assert!(
            matches!(result, Ok(false)),
            "close() error must be non-fatal, got {result:?}"
        );
        assert_eq!(
            state.attempts.load(Ordering::SeqCst),
            1,
            "request was forwarded"
        );
        assert_eq!(
            state.closed.load(Ordering::SeqCst),
            1,
            "close() must have been attempted"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn client_close_timeout_is_non_fatal_after_forwarded_message() {
        // close() hangs longer than the proxy's internal teardown timeout; the
        // proxy must give up on it (timeout branch) without surfacing an error.
        // Paused time lets tokio auto-advance past both the hang and the
        // timeout instantly instead of the test taking 6+ real seconds.
        let state = TestState::new();
        let stdio = state.stdio(VecDeque::from(vec![client_request(1)]));
        let mut client = state.client(0);
        client.close_behavior = CloseBehavior::Hang(Duration::from_secs(100));

        let mut reconnect = no_reconnect;
        let result = run_transport_proxy(stdio, client, "test", None, &mut reconnect, None).await;
        assert!(
            matches!(result, Ok(false)),
            "close() timeout must be non-fatal, got {result:?}"
        );
        assert_eq!(
            state.closed.load(Ordering::SeqCst),
            1,
            "close() must have been entered before timing out"
        );
    }

    #[tokio::test]
    async fn bridge_closed_connection_breaks_loop_after_forwarded_message() {
        // After a message is forwarded, the bridge closes its side of the
        // transport (`receive()` returns None). The proxy must exit the loop via
        // the "bridge connection closed" branch without ever marking
        // bridge_responded, then still attempt teardown (forwarded_any is true).
        let state = TestState::new();
        let mut stdio = state.stdio(VecDeque::from(vec![client_request(1)]));
        stdio.eof_delay = Some(Duration::from_millis(50));

        let mut client = state.client(0);
        client.receive_none_after = Some(2);

        let mut reconnect = no_reconnect;
        let result = run_transport_proxy(stdio, client, "test", None, &mut reconnect, None).await;
        assert!(
            matches!(result, Ok(false)),
            "bridge-initiated close must report bridge_responded == false, got {result:?}"
        );
        assert_eq!(
            state.attempts.load(Ordering::SeqCst),
            1,
            "request was forwarded"
        );
        assert_eq!(
            state.sent.lock().len(),
            0,
            "no bridge message was ever relayed to stdio"
        );
        assert_eq!(
            state.closed.load(Ordering::SeqCst),
            1,
            "forwarded_any == true -> close() must still be attempted"
        );
    }

    /// Claude Code 2.1.319's `server/discover` probe, captured on the wire
    /// (2026-11-06). The 2026-07-28 protocol carries the client's identity in
    /// `_meta` under namespaced keys. Missing them made the proxy guess the
    /// client from its parent processes, which fell through to "Antigravity",
    /// so every Claude Code session behind the hub was offered ahma's harness
    /// file tools (`grep_search`, …) and lost its `elicitation` capability.
    #[test]
    fn synthesized_initialize_keeps_namespaced_client_info_from_discover() {
        let probe = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "server-discover-probe-1",
            "method": "server/discover",
            "params": {"_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {
                    "name": "claude-code", "title": "Claude Code", "version": "2.1.319"
                },
                "io.modelcontextprotocol/clientCapabilities": {
                    "roots": {"listChanged": true},
                    "elicitation": {"form": {}, "url": {}}
                }
            }}
        });

        let init = synthesize_initialize_request(&probe);

        assert_eq!(
            init["params"]["clientInfo"]["name"], "claude-code",
            "{init:#}"
        );
        assert_eq!(
            init["params"]["capabilities"]["elicitation"],
            serde_json::json!({"form": {}, "url": {}}),
            "{init:#}"
        );
        let name = init["params"]["clientInfo"]["name"]
            .as_str()
            .unwrap_or_default();
        assert!(
            crate::client_type::McpClientType::from_client_name(name).has_native_file_tools(),
            "a Claude Code session must not be offered ahma's harness file tools"
        );
    }

    /// The live loop reads the probe back out of rmcp's parsed message, not the
    /// raw line, so the capabilities must survive that round trip too.
    #[test]
    fn discover_capabilities_survive_the_rmcp_round_trip() {
        let raw = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "server-discover-probe-1",
            "method": "server/discover",
            "params": {"_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {"name": "claude-code", "version": "2.1.331"},
                "io.modelcontextprotocol/clientCapabilities": {
                    "roots": {"listChanged": true},
                    "elicitation": {"form": {}, "url": {}}
                }
            }}
        });
        let parsed: RxJsonRpcMessage<RoleServer> = serde_json::from_value(raw).unwrap();
        let val = serde_json::to_value(&parsed).unwrap();

        let init = synthesize_initialize_request(&val);

        assert_eq!(
            init["params"]["capabilities"]["elicitation"],
            serde_json::json!({"form": {}, "url": {}}),
            "{val:#}\n{init:#}"
        );
    }

    /// The un-namespaced spellings the proxy already understood keep working.
    #[test]
    fn synthesized_initialize_keeps_plain_client_info_from_discover() {
        let probe = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "server/discover",
            "params": {"_meta": {
                "clientInfo": {"name": "cursor-vscode", "version": "1"},
                "clientCapabilities": {"roots": {}}
            }}
        });
        let init = synthesize_initialize_request(&probe);
        assert_eq!(init["params"]["clientInfo"]["name"], "cursor-vscode");
        assert_eq!(
            init["params"]["capabilities"],
            serde_json::json!({
                "roots": {},
                // SPEC R-HUB.4.3: added for the bridge, not from the client.
                "experimental": {ahma_common::mcp_methods::ROOTS_ON_FIRST_CALL_CAPABILITY: {}}
            })
        );
    }

    /// SPEC R-HUB.4.1: a 2026-07-28 probe is answered, and opens a modern
    /// session on the bridge: roots, prompts and listen streams now travel
    /// the way that protocol allows (R-HUB.4.3–4.5). It was declined while
    /// they did not (R-HUB.4.2). No test seam: this is the shipped default.
    #[tokio::test]
    async fn server_discover_is_answered_and_opens_a_modern_session() {
        let stdio = Scripted::new(
            vec![serde_json::to_value(client_discover_request(1)).unwrap()],
            |m| match m["id"].as_i64() {
                Some(1) => vec![None],
                _ => Vec::new(),
            },
        );
        let stdio_sent = stdio.sent.clone();
        let bridge = deferred_bridge();
        let bridge_sent = bridge.sent.clone();
        let mut reconnect = || -> Result<Scripted> { panic!("no reconnect in this test") };

        let result = tokio::time::timeout(
            ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick),
            run_transport_proxy(stdio, bridge, "test", None, &mut reconnect, None),
        )
        .await
        .expect("finishes");
        assert!(result.is_ok(), "{result:?}");

        let sent = stdio_sent.lock();
        let reply = sent
            .iter()
            .find(|m| m["id"] == 1)
            .unwrap_or_else(|| panic!("the probe is answered: {sent:#?}"));
        assert!(reply.get("error").is_none(), "{reply:#?}");
        assert_eq!(reply["result"]["resultType"], "complete");
        assert!(
            reply["result"]["supportedVersions"]
                .as_array()
                .is_some_and(|v| v.contains(&serde_json::json!("2026-07-28"))),
            "{reply:#?}"
        );
        let bridge_sent = bridge_sent.lock();
        assert_eq!(bridge_sent[0]["method"], INITIALIZE_METHOD);
        assert!(modern_input::defers_roots(&bridge_sent[0]));
    }

    #[tokio::test]
    async fn notification_without_id_forward_failure_not_relayed_but_counts_toward_teardown() {
        // A notification (no `id`) that fails to forward has no request id to
        // relay an error against, so nothing is sent back to stdio — but the
        // failure still counts toward MAX_CONSECUTIVE_FORWARD_FAILURES and the
        // transport is still torn down once the threshold is hit.
        let state = TestState::new();
        let stdio = state.stdio(VecDeque::from(vec![
            client_notification(),
            client_notification(),
            client_notification(),
            client_notification(),
        ]));
        let client = state.client(usize::MAX);

        let mut reconnect = no_reconnect;
        let result = run_transport_proxy(stdio, client, "test", None, &mut reconnect, None).await;
        assert!(result.is_ok());
        assert_eq!(
            state.attempts.load(Ordering::SeqCst),
            MAX_CONSECUTIVE_FORWARD_FAILURES as usize,
            "gives up after MAX_CONSECUTIVE_FORWARD_FAILURES; 4th notification never tried"
        );
        assert_eq!(
            state.sent.lock().len(),
            0,
            "notifications have no id, so no error can be relayed to stdio"
        );
    }

    #[tokio::test]
    async fn a_threshold_reconnect_leaves_a_session_the_teardown_closes() {
        // The replay delivered `initialize` on the fresh transport, so a
        // session exists there even though every forward before the
        // reconnect failed. The teardown must close it (DELETE /mcp), and the
        // handshake deadline — which watches the same flag — must not end it.
        with_zero_backoff(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let stdio = MockStdio::new(
                VecDeque::from(vec![
                    client_initialize(0),
                    client_request(1),
                    client_request(2),
                ]),
                sent.clone(),
            );
            let fresh_closed = Arc::new(AtomicUsize::new(0));
            let fresh_closed_inner = fresh_closed.clone();
            let mut reconnect = move || -> Result<MockClient> {
                Ok(MockClient {
                    closed: fresh_closed_inner.clone(),
                    ..healthy_client(Arc::new(AtomicUsize::new(0)))
                })
            };

            let result =
                run_transport_proxy(stdio, dead_client(), "test", None, &mut reconnect, None).await;
            assert!(result.is_ok_and(|responded| responded));
            assert_eq!(
                session_event_kinds(&sent.lock()),
                vec!["reconnected".to_string()]
            );
            assert_eq!(
                fresh_closed.load(Ordering::SeqCst),
                1,
                "the adopted transport holds a session; the teardown must close it"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_reconnect_while_disconnected_delivers_initialized_once() {
        // The bridge answers `initialize`, then closes before the client's
        // `notifications/initialized` arrives (it idled out, or handed over).
        // The replay on the fresh transport sends the cached notification;
        // the client's own copy must not follow it.
        with_zero_backoff(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let mut stdio = MockStdio::new(
                VecDeque::from(vec![
                    client_initialize(0),
                    client_notifications_initialized(),
                ]),
                sent.clone(),
            );
            stdio.inbound_delay = Some(Duration::from_millis(50));
            let first = MockClient {
                inbound: VecDeque::from(vec![bridge_response(0)]),
                receive_none_after: Some(1),
                ..healthy_client(Arc::new(AtomicUsize::new(0)))
            };
            let fresh_sent = Arc::new(Mutex::new(Vec::new()));
            let fresh_sent_inner = fresh_sent.clone();
            let mut reconnect = move || -> Result<MockClient> {
                Ok(MockClient {
                    sent: Some(fresh_sent_inner.clone()),
                    ..healthy_client(Arc::new(AtomicUsize::new(0)))
                })
            };

            let result =
                run_transport_proxy(stdio, first, "test", None, &mut reconnect, None).await;
            assert!(result.is_ok());
            let methods: Vec<String> = fresh_sent
                .lock()
                .iter()
                .map(|m| m["method"].as_str().unwrap_or_default().to_string())
                .collect();
            assert_eq!(
                methods,
                vec!["initialize".to_string(), INITIALIZED_METHOD.to_string()],
                "the replay delivers each handshake message exactly once"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_gone_endpoint_reconnect_whose_resend_fails_leaves_a_session_the_teardown_closes() {
        // The gone-endpoint recovery adopted a fresh transport and the replay
        // opened a session on it; then resending the failed request failed
        // too. That session still exists, so the teardown must close it — the
        // rule every reconnect path follows
        // (`a_threshold_reconnect_leaves_a_session_the_teardown_closes`).
        // `initialize` itself fails first, so nothing was ever forwarded
        // before the reconnect: only adopting the fresh transport can mark it.
        with_zero_backoff(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let stdio = MockStdio::new(
                VecDeque::from(vec![client_initialize(0), client_request(1)]),
                sent.clone(),
            );
            // `initialize` is a replayed handshake message, so its gone-endpoint
            // failure takes the failure-count path; request 1's takes the fast
            // path.
            let gone = MockClient {
                send_error_text: GONE_ENDPOINT_SEND_ERROR,
                ..dead_client()
            };
            let fresh_closed = Arc::new(AtomicUsize::new(0));
            let fresh_closed_inner = fresh_closed.clone();
            let mut reconnect = move || -> Result<MockClient> {
                Ok(MockClient {
                    // Send 1 is the replayed `initialize`; send 2, the resend,
                    // fails.
                    fail_sends_from: 2,
                    closed: fresh_closed_inner.clone(),
                    ..healthy_client(Arc::new(AtomicUsize::new(0)))
                })
            };

            let result = run_transport_proxy(stdio, gone, "test", None, &mut reconnect, None).await;
            assert!(result.is_ok_and(|responded| responded));
            assert_eq!(
                fresh_closed.load(Ordering::SeqCst),
                1,
                "the adopted transport holds a session; the teardown must close it"
            );
            let sent = sent.lock();
            assert!(
                sent.iter()
                    .any(|m| m["id"] == serde_json::json!(1) && m.get("error").is_some()),
                "the request whose resend failed is answered with an error: {sent:?}"
            );
            assert_eq!(
                session_event_kinds(&sent),
                vec!["reconnected".to_string()],
                "got {sent:?}"
            );
            let event = sent
                .iter()
                .find(|m| {
                    m.get("method").and_then(|m| m.as_str())
                        == Some(ahma_common::session_event::SESSION_EVENT_METHOD)
                })
                .unwrap();
            assert_eq!(event["params"]["detail"]["cause"], "endpoint_gone");
            assert_eq!(
                event["params"]["detail"]["in_flight_request"],
                "answered_with_error"
            );
        })
        .await;
    }

    type Reaction = dyn Fn(&serde_json::Value) -> Vec<Option<serde_json::Value>> + Send + Sync;

    /// A peer that answers what it is sent: every message the proxy sends it
    /// is recorded and handed to `react`, whose replies `receive` then yields
    /// in order. A `None` reply ends the stream (EOF). Stands in for either
    /// side, so a whole exchange can be scripted without timing tricks.
    struct Scripted {
        sent: Arc<Mutex<Vec<serde_json::Value>>>,
        tx: tokio::sync::mpsc::UnboundedSender<Option<serde_json::Value>>,
        rx: tokio::sync::mpsc::UnboundedReceiver<Option<serde_json::Value>>,
        react: Arc<Reaction>,
    }
    impl Scripted {
        fn new(
            initial: Vec<serde_json::Value>,
            react: impl Fn(&serde_json::Value) -> Vec<Option<serde_json::Value>> + Send + Sync + 'static,
        ) -> Self {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            for message in initial {
                tx.send(Some(message)).expect("open channel");
            }
            Self {
                sent: Arc::new(Mutex::new(Vec::new())),
                tx,
                rx,
                react: Arc::new(react),
            }
        }
        fn record(&self, item: serde_json::Value) {
            for reply in (self.react)(&item) {
                self.tx.send(reply).expect("open channel");
            }
            self.sent.lock().push(item);
        }
        async fn next<T: serde::de::DeserializeOwned>(&mut self) -> Option<T> {
            let value = self.rx.recv().await.flatten()?;
            Some(serde_json::from_value(value).expect("a scripted message deserializes"))
        }
    }
    impl Transport<RoleServer> for Scripted {
        type Error = std::io::Error;
        fn send(
            &mut self,
            item: TxJsonRpcMessage<RoleServer>,
        ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
            self.record(serde_json::to_value(item).unwrap());
            std::future::ready(Ok(()))
        }
        async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
            self.next().await
        }
        async fn close(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }
    impl Transport<RoleClient> for Scripted {
        type Error = std::io::Error;
        fn send(
            &mut self,
            item: TxJsonRpcMessage<RoleClient>,
        ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
            self.record(serde_json::to_value(item).unwrap());
            std::future::ready(Ok(()))
        }
        async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
            self.next().await
        }
        async fn close(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// A 2026-07-28 `tools/call`: capabilities travel per request in `_meta`.
    fn modern_tools_call(id: i64, name: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": {"command": "true"},
                "_meta": {"io.modelcontextprotocol/clientCapabilities": {"roots": {}}}
            }
        });
        if let serde_json::Value::Object(extra) = extra {
            call["params"].as_object_mut().unwrap().extend(extra);
        }
        call
    }

    const MODERN_ROOTS: &str = "file:///workspace";

    /// The bridge as a deferred-roots session sees it: answers `initialize`,
    /// asks for roots only when told they changed, and answers tool calls.
    fn deferred_bridge() -> Scripted {
        Scripted::new(Vec::new(), |m| {
            let id = m.get("id").cloned();
            match m.get("method").and_then(|m| m.as_str()) {
                Some(INITIALIZE_METHOD) => vec![Some(serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {"tools": {"listChanged": true}},
                        "serverInfo": {"name": "ahma", "version": "0.1.0"}
                    }
                }))],
                Some(ahma_common::mcp_methods::ROOTS_LIST_CHANGED_METHOD) => {
                    vec![Some(serde_json::json!({
                        "jsonrpc": "2.0", "id": 50, "method": ROOTS_LIST_METHOD
                    }))]
                }
                Some("tools/call") => vec![Some(serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"content": [{"type": "text", "text": "ok"}]}
                }))],
                _ => Vec::new(),
            }
        })
    }

    async fn run_scripted(stdio: Scripted, bridge: Scripted) {
        let mut reconnect = || -> Result<Scripted> { panic!("no reconnect in this test") };
        let run = run_transport_proxy(stdio, bridge, "test", None, &mut reconnect, None);
        let outcome = tokio::time::timeout(
            ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick),
            run,
        )
        .await
        .expect("the scripted exchange finishes");
        assert!(outcome.is_ok(), "{outcome:?}");
    }

    /// Any message with both a `method` and an `id` — a server request, which a
    /// 2026-07-28 server may not write to stdout (SPEC R-HUB.4.2).
    fn server_requests(sent: &[serde_json::Value]) -> Vec<&serde_json::Value> {
        sent.iter()
            .filter(|m| m.get("method").is_some() && m.get("id").is_some())
            .collect()
    }

    /// SPEC R-HUB.4.3: over 2026-07-28 the scope comes from roots the client
    /// returns in reply to an `InputRequiredResult` on its first tool call. The
    /// call that carried the answer is the one that runs; the bridge never
    /// sees the unanswered first attempt, and nothing reaches stdout but
    /// results.
    #[tokio::test]
    async fn modern_client_roots_arrive_via_input_required_and_never_as_stdio_request() {
        let stdio = Scripted::new(
            vec![serde_json::to_value(client_discover_request(1)).unwrap()],
            |m| match m["id"].as_i64() {
                Some(1) => vec![Some(modern_tools_call(
                    2,
                    "run_terminal_command",
                    serde_json::json!({}),
                ))],
                Some(2) => {
                    let result = &m["result"];
                    assert_eq!(result["resultType"], "input_required", "{m:#?}");
                    assert_eq!(
                        result["inputRequests"]["ahma.roots"]["method"],
                        ROOTS_LIST_METHOD
                    );
                    vec![Some(modern_tools_call(
                        3,
                        "run_terminal_command",
                        serde_json::json!({
                            "inputResponses": {"ahma.roots": {"roots": [{"uri": MODERN_ROOTS}]}},
                            "requestState": result["requestState"].clone(),
                        }),
                    ))]
                }
                Some(3) => vec![None],
                _ => Vec::new(),
            },
        );
        let stdio_sent = stdio.sent.clone();
        let bridge = deferred_bridge();
        let bridge_sent = bridge.sent.clone();

        run_scripted(stdio, bridge).await;

        let stdio_sent = stdio_sent.lock();
        assert!(
            server_requests(&stdio_sent).is_empty(),
            "no server request on a modern client's stdout: {stdio_sent:#?}"
        );
        let answer = stdio_sent
            .iter()
            .find(|m| m["id"] == 3)
            .unwrap_or_else(|| panic!("the retried call is answered: {stdio_sent:#?}"));
        assert_eq!(answer["result"]["content"][0]["text"], "ok");

        let bridge_sent = bridge_sent.lock();
        let init = &bridge_sent[0];
        assert_eq!(init["method"], INITIALIZE_METHOD);
        assert!(
            init["params"]["capabilities"]["experimental"]
                [ahma_common::mcp_methods::ROOTS_ON_FIRST_CALL_CAPABILITY]
                .is_object(),
            "the bridge is told roots come with the first call: {init:#?}"
        );
        let roots_answer = bridge_sent
            .iter()
            .find(|m| m["id"] == 50)
            .unwrap_or_else(|| panic!("the bridge's roots/list is answered: {bridge_sent:#?}"));
        assert_eq!(roots_answer["result"]["roots"][0]["uri"], MODERN_ROOTS);
        let calls: Vec<&serde_json::Value> = bridge_sent
            .iter()
            .filter(|m| m["method"] == "tools/call")
            .collect();
        assert_eq!(calls.len(), 1, "only the answered retry runs: {calls:#?}");
        assert_eq!(calls[0]["id"], 3);
        assert!(calls[0]["params"].get("inputResponses").is_none());
        assert!(calls[0]["params"].get("requestState").is_none());
        let roots_answer_at = bridge_sent.iter().position(|m| m["id"] == 50).unwrap();
        let call_at = bridge_sent
            .iter()
            .position(|m| m["method"] == "tools/call")
            .unwrap();
        assert!(
            roots_answer_at < call_at,
            "the scope is settled before the call it gates"
        );
    }

    /// SPEC R-HUB.4.2: what the bridge asks a modern client is answered for
    /// it, never written to its stdout. (Prompts: R-HUB.4.5, tested below.)
    #[tokio::test]
    async fn modern_client_stdout_never_carries_a_server_request() {
        let stdio = Scripted::new(
            vec![serde_json::to_value(client_discover_request(1)).unwrap()],
            |m| match m["id"].as_i64() {
                Some(1) => vec![Some(modern_tools_call(
                    2,
                    "ahma_help",
                    serde_json::json!({}),
                ))],
                Some(2) => vec![None],
                _ => Vec::new(),
            },
        );
        let stdio_sent = stdio.sent.clone();
        let bridge = Scripted::new(Vec::new(), |m| {
            let id = m.get("id").cloned();
            match m.get("method").and_then(|m| m.as_str()) {
                Some(INITIALIZE_METHOD) => vec![Some(
                    serde_json::to_value(bridge_initialize_response(0)).unwrap(),
                )],
                // A bridge that asks the legacy way, before any tool call.
                Some(INITIALIZED_METHOD) => vec![
                    Some(serde_json::json!({"jsonrpc": "2.0", "id": 60, "method": "ping"})),
                    Some(
                        serde_json::json!({"jsonrpc": "2.0", "id": 62, "method": ROOTS_LIST_METHOD}),
                    ),
                ],
                Some("tools/call") => vec![Some(serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "result": {"content": []}
                }))],
                _ => Vec::new(),
            }
        });
        let bridge_sent = bridge.sent.clone();

        run_scripted(stdio, bridge).await;

        let stdio_sent = stdio_sent.lock();
        assert!(
            server_requests(&stdio_sent).is_empty(),
            "no server request on a modern client's stdout: {stdio_sent:#?}"
        );
        let bridge_sent = bridge_sent.lock();
        let answer = |id: i64| {
            bridge_sent
                .iter()
                .find(|m| m["id"] == id && m.get("method").is_none())
        };
        assert_eq!(
            answer(60).expect("ping answered")["result"],
            serde_json::json!({})
        );
        assert!(
            answer(62).is_none(),
            "roots/list waits for roots the client has not sent yet: {bridge_sent:#?}"
        );
        assert!(
            bridge_sent
                .iter()
                .any(|m| m["id"] == 2 && m["method"] == "tools/call"),
            "a scope-exempt tool runs without roots"
        );
    }

    /// SPEC R-HUB.4.3: a reconnected deferred-roots session is not asked for
    /// roots at handshake time either, so the replay tells it they changed
    /// and answers the `roots/list` that follows from the cache.
    #[tokio::test]
    async fn replay_announces_cached_roots_to_a_deferred_roots_session() {
        let mut init = synthesize_initialize_request(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "server/discover",
            "params": {"_meta": {"io.modelcontextprotocol/clientCapabilities": {"roots": {}}}}
        }));
        init["id"] = serde_json::json!(0);
        let handshake = CachedHandshake {
            init_request: Some(init),
            notif_initialized: Some(
                serde_json::to_value(client_notifications_initialized()).unwrap(),
            ),
            roots_response: Some(serde_json::to_value(client_roots_list_response(0)).unwrap()),
            ..Default::default()
        };
        let mut bridge = deferred_bridge();
        let sent = bridge.sent.clone();

        tokio::time::timeout(
            ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick),
            replay_handshake(&mut bridge, &handshake),
        )
        .await
        .expect("the replay does not wait for a roots/list that never comes")
        .expect("replay succeeds");

        let sent = sent.lock();
        let methods: Vec<&str> = sent
            .iter()
            .map(|m| m["method"].as_str().unwrap_or("(response)"))
            .collect();
        assert_eq!(
            methods,
            vec![
                INITIALIZE_METHOD,
                INITIALIZED_METHOD,
                ahma_common::mcp_methods::ROOTS_LIST_CHANGED_METHOD,
                "(response)"
            ]
        );
        assert_eq!(sent[3]["id"], 50);
        assert_eq!(sent[3]["result"]["roots"][0]["uri"], "file:///workspace");
    }

    const SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";

    /// SPEC R-HUB.4.4: `subscriptions/listen` is acknowledged before anything
    /// is sent on it, stays open, and ends with a `complete` result when the
    /// client cancels it. `tools/list_changed` reaches the client only on that
    /// stream, tagged with its id; notifications the client never asked for
    /// do not reach it at all, and every result says what kind it is.
    #[tokio::test]
    async fn subscriptions_listen_is_acknowledged_and_held_open() {
        let stdio = Scripted::new(
            vec![serde_json::to_value(client_discover_request(1)).unwrap()],
            |m| {
                if m["method"] == "notifications/subscriptions/acknowledged" {
                    return vec![Some(modern_tools_call(
                        3,
                        "ahma_help",
                        serde_json::json!({}),
                    ))];
                }
                match m["id"].as_i64() {
                    Some(1) => vec![Some(serde_json::json!({
                        "jsonrpc": "2.0", "id": 2, "method": SUBSCRIPTIONS_LISTEN_METHOD,
                        "params": {
                            "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"},
                            "notifications": {"toolsListChanged": true, "resourcesListChanged": true}
                        }
                    }))],
                    Some(3) => vec![Some(serde_json::json!({
                        "jsonrpc": "2.0", "method": "notifications/cancelled",
                        "params": {"requestId": 2}
                    }))],
                    Some(2) => vec![None],
                    _ => Vec::new(),
                }
            },
        );
        let stdio_sent = stdio.sent.clone();
        let bridge = Scripted::new(Vec::new(), |m| {
            let id = m.get("id").cloned();
            match m.get("method").and_then(|m| m.as_str()) {
                Some(INITIALIZE_METHOD) => vec![Some(
                    serde_json::to_value(bridge_initialize_response(0)).unwrap(),
                )],
                Some("tools/call") => vec![
                    Some(
                        serde_json::json!({"jsonrpc": "2.0", "method": TOOLS_LIST_CHANGED_METHOD}),
                    ),
                    Some(serde_json::json!({
                        "jsonrpc": "2.0", "method": "notifications/message",
                        "params": {"level": "info", "data": "unasked"}
                    })),
                    Some(serde_json::json!({
                        "jsonrpc": "2.0", "method": "notifications/progress",
                        "params": {"progressToken": "t", "progress": 1}
                    })),
                    Some(
                        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"content": []}}),
                    ),
                ],
                _ => Vec::new(),
            }
        });
        let bridge_sent = bridge.sent.clone();

        run_scripted(stdio, bridge).await;

        let sent = stdio_sent.lock();
        let discover = sent
            .iter()
            .find(|m| m["id"] == 1)
            .expect("discover answered");
        assert_eq!(discover["result"]["resultType"], "complete");
        assert!(discover["result"]["ttlMs"].is_u64(), "{discover:#?}");
        assert!(
            ["public", "private"]
                .contains(&discover["result"]["cacheScope"].as_str().unwrap_or("")),
            "{discover:#?}"
        );

        let position = |pred: &dyn Fn(&serde_json::Value) -> bool| sent.iter().position(pred);
        let ack_at = position(&|m| m["method"] == "notifications/subscriptions/acknowledged")
            .unwrap_or_else(|| panic!("listen acknowledged: {sent:#?}"));
        let ack = &sent[ack_at];
        assert_eq!(ack["params"]["_meta"][SUBSCRIPTION_ID], 2);
        assert_eq!(
            ack["params"]["notifications"],
            serde_json::json!({"toolsListChanged": true}),
            "only what ahma honours is acknowledged"
        );

        let changed: Vec<&serde_json::Value> = sent
            .iter()
            .filter(|m| m["method"] == TOOLS_LIST_CHANGED_METHOD)
            .collect();
        assert_eq!(changed.len(), 1, "{sent:#?}");
        assert_eq!(changed[0]["params"]["_meta"][SUBSCRIPTION_ID], 2);
        assert!(
            sent.iter().all(|m| m["method"] != "notifications/message"),
            "nothing the client did not ask for: {sent:#?}"
        );
        assert!(sent.iter().any(|m| m["method"] == "notifications/progress"));

        let call = sent.iter().find(|m| m["id"] == 3).expect("call answered");
        assert_eq!(call["result"]["resultType"], "complete");
        let listen_done_at = position(&|m| m["id"] == 2).expect("listen completed");
        let call_at = position(&|m| m["id"] == 3).unwrap();
        assert!(call_at < listen_done_at, "held open until cancelled");
        assert_eq!(sent[listen_done_at]["result"]["resultType"], "complete");
        assert_eq!(sent[listen_done_at]["result"]["_meta"][SUBSCRIPTION_ID], 2);

        let bridge_sent = bridge_sent.lock();
        assert!(
            bridge_sent
                .iter()
                .all(|m| m["id"] != 2 && m["method"] != "notifications/cancelled"),
            "the stream is the proxy's; the bridge never sees it: {bridge_sent:#?}"
        );
    }

    /// A 2026-07-28 client may fetch `server/discover` again. The session it
    /// opened already exists: a second `initialize` would replace it.
    #[tokio::test]
    async fn a_repeated_discover_is_answered_without_a_second_initialize() {
        let stdio = Scripted::new(
            vec![serde_json::to_value(client_discover_request(1)).unwrap()],
            |m| match m["id"].as_i64() {
                Some(1) => vec![Some(
                    serde_json::to_value(client_discover_request(2)).unwrap(),
                )],
                Some(2) => vec![None],
                _ => Vec::new(),
            },
        );
        let stdio_sent = stdio.sent.clone();
        let bridge = deferred_bridge();
        let bridge_sent = bridge.sent.clone();

        run_scripted(stdio, bridge).await;

        let sent = stdio_sent.lock();
        let first = sent.iter().find(|m| m["id"] == 1).unwrap();
        let second = sent
            .iter()
            .find(|m| m["id"] == 2)
            .unwrap_or_else(|| panic!("the repeat is answered: {sent:#?}"));
        assert_eq!(first["result"], second["result"]);
        let initializes = bridge_sent
            .lock()
            .iter()
            .filter(|m| m["method"] == INITIALIZE_METHOD)
            .count();
        assert_eq!(initializes, 1);
    }

    /// SPEC R-HUB.4.6: the bridge and worker run the 2025-11-25 session the
    /// synthesized `initialize` opened. A request that still names 2026-07-28
    /// in its `_meta` makes rmcp refuse every server request the worker sends
    /// while handling it — the `sandbox_grant` prompt and `await`'s liveness
    /// ping, as Claude Code 2.1.331 showed live — so the proxy drops that
    /// envelope and keeps the rest of `_meta`.
    #[tokio::test]
    async fn a_modern_request_envelope_never_reaches_the_bridge() {
        let envelope = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {"name": "claude-code", "version": "2.1.331"},
            "io.modelcontextprotocol/clientCapabilities": {
                "roots": {}, "elicitation": {"form": {}}
            },
            "progressToken": 7
        });
        let list = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list",
            "params": {"_meta": envelope.clone()}
        });
        let call = serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "sandbox_grant", "arguments": {}, "_meta": envelope}
        });
        let stdio = Scripted::new(
            vec![serde_json::to_value(client_discover_request(1)).unwrap()],
            move |m| match m["id"].as_i64() {
                Some(1) => vec![Some(list.clone())],
                Some(2) => vec![Some(call.clone())],
                Some(3) => vec![None],
                _ => Vec::new(),
            },
        );
        let bridge = Scripted::new(Vec::new(), |m| {
            match m.get("method").and_then(|m| m.as_str()) {
                Some(INITIALIZE_METHOD) => vec![Some(
                    serde_json::to_value(bridge_initialize_response(0)).unwrap(),
                )],
                Some("tools/list") => vec![Some(serde_json::json!({
                    "jsonrpc": "2.0", "id": m["id"], "result": {"tools": []}
                }))],
                Some("tools/call") => vec![Some(serde_json::json!({
                    "jsonrpc": "2.0", "id": m["id"],
                    "result": {"content": [{"type": "text", "text": "ok"}]}
                }))],
                _ => Vec::new(),
            }
        });
        let bridge_sent = bridge.sent.clone();

        run_scripted(stdio, bridge).await;

        let bridge_sent = bridge_sent.lock();
        for method in ["tools/list", "tools/call"] {
            let request = bridge_sent
                .iter()
                .find(|m| m["method"] == method)
                .unwrap_or_else(|| panic!("{method} reaches the bridge: {bridge_sent:#?}"));
            let meta = &request["params"]["_meta"];
            for key in [
                "io.modelcontextprotocol/protocolVersion",
                "io.modelcontextprotocol/clientInfo",
                "io.modelcontextprotocol/clientCapabilities",
            ] {
                assert!(
                    meta.get(key).is_none(),
                    "{method} still carries {key}: {request:#}"
                );
            }
            assert_eq!(
                meta["progressToken"], 7,
                "{method} keeps the rest: {request:#}"
            );
        }
    }

    const PROMPT_MESSAGE: &str = "Allow writes to /x?";

    fn bridge_elicitation(id: i64) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "elicitation/create",
            "params": {
                "message": PROMPT_MESSAGE,
                "requestedSchema": {"type": "object", "properties": {"ok": {"type": "boolean"}}}
            }
        })
    }

    /// The one input request in an `input_required` result, as (key, request).
    fn only_input_request(result: &serde_json::Value) -> (String, serde_json::Value) {
        assert_eq!(result["resultType"], "input_required", "{result:#?}");
        let requests = result["inputRequests"].as_object().expect("inputRequests");
        assert_eq!(requests.len(), 1, "{result:#?}");
        let (key, request) = requests.iter().next().unwrap();
        (key.clone(), request.clone())
    }

    fn retry_with(
        id: i64,
        name: &str,
        key: &str,
        answer: serde_json::Value,
        state: &serde_json::Value,
    ) -> serde_json::Value {
        modern_tools_call(
            id,
            name,
            serde_json::json!({"inputResponses": {key: answer}, "requestState": state}),
        )
    }

    /// SPEC R-HUB.4.5: a prompt raised while a call runs is that call's
    /// `input_required`. The retry carries the human's answer; the bridge,
    /// still running the original, gets the answer, and its result comes back
    /// as the retry's. The tool runs once.
    #[tokio::test]
    async fn an_approval_prompt_during_a_call_reaches_the_client_as_input_required() {
        let stdio = Scripted::new(
            vec![serde_json::to_value(client_discover_request(1)).unwrap()],
            |m| match m["id"].as_i64() {
                Some(1) => vec![Some(modern_tools_call(
                    2,
                    "sandbox_grant",
                    serde_json::json!({}),
                ))],
                Some(2) => {
                    let (key, request) = only_input_request(&m["result"]);
                    assert_eq!(request["method"], "elicitation/create");
                    assert_eq!(request["params"]["message"], PROMPT_MESSAGE);
                    vec![Some(retry_with(
                        3,
                        "sandbox_grant",
                        &key,
                        serde_json::json!({"action": "accept", "content": {"ok": true}}),
                        &m["result"]["requestState"],
                    ))]
                }
                Some(3) => vec![None],
                _ => Vec::new(),
            },
        );
        let stdio_sent = stdio.sent.clone();
        let bridge = Scripted::new(Vec::new(), |m| {
            if m["id"] == 70 {
                let action = m["result"]["action"].as_str().unwrap_or("none").to_string();
                return vec![Some(serde_json::json!({
                    "jsonrpc": "2.0", "id": 2,
                    "result": {"content": [{"type": "text", "text": action}]}
                }))];
            }
            match m.get("method").and_then(|m| m.as_str()) {
                Some(INITIALIZE_METHOD) => vec![Some(
                    serde_json::to_value(bridge_initialize_response(0)).unwrap(),
                )],
                Some("tools/call") => vec![Some(bridge_elicitation(70))],
                _ => Vec::new(),
            }
        });
        let bridge_sent = bridge.sent.clone();

        run_scripted(stdio, bridge).await;

        let sent = stdio_sent.lock();
        assert!(server_requests(&sent).is_empty(), "{sent:#?}");
        let answer = sent
            .iter()
            .find(|m| m["id"] == 3)
            .unwrap_or_else(|| panic!("the retry gets the original's result: {sent:#?}"));
        assert_eq!(answer["result"]["content"][0]["text"], "accept");
        assert_eq!(answer["result"]["resultType"], "complete");

        let bridge_sent = bridge_sent.lock();
        let calls = bridge_sent
            .iter()
            .filter(|m| m["method"] == "tools/call")
            .count();
        assert_eq!(calls, 1, "the tool runs once: {bridge_sent:#?}");
        let reply = bridge_sent
            .iter()
            .find(|m| m["id"] == 70)
            .expect("the prompt is answered");
        assert_eq!(reply["result"]["action"], "accept");
        assert_eq!(reply["result"]["content"]["ok"], true);
    }

    /// SPEC R-HUB.4.5: a prompt raised while no call runs (an async command
    /// asking for network access) rides the client's next call. Its retry,
    /// which the bridge never saw, is the call that runs.
    #[tokio::test]
    async fn an_approval_prompt_with_no_call_in_flight_rides_the_next_call() {
        let stdio = Scripted::new(
            vec![serde_json::to_value(client_discover_request(1)).unwrap()],
            |m| {
                if m["method"] == "notifications/progress" {
                    return vec![Some(modern_tools_call(3, "await", serde_json::json!({})))];
                }
                match m["id"].as_i64() {
                    Some(1) => vec![Some(modern_tools_call(
                        2,
                        "ahma_help",
                        serde_json::json!({}),
                    ))],
                    Some(3) => {
                        let (key, _) = only_input_request(&m["result"]);
                        vec![Some(retry_with(
                            4,
                            "await",
                            &key,
                            serde_json::json!({"action": "decline"}),
                            &m["result"]["requestState"],
                        ))]
                    }
                    Some(4) => vec![None],
                    _ => Vec::new(),
                }
            },
        );
        let stdio_sent = stdio.sent.clone();
        let bridge = Scripted::new(Vec::new(), |m| {
            let id = m.get("id").cloned();
            match m.get("method").and_then(|m| m.as_str()) {
                Some(INITIALIZE_METHOD) => vec![Some(
                    serde_json::to_value(bridge_initialize_response(0)).unwrap(),
                )],
                Some("tools/call") if id == Some(serde_json::json!(2)) => vec![
                    Some(serde_json::json!({"jsonrpc": "2.0", "id": 2, "result": {"content": []}})),
                    Some(bridge_elicitation(71)),
                    // What the client reacts to, so its next call comes after the prompt.
                    Some(serde_json::json!({
                        "jsonrpc": "2.0", "method": "notifications/progress",
                        "params": {"progressToken": "t", "progress": 1}
                    })),
                ],
                Some("tools/call") => vec![Some(serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": "done"}]}
                }))],
                _ => Vec::new(),
            }
        });
        let bridge_sent = bridge.sent.clone();

        run_scripted(stdio, bridge).await;

        let sent = stdio_sent.lock();
        assert!(server_requests(&sent).is_empty(), "{sent:#?}");
        assert_eq!(
            sent.iter().find(|m| m["id"] == 4).expect("retry answered")["result"]["content"][0]["text"],
            "done"
        );
        let bridge_sent = bridge_sent.lock();
        let reply = bridge_sent
            .iter()
            .find(|m| m["id"] == 71)
            .unwrap_or_else(|| panic!("the prompt is answered: {bridge_sent:#?}"));
        assert_eq!(reply["result"]["action"], "decline");
        let calls: Vec<&serde_json::Value> = bridge_sent
            .iter()
            .filter(|m| m["method"] == "tools/call")
            .collect();
        assert_eq!(
            calls.iter().map(|c| c["id"].clone()).collect::<Vec<_>>(),
            vec![serde_json::json!(2), serde_json::json!(4)],
            "the asked-on call never runs; its retry does"
        );
        assert!(calls[1]["params"].get("inputResponses").is_none());
        let reply_at = bridge_sent.iter().position(|m| m["id"] == 71).unwrap();
        let retry_at = bridge_sent.iter().position(|m| m["id"] == 4).unwrap();
        assert!(reply_at < retry_at, "answered before the call goes on");
    }
}
