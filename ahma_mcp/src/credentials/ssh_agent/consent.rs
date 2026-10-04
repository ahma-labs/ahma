//! Who says yes to a signature, and how each command gets its broker.
//!
//! [`RecordedConsent`] answers from the grants a human already made — a
//! session grant from a dialog, or an `always`/lease grant in the settings
//! file (SPEC R-CRED.3). Anything else is refused at once, with one line
//! saying how to allow it (R-CRED.10), and remembered so the harness's own
//! dialog asks before the next command (R-PERM.10). A command never waits on
//! a human mid-handshake here; the dialog comes between commands.

use super::broker::{Broker, Decision, Destination, KeySources, SignConsent, SignRequest};
use super::host::{BrokerLease, agent_dir};
use ahma_common::harness_asks::{self, SshRefusal};
use ahma_common::ssh_sign::{self, HOST_PREFIX, SSHSIG_PREFIX};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Gives each command the broker it talks to (SPEC R-CRED.1).
pub trait BrokerFactory: Send + Sync + std::fmt::Debug {
    /// A broker for a command running in `working_dir`, or `None` when none
    /// can be served (the command then keeps the human's own agent socket).
    fn lease(&self, working_dir: &Path) -> Option<BrokerLease>;
}

/// The grant key for a destination: `host:SHA256:…` or `sshsig:<namespace>`.
/// `None` for a destination nothing may be granted for: an unverified login
/// or data whose purpose cannot be read (R-CRED.3, R-CRED.5).
pub fn destination_key(destination: &Destination) -> Option<(String, String)> {
    match destination {
        Destination::Host {
            host_key_fingerprint,
            names,
        } if !names.is_empty() => Some((
            format!("{HOST_PREFIX}{host_key_fingerprint}"),
            names.join(", "),
        )),
        Destination::Namespace(ns) => {
            Some((format!("{SSHSIG_PREFIX}{ns}"), format!("`{ns}` signatures")))
        }
        _ => None,
    }
}

/// The one line a refused signature leaves in the command's output.
pub fn refusal_line(request: &SignRequest) -> String {
    let key = if request.key_comment.is_empty() {
        request.key_fingerprint.clone()
    } else {
        format!("{} ({})", request.key_fingerprint, request.key_comment)
    };
    match destination_key(&request.destination) {
        Some((destination, label)) => format!(
            "Blocked until a human approves: ahma's SSH key broker did not sign for {label} \
             with key {key} in this workspace. In Claude Code, ahma asks before the next \
             command here; elsewhere a human runs `ahma permissions grant ssh-sign \"{} for \
             {destination}\"` (add `--for 24h` for a lease). Then re-run.",
            request.key_fingerprint
        ),
        None => match &request.destination {
            Destination::Host {
                host_key_fingerprint,
                ..
            } => format!(
                "Blocked: the server's host key {host_key_fingerprint} is in no known_hosts \
                 entry, so ahma cannot say which server key {key} would sign in to. Connect \
                 once from your own terminal to add it, then re-run."
            ),
            Destination::Unbound { .. } => format!(
                "Blocked: ssh asked to sign a login with key {key} without proving which \
                 server it is connected to (no session binding: OpenSSH older than 8.9, or \
                 a library). ahma signs only for a server the connection proves."
            ),
            _ => format!(
                "Blocked: a program asked to sign data with key {key} whose purpose ahma \
                 cannot read; it signs logins and `ssh-keygen -Y` signatures only."
            ),
        },
    }
}

/// Consent from recorded grants; a refusal is remembered for the dialog.
#[derive(Debug)]
pub struct RecordedConsent {
    workspace: PathBuf,
    settings_file: Option<PathBuf>,
    harness_pid: Option<u32>,
}

impl RecordedConsent {
    pub fn new(
        workspace: PathBuf,
        settings_file: Option<PathBuf>,
        harness_pid: Option<u32>,
    ) -> Self {
        Self {
            workspace,
            settings_file,
            harness_pid,
        }
    }

    fn grants(&self, now: u64) -> Vec<ssh_sign::SshSignGrant> {
        let mut grants = self
            .settings_file
            .as_ref()
            .and_then(|f| ahma_common::config::AhmaSettings::load_from_result(f).ok())
            .map(|s| s.sandbox.ssh_sign)
            .unwrap_or_default();
        if let Some(dir) = ssh_sign::session_dir() {
            grants.extend(ssh_sign::active_sessions(
                &dir,
                now,
                &crate::sandbox::pid_alive,
            ));
        }
        grants
    }
}

#[async_trait::async_trait]
impl SignConsent for RecordedConsent {
    async fn decide(&self, request: &SignRequest) -> Decision {
        let Some((destination, label)) = destination_key(&request.destination) else {
            return Decision::Deny(refusal_line(request));
        };
        let now = ahma_common::session_grants::now_secs();
        if ssh_sign::allowed(
            &self.grants(now),
            &request.key_fingerprint,
            &destination,
            &self.workspace,
            now,
        ) {
            return Decision::Allow;
        }
        if let Some(dir) = harness_asks::default_dir() {
            let refusal = SshRefusal {
                key: request.key_fingerprint.clone(),
                key_comment: request.key_comment.clone(),
                destination,
                label,
                at: now,
                harness_pid: self.harness_pid,
            };
            if let Err(e) = harness_asks::record_ssh_refusal(&dir, &self.workspace, refusal) {
                tracing::debug!("ssh refusal not recorded: {e:#}");
            }
        }
        Decision::Deny(refusal_line(request))
    }
}

/// The broker factory that answers from recorded grants.
#[derive(Debug, Clone)]
pub struct RecordedBrokers {
    home: PathBuf,
    /// The human's own agent (SPEC R-CRED.7).
    upstream: Option<PathBuf>,
    settings_file: Option<PathBuf>,
    harness_pid: Option<u32>,
    scopes: Vec<PathBuf>,
    /// The workspace grants and questions are keyed on, when the caller
    /// already knows it (the terminal hook's); else each command's own.
    workspace: Option<PathBuf>,
}

impl RecordedBrokers {
    /// Key every command's grants and questions on `workspace`.
    pub fn for_workspace(mut self, workspace: PathBuf) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Brokers for this user, signing through `upstream` when it is a live
    /// agent socket.
    pub fn new(
        home: PathBuf,
        upstream: Option<PathBuf>,
        settings_file: Option<PathBuf>,
        harness_pid: Option<u32>,
        scopes: Vec<PathBuf>,
    ) -> Self {
        Self {
            home,
            upstream: upstream.filter(|p| is_socket(p)),
            settings_file,
            harness_pid,
            scopes,
            workspace: None,
        }
    }
}

fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket())
}

impl BrokerFactory for RecordedBrokers {
    fn lease(&self, working_dir: &Path) -> Option<BrokerLease> {
        let workspace = self.workspace.clone().unwrap_or_else(|| {
            crate::adapter::workspace_queue::workspace_key(working_dir, &self.scopes)
        });
        let consent = Arc::new(RecordedConsent::new(
            workspace,
            self.settings_file.clone(),
            self.harness_pid,
        ));
        let broker = Broker::new(
            KeySources::for_home(&self.home, self.upstream.clone()),
            consent,
        );
        match BrokerLease::start(broker, &agent_dir()?) {
            Ok(lease) => Some(lease),
            Err(e) => {
                tracing::warn!("no SSH key broker for this command: {e}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(destination: Destination) -> SignRequest {
        SignRequest {
            key_fingerprint: "SHA256:key".into(),
            key_comment: "me@laptop".into(),
            destination,
        }
    }

    fn github() -> Destination {
        Destination::Host {
            host_key_fingerprint: "SHA256:host".into(),
            names: vec!["github.com".into()],
        }
    }

    #[tokio::test]
    async fn a_recorded_grant_allows_and_anything_else_is_refused_and_remembered() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let settings = tmp.path().join("settings.toml");
        let consent = RecordedConsent::new(ws.clone(), Some(settings.clone()), Some(42));

        let refused = consent.decide(&request(github())).await;
        let Decision::Deny(line) = refused else {
            panic!("no grant yet: {refused:?}")
        };
        assert!(line.starts_with("Blocked until a human approves"), "{line}");
        assert!(
            line.contains("github.com") && line.contains("me@laptop"),
            "{line}"
        );
        assert!(
            line.contains("ahma permissions grant ssh-sign \"SHA256:key for host:SHA256:host\""),
            "{line}"
        );
        let dir = harness_asks::default_dir().unwrap();
        let asks = harness_asks::load(&dir, &ws, ahma_common::session_grants::now_secs());
        assert_eq!(asks.ssh_refusals.len(), 1, "remembered for the dialog");
        assert_eq!(asks.ssh_refusals[0].label, "github.com");

        ssh_sign::persist(
            &settings,
            ssh_sign::SshSignGrant {
                key: "SHA256:key".into(),
                destination: "host:SHA256:host".into(),
                label: "github.com".into(),
                workspace: ws.clone(),
                granted_at: ahma_common::session_grants::now_secs(),
                expires_at: None,
                granted_by: Some("test".into()),
                owner_pid: None,
            },
            "test",
        )
        .unwrap();
        assert_eq!(consent.decide(&request(github())).await, Decision::Allow);
        assert!(
            matches!(
                consent
                    .decide(&request(Destination::Host {
                        host_key_fingerprint: "SHA256:other".into(),
                        names: vec!["evil.example".into()],
                    }))
                    .await,
                Decision::Deny(_)
            ),
            "the grant names one server"
        );
    }

    #[tokio::test]
    async fn what_cannot_be_granted_is_refused_with_why_and_not_remembered() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws2");
        std::fs::create_dir_all(&ws).unwrap();
        let consent = RecordedConsent::new(ws.clone(), None, None);
        for (destination, says) in [
            (
                Destination::Unbound { user: "git".into() },
                "no session binding",
            ),
            (Destination::Unknown, "purpose ahma"),
            (
                Destination::Host {
                    host_key_fingerprint: "SHA256:h".into(),
                    names: vec![],
                },
                "known_hosts",
            ),
        ] {
            let Decision::Deny(line) = consent.decide(&request(destination)).await else {
                panic!("refused")
            };
            assert!(line.contains(says), "{line}");
        }
        let dir = harness_asks::default_dir().unwrap();
        let asks = harness_asks::load(&dir, &ws, ahma_common::session_grants::now_secs());
        assert!(
            asks.ssh_refusals.is_empty(),
            "nothing a human could approve"
        );
    }
}
