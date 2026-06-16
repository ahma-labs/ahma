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
    if let Err(e) = session_manager.handle_roots_changed(session_id).await {
        error!(session_id = %session_id, "Roots change rejected: {}", e);
        Some(error_response_with_status(
            StatusCode::FORBIDDEN,
            -32600,
            "Session terminated: roots change not allowed",
        ))
    } else {
        None
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

    if let Some(id_val) = response_id {
        let id_str = id_val
            .as_str()
            .map_or_else(|| id_val.to_string(), str::to_string);
        if let Some(session) = session_manager.get_session(session_id)
            && let Some((_, sender)) = session.routed_requests.remove(&id_str)
        {
            let _ = sender.send(payload.clone());
            return with_session_header(
                json_response_with_status(StatusCode::ACCEPTED, serde_json::json!({})),
                session_id,
            );
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
    if let Some(result) = payload.get("result") {
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

    if !should_lock_sandbox(&mcp_roots) {
        debug!(
            session_id = %session_id,
            "Skipping sandbox lock: roots list is empty (client has no workspace folder open)"
        );
        return;
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
        Err(e) => {
            error!(session_id = %session_id, "Failed to send request: {}", e);
            error_response(-32603, &format!("Failed to send request: {}", e))
        }
    }
}

fn calculate_tool_timeout(payload: &Value) -> Duration {
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
