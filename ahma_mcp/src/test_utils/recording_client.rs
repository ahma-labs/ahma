//! An MCP client that records what the server pushed to it.
//!
//! Everything ahma sends *to* a client — `notifications/progress` above all —
//! was previously only observable from the inside: unit tests asserted on the
//! router's bookkeeping, and the wire itself was never checked. That gap is not
//! academic. Progress used to be emitted under the token of the `tools/call`
//! that *started* an operation, so an `await` blocking on it produced a stream
//! of notifications addressed to a request the client had already retired —
//! bookkeeping that looks perfectly correct from the server's side while the
//! client sees silence and eventually drops the transport.
//!
//! [`RecordingClient`](crate::test_utils::recording_client::RecordingClient) is a real [`ClientHandler`](rmcp::handler::client::ClientHandler), so it exercises the same
//! serialization, dispatch, and peer plumbing a production client does. Pair it
//! with [`crate::test_utils::in_process`] to assert on delivered notifications
//! without spawning a subprocess.

use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ErrorData as McpError;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientInfo, CustomNotification, ElicitRequestParams, ElicitResult,
    ElicitationAction, ElicitationCapability, FormElicitationCapability, Implementation,
    ListRootsResult, ProgressNotificationParam, ProgressToken, ProtocolVersion, Root,
};
use rmcp::service::{MaybeSendFuture, NotificationContext, RequestContext, RoleClient};

/// Shared, cloneable record of the progress notifications a client received.
///
/// Cheap to clone — hand one clone to [`RecordingClient`] and keep another in
/// the test to assert on.
#[derive(Clone, Default)]
pub struct ProgressLog {
    inner: Arc<LogInner>,
}

#[derive(Default)]
struct LogInner {
    seen: Mutex<Vec<ProgressNotificationParam>>,
    arrived: tokio::sync::Notify,
}

impl ProgressLog {
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, params: ProgressNotificationParam) {
        self.inner.seen.lock().push(params);
        self.inner.arrived.notify_waiters();
    }

    /// Every notification received so far, in arrival order.
    pub fn all(&self) -> Vec<ProgressNotificationParam> {
        self.inner.seen.lock().clone()
    }

    /// Notifications addressed to one progress token.
    ///
    /// This is the assertion that matters for SPEC R2.5.3: *which request* a
    /// notification was addressed to, not merely that one was sent.
    pub fn for_token(&self, token: &ProgressToken) -> Vec<ProgressNotificationParam> {
        self.all()
            .into_iter()
            .filter(|p| &p.progress_token == token)
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.seen.lock().is_empty()
    }

    /// Wait until a notification satisfying `predicate` arrives.
    ///
    /// Returns `Err` with the notifications seen so far if `budget` elapses
    /// first, so a failing assertion says what *did* arrive rather than only
    /// what didn't.
    pub async fn wait_for(
        &self,
        budget: Duration,
        predicate: impl Fn(&ProgressNotificationParam) -> bool,
    ) -> Result<ProgressNotificationParam, Vec<ProgressNotificationParam>> {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            // Register interest *before* scanning: a notification that lands
            // between the scan and the await would otherwise be a lost wakeup
            // and the test would hang until the budget expired.
            let arrived = self.inner.arrived.notified();
            if let Some(hit) = self.all().into_iter().find(|p| predicate(p)) {
                return Ok(hit);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, arrived).await.is_err() {
                // One last scan: the notification may have landed inside the
                // timeout's own teardown.
                return match self.all().into_iter().find(|p| predicate(p)) {
                    Some(hit) => Ok(hit),
                    None => Err(self.all()),
                };
            }
        }
    }
}

/// A `ClientHandler` that records progress notifications and answers
/// `roots/list` with a configured set of roots.
///
/// The client *name* is deliberately configurable: ahma keys real behaviour off
/// `clientInfo.name` — whether progress is pushed at all
/// ([`crate::client_type::McpClientType::supports_progress`]) and how long one
/// request may be held open
/// ([`crate::client_type::McpClientType::request_budget`]) — so a harness that
/// could only ever be one client could not test either rule.
#[derive(Clone)]
pub struct RecordingClient {
    progress: ProgressLog,
    client_name: String,
    roots: Vec<Root>,
    elicitation: Option<ElicitationLog>,
    notifications: NotificationLog,
}

/// The methods of the custom notifications (`notifications/sandbox/configured`,
/// …) a [`RecordingClient`] received, in arrival order. Cloneable.
#[derive(Clone, Debug, Default)]
pub struct NotificationLog {
    methods: Arc<Mutex<Vec<String>>>,
}

impl NotificationLog {
    /// Every custom notification method received so far, in order.
    pub fn methods(&self) -> Vec<String> {
        self.methods.lock().clone()
    }
}

/// What a [`RecordingClient`] was asked through `elicitation/create`, and how it
/// answers. Cloneable: keep one handle in the test to assert on.
#[derive(Clone, Debug)]
pub struct ElicitationLog {
    messages: Arc<Mutex<Vec<String>>>,
    /// The form value the client sends back (`"deny"`, `"read-write-session"`,
    /// …), or `None` to decline outright.
    answer: Option<String>,
    /// Answer with the MCP `cancel` action instead — what a client sends when
    /// it gives up on its own, with no human choice (SPEC R5.3.1).
    cancel: bool,
}

impl ElicitationLog {
    /// Every message the server put to the human, in order.
    pub fn messages(&self) -> Vec<String> {
        self.messages.lock().clone()
    }
}

impl RecordingClient {
    /// Build a client that identifies itself as `client_name`.
    ///
    /// Use a name ahma recognises (`"cursor"`, `"antigravity"`, `"claude-ai"`,
    /// …) to exercise that client's behaviour; anything else maps to
    /// [`crate::client_type::McpClientType::Unknown`].
    pub fn new(client_name: impl Into<String>) -> Self {
        Self {
            progress: ProgressLog::new(),
            client_name: client_name.into(),
            roots: Vec::new(),
            elicitation: None,
            notifications: NotificationLog::default(),
        }
    }

    /// Advertise form elicitation at `initialize` and answer every
    /// `elicitation/create` with `answer` (a grant form value such as `"deny"`
    /// or `"read-write-session"`; `None` declines). Every message is recorded,
    /// so a test can assert on exactly what the human was shown.
    pub fn with_elicitation(mut self, answer: Option<&str>) -> Self {
        self.elicitation = Some(ElicitationLog {
            messages: Arc::new(Mutex::new(Vec::new())),
            answer: answer.map(str::to_string),
            cancel: false,
        });
        self
    }

    /// Advertise form elicitation and answer every `elicitation/create` with
    /// the MCP `cancel` action: dismissed with no choice made, as a client does
    /// on its own deadline. Not a decline — the server must not read it as one
    /// (SPEC R5.3.1). Messages are recorded as with [`Self::with_elicitation`].
    pub fn with_elicitation_cancel(mut self) -> Self {
        self.elicitation = Some(ElicitationLog {
            messages: Arc::new(Mutex::new(Vec::new())),
            answer: None,
            cancel: true,
        });
        self
    }

    /// A handle to the custom notifications this client receives
    /// (`notifications/sandbox/configured` above all).
    pub fn notifications(&self) -> NotificationLog {
        self.notifications.clone()
    }

    /// A handle to the elicitation prompts this client was shown, if
    /// [`Self::with_elicitation`] enabled them.
    pub fn elicitation(&self) -> Option<ElicitationLog> {
        self.elicitation.clone()
    }

    /// Answer `roots/list` with these roots instead of an empty list.
    pub fn with_roots(mut self, roots: Vec<Root>) -> Self {
        self.roots = roots;
        self
    }

    /// A handle to the notifications this client receives.
    pub fn progress(&self) -> ProgressLog {
        self.progress.clone()
    }
}

#[allow(deprecated)] // `roots` / `ListRootsResult`: deprecated by SEP-2577, still how ahma scopes.
impl ClientHandler for RecordingClient {
    fn get_info(&self) -> ClientInfo {
        // These rmcp models are `#[non_exhaustive]`, so build from `Default`
        // and assign rather than using struct literals.
        let mut implementation = Implementation::default();
        implementation.name = self.client_name.clone();
        implementation.version = env!("CARGO_PKG_VERSION").to_string();

        let mut capabilities = ClientCapabilities::default();
        capabilities.roots = Some(Default::default());
        if self.elicitation.is_some() {
            let mut elicitation = ElicitationCapability::default();
            elicitation.form = Some(FormElicitationCapability::default());
            capabilities.elicitation = Some(elicitation);
        }

        let mut info = ClientInfo::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = capabilities;
        info.client_info = implementation;
        info
    }

    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        self.progress.record(params);
        std::future::ready(())
    }

    fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<ListRootsResult, McpError>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(ListRootsResult::new(self.roots.clone())))
    }

    fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<ElicitResult, McpError>> + MaybeSendFuture + '_ {
        let result = match &self.elicitation {
            None => ElicitResult::new(ElicitationAction::Decline),
            Some(log) => {
                if let ElicitRequestParams::FormElicitationParams { message, .. } = &request {
                    log.messages.lock().push(message.clone());
                }
                if log.cancel {
                    ElicitResult::new(ElicitationAction::Cancel)
                } else {
                    match &log.answer {
                        Some(value) => ElicitResult::new(ElicitationAction::Accept)
                            .with_content(serde_json::json!({ "decision": value })),
                        None => ElicitResult::new(ElicitationAction::Decline),
                    }
                }
            }
        };
        std::future::ready(Ok(result))
    }

    fn on_custom_notification(
        &self,
        notification: CustomNotification,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        self.notifications.methods.lock().push(notification.method);
        std::future::ready(())
    }
}

/// Call a tool and learn the progress token the request actually carried.
///
/// An rmcp client **overwrites** whatever `_meta.progressToken` a caller sets
/// with one from its own per-peer counter (`Peer::send_request_with_option`),
/// so a test cannot choose a token — it can only observe the one assigned. That
/// is faithful to production, where the token is the client's to mint, and it
/// is the reason this helper exists: asserting "the notification went to *this*
/// request" (SPEC R2.5.3) requires knowing which token this request got.
pub async fn call_tool_observing_token(
    peer: &rmcp::service::Peer<RoleClient>,
    params: rmcp::model::CallToolRequestParams,
) -> Result<(ProgressToken, rmcp::model::CallToolResult), rmcp::service::ServiceError> {
    use rmcp::model::{CallToolRequest, ClientRequest, ServerResult};
    use rmcp::service::{PeerRequestOptions, ServiceError};

    let handle = peer
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            PeerRequestOptions::no_options(),
        )
        .await?;
    let token = handle.progress_token.clone();
    match handle.await_response().await? {
        ServerResult::CallToolResult(result) => Ok((token, result)),
        _ => Err(ServiceError::UnexpectedResponse),
    }
}
