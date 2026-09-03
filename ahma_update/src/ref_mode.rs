//! Classify update refs into release vs Git branch builds.

use regex::Regex;
use std::sync::LazyLock;

static SEMVER_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^v?[0-9]+\.[0-9]+\.[0-9]+$").expect("valid semver regex"));

/// How an `ahma update [ref]` request should be fulfilled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateMode {
    /// Download the latest published GitHub release.
    LatestRelease,
    /// Download a specific semver release tag.
    TaggedRelease { tag: String },
    /// Build and install from a Git branch or non-semver ref.
    GitRef { branch: String },
}

/// Classify an optional ref argument.
pub fn classify_ref(reference: Option<&str>) -> UpdateMode {
    match reference.map(str::trim).filter(|s| !s.is_empty()) {
        None => UpdateMode::LatestRelease,
        Some(r) if SEMVER_TAG.is_match(r) => UpdateMode::TaggedRelease {
            tag: normalize_release_tag(r),
        },
        Some(r) => UpdateMode::GitRef {
            branch: r.to_string(),
        },
    }
}

/// Normalize `0.6.7` → `v0.6.7` for GitHub release API lookups.
pub fn normalize_release_tag(reference: &str) -> String {
    if reference.starts_with('v') {
        reference.to_string()
    } else {
        format!("v{reference}")
    }
}

/// Strip leading `v` for version comparison display.
pub fn release_version_from_tag(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_none_is_latest() {
        assert_eq!(classify_ref(None), UpdateMode::LatestRelease);
    }

    #[test]
    fn test_classify_semver_tag() {
        assert_eq!(
            classify_ref(Some("0.6.7")),
            UpdateMode::TaggedRelease {
                tag: "v0.6.7".to_string()
            }
        );
        assert_eq!(
            classify_ref(Some("v0.6.7")),
            UpdateMode::TaggedRelease {
                tag: "v0.6.7".to_string()
            }
        );
    }

    #[test]
    fn test_classify_branch_ref() {
        assert_eq!(
            classify_ref(Some("main")),
            UpdateMode::GitRef {
                branch: "main".to_string()
            }
        );
        assert_eq!(
            classify_ref(Some("feature/update")),
            UpdateMode::GitRef {
                branch: "feature/update".to_string()
            }
        );
    }

    #[test]
    fn test_normalize_release_tag() {
        assert_eq!(normalize_release_tag("0.6.7"), "v0.6.7");
        assert_eq!(normalize_release_tag("v0.6.7"), "v0.6.7");
    }
}
