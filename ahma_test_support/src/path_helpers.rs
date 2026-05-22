//! Cross-platform absolute-path helpers for tests.
//!
//! Lexical path logic is identical on Linux, macOS and Windows, but the anchor
//! differs. Using these helpers instead of hard-coding Unix paths lets tests run
//! without platform-specific guards.

use std::path::PathBuf;

/// Returns the platform-appropriate filesystem root used by test helpers.
///
/// * Unix  → `PathBuf::from("/")`
/// * Windows → `PathBuf::from("C:\\")`
pub fn test_root() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/")
    }
    #[cfg(windows)]
    {
        PathBuf::from("C:\\")
    }
    #[cfg(not(any(unix, windows)))]
    {
        PathBuf::from("/")
    }
}

/// Build an absolute path anchored at [`test_root()`] by joining `components`.
pub fn test_abs(components: &[&str]) -> PathBuf {
    let mut p = test_root();
    for &c in components {
        p = p.join(c);
    }
    p
}

/// Returns a path inside the system temp directory.
pub fn test_temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

/// Returns an absolute path guaranteed to be outside any realistic sandbox scope.
///
/// * Unix    → `/nonexistent_scope/file.txt`
/// * Windows → `Z:\\nonexistent_scope\\file.txt`
pub fn test_out_of_scope_path() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/nonexistent_scope/file.txt")
    }
    #[cfg(windows)]
    {
        PathBuf::from("Z:\\nonexistent_scope\\file.txt")
    }
    #[cfg(not(any(unix, windows)))]
    {
        PathBuf::from("/nonexistent_scope/file.txt")
    }
}

/// Returns a platform-appropriate device/special path for blocked-path testing.
///
/// * Unix    → `/dev/null`
/// * Windows → `NUL`
pub fn test_blocked_device_path() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/dev/null")
    }
    #[cfg(windows)]
    {
        PathBuf::from("NUL")
    }
    #[cfg(not(any(unix, windows)))]
    {
        PathBuf::from("/dev/null")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_abs_builds_absolute_paths() {
        let path = test_abs(&["ahma", "path-helper-test"]);

        assert!(
            path.is_absolute(),
            "test_abs should return an absolute path"
        );
        assert!(
            path.ends_with("path-helper-test"),
            "test_abs should preserve the final component: {}",
            path.display()
        );
    }

    #[test]
    fn test_temp_path_is_under_system_temp_dir() {
        let path = test_temp_path("ahma-test-support-marker");

        assert!(
            path.starts_with(std::env::temp_dir()),
            "test_temp_path should stay inside the platform temp dir: {}",
            path.display()
        );
    }

    #[test]
    fn special_paths_are_non_empty() {
        assert!(!test_out_of_scope_path().as_os_str().is_empty());
        assert!(!test_blocked_device_path().as_os_str().is_empty());
    }
}
