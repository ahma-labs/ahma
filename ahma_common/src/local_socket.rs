//! One local-socket transport for every OS: `AF_UNIX` stream sockets.
//!
//! The per-user hub is reached through a single socket file in the user's
//! runtime directory. Unix has always had `AF_UNIX`; Windows has had
//! it since Windows 10 1803, so the same socket file, permission model and
//! rendezvous work on every supported OS — no TCP port, no token, no named-pipe
//! special case.
//!
//! [`LocalListener`] and [`LocalStream`] hide the one difference that remains:
//! tokio can register `AF_UNIX` sockets with its reactor on Unix but not on
//! Windows. There each connection is a blocking `socket2` socket served by two
//! threads — one per direction — bridged to tokio through an in-memory
//! [`tokio::io::duplex`] pipe, so callers see an ordinary
//! [`AsyncRead`] + [`AsyncWrite`] stream either way. The hub has a handful of
//! connections (one per attached client), which is what makes a thread pair per
//! connection acceptable.
//!
//! Neither type removes the socket file: whoever owns the rendezvous (the hub,
//! holding `hub.lock`) unlinks a stale file before [`LocalListener::bind`] and
//! after dropping the listener.

use std::{
    io,
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A listening `AF_UNIX` stream socket bound to a filesystem path.
pub struct LocalListener {
    inner: imp::Listener,
    path: PathBuf,
}

impl LocalListener {
    /// Bind and listen on `path`. Fails if a file already exists there.
    ///
    /// Must be called inside a tokio runtime.
    pub fn bind(path: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: imp::Listener::bind(path)?,
            path: path.to_path_buf(),
        })
    }

    /// Wait for the next connection.
    pub async fn accept(&self) -> io::Result<LocalStream> {
        self.inner.accept().await.map(|inner| LocalStream { inner })
    }

    /// The path this listener is bound to.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A connected `AF_UNIX` stream socket.
pub struct LocalStream {
    inner: imp::Stream,
}

impl LocalStream {
    /// Connect to the listener bound at `path`.
    pub async fn connect(path: &Path) -> io::Result<Self> {
        imp::connect(path).await.map(|inner| Self { inner })
    }
}

impl AsyncRead for LocalStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for LocalStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(unix)]
mod imp {
    use std::{io, path::Path};

    use tokio::net::{UnixListener, UnixStream};

    pub(super) type Stream = UnixStream;

    pub(super) struct Listener(UnixListener);

    impl Listener {
        pub(super) fn bind(path: &Path) -> io::Result<Self> {
            UnixListener::bind(path).map(Self)
        }

        pub(super) async fn accept(&self) -> io::Result<Stream> {
            self.0.accept().await.map(|(stream, _)| stream)
        }
    }

    pub(super) async fn connect(path: &Path) -> io::Result<Stream> {
        UnixStream::connect(path).await
    }
}

#[cfg(windows)]
mod imp {
    use std::{
        io::{self, Read, Write},
        net::Shutdown,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::JoinHandle,
    };

    use socket2::{Domain, SockAddr, Socket, Type};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        runtime::Handle,
        sync::{Mutex, mpsc},
    };

    pub(super) type Stream = tokio::io::DuplexStream;

    /// Per-direction copy buffer, and the in-memory pipe's capacity.
    const BUF: usize = 64 * 1024;

    fn new_socket() -> io::Result<Socket> {
        Socket::new(Domain::UNIX, Type::STREAM, None)
    }

    /// A blocking listener whose accept loop runs on its own thread and hands
    /// each connection to [`Listener::accept`] through a channel.
    pub(super) struct Listener {
        conns: Mutex<mpsc::Receiver<io::Result<Socket>>>,
        closing: Arc<AtomicBool>,
        path: PathBuf,
        thread: Option<JoinHandle<()>>,
    }

    impl Listener {
        pub(super) fn bind(path: &Path) -> io::Result<Self> {
            let socket = new_socket()?;
            socket.bind(&SockAddr::unix(path)?)?;
            socket.listen(128)?;
            let (tx, rx) = mpsc::channel(16);
            let closing = Arc::new(AtomicBool::new(false));
            let stop = Arc::clone(&closing);
            let thread = std::thread::Builder::new()
                .name("ahma-local-accept".into())
                .spawn(move || {
                    loop {
                        let accepted = socket.accept().map(|(conn, _)| conn);
                        if stop.load(Ordering::Acquire) || tx.blocking_send(accepted).is_err() {
                            break;
                        }
                    }
                })?;
            Ok(Self {
                conns: Mutex::new(rx),
                closing,
                path: path.to_path_buf(),
                thread: Some(thread),
            })
        }

        pub(super) async fn accept(&self) -> io::Result<Stream> {
            let conn = self
                .conns
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| io::Error::other("local socket accept thread stopped"))??;
            bridge(conn, Handle::current())
        }
    }

    impl Drop for Listener {
        /// Close the listening socket before returning, so the file left behind
        /// refuses connections. The accept thread is parked in a blocking
        /// `accept()`: closing the channel releases it if it is waiting to hand
        /// over a connection, and a connection of our own wakes it to see
        /// `closing` and drop the socket.
        fn drop(&mut self) {
            self.closing.store(true, Ordering::Release);
            self.conns.get_mut().close();
            let woke = new_socket()
                .and_then(|s| s.connect(&SockAddr::unix(&self.path)?))
                .is_ok();
            if woke && let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    pub(super) async fn connect(path: &Path) -> io::Result<Stream> {
        let addr = SockAddr::unix(path)?;
        let conn = tokio::task::spawn_blocking(move || {
            let conn = new_socket()?;
            conn.connect(&addr)?;
            Ok::<_, io::Error>(conn)
        })
        .await
        .map_err(io::Error::other)??;
        bridge(conn, Handle::current())
    }

    /// Serve a blocking socket to async code: one thread copies socket →
    /// caller, one copies caller → socket, both through a duplex pipe whose
    /// other end is what the caller gets. EOF and errors propagate as a
    /// half-close in the same direction, so `shutdown()` and `read_to_end()`
    /// behave as they do on a tokio socket.
    fn bridge(conn: Socket, rt: Handle) -> io::Result<Stream> {
        let (caller, ours) = tokio::io::duplex(BUF);
        let (mut from_caller, mut to_caller) = tokio::io::split(ours);
        let conn = Arc::new(conn);

        let rx_conn = Arc::clone(&conn);
        let rx_rt = rt.clone();
        std::thread::Builder::new()
            .name("ahma-local-rx".into())
            .spawn(move || {
                let mut buf = vec![0u8; BUF];
                while let Ok(n @ 1..) = (&*rx_conn).read(&mut buf) {
                    if rx_rt.block_on(to_caller.write_all(&buf[..n])).is_err() {
                        // The caller dropped the stream.
                        let _ = rx_conn.shutdown(Shutdown::Read);
                        break;
                    }
                }
                let _ = rx_rt.block_on(to_caller.shutdown());
            })?;

        std::thread::Builder::new()
            .name("ahma-local-tx".into())
            .spawn(move || {
                let mut buf = vec![0u8; BUF];
                while let Ok(n @ 1..) = rt.block_on(from_caller.read(&mut buf)) {
                    if (&*conn).write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                let _ = conn.shutdown(Shutdown::Write);
            })?;

        Ok(caller)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeouts::{TestTimeouts, TimeoutCategory};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
        within_budget(TimeoutCategory::Quick, what, fut).await
    }

    async fn within_budget<T>(
        category: TimeoutCategory,
        what: &str,
        fut: impl std::future::Future<Output = T>,
    ) -> T {
        tokio::time::timeout(TestTimeouts::get(category), fut)
            .await
            .unwrap_or_else(|_| panic!("timed out: {what}"))
    }

    /// Serve one connection: read to EOF, answer with the bytes upper-cased, close.
    async fn serve_upper(listener: &LocalListener) {
        let mut conn = listener.accept().await.expect("accept");
        let mut got = Vec::new();
        conn.read_to_end(&mut got).await.expect("server read");
        conn.write_all(&got.to_ascii_uppercase())
            .await
            .expect("server write");
        conn.shutdown().await.expect("server shutdown");
    }

    async fn ask_upper(path: &Path, msg: &[u8]) -> Vec<u8> {
        let mut client = LocalStream::connect(path).await.expect("connect");
        client.write_all(msg).await.expect("client write");
        // Half-close: the server reads to EOF and can still answer.
        client.shutdown().await.expect("client shutdown");
        let mut answer = Vec::new();
        client.read_to_end(&mut answer).await.expect("client read");
        answer
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn round_trip_with_half_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sock");
        let listener = LocalListener::bind(&path).expect("bind");
        assert_eq!(listener.path(), path);

        let answer = within("round trip", async {
            let (_, answer) = tokio::join!(serve_upper(&listener), ask_upper(&path, b"ping\n"));
            answer
        })
        .await;
        assert_eq!(answer, b"PING\n");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serves_concurrent_connections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sock");
        let listener = LocalListener::bind(&path).expect("bind");
        const N: usize = 8;

        // Eight connections is two OS threads each on the Windows AF_UNIX
        // bridge plus eight blocking connects, on a 2-core runner: the `Quick`
        // budget (20s there) timed out on roughly one main run in three while
        // the same test passed on every PR run. The work is bounded and never
        // waits on anything outside the test, so a slow-runner budget is the
        // honest one; a genuine deadlock still fails, just later.
        within_budget(TimeoutCategory::ToolCall, "concurrent connections", async {
            let server = async {
                let mut conns = Vec::new();
                for _ in 0..N {
                    conns.push(listener.accept().await.expect("accept"));
                }
                // Answer in reverse order: no connection depends on another.
                for mut conn in conns.into_iter().rev() {
                    let mut got = Vec::new();
                    conn.read_to_end(&mut got).await.expect("server read");
                    conn.write_all(&got.to_ascii_uppercase())
                        .await
                        .expect("server write");
                    conn.shutdown().await.expect("server shutdown");
                }
            };
            let clients = futures_join_all((0..N).map(|i| {
                let path = path.clone();
                async move {
                    let msg = format!("client-{i}");
                    let answer = ask_upper(&path, msg.as_bytes()).await;
                    assert_eq!(answer, msg.to_ascii_uppercase().into_bytes());
                }
            }));
            tokio::join!(server, clients);
        })
        .await;
    }

    /// A payload far larger than any internal buffer, written while the echo is
    /// being read back, so a bridge that cannot make progress in both
    /// directions at once deadlocks here instead of in production.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn large_payload_streams_both_ways_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sock");
        let listener = LocalListener::bind(&path).expect("bind");
        let payload: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();

        let echoed = within("large payload", async {
            let server = async {
                let conn = listener.accept().await.expect("accept");
                let (mut r, mut w) = tokio::io::split(conn);
                tokio::io::copy(&mut r, &mut w).await.expect("echo");
                w.shutdown().await.expect("server shutdown");
            };
            let client = async {
                let client = LocalStream::connect(&path).await.expect("connect");
                let (mut r, mut w) = tokio::io::split(client);
                let send = async {
                    w.write_all(&payload).await.expect("client write");
                    w.shutdown().await.expect("client shutdown");
                };
                let recv = async {
                    let mut echoed = Vec::new();
                    r.read_to_end(&mut echoed).await.expect("client read");
                    echoed
                };
                tokio::join!(send, recv).1
            };
            tokio::join!(server, client).1
        })
        .await;
        assert!(echoed == payload, "echo differs from payload");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_without_a_listener_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.sock");
        let result = within("connect", LocalStream::connect(&path)).await;
        assert!(result.is_err(), "connect to a missing socket succeeded");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bind_refuses_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sock");
        let _first = LocalListener::bind(&path).expect("bind");
        assert!(
            LocalListener::bind(&path).is_err(),
            "a second bind on a live socket path succeeded"
        );
    }

    /// Dropping the listener closes it: the file left behind refuses
    /// connections rather than accepting them into a void.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_listener_refuses_connections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sock");
        let listener = LocalListener::bind(&path).expect("bind");
        drop(listener);
        let result = within("connect after drop", LocalStream::connect(&path)).await;
        assert!(result.is_err(), "connect to a dropped listener succeeded");
    }

    /// `futures::future::join_all` without the dependency.
    async fn futures_join_all<F: std::future::Future<Output = ()> + Send + 'static>(
        futs: impl Iterator<Item = F>,
    ) {
        let handles: Vec<_> = futs.map(tokio::spawn).collect();
        for h in handles {
            h.await.expect("client task");
        }
    }
}
