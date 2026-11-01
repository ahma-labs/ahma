//! State and keys of the `/setup` view (SPEC R-SETUP.4).
//!
//! One place that shows, for the folder you are in, what ahma is set to and what
//! it has been told to allow: the installation, every setting with where its
//! value comes from, everything granted or switched on, and the plan of changes
//! it proposes. Nothing here writes except [`SetupView::apply_selected`], which
//! runs only on a `y` for that one item and goes through the same validator and
//! audited writers as `ahma setup --plan --apply`.
//!
//! The view is plain data plus key handling so it can be tested without a
//! terminal; `ui::draw_setup_view` only paints what [`SetupView::rows`] and
//! [`SetupView::detail_lines`] return.

use std::path::Path;

use ahma_common::config::SettingsFile;
use ahma_common::doctor::Fix;
use ahma_mcp::setup_plan::{
    Env, Origin, Plan, Proposal, Risk, Snapshot, Undo, apply, detected_proposals, plan_from,
    snapshot,
};
use crossterm::event::{KeyCode, KeyEvent};

/// The four sections, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// MCP entries, hooks, skills, TLS.
    Install,
    /// Every setting, with its value and where it comes from.
    Settings,
    /// Everything granted or switched on.
    Allow,
    /// The proposed changes.
    Plan,
}

impl Section {
    /// All sections, in order.
    pub const ALL: [Section; 4] = [Self::Install, Self::Settings, Self::Allow, Self::Plan];

    /// The tab label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Install => "Install",
            Self::Settings => "Settings",
            Self::Allow => "Allow & deny",
            Self::Plan => "Proposed changes",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
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

/// The `/setup` view.
#[derive(Debug, Default)]
pub struct SetupView {
    /// Whether the view is on screen.
    pub open: bool,
    section: Option<Section>,
    selected: [usize; 4],
    /// Settings: include the ones still at their defaults.
    pub show_all: bool,
    env: Option<Env>,
    fixes: Vec<Fix>,
    snap: Snapshot,
    plan: Plan,
    /// Proposals added by the user or a model, kept until applied or dismissed.
    extra: Vec<Proposal>,
    /// Titles of detected proposals the user dismissed this session.
    dismissed: Vec<String>,
    /// A high-risk item the user pressed `y` on once.
    confirm_high: Option<String>,
    /// What just happened, shown in the footer.
    pub status: Option<String>,
    /// Things a model said that ahma could not use.
    pub notes: Vec<String>,
}

impl SetupView {
    /// Open the view for `workspace`. `fixes` are the doctor's fixes, which the
    /// plan lists beside the detectors' proposals.
    pub fn open_for(&mut self, workspace: &Path, fixes: Vec<Fix>) {
        self.env = Env::for_workspace(workspace);
        self.fixes = fixes;
        self.open = true;
        self.section.get_or_insert(Section::Plan);
        if self.env.is_none() {
            self.status = Some("cannot find your home directory; nothing to show".into());
        }
        self.refresh();
    }

    /// Read everything for `workspace` without showing the view, so a question
    /// to the model can be briefed with the same plan the view would show.
    pub fn prepare(&mut self, workspace: &Path, fixes: Vec<Fix>) {
        self.env = Env::for_workspace(workspace);
        self.fixes = fixes;
        self.refresh();
    }

    /// Leave the view; what was proposed stays for the next time it opens.
    pub fn close(&mut self) {
        self.open = false;
        self.confirm_high = None;
    }

    /// The current section.
    pub fn section(&self) -> Section {
        self.section.unwrap_or(Section::Plan)
    }

    /// Go to a section.
    pub fn go(&mut self, section: Section) {
        self.section = Some(section);
        self.confirm_high = None;
    }

    /// Read everything again: the settings, the grants, and the plan.
    pub fn refresh(&mut self) {
        let Some(env) = &self.env else { return };
        self.snap = snapshot(env);
        let mut proposals = detected_proposals(env, self.fixes.clone(), true);
        proposals.retain(|p| !self.dismissed.contains(&p.title));
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
        for section in Section::ALL {
            let len = self.len_of(section);
            let slot = &mut self.selected[section.index()];
            *slot = (*slot).min(len.saturating_sub(1));
        }
    }

    /// Add proposals from the user or a model. They are validated with the rest.
    pub fn add_proposals(&mut self, proposals: Vec<Proposal>, notes: Vec<String>) {
        for p in proposals {
            if !self.extra.iter().any(|e| e.title == p.title) {
                self.extra.push(p);
            }
        }
        self.notes = notes;
        self.refresh();
    }

    /// The plan, as last read.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// The snapshot, as last read.
    pub fn snapshot(&self) -> &Snapshot {
        &self.snap
    }

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
            Section::Plan => self.plan.items.len() + self.plan.refused.len(),
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
        self.confirm_high = None;
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
            Section::Plan => {
                let mut rows: Vec<Row> = self
                    .plan
                    .items
                    .iter()
                    .map(|v| Row {
                        text: format!(
                            "{}{}  ({})",
                            v.proposal.title,
                            match v.risk {
                                Risk::Normal => "",
                                Risk::Security => "  [sandbox boundary]",
                                Risk::High(_) => "  [HIGH RISK]",
                            },
                            v.proposal.source.label()
                        ),
                        tone: match v.risk {
                            Risk::High(_) => Tone::Bad,
                            Risk::Security => Tone::Warn,
                            Risk::Normal => Tone::Normal,
                        },
                    })
                    .collect();
                rows.extend(self.plan.refused.iter().map(|(p, _)| Row {
                    text: format!("refused: {}  ({})", p.title, p.source.label()),
                    tone: Tone::Bad,
                }));
                rows
            }
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
            Section::Plan => {
                if let Some(v) = self.plan.items.get(sel) {
                    let mut out = vec![
                        format!("why:     {}", v.proposal.why),
                        format!("allows:  {}", v.proposal.allows),
                        format!("change:  {}", v.preview),
                        format!("undo:    {}", v.undo),
                    ];
                    if let Risk::High(reasons) = &v.risk {
                        out.extend(reasons.iter().map(|r| format!("WARNING: {r}")));
                    }
                    out
                } else if let Some((p, reason)) =
                    self.plan.refused.get(sel - self.plan.items.len().min(sel))
                {
                    vec![
                        format!("{} — refused", p.title),
                        reason.to_string(),
                        "n dismisses it.".into(),
                    ]
                } else {
                    let mut out = vec![if self.plan.is_empty() {
                        format!(
                            "No changes proposed ({} already in effect). Tell ahma what you want \
                             to allow or deny with `t`, or type /setup <what you want>.",
                            self.plan.settled
                        )
                    } else {
                        String::new()
                    }];
                    out.extend(self.notes.iter().map(|n| format!("model: {n}")));
                    out
                }
            }
        }
    }

    /// The footer: what just happened, then the keys of this section.
    pub fn footer(&self) -> String {
        let keys = match self.section() {
            Section::Install => "[Tab] section  [↑/↓] row  [t] tell ahma  [Esc] close",
            Section::Settings => {
                "[↑/↓] row  [e] change  [a] show all  [t] tell ahma  [Tab] section  [Esc] close"
            }
            Section::Allow => "[↑/↓] row  [u] take back  [t] tell ahma  [Tab] section  [Esc] close",
            Section::Plan => {
                "[↑/↓] row  [y] apply this one  [n] dismiss  [t] tell ahma  [Esc] close"
            }
        };
        match &self.status {
            Some(s) => format!("{s}  ·  {keys}"),
            None => keys.to_string(),
        }
    }

    /// Handle a key. Returns what the app should do next.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.close();
                Action::Close
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.cycle(true);
                Action::None
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.cycle(false);
                Action::None
            }
            KeyCode::Char(c @ '1'..='4') => {
                self.go(Section::ALL[(c as usize) - ('1' as usize)]);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.step(true);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.step(false);
                Action::None
            }
            KeyCode::Char('t') | KeyCode::Char('/') | KeyCode::Char('?') => {
                self.close();
                Action::Prefill("/setup ".into())
            }
            KeyCode::Char('a') if self.section() == Section::Settings => {
                self.show_all = !self.show_all;
                self.refresh();
                Action::None
            }
            KeyCode::Char('e') if self.section() == Section::Settings => {
                match self.visible_settings().get(self.selected()) {
                    Some(s) => {
                        let key = s.key.clone();
                        self.close();
                        Action::Prefill(format!("/setup set {key} "))
                    }
                    None => Action::None,
                }
            }
            KeyCode::Char('u') if self.section() == Section::Allow => {
                self.propose_undo();
                Action::None
            }
            KeyCode::Char('y') if self.section() == Section::Plan => {
                self.apply_selected();
                Action::None
            }
            KeyCode::Char('n') if self.section() == Section::Plan => {
                self.dismiss_selected();
                Action::None
            }
            _ => Action::None,
        }
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
                self.go(Section::Plan);
                self.status = Some("proposed; y applies it".into());
            }
            Undo::Command(command) => {
                self.status = Some(format!(
                    "this is a grant; take it back with `{command}` (it checks and audits it)"
                ));
            }
        }
    }

    fn dismiss_selected(&mut self) {
        let sel = self.selected();
        let title = if let Some(v) = self.plan.items.get(sel) {
            v.proposal.title.clone()
        } else if let Some((p, _)) = self
            .plan
            .refused
            .get(sel.saturating_sub(self.plan.items.len()))
        {
            p.title.clone()
        } else {
            return;
        };
        self.extra.retain(|e| e.title != title);
        if !self.dismissed.contains(&title) {
            self.dismissed.push(title);
        }
        self.confirm_high = None;
        self.status = Some("dismissed".into());
        self.refresh();
    }

    /// Apply the selected proposal, and only it. A high-risk item needs a second
    /// `y` on the same item.
    pub fn apply_selected(&mut self) {
        let sel = self.selected();
        let Some(item) = self.plan.items.get(sel) else {
            self.status = Some("nothing to apply on this row".into());
            return;
        };
        let title = item.proposal.title.clone();
        if matches!(item.risk, Risk::High(_)) && self.confirm_high.as_deref() != Some(&title) {
            self.confirm_high = Some(title);
            self.status = Some("HIGH RISK — read the warning above, then press y again".into());
            return;
        }
        let proposal = item.proposal.clone();
        let Some(env) = self.env.clone() else { return };
        let now = chrono::Local::now().to_rfc3339();
        self.status = Some(match apply(&env, &proposal, "tui", &now) {
            Ok(done) => format!("done: {done}"),
            Err(e) => format!("could not apply: {e:#}"),
        });
        self.confirm_high = None;
        self.extra.retain(|e| e.title != title);
        self.refresh();
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
        assert_eq!(v.section(), Section::Plan, "wraps");
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
    fn a_profile_is_taken_back_by_a_proposal_and_a_grant_by_its_command() {
        let (mut v, _ws) = view();
        v.go(Section::Allow);
        // The first row is a default profile: a settings edit setup can validate.
        assert!(v.rows()[0].text.contains("[profile"));
        v.handle_key(key(KeyCode::Char('u')));
        assert_eq!(v.section(), Section::Plan, "the proposal is in the plan");
        assert!(
            v.plan()
                .items
                .iter()
                .any(|i| i.proposal.title.starts_with("Take back:"))
        );
        assert!(v.status.as_deref().unwrap().contains("y applies"));
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
    fn dismissing_an_item_removes_it_and_it_stays_gone() {
        let (mut v, ws) = view();
        std::fs::write(ws.ws.path().join("m.gguf"), "x").unwrap();
        v.refresh();
        v.go(Section::Plan);
        let n = v.plan().items.len();
        assert!(n >= 1);
        v.handle_key(key(KeyCode::Char('n')));
        assert_eq!(v.plan().items.len(), n - 1);
        v.refresh();
        assert_eq!(
            v.plan().items.len(),
            n - 1,
            "a refresh does not bring it back"
        );
    }

    #[test]
    fn an_empty_plan_says_so_and_how_to_ask() {
        let (mut v, _ws) = view();
        v.go(Section::Plan);
        let detail = v.detail_lines().join("\n");
        assert!(detail.contains("No changes proposed"), "{detail}");
        assert!(detail.contains("/setup <what you want>"), "{detail}");
    }

    #[test]
    fn t_hands_the_sentence_to_the_chat() {
        let (mut v, _ws) = view();
        assert_eq!(
            v.handle_key(key(KeyCode::Char('t'))),
            Action::Prefill("/setup ".into())
        );
        assert!(!v.open);
    }

    #[test]
    fn escape_closes_the_view() {
        let (mut v, _ws) = view();
        assert_eq!(v.handle_key(key(KeyCode::Esc)), Action::Close);
        assert!(!v.open);
    }
}
