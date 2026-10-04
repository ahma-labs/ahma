//! Which SSH agent a sandboxed command may reach (SPEC R-CRED.11).
//!
//! `[sandbox] ssh_agent = "broker"` (default) gives each command ahma's broker
//! as its `SSH_AUTH_SOCK` (R-CRED.1). The broker is only a control if the
//! command cannot go around it: the human's own agent signs for anyone who
//! connects, and its socket path is no secret (`launchctl getenv`, a
//! predictable `/private/var/run/com.apple.launchd.*` name). So on macOS the
//! profile also refuses a `connect(2)` to that agent — the adopted upstream
//! by its canonical path, and launchd's per-session listener by pattern —
//! and lets the broker's own sockets back in after those denies.
//!
//! `"passthrough"` is the pre-broker behaviour: no broker, no deny, the
//! human's `SSH_AUTH_SOCK` passed through (for an agent that confirms each
//! use itself, `ssh-add -c`). `"off"` gives a command no agent: no broker,
//! `SSH_AUTH_SOCK` removed from every spawn, and the same denies.
//!
//! Process-global like the signal and GPU toggles: set once at startup from
//! the resolved settings, read by every profile generation and every spawn.

pub use ahma_common::config::SshAgentMode;
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

static MODE: AtomicU8 = AtomicU8::new(0);

/// The human's agent as adopted: the path brokers forward to, and its
/// canonical spelling, which is the only one a Seatbelt rule matches.
#[derive(Debug, Clone)]
struct Upstream {
    socket: PathBuf,
    canonical: PathBuf,
}

static UPSTREAM: RwLock<Option<Upstream>> = RwLock::new(None);

/// launchd's per-session SSH agent listener, whatever a command's environment
/// says (older macOS put it under `/private/tmp`). Canonical spellings only:
/// a `/var/…` or `/tmp/…` pattern never fires.
pub const LAUNCHD_AGENT_REGEX: &str =
    r"^/private/(var/run|tmp)/com\.apple\.launchd\.[^/]+/Listeners$";

/// Install the resolved `[sandbox] ssh_agent`.
pub fn set_ssh_agent_mode(mode: SshAgentMode) {
    let v = match mode {
        SshAgentMode::Broker => 0,
        SshAgentMode::Passthrough => 1,
        SshAgentMode::Off => 2,
    };
    MODE.store(v, Ordering::Relaxed);
}

/// The installed `[sandbox] ssh_agent`.
pub fn ssh_agent_mode() -> SshAgentMode {
    match MODE.load(Ordering::Relaxed) {
        1 => SshAgentMode::Passthrough,
        2 => SshAgentMode::Off,
        _ => SshAgentMode::Broker,
    }
}

/// Whether every spawned command loses `SSH_AUTH_SOCK` (`"off"`).
pub fn strips_agent_env() -> bool {
    ssh_agent_mode() == SshAgentMode::Off
}

/// Record `socket` as the human's agent: brokers forward to it, and the
/// profile refuses a sandboxed command a direct connect to it. `None` clears
/// it. A path a profile cannot hold safely — one with `"`, `\`, a control
/// character or bytes that are not UTF-8 — is refused, and nothing is
/// recorded: written into a rule it would end the string and let the rest of
/// the path speak SBPL.
pub fn set_upstream_agent(socket: Option<&Path>) -> Result<(), String> {
    let adopted = socket
        .map(|socket| {
            let canonical = canonical_socket_path(socket);
            if !profile_safe(&canonical) {
                return Err(format!(
                    "SSH agent socket {} is not used: its path cannot be written into a \
                     sandbox profile safely (it contains a quote, a backslash, a control \
                     character or non-UTF-8 bytes). Sandboxed commands sign only with key \
                     files the broker reads (SPEC R-CRED.11).",
                    canonical.display()
                ));
            }
            Ok(Upstream {
                socket: socket.to_path_buf(),
                canonical,
            })
        })
        .transpose();
    let mut guard = UPSTREAM.write();
    match adopted {
        Ok(upstream) => {
            *guard = upstream;
            Ok(())
        }
        Err(why) => {
            *guard = None;
            Err(why)
        }
    }
}

/// The human's agent socket brokers forward to, if one was adopted.
pub fn upstream_agent() -> Option<PathBuf> {
    UPSTREAM.read().as_ref().map(|u| u.socket.clone())
}

/// The kernel matches a connect by the path it resolves to, so a rule must
/// name the canonical path. Through the parent when the socket itself does
/// not resolve (an agent restarted between discovery and canonicalization),
/// as the container-socket rules do.
fn canonical_socket_path(socket: &Path) -> PathBuf {
    dunce::canonicalize(socket)
        .ok()
        .or_else(|| {
            let parent = dunce::canonicalize(socket.parent()?).ok()?;
            Some(parent.join(socket.file_name()?))
        })
        .unwrap_or_else(|| socket.to_path_buf())
}

/// Whether `path` can sit inside an SBPL string literal as itself.
fn profile_safe(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|s| !s.chars().any(|c| c == '"' || c == '\\' || c.is_control()))
}

/// The connect denies for the human's agent (SPEC R-CRED.11): none under
/// `"passthrough"`; otherwise the adopted upstream by canonical path and
/// launchd's listener by pattern. Emitted after the network rules, since SBPL
/// is last-match-wins; [`seatbelt_broker_connect_allow`] comes after these.
pub fn seatbelt_agent_connect_denies() -> String {
    if ssh_agent_mode() == SshAgentMode::Passthrough {
        return String::new();
    }
    let mut rules = String::new();
    if let Some(upstream) = UPSTREAM.read().as_ref() {
        rules.push_str(&format!(
            "(deny network-outbound (remote unix-socket (path-literal \"{}\")))\n",
            upstream.canonical.display()
        ));
    }
    rules.push_str(&format!(
        "(deny network-outbound (remote unix-socket (path-regex #\"{LAUNCHD_AGENT_REGEX}\")))\n"
    ));
    rules
}

/// The broker's sockets, let back in after the agent denies: only in
/// `"broker"` mode, the only one with a broker to reach.
pub fn seatbelt_broker_connect_allow(agent_dir: &Path) -> String {
    if ssh_agent_mode() != SshAgentMode::Broker {
        return String::new();
    }
    format!(
        "(allow network-outbound (remote unix-socket (subpath \"{}\")))\n",
        agent_dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_round_trips_through_the_process_global() {
        for mode in [
            SshAgentMode::Passthrough,
            SshAgentMode::Off,
            SshAgentMode::Broker,
        ] {
            set_ssh_agent_mode(mode);
            assert_eq!(ssh_agent_mode(), mode);
            assert_eq!(strips_agent_env(), mode == SshAgentMode::Off);
        }
    }

    /// A path that would end the SBPL string is never adopted, and adopting
    /// it clears whatever was adopted before (SPEC R-CRED.11).
    #[cfg(unix)]
    #[test]
    fn an_upstream_that_could_inject_into_a_profile_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("agent.sock");
        set_upstream_agent(Some(&good)).expect("an ordinary path is adopted");
        assert_eq!(upstream_agent(), Some(good.clone()));

        for bad in [
            "evil\")))(allow default)(\"x.sock",
            "back\\slash.sock",
            "new\nline.sock",
        ] {
            let why = set_upstream_agent(Some(&dir.path().join(bad)))
                .expect_err("an unsafe path must be refused");
            assert!(
                why.contains("cannot be written into a sandbox profile"),
                "{why}"
            );
            assert_eq!(upstream_agent(), None, "{bad:?}: nothing stays adopted");
            set_ssh_agent_mode(SshAgentMode::Broker);
            assert!(
                !seatbelt_agent_connect_denies().contains("allow default"),
                "{bad:?}"
            );
        }
        set_upstream_agent(None).unwrap();
    }

    /// The rule names the canonical path: the kernel never matches a
    /// `/var/…` spelling of a `/private/var/…` socket.
    #[cfg(unix)]
    #[test]
    fn the_upstream_deny_names_the_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_dir = dunce::canonicalize(dir.path()).unwrap();
        let link = dir.path().join("link");
        std::fs::create_dir(canonical_dir.join("real")).unwrap();
        std::os::unix::fs::symlink(canonical_dir.join("real"), &link).unwrap();
        let spelled = link.join("agent.sock");

        set_ssh_agent_mode(SshAgentMode::Broker);
        set_upstream_agent(Some(&spelled)).unwrap();
        let rules = seatbelt_agent_connect_denies();
        let want = canonical_dir.join("real").join("agent.sock");
        assert!(
            rules.contains(&format!(
                "(deny network-outbound (remote unix-socket (path-literal \"{}\")))",
                want.display()
            )),
            "{rules}"
        );
        assert_eq!(
            upstream_agent(),
            Some(spelled),
            "brokers keep the path given"
        );
        set_upstream_agent(None).unwrap();
    }

    /// `"passthrough"` emits nothing; `"off"` denies but has no broker to let
    /// back in; `"broker"` does both.
    #[test]
    fn each_mode_emits_its_own_rules() {
        let agent_dir = Path::new("/private/var/run/ahma/agent");
        set_upstream_agent(None).unwrap();

        set_ssh_agent_mode(SshAgentMode::Passthrough);
        assert_eq!(seatbelt_agent_connect_denies(), "");
        assert_eq!(seatbelt_broker_connect_allow(agent_dir), "");

        set_ssh_agent_mode(SshAgentMode::Off);
        assert!(seatbelt_agent_connect_denies().contains(LAUNCHD_AGENT_REGEX));
        assert_eq!(seatbelt_broker_connect_allow(agent_dir), "");

        set_ssh_agent_mode(SshAgentMode::Broker);
        assert!(seatbelt_agent_connect_denies().contains(LAUNCHD_AGENT_REGEX));
        assert_eq!(
            seatbelt_broker_connect_allow(agent_dir),
            "(allow network-outbound (remote unix-socket (subpath \"/private/var/run/ahma/agent\")))\n"
        );
    }
}
