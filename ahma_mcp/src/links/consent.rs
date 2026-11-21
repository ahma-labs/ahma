//! Who says yes to a link (SPEC R-LINK.4–R-LINK.6).
//!
//! [`RecordedLinkConsent`] answers a hooked command from the grants a human
//! already made: `[open_url] allow` for every repo and this repo's
//! `[open_url] repos` entry, in the user's settings file, read afresh for
//! every link so a grant made while the command runs applies. Anything else
//! is refused at once with one line saying how to allow it: a hooked command
//! is never paused (R-LINK.6).

use super::policy::{self, Egress, LinkDecision, LinkGrants, LinkTarget};
use crate::egress::EgressGrants;
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

/// Consent from saved grants only (the terminal hook's).
#[derive(Debug, Clone)]
pub struct RecordedLinkConsent {
    /// The repo `[open_url] repos` is keyed on: its canonical root.
    workspace: PathBuf,
    /// The user's settings file: `[open_url]` is security tier (R-CFG2.1).
    settings_file: Option<PathBuf>,
    /// The effective egress allow list while network restriction is on
    /// (SPEC R-LINK.5); `None` when it is off.
    egress: Option<EgressGrants>,
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
        }
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
        LinkGrants::from_settings(&settings, &self.workspace, Vec::new(), Vec::new())
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
            // Nobody to ask: the line names the grant (R-LINK.6, rung 3).
            LinkDecision::Ask(_) | LinkDecision::Denied => {
                LinkAnswer::Refuse(policy::refusal_line(target, false))
            }
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
