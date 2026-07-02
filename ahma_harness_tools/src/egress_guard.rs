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
//! redirect targets, by the redirect policy.
//!
//! `block_private` is a parameter rather than a hard constant so a legitimate
//! dev workflow (or a test hitting a loopback mock server) can opt out — the
//! equivalent of SPEC R-WEB.3.3's `block_private_ranges = false`. Tool code
//! always uses the strict (`true`) default.

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect;

/// Maximum redirects followed before failing the request.
const MAX_REDIRECTS: usize = 10;

/// Decides whether a **cross-domain** redirect hop may be followed (SPEC R-WEB.8).
///
/// The SSRF resolver already blocks any hop that resolves to a private address,
/// but it says nothing about a hop to a *different public domain*: an approved
/// `api.github.com` that returns `302 Location: https://evil.example/` would
/// otherwise be followed, laundering an unapproved domain through an approved
/// one. This guard closes that hole. The `origin_host` (the host of the URL the
/// tool actually requested) is always permitted; every other host is consulted
/// through `allow`, which the caller wires to the live `[web]` policy so a
/// redirect target is followed only if the policy would independently approve it.
#[derive(Clone)]
pub struct RedirectDomainGuard {
    origin_host: String,
    allow: Arc<dyn Fn(&str) -> bool + Send + Sync>,
}

impl RedirectDomainGuard {
    /// Build a guard for a request whose original host is `origin_host`. `allow`
    /// returns `true` for any other host the caller's policy independently
    /// approves as a redirect target.
    pub fn new(
        origin_host: impl Into<String>,
        allow: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> Self {
        Self {
            origin_host: origin_host.into(),
            allow,
        }
    }

    /// Whether a redirect to `host` may be followed: the origin host itself
    /// (case-insensitive) always may; any other host must be approved by `allow`.
    fn permits(&self, host: &str) -> bool {
        host.eq_ignore_ascii_case(&self.origin_host) || (self.allow)(host)
    }
}

impl std::fmt::Debug for RedirectDomainGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedirectDomainGuard")
            .field("origin_host", &self.origin_host)
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
                return Err(Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "egress blocked: '{host}' resolves only to private/loopback/link-local \
                     addresses (SSRF protection)"
                )));
            }
            let out: Addrs = Box::new(allowed.into_iter());
            Ok(out)
        })
    }
}

/// Redirect policy: cap the chain, reject any redirect whose target host is a
/// blocked IP *literal* (hostname targets are re-checked by the resolver), and —
/// when a [`RedirectDomainGuard`] is supplied — reject a cross-domain hop to a
/// host the `[web]` policy would not approve (R-WEB.8).
fn redirect_policy(
    block_private: bool,
    domain_guard: Option<RedirectDomainGuard>,
) -> redirect::Policy {
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
        // R-WEB.8: a redirect to a *new* domain does not inherit the source
        // domain's approval. We cannot raise an interactive prompt inside this
        // synchronous callback, so an unapproved cross-domain hop is refused with
        // an actionable message rather than followed.
        if let Some(guard) = &domain_guard
            && let Some(host) = attempt.url().host_str()
            && !guard.permits(host)
        {
            let host = host.to_string();
            return attempt.error(anyhow!(
                "egress blocked: redirect to '{host}' is a different domain than the approved \
                 request and the [web] policy does not approve it (R-WEB.8). To allow it, run \
                 `ahma web allow {host}` and retry."
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

/// Build a [`reqwest::Client`] whose DNS resolution and redirect handling are
/// guarded against SSRF, and — when `domain_guard` is set — against cross-domain
/// redirect laundering (R-WEB.8). `block_private` should be `true` for all tool
/// code.
pub fn guarded_client(
    block_private: bool,
    domain_guard: Option<RedirectDomainGuard>,
) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(redirect_policy(block_private, domain_guard))
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
        assert!(guarded_client(true, None).is_ok());
        assert!(guarded_client(false, None).is_ok());
        let guard = RedirectDomainGuard::new("api.github.com", Arc::new(|_| false));
        assert!(guarded_client(true, Some(guard)).is_ok());
    }

    #[test]
    fn redirect_domain_guard_permits_origin_and_allowed_hosts_only() {
        // `allow` approves exactly one extra host; everything else is refused.
        let guard = RedirectDomainGuard::new(
            "api.github.com",
            Arc::new(|h: &str| h == "codeload.github.com"),
        );
        // Origin host is always permitted, case-insensitively.
        assert!(guard.permits("api.github.com"));
        assert!(guard.permits("API.GitHub.com"));
        // A host the policy approves is permitted.
        assert!(guard.permits("codeload.github.com"));
        // An unapproved cross-domain target is refused.
        assert!(!guard.permits("evil.example"));
        assert!(!guard.permits("github.com"));
    }
}
