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

use crate::config::ScopeAccess;
use crate::grant::GrantedBy;

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
    /// Who made it, stored as the text it always was (`"tui"`, `"web"`,
    /// `"cli"`, `"harness dialog"`, a tool name).
    #[serde(default, skip_serializing_if = "GrantedBy::is_unmarked")]
    pub granted_by: GrantedBy,
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
    crate::fs_atomic::write(&file, json).with_context(|| format!("write {}", file.display()))?;
    Ok(file)
}

/// Remove the session grant recorded in `file` (from [`record`]): a grant
/// the run it was approved for showed did not help (SPEC R-PERM.10.3).
/// A file already gone is fine.
pub fn remove(file: &Path) -> Result<()> {
    match std::fs::remove_file(file) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("remove {}", file.display()))
        }
        _ => Ok(()),
    }
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

/// End the session grants of `path` approved for `workspace` (every workspace when `None`), as a
/// revoke of the saved grant does (SPEC R-PERM.11): the undo takes effect now, not when the session
/// ends. Paths compare as the session grant stored them, canonical. Returns how many ended.
pub fn revoke_matching(dir: &Path, path: &Path, workspace: Option<&Path>) -> usize {
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (path, workspace) = (canon(path), workspace.map(canon));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut ended = 0;
    for file in entries.flatten().map(|e| e.path()) {
        let grant = std::fs::read(&file)
            .ok()
            .and_then(|raw| serde_json::from_slice::<SessionGrant>(&raw).ok());
        let Some(grant) = grant else { continue };
        let matches = canon(&grant.path) == path
            && workspace
                .as_deref()
                .is_none_or(|ws| canon(&grant.workspace) == ws);
        if matches && remove(&file).is_ok() {
            ended += 1;
        }
    }
    ended
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
            granted_by: crate::approver::ApproverSurface::Tui.into(),
        }
    }

    /// The file holds its maker as the text it always has (`"tui"`), leaves
    /// the key out when no maker was recorded, and a file naming a maker this
    /// build does not know still loads, as made.
    #[test]
    fn a_session_grant_file_holds_its_maker_as_the_text_it_always_has() {
        use crate::grant::GrantedBy;
        let td = tempdir().unwrap();
        let dir = td.path().join("session-grants");
        let mut g = grant(Path::new("/cache/a"), Path::new("/ws/a"), 7, 1_000);
        let stored = |file: &Path| -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap()
        };
        let file = record(&dir, &g).unwrap();
        let mut on_disk = stored(&file);
        assert_eq!(on_disk["granted_by"], "tui");
        assert_eq!(active(&dir, 1_100, &|_| true), vec![g.clone()]);

        on_disk["granted_by"] = "a surface from a newer ahma".into();
        std::fs::write(&file, serde_json::to_vec(&on_disk).unwrap()).unwrap();
        let read = active(&dir, 1_100, &|_| true);
        assert_eq!(
            read[0].granted_by,
            GrantedBy::Named("a surface from a newer ahma".into())
        );

        g.granted_by = GrantedBy::Unmarked;
        let file = record(&dir, &g).unwrap();
        assert!(
            stored(&file).get("granted_by").is_none(),
            "no maker, no key"
        );
        assert_eq!(active(&dir, 1_100, &|_| true), vec![g]);
    }

    /// Two writers recording the same grant at once (two hooks approving the
    /// same folder) both succeed. Every writer staged its file at one fixed
    /// name, `<file>.json.tmp`, so the second rename found the first had moved
    /// it away and failed, or renamed a file the other was still writing.
    #[test]
    fn writers_recording_the_same_grant_at_once_all_succeed() {
        let td = tempdir().unwrap();
        let dir = td.path().join("session-grants");
        let g = grant(
            Path::new("/github/neubit4"),
            Path::new("/github/ahma"),
            7,
            1_000,
        );
        let failures: Vec<String> = std::thread::scope(|scope| {
            let writers: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        (0..50)
                            .filter_map(|_| record(&dir, &g).err().map(|e| format!("{e:#}")))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            writers
                .into_iter()
                .flat_map(|w| w.join().unwrap())
                .collect()
        });
        assert!(
            failures.is_empty(),
            "{} failed: {:?}",
            failures.len(),
            failures.first()
        );
        assert_eq!(active(&dir, 1_100, &|_| true).len(), 1, "one grant, whole");
        let strays = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .count();
        assert_eq!(strays, 0, "no staging file left behind");
    }

    /// SPEC R-PERM.11: the undo ahma prints ends the grant now, not at the session's end. A
    /// revoke removed only the saved grant and left the session grant the dialog's yes made in
    /// force for up to 12 hours.
    #[test]
    fn a_revoke_ends_the_session_grants_of_that_folder_and_workspace() {
        let td = tempdir().unwrap();
        let dir = td.path().join("session-grants");
        let (repo, other) = (Path::new("/github/neubit4"), Path::new("/cache/x"));
        let (ahma, alt) = (Path::new("/github/ahma"), Path::new("/github/alt"));
        record(&dir, &grant(repo, ahma, 1, 1_000)).unwrap();
        record(&dir, &grant(repo, alt, 2, 1_000)).unwrap();
        record(&dir, &grant(other, ahma, 1, 1_000)).unwrap();
        assert_eq!(revoke_matching(&dir, repo, Some(ahma)), 1);
        let left: Vec<_> = active(&dir, 1_100, &|_| true)
            .into_iter()
            .map(|g| (g.path, g.workspace))
            .collect();
        assert_eq!(
            left,
            vec![
                (other.to_path_buf(), ahma.to_path_buf()),
                (repo.to_path_buf(), alt.to_path_buf())
            ]
        );
        // With no workspace (a `--global` revoke), every session grant of the folder ends.
        assert_eq!(revoke_matching(&dir, repo, None), 1);
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
        let g = crate::grant::ResolvedGrant::session(&a);
        assert_eq!(g.workspace(), Some(Path::new("/ws/a")));
        assert_eq!(g.by.label().as_deref(), Some("tui"));
    }

    /// A session grant keeps its maker as it was made. Its records were
    /// written with `granted_by: "session grant (<maker>)"`, a string that
    /// `grant::GrantedBy` then had to unwrap again to tell whether a person
    /// made it; that it is for the session is its `grant::GrantLife`.
    #[test]
    fn a_session_grant_keeps_its_maker_as_made() {
        let mut g = grant(Path::new("/cache/a"), Path::new("/ws/a"), 7, 1_000);
        g.granted_by = crate::grant::GrantedBy::HarnessDialog;
        let resolved = crate::grant::ResolvedGrant::session(&g);
        assert_eq!(resolved.by, crate::grant::GrantedBy::HarnessDialog);
        assert!(resolved.is_session());
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
