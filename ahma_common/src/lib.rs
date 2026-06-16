//! # Ahma Common: Shared Foundation and CI Stability
//!
//! `ahma_common` provides a collection of shared types, constants, and utilities
//! used across the entire Ahma workspace. Its primary goal is to provide a single,
//! reliable "ground truth" for cross-platform behavior and environmental scaling.
//!
//! ## Core Utilities
//!
//! - **[`timeouts`]**: The most critical component for CI stability. It provides
//!   platform-aware timeout scaling to ensure that slow CI runners (particularly
//!   Windows) don't experience intermittent failures during process spawning or
//!   network operations.
//!
//! ## Design Goal: Workspace Consistency
//!
//! By centralizing these primitives here, we ensure that both the core server and the
//! bridges behave consistently regardless of the OS they are running on.

/// Compile-time build identifier embedded by `build.rs`.
///
/// Equals the short git hash of the commit that built this binary, or a
/// `t<epoch>` fallback when git is unavailable.  Used by the stdio→bridge
/// version check to detect same-semver dev rebuilds (where the semver alone
/// is insufficient to distinguish a fresh binary from a stale bridge daemon).
pub const BUILD_ID: &str = env!("AHMA_BUILD_ID");

pub mod config;
pub mod daemon_hub;
pub mod event_dispatcher;
pub mod file_uri;
pub mod keepalive;
pub mod local_tls;
pub mod observability;
/// Transport-agnostic MCP peer factory abstraction (P6).
pub mod peer_factory;
pub mod peer_transport;
pub mod prompts;
pub mod sandbox_state;
pub mod state_machine;
pub mod timeouts;
