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
//!
//! [`host_signs_key_file`] says which key files the broker signs with itself,
//! so `ahma doctor` knows which keys need the human's agent (SPEC R-DOCTOR.6).

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

/// Whether the broker signs with the private key file `text` on the host
/// itself: an unencrypted ed25519 `openssh-key-v1` key (SPEC R-CRED.8). Any
/// other key — passphrase-protected, RSA, ECDSA, a security key — is used only
/// through the human's own agent. Only the header is read, so `ahma doctor`
/// can tell which keys need `ssh-add` without parsing a secret;
/// `ahma_mcp`'s `ssh_agent::keys::parse_private_key` is what signs, and a test
/// there holds the two to the same answer.
pub fn host_signs_key_file(text: &str) -> bool {
    let body: String = text
        .lines()
        .map(str::trim)
        .skip_while(|l| *l != "-----BEGIN OPENSSH PRIVATE KEY-----")
        .skip(1)
        .take_while(|l| *l != "-----END OPENSSH PRIVATE KEY-----")
        .collect();
    let header = || -> Option<bool> {
        let raw = decode_base64(&body)?;
        let mut rest = raw.strip_prefix(b"openssh-key-v1\0")?;
        let cipher = take_u32_prefixed(&mut rest)?;
        let kdf = take_u32_prefixed(&mut rest)?;
        let _kdf_options = take_u32_prefixed(&mut rest)?;
        if cipher != b"none" || kdf != b"none" {
            return Some(false);
        }
        let key_count = rest.get(..4)?;
        rest = &rest[4..];
        let mut public_blob = take_u32_prefixed(&mut rest)?;
        Some(
            key_count == 1u32.to_be_bytes()
                && take_u32_prefixed(&mut public_blob)? == b"ssh-ed25519",
        )
    };
    header().unwrap_or(false)
}

/// One SSH wire `string` (a big-endian `u32` length, then that many bytes),
/// taken off the front of `rest`.
fn take_u32_prefixed<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
    let value = rest.get(4..4usize.checked_add(len)?)?;
    *rest = &rest[4 + len..];
    Some(value)
}

/// Standard base64 with optional trailing padding: just enough to read a
/// key file's header, without a dependency `ahma_common` does not otherwise need.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod key_file_tests {
    //! Keys made with `ssh-keygen` for these tests (the same fixtures as
    //! `ahma_mcp`'s `ssh_agent::keys`). They protect nothing.
    use super::*;

    const PLAIN_ED25519: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACB0oJMvoCd/m51H/zkgLaQqlFDFSF4LCx+uTEYv+1580AAAAJBHctMAR3LT
AAAAAAtzc2gtZWQyNTUxOQAAACB0oJMvoCd/m51H/zkgLaQqlFDFSF4LCx+uTEYv+1580A
AAAECnXzDMMGVedTdmUvGOkGBVmMAnGzCVA3iMrzC36CjrL3Sgky+gJ3+bnUf/OSAtpCqU
UMVIXgsLH65MRi/7XnzQAAAADGZpeHR1cmVAYWhtYQE=
-----END OPENSSH PRIVATE KEY-----
";
    /// `ssh-keygen -t ed25519 -N secret`.
    const LOCKED_ED25519: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABDeTrtu0X
1MxW2Gr960g2jzAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIGkUmTXRaT8jyUUM
TVcMVJJtkpIIqlTQU+m7SaTUpO4GAAAAkPJ/J591rDg36aO4aRygBcvBa9cBEc6lxVQa2H
MMsh7MoUmUMFA4xrKwsEicX+GLVKRFl8oVs+cfhGakm9NVV7kyV3FI6rG9s1O8ekrH/+dN
Jl0xB6PUZH8pwIYOulHc0vIT74eV+7F00Ic8jZ9IZMgmgixMHDgr39vkSWha7uNeLDsHEP
zdbYUnMIxPdT/DZw==
-----END OPENSSH PRIVATE KEY-----
";
    /// `ssh-keygen -t ecdsa -b 256 -N ''`.
    const PLAIN_ECDSA: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAaAAAABNlY2RzYS
1zaGEyLW5pc3RwMjU2AAAACG5pc3RwMjU2AAAAQQSavwEZgQKcuyf3LdGPDXLblrBhuA5A
nK98/2kDWbfi61tJwDftJicoa05QKuJmij+8DQgky8dh3M7K3kEitjnFAAAAoD18SlY9fE
pWAAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBJq/ARmBApy7J/ct
0Y8NctuWsGG4DkCcr3z/aQNZt+LrW0nAN+0mJyhrTlAq4maKP7wNCCTLx2HczsreQSK2Oc
UAAAAgb7aSNxA3WMmSyfkRHINIQqv++ErAcI2buzfm4qNRfVgAAAAHZWNAYWhtYQE=
-----END OPENSSH PRIVATE KEY-----
";

    /// `ahma doctor` says "run `ssh-add`" only for a key the broker cannot
    /// sign with itself, so the header has to be read exactly.
    #[test]
    fn only_an_unencrypted_ed25519_key_is_signed_on_the_host() {
        assert!(host_signs_key_file(PLAIN_ED25519));
        assert!(!host_signs_key_file(LOCKED_ED25519), "passphrase");
        assert!(!host_signs_key_file(PLAIN_ECDSA), "ecdsa");
        assert!(!host_signs_key_file(
            "-----BEGIN RSA PRIVATE KEY-----\nMII=\n-----END RSA PRIVATE KEY-----"
        ));
        assert!(!host_signs_key_file("not a key"));
        assert!(!host_signs_key_file(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blbn*=\n-----END OPENSSH PRIVATE KEY-----"
        ));
        let truncated: String = PLAIN_ED25519.lines().take(2).collect::<Vec<_>>().join("\n");
        assert!(!host_signs_key_file(&truncated), "no END line, short body");
    }
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
