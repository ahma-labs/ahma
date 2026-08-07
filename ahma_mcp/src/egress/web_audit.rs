//! Web-egress policy enforcement + audit for ahma's own HTTP tools (R-WEB.9).
//!
//! Pure decision→action mapping and audit-record construction, plus a
//! best-effort JSONL appender. Every outbound request from a governed tool is
//! recorded (allowed, denied, or passed through), so there is a forensic trail
//! of what the agent reached out to.

use ahma_common::web_policy::WebDecision;
use serde::Serialize;
use tokio::io::AsyncWriteExt;

/// What the caller should do with a request after consulting the policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchAction {
    /// Perform the request.
    Proceed,
    /// Refuse the request with this human-readable message.
    Deny(String),
}

/// Map a policy decision to a concrete action. In strict `deny` mode an unknown
/// domain yields [`WebDecision::Prompt`]; until an interactive approval surface
/// is wired (a later PR), that is resolved as a deny with an actionable hint.
pub fn action_for(decision: &WebDecision) -> FetchAction {
    match decision {
        WebDecision::Allow { .. } => FetchAction::Proceed,
        WebDecision::Deny { reason } => FetchAction::Deny(format!("web egress blocked: {reason}")),
        WebDecision::Prompt { domain } => FetchAction::Deny(format!(
            "web egress blocked: '{domain}' is not approved and the web policy is `deny`. \
             A human can allow it with:\n  ahma web allow {domain}\n\
             (or add it to [web].always_allow in ~/.ahma/settings.toml)."
        )),
    }
}

/// One audited outbound request (R-WEB.9.2 shape).
#[derive(Debug, Clone, Serialize)]
pub struct WebAuditRecord {
    pub ts: String,
    pub kind: &'static str,
    pub tool: String,
    pub method: &'static str,
    pub url: String,
    pub domain: String,
    pub decision: &'static str,
    pub matched_pattern: Option<String>,
}

/// Build an audit record from a decision. `ts` is supplied by the caller (stamp
/// with `chrono::Local::now().to_rfc3339()`), keeping this pure.
pub fn record(
    tool: &str,
    url: &str,
    domain: &str,
    decision: &WebDecision,
    ts: String,
) -> WebAuditRecord {
    let matched_pattern = match decision {
        WebDecision::Allow { matched } => Some(matched.clone()),
        _ => None,
    };
    WebAuditRecord {
        ts,
        kind: "web_request",
        tool: tool.to_string(),
        method: "GET",
        url: url.to_string(),
        domain: domain.to_string(),
        decision: decision.label(),
        matched_pattern,
    }
}

/// Append `record` as one JSON line to `<log_dir>/web_egress.jsonl`. Best-effort
/// (R-WEB.9.3): any error is logged at debug and swallowed so it never blocks or
/// fails the request.
pub async fn append(record: &WebAuditRecord) {
    let Ok(line) = serde_json::to_string(record) else {
        return;
    };
    let path = crate::utils::logging::project_log_dir().join("web_egress.jsonl");
    let write = async {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        f.write_all(format!("{line}\n").as_bytes()).await
    };
    if let Err(e) = write.await {
        tracing::debug!("web audit append failed ({}): {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_proceeds_deny_and_prompt_refuse() {
        assert_eq!(
            action_for(&WebDecision::Allow {
                matched: "api.github.com".into()
            }),
            FetchAction::Proceed
        );
        assert!(matches!(
            action_for(&WebDecision::Deny {
                reason: "never_allow".into()
            }),
            FetchAction::Deny(_)
        ));
        match action_for(&WebDecision::Prompt {
            domain: "unknown.example".into(),
        }) {
            FetchAction::Deny(msg) => {
                assert!(msg.contains("ahma web allow unknown.example"), "{msg}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn record_captures_decision_and_matched_pattern() {
        let r = record(
            "fetch_webpage",
            "https://api.github.com/x",
            "api.github.com",
            &WebDecision::Allow {
                matched: "*.github.com".into(),
            },
            "2026-07-02T00:00:00Z".into(),
        );
        assert_eq!(r.decision, "allow");
        assert_eq!(r.matched_pattern.as_deref(), Some("*.github.com"));
        assert_eq!(r.method, "GET");
        assert_eq!(r.kind, "web_request");
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"domain\":\"api.github.com\""));

        let denied = record(
            "fetch_webpage",
            "https://x.example/",
            "x.example",
            &WebDecision::Deny {
                reason: "blocked".into(),
            },
            "2026-07-02T00:00:00Z".into(),
        );
        assert_eq!(denied.decision, "deny");
        assert!(denied.matched_pattern.is_none());
    }
}
