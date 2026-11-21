//! The browser as a surface (SPEC R-UI, R-GUI.6, R-GUI.8).
//!
//! [`WebFrontend`] is what `ahma_tui`'s [`Frontend`] looks like when the human
//! is a set of browser tabs: its inputs are what the HTTP handlers send it, and
//! presenting the state means projecting it into the page's wire types and
//! publishing the result for every open stream.
//!
//! It decides nothing. Every answer goes through [`ahma_tui::controller`], which
//! holds the rules; what it adds is what only a surface can know — which tab is
//! looking at a question, and whether anyone is looking at all.

use crate::auth::TabId;
use crate::dto::{WireIntent, WireLinkAnswer, WireReport, WireToolAnswer, WireWebAnswer};
use crate::project;
use ahma_tui::controller::{self, Intent, LinkChoice, Refused, Report, ToolAnswer, WebAnswer};
use ahma_tui::session::{Frontend, Session};
use ahma_tui::state::{AppState, GRANT_ARMING_DELAY, OpKey};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

/// How long the session keeps its hub subscription after the last tab closes: a
/// reload closes a stream and opens another, and must not drop and re-take the
/// subscription each time.
const ATTENDED_GRACE: Duration = Duration::from_secs(5);
/// Presentations closer together than this are merged: a streaming reply is a
/// state change per token, and the page needs a frame, not a token.
const PRESENT_EVERY: Duration = Duration::from_millis(30);

/// What the HTTP handlers send the frontend.
pub enum WebInput {
    /// A paired tab opened its event stream.
    TabOpened(TabId),
    /// Its stream closed.
    TabClosed(TabId),
    Intent {
        tab: TabId,
        intent: WireIntent,
        /// What became of it; the handler answers its request with this.
        reply: oneshot::Sender<Result<(), IntentError>>,
    },
    Report {
        tab: TabId,
        report: WireReport,
    },
}

/// Why an intent changed nothing, for the page to display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentError {
    Stale,
    NotArmed,
    NeedsConfirm,
    NotOffered,
    NotAllowed,
    Nothing,
}

impl From<Refused> for IntentError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::Stale => Self::Stale,
            Refused::NotArmed => Self::NotArmed,
            Refused::SaveLineNotShown | Refused::NeedsConfirmation => Self::NeedsConfirm,
            Refused::NotOffered => Self::NotOffered,
            Refused::NotAllowed => Self::NotAllowed,
            Refused::Nothing => Self::Nothing,
        }
    }
}

/// One region of the view, as published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub name: &'static str,
    pub json: String,
    /// The revision at which this region last changed.
    pub changed: u64,
}

/// The view as last published: every region's JSON, and when each last changed.
/// A stream sends the regions that changed after the revision it last sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Published {
    pub rev: u64,
    pub regions: Vec<Region>,
}

/// What a tab has told the server it is showing.
#[derive(Default)]
struct TabEdge {
    /// Questions this tab reports on screen, in a visible, focused document, and
    /// since when (SPEC R-GUI.6).
    visible_since: HashMap<String, Instant>,
}

pub struct WebFrontend {
    inputs: mpsc::Receiver<WebInput>,
    published: watch::Sender<Arc<Published>>,
    rev: u64,
    tabs: HashMap<TabId, TabEdge>,
    /// When the last tab went, while none is open.
    last_tab_closed: Option<Instant>,
    grace: Duration,
    arming: Duration,
    dirty: bool,
    last_present: Option<Instant>,
}

/// The handles the server holds.
pub struct WebHandles {
    pub inputs: mpsc::Sender<WebInput>,
    pub published: watch::Receiver<Arc<Published>>,
}

impl WebFrontend {
    pub fn new() -> (Self, WebHandles) {
        let (inputs_tx, inputs_rx) = mpsc::channel(64);
        let (published_tx, published_rx) = watch::channel(Arc::new(Published::default()));
        (
            Self {
                inputs: inputs_rx,
                published: published_tx,
                rev: 0,
                tabs: HashMap::new(),
                last_tab_closed: None,
                grace: ATTENDED_GRACE,
                arming: GRANT_ARMING_DELAY,
                dirty: false,
                last_present: None,
            },
            WebHandles {
                inputs: inputs_tx,
                published: published_rx,
            },
        )
    }

    /// Project the state and publish what changed. A revision is spent only when
    /// some region differs from what was published.
    fn flush(&mut self, state: &AppState) {
        self.dirty = false;
        self.last_present = Some(Instant::now());
        // A tab's record of what it showed is only worth keeping while that
        // question is still asked.
        let asked: Vec<&str> = state
            .scope_grant
            .iter()
            .map(|g| g.decision_id.as_str())
            .chain(
                state
                    .scope_grant_queue
                    .iter()
                    .map(|g| g.decision_id.as_str()),
            )
            .chain(state.web_approval.iter().map(|g| g.decision_id.as_str()))
            .chain(
                state
                    .web_approval_queue
                    .iter()
                    .map(|g| g.decision_id.as_str()),
            )
            .chain(state.link_ask.iter().map(|g| g.decision_id.as_str()))
            .chain(state.link_ask_queue.iter().map(|g| g.decision_id.as_str()))
            .collect();
        for tab in self.tabs.values_mut() {
            tab.visible_since
                .retain(|id, _| asked.contains(&id.as_str()));
        }

        let fresh = [
            ("header", serde_json::to_string(&project::header(state))),
            ("work", serde_json::to_string(&project::work(state))),
            ("chat", serde_json::to_string(&project::chat(state))),
            ("log", serde_json::to_string(&project::log(state))),
            ("gates", serde_json::to_string(&project::gates(state))),
        ];
        let previous = self.published.borrow().clone();
        let mut regions = Vec::with_capacity(fresh.len());
        let mut any_changed = false;
        for (name, json) in fresh {
            let Ok(json) = json else { continue };
            let before = previous.regions.iter().find(|r| r.name == name);
            let changed = match before {
                Some(r) if r.json == json => r.changed,
                _ => {
                    any_changed = true;
                    self.rev + 1
                }
            };
            regions.push(Region {
                name,
                json,
                changed,
            });
        }
        if any_changed {
            self.rev += 1;
            self.published.send_replace(Arc::new(Published {
                rev: self.rev,
                regions,
            }));
        }
    }

    fn require_read(&self, tab: TabId, decision_id: &str) -> Result<(), IntentError> {
        match self
            .tabs
            .get(&tab)
            .and_then(|t| t.visible_since.get(decision_id))
        {
            Some(since) if since.elapsed() >= self.arming => Ok(()),
            _ => Err(IntentError::NotArmed),
        }
    }

    fn intent(
        &mut self,
        tab: TabId,
        intent: WireIntent,
        session: &mut Session,
    ) -> Result<(), IntentError> {
        // A granting answer counts only from a tab that has had the question in
        // front of it, visible and focused, for the arming delay (R-GUI.6). The
        // session applies its own delay besides; denying never waits.
        match &intent {
            WireIntent::AnswerScopeGrant {
                decision_id,
                option,
            } if option != "deny" => self.require_read(tab, decision_id)?,
            WireIntent::AnswerWebApproval {
                decision_id,
                answer,
            } if *answer != WireWebAnswer::Deny => self.require_read(tab, decision_id)?,
            WireIntent::AnswerLinkAsk {
                decision_id,
                answer,
            } if *answer != WireLinkAnswer::Deny => self.require_read(tab, decision_id)?,
            _ => {}
        }
        controller::apply(&mut session.state, to_intent(intent)).map_err(IntentError::from)
    }

    fn report(&mut self, tab: TabId, report: WireReport, session: &mut Session) {
        match report {
            WireReport::GateVisible { decision_id } => {
                self.tabs
                    .entry(tab)
                    .or_default()
                    .visible_since
                    .entry(decision_id.clone())
                    .or_insert_with(Instant::now);
                controller::report(&mut session.state, Report::GatePresented { decision_id });
            }
            WireReport::GateHidden { decision_id } => {
                if let Some(edge) = self.tabs.get_mut(&tab) {
                    edge.visible_since.remove(&decision_id);
                }
            }
            WireReport::GateSaveLine {
                decision_id,
                visible,
            } => controller::report(
                &mut session.state,
                Report::GateSaveLine {
                    decision_id,
                    visible,
                },
            ),
        }
    }
}

/// The page's allow-listed intent as the controller's.
pub fn to_intent(wire: WireIntent) -> Intent {
    let op = |id: String, instance: Option<String>| OpKey {
        id,
        instance_id: instance,
    };
    match wire {
        WireIntent::Submit { text } => Intent::Submit { text },
        WireIntent::CancelTurn {} => Intent::CancelTurn,
        WireIntent::AnswerScopeGrant {
            decision_id,
            option,
        } => Intent::AnswerScopeGrant {
            decision_id,
            option,
        },
        WireIntent::ToggleScopeGrantDetail { decision_id } => {
            Intent::ToggleScopeGrantDetail { decision_id }
        }
        WireIntent::AnswerWebApproval {
            decision_id,
            answer,
        } => Intent::AnswerWebApproval {
            decision_id,
            answer: match answer {
                WireWebAnswer::Deny => WebAnswer::Deny,
                WireWebAnswer::Once => WebAnswer::Once,
                WireWebAnswer::Session => WebAnswer::Session,
            },
        },
        WireIntent::AnswerLinkAsk {
            decision_id,
            answer,
        } => Intent::AnswerLinkAsk {
            decision_id,
            choice: match answer {
                WireLinkAnswer::Deny => LinkChoice::Deny,
                WireLinkAnswer::Session => LinkChoice::Session,
            },
        },
        WireIntent::AnswerToolApproval { op_id, answer } => Intent::AnswerToolApproval {
            op_id,
            answer: match answer {
                WireToolAnswer::Reject => ToolAnswer::Reject,
                WireToolAnswer::Once => ToolAnswer::Once,
            },
        },
        WireIntent::CancelOp { id, instance } => Intent::CancelOp {
            op: op(id, instance),
        },
        WireIntent::ReRaiseGrant { id, instance } => Intent::ReRaiseGrant {
            op: op(id, instance),
        },
        WireIntent::OpenSection { key } => Intent::OpenSection { key },
        WireIntent::ChatWith { key } => Intent::ChatWith { key },
        WireIntent::ToggleOpOutput { id, instance } => Intent::ToggleOpOutput {
            op: op(id, instance),
        },
        WireIntent::ToggleProjectFilter {} => Intent::ToggleProjectFilter,
    }
}

impl Frontend for WebFrontend {
    type Input = WebInput;

    async fn next_input(&mut self) -> Option<WebInput> {
        self.inputs.recv().await
    }

    fn handle_input(&mut self, input: WebInput, session: &mut Session) -> Result<()> {
        match input {
            WebInput::TabOpened(tab) => {
                self.tabs.entry(tab).or_default();
                self.last_tab_closed = None;
                // Someone is looking: this session is a subscriber, and the hub
                // may route a permission question here (SPEC R-PERM.3.6).
                session.set_attended(true);
                self.dirty = true;
            }
            WebInput::TabClosed(tab) => {
                self.tabs.remove(&tab);
                if self.tabs.is_empty() {
                    self.last_tab_closed = Some(Instant::now());
                }
            }
            WebInput::Intent { tab, intent, reply } => {
                let result = self.intent(tab, intent, session);
                let _ = reply.send(result);
            }
            WebInput::Report { tab, report } => self.report(tab, report, session),
        }
        Ok(())
    }

    fn tick_interval(&self, state: &AppState) -> Duration {
        if self.dirty {
            PRESENT_EVERY
        } else if ahma_tui::app::chat_in_progress(state) {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(250)
        }
    }

    fn on_tick(&mut self, session: &mut Session) {
        // Nobody has been watching for a while: stop being a viewer.
        if self.tabs.is_empty()
            && self
                .last_tab_closed
                .is_some_and(|at| at.elapsed() >= self.grace)
        {
            session.set_attended(false);
            self.last_tab_closed = None;
        }
        if self.dirty {
            self.flush(&session.state);
        }
    }

    fn present(&mut self, state: &AppState) -> Result<()> {
        if self
            .last_present
            .is_none_or(|at| at.elapsed() >= PRESENT_EVERY)
        {
            self.flush(state);
        } else {
            self.dirty = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahma_common::scope_grant::{GrantReason, ScopeGrantRequest};
    use ahma_tui::session::run_session;
    use ahma_tui::state::{Capabilities, CommandPolicy, ScopeGrantGate};

    fn gui_caps() -> Capabilities {
        Capabilities {
            host_commands: false,
            persistent_answers: false,
            wait_for_presentation: true,
            commands: CommandPolicy::Only(&[]),
        }
    }

    /// A session for the browser with one scope question on screen that no tab
    /// has shown yet.
    fn session_with_a_question() -> Session {
        let mut state = AppState::new("http://127.0.0.1:9", "HTTP", true);
        state.surface = ahma_common::hub::Surface::Web;
        state.caps = gui_caps();
        let mut gate = ScopeGrantGate::from_request(ScopeGrantRequest {
            decision_id: "q".into(),
            path: "/opt/cache".into(),
            access: ahma_common::config::ScopeAccess::Rw,
            reason: GrantReason::StderrHeuristic,
            tool: Some("cargo".into()),
            context: Default::default(),
        });
        gate.restart_arming(true);
        state.scope_grant = Some(gate);
        Session::detached(state)
    }

    const TAB_A: TabId = TabId(1);
    const TAB_B: TabId = TabId(2);

    /// The value of the first granting answer this session may give.
    fn session_answer() -> String {
        ahma_common::grant_prompt::options_for(GrantReason::StderrHeuristic)
            .into_iter()
            .find(|o| {
                o.decision != ahma_common::scope_grant::GrantDecision::Deny
                    && !o.decision.tier().is_persistent()
            })
            .unwrap()
            .value
            .to_string()
    }

    fn saving_answer() -> String {
        ahma_common::grant_prompt::options_for(GrantReason::StderrHeuristic)
            .into_iter()
            .find(|o| o.decision.tier().is_persistent() && o.decision.access().is_some())
            .unwrap()
            .value
            .to_string()
    }

    /// Everything a test does to a running frontend goes through its inputs, as
    /// an HTTP handler's would.
    struct Driver {
        inputs: mpsc::Sender<WebInput>,
    }

    impl Driver {
        async fn open(&self, tab: TabId) {
            self.inputs.send(WebInput::TabOpened(tab)).await.unwrap();
        }

        async fn report(&self, tab: TabId, report: WireReport) {
            self.inputs
                .send(WebInput::Report { tab, report })
                .await
                .unwrap();
        }

        async fn intent(&self, tab: TabId, intent: WireIntent) -> Result<(), IntentError> {
            let (reply, answer) = oneshot::channel();
            self.inputs
                .send(WebInput::Intent { tab, intent, reply })
                .await
                .unwrap();
            answer.await.unwrap()
        }

        async fn visible(&self, tab: TabId) {
            self.report(
                tab,
                WireReport::GateVisible {
                    decision_id: "q".into(),
                },
            )
            .await;
        }

        /// [`Self::grant`] once the frontend counts the tab as having read the
        /// question. `visible` only queues its report, so how long ago the
        /// frontend recorded it depends on the scheduler: a fixed sleep raced
        /// it on a loaded machine. A refusal for not being armed changes
        /// nothing, so asking again until it is, within a bound, is safe.
        async fn grant_once_armed(&self, tab: TabId, option: &str) -> Result<(), IntentError> {
            use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
            let deadline = Instant::now() + TestTimeouts::get(TimeoutCategory::Quick);
            loop {
                match self.grant(tab, option).await {
                    Err(IntentError::NotArmed) if Instant::now() < deadline => {
                        tokio::time::sleep(TestTimeouts::poll_interval() / 10).await;
                    }
                    other => return other,
                }
            }
        }

        async fn grant(&self, tab: TabId, option: &str) -> Result<(), IntentError> {
            self.intent(
                tab,
                WireIntent::AnswerScopeGrant {
                    decision_id: "q".into(),
                    option: option.into(),
                },
            )
            .await
        }
    }

    /// Run `script` against a frontend over `session`, the way the server does:
    /// inputs arrive while the loop runs, and the loop ends when they stop.
    async fn run<F, Fut>(
        session: &mut Session,
        script: F,
    ) -> (WebFrontend, watch::Receiver<Arc<Published>>)
    where
        F: FnOnce(Driver, watch::Receiver<Arc<Published>>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let (mut frontend, handles) = WebFrontend::new();
        frontend.arming = Duration::from_millis(40);
        let published = handles.published.clone();
        let driver = Driver {
            inputs: handles.inputs,
        };
        let drive = async move {
            script(driver, handles.published).await;
        };
        let (result, ()) = tokio::join!(run_session(session, &mut frontend), drive);
        result.unwrap();
        (frontend, published)
    }

    /// A tab that has not reported the question on screen cannot grant it, and
    /// one that has still waits out the arming delay. The session's own delay
    /// is behind this one, so a surface cannot be the weaker of the two.
    #[tokio::test]
    async fn a_granting_answer_needs_the_tab_to_have_shown_the_question_for_the_delay() {
        let mut session = session_with_a_question();
        let answer = session_answer();
        run(&mut session, |d, _| async move {
            d.open(TAB_A).await;
            // Never shown to this tab.
            assert_eq!(d.grant(TAB_A, &answer).await, Err(IntentError::NotArmed));
            // Shown a moment ago.
            d.visible(TAB_A).await;
            assert_eq!(d.grant(TAB_A, &answer).await, Err(IntentError::NotArmed));
        })
        .await;
        assert!(
            session.state.scope_grant.is_some(),
            "nothing was taken from a tab that had not read it"
        );
    }

    /// Reading it is per tab: one tab that has had the question in front of it
    /// does not arm another that has not.
    #[tokio::test]
    async fn a_second_tab_that_never_showed_the_question_cannot_grant_it() {
        let mut session = session_with_a_question();
        let answer = session_answer();
        let (mut frontend, handles) = WebFrontend::new();
        frontend.arming = Duration::from_millis(40);
        let driver = Driver {
            inputs: handles.inputs,
        };
        let drive = async move {
            driver.open(TAB_A).await;
            driver.open(TAB_B).await;
            driver.visible(TAB_A).await;
            // A has had it in front of it long enough; the session agrees.
            tokio::time::sleep(Duration::from_millis(60)).await;
            assert_eq!(
                driver.grant(TAB_B, &answer).await,
                Err(IntentError::NotArmed),
                "B never showed it"
            );
            assert!(
                driver.grant(TAB_A, "deny").await.is_ok(),
                "denying never waits"
            );
        };
        let (result, ()) = tokio::join!(run_session(&mut session, &mut frontend), drive);
        result.unwrap();
        assert!(session.state.scope_grant.is_none(), "A's deny was taken");
    }

    /// Denying is always taken, from a tab that never reported anything.
    #[tokio::test]
    async fn denying_never_waits_for_a_tab_to_have_seen_it() {
        let mut session = session_with_a_question();
        run(&mut session, |d, _| async move {
            d.open(TAB_A).await;
            assert_eq!(d.grant(TAB_A, "deny").await, Ok(()));
        })
        .await;
        assert!(session.state.scope_grant.is_none());
    }

    /// A tab that reports the question hidden is no longer one that has read it.
    #[tokio::test]
    async fn a_tab_that_stops_showing_the_question_loses_its_standing() {
        let mut session = session_with_a_question();
        let answer = session_answer();
        let (mut frontend, handles) = WebFrontend::new();
        frontend.arming = Duration::from_millis(30);
        let driver = Driver {
            inputs: handles.inputs,
        };
        let drive = async move {
            driver.open(TAB_A).await;
            driver.visible(TAB_A).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            driver
                .report(
                    TAB_A,
                    WireReport::GateHidden {
                        decision_id: "q".into(),
                    },
                )
                .await;
            assert_eq!(
                driver.grant(TAB_A, &answer).await,
                Err(IntentError::NotArmed)
            );
        };
        let (result, ()) = tokio::join!(run_session(&mut session, &mut frontend), drive);
        result.unwrap();
        assert!(session.state.scope_grant.is_some());
    }

    /// A tab that has genuinely read the question can answer it, and the
    /// controller's own check agrees: both delays have passed.
    #[tokio::test]
    async fn a_tab_that_has_read_the_question_can_answer_it() {
        let mut session = session_with_a_question();
        let answer = session_answer();
        // A page load shows the question; the session starts its own delay.
        run(&mut session, |d, _| async move {
            d.open(TAB_A).await;
            d.visible(TAB_A).await;
        })
        .await;
        // Time passes: the session's delay is the real one.
        session.state.scope_grant.as_mut().unwrap().shown_at =
            Instant::now() - GRANT_ARMING_DELAY * 2;
        // Another page load shows it again, long enough, and answers.
        run(&mut session, |d, _| async move {
            d.open(TAB_A).await;
            d.visible(TAB_A).await;
            assert_eq!(d.grant_once_armed(TAB_A, &answer).await, Ok(()));
        })
        .await;
        assert!(session.state.scope_grant.is_none(), "the answer was taken");
    }

    /// A saved answer is not this surface's to give, whoever asks and however
    /// long it was read.
    #[tokio::test]
    async fn a_saved_answer_is_refused_whatever_the_tab_has_seen() {
        let mut session = session_with_a_question();
        let saving = saving_answer();
        let (mut frontend, handles) = WebFrontend::new();
        frontend.arming = Duration::ZERO;
        let driver = Driver {
            inputs: handles.inputs,
        };
        let drive = async move {
            driver.open(TAB_A).await;
            driver.visible(TAB_A).await;
            assert_eq!(
                driver.grant(TAB_A, &saving).await,
                Err(IntentError::NotAllowed)
            );
            // And one the question does not offer is refused by name.
            assert_eq!(
                driver.grant(TAB_A, "root-forever").await,
                Err(IntentError::NotOffered)
            );
            // A question that is not the one asked is stale.
            assert_eq!(
                driver
                    .intent(
                        TAB_A,
                        WireIntent::AnswerScopeGrant {
                            decision_id: "ghost".into(),
                            option: "deny".into()
                        }
                    )
                    .await,
                Err(IntentError::Stale)
            );
        };
        let (result, ()) = tokio::join!(run_session(&mut session, &mut frontend), drive);
        result.unwrap();
        assert!(session.state.scope_grant.is_some());
    }

    // ── a link a hooked command was refused ─────────────────────────────────

    /// A browser session over a private test home where a hooked command run by
    /// this (live) process, standing in for the harness, was refused a
    /// github.com link: the question is on screen, unshown, read from the
    /// records as a connected session reads them. Returns the home and the
    /// question's id.
    fn session_with_a_link_question() -> (tempfile::TempDir, Session, String) {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        ahma_common::harness_asks::record_link_refusal(
            &ahma_common::harness_asks::default_dir().unwrap(),
            &home.path().join("ws"),
            ahma_common::harness_asks::LinkRefusal {
                host: "github.com".into(),
                shown: "https://github.com/o/r/pull/12".into(),
                local: false,
                outside_egress: false,
                at: ahma_common::session_grants::now_secs(),
                harness_pid: Some(std::process::id()),
                command_digest: Some(ahma_common::harness_asks::CommandDigest::of(
                    "gh pr view 12 --web",
                )),
            },
        )
        .unwrap();
        let mut asks = ahma_tui::harness_asks::pending_links();
        assert_eq!(asks.len(), 1, "{asks:?}");
        let mut gate = ahma_tui::state::LinkAskGate::from_ask(asks.remove(0));
        gate.restart_arming(true);
        let id = gate.decision_id.clone();
        let mut session = session_with_a_question();
        session.state.scope_grant = None;
        session.state.link_ask = Some(gate);
        (home, session, id)
    }

    fn link_answer(id: &str, answer: WireLinkAnswer) -> WireIntent {
        WireIntent::AnswerLinkAsk {
            decision_id: id.into(),
            answer,
        }
    }

    /// The session's own arming delay has passed (the tab's is set to none).
    fn long_shown(session: &mut Session) {
        session.state.link_ask.as_mut().unwrap().shown_at = Instant::now() - GRANT_ARMING_DELAY * 2;
    }

    /// SPEC R-LINK.4, R-LINK.6, R-GUI.6, R-GUI.7: a tab that has had the link
    /// question in front of it — across the publishing of the view that drew
    /// it — can allow the site for this session: the hook-side session grant,
    /// given by `web`, and the refusal settled so neither this surface nor the
    /// harness dialog asks it again.
    #[test]
    fn a_tab_that_has_read_a_link_question_can_allow_its_site_for_this_session() {
        use ahma_common::permissions::{AuditAction, GrantKind, GrantTier};
        let (home, mut session, id) = session_with_a_link_question();
        let (mut frontend, _handles) = WebFrontend::new();
        frontend.arming = Duration::ZERO;
        frontend.tabs.entry(TAB_A).or_default();
        let session_answer = || link_answer(&id, WireLinkAnswer::Session);
        assert_eq!(
            frontend.intent(TAB_A, session_answer(), &mut session),
            Err(IntentError::NotArmed),
            "never shown to this tab"
        );
        frontend.report(
            TAB_A,
            WireReport::GateVisible {
                decision_id: id.clone(),
            },
            &mut session,
        );
        // The view is published between the report and the answer.
        frontend.flush(&session.state);
        long_shown(&mut session);
        assert_eq!(
            frontend.intent(TAB_A, session_answer(), &mut session),
            Ok(())
        );
        assert!(session.state.link_ask.is_none(), "answered");
        assert!(
            ahma_tui::harness_asks::pending_links().is_empty(),
            "settled: not asked again here or in the harness dialog"
        );
        let grants = ahma_common::open_url::active_sessions(
            &ahma_common::open_url::session_dir().unwrap(),
            ahma_common::session_grants::now_secs(),
            &|pid| pid == std::process::id(),
        );
        assert_eq!(grants.len(), 1, "{grants:?}");
        assert!(grants[0].covers("github.com", &home.path().join("ws")));
        assert_eq!(grants[0].granted_by.as_deref(), Some("web"));
        let audited: Vec<_> = ahma_common::permissions::read_audit_entries(
            &home.path().join(".ahma").join("permissions-audit.jsonl"),
        )
        .into_iter()
        .filter(|e| e.kind == GrantKind::OpenUrl)
        .map(|e| (e.action, e.tier, e.surface))
        .collect();
        assert_eq!(
            audited,
            [(AuditAction::Grant, GrantTier::Session, Some("web".into()))]
        );
    }

    /// SPEC R-GUI.6: reading a link question is per tab, as for every
    /// question — one that never showed it cannot allow it because another
    /// did — and denying never waits; a deny is remembered for the harness's
    /// session (R-LINK.6).
    #[test]
    fn a_tab_that_never_showed_a_link_question_cannot_allow_it_but_can_deny_it() {
        let (_home, mut session, id) = session_with_a_link_question();
        let (mut frontend, _handles) = WebFrontend::new();
        frontend.arming = Duration::ZERO;
        frontend.tabs.entry(TAB_A).or_default();
        frontend.tabs.entry(TAB_B).or_default();
        frontend.report(
            TAB_A,
            WireReport::GateVisible {
                decision_id: id.clone(),
            },
            &mut session,
        );
        frontend.flush(&session.state);
        long_shown(&mut session);
        assert_eq!(
            frontend.intent(
                TAB_B,
                link_answer(&id, WireLinkAnswer::Session),
                &mut session
            ),
            Err(IntentError::NotArmed),
            "B never showed it"
        );
        assert!(session.state.link_ask.is_some());
        assert_eq!(
            frontend.intent(TAB_B, link_answer(&id, WireLinkAnswer::Deny), &mut session),
            Ok(())
        );
        assert!(session.state.link_ask.is_none());
        let asks = ahma_common::harness_asks::load_all(
            &ahma_common::harness_asks::default_dir().unwrap(),
            ahma_common::session_grants::now_secs(),
        );
        assert!(
            asks.iter()
                .any(|a| ahma_common::harness_asks::link_denied_for(
                    a,
                    Some(std::process::id()),
                    "github.com"
                )),
            "denied for the harness's session"
        );
    }

    /// Text typed into chat reaches the controller, which refuses what this
    /// surface may not do: a host command and a command outside its list.
    #[tokio::test]
    async fn text_that_asks_for_more_than_this_surface_may_do_is_refused_by_the_controller() {
        let mut session = session_with_a_question();
        session.state.scope_grant = None;
        run(&mut session, |d, _| async move {
            d.open(TAB_A).await;
            for text in [
                "!touch /tmp/escaped",
                "/setup set tools.timeout_secs 1",
                "/quit",
            ] {
                assert_eq!(
                    d.intent(TAB_A, WireIntent::Submit { text: text.into() })
                        .await,
                    Ok(()),
                    "accepted as text, answered with a notice"
                );
            }
        })
        .await;
        assert!(session.state.windows.is_empty(), "no command ran");
        assert!(!session.state.should_quit);
        let notices = session
            .state
            .chat
            .entries()
            .iter()
            .filter(|e| matches!(e, ahma_tui::state::ChatEntry::Assistant { content, .. }
                if content.contains("not available here") || content.contains("outside the sandbox")))
            .count();
        assert_eq!(notices, 3, "each was refused where it was parsed");
    }

    /// What is published is the projection, with a revision spent only when a
    /// region differs, and each region says when it last changed.
    #[tokio::test]
    async fn the_view_is_published_and_a_revision_is_spent_only_on_change() {
        let mut session = session_with_a_question();
        let (_, published) = run(&mut session, |d, published| async move {
            d.open(TAB_A).await;
            // A moment for the first presentation.
            tokio::time::sleep(Duration::from_millis(80)).await;
            let first = published.borrow().clone();
            assert!(first.rev >= 1);
            let names: Vec<_> = first.regions.iter().map(|r| r.name).collect();
            assert_eq!(names, ["header", "work", "chat", "log", "gates"]);
            let gates = &first
                .regions
                .iter()
                .find(|r| r.name == "gates")
                .unwrap()
                .json;
            assert!(gates.contains(r#""kind":"scope""#) && gates.contains(r#""decision_id":"q""#));

            // Nothing changed: no new revision.
            let _ = d.intent(TAB_A, WireIntent::ToggleProjectFilter {}).await;
            let _ = d.intent(TAB_A, WireIntent::ToggleProjectFilter {}).await;
            tokio::time::sleep(Duration::from_millis(80)).await;
            let steady = published.borrow().clone();
            // Toggling twice returned to where it was.
            assert_eq!(
                steady.regions.iter().find(|r| r.name == "gates"),
                first.regions.iter().find(|r| r.name == "gates"),
            );

            // Answering changes the gates region and only what it touches.
            assert_eq!(d.grant(TAB_A, "deny").await, Ok(()));
            tokio::time::sleep(Duration::from_millis(80)).await;
            let after = published.borrow().clone();
            assert!(after.rev > steady.rev);
            let gates_after = after.regions.iter().find(|r| r.name == "gates").unwrap();
            assert_eq!(
                gates_after.changed, after.rev,
                "gates changed at this revision"
            );
            assert!(gates_after.json.contains(r#""top":null"#));
            let header = after.regions.iter().find(|r| r.name == "header").unwrap();
            assert!(header.changed < after.rev, "the header did not change");
        })
        .await;
        assert!(published.borrow().rev >= 2);
    }

    /// The session subscribes to the hub only while a tab is open, keeps the
    /// subscription across a reload, and lets it go when nobody is back
    /// (SPEC R-GUI.8).
    #[tokio::test]
    async fn the_session_is_attended_only_while_a_tab_is_open() {
        let mut session = session_with_a_question();
        session.state.scope_grant = None;
        assert!(!session.is_attended());
        let (mut frontend, handles) = WebFrontend::new();
        frontend.grace = Duration::from_millis(60);
        let driver = Driver {
            inputs: handles.inputs.clone(),
        };
        let inputs = handles.inputs;
        let drive = async move {
            driver.open(TAB_A).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            // A reload: the stream closes and another opens at once.
            inputs.send(WebInput::TabClosed(TAB_A)).await.unwrap();
            driver.open(TAB_B).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            // Still attended: a tab is open, however long the grace.
            inputs.send(WebInput::TabClosed(TAB_B)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        };
        let (result, ()) = tokio::join!(run_session(&mut session, &mut frontend), drive);
        result.unwrap();
        assert!(
            !session.is_attended(),
            "nobody has been watching for longer than the grace"
        );
    }

    #[tokio::test]
    async fn a_session_with_a_tab_open_is_attended() {
        let mut session = session_with_a_question();
        session.state.scope_grant = None;
        let (mut frontend, handles) = WebFrontend::new();
        let driver = Driver {
            inputs: handles.inputs,
        };
        let drive = async move {
            driver.open(TAB_A).await;
            tokio::time::sleep(Duration::from_millis(40)).await;
        };
        let (result, ()) = tokio::join!(run_session(&mut session, &mut frontend), drive);
        result.unwrap();
        assert!(session.is_attended(), "a tab is open when the loop ends");
    }

    /// An intent is the controller's, field for field.
    #[test]
    fn a_wire_intent_becomes_the_controllers() {
        assert_eq!(
            to_intent(WireIntent::CancelOp {
                id: "o".into(),
                instance: Some("i".into())
            }),
            Intent::CancelOp {
                op: OpKey {
                    id: "o".into(),
                    instance_id: Some("i".into())
                }
            }
        );
        assert_eq!(
            to_intent(WireIntent::AnswerToolApproval {
                op_id: "o".into(),
                answer: WireToolAnswer::Once
            }),
            Intent::AnswerToolApproval {
                op_id: "o".into(),
                answer: ToolAnswer::Once
            }
        );
    }
}
