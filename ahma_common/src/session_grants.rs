//! Session-tier scope grants that outlive one process (SPEC R-PERM.4.4).
//!
//! A `session` answer at a prompt ("read-write for this session", `[s]` in the
//! TUI) used to live only in the memory of the MCP server that asked. Terminal
//! hooks re-derive their sandbox per command from `settings.toml`, so a hook
//! user had no session tier at all: the human approved, the next hooked
//! command was still denied, and the only way out was to persist forever.
//!
//! This module gives the session tier a home every surface can read: one small
//! JSON file per grant under `runtime_dir()/session-grants/` (owner-only, like
//! the hub sockets beside it). A grant names the **workspace** it was approved
//! for and the **pid that owns it** — the MCP server, or the interactive shell
//! an `ahma sandbox grant --session` was typed into — and is dropped the moment
//! that pid is gone or after [`MAX_AGE_SECS`], whichever is first. Nothing here
//! bypasses the hard denylist: callers apply [`crate::scope_grant::classify_grant_risk`]
//! before recording, exactly as `persist_grant` does for the `always` tier.
//!
//! Liveness is a closure (`pid_alive`) rather than a syscall here, so this crate
//! stays free of `libc` and the behaviour is testable without real processes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{PersistentScope, ScopeAccess};

/// Session grants older than this are dropped even if their owner is alive: a
/// "for this session" answer must not quietly become a day-long one.
pub const MAX_AGE_SECS: u64 = 12 * 60 * 60;

/// One session-tier grant, as stored on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGrant {
    /// The granted directory, canonical.
    pub path: PathBuf,
    pub access: ScopeAccess,
    /// The workspace (project root) the human approved it for. Applies only to
    /// sessions and hooked commands working inside it (SPEC R5.4.11).
    pub workspace: PathBuf,
    /// The process whose lifetime bounds the grant.
    pub owner_pid: u32,
    /// Unix seconds when it was approved.
    pub granted_at: u64,
    /// The surface or tool that asked (`"tui"`, `"harness"`, `"cli"`, a tool name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_by: Option<String>,
}

/// The directory session grants live in: `runtime_dir()/session-grants`.
pub fn default_dir() -> Option<PathBuf> {
    Some(crate::hub::runtime_dir()?.join("session-grants"))
}

/// Unix seconds now.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record `grant` under `dir`, returning the file written. One file per
/// `(owner_pid, path, access)`, so re-approving the same thing overwrites
/// rather than accumulates.
pub fn record(dir: &Path, grant: &SessionGrant) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create session-grant dir {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let file = dir.join(file_name(grant));
    let json = serde_json::to_vec_pretty(grant)?;
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &file).with_context(|| format!("rename to {}", file.display()))?;
    Ok(file)
}

fn file_name(grant: &SessionGrant) -> String {
    let key = format!("{}\0{}", grant.path.display(), grant.access.short());
    format!(
        "{}-{}.json",
        grant.owner_pid,
        &crate::digest::sha256_hex(key.as_bytes())[..16]
    )
}

/// Every session grant still in force under `dir`: owner alive and younger
/// than [`MAX_AGE_SECS`]. Files for dead or expired grants are removed as a
/// side effect, so the directory never accumulates.
pub fn active(dir: &Path, now: u64, pid_alive: &dyn Fn(u32) -> bool) -> Vec<SessionGrant> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let grant = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<SessionGrant>(&raw).ok());
        let Some(grant) = grant else {
            // Unreadable or malformed: not a grant anyone can rely on. Drop it.
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let expired = now.saturating_sub(grant.granted_at) > MAX_AGE_SECS;
        if expired || !pid_alive(grant.owner_pid) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        out.push(grant);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.owner_pid.cmp(&b.owner_pid)));
    out
}

/// The active grants that apply to a command whose sandbox scopes are `scopes`:
/// the grant's workspace is one of the scopes, or encloses one of them.
pub fn applying_to(
    dir: &Path,
    scopes: &[PathBuf],
    now: u64,
    pid_alive: &dyn Fn(u32) -> bool,
) -> Vec<SessionGrant> {
    active(dir, now, pid_alive)
        .into_iter()
        .filter(|g| scopes.iter().any(|s| s.starts_with(&g.workspace)))
        .collect()
}

/// Session grants in the shape the sandbox already consumes for persistent
/// grants, so hooks and the edit guard apply them through the same
/// workspace-filtered path (SPEC R5.4.11).
pub fn as_persistent_scopes(grants: &[SessionGrant]) -> Vec<PersistentScope> {
    grants
        .iter()
        .map(|g| PersistentScope {
            path: g.path.clone(),
            access: g.access,
            workspace: Some(g.workspace.clone()),
            granted_by: Some(
                g.granted_by
                    .clone()
                    .map(|by| format!("session grant ({by})"))
                    .unwrap_or_else(|| "session grant".to_string()),
            ),
            granted_at: None,
            note: Some(format!("for this session only (owner pid {})", g.owner_pid)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn grant(path: &Path, ws: &Path, pid: u32, at: u64) -> SessionGrant {
        SessionGrant {
            path: path.to_path_buf(),
            access: ScopeAccess::Rw,
            workspace: ws.to_path_buf(),
            owner_pid: pid,
            granted_at: at,
            granted_by: Some("tui".into()),
        }
    }

    #[test]
    fn a_recorded_grant_is_active_while_its_owner_lives() {
        let td = tempdir().unwrap();
        let dir = td.path().join("session-grants");
        let g = grant(Path::new("/cache/x"), Path::new("/ws/a"), 4242, 1_000);
        record(&dir, &g).unwrap();
        let alive = |pid: u32| pid == 4242;
        assert_eq!(active(&dir, 1_100, &alive), vec![g.clone()]);
        // Re-approving the same thing overwrites, it does not duplicate.
        record(&dir, &g).unwrap();
        assert_eq!(active(&dir, 1_100, &alive).len(), 1);
    }

    #[test]
    fn dead_owner_or_expiry_removes_the_grant_file() {
        let td = tempdir().unwrap();
        let dir = td.path().join("session-grants");
        let live = grant(Path::new("/cache/live"), Path::new("/ws"), 1, 1_000);
        let dead = grant(Path::new("/cache/dead"), Path::new("/ws"), 2, 1_000);
        let old = grant(Path::new("/cache/old"), Path::new("/ws"), 1, 0);
        for g in [&live, &dead, &old] {
            record(&dir, g).unwrap();
        }
        let alive = |pid: u32| pid == 1;
        let now = MAX_AGE_SECS + 500;
        assert_eq!(active(&dir, now, &alive), vec![live]);
        let remaining = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(remaining, 1, "dead and expired files are pruned");
    }

    #[test]
    fn applying_to_filters_by_workspace() {
        let td = tempdir().unwrap();
        let dir = td.path().join("session-grants");
        let a = grant(Path::new("/cache/a"), Path::new("/ws/a"), 1, 1_000);
        let b = grant(Path::new("/cache/b"), Path::new("/ws/b"), 1, 1_000);
        record(&dir, &a).unwrap();
        record(&dir, &b).unwrap();
        let alive = |_: u32| true;
        let scopes = vec![PathBuf::from("/ws/a/crate")];
        assert_eq!(applying_to(&dir, &scopes, 1_100, &alive), vec![a.clone()]);
        assert!(applying_to(&dir, &[PathBuf::from("/elsewhere")], 1_100, &alive).is_empty());
        let ps = as_persistent_scopes(&[a]);
        assert_eq!(ps[0].workspace.as_deref(), Some(Path::new("/ws/a")));
        assert!(
            ps[0]
                .granted_by
                .as_deref()
                .unwrap()
                .contains("session grant")
        );
    }

    #[test]
    fn a_malformed_file_is_dropped_not_trusted() {
        let td = tempdir().unwrap();
        let dir = td.path().join("session-grants");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("junk.json"), b"{not json").unwrap();
        assert!(active(&dir, 1, &|_| true).is_empty());
        assert!(!dir.join("junk.json").exists());
    }
}
