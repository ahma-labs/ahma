//! # Ahma Common: Shared Foundation and CI Stability
//!
//! `ahma_common` provides shared types, configuration, and utilities used across the
//! entire ahma workspace. Its primary goal is a single, reliable source of truth for
//! cross-platform behavior, environmental scaling, and workspace-wide contracts.
//!
//! ## Modules
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`config`] | Workspace-wide configuration types and defaults (mutex groups, tool paths) |
//! | [`daemon_hub`] | Shared hub state for the HTTP bridge daemon |
//! | [`elicitation`] | User-input elicitation request/response types (MCP elicitation extension) |
//! | [`event_dispatcher`] | Broadcast event bus for cross-component notifications |
//! | [`file_uri`] | `file://` URI construction and parsing helpers |
//! | [`hook_consent`] | Consent tracking for permission hooks |
//! | [`keepalive`] | Keepalive ping logic for long-lived HTTP/SSE connections |
//! | [`local_tls`] | Self-signed TLS certificate generation via `rcgen` |
//! | [`observability`] | OpenTelemetry tracing initialisation helpers |
//! | [`peer_factory`] | Transport-agnostic MCP peer factory abstraction (P6) |
//! | [`process_guard`] | RAII guard that kills a child process on drop |
//! | [`prompts`] | Shared MCP prompt definitions |
//! | [`sandbox_state`] | Shared sandbox-lock state communicated across process boundaries |
//! | [`scope_decision`] | Scope-selection decision types surfaced to callers |
//! | [`scope_grant`] | Granted scope record stored after user approval |
//! | [`state_machine`] | Generic state-machine helpers |
//! | [`timeouts`] | **CI stability.** Platform-aware timeout scaling for slow runners (Windows) |
//! | [`workspace_scope`] | Workspace root discovery and scope derivation |
//!
//! ## Design Goal: Workspace Consistency
//!
//! Centralising these primitives here ensures that both the core server and the
//! bridges behave consistently across Linux, macOS, and Windows, without each
//! crate re-implementing its own platform detection or timeout heuristics.

/// Compile-time build identifier embedded by `build.rs`.
///
/// Equals the short git hash of the commit that built this binary, or a
/// `t<epoch>` fallback when git is unavailable.  Used by the stdio→bridge
/// version check to detect same-semver dev rebuilds (where the semver alone
/// is insufficient to distinguish a fresh binary from a stale bridge daemon).
pub const BUILD_ID: &str = env!("AHMA_BUILD_ID");

pub mod config;
pub mod daemon_hub;
pub mod elicitation;
pub mod event_dispatcher;
pub mod file_uri;
pub mod fs_lock;
pub mod hook_consent;
pub mod hostname;
pub mod keepalive;
pub mod local_tls;
pub mod mcp_methods;
pub mod mcp_protocol;
pub mod net_approval;
pub mod observability;
pub mod op_identity;
/// Transport-agnostic MCP peer factory abstraction (P6).
pub mod peer_factory;
pub mod permissions;
pub mod process_guard;
pub mod prompts;
pub mod sandbox_state;
pub mod scope_decision;
pub mod scope_grant;
pub mod session_event;
pub mod skills;
pub mod sse;
pub mod state_machine;
pub mod test_isolation;
pub mod timeouts;
pub mod web_approval;
pub mod web_policy;
pub mod workspace_scope;
