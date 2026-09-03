//! `ahma_mcp::e2e` — every integration test that crosses a process boundary, in one binary.
//!
//! WHY one binary per harness class instead of one per file: each top-level
//! `tests/*.rs` is a separate executable that statically links the whole dependency
//! closure and is relinked for every feature flavour CI builds. Linking once per
//! harness class is the single biggest lever on `target/` size and link time.
//! cargo-nextest still runs every test in its own process, so nothing about
//! isolation changes (env vars, statics and `#[serial]` are all process-local).
//!
//! Membership rule: modules that spawn `ahma` / `generate-tool-schema`
//! (`ClientBuilder`, `Command::new`, `build_binary_cached`, `get_binary_path`), the
//! OS-gated kernel-sandbox suites (their `#![cfg(target_os = "...")]` inner
//! attributes keep working as module attributes), and every file whose harness is
//! mixed or unclear — conservative, so it stays throttled. `.config/nextest.toml`
//! keys `threads-required` and the CI/coverage `retries` off
//! `binary_id(ahma_mcp::e2e)`; the `smoke` profile excludes it.
//!
//! Add a new suite as `tests/e2e/<name>.rs` plus a `mod` line below, in
//! alphabetical order; never as a new top-level `tests/*.rs` file
//! (scripts/check-guardrails.sh rejects those).

mod automatic_async_integration_test;
mod cli_binary_integration_test;
mod cli_extended_integration_test;
mod cli_mode_coverage_test;
mod client_coverage_expansion_test;
mod config_reload_test;
mod direct_stdio_roots_relock_test;
mod extensibility_integration_test;
mod external_mcp_routing_test;
mod file_tools_integration_test;
mod freeform_args_test;
mod full_system_integration_bug_test;
mod generate_schema_test;
mod linux_legacy_kernel_sandbox_test;
mod linux_sandbox_integration_test;
mod macos_sandbox_integration_test;
mod mcp_cancellation_bug_test;
mod mcp_integration_tests;
mod mcp_service;
// Uses `McpClientFixture`, which spawns the `ahma` binary — an e2e harness, not unit.
mod mcp_service_edge_cases_test;
mod mcp_service_integration_coverage_test;
mod mcp_service_integration_test;
mod multiline_argument_handling_test;
mod nested_sandbox_exit_test;
mod nested_seatbelt_deferral_test;
mod proxy_client_integration_test;
mod sandbox_lifecycle_notification_test;
mod sandbox_security_red_team_test;
mod shell_list_tools_integration_test;
mod stdio_handshake_test;
mod test_utils_coverage_test;
mod tools_dir_auto_detection_test;
mod transport_patch_extended_test;
mod update_tools_unit_test;
mod windows_sandbox_integration_test;
