//! The link broker: the host side of one command's link lease (SPEC R-LINK).
//!
//! Served on the command's own [`SocketLease`](crate::credentials::host::SocketLease),
//! which admits only a command this process spawned (R-CRED.1), on the broker
//! thread (R-CRED.9). For each request it checks the argument once
//! ([`LinkTarget::parse`], R-LINK.2), counts it against the command's cap
//! (R-LINK.8), looks the name up so a public name that points at this machine
//! is treated as local (R-LINK.4), asks its [`LinkConsent`], and opens what it
//! checked through its [`Opener`] (R-LINK.3). Every open and refusal is in the
//! execution audit log (R-LINK.9), and each refusal is kept so the command's
//! result can carry it even when the tool that ran the helper hid its output.

use super::consent::{LinkAnswer, LinkConsent};
use super::helper::{self, LinkReply};
use super::opener::Opener;
use super::policy::{self, HostClass, LinkTarget, MAX_LINKS_PER_COMMAND};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// How long the broker waits for a name to resolve before treating it as
/// public (SPEC R-LINK.4: failure is public; the browser resolves it again).
const RESOLVE_WAIT: Duration = Duration::from_secs(2);

/// Whether a public name points at this machine or the local network.
#[async_trait::async_trait]
pub trait LocalResolver: Send + Sync + std::fmt::Debug {
    async fn resolves_local(&self, host: &str) -> bool;
}

/// The system resolver, bounded by a 2 s wait per name.
///
/// `getaddrinfo` runs on a blocking-pool thread, which on Linux may carry the
/// process's Landlock domain if it was created after `restrict_self`; it reads
/// only `/etc`, which every ruleset allows, and Landlock does not restrict
/// UDP, so the lookup works either way.
#[derive(Debug, Clone, Copy, Default)]
pub struct DnsResolver;

#[async_trait::async_trait]
impl LocalResolver for DnsResolver {
    async fn resolves_local(&self, host: &str) -> bool {
        match tokio::time::timeout(RESOLVE_WAIT, tokio::net::lookup_host((host, 443))).await {
            Ok(Ok(mut addrs)) => addrs.any(|a| policy::ip_is_local(a.ip())),
            _ => false,
        }
    }
}

/// Who is told, as it happens, what a command's link broker refused and
/// what it is asking a human while the command waits (SPEC R-LINK.6): on the
/// MCP path, an alert on the command's operation. Shared by the broker and
/// its consent, and set once the command's operation is known
/// ([`LinkLease::watch`](crate::links::host::LinkLease::watch)); until then,
/// and when nobody watches, nothing is told.
#[derive(Clone, Default)]
pub struct LinkWatch(Arc<Mutex<Option<LinkWatcher>>>);

/// What a [`LinkWatch`] tells: one line.
pub type LinkWatcher = Arc<dyn Fn(&str) + Send + Sync>;

impl std::fmt::Debug for LinkWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LinkWatch")
            .field(&self.0.lock().is_some())
            .finish()
    }
}

impl LinkWatch {
    /// Tell `watcher` from now on.
    pub fn set(&self, watcher: LinkWatcher) {
        *self.0.lock() = Some(watcher);
    }

    /// Tell whoever watches `line`.
    pub fn tell(&self, line: &str) {
        let watcher = self.0.lock().clone();
        if let Some(watcher) = watcher {
            watcher(line);
        }
    }
}

/// What the broker did, in order, for the command's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// Opened: the address as audited.
    Opened(String),
    /// Not opened: the line the command was given.
    Refused(String),
}

/// One command's link broker.
#[derive(Debug)]
pub struct LinkBroker {
    consent: Arc<dyn LinkConsent>,
    opener: Arc<dyn Opener>,
    resolver: Arc<dyn LocalResolver>,
    /// Links this command asked for, opened or not (SPEC R-LINK.8).
    asked: AtomicUsize,
    events: Mutex<Vec<LinkEvent>>,
    watch: LinkWatch,
}

impl LinkBroker {
    pub fn new(consent: Arc<dyn LinkConsent>, opener: Arc<dyn Opener>) -> Arc<Self> {
        Self::with_resolver(consent, opener, Arc::new(DnsResolver))
    }

    pub fn with_resolver(
        consent: Arc<dyn LinkConsent>,
        opener: Arc<dyn Opener>,
        resolver: Arc<dyn LocalResolver>,
    ) -> Arc<Self> {
        Self::watched(consent, opener, resolver, LinkWatch::default())
    }

    /// A broker that tells `watch` each refusal; its consent may hold the
    /// same watch, to say while it asks.
    pub fn watched(
        consent: Arc<dyn LinkConsent>,
        opener: Arc<dyn Opener>,
        resolver: Arc<dyn LocalResolver>,
        watch: LinkWatch,
    ) -> Arc<Self> {
        Arc::new(Self {
            consent,
            opener,
            resolver,
            asked: AtomicUsize::new(0),
            events: Mutex::new(Vec::new()),
            watch,
        })
    }

    /// Who is told what this broker refuses, as it refuses it.
    pub fn watch(&self) -> &LinkWatch {
        &self.watch
    }

    /// What the broker did for this command so far.
    pub fn events(&self) -> Vec<LinkEvent> {
        self.events.lock().clone()
    }

    /// The lines of the links it refused, once each.
    pub fn refusal_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for event in self.events.lock().iter() {
            if let LinkEvent::Refused(line) = event
                && !lines.contains(line)
            {
                lines.push(line.clone());
            }
        }
        lines
    }

    /// Answer one argument the helper forwarded.
    pub async fn handle(&self, raw: &str) -> LinkReply {
        let target = match LinkTarget::parse(raw) {
            Ok(target) => target,
            Err(why) => {
                crate::adapter::audit::record_link_refused(None, &why.what()).await;
                return self.refuse(policy::not_a_link_line(raw, &why));
            }
        };
        if self.asked.fetch_add(1, Ordering::SeqCst) >= MAX_LINKS_PER_COMMAND {
            let why = format!("more than {MAX_LINKS_PER_COMMAND} links in one command");
            crate::adapter::audit::record_link_refused(Some(&target.audit_form()), &why).await;
            return self.refuse(format!(
                "ahma did not open a link to {}: a command may open at most \
                 {MAX_LINKS_PER_COMMAND} links (SPEC R-LINK.8). You may show the human the \
                 address: {}",
                target.host(),
                target.shown()
            ));
        }
        let resolved_local = target.class() == HostClass::Public
            && target.host().parse::<std::net::Ipv4Addr>().is_err()
            && self.resolver.resolves_local(target.host()).await;
        match self.consent.decide(&target, resolved_local).await {
            LinkAnswer::Open => match self.opener.open(&target).await {
                Ok(()) => {
                    crate::adapter::audit::record_link_opened(&target.audit_form()).await;
                    self.events
                        .lock()
                        .push(LinkEvent::Opened(target.audit_form()));
                    LinkReply {
                        opened: true,
                        line: format!("ahma opened {} in the human's browser.", target.shown()),
                    }
                }
                Err(e) => {
                    crate::adapter::audit::record_link_refused(Some(&target.audit_form()), &e)
                        .await;
                    self.refuse(format!(
                        "ahma could not open the link ({e}); nothing was opened. You may show \
                         the human the address: {}",
                        target.shown()
                    ))
                }
            },
            LinkAnswer::Refuse(line) => {
                crate::adapter::audit::record_link_refused(
                    Some(&target.audit_form()),
                    "no grant covers it",
                )
                .await;
                self.refuse(line)
            }
        }
    }

    fn refuse(&self, line: String) -> LinkReply {
        self.events.lock().push(LinkEvent::Refused(line.clone()));
        self.watch.tell(&line);
        LinkReply {
            opened: false,
            line,
        }
    }

    /// Serve one connection: one request line, one reply line.
    async fn connection(&self, mut stream: UnixStream) {
        let mut buf = Vec::new();
        let limit = helper::MAX_REQUEST_BYTES as u64 + 1;
        if let Err(e) = (&mut stream).take(limit).read_to_end(&mut buf).await {
            tracing::debug!("link broker read: {e}");
            return;
        }
        let reply = match helper::parse_request(buf.trim_ascii_end()) {
            Ok(request) => self.handle(&request.url).await,
            Err(why) => self.refuse(format!("ahma's link broker refused a request: {why}")),
        };
        let Ok(mut line) = serde_json::to_vec(&reply) else {
            return;
        };
        line.push(b'\n');
        let _ = stream.write_all(&line).await;
        let _ = stream.shutdown().await;
    }
}

/// Served on a per-command [`SocketLease`](crate::credentials::host::SocketLease),
/// which admits only a command this process spawned (SPEC R-CRED.1).
impl crate::credentials::host::SocketService for LinkBroker {
    async fn serve_connection(self: Arc<Self>, stream: UnixStream) {
        self.connection(stream).await;
    }

    fn refused(&self) {
        tracing::warn!("link broker: refused a connection from a process ahma did not spawn");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::links::consent::RecordedLinkConsent;
    use crate::links::opener::RecordingOpener;

    /// Allows every link: the broker's own checks are what is under test.
    #[derive(Debug)]
    struct Yes;

    #[async_trait::async_trait]
    impl LinkConsent for Yes {
        async fn decide(&self, _: &LinkTarget, _: bool) -> LinkAnswer {
            LinkAnswer::Open
        }
    }

    #[derive(Debug)]
    struct Fixed(bool);

    #[async_trait::async_trait]
    impl LocalResolver for Fixed {
        async fn resolves_local(&self, _: &str) -> bool {
            self.0
        }
    }

    fn broker(
        consent: Arc<dyn LinkConsent>,
        local: bool,
    ) -> (Arc<LinkBroker>, tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("opened.jsonl");
        let opener = Arc::new(RecordingOpener { file: file.clone() });
        (
            LinkBroker::with_resolver(consent, opener, Arc::new(Fixed(local))),
            dir,
            file,
        )
    }

    fn opened(file: &std::path::Path) -> usize {
        std::fs::read_to_string(file)
            .map(|t| t.lines().count())
            .unwrap_or(0)
    }

    /// SPEC R-LINK.8: five links open, the sixth is refused with one line and
    /// nothing is opened for it.
    #[tokio::test]
    async fn a_sixth_link_is_refused() {
        let (broker, _dir, file) = broker(Arc::new(Yes), false);
        for i in 0..MAX_LINKS_PER_COMMAND {
            let reply = broker.handle(&format!("https://example.com/{i}")).await;
            assert!(reply.opened, "{}", reply.line);
        }
        let sixth = broker.handle("https://example.com/6").await;
        assert!(!sixth.opened);
        assert!(sixth.line.contains("at most 5 links"), "{}", sixth.line);
        assert_eq!(opened(&file), MAX_LINKS_PER_COMMAND);
        assert_eq!(broker.refusal_lines(), [sixth.line]);
    }

    /// SPEC R-LINK.4: a public name the broker finds pointing at this
    /// machine is local, so "any site" does not cover it.
    #[tokio::test]
    async fn a_name_resolving_to_loopback_is_local() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let settings = home.path().join(".ahma").join("settings.toml");
        ahma_common::open_url::persist_open_url_allow(
            &settings,
            "*",
            &ahma_common::open_url::OpenUrlBinding::AllRepos,
            &ahma_common::approver::ApproverSurface::Cli,
        )
        .unwrap();
        let consent = || {
            Arc::new(RecordedLinkConsent::new(
                "/w/a".into(),
                Some(settings.clone()),
                None,
            ))
        };
        let (local, _d1, f1) = broker(consent(), true);
        let reply = local.handle("https://127.0.0.1.nip.io/").await;
        assert!(!reply.opened, "{}", reply.line);
        assert_eq!(opened(&f1), 0);

        let (public, _d2, f2) = broker(consent(), false);
        assert!(public.handle("https://example.com/").await.opened);
        assert_eq!(opened(&f2), 1);
    }

    /// SPEC R-LINK.2: what `cargo doc --open` passes is a file; it is never
    /// asked about, never opened, and the line says whose it is to open.
    #[tokio::test]
    async fn a_file_is_refused_with_its_path() {
        let (broker, _dir, file) = broker(Arc::new(Yes), false);
        for raw in [
            "/w/a/target/doc/ahma/index.html",
            "file:///w/a/target/doc/ahma/index.html",
        ] {
            let reply = broker.handle(raw).await;
            assert!(!reply.opened);
            assert!(reply.line.contains("local file"), "{}", reply.line);
            assert!(reply.line.contains("/w/a/target/doc"), "{}", reply.line);
        }
        let userinfo = broker.handle("https://github.com@evil.example/").await;
        assert!(!userinfo.opened, "{}", userinfo.line);
        assert_eq!(opened(&file), 0);
    }

    /// What is opened is the parser's serialization, never the raw argument
    /// (SPEC R-LINK.2): an upper-case scheme and host arrive lower-cased.
    #[tokio::test]
    async fn the_serialization_is_opened_not_the_raw_argument() {
        let (broker, _dir, file) = broker(Arc::new(Yes), false);
        assert!(broker.handle("HTTPS://GitHub.COM/x").await.opened);
        let text = std::fs::read_to_string(file).unwrap();
        assert!(text.contains("\"url\":\"https://github.com/x\""), "{text}");
    }
}
