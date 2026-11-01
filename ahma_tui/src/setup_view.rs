//! State and keys of the `/setup` view (SPEC R-SETUP.4).
//!
//! The view is a short conversation first and a reference second. Opened, it
//! shows the first change ahma proposes for the folder you are in, with why it
//! proposes it and what it would allow; one key (`y` or `n`) answers it and the
//! next appears. When every change has an answer, the ones you said yes to are
//! applied, one result screen says what happened, and a key leaves. Behind `d`
//! sits the reference: the installation, every setting with where its value
//! comes from, and everything granted or switched on.
//!
//! Nothing is written before the last answer, and the only writes go through
//! [`apply`] (the validator and audited writers of `ahma setup --plan --apply`)
//! and [`record_declined`] (what you said no to, so it is not asked again).
//!
//! The view is plain data plus key handling so it can be tested without a
//! terminal; `ui::draw_setup_view` only paints [`SetupView::card`],
//! [`SetupView::done_rows`] and [`SetupView::rows`].

use std::path::Path;

use ahma_common::config::SettingsFile;
use ahma_common::doctor::Fix;
use ahma_mcp::setup_plan::{
    Env, Origin, Plan, Proposal, Risk, Snapshot, Undo, apply, declines_are_remembered,
    detected_proposals, forget_declined, pending_count, plan_from, record_declined, snapshot,
    split_declined,
};
use crossterm::event::{KeyCode, KeyEvent};

/// The reference sections behind `d`, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// MCP entries, hooks, skills, TLS.
    Install,
    /// Every setting, with its value and where it comes from.
    Settings,
    /// Everything granted or switched on.
    Allow,
}

impl Section {
    /// All sections, in order.
    pub const ALL: [Section; 3] = [Self::Install, Self::Settings, Self::Allow];

    /// The tab label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Install => "Install",
            Self::Settings => "Settings",
            Self::Allow => "Allow & deny",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// Which screen the view is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// One proposed change at a time, answered with `y` or `n`.
    #[default]
    Review,
    /// What happened to the answers, or "nothing to change".
    Done,
    /// The reference sections ([`Section`]).
    Browse,
}

/// An answer to one proposed change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Apply it.
    Yes,
    /// Do not, and (for what ahma itself raised) do not ask again here.
    No,
}

/// What the app should do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing more.
    None,
    /// Close the view.
    Close,
    /// Close the view and put this text in the chat input, so the user can finish
    /// the sentence (`/setup set tools.timeout_secs `, `/setup `).
    Prefill(String),
}

/// How a row is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Ordinary.
    Normal,
    /// Secondary.
    Dim,
    /// Good.
    Ok,
    /// Needs attention.
    Warn,
    /// Refused or broken.
    Bad,
}

/// One drawn row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The text.
    pub text: String,
    /// How to colour it.
    pub tone: Tone,
}

/// The proposed change on screen, ready to paint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// 1-based position among the proposals.
    pub position: usize,
    /// How many proposals there are.
    pub total: usize,
    /// What is proposed, in one line.
    pub title: String,
    /// Who proposed it (`detected: gpu`, `suggested by …`).
    pub source: String,
    /// `[sandbox boundary]` or `[HIGH RISK]`, when it needs care.
    pub badge: Option<String>,
    /// How the badge and title are coloured.
    pub tone: Tone,
    /// Labelled text: why, what it allows, the exact change, its undo.
    pub fields: Vec<(&'static str, String)>,
    /// Warnings to read before a high-risk `y`.
    pub warnings: Vec<String>,
    /// The question the `y` and `n` answer, worded for how much care it needs.
    pub prompt: String,
    /// This proposal's answer so far.
    pub answer: Option<Answer>,
    /// Every proposal's answer so far, in order, for the progress marks.
    pub progress: Vec<Option<Answer>>,
}

/// What became of one answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Applied; `undo` is the command that takes it back.
    Applied {
        /// The proposal's title.
        title: String,
        /// What the writer reported.
        done: String,
        /// The one command that undoes it.
        undo: String,
    },
    /// Said yes, but it could not be applied.
    Failed {
        /// The proposal's title.
        title: String,
        /// Why.
        reason: String,
    },
    /// Said no.
    Declined {
        /// The proposal's title.
        title: String,
        /// Whether ahma will not ask again in this folder.
        remembered: bool,
    },
}

/// The `/setup` view.
#[derive(Debug, Default)]
pub struct SetupView {
    /// Whether the view is on screen.
    pub open: bool,
    mode: Mode,
    section: Option<Section>,
    selected: [usize; 3],
    /// Settings: include the ones still at their defaults.
    pub show_all: bool,
    env: Option<Env>,
    fixes: Vec<Fix>,
    snap: Snapshot,
    plan: Plan,
    /// The proposal on screen, as an index into `plan.items`.
    at: usize,
    /// Answers so far, by the identity of the change they answer.
    answers: Vec<(String, Answer)>,
    /// What became of the answers, for the result screen.
    outcomes: Vec<Outcome>,
    /// Proposals added by the user or a model, kept until answered.
    extra: Vec<Proposal>,
    /// Identities declined that the store could not remember, so they stay gone
    /// for this session at least.
    dismissed: Vec<String>,
    /// How many earlier declines would be offered again by `r`.
    declined_hidden: usize,
    /// A high-risk item the user pressed `y` on once.
    confirm_high: Option<String>,
    /// What just happened, shown in the footer.
    pub status: Option<String>,
    /// Things a model said that ahma could not use.
    pub notes: Vec<String>,
}

impl SetupView {
    /// Open the view for `workspace`, on the first unanswered proposal, or on
    /// "nothing to change" when there are none. `fixes` are the doctor's fixes,
    /// which the plan lists beside the detectors' proposals.
    pub fn open_for(&mut self, workspace: &Path, fixes: Vec<Fix>) {
        self.env = Env::for_workspace(workspace);
        self.fixes = fixes;
        self.open = true;
        if self.env.is_none() {
            self.status = Some("cannot find your home directory; nothing to show".into());
        }
        self.refresh();
        self.start();
    }

    /// Read everything for `workspace` without showing the view, so a question
    /// to the model can be briefed with the same plan the view would show.
    pub fn prepare(&mut self, workspace: &Path, fixes: Vec<Fix>) {
        self.env = Env::for_workspace(workspace);
        self.fixes = fixes;
        self.refresh();
    }

    /// Leave the view. Answers given but not yet applied are kept for the next
    /// time it opens; nothing has been written.
    pub fn close(&mut self) {
        self.open = false;
        self.confirm_high = None;
    }

    /// Go to the first screen the plan calls for: the first proposal still
    /// without an answer, or the result screen when there is nothing to ask.
    fn start(&mut self) {
        self.outcomes.clear();
        self.confirm_high = None;
        if self.plan.items.is_empty() {
            self.mode = Mode::Done;
        } else {
            self.mode = Mode::Review;
            self.at = self.next_unanswered_from(0).unwrap_or(0);
        }
    }

    /// The screen on show.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The current reference section.
    pub fn section(&self) -> Section {
        self.section.unwrap_or(Section::Install)
    }

    /// Go to a reference section.
    pub fn go(&mut self, section: Section) {
        self.mode = Mode::Browse;
        self.section = Some(section);
        self.confirm_high = None;
    }

    /// Read everything again: the settings, the grants, and the plan.
    pub fn refresh(&mut self) {
        let Some(env) = &self.env else { return };
        self.snap = snapshot(env);
        // What you declined earlier in this folder stays out (SPEC R-SETUP.4.8).
        let (mut proposals, hidden) =
            split_declined(env, detected_proposals(env, self.fixes.clone(), true));
        proposals.retain(|p| !self.dismissed.contains(&p.change.identity()));
        self.declined_hidden = pending_count(env, hidden);
        proposals.extend(self.extra.clone());
        self.plan = plan_from(env, proposals);
        // An addition that is now in effect has done its job.
        let pending: Vec<String> = self
            .plan
            .items
            .iter()
            .map(|v| v.proposal.title.clone())
            .chain(self.plan.refused.iter().map(|(p, _)| p.title.clone()))
            .collect();
        self.extra.retain(|e| pending.contains(&e.title));
        // An answer to something no longer proposed answers nothing.
        let ids: Vec<String> = self
            .plan
            .items
            .iter()
            .map(|v| v.proposal.change.identity())
            .collect();
        self.answers.retain(|(id, _)| ids.contains(id));
        self.at = self.at.min(self.plan.items.len().saturating_sub(1));
        for section in Section::ALL {
            let len = self.len_of(section);
            let slot = &mut self.selected[section.index()];
            *slot = (*slot).min(len.saturating_sub(1));
        }
    }

    /// Add proposals from the user or a model and show the first of them. They
    /// are validated with the rest; one that is refused is listed on the result
    /// screen with its reason.
    pub fn add_proposals(&mut self, proposals: Vec<Proposal>, notes: Vec<String>) {
        let ids: Vec<String> = proposals.iter().map(|p| p.change.identity()).collect();
        for p in proposals {
            if !self.extra.iter().any(|e| e.title == p.title) {
                self.extra.push(p);
            }
        }
        self.notes = notes;
        self.answers.retain(|(id, _)| !ids.contains(id));
        self.refresh();
        match self
            .plan
            .items
            .iter()
            .position(|v| ids.contains(&v.proposal.change.identity()))
        {
            Some(i) => {
                self.mode = Mode::Review;
                self.at = i;
                self.confirm_high = None;
                self.outcomes.clear();
            }
            None => self.start(),
        }
    }

    /// The plan, as last read.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// The snapshot, as last read.
    pub fn snapshot(&self) -> &Snapshot {
        &self.snap
    }

    /// How many earlier declines `r` would offer again.
    pub fn declined_hidden(&self) -> usize {
        self.declined_hidden
    }

    /// What became of the last round of answers.
    pub fn outcomes(&self) -> &[Outcome] {
        &self.outcomes
    }

    // ── Review ──────────────────────────────────────────────────────────────

    fn answer_of(&self, id: &str) -> Option<Answer> {
        self.answers.iter().find(|(i, _)| i == id).map(|(_, a)| *a)
    }

    fn answer_at(&self, index: usize) -> Option<Answer> {
        self.plan
            .items
            .get(index)
            .and_then(|v| self.answer_of(&v.proposal.change.identity()))
    }

    /// The first proposal without an answer at or after `from`, wrapping round.
    fn next_unanswered_from(&self, from: usize) -> Option<usize> {
        let n = self.plan.items.len();
        (0..n)
            .map(|k| (from + k) % n)
            .find(|i| self.answer_at(*i).is_none())
    }

    /// The proposal on screen, ready to paint, in the Review mode.
    pub fn card(&self) -> Option<Card> {
        let v = self.plan.items.get(self.at)?;
        let (badge, tone) = match &v.risk {
            Risk::Normal => (None, Tone::Normal),
            Risk::Security => (Some("[sandbox boundary]".to_string()), Tone::Warn),
            Risk::High(_) => (Some("[HIGH RISK]".to_string()), Tone::Bad),
        };
        Some(Card {
            position: self.at + 1,
            total: self.plan.items.len(),
            title: v.proposal.title.clone(),
            source: v.proposal.source.label(),
            badge,
            tone,
            fields: vec![
                ("Why", v.proposal.why.clone()),
                ("Allows", v.proposal.allows.clone()),
                ("Change", v.preview.clone()),
                ("Undo", v.undo.clone()),
            ],
            warnings: match &v.risk {
                Risk::High(reasons) => reasons.clone(),
                _ => Vec::new(),
            },
            prompt: match &v.risk {
                Risk::Normal => "Apply this change?".into(),
                Risk::Security => "This changes what the sandbox permits. Apply it?".into(),
                Risk::High(_) => "HIGH RISK — read the warning. Apply it? (y, then y again)".into(),
            },
            answer: self.answer_at(self.at),
            progress: (0..self.plan.items.len())
                .map(|i| self.answer_at(i))
                .collect(),
        })
    }

    fn move_to(&mut self, index: usize) {
        self.at = index.min(self.plan.items.len().saturating_sub(1));
        self.confirm_high = None;
        self.status = None;
    }

    /// Record `answer` for the proposal on screen and move on; with the last
    /// answer in, apply what was said yes to.
    fn answer(&mut self, answer: Answer) {
        let Some(item) = self.plan.items.get(self.at) else {
            return;
        };
        let id = item.proposal.change.identity();
        if answer == Answer::Yes
            && matches!(item.risk, Risk::High(_))
            && self.confirm_high.as_deref() != Some(&id)
        {
            self.confirm_high = Some(id);
            self.status = Some("HIGH RISK — read the warning above, then press y again".into());
            return;
        }
        self.confirm_high = None;
        self.status = None;
        self.answers.retain(|(i, _)| *i != id);
        self.answers.push((id, answer));
        match self.next_unanswered_from(self.at + 1) {
            Some(next) => self.at = next,
            None => self.finish(),
        }
    }

    /// Every proposal has an answer: apply the yeses, remember the noes, and
    /// show what happened.
    fn finish(&mut self) {
        let Some(env) = self.env.clone() else { return };
        let items = self.plan.items.clone();
        let now = chrono::Local::now().to_rfc3339();
        let mut outcomes = Vec::new();
        let mut declined: Vec<&Proposal> = Vec::new();
        for item in &items {
            let title = item.proposal.title.clone();
            match self.answer_of(&item.proposal.change.identity()) {
                Some(Answer::Yes) => {
                    outcomes.push(match apply(&env, &item.proposal, "tui", &now) {
                        Ok(done) => Outcome::Applied {
                            title,
                            done,
                            undo: item.undo.clone(),
                        },
                        Err(e) => Outcome::Failed {
                            title,
                            reason: format!("{e:#}"),
                        },
                    })
                }
                Some(Answer::No) => {
                    outcomes.push(Outcome::Declined {
                        title,
                        remembered: declines_are_remembered(&item.proposal.source),
                    });
                    declined.push(&item.proposal);
                }
                None => {}
            }
        }
        if let Err(e) = record_declined(&env, &declined) {
            self.status = Some(format!(
                "could not remember what you declined ({e:#}); it will be asked again next time"
            ));
        }
        // Gone for this session whether or not the store took it.
        self.dismissed
            .extend(declined.iter().map(|p| p.change.identity()));
        let answered: Vec<String> = self.answers.iter().map(|(i, _)| i.clone()).collect();
        self.extra
            .retain(|e| !answered.contains(&e.change.identity()));
        self.answers.clear();
        self.confirm_high = None;
        self.refresh();
        self.mode = Mode::Done;
        self.outcomes = outcomes;
    }

    /// The result screen: what was applied, what failed, what was declined, what
    /// was refused, and what is in the way of asking again.
    pub fn done_rows(&self) -> Vec<Row> {
        let row = |text: String, tone| Row { text, tone };
        let mut rows = Vec::new();
        if self.outcomes.is_empty() {
            rows.push(row(
                format!(
                    "Nothing to change — ahma is set up for this folder ({} already in effect).",
                    self.plan.settled
                ),
                Tone::Ok,
            ));
        } else {
            let applied = self
                .outcomes
                .iter()
                .filter(|o| matches!(o, Outcome::Applied { .. }))
                .count();
            rows.push(row(
                if applied == 0 {
                    "No changes were applied.".to_string()
                } else {
                    format!("{applied} change(s) applied.")
                },
                if applied > 0 { Tone::Ok } else { Tone::Normal },
            ));
        }
        for outcome in &self.outcomes {
            match outcome {
                Outcome::Applied { title, undo, .. } => {
                    rows.push(row(format!("✓ {title}"), Tone::Ok));
                    rows.push(row(format!("    undo: {undo}"), Tone::Dim));
                }
                Outcome::Failed { title, reason } => {
                    rows.push(row(format!("✗ {title}"), Tone::Bad));
                    rows.push(row(format!("    could not apply: {reason}"), Tone::Bad));
                }
                Outcome::Declined { title, remembered } => {
                    rows.push(row(format!("– declined: {title}"), Tone::Dim));
                    if *remembered {
                        rows.push(row(
                            "    not asked again in this folder; r asks again".into(),
                            Tone::Dim,
                        ));
                    }
                }
            }
        }
        for (p, reason) in &self.plan.refused {
            rows.push(row(
                format!("not offered: {} — {reason}", p.title),
                Tone::Warn,
            ));
        }
        rows.extend(
            self.notes
                .iter()
                .map(|n| row(format!("model: {n}"), Tone::Dim)),
        );
        if self.declined_hidden > 0
            && !self.outcomes.iter().any(|o| {
                matches!(
                    o,
                    Outcome::Declined {
                        remembered: true,
                        ..
                    }
                )
            })
        {
            rows.push(row(
                format!(
                    "{} declined earlier, not shown. r asks about them again.",
                    self.declined_hidden
                ),
                Tone::Dim,
            ));
        }
        rows
    }

    // ── Browse ──────────────────────────────────────────────────────────────

    fn visible_settings(&self) -> Vec<&ahma_mcp::setup_plan::SettingView> {
        self.snap
            .settings
            .iter()
            .filter(|s| self.show_all || s.origin != Origin::Default)
            .collect()
    }

    fn len_of(&self, section: Section) -> usize {
        match section {
            Section::Install => self.snap.installs.len(),
            Section::Settings => self.visible_settings().len(),
            Section::Allow => self.snap.allow.len(),
        }
    }

    /// The selected row of the current section.
    pub fn selected(&self) -> usize {
        self.selected[self.section().index()]
    }

    fn step(&mut self, down: bool) {
        let len = self.len_of(self.section());
        let slot = &mut self.selected[self.section().index()];
        if len == 0 {
            *slot = 0;
        } else if down {
            *slot = (*slot + 1).min(len - 1);
        } else {
            *slot = slot.saturating_sub(1);
        }
    }

    fn cycle(&mut self, forward: bool) {
        let i = self.section().index();
        let n = Section::ALL.len();
        let next = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        self.go(Section::ALL[next]);
    }

    /// The rows of the current section.
    pub fn rows(&self) -> Vec<Row> {
        match self.section() {
            Section::Install => self
                .snap
                .installs
                .iter()
                .map(|i| Row {
                    text: format!("{:<22} {}", i.name, i.state),
                    tone: if i.state.starts_with("out of date") {
                        Tone::Warn
                    } else {
                        Tone::Normal
                    },
                })
                .collect(),
            Section::Settings => self
                .visible_settings()
                .iter()
                .map(|s| Row {
                    text: format!(
                        "{:<38} {:<20} [{}]{}",
                        s.key,
                        clip(&s.value, 20),
                        s.origin.badge(),
                        if s.tier == ahma_common::config::SettingsTier::Security {
                            "  security"
                        } else {
                            ""
                        }
                    ),
                    tone: if s.origin == Origin::Default {
                        Tone::Dim
                    } else {
                        Tone::Normal
                    },
                })
                .collect(),
            Section::Allow => self
                .snap
                .allow
                .iter()
                .map(|r| Row {
                    text: format!("[{:<7}] {} — {}", r.area, clip(&r.subject, 44), r.detail),
                    tone: Tone::Normal,
                })
                .collect(),
        }
    }

    /// The explanation of the selected row.
    pub fn detail_lines(&self) -> Vec<String> {
        let sel = self.selected();
        match self.section() {
            Section::Install => self
                .snap
                .installs
                .get(sel)
                .map(|i| {
                    vec![
                        format!("{} — {}", i.name, i.state),
                        format!("Run: {}", i.command),
                    ]
                })
                .unwrap_or_default(),
            Section::Settings => self
                .visible_settings()
                .get(sel)
                .map(|s| s.explanation.lines().map(str::to_string).collect())
                .unwrap_or_default(),
            Section::Allow => self
                .snap
                .allow
                .get(sel)
                .map(|r| {
                    let how = match &r.undo {
                        Undo::Proposal(_) => "Press u to propose taking this back.".to_string(),
                        Undo::Command(c) => {
                            format!("Take it back with: {c}  (it checks and audits the change)")
                        }
                    };
                    vec![format!("{} — {}", r.subject, r.detail), how]
                })
                .unwrap_or_default(),
        }
    }

    /// The footer: what just happened, then the keys of this screen.
    pub fn footer(&self) -> String {
        let more = if self.declined_hidden > 0 {
            format!("  [r] ask again about {} declined", self.declined_hidden)
        } else {
            String::new()
        };
        let pending = self.plan.items.len();
        let keys = match self.mode {
            Mode::Review => "[←/→] previous/next  [d] look around  [t] tell ahma  \
                 [Esc] leave, nothing changes"
                .to_string(),
            Mode::Done => format!("[Enter] done  [d] look around  [t] tell ahma{more}"),
            Mode::Browse => match self.section() {
                Section::Install => format!(
                    "[Tab] section  [↑/↓] row  [p] proposed changes ({pending})  [t] tell ahma  [Esc] close"
                ),
                Section::Settings => format!(
                    "[↑/↓] row  [e] change  [a] show all  [p] proposed ({pending})  [t] tell ahma  [Tab] section  [Esc] close"
                ),
                Section::Allow => format!(
                    "[↑/↓] row  [u] take back  [p] proposed ({pending})  [t] tell ahma  [Tab] section  [Esc] close"
                ),
            },
        };
        match &self.status {
            Some(s) => format!("{s}  ·  {keys}"),
            None => keys,
        }
    }

    // ── Keys ────────────────────────────────────────────────────────────────

    /// Handle a key. Returns what the app should do next.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        match self.mode {
            Mode::Review => self.review_key(key),
            Mode::Done => self.done_key(key),
            Mode::Browse => self.browse_key(key),
        }
    }

    /// Keys every screen shares: asking ahma in words.
    fn tell_ahma(&mut self, key: KeyEvent) -> Option<Action> {
        matches!(
            key.code,
            KeyCode::Char('t') | KeyCode::Char('/') | KeyCode::Char('?')
        )
        .then(|| {
            self.close();
            Action::Prefill("/setup ".into())
        })
    }

    fn review_key(&mut self, key: KeyEvent) -> Action {
        if let Some(action) = self.tell_ahma(key) {
            return action;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.close();
                return Action::Close;
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => self.answer(Answer::Yes),
            KeyCode::Char('n') | KeyCode::Char('N') => self.answer(Answer::No),
            KeyCode::Left
            | KeyCode::Up
            | KeyCode::Char('h')
            | KeyCode::Char('k')
            | KeyCode::Backspace => self.move_to(self.at.saturating_sub(1)),
            KeyCode::Right | KeyCode::Down | KeyCode::Char('l') | KeyCode::Char('j') => {
                self.move_to(self.at + 1)
            }
            KeyCode::Char('d') | KeyCode::Tab => self.go(Section::Install),
            _ => {}
        }
        Action::None
    }

    fn done_key(&mut self, key: KeyEvent) -> Action {
        if let Some(action) = self.tell_ahma(key) {
            return action;
        }
        match key.code {
            KeyCode::Char('d') | KeyCode::Tab => {
                self.go(Section::Install);
                Action::None
            }
            KeyCode::Char('r') if self.declined_hidden > 0 => {
                self.ask_declined_again();
                Action::None
            }
            // The answer keys do nothing here: a second press of the `y` that
            // finished the review must not throw the result away unread.
            KeyCode::Char('y' | 'Y' | 'n' | 'N') => Action::None,
            _ => {
                self.close();
                Action::Close
            }
        }
    }

    /// `r`: forget what was declined in this folder and ask about it again.
    fn ask_declined_again(&mut self) {
        if let Some(env) = &self.env
            && let Err(e) = forget_declined(env)
        {
            self.status = Some(format!("could not forget the earlier answers: {e:#}"));
            return;
        }
        self.dismissed.clear();
        self.refresh();
        self.start();
        self.status = Some("asking again about what you declined".into());
    }

    fn browse_key(&mut self, key: KeyEvent) -> Action {
        if let Some(action) = self.tell_ahma(key) {
            return action;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.close();
                return Action::Close;
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.cycle(true),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.cycle(false),
            KeyCode::Char(c @ '1'..='3') => self.go(Section::ALL[(c as usize) - ('1' as usize)]),
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Char('p') => {
                self.status = None;
                self.start();
            }
            KeyCode::Char('a') if self.section() == Section::Settings => {
                self.show_all = !self.show_all;
                self.refresh();
            }
            KeyCode::Char('e') if self.section() == Section::Settings => {
                if let Some(s) = self.visible_settings().get(self.selected()) {
                    let key = s.key.clone();
                    self.close();
                    return Action::Prefill(format!("/setup set {key} "));
                }
            }
            KeyCode::Char('u') if self.section() == Section::Allow => self.propose_undo(),
            _ => {}
        }
        Action::None
    }

    fn propose_undo(&mut self) {
        let Some(row) = self.snap.allow.get(self.selected()).cloned() else {
            return;
        };
        match row.undo {
            Undo::Proposal(change) => {
                let p = Proposal {
                    title: format!("Take back: {}", row.subject),
                    why: format!("You chose to take back {} ({}).", row.subject, row.detail),
                    allows: "Narrows what tools may do; nothing is widened.".into(),
                    change,
                    source: ahma_mcp::setup_plan::Source::User,
                };
                self.add_proposals(vec![p], Vec::new());
            }
            Undo::Command(command) => {
                self.status = Some(format!(
                    "this is a grant; take it back with `{command}` (it checks and audits it)"
                ));
            }
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Where a `/setup set` request is written: the repository file when
/// `--project` was typed, else yours.
pub fn file_for_flag(project: bool) -> SettingsFile {
    if project {
        SettingsFile::Project
    } else {
        SettingsFile::User
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(v: &mut SetupView, c: char) -> Action {
        v.handle_key(key(KeyCode::Char(c)))
    }

    /// A view over a fresh project folder and a home of its own: the default test
    /// home is shared by every test in the run, so a settings change made here
    /// would show up in another test.
    fn view() -> (SetupView, Fixture) {
        let guard = crate::HOME_SEAM_GUARD.lock();
        let home = tempfile::tempdir().unwrap();
        // SAFETY: held under HOME_SEAM_GUARD, and nextest gives each test a process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".ahma")).unwrap();
        let mut v = SetupView::default();
        v.open_for(ws.path(), Vec::new());
        (
            v,
            Fixture {
                ws,
                _home: home,
                _guard: guard,
            },
        )
    }

    struct Fixture {
        ws: tempfile::TempDir,
        _home: tempfile::TempDir,
        _guard: parking_lot::MutexGuard<'static, ()>,
    }

    impl Fixture {
        fn env(&self) -> Env {
            Env::for_workspace(self.ws.path()).unwrap()
        }

        fn settings(&self) -> ahma_common::config::AhmaSettings {
            ahma_common::config::AhmaSettings::load_from_checked(&self.env().user_file)
                .unwrap_or_default()
        }

        /// A model file (proposes the GPU) and a definition-of-done script
        /// (proposes a source reader): two proposals, then reopen the view.
        fn two_proposals(&self, v: &mut SetupView) {
            std::fs::write(self.ws.path().join("m.gguf"), "x").unwrap();
            std::fs::create_dir_all(self.ws.path().join("scripts")).unwrap();
            std::fs::write(self.ws.path().join("scripts/check.sh"), "#!/bin/sh\n").unwrap();
            std::fs::write(
                self.ws.path().join("AGENTS.md"),
                "## Definition of Done\n\nRun `scripts/check.sh`.\n",
            )
            .unwrap();
            v.open_for(self.ws.path(), Vec::new());
        }
    }

    fn title(v: &SetupView) -> String {
        v.card().expect("a card is on screen").title
    }

    /// Answer every card: `y` for a title containing `yes_for`, `n` otherwise.
    fn answer_all(v: &mut SetupView, yes_for: &str) {
        while v.mode() == Mode::Review {
            let c = if title(v).contains(yes_for) { 'y' } else { 'n' };
            press(v, c);
        }
    }

    #[test]
    fn opening_with_proposals_shows_the_first_with_its_reasons() {
        let (mut v, f) = view();
        f.two_proposals(&mut v);
        assert_eq!(v.mode(), Mode::Review);
        let card = v.card().unwrap();
        assert_eq!((card.position, card.total), (1, 2));
        let labels: Vec<_> = card.fields.iter().map(|(l, _)| *l).collect();
        assert_eq!(labels, ["Why", "Allows", "Change", "Undo"]);
        assert!(card.fields.iter().all(|(_, text)| !text.is_empty()));
        assert!(card.source.starts_with("detected:"), "{}", card.source);
        assert_eq!(
            card.prompt,
            "This changes what the sandbox permits. Apply it?"
        );
        assert!(v.footer().contains("[Esc] leave, nothing changes"));
    }

    #[test]
    fn nothing_is_written_until_the_last_answer_then_yes_applies_and_no_is_remembered() {
        let (mut v, f) = view();
        f.two_proposals(&mut v);
        assert!(!f.settings().sandbox.allow_gpu);

        // The GPU first or second, the answer is yes to it and no to the other.
        let first_is_gpu = title(&v).contains("GPU");
        press(&mut v, if first_is_gpu { 'y' } else { 'n' });
        assert_eq!(v.mode(), Mode::Review, "one answer is not the last");
        assert!(!f.settings().sandbox.allow_gpu, "nothing written yet");
        assert_eq!(v.card().unwrap().position, 2, "it moved to the next change");
        press(&mut v, if first_is_gpu { 'n' } else { 'y' });

        assert_eq!(v.mode(), Mode::Done);
        assert!(f.settings().sandbox.allow_gpu, "the yes was applied");
        assert!(
            f.settings().tools.source_readers.is_empty(),
            "the no was not"
        );
        let outcomes = v.outcomes();
        assert_eq!(outcomes.len(), 2, "{outcomes:?}");
        assert!(outcomes.iter().any(
            |o| matches!(o, Outcome::Applied { undo, .. } if undo.contains("settings unset"))
        ));
        assert!(outcomes.iter().any(|o| matches!(
            o,
            Outcome::Declined {
                remembered: true,
                ..
            }
        )));
        let shown = v
            .done_rows()
            .into_iter()
            .map(|r| r.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(shown.contains('✓') && shown.contains("declined"), "{shown}");

        // A new view of the same folder does not ask about the no again.
        let mut later = SetupView::default();
        later.open_for(f.ws.path(), Vec::new());
        assert_eq!(later.mode(), Mode::Done, "{:?}", later.plan().items.len());
        assert_eq!(later.declined_hidden(), 1);
    }

    #[test]
    fn going_back_changes_an_answer_before_anything_is_applied() {
        let (mut v, f) = view();
        f.two_proposals(&mut v);
        press(&mut v, 'y');
        assert_eq!(v.card().unwrap().position, 2);
        v.handle_key(key(KeyCode::Left));
        let card = v.card().unwrap();
        assert_eq!(card.position, 1);
        assert_eq!(
            card.answer,
            Some(Answer::Yes),
            "the earlier answer is shown"
        );
        press(&mut v, 'n');
        press(&mut v, 'n');
        assert_eq!(v.mode(), Mode::Done);
        assert!(!f.settings().sandbox.allow_gpu);
        assert!(f.settings().tools.source_readers.is_empty());
        assert!(
            v.outcomes()
                .iter()
                .all(|o| matches!(o, Outcome::Declined { .. }))
        );
    }

    #[test]
    fn escape_writes_nothing_and_the_answers_are_kept_for_next_time() {
        let (mut v, f) = view();
        f.two_proposals(&mut v);
        press(&mut v, 'y');
        assert_eq!(v.handle_key(key(KeyCode::Esc)), Action::Close);
        assert!(!v.open);
        assert!(!f.settings().sandbox.allow_gpu);
        assert!(f.settings().tools.source_readers.is_empty());

        v.open_for(f.ws.path(), Vec::new());
        assert_eq!(v.mode(), Mode::Review);
        assert_eq!(
            v.card().unwrap().position,
            2,
            "it resumes at the unanswered one"
        );
    }

    #[test]
    fn a_high_risk_grant_needs_a_second_y_and_moving_cancels_the_first() {
        let (mut v, f) = view();
        // A folder that does not exist yet is a high-risk grant, not a refusal.
        let p = Proposal {
            title: "Grant a folder".into(),
            why: "w".into(),
            allows: "a".into(),
            change: ahma_mcp::setup_plan::Change::GrantPath {
                path: f.ws.path().join("not-yet-there"),
                read_only: true,
            },
            source: ahma_mcp::setup_plan::Source::User,
        };
        v.add_proposals(vec![p], Vec::new());
        assert_eq!(title(&v), "Grant a folder");
        assert!(!v.card().unwrap().warnings.is_empty());

        press(&mut v, 'y');
        assert_eq!(v.mode(), Mode::Review, "the first y only asks again");
        assert!(v.status.as_deref().unwrap().contains("press y again"));
        v.handle_key(key(KeyCode::Right)); // moving cancels it
        press(&mut v, 'y');
        assert_eq!(v.mode(), Mode::Review, "so this y is a first y again");
        press(&mut v, 'y');
        assert_eq!(v.mode(), Mode::Done);
        assert_eq!(v.outcomes().len(), 1);
    }

    #[test]
    fn r_asks_again_about_what_was_declined() {
        let (mut v, f) = view();
        f.two_proposals(&mut v);
        answer_all(&mut v, "never matches");
        assert_eq!(v.mode(), Mode::Done);
        assert!(
            v.outcomes()
                .iter()
                .all(|o| matches!(o, Outcome::Declined { .. }))
        );
        // Said no a moment ago: the result screen offers to take it back.
        assert_eq!(v.declined_hidden(), 2);
        assert!(v.footer().contains("[r] ask again about 2 declined"));

        let mut later = SetupView::default();
        later.open_for(f.ws.path(), Vec::new());
        assert_eq!((later.mode(), later.declined_hidden()), (Mode::Done, 2));
        assert!(later.footer().contains("[r] ask again about 2 declined"));
        press(&mut later, 'r');
        assert_eq!(later.mode(), Mode::Review);
        assert_eq!(later.card().unwrap().total, 2);
    }

    #[test]
    fn an_empty_plan_says_nothing_to_change() {
        let (v, _f) = view();
        assert_eq!(v.mode(), Mode::Done);
        let shown = v
            .done_rows()
            .into_iter()
            .map(|r| r.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(shown.contains("Nothing to change"), "{shown}");
        assert!(v.card().is_none());
    }

    #[test]
    fn the_result_screen_ignores_answer_keys_and_any_other_key_closes_it() {
        let (mut v, _f) = view();
        assert_eq!(v.mode(), Mode::Done);
        // The y that finished a review must not close the result unread.
        assert_eq!(press(&mut v, 'y'), Action::None);
        assert_eq!(press(&mut v, 'n'), Action::None);
        assert!(v.open);
        assert_eq!(v.handle_key(key(KeyCode::Enter)), Action::Close);
        assert!(!v.open);
    }

    #[test]
    fn a_refused_request_is_named_on_the_result_screen_with_its_reason() {
        let (mut v, _f) = view();
        let p =
            ahma_mcp::setup_plan::user_proposal(false, SettingsFile::User, "nothing.at_all", "1");
        v.add_proposals(vec![p], Vec::new());
        assert_eq!(v.mode(), Mode::Done);
        let shown = v
            .done_rows()
            .into_iter()
            .map(|r| r.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(shown.contains("not offered"), "{shown}");
    }

    #[test]
    fn taking_back_a_profile_is_a_change_you_answer() {
        let (mut v, _f) = view();
        v.go(Section::Allow);
        // The first row is a default profile: a settings edit setup can validate.
        assert!(v.rows()[0].text.contains("[profile"));
        press(&mut v, 'u');
        assert_eq!(v.mode(), Mode::Review, "the proposal is on screen");
        assert!(title(&v).starts_with("Take back:"));
        // Nothing was written by proposing it.
        assert!(
            ahma_common::config::AhmaSettings::load()
                .sandbox
                .profiles
                .iter()
                .any(|p| p == "rust")
        );
    }

    #[test]
    fn look_around_reaches_the_reference_and_p_returns_to_the_proposals() {
        let (mut v, f) = view();
        f.two_proposals(&mut v);
        press(&mut v, 'd');
        assert_eq!(v.mode(), Mode::Browse);
        assert!(
            v.footer().contains("[p] proposed changes (2)"),
            "{}",
            v.footer()
        );
        press(&mut v, 'p');
        assert_eq!(v.mode(), Mode::Review);
    }

    #[test]
    fn settings_show_what_you_changed_until_you_ask_for_all() {
        let (mut v, ws) = view();
        let env = Env::for_workspace(ws.ws.path()).unwrap();
        ahma_common::config::AhmaSettings::set_in_file(
            &env.user_file,
            "tools.timeout_secs",
            Some("77"),
        )
        .unwrap();
        v.refresh();
        v.go(Section::Settings);
        let rows = v.rows();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].text.contains("tools.timeout_secs") && rows[0].text.contains("77"));
        assert!(rows[0].text.contains("[you]"), "{}", rows[0].text);
        assert!(v.detail_lines().iter().any(|l| l.contains("default:")));

        v.handle_key(key(KeyCode::Char('a')));
        let all = v.rows();
        assert!(all.len() > 50, "every setting is listed: {}", all.len());
        assert!(
            all.iter()
                .any(|r| r.text.contains("sandbox.allow_gpu") && r.text.contains("security"))
        );
        assert!(
            all.iter().any(|r| r.tone == Tone::Dim),
            "defaults are dimmed"
        );
    }

    #[test]
    fn a_repository_value_is_badged_and_an_ignored_security_key_is_known() {
        let (mut v, ws) = view();
        let project = ws.ws.path().join(".ahma").join("settings.toml");
        ahma_common::config::AhmaSettings::set_in_file_as(
            SettingsFile::Project,
            &project,
            "tools.timeout_secs",
            Some("500"),
        )
        .unwrap();
        v.refresh();
        v.go(Section::Settings);
        let rows = v.rows();
        assert!(
            rows.iter()
                .any(|r| r.text.contains("[repo]") && r.text.contains("500")),
            "{rows:?}"
        );
    }

    #[test]
    fn the_sections_cycle_and_numbers_jump() {
        let (mut v, _ws) = view();
        v.go(Section::Install);
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.section(), Section::Settings);
        v.handle_key(key(KeyCode::BackTab));
        assert_eq!(v.section(), Section::Install);
        v.handle_key(key(KeyCode::BackTab));
        assert_eq!(v.section(), Section::Allow, "wraps");
        v.handle_key(key(KeyCode::Char('3')));
        assert_eq!(v.section(), Section::Allow);
        let rows = v.rows();
        for profile in [
            "rust", "node", "go", "android", "apple", "common", "gh", "sccache",
        ] {
            assert!(
                rows.iter()
                    .any(|r| r.text.contains("[profile") && r.text.contains(profile)),
                "{profile} is listed: {rows:?}"
            );
        }
        assert!(v.footer().contains("[u] take back"));
    }

    #[test]
    fn t_hands_the_sentence_to_the_chat_from_every_screen() {
        let (mut v, f) = view();
        assert_eq!(v.mode(), Mode::Done);
        assert_eq!(press(&mut v, 't'), Action::Prefill("/setup ".into()));
        assert!(!v.open);

        f.two_proposals(&mut v);
        assert_eq!(v.mode(), Mode::Review);
        assert_eq!(press(&mut v, 't'), Action::Prefill("/setup ".into()));
    }

    #[test]
    fn escape_closes_the_view() {
        let (mut v, _ws) = view();
        assert_eq!(v.handle_key(key(KeyCode::Esc)), Action::Close);
        assert!(!v.open);
    }
}
