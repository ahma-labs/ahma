//! The SSH key broker's per-command lease (SPEC R-CRED.1): a [`Broker`] served
//! on its own [`SocketLease`], which names, binds and removes the socket and
//! admits only a command this process spawned. Where brokers run and where
//! their sockets live is [`crate::credentials::host`].

use super::broker::{Broker, BrokerEvent};
use crate::credentials::host::SocketLease;
use std::path::Path;
use std::sync::Arc;

/// One command's broker, served on its own socket for as long as this lives.
pub struct BrokerLease {
    socket: SocketLease,
    broker: Arc<Broker>,
}

impl std::fmt::Debug for BrokerLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerLease")
            .field("socket", &self.socket.socket())
            .finish_non_exhaustive()
    }
}

impl BrokerLease {
    /// Serve `broker` on a new socket in `dir`.
    pub fn start(broker: Arc<Broker>, dir: &Path) -> std::io::Result<Self> {
        let socket = SocketLease::start(Arc::clone(&broker), dir, ".sock")?;
        Ok(Self { socket, broker })
    }

    /// The socket to hand the command as `SSH_AUTH_SOCK`.
    pub fn socket(&self) -> &Path {
        self.socket.socket()
    }

    /// What the broker did for this command.
    pub fn events(&self) -> Vec<BrokerEvent> {
        self.broker.events()
    }

    /// Tell `observer` of each event as it happens.
    pub fn observe(&self, observer: super::broker::EventObserver) {
        self.broker.observe(observer);
    }
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

    /// A process ahma did not spawn is refused: this test process is no
    /// child of itself. A command it spawns into its own process group — as
    /// ahma spawns every command — is answered.
    #[tokio::test]
    async fn only_a_command_this_process_spawned_is_served() {
        let dir = tempfile::tempdir().unwrap();
        let lease = lease(dir.path());
        assert_eq!(list(lease.socket()).await, None, "not a spawned command");
        assert!(matches!(
            lease.events().as_slice(),
            [BrokerEvent::RefusedOperation(why)] if why.contains("ahma did not spawn")
        ));

        if std::process::Command::new("ssh-add")
            .arg("-h")
            .output()
            .is_err()
        {
            eprintln!("skipped the spawned half: ssh-add is not installed");
            return;
        }
        let socket = lease.socket().to_path_buf();
        let out = tokio::task::spawn_blocking(move || {
            use std::os::unix::process::CommandExt;
            std::process::Command::new("ssh-add")
                .arg("-l")
                .env("SSH_AUTH_SOCK", &socket)
                .process_group(0)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        // An empty agent: "The agent has no identities", exit status 1.
        // Refused, ssh-add would say it cannot connect (exit status 2).
        assert_eq!(
            out.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
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
