//! # Stdio MCP Proxy Client
//!
//! When the localhost bridge server (UDS or HTTP) is already running,
//! this module acts as a transparent proxy that forwards all stdio
//! JSON-RPC traffic to the running server.

use crate::transport_patch::PatchedStdioTransport;
use ahma_common::http_retry::{Idempotency, RetryPolicy, ServiceError, send_with_retry};
#[cfg(unix)]
use ahma_common::mcp_methods::ROOTS_LIST_METHOD;
use ahma_common::mcp_methods::{
    INITIALIZE_METHOD, INITIALIZED_METHOD, SERVER_DISCOVER_METHOD, SERVER_INSTRUCTIONS,
    SUBSCRIPTIONS_LISTEN_METHOD,
};
use ahma_common::mcp_protocol::{MCP_PROTOCOL_VERSION_2025_11_25, MCP_PROTOCOL_VERSION_2026_07_28};
use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use rmcp::model::{CustomResult, RequestId, ServerResult};
#[cfg(unix)]
use rmcp::service::RoleClient;
use rmcp::service::{RoleServer, TxJsonRpcMessage};
use rmcp::transport::Transport;
use std::time::Duration;
use tokio::sync::mpsc;

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

/// How many reconnect attempts the proxy makes against a genuinely-dead bridge
/// transport before giving up and exiting for good. Each attempt rebuilds the
/// bridge connection and replays the cached handshake; a transient bridge
/// restart typically recovers within one or two attempts.
const MAX_RECONNECT_ATTEMPTS: u32 = 3;

/// The handshake messages captured the first time they flow through the proxy,
/// so a dead bridge transport can be reconnected *invisibly to the downstream
/// client*: the client (Claude Code, Cursor, …) already sent `initialize` and
/// answered `roots/list` once and does not expect — and in practice will not
/// resend — either on a mid-session reconnect. The proxy replays its own cached
/// copies against a freshly built bridge connection instead.
#[cfg(unix)]
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
}

#[cfg(unix)]
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
#[cfg(unix)]
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
    client
        .receive()
        .await
        .ok_or_else(|| anyhow!("reconnect: bridge closed before answering initialize"))?;

    if let Some(notif) = &handshake.notif_initialized {
        let notif_msg: TxJsonRpcMessage<RoleClient> = serde_json::from_value(notif.clone())
            .context("reconnect: cached notifications/initialized no longer deserializes")?;
        client
            .send(notif_msg)
            .await
            .map_err(|e| anyhow!("reconnect: failed to resend notifications/initialized: {e:?}"))?;
    }

    if let Some(roots_response) = &handshake.roots_response {
        tokio::time::timeout(
            Duration::from_secs(15),
            wait_and_answer_roots_list(client, roots_response),
        )
        .await
        .map_err(|_| anyhow!("reconnect: timed out waiting for roots/list"))??;
    }
    Ok(())
}

#[cfg(unix)]
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
#[cfg(unix)]
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

/// Deserialize one session-event notification and send it downstream,
/// logging (never propagating) either failure: a malformed notification or a
/// failed send must not affect the session that was just saved.
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
fn error_text_indicates_gone_endpoint(text: &str) -> bool {
    text.contains("No such file or directory")
        || text.contains("Connection refused")
        || text.contains("(os error 2)")
        || text.contains("(os error 61)")
        // Debug renderings of a nested io error keep the kind, not the message.
        || text.contains("kind: NotFound")
        || text.contains("kind: ConnectionRefused")
}

/// True when a reconnect failure indicates the bridge endpoint itself is gone
/// (socket file unlinked, nothing listening) rather than a transient error on
/// a live endpoint.
#[cfg(unix)]
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

/// The marker rmcp writes ahead of a folded HTTP response body:
/// `StreamableHttpError::UnexpectedServerResponse(format!("HTTP {status}: {body}"))`
/// in `UnixSocketHttpClient::post_message`.
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum ForwardFailure {
    /// The bridge answered — with a non-2xx HTTP status whose body carried a
    /// JSON-RPC `error`. The endpoint is manifestly alive; relay the error.
    BridgeError(serde_json::Value),
    /// Nothing is listening on the bridge endpoint any more (auto-spawned
    /// bridges unlink their socket and exit after `--idle-timeout`). The
    /// request never reached the bridge, so it can be resent after a reconnect.
    EndpointGone,
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
#[cfg(unix)]
fn classify_forward_failure(display: &str, debug: &str) -> ForwardFailure {
    if let Some(error) = recover_bridge_jsonrpc_error(display) {
        return ForwardFailure::BridgeError(error);
    }
    if error_text_indicates_gone_endpoint(display) || error_text_indicates_gone_endpoint(debug) {
        return ForwardFailure::EndpointGone;
    }
    ForwardFailure::Transient
}

/// The message the proxy invents when a forward failed and *nothing* better
/// could be recovered from it. Deliberately vague, because in that case the
/// proxy genuinely does not know whether the bridge saw the request.
#[cfg(unix)]
const GENERIC_FORWARD_FAILURE_MESSAGE: &str = "Bridge could not service this request; it may \
                                               still be running. Retry, or await the completion \
                                               notification.";

/// Answer a single request id downstream after its forward failed: the bridge's
/// own JSON-RPC error when one could be recovered, otherwise the generic
/// fallback. Best-effort — a failed relay must not end the session.
#[cfg(unix)]
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
            "code": ahma_common::mcp_methods::JSONRPC_REQUEST_TIMEOUT,
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
#[cfg(unix)]
fn message_is_replayed_by_handshake(val: &serde_json::Value) -> bool {
    matches!(
        val.get("method").and_then(|m| m.as_str()),
        Some("initialize") | Some(INITIALIZED_METHOD)
    )
}

/// Rebuild the bridge connection and transparently resend the one request whose
/// forward failed, replacing `client` with the fresh transport.
///
/// Resending is safe and is *not* a duplicate execution: this path only runs
/// when the failure proved the endpoint was gone, so the request never reached
/// the bridge at all.
///
/// Returns `Ok(true)` when the request was resent, `Ok(false)` when the
/// reconnect succeeded but the resend did not (the caller must then answer the
/// request with an error), and `Err` when the reconnect itself failed.
#[cfg(unix)]
async fn reconnect_and_resend<C>(
    client: &mut C,
    pending: &serde_json::Value,
    reconnect: &mut dyn FnMut() -> Result<C>,
    handshake: &CachedHandshake,
    transport: &str,
    respawn: Option<&mut BridgeRespawnFn>,
) -> Result<bool>
where
    C: Transport<RoleClient>,
    C::Error: std::fmt::Debug,
{
    let mut fresh = reconnect_with_retries(reconnect, handshake, transport, respawn).await?;
    // Best-effort: release the dead connection's resources. Bounded so a broken
    // close() cannot stall the now-healthy session waiting on it.
    let _ = tokio::time::timeout(Duration::from_secs(2), client.close()).await;
    let resent = match serde_json::from_value::<TxJsonRpcMessage<RoleClient>>(pending.clone()) {
        Ok(msg) => match fresh.send(msg).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    transport,
                    error = ?e,
                    "Resend on the freshly reconnected bridge failed; \
                     answering the request with an error instead"
                );
                false
            }
        },
        Err(e) => {
            tracing::warn!(
                transport,
                error = %e,
                "Pending request no longer deserializes; cannot resend it"
            );
            false
        }
    };
    *client = fresh;
    Ok(resent)
}

/// Build the `detail` payload for the transparent-reconnect disclosure (#485).
///
/// The two cases must not be conflated: when the failed request was resent on
/// the fresh transport the client has nothing to do, and telling it the request
/// "was answered with an error and can be retried" would be a lie that invites
/// a duplicate call.
#[cfg(unix)]
fn reconnected_detail(reconnects: u32, cause: &str, in_flight_resent: bool) -> serde_json::Value {
    let (in_flight_request, message) = if in_flight_resent {
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
        "cause": cause,
        "reconnects": reconnects,
        "in_flight_request": in_flight_request,
        "message": message,
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
#[cfg(unix)]
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
                respawn_after_reconnect_failure(attempt, &e, respawn.as_deref_mut(), transport)
                    .await;
                last_err = Some(e);
            }
        }
        if attempt < MAX_RECONNECT_ATTEMPTS {
            tokio::time::sleep(reconnect_backoff_base() * attempt).await;
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("reconnect failed for an unknown reason")))
}

/// After a failed reconnect `attempt`, respawn the background bridge when
/// there is another attempt left to retry *and* the failure proves the
/// endpoint itself is gone (not merely glitching) *and* a respawn hook is
/// available. Best-effort: a failed respawn only logs, so the caller still
/// moves on to the next reconnect attempt.
#[cfg(unix)]
async fn respawn_after_reconnect_failure(
    attempt: u32,
    err: &anyhow::Error,
    respawn: Option<&mut BridgeRespawnFn>,
    transport: &str,
) {
    if attempt >= MAX_RECONNECT_ATTEMPTS {
        return;
    }
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
#[cfg(unix)]
fn reconnect_backoff_base() -> Duration {
    let ms = cfg!(debug_assertions)
        .then(|| std::env::var("AHMA_RECONNECT_BACKOFF_MS").ok())
        .flatten()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(500);
    Duration::from_millis(ms)
}

/// Run the stdio proxy connecting to the running UDS or HTTP server.
///
/// Returns `Ok(true)` when the bridge successfully responded to at least one
/// message (normal session end).  Returns `Ok(false)` or `Err` when the bridge
/// closed the connection before sending any response back to the client, which
/// typically indicates a stale or incompatible bridge daemon.
pub async fn run_proxy_client(
    uds_path: Option<&str>,
    http_url: Option<&str>,
    respawn_bridge: Option<BridgeRespawnFn>,
) -> Result<bool> {
    run_proxy_client_with_options(uds_path, http_url, respawn_bridge, "").await
}

/// As [`run_proxy_client`], carrying this client's per-session options in the
/// MCP URL's query so the daemon can apply them to this session's worker and no
/// other (SPEC R-DAEMON.4).
pub async fn run_proxy_client_with_options(
    uds_path: Option<&str>,
    http_url: Option<&str>,
    respawn_bridge: Option<BridgeRespawnFn>,
    session_query: &str,
) -> Result<bool> {
    let handshake_deadline = frontend_handshake_deadline();
    let mcp_uri = append_session_query("http://localhost/mcp", session_query);

    #[cfg(unix)]
    if let Some(path) = uds_path {
        tracing::info!(socket = path, "Proxying stdio to Unix Domain Socket");
        return run_proxy_client_unix(path, &mcp_uri, handshake_deadline, respawn_bridge).await;
    }

    if let Some(url) = http_url {
        tracing::info!(url = url, "Proxying stdio to HTTP server");
        let url = append_session_query(url, session_query);
        return run_proxy_client_http(&url, handshake_deadline).await;
    }

    #[cfg(not(unix))]
    let _ = (uds_path, respawn_bridge);

    Err(anyhow!("No socket or HTTP URL provided for proxy client"))
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

/// Synthesizes a standard `initialize` JSON-RPC message from a `server/discover` probe.
fn synthesize_initialize_request(val: &serde_json::Value) -> serde_json::Value {
    let client_info = val
        .get("params")
        .and_then(|p| {
            p.get("_meta")
                .and_then(|m| m.get("clientInfo"))
                .or_else(|| p.get("clientInfo"))
        })
        .cloned()
        .unwrap_or_else(|| {
            serde_json::json!({
                "name": "modern-client",
                "version": "1.0.0"
            })
        });

    let client_capabilities = val
        .get("params")
        .and_then(|p| {
            p.get("_meta")
                .and_then(|m| m.get("clientCapabilities"))
                .or_else(|| p.get("capabilities"))
        })
        .cloned()
        .unwrap_or_else(|| {
            serde_json::json!({
                "roots": { "listChanged": true }
            })
        });

    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": INITIALIZE_METHOD,
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION_2025_11_25,
            "capabilities": client_capabilities,
            "clientInfo": client_info
        }
    })
}

#[cfg(unix)]
async fn run_proxy_client_unix(
    socket_path: &str,
    mcp_uri: &str,
    handshake_deadline: Option<Duration>,
    respawn_bridge: Option<BridgeRespawnFn>,
) -> Result<bool> {
    use ahma_http_mcp_client::unix_client::unix_socket_transport;

    let client_transport = unix_socket_transport(socket_path, mcp_uri);
    let stdio_transport = PatchedStdioTransport::new_stdio();

    tracing::info!(socket = socket_path, "Proxy connected to bridge via UDS");
    let socket_path_owned = socket_path.to_string();
    let mcp_uri_owned = mcp_uri.to_string();
    let mut reconnect = move || Ok(unix_socket_transport(&socket_path_owned, &mcp_uri_owned));
    let result = run_transport_proxy(
        stdio_transport,
        client_transport,
        "unix",
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

#[cfg(unix)]
async fn run_transport_proxy<S, C>(
    mut stdio: S,
    mut client: C,
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
    let mut handshake = CachedHandshake::default();
    // Track whether we ever forwarded a message to the bridge.  Until the client sends
    // `initialize`, the bridge never creates a session for this proxy (and the underlying
    // rmcp worker is parked awaiting the first message without observing its cancellation
    // token).  In that state `client.close()` would block forever, so we must only attempt
    // the teardown when a session could actually exist.
    let mut forwarded_any = false;
    // Track whether the bridge ever responded (sent a message back to the client).
    // This is the signal used by handle_version_checks to detect a stale bridge:
    // a healthy bridge always replies to `initialize`; a stale one closes silently.
    let mut bridge_responded = false;
    // Count consecutive failures to forward a request to the bridge. Reset on any
    // success. A single failure no longer tears the session down (see
    // MAX_CONSECUTIVE_FORWARD_FAILURES).
    let mut consecutive_forward_failures: u32 = 0;
    // Session-health disclosure (#485): transparent reconnects this session and
    // the monotonic seq for the events that disclose them.
    let mut reconnects: u32 = 0;
    let mut event_seq: u64 = 0;

    // Handshake deadline: if the client never sends its first message (the
    // `initialize` handshake) within this window, the connection was spawned
    // and abandoned — exit so abandoned `serve stdio` spawns cannot accumulate.
    // Disarmed once the first message is forwarded; a live idle session is never
    // killed by this. A far-future sleep stands in for "no deadline".
    let deadline = handshake_deadline.unwrap_or(Duration::from_secs(u64::MAX / 2));
    let handshake_timer = tokio::time::sleep(deadline);
    tokio::pin!(handshake_timer);

    loop {
        tokio::select! {
            biased;
            _ = &mut handshake_timer, if !forwarded_any => {
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

                    let synth_init = synthesize_initialize_request(&val);

                    handshake.observe_client_to_bridge(&synth_init);

                    let init_msg: TxJsonRpcMessage<RoleClient> = match serde_json::from_value(synth_init) {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::error!(transport, error = %e, "Failed to serialize synthesized initialize");
                            continue;
                        }
                    };

                    if let Err(e) = client.send(init_msg).await {
                        tracing::error!(transport, error = %e, "Failed to send synthesized initialize to bridge");
                        continue;
                    }

                    let init_resp = match client.receive().await {
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

                    let discover_result = serde_json::json!({
                        "supportedVersions": [
                            MCP_PROTOCOL_VERSION_2026_07_28,
                            MCP_PROTOCOL_VERSION_2025_11_25
                        ],
                        "capabilities": bridge_capabilities,
                        "instructions": bridge_instructions,
                        "serverInfo": bridge_server_info.clone(),
                        "_meta": {
                            "io.modelcontextprotocol/serverInfo": bridge_server_info
                        }
                    });

                    let req_id: RequestId =
                        serde_json::from_value(discover_id).unwrap_or(RequestId::Number(1));
                    let tx_msg = TxJsonRpcMessage::<RoleServer>::response(
                        ServerResult::CustomResult(CustomResult(discover_result)),
                        req_id,
                    );

                    if let Err(e) = stdio.send(tx_msg).await {
                        tracing::error!(transport, error = ?e, "Failed to send server/discover response to stdio");
                        break;
                    }

                    let notif_init = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": INITIALIZED_METHOD,
                        "params": {}
                    });
                    handshake.observe_client_to_bridge(&notif_init);
                    if let Ok(notif_msg) = serde_json::from_value::<TxJsonRpcMessage<RoleClient>>(notif_init) {
                        let _ = client.send(notif_msg).await;
                    }

                    consecutive_forward_failures = 0;
                    forwarded_any = true;
                    bridge_responded = true;
                    continue;
                }

                if val.get("method").and_then(|m| m.as_str()) == Some(SUBSCRIPTIONS_LISTEN_METHOD) {
                    tracing::debug!(transport, "Handling modern subscriptions/listen request");
                    if let Some(id) = request_id {
                        let req_id: RequestId =
                            serde_json::from_value(id).unwrap_or(RequestId::Number(1));
                        let ok_msg =
                            TxJsonRpcMessage::<RoleServer>::response(ServerResult::empty(()), req_id);
                        let _ = stdio.send(ok_msg).await;
                    }
                    continue;
                }
                handshake.observe_client_to_bridge(&val);
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
                if let Err(e) = client.send(msg).await {
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
                    // Handshake messages are excluded: `replay_handshake` already
                    // resends those, so a resend would duplicate them.
                    if failure == ForwardFailure::EndpointGone
                        && handshake.init_request.is_some()
                        && !message_is_replayed_by_handshake(&val)
                    {
                        tracing::warn!(
                            transport,
                            error = %rendered,
                            "Bridge endpoint is gone; reconnecting immediately and resending \
                             the request rather than waiting for repeated failures"
                        );
                        match reconnect_and_resend(
                            &mut client,
                            &val,
                            reconnect,
                            &handshake,
                            transport,
                            respawn_bridge.as_mut(),
                        )
                        .await
                        {
                            Ok(resent) => {
                                // The reconnect handshake just proved the (fresh)
                                // bridge is alive and responsive.
                                bridge_responded = true;
                                reconnects += 1;
                                if resent {
                                    consecutive_forward_failures = 0;
                                    forwarded_any = true;
                                } else {
                                    consecutive_forward_failures += 1;
                                    if let Some(id) = request_id.clone() {
                                        relay_forward_failure_to_client(&mut stdio, id, None).await;
                                    }
                                }
                                emit_session_event_downstream(
                                    &mut stdio,
                                    &mut event_seq,
                                    ahma_common::session_event::SessionEventKind::Reconnected,
                                    reconnected_detail(reconnects, "endpoint_gone", resent),
                                )
                                .await;
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

                    consecutive_forward_failures += 1;
                    tracing::warn!(
                        transport,
                        error = %rendered,
                        failures = consecutive_forward_failures,
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
                    if consecutive_forward_failures >= MAX_CONSECUTIVE_FORWARD_FAILURES {
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
                                    // Best-effort: release the dead connection's
                                    // resources. Bounded so a broken close() cannot
                                    // stall the now-healthy session waiting on it.
                                    let _ = tokio::time::timeout(
                                        Duration::from_secs(2),
                                        client.close(),
                                    )
                                    .await;
                                    client = fresh;
                                    consecutive_forward_failures = 0;
                                    // The reconnect handshake just proved the
                                    // (fresh) bridge is alive and responsive.
                                    bridge_responded = true;
                                    reconnects += 1;
                                    // Disclose the rebuild to the client (#485).
                                    // The request that tripped the failure was
                                    // answered with an error above; name it so
                                    // the client can re-issue.
                                    emit_session_event_downstream(
                                        &mut stdio,
                                        &mut event_seq,
                                        ahma_common::session_event::SessionEventKind::Reconnected,
                                        reconnected_detail(
                                            reconnects,
                                            "transport_failure",
                                            false,
                                        ),
                                    )
                                    .await;
                                    continue;
                                }
                                Err(e) => {
                                    tracing::error!(
                                        transport,
                                        failures = consecutive_forward_failures,
                                        error = %e,
                                        "Proxy exiting: bridge transport failed repeatedly and \
                                         reconnect also failed"
                                    );
                                    // Terminal disclosure (#485): the pipe dies
                                    // next; at least say why.
                                    emit_session_event_downstream(
                                        &mut stdio,
                                        &mut event_seq,
                                        ahma_common::session_event::SessionEventKind::ReconnectFailed,
                                        serde_json::json!({
                                            "cause": e.to_string(),
                                            "attempts": MAX_RECONNECT_ATTEMPTS,
                                            "message": "ahma bridge is unreachable and reconnect \
                                                        attempts are exhausted; the server \
                                                        connection is closing",
                                        }),
                                    )
                                    .await;
                                    break;
                                }
                            }
                        }
                        tracing::error!(
                            transport,
                            failures = consecutive_forward_failures,
                            "Proxy exiting: bridge transport failed repeatedly (genuinely dead)"
                        );
                        break;
                    }
                    continue;
                }
                consecutive_forward_failures = 0;
                forwarded_any = true;
            }
            client_msg = client.receive() => {
                let Some(msg) = client_msg else {
                    tracing::info!(
                        transport,
                        "Proxy exiting: bridge connection closed"
                    );
                    break;
                };
                let mut val = serde_json::to_value(&msg).unwrap();
                handshake.observe_bridge_to_client(&val);
                // The server cannot know its transport was rebuilt; the proxy
                // owns the heartbeat's `reconnects` field (#485). Only a
                // heartbeat the overlay actually mutated is rebuilt from JSON;
                // every other message forwards as the typed value it arrived as
                // (the server-to-client message type is identical on both
                // transports).
                let tx_msg = if overlay_heartbeat_reconnects(&mut val, reconnects) {
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
                bridge_responded = true;
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
    if forwarded_any {
        match tokio::time::timeout(Duration::from_secs(6), client.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(transport, error = ?e, "Proxy close error (non-fatal)")
            }
            Err(_) => {
                tracing::debug!(transport, "Proxy close timed out (non-fatal)")
            }
        }
    }
    Ok(bridge_responded)
}

/// Perform the HTTP `initialize` handshake: receive the client's `initialize`
/// request from stdio (bounded by `handshake_deadline`), POST it to the
/// bridge, forward the response back to stdio, and return the negotiated
/// session id and protocol version — every subsequent request on this
/// connection must echo both.
/// POST to the bridge, retrying per SPEC R-HTTP.2 (`idempotency` decides
/// whether a timeout or 5xx may be re-sent), and reporting a final failure
/// that names the bridge first (SPEC R-HTTP.3).
async fn proxy_post<F>(
    mcp_url: &str,
    method: &str,
    idempotency: Idempotency,
    build: F,
) -> Result<reqwest::Response>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let service = format!("the ahma bridge at {mcp_url}");
    send_with_retry(&service, &RetryPolicy::DEFAULT, idempotency, build)
        .await
        .map(|(response, _)| response)
        .map_err(|failure| {
            ServiceError::new(
                &service,
                failure.failure,
                anyhow::Error::new(failure.error).context(format!("{method} request")),
            )
            .with_attempts(failure.attempts)
            .into()
        })
}

async fn perform_http_initialize(
    client: &reqwest::Client,
    mcp_url: &str,
    stdio: &mut PatchedStdioTransport,
    handshake_deadline: Option<Duration>,
) -> Result<(String, String)> {
    // Bounded by the handshake deadline so a connection that is spawned and
    // abandoned (no handshake message ever sent) exits rather than parking on
    // stdin forever and piling up.
    let first_recv = stdio.receive();
    let msg = match handshake_deadline {
            Some(deadline) => match tokio::time::timeout(deadline, first_recv).await {
                Ok(msg) => msg,
                Err(_) => {
                    tracing::warn!(
                        ?deadline,
                        "Proxy exiting: no MCP handshake within deadline (connection spawned but abandoned)"
                    );
                    // Exit directly: the stdin reader thread is still blocked on the
                    // held-open pipe, so returning would hang on runtime shutdown.
                    std::process::exit(0);
                }
            },
            None => first_recv.await,
        }
        .ok_or_else(|| {
            tracing::error!("Proxy HTTP handshake failed: no initialize message on stdin");
            anyhow!("No initialize message on stdin")
        })?;

    let val = serde_json::to_value(&msg)?;
    if val.get("method").and_then(|m| m.as_str()) == Some(SERVER_DISCOVER_METHOD) {
        tracing::info!(
            "Received server/discover probe from modern MCP client (2026-07-28); establishing first-class modern stateless session via HTTP"
        );
        let discover_id = val
            .get("id")
            .filter(|id| !id.is_null())
            .cloned()
            .unwrap_or_else(|| serde_json::json!(1));

        let synth_init = synthesize_initialize_request(&val);

        let response = proxy_post(mcp_url, "initialize", Idempotency::NotIdempotent, || {
            client
                .post(mcp_url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "application/json")
                .json(&synth_init)
        })
        .await?;

        if !response.status().is_success() {
            let status = response.status();
            tracing::error!(
                url = %mcp_url,
                status = %status,
                "Proxy HTTP initialize returned non-success status"
            );
            return Err(anyhow!("Initialize failed with HTTP {status}"));
        }

        let session_id = crate::mcp_client::session_id_header(&response).ok_or_else(|| {
            tracing::error!(
                url = %mcp_url,
                "Proxy HTTP initialize missing mcp-session-id header"
            );
            anyhow!("Missing mcp-session-id header in initialize response")
        })?;

        let resp_bytes = response
            .bytes()
            .await
            .context("Failed to read initialize response body")?;

        let protocol_version = serde_json::from_slice::<serde_json::Value>(&resp_bytes)
            .map(|v| ahma_common::mcp_protocol::negotiated_protocol_version(&v))
            .unwrap_or_else(|_| {
                ahma_common::mcp_protocol::DEFAULT_NEGOTIATED_PROTOCOL_VERSION.to_string()
            });

        let init_resp_val =
            serde_json::from_slice::<serde_json::Value>(&resp_bytes).unwrap_or_default();
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

        let discover_result = serde_json::json!({
            "supportedVersions": [
                MCP_PROTOCOL_VERSION_2026_07_28,
                MCP_PROTOCOL_VERSION_2025_11_25
            ],
            "capabilities": bridge_capabilities,
            "instructions": bridge_instructions,
            "serverInfo": bridge_server_info.clone(),
            "_meta": {
                "io.modelcontextprotocol/serverInfo": bridge_server_info
            }
        });

        let req_id: RequestId = serde_json::from_value(discover_id).unwrap_or(RequestId::Number(1));
        let resp_msg = TxJsonRpcMessage::<RoleServer>::response(
            ServerResult::CustomResult(CustomResult(discover_result)),
            req_id,
        );
        stdio
            .send(resp_msg)
            .await
            .context("Failed to forward discover response to stdio")?;

        let notif_init = serde_json::json!({
            "jsonrpc": "2.0",
            "method": INITIALIZED_METHOD,
            "params": {}
        });
        let _ = client
            .post(mcp_url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("mcp-session-id", &session_id)
            .header(
                ahma_common::mcp_protocol::MCP_PROTOCOL_VERSION_HEADER,
                &protocol_version,
            )
            .json(&notif_init)
            .send()
            .await;

        return Ok((session_id, protocol_version));
    }
    let init_val = val;

    let response = proxy_post(mcp_url, "initialize", Idempotency::NotIdempotent, || {
        client
            .post(mcp_url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&init_val)
    })
    .await?;

    if !response.status().is_success() {
        let status = response.status();
        tracing::error!(
            url = %mcp_url,
            status = %status,
            "Proxy HTTP initialize returned non-success status"
        );
        return Err(anyhow!("Initialize failed with HTTP {status}"));
    }

    let session_id = crate::mcp_client::session_id_header(&response).ok_or_else(|| {
        tracing::error!(
            url = %mcp_url,
            "Proxy HTTP initialize missing mcp-session-id header"
        );
        anyhow!("Missing mcp-session-id header in initialize response")
    })?;

    let resp_bytes = response
        .bytes()
        .await
        .context("Failed to read initialize response body")?;
    // The version the server answered with is what every subsequent HTTP
    // request must echo in `MCP-Protocol-Version` (2025-06-18 Streamable HTTP).
    let protocol_version = serde_json::from_slice::<serde_json::Value>(&resp_bytes)
        .map(|v| ahma_common::mcp_protocol::negotiated_protocol_version(&v))
        .unwrap_or_else(|_| {
            ahma_common::mcp_protocol::DEFAULT_NEGOTIATED_PROTOCOL_VERSION.to_string()
        });
    let resp_msg: TxJsonRpcMessage<RoleServer> =
        serde_json::from_slice(&resp_bytes).context("Failed to parse initialize response JSON")?;
    stdio
        .send(resp_msg)
        .await
        .context("Failed to forward initialize response to stdio")?;

    Ok((session_id, protocol_version))
}

/// Background task body: open the bridge's SSE stream for `session_id` and
/// forward every `data:` line that parses as a JSON-RPC message onto
/// `sse_tx`. Runs until the stream ends, the connection fails, or the
/// receiving end of `sse_tx` is dropped.
async fn run_http_sse_listener(
    client: reqwest::Client,
    url: String,
    session_id: String,
    protocol_version: String,
    sse_tx: mpsc::Sender<TxJsonRpcMessage<RoleServer>>,
) {
    let Some(headers) = sse_listener_headers(&session_id, &protocol_version) else {
        return;
    };

    let res = match client.get(&url).headers(headers).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(url = %url, error = %e, "Proxy SSE connection failed");
            return;
        }
    };

    if !res.status().is_success() {
        tracing::error!(
            url = %url,
            status = %res.status(),
            "Proxy SSE stream returned non-success status"
        );
        return;
    }

    pump_sse_stream(res.bytes_stream(), &sse_tx, &url).await;
    tracing::info!(url = %url, "Proxy SSE stream ended");
}

/// Reassemble SSE lines across chunk boundaries and forward each JSON-RPC
/// `data:` payload onto `sse_tx`, until the stream ends, it errors, or the
/// receiving end of `sse_tx` is dropped.
///
/// Stopping on a dropped receiver matters: without it the loop keeps reading
/// and appending to a buffer that can no longer be drained, so it does useless
/// work and grows without bound for the remaining life of the stream.
///
/// Generic over the chunk and error types so it can be driven from a plain
/// in-memory stream in tests; `run_http_sse_listener` passes
/// `reqwest::Response::bytes_stream()`.
async fn pump_sse_stream<S, B, E>(
    mut stream: S,
    sse_tx: &mpsc::Sender<TxJsonRpcMessage<RoleServer>>,
    url: &str,
) where
    S: futures::Stream<Item = std::result::Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(url = %url, error = %e, "Proxy SSE stream error");
                break;
            }
        };

        buffer.push_str(&String::from_utf8_lossy(chunk.as_ref()));
        if forward_buffered_sse_lines(&mut buffer, sse_tx, url).await
            == ForwardOutcome::ChannelClosed
        {
            break;
        }
    }
}

/// Build the SSE request headers for `session_id`, or `None` — after logging —
/// when the bridge handed back a session id that is not a valid HTTP header
/// value.
///
/// Fallible, not `.unwrap()`: `session_id` is whatever the *bridge* returned in
/// `Mcp-Session-Id`, so a byte outside visible ASCII would panic the detached
/// listener task rather than surface anywhere a caller could see it. The
/// protocol-version header already handled the identical construction with
/// `if let Ok`; the session one did not, which is the tell rather than a
/// decision.
fn sse_listener_headers(
    session_id: &str,
    protocol_version: &str,
) -> Option<reqwest::header::HeaderMap> {
    let Ok(session_header) = reqwest::header::HeaderValue::from_str(session_id) else {
        tracing::error!(
            session_id = %session_id,
            "bridge returned a session id that is not a valid HTTP header value; \
             cannot open the SSE stream for it"
        );
        return None;
    };
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("mcp-session-id", session_header);
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("text/event-stream"),
    );
    if let Ok(v) = reqwest::header::HeaderValue::from_str(protocol_version) {
        headers.insert(ahma_common::mcp_protocol::MCP_PROTOCOL_VERSION_HEADER, v);
    }
    Some(headers)
}

/// Whether the SSE forward channel is still usable once a drain pass returns.
#[derive(Debug, PartialEq, Eq)]
enum ForwardOutcome {
    /// Everything currently buffered was forwarded or skipped — keep reading.
    Continue,
    /// The receiving end of `sse_tx` is gone. Nothing further can be delivered,
    /// so the caller must stop reading the stream rather than accumulating a
    /// buffer it can no longer drain.
    ChannelClosed,
}

/// Drain every complete line buffered so far, forwarding each `data:` payload
/// that parses as a JSON-RPC message onto `sse_tx`. An incomplete trailing line
/// stays in `buffer` for the next chunk; a line that is not `data:`, is empty,
/// or does not parse is skipped.
///
/// Stops at the first failed send and reports [`ForwardOutcome::ChannelClosed`]
/// so the caller can shut the listener down. The undrained remainder of
/// `buffer` is deliberately left alone: there is nowhere to deliver it.
async fn forward_buffered_sse_lines(
    buffer: &mut String,
    sse_tx: &mpsc::Sender<TxJsonRpcMessage<RoleServer>>,
    url: &str,
) -> ForwardOutcome {
    while let Some(pos) = buffer.find('\n') {
        let line = buffer.drain(..=pos).collect::<String>();
        let Some(msg) = parse_sse_data_line(&line) else {
            continue;
        };
        if sse_tx.send(msg).await.is_err() {
            tracing::info!(url = %url, "Proxy SSE forward channel closed");
            return ForwardOutcome::ChannelClosed;
        }
    }
    ForwardOutcome::Continue
}

/// Parse one buffered SSE line into a JSON-RPC message, or `None` when the
/// line is not a `data:` line, carries an empty payload, or does not parse —
/// each of those is silently skipped by the caller.
fn parse_sse_data_line(line: &str) -> Option<TxJsonRpcMessage<RoleServer>> {
    let data = line.trim().strip_prefix("data:")?.trim();
    if data.is_empty() {
        return None;
    }
    serde_json::from_str::<TxJsonRpcMessage<RoleServer>>(data).ok()
}

/// Pump messages between stdio and the bridge until stdio hits EOF or the SSE
/// channel closes: client→bridge messages are POSTed to the bridge and (for
/// requests) their response forwarded back to stdio; bridge→client SSE
/// messages are forwarded to stdio directly.
async fn run_http_proxy_loop(
    stdio: &mut PatchedStdioTransport,
    client: &reqwest::Client,
    mcp_url: &str,
    session_id: &str,
    protocol_version: &str,
    sse_rx: &mut mpsc::Receiver<TxJsonRpcMessage<RoleServer>>,
) -> Result<()> {
    loop {
        tokio::select! {
            stdio_msg = stdio.receive() => {
                let Some(msg) = stdio_msg else {
                    tracing::info!(
                        url = %mcp_url,
                        session_id = %session_id,
                        "Proxy exiting: stdio EOF (Cursor client disconnected)"
                    );
                    return Ok(());
                };

                let val = serde_json::to_value(&msg)?;
                let has_id = val.get("id").is_some();
                let is_request = val.get("method").is_some();

                if val.get("method").and_then(|m| m.as_str()) == Some(SUBSCRIPTIONS_LISTEN_METHOD) {
                    tracing::debug!("Handling modern subscriptions/listen request via HTTP");
                    if let Some(id) = val.get("id").filter(|id| !id.is_null()) {
                        let req_id: RequestId =
                            serde_json::from_value(id.clone()).unwrap_or(RequestId::Number(1));
                        let ok_msg =
                            TxJsonRpcMessage::<RoleServer>::response(ServerResult::empty(()), req_id);
                        let _ = stdio.send(ok_msg).await;
                    }
                    continue;
                }

                let method = val.get("method").and_then(|m| m.as_str()).unwrap_or("response");
                let idempotency = match method {
                    "tools/list" | "resources/list" | "prompts/list" | "ping" => {
                        Idempotency::Idempotent
                    }
                    _ => Idempotency::NotIdempotent,
                };
                let resp = proxy_post(mcp_url, method, idempotency, || {
                    let req = client.post(mcp_url)
                        .header(reqwest::header::CONTENT_TYPE, "application/json")
                        .header("mcp-session-id", session_id)
                        .header(
                            ahma_common::mcp_protocol::MCP_PROTOCOL_VERSION_HEADER,
                            protocol_version,
                        )
                        .json(&val);
                    if has_id && is_request {
                        req.header(reqwest::header::ACCEPT, "application/json")
                    } else {
                        req
                    }
                })
                .await
                .with_context(|| format!("proxy session {session_id}"))?;
                if has_id && is_request {
                    if !resp.status().is_success() {
                        tracing::warn!(
                            url = %mcp_url,
                            status = %resp.status(),
                            "Proxy HTTP tool/request returned non-success status"
                        );
                    }
                    let bytes = resp.bytes().await?;
                    if !bytes.is_empty() {
                        let resp_msg: TxJsonRpcMessage<RoleServer> = serde_json::from_slice(&bytes)?;
                        stdio.send(resp_msg).await?;
                    }
                }
            }

            sse_msg = sse_rx.recv() => {
                let Some(msg) = sse_msg else {
                    tracing::info!(
                        url = %mcp_url,
                        session_id = %session_id,
                        "Proxy exiting: SSE channel closed"
                    );
                    return Ok(());
                };
                stdio.send(msg).await?;
            }
        }
    }
}

async fn run_proxy_client_http(
    base_url: &str,
    handshake_deadline: Option<Duration>,
) -> Result<bool> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("Failed to build HTTP client for stdio proxy")?;

    let mcp_url = format!("{}/mcp", base_url.trim_end_matches('/'));

    let mut stdio = PatchedStdioTransport::new_stdio();

    let (session_id, protocol_version) =
        perform_http_initialize(&client, &mcp_url, &mut stdio, handshake_deadline).await?;

    tracing::info!(
        url = %mcp_url,
        session_id = %session_id,
        "Proxy connected to bridge via HTTP"
    );

    // Start the SSE listener in the background.
    let (sse_tx, mut sse_rx) = mpsc::channel::<TxJsonRpcMessage<RoleServer>>(100);
    tokio::spawn(run_http_sse_listener(
        client.clone(),
        mcp_url.clone(),
        session_id.clone(),
        protocol_version.clone(),
        sse_tx,
    ));

    run_http_proxy_loop(
        &mut stdio,
        &client,
        &mcp_url,
        &session_id,
        &protocol_version,
        &mut sse_rx,
    )
    .await?;

    let _ = client
        .delete(&mcp_url)
        .header("mcp-session-id", &session_id)
        .header(
            ahma_common::mcp_protocol::MCP_PROTOCOL_VERSION_HEADER,
            &protocol_version,
        )
        .send()
        .await;

    // The bridge successfully responded to initialize, so it was reachable
    // and communicating — nothing past that point can make this `false`.
    Ok(true)
}

// `run_transport_proxy` and the `RoleClient` import are Unix-only, so these
// tests (which drive it directly with mock transports) are gated to match.
#[cfg(all(test, unix))]
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
            if let Some(msg) = self.inbound.pop_front() {
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

    #[tokio::test]
    async fn reconnect_exhausted_all_attempts_still_exits() {
        // If the bridge is truly gone (every reconnect attempt fails to even
        // build a fresh connection), the proxy must still give up and exit —
        // it must not retry forever.
        with_zero_backoff(async {
            let stdio = MockStdio::new(
                VecDeque::from(vec![
                    client_initialize(0),
                    client_request(1),
                    client_request(2),
                ]),
                Arc::new(Mutex::new(Vec::new())),
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
            let reconnect_calls = Arc::new(AtomicUsize::new(0));
            let reconnect_calls_inner = reconnect_calls.clone();
            let mut reconnect = move || -> Result<MockClient> {
                reconnect_calls_inner.fetch_add(1, Ordering::SeqCst);
                Err(anyhow!("simulated: bridge process is gone"))
            };

            let result =
                run_transport_proxy(stdio, dead_client, "test", None, &mut reconnect, None).await;
            assert!(result.is_ok(), "proxy must exit cleanly, not error out");
            assert_eq!(
                reconnect_calls.load(Ordering::SeqCst),
                MAX_RECONNECT_ATTEMPTS as usize,
                "must try reconnecting exactly MAX_RECONNECT_ATTEMPTS times, then give up"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn reconnect_exhaustion_discloses_reconnect_failed_before_exit() {
        // Session-health disclosure (#485): when the proxy gives up and the
        // pipe is about to die, the client receives a terminal
        // `reconnect_failed` event (error-level mirror) explaining why.
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
            let mut reconnect =
                move || -> Result<MockClient> { Err(anyhow!("simulated: bridge gone")) };

            let result =
                run_transport_proxy(stdio, dead_client, "test", None, &mut reconnect, None).await;
            assert!(result.is_ok());
            let sent = sent.lock();
            let events: Vec<_> = sent
                .iter()
                .filter(|m| {
                    m.get("method").and_then(|m| m.as_str())
                        == Some(ahma_common::session_event::SESSION_EVENT_METHOD)
                })
                .collect();
            assert_eq!(
                events.len(),
                1,
                "expected 1 reconnect_failed event, got {sent:?}"
            );
            assert_eq!(events[0]["params"]["kind"], "reconnect_failed");
            let mirror = sent
                .iter()
                .find(|m| m.get("method").and_then(|m| m.as_str()) == Some("notifications/message"))
                .expect("logging mirror present");
            assert_eq!(mirror["params"]["level"], "error");
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
        };

        let attempts = Arc::new(AtomicUsize::new(0));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut client = MockClient {
            fail_first_n: 0,
            fail_sends_from: usize::MAX,
            send_error_text: TRANSIENT_SEND_ERROR,
            attempts: attempts.clone(),
            inbound: VecDeque::from(vec![
                bridge_response(0),           // answers the replayed initialize
                bridge_roots_list_request(7), // the fresh session's own roots/list
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
    async fn run_proxy_client_without_target_errors() {
        let result = run_proxy_client(None, None, None).await;
        let err = result.expect_err("no socket or URL must be an error");
        assert!(
            err.to_string().contains("No socket or HTTP URL provided"),
            "unexpected error message: {err}"
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

    #[tokio::test]
    async fn server_discover_probe_returns_discover_result_and_establishes_session() {
        let state = TestState::new();
        // Send server/discover first, then subscriptions/listen, then client_request(3)
        let stdio = state.stdio(VecDeque::from(vec![
            client_discover_request(1),
            serde_json::from_value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "subscriptions/listen",
                "params": {"notifications": {"toolsListChanged": true}}
            }))
            .unwrap(),
            client_request(3),
        ]));
        let mut client = state.client(0);
        client.inbound = VecDeque::from(vec![bridge_initialize_response(0), bridge_response(3)]);
        let mut reconnect = no_reconnect;

        let result = run_transport_proxy(stdio, client, "test", None, &mut reconnect, None).await;
        assert!(result.is_ok());

        // Inspect what was sent to stdio
        let sent = state.sent.lock();
        assert!(
            sent.len() >= 2,
            "expected discover response and subscriptions/listen response"
        );
        let first_sent = serde_json::to_value(&sent[0]).expect("json");
        assert_eq!(first_sent["id"], 1);
        assert_eq!(
            first_sent["result"]["supportedVersions"],
            serde_json::json!(["2026-07-28", "2025-11-25"]),
            "first_sent was: {first_sent:#?}"
        );
        assert_eq!(
            first_sent["result"]["capabilities"]["tools"]["listChanged"],
            true
        );
        assert_eq!(first_sent["result"]["serverInfo"]["name"], "ahma");

        let second_sent = serde_json::to_value(&sent[1]).expect("json");
        assert_eq!(second_sent["id"], 2);
        assert_eq!(second_sent["result"], serde_json::json!({}));

        // The bridge should have received:
        // 1. Synthesized initialize (id 0)
        // 2. Synthesized notifications/initialized
        // 3. client_request(3)
        assert_eq!(
            state.attempts.load(Ordering::SeqCst),
            3,
            "bridge received synthesized initialize, initialized notification, and client_request"
        );
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
}

// `pump_sse_stream` is cross-platform, so — unlike the `run_transport_proxy`
// tests above — these are deliberately not gated to Unix.
#[cfg(test)]
mod sse_pump_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A server→client notification that round-trips through
    /// `TxJsonRpcMessage<RoleServer>`. If this ever stops parsing, the
    /// `forwards_data_lines` test below fails loudly rather than the
    /// close-detection test silently passing for the wrong reason.
    const PROGRESS: &str = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":1,"progress":1}}"#;

    /// A stream over `chunks` that records how many it actually yielded, so a
    /// test can assert the pump *stopped reading* rather than merely stopped
    /// forwarding.
    fn counting_stream(
        chunks: Vec<String>,
        yielded: Arc<AtomicUsize>,
    ) -> impl futures::Stream<Item = std::result::Result<Vec<u8>, std::convert::Infallible>> + Unpin
    {
        Box::pin(futures::stream::iter(chunks).map(move |c| {
            yielded.fetch_add(1, Ordering::SeqCst);
            Ok(c.into_bytes())
        }))
    }

    #[tokio::test]
    async fn pump_reassembles_a_frame_split_across_chunks() {
        let (tx, mut rx) = mpsc::channel(8);
        let yielded = Arc::new(AtomicUsize::new(0));

        // Split one `data:` frame mid-JSON so the first chunk contains no
        // newline at all — the pump must hold it and complete it on the next.
        let (head, tail) = PROGRESS.split_at(20);
        let stream = counting_stream(
            vec![format!("data: {head}"), format!("{tail}\n\n")],
            yielded.clone(),
        );

        pump_sse_stream(stream, &tx, "test://sse").await;

        assert!(
            rx.try_recv().is_ok(),
            "the frame split across two chunks should have been reassembled and forwarded"
        );
        assert_eq!(
            yielded.load(Ordering::SeqCst),
            2,
            "both chunks are consumed"
        );
    }

    #[tokio::test]
    async fn pump_stops_reading_once_the_forward_channel_closes() {
        let (tx, rx) = mpsc::channel(1);
        // The stdio side is gone: every send from here on fails.
        drop(rx);

        let yielded = Arc::new(AtomicUsize::new(0));
        let chunks: Vec<String> = (0..50).map(|_| format!("data: {PROGRESS}\n\n")).collect();
        let stream = counting_stream(chunks, yielded.clone());

        pump_sse_stream(stream, &tx, "test://sse").await;

        // Reading past the first failed send is pointless work, and — because
        // the buffer can no longer be drained — it grows without bound for the
        // rest of the stream's life.
        let consumed = yielded.load(Ordering::SeqCst);
        assert_eq!(
            consumed, 1,
            "the listener must stop reading as soon as the forward channel \
             closes, but it consumed {consumed} of 50 chunks"
        );
    }
}
