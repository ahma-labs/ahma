//! # Ahma Common: Shared Foundation and CI Stability
//!
//! `ahma_common` provides shared types, configuration, and utilities used across the
//! entire ahma workspace. Its primary goal is a single, reliable source of truth for
//! cross-platform behavior, environmental scaling, and workspace-wide contracts.
//!
//! ## Modules
//!
//! Grouped by the contract they carry; `ahma_common/SPEC.md` states each one.
//!
//! | Area | Modules |
//! |------|---------|
//! | Configuration | [`config`] (settings schema, trust tiers, provider registry, retired-env handling) |
//! | Permissions and scope | [`permissions`], [`workspace_scope`], [`scope_decision`], [`scope_grant`], [`sandbox_state`], [`elicitation`], [`hook_consent`] |
//! | Network policy and outbound HTTP | [`web_policy`], [`web_approval`], [`net_approval`], [`http_retry`] |
//! | Operations | [`event_dispatcher`], [`op_identity`] |
//! | Per-user daemon | [`daemon_hub`], [`daemon_endpoint`], [`daemon_history`] |
//! | MCP wire | [`mcp_methods`], [`mcp_protocol`], [`session_event`], [`keepalive`], [`sse`], [`peer_factory`] |
//! | Process and platform | [`test_isolation`], [`process_guard`], [`timeouts`], [`file_uri`], [`hostname`], [`fs_lock`], [`local_tls`], [`digest`] |
//! | Shared definitions | [`prompts`], [`skills`], [`simplify_args`], [`state_machine`], [`doctor`], [`observability`] |
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
pub mod daemon_endpoint;
pub mod daemon_history;
pub mod daemon_hub;
/// SHA-256 hex digests, one encoder for every `SHA256SUMS`-style surface.
pub mod digest;
pub mod doctor;
pub mod elicitation;
pub mod event_dispatcher;
pub mod file_uri;
pub mod fs_lock;
pub mod hook_consent;
pub mod hostname;
pub mod http_retry;
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
/// clap arguments for `ahma simplify`, shared by the CLI parser (`ahma_mcp`) and
/// the analysis engine (`ahma_simplify`) so neither depends on the other.
pub mod simplify_args;
pub mod skills;
pub mod sse;
pub mod state_machine;
pub mod test_isolation;
pub mod timeouts;
pub mod web_approval;
pub mod web_policy;
pub mod workspace_scope;
