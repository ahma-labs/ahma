//! Egress policy for task vaults.
//!
//! By default a vault only permits outbound connections to loopback (`127.*`,
//! `::1`).  An operator can widen this by creating an `egress.allowlist` file
//! in the vault root.  Each non-blank, non-comment line is an allowed hostname
//! pattern:
//!
//! ```text
//! # Allow the local Ollama instance
//! localhost
//! 127.0.0.1
//!
//! # Allow a specific remote vLLM host
//! gpu-node-01.internal
//!
//! # Allow all sub-domains of a company domain
//! *.acme.internal
//! ```
//!
//! Wildcard `*` by itself allows all hosts.  Use with caution.
//!
//! ## Enforcement note
//!
//! This module defines the **policy** only.  The kernel-level sandbox (Landlock
//! on Linux, `sandbox-exec` on macOS) does not yet enforce TCP egress at the
//! network layer — that requires the network-restriction LSM hooks or a proxy.
//! Until kernel enforcement is wired in, the policy is checked by the
//! [`ahma_llm_monitor`] and [`ahma_decompose`] clients before each request, and
//! an [`crate::audit::AuditEventKind::EgressDecision`] event is emitted.

use std::path::Path;

use tracing::{debug, warn};

/// Allowlist-based egress policy for a vault.
///
/// Created via [`EgressPolicy::from_vault_path`] or [`EgressPolicy::loopback_only`].
#[derive(Debug, Clone)]
pub struct EgressPolicy {
    /// Allowed host patterns.  Loopback is always implicitly allowed.
    patterns: Vec<Pattern>,
    /// If `true`, all hosts are allowed (the allowlist contained `*`).
    allow_all: bool,
}

#[derive(Debug, Clone)]
enum Pattern {
    /// Exact hostname or IP match.
    Exact(String),
    /// Wildcard `*.suffix` — matches any `<label>.suffix`.
    Suffix(String),
}

impl EgressPolicy {
    /// Load the egress policy from `<vault_root>/egress.allowlist`.
    ///
    /// Returns [`loopback_only`](Self::loopback_only) if the file does not exist.
    pub fn from_vault_path(vault_root: &Path) -> Self {
        let path = vault_root.join("egress.allowlist");
        if !path.exists() {
            debug!(
                "No egress.allowlist in {}; defaulting to loopback-only",
                vault_root.display()
            );
            return Self::loopback_only();
        }
        match std::fs::read_to_string(&path) {
            Ok(contents) => Self::parse(&contents),
            Err(e) => {
                warn!(
                    "Failed to read egress.allowlist at {}: {e}; defaulting to loopback-only",
                    path.display()
                );
                Self::loopback_only()
            }
        }
    }

    /// Deny everything except loopback.
    pub fn loopback_only() -> Self {
        Self {
            patterns: Vec::new(),
            allow_all: false,
        }
    }

    /// Allow all hosts (use only in trusted/offline environments).
    pub fn allow_all() -> Self {
        Self {
            patterns: Vec::new(),
            allow_all: true,
        }
    }

    /// Return `true` if outbound connections to `host` are permitted.
    ///
    /// Loopback addresses are always allowed regardless of the policy.
    pub fn allows(&self, host: &str) -> bool {
        if is_loopback(host) {
            return true;
        }
        if self.allow_all {
            return true;
        }
        self.patterns.iter().any(|p| p.matches(host))
    }

    fn parse(contents: &str) -> Self {
        let mut patterns = Vec::new();
        let mut allow_all = false;

        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "*" {
                allow_all = true;
                warn!(
                    "egress.allowlist contains '*' — all outbound hosts are allowed. \
                     Remove this entry for production use."
                );
                break;
            }
            if let Some(suffix) = line.strip_prefix("*.") {
                patterns.push(Pattern::Suffix(suffix.to_ascii_lowercase()));
            } else {
                patterns.push(Pattern::Exact(line.to_ascii_lowercase()));
            }
        }

        Self {
            patterns,
            allow_all,
        }
    }
}

impl Pattern {
    fn matches(&self, host: &str) -> bool {
        let host_lower = host.to_ascii_lowercase();
        match self {
            Pattern::Exact(exact) => &host_lower == exact,
            Pattern::Suffix(suffix) => {
                // `*.acme.internal` matches `foo.acme.internal` but not `acme.internal`.
                host_lower.ends_with(suffix.as_str())
                    && host_lower.len() > suffix.len() + 1
                    && host_lower.as_bytes()[host_lower.len() - suffix.len() - 1] == b'.'
            }
        }
    }
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host.starts_with("127.")
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::TempDir;

    fn policy_from_str(s: &str) -> EgressPolicy {
        EgressPolicy::parse(s)
    }

    #[test]
    fn loopback_is_always_allowed() {
        let policy = EgressPolicy::loopback_only();
        assert!(policy.allows("localhost"));
        assert!(policy.allows("127.0.0.1"));
        assert!(policy.allows("127.0.0.2"));
        assert!(policy.allows("::1"));
        assert!(!policy.allows("google.com"));
    }

    #[test]
    fn exact_match() {
        let policy = policy_from_str("gpu-node-01.internal");
        assert!(policy.allows("gpu-node-01.internal"));
        assert!(!policy.allows("gpu-node-02.internal"));
    }

    #[test]
    fn wildcard_suffix_match() {
        let policy = policy_from_str("*.acme.internal");
        assert!(policy.allows("foo.acme.internal"));
        assert!(policy.allows("bar.acme.internal"));
        // Must not match the bare suffix itself.
        assert!(!policy.allows("acme.internal"));
        // Must not match unrelated domain.
        assert!(!policy.allows("evil.com"));
    }

    #[test]
    fn allow_all_wildcard() {
        let policy = policy_from_str("*");
        assert!(policy.allows("google.com"));
        assert!(policy.allows("evil.com"));
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let policy = policy_from_str(
            "# This is a comment\n\ngpu-node.internal\n# another comment\n",
        );
        assert!(policy.allows("gpu-node.internal"));
        assert!(!policy.allows("other.internal"));
    }

    #[test]
    fn case_insensitive() {
        let policy = policy_from_str("GPU-NODE.INTERNAL");
        assert!(policy.allows("gpu-node.internal"));
        assert!(policy.allows("GPU-NODE.INTERNAL"));
    }

    #[test]
    fn loads_from_vault_path() {
        let tmp = TempDir::new().unwrap();
        let allowlist = tmp.path().join("egress.allowlist");
        let mut f = std::fs::File::create(&allowlist).unwrap();
        writeln!(f, "# allow Ollama").unwrap();
        writeln!(f, "my-gpu.internal").unwrap();

        let policy = EgressPolicy::from_vault_path(tmp.path());
        assert!(policy.allows("my-gpu.internal"));
        assert!(!policy.allows("google.com"));
    }

    #[test]
    fn missing_allowlist_gives_loopback_only() {
        let tmp = TempDir::new().unwrap();
        let policy = EgressPolicy::from_vault_path(tmp.path());
        assert!(policy.allows("127.0.0.1"));
        assert!(!policy.allows("google.com"));
    }
}
