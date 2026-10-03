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
                    super::trace::ev("notconn.retry", pause.as_millis() as i64);
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(MAX_PAUSE);
                }
                result => {
                    if let Err(e) = &result
                        && e.kind() == io::ErrorKind::NotConnected
                    {
                        super::trace::ev("notconn.final", super::trace::code(e));
                    }
                    return result;
                }
            }
        }
    }
}

/// EXPERIMENT ONLY — NEVER MERGE. A process-wide ring buffer of Windows bridge
/// events, one connection id per bridged socket, dumped by [`trace::Watchdog`]
/// when a test overruns. Under nextest each test is its own process, so the
/// ring holds that test's events only.
#[cfg(any(windows, test))]
#[allow(dead_code)]
pub(crate) mod trace {
    use std::{
        cell::Cell,
        collections::{BTreeMap, VecDeque},
        fmt::Write as _,
        io,
        sync::{
            OnceLock,
            atomic::{AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    };

    const CAP: usize = 16 * 1024;

    #[derive(Clone, Copy)]
    struct Event {
        us: u64,
        conn: u64,
        thread: &'static str,
        what: &'static str,
        n: i64,
    }

    static RING: parking_lot::Mutex<VecDeque<Event>> = parking_lot::const_mutex(VecDeque::new());
    static START: OnceLock<Instant> = OnceLock::new();
    static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

    thread_local! {
        static CONN: Cell<u64> = const { Cell::new(0) };
        static ROLE: Cell<&'static str> = const { Cell::new("caller") };
    }

    /// A fresh connection id (0 is "no connection": accept/connect threads).
    pub(crate) fn new_conn() -> u64 {
        NEXT_CONN.fetch_add(1, Ordering::Relaxed)
    }

    /// Tag this OS thread: every later [`ev`] on it is filed under `conn`.
    pub(crate) fn enter(conn: u64, thread: &'static str) {
        CONN.set(conn);
        ROLE.set(thread);
    }

    pub(crate) fn ev(what: &'static str, n: i64) {
        ev_for(CONN.get(), what, n);
    }

    pub(crate) fn ev_for(conn: u64, what: &'static str, n: i64) {
        let us = START.get_or_init(Instant::now).elapsed().as_micros() as u64;
        let thread = ROLE.get();
        let mut ring = RING.lock();
        if ring.len() == CAP {
            ring.pop_front();
        }
        ring.push_back(Event {
            us,
            conn,
            thread,
            what,
            n,
        });
    }

    /// The OS error code, or -1 when there is none.
    pub(crate) fn code(e: &io::Error) -> i64 {
        e.raw_os_error().map_or(-1, i64::from)
    }

    /// Every event, grouped by connection in time order, then each
    /// connection's last event per thread: where each one is parked now.
    pub(crate) fn dump() -> String {
        let now = START.get_or_init(Instant::now).elapsed().as_micros() as u64;
        let ring = RING.lock().clone();
        let mut by_conn: BTreeMap<u64, Vec<Event>> = BTreeMap::new();
        for e in &ring {
            by_conn.entry(e.conn).or_default().push(*e);
        }
        let mut out = format!(
            "--- local_socket trace: {} events (cap {CAP}), now = {}us ---\n",
            ring.len(),
            now
        );
        for (conn, events) in &by_conn {
            let _ = writeln!(out, "conn {conn}:");
            for e in events {
                let _ = writeln!(
                    out,
                    "  {:>10}us {:<7} {:<22} {}",
                    e.us, e.thread, e.what, e.n
                );
            }
        }
        let _ = writeln!(out, "--- last event per (conn, thread) ---");
        let mut last: BTreeMap<(u64, &'static str), Event> = BTreeMap::new();
        for e in &ring {
            last.insert((e.conn, e.thread), *e);
        }
        for ((conn, thread), e) in &last {
            let _ = writeln!(
                out,
                "  conn {conn:<3} {thread:<7} {:<22} {:<6} {}us ago",
                e.what,
                e.n,
                now.saturating_sub(e.us)
            );
        }
        out
    }

    /// Dumps the trace from a plain OS thread if the guarded test is still
    /// running after `budget`, and again after twice that — so it fires even
    /// when every runtime worker is stuck, and the two dumps tell a stall from
    /// slow progress. Dropping it (the test finished) stops it silently.
    pub(crate) struct Watchdog {
        _done: std::sync::mpsc::Sender<()>,
    }

    impl Watchdog {
        pub(crate) fn start(name: &'static str, budget: Duration) -> Self {
            let (done, finished) = std::sync::mpsc::channel::<()>();
            std::thread::Builder::new()
                .name("ahma-trace-watchdog".into())
                .spawn(move || {
                    for round in 1..=2u32 {
                        match finished.recv_timeout(budget) {
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => eprintln!(
                                "=== WATCHDOG {name}: still running after {:?}\n{}",
                                budget * round,
                                dump()
                            ),
                            _ => return,
                        }
                    }
                })
                .expect("watchdog thread");
            ev_for(0, "watchdog.armed", budget.as_millis() as i64);
            Self { _done: done }
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
        thread::JoinHandle,
    };

    use socket2::{Domain, SockAddr, Socket, Type};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        runtime::Handle,
        sync::{Mutex, mpsc},
    };

    use super::{not_connected, trace};

    pub(super) type Stream = Traced;

    /// EXPERIMENT: the caller's end, recording every poll it sees.
    pub(super) struct Traced {
        io: tokio::io::DuplexStream,
        conn: u64,
    }

    impl Drop for Traced {
        fn drop(&mut self) {
            trace::ev_for(self.conn, "caller.drop", 0);
        }
    }

    fn polled<T>(conn: u64, what: &'static str, p: &std::task::Poll<io::Result<T>>, n: i64) {
        use std::task::Poll;
        match p {
            Poll::Pending => trace::ev_for(conn, what, -2),
            Poll::Ready(Ok(_)) => trace::ev_for(conn, what, n),
            Poll::Ready(Err(e)) => trace::ev_for(conn, what, -1000 - trace::code(e).abs()),
        }
    }

    impl tokio::io::AsyncRead for Traced {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            let before = buf.filled().len();
            let p = std::pin::Pin::new(&mut self.io).poll_read(cx, buf);
            let n = (buf.filled().len() - before) as i64;
            polled(self.conn, "caller.read", &p, n);
            p
        }
    }

    impl tokio::io::AsyncWrite for Traced {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            let p = std::pin::Pin::new(&mut self.io).poll_write(cx, buf);
            let n = match &p {
                std::task::Poll::Ready(Ok(n)) => *n as i64,
                _ => 0,
            };
            polled(self.conn, "caller.write", &p, n);
            p
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::pin::Pin::new(&mut self.io).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            let p = std::pin::Pin::new(&mut self.io).poll_shutdown(cx);
            polled(self.conn, "caller.shutdown", &p, 0);
            p
        }
    }

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
                    trace::enter(0, "accept");
                    loop {
                        trace::ev("accept.enter", 0);
                        let accepted = socket.accept().map(|(conn, _)| conn);
                        match &accepted {
                            Ok(_) => trace::ev("accept.ok", 0),
                            Err(e) => trace::ev("accept.err", trace::code(e)),
                        }
                        if stop.load(Ordering::Acquire) || tx.blocking_send(accepted).is_err() {
                            trace::ev("accept.stop", 0);
                            break;
                        }
                        trace::ev("accept.handed", 0);
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
            trace::ev_for(0, "accept.dequeued", 0);
            bridge(conn, Handle::current(), 0)
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
            trace::ev_for(0, "listener.drop.wake", 0);
            let woke = new_socket()
                .and_then(|s| s.connect(&SockAddr::unix(&self.path)?))
                .is_ok();
            trace::ev_for(0, "listener.drop.woke", i64::from(woke));
            if woke && let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            trace::ev_for(0, "listener.drop.joined", 0);
        }
    }

    pub(super) async fn connect(path: &Path) -> io::Result<Stream> {
        let addr = SockAddr::unix(path)?;
        trace::ev_for(0, "connect.spawn", 0);
        let conn = tokio::task::spawn_blocking(move || {
            trace::enter(0, "connect");
            // A long gap after `connect.spawn` is a starved blocking pool.
            trace::ev("connect.running", 0);
            let conn = new_socket()?;
            let r = conn.connect(&addr);
            match &r {
                Ok(()) => trace::ev("connect.ok", 0),
                Err(e) => trace::ev("connect.err", trace::code(e)),
            }
            r?;
            Ok::<_, io::Error>(conn)
        })
        .await
        .map_err(io::Error::other)??;
        trace::ev_for(0, "connect.joined", 0);
        bridge(conn, Handle::current(), 1)
    }

    /// Send all of `data`, retrying a send refused because the listener has
    /// not accepted the connection yet. `rx_done` is set once the peer has
    /// been heard to finish or fail, after which that refusal is final.
    fn send_all(conn: &Socket, mut data: &[u8], rx_done: &AtomicBool) -> io::Result<()> {
        while !data.is_empty() {
            trace::ev("send.enter", data.len() as i64);
            let sent = not_connected::retry(
                not_connected::GRACE,
                || rx_done.load(Ordering::Acquire),
                || conn.send(data),
            );
            match &sent {
                Ok(n) => trace::ev("send.ret", *n as i64),
                Err(e) => trace::ev("send.err", trace::code(e)),
            }
            match sent {
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
    fn half_close(conn: &Socket, rx_done: &AtomicBool) -> io::Result<()> {
        trace::ev("halfclose.enter", 0);
        let r = not_connected::retry(
            not_connected::GRACE,
            || rx_done.load(Ordering::Acquire),
            || conn.shutdown(Shutdown::Write),
        );
        match &r {
            Ok(()) => trace::ev("halfclose.ok", 0),
            Err(e) => trace::ev("halfclose.err", trace::code(e)),
        }
        r
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
    /// `role`: 0 = accepted (server side), 1 = connected (client side).
    fn bridge(conn: Socket, rt: Handle, role: i64) -> io::Result<Stream> {
        let id = trace::new_conn();
        trace::ev_for(id, "bridge", role);
        let (caller, ours) = tokio::io::duplex(BUF);
        let (mut from_caller, mut to_caller) = tokio::io::split(ours);
        let conn = Arc::new(conn);
        // Set once the peer's EOF or an error has been read: the connection was
        // accepted (or is dead), so a not-connected refusal is no longer transient.
        let rx_done = Arc::new(AtomicBool::new(false));

        let rx_conn = Arc::clone(&conn);
        let rx_rt = rt.clone();
        let rx_finished = Arc::clone(&rx_done);
        std::thread::Builder::new()
            .name("ahma-local-rx".into())
            .spawn(move || {
                trace::enter(id, "rx");
                trace::ev("rx.start", 0);
                let mut buf = vec![0u8; BUF];
                loop {
                    trace::ev("recv.enter", 0);
                    let read = not_connected::retry(
                        not_connected::GRACE,
                        || false,
                        || (&*rx_conn).read(&mut buf),
                    );
                    match &read {
                        Ok(n) => trace::ev("recv.ret", *n as i64),
                        Err(e) => trace::ev("recv.err", trace::code(e)),
                    }
                    let n = match read {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            tracing::debug!(error = %e, "local socket: receive failed");
                            break;
                        }
                    };
                    trace::ev("fwd.block_on.enter", n as i64);
                    let fwd = rx_rt.block_on(to_caller.write_all(&buf[..n]));
                    trace::ev("fwd.block_on.leave", i64::from(fwd.is_ok()));
                    if fwd.is_err() {
                        // The caller dropped the stream.
                        let _ = rx_conn.shutdown(Shutdown::Read);
                        break;
                    }
                }
                rx_finished.store(true, Ordering::Release);
                trace::ev("rx.eof-to-caller.enter", 0);
                let _ = rx_rt.block_on(to_caller.shutdown());
                trace::ev("rx.exit", 0);
            })?;

        std::thread::Builder::new()
            .name("ahma-local-tx".into())
            .spawn(move || {
                trace::enter(id, "tx");
                trace::ev("tx.start", 0);
                let mut buf = vec![0u8; BUF];
                let relayed = loop {
                    // Enter/leave of block_on, and every poll of the read
                    // inside it: a `caller.write`/`caller.shutdown` that is
                    // not followed by a `read.poll` here is a lost wake; a
                    // poll that came and went `pending` is an empty pipe.
                    trace::ev("block_on.enter", 0);
                    let read = rt.block_on(async {
                        let mut read = std::pin::pin!(from_caller.read(&mut buf));
                        std::future::poll_fn(|cx| {
                            let p = read.as_mut().poll(cx);
                            let n = match &p {
                                std::task::Poll::Ready(Ok(n)) => *n as i64,
                                _ => 0,
                            };
                            polled(id, "read.poll", &p, n);
                            p
                        })
                        .await
                    });
                    trace::ev("block_on.leave", read.as_ref().map_or(-1, |n| *n as i64));
                    match read {
                        Ok(0) => break Ok(()),
                        Ok(n) => {
                            if let Err(e) = send_all(&conn, &buf[..n], &rx_done) {
                                break Err(e);
                            }
                        }
                        Err(e) => break Err(e),
                    }
                };
                let closed = match relayed {
                    Ok(()) => half_close(&conn, &rx_done),
                    Err(e) => {
                        tracing::debug!(error = %e, "local socket: send failed");
                        conn.shutdown(Shutdown::Write)
                    }
                };
                if let Err(e) = closed {
                    tracing::debug!(error = %e, "local socket: half-close failed");
                }
                trace::ev("tx.exit", 0);
            })?;

        Ok(Traced {
            io: caller,
            conn: id,
        })
    }

    #[cfg(test)]
    mod tests {
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
                let rx_done = AtomicBool::new(false);
                send_all(&conn, b"early", &rx_done)?;
                half_close(&conn, &rx_done)?;
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
        // EXPERIMENT: a plain-thread watchdog at half the budget (fires even
        // if the runtime is wedged), and a final dump if the timeout fires.
        let budget = TestTimeouts::get(category);
        let _watchdog = trace::Watchdog::start("local_socket test", budget / 2);
        tokio::time::timeout(budget, fut).await.unwrap_or_else(|_| {
            eprintln!("=== TIMEOUT {what}\n{}", trace::dump());
            panic!("timed out: {what}")
        })
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

    /// EXPERIMENT (never merged): the intermittent Windows hang shows up about
    /// once in a dozen CI runs, so run the same scenario many times in one
    /// test, each on a fresh two-worker runtime, with the trace watchdog armed.
    #[cfg(windows)]
    #[test]
    fn concurrent_connections_repeated_for_the_hang() {
        for round in 0..150 {
            eprintln!("concurrent round {round}");
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
                for i in 0..N {
                    conns.push(listener.accept().await.expect("accept"));
                    trace::ev_for(0, "test.server.accepted", i as i64);
                }
                // Answer in reverse order: no connection depends on another.
                for (i, mut conn) in conns.into_iter().enumerate().rev() {
                    let mut got = Vec::new();
                    trace::ev_for(0, "test.server.reading", i as i64);
                    conn.read_to_end(&mut got).await.expect("server read");
                    trace::ev_for(0, "test.server.read", got.len() as i64);
                    conn.write_all(&got.to_ascii_uppercase())
                        .await
                        .expect("server write");
                    conn.shutdown().await.expect("server shutdown");
                    trace::ev_for(0, "test.server.answered", i as i64);
                }
            };
            let clients = futures_join_all((0..N).map(|i| {
                let path = path.clone();
                async move {
                    let msg = format!("client-{i}");
                    trace::ev_for(0, "test.client.start", i as i64);
                    let answer = ask_upper(&path, msg.as_bytes()).await;
                    // Recorded before the assert: a wrong answer that panics a
                    // client task while the server waits looks like a hang.
                    trace::ev_for(0, "test.client.answer", answer.len() as i64);
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
