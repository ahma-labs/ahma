//! How each command gets its link broker (SPEC R-LINK.1): the per-command
//! lease, the helper link that `BROWSER` and `GH_BROWSER` name, and whether
//! this process serves link brokers at all (which the sandbox profile reads,
//! so a command may reach the broker directory whatever `[sandbox] ssh_agent`
//! says).

use super::broker::LinkBroker;
use super::consent::RecordedLinkConsent;
use super::helper::{HELPER_NAME, SOCKET_ENV};
use crate::credentials::host::{SocketLease, agent_dir};
use crate::egress::EgressGrants;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The suffix that tells a link broker's socket from the SSH key broker's.
pub const SOCKET_SUFFIX: &str = ".link.sock";

static SERVED: AtomicBool = AtomicBool::new(false);

/// Record that this process serves its commands a link broker, before any
/// command's sandbox profile is built: from then on the profile lets a
/// command connect to a socket in the broker directory (SPEC R-LINK.1).
pub fn mark_served() {
    SERVED.store(true, Ordering::SeqCst);
}

/// Whether this process serves its commands a link broker.
pub fn served() -> bool {
    SERVED.load(Ordering::SeqCst)
}

/// Gives each command its link broker.
pub trait LinkBrokerFactory: Send + Sync + std::fmt::Debug {
    /// A lease for a command running in `working_dir` for `command`, or
    /// `None` when none can be served (the command then keeps whatever
    /// `BROWSER` it had, and a browser launch fails in the sandbox as before).
    fn lease_for_command(&self, working_dir: &Path, command: Option<&str>) -> Option<LinkLease>;
}

/// One command's link broker, served on its own socket for as long as this
/// lives.
pub struct LinkLease {
    socket: SocketLease,
    broker: Arc<LinkBroker>,
    helper: PathBuf,
}

impl std::fmt::Debug for LinkLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkLease")
            .field("socket", &self.socket.socket())
            .field("helper", &self.helper)
            .finish_non_exhaustive()
    }
}

impl LinkLease {
    /// Serve `broker` on a new `<8 hex>.link.sock` in `dir`, pointing the
    /// command at `helper`.
    pub fn start(broker: Arc<LinkBroker>, dir: &Path, helper: PathBuf) -> std::io::Result<Self> {
        let socket = SocketLease::start(Arc::clone(&broker), dir, SOCKET_SUFFIX)?;
        Ok(Self {
            socket,
            broker,
            helper,
        })
    }

    pub fn socket(&self) -> &Path {
        self.socket.socket()
    }

    /// What the command gets (SPEC R-LINK.1): `BROWSER` and `GH_BROWSER`
    /// naming the helper (gh reads `GH_BROWSER` before its own `browser`
    /// setting and before `BROWSER`), and its socket.
    pub fn env(&self) -> [(&'static str, OsString); 3] {
        [
            ("BROWSER", self.helper.clone().into_os_string()),
            ("GH_BROWSER", self.helper.clone().into_os_string()),
            (SOCKET_ENV, self.socket().as_os_str().to_os_string()),
        ]
    }

    /// The lines of the links it refused this command, once each.
    pub fn refusal_lines(&self) -> Vec<String> {
        self.broker.refusal_lines()
    }

    pub fn broker(&self) -> &LinkBroker {
        &self.broker
    }
}

/// The helper for this binary: `<agent dir>/bin-<12 hex>/ahma-open-url`, a
/// symlink to the running executable. `None` when it cannot be made.
pub fn helper_link() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = dunce::canonicalize(&exe).unwrap_or(exe);
    match helper_link_in(&agent_dir()?, &exe) {
        Ok(link) => Some(link),
        Err(e) => {
            tracing::warn!("no link helper for {}: {e}", exe.display());
            None
        }
    }
}

/// The helper link for `exe` under `dir`, made if missing or pointing
/// elsewhere. One directory per executable, named by a digest of its path, so
/// two ahma builds running at once (a hub, a development build) never repoint
/// each other's link. Replaced by rename, so a command never finds it absent.
pub fn helper_link_in(dir: &Path, exe: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let digest = ahma_common::digest::sha256_hex(exe.as_os_str().as_encoded_bytes());
    let bin = dir.join(format!("bin-{}", &digest[..12]));
    std::fs::create_dir_all(&bin)?;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700))?;
    let link = bin.join(HELPER_NAME);
    if std::fs::read_link(&link).is_ok_and(|to| to == exe) {
        return Ok(link);
    }
    let tmp = bin.join(format!(".{HELPER_NAME}.{:08x}", rand::random::<u32>()));
    std::os::unix::fs::symlink(exe, &tmp)?;
    if let Err(e) = std::fs::rename(&tmp, &link) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(link)
}

/// The link broker factory for a terminal hook: consent from saved grants
/// only ([`RecordedLinkConsent`]), keyed on the hook's workspace.
#[derive(Debug, Clone)]
pub struct RecordedLinks {
    settings_file: Option<PathBuf>,
    scopes: Vec<PathBuf>,
    /// The workspace grants are keyed on, when the caller already knows it
    /// (the terminal hook's); else each command's own.
    workspace: Option<PathBuf>,
    /// The effective egress allow list while network restriction is on.
    egress: Option<EgressGrants>,
}

impl RecordedLinks {
    pub fn new(settings_file: Option<PathBuf>, scopes: Vec<PathBuf>) -> Self {
        Self {
            settings_file,
            scopes,
            workspace: None,
            egress: None,
        }
    }

    /// Key every command's grants on `workspace`.
    pub fn for_workspace(mut self, workspace: PathBuf) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Network restriction is on: "any site" covers only what `egress`
    /// allows (SPEC R-LINK.5).
    pub fn with_egress(mut self, egress: Option<EgressGrants>) -> Self {
        self.egress = egress;
        self
    }
}

impl LinkBrokerFactory for RecordedLinks {
    fn lease_for_command(&self, working_dir: &Path, _command: Option<&str>) -> Option<LinkLease> {
        let workspace = self.workspace.clone().unwrap_or_else(|| {
            crate::adapter::workspace_queue::workspace_key(working_dir, &self.scopes)
        });
        let consent = Arc::new(RecordedLinkConsent::new(
            workspace,
            self.settings_file.clone(),
            self.egress.clone(),
        ));
        let broker = LinkBroker::new(consent, super::opener::for_this_process());
        let helper = helper_link()?;
        match LinkLease::start(broker, &agent_dir()?, helper) {
            Ok(lease) => Some(lease),
            Err(e) => {
                tracing::warn!("no link broker for this command: {e}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC R-LINK.1: the helper is the running binary under the helper's
    /// name, one directory per executable; making it again is a no-op, and a
    /// link left pointing elsewhere is repointed.
    #[test]
    fn the_helper_link_names_the_running_binary_and_survives_a_second_version() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (Path::new("/opt/ahma-a/ahma"), Path::new("/opt/ahma-b/ahma"));
        let link_a = helper_link_in(dir.path(), a).unwrap();
        let link_b = helper_link_in(dir.path(), b).unwrap();
        assert_eq!(link_a.file_name().unwrap(), HELPER_NAME);
        assert_ne!(link_a.parent(), link_b.parent(), "one directory per binary");
        assert_eq!(std::fs::read_link(&link_a).unwrap(), a);
        assert_eq!(std::fs::read_link(&link_b).unwrap(), b);
        assert_eq!(helper_link_in(dir.path(), a).unwrap(), link_a);

        std::fs::remove_file(&link_a).unwrap();
        std::os::unix::fs::symlink(b, &link_a).unwrap();
        helper_link_in(dir.path(), a).unwrap();
        assert_eq!(std::fs::read_link(&link_a).unwrap(), a);
        let stray: Vec<_> = std::fs::read_dir(link_a.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != HELPER_NAME)
            .collect();
        assert!(stray.is_empty(), "no temporary link left: {stray:?}");
    }

    /// SPEC R-LINK.1: a lease hands the command the helper as `BROWSER` and
    /// `GH_BROWSER` and its own `.link.sock`, removed when the lease ends.
    #[tokio::test]
    async fn a_lease_points_the_command_at_the_helper_and_its_own_socket() {
        #[derive(Debug)]
        struct No;
        #[async_trait::async_trait]
        impl super::super::consent::LinkConsent for No {
            async fn decide(
                &self,
                _: &super::super::policy::LinkTarget,
                _: bool,
            ) -> super::super::consent::LinkAnswer {
                super::super::consent::LinkAnswer::Refuse("no".into())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let broker = LinkBroker::new(Arc::new(No), super::super::opener::for_this_process());
        let helper = dir.path().join(HELPER_NAME);
        let socket = {
            let lease = LinkLease::start(broker, dir.path(), helper.clone()).unwrap();
            let env = lease.env();
            assert_eq!(env[0], ("BROWSER", helper.clone().into_os_string()));
            assert_eq!(env[1], ("GH_BROWSER", helper.clone().into_os_string()));
            assert_eq!(env[2].0, SOCKET_ENV);
            assert!(lease.socket().to_string_lossy().ends_with(SOCKET_SUFFIX));
            assert!(lease.socket().exists());
            lease.socket().to_path_buf()
        };
        assert!(!socket.exists());
    }
}
