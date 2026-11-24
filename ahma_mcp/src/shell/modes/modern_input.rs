//! The stdio proxy's side of a 2026-07-28 session (SPEC R-HUB.4.2, R-HUB.4.3).
//!
//! That protocol forbids a server writing requests to a stdio client. A server
//! that needs something from the client answers one of the client's own
//! requests with an `InputRequiredResult`, and the client retries that request
//! with the answers attached. ahma's worker still asks the legacy way, on the
//! server→client channel, so the proxy translates:
//!
//! * no server request ever reaches stdout ([`ModernSession::on_server_request`]);
//! * the client's roots are fetched by answering its first scoped `tools/call`
//!   with `input_required` ([`ModernSession::on_tools_call`]), and the retry
//!   that carries them is the call that runs;
//! * the worker's `roots/list` is answered from those roots, and the call is
//!   held until it has been ([`ModernSession::flush`]), so it never meets a
//!   sandbox that is still waiting for them.
//!
//! The bridge is told not to ask at handshake time by
//! [`ROOTS_ON_FIRST_CALL_CAPABILITY`] on the synthesized `initialize`.
//!
//! Everything here is a pure function of the messages; the proxy loop does the
//! I/O.

use ahma_common::mcp_methods::{
    ROOTS_LIST_CHANGED_METHOD, ROOTS_LIST_METHOD, ROOTS_ON_FIRST_CALL_CAPABILITY, RpcErrorCode,
    is_scope_exempt_tool,
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// The key of ahma's one input request. Server-assigned and opaque to the
/// client, which returns its answer under the same key.
pub(crate) const ROOTS_INPUT_KEY: &str = "ahma.roots";

/// How many times a call is answered with `input_required` before the proxy
/// stops asking and settles on empty roots, so a client that never answers
/// still gets the bridge's own remediation (container root, or a refusal that
/// says how to set a scope) instead of an endless loop.
pub(crate) const MAX_ROOTS_ASKS: u32 = 2;

/// How long a call waits for the bridge to ask for the roots it was just told
/// about. A bridge whose scope was set without the client (`--sandbox-scope`,
/// a default scope) never asks, and its calls must not wait forever.
pub(crate) const HELD_CALL_GRACE: Duration = Duration::from_secs(10);

const CLIENT_CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";

/// What the proxy does with a client `tools/call`.
#[derive(Debug, PartialEq)]
pub(crate) enum CallStep {
    /// Forward this call (with any input fields stripped) to the bridge now.
    Forward(Value),
    /// Answer the client with this `InputRequiredResult`; the bridge never
    /// sees the call.
    AskClient(Value),
    /// The call is held; [`ModernSession::flush`] releases it.
    Held,
}

/// Messages the proxy must send the bridge, in order: answers and
/// notifications first, then the calls they were gating.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Flush {
    pub(crate) to_bridge: Vec<Value>,
    pub(crate) calls: Vec<Value>,
}

/// One modern client's roots exchange.
#[derive(Debug, Default)]
pub(crate) struct ModernSession {
    /// Capabilities the client declared on `server/discover`, used when a
    /// request carries none of its own.
    discover_capabilities: Value,
    /// The client's `roots/list` result, once its retry brought it (or the
    /// proxy gave up asking and settled on none).
    roots: Option<Value>,
    asks: u32,
    request_state: Option<String>,
    /// Ids of bridge `roots/list` requests not answered yet.
    parked_roots_requests: Vec<Value>,
    /// The bridge has been given the roots.
    roots_delivered: bool,
    list_changed_sent: bool,
    held_calls: Vec<Value>,
    held_since: Option<Instant>,
    /// Approval prompts and the calls they ride (SPEC R-HUB.4.5).
    pub(crate) prompts: Prompts,
}

impl ModernSession {
    /// `discover_capabilities`: what the client declared on `server/discover`
    /// (the synthesized `initialize` carries them).
    pub(crate) fn new(discover_capabilities: Value) -> Self {
        Self {
            discover_capabilities,
            ..Self::default()
        }
    }

    /// Decide what happens to a client `tools/call`. A forwarded call keeps
    /// its input fields: [`Prompts::on_call`] reads and strips them next.
    pub(crate) fn on_tools_call(&mut self, call: Value, now: Instant) -> CallStep {
        if self.roots_delivered || is_exempt(&call) {
            return CallStep::Forward(call);
        }
        if self.roots.is_none() {
            match self.answered_roots(&call) {
                Some(roots) => self.roots = Some(roots),
                None if self.may_ask(&call) => return CallStep::AskClient(self.ask()),
                None => self.roots = Some(json!({ "roots": [] })),
            }
        }
        self.held_calls.push(strip_input_fields(call));
        self.held_since.get_or_insert(now);
        CallStep::Held
    }

    /// Answer a request the bridge addressed to the client. `None` means it
    /// is a `roots/list`, parked until [`Self::flush`] can answer it.
    pub(crate) fn on_server_request(&mut self, request: &Value) -> Option<Value> {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        match request.get("method").and_then(Value::as_str) {
            Some(ROOTS_LIST_METHOD) => {
                self.parked_roots_requests.push(id);
                None
            }
            // Prompts go to [`Prompts::on_elicitation`]; one that reaches
            // here anyway is dismissed, so the permission ladder moves on
            // instead of waiting out its budget.
            Some(ELICITATION_METHOD) => Some(response(id, json!({ "action": "cancel" }))),
            // The bridge's liveness probe. This proxy is the client's end of
            // the pipe; stdio EOF, not a ping, is what ends the session.
            Some("ping") => Some(response(id, json!({}))),
            Some(method) => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": RpcErrorCode::MethodNotFound,
                    "message": format!(
                        "{method} cannot reach a 2026-07-28 stdio client: servers may not \
                         write requests to its stdout"
                    ),
                },
            })),
            None => None,
        }
    }

    /// What to send the bridge now. With the roots known it answers every
    /// parked `roots/list`, or, if the bridge has not asked, tells it the roots
    /// changed so it does; held calls follow once the bridge has them.
    pub(crate) fn flush(&mut self) -> Flush {
        let Some(roots) = &self.roots else {
            return Flush::default();
        };
        let mut to_bridge: Vec<Value> = self
            .parked_roots_requests
            .drain(..)
            .map(|id| response(id, roots.clone()))
            .collect();
        if !to_bridge.is_empty() {
            self.roots_delivered = true;
        } else if !self.roots_delivered && !self.list_changed_sent {
            self.list_changed_sent = true;
            to_bridge.push(json!({ "jsonrpc": "2.0", "method": ROOTS_LIST_CHANGED_METHOD }));
        }
        let calls = if self.roots_delivered {
            self.take_held()
        } else {
            Vec::new()
        };
        for call in &calls {
            self.prompts.track(call);
        }
        Flush { to_bridge, calls }
    }

    /// When held calls stop waiting for a bridge that never asks.
    pub(crate) fn held_deadline(&self) -> Option<Instant> {
        self.held_since.map(|since| since + HELD_CALL_GRACE)
    }

    /// Release held calls unanswered by a `roots/list`: the bridge's scope
    /// did not come from the client, and the bridge decides what they get.
    pub(crate) fn release_held(&mut self) -> Vec<Value> {
        self.roots_delivered = true;
        let calls = self.take_held();
        for call in &calls {
            self.prompts.track(call);
        }
        calls
    }

    /// The roots as a cached `roots/list` answer, for the reconnect replay.
    pub(crate) fn roots_response(&self) -> Option<Value> {
        let roots = self.roots.as_ref().filter(|_| self.roots_delivered)?;
        Some(response(json!(0), roots.clone()))
    }

    fn take_held(&mut self) -> Vec<Value> {
        self.held_since = None;
        std::mem::take(&mut self.held_calls)
    }

    fn answered_roots(&self, call: &Value) -> Option<Value> {
        let params = call.get("params")?;
        let state = params.get("requestState").and_then(Value::as_str)?;
        if self.request_state.as_deref() != Some(state) {
            return None;
        }
        let answer = params.get("inputResponses")?.get(ROOTS_INPUT_KEY)?;
        answer.get("roots")?.as_array()?;
        Some(answer.clone())
    }

    /// The client may be sent an input request only for a capability it
    /// declared: on this request, or failing that on `server/discover`.
    fn may_ask(&self, call: &Value) -> bool {
        let capabilities = call
            .pointer("/params/_meta")
            .and_then(|meta| meta.get(CLIENT_CAPABILITIES_META))
            .unwrap_or(&self.discover_capabilities);
        self.asks < MAX_ROOTS_ASKS && capabilities.get("roots").is_some_and(Value::is_object)
    }

    fn ask(&mut self) -> Value {
        self.asks += 1;
        let state = format!("ahma-roots-{}", self.asks);
        self.request_state = Some(state.clone());
        json!({
            "resultType": "input_required",
            "inputRequests": { ROOTS_INPUT_KEY: { "method": ROOTS_LIST_METHOD } },
            "requestState": state,
        })
    }
}

// ── Approval prompts as input_required (SPEC R-HUB.4.5) ────────────────────

const ELICITATION_METHOD: &str = "elicitation/create";
const ELICIT_KEY_PREFIX: &str = "ahma.elicit.";

/// What the proxy does with a client `tools/call` once roots are settled.
#[derive(Debug, PartialEq)]
pub(crate) enum CallRoute {
    /// Forward this call, input fields stripped, through the normal path.
    Forward(Value),
    /// Handled here: write these to the bridge, then these to the client.
    Answer {
        to_bridge: Vec<Value>,
        to_client: Vec<Value>,
    },
}

/// An `input_required` the client has been sent and not yet answered.
#[derive(Debug)]
struct Asked {
    state: String,
    /// The bridge's `elicitation/create` request ids it carries.
    prompts: Vec<Value>,
    /// The call the bridge is still running for it, whose result the retry
    /// receives; `None` when the asked-on call never reached the bridge, so
    /// the retry is what runs.
    bridge_call: Option<Value>,
}

/// Approval prompts for a client that may only be asked through results.
///
/// A prompt the worker raises while a call runs becomes that call's
/// `input_required`: the client's retry carries the answer, the bridge (still
/// running the original) gets it, and the original's result comes back under
/// the retry's id, so the tool runs once. A prompt raised while no call runs —
/// an async command asking for network access — rides the client's next call,
/// and that call's retry is what the bridge runs.
#[derive(Debug, Default)]
pub(crate) struct Prompts {
    asks: u64,
    /// Prompts no call has carried to the client yet: (request id, params).
    parked: Vec<(Value, Value)>,
    asked: Vec<Asked>,
    /// Calls the bridge is running: (id the client awaits, id the bridge knows).
    in_flight: Vec<(Value, Value)>,
    /// Results for calls whose retry has not arrived yet, by bridge id.
    early: Vec<(Value, Value)>,
}

impl Prompts {
    /// Record a call that went to the bridge some other way (released from
    /// the roots hold), so a prompt it raises can find it.
    pub(crate) fn track(&mut self, call: &Value) {
        if let Some(id) = call.get("id").filter(|id| !id.is_null()) {
            self.in_flight.push((id.clone(), id.clone()));
        }
    }

    /// The worker raised a prompt. Returns what to write to the client: the
    /// oldest running call's `input_required`, or nothing if no call is
    /// running (it then rides the next one).
    pub(crate) fn on_elicitation(&mut self, request: &Value) -> Vec<Value> {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        if self.in_flight.is_empty() {
            self.parked.push((id, params));
            return Vec::new();
        }
        let (client_id, bridge_id) = self.in_flight.remove(0);
        let result = self.ask(vec![(id, params)], Some(bridge_id));
        vec![response(client_id, result)]
    }

    /// Route a client `tools/call`.
    pub(crate) fn on_call(&mut self, call: Value) -> CallRoute {
        let client_id = call.get("id").cloned().unwrap_or(Value::Null);
        let state = call
            .pointer("/params/requestState")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(at) = state
            .as_deref()
            .and_then(|state| self.asked.iter().position(|a| a.state == state))
        {
            let asked = self.asked.remove(at);
            let mut to_bridge = answers(&asked.prompts, &call);
            let mut to_client = Vec::new();
            match asked.bridge_call {
                Some(bridge_id) => {
                    if let Some(at) = self.early.iter().position(|(b, _)| *b == bridge_id) {
                        let (_, mut result) = self.early.remove(at);
                        result["id"] = client_id;
                        to_client.push(result);
                    } else {
                        self.in_flight.push((client_id, bridge_id));
                    }
                }
                None => {
                    self.in_flight.push((client_id.clone(), client_id));
                    to_bridge.push(strip_input_fields(call));
                }
            }
            return CallRoute::Answer {
                to_bridge,
                to_client,
            };
        }
        if !self.parked.is_empty() {
            let prompts = std::mem::take(&mut self.parked);
            let result = self.ask(prompts, None);
            return CallRoute::Answer {
                to_bridge: Vec::new(),
                to_client: vec![response(client_id, result)],
            };
        }
        let call = strip_input_fields(call);
        self.track(&call);
        CallRoute::Forward(call)
    }

    /// A response from the bridge. Returns what to write to the client: a
    /// call's result under the id the client now awaits, held back when that
    /// retry has not come yet; any other response as it is.
    pub(crate) fn on_bridge_response(&mut self, response: Value) -> Vec<Value> {
        let Some(id) = response.get("id").cloned() else {
            return vec![response];
        };
        if let Some(at) = self.in_flight.iter().position(|(_, b)| *b == id) {
            let (client_id, _) = self.in_flight.remove(at);
            let mut response = response;
            response["id"] = client_id;
            return vec![response];
        }
        if self
            .asked
            .iter()
            .any(|a| a.bridge_call.as_ref() == Some(&id))
        {
            self.early.push((id, response));
            return Vec::new();
        }
        vec![response]
    }

    /// The worker withdrew a prompt (`notifications/cancelled` from the
    /// bridge): never ask it, and ignore an answer to it.
    pub(crate) fn on_bridge_cancelled(&mut self, notification: &Value) {
        let Some(id) = notification.pointer("/params/requestId") else {
            return;
        };
        self.parked.retain(|(parked, _)| parked != id);
        for asked in &mut self.asked {
            asked.prompts.retain(|prompt| prompt != id);
        }
    }

    /// A client cancel names the id the client knows; the bridge may know
    /// that call by its original id.
    pub(crate) fn rewrite_client_cancel(&self, mut notification: Value) -> Value {
        let bridge_id = notification
            .pointer("/params/requestId")
            .and_then(|id| self.in_flight.iter().find(|(c, _)| c == id))
            .map(|(_, b)| b.clone());
        if let Some(bridge_id) = bridge_id {
            notification["params"]["requestId"] = bridge_id;
        }
        notification
    }

    fn ask(&mut self, prompts: Vec<(Value, Value)>, bridge_call: Option<Value>) -> Value {
        self.asks += 1;
        let state = format!("ahma-elicit-{}", self.asks);
        let requests: serde_json::Map<String, Value> = prompts
            .iter()
            .map(|(id, params)| {
                (
                    elicit_key(id),
                    json!({ "method": ELICITATION_METHOD, "params": params }),
                )
            })
            .collect();
        self.asked.push(Asked {
            state: state.clone(),
            prompts: prompts.into_iter().map(|(id, _)| id).collect(),
            bridge_call,
        });
        json!({
            "resultType": "input_required",
            "inputRequests": requests,
            "requestState": state,
        })
    }
}

/// The input-request key for a bridge prompt id.
fn elicit_key(id: &Value) -> String {
    match id {
        Value::String(s) => format!("{ELICIT_KEY_PREFIX}{s}"),
        other => format!("{ELICIT_KEY_PREFIX}{other}"),
    }
}

/// The bridge's answers to `prompts`, from a retry's `inputResponses`. A
/// prompt the retry left unanswered is cancelled: the human did not decide.
fn answers(prompts: &[Value], call: &Value) -> Vec<Value> {
    let responses = call.pointer("/params/inputResponses");
    prompts
        .iter()
        .map(|id| {
            let answer = responses
                .and_then(|r| r.get(elicit_key(id)))
                .cloned()
                .unwrap_or_else(|| json!({ "action": "cancel" }));
            response(id.clone(), answer)
        })
        .collect()
}

/// Whether a synthesized `initialize` tells the bridge roots come later.
pub(crate) fn defers_roots(initialize: &Value) -> bool {
    initialize
        .pointer("/params/capabilities/experimental")
        .and_then(|experimental| experimental.get(ROOTS_ON_FIRST_CALL_CAPABILITY))
        .is_some()
}

/// Mark a synthesized `initialize` as deferring roots to the first call.
pub(crate) fn mark_defers_roots(initialize: &mut Value) {
    let Some(capabilities) = initialize
        .pointer_mut("/params/capabilities")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let experimental = capabilities
        .entry("experimental")
        .or_insert_with(|| json!({}));
    if let Some(experimental) = experimental.as_object_mut() {
        experimental.insert(ROOTS_ON_FIRST_CALL_CAPABILITY.to_string(), json!({}));
    }
}

/// Whether a message from the bridge is a request addressed to the client.
pub(crate) fn is_server_request(message: &Value) -> bool {
    message.get("method").is_some() && message.get("id").is_some_and(|id| !id.is_null())
}

fn is_exempt(call: &Value) -> bool {
    call.pointer("/params/name")
        .and_then(Value::as_str)
        .is_some_and(is_scope_exempt_tool)
}

/// The bridge and worker speak the legacy protocol: the retry's MRTR fields
/// mean nothing to them.
fn strip_input_fields(mut call: Value) -> Value {
    if let Some(params) = call.get_mut("params").and_then(Value::as_object_mut) {
        params.remove("inputResponses");
        params.remove("requestState");
    }
    call
}

fn response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

// ── subscriptions/listen and what reaches stdout (SPEC R-HUB.4.4) ───────────

const SUBSCRIPTION_ID_META: &str = "io.modelcontextprotocol/subscriptionId";
const ACKNOWLEDGED_METHOD: &str = "notifications/subscriptions/acknowledged";
const CANCELLED_METHOD: &str = "notifications/cancelled";
const PROGRESS_METHOD: &str = "notifications/progress";
const TOOLS_LIST_CHANGED_METHOD: &str = "notifications/tools/list_changed";

/// A modern client's open `subscriptions/listen` streams. Each is named by
/// the id of the request that opened it, which stays unanswered until the
/// client cancels it.
#[derive(Debug, Default)]
pub(crate) struct Listens {
    /// (listen request id, wants `tools/list_changed`)
    streams: Vec<(Value, bool)>,
}

impl Listens {
    /// Open a stream and return its acknowledgement, which must precede
    /// anything sent on it. The filter it echoes is what ahma honours: tool
    /// list changes. ahma has no prompts and pushes no resource updates.
    pub(crate) fn open(&mut self, request: &Value) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let tools = request
            .pointer("/params/notifications/toolsListChanged")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.streams.push((id.clone(), tools));
        let accepted = if tools {
            json!({ "toolsListChanged": true })
        } else {
            json!({})
        };
        json!({
            "jsonrpc": "2.0",
            "method": ACKNOWLEDGED_METHOD,
            "params": { "_meta": { SUBSCRIPTION_ID_META: id }, "notifications": accepted },
        })
    }

    /// Close the stream a client `notifications/cancelled` names, returning
    /// the `complete` result that ends it; `None` when it names no stream.
    pub(crate) fn close(&mut self, cancelled: &Value) -> Option<Value> {
        if cancelled.get("method").and_then(Value::as_str) != Some(CANCELLED_METHOD) {
            return None;
        }
        let id = cancelled.pointer("/params/requestId")?;
        let at = self.streams.iter().position(|(open, _)| open == id)?;
        let (id, _) = self.streams.remove(at);
        Some(response(
            id.clone(),
            json!({ "resultType": "complete", "_meta": { SUBSCRIPTION_ID_META: id } }),
        ))
    }

    /// What a message bound for a modern client's stdout becomes. A server
    /// may write only results, notifications about a request in flight, and
    /// notifications on a stream the client opened for them. So:
    ///
    /// * a result is passed, marked `complete` if it does not say otherwise;
    /// * `tools/list_changed` goes once to each stream that asked for it,
    ///   tagged with that stream's id, and nowhere else;
    /// * progress and stream acknowledgements pass;
    /// * everything else — log messages, ahma's own notifications, and any
    ///   server request that got this far (R-HUB.4.2) — is dropped.
    pub(crate) fn outgoing(&self, message: Value) -> Vec<Value> {
        let method = message.get("method").and_then(Value::as_str);
        match method {
            None => vec![with_result_type(message)],
            Some(_) if is_server_request(&message) => {
                tracing::warn!(
                    ?method,
                    "Dropping a server request bound for a 2026-07-28 client's stdout"
                );
                Vec::new()
            }
            Some(TOOLS_LIST_CHANGED_METHOD) => self
                .streams
                .iter()
                .filter(|(_, tools)| *tools)
                .map(|(id, _)| tagged(&message, id))
                .collect(),
            Some(PROGRESS_METHOD | ACKNOWLEDGED_METHOD) => vec![message],
            Some(_) => Vec::new(),
        }
    }
}

/// Results of this protocol version must say what they are.
fn with_result_type(mut message: Value) -> Value {
    if let Some(result) = message.get_mut("result").and_then(Value::as_object_mut) {
        result
            .entry("resultType")
            .or_insert_with(|| json!("complete"));
    }
    message
}

fn tagged(notification: &Value, subscription: &Value) -> Value {
    let mut notification = notification.clone();
    let params = notification
        .as_object_mut()
        .map(|n| n.entry("params").or_insert_with(|| json!({})));
    if let Some(params) = params.and_then(Value::as_object_mut) {
        let meta = params.entry("_meta").or_insert_with(|| json!({}));
        if let Some(meta) = meta.as_object_mut() {
            meta.insert(SUBSCRIPTION_ID_META.to_string(), subscription.clone());
        }
    }
    notification
}

/// The proxy's stdio, with the R-HUB.4.4 filter applied to everything it
/// writes once the client turned out to be a 2026-07-28 one. Every write goes
/// through here — the bridge's messages and the proxy's own (session events,
/// the post-reconnect `tools/list_changed`) — so no path can skip the rule.
pub(crate) struct ModernStdio<S> {
    inner: S,
    /// `None` until `server/discover`: a legacy client gets every message.
    listens: Option<Listens>,
}

impl<S> ModernStdio<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            listens: None,
        }
    }

    pub(crate) fn go_modern(&mut self) {
        self.listens.get_or_insert_with(Listens::default);
    }

    /// The client's listen streams; `None` for a legacy client.
    pub(crate) fn listens(&mut self) -> Option<&mut Listens> {
        self.listens.as_mut()
    }
}

impl<S> rmcp::transport::Transport<rmcp::service::RoleServer> for ModernStdio<S>
where
    S: rmcp::transport::Transport<rmcp::service::RoleServer>,
{
    type Error = S::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<rmcp::service::RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let messages = match &self.listens {
            None => vec![item],
            Some(listens) => match serde_json::to_value(&item) {
                Ok(value) => listens
                    .outgoing(value)
                    .into_iter()
                    .filter_map(|m| serde_json::from_value(m).ok())
                    .collect(),
                Err(_) => vec![item],
            },
        };
        // Each send's future is `'static` by the trait's contract, but its
        // type still names `&mut self`; boxing erases that, so several can be
        // started in order and awaited together.
        type PendingSend<E> =
            std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), E>> + Send>>;
        let mut sends: Vec<PendingSend<S::Error>> = Vec::with_capacity(messages.len());
        for message in messages {
            sends.push(Box::pin(self.inner.send(message)));
        }
        async move {
            for send in sends {
                send.await?;
            }
            Ok(())
        }
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<
        Output = Option<rmcp::service::RxJsonRpcMessage<rmcp::service::RoleServer>>,
    > + Send {
        self.inner.receive()
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discover_with_roots() -> Value {
        json!({"roots": {}})
    }

    fn call(id: i64, name: &str, extra: Value) -> Value {
        let mut call = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": {}}
        });
        if let Value::Object(extra) = extra {
            call["params"].as_object_mut().unwrap().extend(extra);
        }
        call
    }

    fn answer(state: &Value) -> Value {
        json!({
            "inputResponses": {ROOTS_INPUT_KEY: {"roots": [{"uri": "file:///w"}]}},
            "requestState": state,
        })
    }

    fn asked(step: CallStep) -> Value {
        match step {
            CallStep::AskClient(result) => result,
            other => panic!("expected an input request, got {other:?}"),
        }
    }

    #[test]
    fn a_scoped_call_is_answered_with_an_input_request_for_roots() {
        let mut session = ModernSession::new(discover_with_roots());
        let result = asked(
            session.on_tools_call(call(2, "run_terminal_command", json!({})), Instant::now()),
        );
        assert_eq!(result["resultType"], "input_required");
        assert_eq!(
            result["inputRequests"][ROOTS_INPUT_KEY]["method"],
            ROOTS_LIST_METHOD
        );
        assert!(result["requestState"].is_string());
        assert_eq!(
            session.flush(),
            Flush::default(),
            "nothing reaches the bridge"
        );
    }

    #[test]
    fn the_answered_retry_is_held_until_the_bridge_has_the_roots() {
        let mut session = ModernSession::new(discover_with_roots());
        let now = Instant::now();
        let state =
            asked(session.on_tools_call(call(2, "x", json!({})), now))["requestState"].clone();
        assert_eq!(
            session.on_tools_call(call(3, "x", answer(&state)), now),
            CallStep::Held
        );

        let flush = session.flush();
        assert_eq!(flush.to_bridge.len(), 1);
        assert_eq!(flush.to_bridge[0]["method"], ROOTS_LIST_CHANGED_METHOD);
        assert!(flush.calls.is_empty(), "held until the bridge asks");

        assert_eq!(
            session.on_server_request(&json!({"jsonrpc": "2.0", "id": 9, "method": "roots/list"})),
            None
        );
        let flush = session.flush();
        assert_eq!(flush.to_bridge[0]["id"], 9);
        assert_eq!(flush.to_bridge[0]["result"]["roots"][0]["uri"], "file:///w");
        assert_eq!(flush.calls.len(), 1);
        assert_eq!(flush.calls[0]["id"], 3);
        assert!(flush.calls[0]["params"].get("inputResponses").is_none());
        assert!(flush.calls[0]["params"].get("requestState").is_none());

        assert!(matches!(
            session.on_tools_call(call(4, "x", json!({})), now),
            CallStep::Forward(_)
        ));
        assert!(session.roots_response().is_some());
    }

    #[test]
    fn a_roots_list_the_bridge_sent_first_is_answered_without_a_list_changed() {
        let mut session = ModernSession::new(discover_with_roots());
        let now = Instant::now();
        session.on_server_request(&json!({"jsonrpc": "2.0", "id": 9, "method": "roots/list"}));
        let state =
            asked(session.on_tools_call(call(2, "x", json!({})), now))["requestState"].clone();
        session.on_tools_call(call(3, "x", answer(&state)), now);
        let flush = session.flush();
        assert_eq!(flush.to_bridge.len(), 1);
        assert_eq!(flush.to_bridge[0]["id"], 9);
        assert_eq!(flush.calls.len(), 1);
    }

    #[test]
    fn a_client_that_never_answers_is_asked_twice_then_gets_empty_roots() {
        let mut session = ModernSession::new(discover_with_roots());
        let now = Instant::now();
        for id in 2..2 + i64::from(MAX_ROOTS_ASKS) {
            asked(session.on_tools_call(call(id, "x", json!({})), now));
        }
        assert_eq!(
            session.on_tools_call(call(9, "x", json!({})), now),
            CallStep::Held
        );
        session.on_server_request(&json!({"jsonrpc": "2.0", "id": 5, "method": "roots/list"}));
        let flush = session.flush();
        assert_eq!(flush.to_bridge[0]["result"], json!({"roots": []}));
        assert_eq!(flush.calls[0]["id"], 9);
    }

    #[test]
    fn a_stale_request_state_is_not_an_answer() {
        let mut session = ModernSession::new(discover_with_roots());
        let now = Instant::now();
        asked(session.on_tools_call(call(2, "x", json!({})), now));
        let step = session.on_tools_call(call(3, "x", answer(&json!("forged"))), now);
        assert!(matches!(step, CallStep::AskClient(_)), "{step:?}");
    }

    #[test]
    fn a_client_without_roots_is_never_asked() {
        let mut session = ModernSession::new(json!({}));
        assert_eq!(
            session.on_tools_call(call(2, "x", json!({})), Instant::now()),
            CallStep::Held
        );
        let flush = session.flush();
        assert_eq!(flush.to_bridge[0]["method"], ROOTS_LIST_CHANGED_METHOD);
    }

    #[test]
    fn a_per_request_capability_overrides_discover() {
        let mut session = ModernSession::new(json!({}));
        let with_roots = json!({"_meta": {CLIENT_CAPABILITIES_META: {"roots": {}}}});
        assert!(matches!(
            session.on_tools_call(call(2, "x", with_roots), Instant::now()),
            CallStep::AskClient(_)
        ));
    }

    #[test]
    fn scope_exempt_tools_are_never_held() {
        let mut session = ModernSession::new(discover_with_roots());
        assert!(matches!(
            session.on_tools_call(call(2, "ahma_help", json!({})), Instant::now()),
            CallStep::Forward(_)
        ));
    }

    #[test]
    fn held_calls_are_released_when_the_bridge_never_asks() {
        let mut session = ModernSession::new(json!({}));
        let now = Instant::now();
        session.on_tools_call(call(2, "x", json!({})), now);
        session.flush();
        assert_eq!(session.held_deadline(), Some(now + HELD_CALL_GRACE));
        assert_eq!(session.release_held().len(), 1);
        assert_eq!(session.held_deadline(), None);
        assert!(matches!(
            session.on_tools_call(call(3, "x", json!({})), now),
            CallStep::Forward(_)
        ));
    }

    #[test]
    fn server_requests_other_than_roots_are_answered_for_the_client() {
        let mut session = ModernSession::default();
        let cancel = session
            .on_server_request(&json!({"jsonrpc": "2.0", "id": 1, "method": "elicitation/create"}))
            .unwrap();
        assert_eq!(cancel["result"]["action"], "cancel");
        let pong = session
            .on_server_request(&json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}))
            .unwrap();
        assert_eq!(pong["result"], json!({}));
        let refused = session
            .on_server_request(
                &json!({"jsonrpc": "2.0", "id": 3, "method": "sampling/createMessage"}),
            )
            .unwrap();
        assert_eq!(refused["error"]["code"], -32601);
    }

    #[test]
    fn the_defer_marker_round_trips() {
        let mut init = json!({"params": {"capabilities": {"roots": {}}}});
        assert!(!defers_roots(&init));
        mark_defers_roots(&mut init);
        assert!(defers_roots(&init));
        assert!(init["params"]["capabilities"]["roots"].is_object());
    }

    fn listen(id: i64, notifications: Value) -> Value {
        json!({
            "jsonrpc": "2.0", "id": id, "method": "subscriptions/listen",
            "params": {"_meta": {}, "notifications": notifications}
        })
    }

    #[test]
    fn a_listen_is_acknowledged_with_only_what_ahma_honours() {
        let mut listens = Listens::default();
        let ack = listens.open(&listen(
            7,
            json!({"toolsListChanged": true, "promptsListChanged": true, "resourceSubscriptions": ["file:///x"]}),
        ));
        assert_eq!(ack["method"], ACKNOWLEDGED_METHOD);
        assert_eq!(ack["params"]["_meta"][SUBSCRIPTION_ID_META], 7);
        assert_eq!(
            ack["params"]["notifications"],
            json!({"toolsListChanged": true})
        );
        assert_eq!(
            listens.outgoing(ack.clone()),
            vec![ack],
            "the ack itself passes"
        );
    }

    #[test]
    fn tools_list_changed_goes_to_each_stream_that_asked_tagged() {
        let mut listens = Listens::default();
        let changed = json!({"jsonrpc": "2.0", "method": TOOLS_LIST_CHANGED_METHOD});
        assert!(
            listens.outgoing(changed.clone()).is_empty(),
            "no stream, no notification"
        );
        listens.open(&listen(1, json!({"toolsListChanged": true})));
        listens.open(&listen(2, json!({})));
        listens.open(&listen(3, json!({"toolsListChanged": true})));
        let out = listens.outgoing(changed);
        let ids: Vec<&Value> = out
            .iter()
            .map(|m| &m["params"]["_meta"][SUBSCRIPTION_ID_META])
            .collect();
        assert_eq!(ids, vec![&json!(1), &json!(3)]);
    }

    #[test]
    fn a_cancel_completes_its_stream_and_nothing_else() {
        let mut listens = Listens::default();
        listens.open(&listen(4, json!({"toolsListChanged": true})));
        let cancel = |id: i64| json!({"jsonrpc": "2.0", "method": CANCELLED_METHOD, "params": {"requestId": id}});
        assert_eq!(
            listens.close(&cancel(9)),
            None,
            "another request's cancel is not ours"
        );
        let done = listens.close(&cancel(4)).expect("completes");
        assert_eq!(done["id"], 4);
        assert_eq!(done["result"]["resultType"], "complete");
        assert_eq!(done["result"]["_meta"][SUBSCRIPTION_ID_META], 4);
        let changed = json!({"jsonrpc": "2.0", "method": TOOLS_LIST_CHANGED_METHOD});
        assert!(
            listens.outgoing(changed).is_empty(),
            "a closed stream gets nothing"
        );
    }

    #[test]
    fn outgoing_marks_results_and_drops_what_was_not_asked_for() {
        let listens = Listens::default();
        let result = listens.outgoing(json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        assert_eq!(result[0]["result"]["resultType"], "complete");
        let asked = listens.outgoing(json!({
            "jsonrpc": "2.0", "id": 2, "result": {"resultType": "input_required"}
        }));
        assert_eq!(
            asked[0]["result"]["resultType"], "input_required",
            "kept as is"
        );
        let error = json!({"jsonrpc": "2.0", "id": 3, "error": {"code": 1, "message": "m"}});
        assert_eq!(listens.outgoing(error.clone()), vec![error]);
        let progress = json!({"jsonrpc": "2.0", "method": PROGRESS_METHOD, "params": {}});
        assert_eq!(listens.outgoing(progress.clone()), vec![progress]);
        for dropped in [
            json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {}}),
            json!({"jsonrpc": "2.0", "method": "notifications/ahma/session_event", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 5, "method": "roots/list"}),
        ] {
            assert!(listens.outgoing(dropped.clone()).is_empty(), "{dropped}");
        }
    }

    fn prompt(id: i64) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": ELICITATION_METHOD, "params": {"message": "m"}})
    }

    fn forwarded(route: CallRoute) -> Value {
        match route {
            CallRoute::Forward(call) => call,
            other => panic!("expected a forward, got {other:?}"),
        }
    }

    fn answered(route: CallRoute) -> (Vec<Value>, Vec<Value>) {
        match route {
            CallRoute::Answer {
                to_bridge,
                to_client,
            } => (to_bridge, to_client),
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    fn retry(id: i64, asked: &Value, answer: Option<Value>) -> Value {
        let state = asked["result"]["requestState"].clone();
        let key = asked["result"]["inputRequests"]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        let responses = answer
            .map(|a| json!({ key: a }))
            .unwrap_or_else(|| json!({}));
        call(
            id,
            "x",
            json!({"inputResponses": responses, "requestState": state}),
        )
    }

    #[test]
    fn a_result_that_beats_the_retry_waits_for_it() {
        let mut prompts = Prompts::default();
        forwarded(prompts.on_call(call(2, "x", json!({}))));
        let asked = prompts.on_elicitation(&prompt(70)).remove(0);
        assert_eq!(asked["id"], 2);
        let early = json!({"jsonrpc": "2.0", "id": 2, "result": {"content": []}});
        assert!(
            prompts.on_bridge_response(early).is_empty(),
            "held for the retry"
        );
        let (to_bridge, to_client) =
            answered(prompts.on_call(retry(3, &asked, Some(json!({"action": "accept"})))));
        assert_eq!(to_bridge[0]["id"], 70);
        assert_eq!(to_client.len(), 1);
        assert_eq!(to_client[0]["id"], 3, "delivered under the retry's id");
    }

    #[test]
    fn a_prompt_the_retry_leaves_unanswered_is_cancelled() {
        let mut prompts = Prompts::default();
        forwarded(prompts.on_call(call(2, "x", json!({}))));
        let asked = prompts.on_elicitation(&prompt(70)).remove(0);
        let (to_bridge, _) = answered(prompts.on_call(retry(3, &asked, None)));
        assert_eq!(to_bridge[0]["result"]["action"], "cancel");
    }

    #[test]
    fn a_withdrawn_prompt_is_never_asked() {
        let mut prompts = Prompts::default();
        prompts.on_elicitation(&prompt(71));
        prompts.on_bridge_cancelled(&json!({
            "jsonrpc": "2.0", "method": CANCELLED_METHOD, "params": {"requestId": 71}
        }));
        let call = forwarded(prompts.on_call(call(2, "x", json!({}))));
        assert_eq!(call["id"], 2, "nothing left to ask, so the call just runs");
    }

    #[test]
    fn a_client_cancel_reaches_the_call_the_bridge_knows() {
        let mut prompts = Prompts::default();
        forwarded(prompts.on_call(call(2, "x", json!({}))));
        let asked = prompts.on_elicitation(&prompt(70)).remove(0);
        answered(prompts.on_call(retry(3, &asked, Some(json!({"action": "accept"})))));
        let cancel =
            json!({"jsonrpc": "2.0", "method": CANCELLED_METHOD, "params": {"requestId": 3}});
        assert_eq!(
            prompts.rewrite_client_cancel(cancel)["params"]["requestId"],
            2
        );
        let other =
            json!({"jsonrpc": "2.0", "method": CANCELLED_METHOD, "params": {"requestId": 9}});
        assert_eq!(
            prompts.rewrite_client_cancel(other)["params"]["requestId"],
            9
        );
    }
}
