//! # Platform Shell Selection and Command Timeout Configuration
//!
//! This module provides two small pieces of execution infrastructure:
//!
//! * [`platform_shell_program`] — the shell binary used for command execution
//!   (`powershell` on Windows, `bash` elsewhere).
//! * [`ShellPoolConfig`] / [`ShellPoolManager`] — the shared default command
//!   timeout consumed by the [`Adapter`](crate::adapter::Adapter).
//!
//! ## Historical note
//!
//! This module once contained a prewarmed shell pool (`PrewarmedShell`,
//! `ShellPool`, and the pooling half of `ShellPoolManager`). That machinery had
//! zero production callers — command execution runs through
//! [`ShellSessionManager`](crate::shell_session::ShellSessionManager) PTY
//! sessions and direct sandboxed spawns — so it was removed as dead code. The
//! `ShellPoolManager` name is retained to keep the adapter construction API
//! stable; it is now purely a timeout-configuration holder.

use std::time::Duration;

// ---------------------------------------------------------------------------
// Platform-specific shell helpers
// ---------------------------------------------------------------------------

/// The shell binary used for command execution.
/// On Windows we use the built-in PowerShell (`powershell`); on all other
/// platforms we use `bash`.
///
/// Also used by `mcp_service` handlers to build progress descriptions.
pub(crate) fn platform_shell_program() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "powershell"
    }
    #[cfg(not(target_os = "windows"))]
    {
        "bash"
    }
}

/// Execution timeout configuration shared through the [`Adapter`](crate::adapter::Adapter).
///
/// Only `command_timeout` is consumed in production: it is the default budget
/// applied to a tool invocation when the tool config does not specify its own
/// timeout.
#[derive(Debug, Clone)]
pub struct ShellPoolConfig {
    /// Default per-command execution timeout.
    pub command_timeout: Duration,
}

impl Default for ShellPoolConfig {
    fn default() -> Self {
        Self {
            command_timeout: Duration::from_secs(300),
        }
    }
}

/// Holder for the shared [`ShellPoolConfig`].
///
/// The prewarmed shell pool this manager once oversaw was removed as dead
/// code; production execution runs through
/// [`ShellSessionManager`](crate::shell_session::ShellSessionManager) PTY
/// sessions. The adapter keeps an `Arc<ShellPoolManager>` purely to read the
/// default command timeout via [`config`](Self::config).
#[derive(Debug)]
pub struct ShellPoolManager {
    config: ShellPoolConfig,
}

impl ShellPoolManager {
    /// Create a new manager holding `config`.
    pub fn new(config: ShellPoolConfig) -> Self {
        Self { config }
    }

    /// Get the shared configuration.
    pub fn config(&self) -> &ShellPoolConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::logging::init_test_logging;

    #[test]
    fn test_shell_pool_config_default_timeout() {
        init_test_logging();
        let config = ShellPoolConfig::default();
        assert_eq!(config.command_timeout, Duration::from_secs(300));
    }

    #[test]
    fn test_manager_returns_configured_timeout() {
        init_test_logging();
        let manager = ShellPoolManager::new(ShellPoolConfig {
            command_timeout: Duration::from_secs(42),
        });
        assert_eq!(manager.config().command_timeout, Duration::from_secs(42));
    }

    #[test]
    fn test_platform_shell_program_matches_platform() {
        init_test_logging();
        #[cfg(target_os = "windows")]
        assert_eq!(platform_shell_program(), "powershell");
        #[cfg(not(target_os = "windows"))]
        assert_eq!(platform_shell_program(), "bash");
    }
}
