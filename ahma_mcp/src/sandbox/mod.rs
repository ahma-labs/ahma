//! # Kernel-Level Sandboxing: The Security Boundary
//!
//! This module implements the core security philosophy of Ahma: **Kernel-Enforced
//! Isolation**. Unlike user-space checks that can be bypassed by clever shell
//! engineering, Ahma relies on the OS kernel to block unauthorized filesystem
//! access at the syscall level.
//!
//! ## Security Philosophy
//!
//! 1. **Immutable Scope**: Once a sandbox is initialized and "locked" at the start
//!    of a session, it cannot be expanded. This prevents "scope creep" by a compromised
//!    agent.
//! 2. **Fail-Closed Strategy**: If a platform-specific security backend is unavailable
//!    or fails to initialize, Ahma defaults to a "fail-closed" state, refusing to execute
//!    commands in strict mode unless sandboxing is explicitly disabled by the operator.
//! 3. **Minimal Whitelisting**: Beyond the explicitly granted workspace roots, only
//!    essential system binaries and library paths (e.g., `/usr/bin`, `/etc/ssl`) are
//!    whitelisted for read/execute access.
//!
//! ## Platform Implementations
//!
//! While the mechanisms differ by OS, they all provide the same functional guarantee
//! of read/write isolation for the AI:
//!
//! - **Linux (Landlock)**: Uses the Landlock LSM (available in kernel 5.13+). Because
//!   `landlock_restrict_self(2)` only restricts the calling thread, each spawned command
//!   gets the ruleset applied in `pre_exec` (between fork and exec), guaranteeing
//!   kernel-level containment regardless of which runtime thread spawns it.
//! - **macOS (Seatbelt)**: Uses the system's `sandbox-exec` utility with a dynamically
//!   generated SBPL (Sandbox Binary Policy Language) profile.
//! - **Windows (Job Objects)**: Uses Job Objects to ensure child process cleanup and (in
//!   development) AppContainer isolation for filesystem gating.
//!
//! ## Architecture
//!
//! The [`Sandbox`](crate::sandbox::Sandbox) struct acts as the primary orchestrator. It holds the security
//! policy and is used by the [`Adapter`](crate::adapter::Adapter) to validate paths
//! and wrap command executions in platform-appropriate security wrappers.

mod command;
pub(crate) mod core;
pub mod display;
mod error;
#[cfg(target_os = "linux")]
mod landlock;
pub(crate) mod pkg_cache;
mod prerequisites;
mod scopes;
#[cfg(target_os = "macos")]
mod seatbelt;
mod types;
#[cfg(target_os = "windows")]
mod windows;

pub use core::Sandbox;
pub use core::{add_log_exception, is_target_allowed, load_exceptions};
pub use display::{ScopeSource, ScopeView};
pub use error::SandboxError;
#[cfg(target_os = "linux")]
pub use landlock::{
    apply_landlock_ruleset_in_child, enforce_landlock_sandbox, landlock_ruleset_fd,
};
pub use prerequisites::{
    check_sandbox_prerequisites, exit_with_sandbox_error, test_sandbox_exec_available,
};
pub use scopes::normalize_path_lexically;
pub use types::{SandboxMode, ScopesGuard};
#[cfg(target_os = "windows")]
pub use windows::{check_windows_sandbox_available, enforce_windows_sandbox};
