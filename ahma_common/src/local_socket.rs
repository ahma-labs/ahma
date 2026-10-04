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
//! A Windows `AF_UNIX` `connect()` can return before the listener has accepted
//! the connection, so a client that writes and half-closes at once can do so
//! while its connection is still waiting in the listener's queue. The bridge
//! retries a call refused as not connected (`WSAENOTCONN`) rather than dropping
//! the bytes or the half-close (SPEC R-HUB.2): a lost half-close is a hang, with
//! the server waiting for an end of request that never comes and the client
//! waiting for an answer to it.
//!
//! Dropping a [`LocalStream`] closes the connection on every OS, whether or not
//! the peer ever sends or closes anything (SPEC R-HUB.2). On Windows that takes
//! more than letting go of the socket: the receiving thread is blocked in
//! `recv`, which no `shutdown` wakes, so the bridge sends what was written,
//! half-closes, and then cancels that receive (`CancelIoEx`).
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

/// Retrying the blocking socket calls of the Windows bridge while the
/// connection is not yet accepted. Platform-neutral so it is tested everywhere.
#[cfg(any(windows, test))]
mod not_connected {
    use std::{
        io,
        time::{Duration, Instant},
    };

    /// How long a call refused as not connected is retried. It bounds the wait
    /// for a listener to accept a connection that is already queued; a hub
    /// that has not accepted in this long is not going to.
    pub(super) const GRACE: Duration = Duration::from_secs(30);

    /// Longest pause between attempts: the accept usually lands within
    /// milliseconds, so the backoff starts at 1ms and stays short.
    const MAX_PAUSE: Duration = Duration::from_millis(50);

    /// Run `call`, retrying while it fails with [`io::ErrorKind::NotConnected`]
    /// (`WSAENOTCONN` on Windows) for up to `grace`, unless `give_up()` says
    /// the connection is past the point where that error can be transient.
    /// Every other outcome, success or error, is returned at once.
    pub(super) fn retry<T>(
        grace: Duration,
        give_up: impl Fn() -> bool,
        mut call: impl FnMut() -> io::Result<T>,
    ) -> io::Result<T> {
        let deadline = Instant::now() + grace;
        let mut pause = Duration::from_millis(1);
        loop {
            match call() {
                Err(e)
                    if e.kind() == io::ErrorKind::NotConnected
                        && !give_up()
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(MAX_PAUSE);
                }
                result => return result,
            }
        }
    }
}

/// The Windows bridge between one blocking socket and async callers. It is
/// written against a `BlockingSocket` trait rather than `socket2` so that
/// its lifecycle — above all, that dropping the stream closes the socket — is
/// tested on every OS, not only on the one that ships it.
#[cfg(any(windows, test))]
mod bridge {
    use std::{
        io,
        pin::Pin,
        sync::Arc,
        task::{Context, Poll},
        time::{Duration, Instant},
    };

    use parking_lot::{Condvar, Mutex, MutexGuard};
    use tokio::{
        io::{
            AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf, ReadHalf,
            WriteHalf,
        },
        runtime::Handle,
    };

    use super::not_connected;

    /// Per-direction copy buffer, and the in-memory pipe's capacity.
    const BUF: usize = 64 * 1024;

    /// Pause between attempts to abort a receive. An abort reaches only a
    /// receive already in progress (see [`BlockingSocket::abort_recv`]), so one
    /// that lands just before the receiving thread blocks is repeated.
    const ABORT_RETRY: Duration = Duration::from_millis(20);

    /// How long a receive that will not wake is aborted before it is left
    /// blocked, holding the socket until the peer acts — what happened to every
    /// dropped connection before aborting existed.
    const ABORT_GRACE: Duration = Duration::from_secs(5);

    /// The longest a single `recv` waits before the receiving thread looks
    /// again. The end of the peer's stream is not always delivered to a `recv`
    /// that is already waiting (measured on Windows `AF_UNIX`: a server whose
    /// receive was re-armed as the client's half-close landed waited for it
    /// forever), but a `recv` that starts afterwards returns it. A bounded
    /// receive turns that lost wake-up into a delay of at most this long, and
    /// also honours a pause or an abort that `abort_recv` missed.
    const RECV_RECHECK: Duration = Duration::from_millis(50);

    /// The blocking calls the bridge makes on its socket.
    pub(super) trait BlockingSocket: Send + Sync + 'static {
        fn recv(&self, buf: &mut [u8]) -> io::Result<usize>;
        fn send(&self, data: &[u8]) -> io::Result<usize>;
        fn shutdown_write(&self) -> io::Result<()>;
        /// Make every later `recv` give up after `every` with
        /// [`io::ErrorKind::TimedOut`] or [`io::ErrorKind::WouldBlock`]
        /// (`SO_RCVTIMEO`), which the receiving thread takes as "nothing yet".
        fn bound_recv(&self, every: Duration) -> io::Result<()>;
        /// Make a `recv` blocked in another thread return, with anything.
        ///
        /// Called only once the caller has dropped the stream, so whatever that
        /// receive would have produced is discarded anyway. It need not affect
        /// a `recv` that starts afterwards: the bridge repeats it until the
        /// receiving thread has finished. A `shutdown` is not enough on
        /// Windows: it does not wake a `recv` blocked in another thread.
        fn abort_recv(&self) -> io::Result<()>;
        /// Whether a half-close made while a `recv` is blocked on the same
        /// socket in another thread can be lost, so the receive must step out
        /// of `recv` for it (measured on Windows `AF_UNIX`: the peer of an
        /// accepted socket never saw the end of the stream).
        const HALF_CLOSE_NEEDS_QUIET_RECEIVE: bool = false;
    }

    #[derive(Default)]
    struct State {
        /// The caller dropped its end of the stream.
        caller_gone: bool,
        /// The receiving thread is to stop; set only once `caller_gone`.
        abort: bool,
        /// The receiving thread has finished: it read the peer's end of
        /// stream or an error, or it was aborted.
        rx_done: bool,
        /// The sending thread asks the receiving one to step out of `recv`
        /// while it half-closes ([`BlockingSocket::HALF_CLOSE_NEEDS_QUIET_RECEIVE`]).
        pause: bool,
        /// The receiving thread is out of `recv` and waits for `pause` to end.
        paused: bool,
    }

    /// What the caller's stream and the two relay threads tell each other.
    #[derive(Default)]
    struct Lifecycle {
        state: Mutex<State>,
        changed: Condvar,
    }

    impl Lifecycle {
        fn update(&self, f: impl FnOnce(&mut State)) {
            f(&mut self.state.lock());
            self.changed.notify_all();
        }

        fn aborting(&self) -> bool {
            self.state.lock().abort
        }

        fn rx_done(&self) -> bool {
            self.state.lock().rx_done
        }

        /// If the sender asked for a pause, acknowledge it and wait until it
        /// ends (or the receive is aborted). Returns whether there was one.
        fn wait_out_pause(&self) -> bool {
            let mut state = self.state.lock();
            if !state.pause {
                return false;
            }
            state.paused = true;
            self.changed.notify_all();
            while state.pause && !state.abort {
                self.changed.wait(&mut state);
            }
            state.paused = false;
            true
        }
    }

    /// The caller's end of a bridged connection: an in-memory pipe to the two
    /// relay threads, which also tells them when the caller lets go of it.
    pub(super) struct BridgedStream {
        pipe: DuplexStream,
        lifecycle: Arc<Lifecycle>,
    }

    impl Drop for BridgedStream {
        /// Runs before `pipe` is dropped, so the relay that sees the pipe close
        /// already knows the caller is gone rather than merely half-closed.
        fn drop(&mut self) {
            self.lifecycle.update(|s| s.caller_gone = true);
        }
    }

    impl AsyncRead for BridgedStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.pipe).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for BridgedStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.pipe).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.pipe).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.pipe).poll_shutdown(cx)
        }
    }

    /// Serve a blocking socket to async code: one thread copies socket →
    /// caller, one copies caller → socket, both through a duplex pipe whose
    /// other end is what the caller gets. EOF and errors propagate as a
    /// half-close in the same direction, so `shutdown()` and `read_to_end()`
    /// behave as they do on a tokio socket.
    ///
    /// Either thread may run before the listener has accepted the connection
    /// (a client that connects, writes and half-closes at once), so a call
    /// refused as not connected is retried, never taken for the end of the
    /// stream (SPEC R-HUB.2). Taking it for the end loses the request or its
    /// half-close, and both ends then wait on each other forever.
    ///
    /// Dropping the stream closes the socket once everything written before
    /// the drop has been sent and half-closed, whether or not the peer ever
    /// sends or closes anything (SPEC R-HUB.2). The socket is shared by the two
    /// threads and closes when the second lets go of it.
    pub(super) fn bridge<S: BlockingSocket>(sock: S, rt: Handle) -> io::Result<BridgedStream> {
        sock.bound_recv(RECV_RECHECK)?;
        let (caller, ours) = tokio::io::duplex(BUF);
        let (from_caller, to_caller) = tokio::io::split(ours);
        let sock = Arc::new(sock);
        let lifecycle = Arc::new(Lifecycle::default());

        let rx = {
            let (sock, lifecycle, rt) = (Arc::clone(&sock), Arc::clone(&lifecycle), rt.clone());
            move || receive(&*sock, to_caller, &lifecycle, &rt)
        };
        std::thread::Builder::new()
            .name("ahma-local-rx".into())
            .spawn(rx)?;

        let tx = {
            let lifecycle = Arc::clone(&lifecycle);
            move || transmit(&*sock, from_caller, &lifecycle, &rt)
        };
        std::thread::Builder::new()
            .name("ahma-local-tx".into())
            .spawn(tx)?;

        Ok(BridgedStream {
            pipe: caller,
            lifecycle,
        })
    }

    /// Socket → caller, until the peer ends its stream, the socket fails, or
    /// [`transmit`] aborts the receive because the caller is gone.
    fn receive<S: BlockingSocket>(
        sock: &S,
        mut to_caller: WriteHalf<DuplexStream>,
        lifecycle: &Lifecycle,
        rt: &Handle,
    ) {
        let mut buf = vec![0u8; BUF];
        let mut forwarding = true;
        while !lifecycle.aborting() {
            lifecycle.wait_out_pause();
            if lifecycle.aborting() {
                break;
            }
            let read = not_connected::retry(
                not_connected::GRACE,
                || lifecycle.aborting(),
                || sock.recv(&mut buf),
            );
            let n = match read {
                Ok(0) => break,
                Ok(n) => n,
                // Nothing yet: the bounded receive gave up (`RECV_RECHECK`), or
                // an aborted one reports itself as interrupted or, on Windows,
                // as `TimedOut` (`WSA_OPERATION_ABORTED`). Look again; the loop
                // condition and the pause check tell these apart. Taking a
                // quiet spell for the end of the stream would cut the answer
                // short, and never looking again can miss the end entirely.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue;
                }
                // A receive the sender interrupted to half-close: not an
                // error, and the loop takes it up again after the pause.
                Err(_) if lifecycle.wait_out_pause() => continue,
                Err(e) => {
                    if !lifecycle.aborting() {
                        tracing::debug!(error = %e, "local socket: receive failed");
                    }
                    break;
                }
            };
            // Once the caller is gone, keep reading and discarding until
            // aborted rather than stopping: closing a socket with unread data
            // in it can reset the connection, and a reset can destroy the
            // reply still on its way to the peer.
            if forwarding && rt.block_on(to_caller.write_all(&buf[..n])).is_err() {
                forwarding = false;
            }
        }
        lifecycle.update(|s| s.rx_done = true);
        let _ = rt.block_on(to_caller.shutdown());
    }

    /// Caller → socket, then the end of the connection: once the caller's
    /// last bytes are sent and the half-close is made, wait for the caller to
    /// drop the stream and abort the receive, so that the socket closes even
    /// when the peer never acts.
    fn transmit<S: BlockingSocket>(
        sock: &S,
        mut from_caller: ReadHalf<DuplexStream>,
        lifecycle: &Lifecycle,
        rt: &Handle,
    ) {
        let mut buf = vec![0u8; BUF];
        let relayed = loop {
            match rt.block_on(from_caller.read(&mut buf)) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    if let Err(e) = send_all(sock, &buf[..n], || lifecycle.rx_done()) {
                        break Err(e);
                    }
                }
                Err(e) => break Err(e),
            }
        };
        let closed = match relayed {
            Ok(()) => {
                with_quiet_receive(sock, lifecycle, || half_close(sock, || lifecycle.rx_done()))
            }
            Err(e) => {
                tracing::debug!(error = %e, "local socket: send failed");
                sock.shutdown_write()
            }
        };
        if let Err(e) = closed {
            tracing::debug!(error = %e, "local socket: half-close failed");
        }
        abort_receive_once_the_caller_is_gone(sock, lifecycle);
    }

    /// Run `f` (the half-close) with no `recv` in progress on the socket, where
    /// the platform needs that ([`BlockingSocket::HALF_CLOSE_NEEDS_QUIET_RECEIVE`]):
    /// ask the receiving thread to pause, cancel its blocked `recv` until it
    /// says it is out, run `f`, and let it resume. Bounded by `ABORT_GRACE`;
    /// past that `f` runs anyway, as it did before.
    fn with_quiet_receive<S: BlockingSocket, T>(
        sock: &S,
        lifecycle: &Lifecycle,
        f: impl FnOnce() -> T,
    ) -> T {
        if !S::HALF_CLOSE_NEEDS_QUIET_RECEIVE {
            return f();
        }
        {
            let mut state = lifecycle.state.lock();
            if state.rx_done {
                drop(state);
                return f();
            }
            state.pause = true;
            lifecycle.changed.notify_all();
            let deadline = Instant::now() + ABORT_GRACE;
            while !state.paused && !state.rx_done && Instant::now() < deadline {
                if let Err(e) = MutexGuard::unlocked(&mut state, || sock.abort_recv()) {
                    tracing::debug!(error = %e, "local socket: pausing the receive failed");
                }
                lifecycle.changed.wait_for(&mut state, ABORT_RETRY);
            }
        }
        let out = f();
        lifecycle.update(|s| s.pause = false);
        out
    }

    fn abort_receive_once_the_caller_is_gone<S: BlockingSocket>(sock: &S, lifecycle: &Lifecycle) {
        let mut state = lifecycle.state.lock();
        while !state.caller_gone && !state.rx_done {
            lifecycle.changed.wait(&mut state);
        }
        if state.rx_done {
            return;
        }
        state.abort = true;
        let deadline = Instant::now() + ABORT_GRACE;
        while !state.rx_done {
            if Instant::now() >= deadline {
                tracing::debug!(
                    "local socket: a blocked receive did not abort; the socket stays open until the peer closes it"
                );
                return;
            }
            if let Err(e) = MutexGuard::unlocked(&mut state, || sock.abort_recv()) {
                tracing::debug!(error = %e, "local socket: aborting the receive failed");
            }
            lifecycle.changed.wait_for(&mut state, ABORT_RETRY);
        }
    }

    /// Send all of `data`, retrying a send refused because the listener has
    /// not accepted the connection yet. Once `give_up()` — the peer has been
    /// heard to finish or fail — that refusal is final.
    pub(super) fn send_all<S: BlockingSocket>(
        sock: &S,
        mut data: &[u8],
        give_up: impl Fn() -> bool,
    ) -> io::Result<()> {
        while !data.is_empty() {
            match not_connected::retry(not_connected::GRACE, &give_up, || sock.send(data)) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => data = &data[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Half-close the connection, with the same retry as [`send_all`]: the
    /// end of a request is as much a part of it as its bytes.
    pub(super) fn half_close<S: BlockingSocket>(
        sock: &S,
        give_up: impl Fn() -> bool,
    ) -> io::Result<()> {
        not_connected::retry(not_connected::GRACE, give_up, || sock.shutdown_write())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::timeouts::{TestTimeouts, TimeoutCategory};
        use std::{
            io::{Read, Write},
            net::Shutdown,
        };

        fn quick() -> Duration {
            TestTimeouts::get(TimeoutCategory::Quick)
        }

        /// A runtime for the bridge's threads to drive the in-memory pipe
        /// with; the test itself runs on a plain thread.
        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("runtime")
        }

        /// A socket whose half-close never reaches the peer, so the peer can
        /// learn that the stream ended only from the socket being closed.
        struct LossyFin<S>(S);

        impl<S: BlockingSocket> BlockingSocket for LossyFin<S> {
            const HALF_CLOSE_NEEDS_QUIET_RECEIVE: bool = S::HALF_CLOSE_NEEDS_QUIET_RECEIVE;

            fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
                self.0.recv(buf)
            }
            fn send(&self, data: &[u8]) -> io::Result<usize> {
                self.0.send(data)
            }
            fn shutdown_write(&self) -> io::Result<()> {
                Ok(())
            }
            fn bound_recv(&self, every: Duration) -> io::Result<()> {
                self.0.bound_recv(every)
            }
            fn abort_recv(&self) -> io::Result<()> {
                self.0.abort_recv()
            }
        }

        #[cfg(unix)]
        type Sock = std::os::unix::net::UnixStream;

        /// Test-only: the production Unix path is tokio's own socket.
        #[cfg(unix)]
        impl BlockingSocket for Sock {
            fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
                let mut this = self;
                this.read(buf)
            }
            fn send(&self, data: &[u8]) -> io::Result<usize> {
                let mut this = self;
                this.write(data)
            }
            fn shutdown_write(&self) -> io::Result<()> {
                self.shutdown(Shutdown::Write)
            }
            fn bound_recv(&self, every: Duration) -> io::Result<()> {
                self.set_read_timeout(Some(every))
            }
            /// Unlike on Windows, `shutdown(Read)` wakes a blocked `recv` here
            /// (and every later one returns at once), and it sends the peer
            /// nothing it could read as the end of the stream.
            fn abort_recv(&self) -> io::Result<()> {
                self.shutdown(Shutdown::Read)
            }
        }

        /// Ours, the peer's, and whatever must outlive them.
        #[cfg(unix)]
        fn connected_pair() -> (Option<tempfile::TempDir>, Sock, Sock) {
            let (ours, peer) = Sock::pair().expect("socket pair");
            (None, ours, peer)
        }

        #[cfg(windows)]
        type Sock = socket2::Socket;

        /// Ours (accepted, as the hub's side is), the peer's, and the
        /// directory holding the socket file.
        #[cfg(windows)]
        fn connected_pair() -> (Option<tempfile::TempDir>, Sock, Sock) {
            use socket2::{Domain, SockAddr, Type};
            let dir = tempfile::tempdir().unwrap();
            let addr = SockAddr::unix(dir.path().join("t.sock")).unwrap();
            let new = || Sock::new(Domain::UNIX, Type::STREAM, None).unwrap();
            let listener = new();
            listener.bind(&addr).unwrap();
            listener.listen(1).unwrap();
            let peer = new();
            peer.connect(&addr).unwrap();
            let (ours, _) = listener.accept().unwrap();
            (Some(dir), ours, peer)
        }

        /// Dropping the stream closes the socket even when nothing else would:
        /// the half-close is lost and the peer never sends or closes anything.
        /// The reply written just before the drop still arrives, then the end
        /// of the stream. Before this, the receiving thread stayed blocked in
        /// `recv`, holding the socket open, until the peer acted — one thread
        /// and one socket leaked for each client that went quiet after the
        /// hub's last reply, and a peer waiting for our end waited forever.
        #[test]
        fn a_dropped_stream_closes_its_socket_even_if_the_half_close_is_lost() {
            let rt = runtime();
            let (_dir, ours, peer) = connected_pair();
            let reply = b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n";
            let mut stream = bridge(LossyFin(ours), rt.handle().clone()).expect("bridge");
            rt.block_on(stream.write_all(reply))
                .expect("write the reply");
            drop(stream);

            peer.set_read_timeout(Some(quick())).unwrap();
            let mut got = Vec::new();
            (&peer)
                .read_to_end(&mut got)
                .expect("the reply and then the end of the stream, the peer doing nothing");
            assert_eq!(got, reply);
        }

        /// The receive is aborted for a caller that is gone, never for one
        /// that has only finished sending: a half-closed stream still gets
        /// its answer.
        #[test]
        fn a_half_closed_stream_still_receives_the_answer() {
            let rt = runtime();
            let (_dir, ours, peer) = connected_pair();
            let mut stream = bridge(ours, rt.handle().clone()).expect("bridge");
            rt.block_on(async {
                stream.write_all(b"ping").await?;
                stream.shutdown().await
            })
            .expect("request and half-close");

            peer.set_read_timeout(Some(quick())).unwrap();
            let mut request = Vec::new();
            (&peer)
                .read_to_end(&mut request)
                .expect("the request, then its half-close");
            assert_eq!(request, b"ping");
            (&peer).write_all(b"pong").expect("answer");
            peer.shutdown(Shutdown::Write).expect("end the answer");

            let mut answer = Vec::new();
            rt.block_on(async {
                tokio::time::timeout(quick(), stream.read_to_end(&mut answer)).await
            })
            .expect("the answer within budget")
            .expect("read the answer");
            assert_eq!(answer, b"pong");
        }

        /// A socket that, like an accepted Windows `AF_UNIX` socket, loses a
        /// half-close made while a `recv` is blocked on it, and whose abort
        /// reaches only a `recv` already in progress.
        #[derive(Default)]
        struct FinState {
            in_recv: bool,
            aborted: bool,
            peer_saw_eof: bool,
            lost_fins: usize,
            incoming: Vec<u8>,
            peer_closed: bool,
        }

        #[derive(Default)]
        struct Fin {
            state: Mutex<FinState>,
            changed: Condvar,
        }

        struct LosesFinDuringRecv(Arc<Fin>);

        impl BlockingSocket for LosesFinDuringRecv {
            const HALF_CLOSE_NEEDS_QUIET_RECEIVE: bool = true;

            fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
                let fin = &*self.0;
                let mut s = fin.state.lock();
                let out = loop {
                    if !s.incoming.is_empty() {
                        let n = s.incoming.len().min(buf.len());
                        buf[..n].copy_from_slice(&s.incoming[..n]);
                        s.incoming.drain(..n);
                        break Ok(n);
                    }
                    if s.peer_closed {
                        break Ok(0);
                    }
                    if s.aborted {
                        s.aborted = false;
                        break Err(io::Error::other("the I/O operation has been aborted"));
                    }
                    s.in_recv = true;
                    fin.changed.notify_all();
                    fin.changed.wait(&mut s);
                };
                s.in_recv = false;
                out
            }
            fn send(&self, data: &[u8]) -> io::Result<usize> {
                Ok(data.len())
            }
            fn shutdown_write(&self) -> io::Result<()> {
                let fin = &*self.0;
                let mut s = fin.state.lock();
                if s.in_recv {
                    s.lost_fins += 1;
                } else {
                    s.peer_saw_eof = true;
                }
                fin.changed.notify_all();
                Ok(())
            }
            /// Unbounded on purpose: this models the cancel path alone.
            fn bound_recv(&self, _: Duration) -> io::Result<()> {
                Ok(())
            }
            fn abort_recv(&self) -> io::Result<()> {
                let fin = &*self.0;
                let mut s = fin.state.lock();
                if s.in_recv {
                    s.aborted = true;
                    fin.changed.notify_all();
                }
                Ok(())
            }
        }

        /// The hub's case: the server half-closes its answer while its own
        /// receive still waits on the socket. Where that half-close would be
        /// lost (an accepted Windows AF_UNIX socket), the receive steps out of
        /// `recv` for it, the peer sees the end, and receiving resumes.
        #[test]
        fn a_half_close_is_not_lost_to_a_receive_in_progress() {
            let rt = runtime();
            let fin = Arc::new(Fin::default());
            let mut stream =
                bridge(LosesFinDuringRecv(Arc::clone(&fin)), rt.handle().clone()).expect("bridge");
            let wait_until = |what: &str, done: &dyn Fn(&FinState) -> bool| {
                let deadline = std::time::Instant::now() + quick();
                let mut s = fin.state.lock();
                while !done(&s) {
                    assert!(std::time::Instant::now() < deadline, "timed out: {what}");
                    fin.changed.wait_for(&mut s, quick());
                }
            };
            wait_until("the receive is blocked", &|s| s.in_recv);

            rt.block_on(async {
                stream.write_all(b"answer").await?;
                stream.shutdown().await
            })
            .expect("answer and half-close");
            wait_until("the peer sees the end", &|s| s.peer_saw_eof);
            assert_eq!(fin.state.lock().lost_fins, 0, "a half-close was lost");

            {
                let mut s = fin.state.lock();
                s.incoming.extend_from_slice(b"more");
                s.peer_closed = true;
                fin.changed.notify_all();
            }
            let mut got = Vec::new();
            rt.block_on(async {
                tokio::time::timeout(quick(), stream.read_to_end(&mut got)).await
            })
            .expect("receiving resumed")
            .expect("read");
            assert_eq!(got, b"more");
        }

        #[derive(Default)]
        struct LateState {
            in_recv: bool,
            waiting: bool,
            aborts: usize,
            closed: bool,
        }

        #[derive(Default)]
        struct Late {
            state: Mutex<LateState>,
            changed: Condvar,
        }

        impl Late {
            /// Wait, within the `Quick` budget, until `done` holds.
            fn wait(&self, what: &str, done: impl Fn(&LateState) -> bool) {
                let deadline = Instant::now() + quick();
                let mut s = self.state.lock();
                while !done(&s) {
                    let timed_out = self.changed.wait_until(&mut s, deadline).timed_out();
                    assert!(
                        !timed_out || done(&s),
                        "timed out: {what} ({} aborts)",
                        s.aborts
                    );
                }
            }
        }

        /// A socket whose `recv` blocks only after the first abort has come
        /// and gone, and which a later abort wakes — the order in which a
        /// Windows `CancelIoEx` misses a receive that has not started yet.
        struct LateReceiver(Arc<Late>);

        impl BlockingSocket for LateReceiver {
            fn recv(&self, _: &mut [u8]) -> io::Result<usize> {
                let late = &*self.0;
                let mut s = late.state.lock();
                s.in_recv = true;
                late.changed.notify_all();
                while s.aborts == 0 {
                    late.changed.wait(&mut s);
                }
                s.waiting = true;
                while s.waiting {
                    late.changed.wait(&mut s);
                }
                Err(io::ErrorKind::Interrupted.into())
            }
            fn send(&self, data: &[u8]) -> io::Result<usize> {
                Ok(data.len())
            }
            fn shutdown_write(&self) -> io::Result<()> {
                Ok(())
            }
            /// Unbounded on purpose: only an abort may wake this receive.
            fn bound_recv(&self, _: Duration) -> io::Result<()> {
                Ok(())
            }
            fn abort_recv(&self) -> io::Result<()> {
                let late = &*self.0;
                let mut s = late.state.lock();
                s.aborts += 1;
                s.waiting = false;
                late.changed.notify_all();
                Ok(())
            }
        }

        impl Drop for LateReceiver {
            fn drop(&mut self) {
                let late = &*self.0;
                late.state.lock().closed = true;
                late.changed.notify_all();
            }
        }

        /// An abort that misses the receive is repeated until the receiving
        /// thread has finished, and only then is the socket closed.
        #[test]
        fn an_abort_that_misses_the_receive_is_repeated() {
            let rt = runtime();
            let late = Arc::new(Late::default());
            let stream =
                bridge(LateReceiver(Arc::clone(&late)), rt.handle().clone()).expect("bridge");
            late.wait("the receiving thread enters recv", |s| s.in_recv);
            drop(stream);
            late.wait("the socket is closed", |s| s.closed);
            assert!(
                late.state.lock().aborts >= 2,
                "the first abort was bound to miss"
            );
        }

        #[derive(Default)]
        struct WakeState {
            bound: Option<Duration>,
            in_recv: bool,
            timeouts: usize,
            aborted: bool,
            incoming: Vec<u8>,
            peer_closed: bool,
        }

        #[derive(Default)]
        struct Wake {
            state: Mutex<WakeState>,
            changed: Condvar,
        }

        impl Wake {
            /// Wait, within the `Quick` budget, until `done` holds, and keep
            /// the lock so the caller acts on exactly that state.
            fn wait(
                &self,
                what: &str,
                done: impl Fn(&WakeState) -> bool,
            ) -> MutexGuard<'_, WakeState> {
                let deadline = Instant::now() + quick();
                let mut s = self.state.lock();
                while !done(&s) {
                    let timed_out = self.changed.wait_until(&mut s, deadline).timed_out();
                    assert!(!timed_out || done(&s), "timed out: {what}");
                }
                s
            }
        }

        /// A socket that loses the wake-up for the peer's end of stream when
        /// it lands on a `recv` already waiting — the race seen on Windows
        /// `AF_UNIX`. The end is recorded, so a `recv` that starts afterwards
        /// returns 0; the waiting one is never told. Its `recv` honours
        /// `bound_recv` as `SO_RCVTIMEO` does.
        struct LosesEofWakeup(Arc<Wake>);

        impl BlockingSocket for LosesEofWakeup {
            fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
                let wake = &*self.0;
                let mut s = wake.state.lock();
                if s.peer_closed && s.incoming.is_empty() {
                    return Ok(0);
                }
                let deadline = s.bound.map(|b| Instant::now() + b);
                s.in_recv = true;
                wake.changed.notify_all();
                let out = loop {
                    if !s.incoming.is_empty() {
                        let n = s.incoming.len().min(buf.len());
                        buf[..n].copy_from_slice(&s.incoming[..n]);
                        s.incoming.drain(..n);
                        break Ok(n);
                    }
                    if s.aborted {
                        s.aborted = false;
                        break Err(io::Error::other("the I/O operation has been aborted"));
                    }
                    // `peer_closed` is deliberately not looked at here: the
                    // wake-up for it was lost.
                    match deadline {
                        Some(d) if Instant::now() >= d => {
                            s.timeouts += 1;
                            break Err(io::ErrorKind::TimedOut.into());
                        }
                        Some(d) => {
                            let _ = wake.changed.wait_until(&mut s, d);
                        }
                        None => wake.changed.wait(&mut s),
                    }
                };
                s.in_recv = false;
                wake.changed.notify_all();
                out
            }
            fn send(&self, data: &[u8]) -> io::Result<usize> {
                Ok(data.len())
            }
            fn shutdown_write(&self) -> io::Result<()> {
                Ok(())
            }
            fn bound_recv(&self, every: Duration) -> io::Result<()> {
                self.0.state.lock().bound = Some(every);
                Ok(())
            }
            fn abort_recv(&self) -> io::Result<()> {
                let wake = &*self.0;
                let mut s = wake.state.lock();
                if s.in_recv {
                    s.aborted = true;
                    wake.changed.notify_all();
                }
                Ok(())
            }
        }

        /// The Windows hang in `concurrent_connections_survive_many_rounds`:
        /// the server's receive was re-armed just as the client's half-close
        /// arrived, and the end of the request never reached it. The bridge
        /// bounds every `recv`, takes a receive that comes back empty-handed
        /// for "nothing yet" rather than for the end, and so finds the end of
        /// the stream on its next look.
        #[test]
        fn an_end_of_stream_whose_wakeup_is_lost_is_still_seen() {
            let rt = runtime();
            let wake = Arc::new(Wake::default());
            let mut stream =
                bridge(LosesEofWakeup(Arc::clone(&wake)), rt.handle().clone()).expect("bridge");

            // A quiet spell longer than one bounded receive is not the end.
            let mut s = wake.wait("a receive looks again after coming back empty", |s| {
                s.timeouts >= 1 && s.in_recv
            });
            s.incoming.extend_from_slice(b"request");
            wake.changed.notify_all();
            drop(s);

            // The request is taken and the next receive is waiting: the end
            // of the stream lands on it, and its wake-up is lost.
            let mut s = wake.wait("the request is taken and the receive re-armed", |s| {
                s.incoming.is_empty() && s.in_recv
            });
            s.peer_closed = true;
            drop(s);

            let mut got = Vec::new();
            rt.block_on(async {
                tokio::time::timeout(quick(), stream.read_to_end(&mut got)).await
            })
            .expect("the end of the request within budget")
            .expect("read");
            assert_eq!(got, b"request");
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::{
        io::{self, Read},
        net::Shutdown,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use socket2::{Domain, SockAddr, Socket, Type};
    use tokio::{
        runtime::Handle,
        sync::{Mutex, mpsc, oneshot},
    };

    use super::bridge::{BlockingSocket, BridgedStream, bridge};

    pub(super) type Stream = BridgedStream;

    /// How long dropping a listener waits for its accept thread to close the
    /// listening socket. It is woken by a connection of our own and normally
    /// takes no time; the bound only keeps a runtime thread from blocking
    /// forever in `Drop` if that ever stops being true.
    const ACCEPT_STOP_WAIT: Duration = Duration::from_secs(5);

    fn new_socket() -> io::Result<Socket> {
        Socket::new(Domain::UNIX, Type::STREAM, None)
    }

    impl BlockingSocket for Socket {
        // Measured on windows-latest: an AF_UNIX socket's half-close, accepted
        // (#168) or connecting (#169, a client's request), never reached the
        // peer while a `recv` was blocked on it.
        const HALF_CLOSE_NEEDS_QUIET_RECEIVE: bool = true;

        fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
            let mut this = self;
            this.read(buf)
        }

        fn send(&self, data: &[u8]) -> io::Result<usize> {
            Socket::send(self, data)
        }

        fn shutdown_write(&self) -> io::Result<()> {
            self.shutdown(Shutdown::Write)
        }

        /// `SO_RCVTIMEO`: a timed-out `recv` fails with `WSAETIMEDOUT`
        /// (`TimedOut`). Effective because `socket2` opens sockets with
        /// `WSA_FLAG_OVERLAPPED`; Winsock ends the wait by cancelling the
        /// receive, the same path as [`BlockingSocket::abort_recv`].
        fn bound_recv(&self, every: Duration) -> io::Result<()> {
            self.set_read_timeout(Some(every))
        }

        /// `CancelIoEx` with no `OVERLAPPED` cancels every operation pending
        /// on the socket, issued by any thread; a blocking receive is one, as
        /// `socket2` opens sockets for overlapped I/O. It does not affect a
        /// receive that starts afterwards, which the bridge allows for.
        fn abort_recv(&self) -> io::Result<()> {
            use std::os::windows::io::AsRawSocket;
            use windows_sys::Win32::{Foundation::ERROR_NOT_FOUND, System::IO::CancelIoEx};

            // SAFETY: the handle is this live socket's own, borrowed for the
            // duration of the call; a null OVERLAPPED is documented to mean
            // "all I/O on the handle".
            let cancelled =
                unsafe { CancelIoEx(self.as_raw_socket() as usize as _, std::ptr::null()) };
            if cancelled != 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            // Nothing was pending: the receiving thread is between calls.
            if e.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
                Ok(())
            } else {
                Err(e)
            }
        }
    }

    /// A blocking listener whose accept loop runs on its own thread and hands
    /// each connection to [`Listener::accept`] through a channel.
    pub(super) struct Listener {
        conns: Mutex<mpsc::Receiver<io::Result<Socket>>>,
        closing: Arc<AtomicBool>,
        path: PathBuf,
        /// Disconnected once the accept thread has closed the listening socket.
        stopped: parking_lot::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl Listener {
        pub(super) fn bind(path: &Path) -> io::Result<Self> {
            let socket = new_socket()?;
            socket.bind(&SockAddr::unix(path)?)?;
            socket.listen(128)?;
            let (tx, rx) = mpsc::channel(16);
            let (stopped_tx, stopped) = std::sync::mpsc::channel::<()>();
            let closing = Arc::new(AtomicBool::new(false));
            let stop = Arc::clone(&closing);
            std::thread::Builder::new()
                .name("ahma-local-accept".into())
                .spawn(move || {
                    loop {
                        let accepted = socket.accept().map(|(conn, _)| conn);
                        if stop.load(Ordering::Acquire) || tx.blocking_send(accepted).is_err() {
                            break;
                        }
                    }
                    // Close the listening socket, then say so.
                    drop(socket);
                    drop(stopped_tx);
                })?;
            Ok(Self {
                conns: Mutex::new(rx),
                closing,
                path: path.to_path_buf(),
                stopped: parking_lot::Mutex::new(stopped),
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
        /// `closing` and drop the socket. The wait for that is bounded: this
        /// runs on whichever thread drops the listener, often a runtime worker.
        fn drop(&mut self) {
            self.closing.store(true, Ordering::Release);
            self.conns.get_mut().close();
            let woke = new_socket()
                .and_then(|s| s.connect(&SockAddr::unix(&self.path)?))
                .is_ok();
            if woke
                && let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                    self.stopped.get_mut().recv_timeout(ACCEPT_STOP_WAIT)
            {
                tracing::debug!("local socket: the accept thread did not stop in time");
            }
        }
    }

    /// The blocking `connect` runs on a thread of its own rather than through
    /// `spawn_blocking`: a runtime waits for its blocking tasks when it shuts
    /// down, so one connect that never returned would hang the shutdown with
    /// it. A caller that gives up on the connect (a `timeout`) leaves only a
    /// detached thread behind.
    pub(super) async fn connect(path: &Path) -> io::Result<Stream> {
        let addr = SockAddr::unix(path)?;
        let (done, connected) = oneshot::channel();
        std::thread::Builder::new()
            .name("ahma-local-connect".into())
            .spawn(move || {
                let _ = done.send(new_socket().and_then(|conn| {
                    conn.connect(&addr)?;
                    Ok(conn)
                }));
            })?;
        let conn = connected
            .await
            .map_err(|_| io::Error::other("local socket connect thread stopped"))??;
        bridge(conn, Handle::current())
    }

    #[cfg(test)]
    mod tests {
        use super::super::bridge::{half_close, send_all};
        use super::*;
        use crate::timeouts::{TestTimeouts, TimeoutCategory};

        /// The OS-level window the bridge has to survive, made deterministic:
        /// a client sends and half-closes before the listener calls `accept`
        /// at all. Once accepted, the server must read the whole request and
        /// then EOF. A half-close lost here is the hang that
        /// `serves_concurrent_connections` hit on Windows CI.
        #[test]
        fn a_request_half_closed_before_accept_reaches_the_server() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("t.sock");
            let listener = new_socket().unwrap();
            listener.bind(&SockAddr::unix(&path).unwrap()).unwrap();
            listener.listen(8).unwrap();

            let addr = SockAddr::unix(&path).unwrap();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let client = std::thread::spawn(move || {
                let conn = new_socket()?;
                conn.connect(&addr)?;
                send_all(&conn, b"early", || false)?;
                half_close(&conn, || false)?;
                let _ = done_tx.send(());
                // Hand the socket back so it stays open until the server has
                // read: the server must see a half-close, not a closed socket.
                Ok::<_, io::Error>(conn)
            });
            // Let the client finish before anything is accepted. If Windows
            // makes it wait for the accept (WSAENOTCONN), it is still retrying
            // when this times out, and the accept below releases it.
            let _ = done_rx.recv_timeout(TestTimeouts::short_delay());

            let (server, _) = listener.accept().expect("accept");
            server
                .set_read_timeout(Some(TestTimeouts::get(TimeoutCategory::Quick)))
                .unwrap();
            let mut got = Vec::new();
            (&server)
                .read_to_end(&mut got)
                .expect("the request and then EOF");
            assert_eq!(got, b"early");
            let _conn = client
                .join()
                .expect("client thread")
                .expect("client send and half-close");
        }
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
        run_concurrent_connections().await;
    }

    /// The Windows hang, repeated until it would show: in a CI experiment
    /// (#169) round 140 of 150 stalled. A client's half-close made while its
    /// own receive was blocked in `recv` never reached the server, which
    /// waited for the end of a request that never came. Each round is a few
    /// milliseconds, so the whole repeat stays well inside the budget.
    #[cfg(windows)]
    #[test]
    fn concurrent_connections_survive_many_rounds() {
        for _ in 0..150 {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_concurrent_connections());
        }
    }

    async fn run_concurrent_connections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sock");
        let listener = LocalListener::bind(&path).expect("bind");
        const N: usize = 8;

        // On Windows this once timed out at the 20s `Quick` budget, was given
        // the 120s `ToolCall` budget on the theory that the runner was slow,
        // and then hung for all 120s: it was never slowness. A client that
        // half-closed while its connection still waited in the listener's
        // queue lost that half-close, so the server waited for the end of a
        // request that never came. Keep the short budget: a hang should fail
        // fast. `clients_that_half_close_before_the_server_accepts_are_served`
        // forces that window instead of hoping to hit it.
        within("concurrent connections", async {
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

    /// Every client connects, writes and half-closes before the server
    /// accepts a single connection. On Unix the kernel queues all of that; on
    /// Windows a client's send and half-close can reach a connection the
    /// listener has not accepted yet, and must not be lost (SPEC R-HUB.2).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clients_that_half_close_before_the_server_accepts_are_served() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sock");
        let listener = LocalListener::bind(&path).expect("bind");
        const N: usize = 16;

        within("half-close before accept", async {
            let (sent_tx, mut sent_rx) = tokio::sync::mpsc::channel(N);
            let clients: Vec<_> = (0..N)
                .map(|i| {
                    let path = path.clone();
                    let sent = sent_tx.clone();
                    tokio::spawn(async move {
                        let msg = format!("early-{i}");
                        let mut client = LocalStream::connect(&path).await.expect("connect");
                        client
                            .write_all(msg.as_bytes())
                            .await
                            .expect("client write");
                        client.shutdown().await.expect("client shutdown");
                        sent.send(()).await.expect("report the request sent");
                        let mut answer = Vec::new();
                        client.read_to_end(&mut answer).await.expect("client read");
                        assert_eq!(answer, msg.to_ascii_uppercase().into_bytes());
                    })
                })
                .collect();
            drop(sent_tx);
            for _ in 0..N {
                sent_rx
                    .recv()
                    .await
                    .expect("a client stopped before sending");
            }
            // Only now does the server accept anything.
            for _ in 0..N {
                serve_upper(&listener).await;
            }
            for client in clients {
                client.await.expect("client task");
            }
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

    #[test]
    fn a_call_refused_as_not_connected_is_retried_until_it_succeeds() {
        let mut calls = 0;
        let result = not_connected::retry(
            not_connected::GRACE,
            || false,
            || {
                calls += 1;
                if calls < 4 {
                    Err(io::Error::from(io::ErrorKind::NotConnected))
                } else {
                    Ok(calls)
                }
            },
        );
        assert_eq!(result.expect("retried to success"), 4);
    }

    #[test]
    fn any_other_error_is_returned_at_once() {
        let mut calls = 0;
        let result: io::Result<()> = not_connected::retry(
            not_connected::GRACE,
            || false,
            || {
                calls += 1;
                Err(io::ErrorKind::BrokenPipe.into())
            },
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(calls, 1);
    }

    /// Once the peer has been heard to finish, "not connected" is final.
    #[test]
    fn not_connected_is_final_once_the_caller_gives_up() {
        let mut calls = 0;
        let result: io::Result<()> = not_connected::retry(
            not_connected::GRACE,
            || true,
            || {
                calls += 1;
                Err(io::ErrorKind::NotConnected.into())
            },
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotConnected);
        assert_eq!(calls, 1);
    }

    #[test]
    fn not_connected_is_final_after_the_grace_period() {
        let mut calls = 0;
        let result: io::Result<()> = not_connected::retry(
            std::time::Duration::ZERO,
            || false,
            || {
                calls += 1;
                Err(io::ErrorKind::NotConnected.into())
            },
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotConnected);
        assert_eq!(calls, 1);
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
