//! `ahma_http_bridge::unit` — bridge tests that spawn nothing: `SessionManager`
//! driven directly, plus the in-process bridge harness (`spawn_in_process_server`).
//!
//! WHY a separate binary from `e2e`: these were previously throttled and granted
//! CI retries by the `package(ahma_http_bridge)` filter alongside the subprocess
//! suites, contradicting the RETRY POLICY in `.config/nextest.toml` (in-process
//! and deterministic ⇒ no retries). Splitting them out lets nextest key the
//! throttle off `binary_id(ahma_http_bridge::e2e)` only; this binary has no
//! override and runs with full parallelism. cargo-nextest still runs every test
//! in its own process, so isolation is unchanged.
//!
//! Add a new suite as `tests/unit/<name>.rs` plus a `mod` line below, alphabetically.

#[path = "../common/mod.rs"]
mod common;

mod client_response_routing_test;
mod sandbox_security_test;
mod session_coverage_test;
mod session_sandbox_test;
