//! How the bridge routes each shape of message a client can POST to `/mcp`,
//! pinned on the wire: the HTTP status, the body, and whether the message
//! reached the session's subprocess.
//!
//! The bridge classifies every POSTed message exactly once
//! (`Inbound`, `src/inbound.rs`): a request (has an `id`) is
//! forwarded and answered, a notification (no `id`) is forwarded and answered
//! `202` with no body, and a client response (an `id`, no string `method`, and
//! a `result` or `error`) is delivered to the server request waiting for it.
//! The edge cases below — a `null` id, a message that has both a `method` and
//! a `result`, a payload that is not an object, a batch array, bytes that are
//! not JSON — record what the bridge does with each today, so a change to any
//! of them is a deliberate one.
//!
//! In-process: the bridge runs on a loopback port with an injected peer that
//! records every line it is sent, so nothing is spawned. Every case runs on
//! both POST transports (JSON and SSE), per R15.5.

use crate::common;

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use common::{TransportMode, make_h2_client, spawn_in_process_server};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
const SENTINEL_METHOD: &str = "notifications/test/sentinel";

/// A peer that records every message the bridge writes to it, and answers
/// every one that expects an answer: anything with an `id`, except a message
/// that is itself a response (no `method`, and a `result` or `error`).
struct RecordingPeerFactory {
    received: Arc<Mutex<Vec<Value>>>,
}

impl ahma_common::peer_factory::PeerFactory for RecordingPeerFactory {
    fn create(
        &self,
        _options: ahma_common::peer_factory::PeerSpawnOptions,
    ) -> ahma_common::peer_factory::BoxFuture<anyhow::Result<ahma_common::peer_factory::PeerStreams>>
    {
        let received = Arc::clone(&self.received);
        Box::pin(async move {
            let (bridge_end, peer_end) = tokio::io::duplex(64 * 1024);
            let (peer_read, mut peer_write) = tokio::io::split(peer_end);
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                let mut lines = BufReader::new(peer_read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    received.lock().unwrap().push(msg.clone());
                    let Some(id) = msg.get("id") else { continue };
                    let is_response = msg.get("method").is_none()
                        && (msg.get("result").is_some() || msg.get("error").is_some());
                    if is_response {
                        continue;
                    }
                    let method = msg.get("method").cloned().unwrap_or(Value::Null);
                    let result = if method == json!("initialize") {
                        json!({
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "serverInfo": {"name": "recording-peer", "version": "1.0"}
                        })
                    } else {
                        json!({"echo": method})
                    };
                    let mut reply =
                        json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
                    reply.push('\n');
                    if peer_write.write_all(reply.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
            let (bridge_read, bridge_write) = tokio::io::split(bridge_end);
            Ok(ahma_common::peer_factory::PeerStreams {
                stdin: Box::new(bridge_write),
                stdout: Box::new(bridge_read),
                stderr: None,
                shutdown_fn: None,
                exit_cause: None,
            })
        })
    }
}

/// What the client saw for one POST.
#[derive(Debug)]
struct Reply {
    status: u16,
    content_type: String,
    session_header: Option<String>,
    body: String,
}

impl Reply {
    /// The JSON-RPC message the reply carries: the body itself on the JSON
    /// transport, the last `data:` event on the SSE one.
    fn message(&self) -> Value {
        if self.content_type.contains("text/event-stream") {
            let data = self
                .body
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(str::trim)
                .next_back()
                .unwrap_or_else(|| panic!("SSE reply carried no data event: {self:?}"));
            serde_json::from_str(data).unwrap_or_else(|e| panic!("bad SSE data {data}: {e}"))
        } else {
            serde_json::from_str(&self.body)
                .unwrap_or_else(|e| panic!("reply body is not JSON ({e}): {self:?}"))
        }
    }
}

/// A bridge, an initialized session on it, and the record of what its peer
/// was sent.
struct Harness {
    _server: common::InProcessServerInstance,
    client: reqwest::Client,
    url: String,
    mode: TransportMode,
    session_id: String,
    received: Arc<Mutex<Vec<Value>>>,
    sentinels: AtomicU64,
}

fn accept_header(mode: TransportMode) -> &'static str {
    match mode {
        TransportMode::Json => "application/json",
        TransportMode::Sse => "application/json, text/event-stream",
    }
}

async fn post_raw(
    client: &reqwest::Client,
    url: &str,
    mode: TransportMode,
    session_id: Option<&str>,
    body: Vec<u8>,
) -> Reply {
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", accept_header(mode))
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .body(body);
    if let Some(id) = session_id {
        req = req.header(MCP_SESSION_ID_HEADER, id);
    }
    let resp = req.send().await.expect("POST /mcp");
    let status = resp.status().as_u16();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };
    let content_type = header("content-type").unwrap_or_default();
    let session_header = header(MCP_SESSION_ID_HEADER);
    let body = resp.text().await.expect("read reply body");
    Reply {
        status,
        content_type,
        session_header,
        body,
    }
}

impl Harness {
    /// Start a bridge and take one session through `initialize` and
    /// `notifications/initialized`, so a request is forwarded at once.
    async fn start(mode: TransportMode) -> Self {
        let received = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(RecordingPeerFactory {
            received: Arc::clone(&received),
        });
        let server = spawn_in_process_server(factory)
            .await
            .expect("spawn in-process bridge");
        let url = format!("{}/mcp", server.base_url());
        let client = make_h2_client();

        let init = post_raw(
            &client,
            &url,
            mode,
            None,
            initialize_payload(json!("init")).to_string().into_bytes(),
        )
        .await;
        assert_eq!(init.status, 200, "initialize failed: {init:?}");
        let session_id = init
            .session_header
            .clone()
            .expect("initialize returns a session id");

        let harness = Self {
            _server: server,
            client,
            url,
            mode,
            session_id,
            received,
            sentinels: AtomicU64::new(0),
        };
        let initialized = harness
            .post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        assert_eq!(initialized.status, 202, "initialized: {initialized:?}");
        harness
    }

    async fn post(&self, payload: Value) -> Reply {
        self.post_bytes(payload.to_string().into_bytes()).await
    }

    async fn post_bytes(&self, body: Vec<u8>) -> Reply {
        post_raw(
            &self.client,
            &self.url,
            self.mode,
            Some(&self.session_id),
            body,
        )
        .await
    }

    /// Whether `payload` reached the subprocess.
    ///
    /// Answered without sleeping: a sentinel notification is posted after it,
    /// and the session writes to its subprocess in order, so once the sentinel
    /// has arrived everything posted before it has too. Each sentinel is
    /// numbered, so a check never stops at an earlier check's sentinel.
    async fn subprocess_received(&self, payload: &Value) -> bool {
        let seq = self.sentinels.fetch_add(1, Ordering::Relaxed);
        let sentinel = json!({"jsonrpc": "2.0", "method": SENTINEL_METHOD, "params": {"seq": seq}});
        let reply = self.post(sentinel.clone()).await;
        assert_eq!(reply.status, 202, "sentinel: {reply:?}");
        let deadline = tokio::time::Instant::now() + TestTimeouts::get(TimeoutCategory::Quick);
        loop {
            {
                let received = self.received.lock().unwrap();
                if let Some(at) = received.iter().position(|m| *m == sentinel) {
                    return received[..at].contains(payload);
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the sentinel never reached the subprocess"
            );
            tokio::time::sleep(TestTimeouts::poll_interval()).await;
        }
    }
}

fn initialize_payload(id: Value) -> Value {
    let mut payload = json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {"roots": {"listChanged": true}},
            "clientInfo": {"name": "inbound-routing-test", "version": "1.0"}
        }
    });
    if !id.is_null() {
        payload["id"] = id;
    }
    payload
}

/// A request is forwarded and its answer returned with the id exactly as the
/// client wrote it — a number stays a number, a string stays a string.
async fn assert_request_answered(h: &Harness, payload: Value, method: Value) {
    let id = payload["id"].clone();
    let reply = h.post(payload.clone()).await;
    assert_eq!(reply.status, 200, "{reply:?}");
    assert_eq!(
        reply.session_header.as_deref(),
        Some(h.session_id.as_str()),
        "{reply:?}"
    );
    let expected_type = match h.mode {
        TransportMode::Json => "application/json",
        TransportMode::Sse => "text/event-stream",
    };
    assert!(reply.content_type.contains(expected_type), "{reply:?}");
    assert_eq!(
        reply.message(),
        json!({"jsonrpc": "2.0", "id": id, "result": {"echo": method}})
    );
    assert!(h.subprocess_received(&payload).await, "not forwarded");
}

/// A notification is forwarded and acknowledged `202` with no body.
async fn assert_notification_accepted(h: &Harness, payload: Value) {
    let reply = h.post(payload.clone()).await;
    assert_eq!(reply.status, 202, "{reply:?}");
    assert_eq!(reply.body, "", "a notification gets no body: {reply:?}");
    assert_eq!(
        reply.session_header.as_deref(),
        Some(h.session_id.as_str()),
        "{reply:?}"
    );
    assert!(h.subprocess_received(&payload).await, "not forwarded");
}

/// A client response is forwarded to the subprocess and acknowledged `202`
/// with the JSON body `{}`.
async fn assert_client_response_accepted(h: &Harness, payload: Value) {
    let reply = h.post(payload.clone()).await;
    assert_eq!(reply.status, 202, "{reply:?}");
    assert!(reply.content_type.contains("application/json"), "{reply:?}");
    assert_eq!(reply.body, "{}", "{reply:?}");
    assert_eq!(
        reply.session_header.as_deref(),
        Some(h.session_id.as_str()),
        "{reply:?}"
    );
    assert!(h.subprocess_received(&payload).await, "not forwarded");
}

async fn run_numeric_and_string_ids_are_echoed_as_sent(mode: TransportMode) {
    let h = Harness::start(mode).await;
    assert_request_answered(
        &h,
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list"}),
        json!("tools/list"),
    )
    .await;
    // `"7"` is a different id from `7` and must come back as a string.
    assert_request_answered(
        &h,
        json!({"jsonrpc": "2.0", "id": "7", "method": "tools/list"}),
        json!("tools/list"),
    )
    .await;
}

#[tokio::test]
async fn numeric_and_string_ids_are_echoed_as_sent_json() {
    run_numeric_and_string_ids_are_echoed_as_sent(TransportMode::Json).await;
}

#[tokio::test]
async fn numeric_and_string_ids_are_echoed_as_sent_sse() {
    run_numeric_and_string_ids_are_echoed_as_sent(TransportMode::Sse).await;
}

async fn run_notification_is_202_with_no_body(mode: TransportMode) {
    let h = Harness::start(mode).await;
    assert_notification_accepted(
        &h,
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}}),
    )
    .await;
}

#[tokio::test]
async fn notification_is_202_with_no_body_json() {
    run_notification_is_202_with_no_body(TransportMode::Json).await;
}

#[tokio::test]
async fn notification_is_202_with_no_body_sse() {
    run_notification_is_202_with_no_body(TransportMode::Sse).await;
}

/// The client's answer to a server → client request (`roots/list`,
/// elicitation, …) is a response, not a request: it is never answered, only
/// passed on. An `error` answer is a response too, and so is one whose
/// `method` is not a string (it counts as absent).
async fn run_client_responses_are_forwarded_and_acknowledged(mode: TransportMode) {
    let h = Harness::start(mode).await;
    assert_client_response_accepted(
        &h,
        json!({"jsonrpc": "2.0", "id": 0, "result": {"action": "accept", "content": {}}}),
    )
    .await;
    assert_client_response_accepted(
        &h,
        json!({"jsonrpc": "2.0", "id": "elicit-1", "error": {"code": -1, "message": "declined"}}),
    )
    .await;
    assert_client_response_accepted(
        &h,
        json!({"jsonrpc": "2.0", "id": 3, "method": 7, "result": {}}),
    )
    .await;
}

#[tokio::test]
async fn client_responses_are_forwarded_and_acknowledged_json() {
    run_client_responses_are_forwarded_and_acknowledged(TransportMode::Json).await;
}

#[tokio::test]
async fn client_responses_are_forwarded_and_acknowledged_sse() {
    run_client_responses_are_forwarded_and_acknowledged(TransportMode::Sse).await;
}

/// A response with a `null` id matches no pending request (none is ever
/// registered under `null`), so it is simply passed on.
async fn run_client_response_with_null_id_is_forwarded(mode: TransportMode) {
    let h = Harness::start(mode).await;
    assert_client_response_accepted(&h, json!({"jsonrpc": "2.0", "id": null, "result": {}})).await;
}

#[tokio::test]
async fn client_response_with_null_id_is_forwarded_json() {
    run_client_response_with_null_id_is_forwarded(TransportMode::Json).await;
}

#[tokio::test]
async fn client_response_with_null_id_is_forwarded_sse() {
    run_client_response_with_null_id_is_forwarded(TransportMode::Sse).await;
}

/// A `method` wins over a `result`: with an id it is a request, without one a
/// notification.
async fn run_method_with_result_is_a_request_or_notification(mode: TransportMode) {
    let h = Harness::start(mode).await;
    assert_request_answered(
        &h,
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list", "result": {}}),
        json!("tools/list"),
    )
    .await;
    assert_notification_accepted(
        &h,
        json!({"jsonrpc": "2.0", "method": "notifications/progress", "result": {}}),
    )
    .await;
}

#[tokio::test]
async fn method_with_result_is_a_request_or_notification_json() {
    run_method_with_result_is_a_request_or_notification(TransportMode::Json).await;
}

#[tokio::test]
async fn method_with_result_is_a_request_or_notification_sse() {
    run_method_with_result_is_a_request_or_notification(TransportMode::Sse).await;
}

/// Messages that are neither a well-formed request nor a response are routed
/// by their `id` alone, exactly like a request or a notification:
/// * an `id` with no `method`, `result` or `error` is forwarded as a request
///   and the subprocess's answer returned;
/// * a `result` with no `id` is forwarded as a notification, `202`;
/// * a payload that is not an object has no `id`, so it is forwarded as a
///   notification too.
async fn run_shapeless_messages_route_by_id(mode: TransportMode) {
    let h = Harness::start(mode).await;
    assert_request_answered(&h, json!({"jsonrpc": "2.0", "id": 9}), Value::Null).await;
    assert_notification_accepted(&h, json!({"jsonrpc": "2.0", "result": {}})).await;
    assert_notification_accepted(&h, json!(42)).await;
    assert_notification_accepted(&h, json!("hello")).await;
}

#[tokio::test]
async fn shapeless_messages_route_by_id_json() {
    run_shapeless_messages_route_by_id(TransportMode::Json).await;
}

#[tokio::test]
async fn shapeless_messages_route_by_id_sse() {
    run_shapeless_messages_route_by_id(TransportMode::Sse).await;
}

/// A request whose `id` is `null` is forwarded, but the bridge registers no
/// pending entry for it (a `null` id keys as a notification at the session
/// layer), so it does not wait for the subprocess: the client gets HTTP 200
/// carrying `{"jsonrpc":"2.0","result":null}` — no `id` at all.
async fn run_request_with_null_id_gets_an_idless_null_result(mode: TransportMode) {
    let h = Harness::start(mode).await;
    let payload = json!({"jsonrpc": "2.0", "id": null, "method": "tools/list"});
    let reply = h.post(payload.clone()).await;
    assert_eq!(reply.status, 200, "{reply:?}");
    assert_eq!(
        reply.session_header.as_deref(),
        Some(h.session_id.as_str()),
        "{reply:?}"
    );
    assert_eq!(reply.message(), json!({"jsonrpc": "2.0", "result": null}));
    assert!(h.subprocess_received(&payload).await, "not forwarded");
}

#[tokio::test]
async fn request_with_null_id_gets_an_idless_null_result_json() {
    run_request_with_null_id_gets_an_idless_null_result(TransportMode::Json).await;
}

#[tokio::test]
async fn request_with_null_id_gets_an_idless_null_result_sse() {
    run_request_with_null_id_gets_an_idless_null_result(TransportMode::Sse).await;
}

/// A JSON-RPC batch array is refused before any routing: `400`, `-32600`,
/// `id: null`, and nothing reaches the subprocess.
async fn run_batch_array_is_refused(mode: TransportMode) {
    let h = Harness::start(mode).await;
    let payload = json!([{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}]);
    let reply = h.post(payload.clone()).await;
    assert_eq!(reply.status, 400, "{reply:?}");
    assert!(reply.content_type.contains("application/json"), "{reply:?}");
    let body: Value = serde_json::from_str(&reply.body).expect("JSON body");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], Value::Null);
    assert_eq!(body["error"]["code"], -32600);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("batch requests are not supported"),
        "{body}"
    );
    assert!(
        !h.subprocess_received(&payload).await,
        "batch was forwarded"
    );
    assert!(
        !h.subprocess_received(&payload[0]).await,
        "batch element was forwarded"
    );
}

#[tokio::test]
async fn batch_array_is_refused_json() {
    run_batch_array_is_refused(TransportMode::Json).await;
}

#[tokio::test]
async fn batch_array_is_refused_sse() {
    run_batch_array_is_refused(TransportMode::Sse).await;
}

/// Bytes that are not JSON never reach the bridge's routing: the HTTP layer
/// refuses them with `400` and a plain-text explanation, not a JSON-RPC
/// error.
async fn run_malformed_json_is_refused_by_the_http_layer(mode: TransportMode) {
    let h = Harness::start(mode).await;
    let reply = h
        .post_bytes(br#"{"jsonrpc": "2.0", "id": 1, "method": "#.to_vec())
        .await;
    assert_eq!(reply.status, 400, "{reply:?}");
    assert!(reply.content_type.contains("text/plain"), "{reply:?}");
    assert!(
        reply
            .body
            .starts_with("Failed to parse the request body as JSON"),
        "{reply:?}"
    );
}

#[tokio::test]
async fn malformed_json_is_refused_by_the_http_layer_json() {
    run_malformed_json_is_refused_by_the_http_layer(TransportMode::Json).await;
}

#[tokio::test]
async fn malformed_json_is_refused_by_the_http_layer_sse() {
    run_malformed_json_is_refused_by_the_http_layer(TransportMode::Sse).await;
}

/// Without a session header, everything but `initialize` (and the two
/// session-less probes) is refused `400` / `-32600`, echoing the message's
/// id — a client response included.
async fn run_messages_without_a_session_are_refused_with_their_id(mode: TransportMode) {
    let h = Harness::start(mode).await;
    for (payload, id) in [
        (json!({"jsonrpc": "2.0", "id": 4, "result": {}}), json!(4)),
        (
            json!({"jsonrpc": "2.0", "id": "r", "method": "tools/list"}),
            json!("r"),
        ),
        (
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled"}),
            Value::Null,
        ),
    ] {
        let reply = post_raw(
            &h.client,
            &h.url,
            mode,
            None,
            payload.to_string().into_bytes(),
        )
        .await;
        assert_eq!(reply.status, 400, "{payload}: {reply:?}");
        let body: Value = serde_json::from_str(&reply.body).expect("JSON body");
        assert_eq!(body["id"], id, "{payload}: {body}");
        assert_eq!(body["error"]["code"], -32600, "{payload}: {body}");
    }
}

#[tokio::test]
async fn messages_without_a_session_are_refused_with_their_id_json() {
    run_messages_without_a_session_are_refused_with_their_id(TransportMode::Json).await;
}

#[tokio::test]
async fn messages_without_a_session_are_refused_with_their_id_sse() {
    run_messages_without_a_session_are_refused_with_their_id(TransportMode::Sse).await;
}

/// `initialize` is routed by its method alone: sent as a notification (no
/// `id`) it still creates a session, and — as for any id-less message the
/// session forwards — the bridge does not wait for the subprocess and answers
/// HTTP 200 with `{"jsonrpc":"2.0","result":null}`.
async fn run_initialize_without_id_creates_a_session(mode: TransportMode) {
    let h = Harness::start(mode).await;
    let reply = post_raw(
        &h.client,
        &h.url,
        mode,
        None,
        initialize_payload(Value::Null).to_string().into_bytes(),
    )
    .await;
    assert_eq!(reply.status, 200, "{reply:?}");
    let session = reply.session_header.clone().expect("a session was created");
    assert_ne!(session, h.session_id);
    assert_eq!(reply.message(), json!({"jsonrpc": "2.0", "result": null}));
}

#[tokio::test]
async fn initialize_without_id_creates_a_session_json() {
    run_initialize_without_id_creates_a_session(TransportMode::Json).await;
}

#[tokio::test]
async fn initialize_without_id_creates_a_session_sse() {
    run_initialize_without_id_creates_a_session(TransportMode::Sse).await;
}
