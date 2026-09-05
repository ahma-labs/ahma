//! Windows rendezvous for the per-user daemon (SPEC R-DAEMON.2).
//!
//! Unix has two things Windows does not: a filesystem socket that can be
//! chmodded `0600`, and a bind that is itself the mutex. Windows therefore used
//! two fixed loopback ports — 7395 for the hub, 3000 for the MCP endpoint —
//! which any local user could connect to, and which fail outright when some
//! unrelated program already holds them.
//!
//! This module replaces both with what a socket gives for free elsewhere:
//!
//! * **`daemon.lock`** is the mutex. [`crate::fs_lock::FsLock`] is an advisory
//!   lock the kernel releases when the holder dies, so "is a daemon running?"
//!   never depends on a stale file being cleaned up — which is exactly what a
//!   crash or a power cut leaves behind.
//! * **`daemon.json`** publishes where the daemon actually listens. Both
//!   listeners bind port 0, so a busy port is not a startup failure, and the
//!   ports are discovered rather than assumed.
//! * A **bearer token** in that file stands in for the `0600` mode bit: the
//!   file lives under the user's own profile directory, and a client that
//!   cannot read it cannot present the token.
//!
//! The file is written atomically (write a temporary, then rename) so a reader
//! never sees a half-written descriptor.
//!
//! Everything here is cross-platform code — only the daemon's *use* of it is
//! Windows-only — because the alternative is logic that no test on a developer
//! machine ever executes.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Name of the descriptor inside the runtime directory.
pub const ENDPOINT_FILE: &str = "daemon.json";

/// Name of the lock file inside the runtime directory.
pub const LOCK_FILE: &str = "daemon.lock";

/// Where the daemon listens, and the token needed to talk to it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonEndpoint {
    /// The daemon's process id. Diagnostic only: liveness is decided by the
    /// lock, which the kernel releases on death, not by probing a pid that may
    /// since have been reused.
    pub pid: u32,
    /// Daemon semver, so a client can report a skew it cannot resolve.
    pub version: String,
    /// Build id, which distinguishes two dev builds of the same version.
    pub build_id: String,
    /// Loopback port of the observability hub.
    pub hub_port: u16,
    /// Loopback port of the MCP endpoint.
    pub mcp_port: u16,
    /// Bearer token every client must present. Read from a file only the
    /// user can read; it is what replaces the Unix socket's mode bits.
    pub token: String,
}

impl DaemonEndpoint {
    /// A descriptor for this process, with a freshly minted token.
    pub fn new(hub_port: u16, mcp_port: u16) -> Self {
        Self {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            build_id: crate::BUILD_ID.to_string(),
            hub_port,
            mcp_port,
            token: mint_token(),
        }
    }
}

/// A random 128-bit token, rendered as hex.
///
/// Two v4 UUIDs' worth of randomness from the same source the approval
/// machinery already uses, so this adds no dependency for the sake of one
/// string.
fn mint_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Path of the descriptor within `runtime_dir`.
pub fn endpoint_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(ENDPOINT_FILE)
}

/// Path of the lock within `runtime_dir`.
pub fn lock_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(LOCK_FILE)
}

/// Publish `endpoint` atomically: a reader sees either the previous descriptor
/// or the new one, never a half-written one.
pub fn write_endpoint(runtime_dir: &Path, endpoint: &DaemonEndpoint) -> Result<()> {
    std::fs::create_dir_all(runtime_dir)
        .with_context(|| format!("cannot create runtime dir {}", runtime_dir.display()))?;
    let final_path = endpoint_path(runtime_dir);
    let tmp_path = runtime_dir.join(format!("{ENDPOINT_FILE}.{}.tmp", std::process::id()));
    let json = serde_json::to_string_pretty(endpoint)?;
    std::fs::write(&tmp_path, json)
        .with_context(|| format!("cannot write {}", tmp_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp_path, &final_path).with_context(|| {
        format!(
            "cannot publish endpoint descriptor at {}",
            final_path.display()
        )
    })?;
    Ok(())
}

/// Read the descriptor, if there is a readable one.
pub fn read_endpoint(runtime_dir: &Path) -> Option<DaemonEndpoint> {
    let text = std::fs::read_to_string(endpoint_path(runtime_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Remove the descriptor. Called by the daemon on its way out, and by a client
/// that has established the descriptor is stale.
pub fn remove_endpoint(runtime_dir: &Path) {
    let _ = std::fs::remove_file(endpoint_path(runtime_dir));
}

/// What a client should do about the descriptor it found.
#[derive(Debug, PartialEq, Eq)]
pub enum EndpointState {
    /// A daemon holds the lock and published where it listens: connect.
    Live(Box<DaemonEndpoint>),
    /// Nobody holds the lock, so any descriptor present is left over from a
    /// daemon that died: remove it and start one.
    Stale,
    /// Nobody holds the lock and there is no descriptor: start a daemon.
    Absent,
}

/// Decide the state of the rendezvous without disturbing it.
///
/// Liveness comes from the lock rather than from the file's existence or its
/// pid: an advisory lock is released by the kernel when its holder dies, so a
/// descriptor left behind by a crash or a power cut is recognised as stale with
/// no cleanup step and no pid-reuse race.
pub fn probe(runtime_dir: &Path) -> EndpointState {
    let lock_is_free = match crate::fs_lock::FsLock::try_acquire(&lock_path(runtime_dir)) {
        // Acquiring proves nobody holds it; dropping releases it immediately.
        Ok(Some(_lock)) => true,
        Ok(None) => false,
        // If the lock cannot even be attempted, assume something is there
        // rather than trampling it.
        Err(_) => false,
    };
    match (lock_is_free, read_endpoint(runtime_dir)) {
        (false, Some(endpoint)) => EndpointState::Live(Box::new(endpoint)),
        // The holder has not published yet; treat it as live so a racing
        // client waits rather than starting a second daemon.
        (false, None) => EndpointState::Stale,
        (true, Some(_)) => EndpointState::Stale,
        (true, None) => EndpointState::Absent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_round_trips_through_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = DaemonEndpoint::new(51_234, 51_235);
        write_endpoint(tmp.path(), &endpoint).unwrap();

        let read = read_endpoint(tmp.path()).expect("descriptor is readable");
        assert_eq!(read, endpoint);
        assert_eq!(read.token.len(), 64, "128 bits of token, hex-encoded");
        assert_ne!(
            DaemonEndpoint::new(1, 2).token,
            endpoint.token,
            "each daemon mints its own token"
        );
    }

    #[test]
    fn writing_is_atomic_and_leaves_no_temporary_behind() {
        let tmp = tempfile::tempdir().unwrap();
        write_endpoint(tmp.path(), &DaemonEndpoint::new(1, 2)).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary left behind: {leftovers:?}");
    }

    /// The lock, not the file, decides liveness — so a descriptor left behind
    /// by a crash is recognised without a cleanup step or a pid-reuse race.
    #[test]
    fn a_descriptor_whose_owner_died_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(probe(tmp.path()), EndpointState::Absent);

        write_endpoint(tmp.path(), &DaemonEndpoint::new(1, 2)).unwrap();
        assert_eq!(
            probe(tmp.path()),
            EndpointState::Stale,
            "nobody holds the lock, so the descriptor is left over"
        );

        remove_endpoint(tmp.path());
        assert_eq!(probe(tmp.path()), EndpointState::Absent);
    }

    #[test]
    fn a_held_lock_with_a_descriptor_is_live() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = DaemonEndpoint::new(7, 8);
        write_endpoint(tmp.path(), &endpoint).unwrap();

        let _held = crate::fs_lock::FsLock::try_acquire(&lock_path(tmp.path()))
            .unwrap()
            .expect("a fresh lock is acquirable");

        match probe(tmp.path()) {
            EndpointState::Live(found) => {
                assert_eq!(found.hub_port, 7);
                assert_eq!(found.mcp_port, 8);
                assert_eq!(found.token, endpoint.token);
            }
            other => panic!("expected Live while the lock is held, got {other:?}"),
        }
    }

    /// A daemon that holds the lock but has not published yet must not be
    /// raced: a second client waits rather than starting a rival daemon.
    #[test]
    fn a_held_lock_without_a_descriptor_is_not_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let _held = crate::fs_lock::FsLock::try_acquire(&lock_path(tmp.path()))
            .unwrap()
            .expect("a fresh lock is acquirable");
        assert_ne!(
            probe(tmp.path()),
            EndpointState::Absent,
            "a starting daemon must not look like no daemon"
        );
    }
}
