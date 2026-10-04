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
/// **defaults to `true`** — see the `sandbox::seatbelt` module for the emitted
/// rules. (Named rather than linked: that module is `cfg(target_os = "macos")`,
/// so an intra-doc link to it resolves on macOS and breaks the docs build on
/// every other platform.)
///
/// The macOS keychain is encrypted at rest, so denying file access to it protects
/// only against offline theft of the encrypted database, not against secret
/// extraction (that goes through `securityd`, which is ACL-gated regardless of the
/// sandbox and reachable via the always-allowed `mach-lookup`). Blocking it mostly
/// just breaks `gh`, `git-credential-osxkeychain`, and similar tools, so the
/// default is to allow it; the paranoid opt back out via `allow_keychain = false`.
static KEYCHAIN_ACCESS_ALLOWED: RwLock<bool> = RwLock::new(false);

/// Container-daemon sockets at fixed, non-home locations.
///
/// Reaching a container daemon is a *total* sandbox escape and does not require
/// breaking anything: talk to `docker.sock`, ask for a `--privileged` container
/// with `/` bind-mounted, and the hub — a root process entirely outside the
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

/// `~/.ssh`: denied as a whole (SPEC R6.2.3), so a private key is unreadable
/// whatever it is called. Denying only `id_*` left `github_ed25519`,
/// `deploy_key` and every other name readable. ssh uses keys through the
/// agent, which never hands them over.
pub fn ssh_dir(home: &Path) -> PathBuf {
    home.join(".ssh")
}

/// Anchored regex for what ssh reads in `~/.ssh` that holds no secret: the
/// directory itself, `config`, `known_hosts*`, public keys and certificates
/// (`*.pub`) and `allowed_signers`. Emitted after the deny on [`ssh_dir`]
/// (SBPL is last-match-wins).
pub fn ssh_client_readable_regex(home: &Path) -> String {
    format!(
        "^{}/\\.ssh(/(config|known_hosts[^/]*|[^/]*\\.pub|allowed_signers))?$",
        regex_escape(&home.to_string_lossy())
    )
}

/// Whether a file directly in `~/.ssh` named `name` is one ssh reads that
/// holds no secret — the same set [`ssh_client_readable_regex`] lets through.
fn is_ssh_client_file(name: &str) -> bool {
    name == "config"
        || name == "allowed_signers"
        || name.starts_with("known_hosts")
        || name.ends_with(".pub")
}

/// Directories under `~/.ssh` that hold no secret: `config.d` (included
/// configuration) and `agent` (where OpenSSH keeps agent sockets).
pub fn ssh_client_readable_dirs(home: &Path) -> Vec<PathBuf> {
    vec![ssh_dir(home).join("config.d"), ssh_dir(home).join("agent")]
}

/// The paths git and ssh read for their own configuration that exist under
/// `home`: the ssh client files [`ssh_client_readable_regex`] lets through,
/// `~/.ssh/config.d`, `~/.gitconfig` and `~/.config/git`. Linux grants these
/// one by one, read-only, since its sandbox reads nothing in home by default:
/// git ran with no identity and no credential helper, and ssh without
/// `known_hosts`.
pub fn client_config_read_paths(home: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(ssh_dir(home))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_ssh_client_file)
        })
        .collect();
    out.extend(
        [
            ssh_dir(home).join("config.d"),
            home.join(".gitconfig"),
            home.join(".config").join("git"),
        ]
        .into_iter()
        .filter(|p| p.exists()),
    );
    out
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
    ahma_common::scope_grant::CREDENTIAL_READ_DENIED
        .iter()
        .map(|rel| rel.split('/').fold(home.to_path_buf(), |p, c| p.join(c)))
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

/// Whether the sandbox refuses a *read* of `path`: inside the installed
/// credential set, or an SSH private key. Everything else is readable on
/// macOS, which is what lets a refusal there be told apart: "Operation not
/// permitted" on a path reads are refused for was a read; anywhere else it
/// was a write.
pub fn is_read_denied(path: &Path) -> bool {
    if credential_read_denies()
        .iter()
        .any(|deny| path.starts_with(deny))
    {
        return true;
    }
    let Some(home) = ahma_common::config::ahma_home_dir() else {
        return false;
    };
    path.parent() == Some(home.join(".ssh").as_path())
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with("id_") && !name.ends_with(".pub"))
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

    /// `~/.ssh` is denied as a whole and the files ssh needs that hold no
    /// secret are let back in. Denying only `id_*` left every other key name
    /// (`github_ed25519`, `deploy_key`) readable.
    #[test]
    fn ssh_is_denied_but_its_client_files_are_readable() {
        let home = Path::new("/home/u");
        assert_eq!(ssh_dir(home), Path::new("/home/u/.ssh"));
        let allow = regex::Regex::new(&ssh_client_readable_regex(home)).unwrap();
        for ok in [
            "/home/u/.ssh",
            "/home/u/.ssh/config",
            "/home/u/.ssh/known_hosts",
            "/home/u/.ssh/known_hosts.old",
            "/home/u/.ssh/known_hosts2",
            "/home/u/.ssh/id_ed25519.pub",
            "/home/u/.ssh/github-cert.pub",
            "/home/u/.ssh/allowed_signers",
        ] {
            assert!(allow.is_match(ok), "{ok}");
        }
        for secret in [
            "/home/u/.ssh/id_ed25519",
            "/home/u/.ssh/github_ed25519",
            "/home/u/.ssh/deploy_key",
            "/home/u/.ssh/config.bak/id_rsa",
            "/home/u/.ssh/sub/known_hosts",
            "/home/uX/.ssh/config",
        ] {
            assert!(!allow.is_match(secret), "{secret}");
        }
        assert_eq!(
            ssh_client_readable_dirs(home),
            vec![
                PathBuf::from("/home/u/.ssh/config.d"),
                PathBuf::from("/home/u/.ssh/agent")
            ]
        );
    }

    /// The paths a sandboxed git and ssh read for their own configuration,
    /// listed from what exists: Linux grants them one by one, since its
    /// sandbox reads nothing in home by default (git had no identity and no
    /// credential helper there).
    #[test]
    fn client_config_paths_are_the_ones_that_exist() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join(".ssh/config.d")).unwrap();
        std::fs::create_dir_all(h.join(".config/git")).unwrap();
        for f in [
            ".ssh/config",
            ".ssh/known_hosts",
            ".ssh/id_ed25519",
            ".ssh/id_ed25519.pub",
            ".ssh/github_ed25519",
            ".gitconfig",
            ".git-credentials",
        ] {
            std::fs::write(h.join(f), "x").unwrap();
        }
        let mut got = client_config_read_paths(h);
        got.sort();
        let mut want: Vec<PathBuf> = [
            ".ssh/config",
            ".ssh/known_hosts",
            ".ssh/id_ed25519.pub",
            ".ssh/config.d",
            ".gitconfig",
            ".config/git",
        ]
        .iter()
        .map(|f| f.split('/').fold(h.to_path_buf(), |p, c| p.join(c)))
        .collect();
        want.sort();
        assert_eq!(got, want);
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
