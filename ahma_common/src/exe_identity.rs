//! Which ahma binary this is, and whether it has since been replaced
//! (SPEC R-HUB.5).
//!
//! A version string alone cannot answer either question. The git short hash
//! does not change on a dirty rebuild, so a developer's `cargo build` looked
//! like the hub already running; and nothing about a running process says
//! that the file it was started from has been overwritten since. The file's
//! size and modification time answer both: an install writes a new file, and
//! between two builds of one version the newer one is the one built later.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A binary's identity: what it says it is, and the file it was started from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExeIdentity {
    /// Semver, `CARGO_PKG_VERSION`.
    pub version: String,
    /// [`crate::BUILD_ID`]: the git short hash, or a timestamp fallback.
    pub build_id: String,
    /// The executable's path, as it was when the process started.
    pub path: PathBuf,
    /// The file's size in bytes.
    pub size: u64,
    /// The file's modification time, milliseconds since the Unix epoch.
    pub mtime_ms: u64,
}

/// `(size, mtime_ms)` of the file at `path`, if it can be read.
fn file_stamp(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some((meta.len(), u64::try_from(mtime_ms).ok()?))
}

impl ExeIdentity {
    /// The identity of the file at `path`, for a binary that reports
    /// `version` and `build_id`.
    pub fn at(path: &Path, version: &str, build_id: &str) -> Option<Self> {
        let (size, mtime_ms) = file_stamp(path)?;
        Some(Self {
            version: version.to_string(),
            build_id: build_id.to_string(),
            path: path.to_path_buf(),
            size,
            mtime_ms,
        })
    }

    /// This process's own identity, read once — the first time it is asked
    /// for, which a long-lived process must do at startup, before an install
    /// can replace the file under it.
    pub fn this_process() -> Option<&'static Self> {
        static IDENTITY: std::sync::OnceLock<Option<ExeIdentity>> = std::sync::OnceLock::new();
        IDENTITY
            .get_or_init(|| {
                let path = std::env::current_exe().ok()?;
                Self::at(&path, env!("CARGO_PKG_VERSION"), crate::BUILD_ID)
            })
            .as_ref()
    }

    /// Has the file at [`ExeIdentity::path`] been replaced since?
    ///
    /// `false` while the path is missing: an installer that removes the old
    /// file before writing the new one is mid-install, not finished, and the
    /// next look will see the new file.
    pub fn replaced_on_disk(&self) -> bool {
        file_stamp(&self.path).is_some_and(|now| now != (self.size, self.mtime_ms))
    }

    /// Is this build strictly newer than `other`?
    ///
    /// A newer semver is newer. Within one semver, a different build is newer
    /// only if its file is newer: that is what tells a fresh `cargo build`
    /// from an older install elsewhere on the `PATH`, and "different" alone
    /// made two installed copies take turns replacing each other's hub.
    /// An unreadable version is never newer, and is older than any readable
    /// one: it is not evidence that the build is current.
    pub fn is_strictly_newer_than(&self, other: &Self) -> bool {
        match (parse_semver(&self.version), parse_semver(&other.version)) {
            (Some(ours), Some(theirs)) if ours != theirs => ours > theirs,
            (Some(_), Some(_)) => self.build_id != other.build_id && self.mtime_ms > other.mtime_ms,
            // An unreadable version is not evidence that a build is current.
            (Some(_), None) => true,
            (None, _) => false,
        }
    }
}

/// `major.minor.patch`, ignoring any pre-release or build suffix.
fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(version: &str, build_id: &str, mtime_ms: u64) -> ExeIdentity {
        ExeIdentity {
            version: version.into(),
            build_id: build_id.into(),
            path: PathBuf::from("ahma"),
            size: 1,
            mtime_ms,
        }
    }

    #[test]
    fn a_newer_version_is_newer_whatever_the_files_say() {
        assert!(id("0.22.0", "a", 1).is_strictly_newer_than(&id("0.21.9", "b", 9)));
        assert!(!id("0.21.9", "b", 9).is_strictly_newer_than(&id("0.22.0", "a", 1)));
        assert!(id("0.21.10", "a", 1).is_strictly_newer_than(&id("0.21.9", "b", 1)));
    }

    /// Within one version, only the later-built file is newer. "Different"
    /// alone made two installed copies take turns replacing each other's hub.
    #[test]
    fn within_a_version_the_later_build_is_newer() {
        let installed = id("0.21.8", "abc", 1_000);
        let rebuilt = id("0.21.8", "def", 2_000);
        assert!(rebuilt.is_strictly_newer_than(&installed));
        assert!(!installed.is_strictly_newer_than(&rebuilt));
        assert!(
            !id("0.21.8", "def", 1_000).is_strictly_newer_than(&installed),
            "an equal time proves nothing either way"
        );
        assert!(
            !id("0.21.8", "abc", 2_000).is_strictly_newer_than(&installed),
            "the same build copied later is the same build"
        );
    }

    #[test]
    fn an_unreadable_version_is_never_newer() {
        assert!(!id("dev", "a", 9).is_strictly_newer_than(&id("0.21.8", "b", 1)));
        assert!(!id("dev", "a", 9).is_strictly_newer_than(&id("dev", "b", 1)));
        assert!(
            id("0.21.8", "a", 1).is_strictly_newer_than(&id("dev", "b", 9)),
            "and older than any readable one"
        );
    }

    /// An install writes a new file at the path; the running process's
    /// identity no longer matches it.
    #[test]
    fn an_install_over_the_path_is_seen_as_a_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ahma");
        std::fs::write(&path, b"old build").unwrap();
        let running = ExeIdentity::at(&path, "0.21.8", "abc").expect("readable");
        assert!(!running.replaced_on_disk());

        std::fs::write(&path, b"a new, longer build").unwrap();
        assert!(running.replaced_on_disk());
    }

    /// Mid-install the path can be briefly missing; that is not a finished
    /// replacement, and the next look sees the new file.
    #[test]
    fn a_missing_file_is_not_yet_a_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ahma");
        std::fs::write(&path, b"old build").unwrap();
        let running = ExeIdentity::at(&path, "0.21.8", "abc").expect("readable");
        std::fs::remove_file(&path).unwrap();
        assert!(!running.replaced_on_disk());
    }

    #[test]
    fn this_process_knows_its_own_version() {
        let me = ExeIdentity::this_process().expect("the test binary is readable");
        assert_eq!(me.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(me.build_id, crate::BUILD_ID);
        assert!(!me.replaced_on_disk());
    }

    #[test]
    fn it_round_trips_as_json() {
        let me = id("0.21.8", "abc", 42);
        let back: ExeIdentity = serde_json::from_str(&serde_json::to_string(&me).unwrap()).unwrap();
        assert_eq!(back, me);
    }
}
