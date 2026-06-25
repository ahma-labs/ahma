//! Detection of external **build-cache** directories (sccache, ccache) that live
//! outside the workspace and would otherwise be denied by the sandbox.
//!
//! ## Why this exists (P1a)
//!
//! A compiler cache speeds up rebuilds by storing artifacts in a shared
//! directory — `~/Library/Caches/Mozilla.sccache`, `~/.cache/sccache`, etc. That
//! directory is **outside** the workspace sandbox scope, so a sandboxed `cargo`
//! build that runs through `RUSTC_WRAPPER=sccache` is denied when it reads/writes
//! the cache (and on macOS can contaminate the target dir — see
//! [`super::build_diagnostics`]). The pre-existing flow only surfaced this *after*
//! a build failed, and a granted scope took effect only on the next restart.
//!
//! This module lets ahma **detect** those caches at startup so the scope can be
//! granted *before* the sandbox locks — but detection alone grants nothing. The
//! caller (CLI startup) folds a detected cache into the writable scope set only
//! when the user has explicitly opted in (`sandbox.trust_build_caches`); without
//! consent it merely logs an actionable hint. Secure by default: nothing widens
//! the sandbox without the human saying so.
//!
//! Detection is pure and environment-driven, with an injectable env accessor and
//! home directory so it is unit-testable without touching the real process
//! environment.

use std::path::{Path, PathBuf};

/// An external build cache detected as active in the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildCache {
    /// The cache tool, e.g. `"sccache"` or `"ccache"` — used in logs/hints.
    pub tool: &'static str,
    /// The cache directory that needs read+write sandbox access.
    pub dir: PathBuf,
    /// Where the directory came from — an explicit env var or the platform default.
    pub source: CacheSource,
}

/// Provenance of a detected cache directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSource {
    /// The path was taken from this explicitly-set environment variable.
    Env(&'static str),
    /// The path is the tool's platform default (no env override present).
    PlatformDefault,
}

/// Detect active external build caches from the real process environment.
pub fn detect() -> Vec<BuildCache> {
    detect_with(&|k| std::env::var(k).ok(), real_home().as_deref())
}

/// Testable core of [`detect`]. `get_env` resolves an environment variable by
/// name; `home` is the user's home directory (used to build platform defaults).
pub fn detect_with(
    get_env: &dyn Fn(&str) -> Option<String>,
    home: Option<&Path>,
) -> Vec<BuildCache> {
    let mut out = Vec::new();
    if let Some(c) = detect_sccache(get_env, home) {
        out.push(c);
    }
    if let Some(c) = detect_ccache(get_env) {
        out.push(c);
    }
    out
}

/// sccache is considered active when a rustc wrapper points at it **or**
/// `SCCACHE_DIR` is set. The cache dir is `SCCACHE_DIR` when present, else the
/// platform default.
fn detect_sccache(
    get_env: &dyn Fn(&str) -> Option<String>,
    home: Option<&Path>,
) -> Option<BuildCache> {
    let wrapper_is_sccache = ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"]
        .iter()
        .filter_map(|k| get_env(k))
        .any(|v| v.to_lowercase().contains("sccache"));
    let explicit_dir = get_env("SCCACHE_DIR").filter(|s| !s.is_empty());

    if !wrapper_is_sccache && explicit_dir.is_none() {
        return None;
    }

    let (dir, source) = match explicit_dir {
        Some(d) => (PathBuf::from(d), CacheSource::Env("SCCACHE_DIR")),
        None => (
            sccache_default_dir(get_env, home?)?,
            CacheSource::PlatformDefault,
        ),
    };
    Some(BuildCache {
        tool: "sccache",
        dir,
        source,
    })
}

/// ccache is detected **only** via an explicit `CCACHE_DIR`. Inferring it from a
/// default location or a probed binary risks false positives (ccache is usually
/// a C/C++ tool, rarely exercised by a sandboxed Rust build), and a runtime
/// denial is still diagnosed by [`super::build_diagnostics`]. Keying on the
/// explicit env var keeps detection conservative.
fn detect_ccache(get_env: &dyn Fn(&str) -> Option<String>) -> Option<BuildCache> {
    let dir = get_env("CCACHE_DIR").filter(|s| !s.is_empty())?;
    Some(BuildCache {
        tool: "ccache",
        dir: PathBuf::from(dir),
        source: CacheSource::Env("CCACHE_DIR"),
    })
}

/// The platform default sccache cache directory, honouring `XDG_CACHE_HOME`
/// (Linux) and `LOCALAPPDATA` (Windows) when present.
fn sccache_default_dir(get_env: &dyn Fn(&str) -> Option<String>, home: &Path) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(home.join("Library/Caches/Mozilla.sccache"))
    } else if cfg!(target_os = "windows") {
        // %LOCALAPPDATA%\Mozilla\sccache, falling back to the conventional path.
        let base = get_env("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Local"));
        Some(base.join("Mozilla").join("sccache"))
    } else {
        // Linux/other: $XDG_CACHE_HOME/sccache, else ~/.cache/sccache.
        let base = get_env("XDG_CACHE_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"));
        Some(base.join("sccache"))
    }
}

/// The user's home directory from the environment (`HOME`, then `USERPROFILE`).
fn real_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build an env accessor from a fixed map.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    const HOME: &str = "/home/tester";

    #[test]
    fn no_cache_env_detects_nothing() {
        let env = env_of(&[]);
        assert!(detect_with(&env, Some(Path::new(HOME))).is_empty());
    }

    #[test]
    fn rustc_wrapper_sccache_uses_platform_default() {
        let env = env_of(&[("RUSTC_WRAPPER", "/usr/local/bin/sccache")]);
        let caches = detect_with(&env, Some(Path::new(HOME)));
        let sccache = caches
            .iter()
            .find(|c| c.tool == "sccache")
            .expect("sccache detected");
        assert_eq!(sccache.source, CacheSource::PlatformDefault);
        // Platform-specific default path ends with `sccache`.
        assert!(
            sccache.dir.ends_with("sccache") || sccache.dir.ends_with("Mozilla.sccache"),
            "default dir should be an sccache path: {:?}",
            sccache.dir
        );
    }

    #[test]
    fn workspace_wrapper_also_triggers_sccache() {
        let env = env_of(&[("RUSTC_WORKSPACE_WRAPPER", "sccache")]);
        let caches = detect_with(&env, Some(Path::new(HOME)));
        assert!(caches.iter().any(|c| c.tool == "sccache"));
    }

    #[test]
    fn explicit_sccache_dir_overrides_default_and_marks_source() {
        let env = env_of(&[
            ("RUSTC_WRAPPER", "sccache"),
            ("SCCACHE_DIR", "/custom/sccache/cache"),
        ]);
        let caches = detect_with(&env, Some(Path::new(HOME)));
        let sccache = caches.iter().find(|c| c.tool == "sccache").unwrap();
        assert_eq!(sccache.dir, PathBuf::from("/custom/sccache/cache"));
        assert_eq!(sccache.source, CacheSource::Env("SCCACHE_DIR"));
    }

    #[test]
    fn sccache_dir_alone_detects_even_without_wrapper() {
        let env = env_of(&[("SCCACHE_DIR", "/c")]);
        let caches = detect_with(&env, Some(Path::new(HOME)));
        assert!(
            caches
                .iter()
                .any(|c| c.tool == "sccache" && c.dir == Path::new("/c"))
        );
    }

    #[test]
    fn empty_sccache_dir_is_ignored() {
        // An empty env value must not be treated as a real path.
        let env = env_of(&[("SCCACHE_DIR", "")]);
        assert!(detect_with(&env, Some(Path::new(HOME))).is_empty());
    }

    #[test]
    fn non_sccache_wrapper_is_not_detected() {
        let env = env_of(&[("RUSTC_WRAPPER", "/usr/bin/some-other-wrapper")]);
        assert!(detect_with(&env, Some(Path::new(HOME))).is_empty());
    }

    #[test]
    fn ccache_detected_only_via_explicit_dir() {
        let env = env_of(&[("CCACHE_DIR", "/home/tester/.ccache")]);
        let caches = detect_with(&env, Some(Path::new(HOME)));
        let ccache = caches
            .iter()
            .find(|c| c.tool == "ccache")
            .expect("ccache detected");
        assert_eq!(ccache.dir, PathBuf::from("/home/tester/.ccache"));
        assert_eq!(ccache.source, CacheSource::Env("CCACHE_DIR"));
    }

    #[test]
    fn sccache_without_home_yields_nothing_for_default() {
        // No home dir to anchor the platform default ⇒ cannot resolve a path.
        let env = env_of(&[("RUSTC_WRAPPER", "sccache")]);
        assert!(detect_with(&env, None).is_empty());
    }

    #[test]
    fn sccache_without_home_still_works_with_explicit_dir() {
        let env = env_of(&[("SCCACHE_DIR", "/abs/cache")]);
        let caches = detect_with(&env, None);
        assert_eq!(caches.len(), 1);
        assert_eq!(caches[0].dir, PathBuf::from("/abs/cache"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_default_honours_xdg_cache_home() {
        let env = env_of(&[
            ("RUSTC_WRAPPER", "sccache"),
            ("XDG_CACHE_HOME", "/xdg/cache"),
        ]);
        let caches = detect_with(&env, Some(Path::new(HOME)));
        assert_eq!(caches[0].dir, PathBuf::from("/xdg/cache/sccache"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_default_without_xdg_uses_dot_cache() {
        let env = env_of(&[("RUSTC_WRAPPER", "sccache")]);
        let caches = detect_with(&env, Some(Path::new(HOME)));
        assert_eq!(caches[0].dir, PathBuf::from("/home/tester/.cache/sccache"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_default_is_library_caches() {
        let env = env_of(&[("RUSTC_WRAPPER", "sccache")]);
        let caches = detect_with(&env, Some(Path::new(HOME)));
        assert_eq!(
            caches[0].dir,
            PathBuf::from("/home/tester/Library/Caches/Mozilla.sccache")
        );
    }
}
