//! Grants that let the SSH key broker sign (SPEC R-CRED.3).
//!
//! A grant names a key (its `SHA256:` fingerprint), what it may sign for — a
//! server, by the fingerprint of the host key the server proved it holds, or
//! an `ssh-keygen -Y` namespace — and the workspace whose commands may ask.
//! The `always` and lease tiers live in `[[sandbox.ssh_sign]]` in the settings
//! file, written only by ahma's control plane and audited; the `session` tier
//! is one owner-only file per grant under `runtime_dir()/ssh-sign/`, bound to
//! the process whose life it lasts, like a session scope grant
//! ([`crate::session_grants`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A destination prefix for a server, by host key fingerprint.
pub const HOST_PREFIX: &str = "host:";
/// A destination prefix for an `ssh-keygen -Y sign` namespace.
pub const SSHSIG_PREFIX: &str = "sshsig:";

/// One grant to sign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshSignGrant {
    /// The key, `SHA256:…` as `ssh-keygen -l` prints it.
    pub key: String,
    /// What it may sign for: `host:SHA256:…` (a server's host key) or
    /// `sshsig:<namespace>`.
    pub destination: String,
    /// What the human read when they approved: the server's names, or the
    /// namespace. Shown in listings; never matched on.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// The workspace whose commands may use it.
    pub workspace: PathBuf,
    /// Unix seconds when it was approved.
    #[serde(default)]
    pub granted_at: u64,
    /// When it lapses (a lease), Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// The surface that asked (`"harness dialog"`, `"tui"`, `"cli"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_by: Option<String>,
    /// For the session tier, the process whose life bounds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pid: Option<u32>,
}

impl SshSignGrant {
    /// Whether this grant lets `key` sign for `destination` in a command
    /// working in `workspace` at `now`.
    pub fn covers(&self, key: &str, destination: &str, workspace: &Path, now: u64) -> bool {
        self.key == key
            && self.destination == destination
            && workspace.starts_with(&self.workspace)
            && self.expires_at.is_none_or(|t| now < t)
    }
}

/// The directory session-tier grants live in: `runtime_dir()/ssh-sign`.
pub fn session_dir() -> Option<PathBuf> {
    Some(crate::hub::runtime_dir()?.join("ssh-sign"))
}

/// Record a session-tier grant under `dir`. One file per (owner, key,
/// destination, workspace): approving it again replaces it.
pub fn record_session(dir: &Path, grant: &SshSignGrant) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let key = format!(
        "{}\0{}\0{}",
        grant.key,
        grant.destination,
        grant.workspace.display()
    );
    let file = dir.join(format!(
        "{}-{}.json",
        grant.owner_pid.unwrap_or(0),
        &crate::digest::sha256_hex(key.as_bytes())[..16]
    ));
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(grant)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &file).with_context(|| format!("rename to {}", file.display()))?;
    Ok(file)
}

/// Every session-tier grant under `dir` still in force: its owner alive and
/// younger than [`crate::session_grants::MAX_AGE_SECS`]. Others are removed.
pub fn active_sessions(dir: &Path, now: u64, pid_alive: &dyn Fn(u32) -> bool) -> Vec<SshSignGrant> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let grant = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<SshSignGrant>(&raw).ok());
        let live = grant.as_ref().is_some_and(|g| {
            now.saturating_sub(g.granted_at) <= crate::session_grants::MAX_AGE_SECS
                && g.owner_pid.is_some_and(pid_alive)
        });
        match grant {
            Some(g) if live => out.push(g),
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    out
}

/// Save an `always` or lease grant to the settings file, and audit it.
pub fn persist(settings_file: &Path, grant: SshSignGrant, surface: &str) -> Result<()> {
    let mut settings = crate::config::AhmaSettings::load_from_result(settings_file)
        .map_err(|e| anyhow::anyhow!(e))
        .with_context(|| {
            format!(
                "refusing to overwrite unparseable {}",
                settings_file.display()
            )
        })?;
    let tier = if grant.expires_at.is_some() {
        crate::permissions::GrantTier::Lease
    } else {
        crate::permissions::GrantTier::Always
    };
    let subject = format!("{} for {}", grant.key, grant.destination);
    let list = &mut settings.sandbox.ssh_sign;
    list.retain(|g| {
        !(g.key == grant.key
            && g.destination == grant.destination
            && g.workspace == grant.workspace)
    });
    list.push(grant);
    settings
        .save_to(settings_file)
        .with_context(|| format!("failed to write {}", settings_file.display()))?;
    crate::permissions::append_audit(&crate::permissions::audit_entry(
        crate::config::fmt_utc_datetime(crate::session_grants::now_secs()),
        crate::permissions::AuditAction::Grant,
        crate::permissions::GrantKind::SshSign,
        subject,
        None,
        tier,
        Some(surface.to_string()),
    ));
    Ok(())
}

/// Whether any grant in `grants` lets `key` sign for `destination` from
/// `workspace` at `now`.
pub fn allowed(
    grants: &[SshSignGrant],
    key: &str,
    destination: &str,
    workspace: &Path,
    now: u64,
) -> bool {
    grants
        .iter()
        .any(|g| g.covers(key, destination, workspace, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(owner_pid: Option<u32>, expires_at: Option<u64>) -> SshSignGrant {
        SshSignGrant {
            key: "SHA256:key".into(),
            destination: format!("{HOST_PREFIX}SHA256:host"),
            label: "github.com".into(),
            workspace: PathBuf::from("/ws/repo"),
            granted_at: 1_000,
            expires_at,
            granted_by: Some("test".into()),
            owner_pid,
        }
    }

    #[test]
    fn a_grant_covers_its_key_destination_and_workspace_only() {
        let g = grant(None, Some(2_000));
        let dest = format!("{HOST_PREFIX}SHA256:host");
        assert!(g.covers("SHA256:key", &dest, Path::new("/ws/repo"), 1_500));
        assert!(g.covers("SHA256:key", &dest, Path::new("/ws/repo/sub"), 1_500));
        assert!(!g.covers("SHA256:other", &dest, Path::new("/ws/repo"), 1_500));
        assert!(!g.covers(
            "SHA256:key",
            "host:SHA256:evil",
            Path::new("/ws/repo"),
            1_500
        ));
        assert!(!g.covers("SHA256:key", &dest, Path::new("/ws/other"), 1_500));
        assert!(
            !g.covers("SHA256:key", &dest, Path::new("/ws/repo"), 2_000),
            "lapsed"
        );
    }

    #[test]
    fn session_grants_last_as_long_as_their_owner() {
        let dir = tempfile::tempdir().unwrap();
        let now = crate::session_grants::now_secs();
        let mut live = grant(Some(1), None);
        live.granted_at = now;
        let mut gone = grant(Some(2), None);
        gone.granted_at = now;
        gone.destination = "sshsig:git".into();
        record_session(dir.path(), &live).unwrap();
        record_session(dir.path(), &gone).unwrap();
        let alive = |pid: u32| pid == 1;
        assert_eq!(active_sessions(dir.path(), now, &alive), vec![live.clone()]);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "a dead owner's grant is removed"
        );
        assert!(
            active_sessions(
                dir.path(),
                now + crate::session_grants::MAX_AGE_SECS + 1,
                &alive
            )
            .is_empty(),
            "and an old one"
        );
    }

    #[test]
    fn a_persisted_grant_replaces_its_earlier_self_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.toml");
        persist(&file, grant(None, Some(5_000)), "test").unwrap();
        persist(&file, grant(None, None), "test").unwrap();
        let settings = crate::config::AhmaSettings::load_from_result(&file).unwrap();
        assert_eq!(settings.sandbox.ssh_sign, vec![grant(None, None)]);
    }
}
