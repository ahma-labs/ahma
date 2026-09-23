//! # Platform-Aware Timeouts: The CI Stability Engine
//!
//! In a project like Ahma that relies heavily on process spawning, shell interaction,
//! and terminal communication, hardcoded timeouts are a recipe for flaky tests and
//! unstable deployments.
//!
//! ## Why Scaling is Required
//!
//! Windows CI runners, especially when running under coverage instrumentations, can
//! be 4x to 8x slower than Linux/macOS environments for specific tasks:
//! - **Binary Loading**: Spawning a new Rust binary takes significantly longer.
//! - **Pipe Buffering**: Stdio communication has different latency characteristics.
//! - **Sandbox Setup**: Initializing security jobs or AppContainers adds overhead.
//!
//! This module provides a centralized scaling engine that adjusts every timeout
//! based on the detected OS and environment (e.g., `cargo llvm-cov`).
//!
//! # Usage
//!
//! ```rust
//! use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
//! use std::time::Duration;
//!
//! // Get a timeout for a specific category
//! let timeout = TestTimeouts::get(TimeoutCategory::ProcessSpawn);
//!
//! // Scale a custom duration
//! let custom = TestTimeouts::scale(Duration::from_secs(5));
//!
//! // Get the platform multiplier directly
//! let multiplier = TestTimeouts::multiplier();
//! ```
//!
//! # Design Rationale
//!
//! Rather than hardcoding platform checks throughout tests, this module:
//! 1. Centralizes timeout logic for consistency
//! 2. Provides semantic categories (spawn, handshake, tool call) with sensible defaults
//! 3. Allows environment-based overrides for debugging
//! 4. Accounts for coverage mode which adds additional overhead

use std::sync::OnceLock;
use std::time::Duration;

/// Timeout categories with platform-aware defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutCategory {
    /// Process spawn and initial startup (binary loading, shell pool init)
    ProcessSpawn,
    /// MCP handshake completion (initialize, roots/list exchange)
    Handshake,
    /// Individual tool call execution
    ToolCall,
    /// Waiting for sandbox readiness after roots exchange
    SandboxReady,
    /// HTTP request/response cycle
    HttpRequest,
    /// SSE stream operations
    SseStream,
    /// Health check polling
    HealthCheck,
    /// Test cleanup operations
    Cleanup,
    /// Short operations (sub-second on fast platforms)
    Quick,
}

/// Platform-aware timeout configuration.
pub struct TestTimeouts;

impl TestTimeouts {
    /// Get the platform multiplier.
    ///
    /// - Windows: 4x base timeout (CI runners are significantly slower)
    /// - Coverage mode: Additional 2x on top of platform multiplier
    /// - Default: 1x
    pub fn multiplier() -> u64 {
        // Both inputs (target platform, coverage env vars) are fixed for the
        // life of the process, and this is called pervasively across the test
        // suite — compute it once rather than re-reading the environment on
        // every timeout lookup.
        static MULTIPLIER: OnceLock<u64> = OnceLock::new();
        *MULTIPLIER.get_or_init(|| {
            let base = if cfg!(windows) { 4 } else { 1 };
            let coverage_multiplier = if is_coverage_mode() { 2 } else { 1 };
            base * coverage_multiplier
        })
    }

    /// Unscaled base timeout (seconds) for a category, before the platform
    /// multiplier is applied.  Centralized so the harness-backstop guard test
    /// can reason about Windows scaling independently of the host platform.
    pub const fn base_secs(category: TimeoutCategory) -> u64 {
        match category {
            TimeoutCategory::ProcessSpawn => 30,
            TimeoutCategory::Handshake => 60,
            TimeoutCategory::ToolCall => 30,
            TimeoutCategory::SandboxReady => 60,
            TimeoutCategory::HttpRequest => 30,
            TimeoutCategory::SseStream => 120,
            TimeoutCategory::HealthCheck => 15,
            TimeoutCategory::Cleanup => 10,
            TimeoutCategory::Quick => 5,
        }
    }

    /// Get the timeout for a specific category.
    pub fn get(category: TimeoutCategory) -> Duration {
        Duration::from_secs(Self::base_secs(category) * Self::multiplier())
    }

    /// Scale a custom duration by the platform multiplier.
    pub fn scale(duration: Duration) -> Duration {
        duration * Self::multiplier() as u32
    }

    /// Scale milliseconds by the platform multiplier.
    pub fn scale_millis(millis: u64) -> Duration {
        Duration::from_millis(millis * Self::multiplier())
    }

    /// Scale seconds by the platform multiplier.
    pub fn scale_secs(secs: u64) -> Duration {
        Duration::from_secs(secs * Self::multiplier())
    }

    /// Get an iterator delay (for polling loops) that's appropriate for the platform.
    /// On Windows, we use longer delays to avoid overwhelming slow CI.
    pub fn poll_interval() -> Duration {
        if cfg!(windows) {
            Duration::from_millis(500)
        } else {
            Duration::from_millis(100)
        }
    }

    /// Get a short delay for inter-operation pauses.
    /// Useful after SSE exchanges before polling, etc.
    pub fn short_delay() -> Duration {
        if cfg!(windows) {
            Duration::from_secs(3)
        } else {
            Duration::from_millis(100)
        }
    }
}

/// Check if running in coverage mode (adds significant overhead).
fn is_coverage_mode() -> bool {
    std::env::var_os("LLVM_PROFILE_FILE").is_some() || std::env::var_os("CARGO_LLVM_COV").is_some()
}

/// The nextest per-test hard-kill backstop (seconds) for the subprocess-heavy
/// packages under the **CI** profile: `slow-timeout = { period = "180s",
/// terminate-after = 2 }` ⇒ 360s.  See `.config/nextest.toml`.
///
/// # Why this matters — the "opaque hang" failure mode
///
/// Any in-test loop deadline (e.g. an SSE handshake wait) MUST fire *before*
/// this backstop on every platform multiplier.  If an in-test deadline is
/// larger, nextest force-kills the test process first, and the failure presents
/// as an unexplained `TIMEOUT [360s]` with no assertion message — exactly the
/// Windows symptom that sent us in circles.  The Windows CI multiplier is ×4,
/// so a bounded category must keep `base_secs × 4 < 360` ⇒ `base_secs < 90`.
///
/// `SseStream` (120s base ⇒ 480s on Windows) deliberately exceeds this and must
/// therefore only ever bound *request ceilings* on operations that complete far
/// sooner in practice — never a handshake/readiness loop deadline.  The guard
/// test below enforces this for every other category.
pub const NEXTEST_CI_HARD_KILL_SECS: u64 = 360;

/// Windows CI timeout multiplier (kept in sync with [`TestTimeouts::multiplier`]).
pub const WINDOWS_CI_MULTIPLIER: u64 = 4;

/// Default idle-timeout (seconds) for **auto-spawned** bridges.
///
/// An auto-spawned bridge is one started implicitly by `ahma serve stdio` (proxy mode) or
/// `ahma tui` when no bridge is running.  The bridge exits this many seconds after the last
/// MCP session closes, so orphaned processes cannot accumulate after Cursor or the TUI quit.
///
/// Explicitly-started bridges (`ahma serve http`, `ahma serve unix`) default to no timeout
/// and remain running until stopped — they are user-managed servers.
///
/// Both spawn sites reference this constant so they always agree on the default.
pub const AUTO_SPAWNED_BRIDGE_IDLE_TIMEOUT_SECS: u64 = 10;

/// Hard ceiling (seconds) the HTTP bridge applies to a single `tools/call`.
///
/// The relationship this constant pins down: the in-process `await` tool bounds
/// itself by the configurable await timeout (default
/// [`crate::config::DEFAULT_AWAIT_TIMEOUT_SECS`] = 540s) and returns a graceful
/// "still running" result, and a caller-supplied `timeout_seconds` is capped at
/// this ceiling — so the bridge's own budget for the `await` call must sit
/// *strictly above* it (ceiling + margin) for the graceful in-process path to
/// fire before the bridge guillotines the HTTP request. Client request budgets
/// must not exceed it either: a budget above the ceiling promises something the
/// transport cannot keep.
pub const BRIDGE_TOOL_CALL_CEILING_SECS: u64 = 600;

/// Deadline (seconds) for the IDE-facing `ahma serve stdio` frontend to observe
/// the client's MCP handshake (the first stdin message, i.e. `initialize`).
///
/// A real MCP client sends `initialize` within milliseconds of the connection
/// opening — well before this deadline, which only starts counting once the
/// proxy loop is reading stdin (after the background bridge is healthy). If no
/// message arrives in time, the connection was spawned and abandoned (the
/// dominant cause of `ahma serve stdio` process pile-up: an editor that
/// repeatedly spawns servers without reaping them). The frontend then exits so
/// such abandoned spawns cannot accumulate.
///
/// This bounds ONLY the pre-handshake window; once the first message is seen the
/// deadline is disarmed and a live (possibly idle) session is never killed.
/// Overridable for tests (debug builds only) via `AHMA_FRONTEND_HANDSHAKE_DEADLINE_SECS`
/// env var; `0` disables the deadline.
pub const FRONTEND_HANDSHAKE_DEADLINE_SECS: u64 = 30;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multiplier_is_at_least_one() {
        assert!(TestTimeouts::multiplier() >= 1);
    }

    #[test]
    fn test_scale_preserves_zero() {
        assert_eq!(TestTimeouts::scale(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn test_categories_return_positive_durations() {
        let categories = [
            TimeoutCategory::ProcessSpawn,
            TimeoutCategory::Handshake,
            TimeoutCategory::ToolCall,
            TimeoutCategory::SandboxReady,
            TimeoutCategory::HttpRequest,
            TimeoutCategory::SseStream,
            TimeoutCategory::HealthCheck,
            TimeoutCategory::Cleanup,
            TimeoutCategory::Quick,
        ];

        for cat in categories {
            let timeout = TestTimeouts::get(cat);
            assert!(
                timeout.as_secs() > 0,
                "{:?} should have positive timeout",
                cat
            );
        }
    }

    #[test]
    fn test_scale_secs_matches_scale() {
        let secs = 10;
        assert_eq!(
            TestTimeouts::scale_secs(secs),
            TestTimeouts::scale(Duration::from_secs(secs))
        );
    }

    #[test]
    fn test_poll_interval_is_reasonable() {
        let interval = TestTimeouts::poll_interval();
        assert!(interval.as_millis() >= 100);
        assert!(interval.as_millis() <= 1000);
    }

    /// Invariant guard: every category used as an *in-test loop deadline* must
    /// fire before nextest's per-test hard-kill backstop on Windows (×4).  If
    /// this fails, a Windows test would be force-killed mid-wait and present as
    /// an opaque `TIMEOUT [360s]` with no diagnostic — the exact whack-a-mole
    /// failure mode this constant documents.  `SseStream` is intentionally
    /// excluded: it bounds request ceilings on fast-completing operations, never
    /// a handshake/readiness loop (enforced by code review + this comment).
    #[test]
    fn bounded_categories_fire_before_nextest_backstop() {
        let bounded = [
            TimeoutCategory::ProcessSpawn,
            TimeoutCategory::Handshake,
            TimeoutCategory::ToolCall,
            TimeoutCategory::SandboxReady,
            TimeoutCategory::HttpRequest,
            TimeoutCategory::HealthCheck,
            TimeoutCategory::Cleanup,
            TimeoutCategory::Quick,
        ];
        for cat in bounded {
            let windows_scaled = TestTimeouts::base_secs(cat) * WINDOWS_CI_MULTIPLIER;
            assert!(
                windows_scaled < NEXTEST_CI_HARD_KILL_SECS,
                "{:?} scaled to {}s on Windows (×{}) exceeds the {}s nextest backstop; \
                 an in-test deadline using it would be force-killed before it can fail cleanly",
                cat,
                windows_scaled,
                WINDOWS_CI_MULTIPLIER,
                NEXTEST_CI_HARD_KILL_SECS,
            );
        }
    }

    /// Documents *why* `SseStream` is the lone exception, so a future change that
    /// repurposes it as a loop deadline is caught in review against a real number.
    #[test]
    fn sse_stream_is_the_known_backstop_exception() {
        let windows_scaled =
            TestTimeouts::base_secs(TimeoutCategory::SseStream) * WINDOWS_CI_MULTIPLIER;
        assert!(
            windows_scaled >= NEXTEST_CI_HARD_KILL_SECS,
            "If SseStream now fits under the backstop, fold it into the bounded set above"
        );
    }
}
