use crate::error::BridgeError;
use crate::session::{McpRoot, SessionManager, request_timeout_secs, tool_call_timeout_secs};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures::stream::{self, StreamExt};
use serde_json::Value;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::oneshot;
use tokio_stream::wrappers::BroadcastStream;
use tracing::{debug, error, info, warn};

/// MCP Session-Id header name (per MCP spec 2025-03-26)
const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

/// Create a JSON response with appropriate headers
fn json_response(value: Value) -> Response {
    json_response_with_status(StatusCode::OK, value)
}

/// Create a JSON response with the provided status.
fn json_response_with_status(status: StatusCode, value: Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&value).unwrap_or_default()))
        .unwrap_or_else(|_| (status, "Failed to create response").into_response())
}

/// Build a JSON-RPC error object.
fn json_rpc_error_value(code: i32, message: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "error": {
            "code": code,
            "message": message
        }
    })
}

/// Build the recoverable "request timed out" response for a forwarded request.
///
/// Returned with **HTTP 200** (not 500) and the original request `id` so the
/// rmcp client correlates it to the pending request and surfaces it as that
/// request's error — WITHOUT treating the transport as dead. A 500 here would
/// make the rmcp streamable-HTTP client raise `UnexpectedServerResponse`, which
/// `proxy_client` treats as fatal and tears the whole MCP session down. The
/// operation itself keeps running in the subprocess; the caller can await again
/// or wait for the completion notification.
fn request_timeout_response(payload: &Value) -> Response {
    let id = payload.get("id").cloned().unwrap_or(Value::Null);
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32002,
            "message": "Operation still running: the bridge wait window elapsed \
                        before the tool returned. The operation continues in the \
                        background — await again or wait for the completion \
                        notification."
        }
    });
    json_response_with_status(StatusCode::OK, body)
}

/// Attach MCP session header when available.
fn with_session_header(mut response: Response, session_id: &str) -> Response {
    let header_value = HeaderValue::from_str(session_id)
        .ok()
        .unwrap_or_else(|| HeaderValue::from_static("invalid"));
    response
        .headers_mut()
        .insert(MCP_SESSION_ID_HEADER, header_value);
    response
}

/// Create an error response with the provided status and JSON-RPC code.
fn error_response_with_status(status: StatusCode, code: i32, message: &str) -> Response {
    json_response_with_status(status, json_rpc_error_value(code, message))
}

/// Create an error response in the appropriate format
fn error_response(code: i32, message: &str) -> Response {
    error_response_with_status(StatusCode::INTERNAL_SERVER_ERROR, code, message)
}

fn missing_session_id_response() -> Response {
    error_response_with_status(
        StatusCode::BAD_REQUEST,
        -32600,
        "Missing Mcp-Session-Id header. Send initialize request first.",
    )
}

fn session_not_found_response() -> Response {
    error_response_with_status(
        StatusCode::FORBIDDEN,
        -32600,
        "Session not found or terminated",
    )
}

async fn session_has_sampling(s: &crate::session::Session) -> bool {
    let caps = s.capabilities.lock().await;
    caps.as_ref().and_then(|c| c.get("sampling")).is_some()
}

async fn session_name_matches_label(s: &crate::session::Session, target: &str) -> bool {
    let info_guard = s.client_info.lock().await;
    let name = info_guard
        .as_ref()
        .and_then(|info| info.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("");
    name.to_lowercase().contains(&target.to_lowercase())
}

async fn find_target_session_for_sampling(
    session_manager: &SessionManager,
    current_session_id: &str,
    target_label: Option<&str>,
) -> Option<Arc<crate::session::Session>> {
    let other_sessions = || {
        session_manager
            .get_all_sessions()
            .into_iter()
            .filter(|s| s.id != current_session_id)
    };

    // First pass: find a session with sampling that also matches the label (if given)
    for s in other_sessions() {
        if !session_has_sampling(&s).await {
            continue;
        }
        let label_matches = match target_label {
            Some(target) => session_name_matches_label(&s, target).await,
            None => true,
        };
        if label_matches {
            return Some(s);
        }
    }

    // Fallback: if label didn't match exactly, return any session with sampling
    if target_label.is_some() {
        for s in other_sessions() {
            if session_has_sampling(&s).await {
                return Some(s);
            }
        }
    }

    None
}

async fn handle_routed_sampling_request(
    session_manager: &SessionManager,
    session_id: &str,
    payload: &Value,
    is_sse: bool,
) -> Response {
    let params = payload.get("params");
    let target_label = params
        .and_then(|p| p.get("__route_target_label"))
        .and_then(|l| l.as_str())
        .map(String::from);

    let target_session = match find_target_session_for_sampling(
        session_manager,
        session_id,
        target_label.as_deref(),
    )
    .await
    {
        Some(s) => s,
        None => {
            let err_msg =
                "No active IDE session (Cursor, VS Code, etc.) with sampling capability found. \
                 Make sure your IDE is running and connected to ahma."
                    .to_string();
            return error_response(-32603, &err_msg);
        }
    };

    // Acquire a sampling permit (bounded concurrency — default 3 simultaneous requests).
    // `acquire()` is cancel-safe and returns Err only if the semaphore is closed, which
    // cannot happen here because the semaphore lives in the Session arc.
    let _permit = target_session
        .sampling_semaphore
        .acquire()
        .await
        .expect("sampling semaphore closed unexpectedly");

    let routed_id = format!("route_{}", uuid::Uuid::new_v4());
    let (tx, rx) = oneshot::channel();
    target_session.routed_requests.insert(routed_id.clone(), tx);

    let mut routed_payload = payload.clone();
    routed_payload["id"] = serde_json::json!(routed_id);
    if let Some(params_mut) = routed_payload
        .get_mut("params")
        .and_then(|p| p.as_object_mut())
    {
        params_mut.remove("__route_target_label");
    }

    let json_str = match serde_json::to_string(&routed_payload) {
        Ok(s) => s,
        Err(e) => {
            target_session.routed_requests.remove(&routed_id);
            return error_response(-32603, &format!("Failed to serialize routed payload: {e}"));
        }
    };

    if target_session.broadcast(json_str).is_err() {
        target_session.routed_requests.remove(&routed_id);
        return error_response(-32603, "Target session's SSE channel is closed");
    }

    let timeout = Duration::from_secs(120);
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(response)) => {
            let mut final_response = response;
            final_response["id"] = payload
                .get("id")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            if is_sse {
                if let Some(session) = session_manager.get_session(session_id) {
                    let (id, json_str) = session_sse_event(&session, &final_response);
                    with_session_header(sse_single_event_response_with_id(id, json_str), session_id)
                } else {
                    with_session_header(
                        sse_single_event_response_with_id(1, serialize_sse_data(&final_response)),
                        session_id,
                    )
                }
            } else {
                with_session_header(json_response(final_response), session_id)
            }
        }
        Ok(Err(_)) => error_response(-32603, "Routed request sender dropped"),
        Err(_) => {
            target_session.routed_requests.remove(&routed_id);
            error_response(-32002, "Request timed out on the client side")
        }
    }
}

/// Handles requests in session isolation mode.
#[tracing::instrument(skip_all, fields(method, session_id))]
pub async fn handle_session_isolated_request(
    session_manager: Arc<SessionManager>,
    headers: HeaderMap,
    payload: Value,
) -> Response {
    let session_id = headers
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let method = payload.get("method").and_then(|m| m.as_str());

    tracing::Span::current().record("method", method.unwrap_or(""));
    tracing::Span::current().record("session_id", session_id.as_deref().unwrap_or(""));
    debug!(method = ?method, session_id = ?session_id, has_id = payload.get("id").is_some(), "Incoming MCP request");

    if method == Some("initialize") && session_id.is_none() {
        return handle_initialize(&session_manager, &payload).await;
    }
    if let Some(session_id) = session_id {
        if method == Some("sampling/createMessage") {
            return handle_routed_sampling_request(&session_manager, &session_id, &payload, false)
                .await;
        }
        return handle_existing_session_request(&session_manager, &session_id, method, &payload)
            .await;
    }

    debug!(
        "Request without session ID for non-initialize method: {:?}",
        method
    );
    missing_session_id_response()
}

fn validate_initialize_payload(payload: &Value) -> Option<Response> {
    if payload
        .get("params")
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str())
        .is_none()
    {
        Some(error_response(
            -32602,
            "Invalid initialize params: missing params.protocolVersion",
        ))
    } else {
        None
    }
}

async fn handle_initialize_error(
    session_manager: &SessionManager,
    session_id: &str,
    error: crate::error::BridgeError,
) -> Response {
    error!(session_id = %session_id, "Failed to send initialize request: {}", error);
    let _ = session_manager
        .terminate_session(
            session_id,
            crate::session::SessionTerminationReason::ProcessCrashed,
        )
        .await;
    error_response(-32603, &format!("Failed to initialize session: {}", error))
}

async fn register_session_details(
    session_manager: &Arc<SessionManager>,
    session_id: &str,
    payload: &Value,
) {
    if let Some(session) = session_manager.get_session(session_id) {
        *session.session_manager.lock().await = Some(Arc::downgrade(session_manager));

        let client_info = payload
            .get("params")
            .and_then(|p| p.get("clientInfo"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let capabilities = payload
            .get("params")
            .and_then(|p| p.get("capabilities"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        session.set_client_info(client_info, capabilities).await;
    }
}

/// Handles initialization requests by creating a new session.
#[tracing::instrument(skip_all, fields(session_id))]
async fn handle_initialize(session_manager: &Arc<SessionManager>, payload: &Value) -> Response {
    debug!("Processing initialize request (no session ID)");

    if let Some(err_response) = validate_initialize_payload(payload) {
        return err_response;
    }

    info!("Creating new session for initialize request");
    let new_session_id = match create_session_or_error(session_manager).await {
        Ok(id) => id,
        Err(e) => return e,
    };

    register_session_details(session_manager, &new_session_id, payload).await;

    info!(session_id = %new_session_id, "Session created, forwarding initialize request");
    match session_manager
        .send_request(
            &new_session_id,
            payload,
            Some(Duration::from_secs(request_timeout_secs())),
        )
        .await
    {
        Ok(response) => with_session_header(json_response(response), &new_session_id),
        Err(e) => handle_initialize_error(session_manager, &new_session_id, e).await,
    }
}

/// Create a new session or return an error response.
///
/// Distinguishes session-limit errors (HTTP 429) from other failures (HTTP 500).
async fn create_session_or_error(
    session_manager: &Arc<SessionManager>,
) -> Result<String, Response> {
    match session_manager.create_session().await {
        Ok(id) => Ok(id),
        Err(e) => {
            error!("Failed to create session: {}", e);
            let response = if e.to_string().contains("Session limit exceeded") {
                error_response_with_status(
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    -32002,
                    &format!("Failed to create session: {}", e),
                )
            } else {
                error_response(-32603, &format!("Failed to create session: {}", e))
            };
            Err(response)
        }
    }
}

fn check_session_exists(session_manager: &SessionManager, session_id: &str) -> Option<Response> {
    if !session_manager.session_exists(session_id) {
        warn!(session_id = %session_id, "Request for non-existent or terminated session");
        Some(session_not_found_response())
    } else {
        None
    }
}

async fn handle_roots_changed_request(
    session_manager: &SessionManager,
    session_id: &str,
) -> Option<Response> {
    match session_manager.handle_roots_changed(session_id).await {
        // Tolerated no-op: sandbox already locked. Acknowledge with success and
        // short-circuit so the notification is NOT forwarded to the subprocess
        // (its scope is locked too). Keeps the session alive — see
        // SessionManager::handle_roots_changed for the rationale.
        Ok(true) => Some(with_session_header(
            json_response_with_status(StatusCode::ACCEPTED, serde_json::json!({})),
            session_id,
        )),
        // Sandbox still AwaitingRoots: proceed with the normal handshake (forward).
        Ok(false) => None,
        Err(e) => {
            error!(session_id = %session_id, "Roots change handling failed: {}", e);
            Some(session_not_found_response())
        }
    }
}

/// Handles requests for an existing session.
async fn handle_existing_session_request(
    session_manager: &SessionManager,
    session_id: &str,
    method: Option<&str>,
    payload: &Value,
) -> Response {
    if let Some(response) = check_session_exists(session_manager, session_id) {
        return response;
    }

    if method == Some("notifications/roots/list_changed")
        && let Some(response) = handle_roots_changed_request(session_manager, session_id).await
    {
        return response;
    }

    if method == Some("tools/call")
        && let Some(response) = check_sandbox_lock(session_manager, session_id)
    {
        return response;
    }

    let is_initialized_notification = method == Some("notifications/initialized");
    if is_initialized_notification {
        debug!(session_id = %session_id, "Received notifications/initialized");
    }

    let is_client_response = is_client_response(method, payload);

    if let Some(response) = check_initialization_required(
        session_manager,
        session_id,
        method,
        is_initialized_notification,
        is_client_response,
    )
    .await
    {
        return response;
    }

    if is_client_response {
        return handle_client_response(session_manager, session_id, payload).await;
    }
    forward_request(
        session_manager,
        session_id,
        method,
        payload,
        is_initialized_notification,
    )
    .await
}

/// Checks whether MCP initialization is required and waits for it if so.
///
/// Returns `Some(Response)` if initialization timed out, `None` to proceed.
async fn check_initialization_required(
    session_manager: &SessionManager,
    session_id: &str,
    method: Option<&str>,
    is_initialized_notification: bool,
    is_client_response: bool,
) -> Option<Response> {
    if is_initialized_notification || is_client_response {
        return None;
    }

    let session = session_manager.get_session(session_id)?;
    if session.is_mcp_initialized() {
        return None;
    }

    wait_for_initialization(&session, session_id, method).await
}

fn is_client_response(method: Option<&str>, payload: &Value) -> bool {
    method.is_none()
        && payload.get("id").is_some()
        && (payload.get("result").is_some() || payload.get("error").is_some())
}

fn build_handshake_timeout_response(
    session_manager: &SessionManager,
    session_id: &str,
    elapsed_secs: u64,
    sse_connected: bool,
    mcp_initialized: bool,
) -> Response {
    let error_msg = handshake_timeout_message(
        elapsed_secs,
        sse_connected,
        mcp_initialized,
        session_manager.requires_client_roots(),
    );
    error!(session_id = %session_id, "Handshake timeout: SSE={}, initialized={}", sse_connected, mcp_initialized);
    with_session_header(
        error_response_with_status(StatusCode::GATEWAY_TIMEOUT, -32002, &error_msg),
        session_id,
    )
}

fn get_conflict_message(requires_client_roots: bool) -> &'static str {
    if requires_client_roots {
        "Sandbox initializing from client roots. This server requires roots/list from client; configure --sandbox-scope for clients without roots support."
    } else {
        "Sandbox initializing from client roots or explicit fallback scope - retry tools/call after handshake completes"
    }
}

/// Checks if the sandbox is locked for `tools/call` requests.
fn check_sandbox_lock(session_manager: &SessionManager, session_id: &str) -> Option<Response> {
    let session = session_manager.get_session(session_id)?;

    match session.current_sandbox_state() {
        ahma_common::sandbox_state::SandboxState::Active { .. } => return None,
        ahma_common::sandbox_state::SandboxState::Failed { error } => {
            return Some(with_session_header(
                error_response_with_status(
                    StatusCode::FORBIDDEN,
                    -32000,
                    &format!("Sandbox configuration failed: {}", error),
                ),
                session_id,
            ));
        }
        ahma_common::sandbox_state::SandboxState::Terminated => {
            return Some(with_session_header(
                error_response_with_status(StatusCode::FORBIDDEN, -32000, "Session terminated"),
                session_id,
            ));
        }
        ahma_common::sandbox_state::SandboxState::AwaitingRoots
        | ahma_common::sandbox_state::SandboxState::Configuring { .. } => {}
    }

    let (sse_connected, mcp_initialized) =
        (session.is_sse_connected(), session.is_mcp_initialized());
    debug!(session_id = %session_id, sse_connected, mcp_initialized, sandbox_locked = false, "tools/call blocked - sandbox not yet locked");

    if let Some(elapsed_secs) = session.is_handshake_timed_out() {
        return Some(build_handshake_timeout_response(
            session_manager,
            session_id,
            elapsed_secs,
            sse_connected,
            mcp_initialized,
        ));
    }

    Some(with_session_header(
        error_response_with_status(
            StatusCode::CONFLICT,
            -32001,
            get_conflict_message(session_manager.requires_client_roots()),
        ),
        session_id,
    ))
}

/// Build the detailed error message for a handshake timeout.
fn handshake_timeout_message(
    elapsed_secs: u64,
    sse_connected: bool,
    mcp_initialized: bool,
    requires_roots: bool,
) -> String {
    let roots_requirement = if requires_roots {
        "No explicit server sandbox scope is configured; client roots/list is required."
    } else {
        "Server has explicit fallback sandbox scope configured for no-roots clients."
    };

    format!(
        "Handshake timeout after {}s - sandbox not locked. \
            SSE connected: {}, MCP initialized: {}. \
            Ensure client: 1) opens SSE stream (GET /mcp with session header), \
            2) sends notifications/initialized, \
            3) responds to roots/list request over SSE. {} \
            Use --handshake-timeout-secs to adjust timeout.",
        elapsed_secs, sse_connected, mcp_initialized, roots_requirement
    )
}

/// Waits for MCP initialization before forwarding a request.
async fn wait_for_initialization(
    session: &crate::session::Session,
    session_id: &str,
    method: Option<&str>,
) -> Option<Response> {
    debug!(
        session_id = %session_id,
        method = ?method,
        "Waiting for MCP initialization before forwarding request"
    );

    let init_timeout = Duration::from_secs(30);
    let wait_result = tokio::time::timeout(init_timeout, session.wait_for_mcp_initialized()).await;

    debug!(
        session_id = %session_id,
        method = ?method,
        "Wait for MCP initialization result: {:?}",
        wait_result
    );

    if wait_result.is_err() {
        warn!(
            session_id = %session_id,
            method = ?method,
            "Timeout waiting for MCP initialization"
        );
        return Some(with_session_header(
            error_response_with_status(
                StatusCode::GATEWAY_TIMEOUT,
                -32002,
                "Timeout waiting for MCP initialization - client must send notifications/initialized first",
            ),
            session_id,
        ));
    }
    debug!(
        session_id = %session_id,
        method = ?method,
        "MCP initialized, proceeding with request"
    );
    None
}

/// Handles a client response (e.g. to `roots/list`).
async fn handle_client_response(
    session_manager: &SessionManager,
    session_id: &str,
    payload: &Value,
) -> Response {
    let response_id = payload.get("id");
    let has_result = payload.get("result").is_some();
    let has_error = payload.get("error").is_some();

    let mut is_roots_list = false;
    if let Some(id_val) = response_id {
        let id_str = id_val
            .as_str()
            .map_or_else(|| id_val.to_string(), str::to_string);
        if let Some(session) = session_manager.get_session(session_id) {
            if let Some((_, sender)) = session.routed_requests.remove(&id_str) {
                let _ = sender.send(payload.clone());
                return with_session_header(
                    json_response_with_status(StatusCode::ACCEPTED, serde_json::json!({})),
                    session_id,
                );
            }
            if let Some((_, method)) = session.pending_client_requests.remove(&id_str)
                && method == "roots/list"
            {
                is_roots_list = true;
            }
        }
    }

    debug!(
        session_id = %session_id,
        response_id = ?response_id,
        has_result = has_result,
        has_error = has_error,
        "Received client response (SSE callback), forwarding to subprocess"
    );

    // Check if this is a roots/list response - extract roots and lock sandbox
    if is_roots_list && let Some(result) = payload.get("result") {
        try_lock_sandbox_from_roots(session_manager, session_id, result).await;
    }

    // Always forward response to subprocess
    if let Err(e) = session_manager.send_message(session_id, payload).await {
        error!(
            session_id = %session_id,
            "Failed to forward client response: {}", e
        );
        return error_response(-32603, &format!("Failed to forward response: {}", e));
    }

    with_session_header(
        json_response_with_status(StatusCode::ACCEPTED, serde_json::json!({})),
        session_id,
    )
}

/// Returns true if the sandbox should be locked based on the roots list.
///
/// An empty `mcp_roots` slice must NOT trigger a lock even when SSE is
/// connected.  Locking on empty roots would silently scope every tool call to
/// an empty sandbox, causing confusing "path outside sandbox" errors instead of
/// the observable HTTP 409 / JSON-RPC -32001 that tells the user to open a
/// workspace folder or supply `--sandbox-scope`.
fn should_lock_sandbox(mcp_roots: &[McpRoot]) -> bool {
    !mcp_roots.is_empty()
}

fn collect_valid_mcp_roots(session_id: &str, roots: &[Value]) -> Vec<McpRoot> {
    let mcp_roots: Vec<McpRoot> = roots
        .iter()
        .filter_map(|root| {
            let parsed = serde_json::from_value::<McpRoot>(root.clone());
            match &parsed {
                Ok(parsed_root) => {
                    info!(session_id = %session_id, "Successfully parsed root: {:?}", parsed_root)
                }
                Err(e) => warn!(session_id = %session_id, "Failed to parse root {:?}: {}", root, e),
            }
            parsed.ok()
        })
        .collect();

    info!(
        session_id = %session_id,
        "Extracted {} valid McpRoot instances from {} raw roots",
        mcp_roots.len(),
        roots.len()
    );

    mcp_roots
}

fn parse_roots_list_result(session_id: &str, result: &Value) -> Option<Vec<McpRoot>> {
    let Some(roots) = result.get("roots").and_then(|r| r.as_array()) else {
        warn!(session_id = %session_id, "roots/list response missing 'roots' array or it is invalid: {:?}", result);
        return Some(vec![]);
    };

    Some(collect_valid_mcp_roots(session_id, roots))
}

/// Attempt to lock sandbox from a `roots/list` style result payload.
async fn try_lock_sandbox_from_roots(
    session_manager: &SessionManager,
    session_id: &str,
    result: &Value,
) {
    let Some(mcp_roots) = parse_roots_list_result(session_id, result) else {
        return;
    };

    // An empty roots list is a *decision point*, not a no-op.
    //
    // This used to just `return`, leaving the session parked in `AwaitingRoots`
    // forever: `resolve_sandbox_scopes` — which already knows exactly what to do
    // here (use the configured fallback scope, or reject with an actionable
    // message) — was never even called. The session then hung on whatever won the
    // race: the gate correctly 409ing, or the subprocess's own zero-scope
    // `configured` notification prematurely opening it.
    //
    // So: with a fallback scope configured, fall through and let
    // `resolve_sandbox_scopes` apply it. Without one, fail the session *now*, with
    // the reason, so `tools/call` returns a deterministic 403 instead of hanging
    // until the handshake times out.
    if !should_lock_sandbox(&mcp_roots) {
        if session_manager.requires_client_roots() {
            warn!(
                session_id = %session_id,
                "Client returned an empty roots list and no fallback sandbox scope is \
                 configured; failing the session rather than leaving it unlocked"
            );
            session_manager.fail_sandbox(
                session_id,
                "Client did not provide roots/list entries. Configure an explicit sandbox \
                 scope on server startup (e.g. --sandbox-scope /path/to/project) or use a \
                 client that supports roots/list.",
            );
            return;
        }

        debug!(
            session_id = %session_id,
            "Roots list is empty (client has no workspace folder open); using the configured \
             fallback sandbox scope"
        );
    }

    info!(
        session_id = %session_id,
        roots = ?mcp_roots,
        "Locking sandbox from roots/list response"
    );

    match session_manager.lock_sandbox(session_id, &mcp_roots).await {
        Ok(true) => {
            info!(
                session_id = %session_id,
                "Sandbox locked from first roots/list response"
            );
        }
        Ok(false) => {}
        Err(e) => {
            warn!(
                session_id = %session_id,
                "Failed to record sandbox scopes: {}", e
            );
        }
    }
}

/// Handle roots/list response side-effect: lock sandbox.
async fn handle_roots_list_response(
    session_manager: &SessionManager,
    session_id: &str,
    method: Option<&str>,
    response: &Value,
) {
    if method == Some("roots/list")
        && let Some(result) = response.get("result")
    {
        try_lock_sandbox_from_roots(session_manager, session_id, result).await;
    }
}

/// Mark the session as MCP-initialized if applicable.
async fn mark_session_initialized(
    session_manager: &SessionManager,
    session_id: &str,
    is_initialized_notification: bool,
) {
    if !is_initialized_notification {
        return;
    }
    if let Some(session) = session_manager.get_session(session_id) {
        match session.mark_mcp_initialized().await {
            Ok(true) | Ok(false) => {
                // Auto-lock from default_scope if configured so that clients
                // that don't send roots/list (or don't open an SSE stream) still work.
                session_manager.auto_lock_if_default_scope(session_id).await;
            }
            Err(e) => {
                warn!(
                    session_id = %session_id,
                    "Failed to mark MCP initialized: {}", e
                );
            }
        }
    }
}

/// Forwards a request to the session manager.
async fn forward_request(
    session_manager: &SessionManager,
    session_id: &str,
    method: Option<&str>,
    payload: &Value,
    is_initialized_notification: bool,
) -> Response {
    let request_timeout = if method == Some("tools/call") {
        calculate_tool_timeout(payload)
    } else {
        Duration::from_secs(request_timeout_secs())
    };

    match session_manager
        .send_request(session_id, payload, Some(request_timeout))
        .await
    {
        Ok(response) => {
            handle_roots_list_response(session_manager, session_id, method, &response).await;
            mark_session_initialized(session_manager, session_id, is_initialized_notification)
                .await;
            with_session_header(json_response(response), session_id)
        }
        // A per-request timeout is recoverable: the subprocess is alive and the
        // operation is still running, only our wait window elapsed. Return it as
        // an HTTP 200 JSON-RPC error so the session survives (see
        // `request_timeout_response`). A genuine transport/protocol failure still
        // gets a fatal -32603 / HTTP 500 below.
        Err(BridgeError::Timeout) => {
            warn!(
                session_id = %session_id,
                "Forwarded request timed out; operation still running — returning \
                 recoverable timeout (session preserved)"
            );
            with_session_header(request_timeout_response(payload), session_id)
        }
        Err(e) => {
            error!(session_id = %session_id, "Failed to send request: {}", e);
            error_response(-32603, &format!("Failed to send request: {}", e))
        }
    }
}

/// Bridge wait budget for the `await` meta-tool.
///
/// `await` blocks in-process for up to its own ceiling — `DEFAULT_AWAIT_TIMEOUT`
/// in `ahma_mcp::mcp_service::handlers::await_tool` (600s), capped by the same
/// 600s ceiling that `calculate_tool_timeout` applies to operations — and then
/// returns a graceful "still running" result. The bridge must grant it a
/// strictly larger budget so the in-process path fires first; otherwise the
/// bridge guillotines the call and (before the recoverable-timeout fix) tore the
/// whole MCP session down. The +60s margin covers scheduling/IO slack.
const AWAIT_TOOL_BRIDGE_TIMEOUT_SECS: u64 = 660;

fn calculate_tool_timeout(payload: &Value) -> Duration {
    let tool_name = payload
        .get("params")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str());

    // `await` self-bounds in-process and returns gracefully; give it headroom
    // over its own ceiling rather than the short default tool-call budget.
    if tool_name == Some("await") {
        return Duration::from_secs(AWAIT_TOOL_BRIDGE_TIMEOUT_SECS);
    }

    let arg_timeout_secs = payload
        .get("params")
        .and_then(|p| p.get("arguments"))
        .and_then(|a| a.get("timeout_seconds"))
        .and_then(|v| v.as_u64());

    let default_secs = tool_call_timeout_secs();
    let effective_secs = arg_timeout_secs
        .map(|v| v.min(600)) // Cap at 10 minutes
        .unwrap_or(default_secs);

    Duration::from_secs(effective_secs)
}

// ─── POST SSE streaming ──────────────────────────────────────────────

fn serialize_sse_data(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn sse_event(id: u64, data: String) -> Event {
    Event::default().id(id.to_string()).data(data)
}

fn sse_single_event_response_with_id(id: u64, data: String) -> Response {
    let event_stream = stream::once(async move { Ok::<_, Infallible>(sse_event(id, data)) });
    Sse::new(event_stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn session_sse_event(session: &crate::session::Session, value: &Value) -> (u64, String) {
    let json_str = serialize_sse_data(value);
    let id = session.assign_event_id(&json_str);
    (id, json_str)
}

/// Handles POST requests that accept `text/event-stream` (SSE) responses.
///
/// Per MCP Streamable HTTP spec, POST with `Accept: text/event-stream` returns
/// an SSE stream containing the JSON-RPC response event plus any interleaved
/// server notifications. For requests (with `id`), the stream forwards broadcast
/// events and delivers the response, then closes. For notifications (no `id`),
/// a single acknowledgment event is returned.
/// Handles requests in session isolation mode (SSE transport).
#[tracing::instrument(skip_all, fields(method, session_id))]
fn build_interleaved_sse_stream(
    session: Arc<crate::session::Session>,
    session_id: String,
    rx: tokio::sync::broadcast::Receiver<(u64, String)>,
    response: Value,
) -> impl futures::Stream<Item = Result<Event, Infallible>> {
    let (response_id, response_json) = session_sse_event(&session, &response);

    let session_clone = session.clone();
    let sid = session_id;
    let notification_stream = BroadcastStream::new(rx).filter_map(move |result| {
        let sid = sid.clone();
        let session_ref = session_clone.clone();
        async move {
            match result {
                Ok((id, msg)) => {
                    debug!(session_id = %sid, event_id = id, "POST SSE notification: {}", msg);
                    Some(Ok::<_, Infallible>(sse_event(id, msg)))
                }
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                    session_ref.record_lagged_events(n);
                    Some(Ok(
                        Event::default().comment(format!("lagged: {} events dropped", n))
                    ))
                }
            }
        }
    });

    let response_event =
        stream::once(async move { Ok::<_, Infallible>(sse_event(response_id, response_json)) });

    notification_stream
        .take_until(tokio::time::sleep(Duration::from_millis(50)))
        .chain(response_event)
}

async fn check_sse_request_gating(
    session_manager: &SessionManager,
    session_id: &str,
    method: Option<&str>,
    payload: &Value,
    is_initialized_notification: bool,
) -> Option<Response> {
    // Validate session exists
    if let Some(response) = check_session_exists(session_manager, session_id) {
        return Some(response);
    }

    // Client responses and notifications that modify state use the same JSON path
    let is_client_response = is_client_response(method, payload);
    if is_client_response {
        return Some(handle_client_response(session_manager, session_id, payload).await);
    }

    // Roots changed check
    if method == Some("notifications/roots/list_changed")
        && let Some(response) = handle_roots_changed_request(session_manager, session_id).await
    {
        return Some(response);
    }

    // Sandbox gating for tools/call
    if method == Some("tools/call")
        && let Some(response) = check_sandbox_lock(session_manager, session_id)
    {
        return Some(response);
    }

    // Wait for MCP initialization if needed
    if let Some(response) = check_initialization_required(
        session_manager,
        session_id,
        method,
        is_initialized_notification,
        false,
    )
    .await
    {
        return Some(response);
    }

    None
}

/// Handles requests in session isolation mode (SSE transport).
#[tracing::instrument(skip_all, fields(method, session_id))]
pub async fn handle_session_isolated_request_sse(
    session_manager: Arc<SessionManager>,
    headers: HeaderMap,
    payload: Value,
) -> Response {
    let session_id = headers
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let method = payload.get("method").and_then(|m| m.as_str());
    let has_id = payload.get("id").is_some();

    tracing::Span::current().record("method", method.unwrap_or(""));
    tracing::Span::current().record("session_id", session_id.as_deref().unwrap_or(""));
    debug!(method = ?method, session_id = ?session_id, has_id, "Incoming MCP POST SSE request");

    // Initialize: create session, forward, return SSE with response
    if method == Some("initialize") && session_id.is_none() {
        return handle_initialize_sse(&session_manager, &payload).await;
    }

    let Some(session_id) = session_id else {
        return missing_session_id_response();
    };

    if method == Some("sampling/createMessage") {
        return handle_routed_sampling_request(&session_manager, &session_id, &payload, true).await;
    }

    let is_initialized_notification = method == Some("notifications/initialized");

    if let Some(response) = check_sse_request_gating(
        &session_manager,
        &session_id,
        method,
        &payload,
        is_initialized_notification,
    )
    .await
    {
        return response;
    }

    // For notifications (no id): forward and return a single SSE ack event
    if !has_id {
        return forward_notification_sse(
            &session_manager,
            &session_id,
            method,
            &payload,
            is_initialized_notification,
        )
        .await;
    }

    // For requests (has id): subscribe to broadcast, forward request, stream
    // broadcast events + response event
    forward_request_sse(
        &session_manager,
        &session_id,
        method,
        &payload,
        is_initialized_notification,
    )
    .await
}

fn build_initialize_sse_response(
    session_manager: &Arc<SessionManager>,
    session_id: &str,
    response: &Value,
) -> Response {
    let (event_id, json_str) = session_manager
        .get_session(session_id)
        .map(|session| session_sse_event(&session, response))
        .unwrap_or_else(|| (1, serialize_sse_data(response)));
    sse_single_event_response_with_id(event_id, json_str)
}

/// Handle initialize with SSE response.
async fn handle_initialize_sse(session_manager: &Arc<SessionManager>, payload: &Value) -> Response {
    if let Some(err_response) = validate_initialize_payload(payload) {
        return err_response;
    }

    let new_session_id = match create_session_or_error(session_manager).await {
        Ok(id) => id,
        Err(e) => return e,
    };

    register_session_details(session_manager, &new_session_id, payload).await;

    match session_manager
        .send_request(
            &new_session_id,
            payload,
            Some(Duration::from_secs(request_timeout_secs())),
        )
        .await
    {
        Ok(response) => with_session_header(
            build_initialize_sse_response(session_manager, &new_session_id, &response),
            &new_session_id,
        ),
        Err(e) => handle_initialize_error(session_manager, &new_session_id, e).await,
    }
}

/// Forward a notification (no id) and return HTTP 202 Accepted.
///
/// Per MCP Streamable HTTP spec §3.2.1, the server MUST respond with HTTP 202
/// for JSON-RPC notifications (messages without an `id`).  Returning an SSE
/// stream here causes rmcp clients that call `expect_accepted_or_json()` to
/// reject the response with `UnexpectedServerResponse("expect accepted or json,
/// got Sse(...)")`, which terminates the proxy transport and — when the
/// transport is the stdio proxy's Unix-socket client — causes BrokenPipe on
/// the next stdin write from the test driver.
async fn forward_notification_sse(
    session_manager: &SessionManager,
    session_id: &str,
    _method: Option<&str>,
    payload: &Value,
    is_initialized_notification: bool,
) -> Response {
    let request_timeout = Duration::from_secs(request_timeout_secs());

    match session_manager
        .send_request(session_id, payload, Some(request_timeout))
        .await
    {
        Ok(_response) => {
            mark_session_initialized(session_manager, session_id, is_initialized_notification)
                .await;
            with_session_header(StatusCode::ACCEPTED.into_response(), session_id)
        }
        Err(e) => {
            error!(session_id = %session_id, "Failed to forward notification: {}", e);
            error_response(-32603, &format!("Failed to send request: {}", e))
        }
    }
}

/// Forward a request (has id) and return an SSE stream with interleaved
/// broadcast events and the response event.
async fn forward_request_sse(
    session_manager: &SessionManager,
    session_id: &str,
    method: Option<&str>,
    payload: &Value,
    is_initialized_notification: bool,
) -> Response {
    let session = match session_manager.get_session(session_id) {
        Some(s) => s,
        None => return session_not_found_response(),
    };

    // Subscribe to broadcast BEFORE sending the request so we don't miss events
    let rx = session.subscribe();

    let request_timeout = if method == Some("tools/call") {
        calculate_tool_timeout(payload)
    } else {
        Duration::from_secs(request_timeout_secs())
    };

    // Send the request to the subprocess
    let response_result = session_manager
        .send_request(session_id, payload, Some(request_timeout))
        .await;

    match response_result {
        Ok(response) => {
            handle_roots_list_response(session_manager, session_id, method, &response).await;
            mark_session_initialized(session_manager, session_id, is_initialized_notification)
                .await;

            // Build SSE stream: broadcast events that arrived during processing + the response
            let combined =
                build_interleaved_sse_stream(session, session_id.to_string(), rx, response);

            with_session_header(
                Sse::new(combined)
                    .keep_alive(KeepAlive::default())
                    .into_response(),
                session_id,
            )
        }
        Err(e) => {
            error!(session_id = %session_id, "Failed to send request: {}", e);
            error_response(-32603, &format!("Failed to send request: {}", e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::{BoxFuture, PeerFactory, PeerStreams};
    use crate::session::SessionManagerConfig;
    use ahma_common::sandbox_state::SandboxState;
    use serde_json::json;
    use std::path::PathBuf;

    // ─── Test helpers ────────────────────────────────────────────────────

    /// A peer whose "stdout" never reaches EOF, so the bridge I/O loop blocks
    /// and the session stays alive (not terminated) for the duration of a test.
    struct KeepAlivePeerFactory;
    impl PeerFactory for KeepAlivePeerFactory {
        fn create(&self) -> BoxFuture<anyhow::Result<PeerStreams>> {
            Box::pin(async move {
                let (bridge_end, peer_end) = tokio::io::duplex(1024);
                // Leak the peer end so the bridge read side never sees EOF.
                std::mem::forget(peer_end);
                let (bridge_read, bridge_write) = tokio::io::split(bridge_end);
                Ok(PeerStreams {
                    stdin: Box::new(bridge_write),
                    stdout: Box::new(bridge_read),
                    stderr: None,
                    shutdown_fn: None,
                    exit_cause: None,
                })
            })
        }
    }

    /// A peer factory that always fails to construct.
    struct FailingPeerFactory;
    impl PeerFactory for FailingPeerFactory {
        fn create(&self) -> BoxFuture<anyhow::Result<PeerStreams>> {
            Box::pin(async move { Err(anyhow::anyhow!("peer construction failed")) })
        }
    }

    fn manager_with(
        default_scope: Option<PathBuf>,
        max_sessions: usize,
        handshake_timeout_secs: u64,
        factory: Arc<dyn PeerFactory>,
    ) -> Arc<SessionManager> {
        Arc::new(SessionManager::new(SessionManagerConfig {
            server_command: String::new(),
            server_args: vec![],
            default_scope,
            enable_colored_output: false,
            handshake_timeout_secs,
            max_sessions,
            peer_factory: Some(factory),
        }))
    }

    fn keepalive_manager() -> Arc<SessionManager> {
        manager_with(None, 10, 3600, Arc::new(KeepAlivePeerFactory))
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body bytes");
        serde_json::from_slice(&bytes).expect("body is valid JSON")
    }

    async fn body_string(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body bytes");
        String::from_utf8_lossy(&bytes).to_string()
    }

    fn header_session_id(resp: &Response) -> Option<String> {
        resp.headers()
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    }

    fn content_type(resp: &Response) -> String {
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }

    fn headers_with_session(id: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(MCP_SESSION_ID_HEADER, HeaderValue::from_str(id).unwrap());
        h
    }

    // ─── Pure response builders ─────────────────────────────────────────

    #[tokio::test]
    async fn json_response_is_ok_with_json_body() {
        let resp = json_response(json!({"a": 1, "b": "x"}));
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(content_type(&resp).contains("application/json"));
        assert_eq!(body_json(resp).await, json!({"a": 1, "b": "x"}));
    }

    #[tokio::test]
    async fn json_response_with_status_uses_given_status() {
        let resp = json_response_with_status(StatusCode::IM_A_TEAPOT, json!({"k": true}));
        assert_eq!(resp.status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(body_json(resp).await, json!({"k": true}));
    }

    #[test]
    fn json_rpc_error_value_has_code_and_message() {
        let v = json_rpc_error_value(-32000, "boom");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["error"]["code"], -32000);
        assert_eq!(v["error"]["message"], "boom");
    }

    #[test]
    fn with_session_header_attaches_valid_id() {
        let resp = with_session_header(json_response(json!({})), "session-abc");
        assert_eq!(header_session_id(&resp).as_deref(), Some("session-abc"));
    }

    #[test]
    fn with_session_header_falls_back_on_invalid_id() {
        // Newline is not a valid header value character.
        let resp = with_session_header(json_response(json!({})), "bad\nvalue");
        assert_eq!(header_session_id(&resp).as_deref(), Some("invalid"));
    }

    #[tokio::test]
    async fn error_response_defaults_to_500() {
        let resp = error_response(-32603, "internal");
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], -32603);
        assert_eq!(body["error"]["message"], "internal");
    }

    #[tokio::test]
    async fn error_response_with_status_honors_status() {
        let resp = error_response_with_status(StatusCode::FORBIDDEN, -32000, "nope");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_json(resp).await["error"]["code"], -32000);
    }

    #[tokio::test]
    async fn missing_session_id_response_is_400() {
        let resp = missing_session_id_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], -32600);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Missing")
        );
    }

    #[tokio::test]
    async fn session_not_found_response_is_403() {
        let resp = session_not_found_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], -32600);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not found")
        );
    }

    // ─── validate_initialize_payload ────────────────────────────────────

    #[tokio::test]
    async fn validate_initialize_payload_rejects_missing_protocol_version() {
        let payload = json!({"jsonrpc": "2.0", "method": "initialize", "params": {}});
        let resp = validate_initialize_payload(&payload).expect("should reject");
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_json(resp).await["error"]["code"], -32602);
    }

    #[test]
    fn validate_initialize_payload_accepts_valid() {
        let payload = json!({"params": {"protocolVersion": "2025-03-26"}});
        assert!(validate_initialize_payload(&payload).is_none());
    }

    // ─── is_client_response ─────────────────────────────────────────────

    #[test]
    fn is_client_response_true_for_result_without_method() {
        let payload = json!({"id": 1, "result": {}});
        assert!(is_client_response(None, &payload));
    }

    #[test]
    fn is_client_response_true_for_error_without_method() {
        let payload = json!({"id": 1, "error": {"code": -1}});
        assert!(is_client_response(None, &payload));
    }

    #[test]
    fn is_client_response_false_when_method_present() {
        let payload = json!({"id": 1, "result": {}});
        assert!(!is_client_response(Some("tools/call"), &payload));
    }

    #[test]
    fn is_client_response_false_without_result_or_error() {
        let payload = json!({"id": 1});
        assert!(!is_client_response(None, &payload));
    }

    #[test]
    fn is_client_response_false_without_id() {
        let payload = json!({"result": {}});
        assert!(!is_client_response(None, &payload));
    }

    // ─── get_conflict_message / handshake_timeout_message ───────────────

    #[test]
    fn get_conflict_message_branches() {
        assert!(get_conflict_message(true).contains("configure --sandbox-scope"));
        assert!(get_conflict_message(false).contains("explicit fallback scope"));
    }

    #[test]
    fn handshake_timeout_message_branches() {
        let req = handshake_timeout_message(12, true, false, true);
        assert!(req.contains("client roots/list is required"));
        assert!(req.contains("12s"));
        let fallback = handshake_timeout_message(7, false, true, false);
        assert!(fallback.contains("explicit fallback sandbox scope"));
    }

    // ─── should_lock_sandbox / roots parsing ────────────────────────────

    #[test]
    fn should_lock_sandbox_only_when_non_empty() {
        assert!(!should_lock_sandbox(&[]));
        let roots = vec![McpRoot {
            uri: "file:///a".into(),
            name: None,
        }];
        assert!(should_lock_sandbox(&roots));
    }

    #[test]
    fn collect_valid_mcp_roots_keeps_only_parseable() {
        let all_valid = vec![
            json!({"uri": "file:///a"}),
            json!({"uri": "file:///b", "name": "b"}),
        ];
        assert_eq!(collect_valid_mcp_roots("sid", &all_valid).len(), 2);

        let mixed = vec![
            json!({"uri": "file:///a"}),
            json!({"name": "missing uri"}),
            json!(42),
        ];
        assert_eq!(collect_valid_mcp_roots("sid", &mixed).len(), 1);

        let none_valid = vec![json!({"name": "x"}), json!("a string")];
        assert_eq!(collect_valid_mcp_roots("sid", &none_valid).len(), 0);
    }

    #[test]
    fn parse_roots_list_result_handles_all_shapes() {
        let with_roots = json!({"roots": [{"uri": "file:///a"}, {"uri": "file:///b"}]});
        assert_eq!(
            parse_roots_list_result("sid", &with_roots).unwrap().len(),
            2
        );

        let no_roots = json!({"something": "else"});
        assert!(
            parse_roots_list_result("sid", &no_roots)
                .unwrap()
                .is_empty()
        );

        let roots_not_array = json!({"roots": "oops"});
        assert!(
            parse_roots_list_result("sid", &roots_not_array)
                .unwrap()
                .is_empty()
        );
    }

    // ─── calculate_tool_timeout ─────────────────────────────────────────

    #[test]
    fn calculate_tool_timeout_uses_argument_value() {
        let payload = json!({"params": {"arguments": {"timeout_seconds": 30}}});
        assert_eq!(calculate_tool_timeout(&payload), Duration::from_secs(30));
    }

    #[test]
    fn calculate_tool_timeout_caps_at_600() {
        let payload = json!({"params": {"arguments": {"timeout_seconds": 9999}}});
        assert_eq!(calculate_tool_timeout(&payload), Duration::from_secs(600));
    }

    #[test]
    fn calculate_tool_timeout_defaults_without_arguments() {
        let payload = json!({"params": {"arguments": {}}});
        assert_eq!(
            calculate_tool_timeout(&payload),
            Duration::from_secs(tool_call_timeout_secs())
        );
        let no_params = json!({"method": "tools/call"});
        assert_eq!(
            calculate_tool_timeout(&no_params),
            Duration::from_secs(tool_call_timeout_secs())
        );
    }

    #[test]
    fn calculate_tool_timeout_await_gets_budget_above_inprocess_ceiling() {
        // `await` blocks in-process up to 600s and then returns a graceful
        // "still running" result. The bridge budget must STRICTLY EXCEED that
        // ceiling so the in-process path fires first instead of the bridge
        // guillotining the call.
        let payload = json!({"params": {"name": "await", "arguments": {}}});
        assert_eq!(
            calculate_tool_timeout(&payload),
            Duration::from_secs(AWAIT_TOOL_BRIDGE_TIMEOUT_SECS)
        );
        const {
            assert!(
                AWAIT_TOOL_BRIDGE_TIMEOUT_SECS > 600,
                "await bridge budget must exceed the 600s in-process await ceiling"
            )
        };
        // The extended budget applies regardless of any client-sent timeout arg.
        let with_arg = json!({"params": {"name": "await", "arguments": {"timeout_seconds": 5}}});
        assert_eq!(
            calculate_tool_timeout(&with_arg),
            Duration::from_secs(AWAIT_TOOL_BRIDGE_TIMEOUT_SECS)
        );
    }

    // ─── SSE helpers ────────────────────────────────────────────────────

    #[test]
    fn serialize_sse_data_serializes_value() {
        assert_eq!(serialize_sse_data(&json!({"x": 1})), "{\"x\":1}");
    }

    #[tokio::test]
    async fn sse_single_event_response_carries_data_and_id() {
        let resp = sse_single_event_response_with_id(7, "payload-data".to_string());
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(content_type(&resp).contains("text/event-stream"));
        let body = body_string(resp).await;
        assert!(body.contains("payload-data"), "body was: {body}");
        assert!(body.contains('7'), "body was: {body}");
    }

    // ─── build_handshake_timeout_response ───────────────────────────────

    #[tokio::test]
    async fn build_handshake_timeout_response_is_504_with_header() {
        let mgr = manager_with(None, 10, 3600, Arc::new(KeepAlivePeerFactory));
        let resp = build_handshake_timeout_response(&mgr, "sid-x", 50, false, false);
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(header_session_id(&resp).as_deref(), Some("sid-x"));
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], -32002);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Handshake timeout")
        );
    }

    // ─── check_session_exists ───────────────────────────────────────────

    #[test]
    fn check_session_exists_returns_403_for_unknown() {
        let mgr = keepalive_manager();
        let resp = check_session_exists(&mgr, "does-not-exist").expect("should be Some");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn check_session_exists_none_for_live_session() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        assert!(check_session_exists(&mgr, &id).is_none());
    }

    // ─── check_sandbox_lock ─────────────────────────────────────────────

    #[test]
    fn check_sandbox_lock_none_for_unknown_session() {
        let mgr = keepalive_manager();
        assert!(check_sandbox_lock(&mgr, "missing").is_none());
    }

    #[tokio::test]
    async fn check_sandbox_lock_conflict_while_awaiting_roots() {
        let mgr = manager_with(None, 10, 3600, Arc::new(KeepAlivePeerFactory));
        let id = mgr.create_session().await.expect("create session");
        let resp = check_sandbox_lock(&mgr, &id).expect("should be Some");
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(header_session_id(&resp).as_deref(), Some(id.as_str()));
        assert_eq!(body_json(resp).await["error"]["code"], -32001);
    }

    #[tokio::test]
    async fn check_sandbox_lock_times_out_when_handshake_expired() {
        // handshake_timeout_secs = 0 → immediately timed out.
        let mgr = manager_with(None, 10, 0, Arc::new(KeepAlivePeerFactory));
        let id = mgr.create_session().await.expect("create session");
        let resp = check_sandbox_lock(&mgr, &id).expect("should be Some");
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body_json(resp).await["error"]["code"], -32002);
    }

    // ─── Top-level entry points ─────────────────────────────────────────

    #[tokio::test]
    async fn isolated_request_missing_session_is_400() {
        let mgr = keepalive_manager();
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let resp = handle_session_isolated_request(mgr, HeaderMap::new(), payload).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn isolated_request_unknown_session_is_403() {
        let mgr = keepalive_manager();
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let resp =
            handle_session_isolated_request(mgr, headers_with_session("nope"), payload).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn isolated_sse_request_missing_session_is_400() {
        let mgr = keepalive_manager();
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let resp = handle_session_isolated_request_sse(mgr, HeaderMap::new(), payload).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn isolated_sse_request_unknown_session_is_403() {
        let mgr = keepalive_manager();
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let resp =
            handle_session_isolated_request_sse(mgr, headers_with_session("nope"), payload).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    // ─── handle_initialize validation paths ─────────────────────────────

    #[tokio::test]
    async fn handle_initialize_rejects_invalid_payload() {
        let mgr = keepalive_manager();
        let payload = json!({"jsonrpc": "2.0", "method": "initialize", "params": {}});
        let resp = handle_initialize(&mgr, &payload).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_json(resp).await["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn handle_initialize_sse_rejects_invalid_payload() {
        let mgr = keepalive_manager();
        let payload = json!({"jsonrpc": "2.0", "method": "initialize", "params": {}});
        let resp = handle_initialize_sse(&mgr, &payload).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_json(resp).await["error"]["code"], -32602);
    }

    // ─── create_session_or_error ────────────────────────────────────────

    #[tokio::test]
    async fn create_session_or_error_returns_429_on_limit() {
        let mgr = manager_with(None, 0, 3600, Arc::new(KeepAlivePeerFactory));
        let err = create_session_or_error(&mgr)
            .await
            .expect_err("should fail");
        assert_eq!(err.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body_json(err).await["error"]["code"], -32002);
    }

    #[tokio::test]
    async fn create_session_or_error_returns_500_on_factory_failure() {
        let mgr = manager_with(None, 10, 3600, Arc::new(FailingPeerFactory));
        let err = create_session_or_error(&mgr)
            .await
            .expect_err("should fail");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_json(err).await["error"]["code"], -32603);
    }

    #[tokio::test]
    async fn create_session_or_error_ok_returns_id() {
        let mgr = keepalive_manager();
        let id = create_session_or_error(&mgr).await.expect("should succeed");
        assert!(!id.is_empty());
    }

    // ─── sampling routing ───────────────────────────────────────────────

    #[tokio::test]
    async fn find_target_session_for_sampling_none_when_empty() {
        let mgr = keepalive_manager();
        assert!(
            find_target_session_for_sampling(&mgr, "sid", None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn routed_sampling_request_errors_without_target() {
        let mgr = keepalive_manager();
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sampling/createMessage",
            "params": {}
        });
        let resp = handle_routed_sampling_request(&mgr, "sid", &payload, false).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], -32603);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("No active IDE session")
        );
    }

    // ─── roots changed handling ─────────────────────────────────────────

    #[tokio::test]
    async fn roots_changed_request_proceeds_while_awaiting_roots() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        // AwaitingRoots → Ok(false) → None (proceed with normal handshake).
        assert!(handle_roots_changed_request(&mgr, &id).await.is_none());
    }

    #[tokio::test]
    async fn roots_changed_request_noop_after_lock() {
        let temp = tempfile::tempdir().unwrap();
        let mgr = manager_with(
            Some(temp.path().to_path_buf()),
            10,
            3600,
            Arc::new(KeepAlivePeerFactory),
        );
        let id = mgr.create_session().await.expect("create session");
        // Lock the sandbox via the default scope (empty roots).
        assert!(mgr.lock_sandbox(&id, &[]).await.expect("lock"));
        let resp = handle_roots_changed_request(&mgr, &id)
            .await
            .expect("locked → Some(202)");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        assert_eq!(header_session_id(&resp).as_deref(), Some(id.as_str()));
    }

    #[tokio::test]
    async fn roots_changed_request_errors_for_unknown_session() {
        let mgr = keepalive_manager();
        let resp = handle_roots_changed_request(&mgr, "missing")
            .await
            .expect("err → Some(403)");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    // ─── handle_client_response ─────────────────────────────────────────

    #[tokio::test]
    async fn handle_client_response_acks_forwarded_response() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let payload = json!({"jsonrpc": "2.0", "id": "abc", "result": {}});
        let resp = handle_client_response(&mgr, &id, &payload).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        assert_eq!(header_session_id(&resp).as_deref(), Some(id.as_str()));
    }

    #[tokio::test]
    async fn handle_client_response_errors_when_session_terminated() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        // Force the forward to fail.
        mgr.get_session(&id).unwrap().set_terminated(true);
        let payload = json!({"jsonrpc": "2.0", "id": "abc", "result": {}});
        let resp = handle_client_response(&mgr, &id, &payload).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_json(resp).await["error"]["code"], -32603);
    }

    // ─── try_lock_sandbox_from_roots ────────────────────────────────────

    /// Empty roots with no fallback scope must FAIL the session, not leave it in
    /// limbo.
    ///
    /// This test previously asserted the session stayed in `AwaitingRoots` — which
    /// encoded the bug. Parking there meant `resolve_sandbox_scopes` (which has a
    /// correct, actionable rejection for exactly this case) was never called, and
    /// the session's fate was decided by whatever won a race: the `tools/call` gate
    /// correctly 409ing, or the subprocess's own zero-scope `configured`
    /// notification prematurely flipping it to `Active` and opening the gate. That
    /// race is the CI flake in `test_empty_roots_rejection`.
    ///
    /// Failing immediately is both the honest answer and a deterministic one.
    #[tokio::test]
    async fn empty_roots_without_a_fallback_scope_fails_the_session() {
        let mgr = keepalive_manager();
        assert!(
            mgr.requires_client_roots(),
            "precondition: no fallback scope is configured"
        );
        let id = mgr.create_session().await.expect("create session");

        try_lock_sandbox_from_roots(&mgr, &id, &json!({"roots": []})).await;

        match mgr.get_session(&id).unwrap().current_sandbox_state() {
            SandboxState::Failed { error } => assert!(
                error.contains("roots/list") && error.contains("sandbox scope"),
                "the failure must tell the user how to fix it, got: {error}"
            ),
            other => panic!("empty roots must fail the session, got {other:?}"),
        }
    }

    /// With a fallback scope configured, empty roots are not an error at all — the
    /// fallback applies. The early `return` used to skip that too.
    #[tokio::test]
    async fn empty_roots_with_a_fallback_scope_locks_to_the_fallback() {
        let scope = std::env::temp_dir().join("ahma-fallback-scope");
        let mgr = manager_with(
            Some(scope.clone()),
            10,
            3600,
            Arc::new(KeepAlivePeerFactory),
        );
        assert!(
            !mgr.requires_client_roots(),
            "precondition: fallback exists"
        );
        let id = mgr.create_session().await.expect("create session");

        try_lock_sandbox_from_roots(&mgr, &id, &json!({"roots": []})).await;

        match mgr.get_session(&id).unwrap().current_sandbox_state() {
            SandboxState::Configuring { scopes } | SandboxState::Active { scopes } => {
                assert_eq!(scopes, vec![scope], "the fallback scope must be applied");
            }
            other => panic!("expected the fallback scope to be locked, got {other:?}"),
        }
    }

    // ─── forward_notification_sse ───────────────────────────────────────

    #[tokio::test]
    async fn forward_notification_sse_returns_202() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        // A notification (no id) → send_request returns immediately with null.
        let payload = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let resp =
            forward_notification_sse(&mgr, &id, Some("notifications/initialized"), &payload, true)
                .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        assert_eq!(header_session_id(&resp).as_deref(), Some(id.as_str()));
    }

    // ─── Additional helpers for sampling / roots tests ──────────────────

    /// Build a cross-platform `file://` URI for an absolute path.
    fn path_to_file_uri(path: &std::path::Path) -> String {
        let s = path.to_string_lossy().replace('\\', "/");
        if s.starts_with('/') {
            // Unix: "/abs" → "file:///abs"
            format!("file://{s}")
        } else {
            // Windows drive path: "C:/Users/.." → "file:///C:/Users/.."
            format!("file:///{s}")
        }
    }

    async fn make_sampling_session(
        mgr: &Arc<SessionManager>,
        name: &str,
    ) -> Arc<crate::session::Session> {
        let id = mgr.create_session().await.expect("create session");
        let session = mgr.get_session(&id).expect("session exists");
        session
            .set_client_info(json!({ "name": name }), json!({ "sampling": {} }))
            .await;
        session
    }

    // ─── session_has_sampling / session_name_matches_label ──────────────

    #[tokio::test]
    async fn session_has_sampling_reflects_capabilities() {
        let mgr = keepalive_manager();
        let with = make_sampling_session(&mgr, "Cursor").await;
        assert!(session_has_sampling(&with).await);

        let id = mgr.create_session().await.expect("create session");
        let without = mgr.get_session(&id).unwrap();
        // No capabilities set at all → no sampling.
        assert!(!session_has_sampling(&without).await);

        without
            .set_client_info(json!({"name": "x"}), json!({"roots": {}}))
            .await;
        assert!(!session_has_sampling(&without).await);
    }

    #[tokio::test]
    async fn session_name_matches_label_is_case_insensitive() {
        let mgr = keepalive_manager();
        let s = make_sampling_session(&mgr, "Cursor IDE").await;
        assert!(session_name_matches_label(&s, "cursor").await);
        assert!(session_name_matches_label(&s, "IDE").await);
        assert!(!session_name_matches_label(&s, "zed").await);

        // Session without client_info → empty name → only matches empty target.
        let id = mgr.create_session().await.expect("create session");
        let bare = mgr.get_session(&id).unwrap();
        assert!(!session_name_matches_label(&bare, "anything").await);
    }

    // ─── find_target_session_for_sampling ───────────────────────────────

    #[tokio::test]
    async fn find_target_session_prefers_label_match() {
        let mgr = keepalive_manager();
        let cursor = make_sampling_session(&mgr, "Cursor").await;
        let _vscode = make_sampling_session(&mgr, "VSCode").await;

        let found = find_target_session_for_sampling(&mgr, "current", Some("cursor"))
            .await
            .expect("should find a session");
        assert_eq!(found.id, cursor.id);
    }

    #[tokio::test]
    async fn find_target_session_falls_back_when_label_unmatched() {
        let mgr = keepalive_manager();
        let _a = make_sampling_session(&mgr, "VSCode").await;
        let _b = make_sampling_session(&mgr, "Zed").await;

        // No session named "cursor" → fallback to any session with sampling.
        let found = find_target_session_for_sampling(&mgr, "current", Some("cursor")).await;
        assert!(found.is_some());
        assert!(session_has_sampling(&found.unwrap()).await);
    }

    #[tokio::test]
    async fn find_target_session_none_when_no_sampling() {
        let mgr = keepalive_manager();
        // A session that exists but advertises no sampling.
        let id = mgr.create_session().await.expect("create session");
        mgr.get_session(&id)
            .unwrap()
            .set_client_info(json!({"name": "n"}), json!({"roots": {}}))
            .await;
        assert!(
            find_target_session_for_sampling(&mgr, "current", None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn find_target_session_excludes_current_session() {
        let mgr = keepalive_manager();
        let only = make_sampling_session(&mgr, "Cursor").await;
        // When the only sampling session IS the current one, it is excluded.
        assert!(
            find_target_session_for_sampling(&mgr, &only.id, None)
                .await
                .is_none()
        );
    }

    // ─── handle_routed_sampling_request: broadcast closed ───────────────

    #[tokio::test]
    async fn routed_sampling_errors_when_target_sse_channel_closed() {
        let mgr = keepalive_manager();
        let current = mgr.create_session().await.expect("create session");
        // Target has sampling but no SSE subscriber → broadcast fails.
        let _target = make_sampling_session(&mgr, "Cursor").await;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "sampling/createMessage",
            "params": {}
        });
        let resp = handle_routed_sampling_request(&mgr, &current, &payload, false).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], -32603);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("SSE channel is closed")
        );
    }

    // ─── handle_routed_sampling_request: success (json + sse) ───────────

    async fn drive_routed_response(target: Arc<crate::session::Session>, result: Value) {
        tokio::spawn(async move {
            for _ in 0..400 {
                let key = target
                    .routed_requests
                    .iter()
                    .next()
                    .map(|e| e.key().clone());
                if let Some(key) = key
                    && let Some((_, sender)) = target.routed_requests.remove(&key)
                {
                    let _ = sender.send(result.clone());
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
    }

    #[tokio::test]
    async fn routed_sampling_success_returns_json_with_original_id() {
        let mgr = keepalive_manager();
        let current = mgr.create_session().await.expect("create session");
        let target = make_sampling_session(&mgr, "Cursor").await;
        // Keep an SSE subscriber alive so broadcast succeeds.
        let _rx = target.subscribe();
        drive_routed_response(
            target.clone(),
            json!({"jsonrpc": "2.0", "result": {"ok": true}}),
        )
        .await;

        let payload = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "sampling/createMessage",
            "params": {}
        });
        let resp = handle_routed_sampling_request(&mgr, &current, &payload, false).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        // The original request id is restored onto the response.
        assert_eq!(body["id"], 42);
        assert_eq!(body["result"]["ok"], true);
    }

    #[tokio::test]
    async fn routed_sampling_success_returns_sse_event() {
        let mgr = keepalive_manager();
        let current = mgr.create_session().await.expect("create session");
        let target = make_sampling_session(&mgr, "Cursor").await;
        let _rx = target.subscribe();
        drive_routed_response(
            target.clone(),
            json!({"jsonrpc": "2.0", "result": {"answer": 7}}),
        )
        .await;

        let payload = json!({
            "jsonrpc": "2.0",
            "id": "req-1",
            "method": "sampling/createMessage",
            "params": { "__route_target_label": "cursor" }
        });
        let resp = handle_routed_sampling_request(&mgr, &current, &payload, true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(content_type(&resp).contains("text/event-stream"));
        assert_eq!(header_session_id(&resp).as_deref(), Some(current.as_str()));
        let body = body_string(resp).await;
        assert!(body.contains("answer"), "body was: {body}");
        assert!(body.contains("req-1"), "body was: {body}");
    }

    // ─── handle_session_isolated_request routing branches ───────────────

    #[tokio::test]
    async fn isolated_request_initialize_validates_payload() {
        let mgr = keepalive_manager();
        // initialize with no session id but invalid params → -32602.
        let payload = json!({"jsonrpc": "2.0", "method": "initialize", "params": {}});
        let resp = handle_session_isolated_request(mgr, HeaderMap::new(), payload).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_json(resp).await["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn isolated_request_routes_sampling_without_target() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sampling/createMessage",
            "params": {}
        });
        let resp = handle_session_isolated_request(mgr, headers_with_session(&id), payload).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            body_json(resp).await["error"]["message"]
                .as_str()
                .unwrap()
                .contains("No active IDE session")
        );
    }

    #[tokio::test]
    async fn isolated_sse_request_routes_sampling_without_target() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sampling/createMessage",
            "params": {}
        });
        let resp =
            handle_session_isolated_request_sse(mgr, headers_with_session(&id), payload).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_json(resp).await["error"]["code"], -32603);
    }

    // ─── handle_initialize_error ────────────────────────────────────────

    #[tokio::test]
    async fn handle_initialize_error_terminates_session_and_returns_500() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        assert!(mgr.session_exists(&id));
        let resp = handle_initialize_error(
            &mgr,
            &id,
            crate::error::BridgeError::Communication("boom".to_string()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_json(resp).await["error"]["code"], -32603);
        // Session is terminated/removed by the error handler.
        assert!(!mgr.session_exists(&id));
    }

    // ─── register_session_details ───────────────────────────────────────

    #[tokio::test]
    async fn register_session_details_stores_client_info_and_capabilities() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let payload = json!({
            "params": {
                "protocolVersion": "2025-03-26",
                "clientInfo": {"name": "Cursor"},
                "capabilities": {"sampling": {}}
            }
        });
        register_session_details(&mgr, &id, &payload).await;
        let session = mgr.get_session(&id).unwrap();
        assert_eq!(
            session.client_info.lock().await.clone().unwrap()["name"],
            "Cursor"
        );
        assert!(session.capabilities.lock().await.clone().unwrap()["sampling"].is_object());
    }

    #[tokio::test]
    async fn register_session_details_defaults_to_null_when_missing() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let payload = json!({"params": {"protocolVersion": "2025-03-26"}});
        register_session_details(&mgr, &id, &payload).await;
        let session = mgr.get_session(&id).unwrap();
        assert!(session.client_info.lock().await.clone().unwrap().is_null());
        assert!(session.capabilities.lock().await.clone().unwrap().is_null());
    }

    // ─── try_lock_sandbox_from_roots: non-empty roots ───────────────────

    #[tokio::test]
    async fn try_lock_sandbox_locks_from_valid_roots() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let temp = tempfile::tempdir().unwrap();
        let uri = path_to_file_uri(temp.path());
        let result = json!({"roots": [{"uri": uri}]});

        try_lock_sandbox_from_roots(&mgr, &id, &result).await;
        assert!(matches!(
            mgr.get_session(&id).unwrap().current_sandbox_state(),
            SandboxState::Configuring { .. }
        ));

        // Second call is a no-op (already locked → lock_sandbox Ok(false)).
        try_lock_sandbox_from_roots(&mgr, &id, &result).await;
        assert!(matches!(
            mgr.get_session(&id).unwrap().current_sandbox_state(),
            SandboxState::Configuring { .. }
        ));
    }

    #[tokio::test]
    async fn try_lock_sandbox_warns_when_roots_have_no_valid_file_uri() {
        // No default_scope and roots with a non-file URI → resolve fails → Err
        // branch (logged warning, sandbox stays AwaitingRoots).
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let result = json!({"roots": [{"uri": "https://example.com/not-a-file"}]});
        try_lock_sandbox_from_roots(&mgr, &id, &result).await;
        assert!(matches!(
            mgr.get_session(&id).unwrap().current_sandbox_state(),
            SandboxState::AwaitingRoots
        ));
    }

    // ─── handle_roots_list_response ─────────────────────────────────────

    #[tokio::test]
    async fn handle_roots_list_response_locks_on_roots_list_method() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let temp = tempfile::tempdir().unwrap();
        let response = json!({"result": {"roots": [{"uri": path_to_file_uri(temp.path())}]}});
        handle_roots_list_response(&mgr, &id, Some("roots/list"), &response).await;
        assert!(matches!(
            mgr.get_session(&id).unwrap().current_sandbox_state(),
            SandboxState::Configuring { .. }
        ));
    }

    #[tokio::test]
    async fn handle_roots_list_response_ignores_other_methods() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let temp = tempfile::tempdir().unwrap();
        let response = json!({"result": {"roots": [{"uri": path_to_file_uri(temp.path())}]}});
        handle_roots_list_response(&mgr, &id, Some("tools/list"), &response).await;
        // Not a roots/list method → no lock.
        assert!(matches!(
            mgr.get_session(&id).unwrap().current_sandbox_state(),
            SandboxState::AwaitingRoots
        ));
    }

    // ─── handle_client_response: routed + roots/list paths ──────────────

    #[tokio::test]
    async fn handle_client_response_resolves_routed_request() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let session = mgr.get_session(&id).unwrap();
        let (tx, rx) = oneshot::channel();
        session.routed_requests.insert("route-xyz".to_string(), tx);

        let payload = json!({"jsonrpc": "2.0", "id": "route-xyz", "result": {"v": 9}});
        let resp = handle_client_response(&mgr, &id, &payload).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        // The routed sender received the payload.
        let received = rx.await.expect("sender delivered");
        assert_eq!(received["result"]["v"], 9);
    }

    #[tokio::test]
    async fn handle_client_response_locks_sandbox_on_roots_list_reply() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let session = mgr.get_session(&id).unwrap();
        // Register the pending server→client roots/list request.
        session
            .pending_client_requests
            .insert("rl-7".to_string(), "roots/list".to_string());

        let temp = tempfile::tempdir().unwrap();
        let payload = json!({
            "jsonrpc": "2.0",
            "id": "rl-7",
            "result": {"roots": [{"uri": path_to_file_uri(temp.path())}]}
        });
        let resp = handle_client_response(&mgr, &id, &payload).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        assert!(matches!(
            mgr.get_session(&id).unwrap().current_sandbox_state(),
            SandboxState::Configuring { .. }
        ));
    }

    // ─── mark_session_initialized ───────────────────────────────────────

    #[tokio::test]
    async fn mark_session_initialized_noop_when_not_a_notification() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        mark_session_initialized(&mgr, &id, false).await;
        assert!(!mgr.get_session(&id).unwrap().is_mcp_initialized());
    }

    #[tokio::test]
    async fn mark_session_initialized_sets_flag_and_auto_locks_default_scope() {
        let temp = tempfile::tempdir().unwrap();
        let mgr = manager_with(
            Some(temp.path().to_path_buf()),
            10,
            3600,
            Arc::new(KeepAlivePeerFactory),
        );
        let id = mgr.create_session().await.expect("create session");
        mark_session_initialized(&mgr, &id, true).await;
        let session = mgr.get_session(&id).unwrap();
        assert!(session.is_mcp_initialized());
        // default_scope present → auto-locked into Configuring.
        assert!(matches!(
            session.current_sandbox_state(),
            SandboxState::Configuring { .. }
        ));
    }

    // ─── forward_request ────────────────────────────────────────────────

    #[tokio::test]
    async fn forward_request_ok_for_notification_returns_200() {
        // A notification (no id) returns immediately from send_request.
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let payload = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let resp =
            forward_request(&mgr, &id, Some("notifications/initialized"), &payload, true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(header_session_id(&resp).as_deref(), Some(id.as_str()));
        assert!(mgr.get_session(&id).unwrap().is_mcp_initialized());
    }

    #[tokio::test]
    async fn forward_request_tools_call_branch_uses_tool_timeout() {
        // tools/call with no id exercises the calculate_tool_timeout branch and
        // still returns immediately (notification semantics).
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let payload = json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {"arguments": {"timeout_seconds": 5}}
        });
        let resp = forward_request(&mgr, &id, Some("tools/call"), &payload, false).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn forward_request_timeout_is_recoverable_http_200_not_500() {
        // REGRESSION: a request the subprocess never answers must yield a
        // RECOVERABLE timeout — HTTP 200 with a JSON-RPC error carrying the
        // request id — NOT a fatal HTTP 500. A 500 makes the rmcp client raise
        // UnexpectedServerResponse, which the stdio proxy treats as fatal and
        // tears the whole MCP session down (the "one timeout kills every Ahma
        // tool" bug). The operation keeps running; only our wait window elapsed.
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        mark_session_initialized(&mgr, &id, true).await;
        // id present (so it's a real request, not a notification) and
        // timeout_seconds: 0 → the bridge wait window elapses immediately.
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "tools/call",
            "params": {"name": "run_terminal_command", "arguments": {"timeout_seconds": 0}}
        });
        let resp = forward_request(&mgr, &id, Some("tools/call"), &payload, false).await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a per-request timeout must not be a transport-fatal non-2xx"
        );
        assert_eq!(header_session_id(&resp).as_deref(), Some(id.as_str()));
        let body = body_json(resp).await;
        assert_eq!(
            body["id"],
            json!(42),
            "must echo the request id for correlation"
        );
        assert_eq!(body["error"]["code"], json!(-32002));
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("still running"),
            "message should explain the op continues: {body}"
        );
    }

    #[tokio::test]
    async fn forward_request_errors_when_session_terminated() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        mgr.get_session(&id).unwrap().set_terminated(true);
        let payload = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let resp =
            forward_request(&mgr, &id, Some("notifications/initialized"), &payload, true).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_json(resp).await["error"]["code"], -32603);
    }

    // ─── handle_existing_session_request branches ───────────────────────

    #[tokio::test]
    async fn existing_session_tools_call_blocked_before_sandbox_lock() {
        // HARD INVARIANT: tools/call before sandbox lock → HTTP 409 / -32001.
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {}});
        let resp = handle_existing_session_request(&mgr, &id, Some("tools/call"), &payload).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(body_json(resp).await["error"]["code"], -32001);
    }

    #[tokio::test]
    async fn existing_session_handles_client_response() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let payload = json!({"jsonrpc": "2.0", "id": "abc", "result": {}});
        let resp = handle_existing_session_request(&mgr, &id, None, &payload).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn existing_session_unknown_is_403() {
        let mgr = keepalive_manager();
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let resp =
            handle_existing_session_request(&mgr, "missing", Some("tools/list"), &payload).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    // ─── check_initialization_required / wait_for_initialization ────────

    #[tokio::test]
    async fn check_initialization_required_none_for_notification() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        assert!(
            check_initialization_required(
                &mgr,
                &id,
                Some("notifications/initialized"),
                true,
                false
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn check_initialization_required_none_for_client_response() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        assert!(
            check_initialization_required(&mgr, &id, None, false, true)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn check_initialization_required_none_for_unknown_session() {
        let mgr = keepalive_manager();
        assert!(
            check_initialization_required(&mgr, "missing", Some("tools/list"), false, false)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn check_initialization_required_none_when_already_initialized() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        mgr.get_session(&id)
            .unwrap()
            .mark_mcp_initialized()
            .await
            .unwrap();
        assert!(
            check_initialization_required(&mgr, &id, Some("tools/list"), false, false)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn wait_for_initialization_returns_none_when_already_initialized() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let session = mgr.get_session(&id).unwrap();
        session.mark_mcp_initialized().await.unwrap();
        assert!(
            wait_for_initialization(&session, &id, Some("tools/list"))
                .await
                .is_none()
        );
    }

    // ─── session_sse_event / build_initialize_sse_response ──────────────

    #[tokio::test]
    async fn session_sse_event_assigns_id_and_serializes() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let session = mgr.get_session(&id).unwrap();
        let (event_id, json_str) = session_sse_event(&session, &json!({"k": "v"}));
        assert!(event_id >= 1);
        assert_eq!(json_str, "{\"k\":\"v\"}");
    }

    #[tokio::test]
    async fn build_initialize_sse_response_uses_session_when_present() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let resp = build_initialize_sse_response(&mgr, &id, &json!({"hello": "world"}));
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(content_type(&resp).contains("text/event-stream"));
        let body = body_string(resp).await;
        assert!(body.contains("hello"), "body was: {body}");
    }

    #[tokio::test]
    async fn build_initialize_sse_response_falls_back_for_unknown_session() {
        let mgr = keepalive_manager();
        let resp = build_initialize_sse_response(&mgr, "missing", &json!({"fallback": 1}));
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("fallback"), "body was: {body}");
        // Fallback id is 1.
        assert!(
            body.contains("id: 1") || body.contains('1'),
            "body was: {body}"
        );
    }

    // ─── build_interleaved_sse_stream ───────────────────────────────────

    #[tokio::test]
    async fn interleaved_sse_stream_emits_notifications_and_response() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let session = mgr.get_session(&id).unwrap();
        let rx = session.subscribe();
        // Buffer a notification into the broadcast channel before building.
        session
            .broadcast("{\"note\":\"interleaved\"}".to_string())
            .expect("broadcast ok");

        let stream =
            build_interleaved_sse_stream(session, id.clone(), rx, json!({"resp": "final"}));
        let resp = Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response();
        let body = body_string(resp).await;
        assert!(body.contains("interleaved"), "body was: {body}");
        assert!(body.contains("final"), "body was: {body}");
    }

    // ─── check_sse_request_gating ───────────────────────────────────────

    #[tokio::test]
    async fn sse_gating_returns_403_for_unknown_session() {
        let mgr = keepalive_manager();
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let resp = check_sse_request_gating(&mgr, "missing", Some("tools/list"), &payload, false)
            .await
            .expect("should gate");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn sse_gating_handles_client_response() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let payload = json!({"jsonrpc": "2.0", "id": "abc", "result": {}});
        let resp = check_sse_request_gating(&mgr, &id, None, &payload, false)
            .await
            .expect("client response is handled in gating");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn sse_gating_blocks_tools_call_before_lock() {
        // HARD INVARIANT mirror for the SSE path.
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {}});
        let resp = check_sse_request_gating(&mgr, &id, Some("tools/call"), &payload, false)
            .await
            .expect("tools/call gated");
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(body_json(resp).await["error"]["code"], -32001);
    }

    #[tokio::test]
    async fn sse_gating_returns_none_when_initialized_and_allowed() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        mgr.get_session(&id)
            .unwrap()
            .mark_mcp_initialized()
            .await
            .unwrap();
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        assert!(
            check_sse_request_gating(&mgr, &id, Some("ping"), &payload, false)
                .await
                .is_none()
        );
    }

    // ─── forward_request_sse ────────────────────────────────────────────

    #[tokio::test]
    async fn forward_request_sse_session_not_found_is_403() {
        let mgr = keepalive_manager();
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let resp = forward_request_sse(&mgr, "missing", Some("tools/list"), &payload, false).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn forward_request_sse_ok_returns_sse_stream() {
        // A no-id payload returns immediately from send_request, exercising the
        // Ok branch and the interleaved stream construction.
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        let payload = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let resp =
            forward_request_sse(&mgr, &id, Some("notifications/initialized"), &payload, true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(content_type(&resp).contains("text/event-stream"));
        assert_eq!(header_session_id(&resp).as_deref(), Some(id.as_str()));
    }

    #[tokio::test]
    async fn forward_request_sse_errors_when_session_terminated() {
        let mgr = keepalive_manager();
        let id = mgr.create_session().await.expect("create session");
        mgr.get_session(&id).unwrap().set_terminated(true);
        let payload = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let resp =
            forward_request_sse(&mgr, &id, Some("notifications/initialized"), &payload, true).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_json(resp).await["error"]["code"], -32603);
    }
}
