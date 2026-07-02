//! Web-egress domain policy (SPEC §4.6 R-WEB): pattern parsing, matching, and
//! the allow/deny/prompt decision for outbound HTTP made by ahma's own tools.
//!
//! This module is **pure**: it decides *what* a request should do given the
//! configured policy and any in-session grants/denies. It does not perform the
//! request, prompt a human, or touch the network — those live in the wiring
//! layer. Private-range/SSRF blocking is enforced separately at connection time
//! by the egress guard and is *always on* regardless of this policy.
//!
//! ## Pattern syntax (R-WEB.4)
//!
//! ```text
//! pattern = [scheme "://"] domain [":" port]
//! scheme  = "https" | "http"
//! domain  = exact | "*." hostname     // single-level wildcard only
//! ```
//!
//! - `api.github.com` matches `api.github.com` only (not `github.com`, not
//!   `raw.github.com`, not `deep.api.github.com`).
//! - `*.github.com` matches exactly one label deep (`api.github.com`,
//!   `raw.github.com`) but not `github.com` or `deep.api.github.com`.
//! - A bare `*`, a TLD-level wildcard (`*.com`), or a private-IP / `localhost`
//!   literal is rejected at parse time.
//! - Host matching is case-insensitive; scheme is case-sensitive.

use std::net::IpAddr;
use std::str::FromStr;

use crate::config::{WebDefaultPolicy, WebSettings};

/// The outcome of evaluating a URL against the web policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebDecision {
    /// Permit the request without prompting. `matched` names the rule that
    /// allowed it (a pattern, `"session"`, or `"default-allow"`).
    Allow { matched: String },
    /// Block the request. `reason` is a short human-readable cause.
    Deny { reason: String },
    /// Hold the request and raise a human approval prompt (strict `deny` mode,
    /// unknown domain). `domain` is what would be approved.
    Prompt { domain: String },
}

impl WebDecision {
    /// The audit-log token for this decision.
    pub fn label(&self) -> &'static str {
        match self {
            WebDecision::Allow { .. } => "allow",
            WebDecision::Deny { .. } => "deny",
            WebDecision::Prompt { .. } => "prompt",
        }
    }
}

/// A parsed, validated domain pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebPattern {
    scheme: Option<String>,
    host: HostMatch,
    port: Option<u16>,
    /// The original text, preserved for display and audit.
    raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostMatch {
    /// Matches this exact hostname (case-insensitive).
    Exact(String),
    /// `*.parent` — matches exactly one label followed by `.parent`.
    Wildcard { parent: String },
}

/// Why a pattern string was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternError(pub String);

impl std::fmt::Display for PatternError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PatternError {}

impl WebPattern {
    /// Parse and validate a pattern (R-WEB.4). Rejects bare `*`, TLD-level
    /// wildcards, and private-IP / `localhost` literals.
    pub fn parse(input: &str) -> Result<Self, PatternError> {
        let raw = input.trim().to_string();
        if raw.is_empty() {
            return Err(PatternError("empty pattern".into()));
        }

        // Optional scheme.
        let (scheme, rest) = match raw.split_once("://") {
            Some((s, r)) => {
                let s = s.to_string();
                if s != "http" && s != "https" {
                    return Err(PatternError(format!("unsupported scheme '{s}'")));
                }
                (Some(s), r.to_string())
            }
            None => (None, raw.clone()),
        };

        // Reject IPv6 literals (bracketed or not) up front: they contain two or
        // more colons, which would also confuse port splitting. Web patterns are
        // domain-only, and private-range blocking governs IPs at connect time.
        if rest.matches(':').count() >= 2 {
            return Err(PatternError(format!(
                "'{rest}' looks like an IP literal; web patterns are domain-only"
            )));
        }

        // Optional port. A single ':' is unambiguously a port separator now.
        let (host_str, port) = match rest.rsplit_once(':') {
            Some((h, p)) => {
                let port = p
                    .parse::<u16>()
                    .map_err(|_| PatternError(format!("invalid port '{p}'")))?;
                (h.to_string(), Some(port))
            }
            None => (rest.clone(), None),
        };

        if host_str.is_empty() {
            return Err(PatternError("missing host".into()));
        }
        // Reject a bare `*` and TLD-level wildcards early.
        if host_str == "*" {
            return Err(PatternError("bare '*' is not allowed".into()));
        }

        let host = if let Some(parent) = host_str.strip_prefix("*.") {
            if parent.is_empty() || !parent.contains('.') {
                // `*.com` (TLD-level) or `*.` — too broad.
                return Err(PatternError(format!(
                    "'{host_str}' is too broad; a wildcard needs at least two labels (e.g. *.github.com)"
                )));
            }
            HostMatch::Wildcard {
                parent: parent.to_ascii_lowercase(),
            }
        } else {
            // Reject IP-literal and localhost hosts — SSRF surface, and
            // private-range blocking already governs them at connect time.
            if host_str.eq_ignore_ascii_case("localhost") {
                return Err(PatternError(
                    "'localhost' is not a valid web pattern".into(),
                ));
            }
            if IpAddr::from_str(host_str.trim_matches(['[', ']'])).is_ok() {
                return Err(PatternError(format!(
                    "'{host_str}' is an IP literal; web patterns are domain-only"
                )));
            }
            HostMatch::Exact(host_str.to_ascii_lowercase())
        };

        Ok(WebPattern {
            scheme,
            host,
            port,
            raw,
        })
    }

    /// `true` if this is an `http://`-scheme pattern (cleartext; callers may warn).
    pub fn is_cleartext(&self) -> bool {
        self.scheme.as_deref() == Some("http")
    }

    /// The original pattern text.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Does this pattern match the given request coordinates? `scheme` is
    /// case-sensitive; `host` is matched case-insensitively.
    pub fn matches(&self, scheme: &str, host: &str, port: u16) -> bool {
        if let Some(s) = &self.scheme
            && s != scheme
        {
            return false;
        }
        if let Some(p) = self.port
            && p != port
        {
            return false;
        }
        let host = host.to_ascii_lowercase();
        match &self.host {
            HostMatch::Exact(h) => *h == host,
            HostMatch::Wildcard { parent } => {
                // Exactly one label in front of `.parent`.
                match host.strip_suffix(parent) {
                    Some(prefix) => {
                        // prefix must be "<label>." with no interior dots.
                        prefix.ends_with('.') && {
                            let label = &prefix[..prefix.len() - 1];
                            !label.is_empty() && !label.contains('.')
                        }
                    }
                    None => false,
                }
            }
        }
    }
}

/// Split a URL into `(scheme, host, port)` for matching. Returns `None` if the
/// URL has no host or an unsupported scheme.
pub fn url_coordinates(url: &str) -> Option<(String, String, u16)> {
    let parsed = url::Url::parse(url).ok()?;
    let scheme = parsed.scheme().to_string();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = parsed.host_str()?.to_string();
    let port = parsed
        .port()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    Some((scheme, host, port))
}

/// The compiled web policy: the parsed `always_allow`/`never_allow` lists plus
/// the default policy. Session grants/denies are supplied per-decision so this
/// stays immutable and cheap to share.
#[derive(Debug, Clone)]
pub struct WebPolicy {
    default_policy: WebDefaultPolicy,
    always_allow: Vec<WebPattern>,
    never_allow: Vec<WebPattern>,
}

impl WebPolicy {
    /// Build from settings, discarding (and returning) any invalid patterns so
    /// the caller can warn without failing startup.
    pub fn from_settings(web: &WebSettings) -> (Self, Vec<PatternError>) {
        let mut errors = Vec::new();
        let compile = |list: &[String], errors: &mut Vec<PatternError>| -> Vec<WebPattern> {
            list.iter()
                .filter_map(|p| match WebPattern::parse(p) {
                    Ok(pat) => Some(pat),
                    Err(e) => {
                        errors.push(PatternError(format!("{}: {}", p, e.0)));
                        None
                    }
                })
                .collect()
        };
        let always_allow = compile(&web.always_allow, &mut errors);
        let never_allow = compile(&web.never_allow, &mut errors);
        (
            WebPolicy {
                default_policy: web.default_policy,
                always_allow,
                never_allow,
            },
            errors,
        )
    }

    /// Decide what to do with `url`, given the domains already granted or denied
    /// this session. Order (R-WEB.2): `never_allow` blocks first (cannot be
    /// overridden), then `always_allow`, then session grant, then session deny,
    /// then the default policy.
    pub fn decide(
        &self,
        url: &str,
        session_grants: &[String],
        session_denies: &[String],
    ) -> WebDecision {
        let Some((scheme, host, port)) = url_coordinates(url) else {
            return WebDecision::Deny {
                reason: "unsupported or malformed URL (only http/https with a host)".into(),
            };
        };

        if let Some(p) = self
            .never_allow
            .iter()
            .find(|p| p.matches(&scheme, &host, port))
        {
            return WebDecision::Deny {
                reason: format!("'{host}' matches never_allow pattern '{}'", p.as_str()),
            };
        }
        if let Some(p) = self
            .always_allow
            .iter()
            .find(|p| p.matches(&scheme, &host, port))
        {
            return WebDecision::Allow {
                matched: p.as_str().to_string(),
            };
        }
        let host_lc = host.to_ascii_lowercase();
        if session_grants
            .iter()
            .any(|g| g.eq_ignore_ascii_case(&host_lc))
        {
            return WebDecision::Allow {
                matched: "session".into(),
            };
        }
        if session_denies
            .iter()
            .any(|d| d.eq_ignore_ascii_case(&host_lc))
        {
            return WebDecision::Deny {
                reason: format!("'{host}' was denied earlier this session"),
            };
        }
        match self.default_policy {
            WebDefaultPolicy::Allow => WebDecision::Allow {
                matched: "default-allow".into(),
            },
            WebDefaultPolicy::Deny => WebDecision::Prompt { domain: host_lc },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pat: &str, url: &str) -> bool {
        let (s, h, p) = url_coordinates(url).unwrap();
        WebPattern::parse(pat).unwrap().matches(&s, &h, p)
    }

    #[test]
    fn exact_matches_only_itself() {
        assert!(m("api.github.com", "https://api.github.com/x"));
        assert!(!m("api.github.com", "https://github.com/x"));
        assert!(!m("api.github.com", "https://raw.api.github.com/x"));
        assert!(!m("github.com", "https://api.github.com/x"));
    }

    #[test]
    fn wildcard_is_single_level() {
        assert!(m("*.github.com", "https://api.github.com/x"));
        assert!(m("*.github.com", "https://raw.github.com/x"));
        assert!(!m("*.github.com", "https://github.com/x")); // no label
        assert!(!m("*.github.com", "https://deep.api.github.com/x")); // two labels
    }

    #[test]
    fn scheme_and_port_qualifiers() {
        assert!(m("https://api.github.com", "https://api.github.com/x"));
        assert!(!m("https://api.github.com", "http://api.github.com/x"));
        assert!(m("api.github.com:8080", "http://api.github.com:8080/x"));
        assert!(!m("api.github.com:8080", "http://api.github.com/x")); // port 80
    }

    #[test]
    fn host_match_is_case_insensitive() {
        assert!(m("API.GitHub.com", "https://api.github.com/x"));
        assert!(m("*.GitHub.com", "https://API.github.com/x"));
    }

    #[test]
    fn rejects_broad_and_literal_patterns() {
        for bad in [
            "*",
            "*.com",
            "*.",
            "localhost",
            "127.0.0.1",
            "10.0.0.1",
            "::1",
            "[::1]",
        ] {
            assert!(WebPattern::parse(bad).is_err(), "'{bad}' must be rejected");
        }
    }

    #[test]
    fn accepts_valid_patterns() {
        for good in [
            "api.github.com",
            "*.github.com",
            "https://api.github.com",
            "http://dev.local.test",
            "api.github.com:8080",
        ] {
            assert!(WebPattern::parse(good).is_ok(), "'{good}' should parse");
        }
    }

    fn policy(default_deny: bool, allow: &[&str], never: &[&str]) -> WebPolicy {
        let web = WebSettings {
            default_policy: if default_deny {
                WebDefaultPolicy::Deny
            } else {
                WebDefaultPolicy::Allow
            },
            always_allow: allow.iter().map(|s| s.to_string()).collect(),
            never_allow: never.iter().map(|s| s.to_string()).collect(),
            ..WebSettings::default()
        };
        let (p, errs) = WebPolicy::from_settings(&web);
        assert!(errs.is_empty(), "unexpected pattern errors: {errs:?}");
        p
    }

    #[test]
    fn never_allow_beats_everything() {
        let p = policy(false, &["*.github.com"], &["api.github.com"]);
        // never_allow wins even though always_allow would match and default=allow.
        assert!(matches!(
            p.decide("https://api.github.com/x", &["api.github.com".into()], &[]),
            WebDecision::Deny { .. }
        ));
    }

    #[test]
    fn allow_mode_passes_unknown_domains() {
        let p = policy(false, &[], &[]);
        assert!(matches!(
            p.decide("https://anything.example/x", &[], &[]),
            WebDecision::Allow { .. }
        ));
    }

    #[test]
    fn deny_mode_prompts_for_unknown_and_allows_known() {
        let p = policy(true, &["api.github.com"], &[]);
        assert!(matches!(
            p.decide("https://api.github.com/x", &[], &[]),
            WebDecision::Allow { .. }
        ));
        match p.decide("https://unknown.example/x", &[], &[]) {
            WebDecision::Prompt { domain } => assert_eq!(domain, "unknown.example"),
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn deny_mode_honors_session_grant_and_deny() {
        let p = policy(true, &[], &[]);
        assert!(matches!(
            p.decide("https://ok.example/x", &["ok.example".into()], &[]),
            WebDecision::Allow { matched } if matched == "session"
        ));
        assert!(matches!(
            p.decide("https://no.example/x", &[], &["no.example".into()]),
            WebDecision::Deny { .. }
        ));
    }

    #[test]
    fn malformed_url_is_denied() {
        let p = policy(false, &[], &[]);
        assert!(matches!(
            p.decide("ftp://x/", &[], &[]),
            WebDecision::Deny { .. }
        ));
        assert!(matches!(
            p.decide("not a url", &[], &[]),
            WebDecision::Deny { .. }
        ));
    }
}
