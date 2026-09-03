//! `ahma_http_bridge::e2e` — every bridge test that spawns an ahma HTTP server
//! subprocess, in one binary.
//!
//! WHY: each top-level `tests/*.rs` used to be its own executable, and 19 of them
//! recompiled the 2,138-line `tests/common/` helper tree from source before
//! linking the full dependency closure. One binary per harness class links once.
//! cargo-nextest still runs every test in its own process, so isolation is
//! unchanged. `.config/nextest.toml` throttles this binary
//! (`binary_id(ahma_http_bridge::e2e)`, `threads-required`) and grants it CI
//! retries because every test here crosses a process boundary; the `smoke`
//! profile excludes it.
//!
//! Add a new suite as `tests/e2e/<name>.rs` plus a `mod` line below, alphabetically.
//! Inside a module, reach the shared helpers with `use crate::common;`.

#[path = "../common/mod.rs"]
mod common;

mod fast_error_response_test;
mod handshake_concurrency_test;
mod handshake_timeout_test;
mod http3_integration_test;
mod http_bridge_integration_test;
mod http_initialize_validation_integration_test;
mod http_roots_handshake_integration_test;
mod per_client_tools_dir_test;
mod progress_token_http_integration_test;
mod rate_limit_test;
mod request_handler_coverage_test;
mod restart_test;
mod sampling_routing_test;
mod sandbox_roots_handshake_test;
mod session_delete_test;
mod sighup_token_reload_test;
mod sse_endpoint_test;
mod sse_streaming_test;
