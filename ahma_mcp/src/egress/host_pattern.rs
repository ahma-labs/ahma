//! The one host matcher.
//!
//! Every place ahma decides "may a subprocess reach this hostname?" resolves to
//! [`HostPattern::matches`]. There is deliberately exactly one implementation,
//! because the failure mode of having two is not a wrong answer somewhere — it is
//! a *quietly divergent* answer in the surface nobody tested.
//!
//! ## Semantics, stated precisely
//!
//! A pattern is one of three forms. Both the pattern and the candidate are
//! normalised first (see [`normalize`]): ASCII-lowercased, with a single trailing
//! root dot removed.
//!
//! | Form | Matches | Does **not** match |
//! |---|---|---|
//! | `crates.io` (exact) | `crates.io`, `CRATES.IO`, `crates.io.` | `index.crates.io`, `evilcrates.io`, `crates.io.evil.com` |
//! | `*.crates.io` (wildcard) | `index.crates.io`, `static.crates.io` | `crates.io`, `a.b.crates.io`, `evilcrates.io` |
//! | `*` (any) | everything | — |
//!
//! Three properties are load-bearing and each has a test:
//!
//! * **A wildcard covers exactly one label.** `*.crates.io` does not reach
//!   `a.b.crates.io`. Depth is where a subdomain-takeover on some forgotten
//!   third-level name turns into egress, so widening it must be a decision
//!   someone writes down, not a side effect of the matcher being lenient.
//! * **A wildcard does not cover its own base.** `*.crates.io` does not match
//!   `crates.io`. List both if you mean both.
//! * **Matching is label-boundary-anchored, never a suffix test.** The classic
//!   bug in this exact code is `candidate.ends_with(suffix)`, which happily lets
//!   `evilcrates.io` satisfy a rule written for `crates.io` — an attacker
//!   registers the concatenation and the allowlist hands them the traffic.
//!   [`wildcard_must_not_match_a_concatenated_lookalike`] is the regression test.
//!
//! ## Non-ASCII is rejected, not transformed
//!
//! A pattern containing non-ASCII bytes fails to parse, and a candidate
//! containing them never matches anything. ahma does **not** perform IDNA/UTS-46
//! conversion here, for the reason that makes homograph attacks work: `аpple.com`
//! (Cyrillic а) and `apple.com` are different hosts that render identically, so a
//! matcher that silently normalised one into the other would be deciding, on the
//! reader's behalf, that two visually-equal strings are the same host.
//!
//! Internationalised domains are still reachable — write the A-label
//! (`xn--80ak6aa92e.com`) that DNS actually carries. That is more typing and it is
//! the point: the allowlist then says exactly what it permits.

use std::fmt;

/// A parsed hostname rule.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HostPattern {
    /// Matches exactly this host.
    Exact(String),
    /// `*.suffix` — matches one, and only one, additional label above `suffix`.
    Wildcard(String),
    /// `*` — matches every host. Never produced by a shipped profile
    /// ([`HostPattern::parse_profile_host`] rejects it); an operator writing it
    /// into `[network] allow` is opting out of the restriction entirely.
    Any,
}

/// Why a host string was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPatternError {
    /// Empty, or only whitespace.
    Empty,
    /// Contains a byte outside ASCII. See the module docs: write punycode.
    NonAscii,
    /// Contains a character that cannot appear in a hostname (a scheme, path,
    /// port, credentials, or whitespace — usually a URL pasted where a host
    /// belongs).
    NotAHostname(char),
    /// An empty label: a leading dot, a trailing dot beyond the single root dot,
    /// or `..` inside.
    EmptyLabel,
    /// `*` used where a blanket grant is not permitted.
    BlanketNotAllowed,
    /// A `*` somewhere other than as the whole pattern or its leading label —
    /// `a*.b.com` and `*.*.b.com` are not supported and are refused rather than
    /// silently read as something else.
    MisplacedWildcard,
}

impl fmt::Display for HostPatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty host pattern"),
            Self::NonAscii => write!(
                f,
                "non-ASCII host pattern; write the punycode A-label instead \
                 (e.g. `xn--80ak6aa92e.com`) so the rule says exactly what it permits"
            ),
            Self::NotAHostname(c) => write!(
                f,
                "'{c}' cannot appear in a hostname — write the bare host \
                 (`registry.npmjs.org`), not a URL, port, or path"
            ),
            Self::EmptyLabel => write!(f, "empty label in host pattern (a stray or doubled '.')"),
            Self::BlanketNotAllowed => write!(
                f,
                "`*` (all hosts) is not permitted here — name the hosts explicitly"
            ),
            Self::MisplacedWildcard => write!(
                f,
                "`*` is only supported as the whole pattern or as the leading label (`*.example.com`)"
            ),
        }
    }
}

impl std::error::Error for HostPatternError {}

/// Normalise a host or pattern to its comparable form: ASCII-lowercased, with a
/// single trailing root dot removed (`crates.io.` and `crates.io` are the same
/// name; only the wire format differs).
///
/// Returns `Err` for anything that is not a hostname at all, so a caller never
/// has to guess whether an odd string was rejected or silently accepted.
fn normalize(raw: &str) -> Result<String, HostPatternError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(HostPatternError::Empty);
    }
    if !trimmed.is_ascii() {
        return Err(HostPatternError::NonAscii);
    }
    // One trailing root dot is the FQDN spelling of the same name; more than one
    // is an empty label and is caught below.
    let stripped = trimmed.strip_suffix('.').unwrap_or(trimmed);
    if stripped.is_empty() {
        return Err(HostPatternError::Empty);
    }
    for c in stripped.chars() {
        // Permit the wildcard char here; placement is validated by the parser,
        // which is the only thing that knows whether a wildcard is legal.
        let ok = c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '*');
        if !ok {
            return Err(HostPatternError::NotAHostname(c));
        }
    }
    if stripped.split('.').any(str::is_empty) {
        return Err(HostPatternError::EmptyLabel);
    }
    Ok(stripped.to_ascii_lowercase())
}

impl HostPattern {
    /// Parse an operator-supplied pattern (`[network] allow`), where the blanket
    /// `*` is a legitimate — if drastic — thing to write.
    pub fn parse(raw: &str) -> Result<Self, HostPatternError> {
        Self::parse_inner(raw, true)
    }

    /// Parse a pattern from a shipped or contributed **profile**, where `*` is
    /// refused.
    ///
    /// A profile is enabled by default (R-PERM.5), so a profile that could write
    /// `*` would be a default-on blanket egress grant arriving through a data
    /// file — precisely the invisible, unrefusable grant the profile system was
    /// built to eliminate. Widening to everything stays a decision the operator
    /// makes in their own settings file, under their own name.
    pub fn parse_profile_host(raw: &str) -> Result<Self, HostPatternError> {
        Self::parse_inner(raw, false)
    }

    fn parse_inner(raw: &str, allow_blanket: bool) -> Result<Self, HostPatternError> {
        let host = normalize(raw)?;
        if host == "*" {
            return if allow_blanket {
                Ok(Self::Any)
            } else {
                Err(HostPatternError::BlanketNotAllowed)
            };
        }
        if let Some(suffix) = host.strip_prefix("*.") {
            if suffix.contains('*') {
                return Err(HostPatternError::MisplacedWildcard);
            }
            return Ok(Self::Wildcard(suffix.to_string()));
        }
        if host.contains('*') {
            return Err(HostPatternError::MisplacedWildcard);
        }
        Ok(Self::Exact(host))
    }

    /// Whether `candidate` — a hostname as it arrived from a subprocess's
    /// CONNECT target or `Host:` header, without a port — is permitted.
    ///
    /// A candidate that is not a well-formed ASCII hostname matches nothing, not
    /// even [`HostPattern::Any`] for the non-ASCII case: the safe direction for
    /// an input we cannot compare honestly is "no".
    pub fn matches(&self, candidate: &str) -> bool {
        let Ok(host) = normalize(candidate) else {
            return false;
        };
        // A candidate is a name, never a pattern. `*` arriving from the wire is
        // not a hostname, so it cannot be made to satisfy a wildcard rule.
        if host.contains('*') {
            return false;
        }
        match self {
            Self::Any => true,
            Self::Exact(want) => &host == want,
            Self::Wildcard(suffix) => match host.strip_suffix(suffix.as_str()) {
                // The `.` check is the whole security property: without it,
                // `evilcrates.io` strips to `evil` and satisfies `*.crates.io`.
                Some(rest) => match rest.strip_suffix('.') {
                    Some(label) => !label.is_empty() && !label.contains('.'),
                    None => false,
                },
                None => false,
            },
        }
    }

    /// The canonical text form, which round-trips through [`HostPattern::parse`].
    pub fn as_str(&self) -> String {
        match self {
            Self::Any => "*".to_string(),
            Self::Exact(h) => h.clone(),
            Self::Wildcard(s) => format!("*.{s}"),
        }
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> HostPattern {
        HostPattern::parse(s).expect("valid pattern")
    }

    #[test]
    fn exact_matches_only_that_host() {
        let rule = p("crates.io");
        assert!(rule.matches("crates.io"));
        assert!(!rule.matches("index.crates.io"), "no implicit subdomains");
        assert!(!rule.matches("io"), "no implicit parents");
    }

    #[test]
    fn matching_is_case_insensitive_in_both_directions() {
        assert!(p("CRATES.IO").matches("crates.io"));
        assert!(p("crates.io").matches("CrAtEs.Io"));
        assert!(p("*.CRATES.IO").matches("Index.Crates.IO"));
    }

    #[test]
    fn a_single_trailing_root_dot_is_the_same_name() {
        // `crates.io.` is how the name appears on the wire; treating it as a
        // different host would be a bypass in one direction and a spurious denial
        // in the other.
        assert!(p("crates.io").matches("crates.io."));
        assert!(p("crates.io.").matches("crates.io"));
        assert!(p("*.crates.io").matches("index.crates.io."));
        // …but a doubled dot is an empty label, not a name.
        assert!(!p("crates.io").matches("crates.io.."));
        assert_eq!(
            HostPattern::parse("crates.io.."),
            Err(HostPatternError::EmptyLabel)
        );
    }

    #[test]
    fn wildcard_covers_exactly_one_label() {
        let rule = p("*.crates.io");
        assert!(rule.matches("index.crates.io"));
        assert!(rule.matches("static.crates.io"));
        assert!(
            !rule.matches("crates.io"),
            "a wildcard does not cover its own base — list both if you mean both"
        );
        assert!(
            !rule.matches("a.b.crates.io"),
            "one label only; deeper names must be named"
        );
    }

    #[test]
    fn wildcard_must_not_match_a_concatenated_lookalike() {
        // THE regression test for this module. A suffix test without a
        // label-boundary check accepts every one of these, and each is a domain
        // an attacker can simply register.
        let rule = p("*.crates.io");
        for imposter in [
            "evilcrates.io",
            "notcrates.io",
            "xcrates.io",
            "my-crates.io",
        ] {
            assert!(
                !rule.matches(imposter),
                "{imposter} must not satisfy a rule written for crates.io"
            );
        }
        // The exact form has the same property.
        assert!(!p("crates.io").matches("evilcrates.io"));
        // And the suffix must be at the *end*: a host that merely contains the
        // allowed name is a different host.
        assert!(!rule.matches("index.crates.io.evil.example"));
        assert!(!p("crates.io").matches("crates.io.evil.example"));
    }

    #[test]
    fn a_bare_label_cannot_satisfy_a_wildcard() {
        // `.crates.io` would strip to an empty label; nothing should match.
        let rule = p("*.crates.io");
        assert!(!rule.matches(".crates.io"));
        assert!(!rule.matches(""));
    }

    #[test]
    fn non_ascii_is_rejected_rather_than_folded() {
        // Homograph defence: `сrates.io` with a Cyrillic 'с' renders identically
        // to the real thing. Refusing to parse it means a reviewer sees an error
        // instead of a rule that looks correct and is not.
        let cyrillic = "\u{441}rates.io"; // U+0441 CYRILLIC SMALL LETTER ES
        assert_eq!(
            HostPattern::parse(cyrillic),
            Err(HostPatternError::NonAscii)
        );
        // …and a non-ASCII candidate matches nothing, including the blanket rule.
        assert!(!p("crates.io").matches(cyrillic));
        assert!(!p("*.crates.io").matches(&format!("index.{cyrillic}")));
        assert!(
            !p("*").matches(cyrillic),
            "an input we cannot compare honestly is denied, not waved through"
        );
        // Punycode is ASCII and works normally — the escape hatch is real.
        assert!(p("xn--80ak6aa92e.com").matches("XN--80AK6AA92E.COM"));
    }

    #[test]
    fn urls_and_ports_are_refused_not_silently_truncated() {
        // Pasting a URL into an allowlist is the common operator mistake. Reading
        // `https://crates.io/api` as the host `https` (or as `crates.io`) both
        // end badly; refusing makes the mistake visible.
        for bad in [
            "https://crates.io",
            "crates.io/api",
            "crates.io:443",
            "user@crates.io",
            "crates io",
        ] {
            assert!(
                matches!(
                    HostPattern::parse(bad),
                    Err(HostPatternError::NotAHostname(_))
                ),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn empty_and_malformed_patterns_are_refused() {
        assert_eq!(HostPattern::parse(""), Err(HostPatternError::Empty));
        assert_eq!(HostPattern::parse("   "), Err(HostPatternError::Empty));
        assert_eq!(HostPattern::parse("."), Err(HostPatternError::Empty));
        assert_eq!(
            HostPattern::parse(".crates.io"),
            Err(HostPatternError::EmptyLabel)
        );
        assert_eq!(
            HostPattern::parse("a..b"),
            Err(HostPatternError::EmptyLabel)
        );
    }

    #[test]
    fn wildcards_are_only_supported_in_the_leading_label() {
        // Refusing is the point: `api*.example.com` silently parsed as an exact
        // host would never match anything, and the operator would believe it did.
        for bad in ["a*.example.com", "*.*.example.com", "example.*", "ex*mple"] {
            assert_eq!(
                HostPattern::parse(bad),
                Err(HostPatternError::MisplacedWildcard),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn a_wire_host_of_star_never_matches() {
        // Defence in depth: `matches` is fed strings from a subprocess's Host:
        // header, which is attacker-influenced. A literal `*` there is not a name.
        assert!(!p("*.crates.io").matches("*.crates.io"));
        assert!(!p("crates.io").matches("*"));
    }

    #[test]
    fn blanket_is_operator_only_never_profile() {
        // A profile is enabled by default, so `*` inside one would be a default-on
        // blanket egress grant delivered by a data file.
        assert_eq!(HostPattern::parse("*"), Ok(HostPattern::Any));
        assert_eq!(
            HostPattern::parse_profile_host("*"),
            Err(HostPatternError::BlanketNotAllowed)
        );
        // A profile may still use a scoped wildcard.
        assert_eq!(
            HostPattern::parse_profile_host("*.crates.io"),
            Ok(HostPattern::Wildcard("crates.io".into()))
        );
    }

    #[test]
    fn canonical_text_round_trips() {
        for s in ["crates.io", "*.crates.io", "*"] {
            assert_eq!(p(s).as_str(), s);
            assert_eq!(HostPattern::parse(&p(s).as_str()), Ok(p(s)));
        }
        // Normalisation is visible in the canonical form, so a display never
        // shows one thing while matching another.
        assert_eq!(p("CRATES.IO.").as_str(), "crates.io");
    }
}
