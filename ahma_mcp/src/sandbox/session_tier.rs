//! The `session` grant tier, shared across processes (SPEC R-PERM.4.4).
//!
//! `ahma_common::session_grants` owns the on-disk form; this module supplies
//! what only this crate can: the pid-liveness probe, the recording call every
//! approving surface makes, and the projection into the `PersistentScope`
//! list that hooks and the edit guard already filter by workspace.

use std::path::{Path, PathBuf};

use ahma_common::config::{PersistentScope, ScopeAccess};
use ahma_common::session_grants::{self, SessionGrant};

/// Whether `pid` is alive. `kill(pid, 0)` on Unix: success or `EPERM` (alive,
/// another user's) both mean alive. Off Unix the age bound alone applies.
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 performs only the permission check; nothing is sent.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Record a human-approved `session`-tier grant so hooked commands and native
/// edits in `workspace` honour it for as long as `owner_pid` lives (at most
/// [`session_grants::MAX_AGE_SECS`]). Callers have already passed the hard
/// denylist ([`super::Sandbox::add_live_grant`] refuses before they get here).
/// Best-effort: a write failure is logged, never fatal.
pub fn record_session_grant(
    path: &Path,
    access: ScopeAccess,
    workspace: Option<&Path>,
    owner_pid: u32,
    granted_by: &str,
) {
    let Some(workspace) = workspace else {
        tracing::debug!(
            path = %path.display(),
            "session grant not shared with hooks: no workspace to bind it to"
        );
        return;
    };
    let Some(dir) = session_grants::default_dir() else {
        return;
    };
    let grant = SessionGrant {
        path: dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        access,
        workspace: dunce::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf()),
        owner_pid,
        granted_at: session_grants::now_secs(),
        granted_by: Some(granted_by.to_string()),
    };
    match session_grants::record(&dir, &grant) {
        Ok(file) => tracing::info!(
            path = %grant.path.display(),
            access = access.label(),
            workspace = %grant.workspace.display(),
            "session grant recorded for terminal hooks and the edit guard: {}",
            file.display()
        ),
        Err(e) => tracing::warn!("could not record session grant: {e:#}"),
    }
}

/// The session grants in force for a command whose sandbox scopes are
/// `scopes`, as persistent-scope records (workspace-bound), ready to append to
/// `AppConfig::persistent_scopes`.
pub fn session_scopes_for(scopes: &[PathBuf]) -> Vec<PersistentScope> {
    let Some(dir) = session_grants::default_dir() else {
        return Vec::new();
    };
    let grants = session_grants::applying_to(&dir, scopes, session_grants::now_secs(), &pid_alive);
    session_grants::as_persistent_scopes(&grants)
}

/// Every session grant alive on this machine, for `ahma sandbox list`.
pub fn all_active() -> Vec<SessionGrant> {
    let Some(dir) = session_grants::default_dir() else {
        return Vec::new();
    };
    session_grants::active(&dir, session_grants::now_secs(), &pid_alive)
}

#[cfg(test)]
mod tests {
    #[test]
    fn this_process_is_alive_and_a_bogus_pid_is_not() {
        assert!(super::pid_alive(std::process::id()));
        #[cfg(unix)]
        assert!(!super::pid_alive(u32::MAX - 7));
    }
}
