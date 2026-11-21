//! The session, and the one loop every surface runs on (SPEC R-UI).
//!
//! A [`Session`] is the state of one human's view of ahma and the connections
//! that feed it: what the hub and the MCP server report, what local tasks
//! finish, the ticks that keep time-based things moving. It knows nothing of
//! terminals or browsers. A [`Frontend`] is the surface: it waits for the
//! human's input, decodes it into the controller's [`crate::controller::Intent`]
//! and [`crate::controller::Report`], and presents the state. [`run_session`] is
//! the loop that joins the two, so `ahma tui` and `ahma web` differ in what
//! they draw and read, not in how a session behaves.

use crate::connection::ResolvedConnection;
use crate::llm_bridge::BridgeEvent;
use crate::mcp_source::SourceEvent;
use crate::state::{AppState, Capabilities};
use anyhow::Result;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How many queued source or bridge events one pass drains before presenting.
/// Batching turns a firehose of per-line output (a chatty build) into one
/// presentation per pass instead of one per event, while the bound keeps input
/// and ticks responsive during a flood.
const MAX_EVENT_DRAIN_PER_PASS: usize = 256;

/// What a session is started with.
pub struct SessionOptions {
    pub profile_override: Option<String>,
    pub workspace_path: Option<std::path::PathBuf>,
    pub token_prefs: crate::TokenPrefs,
    /// A command run before the first frame, as if typed (`ahma setup` opens
    /// `/setup`).
    pub initial_command: Option<String>,
    pub unicode: bool,
    /// Which surface this is, and what it may do (SPEC R-GUI.9).
    pub surface: ahma_common::hub::Surface,
    pub caps: Capabilities,
}

/// The next thing for a session to handle that did not come from the human.
///
/// Not boxed: a source event is moved once, from the channel to its handler, and
/// a build's output is one of these per line — an allocation each would cost
/// more than the bytes it saves.
#[allow(clippy::large_enum_variant)]
pub enum SessionInput {
    Source(SourceEvent),
    Bridge(BridgeEvent),
    /// Nothing happened for a while; time-based work runs.
    Tick,
}

/// One human's view of ahma.
pub struct Session {
    pub state: AppState,
    mcp_tx: mpsc::Sender<SourceEvent>,
    mcp_rx: mpsc::Receiver<SourceEvent>,
    bridge_rx: mpsc::Receiver<BridgeEvent>,
    /// The hub subscription, while someone is attached (see
    /// [`Self::set_attended`]).
    hub_source: Option<tokio::task::JoinHandle<()>>,
    /// The MCP source task; [`Self::close`] waits for it to end the hub MCP
    /// session.
    mcp_source: Option<tokio::task::JoinHandle<()>>,
    last_discovery: Instant,
    /// Questions a refused terminal-hook command left for the harness dialog
    /// (SPEC R-PERM.10(e)) are re-read on the tick; `None` reads on the first.
    last_harness_ask_poll: Option<Instant>,
    /// Where those questions are read: this machine's records for a connected
    /// session, nothing for a detached one.
    harness_asks: fn() -> Vec<crate::harness_asks::HarnessAsk>,
    /// Where the links refused terminal-hook commands left are read (SPEC
    /// R-LINK.6): this machine's records for a connected session on any
    /// surface ([`harness_link_source`]), nothing for a detached one.
    harness_link_asks: fn() -> Vec<crate::harness_asks::HarnessLinkAsk>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // A subscription that outlives its session would count as an attached
        // human with nobody there (SPEC R-PERM.3.6).
        if let Some(task) = self.hub_source.take() {
            task.abort();
        }
    }
}

/// Where a connected session on `surface` reads the links refused
/// terminal-hook commands left (SPEC R-LINK.6): this machine's records, on
/// every surface, since every one draws the question — the terminal with its
/// keys, the browser and ahma's own window with the answers a surface that
/// may not save can give (R-GUI.9). A question is read only where it can be
/// drawn: one waiting unseen would hold the gate. A new surface decides here.
fn harness_link_source(
    surface: ahma_common::hub::Surface,
) -> fn() -> Vec<crate::harness_asks::HarnessLinkAsk> {
    use ahma_common::hub::Surface;
    match surface {
        Surface::Tui | Surface::Web | Surface::Desktop => crate::harness_asks::pending_links,
    }
}

impl Session {
    /// Start a session: build the state, connect the sources, and ask the
    /// questions a first look asks.
    ///
    /// The hub subscription is not started here: the caller says when someone
    /// is attached ([`Self::set_attended`]). The terminal is attached from the
    /// first frame; a browser session only while a tab is open.
    pub fn start(connection: &ResolvedConnection, options: SessionOptions) -> Self {
        use crate::llm_bridge::spawn_discovery_task;
        use crate::mcp_source::spawn_mcp_source_with_handle;

        let SessionOptions {
            profile_override,
            workspace_path,
            token_prefs,
            initial_command,
            unicode,
            surface,
            caps,
        } = options;
        let mut state = AppState::new(
            &connection.display_url,
            connection.transport_label(),
            unicode,
        );
        state.surface = surface;
        state.caps = caps;
        if let Some(ref path) = workspace_path {
            // Canonicalized like `project_root` below: this string becomes the chat
            // session's `McpChatConfig.workspace_root` and hence its `roots/list`
            // answer, so a relative or symlinked spelling here would hand the server
            // a different-looking scope than the one the bridge locked.
            state.workspace = dunce::canonicalize(path)
                .unwrap_or_else(|_| path.clone())
                .to_string_lossy()
                .into_owned();
        }
        // Project root for the task-tree filter: the explicit path argument, else
        // the directory the session was started from. Canonicalized so it compares
        // against instance sandbox scopes (which are canonicalized at lock time).
        state.project_root = workspace_path
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .map(|p| {
                std::fs::canonicalize(&p)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            });
        state.mcp_http_base_url = crate::app::http_base_url(connection);
        state.token_prefs = token_prefs;
        // Cache the effective minimize state (flag > env > settings) for the status
        // bar and the `/minimize` switch, so neither has to re-read settings.
        state.minimize_tokens = crate::app::resolve_token_prefs(&state).0;

        if let Some(profile_name) = profile_override
            && let Ok(cwd) = std::env::current_dir()
        {
            if let Ok(profile) = crate::agent_config::get_profile(&cwd, &profile_name) {
                state.active_profile = Some(profile.name.clone());
                state.current_provider_url = Some(profile.provider_url);
                state.llm_selection = Some(crate::state::LlmSelection::profile(
                    profile.name.clone(),
                    profile.model,
                ));
            } else {
                // The user asked for a specific profile by name and did not get it;
                // opening as if they had never passed the flag is a surprise.
                crate::startup_notices::push(
                    crate::startup_notices::Level::Warn,
                    format!(
                        "--profile {profile_name} could not be loaded; continuing without it. \
                         Use /agent list to see the profiles saved for this project."
                    ),
                );
            }
        }

        // Everything that happened before the first frame (bridge restarts, a self
        // re-exec, a refused reuse, this profile failure) surfaces here rather than
        // in a log file the user was never told about.
        crate::app::drain_startup_notices(&mut state);
        crate::app::load_granted_scopes(&mut state);
        // Trusting a folder and the first-run tour write to the user's files:
        // only a surface that may save answers asks (SPEC R-GUI.9).
        if state.caps.persistent_answers {
            crate::app::offer_folder_trust(&mut state);
            crate::app::maybe_show_intro_on_first_run(&mut state);
        }
        // `ahma setup` opens straight onto /setup. After the trust question and the
        // intro so neither is skipped; the command is the one the user could type.
        if let Some(command) = initial_command {
            crate::app::dispatch_nav_command(&command, &mut state);
            // `ahma setup` is a task, not a session: when its view is done, so is
            // the program. (`/setup` typed inside a running TUI just closes it.)
            state.setup_exit_on_close = state.setup_view.open;
        }

        let (mcp_tx, mcp_rx) = mpsc::channel::<SourceEvent>(256);
        let (mcp_source_tx, mcp_source) =
            spawn_mcp_source_with_handle(connection.clone(), mcp_tx.clone(), workspace_path);
        state.mcp_source_tx = Some(mcp_source_tx);
        // A session is a subscriber, never the hub (SPEC R-HUB.9): the per-user
        // hub owns the hub, and a session that goes away sends nothing but EOF.
        //
        // A second, outgoing connection only where work happens here: a `!`
        // command runs in the terminal, unsandboxed, and used to be the one kind
        // of work the unified view could not see (SPEC R-HUB.9). A surface that
        // may not run one has nothing to report.
        if state.caps.host_commands {
            state.tui_reporter = Some(crate::tui_reporter::spawn_tui_reporter(
                state.workspace.clone(),
            ));
        }

        // The bridge channel carries provider discovery results and LLM tokens.
        let (bridge_tx, bridge_rx) = mpsc::channel::<BridgeEvent>(512);
        spawn_discovery_task(bridge_tx.clone());
        // Populate the external MCP tools counter at startup (avoids needing `/mcp refresh`).
        crate::llm_bridge::spawn_external_tools_refresh(
            state.mcp_connections.clone(),
            bridge_tx.clone(),
        );
        // Store the sender so chat actions can spawn tasks later.
        state.bridge_tx = Some(bridge_tx);

        Self {
            state,
            mcp_tx,
            mcp_rx,
            bridge_rx,
            hub_source: None,
            mcp_source: Some(mcp_source),
            last_discovery: Instant::now(),
            last_harness_ask_poll: None,
            harness_asks: crate::harness_asks::pending,
            harness_link_asks: harness_link_source(surface),
        }
    }

    /// End the session cleanly: close the MCP source's command channel and
    /// wait, at most `within`, for it to end the hub MCP session, so the
    /// hub stops that session's worker now instead of when it idles out.
    pub async fn close(mut self, within: std::time::Duration) {
        self.state.mcp_source_tx = None;
        if let Some(task) = self.mcp_source.take()
            && tokio::time::timeout(within, task).await.is_err()
        {
            tracing::debug!("session: the MCP source did not finish within {within:?}");
        }
    }

    /// A session over `state` with no connections: what a test drives, feeding
    /// it events by hand. It reads nothing from this machine, the questions
    /// terminal hooks left included: give it those with
    /// [`Self::with_harness_asks`].
    pub fn detached(state: AppState) -> Self {
        let (mcp_tx, mcp_rx) = mpsc::channel(256);
        let (_bridge_tx, bridge_rx) = mpsc::channel(512);
        Self {
            state,
            mcp_tx,
            mcp_rx,
            bridge_rx,
            hub_source: None,
            mcp_source: None,
            last_discovery: Instant::now(),
            last_harness_ask_poll: None,
            harness_asks: Vec::new,
            harness_link_asks: Vec::new,
        }
    }

    /// Read the questions terminal hooks left from `source` instead.
    pub fn with_harness_asks(
        mut self,
        source: fn() -> Vec<crate::harness_asks::HarnessAsk>,
    ) -> Self {
        self.harness_asks = source;
        self
    }

    /// Read the links terminal hooks left from `source` instead.
    pub fn with_harness_link_asks(
        mut self,
        source: fn() -> Vec<crate::harness_asks::HarnessLinkAsk>,
    ) -> Self {
        self.harness_link_asks = source;
        self
    }

    /// Say whether a human is attached. The hub routes a permission question to
    /// a surface only while it counts one watching (SPEC R-PERM.3.6), so a
    /// session subscribes while attached and leaves the moment it is not.
    /// Subscribing again replays what the hub keeps, so nothing is lost by
    /// having been away.
    pub fn set_attended(&mut self, attended: bool) {
        match (attended, self.hub_source.is_some()) {
            (true, false) => {
                self.hub_source = Some(crate::hub_source::spawn_hub_source(self.mcp_tx.clone()));
            }
            (false, true) => {
                if let Some(task) = self.hub_source.take() {
                    task.abort();
                }
            }
            _ => {}
        }
    }

    /// Whether the hub subscription is running.
    pub fn is_attended(&self) -> bool {
        self.hub_source.is_some()
    }

    /// Wait for the next thing that did not come from the human: an event from a
    /// source or a local task, or `tick` of quiet.
    pub async fn recv(&mut self, tick: Duration) -> SessionInput {
        tokio::select! {
            biased;
            Some(event) = self.mcp_rx.recv() => SessionInput::Source(event),
            Some(event) = self.bridge_rx.recv() => SessionInput::Bridge(event),
            _ = tokio::time::sleep(tick) => SessionInput::Tick,
        }
    }

    /// Apply one input.
    pub fn handle(&mut self, input: SessionInput) {
        match input {
            SessionInput::Source(event) => crate::app::handle_source_event(event, &mut self.state),
            SessionInput::Bridge(event) => crate::app::handle_bridge_event(event, &mut self.state),
            SessionInput::Tick => self.tick(),
        }
    }

    /// Time-based work: look for local model servers again now and then while
    /// idle (Ollama started after the session, LM Studio quit), let finished
    /// command windows go, and pulse the liveness spinner while a turn is quiet.
    fn tick(&mut self) {
        let now = Instant::now();
        let state = &mut self.state;
        if !crate::app::chat_in_progress(state)
            && now.duration_since(self.last_discovery) >= crate::app::PROVIDER_REDISCOVERY_EVERY
        {
            self.last_discovery = now;
            if let Some(tx) = state.bridge_tx.clone() {
                crate::llm_bridge::spawn_discovery_task(tx);
            }
        }
        for w in &mut state.windows {
            if w.visible
                && w.finished_at
                    .is_some_and(|t| now.duration_since(t) >= Duration::from_secs(300))
            {
                w.visible = false;
            }
        }
        // While a turn is in flight but quiet, pulse the liveness spinner slowly
        // so the human can tell it is still alive (and not silently timed out);
        // token arrivals drive it fast.
        if crate::app::chat_in_progress(state) {
            state.tick_waiting_spinner();
        }
    }

    /// What every pass does before presenting: re-read the harness questions
    /// (throttled, and checked every pass so a busy stream cannot starve it),
    /// then drain the ready backlog so a burst costs one presentation.
    pub fn settle(&mut self) {
        let now = Instant::now();
        if self
            .last_harness_ask_poll
            .is_none_or(|t| now.duration_since(t) >= crate::harness_asks::POLL_EVERY)
        {
            self.last_harness_ask_poll = Some(now);
            crate::app::sync_harness_asks(&mut self.state, (self.harness_asks)());
            crate::app::sync_harness_link_asks(&mut self.state, (self.harness_link_asks)());
        }
        let mut drained = 0usize;
        while drained < MAX_EVENT_DRAIN_PER_PASS
            && let Ok(event) = self.mcp_rx.try_recv()
        {
            crate::app::handle_source_event(event, &mut self.state);
            drained += 1;
        }
        while drained < MAX_EVENT_DRAIN_PER_PASS
            && let Ok(event) = self.bridge_rx.try_recv()
        {
            crate::app::handle_bridge_event(event, &mut self.state);
            drained += 1;
        }
    }
}

/// A surface: where a human's input comes from and where the state is shown.
///
/// What it must do is small, and none of it is a rule. It decodes what its
/// human did into [`crate::controller::Intent`]s and
/// [`crate::controller::Report`]s and hands them to [`crate::controller`]; it
/// shows the state; it says when it is gone. Whether an answer is taken, what a
/// session may do and when a granting key counts are the controller's, so they
/// are the same on every surface.
pub trait Frontend {
    /// The surface's own raw input: a terminal event, a browser's request.
    type Input;

    /// Wait for the next input; `None` when the surface is gone. Cancel-safe.
    fn next_input(&mut self) -> impl std::future::Future<Output = Option<Self::Input>>;

    /// Act on one input: decode it and apply it to the session.
    fn handle_input(&mut self, input: Self::Input, session: &mut Session) -> Result<()>;

    /// How long to wait in quiet before the next tick. A surface that animates
    /// asks for a short one; the default is for one that does not.
    fn tick_interval(&self, state: &AppState) -> Duration {
        if crate::app::chat_in_progress(state) {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(250)
        }
    }

    /// Called after every tick, for what only this surface has to keep moving.
    fn on_tick(&mut self, _session: &mut Session) {}

    /// Show the state. Called once before the loop and once after every pass.
    fn present(&mut self, state: &AppState) -> Result<()>;
}

/// Run `session` on `frontend` until the session quits or the surface goes.
pub async fn run_session<F: Frontend>(session: &mut Session, frontend: &mut F) -> Result<()> {
    // Moved once into the match below; see `SessionInput` for why it is not boxed.
    #[allow(clippy::large_enum_variant)]
    enum Woke<I> {
        Input(Option<I>),
        Session(SessionInput),
    }

    frontend.present(&session.state)?;
    loop {
        let tick = frontend.tick_interval(&session.state);
        let woke = tokio::select! {
            biased;
            input = frontend.next_input() => Woke::Input(input),
            input = session.recv(tick) => Woke::Session(input),
        };
        match woke {
            Woke::Input(Some(input)) => frontend.handle_input(input, session)?,
            Woke::Input(None) => break,
            Woke::Session(input) => {
                let ticked = matches!(input, SessionInput::Tick);
                session.handle(input);
                if ticked {
                    frontend.on_tick(session);
                }
            }
        }
        session.settle();
        frontend.present(&session.state)?;
        if session.state.should_quit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{Intent, Report};

    /// Closing a session lets its MCP source finish: the source ends the hub
    /// MCP session (DELETE) when its command channel closes, and on a
    /// current-thread runtime that is about to be dropped, a task that is not
    /// awaited is cancelled before it can, leaving the hub's worker for that
    /// session running until the session idles out.
    #[tokio::test]
    async fn closing_a_session_waits_for_its_mcp_source_to_finish() {
        let mut session = Session::detached(AppState::new("http://localhost:3000", "HTTP", true));
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<crate::mcp_source::McpSourceCommand>(1);
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
        session.state.mcp_source_tx = Some(cmd_tx);
        session.mcp_source = Some(tokio::spawn(async move {
            // What the real source does: run until its channel closes, then
            // clean up before returning.
            while cmd_rx.recv().await.is_some() {}
            tokio::task::yield_now().await;
            let _ = done_tx.send(());
        }));
        session
            .close(ahma_common::timeouts::TestTimeouts::get(
                ahma_common::timeouts::TimeoutCategory::Quick,
            ))
            .await;
        assert!(
            done_rx.try_recv().is_ok(),
            "the source finished its clean-up before close returned"
        );
    }

    /// What a scripted surface does next.
    enum Step {
        Intent(Intent),
        Report(Report),
        /// Pause the surface so the session's own work can run.
        Idle,
    }

    /// A surface that plays a script and records what it was asked to show.
    struct Scripted {
        script: std::collections::VecDeque<Step>,
        presented: usize,
        handled: Vec<&'static str>,
        ticks: usize,
    }

    impl Scripted {
        fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            Self {
                script: steps.into_iter().collect(),
                presented: 0,
                handled: vec![],
                ticks: 0,
            }
        }
    }

    impl Frontend for Scripted {
        type Input = Step;

        async fn next_input(&mut self) -> Option<Step> {
            match self.script.pop_front() {
                Some(Step::Idle) => std::future::pending().await,
                other => other,
            }
        }

        fn handle_input(&mut self, input: Step, session: &mut Session) -> Result<()> {
            match input {
                Step::Intent(intent) => {
                    self.handled.push("intent");
                    let _ = crate::controller::apply(&mut session.state, intent);
                }
                Step::Report(report) => {
                    self.handled.push("report");
                    crate::controller::report(&mut session.state, report);
                }
                Step::Idle => {}
            }
            Ok(())
        }

        fn tick_interval(&self, _state: &AppState) -> Duration {
            Duration::from_millis(5)
        }

        fn on_tick(&mut self, _session: &mut Session) {
            self.ticks += 1;
        }

        fn present(&mut self, _state: &AppState) -> Result<()> {
            self.presented += 1;
            Ok(())
        }
    }

    fn detached() -> Session {
        Session::detached(AppState::new("http://127.0.0.1:9", "HTTP", true))
    }

    /// The surface is shown the state once before anything happens and once
    /// after every input, so what it shows is never older than the last thing
    /// it did.
    #[tokio::test]
    async fn the_loop_presents_after_every_event() {
        let mut session = detached();
        let mut front = Scripted::new([
            Step::Intent(Intent::Submit {
                text: "/help".into(),
            }),
            Step::Intent(Intent::Submit {
                text: "/help".into(),
            }),
        ]);
        run_session(&mut session, &mut front).await.unwrap();
        // First frame, one per input.
        assert_eq!(front.presented, 3);
        assert_eq!(front.handled, ["intent", "intent"]);
    }

    /// A surface that is gone ends the loop; nothing is left waiting on it.
    #[tokio::test]
    async fn the_loop_ends_when_the_frontend_closes() {
        let mut session = detached();
        let mut front = Scripted::new([]);
        let ended = tokio::time::timeout(
            ahma_common::timeouts::TestTimeouts::scale_secs(5),
            run_session(&mut session, &mut front),
        )
        .await;
        assert!(ended.expect("the loop must end").is_ok());
    }

    /// A report and the intent after it are applied in that order: an answer
    /// that depends on what the surface showed sees it. The save-line report
    /// arrives before the second press, and the press is taken.
    #[tokio::test]
    async fn a_report_is_applied_before_the_next_intent() {
        use crate::state::ScopeGrantGate;
        use ahma_common::scope_grant::{GrantReason, ScopeGrantRequest};
        let mut session = detached();
        let mut gate = ScopeGrantGate::from_request(ScopeGrantRequest {
            decision_id: "q".into(),
            path: "/opt/cache".into(),
            access: ahma_common::config::ScopeAccess::Rw,
            reason: GrantReason::StderrHeuristic,
            tool: None,
            context: Default::default(),
        });
        gate.shown_at = Instant::now() - crate::state::GRANT_ARMING_DELAY * 2;
        session.state.scope_grant = Some(gate);
        let saving = ahma_common::grant_prompt::options_for(GrantReason::StderrHeuristic)
            .into_iter()
            .find(|o| o.decision.tier().is_persistent() && o.decision.access().is_some())
            .unwrap()
            .value;
        let press = || {
            Step::Intent(Intent::AnswerScopeGrant {
                decision_id: "q".into(),
                option: saving.to_string(),
            })
        };
        // The first press brings the line into view and saves nothing; the
        // surface then reports it shown; the second press saves.
        let mut front = Scripted::new([
            press(),
            Step::Report(Report::GateSaveLine {
                decision_id: "q".into(),
                visible: true,
            }),
            press(),
        ]);
        run_session(&mut session, &mut front).await.unwrap();
        assert!(
            session.state.scope_grant.is_none(),
            "answered by the second press"
        );
        assert_eq!(front.handled, ["intent", "report", "intent"]);
    }

    /// A session takes the questions refused terminal-hook commands left only
    /// from its own source: a detached one reads nothing from this machine, so
    /// a record another process left cannot take the screen in a test (it did,
    /// on a Windows runner, after the question under test was answered); a
    /// session given a source shows what that source holds.
    #[tokio::test]
    async fn a_session_takes_harness_questions_only_from_its_source() {
        fn one() -> Vec<crate::harness_asks::HarnessAsk> {
            vec![crate::harness_asks::HarnessAsk {
                workspace: std::env::temp_dir().join("ws"),
                question: ahma_common::harness_asks::Question {
                    dir: std::env::temp_dir().join("elsewhere"),
                    access: ahma_common::config::ScopeAccess::Ro,
                    covers: vec![],
                    programs: vec![],
                },
                harness_pid: std::process::id(),
            }]
        }
        let mut quiet = detached();
        run_session(&mut quiet, &mut Scripted::new([Step::Idle]))
            .await
            .unwrap();
        assert!(quiet.state.scope_grant.is_none(), "read nothing");

        let mut fed = detached().with_harness_asks(one);
        run_session(&mut fed, &mut Scripted::new([Step::Idle]))
            .await
            .unwrap();
        assert!(
            fed.state.scope_grant.is_some(),
            "showed what its source held"
        );
    }

    /// SPEC R-LINK.6, R-GUI.9: every surface draws the question about a link
    /// a hooked command was refused — the terminal, the browser and ahma's own
    /// window — so a connected session on any of them reads this machine's
    /// records, and a refusal waiting for a live harness reaches it.
    #[test]
    fn every_surface_reads_the_links_hooked_commands_were_refused() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let dir = ahma_common::harness_asks::default_dir().unwrap();
        ahma_common::harness_asks::record_link_refusal(
            &dir,
            &home.path().join("ws"),
            ahma_common::harness_asks::LinkRefusal {
                host: "github.com".into(),
                shown: "https://github.com/o/r/pull/1".into(),
                local: false,
                outside_egress: false,
                at: ahma_common::session_grants::now_secs(),
                harness_pid: Some(std::process::id()),
                command_digest: None,
            },
        )
        .unwrap();
        use ahma_common::hub::Surface;
        for surface in [Surface::Tui, Surface::Web, Surface::Desktop] {
            let asks = harness_link_source(surface)();
            assert_eq!(asks.len(), 1, "{surface:?}: {asks:?}");
            assert_eq!(asks[0].refusal.host, "github.com", "{surface:?}");
        }
    }

    /// Quiet is time: a tick reaches the surface, so what only it animates
    /// keeps moving, and the session's own time-based work runs.
    #[tokio::test]
    async fn a_quiet_surface_is_ticked() {
        let mut session = detached();
        // A finished window goes after five minutes of being finished.
        let now = Instant::now();
        let mut window = crate::state::TuiWindow {
            id: 1,
            label: "done".into(),
            status: crate::state::WindowStatus::Finished,
            content: vec![],
            collapsed: false,
            finished_at: Some(now - Duration::from_secs(301)),
            duration_ms: None,
            last_output_at: None,
            runner: crate::state::WindowRunner::Host,
            command: "true".into(),
            working_dir: ".".into(),
            llm_model: None,
            visible: true,
            abort_tx: Default::default(),
            op_id: None,
        };
        window.visible = true;
        session.state.windows.push(window);
        let mut front = Scripted::new([Step::Idle]);
        let _ = tokio::time::timeout(
            Duration::from_millis(200),
            run_session(&mut session, &mut front),
        )
        .await;
        assert!(front.ticks > 0, "the surface was ticked");
        assert!(
            !session.state.windows[0].visible,
            "the old window was let go"
        );
    }

    /// With nobody attached the session holds no hub subscription, and it leaves
    /// when its human does (SPEC R-GUI.8).
    #[tokio::test]
    async fn a_session_subscribes_only_while_attended() {
        let mut session = detached();
        assert!(!session.is_attended());
        session.set_attended(true);
        assert!(session.is_attended());
        session.set_attended(true);
        assert!(session.is_attended(), "attending twice is attending once");
        session.set_attended(false);
        assert!(!session.is_attended());
    }
}
