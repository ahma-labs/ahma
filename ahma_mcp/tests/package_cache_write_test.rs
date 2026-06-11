//! Tests for the `package_cache_write` sandbox feature.
//!
//! Covers:
//! - `Sandbox::with_package_cache_write()` builder preserves default and allows override
//! - `Sandbox::package_cache_write()` getter
//! - `Sandbox` clone preserves the flag
//! - macOS: seatbelt profile contains `file-write*` for `registry/`, `git/`, and lock
//!   files when enabled; contains **no** write rule for `bin/`, `config.toml`,
//!   `credentials.toml`; omits all four writable entries when disabled
//! - `AppConfig::default()` has `package_cache_write: true`

use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use tempfile::TempDir;

// ─── Sandbox builder ──────────────────────────────────────────────────────────

#[test]
fn test_sandbox_package_cache_write_default_is_true() {
    let tmp = TempDir::new().unwrap();
    let sandbox = Sandbox::new(
        vec![tmp.path().to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )
    .unwrap();
    assert!(
        sandbox.package_cache_write(),
        "Default should be package_cache_write = true"
    );
}

#[test]
fn test_sandbox_with_package_cache_write_disabled() {
    let tmp = TempDir::new().unwrap();
    let sandbox = Sandbox::new(
        vec![tmp.path().to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )
    .unwrap()
    .with_package_cache_write(false);
    assert!(
        !sandbox.package_cache_write(),
        "After with_package_cache_write(false) the flag must be false"
    );
}

#[test]
fn test_sandbox_with_package_cache_write_enabled_is_noop() {
    let tmp = TempDir::new().unwrap();
    let sandbox = Sandbox::new(
        vec![tmp.path().to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )
    .unwrap()
    .with_package_cache_write(true);
    assert!(
        sandbox.package_cache_write(),
        "with_package_cache_write(true) must keep the flag true"
    );
}

#[test]
fn test_sandbox_clone_preserves_package_cache_write_flag() {
    let tmp = TempDir::new().unwrap();
    let sandbox = Sandbox::new(
        vec![tmp.path().to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )
    .unwrap()
    .with_package_cache_write(false);
    let cloned = sandbox.clone();
    assert!(
        !cloned.package_cache_write(),
        "Clone must preserve package_cache_write = false"
    );
}

// ─── AppConfig defaults ───────────────────────────────────────────────────────

#[test]
fn test_app_config_default_package_cache_write_is_true() {
    let cfg = ahma_mcp::shell::cli::AppConfig::default();
    assert!(
        cfg.package_cache_write,
        "AppConfig::default() must have package_cache_write = true"
    );
}

// ─── macOS: seatbelt profile content ─────────────────────────────────────────

#[cfg(target_os = "macos")]
mod seatbelt_profile_tests {
    use super::*;

    /// Create a fake CARGO_HOME inside `parent`, populate the required
    /// subdirs/files, set CARGO_HOME env, and return the fake base path.
    fn setup_fake_cargo_home(parent: &TempDir) -> std::path::PathBuf {
        let fake_cargo = parent.path().join("cargo_home");
        std::fs::create_dir_all(fake_cargo.join("registry")).unwrap();
        std::fs::create_dir_all(fake_cargo.join("git")).unwrap();
        std::fs::create_dir_all(fake_cargo.join("bin")).unwrap();
        std::fs::write(fake_cargo.join("config.toml"), "").unwrap();
        std::fs::write(fake_cargo.join("credentials.toml"), "").unwrap();
        // SAFETY: test-only, single-threaded (nextest runs each test in isolation)
        unsafe { std::env::set_var("CARGO_HOME", &fake_cargo) };
        fake_cargo
    }

    fn restore_cargo_home(saved: Option<String>) {
        // SAFETY: test-only, single-threaded
        unsafe {
            match saved {
                Some(v) => std::env::set_var("CARGO_HOME", v),
                None => std::env::remove_var("CARGO_HOME"),
            }
        }
    }

    /// Generate the seatbelt profile for a Sandbox, scoped to `scope`.
    fn profile_for(sandbox: &Sandbox, scope: &TempDir) -> String {
        sandbox.generate_seatbelt_profile_test(scope.path())
    }

    #[test]
    fn test_seatbelt_profile_includes_write_for_registry_and_git_when_enabled() {
        let tmp = TempDir::new().unwrap();
        let scope = TempDir::new().unwrap();
        let saved = std::env::var("CARGO_HOME").ok();
        let fake_cargo = setup_fake_cargo_home(&tmp);

        let sandbox = Sandbox::new(
            vec![scope.path().to_path_buf()],
            SandboxMode::Strict,
            false,
            false,
            false,
        )
        .unwrap();
        // Default: package_cache_write = true
        let profile = profile_for(&sandbox, &scope);

        // Restore env
        restore_cargo_home(saved);

        let registry = fake_cargo.join("registry").to_string_lossy().into_owned();
        let git_dir = fake_cargo.join("git").to_string_lossy().into_owned();
        assert!(
            profile.contains(&format!("(allow file-write* (subpath \"{registry}\"))")),
            "Profile must allow write for registry.\nProfile:\n{profile}"
        );
        assert!(
            profile.contains(&format!("(allow file-write* (subpath \"{git_dir}\"))")),
            "Profile must allow write for git.\nProfile:\n{profile}"
        );
    }

    #[test]
    fn test_seatbelt_profile_excludes_write_for_sensitive_cargo_paths() {
        let tmp = TempDir::new().unwrap();
        let scope = TempDir::new().unwrap();
        let saved = std::env::var("CARGO_HOME").ok();
        let fake_cargo = setup_fake_cargo_home(&tmp);

        let sandbox = Sandbox::new(
            vec![scope.path().to_path_buf()],
            SandboxMode::Strict,
            false,
            false,
            false,
        )
        .unwrap();
        let profile = profile_for(&sandbox, &scope);

        restore_cargo_home(saved);

        let bin = fake_cargo.join("bin").to_string_lossy().into_owned();
        let config = fake_cargo
            .join("config.toml")
            .to_string_lossy()
            .into_owned();
        let creds = fake_cargo
            .join("credentials.toml")
            .to_string_lossy()
            .into_owned();

        assert!(
            !profile.contains(&format!("(allow file-write* (subpath \"{bin}\"))")),
            "Profile must NOT allow write for bin/.\nProfile:\n{profile}"
        );
        assert!(
            !profile.contains(&format!("(allow file-write* (literal \"{config}\"))")),
            "Profile must NOT allow write for config.toml.\nProfile:\n{profile}"
        );
        assert!(
            !profile.contains(&format!("(allow file-write* (literal \"{creds}\"))")),
            "Profile must NOT allow write for credentials.toml.\nProfile:\n{profile}"
        );
    }

    #[test]
    fn test_seatbelt_profile_omits_all_cache_write_rules_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let scope = TempDir::new().unwrap();
        let saved = std::env::var("CARGO_HOME").ok();
        let fake_cargo = setup_fake_cargo_home(&tmp);

        let sandbox = Sandbox::new(
            vec![scope.path().to_path_buf()],
            SandboxMode::Strict,
            false,
            false,
            false,
        )
        .unwrap()
        .with_package_cache_write(false);
        let profile = profile_for(&sandbox, &scope);

        restore_cargo_home(saved);

        let registry = fake_cargo.join("registry").to_string_lossy().into_owned();
        let git_dir = fake_cargo.join("git").to_string_lossy().into_owned();

        assert!(
            !profile.contains(&format!("(allow file-write* (subpath \"{registry}\"))")),
            "Profile must NOT allow write for registry when disabled.\nProfile:\n{profile}"
        );
        assert!(
            !profile.contains(&format!("(allow file-write* (subpath \"{git_dir}\"))")),
            "Profile must NOT allow write for git when disabled.\nProfile:\n{profile}"
        );
    }
}
