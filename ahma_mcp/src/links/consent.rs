//! Who says yes to a link (SPEC R-LINK.4–R-LINK.6).
//!
//! [`RecordedLinkConsent`] answers a hooked command from the grants a human
//! already made: `[open_url] allow` for every repo and this repo's
//! `[open_url] repos` entry, in the user's settings file, read afresh for
//! every link so a grant made while the command runs applies, and a site the
//! human allowed for this one run in the harness dialog or for this session
//! at another surface (the TUI, [`session_sites`]). Anything else is
//! refused at once with one line: a hooked command is never paused
//! (R-LINK.6). The refusal is recorded for the harness dialog, which asks
//! when the command runs again.

use super::policy::{self, Egress, LinkDecision, LinkGrants, LinkQuestion, LinkTarget};
use crate::egress::{EgressGrants, HostPattern};
use ahma_common::harness_asks::{self, CommandDigest, LinkRefusal};
use std::path::{Path, PathBuf};

/// The grants a hooked command's links are decided by (SPEC R-LINK.4): the
/// saved ones that apply in `workspace` (`[open_url] allow` and its
/// `[open_url] repos` entry, read from `settings_file` now), the sites a human
/// allowed for this run (`this_run`) and the sites allowed for this session
/// in it ([`session_sites`]). What the broker, the harness dialog and the TUI
/// all read, so none of them asks about what another would open.
///
/// Plain std I/O: this runs on the broker thread, and `tokio::fs` would hand
/// it to a blocking-pool thread that may be created after the process
/// restricted itself (SPEC R-CRED.9). Small files, as the SSH key broker's
/// consent reads them.
pub fn hook_grants(
    settings_file: Option<&Path>,
    workspace: &Path,
    mut this_run: Vec<HostPattern>,
) -> LinkGrants {
    let settings = settings_file
        .and_then(|f| ahma_common::config::AhmaSettings::load_from_result(f).ok())
        .map(|s| s.open_url)
        .unwrap_or_default();
    this_run.extend(session_sites(workspace));
    LinkGrants::from_settings(&settings, workspace, this_run, Vec::new())
}

/// The sites a human allowed "this site, this session" for hooked commands
/// working in `workspace`: the session grant files
/// ([`ahma_common::open_url::active_sessions`]) whose harness process still
/// lives and that apply in this repo: given in it or in a repo enclosing it
/// ([`ahma_common::open_url::OpenUrlSessionGrant::applies_in`], SPEC R-LINK.4).
pub fn session_sites(workspace: &Path) -> Vec<HostPattern> {
    let Some(dir) = ahma_common::open_url::session_dir() else {
        return Vec::new();
    };
    let now = ahma_common::session_grants::now_secs();
    ahma_common::open_url::active_sessions(&dir, now, &crate::sandbox::pid_alive)
        .into_iter()
        .filter(|g| g.applies_in(workspace))
        // Always one exact site: a session answer names the site asked about.
        .map(|g| HostPattern::Exact(g.site.to_ascii_lowercase()))
        .collect()
}

/// What the broker does with one link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkAnswer {
    Open,
    /// Not opened; the one line the command gets.
    Refuse(String),
}

/// Decides each link a command asks to open.
#[async_trait::async_trait]
pub trait LinkConsent: Send + Sync + std::fmt::Debug {
    /// `resolved_local`: the broker looked the name up and it names this
    /// machine or the local network (SPEC R-LINK.4).
    async fn decide(&self, target: &LinkTarget, resolved_local: bool) -> LinkAnswer;
}

/// Consent from saved grants and a harness-dialog yes (the terminal hook's).
#[derive(Debug, Clone)]
pub struct RecordedLinkConsent {
    /// The repo `[open_url] repos` is keyed on: its canonical root. Also the
    /// workspace its refusals are recorded under for the harness dialog.
    workspace: PathBuf,
    /// The user's settings file: `[open_url]` is security tier (R-CFG2.1).
    settings_file: Option<PathBuf>,
    /// The effective egress allow list while network restriction is on
    /// (SPEC R-LINK.5); `None` when it is off.
    egress: Option<EgressGrants>,
    /// The harness process asking for this command, when there is one.
    harness_pid: Option<u32>,
    /// Digest of the command this consent answers for: the dialog asks when
    /// it runs again (R-LINK.6).
    command_digest: Option<CommandDigest>,
    /// The harness session, to tell a site the human declined in it.
    session_id: Option<String>,
    /// Sites the human allowed for this run only, by a yes in the harness
    /// dialog ("this time").
    this_run: Vec<HostPattern>,
}

impl RecordedLinkConsent {
    pub fn new(
        workspace: PathBuf,
        settings_file: Option<PathBuf>,
        egress: Option<EgressGrants>,
    ) -> Self {
        Self {
            workspace,
            settings_file,
            egress,
            harness_pid: None,
            command_digest: None,
            session_id: None,
            this_run: Vec::new(),
        }
    }

    /// Answer for `command`, run by the harness process `harness_pid` in its
    /// session `session_id`: a refusal is recorded so the harness dialog asks
    /// when it runs again (SPEC R-LINK.6).
    pub fn for_harness(
        mut self,
        command: &str,
        harness_pid: Option<u32>,
        session_id: Option<String>,
    ) -> Self {
        self.command_digest = Some(CommandDigest::of(command));
        self.harness_pid = harness_pid;
        self.session_id = session_id;
        self
    }

    /// Open links to `host` in this run: the human said yes ("this time") in
    /// the harness dialog before it.
    pub fn allowing_this_run(mut self, host: Option<&str>) -> Self {
        self.this_run
            .extend(host.map(|h| HostPattern::Exact(h.to_ascii_lowercase())));
        self
    }

    fn grants(&self) -> LinkGrants {
        hook_grants(
            self.settings_file.as_deref(),
            &self.workspace,
            self.this_run.clone(),
        )
    }

    /// Record `question` for the harness dialog, and say whether a re-run
    /// will ask: not when there is no command or harness to ask before, and
    /// not when the human already declined the site in this session, in its
    /// dialog or at another surface for this harness.
    fn record_for_dialog(&self, question: &LinkQuestion) -> bool {
        if self.command_digest.is_none() && self.harness_pid.is_none() {
            return false;
        }
        let Some(dir) = harness_asks::default_dir() else {
            return false;
        };
        let now = ahma_common::session_grants::now_secs();
        let refusal = LinkRefusal {
            host: question.host.clone(),
            shown: question.shown.clone(),
            local: question.local,
            outside_egress: question.outside_egress,
            at: now,
            harness_pid: self.harness_pid,
            command_digest: self.command_digest.clone(),
        };
        if let Err(e) = harness_asks::record_link_refusal(&dir, &self.workspace, refusal) {
            tracing::debug!("link refusal not recorded: {e:#}");
            return false;
        }
        let asks = harness_asks::load(&dir, &self.workspace, now);
        let declined = self
            .session_id
            .as_deref()
            .is_some_and(|session| harness_asks::link_declined(&asks, session, &question.host))
            || harness_asks::link_denied_for(&asks, self.harness_pid, &question.host);
        !declined
    }
}

#[async_trait::async_trait]
impl LinkConsent for RecordedLinkConsent {
    async fn decide(&self, target: &LinkTarget, resolved_local: bool) -> LinkAnswer {
        let allows = |host: &str| {
            self.egress
                .as_ref()
                .is_some_and(|g| g.grant_for(host).is_some())
        };
        let egress = match self.egress {
            Some(_) => Egress::Restricted(&allows),
            None => Egress::Open,
        };
        match policy::decide(target, resolved_local, &self.grants(), &egress) {
            LinkDecision::Open => LinkAnswer::Open,
            // Left for the harness dialog, which asks when this command, or
            // one opening the same site, runs again (R-LINK.6).
            LinkDecision::Ask(question) => {
                let retry_asks = self.record_for_dialog(&question);
                LinkAnswer::Refuse(policy::refusal_line(target, retry_asks))
            }
            // Nobody to ask: the line names the grant (R-LINK.6, rung 3).
            LinkDecision::Denied => LinkAnswer::Refuse(policy::refusal_line(target, false)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahma_common::approver::ApproverSurface;
    use ahma_common::open_url::{OpenUrlBinding, persist_open_url_allow};

    fn target(raw: &str) -> LinkTarget {
        LinkTarget::parse(raw).unwrap()
    }

    /// SPEC R-LINK.4: a saved grant opens, for every repo or for its repo
    /// only, read from the settings file at the moment of the link; without
    /// one the link is refused with the grant line, never asked mid-command.
    #[tokio::test]
    async fn a_saved_grant_opens_and_anything_else_is_refused_with_the_grant_line() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let file = home.path().join(".ahma").join("settings.toml");
        let repo_a = PathBuf::from("/w/a");
        let consent = RecordedLinkConsent::new(repo_a.clone(), Some(file.clone()), None);
        let link = target("https://github.com/ahma-labs/ahma/pull/12");

        let LinkAnswer::Refuse(line) = consent.decide(&link, false).await else {
            panic!("no grant yet");
        };
        assert!(
            line.contains("`ahma permissions grant open-url github.com`"),
            "{line}"
        );

        let elsewhere = OpenUrlBinding::Repo(PathBuf::from("/w/b"));
        persist_open_url_allow(&file, "github.com", &elsewhere, &ApproverSurface::Cli).unwrap();
        assert!(
            matches!(consent.decide(&link, false).await, LinkAnswer::Refuse(_)),
            "another repo's grant does not apply here"
        );

        persist_open_url_allow(
            &file,
            "github.com",
            &OpenUrlBinding::AllRepos,
            &ApproverSurface::Cli,
        )
        .unwrap();
        assert_eq!(consent.decide(&link, false).await, LinkAnswer::Open);
    }

    /// SPEC R-LINK.6: a hooked command's ungranted link is refused at once,
    /// recorded for the harness dialog with what the question shows, and its
    /// line says a re-run asks; once the human declined the site in this
    /// session the line names only the grant. A consent with no command or
    /// harness to ask before records nothing.
    #[tokio::test]
    async fn an_ungranted_hooked_link_is_recorded_and_says_rerun_asks() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let ws = home.path().join("ws");
        let cmd = "gh pr view 12 --web";
        let consent = RecordedLinkConsent::new(ws.clone(), None, None).for_harness(
            cmd,
            Some(42),
            Some("s1".into()),
        );
        let link = target("https://github.com/ahma-labs/ahma/pull/12");

        let LinkAnswer::Refuse(line) = consent.decide(&link, false).await else {
            panic!("no grant");
        };
        assert!(line.contains("re-run this command and ahma asks"), "{line}");
        let dir = harness_asks::default_dir().unwrap();
        let now = ahma_common::session_grants::now_secs();
        let asks = harness_asks::load(&dir, &ws, now);
        assert_eq!(asks.link_refusals.len(), 1, "remembered for the dialog");
        let r = &asks.link_refusals[0];
        assert_eq!(r.host, "github.com");
        assert_eq!(r.shown, "https://github.com/ahma-labs/ahma/pull/12");
        assert_eq!(r.harness_pid, Some(42));
        assert_eq!(r.command_digest, Some(CommandDigest::of(cmd)));

        // Asked in s1 and declined: the token is never spent.
        harness_asks::mark_link_asked(&dir, &ws, "s1", r, cmd, Some(42), now).unwrap();
        let LinkAnswer::Refuse(line) = consent.decide(&link, false).await else {
            panic!("still no grant");
        };
        assert!(!line.contains("re-run this command"), "{line}");
        assert!(
            line.contains("`ahma permissions grant open-url github.com`"),
            "{line}"
        );

        let elsewhere = home.path().join("other");
        let silent = RecordedLinkConsent::new(elsewhere.clone(), None, None);
        let LinkAnswer::Refuse(line) = silent.decide(&link, false).await else {
            panic!("no grant");
        };
        assert!(!line.contains("Claude Code"), "{line}");
        assert!(
            harness_asks::load(&dir, &elsewhere, now)
                .link_refusals
                .is_empty(),
            "nothing for a dialog to ask"
        );
    }

    /// SPEC R-LINK.6: a yes in the harness dialog opens the site's links in
    /// the run it approved, and only that site.
    #[tokio::test]
    async fn a_yes_this_time_opens_that_site_in_this_run_only() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let ws = home.path().join("ws");
        let approved =
            RecordedLinkConsent::new(ws.clone(), None, None).allowing_this_run(Some("GitHub.com"));
        assert_eq!(
            approved
                .decide(&target("https://github.com/o/r"), false)
                .await,
            LinkAnswer::Open
        );
        assert!(matches!(
            approved.decide(&target("https://docs.rs/"), false).await,
            LinkAnswer::Refuse(_)
        ));
        let next_run = RecordedLinkConsent::new(ws, None, None);
        assert!(matches!(
            next_run
                .decide(&target("https://github.com/o/r"), false)
                .await,
            LinkAnswer::Refuse(_)
        ));
    }

    /// SPEC R-LINK.4, R-LINK.5: "any site" covers neither a name the broker
    /// found local nor, under network restriction, a site off the egress
    /// allow list.
    #[tokio::test]
    async fn any_site_covers_no_local_name_and_nothing_off_the_egress_list() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let file = home.path().join(".ahma").join("settings.toml");
        persist_open_url_allow(&file, "*", &OpenUrlBinding::AllRepos, &ApproverSurface::Cli)
            .unwrap();
        let open = RecordedLinkConsent::new(PathBuf::from("/w/a"), Some(file.clone()), None);
        let link = target("https://example.com/");
        assert_eq!(open.decide(&link, false).await, LinkAnswer::Open);
        assert!(matches!(
            open.decide(&link, true).await,
            LinkAnswer::Refuse(_)
        ));

        let egress = EgressGrants::compute(crate::egress::EgressGrantSources {
            operator_allow: &["github.com".to_string()],
            enabled_profiles: &[],
            profile_hosts: false,
            deny_profile_hosts: &[],
        });
        let restricted = RecordedLinkConsent::new(PathBuf::from("/w/a"), Some(file), Some(egress));
        assert!(matches!(
            restricted.decide(&link, false).await,
            LinkAnswer::Refuse(_)
        ));
        assert_eq!(
            restricted
                .decide(&target("https://github.com/x"), false)
                .await,
            LinkAnswer::Open
        );
    }

    /// SPEC R-LINK.4: "this site, this session", given in the TUI for a
    /// hooked command's refused link, is a session grant file the hook's
    /// consent reads: links to that site open for commands in that repo while
    /// the harness lives, and nowhere else.
    #[tokio::test]
    async fn a_session_grant_opens_its_site_for_hooked_commands_in_its_repo() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let ws = home.path().join("ws");
        let link = target("https://github.com/o/r/pull/1");
        let consent = RecordedLinkConsent::new(ws.clone(), None, None);
        assert!(matches!(
            consent.decide(&link, false).await,
            LinkAnswer::Refuse(_)
        ));
        let grant = ahma_common::open_url::OpenUrlSessionGrant {
            site: "github.com".into(),
            workspace: ws.clone(),
            owner_pid: std::process::id(),
            granted_at: ahma_common::session_grants::now_secs(),
            granted_by: ahma_common::approver::ApproverSurface::Tui.into(),
        };
        let dir = ahma_common::open_url::session_dir().unwrap();
        ahma_common::open_url::record_session(&dir, &grant, &ApproverSurface::Tui).unwrap();
        assert_eq!(consent.decide(&link, false).await, LinkAnswer::Open);
        assert!(
            matches!(
                consent.decide(&target("https://docs.rs/"), false).await,
                LinkAnswer::Refuse(_)
            ),
            "that site only"
        );
        let elsewhere = RecordedLinkConsent::new(home.path().join("other"), None, None);
        assert!(
            matches!(elsewhere.decide(&link, false).await, LinkAnswer::Refuse(_)),
            "that repo only"
        );
    }

    /// Records "github.com, this session" for `ws` (canonical, as the TUI
    /// records it), owned by this test process.
    fn record_github_session(ws: &Path) {
        let grant = ahma_common::open_url::OpenUrlSessionGrant {
            site: "github.com".into(),
            workspace: ws.to_path_buf(),
            owner_pid: std::process::id(),
            granted_at: ahma_common::session_grants::now_secs(),
            granted_by: ahma_common::approver::ApproverSurface::Tui.into(),
        };
        let dir = ahma_common::open_url::session_dir().unwrap();
        ahma_common::open_url::record_session(&dir, &grant, &ApproverSurface::Tui).unwrap();
    }

    /// SPEC R-LINK.4: a session grant applies to a command's workspace as
    /// resolved paths, so the repo it was given in, spelled through `..`,
    /// still opens the site's links.
    #[test]
    fn a_session_site_applies_through_a_dotdot_workspace() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let ws = home.path().join("ws");
        let beside = home.path().join("beside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&beside).unwrap();
        record_github_session(&dunce::canonicalize(&ws).unwrap());
        assert_eq!(
            session_sites(&beside.join("..").join("ws")),
            [HostPattern::Exact("github.com".into())]
        );
        assert!(session_sites(&beside).is_empty(), "that repo only");
    }

    /// SPEC R-LINK.4: the repo reached through a symlink is the repo
    /// the session grant was given in.
    #[cfg(unix)]
    #[test]
    fn a_session_site_applies_through_a_symlinked_workspace() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let ws = home.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let link = home.path().join("link");
        std::os::unix::fs::symlink(&ws, &link).unwrap();
        record_github_session(&dunce::canonicalize(&ws).unwrap());
        assert_eq!(
            session_sites(&link),
            [HostPattern::Exact("github.com".into())]
        );
    }

    /// SPEC R-LINK.6: a site denied in the TUI for this harness is not asked
    /// again in its session, so the refusal line names only the grant.
    #[tokio::test]
    async fn a_site_denied_in_the_tui_is_refused_naming_only_the_grant() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let ws = home.path().join("ws");
        let dir = harness_asks::default_dir().unwrap();
        let now = ahma_common::session_grants::now_secs();
        harness_asks::answer_link(&dir, &ws, "github.com", Some(42), true, now).unwrap();
        let link = target("https://github.com/o/r");
        let consent = |pid: u32| {
            RecordedLinkConsent::new(ws.clone(), None, None).for_harness(
                "gh pr view --web",
                Some(pid),
                Some("s1".into()),
            )
        };
        let LinkAnswer::Refuse(line) = consent(42).decide(&link, false).await else {
            panic!("denied");
        };
        assert!(!line.contains("re-run this command"), "{line}");
        assert!(
            line.contains("`ahma permissions grant open-url github.com`"),
            "{line}"
        );
        let LinkAnswer::Refuse(line) = consent(43).decide(&link, false).await else {
            panic!("no grant");
        };
        assert!(
            line.contains("re-run this command"),
            "another harness's session still asks: {line}"
        );
    }
}
