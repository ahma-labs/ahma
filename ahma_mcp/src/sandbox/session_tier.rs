//! The `session` grant tier, shared across processes (SPEC R-PERM.4.4).
//!
//! `ahma_common::session_grants` owns the on-disk form; this module supplies
//! what only this crate can: the pid-liveness probe, the recording call every
//! approving surface makes, and the session grants in force for a command, as
//! [`ResolvedGrant`]s whose life says they are for the session.

use std::path::{Path, PathBuf};

use ahma_common::config::ScopeAccess;
use ahma_common::grant::{GrantedBy, ResolvedGrant};
use ahma_common::session_grants::{self, SessionGrant};

/// Whether `pid` is alive, on every platform: the one probe
/// ([`ahma_common::pid_alive::pid_alive`]).
pub use ahma_common::pid_alive::pid_alive;

/// Record a human-approved `session`-tier grant so hooked commands and native
/// edits in `workspace` honour it for as long as `owner_pid` lives (at most
/// [`session_grants::MAX_AGE_SECS`]). Callers have already passed the hard
/// denylist ([`super::Sandbox::add_live_grant`] refuses before they get here).
/// Best-effort: a write failure is logged, never fatal. Returns the file
/// written, so a grant that turns out not to help can be withdrawn.
pub fn record_session_grant(
    path: &Path,
    access: ScopeAccess,
    workspace: Option<&Path>,
    owner_pid: u32,
    granted_by: GrantedBy,
) -> Option<std::path::PathBuf> {
    let Some(workspace) = workspace else {
        tracing::debug!(
            path = %path.display(),
            "session grant not shared with hooks: no workspace to bind it to"
        );
        return None;
    };
    let dir = session_grants::default_dir()?;
    let grant = SessionGrant {
        path: dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        access,
        workspace: dunce::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf()),
        owner_pid,
        granted_at: session_grants::now_secs(),
        granted_by,
    };
    match session_grants::record(&dir, &grant) {
        Ok(file) => {
            tracing::info!(
                path = %grant.path.display(),
                access = access.label(),
                workspace = %grant.workspace.display(),
                "session grant recorded for terminal hooks and the edit guard: {}",
                file.display()
            );
            Some(file)
        }
        Err(e) => {
            tracing::warn!("could not record session grant: {e:#}");
            None
        }
    }
}

/// The session grants in force for a command whose sandbox scopes are
/// `scopes`, each bound to its workspace, for the hooked command's sandbox
/// (`AppConfig::session_grants`), the edit guard and the harness-ask checks.
pub fn session_grants_for(scopes: &[PathBuf]) -> Vec<ResolvedGrant> {
    let Some(dir) = session_grants::default_dir() else {
        return Vec::new();
    };
    let grants = session_grants::applying_to(&dir, scopes, session_grants::now_secs(), &pid_alive);
    grants.iter().map(ResolvedGrant::session).collect()
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
        // On every platform: off Unix this once answered `true` for any pid,
        // so a session grant outlived its owner (SPEC R-PERM.4.4).
        assert!(!super::pid_alive(u32::MAX - 7));
    }
}
