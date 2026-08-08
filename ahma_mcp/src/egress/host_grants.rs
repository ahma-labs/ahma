//! The reachable-host union: who may a sandboxed subprocess talk to, and *who
//! decided that*.
//!
//! ## Why this exists
//!
//! `--restrict-network` was already the right mechanism. It is kernel-enforced on
//! macOS (Seatbelt denies outbound IP except the proxy) and on Linux 6.7+
//! (Landlock restricts outbound TCP to the proxy port), it routes everything
//! through a guarded proxy that vets the resolved IP against DNS rebinding, and
//! it denies all by default. It had one problem, and it was fatal to adoption:
//! the operator who turned it on watched `cargo build` fail, then `npm install`,
//! then `go mod download`, and had to reverse-engineer a registry's CDN topology
//! before anything worked. So essentially nobody turned it on, and the usable
//! default stayed "reach anything".
//!
//! This is the eighth escape in the Pillar Security series read the right way
//! round. OpenAI's eval sandbox had exactly one sanctioned egress path — a
//! package-registry proxy — and when that single trusted component turned out to
//! carry a zero-day, it *was* the boundary. The lesson is not "never allow
//! egress". It is that the sanctioned path must be **deliberate and narrow**.
//! ahma's was neither, because the only configuration anyone ran permitted
//! everything.
//!
//! ## The mechanism
//!
//! The profile system already solved this identical problem for filesystem paths
//! ([`crate::sandbox::profiles`]): each toolchain declares what it needs, ships
//! enabled by default, and is refusable individually. Host declarations are the
//! same idea one layer out — a pre-answered bundle of grant questions — and reuse
//! the same data files, the same enable/disable switch, and the same provenance.
//!
//! Two sources feed one list:
//!
//! * **`[network] allow`** — the operator's own hosts, always in effect.
//! * **enabled profiles' `[[hosts]]`** — the toolchain hosts, in effect unless
//!   switched off.
//!
//! They **compose**; profile hosts never replace an operator's list and an
//! operator's list never suppresses a profile's. Either one alone is a usable
//! configuration, which is what lets an operator start from "the profiles get me
//! building" and add their own hosts one at a time.
//!
//! ## The default does not change, and must not be changed here
//!
//! `--restrict-network` stays **opt-in**. Everything in this module is inert
//! until an operator turns restriction on; a default install reaches the network
//! exactly as it did before.
//!
//! This is deliberate and is not an unfinished job. Flipping the default would
//! break the first command every existing user runs — including anyone whose
//! toolchain has no shipped profile, which is most toolchains. The purpose of
//! this work is to make restricted mode *painless enough* that defaulting it on
//! becomes a decision someone can responsibly make later, on evidence. Until
//! then, leave [`ahma_common::config::NetworkSettings::restrict`] defaulting to
//! `false`.

use crate::egress::allowlist::EgressAllowlist;
use crate::egress::host_pattern::HostPattern;
use crate::sandbox::profiles;

/// Who granted a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantSource {
    /// `[network] allow` in the operator's settings file.
    Operator,
    /// A shipped sandbox profile, named.
    Profile(String),
}

impl GrantSource {
    /// The short label used in disclosures, matching the provenance vocabulary
    /// the filesystem grants already use (`builtin-profile(rust)`).
    pub fn label(&self) -> String {
        match self {
            Self::Operator => "[network] allow".to_string(),
            Self::Profile(p) => format!("builtin-profile({p})"),
        }
    }
}

/// One host, and the decision behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostGrant {
    /// The matcher this grant installs.
    pub pattern: HostPattern,
    /// Who granted it.
    pub source: GrantSource,
    /// Why, when the source recorded one. Profiles always do; an operator's
    /// `[network] allow` is a bare list, so `None` there.
    pub reason: Option<String>,
}

/// The inputs to the union, named rather than positional — the arguments are
/// three lists and a flag, and a caller that transposes two of them would silently
/// widen or narrow egress.
#[derive(Debug, Clone, Copy)]
pub struct EgressGrantSources<'a> {
    /// `[network] allow`.
    pub operator_allow: &'a [String],
    /// `[sandbox] profiles` — the profiles that are enabled at all.
    pub enabled_profiles: &'a [String],
    /// `[network] profile_hosts`: the master switch for profile-contributed
    /// hosts. `false` keeps every profile's *path* grants and drops all of its
    /// *host* grants.
    pub profile_hosts: bool,
    /// `[network] deny_profile_hosts`: per-profile version of the same thing.
    pub deny_profile_hosts: &'a [String],
}

/// The computed union of everything that may be reached, with provenance.
#[derive(Debug, Clone, Default)]
pub struct EgressGrants {
    grants: Vec<HostGrant>,
}

impl EgressGrants {
    /// Compute the union.
    ///
    /// Operator entries come first so that a disclosure reads "what you asked
    /// for, then what the toolchains asked for on your behalf".
    ///
    /// A profile's hosts are dropped when the profile is disabled in
    /// `[sandbox] profiles` (it contributes nothing at all), when
    /// `[network] profile_hosts = false` (no profile contributes hosts), or when
    /// it is named in `[network] deny_profile_hosts` (this profile keeps its
    /// paths and loses its hosts). The three compose in that order; the coarsest
    /// wins, which is the safe direction.
    pub fn compute(sources: EgressGrantSources<'_>) -> Self {
        let mut grants = Vec::new();

        for raw in sources.operator_allow {
            match HostPattern::parse(raw) {
                Ok(pattern) => grants.push(HostGrant {
                    pattern,
                    source: GrantSource::Operator,
                    reason: None,
                }),
                Err(e) => tracing::warn!(
                    "[network] allow entry '{raw}' is not a host pattern: {e}. It grants \
                     nothing; fix or remove it."
                ),
            }
        }

        if sources.profile_hosts {
            for host in profiles::profile_hosts(sources.enabled_profiles) {
                if sources
                    .deny_profile_hosts
                    .iter()
                    .any(|d| d == &host.profile)
                {
                    continue;
                }
                grants.push(HostGrant {
                    pattern: host.pattern,
                    source: GrantSource::Profile(host.profile),
                    reason: Some(host.reason),
                });
            }
        }

        Self { grants }
    }

    /// Every grant, in disclosure order.
    pub fn grants(&self) -> &[HostGrant] {
        &self.grants
    }

    /// Whether anything at all is reachable. `false` means deny-all: restriction
    /// is on and nothing was granted, which is a legitimate — and loud —
    /// configuration rather than a misconfiguration.
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// The allowlist the proxy enforces.
    pub fn allowlist(&self) -> EgressAllowlist {
        EgressAllowlist::from_patterns(self.grants.iter().map(|g| g.pattern.clone()))
    }

    /// Which grant, if any, admits `host`. Used by disclosures and diagnostics so
    /// "why could it reach that?" has an answer that names a profile.
    pub fn grant_for(&self, host: &str) -> Option<&HostGrant> {
        self.grants.iter().find(|g| g.pattern.matches(host))
    }

    /// The human-readable union, one host per line, each naming its source.
    ///
    /// This is the R-PERM.5.2 obligation for hosts: the display that lists a
    /// profile carries what saying yes to it costs. A merged anonymous list —
    /// "reachable: crates.io, proxy.golang.org, registry.npmjs.org" — is not
    /// refusable, because a reader cannot tell which entries came from a profile
    /// they could simply switch off.
    pub fn disclosure(&self) -> String {
        if self.grants.is_empty() {
            return "  (nothing — all subprocess egress is denied)".to_string();
        }
        let width = self
            .grants
            .iter()
            .map(|g| g.pattern.as_str().len())
            .max()
            .unwrap_or(0);
        self.grants
            .iter()
            .map(|g| {
                let reason = g
                    .reason
                    .as_deref()
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default();
                format!(
                    "  {host:<width$}  {src}{reason}",
                    host = g.pattern.as_str(),
                    src = g.source.label(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The profiles that actually contributed at least one host, for the
    /// one-line "…and here is how to turn them off" hint.
    pub fn contributing_profiles(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for g in &self.grants {
            if let GrantSource::Profile(p) = &g.source
                && !out.iter().any(|e| e == p)
            {
                out.push(p.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_profiles() -> Vec<String> {
        profiles::default_profile_names()
    }

    fn compute(
        operator: &[&str],
        enabled: &[String],
        hosts_on: bool,
        denied: &[&str],
    ) -> EgressGrants {
        let operator: Vec<String> = operator.iter().map(|s| s.to_string()).collect();
        let denied: Vec<String> = denied.iter().map(|s| s.to_string()).collect();
        EgressGrants::compute(EgressGrantSources {
            operator_allow: &operator,
            enabled_profiles: enabled,
            profile_hosts: hosts_on,
            deny_profile_hosts: &denied,
        })
    }

    #[test]
    fn the_default_profile_set_makes_the_three_toolchains_work() {
        // The whole adoption argument in one assertion: with restriction on and
        // nothing hand-written, the first `cargo build` / `npm install` /
        // `go mod download` must not fail.
        let grants = compute(&[], &all_profiles(), true, &[]);
        let list = grants.allowlist();
        for host in [
            "index.crates.io",
            "static.crates.io",
            "crates.io",
            "static.rust-lang.org",
            "github.com",
            "registry.npmjs.org",
            "registry.yarnpkg.com",
            "nodejs.org",
            "proxy.golang.org",
            "sum.golang.org",
        ] {
            assert!(list.allows(host), "{host} must be reachable out of the box");
        }
    }

    #[test]
    fn the_union_is_still_narrow() {
        // "Deliberate and narrow" is the actual requirement; a profile set that
        // quietly reached the whole internet would have solved the wrong problem.
        let list = compute(&[], &all_profiles(), true, &[]).allowlist();
        for host in [
            "example.com",
            "evil.example",
            "storage.googleapis.com",
            "objects.githubusercontent.com",
            "raw.githubusercontent.com",
            "pypi.org",
        ] {
            assert!(
                !list.allows(host),
                "{host} must NOT be reachable by default"
            );
        }
    }

    #[test]
    fn no_shipped_profile_grants_a_blanket_or_a_lookalike() {
        // Two failures a data file could introduce without anyone noticing: a `*`
        // entry (parsed away by `parse_profile_host`, so this asserts the data
        // never tries), and a rule whose suffix an attacker can prefix.
        let grants = compute(&[], &all_profiles(), true, &[]);
        for g in grants.grants() {
            assert_ne!(
                g.pattern,
                HostPattern::Any,
                "a shipped profile must never grant blanket egress"
            );
        }
        let list = grants.allowlist();
        assert!(!list.allows("evilcrates.io"));
        assert!(!list.allows("notgithub.com"));
        assert!(!list.allows("registry.npmjs.org.evil.example"));
    }

    #[test]
    fn operator_hosts_compose_with_profile_hosts_rather_than_replacing_them() {
        // The bug this guards is the natural implementation: "if the operator
        // configured an allowlist, use theirs". That would break every toolchain
        // the moment an operator added one internal host.
        let grants = compute(&["artifacts.internal.example"], &all_profiles(), true, &[]);
        let list = grants.allowlist();
        assert!(list.allows("artifacts.internal.example"), "operator's host");
        assert!(list.allows("index.crates.io"), "…and the profiles' hosts");

        // Symmetrically, the profiles do not suppress the operator: with every
        // profile off, the operator's list is still the allowlist.
        let only_operator = compute(&["artifacts.internal.example"], &[], true, &[]);
        assert!(
            only_operator
                .allowlist()
                .allows("artifacts.internal.example")
        );
        assert!(!only_operator.allowlist().allows("index.crates.io"));
    }

    #[test]
    fn disabling_a_profile_removes_exactly_its_hosts() {
        let without_go: Vec<String> = all_profiles().into_iter().filter(|n| n != "go").collect();
        let list = compute(&[], &without_go, true, &[]).allowlist();
        assert!(
            !list.allows("proxy.golang.org"),
            "the disabled profile's host"
        );
        assert!(!list.allows("sum.golang.org"));
        assert!(list.allows("index.crates.io"), "…and only its hosts");
        assert!(list.allows("registry.npmjs.org"));
    }

    #[test]
    fn profile_hosts_can_be_dropped_without_dropping_path_grants() {
        // The operator control that had to compose: `[network] profile_hosts =
        // false` must not cost you `~/.cargo`, or the knob is unusable and nobody
        // touches it.
        let grants = compute(&["artifacts.internal.example"], &all_profiles(), false, &[]);
        assert!(!grants.allowlist().allows("index.crates.io"));
        assert!(grants.allowlist().allows("artifacts.internal.example"));
        assert!(grants.contributing_profiles().is_empty());

        // The filesystem side is untouched — same enabled list, same rules.
        let rules = profiles::resolved_rules(&all_profiles(), true);
        assert!(
            rules.iter().any(|r| r.profile == "rust"),
            "turning off profile hosts must leave the toolchain paths granted"
        );
    }

    #[test]
    fn a_single_profiles_hosts_can_be_dropped_without_dropping_the_others() {
        // Per-profile version of the same control: "I write Go, but this machine
        // must never reach the public module mirror" without losing ~/.go.
        let grants = compute(&[], &all_profiles(), true, &["go"]);
        let list = grants.allowlist();
        assert!(!list.allows("proxy.golang.org"));
        assert!(list.allows("index.crates.io"));
        assert!(!grants.contributing_profiles().iter().any(|p| p == "go"));
        assert!(grants.contributing_profiles().iter().any(|p| p == "rust"));

        let rules = profiles::resolved_rules(&all_profiles(), true);
        assert!(rules.iter().any(|r| r.profile == "go"), "go paths remain");
    }

    #[test]
    fn nothing_granted_means_deny_all_not_allow_all() {
        // The failure mode worth being paranoid about: an empty union that
        // degrades open would turn "restrict the network" into "do nothing".
        let grants = compute(&[], &[], true, &[]);
        assert!(grants.is_empty());
        assert!(!grants.allowlist().allows("crates.io"));
        assert!(!grants.allowlist().allows("anything.example"));
        assert!(grants.disclosure().contains("denied"));
    }

    #[test]
    fn disclosure_names_the_granting_profile_for_every_host() {
        // R-PERM.5.2: a merged anonymous list is not refusable. A reader must be
        // able to go from a host they do not recognise to the switch that removes
        // it.
        let grants = compute(&["artifacts.internal.example"], &all_profiles(), true, &[]);
        let text = grants.disclosure();

        assert!(
            text.contains("artifacts.internal.example") && text.contains("[network] allow"),
            "the operator's own host is attributed to the operator:\n{text}"
        );
        for (host, profile) in [
            ("index.crates.io", "rust"),
            ("registry.npmjs.org", "node"),
            ("proxy.golang.org", "go"),
        ] {
            let line = text
                .lines()
                .find(|l| l.contains(host))
                .unwrap_or_else(|| panic!("{host} missing from disclosure:\n{text}"));
            assert!(
                line.contains(&format!("builtin-profile({profile})")),
                "{host} must name the profile that granted it, got: {line}"
            );
        }
        // And the reason travels with the host, so the line is reviewable.
        assert!(
            text.lines()
                .find(|l| l.contains("sum.golang.org"))
                .is_some_and(|l| l.contains("checksum")),
            "each profile host carries why the toolchain needs it:\n{text}"
        );
    }

    #[test]
    fn grant_for_answers_why_a_host_was_reachable() {
        let grants = compute(&[], &all_profiles(), true, &[]);
        let g = grants
            .grant_for("static.crates.io")
            .expect("a reachable host has a grant");
        assert_eq!(g.source, GrantSource::Profile("rust".into()));
        assert!(grants.grant_for("evil.example").is_none());
    }

    #[test]
    fn a_malformed_operator_entry_is_dropped_not_coerced() {
        // An entry that cannot match must not look like one that can; the
        // operator would otherwise read their own settings file and conclude
        // egress works.
        let grants = compute(&["https://crates.io", "ok.example"], &[], true, &[]);
        assert_eq!(grants.grants().len(), 1);
        assert!(grants.allowlist().allows("ok.example"));
        assert!(!grants.allowlist().allows("crates.io"));
    }

    #[test]
    fn every_shipped_host_carries_a_reason() {
        // The reason is what makes the disclosure actionable; a profile that
        // shipped a bare hostname would degrade the display to a list of names.
        for g in compute(&[], &all_profiles(), true, &[]).grants() {
            assert!(
                g.reason.as_deref().is_some_and(|r| r.len() > 15),
                "{} needs a substantive reason",
                g.pattern
            );
        }
    }
}
