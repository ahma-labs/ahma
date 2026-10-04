//! The SSH key broker (SPEC R-CRED.1–R-CRED.6, R-CRED.10): an SSH agent a
//! sandboxed command talks to through its `SSH_AUTH_SOCK`, served outside the
//! sandbox. It lists the keys the human could sign with, and signs only what
//! a [`SignConsent`] allows — for a destination the server proved — either
//! through the human's own agent or with a key file read on the host. It
//! refuses everything else and records what it did.

use super::keys::{self, FileKey};
use super::known_hosts;
use super::proto::{self, Request};
use super::purpose::{self, Purpose, SessionBind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

/// How long the human's agent has to list its keys.
const UPSTREAM_LIST_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
/// How long it has to sign: an agent may ask the human first (`ssh-add -c`,
/// 1Password, Secretive), which takes a person's time.
const UPSTREAM_SIGN_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Where a signature would go, as the human is asked about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// A server the connection is bound to (SPEC R-CRED.4). `names` are the
    /// `known_hosts` names that hold its key; empty when none do.
    Host {
        host_key_fingerprint: String,
        names: Vec<String>,
    },
    /// A login on a connection that sent no verifiable binding (R-CRED.5).
    Unbound { user: String },
    /// An `ssh-keygen -Y sign` signature in a namespace (`git`, `file`).
    Namespace(String),
    /// Data whose purpose cannot be read.
    Unknown,
}

impl Destination {
    /// Whether an answer about this destination may be remembered beyond the
    /// session (SPEC R-CRED.3, R-CRED.5).
    pub fn may_persist(&self) -> bool {
        matches!(
            self,
            Destination::Host { names, .. } if !names.is_empty()
        ) || matches!(self, Destination::Namespace(_))
    }
}

/// One signature a command asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignRequest {
    pub key_fingerprint: String,
    pub key_comment: String,
    pub destination: Destination,
}

/// What the broker does with a sign request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Refused, with the one line the command's output gets (SPEC R-PERM.9).
    Deny(String),
}

/// Who decides whether a signature may be given. Implementations ask a human
/// through the R-PERM.3 ladder, or answer from recorded grants; the broker
/// itself never decides yes.
#[async_trait::async_trait]
pub trait SignConsent: Send + Sync {
    async fn decide(&self, request: &SignRequest) -> Decision;
}

/// What the broker did, for the audit log and the command's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerEvent {
    Signed {
        request: SignRequest,
        via_agent: bool,
    },
    Refused {
        request: SignRequest,
        why: String,
    },
    /// A message type or extension the broker never serves (R-CRED.2), or a
    /// forwarded connection (R-CRED.6).
    RefusedOperation(String),
}

/// Where the broker finds keys: the human's agent, and key files.
#[derive(Debug, Clone)]
pub struct KeySources {
    /// The human's own agent socket (SPEC R-CRED.7), if one answered.
    pub upstream: Option<PathBuf>,
    /// The directory holding key files (`~/.ssh`).
    pub ssh_dir: PathBuf,
    /// `known_hosts` files, in the order ssh reads them.
    pub known_hosts: Vec<PathBuf>,
}

impl KeySources {
    /// The human's usual locations under `home`, with `upstream` as found.
    pub fn for_home(home: &Path, upstream: Option<PathBuf>) -> Self {
        let ssh = home.join(".ssh");
        Self {
            upstream,
            known_hosts: vec![ssh.join("known_hosts"), ssh.join("known_hosts2")],
            ssh_dir: ssh,
        }
    }
}

/// The SSH agent served to one sandboxed command.
pub struct Broker {
    sources: KeySources,
    consent: Arc<dyn SignConsent>,
    events: parking_lot::Mutex<Vec<BrokerEvent>>,
    /// Told of each event as it happens, so a running operation can say a
    /// signature was refused while the command still runs (SPEC R-CRED.10).
    observer: parking_lot::Mutex<Option<EventObserver>>,
}

/// Called with each [`BrokerEvent`] the moment the broker records it.
pub type EventObserver = Arc<dyn Fn(&BrokerEvent) + Send + Sync>;

impl Broker {
    pub fn new(sources: KeySources, consent: Arc<dyn SignConsent>) -> Arc<Self> {
        Arc::new(Self {
            sources,
            consent,
            events: parking_lot::Mutex::new(Vec::new()),
            observer: parking_lot::Mutex::new(None),
        })
    }

    /// Everything the broker did so far, oldest first.
    pub fn events(&self) -> Vec<BrokerEvent> {
        self.events.lock().clone()
    }

    /// Tell `observer` of every event from now on.
    pub fn observe(&self, observer: EventObserver) {
        *self.observer.lock() = Some(observer);
    }

    fn record(&self, event: BrokerEvent) {
        tracing::info!(?event, "ssh broker");
        let observer = self.observer.lock().clone();
        if let Some(observer) = observer {
            observer(&event);
        }
        self.events.lock().push(event);
    }

    /// Serve connections on `listener` until `cancel` fires, answering only
    /// commands this process spawned (SPEC R-CRED.1). A connection from
    /// anywhere else is closed unanswered and recorded.
    pub(super) async fn serve_filtered(
        self: Arc<Self>,
        listener: UnixListener,
        cancel: CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        if !super::host::admitted(&stream) {
                            self.record(BrokerEvent::RefusedOperation(
                                "a connection from a process ahma did not spawn".into(),
                            ));
                            continue;
                        }
                        let broker = Arc::clone(&self);
                        let cancel = cancel.clone();
                        tokio::spawn(async move {
                            tokio::select! {
                                _ = cancel.cancelled() => {}
                                _ = broker.connection(stream) => {}
                            }
                        });
                    }
                    Err(e) => {
                        tracing::debug!("ssh broker accept: {e}");
                        return;
                    }
                },
            }
        }
    }

    /// Serve every connection on `listener` until `cancel` fires: for tests
    /// that talk to a broker directly.
    #[cfg(test)]
    pub async fn serve(self: Arc<Self>, listener: UnixListener, cancel: CancellationToken) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let broker = Arc::clone(&self);
                        let cancel = cancel.clone();
                        tokio::spawn(async move {
                            tokio::select! {
                                _ = cancel.cancelled() => {}
                                _ = broker.connection(stream) => {}
                            }
                        });
                    }
                    Err(e) => {
                        tracing::debug!("ssh broker accept: {e}");
                        return;
                    }
                },
            }
        }
    }

    /// One client connection: requests in order, each answered.
    async fn connection(&self, mut stream: UnixStream) {
        let mut bound: Option<SessionBind> = None;
        while let Ok(Some(body)) = proto::read_message(&mut stream).await {
            let answer = match proto::parse_request(&body) {
                Some(Request::RequestIdentities) => self.identities().await,
                Some(Request::Sign {
                    key_blob,
                    data,
                    flags,
                }) => self.sign(bound.as_ref(), &key_blob, &data, flags).await,
                Some(Request::Extension { name, contents })
                    if name == "session-bind@openssh.com" =>
                {
                    // A binding that does not verify binds nothing: later
                    // logins on this connection count as unbound.
                    match SessionBind::parse(&contents) {
                        Some(b) => {
                            bound = Some(b);
                            proto::success()
                        }
                        None => proto::failure(),
                    }
                }
                Some(Request::Extension { name, .. }) => {
                    self.record(BrokerEvent::RefusedOperation(format!("extension {name}")));
                    proto::failure()
                }
                Some(Request::Other(kind)) => {
                    self.record(BrokerEvent::RefusedOperation(format!(
                        "message type {kind}"
                    )));
                    proto::failure()
                }
                None => proto::failure(),
            };
            if proto::write_message(&mut stream, &answer).await.is_err() {
                return;
            }
        }
    }

    /// The keys a command may ask to sign with: the human's agent's, then
    /// key files the host can sign with. Listing reveals public keys only.
    async fn identities(&self) -> Vec<u8> {
        let mut ids = self.upstream_identities().await.unwrap_or_default();
        for key in self.file_keys() {
            if !ids.iter().any(|(blob, _)| blob == key.public_blob()) {
                ids.push((key.public_blob().to_vec(), key.comment().to_string()));
            }
        }
        proto::identities_answer(&ids)
    }

    async fn sign(
        &self,
        bound: Option<&SessionBind>,
        key_blob: &[u8],
        data: &[u8],
        flags: u32,
    ) -> Vec<u8> {
        if bound.is_some_and(|b| b.forwarding) {
            self.record(BrokerEvent::RefusedOperation(
                "a signature on a forwarded connection".into(),
            ));
            return proto::failure();
        }
        let upstream_ids = self.upstream_identities().await.unwrap_or_default();
        let upstream_comment = upstream_ids
            .iter()
            .find(|(blob, _)| blob == key_blob)
            .map(|(_, c)| c.clone());
        let file_key = if upstream_comment.is_none() {
            self.file_keys()
                .into_iter()
                .find(|k| k.public_blob() == key_blob)
        } else {
            None
        };
        let comment = match (&upstream_comment, &file_key) {
            (Some(c), _) => c.clone(),
            (None, Some(k)) => k.comment().to_string(),
            (None, None) => return proto::failure(),
        };
        let request = SignRequest {
            key_fingerprint: keys::fingerprint(key_blob),
            key_comment: comment,
            destination: self.destination(bound, &purpose::classify(data)),
        };
        if let Decision::Deny(why) = self.consent.decide(&request).await {
            self.record(BrokerEvent::Refused { request, why });
            return proto::failure();
        }
        let signature = match &file_key {
            Some(key) => key.sign(data),
            None => self.upstream_sign(key_blob, data, flags).await,
        };
        match signature {
            Some(sig) => {
                self.record(BrokerEvent::Signed {
                    request,
                    via_agent: file_key.is_none(),
                });
                proto::sign_response(&sig)
            }
            None => {
                self.record(BrokerEvent::Refused {
                    request,
                    why: "the signature could not be made".into(),
                });
                proto::failure()
            }
        }
    }

    /// Where a signature for `purpose` goes, on a connection bound to `bound`.
    fn destination(&self, bound: Option<&SessionBind>, purpose: &Purpose) -> Destination {
        match purpose {
            Purpose::Userauth { user, .. } => match bound {
                Some(b) if b.covers(purpose) => {
                    let names = self.known_names(&b.host_key);
                    Destination::Host {
                        host_key_fingerprint: keys::fingerprint(&b.host_key),
                        names,
                    }
                }
                _ => Destination::Unbound { user: user.clone() },
            },
            Purpose::Sshsig { namespace } => Destination::Namespace(namespace.clone()),
            Purpose::Unknown => Destination::Unknown,
        }
    }

    fn known_names(&self, host_key: &[u8]) -> Vec<String> {
        let mut names = Vec::new();
        for file in &self.sources.known_hosts {
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            let found = known_hosts::lookup(&text, host_key, &[]);
            if found.revoked {
                return Vec::new();
            }
            for name in found.names {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }

    /// Unencrypted ed25519 key files in the key directory.
    fn file_keys(&self) -> Vec<FileKey> {
        std::fs::read_dir(&self.sources.ssh_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_none_or(|e| e != "pub"))
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .filter_map(|text| keys::parse_private_key(&text).ok())
            .collect()
    }

    async fn upstream_identities(&self) -> Option<Vec<(Vec<u8>, String)>> {
        let answer = self
            .upstream_call(&[proto::SSH_AGENTC_REQUEST_IDENTITIES], UPSTREAM_LIST_WAIT)
            .await?;
        proto::parse_identities_answer(&answer)
    }

    async fn upstream_sign(&self, key_blob: &[u8], data: &[u8], flags: u32) -> Option<Vec<u8>> {
        let answer = self
            .upstream_call(
                &proto::sign_request(key_blob, data, flags),
                UPSTREAM_SIGN_WAIT,
            )
            .await?;
        proto::parse_sign_response(&answer)
    }

    /// One request to the human's agent, bounded: an agent that does not
    /// answer is an agent with no keys, never a command that hangs.
    async fn upstream_call(&self, body: &[u8], wait: std::time::Duration) -> Option<Vec<u8>> {
        let path = self.sources.upstream.as_ref()?;
        tokio::time::timeout(wait, async {
            let mut stream = UnixStream::connect(path).await.ok()?;
            proto::write_message(&mut stream, body).await.ok()?;
            proto::read_message(&mut stream).await.ok()?
        })
        .await
        .ok()?
    }
}

#[cfg(test)]
mod tests {
    use super::super::keys::fixtures::{PLAIN_ED25519, PLAIN_ED25519_FP};
    use super::super::keys::{self, parse_private_key};
    use super::super::proto::{self, Writer};
    use super::*;

    /// Answers every request the same way, and remembers what it was asked.
    struct Stub {
        answer: Decision,
        asked: parking_lot::Mutex<Vec<SignRequest>>,
    }

    #[async_trait::async_trait]
    impl SignConsent for Stub {
        async fn decide(&self, request: &SignRequest) -> Decision {
            self.asked.lock().push(request.clone());
            self.answer.clone()
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        socket: PathBuf,
        broker: Arc<Broker>,
        consent: Arc<Stub>,
        cancel: CancellationToken,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    /// A broker over a home holding the fixture key, a `known_hosts` naming
    /// the fixture key as `github.com`'s host key, and no human agent.
    fn fixture(answer: Decision) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), PLAIN_ED25519).unwrap();
        std::fs::write(home.join(".ssh/id_ed25519.pub"), "ignored").unwrap();
        let line = format!(
            "github.com {}\n",
            super::super::keys::fixtures::PLAIN_ED25519_PUB
                .rsplit_once(' ')
                .unwrap()
                .0
        );
        std::fs::write(home.join(".ssh/known_hosts"), line).unwrap();
        let consent = Arc::new(Stub {
            answer,
            asked: parking_lot::Mutex::new(Vec::new()),
        });
        let broker = Broker::new(KeySources::for_home(&home, None), consent.clone());
        let socket = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let cancel = CancellationToken::new();
        tokio::spawn(Arc::clone(&broker).serve(listener, cancel.clone()));
        Fixture {
            _dir: dir,
            socket,
            broker,
            consent,
            cancel,
        }
    }

    async fn call(stream: &mut UnixStream, body: &[u8]) -> Vec<u8> {
        proto::write_message(stream, body).await.unwrap();
        proto::read_message(stream).await.unwrap().unwrap()
    }

    fn userauth(session: &[u8]) -> Vec<u8> {
        Writer::new()
            .string(session)
            .u8(50)
            .string(b"git")
            .string(b"ssh-connection")
            .string(b"publickey")
            .u8(1)
            .string(b"ssh-ed25519")
            .string(b"client-key")
            .finish()
    }

    /// The `session-bind@openssh.com` extension message, signed by the
    /// fixture key standing in for the server's host key.
    fn bind(session: &[u8], forwarding: bool) -> Vec<u8> {
        let host = parse_private_key(PLAIN_ED25519).unwrap();
        Writer::new()
            .u8(proto::SSH_AGENTC_EXTENSION)
            .string(b"session-bind@openssh.com")
            .string(host.public_blob())
            .string(session)
            .string(&host.sign(session).unwrap())
            .u8(forwarding as u8)
            .finish()
    }

    fn public_blob() -> Vec<u8> {
        parse_private_key(PLAIN_ED25519)
            .unwrap()
            .public_blob()
            .to_vec()
    }

    #[tokio::test]
    async fn it_lists_the_key_files_the_host_can_sign_with() {
        let f = fixture(Decision::Allow);
        let mut s = UnixStream::connect(&f.socket).await.unwrap();
        let ids = proto::parse_identities_answer(
            &call(&mut s, &[proto::SSH_AGENTC_REQUEST_IDENTITIES]).await,
        )
        .unwrap();
        assert_eq!(ids, vec![(public_blob(), "fixture@ahma".to_string())]);
        assert!(f.consent.asked.lock().is_empty(), "listing asks nobody");
    }

    #[tokio::test]
    async fn a_bound_login_is_asked_about_by_host_and_signed_when_allowed() {
        let f = fixture(Decision::Allow);
        let mut s = UnixStream::connect(&f.socket).await.unwrap();
        let session = b"session-0123456789abcdef";
        assert_eq!(call(&mut s, &bind(session, false)).await, proto::success());
        let data = userauth(session);
        let answer = call(&mut s, &proto::sign_request(&public_blob(), &data, 0)).await;
        let sig = proto::parse_sign_response(&answer).expect("signed");
        assert!(keys::verify(&public_blob(), &data, &sig));

        let asked = f.consent.asked.lock().clone();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].key_fingerprint, PLAIN_ED25519_FP);
        assert_eq!(
            asked[0].destination,
            Destination::Host {
                host_key_fingerprint: PLAIN_ED25519_FP.into(),
                names: vec!["github.com".into()],
            }
        );
        assert!(asked[0].destination.may_persist());
        assert!(matches!(
            f.broker.events().as_slice(),
            [BrokerEvent::Signed {
                via_agent: false,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn a_refusal_signs_nothing_and_is_recorded() {
        let f = fixture(Decision::Deny("nobody approved it".into()));
        let mut s = UnixStream::connect(&f.socket).await.unwrap();
        let answer = call(
            &mut s,
            &proto::sign_request(&public_blob(), &userauth(b"sid"), 0),
        )
        .await;
        assert_eq!(answer, proto::failure());
        let asked = f.consent.asked.lock().clone();
        assert_eq!(
            asked[0].destination,
            Destination::Unbound { user: "git".into() },
            "no binding: the destination is not known"
        );
        assert!(!asked[0].destination.may_persist());
        assert!(matches!(
            f.broker.events().as_slice(),
            [BrokerEvent::Refused { why, .. }] if why == "nobody approved it"
        ));
    }

    #[tokio::test]
    async fn a_login_for_another_session_is_not_the_bound_host() {
        let f = fixture(Decision::Allow);
        let mut s = UnixStream::connect(&f.socket).await.unwrap();
        call(&mut s, &bind(b"session-A", false)).await;
        call(
            &mut s,
            &proto::sign_request(&public_blob(), &userauth(b"session-B"), 0),
        )
        .await;
        assert!(matches!(
            f.consent.asked.lock()[0].destination,
            Destination::Unbound { .. }
        ));
    }

    #[tokio::test]
    async fn forwarding_and_key_management_are_refused_without_asking() {
        let f = fixture(Decision::Allow);
        let mut s = UnixStream::connect(&f.socket).await.unwrap();
        call(&mut s, &bind(b"sid", true)).await;
        let answer = call(
            &mut s,
            &proto::sign_request(&public_blob(), &userauth(b"sid"), 0),
        )
        .await;
        assert_eq!(answer, proto::failure(), "a forwarded connection");
        // ADD_IDENTITY, REMOVE_ALL_IDENTITIES, LOCK, and an unknown extension.
        for body in [
            vec![17u8, 0, 0, 0, 0],
            vec![19u8],
            vec![22u8, 0, 0, 0, 0],
            Writer::new()
                .u8(proto::SSH_AGENTC_EXTENSION)
                .string(b"restrict-destination-v00@openssh.com")
                .finish(),
        ] {
            assert_eq!(call(&mut s, &body).await, proto::failure(), "{body:?}");
        }
        assert!(f.consent.asked.lock().is_empty());
        assert_eq!(f.broker.events().len(), 5);
    }

    /// A key the human's agent holds is listed from it and signed by it: the
    /// broker reads no file for it. (The agent here is a second broker that
    /// holds the fixture key as a file.)
    #[tokio::test]
    async fn a_key_in_the_humans_agent_is_signed_by_the_agent() {
        let agent = fixture(Decision::Allow);
        let empty = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(empty.path().join(".ssh")).unwrap();
        let consent = Arc::new(Stub {
            answer: Decision::Allow,
            asked: parking_lot::Mutex::new(Vec::new()),
        });
        let broker = Broker::new(
            KeySources::for_home(empty.path(), Some(agent.socket.clone())),
            consent.clone(),
        );
        let socket = empty.path().join("outer.sock");
        let cancel = CancellationToken::new();
        tokio::spawn(
            Arc::clone(&broker).serve(UnixListener::bind(&socket).unwrap(), cancel.clone()),
        );

        let mut s = UnixStream::connect(&socket).await.unwrap();
        let ids = proto::parse_identities_answer(
            &call(&mut s, &[proto::SSH_AGENTC_REQUEST_IDENTITIES]).await,
        )
        .unwrap();
        assert_eq!(ids, vec![(public_blob(), "fixture@ahma".to_string())]);
        let data = userauth(b"sid");
        let sig = proto::parse_sign_response(
            &call(&mut s, &proto::sign_request(&public_blob(), &data, 0)).await,
        )
        .expect("signed through the agent");
        assert!(keys::verify(&public_blob(), &data, &sig));
        assert!(matches!(
            broker.events().as_slice(),
            [BrokerEvent::Signed {
                via_agent: true,
                ..
            }]
        ));
        assert_eq!(consent.asked.lock().len(), 1, "the broker still asks first");
        cancel.cancel();
    }

    /// OpenSSH's own client signs through the broker: `ssh-keygen -Y sign`
    /// given only the public key uses `SSH_AUTH_SOCK`, and `ssh-keygen -Y
    /// verify` accepts the result. Skipped where `ssh-keygen` is not installed.
    #[tokio::test]
    async fn openssh_signs_through_the_broker() {
        if std::process::Command::new("ssh-keygen")
            .arg("-?")
            .output()
            .is_err()
        {
            eprintln!("skipped: ssh-keygen is not installed");
            return;
        }
        let f = fixture(Decision::Allow);
        let work = tempfile::tempdir().unwrap();
        let public = work.path().join("key.pub");
        std::fs::write(
            &public,
            format!("{}\n", super::super::keys::fixtures::PLAIN_ED25519_PUB),
        )
        .unwrap();
        let message = work.path().join("message");
        std::fs::write(&message, "signed through the broker\n").unwrap();
        let socket = f.socket.clone();
        let sign = tokio::task::spawn_blocking({
            let (public, message) = (public.clone(), message.clone());
            move || {
                std::process::Command::new("ssh-keygen")
                    .args(["-Y", "sign", "-n", "file", "-f"])
                    .arg(&public)
                    .arg(&message)
                    .env("SSH_AUTH_SOCK", &socket)
                    .output()
                    .unwrap()
            }
        })
        .await
        .unwrap();
        assert!(
            sign.status.success(),
            "ssh-keygen -Y sign: {}",
            String::from_utf8_lossy(&sign.stderr)
        );
        let signers = work.path().join("allowed_signers");
        std::fs::write(
            &signers,
            format!(
                "me@ahma {}\n",
                super::super::keys::fixtures::PLAIN_ED25519_PUB
            ),
        )
        .unwrap();
        let verify = std::process::Command::new("ssh-keygen")
            .args(["-Y", "verify", "-n", "file", "-I", "me@ahma", "-f"])
            .arg(&signers)
            .arg("-s")
            .arg(work.path().join("message.sig"))
            .stdin(std::fs::File::open(&message).unwrap())
            .output()
            .unwrap();
        assert!(
            verify.status.success(),
            "ssh-keygen -Y verify: {}",
            String::from_utf8_lossy(&verify.stderr)
        );
        assert_eq!(
            f.consent.asked.lock()[0].destination,
            Destination::Namespace("file".into())
        );
    }

    #[tokio::test]
    async fn an_unknown_key_or_a_forged_binding_gets_nothing() {
        let f = fixture(Decision::Allow);
        let mut s = UnixStream::connect(&f.socket).await.unwrap();
        let other = Writer::new()
            .string(b"ssh-ed25519")
            .string(&[9u8; 32])
            .finish();
        assert_eq!(
            call(&mut s, &proto::sign_request(&other, b"data", 0)).await,
            proto::failure()
        );
        let mut forged = bind(b"sid", false);
        let last = forged.len() - 2;
        forged[last] ^= 1;
        assert_eq!(call(&mut s, &forged).await, proto::failure());
        assert!(f.consent.asked.lock().is_empty());
    }
}
