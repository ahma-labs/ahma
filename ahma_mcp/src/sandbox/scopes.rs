use anyhow::{Result, anyhow};
use dunce;
use std::path::{Path, PathBuf};

use super::types::SandboxMode;

/// Returns `true` when `path` is a filesystem root — every absolute path on
/// this platform is a descendant of it — making it unsuitable as a sandbox
/// scope because it provides no containment.
///
/// | Platform | Root examples                         |
/// |----------|---------------------------------------|
/// | Unix     | `/`                                   |
/// | Windows  | `C:\`, `D:\`, `\\server\share` (UNC)  |
pub(crate) fn is_filesystem_root(path: &Path) -> bool {
    use std::path::Component;
    let mut it = path.components();
    match it.next() {
        // Unix root: just a single RootDir component
        Some(Component::RootDir) => it.next().is_none(),
        // Windows drive root: Prefix("C:") + RootDir, nothing after
        Some(Component::Prefix(_)) => {
            matches!(it.next(), Some(Component::RootDir)) && it.next().is_none()
        }
        _ => false,
    }
}

/// How broad a candidate scope is relative to the user's home directory.
///
/// `AboveHome` is the same class of over-broad scope as a filesystem root:
/// `/Users`, `/home`, `/Volumes`, `C:\Users` each span every account on the
/// machine, so locking to one is materially the same risk as locking to `/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HomeBreadth {
    /// Neither the home directory nor an ancestor of it — acceptable breadth.
    Contained,
    /// Exactly the home directory: exposes every dotfile, key and credential.
    HomeItself,
    /// A **strict ancestor** of the home directory: spans every user account.
    AboveHome,
}

/// Resolve `path` for comparison: real canonicalization when the path exists,
/// lexical normalization otherwise (a scope that does not exist is rejected
/// elsewhere; this keeps the classification total).
fn resolve_for_comparison(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| normalize_path_lexically(path))
}

/// Classify `candidate` against `home` (SPEC R5.2.4 hard rejections).
///
/// The relationship is tested **structurally** — resolve the home directory and
/// ask whether the candidate contains it — rather than against a denylist of
/// platform literals, so it stays correct for non-standard home locations
/// (a relocated macOS home, `/home` automounted from `/mnt/home`, a corporate
/// network home, `AHMA_TEST_HOME` in tests).
///
/// Both sides go through [`resolve_for_comparison`] before comparing, for two
/// reasons: SPEC R5.7 requires the rejection to hold *after* symlink
/// resolution, and `Path::starts_with` is case-sensitive on Windows though the
/// filesystem is not — a raw `C:\users` would otherwise slip past `C:\Users`.
///
/// This is a **hard** rejection with no CLI escape hatch, deliberately.
/// `--sandbox-scope` is carried in client-owned MCP config files written by
/// `ahma setup`, which is exactly where a naive or hostile setup would put an
/// over-broad path; a flag or environment variable to bypass this would live in
/// the same file that the check exists to distrust. Any future escape hatch
/// belongs in user-owned `~/.ahma/settings.toml` only.
pub(crate) fn home_breadth(candidate: &Path, home: Option<&Path>) -> HomeBreadth {
    let Some(home) = home else {
        return HomeBreadth::Contained;
    };
    // An empty candidate has no components, so `starts_with` would match every
    // home. Empty paths are rejected by their own check, with their own message.
    if candidate.as_os_str().is_empty() {
        return HomeBreadth::Contained;
    }

    let candidate = resolve_for_comparison(candidate);
    let home = resolve_for_comparison(home);

    if candidate == home {
        HomeBreadth::HomeItself
    } else if home.starts_with(&candidate) {
        HomeBreadth::AboveHome
    } else {
        HomeBreadth::Contained
    }
}

/// Canonicalize and validate a list of sandbox scopes.
///
/// Rejects filesystem roots and empty paths in Strict mode, plus the home
/// directory and any strict ancestor of it ([`home_breadth`], SPEC R5.2.4).
/// Falls back to raw paths in Test mode when canonicalization fails.
///
/// For symlink-aware compatibility, this preserves both canonical and absolute
/// alias paths (when they differ). This allows equivalent paths to validate
/// correctly even when lexical normalization is used for non-existent targets.
pub(super) fn canonicalize_scopes(
    scopes: Vec<PathBuf>,
    mode: SandboxMode,
    context: &str,
) -> Result<Vec<PathBuf>> {
    canonicalize_scopes_with_home(
        scopes,
        mode,
        context,
        ahma_common::config::ahma_home_dir().as_deref(),
    )
}

/// [`canonicalize_scopes`] with the home directory injected, so the R5.2.4
/// breadth rejections are testable against a fabricated home instead of the
/// developer's real one (no process-global env mutation, no writes to `$HOME`).
fn canonicalize_scopes_with_home(
    scopes: Vec<PathBuf>,
    mode: SandboxMode,
    context: &str,
    home: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let cwd = std::env::current_dir().ok();
    let mut canonicalized = Vec::with_capacity(scopes.len() * 2);

    let mut push_unique = |candidate: PathBuf| {
        if !canonicalized.contains(&candidate) {
            canonicalized.push(candidate);
        }
    };

    for scope in scopes {
        if mode != SandboxMode::Test && (is_filesystem_root(&scope) || scope == Path::new("")) {
            return Err(anyhow!(
                "Filesystem root or empty path is not a valid sandbox scope \
                 (path: '{}', OS: {}). {}",
                scope.display(),
                std::env::consts::OS,
                context
            ));
        }

        let absolute_alias = if scope.is_absolute() {
            Some(scope.clone())
        } else {
            cwd.as_ref()
                .map(|c| normalize_path_lexically(&c.join(&scope)))
        };

        let canonical = match dunce::canonicalize(&scope) {
            Ok(c) => c,
            Err(e) => {
                if mode == SandboxMode::Test {
                    scope.clone()
                } else {
                    return Err(anyhow!(
                        "Failed to canonicalize sandbox scope '{}': {}",
                        scope.display(),
                        e
                    ));
                }
            }
        };

        if mode != SandboxMode::Test && is_filesystem_root(&canonical) {
            return Err(anyhow!(
                "Filesystem root is not a valid sandbox scope \
                 (resolved from '{}', OS: {}). {}",
                scope.display(),
                std::env::consts::OS,
                context
            ));
        }

        // SPEC R5.2.4: the home directory and anything above it are hard
        // rejections, checked here (post-canonicalization) so a symlinked or
        // differently-cased spelling cannot walk past it (R5.7).
        if mode != SandboxMode::Test {
            match home_breadth(&canonical, home) {
                HomeBreadth::Contained => {}
                HomeBreadth::HomeItself => {
                    return Err(anyhow!(
                        "Your home directory is not a valid sandbox scope: '{}' exposes every \
                         dotfile, key and credential under it (resolved from '{}', OS: {}). \
                         Scope to the project you are working on, or to a container directory \
                         under your home directory (e.g. ~/github, or ~/github/myproject). {}",
                        canonical.display(),
                        scope.display(),
                        std::env::consts::OS,
                        context
                    ));
                }
                HomeBreadth::AboveHome => {
                    return Err(anyhow!(
                        "'{}' is an ancestor of your home directory and is not a valid sandbox \
                         scope: it spans every user account on this machine, which is materially \
                         the same as scoping to the filesystem root (resolved from '{}', OS: {}). \
                         Scope to the project you are working on, or to a container directory \
                         under your home directory (e.g. ~/github, or ~/github/myproject). {}",
                        canonical.display(),
                        scope.display(),
                        std::env::consts::OS,
                        context
                    ));
                }
            }
        }

        push_unique(canonical.clone());

        if let Some(alias) = absolute_alias {
            let alias = normalize_path_lexically(&alias);
            if alias != canonical {
                push_unique(alias);
            }
        }
    }
    Ok(canonicalized)
}

/// Normalize a path lexically (without filesystem access).
pub fn normalize_path_lexically(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut stack = Vec::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never pop a root anchor (RootDir or Prefix like "C:") —
                // that would allow escaping to an invalid path on Windows.
                if stack
                    .last()
                    .is_some_and(|c| !matches!(c, Component::RootDir | Component::Prefix(_)))
                {
                    stack.pop();
                }
            }
            c => stack.push(c),
        }
    }

    stack.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::path_helpers::{test_abs, test_root};
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // is_filesystem_root — cross-platform
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_filesystem_root_unix_root() {
        assert!(is_filesystem_root(&test_root()));
    }

    #[test]
    fn test_is_filesystem_root_unix_subdir() {
        assert!(!is_filesystem_root(&test_abs(&["home", "user"])));
    }

    #[test]
    fn test_is_filesystem_root_relative_path() {
        assert!(!is_filesystem_root(Path::new("relative/path")));
    }

    #[test]
    fn test_is_filesystem_root_empty() {
        assert!(!is_filesystem_root(Path::new("")));
    }

    #[cfg(windows)]
    #[test]
    fn test_is_filesystem_root_windows_drive_roots() {
        assert!(is_filesystem_root(Path::new("C:\\")));
        assert!(is_filesystem_root(Path::new("D:\\")));
    }

    #[cfg(windows)]
    #[test]
    fn test_is_filesystem_root_windows_subdir_not_root() {
        assert!(!is_filesystem_root(Path::new("C:\\Users\\test")));
    }

    #[cfg(windows)]
    #[test]
    fn test_is_filesystem_root_windows_unc_root_vs_subpath() {
        // UNC share root (trailing slash → Prefix + RootDir, nothing after)
        assert!(is_filesystem_root(Path::new("\\\\server\\share\\")));
        // UNC subpath is NOT a root
        assert!(!is_filesystem_root(Path::new("\\\\server\\share\\path")));
    }

    // -----------------------------------------------------------------------
    // normalize_path_lexically — dotdot safety, cross-platform
    // -----------------------------------------------------------------------

    #[test]
    fn test_normalize_extra_dotdot_cannot_escape_unix_root() {
        // Excess `..` beyond root must never collapse the RootDir sentinel.
        let result = normalize_path_lexically(&test_abs(&["a", "..", "..", ".."]));
        assert_eq!(result, test_root());
    }

    #[test]
    fn test_normalize_removes_current_dir() {
        assert_eq!(
            normalize_path_lexically(&test_abs(&["a", ".", "b"])),
            test_abs(&["a", "b"]),
        );
    }

    #[test]
    fn test_normalize_removes_parent_dir() {
        assert_eq!(
            normalize_path_lexically(&test_abs(&["a", "b", "..", "c"])),
            test_abs(&["a", "c"]),
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_normalize_dotdot_cannot_escape_windows_drive_root() {
        // Multiple `..` beyond drive root must not pop Prefix or RootDir.
        let result = normalize_path_lexically(Path::new("C:\\a\\..\\..\\.."));
        assert_eq!(result, PathBuf::from("C:\\"));
    }

    #[cfg(windows)]
    #[test]
    fn test_normalize_drive_relative_dotdot_stays_on_drive() {
        // Drive-relative path: `..` must not pop the Prefix anchor.
        // e.g. C:..\escape must remain under the C: prefix, not become bare "escape".
        let result = normalize_path_lexically(Path::new("C:..\\escape"));
        assert!(
            result
                .to_str()
                .map(|s| s.starts_with("C:"))
                .unwrap_or(false),
            "expected C:-prefixed result, got {}",
            result.display()
        );
    }

    // -----------------------------------------------------------------------
    // canonicalize_scopes — root rejection (cross-platform & Windows-only)
    // -----------------------------------------------------------------------

    #[test]
    fn test_canonicalize_rejects_unix_root_in_strict_mode() {
        let err = canonicalize_scopes(vec![test_root()], SandboxMode::Strict, "test context")
            .unwrap_err();
        assert!(
            err.to_string().contains("not a valid sandbox scope"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_canonicalize_rejects_empty_path_in_strict_mode() {
        let result =
            canonicalize_scopes(vec![PathBuf::from("")], SandboxMode::Strict, "test context");
        assert!(
            result.is_err(),
            "empty path must be rejected in Strict mode"
        );
    }

    #[test]
    fn test_canonicalize_accepts_real_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let result = canonicalize_scopes(
            vec![dir.path().to_path_buf()],
            SandboxMode::Strict,
            "test context",
        );
        assert!(
            result.is_ok(),
            "real existing dir must be accepted: {:?}",
            result
        );
    }

    #[test]
    fn test_canonicalize_root_allowed_in_test_mode() {
        // SandboxMode::Test bypasses the root guard (used by test harnesses).
        let result =
            canonicalize_scopes(vec![PathBuf::from("/")], SandboxMode::Test, "test context");
        // Might succeed or fail for other reasons, but NOT the sandbox root guard.
        if let Err(e) = result {
            assert!(
                !e.to_string().contains("not a valid sandbox scope"),
                "Test mode must not apply root guard: {e}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn test_canonicalize_rejects_windows_drive_root_in_strict_mode() {
        let err = canonicalize_scopes(
            vec![PathBuf::from("C:\\")],
            SandboxMode::Strict,
            "test context",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not a valid sandbox scope"),
            "C:\\ must be rejected as sandbox scope: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // R5.2.4 breadth rejections — home directory and its ancestors
    //
    // Every case uses a *fabricated* home under a tempdir, so nothing is
    // written to the developer's real `$HOME` and no process-global env is
    // mutated (tests stay parallel-safe).
    // -----------------------------------------------------------------------

    /// `<tmp>/Users/alice` as home, with `<tmp>/Users` standing in for
    /// `/Users`, `/home`, `/Volumes` or `C:\Users`.
    fn fake_home() -> (tempfile::TempDir, PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let home = td.path().join("Users").join("alice");
        std::fs::create_dir_all(&home).unwrap();
        (td, home)
    }

    #[test]
    fn test_home_breadth_flags_strict_ancestor_of_home() {
        let (td, home) = fake_home();
        let users = home.parent().unwrap().to_path_buf();
        assert_eq!(
            home_breadth(&users, Some(&home)),
            HomeBreadth::AboveHome,
            "the directory holding every user account must be flagged"
        );
        assert_eq!(
            home_breadth(td.path(), Some(&home)),
            HomeBreadth::AboveHome,
            "a higher ancestor must be flagged too"
        );
    }

    #[test]
    fn test_home_breadth_flags_home_itself() {
        let (_td, home) = fake_home();
        assert_eq!(home_breadth(&home, Some(&home)), HomeBreadth::HomeItself);
    }

    #[test]
    fn test_home_breadth_flags_filesystem_root() {
        let (_td, home) = fake_home();
        // A root is by definition an ancestor of every home on that volume.
        // Only assert when the fabricated home really lives under this root
        // (on Windows the temp dir may sit on another drive).
        let root = test_root();
        if resolve_for_comparison(&home).starts_with(&root) {
            assert_eq!(home_breadth(&root, Some(&home)), HomeBreadth::AboveHome);
        }
    }

    #[test]
    fn test_home_breadth_allows_project_and_container_dirs_under_home() {
        let (_td, home) = fake_home();
        let project = home.join("myproject");
        let container = home.join("github");
        let nested = container.join("ahma");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&project).unwrap();

        for p in [&project, &container, &nested] {
            assert_eq!(
                home_breadth(p, Some(&home)),
                HomeBreadth::Contained,
                "{} must be an acceptable scope",
                p.display()
            );
        }
    }

    #[test]
    fn test_home_breadth_without_home_is_contained() {
        // No resolvable home → this rule simply does not fire; the filesystem
        // root guard still does.
        assert_eq!(
            home_breadth(&test_abs(&["Users"]), None),
            HomeBreadth::Contained
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_home_breadth_sees_through_symlinked_home() {
        // R5.7: the rejection must hold after symlink resolution. Needs a
        // genuinely Unix-only API (std::os::unix::fs::symlink).
        let td = tempfile::tempdir().unwrap();
        let real_users = td.path().join("real");
        let home = real_users.join("alice");
        std::fs::create_dir_all(&home).unwrap();
        let link = td.path().join("link");
        std::os::unix::fs::symlink(&real_users, &link).unwrap();

        // Home is spelled through the symlink; the candidate is the real dir.
        assert_eq!(
            home_breadth(&real_users, Some(&link.join("alice"))),
            HomeBreadth::AboveHome
        );
        // And the mirror image: candidate spelled through the symlink.
        assert_eq!(home_breadth(&link, Some(&home)), HomeBreadth::AboveHome);
    }

    #[test]
    fn test_canonicalize_rejects_ancestor_of_home_in_strict_mode() {
        let (_td, home) = fake_home();
        let users = home.parent().unwrap().to_path_buf();
        let err = canonicalize_scopes_with_home(
            vec![users.clone()],
            SandboxMode::Strict,
            "test context",
            Some(&home),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("ancestor of your home directory"),
            "must say why it is too broad: {msg}"
        );
        assert!(
            msg.contains(&users.display().to_string())
                || msg.contains(&resolve_for_comparison(&users).display().to_string()),
            "must name the rejected path: {msg}"
        );
        assert!(
            msg.contains("~/github"),
            "must point at a workable alternative: {msg}"
        );
    }

    #[test]
    fn test_canonicalize_rejects_home_itself_in_strict_mode() {
        let (_td, home) = fake_home();
        let err = canonicalize_scopes_with_home(
            vec![home.clone()],
            SandboxMode::Strict,
            "test context",
            Some(&home),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("home directory is not a valid sandbox scope"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("~/github"),
            "must point at a workable alternative: {msg}"
        );
    }

    #[test]
    fn test_canonicalize_still_rejects_root_with_home_known() {
        let (_td, home) = fake_home();
        let err = canonicalize_scopes_with_home(
            vec![test_root()],
            SandboxMode::Strict,
            "test context",
            Some(&home),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not a valid sandbox scope"),
            "filesystem root must stay rejected: {err}"
        );
    }

    #[test]
    fn test_canonicalize_accepts_project_and_container_dirs_under_home() {
        let (_td, home) = fake_home();
        let project = home.join("myproject");
        let container = home.join("github");
        let nested = container.join("ahma");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        for p in [project, container, nested] {
            let result = canonicalize_scopes_with_home(
                vec![p.clone()],
                SandboxMode::Strict,
                "test context",
                Some(&home),
            );
            assert!(
                result.is_ok(),
                "{} must be accepted: {:?}",
                p.display(),
                result
            );
        }
    }

    #[test]
    fn test_canonicalize_home_breadth_exempt_in_test_mode() {
        let (_td, home) = fake_home();
        let users = home.parent().unwrap().to_path_buf();
        for candidate in [users, home.clone()] {
            let result = canonicalize_scopes_with_home(
                vec![candidate.clone()],
                SandboxMode::Test,
                "test context",
                Some(&home),
            );
            assert!(
                result.is_ok(),
                "Test mode must not apply the R5.2.4 breadth guard to {}: {:?}",
                candidate.display(),
                result
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn test_canonicalize_rejects_unc_root_in_strict_mode() {
        let err = canonicalize_scopes(
            vec![PathBuf::from("\\\\server\\share\\")],
            SandboxMode::Strict,
            "test context",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not a valid sandbox scope"),
            "UNC root must be rejected as sandbox scope: {err}"
        );
    }
}
