//! Detection of processes spawned (directly or transitively) by a test harness.
//!
//! ahma's proxy, bridge, and daemon rendezvous on machine-global singleton
//! endpoints (`/tmp/ahma.sock`, `~/.ahma/daemon.sock`, the Windows daemon TCP
//! port). A test-spawned `ahma` binary that resolves those endpoints can tear
//! down a developer's *live* MCP session: it may restart the shared bridge
//! (version mismatch), unlink its socket while binding its own, or dispatch
//! work to the live daemon hub.
//!
//! `cfg!(test)` cannot protect against this: integration tests spawn ordinary
//! debug/release binaries in which `cfg!(test)` is `false`. The
//! `AHMA_TEST_ISOLATION` plumbing variable covers harnesses that remember to
//! set it, but it is opt-in per spawn site and has historically been missed.
//!
//! This module closes the hole structurally: `cargo nextest` exports `NEXTEST=1`
//! to every test process, and child processes inherit it, so any binary spawned
//! anywhere under a nextest run can self-detect the test context — no harness
//! cooperation required.

/// True when this process was spawned under a test harness and must therefore
/// never resolve machine-global endpoints.
///
/// Detected via (either):
/// - `AHMA_TEST_ISOLATION` — explicit plumbing set by test helpers such as
///   `ahma_mcp::test_utils::cli::test_command`.
/// - `NEXTEST` — exported by `cargo nextest` to every test process and
///   inherited by everything those tests spawn.
///
/// Plain `cargo test` sets no comparably distinctive runtime variable, so
/// harnesses invoked that way still need `AHMA_TEST_ISOLATION`; nextest is this
/// repo's standard runner and is what CI uses.
pub fn spawned_under_test_harness() -> bool {
    std::env::var_os("AHMA_TEST_ISOLATION").is_some() || std::env::var_os("NEXTEST").is_some()
}

/// A short stable identifier for the current test run, shared by every process
/// in the run's tree.
///
/// Derived from `NEXTEST_RUN_ID` (a UUID nextest exports to all test
/// processes), falling back to this process's PID when absent. Use it to name
/// per-run private endpoints that parent and child processes must agree on —
/// a PID-based name would differ between a test and the binaries it spawns.
pub fn test_run_discriminator() -> String {
    match std::env::var("NEXTEST_RUN_ID") {
        // First UUID group (8 hex chars) keeps Unix socket paths well under
        // the 104-byte macOS limit.
        Ok(id) => id.chars().take(8).collect(),
        Err(_) => std::process::id().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    /// Serializes env-var mutation across tests in this module.
    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct EnvVarGuard {
        key: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
            let original = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, original }
        }

        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn detects_nextest_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _iso = EnvVarGuard::unset("AHMA_TEST_ISOLATION");
        let _next = EnvVarGuard::set("NEXTEST", "1");
        assert!(spawned_under_test_harness());
    }

    #[test]
    fn detects_explicit_isolation_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _next = EnvVarGuard::unset("NEXTEST");
        let _iso = EnvVarGuard::set("AHMA_TEST_ISOLATION", "1");
        assert!(spawned_under_test_harness());
    }

    #[test]
    fn false_outside_any_test_harness() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _iso = EnvVarGuard::unset("AHMA_TEST_ISOLATION");
        let _next = EnvVarGuard::unset("NEXTEST");
        assert!(!spawned_under_test_harness());
    }

    #[test]
    fn discriminator_prefers_run_id_and_stays_short() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _id = EnvVarGuard::set("NEXTEST_RUN_ID", "a1b2c3d4-5678-90ab-cdef-1234567890ab");
        assert_eq!(test_run_discriminator(), "a1b2c3d4");
    }

    #[test]
    fn discriminator_falls_back_to_pid() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _id = EnvVarGuard::unset("NEXTEST_RUN_ID");
        assert_eq!(test_run_discriminator(), std::process::id().to_string());
    }
}
