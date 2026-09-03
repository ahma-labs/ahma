//! macOS credential-read deny list.
//!
//! On macOS the Seatbelt profile grants sandboxed commands global file-*read*
//! access (an APFS firmlink / cryptex workaround — see `super::seatbelt`).
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

use parking_lot::RwLock;
use std::path::{Path, PathBuf};

static CREDENTIAL_READ_DENIES: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());

/// Whether sandboxed tools may read/write the macOS login keychain
/// (`~/Library/Keychains`) and the `com.apple.security*` preference plists.
///
/// Empty (`false`) until installed at startup via [`set_keychain_access_allowed`],
/// matching the credential-deny-list pattern: in-process embedders and Test-mode
/// sandboxes (which never emit a Seatbelt profile) are unaffected. On real
/// startup it is set to the resolved `[sandbox] allow_keychain` value, which
/// **defaults to `true`** — see [`super::seatbelt`] for the emitted rules.
///
/// The macOS keychain is encrypted at rest, so denying file access to it protects
/// only against offline theft of the encrypted database, not against secret
/// extraction (that goes through `securityd`, which is ACL-gated regardless of the
/// sandbox and reachable via the always-allowed `mach-lookup`). Blocking it mostly
/// just breaks `gh`, `git-credential-osxkeychain`, and similar tools, so the
/// default is to allow it; the paranoid opt back out via `allow_keychain = false`.
static KEYCHAIN_ACCESS_ALLOWED: RwLock<bool> = RwLock::new(false);

/// Home-relative credential directories denied by default. Chosen so no common
/// build / test / VCS tool breaks: `~/.ssh` and `~/.config/gh` are intentionally
/// **absent** (git-over-ssh and `gh` need them) and can be added via
/// `[sandbox] deny_credential_reads`. `~/Library/Keychains` is likewise absent —
/// it is governed by the dedicated `[sandbox] allow_keychain` toggle (default on;
/// see `KEYCHAIN_ACCESS_ALLOWED`) which re-adds it to the deny set when disabled.
const DEFAULT_DENY_RELATIVE: [&str; 7] = [
    ".ahma", // ahma's own bearer token, TLS keys, and scope-grant store
    ".aws",
    ".gnupg",
    ".config/gcloud",
    ".kube",
    ".docker",
    ".netrc",
];

/// Container-daemon sockets at fixed, non-home locations.
///
/// Reaching a container daemon is a *total* sandbox escape and does not require
/// breaking anything: talk to `docker.sock`, ask for a `--privileged` container
/// with `/` bind-mounted, and the daemon — a root process entirely outside the
/// sandbox — performs the write on your behalf. Nothing in a filesystem policy
/// scoped to the workspace can see that write happen.
///
/// These are denied for **read and write**: a unix-socket client needs to open
/// the socket node, so removing file access removes the capability.
const CONTAINER_SOCKET_ABSOLUTE: [&str; 4] = [
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/var/run/containerd/containerd.sock",
    "/run/podman/podman.sock",
];

/// Home-relative container sockets with a fixed path.
const CONTAINER_SOCKET_HOME_RELATIVE: [&str; 1] = [".docker/run/docker.sock"];

/// Home-relative container sockets whose middle path component varies (the VM
/// instance name), expressed as regex bodies rather than subpaths so a blanket
/// deny on `~/.colima` / `~/.lima` does not take the whole CLI down with it.
const CONTAINER_SOCKET_HOME_REGEX: [&str; 2] = [r"/\.colima/.*/docker\.sock$", r"/\.lima/.*/sock$"];

/// Every fixed-path container socket, resolved against `home`.
///
/// The order is irrelevant (they are all denies) but is kept stable so profile
/// text is deterministic and testable.
pub fn container_socket_denies(home: &Path) -> Vec<PathBuf> {
    CONTAINER_SOCKET_ABSOLUTE
        .iter()
        .map(PathBuf::from)
        .chain(CONTAINER_SOCKET_HOME_RELATIVE.iter().map(|r| home.join(r)))
        .collect()
}

/// Anchored regex bodies for the variable-path container sockets under `home`.
/// Each is a complete pattern ready to drop into an SBPL `(regex #"…")`.
pub fn container_socket_deny_regexes(home: &Path) -> Vec<String> {
    let home_re = regex_escape(&home.to_string_lossy());
    CONTAINER_SOCKET_HOME_REGEX
        .iter()
        .map(|tail| format!("^{home_re}{tail}"))
        .collect()
}

/// Anchored regex matching SSH **private** key material in `~/.ssh` (`id_rsa`,
/// `id_ed25519`, `id_ecdsa_sk`, …).
///
/// `~/.ssh` as a whole is deliberately absent from `DEFAULT_DENY_RELATIVE` so
/// that git-over-ssh keeps working, but that left the private keys themselves
/// readable under the blanket macOS `(allow file-read*)`. Denying only the
/// `id_*` files keeps `known_hosts` and `config` readable — which is what the
/// ssh client actually needs from the sandbox — while the key bytes stay out of
/// reach. The agent can still *use* ssh: `ssh-agent` and the `ssh` binary that
/// reads the key run outside this profile's read policy for their own purposes.
pub fn ssh_private_key_deny_regex(home: &Path) -> String {
    format!(
        "^{}/\\.ssh/id_[^/]*$",
        regex_escape(&home.to_string_lossy())
    )
}

/// Anchored regex re-allowing SSH **public** keys, which are not secret and are
/// routinely read (e.g. to print a fingerprint). Must be emitted *after*
/// [`ssh_private_key_deny_regex`] — SBPL is last-match-wins.
pub fn ssh_public_key_allow_regex(home: &Path) -> String {
    format!(
        "^{}/\\.ssh/id_[^/]*\\.pub$",
        regex_escape(&home.to_string_lossy())
    )
}

/// Escape regex metacharacters so a filesystem path can be embedded in an SBPL
/// `(regex #"…")` literal. Home directories contain `.` routinely and may
/// contain `+`, `(`, or `)`; an unescaped `.` would turn the anchor into a
/// wildcard and widen the rule.
pub fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        if matches!(
            ch,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

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
    let mut guard = CREDENTIAL_READ_DENIES.write();
    *guard = paths;
}

/// The currently-installed deny set (empty if none installed).
pub fn credential_read_denies() -> Vec<PathBuf> {
    CREDENTIAL_READ_DENIES.read().clone()
}

/// Install whether sandboxed tools may access the macOS keychain (called once at
/// startup with the resolved `[sandbox] allow_keychain` value). See
/// `KEYCHAIN_ACCESS_ALLOWED`.
pub fn set_keychain_access_allowed(allowed: bool) {
    let mut guard = KEYCHAIN_ACCESS_ALLOWED.write();
    *guard = allowed;
}

/// Whether sandboxed tools may access the macOS keychain (`false` until installed
/// at startup). Read by the Seatbelt profile builder to decide whether to emit the
/// keychain write / security-prefs allow rules.
pub fn keychain_access_allowed() -> bool {
    *KEYCHAIN_ACCESS_ALLOWED.read()
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
        // git-over-ssh / gh must keep working by default.
        assert!(!set.contains(&home.join(".ssh")));
        assert!(!set.contains(&home.join(".config/gh")));
        // The keychain is governed by the `allow_keychain` toggle (default on),
        // not the built-in deny set, so `gh`/keychain tools work by default.
        assert!(!set.contains(&home.join("Library/Keychains")));
    }

    #[test]
    fn keychain_access_flag_set_and_get_roundtrip() {
        set_keychain_access_allowed(true);
        assert!(keychain_access_allowed());
        set_keychain_access_allowed(false);
        assert!(!keychain_access_allowed());
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
    fn container_socket_denies_cover_docker_podman_and_containerd() {
        let home = Path::new("/home/u");
        let set = container_socket_denies(home);
        assert!(set.contains(&PathBuf::from("/var/run/docker.sock")));
        assert!(set.contains(&PathBuf::from("/run/docker.sock")));
        assert!(set.contains(&PathBuf::from("/var/run/containerd/containerd.sock")));
        assert!(set.contains(&PathBuf::from("/run/podman/podman.sock")));
        assert!(set.contains(&home.join(".docker/run/docker.sock")));
    }

    #[test]
    fn container_socket_regexes_are_anchored_at_the_escaped_home() {
        let home = Path::new("/home/u.name");
        let res = container_socket_deny_regexes(home);
        assert!(
            res.iter().all(|r| r.starts_with("^/home/u\\.name/")),
            "regexes must be anchored at an escaped home: {res:?}"
        );
        assert!(
            res.iter()
                .any(|r| r.ends_with(r"/\.colima/.*/docker\.sock$")),
            "{res:?}"
        );
        assert!(
            res.iter().any(|r| r.ends_with(r"/\.lima/.*/sock$")),
            "{res:?}"
        );
    }

    #[test]
    fn ssh_regexes_deny_private_keys_and_re_allow_public_ones() {
        let home = Path::new("/home/u");
        let deny = ssh_private_key_deny_regex(home);
        let allow = ssh_public_key_allow_regex(home);
        assert_eq!(deny, r"^/home/u/\.ssh/id_[^/]*$");
        assert_eq!(allow, r"^/home/u/\.ssh/id_[^/]*\.pub$");

        // Sanity-check the intent with a real regex engine: `known_hosts` and
        // `config` must not match the deny, and the allow must cover `.pub`.
        let deny_re = regex::Regex::new(&deny).expect("deny regex compiles");
        let allow_re = regex::Regex::new(&allow).expect("allow regex compiles");
        assert!(deny_re.is_match("/home/u/.ssh/id_ed25519"));
        assert!(deny_re.is_match("/home/u/.ssh/id_rsa"));
        assert!(deny_re.is_match("/home/u/.ssh/id_ed25519.pub"));
        assert!(allow_re.is_match("/home/u/.ssh/id_ed25519.pub"));
        assert!(!deny_re.is_match("/home/u/.ssh/known_hosts"));
        assert!(!deny_re.is_match("/home/u/.ssh/config"));
        // The anchor must not let a look-alike home match.
        assert!(!deny_re.is_match("/home/uX/.ssh/id_rsa"));
    }

    #[test]
    fn regex_escape_escapes_metacharacters() {
        assert_eq!(regex_escape("/home/u.name"), r"/home/u\.name");
        assert_eq!(regex_escape("a+b(c)"), r"a\+b\(c\)");
        assert_eq!(regex_escape("plain"), "plain");
    }

    #[test]
    fn set_and_get_roundtrip() {
        set_credential_read_denies(vec![PathBuf::from("/x/.aws")]);
        assert_eq!(credential_read_denies(), vec![PathBuf::from("/x/.aws")]);
        set_credential_read_denies(Vec::new());
    }
}
