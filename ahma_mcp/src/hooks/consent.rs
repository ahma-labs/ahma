//! File-backed hook fall-open consent store (SPEC R5.5.3).
//!
//! Terminal hooks run as a fresh `ahma hooks exec` process per command, so the
//! one-time consent decision cannot live in process memory — it must be shared
//! across hook invocations within a session yet never persist across a restart.
//!
//! This store keeps a single marker file under the system temp directory whose
//! validity is bound to a **session nonce**. When the nonce changes (system
//! reboot, or an explicit `ahma hooks revoke`), any prior marker no longer
//! matches and consent is gone — satisfying "scoped to the session, never
//! persisted" (R5.5.3). The pure counting/banner semantics are delegated to
//! [`ahma_common::hook_consent::HookConsentLedger`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Marker {
    /// The session nonce this consent belongs to. A mismatch ⇒ stale ⇒ ignored.
    session: String,
    /// How many commands have run unsandboxed under this consent.
    count: u64,
}

/// A consent store bound to a session nonce and a marker directory.
pub struct HookConsentStore {
    dir: PathBuf,
    session: String,
}

impl HookConsentStore {
    /// The store for the current session, using the system temp directory.
    pub fn current() -> Self {
        Self::with(std::env::temp_dir(), session_nonce())
    }

    /// Construct a store with an explicit marker directory and session nonce
    /// (used by tests for hermetic isolation).
    pub fn with(dir: PathBuf, session: String) -> Self {
        Self { dir, session }
    }

    fn marker_path(&self) -> PathBuf {
        self.dir.join("ahma-hook-consent.json")
    }

    fn read_marker(&self) -> Option<Marker> {
        let raw = std::fs::read_to_string(self.marker_path()).ok()?;
        let marker: Marker = serde_json::from_str(&raw).ok()?;
        // A marker from a different session (e.g. before a reboot) is stale.
        (marker.session == self.session).then_some(marker)
    }

    fn write_marker(&self, marker: &Marker) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let raw = serde_json::to_string(marker).expect("marker serializes");
        std::fs::write(self.marker_path(), raw)
    }

    /// Whether unsandboxed hook execution is currently consented for this session.
    pub fn is_consented(&self) -> bool {
        self.read_marker().is_some()
    }

    /// Record explicit user consent (R5.5.3). Idempotent; preserves the count.
    pub fn grant(&self) -> std::io::Result<()> {
        let count = self.read_marker().map(|m| m.count).unwrap_or(0);
        self.write_marker(&Marker {
            session: self.session.clone(),
            count,
        })
    }

    /// Revoke consent (delete the marker). After this, fall-open fails closed
    /// again until re-granted.
    pub fn revoke(&self) -> std::io::Result<()> {
        match std::fs::remove_file(self.marker_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Count one unsandboxed run and return the new total. No-op (returns 0) when
    /// not consented.
    pub fn record_unsandboxed_run(&self) -> std::io::Result<u64> {
        let Some(mut marker) = self.read_marker() else {
            return Ok(0);
        };
        marker.count += 1;
        let count = marker.count;
        self.write_marker(&marker)?;
        Ok(count)
    }

    /// Current unsandboxed-run count for the session.
    pub fn count(&self) -> u64 {
        self.read_marker().map(|m| m.count).unwrap_or(0)
    }

    /// The persistent banner to display while consent is active (R5.5.3).
    pub fn banner(&self) -> Option<String> {
        use ahma_common::hook_consent::HookConsentLedger;
        if !self.is_consented() {
            return None;
        }
        // Reuse the canonical banner text from the pure ledger.
        let mut ledger = HookConsentLedger::new(0);
        ledger.grant_consent();
        for _ in 0..self.count() {
            ledger.evaluate(false);
        }
        ledger.banner()
    }
}

/// A best-effort session nonce that changes on reboot. Hook consent is bound to
/// it so a restart of the machine clears consent (R5.5.3). On platforms where a
/// boot identifier is unavailable, falls back to a stable per-user string (the
/// weaker guarantee is documented; `ahma hooks revoke` always clears consent).
fn session_nonce() -> String {
    // Linux: the kernel boot id changes on every boot.
    #[cfg(target_os = "linux")]
    if let Ok(id) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        return format!("boot:{}", id.trim());
    }
    // macOS: kern.boottime is stable within a boot, changes across reboots.
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("sysctl")
        .args(["-n", "kern.boottime"])
        .output()
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout);
        return format!("boot:{}", s.trim());
    }
    // Fallback: user-stable. Cleared only by reboot of the marker dir's tmpfs or
    // by `ahma hooks revoke`.
    "session:fallback".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn not_consented_by_default() {
        let dir = tempdir().unwrap();
        let store = HookConsentStore::with(dir.path().to_path_buf(), "s1".into());
        assert!(!store.is_consented());
        assert!(store.banner().is_none());
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn grant_then_consented_and_counts() {
        let dir = tempdir().unwrap();
        let store = HookConsentStore::with(dir.path().to_path_buf(), "s1".into());
        store.grant().unwrap();
        assert!(store.is_consented());
        assert_eq!(store.record_unsandboxed_run().unwrap(), 1);
        assert_eq!(store.record_unsandboxed_run().unwrap(), 2);
        assert_eq!(store.count(), 2);
        let banner = store.banner().unwrap();
        assert!(banner.contains("UNSANDBOXED") && banner.contains('2'));
    }

    #[test]
    fn marker_from_other_session_is_ignored() {
        let dir = tempdir().unwrap();
        // Session 1 grants and runs.
        let s1 = HookConsentStore::with(dir.path().to_path_buf(), "s1".into());
        s1.grant().unwrap();
        s1.record_unsandboxed_run().unwrap();
        // Session 2 (e.g. after reboot) shares the dir but a different nonce:
        // the prior marker is stale, so consent does NOT carry over (R5.5.3).
        let s2 = HookConsentStore::with(dir.path().to_path_buf(), "s2".into());
        assert!(!s2.is_consented());
        assert_eq!(s2.count(), 0);
    }

    #[test]
    fn revoke_clears_consent() {
        let dir = tempdir().unwrap();
        let store = HookConsentStore::with(dir.path().to_path_buf(), "s1".into());
        store.grant().unwrap();
        assert!(store.is_consented());
        store.revoke().unwrap();
        assert!(!store.is_consented());
        // revoke is idempotent
        store.revoke().unwrap();
    }

    #[test]
    fn record_run_without_consent_is_noop() {
        let dir = tempdir().unwrap();
        let store = HookConsentStore::with(dir.path().to_path_buf(), "s1".into());
        assert_eq!(store.record_unsandboxed_run().unwrap(), 0);
        assert!(!store.is_consented());
    }
}
