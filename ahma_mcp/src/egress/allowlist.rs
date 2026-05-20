//! Per-vault domain allowlist for the egress proxy.
//!
//! The allowlist is stored as a plain-text file (`egress.allowlist`) inside
//! the vault directory.  Each non-empty, non-comment line is a domain pattern.
//!
//! ## Matching rules
//!
//! - Exact domain: `api.openai.com` matches only `api.openai.com`.
//! - Subdomain wildcard: `*.openai.com` matches any direct subdomain of `openai.com`.
//! - All traffic (dangerous): `*` matches everything — use only for development.
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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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
    patterns: Vec<AllowPattern>,
}

#[derive(Debug, Clone)]
enum AllowPattern {
    /// Matches exactly the given domain (case-insensitive).
    Exact(String),
    /// Matches `*.suffix` — any single-level subdomain of `suffix`.
    Wildcard(String),
    /// Matches everything.
    Any,
}

impl EgressAllowlist {
    /// Create an empty (deny-all) allowlist.
    pub fn deny_all() -> Self {
        Self::default()
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

    /// Parse allowlist from a string (used for tests and in-memory configs).
    pub fn from_str(s: &str) -> Self {
        let mut patterns = vec![];
        for line in s.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if trimmed == "*" {
                patterns.push(AllowPattern::Any);
            } else if let Some(suffix) = trimmed.strip_prefix("*.") {
                patterns.push(AllowPattern::Wildcard(suffix.to_ascii_lowercase()));
            } else {
                patterns.push(AllowPattern::Exact(trimmed.to_ascii_lowercase()));
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
        let lower = domain.to_ascii_lowercase();
        for pattern in &self.patterns {
            match pattern {
                AllowPattern::Any => return true,
                AllowPattern::Exact(d) => {
                    if lower == *d {
                        return true;
                    }
                }
                AllowPattern::Wildcard(suffix) => {
                    // *.example.com matches ONLY direct single-level subdomains:
                    //   api.example.com  → YES (prefix "api", no dots)
                    //   deep.api.example.com → NO  (prefix "deep.api", contains a dot)
                    //   example.com → NO (no prefix at all)
                    if let Some(rest) = lower.strip_suffix(suffix.as_str()) {
                        if let Some(label) = rest.strip_suffix('.') {
                            if !label.is_empty() && !label.contains('.') {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// Add a domain pattern to this allowlist.
    pub fn add(&mut self, pattern: &str) {
        let trimmed = pattern.trim();
        if trimmed == "*" {
            self.patterns.push(AllowPattern::Any);
        } else if let Some(suffix) = trimmed.strip_prefix("*.") {
            self.patterns
                .push(AllowPattern::Wildcard(suffix.to_ascii_lowercase()));
        } else {
            self.patterns
                .push(AllowPattern::Exact(trimmed.to_ascii_lowercase()));
        }
    }

    /// Render to the file format.
    fn to_file_string(&self) -> String {
        let mut out = String::from("# ahma egress allowlist\n# One domain pattern per line.\n\n");
        for p in &self.patterns {
            match p {
                AllowPattern::Any => out.push_str("*\n"),
                AllowPattern::Exact(d) => out.push_str(&format!("{d}\n")),
                AllowPattern::Wildcard(s) => out.push_str(&format!("*.{s}\n")),
            }
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
