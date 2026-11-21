//! Who says yes to a link (SPEC R-LINK.4–R-LINK.6).
//!
//! [`RecordedLinkConsent`] answers a hooked command from the grants a human
//! already made: `[open_url] allow` for every repo and this repo's
//! `[open_url] repos` entry, in the user's settings file, read afresh for
//! every link so a grant made while the command runs applies, and a site the
//! human allowed for this one run in the harness dialog. Anything else is
//! refused at once with one line: a hooked command is never paused
//! (R-LINK.6). The refusal is recorded for the harness dialog, which asks
//! when the command runs again.

use super::policy::{self, Egress, LinkDecision, LinkGrants, LinkQuestion, LinkTarget};
use crate::egress::{EgressGrants, HostPattern};
use ahma_common::harness_asks::{self, CommandDigest, LinkRefusal};
use std::path::PathBuf;

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
        // Read with plain std I/O: this runs on the broker thread, and
        // `tokio::fs` would hand it to a blocking-pool thread that may be
        // created after the process restricted itself (SPEC R-CRED.9). One
        // small file, as the SSH key broker's consent reads it.
        let settings = self
            .settings_file
            .as_ref()
            .and_then(|f| ahma_common::config::AhmaSettings::load_from_result(f).ok())
            .map(|s| s.open_url)
            .unwrap_or_default();
        LinkGrants::from_settings(
            &settings,
            &self.workspace,
            self.this_run.clone(),
            Vec::new(),
        )
    }

    /// Record `question` for the harness dialog, and say whether a re-run
    /// will ask: not when there is no command or harness to ask before, and
    /// not when the human already declined the site in this session.
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
        let declined = self.session_id.as_deref().is_some_and(|session| {
            let asks = harness_asks::load(&dir, &self.workspace, now);
            harness_asks::link_declined(&asks, session, &question.host)
        });
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
}
