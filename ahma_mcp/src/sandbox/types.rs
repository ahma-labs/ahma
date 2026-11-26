use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    Strict,
    Test,
}

/// Whether a command may write the workspace scopes or only read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAccess {
    /// An ordinary command: reads and writes the workspace.
    ReadWrite,
    /// A read-lane command (SPEC R2.7.4): reads and executes, never writes.
    ReadOnly,
}

impl WorkspaceAccess {
    pub fn is_writable(self) -> bool {
        self == Self::ReadWrite
    }
}

/// What the kernel lets one command reach. Named fields, because the write
/// and read path lists are the same type and the flags are all `bool`: passed
/// in order, a swap turned read access into write access without a compile
/// error. Built by `Sandbox::kernel_policy`, the one place the rules live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelPolicy {
    /// The workspace scopes, granted at [`Self::access`].
    pub workspace: Vec<PathBuf>,
    pub access: WorkspaceAccess,
    /// Extra paths readable but never writable (`--livelog` targets).
    pub read_only: Vec<PathBuf>,
    /// Whether the system temp directory is writable.
    pub temp_writable: bool,
    /// Whether the toolchains' package caches are writable (R-PERM.5.5).
    pub package_cache_write: bool,
    /// The only TCP port a command may connect to (the egress proxy), or
    /// `None` for unrestricted network.
    pub connect_tcp_port: Option<u16>,
    /// With `connect_tcp_port` set, the further TCP ports a command may
    /// connect to (`[network] local_ports`, SPEC R-WEB.16.13). Landlock matches
    /// the port alone, so each is reachable on every host.
    pub local_tcp_ports: Vec<u16>,
}

/// A guard that holds a read lock on the sandbox scopes.
pub struct ScopesGuard<'a>(pub(super) parking_lot::RwLockReadGuard<'a, Vec<PathBuf>>);

impl std::ops::Deref for ScopesGuard<'_> {
    type Target = [PathBuf];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Debug for ScopesGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sandbox_mode_equality() {
        assert_eq!(SandboxMode::Strict, SandboxMode::Strict);
        assert_eq!(SandboxMode::Test, SandboxMode::Test);
        assert_ne!(SandboxMode::Strict, SandboxMode::Test);
    }

    #[test]
    fn test_sandbox_mode_copy() {
        let m = SandboxMode::Strict;
        let m2 = m; // copy
        assert_eq!(m, m2);
    }

    #[test]
    fn test_sandbox_mode_clone() {
        let m = SandboxMode::Test;
        #[allow(clippy::clone_on_copy)]
        let m2 = m.clone();
        assert_eq!(m, m2);
    }

    #[test]
    fn test_sandbox_mode_debug() {
        let strict = format!("{:?}", SandboxMode::Strict);
        let test = format!("{:?}", SandboxMode::Test);
        assert!(strict.contains("Strict"));
        assert!(test.contains("Test"));
    }
}
