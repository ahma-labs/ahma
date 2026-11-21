//! Where brokers run, and the per-command socket each one is served on.
//!
//! Brokers run on one dedicated thread with its own runtime, started before
//! anything restricts the process (SPEC R-CRED.9): on Linux `restrict_self`
//! binds the calling thread and every thread it starts later, so a broker on
//! a restricted thread could not read the key it signs with.
//!
//! Each sandboxed command gets its own socket in [`agent_dir`], named at
//! random, and a [`SocketLease`] answers only a connection from a process
//! group led by a command this ahma process spawned (SPEC R-CRED.1): every
//! command ahma runs leads its own group, and the kernel says who connected.
//! A command of another session, another ahma process or anything else on the
//! machine is refused, so no one borrows another session's consent. The check
//! is in the lease, not in the broker, so no broker can be served without it.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::net::{UnixListener, UnixStream};
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

/// The longest socket path a Unix socket address holds: 103 chars on macOS (104-byte `sun_path`),
/// 107 chars on Linux (108-byte `sun_path`).
#[cfg(target_os = "macos")]
const SUN_PATH_MAX: usize = 103;
#[cfg(not(target_os = "macos"))]
const SUN_PATH_MAX: usize = 107;

/// What a [`SocketLease`] serves: a broker's side of one connection.
pub trait SocketService: Send + Sync + 'static {
    /// Answer one connection the lease admitted (SPEC R-CRED.1), until it
    /// ends or the lease does.
    fn serve_connection(self: Arc<Self>, stream: UnixStream) -> impl Future<Output = ()> + Send;

    /// A connection the lease refused: from a process ahma did not spawn. It
    /// is closed unanswered once this returns.
    fn refused(&self);
}

/// One command's socket, served on the [`host`] runtime for as long as this
/// lives, and removed when it ends.
#[derive(Debug)]
pub struct SocketLease {
    socket: PathBuf,
    cancel: CancellationToken,
}

impl SocketLease {
    /// Serve `service` on a new socket in `dir`, named `<8 hex><suffix>`
    /// (`.sock` for the SSH key broker). A path too long for a socket address
    /// is an error, never truncated.
    pub fn start<S: SocketService>(
        service: Arc<S>,
        dir: &Path,
        suffix: &str,
    ) -> std::io::Result<Self> {
        let name: String = (0..8)
            .map(|_| format!("{:x}", rand::random::<u8>() & 0xf))
            .collect();
        let socket = dir.join(format!("{name}{suffix}"));
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
            UnixListener::from_std(std_listener)?
        };
        let cancel = CancellationToken::new();
        runtime.spawn(serve(listener, service, cancel.clone()));
        Ok(Self { socket, cancel })
    }

    /// The socket to hand the command.
    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Drop for SocketLease {
    fn drop(&mut self) {
        self.cancel.cancel();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Serve connections on `listener` until `cancel` fires, answering only
/// commands this process spawned (SPEC R-CRED.1). A connection from anywhere
/// else is closed unanswered and reported to `service`.
async fn serve<S: SocketService>(
    listener: UnixListener,
    service: Arc<S>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    if !admitted(&stream) {
                        service.refused();
                        continue;
                    }
                    let service = Arc::clone(&service);
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = cancel.cancelled() => {}
                            _ = service.serve_connection(stream) => {}
                        }
                    });
                }
                Err(e) => {
                    tracing::debug!("broker accept: {e}");
                    return;
                }
            },
        }
    }
}

/// Whether the peer of `stream` may use this process's brokers: the same
/// user, in a process group led by a direct child of this process — a
/// command ahma spawned, which `base_command` makes a group leader.
fn admitted(stream: &UnixStream) -> bool {
    let Ok(cred) = stream.peer_cred() else {
        return false;
    };
    // SAFETY: `geteuid` has no preconditions.
    if cred.uid() != unsafe { libc::geteuid() } {
        return false;
    }
    let Some(pid) = cred.pid() else {
        return false;
    };
    // SAFETY: `getpgid` has no preconditions; a vanished pid returns -1.
    let group = unsafe { libc::getpgid(pid) };
    if group <= 0 {
        return false;
    }
    let leader = sysinfo::Pid::from_u32(group as u32);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[leader]), true);
    system
        .process(leader)
        .and_then(|p| p.parent())
        .is_some_and(|parent| parent.as_u32() == std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt;

    /// A service that is not the SSH broker: counts what it was given.
    #[derive(Default)]
    struct Counting {
        served: AtomicUsize,
        refused: AtomicUsize,
    }

    impl SocketService for Counting {
        async fn serve_connection(self: Arc<Self>, _: UnixStream) {
            self.served.fetch_add(1, Ordering::SeqCst);
        }

        fn refused(&self) {
            self.refused.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The admission check belongs to the lease, so any service gets it: a
    /// connection from this test process — no child of itself — is closed
    /// unanswered and reported as refused, never served. The socket carries
    /// the service's suffix and is gone when the lease ends.
    #[tokio::test]
    async fn any_service_is_served_behind_the_admission_check() {
        let dir = tempfile::tempdir().unwrap();
        let service = Arc::new(Counting::default());
        let socket = {
            let lease = SocketLease::start(Arc::clone(&service), dir.path(), ".test.sock").unwrap();
            let name = lease.socket().file_name().unwrap().to_str().unwrap();
            assert!(
                name.len() == 8 + ".test.sock".len() && name.ends_with(".test.sock"),
                "{name}"
            );
            let mut stream = UnixStream::connect(lease.socket()).await.unwrap();
            let mut rest = Vec::new();
            // The lease closes a refused connection after reporting it.
            stream.read_to_end(&mut rest).await.unwrap();
            assert!(rest.is_empty());
            assert_eq!(service.refused.load(Ordering::SeqCst), 1);
            assert_eq!(service.served.load(Ordering::SeqCst), 0);
            lease.socket().to_path_buf()
        };
        assert!(!socket.exists());
    }
}
