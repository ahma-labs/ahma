//! First-party signed bundle index.
//!
//! The bundle index is a JSON file (published at a well-known URL) that lists
//! known MTDF tool bundles with their names, versions, descriptions, and content
//! hashes.  Ahma checks the index when loading third-party bundles and rejects
//! any bundle not in the index unless `--allow-unsigned` is set.
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
}
