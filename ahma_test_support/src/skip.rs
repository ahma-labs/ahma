//! One verdict on whether a test may skip itself.
//!
//! A test that cannot set up its fixtures has two honest options, and which one
//! is right depends entirely on where it is running. On a developer's machine a
//! missing prerequisite — no `powershell`, a port already bound, a tool the
//! workspace does not ship — is noise, and skipping keeps the suite usable. On
//! CI the same condition is a regression, because CI is the environment the
//! project controls: if the fixture cannot be built there, something broke.
//!
//! The failure mode this exists to prevent is a skip that reads as a pass. A
//! test that prints `WARNING skipping…` and returns produces a green run
//! indistinguishable from one where every assertion held. `ahma_http_bridge`'s
//! `setup_test_mcp` already got this right — it panics when `CI` is set — but
//! the verdict was written inline in one function, so the six other bail-out
//! sites around it kept skipping silently in CI: a server that failed to spawn,
//! a tools directory that went missing, a session that would not initialize.
//! Each was a real failure reported as success.
//!
//! Which is exactly the rule AGENTS.md states for the codebase generally: a
//! constraint written as a comment in the one file that obeys it does not bind
//! the next file to implement the same thing. So the verdict lives here, and
//! every skip site calls it.
//!
//! ```no_run
//! # use ahma_test_support::skip::skip_or_fail;
//! let Some(server) = spawn_server() else {
//!     skip_or_fail("server spawn failed");
//!     return;
//! };
//! # fn spawn_server() -> Option<()> { None }
//! ```

/// Set by the test harness to make a skip fatal outside CI — for reproducing a
/// CI-only failure locally, or for a pre-merge run that should be as strict.
pub const FAIL_ON_SKIP_ENV: &str = "AHMA_TEST_FAIL_ON_SETUP_ERROR";

/// Whether a skipped test should instead fail.
///
/// True when `CI` is set (GitHub Actions and essentially every other runner set
/// it) or when [`FAIL_ON_SKIP_ENV`] is set explicitly.
pub fn skips_are_failures() -> bool {
    std::env::var_os("CI").is_some() || std::env::var_os(FAIL_ON_SKIP_ENV).is_some()
}

/// Skip the calling test, or panic if skipping is not allowed here.
///
/// Call this instead of a bare `eprintln!` + `return` at every point a test
/// gives up on its own setup. `reason` should name the thing that failed and,
/// where there is one, the underlying error — it is the only diagnostic a CI
/// failure will carry.
///
/// Returns normally when the skip is permitted, so the caller still does its own
/// `return`; making this diverge would force every call site into a shape that
/// does not fit `let … else`.
#[track_caller]
pub fn skip_or_fail(reason: &str) {
    if skips_are_failures() {
        panic!(
            "test setup failed and skipping is not allowed here: {reason}\n\
             (a skip in CI is indistinguishable from a pass, so this is a failure. \
             Set {FAIL_ON_SKIP_ENV} to reproduce locally, or unset CI to allow the skip.)"
        );
    }
    eprintln!("WARNING skipping test: {reason}");
}

/// [`skip_or_fail`] for a missing external prerequisite, e.g. a tool the
/// workspace does not build.
///
/// Separate from [`skip_or_fail`] because the two answer different questions.
/// A prerequisite that CI is supposed to install is a CI regression when it is
/// absent, so this is fatal there too; a prerequisite CI deliberately does not
/// provide belongs behind `#[cfg]` or an `#[ignore]` with a reason, not behind a
/// runtime skip. The distinct message keeps the two apart in a failure log.
#[track_caller]
pub fn skip_or_fail_missing(prerequisite: &str) {
    skip_or_fail(&format!("missing prerequisite: {prerequisite}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env reads are process-global, so these two cases share one test
    /// rather than racing each other across nextest's threads.
    #[test]
    fn the_verdict_follows_the_environment() {
        // `cargo nextest` does not set CI itself; on a CI runner it is set for us.
        let on_ci = std::env::var_os("CI").is_some();
        assert_eq!(
            skips_are_failures(),
            on_ci || std::env::var_os(FAIL_ON_SKIP_ENV).is_some(),
            "skips must be fatal exactly when CI or {FAIL_ON_SKIP_ENV} is set"
        );
    }

    #[test]
    fn a_permitted_skip_returns_rather_than_diverging() {
        if skips_are_failures() {
            // On CI the permitted-skip path cannot be reached; assert the other
            // half instead, so this test proves something in both environments.
            let panicked = std::panic::catch_unwind(|| skip_or_fail("deliberate")).is_err();
            assert!(panicked, "with CI set, a skip must panic");
            return;
        }
        skip_or_fail("deliberate, permitted skip");
        skip_or_fail_missing("a tool that does not exist");
    }
}
