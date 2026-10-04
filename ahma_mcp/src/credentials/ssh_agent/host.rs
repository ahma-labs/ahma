//! Where brokers run, and the per-command lease that serves one.
//!
//! Brokers run on one dedicated thread with its own runtime, started before
//! anything restricts the process (SPEC R-CRED.9): on Linux `restrict_self`
//! binds the calling thread and every thread it starts later, so a broker on
//! a restricted thread could not read the key it signs with.
//!
//! Each sandboxed command gets its own socket in [`agent_dir`], named at
//! random, and the broker answers only connections from that command's own
//! process group (SPEC R-CRED.1): the kernel tells it who connected, so one
//! command cannot borrow another's consent.

use super::broker::{Broker, BrokerEvent};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio_util::sync::CancellationToken;

/// The runtime brokers run on, started on first use.
pub fn host() -> &'static tokio::runtime::Handle {
    static HOST: OnceLock<tokio::runtime::Handle> = OnceLock::new();
    HOST.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("ahma-credentials".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("the credential runtime starts");
                let _ = tx.send(runtime.handle().clone());
                runtime.block_on(std::future::pending::<()>());
            })
            .expect("the credential thread starts");
        rx.recv()
            .expect("the credential runtime reports its handle")
    })
}

/// Start the broker thread now, before the process restricts itself
/// (SPEC R-CRED.9). Cheap and idempotent.
pub fn start() {
    let _ = host();
}

/// The directory broker sockets live in: `<runtime dir>/agent`, owner-only.
/// The sandbox profile lets a command connect to a socket here and nothing
/// else (it cannot create, remove or list them).
pub fn agent_dir() -> Option<PathBuf> {
    let dir = ahma_common::hub::runtime_dir()?.join("agent");
    std::fs::create_dir_all(&dir).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok()?;
    }
    Some(dir)
}

/// The longest socket path a Unix socket address holds (macOS `sun_path`).
const SUN_PATH_MAX: usize = 103;

/// One command's broker, served on its own socket for as long as this lives.
pub struct BrokerLease {
    socket: PathBuf,
    broker: Arc<Broker>,
    /// The process group allowed to connect; set once the command is spawned.
    group: Arc<OnceLock<u32>>,
    cancel: CancellationToken,
}

impl std::fmt::Debug for BrokerLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerLease")
            .field("socket", &self.socket)
            .field("group", &self.group.get())
            .finish_non_exhaustive()
    }
}

impl BrokerLease {
    /// Serve `broker` on a new socket in `dir`.
    pub fn start(broker: Arc<Broker>, dir: &Path) -> std::io::Result<Self> {
        let name: String = (0..16)
            .map(|_| format!("{:x}", rand::random::<u8>() & 0xf))
            .collect();
        let socket = dir.join(format!("{name}.sock"));
        if socket.as_os_str().len() > SUN_PATH_MAX {
            return Err(std::io::Error::other(format!(
                "{} is too long for a socket address",
                socket.display()
            )));
        }
        let std_listener = std::os::unix::net::UnixListener::bind(&socket)?;
        std_listener.set_nonblocking(true)?;
        let runtime = host();
        let listener = {
            let _entered = runtime.enter();
            tokio::net::UnixListener::from_std(std_listener)?
        };
        let cancel = CancellationToken::new();
        let group: Arc<OnceLock<u32>> = Arc::default();
        runtime.spawn(Arc::clone(&broker).serve_filtered(
            listener,
            cancel.clone(),
            Arc::clone(&group),
        ));
        Ok(Self {
            socket,
            broker,
            group,
            cancel,
        })
    }

    /// The socket to hand the command as `SSH_AUTH_SOCK`.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Admit connections from the process group led by `pid` — the command
    /// just spawned, which `base_command` makes a group leader.
    pub fn admit_group(&self, pid: u32) {
        let _ = self.group.set(pid);
    }

    /// What the broker did for this command.
    pub fn events(&self) -> Vec<BrokerEvent> {
        self.broker.events()
    }
}

impl Drop for BrokerLease {
    fn drop(&mut self) {
        self.cancel.cancel();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Whether the peer of `stream` may use a broker whose command leads process
/// group `group`: the same user as this process, in that group.
pub(super) fn admitted(stream: &tokio::net::UnixStream, group: Option<u32>) -> bool {
    let Ok(cred) = stream.peer_cred() else {
        return false;
    };
    // SAFETY: `geteuid` has no preconditions.
    if cred.uid() != unsafe { libc::geteuid() } {
        return false;
    }
    let (Some(group), Some(pid)) = (group, cred.pid()) else {
        return false;
    };
    // SAFETY: `getpgid` has no preconditions; a vanished pid returns -1.
    let peer_group = unsafe { libc::getpgid(pid) };
    peer_group >= 0 && peer_group as u32 == group
}

#[cfg(test)]
mod tests {
    use super::super::broker::{Decision, KeySources, SignConsent, SignRequest};
    use super::super::proto;
    use super::*;

    struct Allow;
    #[async_trait::async_trait]
    impl SignConsent for Allow {
        async fn decide(&self, _: &SignRequest) -> Decision {
            Decision::Allow
        }
    }

    fn lease(dir: &Path) -> BrokerLease {
        let broker = Broker::new(KeySources::for_home(dir, None), Arc::new(Allow));
        BrokerLease::start(broker, dir).unwrap()
    }

    async fn list(socket: &Path) -> Option<Vec<u8>> {
        let mut s = tokio::net::UnixStream::connect(socket).await.ok()?;
        proto::write_message(&mut s, &[proto::SSH_AGENTC_REQUEST_IDENTITIES])
            .await
            .ok()?;
        proto::read_message(&mut s).await.ok()?
    }

    /// Only the command's own process group is answered: this test process
    /// leads no admitted group until it is admitted.
    #[tokio::test]
    async fn only_the_admitted_process_group_is_served() {
        let dir = tempfile::tempdir().unwrap();
        let lease = lease(dir.path());
        assert_eq!(list(lease.socket()).await, None, "nobody admitted yet");

        // SAFETY: `getpgrp` has no preconditions.
        let own_group = unsafe { libc::getpgrp() } as u32;
        lease.admit_group(own_group);
        let answer = list(lease.socket()).await.expect("admitted");
        assert_eq!(proto::parse_identities_answer(&answer), Some(vec![]));
        assert!(matches!(
            lease.events().as_slice(),
            [BrokerEvent::RefusedOperation(why)] if why.contains("process group")
        ));
    }

    #[tokio::test]
    async fn the_socket_is_gone_when_the_lease_ends() {
        let dir = tempfile::tempdir().unwrap();
        let socket = {
            let lease = lease(dir.path());
            assert!(lease.socket().exists());
            lease.socket().to_path_buf()
        };
        assert!(!socket.exists());
    }

    #[test]
    fn a_socket_path_too_long_for_an_address_is_refused_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("d".repeat(120));
        std::fs::create_dir_all(&deep).unwrap();
        let broker = Broker::new(KeySources::for_home(dir.path(), None), Arc::new(Allow));
        assert!(BrokerLease::start(broker, &deep).is_err());
    }
}
