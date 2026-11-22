//! What a message the client POSTs to `/mcp` is, decided once.
//!
//! The mirror image of the bridge's classification of what its subprocess
//! writes (`session::SubprocessFrame`): every message arriving from the client
//! is classified here, at the top of the `POST /mcp` pipeline, and every later
//! routing decision — answered or `202`, forwarded or delivered to a waiting
//! server request, which id an error echoes — matches on the result instead of
//! re-reading `id`, `method`, `result` and `error` from the raw JSON
//! (SPEC R8.3.8).

use serde_json::Value;

/// How one JSON-RPC message POSTed by the client must be routed.
///
/// The rules, in order:
///
/// 1. No string `method`, an `id`, and a `result` or `error` member: the
///    client's answer to a request the server sent it (`roots/list`,
///    elicitation, a routed `sampling/createMessage`) — a [`Response`].
/// 2. Otherwise an `id` makes it a [`Request`], and its absence a
///    [`Notification`].
///
/// Each rule tests *presence*: an `id` of `null` is an `id`, and so is a
/// `result` of `null`. A `method` that is present but not a string counts as
/// absent, as it does for the subprocess's frames. A payload that is not an
/// object has none of the members, so it is a [`Notification`] with no method
/// (a batch array never gets this far: the HTTP handler refuses it first).
///
/// The variants borrow from the payload, which is still forwarded verbatim.
///
/// [`Response`]: Inbound::Response
/// [`Request`]: Inbound::Request
/// [`Notification`]: Inbound::Notification
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Inbound<'a> {
    /// Has an `id`: forwarded, and answered with the subprocess's response —
    /// except under a `null` id, which the session does not wait for
    /// (`session::RpcId::of_request`).
    /// `method` is `None` when the message has none (or not a string); such a
    /// message is still forwarded, and the subprocess answers it.
    Request {
        id: &'a Value,
        method: Option<&'a str>,
    },
    /// No `id`: forwarded, and answered HTTP 202 with no body.
    Notification { method: Option<&'a str> },
    /// The client answering a server → client request: delivered to the
    /// routed request waiting for it, or else forwarded to the subprocess;
    /// acknowledged `202`, never answered itself.
    Response {
        id: &'a Value,
        /// The `result` member, when it has one (a `roots/list` answer
        /// carries the roots that lock the sandbox).
        result: Option<&'a Value>,
    },
}

impl<'a> Inbound<'a> {
    /// Classify one parsed message. Total: every JSON value is one of the
    /// three.
    pub(crate) fn classify(payload: &'a Value) -> Self {
        let method = payload.get("method").and_then(Value::as_str);
        let Some(id) = payload.get("id") else {
            return Self::Notification { method };
        };
        let result = payload.get("result");
        if method.is_none() && (result.is_some() || payload.get("error").is_some()) {
            return Self::Response { id, result };
        }
        Self::Request { id, method }
    }

    /// The message's `method`; a response has none.
    pub(crate) fn method(&self) -> Option<&'a str> {
        match self {
            Self::Request { method, .. } | Self::Notification { method } => *method,
            Self::Response { .. } => None,
        }
    }

    /// The `id` an answer to this message carries: its own, or JSON `null`
    /// for a notification. Never omitted — an error without an `id` cannot be
    /// matched to the request it answers.
    pub(crate) fn reply_id(&self) -> Value {
        match self {
            Self::Request { id, .. } | Self::Response { id, .. } => (*id).clone(),
            Self::Notification { .. } => Value::Null,
        }
    }

    /// A short name for logs.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Request { .. } => "request",
            Self::Notification { .. } => "notification",
            Self::Response { .. } => "response",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn classify(payload: &Value) -> Inbound<'_> {
        Inbound::classify(payload)
    }

    #[test]
    fn request_carries_its_id_and_method() {
        let numeric = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        assert_eq!(
            classify(&numeric),
            Inbound::Request {
                id: &json!(1),
                method: Some("tools/list")
            }
        );
        // `"1"` is not `1`: the id is carried exactly as written.
        let string = json!({"jsonrpc": "2.0", "id": "1", "method": "tools/list"});
        assert_eq!(classify(&string).reply_id(), json!("1"));
        assert_eq!(classify(&numeric).reply_id(), json!(1));
    }

    #[test]
    fn notification_has_no_id() {
        let payload = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let inbound = classify(&payload);
        assert_eq!(
            inbound,
            Inbound::Notification {
                method: Some("notifications/initialized")
            }
        );
        assert_eq!(inbound.method(), Some("notifications/initialized"));
        assert_eq!(inbound.reply_id(), Value::Null);
    }

    #[test]
    fn response_has_an_id_and_a_result_or_error_but_no_method() {
        let ok = json!({"jsonrpc": "2.0", "id": 0, "result": {"roots": []}});
        assert_eq!(
            classify(&ok),
            Inbound::Response {
                id: &json!(0),
                result: Some(&json!({"roots": []}))
            }
        );
        let err = json!({"jsonrpc": "2.0", "id": "e", "error": {"code": -1, "message": "no"}});
        assert_eq!(
            classify(&err),
            Inbound::Response {
                id: &json!("e"),
                result: None
            }
        );
        assert_eq!(classify(&err).method(), None);
        assert_eq!(classify(&err).reply_id(), json!("e"));
    }

    #[test]
    fn a_null_id_is_still_an_id() {
        let request = json!({"jsonrpc": "2.0", "id": null, "method": "tools/list"});
        assert_eq!(
            classify(&request),
            Inbound::Request {
                id: &Value::Null,
                method: Some("tools/list")
            }
        );
        let response = json!({"jsonrpc": "2.0", "id": null, "result": {}});
        assert!(matches!(classify(&response), Inbound::Response { .. }));
    }

    #[test]
    fn a_null_result_is_still_a_result() {
        let payload = json!({"jsonrpc": "2.0", "id": 2, "result": null});
        assert_eq!(
            classify(&payload),
            Inbound::Response {
                id: &json!(2),
                result: Some(&Value::Null)
            }
        );
    }

    #[test]
    fn a_method_wins_over_a_result() {
        let with_id = json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list", "result": {}});
        assert_eq!(
            classify(&with_id),
            Inbound::Request {
                id: &json!(5),
                method: Some("tools/list")
            }
        );
        let without_id = json!({"jsonrpc": "2.0", "method": "notifications/x", "result": {}});
        assert_eq!(
            classify(&without_id),
            Inbound::Notification {
                method: Some("notifications/x")
            }
        );
    }

    #[test]
    fn a_method_that_is_not_a_string_counts_as_absent() {
        let response = json!({"jsonrpc": "2.0", "id": 3, "method": 7, "result": {}});
        assert!(matches!(classify(&response), Inbound::Response { .. }));
        let request = json!({"jsonrpc": "2.0", "id": 3, "method": 7});
        assert_eq!(
            classify(&request),
            Inbound::Request {
                id: &json!(3),
                method: None
            }
        );
    }

    #[test]
    fn without_a_method_result_or_error_the_id_decides() {
        let with_id = json!({"jsonrpc": "2.0", "id": 9});
        assert_eq!(
            classify(&with_id),
            Inbound::Request {
                id: &json!(9),
                method: None
            }
        );
        let result_without_id = json!({"jsonrpc": "2.0", "result": {}});
        assert_eq!(
            classify(&result_without_id),
            Inbound::Notification { method: None }
        );
    }

    #[test]
    fn a_payload_that_is_not_an_object_is_a_notification_without_a_method() {
        for payload in [json!(42), json!("hello"), json!(null), json!([{"id": 1}])] {
            assert_eq!(
                classify(&payload),
                Inbound::Notification { method: None },
                "{payload}"
            );
        }
    }

    #[test]
    fn kind_names_each_variant() {
        assert_eq!(classify(&json!({"id": 1, "method": "m"})).kind(), "request");
        assert_eq!(classify(&json!({"method": "m"})).kind(), "notification");
        assert_eq!(classify(&json!({"id": 1, "result": {}})).kind(), "response");
    }
}
