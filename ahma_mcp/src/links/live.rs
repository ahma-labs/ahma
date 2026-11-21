//! Links on the MCP path: asked while the command waits (SPEC R-LINK.6).
//!
//! A command ahma runs for an MCP client gets a link broker as a hooked
//! command does (R-LINK.1), with one difference: nothing stops it waiting.
//! An ungranted link is put to the human through the R-PERM.3 ladder while
//! the command waits, for at most the elicitation budget, and the answer
//! applies to that link at once — "this time" means this link, now.
//!
//! - [`LiveLinks`]: the factory the MCP server gives the adapter, one lease
//!   per command.
//! - [`LinkSession`]: what the session remembers — sites declined in it, a
//!   question in flight per site, the asking surface and its strike — shared
//!   by every command of the session.
//! - [`LiveLinkConsent`]: one command's consent: saved grants, the session's
//!   answers, then ask.
//! - [`LinkAsker`] / [`PeerLinkAsker`]: rung 1, the MCP client's
//!   `elicitation/create`. Rung 2 (an attached TUI, `ahma web`, `ahma
//!   desktop`) is another asker behind the same trait; until one is
//!   installed, a link nobody can be asked about is refused with the line
//!   naming `ahma permissions grant open-url <site>` (rung 3).

use super::broker::{LinkBroker, LinkWatch};
use super::consent::{LinkAnswer, LinkConsent};
use super::host::{LinkBrokerFactory, LinkLease, helper_link};
use super::policy::{
    self, Effect, Egress, LinkChoice, LinkDecision, LinkGrants, LinkQuestion, LinkTarget, Surface,
};
use crate::egress::{EgressGrants, HostPattern};
use ahma_common::approver::ApproverSurface;
use ahma_common::net_approval::NetApprovalCoordinator;
use ahma_common::permissions::GrantTier;
use ahma_common::scope_grant::GrantCoordinator;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Why a link was not asked about: the agent read
/// [`policy::unasked_refusal_line`], and the human learns the question did
/// not reach them (SPEC R-PERM.3.2).
const NO_SURFACE: &str = "no MCP client that can show a question is connected";
const DEMOTED: &str = "asking in the MCP client failed earlier this session";
const BUDGET_SPENT: &str = "ahma has already asked five questions in the last ten minutes \
     (SPEC R-PERM.4.5); ask the human in conversation";

/// What happened when a surface was asked about a link (SPEC R-PERM.3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkAskOutcome {
    /// The human chose. A decline is an answer: [`LinkChoice::Deny`].
    Answered(LinkChoice),
    /// There was nothing to ask: no client attached, or it cannot show a
    /// question. Not a fault. The reason, for the refusal line.
    Unavailable(String),
    /// The question closed without a choice: the client's `cancel`, or the
    /// wait ran out. Nobody's decision, so nothing is remembered.
    Dismissed(String),
    /// Asking failed: the transport broke or the reply was unreadable. The
    /// one outcome that demotes the surface for the session.
    Failed(String),
}

/// A surface a link question can be put to while the command waits.
#[async_trait::async_trait]
pub trait LinkAsker: Send + Sync + std::fmt::Debug {
    /// Where the question goes and for how long the command waits, for the
    /// alert on the waiting command ("in the MCP client, for at most 45 s").
    fn where_and_how_long(&self) -> String;

    /// Put `question` to the human, offering `choices` (deny first) with
    /// [`LinkQuestion::text`] — the site, the address and the guidance
    /// (SPEC R-LINK.7).
    async fn ask(&self, question: &LinkQuestion, choices: &[LinkChoice]) -> LinkAskOutcome;
}

/// How a question about one site settled, for every command waiting on it
/// (SPEC R-PERM.3.8: one answer settles every waiting copy, each at its own
/// tier — a "this time" opens each waiting command's link once).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Settled {
    Allowed,
    Declined,
    /// Nobody answered: why, for the refusal line.
    Unasked(String),
}

/// What an MCP session remembers about links, shared by all its commands.
#[derive(Default)]
pub struct LinkSession {
    /// Rung 1, installed once the MCP peer slot exists.
    asker: RwLock<Option<Arc<dyn LinkAsker>>>,
    /// The session's network grants: under network restriction "any site"
    /// covers a site the human allowed egress to this session (SPEC R-LINK.5).
    net: RwLock<Option<Arc<NetApprovalCoordinator>>>,
    /// The session's prompt budget, shared with every grant question
    /// (SPEC R-PERM.4.5). `None`: not counted.
    budget: RwLock<Option<Arc<GrantCoordinator>>>,
    state: Mutex<SessionState>,
}

impl std::fmt::Debug for LinkSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkSession")
            .field("asker", &self.asker.read().is_some())
            .field("state", &*self.state.lock())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct SessionState {
    /// "This site, this session" answers (a surface that offers it).
    granted: Vec<HostPattern>,
    /// Sites the human declined this session: not asked again (R-LINK.6).
    denied: Vec<String>,
    /// A question in flight, per repo and site: a second link to the same
    /// site waits for its answer instead of asking again (R-PERM.3.8).
    asking: HashMap<(PathBuf, String), tokio::sync::watch::Receiver<Option<Settled>>>,
    /// The asking surface failed once: skip it for the session (R-PERM.3.1).
    demoted: bool,
}

impl LinkSession {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Install rung 1, the surface link questions are put to.
    pub fn set_asker(&self, asker: Arc<dyn LinkAsker>) {
        *self.asker.write() = Some(asker);
        self.state.lock().demoted = false;
    }

    /// Count network grants given in this session as allowed egress
    /// (SPEC R-LINK.5).
    pub fn set_network_grants(&self, net: Arc<NetApprovalCoordinator>) {
        *self.net.write() = Some(net);
    }

    /// Charge link questions to this session's prompt budget (SPEC R-PERM.4.5).
    pub fn set_budget(&self, coordinator: Arc<GrantCoordinator>) {
        *self.budget.write() = Some(coordinator);
    }

    /// Sites declined in this session.
    pub fn declined(&self) -> Vec<String> {
        self.state.lock().denied.clone()
    }
}

/// Removes a question from the in-flight set however its asker ends — answered,
/// or dropped because the command that asked was killed — so the next link to
/// the site asks again instead of waiting on a question nobody holds.
struct InFlight {
    session: Arc<LinkSession>,
    key: (PathBuf, String),
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.session.state.lock().asking.remove(&self.key);
    }
}

/// One command's consent on the MCP path (SPEC R-LINK.4–R-LINK.6).
#[derive(Debug)]
pub struct LiveLinkConsent {
    /// The repo `[open_url] repos` is keyed on, and a "this repo only"
    /// answer is saved for.
    workspace: PathBuf,
    settings_file: Option<PathBuf>,
    /// The configured egress allow list while network restriction is on.
    egress: Option<EgressGrants>,
    session: Arc<LinkSession>,
    /// The command's alert line while it waits.
    watch: LinkWatch,
}

impl LiveLinkConsent {
    pub fn new(
        workspace: PathBuf,
        settings_file: Option<PathBuf>,
        egress: Option<EgressGrants>,
        session: Arc<LinkSession>,
        watch: LinkWatch,
    ) -> Self {
        Self {
            workspace,
            settings_file,
            egress,
            session,
            watch,
        }
    }

    /// Saved grants, read afresh for every link with plain std I/O: this runs
    /// on the broker thread (SPEC R-CRED.9), as [`super::consent::RecordedLinkConsent`]
    /// reads them. Plus this session's answers.
    fn grants(&self) -> LinkGrants {
        let settings = self
            .settings_file
            .as_ref()
            .and_then(|f| ahma_common::config::AhmaSettings::load_from_result(f).ok())
            .map(|s| s.open_url)
            .unwrap_or_default();
        let state = self.session.state.lock();
        LinkGrants::from_settings(
            &settings,
            &self.workspace,
            state.granted.clone(),
            state.denied.clone(),
        )
    }

    /// Ask about `question`, or wait for the answer to the same question
    /// already asked (SPEC R-PERM.3.8), and say what that answer means for
    /// this link.
    async fn ask(&self, target: &LinkTarget, question: LinkQuestion) -> LinkAnswer {
        let key = (self.workspace.clone(), question.host.clone());
        let mine = {
            let mut state = self.session.state.lock();
            match state.asking.get(&key) {
                Some(waiting) => Err(waiting.clone()),
                None => {
                    let (tx, rx) = tokio::sync::watch::channel(None);
                    state.asking.insert(key.clone(), rx);
                    Ok(tx)
                }
            }
        };
        let settled = match mine {
            Ok(tx) => {
                let _in_flight = InFlight {
                    session: Arc::clone(&self.session),
                    key,
                };
                let settled = self.ask_now(&question).await;
                let _ = tx.send(Some(settled.clone()));
                settled
            }
            Err(mut waiting) => match waiting.wait_for(Option::is_some).await {
                Ok(settled) => settled
                    .clone()
                    .unwrap_or(Settled::Unasked(NO_SURFACE.into())),
                // The asker went away unanswered (its command was killed).
                Err(_) => Settled::Unasked("the question was withdrawn unanswered".into()),
            },
        };
        match settled {
            Settled::Allowed => LinkAnswer::Open,
            Settled::Declined => LinkAnswer::Refuse(policy::refusal_line(target, false)),
            Settled::Unasked(why) => LinkAnswer::Refuse(policy::unasked_refusal_line(target, &why)),
        }
    }

    /// Put the question to the asking surface, within the session's budget,
    /// and apply the answer.
    async fn ask_now(&self, question: &LinkQuestion) -> Settled {
        let choices = question.choices(Surface::Live);
        let asker = if self.session.state.lock().demoted {
            return Settled::Unasked(DEMOTED.into());
        } else {
            self.session.asker.read().clone()
        };
        let Some(asker) = asker else {
            return Settled::Unasked(NO_SURFACE.into());
        };
        let budget = self.session.budget.read().clone();
        let charge = match &budget {
            Some(coordinator) => match coordinator.charge_question() {
                Some(charge) => Some(charge),
                None => return Settled::Unasked(BUDGET_SPENT.into()),
            },
            None => None,
        };
        self.watch.tell(&format!(
            "Waiting for the human: ahma is asking {} whether this command may open a link \
             to {} in the browser (SPEC R-LINK.6). The command continues when they answer.",
            asker.where_and_how_long(),
            question.host
        ));
        match asker.ask(question, &choices).await {
            LinkAskOutcome::Answered(choice) => {
                // An answer the question did not offer is not consent.
                let choice = if choices.contains(&choice) {
                    choice
                } else {
                    LinkChoice::Deny
                };
                self.apply(choice, question)
            }
            LinkAskOutcome::Unavailable(why) => {
                if let (Some(coordinator), Some(charge)) = (&budget, charge) {
                    coordinator.refund_question(charge);
                }
                Settled::Unasked(why)
            }
            LinkAskOutcome::Dismissed(why) => Settled::Unasked(why),
            LinkAskOutcome::Failed(why) => {
                self.session.state.lock().demoted = true;
                Settled::Unasked(format!("asking in the MCP client failed ({why})"))
            }
        }
    }

    /// What `choice` does (SPEC R-LINK.6, R-LINK.9): a deny is remembered
    /// for the session; "this time" opens this link and is audited as a once
    /// grant; a lasting answer is saved by the one writer, audited with the
    /// surface that answered, and holds for the session even if the write
    /// fails.
    fn apply(&self, choice: LinkChoice, question: &LinkQuestion) -> Settled {
        let surface = ApproverSurface::Harness;
        let now = ahma_common::session_grants::now_secs();
        match policy::effect(choice, &question.host, &self.workspace) {
            Effect::Refuse { remember_deny } => {
                if remember_deny {
                    self.session
                        .state
                        .lock()
                        .denied
                        .push(question.host.to_ascii_lowercase());
                }
                Settled::Declined
            }
            Effect::OpenOnce => {
                ahma_common::open_url::audit_unsaved_grant(
                    &question.host,
                    GrantTier::Once,
                    &surface,
                    now,
                );
                Settled::Allowed
            }
            Effect::Session(pattern) => {
                ahma_common::open_url::audit_unsaved_grant(
                    &question.host,
                    GrantTier::Session,
                    &surface,
                    now,
                );
                self.session.state.lock().granted.push(pattern);
                Settled::Allowed
            }
            Effect::Persist(pattern, binding) => {
                let saved = self.settings_file.as_deref().map(|file| {
                    ahma_common::open_url::persist_open_url_allow(
                        file,
                        &pattern.as_str(),
                        &binding,
                        &surface,
                    )
                });
                match saved {
                    Some(Ok(_)) => {}
                    Some(Err(e)) => tracing::warn!(
                        "a link grant for {} was approved but could not be saved: {e:#}; it \
                         holds for this session",
                        pattern.as_str()
                    ),
                    None => tracing::warn!(
                        "a link grant for {} was approved but there is no settings file to \
                         save it in; it holds for this session",
                        pattern.as_str()
                    ),
                }
                // A "this repo only" answer stays in its repo this session too.
                if binding_is_here(&binding, &self.workspace) {
                    self.session.state.lock().granted.push(pattern);
                }
                Settled::Allowed
            }
        }
    }
}

/// Whether a grant saved with `binding` applies in `workspace` (always, for
/// one saved for every repo).
fn binding_is_here(binding: &ahma_common::open_url::OpenUrlBinding, workspace: &Path) -> bool {
    match binding {
        ahma_common::open_url::OpenUrlBinding::AllRepos => true,
        ahma_common::open_url::OpenUrlBinding::Repo(repo) => repo == workspace,
    }
}

#[async_trait::async_trait]
impl LinkConsent for LiveLinkConsent {
    async fn decide(&self, target: &LinkTarget, resolved_local: bool) -> LinkAnswer {
        let decision = {
            let net = self.session.net.read().clone();
            let allows = |host: &str| {
                self.egress
                    .as_ref()
                    .is_some_and(|g| g.grant_for(host).is_some())
                    || net.as_ref().is_some_and(|n| n.is_session_granted(host))
            };
            let egress = match self.egress {
                Some(_) => Egress::Restricted(&allows),
                None => Egress::Open,
            };
            policy::decide(target, resolved_local, &self.grants(), &egress)
        };
        match decision {
            LinkDecision::Open => LinkAnswer::Open,
            // Declined this session: the line names only the grant (R-LINK.6).
            LinkDecision::Denied => LinkAnswer::Refuse(policy::refusal_line(target, false)),
            LinkDecision::Ask(question) => self.ask(target, question).await,
        }
    }
}

/// Rung 1: the MCP client's `elicitation/create` (SPEC R-PERM.3, R5.3.1).
/// Holds the service's peer slot, filled in when a client connects.
#[derive(Debug)]
pub struct PeerLinkAsker {
    peer: Arc<RwLock<Option<rmcp::service::Peer<rmcp::service::RoleServer>>>>,
}

impl PeerLinkAsker {
    pub fn new(peer: Arc<RwLock<Option<rmcp::service::Peer<rmcp::service::RoleServer>>>>) -> Self {
        Self { peer }
    }

    fn budget(&self) -> Option<std::time::Duration> {
        let peer = self.peer.read().clone()?;
        Some(crate::client_type::McpClientType::from_peer(&peer).elicitation_budget())
    }
}

/// The form: one titled single-select, deny first and the default, every
/// answer labelled in words (SPEC R-PERM.3.4, R5.3.1: Enter alone never
/// opens a link).
fn link_form_schema(choices: &[LinkChoice]) -> rmcp::model::ElicitationSchema {
    use rmcp::model::{
        ConstTitle, ElicitationSchema, EnumSchema, PrimitiveSchemaDefinition,
        SingleSelectEnumSchema, TitledSingleSelectEnumSchema,
    };
    let one_of: Vec<ConstTitle> = choices
        .iter()
        .map(|c| ConstTitle::new(c.value(), c.label()))
        .collect();
    let mut select = TitledSingleSelectEnumSchema::new(one_of);
    select.title = Some("Your decision".into());
    select.description = Some(
        "Deny is the default. \"This time\" opens this link now and remembers nothing.".into(),
    );
    select.default = Some(LinkChoice::Deny.value().to_string());
    let mut props = std::collections::BTreeMap::new();
    props.insert(
        "decision".to_string(),
        PrimitiveSchemaDefinition::Enum(EnumSchema::Single(SingleSelectEnumSchema::Titled(select))),
    );
    let mut schema = ElicitationSchema::new(props);
    schema.required = Some(vec!["decision".to_string()]);
    schema
}

#[derive(Debug, serde::Deserialize)]
struct LinkForm {
    decision: String,
}

#[async_trait::async_trait]
impl LinkAsker for PeerLinkAsker {
    fn where_and_how_long(&self) -> String {
        match self.budget() {
            Some(budget) => format!("in the MCP client, for at most {} s,", budget.as_secs()),
            None => "in the MCP client".to_string(),
        }
    }

    async fn ask(&self, question: &LinkQuestion, choices: &[LinkChoice]) -> LinkAskOutcome {
        use rmcp::model::{
            ClientResult, ElicitRequest, ElicitRequestParams, ElicitationAction, ServerRequest,
        };
        let Some(peer) = self.peer.read().clone() else {
            return LinkAskOutcome::Unavailable(NO_SURFACE.into());
        };
        if !peer
            .supported_elicitation_modes()
            .contains(&rmcp::service::ElicitationMode::Form)
        {
            return LinkAskOutcome::Unavailable(
                "the MCP client cannot show questions (it did not offer elicitation)".into(),
            );
        }
        // Bounded by this client's patience, so ahma resolves the prompt
        // (SPEC R5.3.1); the command waits at most this long.
        let budget = crate::client_type::McpClientType::from_peer(&peer).elicitation_budget();
        let request = ElicitRequest::new(ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: question.text(),
            requested_schema: link_form_schema(choices),
        });
        let sent = tokio::time::timeout(
            budget,
            peer.send_request(ServerRequest::ElicitRequest(request)),
        )
        .await;
        match sent {
            Ok(Ok(ClientResult::ElicitResult(result))) => match result.action {
                ElicitationAction::Accept => match result
                    .content
                    .and_then(|v| serde_json::from_value::<LinkForm>(v).ok())
                {
                    Some(form) => {
                        LinkAskOutcome::Answered(LinkChoice::from_value(&form.decision, choices))
                    }
                    None => LinkAskOutcome::Dismissed(
                        "the question was accepted without a choice".into(),
                    ),
                },
                ElicitationAction::Decline => LinkAskOutcome::Answered(LinkChoice::Deny),
                ElicitationAction::Cancel => LinkAskOutcome::Dismissed(
                    "the question in the MCP client was closed without an answer".into(),
                ),
                _ => LinkAskOutcome::Dismissed("the MCP client gave an unknown answer".into()),
            },
            Ok(Ok(other)) => LinkAskOutcome::Failed(format!("unexpected reply: {other:?}")),
            Ok(Err(e)) => LinkAskOutcome::Failed(e.to_string()),
            Err(_) => LinkAskOutcome::Dismissed(format!(
                "nobody answered the question in the MCP client within {} s",
                budget.as_secs()
            )),
        }
    }
}

/// The link broker factory for the MCP server (SPEC R-LINK.1): every command
/// gets a broker whose consent asks while it waits ([`LiveLinkConsent`]),
/// sharing the session's [`LinkSession`].
#[derive(Debug)]
pub struct LiveLinks {
    settings_file: Option<PathBuf>,
    scopes: Vec<PathBuf>,
    egress: Option<EgressGrants>,
    session: Arc<LinkSession>,
    /// Made once, before the process restricts itself (SPEC R-CRED.9).
    helper: Option<PathBuf>,
    /// The broker directory, made with the helper.
    dir: Option<PathBuf>,
}

impl LiveLinks {
    /// For a server with `settings_file` (the user's), `scopes`, and the
    /// effective egress allow list while network restriction is on. Starts
    /// the broker thread and makes the helper link now, before anything
    /// restricts the process (SPEC R-CRED.9).
    pub fn new(
        settings_file: Option<PathBuf>,
        scopes: Vec<PathBuf>,
        egress: Option<EgressGrants>,
    ) -> Self {
        crate::credentials::host::start();
        Self {
            settings_file,
            scopes,
            egress,
            session: LinkSession::new(),
            helper: helper_link(),
            dir: crate::credentials::host::agent_dir(),
        }
    }

    /// Point commands at `helper` instead of this binary's own link.
    pub fn with_helper(mut self, helper: PathBuf) -> Self {
        self.helper = Some(helper);
        self
    }

    /// The session every command's consent shares.
    pub fn session(&self) -> &Arc<LinkSession> {
        &self.session
    }

    /// Put link questions to the MCP client that connects to `service`
    /// (rung 1 of R-PERM.3), count the network grants given in its session
    /// as allowed egress (SPEC R-LINK.5), and spend `budget`, the session's
    /// prompt budget, on them (R-PERM.4.5). The one wiring `ahma serve` and
    /// the in-process test server share.
    pub fn attach(
        &self,
        service: &crate::mcp_service::AhmaMcpService,
        budget: Option<Arc<GrantCoordinator>>,
    ) {
        self.session
            .set_asker(Arc::new(PeerLinkAsker::new(service.peer.clone())));
        self.session
            .set_network_grants(service.net_approval.clone());
        if let Some(budget) = budget {
            self.session.set_budget(budget);
        }
    }
}

impl LinkBrokerFactory for LiveLinks {
    fn lease_for_command(&self, working_dir: &Path, _command: Option<&str>) -> Option<LinkLease> {
        let workspace = crate::adapter::workspace_queue::workspace_key(working_dir, &self.scopes);
        let watch = LinkWatch::default();
        let consent = LiveLinkConsent::new(
            workspace,
            self.settings_file.clone(),
            self.egress.clone(),
            Arc::clone(&self.session),
            watch.clone(),
        );
        let broker = LinkBroker::watched(
            Arc::new(consent),
            super::opener::for_this_process(),
            Arc::new(super::broker::DnsResolver),
            watch,
        );
        match LinkLease::start(broker, self.dir.as_deref()?, self.helper.clone()?) {
            Ok(lease) => Some(lease),
            Err(e) => {
                tracing::warn!("no link broker for this command: {e}");
                None
            }
        }
    }

    fn helper(&self) -> Option<PathBuf> {
        self.helper.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahma_common::open_url::{OpenUrlBinding, persist_open_url_allow};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Answers every question with the next scripted outcome, counting them.
    #[derive(Debug)]
    struct Scripted {
        outcomes: Mutex<Vec<LinkAskOutcome>>,
        asked: AtomicUsize,
        /// Held until released, so a test can line up a second link behind it.
        gate: Option<Arc<tokio::sync::Notify>>,
        shown: Mutex<Vec<(String, Vec<LinkChoice>)>>,
    }

    impl Scripted {
        fn new(outcomes: Vec<LinkAskOutcome>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: Mutex::new(outcomes),
                asked: AtomicUsize::new(0),
                gate: None,
                shown: Mutex::new(Vec::new()),
            })
        }
        fn gated(outcome: LinkAskOutcome, gate: Arc<tokio::sync::Notify>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: Mutex::new(vec![outcome]),
                asked: AtomicUsize::new(0),
                gate: Some(gate),
                shown: Mutex::new(Vec::new()),
            })
        }
        fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl LinkAsker for Scripted {
        fn where_and_how_long(&self) -> String {
            "in the test".into()
        }
        async fn ask(&self, question: &LinkQuestion, choices: &[LinkChoice]) -> LinkAskOutcome {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.shown.lock().push((question.text(), choices.to_vec()));
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            let mut outcomes = self.outcomes.lock();
            if outcomes.is_empty() {
                LinkAskOutcome::Unavailable("nothing scripted".into())
            } else {
                outcomes.remove(0)
            }
        }
    }

    fn target(raw: &str) -> LinkTarget {
        LinkTarget::parse(raw).unwrap()
    }

    struct Fixture {
        home: tempfile::TempDir,
        session: Arc<LinkSession>,
        settings: PathBuf,
        watched: Arc<Mutex<Vec<String>>>,
    }

    impl Fixture {
        fn new(asker: Option<Arc<dyn LinkAsker>>) -> Self {
            let home = tempfile::tempdir().unwrap();
            // SAFETY: nextest runs each test in its own process.
            unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
            let settings = home.path().join(".ahma").join("settings.toml");
            let session = LinkSession::new();
            if let Some(asker) = asker {
                session.set_asker(asker);
            }
            Self {
                home,
                session,
                settings,
                watched: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn ws(&self) -> PathBuf {
            self.home.path().join("repo")
        }

        fn consent(&self) -> LiveLinkConsent {
            let watch = LinkWatch::default();
            let watched = Arc::clone(&self.watched);
            watch.set(Arc::new(move |line: &str| watched.lock().push(line.into())));
            LiveLinkConsent::new(
                self.ws(),
                Some(self.settings.clone()),
                None,
                Arc::clone(&self.session),
                watch,
            )
        }

        fn audit(&self) -> String {
            std::fs::read_to_string(ahma_common::permissions::audit_path().unwrap())
                .unwrap_or_default()
        }
    }

    /// SPEC R-LINK.6: the question offers, deny first, deny · this time ·
    /// always for this site — all repos · this repo only · any site, and
    /// carries the site, the address and the guidance (R-LINK.7); while the
    /// command waits, its operation is told where the question went.
    #[tokio::test]
    async fn a_live_question_offers_the_answers_in_order_with_the_guidance() {
        let asker = Scripted::new(vec![LinkAskOutcome::Answered(LinkChoice::Once)]);
        let f = Fixture::new(Some(asker.clone()));
        let link = target("https://github.com/o/r/pull/1");
        assert_eq!(f.consent().decide(&link, false).await, LinkAnswer::Open);
        let shown = asker.shown.lock().clone();
        let (text, choices) = &shown[0];
        assert_eq!(
            choices,
            &[
                LinkChoice::Deny,
                LinkChoice::Once,
                LinkChoice::AlwaysSiteAllRepos,
                LinkChoice::AlwaysSiteThisRepo,
                LinkChoice::AlwaysAny,
            ]
        );
        assert!(text.contains("Site: github.com"), "{text}");
        assert!(text.contains("https://github.com/o/r/pull/1"), "{text}");
        assert!(text.ends_with(ahma_common::open_url::GUIDANCE), "{text}");
        let watched = f.watched.lock().clone();
        assert!(
            watched
                .iter()
                .any(|l| l.starts_with("Waiting for the human") && l.contains("github.com")),
            "{watched:?}"
        );
    }

    /// SPEC R-LINK.6, R-LINK.9: "this time" opens this link and remembers
    /// nothing — the next link to the site is asked again — and is audited
    /// as a once grant answered in the agent's own prompt.
    #[tokio::test]
    async fn this_time_opens_once_is_audited_and_asks_again_next_time() {
        let asker = Scripted::new(vec![
            LinkAskOutcome::Answered(LinkChoice::Once),
            LinkAskOutcome::Answered(LinkChoice::Once),
        ]);
        let f = Fixture::new(Some(asker.clone()));
        let link = target("https://github.com/o/r");
        assert_eq!(f.consent().decide(&link, false).await, LinkAnswer::Open);
        assert_eq!(f.consent().decide(&link, false).await, LinkAnswer::Open);
        assert_eq!(asker.asked(), 2, "a yes was for one link");
        let audit = f.audit();
        assert!(audit.contains("\"open-url\""), "{audit}");
        assert!(audit.contains("\"once\""), "{audit}");
        assert!(audit.contains("\"harness\""), "{audit}");
        assert!(!f.settings.exists(), "nothing saved");
    }

    /// SPEC R-LINK.6, R-LINK.4, R-LINK.9: "always for this site — all repos"
    /// saves the site for every repo through the one writer, audited with the
    /// harness surface; the next link to the site opens without a question.
    /// "This repo only" saves it for this repo.
    #[tokio::test]
    async fn always_for_this_site_is_saved_audited_and_not_asked_again() {
        let asker = Scripted::new(vec![
            LinkAskOutcome::Answered(LinkChoice::AlwaysSiteAllRepos),
            LinkAskOutcome::Answered(LinkChoice::AlwaysSiteThisRepo),
        ]);
        let f = Fixture::new(Some(asker.clone()));
        let consent = f.consent();
        assert_eq!(
            consent.decide(&target("https://github.com/a"), false).await,
            LinkAnswer::Open
        );
        assert_eq!(
            consent.decide(&target("https://github.com/b"), false).await,
            LinkAnswer::Open
        );
        assert_eq!(asker.asked(), 1, "saved, so not asked again");
        let saved = ahma_common::config::AhmaSettings::load_from_result(&f.settings)
            .unwrap()
            .open_url;
        assert_eq!(saved.allow, ["github.com"]);

        assert_eq!(
            consent.decide(&target("https://docs.rs/x"), false).await,
            LinkAnswer::Open
        );
        let saved = ahma_common::config::AhmaSettings::load_from_result(&f.settings)
            .unwrap()
            .open_url;
        assert_eq!(saved.repos.len(), 1);
        assert_eq!(saved.repos[0].workspace, f.ws());
        assert_eq!(saved.repos[0].allow, ["docs.rs"]);
        let audit = f.audit();
        assert!(audit.contains("\"always\""), "{audit}");
        assert!(audit.contains("\"harness\""), "{audit}");
        assert!(audit.contains("docs.rs in "), "{audit}");
    }

    /// SPEC R-LINK.6: a deny is remembered for the session — the site is not
    /// asked again, and its line names only the grant — and a saved grant
    /// made afterwards still wins (R-LINK.4's order).
    #[tokio::test]
    async fn a_deny_is_remembered_for_the_session() {
        let asker = Scripted::new(vec![LinkAskOutcome::Answered(LinkChoice::Deny)]);
        let f = Fixture::new(Some(asker.clone()));
        let link = target("https://github.com/o/r");
        for _ in 0..2 {
            let LinkAnswer::Refuse(line) = f.consent().decide(&link, false).await else {
                panic!("declined");
            };
            assert!(
                line.contains("`ahma permissions grant open-url github.com`"),
                "{line}"
            );
            assert!(!line.contains("could not ask"), "{line}");
        }
        assert_eq!(asker.asked(), 1, "declined once, asked once");
        assert_eq!(f.session.declined(), ["github.com"]);
        persist_open_url_allow(
            &f.settings,
            "github.com",
            &OpenUrlBinding::AllRepos,
            &ApproverSurface::Cli,
        )
        .unwrap();
        assert_eq!(f.consent().decide(&link, false).await, LinkAnswer::Open);
    }

    /// SPEC R-LINK.6 (rung 3), R-PERM.3.2: nobody to ask — no client, one
    /// that cannot show a question, a question closed unanswered — refuses
    /// with the grant line saying why nobody was asked; none of them is a
    /// decision, so nothing is remembered and the site is asked next time.
    #[tokio::test]
    async fn nobody_to_ask_refuses_with_the_grant_line_and_remembers_nothing() {
        let f = Fixture::new(None);
        let link = target("https://github.com/o/r");
        let LinkAnswer::Refuse(line) = f.consent().decide(&link, false).await else {
            panic!("nobody to ask");
        };
        assert!(line.contains("could not ask: no MCP client"), "{line}");
        assert!(
            line.contains("`ahma permissions grant open-url github.com`"),
            "{line}"
        );

        let asker = Scripted::new(vec![
            LinkAskOutcome::Unavailable("the MCP client cannot show questions".into()),
            LinkAskOutcome::Dismissed("closed".into()),
            LinkAskOutcome::Answered(LinkChoice::Once),
        ]);
        f.session.set_asker(asker.clone());
        for why in ["cannot show questions", "closed"] {
            let LinkAnswer::Refuse(line) = f.consent().decide(&link, false).await else {
                panic!("unanswered");
            };
            assert!(line.contains(why), "{line}");
        }
        assert!(f.session.declined().is_empty(), "no decision was made");
        assert_eq!(f.consent().decide(&link, false).await, LinkAnswer::Open);
    }

    /// SPEC R-PERM.3.1: a surface that fails to ask is skipped for the rest
    /// of the session; a decline never demotes it.
    #[tokio::test]
    async fn a_failed_ask_demotes_the_surface_and_a_decline_does_not() {
        let asker = Scripted::new(vec![
            LinkAskOutcome::Answered(LinkChoice::Deny),
            LinkAskOutcome::Failed("transport closed".into()),
            LinkAskOutcome::Answered(LinkChoice::Once),
        ]);
        let f = Fixture::new(Some(asker.clone()));
        let _ = f
            .consent()
            .decide(&target("https://a.example/"), false)
            .await;
        let _ = f
            .consent()
            .decide(&target("https://b.example/"), false)
            .await;
        assert_eq!(asker.asked(), 2, "a decline did not demote it");
        let LinkAnswer::Refuse(line) = f
            .consent()
            .decide(&target("https://c.example/"), false)
            .await
        else {
            panic!("demoted");
        };
        assert!(line.contains("failed earlier this session"), "{line}");
        assert_eq!(asker.asked(), 2, "not asked after it failed");
    }

    /// SPEC R-LINK.6, R-PERM.3.8: a site is asked at most once at a time; a
    /// second command's link to it waits for that answer and is settled by
    /// it at its own tier — "this time" opens each waiting link once.
    #[tokio::test]
    async fn copies_of_one_question_wait_for_one_answer() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let asker = Scripted::gated(LinkAskOutcome::Answered(LinkChoice::Once), gate.clone());
        let f = Fixture::new(Some(asker.clone()));
        let (first, second) = (f.consent(), f.consent());
        let a = target("https://github.com/a");
        let b = target("https://github.com/b");
        let both = async {
            tokio::join!(first.decide(&a, false), second.decide(&b, false), async {
                // Released only once both are waiting on the one question.
                while asker.asked() == 0 || f.session.state.lock().asking.is_empty() {
                    tokio::task::yield_now().await;
                }
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
                gate.notify_one();
            })
        };
        let (one, two, ()) = both.await;
        assert_eq!((one, two), (LinkAnswer::Open, LinkAnswer::Open));
        assert_eq!(asker.asked(), 1, "one question for both copies");
        assert!(
            f.session.state.lock().asking.is_empty(),
            "nothing left in flight"
        );
    }

    /// SPEC R-PERM.4.5: link questions count toward the session's prompt
    /// budget; past it nothing is asked and the line says to ask in
    /// conversation. A question that reached nobody costs nothing.
    #[tokio::test]
    async fn link_questions_spend_the_session_budget() {
        let asker = Scripted::new(
            (0..ahma_common::scope_grant::PROMPT_BUDGET)
                .map(|_| LinkAskOutcome::Answered(LinkChoice::Once))
                .collect(),
        );
        let f = Fixture::new(Some(asker.clone()));
        let coordinator = Arc::new(GrantCoordinator::new());
        f.session.set_budget(coordinator.clone());
        for i in 0..ahma_common::scope_grant::PROMPT_BUDGET {
            let link = target(&format!("https://site{i}.example/"));
            assert_eq!(f.consent().decide(&link, false).await, LinkAnswer::Open);
        }
        assert!(
            coordinator.budget_exhausted(),
            "shared with grant questions"
        );
        let LinkAnswer::Refuse(line) = f
            .consent()
            .decide(&target("https://late.example/"), false)
            .await
        else {
            panic!("budget spent");
        };
        assert!(line.contains("ask the human in conversation"), "{line}");

        let unseen = Fixture::new(Some(Scripted::new(vec![LinkAskOutcome::Unavailable(
            "cannot".into(),
        )])));
        let fresh = Arc::new(GrantCoordinator::new());
        unseen.session.set_budget(fresh.clone());
        let _ = unseen
            .consent()
            .decide(&target("https://x.example/"), false)
            .await;
        assert!(
            fresh.charge_question().is_some() && !fresh.budget_exhausted(),
            "refunded"
        );
    }

    /// SPEC R-LINK.4: "always (any site)" is offered only where it would
    /// cover the link — never for a local host.
    #[tokio::test]
    async fn any_site_is_not_offered_for_a_local_host() {
        let asker = Scripted::new(vec![LinkAskOutcome::Answered(LinkChoice::AlwaysAny)]);
        let f = Fixture::new(Some(asker.clone()));
        let LinkAnswer::Refuse(_) = f
            .consent()
            .decide(&target("http://localhost:8080/"), false)
            .await
        else {
            panic!("an answer it did not offer is a deny");
        };
        let shown = asker.shown.lock().clone();
        assert!(!shown[0].1.contains(&LinkChoice::AlwaysAny));
        assert!(!f.settings.exists(), "nothing saved");
    }
}
