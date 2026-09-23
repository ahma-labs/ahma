//! # Bundle Registry
//!
//! Maps CLI bundle flags to tool config names. This is the single source of truth
//! for progressive disclosure: when a user calls `activate_tools reveal <bundle>`,
//! the registry determines which tool configs to expose.

use std::collections::HashSet;

/// Metadata for a single tool bundle.
#[derive(Debug, Clone)]
pub struct BundleInfo {
    /// Human-facing bundle name (matches CLI flag minus `--`).
    pub name: &'static str,
    /// The `ToolConfig.name` value produced by the bundle's JSON.
    pub config_tool_name: &'static str,
    /// Short description shown by `activate_tools list`.
    pub description: &'static str,
    /// Action-oriented hint for the AI. Appears in the `activate_tools` description
    /// to tell the AI exactly WHEN it should activate this bundle.
    pub ai_hint: &'static str,
}

/// All known bundles. Order determines listing order.
pub const BUNDLES: &[BundleInfo] = &[
    BundleInfo {
        name: "fileutils",
        config_tool_name: "file-tools",
        description: "File operations — ls/dir, cp, mv, rm, grep, find, diff",
        ai_hint: "Need to search, copy, move, delete, or diff files? Activate 'fileutils' for ls, cp, mv, rm, grep, find, and diff.",
    },
    BundleInfo {
        name: "github",
        config_tool_name: "gh",
        description: "GitHub CLI — pull requests, Actions, caches, workflows",
        ai_hint: "Need to create PRs, check CI status, manage GitHub Actions, or work with GitHub? Activate 'github' for the gh CLI.",
    },
    BundleInfo {
        name: "git",
        config_tool_name: "git",
        description: "Git version control — status, add, commit, push, log",
        ai_hint: "Need to commit, push, check status, view logs, or manage branches? Activate 'git' for git version control commands.",
    },
    BundleInfo {
        name: "python",
        config_tool_name: "python",
        description: "Python interpreter — scripts, inline code, modules",
        ai_hint: "Need to run Python scripts, execute inline Python code, or manage Python modules? Activate 'python' for the Python interpreter.",
    },
    BundleInfo {
        name: "simplify",
        config_tool_name: "simplify",
        description: "Code complexity analyzer — reports hotspots with AI fix suggestions",
        ai_hint: "Need to analyze code complexity, find hotspots, or get AI-powered simplification suggestions? Activate 'simplify'.",
    },
];

/// Returns the set of `config_tool_name` values for bundles that are loaded
/// (i.e., their configs are present in the config map).
pub fn loaded_bundle_names(config_keys: &HashSet<String>) -> Vec<&'static BundleInfo> {
    BUNDLES
        .iter()
        .filter(|b| config_keys.contains(b.config_tool_name))
        .collect()
}

/// Looks up a bundle by its human-facing name.
pub fn find_bundle(name: &str) -> Option<&'static BundleInfo> {
    BUNDLES.iter().find(|b| b.name == name)
}

/// Returns the config tool name for a bundle by its human-facing name.
pub fn bundle_config_name(name: &str) -> Option<&'static str> {
    find_bundle(name).map(|b| b.config_tool_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Every entry the registry is expected to ship, as (name, config_tool_name).
    const EXPECTED: &[(&str, &str)] = &[
        ("fileutils", "file-tools"),
        ("github", "gh"),
        ("git", "git"),
        ("python", "python"),
        ("simplify", "simplify"),
    ];

    #[test]
    fn bundles_constant_is_non_empty() {
        assert!(!BUNDLES.is_empty(), "BUNDLES must not be empty");
    }

    #[test]
    fn bundles_contains_every_known_bundle_with_correct_config_name() {
        for (name, config) in EXPECTED {
            let found = BUNDLES
                .iter()
                .find(|b| b.name == *name)
                .unwrap_or_else(|| panic!("bundle '{name}' missing from BUNDLES"));
            assert_eq!(
                found.config_tool_name, *config,
                "bundle '{name}' has wrong config_tool_name"
            );
        }
    }

    #[test]
    fn bundles_count_matches_expected() {
        assert_eq!(
            BUNDLES.len(),
            EXPECTED.len(),
            "BUNDLES length changed; update EXPECTED to match"
        );
    }

    #[test]
    fn bundle_fields_are_populated() {
        for b in BUNDLES {
            assert!(!b.name.is_empty(), "name must be non-empty");
            assert!(
                !b.config_tool_name.is_empty(),
                "config_tool_name must be non-empty"
            );
            assert!(!b.description.is_empty(), "description must be non-empty");
            assert!(!b.ai_hint.is_empty(), "ai_hint must be non-empty");
        }
    }

    #[test]
    fn bundle_info_is_debug_and_clone() {
        // Exercise the derived Debug + Clone impls.
        let original = &BUNDLES[0];
        let cloned = original.clone();
        assert_eq!(cloned.name, original.name);
        assert_eq!(cloned.config_tool_name, original.config_tool_name);
        let dbg = format!("{cloned:?}");
        assert!(
            dbg.contains(original.name),
            "Debug output should include name"
        );
    }

    #[test]
    fn loaded_bundle_names_returns_matching_subset() {
        let mut keys = HashSet::new();
        keys.insert("gh".to_string());
        keys.insert("git".to_string());

        let loaded = loaded_bundle_names(&keys);
        let names: Vec<&str> = loaded.iter().map(|b| b.name).collect();

        assert_eq!(loaded.len(), 2, "expected exactly two matching bundles");
        assert!(
            names.contains(&"github"),
            "gh key should yield the 'github' bundle"
        );
        assert!(names.contains(&"git"), "git key should yield 'git' bundle");
        assert!(
            !names.contains(&"python"),
            "python should not be loaded when its key is absent"
        );
    }

    #[test]
    fn loaded_bundle_names_empty_set_returns_empty() {
        let keys: HashSet<String> = HashSet::new();
        let loaded = loaded_bundle_names(&keys);
        assert!(loaded.is_empty(), "empty key set must yield no bundles");
    }

    #[test]
    fn loaded_bundle_names_unrelated_key_returns_empty() {
        let mut keys = HashSet::new();
        keys.insert("does-not-exist".to_string());
        // A config_tool_name is "file-tools", so the bundle NAME "fileutils"
        // must NOT match a key lookup.
        keys.insert("fileutils".to_string());

        let loaded = loaded_bundle_names(&keys);
        assert!(
            loaded.is_empty(),
            "unrelated keys (incl. bundle name, not config name) must yield no bundles"
        );
    }

    #[test]
    fn loaded_bundle_names_matches_on_config_name_not_human_name() {
        let mut keys = HashSet::new();
        keys.insert("file-tools".to_string()); // config_tool_name for the "fileutils" bundle
        let loaded = loaded_bundle_names(&keys);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "fileutils");
    }

    #[test]
    fn find_bundle_known_name_returns_some() {
        let b = find_bundle("github").expect("'github' bundle should exist");
        assert_eq!(b.name, "github");
        assert_eq!(b.config_tool_name, "gh");
    }

    #[test]
    fn find_bundle_unknown_name_returns_none() {
        assert!(find_bundle("nonexistent").is_none());
        // Looking up by config name (not human name) must also miss.
        assert!(find_bundle("gh").is_none());
        // `rust` was removed (its cargo.json went with it); it must not be
        // listed as if it could load.
        assert!(find_bundle("rust").is_none());
    }

    #[test]
    fn bundle_config_name_known_returns_config_tool_name() {
        for (name, config) in EXPECTED {
            assert_eq!(
                bundle_config_name(name),
                Some(*config),
                "bundle_config_name('{name}') mismatch"
            );
        }
    }

    #[test]
    fn bundle_config_name_unknown_returns_none() {
        assert!(bundle_config_name("nonexistent").is_none());
        assert!(bundle_config_name("").is_none());
    }
}
