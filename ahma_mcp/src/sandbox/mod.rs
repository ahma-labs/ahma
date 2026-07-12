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

pub mod build_diagnostics;
pub mod capability_denial;
mod command;
pub mod confinement;
pub(crate) mod core;
pub mod credential_reads;
pub mod denial_scan;
pub mod display;
mod error;
pub mod grant_channel;
pub mod host_detect;
#[cfg(target_os = "linux")]
mod landlock;
pub mod permission_broker;
mod prerequisites;
pub mod profiles;
mod scope_lock;
mod scopes;
#[cfg(target_os = "macos")]
mod seatbelt;
mod types;
#[cfg(target_os = "windows")]
mod windows;

pub use capability_denial::{
    Capability, EnforcingLayer, capability_denial_disclosure, scan_capability_denial,
};
pub use command::{
    apply_egress_proxy_env, is_secret_env_name, scrub_secret_env, secret_env_keys,
    set_egress_proxy_env, set_secret_env_allow,
};
pub use confinement::outer_confinement;
pub use core::Sandbox;
pub use core::{add_log_exception, is_target_allowed, load_exceptions};
pub use credential_reads::{
    default_credential_read_denies, effective_credential_read_denies, keychain_access_allowed,
    set_credential_read_denies, set_keychain_access_allowed,
};
pub use denial_scan::{DenialHit, scan_denial};
pub use display::{ActiveSandbox, ScopeSource, ScopeView};
pub use error::SandboxError;
pub use grant_channel::{HubGrantNotifier, LoggingGrantNotifier, ScopeGrantNotifier};
pub use host_detect::{HostSandbox, detect_host_sandbox};
#[cfg(target_os = "linux")]
pub use landlock::{
    apply_landlock_ruleset_in_child, enforce_landlock_sandbox, landlock_ruleset_fd,
};
pub use permission_broker::{
    AskedAt, ElicitOutcome, ElicitationSurface, PeerElicitationSurface, PermissionBroker,
    hook_fail_closed_message,
};
pub use prerequisites::{
    check_sandbox_prerequisites, exit_with_sandbox_error, test_sandbox_exec_available,
};
pub use scope_lock::ScopeLockState;
pub use scopes::normalize_path_lexically;
pub use types::{SandboxMode, ScopesGuard};
#[cfg(target_os = "windows")]
pub use windows::{check_windows_sandbox_available, enforce_windows_sandbox};
