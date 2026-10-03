//! SSRF / private-range egress guard for outbound HTTP made by ahma's own
//! tools (currently [`crate::fetch_webpage`]).
//!
//! The filesystem sandbox governs what the agent can read and write locally; it
//! says nothing about what the agent can *send outward*. An unrestricted
//! outbound request from the ahma process — which runs with the user's full
//! network privileges — can reach cloud-metadata endpoints
//! (`169.254.169.254`), loopback admin services (`127.0.0.1:PORT/admin`), or
//! RFC-1918 hosts (a home router) that no browser origin model would allow.
//!
//! This module blocks those at **connection time on the resolved IP**, not on
//! the hostname string, via a custom [`reqwest`] DNS resolver. Because the
//! resolver runs for the initial request *and every redirect hop*, it also
//! resists DNS-rebinding: a domain that flips its DNS to a private address
//! after approval is still blocked when the socket is actually opened. IP
//! *literals* (which bypass DNS) are rejected up front by [`check_url`] and, for
//! redirect targets, by the redirect policy or — on a fetch that follows its own
//! redirects under a [`RedirectDomainGuard`] — by `check_url` again on every hop.
//!
//! `block_private` is a parameter rather than a hard constant so a legitimate
//! dev workflow (or a test hitting a loopback mock server) can opt out — the
//! equivalent of SPEC R-WEB.3.3's `block_private_ranges = false`. Tool code
//! always uses the strict (`true`) default.

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;

use ahma_common::web_policy::WebDecision;
use anyhow::{Result, anyhow};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect;

pub use ahma_common::config::RedirectPolicy;

/// Maximum redirects followed before failing the request.
pub const MAX_REDIRECTS: usize = 10;

/// What to do with one redirect hop (SPEC R-WEB.8). Produced by
/// [`decide_redirect`], the single place the `on_redirect_to_new_domain` rules
/// live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectStep {
    /// Follow the hop.
    Follow,
    /// Refuse the hop; the message is the user-facing reason.
    Refuse(String),
    /// `prompt` mode only: ask a human about `domain` exactly as for a fresh
    /// request to it, and follow the hop iff the answer is yes.
    Ask {
        /// The redirect target's host, lowercased.
        domain: String,
    },
}

/// Decide what to do with a redirect from `origin_host` to `target_host`
/// under `mode` (`[web] on_redirect_to_new_domain`), given `verdict` — what the
/// live `[web]` policy, with this session's grants and denies, decides for the
/// target URL.
///
/// - Same host (case-insensitive, any scheme/port — so `http` → `https` too):
///   always [`RedirectStep::Follow`] (R-WEB.8.3).
/// - `policy`: follow iff `verdict` is `Allow`; otherwise refuse with the
///   `ahma web allow <host>` hint. Never asks.
/// - `block`: refuse every other host, whatever the policy says.
/// - `prompt`: `Allow` follows and `Deny` refuses without asking; an unknown
///   host (`Prompt`, strict `deny` mode) yields [`RedirectStep::Ask`], i.e. the
///   same three-tier approval a fresh request to that host would get.
///
/// Pure: the private-range checks (R-WEB.3 / R-WEB.8.4) are the caller's, on
/// every hop, before and independently of this decision.
pub fn decide_redirect(
    origin_host: &str,
    target_host: &str,
    target_url: &str,
    mode: RedirectPolicy,
    verdict: &WebDecision,
) -> RedirectStep {
    if target_host.eq_ignore_ascii_case(origin_host) {
        return RedirectStep::Follow;
    }
    let host = target_host;
    let not_approved = |why: &str| {
        RedirectStep::Refuse(format!(
            "egress blocked: '{origin_host}' redirected to '{host}' ({target_url}), a different \
             domain than the approved request, and {why} (R-WEB.8). To allow it, run \
             `ahma web allow {host}` and retry."
        ))
    };
    match (mode, verdict) {
        (RedirectPolicy::Block, _) => RedirectStep::Refuse(format!(
            "egress blocked: '{origin_host}' redirected to '{host}' ({target_url}), a different \
             domain, and [web] on_redirect_to_new_domain = \"block\" never follows a redirect \
             to another host (R-WEB.8). Fetch that URL directly if you need it, or set \
             on_redirect_to_new_domain to \"policy\" or \"prompt\" in ~/.ahma/settings.toml."
        )),
        (_, WebDecision::Allow { .. }) => RedirectStep::Follow,
        (_, WebDecision::Deny { reason }) => {
            not_approved(&format!("the [web] policy refuses it: {reason}"))
        }
        (RedirectPolicy::Prompt, WebDecision::Prompt { domain }) => RedirectStep::Ask {
            domain: domain.clone(),
        },
        (RedirectPolicy::Policy, WebDecision::Prompt { .. }) => {
            not_approved("the [web] policy does not approve it")
        }
    }
}

/// The live `[web]` verdict for a redirect target URL (with this session's
/// grants and denies — read at the time of the hop, not when the request began).
pub type RedirectVerdictFn = Arc<dyn Fn(&str) -> WebDecision + Send + Sync>;

/// The answer to one `prompt`-mode approval: `Ok(())` to follow the hop, or
/// `Err(reason)` to fail the request with that reason.
pub type ApproveFuture = Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send>>;

/// `prompt` mode's approval, called with `(domain, url)` of the redirect
/// target. The caller wires it to the same three-tier approval flow a fresh
/// request uses (R-WEB.5–R-WEB.7). Build one with
/// [`RedirectDomainGuard::with_approver`].
pub type RedirectApproveFn = Arc<dyn Fn(String, String) -> ApproveFuture + Send + Sync>;

/// Enforces `[web] on_redirect_to_new_domain` on a fetch (SPEC R-WEB.8).
///
/// The SSRF resolver already blocks any hop that resolves to a private address,
/// but it says nothing about a hop to a *different public domain*: an approved
/// `api.github.com` that returns `302 Location: https://evil.example/` would
/// otherwise be followed, laundering an unapproved domain through an approved
/// one. A fetch given this guard follows redirects itself (automatic redirects
/// are off), so every hop can be decided by [`decide_redirect`] — including the
/// asynchronous approval `prompt` mode needs, which reqwest's synchronous
/// redirect callback could not await.
#[derive(Clone)]
pub struct RedirectDomainGuard {
    mode: RedirectPolicy,
    verdict: RedirectVerdictFn,
    approve: Option<RedirectApproveFn>,
}

impl RedirectDomainGuard {
    /// A guard applying `mode`, with `verdict` giving the live policy decision
    /// for a redirect target URL. Without an approver (see
    /// [`Self::with_approver`]) a hop that `prompt` mode would ask about is
    /// refused with the `ahma web allow` hint, as `policy` mode refuses it.
    pub fn new(mode: RedirectPolicy, verdict: RedirectVerdictFn) -> Self {
        Self {
            mode,
            verdict,
            approve: None,
        }
    }

    /// Attach the approval flow `prompt` mode asks through: `approve(domain,
    /// url)` resolves to `Ok(())` to follow the hop or `Err(reason)` to refuse.
    pub fn with_approver<F, Fut>(mut self, approve: F) -> Self
    where
        F: Fn(String, String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), String>> + Send + 'static,
    {
        self.approve = Some(Arc::new(
            move |domain: String, url: String| -> ApproveFuture { Box::pin(approve(domain, url)) },
        ));
        self
    }

    /// The configured mode.
    pub fn mode(&self) -> RedirectPolicy {
        self.mode
    }

    /// Decide one hop from `origin_host` to `target`: [`decide_redirect`] with
    /// the live verdict for `target`.
    pub fn decide(&self, origin_host: &str, target: &reqwest::Url) -> RedirectStep {
        let target_host = target.host_str().unwrap_or_default();
        let verdict = (self.verdict)(target.as_str());
        decide_redirect(
            origin_host,
            target_host,
            target.as_str(),
            self.mode,
            &verdict,
        )
    }

    /// Resolve a [`RedirectStep::Ask`] through the approver. `Ok(())` means
    /// approved; with no approver attached nothing can ask, so it is refused.
    pub async fn ask(&self, domain: &str, url: &str) -> std::result::Result<(), String> {
        match &self.approve {
            Some(approve) => approve(domain.to_string(), url.to_string()).await,
            None => Err(format!(
                "egress blocked: a redirect went to '{domain}' ({url}), which is not approved, \
                 and no approval prompt is available (R-WEB.8). To allow it, run \
                 `ahma web allow {domain}` and retry."
            )),
        }
    }
}

impl std::fmt::Debug for RedirectDomainGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedirectDomainGuard")
            .field("mode", &self.mode)
            .field("approver", &self.approve.is_some())
            .finish_non_exhaustive()
    }
}

/// Return `true` if `ip` is in a range that outbound tool requests must never
/// reach: loopback, RFC-1918 private, link-local / cloud-metadata
/// (`169.254.0.0/16`), CGNAT (`100.64.0.0/10`), unspecified, broadcast,
/// documentation, multicast, IPv6 unique-local (`fc00::/7`) and link-local
/// (`fe80::/10`), and any of the above smuggled through an IPv4-mapped IPv6
/// address (`::ffff:a.b.c.d`).
pub fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                // 100.64.0.0/10 carrier-grade NAT
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_ip(&IpAddr::V4(mapped));
            }
            let seg0 = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fc00::/7 unique-local
                || (seg0 & 0xfe00) == 0xfc00
                // fe80::/10 link-local
                || (seg0 & 0xffc0) == 0xfe80
        }
    }
}

/// The guard refused a destination. Typed so a caller can tell a policy
/// refusal from a network failure anywhere in a `reqwest` error's source
/// chain — the refusal must never be retried or reported as an outage.
#[derive(Debug)]
pub struct EgressBlocked(String);

impl std::fmt::Display for EgressBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EgressBlocked {}

impl EgressBlocked {
    /// A refusal with this user-facing message.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Whether `error` was caused by the guard refusing its destination.
pub fn is_egress_blocked(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(e) = current {
        if e.is::<EgressBlocked>() {
            return true;
        }
        current = e.source();
    }
    false
}

/// A [`reqwest`] DNS resolver that drops blocked addresses from every
/// resolution. If a name resolves *only* to blocked addresses the resolution
/// fails, so the connection is never attempted.
struct GuardedResolver {
    block_private: bool,
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let block_private = self.block_private;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let resolved: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if !block_private {
                let all: Addrs = Box::new(resolved.into_iter());
                return Ok(all);
            }
            let allowed: Vec<_> = resolved
                .into_iter()
                .filter(|a| !is_blocked_ip(&a.ip()))
                .collect();
            if allowed.is_empty() {
                return Err(Box::new(EgressBlocked(format!(
                    "egress blocked: '{host}' resolves only to private/loopback/link-local \
                     addresses (SSRF protection)"
                )))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            let out: Addrs = Box::new(allowed.into_iter());
            Ok(out)
        })
    }
}

/// Redirect policy for a fetch **without** a [`RedirectDomainGuard`]: cap the
/// chain and reject any redirect whose target host is a blocked IP *literal*
/// (hostname targets are re-checked by the resolver). A guarded fetch turns
/// automatic redirects off and applies the same checks per hop itself.
fn redirect_policy(block_private: bool) -> redirect::Policy {
    redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error(anyhow!("too many redirects (>{MAX_REDIRECTS})"));
        }
        // Copy any blocked IP-literal target out of the borrowed `attempt` before
        // moving it into `error()`/`follow()`.
        let blocked = block_private
            .then(|| attempt.url().host_str())
            .flatten()
            .and_then(|h| h.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .filter(is_blocked_ip);
        if let Some(ip) = blocked {
            return attempt.error(anyhow!(
                "egress blocked: redirect to private/loopback address {ip} (SSRF protection)"
            ));
        }
        attempt.follow()
    })
}

/// Reject a URL before any connection is made: only `http`/`https` are allowed,
/// and a blocked IP *literal* host fails fast (DNS-based hosts are enforced by
/// the resolver at connect time).
pub fn check_url(url: &str, block_private: bool) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|e| anyhow!("invalid URL '{url}': {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(anyhow!(
                "unsupported URL scheme '{other}' (only http/https)"
            ));
        }
    }
    if block_private
        && let Some(host) = parsed.host_str()
        && let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>()
        && is_blocked_ip(&ip)
    {
        return Err(anyhow!(
            "egress blocked: '{host}' is a private/loopback/link-local address (SSRF protection)"
        ));
    }
    Ok(())
}

/// Build a [`reqwest::Client`] whose DNS resolution is guarded against SSRF on
/// every connection. With `follow_redirects` it follows redirects itself under
/// `redirect_policy`; without, a 3xx is returned to the caller, which must
/// follow it under a [`RedirectDomainGuard`] (re-running [`check_url`] on each
/// hop). `block_private` should be `true` for all tool code.
pub fn guarded_client(
    block_private: bool,
    follow_redirects: bool,
) -> reqwest::Result<reqwest::Client> {
    let policy = if follow_redirects {
        redirect_policy(block_private)
    } else {
        redirect::Policy::none()
    };
    reqwest::Client::builder()
        .redirect(policy)
        .dns_resolver(Arc::new(GuardedResolver { block_private }))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn ip(s: &str) -> IpAddr {
        IpAddr::from_str(s).unwrap()
    }

    #[test]
    fn blocks_ssrf_ranges() {
        for s in [
            "127.0.0.1",
            "127.9.9.9",
            "169.254.169.254", // AWS/GCP metadata
            "10.0.0.1",
            "172.16.5.4",
            "192.168.1.1",
            "0.0.0.0",
            "100.64.0.1", // CGNAT
            "::1",
            "fe80::1",          // link-local
            "fc00::1",          // unique-local
            "::ffff:127.0.0.1", // IPv4-mapped loopback
            "::ffff:169.254.169.254",
        ] {
            assert!(is_blocked_ip(&ip(s)), "{s} must be blocked");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for s in ["8.8.8.8", "1.1.1.1", "140.82.112.3", "2606:4700:4700::1111"] {
            assert!(!is_blocked_ip(&ip(s)), "{s} must be allowed");
        }
    }

    #[test]
    fn check_url_rejects_private_ip_literals() {
        for u in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:8080/admin",
            "http://[::1]:9000/",
            "https://10.1.2.3/",
        ] {
            assert!(check_url(u, true).is_err(), "{u} must be rejected");
        }
    }

    #[test]
    fn check_url_allows_public_and_hostnames() {
        for u in [
            "https://api.github.com/x",
            "http://example.com/",
            "https://8.8.8.8/",
        ] {
            assert!(check_url(u, true).is_ok(), "{u} should pass pre-check");
        }
    }

    #[test]
    fn check_url_rejects_non_http_schemes() {
        for u in ["file:///etc/passwd", "ftp://example.com/x", "gopher://x/"] {
            assert!(check_url(u, true).is_err(), "{u} must be rejected");
        }
    }

    #[test]
    fn check_url_permissive_mode_allows_loopback() {
        assert!(check_url("http://127.0.0.1:8080/", false).is_ok());
    }

    #[test]
    fn guarded_client_builds() {
        assert!(guarded_client(true, true).is_ok());
        assert!(guarded_client(false, true).is_ok());
        assert!(guarded_client(true, false).is_ok());
    }

    fn allow() -> WebDecision {
        WebDecision::Allow {
            matched: "always_allow".into(),
        }
    }
    fn deny() -> WebDecision {
        WebDecision::Deny {
            reason: "'cdn.example' matches never_allow pattern 'cdn.example'".into(),
        }
    }
    fn unknown() -> WebDecision {
        WebDecision::Prompt {
            domain: "cdn.example".into(),
        }
    }

    const MODES: [RedirectPolicy; 3] = [
        RedirectPolicy::Policy,
        RedirectPolicy::Block,
        RedirectPolicy::Prompt,
    ];

    /// R-WEB.8.3: a hop to the origin host is followed in every mode, whatever
    /// the policy would say about it — the host was already approved.
    #[test]
    fn same_host_redirect_follows_in_every_mode() {
        for mode in MODES {
            for verdict in [allow(), deny(), unknown()] {
                assert_eq!(
                    decide_redirect(
                        "api.github.com",
                        "API.GitHub.com",
                        "https://API.GitHub.com/x",
                        mode,
                        &verdict
                    ),
                    RedirectStep::Follow,
                    "{mode:?} / {verdict:?}"
                );
            }
        }
    }

    fn other_host(mode: RedirectPolicy, verdict: WebDecision) -> RedirectStep {
        decide_redirect(
            "api.github.com",
            "cdn.example",
            "https://cdn.example/f",
            mode,
            &verdict,
        )
    }

    fn assert_refused(step: RedirectStep, must_contain: &[&str]) {
        let RedirectStep::Refuse(msg) = step else {
            panic!("expected Refuse, got {step:?}");
        };
        for needle in must_contain {
            assert!(msg.contains(needle), "missing '{needle}' in: {msg}");
        }
    }

    #[test]
    fn policy_mode_follows_iff_the_policy_allows_and_never_asks() {
        assert_eq!(
            other_host(RedirectPolicy::Policy, allow()),
            RedirectStep::Follow
        );
        assert_refused(
            other_host(RedirectPolicy::Policy, deny()),
            &["cdn.example", "never_allow", "ahma web allow cdn.example"],
        );
        assert_refused(
            other_host(RedirectPolicy::Policy, unknown()),
            &[
                "cdn.example",
                "different domain",
                "ahma web allow cdn.example",
            ],
        );
    }

    #[test]
    fn block_mode_refuses_every_other_host_even_an_allowed_one() {
        for verdict in [allow(), deny(), unknown()] {
            assert_refused(
                other_host(RedirectPolicy::Block, verdict),
                &[
                    "cdn.example",
                    "https://cdn.example/f",
                    "on_redirect_to_new_domain = \"block\"",
                ],
            );
        }
    }

    #[test]
    fn prompt_mode_asks_only_when_a_fresh_request_would() {
        assert_eq!(
            other_host(RedirectPolicy::Prompt, allow()),
            RedirectStep::Follow
        );
        assert_refused(
            other_host(RedirectPolicy::Prompt, deny()),
            &["cdn.example", "never_allow"],
        );
        assert_eq!(
            other_host(RedirectPolicy::Prompt, unknown()),
            RedirectStep::Ask {
                domain: "cdn.example".into()
            }
        );
    }

    #[test]
    fn guard_decides_with_the_live_verdict_for_the_target_url() {
        // The verdict closure sees the full target URL (scheme and port included,
        // so a scheme/port-qualified always_allow pattern can match).
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen2 = Arc::clone(&seen);
        let guard = RedirectDomainGuard::new(
            RedirectPolicy::Policy,
            Arc::new(move |url: &str| {
                seen2.lock().unwrap().push(url.to_string());
                if url.starts_with("https://codeload.github.com") {
                    allow()
                } else {
                    unknown()
                }
            }),
        );
        let ok = reqwest::Url::parse("https://codeload.github.com:8443/a").unwrap();
        let bad = reqwest::Url::parse("https://evil.example/").unwrap();
        assert_eq!(guard.decide("api.github.com", &ok), RedirectStep::Follow);
        assert!(matches!(
            guard.decide("api.github.com", &bad),
            RedirectStep::Refuse(_)
        ));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "https://codeload.github.com:8443/a".to_string(),
                "https://evil.example/".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn guard_without_approver_refuses_an_ask_with_the_hint() {
        let guard = RedirectDomainGuard::new(RedirectPolicy::Prompt, Arc::new(|_: &str| unknown()));
        let err = guard
            .ask("cdn.example", "https://cdn.example/f")
            .await
            .unwrap_err();
        assert!(err.contains("ahma web allow cdn.example"), "{err}");
    }
}
