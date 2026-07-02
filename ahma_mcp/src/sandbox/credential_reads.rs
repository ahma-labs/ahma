//! macOS credential-read deny list.
//!
//! On macOS the Seatbelt profile grants sandboxed commands global file-*read*
//! access (an APFS firmlink / cryptex workaround — see [`super::seatbelt`]).
//! Write access stays scoped, but *reads* are wide open, so a prompt-injected or
//! malicious tool could `cat` the user's credentials and exfiltrate them.
//!
//! This module holds the effective set of credential directories whose reads are
//! denied. Seatbelt emits a `(deny file-read* (subpath …))` for each, placed
//! after the global allow but before the workspace-scope allows, so an explicit
//! scope grant still wins (last-match-wins in SBPL) while credentials are denied
//! by default.
//!
//! The set is process-global (a single operator policy) and installed once at
//! startup via [`set_credential_read_denies`]. It is empty until then, so
//! in-process embedders and Test-mode sandboxes (which never emit a Seatbelt
//! profile) are unaffected. On Linux/Windows the list is unused because reads
//! are already scoped to the sandbox.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

static CREDENTIAL_READ_DENIES: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());

/// Home-relative credential directories denied by default. Chosen so no common
/// build / test / VCS tool breaks: `~/.ssh` and `~/.config/gh` are intentionally
/// **absent** (git-over-ssh and `gh` need them) and can be added via
/// `[sandbox] deny_credential_reads`.
const DEFAULT_DENY_RELATIVE: [&str; 8] = [
    ".ahma", // ahma's own bearer token, TLS keys, and scope-grant store
    ".aws",
    ".gnupg",
    ".config/gcloud",
    ".kube",
    ".docker",
    ".netrc",
    "Library/Keychains",
];

/// Expand a leading `~` / `~/…` against `home`; otherwise return the path as-is.
fn expand_tilde(path: &Path, home: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        return home.join(rest);
    }
    path.to_path_buf()
}

/// The built-in default deny set, resolved against `home`.
pub fn default_credential_read_denies(home: &Path) -> Vec<PathBuf> {
    DEFAULT_DENY_RELATIVE
        .iter()
        .map(|rel| home.join(rel))
        .collect()
}

/// Compute the effective deny set: the built-in default plus `extra_deny`, minus
/// `allow` (all `~`-expanded). A path in both `extra_deny` and `allow` ends up
/// allowed. Order-preserving and de-duplicated.
pub fn effective_credential_read_denies(
    home: &Path,
    extra_deny: &[PathBuf],
    allow: &[PathBuf],
) -> Vec<PathBuf> {
    let allow_set: Vec<PathBuf> = allow.iter().map(|p| expand_tilde(p, home)).collect();
    let mut out: Vec<PathBuf> = Vec::new();
    let candidates = default_credential_read_denies(home)
        .into_iter()
        .chain(extra_deny.iter().map(|p| expand_tilde(p, home)));
    for p in candidates {
        if !allow_set.contains(&p) && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Install the effective credential-read deny set (called once at startup).
pub fn set_credential_read_denies(paths: Vec<PathBuf>) {
    if let Ok(mut guard) = CREDENTIAL_READ_DENIES.write() {
        *guard = paths;
    }
}

/// The currently-installed deny set (empty if none installed).
pub fn credential_read_denies() -> Vec<PathBuf> {
    CREDENTIAL_READ_DENIES
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_set_covers_ahma_and_cloud_creds_but_not_ssh() {
        let home = Path::new("/home/u");
        let set = default_credential_read_denies(home);
        assert!(set.contains(&home.join(".ahma")));
        assert!(set.contains(&home.join(".aws")));
        assert!(set.contains(&home.join("Library/Keychains")));
        // git-over-ssh / gh must keep working by default.
        assert!(!set.contains(&home.join(".ssh")));
        assert!(!set.contains(&home.join(".config/gh")));
    }

    #[test]
    fn extra_deny_is_added_and_tilde_expanded() {
        let home = Path::new("/home/u");
        let eff = effective_credential_read_denies(home, &[PathBuf::from("~/.ssh")], &[]);
        assert!(
            eff.contains(&home.join(".ssh")),
            "extra deny ~/.ssh must be added"
        );
    }

    #[test]
    fn allow_removes_default_deny() {
        let home = Path::new("/home/u");
        let eff = effective_credential_read_denies(home, &[], &[PathBuf::from("~/.aws")]);
        assert!(
            !eff.contains(&home.join(".aws")),
            "~/.aws must be re-allowed"
        );
        assert!(eff.contains(&home.join(".ahma")), "other defaults remain");
    }

    #[test]
    fn allow_wins_over_extra_deny() {
        let home = Path::new("/home/u");
        let eff = effective_credential_read_denies(
            home,
            &[PathBuf::from("~/.ssh")],
            &[PathBuf::from("~/.ssh")],
        );
        assert!(
            !eff.contains(&home.join(".ssh")),
            "allow must override extra deny"
        );
    }

    #[test]
    fn set_and_get_roundtrip() {
        set_credential_read_denies(vec![PathBuf::from("/x/.aws")]);
        assert_eq!(credential_read_denies(), vec![PathBuf::from("/x/.aws")]);
        set_credential_read_denies(Vec::new());
    }
}
