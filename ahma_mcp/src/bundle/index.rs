//! Parser for the first-party bundle index format.
//!
//! The bundle index is a JSON file listing known MTDF tool bundles with their
//! names, versions, descriptions, and content hashes.
//!
//! **Not yet wired to anything.** This module previously documented itself as
//! "Ahma checks the index when loading third-party bundles and rejects any bundle
//! not in the index unless `--allow-unsigned` is set". No such check exists, no
//! `--allow-unsigned` flag exists, and [`BundleIndex`] has no caller outside this
//! file — it is a parser waiting for the gate SPEC.md §11 describes. Stated here
//! rather than left implied, because a reader who takes the old wording at face
//! value concludes ahma has a trust boundary around bundle loading that it does
//! not have.
//!
//! ## Index format
//!
//! ```json
//! {
//!   "version": 1,
//!   "bundles": [
//!     {
//!       "name": "rust",
//!       "version": "1.0.0",
//!       "description": "Rust/Cargo build tools",
//!       "author": "ahma-project",
//!       "url": "https://github.com/paulirotta/ahma/releases/...",
//!       "sha256": "aabbcc..."
//!     }
//!   ]
//! }
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

// ─────────────────────────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────────────────────────

/// A single entry in the bundle index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleEntry {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub url: Option<String>,
    /// Content hash of the bundle archive.
    pub sha256: String,
}

/// The complete bundle index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleIndex {
    pub version: u32,
    pub bundles: Vec<BundleEntry>,
}

impl BundleIndex {
    /// Load a bundle index from a local JSON file.
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read bundle index: {}", path.display()))?;
        let index: BundleIndex =
            serde_json::from_str(&contents).context("Failed to parse bundle index JSON")?;
        Ok(index)
    }

    /// Load a bundle index from a URL (async — use `.await` in an async context).
    ///
    /// For CLI use, run this via `tokio::runtime::Handle::current().block_on(...)`.
    pub async fn load_from_url_async(url: &str) -> Result<Self> {
        let response = reqwest::get(url)
            .await
            .with_context(|| format!("Failed to fetch bundle index from {url}"))?;
        let index: BundleIndex = response
            .json()
            .await
            .context("Failed to parse bundle index from URL")?;
        Ok(index)
    }

    /// Look up a bundle by name.
    pub fn find(&self, name: &str) -> Option<&BundleEntry> {
        self.bundles.iter().find(|b| b.name == name)
    }

    /// Check whether a bundle name + sha256 pair appears in the index.
    pub fn is_trusted(&self, name: &str, sha256: &str) -> bool {
        self.bundles
            .iter()
            .any(|b| b.name == name && b.sha256 == sha256)
    }

    /// Return the built-in first-party index (compiled into the binary).
    pub fn builtin() -> Self {
        let json = include_str!("../../../assets/bundle-index.json");
        serde_json::from_str(json).unwrap_or(Self {
            version: 1,
            bundles: vec![],
        })
    }

    /// Save the index to a local file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json.as_bytes())
            .with_context(|| format!("Failed to write bundle index: {}", path.display()))?;
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_index() -> BundleIndex {
        BundleIndex {
            version: 1,
            bundles: vec![BundleEntry {
                name: "rust".into(),
                version: "1.0.0".into(),
                description: "Rust tools".into(),
                author: "ahma-project".into(),
                url: None,
                sha256: "aabbccdd".into(),
            }],
        }
    }

    #[test]
    fn find_known_bundle() {
        let idx = sample_index();
        assert!(idx.find("rust").is_some());
        assert!(idx.find("python").is_none());
    }

    #[test]
    fn is_trusted_checks_name_and_hash() {
        let idx = sample_index();
        assert!(idx.is_trusted("rust", "aabbccdd"));
        assert!(!idx.is_trusted("rust", "wronghash"));
        assert!(!idx.is_trusted("python", "aabbccdd"));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("index.json");
        let idx = sample_index();
        idx.save(&path).unwrap();

        let loaded = BundleIndex::load_from_file(&path).unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.bundles.len(), 1);
        assert_eq!(loaded.bundles[0].name, "rust");
    }

    #[test]
    fn builtin_index_is_well_formed() {
        let idx = BundleIndex::builtin();
        // version field is the compiled-in v1 schema
        assert_eq!(idx.version, 1);
        // Built-in index must not be empty (the unwrap_or empty fallback is the failure case)
        assert!(!idx.bundles.is_empty(), "builtin index should list bundles");

        // Known first-party bundles are present and well-formed.
        for name in ["rust", "python", "git", "fileutils", "github"] {
            let entry = idx
                .find(name)
                .unwrap_or_else(|| panic!("builtin index should contain `{name}`"));
            assert_eq!(entry.name, name);
            assert!(!entry.version.is_empty());
            assert!(!entry.description.is_empty());
            assert_eq!(entry.author, "ahma-project");
            assert_eq!(entry.sha256, "builtin");
            assert!(entry.url.is_none());
        }

        // Spot-check a specific description so the field is load-bearing.
        assert_eq!(
            idx.find("rust").unwrap().description,
            "Rust/Cargo build and test tools"
        );
    }

    #[test]
    fn load_from_file_missing_path_is_err() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does_not_exist.json");
        let result = BundleIndex::load_from_file(&missing);
        assert!(result.is_err(), "loading a non-existent path must error");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("Failed to read bundle index"),
            "error should carry read context, got: {msg}"
        );
    }

    #[test]
    fn load_from_file_malformed_json_is_err() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("garbage.json");
        std::fs::write(&path, b"{ this is not valid json ]").unwrap();
        let result = BundleIndex::load_from_file(&path);
        assert!(result.is_err(), "malformed JSON must error");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("Failed to parse bundle index JSON"),
            "error should carry parse context, got: {msg}"
        );
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let tmp = TempDir::new().unwrap();
        // Nested path whose parent dirs do not yet exist -> exercises create_dir_all branch.
        let path = tmp.path().join("a").join("b").join("c").join("index.json");
        let idx = sample_index();
        idx.save(&path).unwrap();

        assert!(path.exists(), "save should have created the nested file");
        let loaded = BundleIndex::load_from_file(&path).unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.bundles.len(), 1);
        assert_eq!(loaded.bundles[0].name, "rust");
        assert_eq!(loaded.bundles[0].sha256, "aabbccdd");
    }

    #[test]
    fn find_and_is_trusted_negative_branches() {
        let idx = sample_index();
        // find returns None for an unknown name.
        assert!(idx.find("nonexistent").is_none());
        // is_trusted false for unknown name AND for known name with wrong hash.
        assert!(!idx.is_trusted("nonexistent", "aabbccdd"));
        assert!(!idx.is_trusted("rust", "00000000"));
        // Sanity: the positive case still holds.
        assert!(idx.is_trusted("rust", "aabbccdd"));
    }
}
