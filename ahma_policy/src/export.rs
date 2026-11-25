//! `ahma policy export --cedar` and `ahma policy verify` (SPEC R-POLICY.6).
//!
//! The export renders what ahma enforces in userspace as Cedar, for review in a
//! pull request or by a security team: the built-in hard denylist and
//! `never_allow` as forbids, saved grants and allowlists as permits. It is a
//! picture, not a source: it contains permits, which team policy refuses, so it
//! can never be loaded back to widen anything.
//!
//! `verify` checks the invariants that matter most on concrete requests,
//! through ahma's own decision code, including any loaded team policy. It is a
//! check of named cases, not a proof over all requests; symbolic proof with
//! Cedar's SMT compiler is not shipped (it needs a cvc5 solver).

use ahma_common::config::{AhmaSettings, ScopeAccess};
use ahma_common::scope_grant::{GrantRisk, classify_grant_risk, sensitive_dirs};
use cedar_policy::{PolicySet, Schema, ValidationMode, Validator};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// The effective userspace policy as Cedar text.
pub fn export(settings: &AhmaSettings, home: Option<&Path>) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "// ahma's effective userspace policy, exported for review (SPEC R-POLICY.6).\n\
         // Not loadable as team policy: it holds permits. Session-only grants and\n\
         // answers are not included. The kernel sandbox is built from what this admits.\n"
    );

    let _ = writeln!(
        out,
        "// --- Built-in: never granted, whatever anyone answers ---"
    );
    let mut forbidden: Vec<PathBuf> = home.map(sensitive_dirs).unwrap_or_default();
    forbidden.push(ahma_common::policy::system_policy_dir());
    for dir in forbidden {
        let _ = writeln!(
            out,
            "@reason(\"credentials, secrets or ahma's own control files\")\n\
             forbid(principal, action in [Ahma::Action::\"read\", Ahma::Action::\"write\"], \
             resource in Ahma::Path::{});\n",
            quote(&dir.display().to_string())
        );
    }
    if let Some(home) = home {
        let _ = writeln!(
            out,
            "@reason(\"the whole home directory\")\n\
             forbid(principal, action in [Ahma::Action::\"read\", Ahma::Action::\"write\"], \
             resource == Ahma::Path::{});\n",
            quote(&home.display().to_string())
        );
    }

    let _ = writeln!(out, "// --- [web] never_allow ---");
    for pattern in &settings.web.never_allow {
        host_rule(&mut out, "forbid", "fetch", pattern, None);
    }

    let _ = writeln!(
        out,
        "// --- Saved filesystem grants ([sandbox] persistent_scopes) ---"
    );
    for grant in &settings.sandbox.persistent_scopes {
        let actions = match grant.access {
            ScopeAccess::Ro => "[Ahma::Action::\"read\"]",
            ScopeAccess::Rw => "[Ahma::Action::\"read\", Ahma::Action::\"write\"]",
        };
        let when = grant
            .workspace
            .as_ref()
            .map(|ws| {
                format!(
                    "\n  when {{ principal has workspace && principal.workspace == {} }}",
                    quote(&ws.display().to_string())
                )
            })
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "permit(principal, action in {actions}, resource in Ahma::Path::{}){when};\n",
            quote(&grant.path.display().to_string())
        );
    }

    let _ = writeln!(out, "// --- [network] allow (with --restrict-network) ---");
    for pattern in &settings.network.allow {
        host_rule(&mut out, "permit", "connect", pattern, None);
    }
    let _ = writeln!(out, "// --- [web] always_allow ---");
    for pattern in &settings.web.always_allow {
        host_rule(&mut out, "permit", "fetch", pattern, None);
    }
    out
}

/// One rule for a host pattern: exact, `*.domain`, or `*`. A scheme or port
/// qualifier is dropped, since the schema has hosts only; it is noted.
fn host_rule(out: &mut String, effect: &str, action: &str, pattern: &str, note: Option<&str>) {
    let bare = pattern
        .split_once("://")
        .map_or(pattern, |(_, rest)| rest)
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut comment = note.map(str::to_string);
    let resource = if bare == "*" {
        "resource".to_string()
    } else if let Some(domain) = bare.strip_prefix("*.") {
        comment = Some(format!(
            "`{pattern}` covers one label; `in` covers every depth below {domain}"
        ));
        format!("resource in Ahma::Host::{}", quote(domain))
    } else {
        format!("resource == Ahma::Host::{}", quote(&bare))
    };
    if bare != pattern && !pattern.starts_with("*.") && pattern != "*" {
        comment = Some(format!(
            "from `{pattern}`; scheme and port are not modelled"
        ));
    }
    if let Some(c) = comment {
        let _ = writeln!(out, "// {c}");
    }
    let _ = writeln!(
        out,
        "{effect}(principal, action == Ahma::Action::\"{action}\", {resource});\n"
    );
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Check that `text` is Cedar that ahma's schema accepts, permits included.
pub fn validate_text(text: &str) -> Result<(), String> {
    let (schema, _) = Schema::from_cedarschema_str(crate::SCHEMA).map_err(|e| e.to_string())?;
    let set = PolicySet::from_str(text).map_err(|e| e.to_string())?;
    let result = Validator::new(schema).validate(&set, ValidationMode::Strict);
    if result.validation_passed() {
        Ok(())
    } else {
        Err(result
            .validation_errors()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; "))
    }
}

/// One invariant checked on one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub claim: String,
    pub held: bool,
}

/// The invariants `ahma policy verify` checks: no grant, of any access, ever
/// opens a credential directory, ahma's own control files, a policy directory,
/// the home directory or a filesystem root. Checked through
/// `classify_grant_risk`, so any loaded team policy takes part.
pub fn verify(home: Option<&Path>) -> Vec<Check> {
    let mut probes: Vec<(String, PathBuf)> = Vec::new();
    if let Some(home) = home {
        for dir in sensitive_dirs(home) {
            probes.push((
                format!("no grant opens {}", dir.display()),
                dir.join("probe"),
            ));
        }
        probes.push((
            "no grant opens the user's team-policy directory".into(),
            home.join(".ahma").join("policy").join("team.cedar"),
        ));
        probes.push((
            "no grant opens the home directory".into(),
            home.to_path_buf(),
        ));
    }
    let system = ahma_common::policy::system_policy_dir();
    probes.push((
        format!(
            "no grant opens the machine's team-policy directory {}",
            system.display()
        ),
        system.join("team.cedar"),
    ));
    let root = home
        .and_then(|h| h.ancestors().last())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"));
    probes.push(("no grant opens a filesystem root".into(), root));

    probes
        .into_iter()
        .map(|(claim, path)| Check {
            claim,
            held: matches!(classify_grant_risk(&path, home, &[]), GrantRisk::Refused(_)),
        })
        .collect()
}
