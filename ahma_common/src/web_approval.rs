//! Web-egress session-approval coordination (SPEC §4.6 R-WEB.5) — the in-session
//! half of the web policy, sibling to [`crate::scope_grant`] for the filesystem
//! sandbox.
//!
//! [`crate::web_policy`] decides *statically* whether a URL is allowed, denied, or
//! needs a prompt, given the configured `always_allow`/`never_allow` lists and the
//! default policy. When the default policy is `deny`, an unknown domain yields
//! [`crate::web_policy::WebDecision::Prompt`]. This module is what turns that prompt
//! into a *session decision*: a human answers once, and the answer is remembered for
//! the rest of the session so the same domain is not re-asked on every request.
//!
//! ## What a session decision may and may not do
//!
//! Unlike a scope grant — which can only *persist for the next start* and never
//! widens the live sandbox (SPEC R5) — a web approval **does** take effect in the
//! live session, because web egress is a per-request policy check, not a
//! kernel-locked scope. The three "yes" answers differ only in how long they last:
//!
//!  - [`WebApprovalDecision::AllowOnce`] — permit *this* request; remember nothing.
//!  - [`WebApprovalDecision::AllowSession`] — add the domain to the session grant
//!    set so [`crate::web_policy::WebPolicy::decide`] allows it until the server
//!    restarts, but write nothing to disk.
//!  - [`WebApprovalDecision::AllowAlways`] — the caller additionally persists the
//!    domain to `[web].always_allow` in `~/.ahma/settings.toml` (via the same
//!    `ahma web allow` path), so it survives restarts.
//!
//! A [`WebApprovalDecision::Deny`] adds the domain to the session deny set, which
//! both blocks it and suppresses re-asking for the rest of the session.
//!
//! ## What [`WebApprovalCoordinator`] guarantees (mirrors [`crate::scope_grant`])
//!
//!  - **Dedup / debounce**: a domain with a decision already in flight is asked
//!    **at most once**; concurrent requests to the same unknown domain fan in to one
//!    prompt.
//!  - **No re-ask loops**: once a domain is granted or denied for the session it is
//!    never asked again — [`begin`] returns `None`.
//!  - **First-answer-wins**: when a prompt is fanned to several surfaces, the first
//!    answer resolves it and later answers no-op ([`resolve`] is idempotent).
//!
//! [`begin`]: WebApprovalCoordinator::begin
//! [`resolve`]: WebApprovalCoordinator::resolve

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// A request to approve outbound web access to `domain`, fanned to every capable
/// surface under one `decision_id`.
///
/// `Serialize`/`Deserialize` so it can travel over the daemon hub (TUI) and inside
/// an MCP `elicitation/create` payload, exactly like
/// [`crate::scope_grant::ScopeGrantRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebApprovalRequest {
    /// Correlates the fan-out and the answer; binds to the asking session.
    pub decision_id: String,
    /// The host being requested (lower-cased), e.g. `"api.github.com"`. This is
    /// what an approval grants — not the full URL — so one answer covers every
    /// path on the host.
    pub domain: String,
    /// The full URL that triggered the prompt, shown at the surface for context.
    pub url: String,
    /// The tool that requested egress (e.g. `"fetch_webpage"`), for the prompt text
    /// and the audit trail.
    pub tool: Option<String>,
}

/// The human's answer at any surface. Four-valued: a plain `Deny` (the safe
/// default / Enter choice — SPEC R-WEB.6 mirrors R5.3.1: Enter must never widen)
/// plus the three "yes" durations described in the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebApprovalDecision {
    /// Refuse. Suppresses re-asking this domain for the session.
    Deny,
    /// Allow this one request; remember nothing.
    AllowOnce,
    /// Allow this domain for the rest of the session (in-memory only).
    AllowSession,
    /// Allow this domain and persist it to `[web].always_allow`.
    AllowAlways,
}

/// What the caller should do after [`WebApprovalCoordinator::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebResolveOutcome {
    /// Permit this request only; nothing was remembered.
    AllowOnce {
        /// The approved host.
        domain: String,
    },
    /// The domain was added to the session grant set; future requests to it will be
    /// allowed by [`crate::web_policy::WebPolicy::decide`] without a prompt.
    AllowSession {
        /// The approved host.
        domain: String,
    },
    /// Allow now *and* persist: the caller writes `domain` to `[web].always_allow`.
    /// It is also added to the session grant set so it takes effect immediately.
    Persist {
        /// The approved host.
        domain: String,
    },
    /// The human denied; the domain is now in the session deny set.
    Denied {
        /// The denied host.
        domain: String,
    },
    /// This `decision_id` was already resolved (a twin surface answered first).
    AlreadyResolved,
    /// This `decision_id` is not in flight (stale or never issued). Ignored.
    Unknown,
}

/// Coordinates in-flight web-approval decisions and the session grant/deny sets.
/// Cheap to share behind an `Arc`; all state is behind one mutex.
#[derive(Debug, Default)]
pub struct WebApprovalCoordinator {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// In-flight decisions awaiting an answer, by `decision_id`.
    in_flight: HashMap<String, WebApprovalRequest>,
    /// Domains currently awaiting an answer — the dedup gate.
    active_domains: HashSet<String>,
    /// Domains approved for the rest of the session (threaded into `decide`).
    session_grants: HashSet<String>,
    /// Domains denied for the rest of the session (threaded into `decide`, and the
    /// re-ask suppressor).
    session_denies: HashSet<String>,
    /// `decision_id`s already resolved — makes [`WebApprovalCoordinator::resolve`]
    /// idempotent so a late twin answer no-ops.
    resolved: HashSet<String>,
}

/// Lower-case a host so grant/deny/dedup all key on one canonical spelling.
fn norm(domain: &str) -> String {
    domain.trim().to_ascii_lowercase()
}

impl WebApprovalCoordinator {
    /// Create an empty coordinator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a decision for `domain`, or return `None` if it should not be asked:
    /// the domain already has a session grant or deny, or a decision for it is
    /// already in flight.
    ///
    /// On `Some`, a fresh `decision_id` is minted and the request recorded as
    /// in-flight; the caller fans it out to every capable surface.
    pub fn begin(
        &self,
        domain: &str,
        url: &str,
        tool: Option<String>,
    ) -> Option<WebApprovalRequest> {
        let domain = norm(domain);
        let mut inner = self.inner.lock().unwrap();
        if inner.session_grants.contains(&domain)
            || inner.session_denies.contains(&domain)
            || inner.active_domains.contains(&domain)
        {
            return None;
        }
        let decision_id = uuid::Uuid::new_v4().to_string();
        let req = WebApprovalRequest {
            decision_id: decision_id.clone(),
            domain: domain.clone(),
            url: url.to_string(),
            tool,
        };
        inner.active_domains.insert(domain);
        inner.in_flight.insert(decision_id, req.clone());
        Some(req)
    }

    /// Resolve a decision with the human's answer. First-answer-wins and
    /// idempotent: a second call for the same `decision_id` returns
    /// [`WebResolveOutcome::AlreadyResolved`].
    pub fn resolve(&self, decision_id: &str, decision: WebApprovalDecision) -> WebResolveOutcome {
        let mut inner = self.inner.lock().unwrap();
        if inner.resolved.contains(decision_id) {
            return WebResolveOutcome::AlreadyResolved;
        }
        let Some(req) = inner.in_flight.remove(decision_id) else {
            return WebResolveOutcome::Unknown;
        };
        inner.resolved.insert(decision_id.to_string());
        inner.active_domains.remove(&req.domain);
        let domain = req.domain;

        match decision {
            WebApprovalDecision::Deny => {
                inner.session_denies.insert(domain.clone());
                WebResolveOutcome::Denied { domain }
            }
            WebApprovalDecision::AllowOnce => {
                // Remember nothing: the domain stays askable if it comes up again.
                WebResolveOutcome::AllowOnce { domain }
            }
            WebApprovalDecision::AllowSession => {
                inner.session_grants.insert(domain.clone());
                WebResolveOutcome::AllowSession { domain }
            }
            WebApprovalDecision::AllowAlways => {
                // Take effect immediately for the session; the caller persists.
                inner.session_grants.insert(domain.clone());
                WebResolveOutcome::Persist { domain }
            }
        }
    }

    /// Cancel a decision without an answer (e.g. the asking session terminated).
    /// Marks it resolved so a late answer no-ops and frees the dedup gate; the
    /// domain is *not* added to any session set, so a future request may re-ask.
    /// Returns the request if it was in flight.
    pub fn cancel(&self, decision_id: &str) -> Option<WebApprovalRequest> {
        let mut inner = self.inner.lock().unwrap();
        let req = inner.in_flight.remove(decision_id);
        if let Some(r) = &req {
            inner.active_domains.remove(&r.domain);
        }
        inner.resolved.insert(decision_id.to_string());
        req
    }

    /// Snapshot of the domains granted for this session, to pass as `session_grants`
    /// to [`crate::web_policy::WebPolicy::decide`].
    pub fn session_grants(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .session_grants
            .iter()
            .cloned()
            .collect()
    }

    /// Snapshot of the domains denied for this session, to pass as `session_denies`
    /// to [`crate::web_policy::WebPolicy::decide`].
    pub fn session_denies(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .session_denies
            .iter()
            .cloned()
            .collect()
    }

    /// Whether `decision_id` is still awaiting an answer.
    pub fn is_in_flight(&self, decision_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .in_flight
            .contains_key(decision_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coord() -> WebApprovalCoordinator {
        WebApprovalCoordinator::new()
    }

    #[test]
    fn begin_dedups_same_domain_case_insensitively() {
        let c = coord();
        let first = c.begin("API.github.com", "https://api.github.com/x", None);
        assert!(first.is_some(), "first ask issues a decision");
        // Different spelling of the same host is deduped while in flight.
        assert!(
            c.begin("api.github.com", "https://api.github.com/y", None)
                .is_none(),
            "an in-flight domain is not re-asked"
        );
        // The request carries the normalized (lower-cased) domain.
        assert_eq!(first.unwrap().domain, "api.github.com");
    }

    #[test]
    fn allow_session_grants_and_threads_into_snapshot() {
        let c = coord();
        let req = c
            .begin(
                "ok.example",
                "https://ok.example/x",
                Some("fetch_webpage".into()),
            )
            .unwrap();
        let out = c.resolve(&req.decision_id, WebApprovalDecision::AllowSession);
        assert_eq!(
            out,
            WebResolveOutcome::AllowSession {
                domain: "ok.example".into()
            }
        );
        assert_eq!(c.session_grants(), vec!["ok.example".to_string()]);
        assert!(c.session_denies().is_empty());
        // A granted domain is never asked again this session.
        assert!(
            c.begin("ok.example", "https://ok.example/z", None)
                .is_none(),
            "a session-granted domain is not re-asked"
        );
    }

    #[test]
    fn allow_once_remembers_nothing() {
        let c = coord();
        let req = c
            .begin("once.example", "https://once.example/x", None)
            .unwrap();
        let out = c.resolve(&req.decision_id, WebApprovalDecision::AllowOnce);
        assert_eq!(
            out,
            WebResolveOutcome::AllowOnce {
                domain: "once.example".into()
            }
        );
        // Nothing was remembered: neither set contains it, and it is askable again.
        assert!(c.session_grants().is_empty());
        assert!(c.session_denies().is_empty());
        assert!(
            c.begin("once.example", "https://once.example/y", None)
                .is_some(),
            "allow-once must not suppress a later prompt"
        );
    }

    #[test]
    fn allow_always_grants_session_and_signals_persist() {
        let c = coord();
        let req = c
            .begin("keep.example", "https://keep.example/x", None)
            .unwrap();
        let out = c.resolve(&req.decision_id, WebApprovalDecision::AllowAlways);
        assert_eq!(
            out,
            WebResolveOutcome::Persist {
                domain: "keep.example".into()
            }
        );
        // Persist also takes effect immediately for the live session.
        assert_eq!(c.session_grants(), vec!["keep.example".to_string()]);
    }

    #[test]
    fn deny_blocks_and_suppresses_reask() {
        let c = coord();
        let req = c.begin("no.example", "https://no.example/x", None).unwrap();
        let out = c.resolve(&req.decision_id, WebApprovalDecision::Deny);
        assert_eq!(
            out,
            WebResolveOutcome::Denied {
                domain: "no.example".into()
            }
        );
        assert_eq!(c.session_denies(), vec!["no.example".to_string()]);
        assert!(
            c.begin("no.example", "https://no.example/y", None)
                .is_none(),
            "a denied domain is not re-asked"
        );
    }

    #[test]
    fn resolve_is_idempotent_first_answer_wins() {
        let c = coord();
        let req = c
            .begin("race.example", "https://race.example/x", None)
            .unwrap();
        let first = c.resolve(&req.decision_id, WebApprovalDecision::AllowSession);
        assert!(matches!(first, WebResolveOutcome::AllowSession { .. }));
        // A twin surface answering deny later cannot flip the granted domain.
        let second = c.resolve(&req.decision_id, WebApprovalDecision::Deny);
        assert_eq!(second, WebResolveOutcome::AlreadyResolved);
        assert_eq!(c.session_grants(), vec!["race.example".to_string()]);
        assert!(c.session_denies().is_empty());
    }

    #[test]
    fn resolve_unknown_decision_id_is_ignored() {
        let c = coord();
        assert_eq!(
            c.resolve("never-issued", WebApprovalDecision::AllowSession),
            WebResolveOutcome::Unknown
        );
    }

    #[test]
    fn cancel_frees_gate_and_blocks_late_answer_without_remembering() {
        let c = coord();
        let req = c
            .begin("gone.example", "https://gone.example/x", None)
            .unwrap();
        let cancelled = c.cancel(&req.decision_id);
        assert_eq!(
            cancelled.as_ref().map(|r| r.decision_id.clone()),
            Some(req.decision_id.clone())
        );
        // Late answer after cancel no-ops.
        assert_eq!(
            c.resolve(&req.decision_id, WebApprovalDecision::AllowSession),
            WebResolveOutcome::AlreadyResolved
        );
        // Not remembered in either set — a fresh request may legitimately re-ask.
        assert!(c.session_grants().is_empty());
        assert!(c.session_denies().is_empty());
        assert!(
            c.begin("gone.example", "https://gone.example/y", None)
                .is_some()
        );
    }
}
