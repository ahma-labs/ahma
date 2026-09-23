//! `ahma_mcp::unit` — every pure-unit and in-process integration test, in one binary.
//!
//! WHY one binary per harness class instead of one per file: each top-level
//! `tests/*.rs` is a separate executable that statically links the whole dependency
//! closure and is relinked for every feature flavour CI builds. Linking once per
//! harness class is the single biggest lever on `target/` size and link time.
//! cargo-nextest still runs every test in its own process, so nothing about
//! isolation changes (env vars, statics and `#[serial]` are all process-local).
//!
//! Membership rule: a module belongs here only if it never forks a process
//! (`create_in_process_mcp_*`, `McpClientFixture`, `RecordingClient`, or plain
//! library calls). Anything that spawns `ahma` — or whose harness is mixed — lives
//! in `tests/e2e/main.rs`. `.config/nextest.toml` deliberately has NO override for
//! `binary_id(ahma_mcp::unit)`: it runs with full parallelism and no CI retries,
//! because an in-process test that flakes is a real bug.
//!
//! Add a new suite as `tests/unit/<name>.rs` plus a `mod` line below, in
//! alphabetical order; never as a new top-level `tests/*.rs` file
//! (scripts/check-guardrails.sh rejects those).

mod adapter_extended_coverage_test;
mod adapter_retry_integration_test;
mod adapter_sandbox_scope_regression_test;
mod async_first_contract_e2e_test;
mod builtin_tools_test;
mod cli_pure_functions_test;
mod client_coverage_test;
mod config_coverage_test;
mod development_workflow_invariants_test;
mod exec_audit_test;
mod execution_mode_test;
mod file_edit_tools_test;
mod file_tools_schema_validation_test;
mod flattened_tool_test;
mod graceful_shutdown_test;
mod guard_rail_test;
mod harness_tool_client_gating_test;
mod hooks_readiness_test;
mod idle_watchdog_liveness_test;
mod livelog_config_test;
mod livelog_file_monitor_test;
mod livelog_integration_test;
mod livelog_pipeline_test;
mod log_monitor_integration_test;
mod logging_unit_test;
mod mcp_service_coverage_improvement_test;
mod operation_monitor;
mod package_cache_write_test;
mod path_security_edge_cases_test;
mod path_security_test;
mod permission_ladder_test;
mod progress_notification_e2e_test;
mod pty_session_exec_test;
mod retired_env_drift_test;
mod retry_logic_test;
mod sandbox_command_test;
mod sandbox_coverage_test;
mod sandbox_error_unit_test;
mod sandbox_gate_covers_every_tool_test;
mod sandbox_high_security_test;
mod sandbox_livelog_test;
mod sandbox_unit_test;
mod schema_sync_test;
mod schema_validation;
mod schema_validation_test;
mod security_and_depth_test;
mod sequence_failure_edge_cases_test;
mod sequence_integration_coverage_test;
mod status_polling_anti_pattern_test;
mod terminal_output_test;
mod time_serde_test;
mod tmp_flag_test;
mod tool_availability_coverage_test;
mod tool_availability_integration_test;
mod tool_config_schema_validation_test;
mod tool_examples_execution_test;
mod tool_suite;
mod update_test;
mod vscode_mcp_config_test;
