//! Subprocess network-egress session-approval coordination (SPEC §4.6 R-NET) —
//! the in-session sibling of [`crate::web_approval`] for the guarded egress
//! proxy, and of [`crate::scope_grant`] for the filesystem sandbox.
//!
//! The egress proxy's static `[network] allow` list decides whether a
//! sandboxed subprocess's request is forwarded. When `--restrict-network` is
//! on and a subprocess reaches an unlisted domain, this module turns that into
//! a *session decision*: a human answers once (via MCP `elicitation/create`),
//! and the answer is remembered for the rest of the session so the same
//! domain is not re-asked on every connection.
//!
//! ## What a session decision may and may not do
//!
//! Mirrors [`crate::web_approval`] exactly: `allow_once` permits one
//! connection and remembers nothing; `allow_session` grants the domain for
//! the rest of the session (in-memory only); `allow_always` additionally
//! persists the domain to `[network].allow` in `~/.ahma/settings.toml`, so it
//! survives restarts. A `deny` adds the domain to the session deny set, which
//! both blocks it and suppresses re-asking for the rest of the session.
//!
//! ## What [`NetApprovalCoordinator`] guarantees
//!
//!  - **Dedup / debounce**: a domain with a decision already in flight is
//!    asked **at most once**; a second subprocess connection to the same
//!    unknown domain while the first is awaiting an answer is denied outright
//!    rather than raising a second prompt.
//!  - **No re-ask loops**: once a domain is granted or denied for the session
//!    it is never asked again — [`begin`] returns `None`.
//!  - **First-answer-wins**: [`resolve`] is idempotent.
//!
//! [`begin`]: NetApprovalCoordinator::begin
//! [`resolve`]: NetApprovalCoordinator::resolve

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::AhmaSettings;

/// A request to approve outbound subprocess egress to `domain`.
///
/// `Serialize`/`Deserialize` for parity with [`crate::web_approval::WebApprovalRequest`]
/// (future hub/TUI transport).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetApprovalRequest {
    /// Correlates the fan-out and the answer; binds to the asking session.
    pub decision_id: String,
    /// The host being requested (lower-cased), e.g. `"crates.io"`. This is
    /// what an approval grants, not the full target — so one answer covers
    /// every port/path on the host.
    pub domain: String,
    /// The `host:port` target that triggered the prompt, shown at the surface
    /// for context.
    pub target: String,
}

/// The human's answer. Four-valued, mirrors [`crate::web_approval::WebApprovalDecision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetApprovalDecision {
    /// Refuse. Suppresses re-asking this domain for the session.
    Deny,
    /// Allow this one connection; remember nothing.
    AllowOnce,
    /// Allow this domain for the rest of the session (in-memory only).
    AllowSession,
    /// Allow this domain and persist it to `[network].allow`.
    AllowAlways,
}

/// What the caller should do after [`NetApprovalCoordinator::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetResolveOutcome {
    /// Permit this connection only; nothing was remembered.
    AllowOnce {
        /// The approved host.
        domain: String,
    },
    /// The domain was added to the session grant set.
    AllowSession {
        /// The approved host.
        domain: String,
    },
    /// Allow now *and* persist: the caller writes `domain` to `[network].allow`.
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

/// Coordinates in-flight network-approval decisions and the session
/// grant/deny sets. Cheap to share behind an `Arc`; all state is behind one
/// mutex. Structurally identical to
/// [`crate::web_approval::WebApprovalCoordinator`].
#[derive(Debug, Default)]
pub struct NetApprovalCoordinator {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    in_flight: HashMap<String, NetApprovalRequest>,
    active_domains: HashSet<String>,
    session_grants: HashSet<String>,
    session_denies: HashSet<String>,
    resolved: HashSet<String>,
}

fn norm(domain: &str) -> String {
    domain.trim().to_ascii_lowercase()
}

impl NetApprovalCoordinator {
    /// Create an empty coordinator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a decision for `domain`, or return `None` if it should not be
    /// asked: the domain already has a session grant or deny, or a decision
    /// for it is already in flight.
    pub fn begin(&self, domain: &str, target: &str) -> Option<NetApprovalRequest> {
        let domain = norm(domain);
        let mut inner = self.inner.lock();
        if inner.session_grants.contains(&domain)
            || inner.session_denies.contains(&domain)
            || inner.active_domains.contains(&domain)
        {
            return None;
        }
        let decision_id = uuid::Uuid::new_v4().to_string();
        let req = NetApprovalRequest {
            decision_id: decision_id.clone(),
            domain: domain.clone(),
            target: target.to_string(),
        };
        inner.active_domains.insert(domain);
        inner.in_flight.insert(decision_id, req.clone());
        Some(req)
    }

    /// Resolve a decision with the human's answer. First-answer-wins and
    /// idempotent.
    pub fn resolve(&self, decision_id: &str, decision: NetApprovalDecision) -> NetResolveOutcome {
        let mut inner = self.inner.lock();
        if inner.resolved.contains(decision_id) {
            return NetResolveOutcome::AlreadyResolved;
        }
        let Some(req) = inner.in_flight.remove(decision_id) else {
            return NetResolveOutcome::Unknown;
        };
        inner.resolved.insert(decision_id.to_string());
        inner.active_domains.remove(&req.domain);
        let domain = req.domain;

        match decision {
            NetApprovalDecision::Deny => {
                inner.session_denies.insert(domain.clone());
                NetResolveOutcome::Denied { domain }
            }
            NetApprovalDecision::AllowOnce => NetResolveOutcome::AllowOnce { domain },
            NetApprovalDecision::AllowSession => {
                inner.session_grants.insert(domain.clone());
                NetResolveOutcome::AllowSession { domain }
            }
            NetApprovalDecision::AllowAlways => {
                inner.session_grants.insert(domain.clone());
                NetResolveOutcome::Persist { domain }
            }
        }
    }

    /// Cancel a decision without an answer. The domain is *not* added to any
    /// session set, so a future request may re-ask. Returns the request if it
    /// was in flight.
    pub fn cancel(&self, decision_id: &str) -> Option<NetApprovalRequest> {
        let mut inner = self.inner.lock();
        let req = inner.in_flight.remove(decision_id);
        if let Some(r) = &req {
            inner.active_domains.remove(&r.domain);
        }
        inner.resolved.insert(decision_id.to_string());
        req
    }

    /// Whether `domain` was granted for this session.
    pub fn is_session_granted(&self, domain: &str) -> bool {
        self.inner.lock().session_grants.contains(&norm(domain))
    }

    /// Whether `domain` was denied for this session.
    pub fn is_session_denied(&self, domain: &str) -> bool {
        self.inner.lock().session_denies.contains(&norm(domain))
    }

    /// Snapshot of the domains granted for this session.
    pub fn session_grants(&self) -> Vec<String> {
        self.inner.lock().session_grants.iter().cloned().collect()
    }

    /// Snapshot of the domains denied for this session.
    pub fn session_denies(&self) -> Vec<String> {
        self.inner.lock().session_denies.iter().cloned().collect()
    }

    /// Whether `decision_id` is still awaiting an answer.
    pub fn is_in_flight(&self, decision_id: &str) -> bool {
        self.inner.lock().in_flight.contains_key(decision_id)
    }
}

/// Persist an approved domain to `[network].allow` in the settings file — the
/// durable form of a [`NetApprovalDecision::AllowAlways`] answer. Mirrors
/// [`crate::web_approval::persist_web_allow`]: a strict load (so a corrupt
/// settings file is never silently clobbered), a case-insensitive dedup, then
/// save. The settings file lives outside every sandbox scope, so a sandboxed
/// subprocess cannot reach it. Returns `true` if the domain was newly added,
/// `false` if it was already present.
pub fn persist_net_allow(settings_file: &Path, domain: &str) -> Result<bool> {
    let domain = norm(domain);
    let mut settings = AhmaSettings::load_from_result(settings_file)
        .map_err(|e| anyhow::anyhow!(e))
        .with_context(|| {
            format!(
                "refusing to overwrite unparseable {}",
                settings_file.display()
            )
        })?;
    if settings
        .network
        .allow
        .iter()
        .any(|p| p.eq_ignore_ascii_case(&domain))
    {
        return Ok(false);
    }
    settings.network.allow.push(domain);
    settings
        .save_to(settings_file)
        .with_context(|| format!("failed to write {}", settings_file.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coord() -> NetApprovalCoordinator {
        NetApprovalCoordinator::new()
    }

    #[test]
    fn begin_dedups_same_domain_case_insensitively() {
        let c = coord();
        let first = c.begin("Crates.io", "crates.io:443");
        assert!(first.is_some(), "first ask issues a decision");
        assert!(
            c.begin("crates.io", "crates.io:443").is_none(),
            "an in-flight domain is not re-asked"
        );
        assert_eq!(first.unwrap().domain, "crates.io");
    }

    #[test]
    fn allow_session_grants_and_threads_into_snapshot() {
        let c = coord();
        let req = c.begin("ok.example", "ok.example:443").unwrap();
        let out = c.resolve(&req.decision_id, NetApprovalDecision::AllowSession);
        assert_eq!(
            out,
            NetResolveOutcome::AllowSession {
                domain: "ok.example".into()
            }
        );
        assert_eq!(c.session_grants(), vec!["ok.example".to_string()]);
        assert!(c.session_denies().is_empty());
        assert!(c.is_session_granted("OK.Example"));
        assert!(
            c.begin("ok.example", "ok.example:443").is_none(),
            "a session-granted domain is not re-asked"
        );
    }

    #[test]
    fn allow_once_remembers_nothing() {
        let c = coord();
        let req = c.begin("once.example", "once.example:443").unwrap();
        let out = c.resolve(&req.decision_id, NetApprovalDecision::AllowOnce);
        assert_eq!(
            out,
            NetResolveOutcome::AllowOnce {
                domain: "once.example".into()
            }
        );
        assert!(c.session_grants().is_empty());
        assert!(c.session_denies().is_empty());
        assert!(
            c.begin("once.example", "once.example:443").is_some(),
            "allow-once must not suppress a later prompt"
        );
    }

    #[test]
    fn allow_always_grants_session_and_signals_persist() {
        let c = coord();
        let req = c.begin("keep.example", "keep.example:443").unwrap();
        let out = c.resolve(&req.decision_id, NetApprovalDecision::AllowAlways);
        assert_eq!(
            out,
            NetResolveOutcome::Persist {
                domain: "keep.example".into()
            }
        );
        assert_eq!(c.session_grants(), vec!["keep.example".to_string()]);
    }

    #[test]
    fn deny_blocks_and_suppresses_reask() {
        let c = coord();
        let req = c.begin("no.example", "no.example:443").unwrap();
        let out = c.resolve(&req.decision_id, NetApprovalDecision::Deny);
        assert_eq!(
            out,
            NetResolveOutcome::Denied {
                domain: "no.example".into()
            }
        );
        assert_eq!(c.session_denies(), vec!["no.example".to_string()]);
        assert!(c.is_session_denied("no.example"));
        assert!(
            c.begin("no.example", "no.example:443").is_none(),
            "a denied domain is not re-asked"
        );
    }

    #[test]
    fn resolve_is_idempotent_first_answer_wins() {
        let c = coord();
        let req = c.begin("race.example", "race.example:443").unwrap();
        let first = c.resolve(&req.decision_id, NetApprovalDecision::AllowSession);
        assert!(matches!(first, NetResolveOutcome::AllowSession { .. }));
        let second = c.resolve(&req.decision_id, NetApprovalDecision::Deny);
        assert_eq!(second, NetResolveOutcome::AlreadyResolved);
        assert_eq!(c.session_grants(), vec!["race.example".to_string()]);
        assert!(c.session_denies().is_empty());
    }

    #[test]
    fn resolve_unknown_decision_id_is_ignored() {
        let c = coord();
        assert_eq!(
            c.resolve("never-issued", NetApprovalDecision::AllowSession),
            NetResolveOutcome::Unknown
        );
    }

    #[test]
    fn persist_net_allow_adds_then_dedups_case_insensitively() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join(".ahma").join("settings.toml");

        assert!(persist_net_allow(&file, "Crates.IO").unwrap());
        let reloaded = AhmaSettings::load_from_result(&file).unwrap();
        assert_eq!(reloaded.network.allow, vec!["crates.io".to_string()]);

        assert!(!persist_net_allow(&file, "crates.io").unwrap());
        let reloaded = AhmaSettings::load_from_result(&file).unwrap();
        assert_eq!(
            reloaded.network.allow.len(),
            1,
            "domain must not be duplicated"
        );
    }

    #[test]
    fn persist_net_allow_refuses_to_clobber_corrupt_settings() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".ahma");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.toml");
        std::fs::write(&file, "this is : not valid toml [[[").unwrap();
        assert!(
            persist_net_allow(&file, "crates.io").is_err(),
            "a corrupt settings file must not be silently overwritten"
        );
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("not valid toml"),
            "the bad contents are left intact for the human to fix"
        );
    }

    #[test]
    fn cancel_frees_gate_and_blocks_late_answer_without_remembering() {
        let c = coord();
        let req = c.begin("gone.example", "gone.example:443").unwrap();
        let cancelled = c.cancel(&req.decision_id);
        assert_eq!(
            cancelled.as_ref().map(|r| r.decision_id.clone()),
            Some(req.decision_id.clone())
        );
        assert_eq!(
            c.resolve(&req.decision_id, NetApprovalDecision::AllowSession),
            NetResolveOutcome::AlreadyResolved
        );
        assert!(c.session_grants().is_empty());
        assert!(c.session_denies().is_empty());
        assert!(c.begin("gone.example", "gone.example:443").is_some());
    }
}
