//! Sandbox profiles: toolchain carve-outs as **shipped data**, not compiled-in
//! special cases (SPEC R-PERM.5).
//!
//! ## What this replaces, and why
//!
//! The sandbox backends used to carry hard-coded lists of application paths —
//! `[".cargo", ".rustup", ".nvm", ".npm", ".go", ".cache"]` in `landlock.rs`,
//! `[".cargo", ".rustup"]` in `seatbelt.rs`, and a cargo-shaped writable set in
//! `pkg_cache.rs`. Those lists worked, for the person who wrote them. They have
//! three properties that do not survive contact with the real world:
//!
//! * **They cannot scale.** ahma will meet thousands of toolchains it has never
//!   heard of. Shipping a Rust developer's carve-outs and calling the model
//!   general is a claim the code cannot back.
//! * **They are invisible.** A user cannot see that ahma quietly gave every
//!   sandboxed command read+execute over `~/.cache`, because nothing displays it.
//! * **They are not refusable.** Someone hardening a machine has no way to say
//!   "not that one" short of patching the binary.
//!
//! A profile fixes all three at once, because a profile is nothing more than a
//! **pre-answered bundle of grant questions** — the same paths the user would end
//! up granting one kernel denial at a time, shipped so they don't have to. Being
//! data, it can be listed with provenance (`builtin-profile(rust)`), disabled
//! individually (`[sandbox] profiles`), and contributed by someone who uses a
//! toolchain no ahma developer has ever run.
//!
//! ## What is *not* a profile
//!
//! Platform invariants stay in the backends: `/usr`, `/bin`, `/etc` read+execute;
//! device paths; the temp directory; credential-directory denials. The test is
//! *app-specific*, not *platform-specific* — `/usr` is not a carve-out for an
//! application, it is what a process needs to be a process.
//!
//! Nor is macOS's blanket `(allow file-read*)`. That is a platform *limitation*
//! (APFS firmlinks defeat subpath matching for reads), not a grant, so it cannot
//! be expressed as one — it is disclosed instead (R-PERM.5.1,
//! [`macos_read_disclosure`]).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::egress::host_pattern::HostPattern;

/// What a profile rule grants.
///
/// Three levels, not two, because the distinction is load-bearing: Landlock's
/// read set does **not** include `Execute`, and `~/.cargo/bin/cargo` is a binary
/// the sandboxed command must be able to *run*, not merely read. Folding `rx`
/// into `ro` would leave a sandbox that can read the compiler but not start it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileAccess {
    /// Read only.
    Ro,
    /// Read and execute — a toolchain directory holding binaries.
    Rx,
    /// Read, execute, and write — a cache the tool fills in as it works.
    Rw,
}

impl ProfileAccess {
    /// Whether this level permits writes.
    pub fn is_write(self) -> bool {
        matches!(self, ProfileAccess::Rw)
    }
    /// Whether this level permits executing binaries beneath the path.
    pub fn is_execute(self) -> bool {
        matches!(self, ProfileAccess::Rx | ProfileAccess::Rw)
    }
}

/// Whether a rule targets a directory subtree or one literal file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleKind {
    /// A directory; the rule applies to everything beneath it.
    #[default]
    Dir,
    /// A single file (cargo's cross-process lock files are the motivating case).
    File,
}

/// One rule as written in a profile's TOML.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    path: String,
    access: ProfileAccess,
    #[serde(default)]
    kind: RuleKind,
    /// Create the path if it does not exist. Landlock cannot open a file
    /// descriptor to a path that is not there, so a cache directory that has not
    /// been created yet would silently receive no rule at all.
    #[serde(default)]
    precreate: bool,
}

/// One host entry as written in a profile's TOML.
///
/// `reason` is mandatory, not decorative. A hostname on its own is unreviewable —
/// nobody can tell whether `storage.googleapis.com` is load-bearing or cargo-cult
/// by looking at it — and R-PERM.5.2 requires that a profile's cost be visible
/// wherever the profile is listed. The reason is what makes the disclosure a
/// statement a user can actually act on rather than a list of names.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHost {
    host: String,
    reason: String,
}

/// A profile as written in its TOML file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    name: String,
    description: String,
    rules: Vec<RawRule>,
    /// Hostnames the toolchain must reach when `--restrict-network` is on.
    ///
    /// Absent in a profile whose toolchain fetches nothing (`common`), which is
    /// why this defaults rather than being required.
    #[serde(default)]
    hosts: Vec<RawHost>,
    /// Paths that this profile asserts must **never** become writable. Not a
    /// mechanism — the backends are allow-lists, so a path is unwritable simply
    /// by not being granted — but an *assertion*, checked by tests, so a later
    /// edit that "simplifies" the rules into one broad `rw` grant fails loudly
    /// instead of quietly handing over `~/.cargo/credentials.toml`.
    #[serde(default)]
    deny_write: Vec<String>,
}

/// A rule with its path resolved against the real environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRule {
    /// The profile that contributed this rule, for provenance in every display.
    pub profile: String,
    /// The absolute path the rule applies to.
    pub path: PathBuf,
    /// What it grants.
    pub access: ProfileAccess,
    /// Directory subtree or literal file.
    pub kind: RuleKind,
    /// Whether the path should be created if absent.
    pub precreate: bool,
}

/// A hostname a profile grants, carrying the profile that granted it.
///
/// The provenance is the reason this is a struct rather than a bare
/// `HostPattern`. A merged, anonymous list of reachable hosts is not refusable:
/// a user who sees `proxy.golang.org` in an allowlist and writes no Go cannot
/// tell whether removing it is safe. Naming the granting profile turns the
/// question into "do I want the `go` profile?", which they can answer
/// (R-PERM.5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileHost {
    /// The profile that contributed this host.
    pub profile: String,
    /// The parsed pattern. Validated at load; a malformed entry never gets here.
    pub pattern: HostPattern,
    /// Why this toolchain needs it, shown alongside the host in every disclosure.
    pub reason: String,
}

/// A loaded, name-addressable profile.
#[derive(Debug, Clone)]
pub struct SandboxProfile {
    /// Short name (`rust`), used in `[sandbox] profiles` and in provenance.
    pub name: String,
    /// One-line human description, shown by `ahma permissions list`.
    pub description: String,
    /// The rules it contributes, with paths still unresolved.
    rules: Vec<RawRule>,
    /// Paths this profile asserts must never be writable.
    deny_write: Vec<String>,
    /// Hostnames it contributes to the egress allowlist under restriction.
    hosts: Vec<RawHost>,
}

/// The profiles ahma ships, parsed from the TOML data files in `profiles/`.
///
/// Compiled in via `include_str!` so a released binary is self-contained (no
/// data files to install, nothing to go missing), while the *source of truth* is
/// still a data file anyone can read and copy.
pub fn builtin_profiles() -> &'static [SandboxProfile] {
    // Parsed once per process: the sources are compiled in and immutable, and
    // this sits on the sandbox-spawn hot path (every `run_terminal_command`
    // resolves profile rules) — re-parsing four TOML documents per spawn was
    // pure waste.
    static PROFILES: std::sync::OnceLock<Vec<SandboxProfile>> = std::sync::OnceLock::new();
    PROFILES.get_or_init(|| {
        const SOURCES: &[&str] = &[
            include_str!("../../profiles/rust.toml"),
            include_str!("../../profiles/node.toml"),
            include_str!("../../profiles/go.toml"),
            include_str!("../../profiles/common.toml"),
        ];
        SOURCES
            .iter()
            .filter_map(|src| match toml::from_str::<RawProfile>(src) {
                Ok(raw) => Some(SandboxProfile {
                    name: raw.name,
                    description: raw.description,
                    rules: raw.rules,
                    deny_write: raw.deny_write,
                    hosts: raw.hosts,
                }),
                Err(e) => {
                    // A malformed builtin is a build-time bug, not a user problem; it
                    // is pinned by `every_builtin_profile_parses`. Degrade rather than
                    // panic so one bad profile cannot brick every sandbox.
                    tracing::error!("built-in sandbox profile failed to parse: {e}");
                    None
                }
            })
            .collect()
    })
}

/// The `[sandbox] profiles` list from `~/.ahma/settings.toml`, loaded once per
/// process.
///
/// Sandbox configuration is fixed for the life of a session (the R5 invariant:
/// scope and enforcement decisions never change mid-session; a settings edit —
/// like a persistent scope grant, R5.4.4/R5.4.5 — takes effect on the next
/// server start). Both kernel backends used to re-read and re-parse the whole
/// settings file on **every sandboxed spawn**, which was hot-path waste and
/// implied a mid-session behavior change the sandbox forbids.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn enabled_profile_names() -> &'static [String] {
    static ENABLED: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    ENABLED.get_or_init(|| ahma_common::config::AhmaSettings::load().sandbox.profiles)
}

/// The names of every shipped profile — the default value of `[sandbox] profiles`.
///
/// Default is **opt-out**: everything ships enabled, so today's behavior is
/// preserved exactly. What changes is that it is now *visible* and *refusable*,
/// rather than invisible and mandatory.
pub fn default_profile_names() -> Vec<String> {
    builtin_profiles().iter().map(|p| p.name.clone()).collect()
}

/// Resolve the rules contributed by the enabled profiles.
///
/// `enabled` is `[sandbox] profiles`. An unknown name is a warning, not an error:
/// a settings file mentioning a profile from a newer ahma should not stop the
/// older one from starting.
///
/// When `package_cache_write` is `false` the `rw` rules are **downgraded to
/// `rx`**, not dropped — the toolchain stays runnable, only its caches become
/// read-only. That preserves the meaning the flag has always had
/// (`--no-package-cache-write` = "the strictest isolation", not "break cargo").
pub fn resolved_rules(enabled: &[String], package_cache_write: bool) -> Vec<ResolvedRule> {
    let profiles = builtin_profiles();
    let mut out = Vec::new();

    for name in enabled {
        let Some(profile) = profiles.iter().find(|p| &p.name == name) else {
            tracing::warn!(
                "[sandbox] profiles names an unknown profile '{name}'; ignoring it. \
                 Known profiles: {}",
                profiles
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            continue;
        };

        for rule in &profile.rules {
            let Some(path) = resolve_path(&rule.path) else {
                continue;
            };
            let access = match (rule.access, package_cache_write) {
                (ProfileAccess::Rw, false) => ProfileAccess::Rx,
                (a, _) => a,
            };
            out.push(ResolvedRule {
                profile: profile.name.clone(),
                path,
                access,
                kind: rule.kind,
                precreate: rule.precreate && access.is_write(),
            });
        }
    }
    out
}

/// The rules that actually apply on this machine: [`resolved_rules`], minus the
/// ones whose path does not exist (after pre-creating the ones that ask for it).
///
/// A rule for a path that is not there is not merely useless — on Linux it cannot
/// even be expressed, because Landlock needs a file descriptor.
pub fn applicable_rules(enabled: &[String], package_cache_write: bool) -> Vec<ResolvedRule> {
    let rules = resolved_rules(enabled, package_cache_write);
    for rule in &rules {
        if rule.precreate {
            precreate(rule);
        }
    }
    rules.into_iter().filter(|r| r.path.exists()).collect()
}

/// Create a rule's path if it is missing. Best-effort: a failure just means the
/// rule is skipped by [`applicable_rules`], which is the safe direction.
fn precreate(rule: &ResolvedRule) {
    if rule.path.exists() {
        return;
    }
    match rule.kind {
        RuleKind::Dir => {
            if let Err(e) = std::fs::create_dir_all(&rule.path) {
                tracing::debug!("could not pre-create {}: {e}", rule.path.display());
            }
        }
        RuleKind::File => {
            if let Some(parent) = rule.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&rule.path)
            {
                tracing::debug!("could not pre-create {}: {e}", rule.path.display());
            }
        }
    }
}

/// Expand `~` and `${VAR}` / `${VAR:-default}` in a profile path.
///
/// The `${VAR:-default}` form is what lets a profile stay honest about a
/// toolchain that can be *relocated*: cargo reads `$CARGO_HOME`, and a profile
/// that hard-coded `~/.cargo` would silently grant nothing on a machine that had
/// moved it — the exact failure a static path list cannot see coming.
///
/// Returns `None` when the path cannot be resolved (no home directory), which
/// simply drops the rule.
fn resolve_path(raw: &str) -> Option<PathBuf> {
    let expanded = expand_vars(raw);
    let expanded = expand_tilde(&expanded)?;
    Some(PathBuf::from(expanded))
}

/// Substitute `${VAR}` and `${VAR:-default}` from the environment.
fn expand_vars(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // Unterminated `${` — emit it literally rather than eating the path.
            out.push_str(&rest[start..]);
            return out;
        };
        let spec = &after[..end];
        let (name, default) = match spec.split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (spec, None),
        };
        match std::env::var(name) {
            Ok(v) if !v.is_empty() => out.push_str(&v),
            _ => out.push_str(default.unwrap_or("")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Expand a leading `~`.
fn expand_tilde(raw: &str) -> Option<String> {
    if raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\") {
        let home = ahma_common::config::ahma_home_dir()?;
        let rest = raw.trim_start_matches('~').trim_start_matches(['/', '\\']);
        return Some(if rest.is_empty() {
            home.to_string_lossy().into_owned()
        } else {
            home.join(rest).to_string_lossy().into_owned()
        });
    }
    Some(raw.to_string())
}

/// The paths every enabled profile asserts must never be writable, resolved.
///
/// Exposed so tests can hold the assertion, and so a future `ahma permissions
/// list --profiles` can show a user what a profile refuses as well as what it
/// grants — the more reassuring half of the answer.
pub fn deny_write_paths(enabled: &[String]) -> Vec<PathBuf> {
    builtin_profiles()
        .iter()
        .filter(|p| enabled.iter().any(|n| n == &p.name))
        .flat_map(|p| p.deny_write.iter().filter_map(|s| resolve_path(s)))
        .collect()
}

/// The hostnames the enabled profiles grant, each carrying the profile that
/// granted it and why (SPEC R-PERM.5, R-PERM.5.2).
///
/// **These are inert unless `--restrict-network` / `[network] restrict` is on.**
/// With restriction off — still the default — the sandbox permits all egress and
/// this list is never consulted, so adding a host here widens nothing on a
/// default install. It only makes the *narrow* configuration usable, which is the
/// entire point: restricted mode was already kernel-enforced and already correct,
/// and nobody turned it on because the first `cargo build` failed.
///
/// A malformed or blanket (`*`) entry in a shipped profile is dropped with an
/// error rather than admitted. A profile is enabled by default, so a `*` in one
/// would be a default-on blanket egress grant arriving through a data file —
/// exactly the invisible, unrefusable grant profiles exist to eliminate.
///
/// Order follows [`builtin_profiles`], not `enabled`, so two machines with the
/// same profiles enabled disclose the same list in the same order regardless of
/// how the operator happened to spell their settings file.
pub fn profile_hosts(enabled: &[String]) -> Vec<ProfileHost> {
    let mut out = Vec::new();
    for profile in builtin_profiles() {
        if !enabled.iter().any(|n| n == &profile.name) {
            continue;
        }
        for raw in &profile.hosts {
            match HostPattern::parse_profile_host(&raw.host) {
                Ok(pattern) => out.push(ProfileHost {
                    profile: profile.name.clone(),
                    pattern,
                    reason: raw.reason.clone(),
                }),
                Err(e) => tracing::error!(
                    "sandbox profile '{}' declares an unusable host '{}': {e}; ignoring it",
                    profile.name,
                    raw.host
                ),
            }
        }
    }
    out
}

/// The disclosure macOS owes its users (SPEC R-PERM.5.1).
///
/// macOS Seatbelt grants blanket `(allow file-read*)` because APFS firmlinks and
/// cryptex volumes defeat subpath matching for reads — a sandboxed command's
/// resolved vnodes simply do not sit under `/usr`, `/System`, and friends. So on
/// macOS, **writes are kernel-scoped but reads are not**.
///
/// That cannot be represented as a profile, because it is not a grant anyone
/// made. It can, however, be *said out loud* — which is the same honesty R7.5
/// demands when ahma defers to a host sandbox. A limitation the user cannot see
/// is a limitation the user cannot compensate for.
pub fn macos_read_disclosure() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some(
            "macOS: writes are kernel-scoped to the sandbox, but reads are NOT restricted \
             (a platform limitation — APFS firmlinks defeat read subpath matching). Treat \
             any file this user can read as readable by a sandboxed command.",
        )
    } else {
        None
    }
}

/// Whether `path` is inside any rule granted by the enabled profiles.
pub fn is_profile_path(path: &Path, enabled: &[String], package_cache_write: bool) -> bool {
    resolved_rules(enabled, package_cache_write)
        .iter()
        .any(|r| path.starts_with(&r.path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        default_profile_names()
    }

    #[test]
    fn every_builtin_profile_parses() {
        // A malformed shipped profile is a build-time bug that would silently
        // shrink the sandbox's usable surface for everyone.
        let profiles = builtin_profiles();
        assert_eq!(
            profiles.len(),
            4,
            "all four shipped profiles parse: {:?}",
            profiles.iter().map(|p| &p.name).collect::<Vec<_>>()
        );
        for p in profiles {
            assert!(!p.description.is_empty(), "{} needs a description", p.name);
            assert!(!p.rules.is_empty(), "{} grants nothing", p.name);
        }
    }

    #[test]
    fn the_rust_profile_reproduces_the_carve_outs_it_replaced() {
        // This is the golden assertion for the refactor: the paths that used to be
        // hard-coded in landlock.rs / seatbelt.rs / pkg_cache.rs must come out of
        // the profile unchanged, or this "pure refactor" quietly changed what the
        // sandbox allows.
        let rules = resolved_rules(&names(), true);
        let rust: Vec<_> = rules.iter().filter(|r| r.profile == "rust").collect();

        let cargo = super::resolve_path("${CARGO_HOME:-~/.cargo}").unwrap();
        let rustup = super::resolve_path("${RUSTUP_HOME:-~/.rustup}").unwrap();

        let find = |p: &PathBuf| rust.iter().find(|r| &r.path == p).map(|r| r.access);

        assert_eq!(
            find(&cargo),
            Some(ProfileAccess::Rx),
            "~/.cargo: read+execute"
        );
        assert_eq!(
            find(&rustup),
            Some(ProfileAccess::Rx),
            "~/.rustup: read+execute"
        );
        assert_eq!(
            find(&cargo.join("registry")),
            Some(ProfileAccess::Rw),
            "the registry cache must stay writable or `cargo add` breaks in the sandbox"
        );
        assert_eq!(find(&cargo.join("git")), Some(ProfileAccess::Rw));
        assert_eq!(find(&cargo.join(".package-cache")), Some(ProfileAccess::Rw));
        assert_eq!(
            find(&cargo.join(".package-cache-mutate")),
            Some(ProfileAccess::Rw)
        );
    }

    #[test]
    fn no_profile_ever_makes_credentials_or_bin_writable() {
        // The single most important property of the rust profile. Granting all of
        // ~/.cargo would be one line shorter and would hand a sandboxed command the
        // crates.io token and write access to every binary on the user's PATH.
        let rules = resolved_rules(&names(), true);
        for denied in deny_write_paths(&names()) {
            let writable = rules
                .iter()
                .filter(|r| r.access.is_write())
                .any(|r| denied.starts_with(&r.path));
            assert!(
                !writable,
                "{} must never be writable — it holds credentials or executables",
                denied.display()
            );
        }
    }

    #[test]
    fn node_and_go_toolchains_are_read_execute_never_write() {
        let rules = resolved_rules(&names(), true);
        for r in rules
            .iter()
            .filter(|r| r.profile == "node" || r.profile == "go")
        {
            assert_eq!(
                r.access,
                ProfileAccess::Rx,
                "{} grants more than read+execute",
                r.path.display()
            );
        }
    }

    #[test]
    fn disabling_a_profile_removes_exactly_its_rules() {
        // The point of profiles being data: someone hardening a machine can say
        // "not that one" without patching the binary.
        let without_rust: Vec<String> = names().into_iter().filter(|n| n != "rust").collect();
        let rules = resolved_rules(&without_rust, true);
        assert!(
            !rules.iter().any(|r| r.profile == "rust"),
            "a disabled profile contributes nothing"
        );
        assert!(
            rules.iter().any(|r| r.profile == "node"),
            "…and the others are untouched"
        );

        // And disabling everything really does mean everything.
        assert!(resolved_rules(&[], true).is_empty());
    }

    #[test]
    fn no_package_cache_write_downgrades_caches_to_read_execute() {
        // `--no-package-cache-write` means "the strictest isolation", not "break
        // the toolchain": the compiler must still be runnable, only its caches go
        // read-only.
        let rules = resolved_rules(&names(), false);
        assert!(
            !rules.iter().any(|r| r.access.is_write()),
            "no rule is writable when package_cache_write is off"
        );
        let cargo = super::resolve_path("${CARGO_HOME:-~/.cargo}").unwrap();
        assert!(
            rules
                .iter()
                .any(|r| r.path == cargo && r.access == ProfileAccess::Rx),
            "the toolchain stays executable — turning off cache writes must not \
             make `cargo` itself unrunnable"
        );
        assert!(
            !rules.iter().any(|r| r.precreate),
            "nothing is pre-created when nothing is writable"
        );
    }

    #[test]
    fn an_unknown_profile_name_is_ignored_not_fatal() {
        // A settings file naming a profile from a newer ahma must not stop an older
        // one from starting.
        let rules = resolved_rules(&["rust".into(), "from-the-future".into()], true);
        assert!(rules.iter().any(|r| r.profile == "rust"));
    }

    #[test]
    fn env_vars_are_expanded_with_defaults() {
        // The `${VAR:-default}` form is what makes a relocated toolchain work. A
        // profile that hard-coded ~/.cargo would grant nothing at all on a machine
        // with CARGO_HOME set elsewhere — and would do it silently.
        assert_eq!(
            expand_vars("${AHMA_PROFILE_TEST_UNSET:-fallback}"),
            "fallback"
        );
        assert_eq!(expand_vars("a/${AHMA_PROFILE_TEST_UNSET:-b}/c"), "a/b/c");
        assert_eq!(expand_vars("no vars here"), "no vars here");
        // An unterminated `${` is emitted literally rather than swallowing the path.
        assert_eq!(expand_vars("${OPEN"), "${OPEN");
    }

    #[test]
    fn access_levels_mean_what_they_say() {
        // Landlock's read set excludes Execute, so conflating rx with ro would give
        // a sandbox that can read the compiler but not start it.
        assert!(!ProfileAccess::Ro.is_execute());
        assert!(ProfileAccess::Rx.is_execute());
        assert!(ProfileAccess::Rw.is_execute());
        assert!(!ProfileAccess::Ro.is_write());
        assert!(!ProfileAccess::Rx.is_write());
        assert!(ProfileAccess::Rw.is_write());
    }

    #[test]
    fn builtin_profile_names_match_the_settings_default() {
        // `ahma_common` sits below `ahma_mcp` and so cannot import the profile
        // data; it spells the default list out. If the two drift, a profile ships
        // that nothing enables — silently, and with no error anywhere.
        let from_data = default_profile_names();
        let from_settings = ahma_common::config::SandboxSettings::default().profiles;
        assert_eq!(
            from_data, from_settings,
            "[sandbox] profiles default must list exactly the shipped profiles"
        );
    }

    #[test]
    fn each_toolchain_profile_declares_the_hosts_its_toolchain_needs() {
        // The host list is the network analogue of the path rules: without it,
        // `--restrict-network` denies cargo its registry and the operator has to
        // reverse-engineer a CDN topology before their first build succeeds.
        let hosts = profile_hosts(&names());
        let of = |p: &str| -> Vec<String> {
            hosts
                .iter()
                .filter(|h| h.profile == p)
                .map(|h| h.pattern.as_str())
                .collect()
        };
        assert!(of("rust").contains(&"index.crates.io".to_string()));
        assert!(of("rust").contains(&"static.crates.io".to_string()));
        assert!(of("node").contains(&"registry.npmjs.org".to_string()));
        assert!(of("go").contains(&"proxy.golang.org".to_string()));
        assert!(
            of("common").is_empty(),
            "a shared cache directory is not an ecosystem and reaches nothing of its own; \
             a host here would be granted to everyone, since `common` is on by default"
        );
    }

    #[test]
    fn a_profiles_hosts_travel_with_the_profile() {
        // Same refusability property the path rules have: one name removed from
        // `[sandbox] profiles` takes exactly that profile's hosts with it.
        let without_node: Vec<String> = names().into_iter().filter(|n| n != "node").collect();
        let hosts = profile_hosts(&without_node);
        assert!(!hosts.iter().any(|h| h.profile == "node"));
        assert!(hosts.iter().any(|h| h.profile == "rust"));
        assert!(profile_hosts(&[]).is_empty());
    }

    #[test]
    fn no_shipped_profile_can_grant_blanket_egress() {
        // A profile is enabled by default, so a `*` in one would be a default-on
        // blanket grant delivered by a data file — the invisible, unrefusable
        // grant this whole system exists to eliminate. The parser refuses it; this
        // asserts the shipped data never tries.
        for h in profile_hosts(&names()) {
            assert_ne!(h.pattern, HostPattern::Any, "{} grants '*'", h.profile);
            assert!(
                !h.reason.trim().is_empty(),
                "{} must say why it needs {}",
                h.profile,
                h.pattern
            );
        }
    }

    #[test]
    fn host_declarations_do_not_disturb_the_path_rules() {
        // Adding hosts to the profile data must be purely additive: the paths a
        // machine grants are the same before and after, or this "extension"
        // quietly changed the filesystem sandbox.
        let cargo = super::resolve_path("${CARGO_HOME:-~/.cargo}").unwrap();
        let rules = resolved_rules(&names(), true);
        assert_eq!(
            rules.iter().find(|r| r.path == cargo).map(|r| r.access),
            Some(ProfileAccess::Rx)
        );
        assert!(
            rules
                .iter()
                .any(|r| r.path == cargo.join("registry") && r.access == ProfileAccess::Rw)
        );
    }

    #[test]
    fn macos_discloses_that_reads_are_unrestricted() {
        // R-PERM.5.1: a limitation the user cannot see is one they cannot
        // compensate for.
        if cfg!(target_os = "macos") {
            let text = macos_read_disclosure().expect("macOS must disclose that reads are open");
            assert!(
                text.contains("reads"),
                "the disclosure says what is not protected"
            );
            assert!(text.to_lowercase().contains("write"), "…and what still is");
        } else {
            assert!(
                macos_read_disclosure().is_none(),
                "other platforms scope reads properly and have nothing to disclose"
            );
        }
    }
}
