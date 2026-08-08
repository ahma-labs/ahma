//! The domain allowlist the egress proxy forwards on.
//!
//! Two producers feed the same list:
//!
//! * the operator's `[network] allow` (and a per-vault `egress.allowlist` file), and
//! * the hosts contributed by enabled sandbox profiles ([`super::host_grants`]).
//!
//! Both are parsed and matched by [`HostPattern`] — see that module for the exact
//! matching semantics. This type is the set; the pattern type is the decision.
//!
//! ## File format
//!
//! ```text
//! # ahma egress allowlist
//! # Lines starting with '#' are comments; blank lines are ignored.
//!
//! api.openai.com
//! *.anthropic.com
//! ```
//!
//! A line that is not a well-formed host pattern is **dropped with a warning**,
//! not coerced. Reading `https://crates.io` as the exact host `https://crates.io`
//! would produce a rule that never fires while looking like it works, and a
//! silently-inert allowlist entry is how an operator ends up believing egress is
//! permitted when it is not — or the reverse.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::host_pattern::HostPattern;

// ─────────────────────────────────────────────────────────────────────────────
// EgressAllowlist
// ─────────────────────────────────────────────────────────────────────────────

/// A set of domain patterns that the egress proxy will forward.
///
/// Domains absent from the list are silently dropped (408 Request Timeout
/// returned to the subprocess, indistinguishable from a real network timeout
/// to prevent information leakage about the allowlist contents).
#[derive(Debug, Clone, Default)]
pub struct EgressAllowlist {
    patterns: Vec<HostPattern>,
}

impl EgressAllowlist {
    /// Create an empty (deny-all) allowlist.
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// Build from already-parsed patterns — the path the profile-host union takes
    /// ([`super::host_grants::EgressGrants::allowlist`]), where the strings were
    /// validated at their source and re-parsing them would be a second chance to
    /// disagree with the first.
    pub fn from_patterns(patterns: impl IntoIterator<Item = HostPattern>) -> Self {
        Self {
            patterns: patterns.into_iter().collect(),
        }
    }

    /// Load an allowlist from a file.
    ///
    /// If the file does not exist, returns a deny-all allowlist.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::deny_all());
        }
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read egress allowlist: {}", path.display()))?;
        Ok(Self::from_str(&contents))
    }

    /// Parse an allowlist from newline-separated patterns.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        let mut patterns = vec![];
        for line in s.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            match HostPattern::parse(trimmed) {
                Ok(p) => patterns.push(p),
                Err(e) => tracing::warn!(
                    "ignoring egress allowlist entry '{trimmed}': {e}. It grants nothing; \
                     fix or remove it."
                ),
            }
        }
        Self { patterns }
    }

    /// Save this allowlist to a file (creates parent directories if needed).
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = self.to_file_string();
        std::fs::write(path, contents.as_bytes())
            .with_context(|| format!("Failed to write egress allowlist: {}", path.display()))?;
        Ok(())
    }

    /// Return `true` if `domain` is permitted by this allowlist.
    pub fn allows(&self, domain: &str) -> bool {
        self.patterns.iter().any(|p| p.matches(domain))
    }

    /// Add a domain pattern. A malformed pattern is refused with a warning rather
    /// than stored as an entry that can never match.
    pub fn add(&mut self, pattern: &str) {
        match HostPattern::parse(pattern) {
            Ok(p) => self.patterns.push(p),
            Err(e) => tracing::warn!("ignoring egress allowlist entry '{pattern}': {e}"),
        }
    }

    /// The patterns in this list, in canonical text form.
    pub fn entries(&self) -> Vec<String> {
        self.patterns.iter().map(HostPattern::as_str).collect()
    }

    /// Render to the file format.
    fn to_file_string(&self) -> String {
        let mut out = String::from("# ahma egress allowlist\n# One domain pattern per line.\n\n");
        for p in &self.patterns {
            out.push_str(&p.as_str());
            out.push('\n');
        }
        out
    }

    /// Return the path at which a vault's allowlist file should live.
    pub fn vault_path(vault_root: &Path) -> PathBuf {
        vault_root.join("egress.allowlist")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn al(src: &str) -> EgressAllowlist {
        EgressAllowlist::from_str(src)
    }

    #[test]
    fn deny_all_blocks_everything() {
        let list = EgressAllowlist::deny_all();
        assert!(!list.allows("api.openai.com"));
        assert!(!list.allows("localhost"));
    }

    #[test]
    fn exact_match_allows_domain() {
        let list = al("api.openai.com");
        assert!(list.allows("api.openai.com"));
        assert!(list.allows("API.OPENAI.COM"), "case insensitive");
        assert!(!list.allows("openai.com"), "parent domain not allowed");
        assert!(!list.allows("other.openai.com"), "sibling not allowed");
    }

    #[test]
    fn wildcard_allows_subdomains() {
        let list = al("*.openai.com");
        assert!(list.allows("api.openai.com"));
        assert!(list.allows("beta.openai.com"));
        assert!(!list.allows("openai.com"), "base domain not matched");
        assert!(
            !list.allows("deep.api.openai.com"),
            "two levels not matched"
        );
        assert!(
            !list.allows("evilopenai.com"),
            "no label boundary, no match — see host_pattern for the full case"
        );
    }

    #[test]
    fn star_allows_everything() {
        let list = al("*");
        assert!(list.allows("anything.example.com"));
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let list = al("# comment\n\napi.openai.com\n# another comment\n");
        assert!(list.allows("api.openai.com"));
        assert!(!list.allows("other.com"));
    }

    #[test]
    fn a_malformed_entry_is_dropped_not_coerced() {
        // A URL pasted into the allowlist must not become an exact host that can
        // never match: the operator would read the file, see their domain, and
        // conclude egress works.
        let list = al("https://crates.io\ncrates.io\n");
        assert_eq!(
            list.entries(),
            vec!["crates.io"],
            "only the well-formed entry survives"
        );
        assert!(list.allows("crates.io"));
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("egress.allowlist");
        let mut list = EgressAllowlist::deny_all();
        list.add("api.openai.com");
        list.add("*.anthropic.com");
        list.save(&path).unwrap();

        let loaded = EgressAllowlist::load(&path).unwrap();
        assert!(loaded.allows("api.openai.com"));
        assert!(loaded.allows("claude.anthropic.com"));
        assert!(!loaded.allows("google.com"));
    }
}
