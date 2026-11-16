//! What a surface may ask of a session, and the rules every surface answers to
//! (SPEC R-UI).
//!
//! A surface — the terminal, a browser — decodes its own input into an
//! [`Intent`] and tells the session what it showed in a [`Report`]. Everything
//! that decides whether a human's answer is taken lives here, once: that the
//! question named is the one on screen, that it was shown and read before a
//! granting answer counts (R-PERM.3.5), that a saving answer waited for the
//! exact line it saves (R-PERM.2), and what this session may do at all
//! (R-GUI.9). The terminal's key handlers call these functions after turning a
//! key into an intent; nothing here knows what a key is.
//!
//! An intent names its target — a decision id, an operation — and never a
//! position, so it means the same thing whichever surface sent it and whatever
//! that surface had selected.

use crate::state::{AppState, GateView, OpKey};
use ahma_common::scope_grant::GrantDecision;
use ahma_common::web_approval::WebApprovalDecision;

/// An answer to a web-egress question. `Always` writes a setting, so it needs a
/// session that may ([`crate::state::Capabilities::persistent_answers`]) and a
/// second step that shows the line it saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebAnswer {
    Deny,
    Once,
    Session,
    Always,
}

/// An answer to the chat's tool-approval gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolAnswer {
    Reject,
    Once,
    /// Always allow this tool: persisted.
    Always,
}

/// What a human asked a session to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// A line from the input: a chat turn, a `/` command, a `#` goal or a `!`
    /// host command. What it means, and whether this session may, is decided
    /// here, not by the surface.
    Submit {
        text: String,
    },
    /// End the turn in flight.
    CancelTurn,
    /// Answer the sandbox-scope question `decision_id` with one of the options
    /// it offers, named by its stable value (`read-write-session`).
    AnswerScopeGrant {
        decision_id: String,
        option: String,
    },
    /// Show or hide the evidence and risk detail of the question.
    ToggleScopeGrantDetail {
        decision_id: String,
    },
    AnswerWebApproval {
        decision_id: String,
        answer: WebAnswer,
    },
    AnswerToolApproval {
        op_id: String,
        answer: ToolAnswer,
    },
    /// Cancel an operation, on the instance that ran it.
    CancelOp {
        op: OpKey,
    },
    /// Ask again for the access a denied operation needed.
    ReRaiseGrant {
        op: OpKey,
    },
    /// Open the section `key`, or with `None` close the one that is open: one
    /// is open at a time.
    OpenSection {
        key: Option<String>,
    },
    /// Send the next chat turns to the window `key`.
    ChatWith {
        key: String,
    },
    /// Show or hide the tail of an operation's output inline.
    ToggleOpOutput {
        op: OpKey,
    },
    /// This project, or every project.
    ToggleProjectFilter,
}

/// What a surface tells the session about what it showed. These are not
/// requests: they are facts the rules above depend on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// The question is on screen, in a window the human can see. Its arming
    /// delay starts now, the first time only (R-GUI.6).
    GatePresented { decision_id: String },
    /// Whether the whole "If you choose always" section is on screen. A saving
    /// answer is taken only while it is (R-PERM.3.5).
    GateSaveLine { decision_id: String, visible: bool },
}

/// Why an intent changed nothing. A surface may say why; it never acts on it
/// beyond that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The question or operation named is not the one in question any more: it
    /// was answered, withdrawn or replaced.
    Stale,
    /// Typed ahead of the human reading it, or no surface has shown it yet
    /// (R-PERM.3.5, R-GUI.6). Denying never waits.
    NotArmed,
    /// A saving answer: the first press brings the exact line into view; the
    /// answer is taken once that was reported shown (R-PERM.3.5).
    SaveLineNotShown,
    /// `Always` for a web host: the first step shows the line it would write.
    NeedsConfirmation,
    /// The question does not offer that answer.
    NotOffered,
    /// This session may not (R-GUI.9).
    NotAllowed,
    /// There was nothing to act on.
    Nothing,
}

/// Do what a human asked.
pub fn apply(state: &mut AppState, intent: Intent) -> Result<(), Refused> {
    match intent {
        Intent::Submit { text } => {
            crate::app::submit_text(state, text);
            Ok(())
        }
        Intent::CancelTurn => crate::app::cancel_turn(state)
            .then_some(())
            .ok_or(Refused::Nothing),
        Intent::AnswerScopeGrant {
            decision_id,
            option,
        } => answer_scope_grant(state, &decision_id, &option),
        Intent::ToggleScopeGrantDetail { decision_id } => {
            let on_screen = state
                .scope_grant
                .as_ref()
                .is_some_and(|g| g.decision_id == decision_id);
            if !on_screen {
                return Err(Refused::Stale);
            }
            crate::app::toggle_scope_grant_detail(state);
            Ok(())
        }
        Intent::AnswerWebApproval {
            decision_id,
            answer,
        } => answer_web_approval(state, &decision_id, answer),
        Intent::AnswerToolApproval { op_id, answer } => answer_tool_approval(state, &op_id, answer),
        Intent::CancelOp { op } => {
            if state.find_op(&op).is_none() {
                return Err(Refused::Stale);
            }
            crate::app::cancel_op(state, op);
            Ok(())
        }
        Intent::ReRaiseGrant { op } => crate::app::reraise_grant(state, &op),
        Intent::OpenSection { key } => {
            let now = crate::ui::wall_ms();
            match key {
                Some(key) if state.open_section.as_deref() != Some(key.as_str()) => {
                    state.open_section(&key, now)
                }
                Some(_) | None => {
                    if let Some(open) = state.open_section.clone() {
                        state.toggle_section(&open, now);
                    }
                }
            }
            Ok(())
        }
        Intent::ChatWith { key } => crate::app::chat_with(state, &key),
        Intent::ToggleOpOutput { op } => {
            if state.find_op(&op).is_none() {
                return Err(Refused::Stale);
            }
            state.expanded_op = match state.expanded_op.as_deref() {
                Some(open) if open == op.id => None,
                _ => Some(op.id),
            };
            Ok(())
        }
        Intent::ToggleProjectFilter => {
            state.show_all_projects = !state.show_all_projects;
            Ok(())
        }
    }
}

/// Record what a surface showed.
pub fn report(state: &mut AppState, report: Report) {
    match report {
        Report::GatePresented { decision_id } => {
            if let Some(gate) = state
                .scope_grant
                .as_mut()
                .filter(|g| g.decision_id == decision_id)
            {
                gate.presented();
            }
            if let Some(gate) = state
                .web_approval
                .as_mut()
                .filter(|g| g.decision_id == decision_id)
            {
                gate.presented();
            }
        }
        Report::GateSaveLine {
            decision_id,
            visible,
        } => {
            // Only for the question on screen: a report about another one
            // unlocks nothing.
            if let Some(gate) = state
                .scope_grant
                .as_ref()
                .filter(|g| g.decision_id == decision_id)
            {
                let view = gate.view.get();
                gate.view.set(GateView {
                    save_line_visible: visible,
                    ..view
                });
            }
        }
    }
}

/// Answer the sandbox-scope question `decision_id` with the option whose value
/// is `option`.
///
/// The one place the rules for taking a grant live (R-PERM.3.5, R-PERM.2,
/// R5.3.1): a granting answer waits until the question was shown and its
/// arming delay has passed; one that is saved to settings is never taken
/// without the exact line it writes on screen. Denying is always taken.
pub fn answer_scope_grant(
    state: &mut AppState,
    decision_id: &str,
    option: &str,
) -> Result<(), Refused> {
    let Some(gate) = state
        .scope_grant
        .as_ref()
        .filter(|g| g.decision_id == decision_id)
    else {
        return Err(Refused::Stale);
    };
    let Some(chosen) = ahma_common::grant_prompt::options_for(gate.request.reason)
        .into_iter()
        .find(|o| o.value == option)
    else {
        return Err(Refused::NotOffered);
    };
    let granting = chosen.decision != GrantDecision::Deny;
    // `Deny.tier()` is the catch-all `Always`, which a deny is not: it saves
    // nothing and never waits.
    let saving = granting && chosen.decision.tier().is_persistent();
    // What this session may do comes first, so it learns nothing about timing
    // from an answer it could not give.
    if saving && !state.caps.persistent_answers {
        return Err(Refused::NotAllowed);
    }
    // Typed ahead, not read: it answers nothing (deny never waits).
    if granting && !gate.armed() {
        return Err(Refused::NotArmed);
    }
    // A grant that is saved to settings is never given without the exact line
    // it writes on screen: the first press scrolls that line into view, and a
    // second saves only if it was shown whole.
    if saving && !(gate.show_detail && gate.view.get().save_line_visible) {
        crate::app::reveal_scope_grant_save_line(state);
        return Err(Refused::SaveLineNotShown);
    }
    crate::app::resolve_scope_grant(state, chosen.decision);
    Ok(())
}

/// Answer the web-egress question `decision_id`.
pub fn answer_web_approval(
    state: &mut AppState,
    decision_id: &str,
    answer: WebAnswer,
) -> Result<(), Refused> {
    let Some(gate) = state
        .web_approval
        .as_ref()
        .filter(|g| g.decision_id == decision_id)
    else {
        return Err(Refused::Stale);
    };
    if answer == WebAnswer::Deny {
        crate::app::resolve_web_approval(state, WebApprovalDecision::Deny);
        return Ok(());
    }
    if answer == WebAnswer::Always && !state.caps.persistent_answers {
        return Err(Refused::NotAllowed);
    }
    if !gate.armed() {
        return Err(Refused::NotArmed);
    }
    match answer {
        WebAnswer::Once => crate::app::resolve_web_approval(state, WebApprovalDecision::AllowOnce),
        WebAnswer::Session => {
            crate::app::resolve_web_approval(state, WebApprovalDecision::AllowSession)
        }
        // Saving to settings shows the exact line first; the second step saves
        // it (R-PERM.3.5).
        WebAnswer::Always if gate.confirm_always => {
            crate::app::resolve_web_approval(state, WebApprovalDecision::AllowAlways)
        }
        WebAnswer::Always => {
            if let Some(gate) = state.web_approval.as_mut() {
                gate.confirm_always = true;
            }
            return Err(Refused::NeedsConfirmation);
        }
        WebAnswer::Deny => unreachable!("handled above"),
    }
    Ok(())
}

/// Answer the chat's tool-approval gate for `op_id`.
pub fn answer_tool_approval(
    state: &mut AppState,
    op_id: &str,
    answer: ToolAnswer,
) -> Result<(), Refused> {
    if state.approval.as_ref().is_none_or(|g| g.op_id != op_id) {
        return Err(Refused::Stale);
    }
    match answer {
        ToolAnswer::Reject => crate::app::resolve_approval(state, false),
        ToolAnswer::Once => crate::app::resolve_approval(state, true),
        ToolAnswer::Always if !state.caps.persistent_answers => return Err(Refused::NotAllowed),
        ToolAnswer::Always => crate::app::resolve_approval_always(state),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Capabilities, GRANT_ARMING_DELAY, ScopeGrantGate, WebApprovalGate};
    use ahma_common::config::ScopeAccess;
    use ahma_common::grant_prompt::options_for;
    use ahma_common::scope_grant::{GrantReason, ScopeGrantRequest};

    fn gui_caps() -> Capabilities {
        Capabilities {
            host_commands: false,
            persistent_answers: false,
            wait_for_presentation: true,
            commands: crate::state::CommandPolicy::Only(&[]),
        }
    }

    fn state(caps: Capabilities) -> AppState {
        let mut state = AppState::new("http://127.0.0.1:9", "HTTP", true);
        state.surface = ahma_common::hub::Surface::Web;
        state.caps = caps;
        state
    }

    fn request(id: &str) -> ScopeGrantRequest {
        ScopeGrantRequest {
            decision_id: id.into(),
            path: std::path::PathBuf::from("/opt/cache"),
            access: ScopeAccess::Rw,
            reason: GrantReason::StderrHeuristic,
            tool: Some("cargo".into()),
            context: Default::default(),
        }
    }

    /// A question on screen in `state`, shown to nobody yet when the session
    /// waits for presentation.
    fn ask(state: &mut AppState, id: &str) {
        let mut gate = ScopeGrantGate::from_request(request(id));
        gate.restart_arming(state.caps.wait_for_presentation);
        state.scope_grant = Some(gate);
    }

    /// The value of the first option offered that is `saving` or not.
    fn option(saving: bool) -> String {
        options_for(GrantReason::StderrHeuristic)
            .into_iter()
            .find(|o| {
                o.decision != GrantDecision::Deny && o.decision.tier().is_persistent() == saving
            })
            .expect("the question offers such an answer")
            .value
            .to_string()
    }

    /// Let a question that was shown be read.
    fn read(state: &mut AppState) {
        let gate = state.scope_grant.as_mut().unwrap();
        gate.shown_at = std::time::Instant::now() - GRANT_ARMING_DELAY * 2;
    }

    fn answer(state: &mut AppState, id: &str, option: &str) -> Result<(), Refused> {
        apply(
            state,
            Intent::AnswerScopeGrant {
                decision_id: id.into(),
                option: option.into(),
            },
        )
    }

    /// The question an answer names must be the one on screen: one that was
    /// replaced, answered elsewhere or never existed changes nothing and
    /// leaves the real one waiting.
    #[test]
    fn an_answer_for_a_question_not_on_screen_is_ignored() {
        let mut state = state(Capabilities::terminal());
        ask(&mut state, "real");
        read(&mut state);
        assert_eq!(
            answer(&mut state, "ghost", &option(false)),
            Err(Refused::Stale)
        );
        assert!(state.scope_grant.is_some(), "the real question still waits");
        assert_eq!(
            apply(
                &mut state,
                Intent::ToggleScopeGrantDetail {
                    decision_id: "ghost".into()
                }
            ),
            Err(Refused::Stale)
        );
        assert_eq!(
            answer_web_approval(&mut state, "ghost", WebAnswer::Deny),
            Err(Refused::Stale)
        );
        assert_eq!(
            answer_tool_approval(&mut state, "ghost", ToolAnswer::Once),
            Err(Refused::Stale)
        );
    }

    /// An answer the question does not offer is refused by name, whatever
    /// string a surface sends.
    #[test]
    fn a_decision_the_question_does_not_offer_is_refused() {
        let mut state = state(Capabilities::terminal());
        ask(&mut state, "q");
        state.scope_grant.as_mut().unwrap().awaiting_presentation = false;
        read(&mut state);
        for bogus in ["root", "read-write-forever", "", "n; y"] {
            assert_eq!(answer(&mut state, "q", bogus), Err(Refused::NotOffered));
        }
        assert!(state.scope_grant.is_some());
    }

    /// A granting answer typed before the question could be read answers
    /// nothing; denying is never made to wait (R-PERM.3.5).
    #[test]
    fn a_grant_before_arming_is_swallowed_and_a_deny_is_not() {
        let mut state = state(Capabilities::terminal());
        ask(&mut state, "q");
        assert_eq!(
            answer(&mut state, "q", &option(false)),
            Err(Refused::NotArmed)
        );
        assert!(state.scope_grant.is_some());
        assert_eq!(answer(&mut state, "q", "deny"), Ok(()));
        assert!(state.scope_grant.is_none());
    }

    /// For a surface that reports what it showed, the delay does not start
    /// when the question arrived but when a surface says it is on screen
    /// (R-GUI.6): an unwatched question is never answerable.
    #[test]
    fn a_question_no_surface_has_shown_cannot_be_granted() {
        let mut state = state(gui_caps());
        ask(&mut state, "q");
        // However long it waited.
        state.scope_grant.as_mut().unwrap().shown_at =
            std::time::Instant::now() - GRANT_ARMING_DELAY * 100;
        assert_eq!(
            answer(&mut state, "q", &option(false)),
            Err(Refused::NotArmed)
        );

        // A report about another question starts nothing.
        report(
            &mut state,
            Report::GatePresented {
                decision_id: "other".into(),
            },
        );
        read(&mut state);
        assert_eq!(
            answer(&mut state, "q", &option(false)),
            Err(Refused::NotArmed)
        );

        // Reported shown: the delay starts now, and is still a delay.
        report(
            &mut state,
            Report::GatePresented {
                decision_id: "q".into(),
            },
        );
        assert_eq!(
            answer(&mut state, "q", &option(false)),
            Err(Refused::NotArmed),
            "shown a moment ago is not read yet"
        );
        read(&mut state);
        assert_eq!(answer(&mut state, "q", &option(false)), Ok(()));
    }

    /// Presenting again restarts nothing: a surface that keeps re-reporting
    /// cannot keep a question from becoming answerable, nor make it so sooner.
    #[test]
    fn presentation_counts_the_first_time_only() {
        let mut state = state(gui_caps());
        ask(&mut state, "q");
        let present = |state: &mut AppState| {
            report(
                state,
                Report::GatePresented {
                    decision_id: "q".into(),
                },
            )
        };
        present(&mut state);
        let first = state.scope_grant.as_ref().unwrap().shown_at;
        std::thread::sleep(std::time::Duration::from_millis(5));
        present(&mut state);
        assert_eq!(state.scope_grant.as_ref().unwrap().shown_at, first);
    }

    /// A session that may not save cannot, however the question is asked:
    /// the refusal comes before anything about timing.
    #[test]
    fn a_session_without_persistent_answers_cannot_save_one() {
        let mut state = state(gui_caps());
        ask(&mut state, "q");
        report(
            &mut state,
            Report::GatePresented {
                decision_id: "q".into(),
            },
        );
        read(&mut state);
        let saving = option(true);
        assert_eq!(answer(&mut state, "q", &saving), Err(Refused::NotAllowed));
        assert!(!state.scope_grant.as_ref().unwrap().show_detail);
        // The narrower answers are still its to give.
        assert_eq!(answer(&mut state, "q", &option(false)), Ok(()));
    }

    /// A saving answer shows the exact line it writes first, and is taken only
    /// after a surface reported that line whole on screen (R-PERM.3.5).
    #[test]
    fn a_saving_answer_waits_for_its_line_to_be_shown() {
        let mut state = state(Capabilities::terminal());
        ask(&mut state, "q");
        read(&mut state);
        let saving = option(true);

        // First press: the line is brought into view, nothing is saved.
        assert_eq!(
            answer(&mut state, "q", &saving),
            Err(Refused::SaveLineNotShown)
        );
        assert!(state.scope_grant.as_ref().unwrap().show_detail);
        // Pressing again without it having been shown still saves nothing.
        assert_eq!(
            answer(&mut state, "q", &saving),
            Err(Refused::SaveLineNotShown)
        );
        assert!(state.scope_grant.is_some());

        // A report about another question unlocks nothing.
        report(
            &mut state,
            Report::GateSaveLine {
                decision_id: "other".into(),
                visible: true,
            },
        );
        assert_eq!(
            answer(&mut state, "q", &saving),
            Err(Refused::SaveLineNotShown)
        );

        // Shown whole: the second press saves.
        report(
            &mut state,
            Report::GateSaveLine {
                decision_id: "q".into(),
                visible: true,
            },
        );
        assert_eq!(answer(&mut state, "q", &saving), Ok(()));
        assert!(state.scope_grant.is_none());
    }

    /// Hiding the detail takes the line off screen again: the report that it
    /// was visible is forgotten with it.
    #[test]
    fn the_save_line_report_is_forgotten_when_the_detail_is_toggled_away() {
        let mut state = state(Capabilities::terminal());
        ask(&mut state, "q");
        read(&mut state);
        let saving = option(true);
        let _ = answer(&mut state, "q", &saving);
        report(
            &mut state,
            Report::GateSaveLine {
                decision_id: "q".into(),
                visible: true,
            },
        );
        apply(
            &mut state,
            Intent::ToggleScopeGrantDetail {
                decision_id: "q".into(),
            },
        )
        .unwrap();
        assert_eq!(
            answer(&mut state, "q", &saving),
            Err(Refused::SaveLineNotShown)
        );
    }

    fn op(id: &str) -> crate::state::Operation {
        crate::state::Operation::new(id, "run_terminal_command", crate::state::OpStatus::Running)
    }

    /// Showing output is a toggle on an operation that exists; one that is gone
    /// is stale, not a no-op that looks like it worked.
    #[test]
    fn an_operations_output_is_shown_and_hidden_by_name() {
        let mut state = state(Capabilities::terminal());
        state.operations.push(op("a"));
        let key = |id: &str| OpKey {
            id: id.into(),
            instance_id: None,
        };
        assert_eq!(
            apply(&mut state, Intent::ToggleOpOutput { op: key("a") }),
            Ok(())
        );
        assert_eq!(state.expanded_op.as_deref(), Some("a"));
        assert_eq!(
            apply(&mut state, Intent::ToggleOpOutput { op: key("a") }),
            Ok(())
        );
        assert_eq!(state.expanded_op, None);
        assert_eq!(
            apply(&mut state, Intent::ToggleOpOutput { op: key("gone") }),
            Err(Refused::Stale)
        );
    }

    #[test]
    fn one_section_is_open_at_a_time_and_none_closes_it() {
        let mut state = state(Capabilities::terminal());
        let open = |state: &mut AppState, key: Option<&str>| {
            apply(
                state,
                Intent::OpenSection {
                    key: key.map(str::to_string),
                },
            )
            .unwrap()
        };
        open(&mut state, Some("s1"));
        assert_eq!(state.open_section.as_deref(), Some("s1"));
        open(&mut state, Some("s2"));
        assert_eq!(state.open_section.as_deref(), Some("s2"), "one at a time");
        open(&mut state, Some("s2"));
        assert_eq!(state.open_section, None, "the open one again closes it");
        open(&mut state, Some("s1"));
        open(&mut state, None);
        assert_eq!(state.open_section, None);
        open(&mut state, None);
        assert_eq!(state.open_section, None, "closing nothing is fine");
    }

    #[test]
    fn the_project_filter_toggles() {
        let mut state = state(Capabilities::terminal());
        assert!(!state.show_all_projects);
        apply(&mut state, Intent::ToggleProjectFilter).unwrap();
        assert!(state.show_all_projects);
        apply(&mut state, Intent::ToggleProjectFilter).unwrap();
        assert!(!state.show_all_projects);
    }

    /// A surface that cannot show the terminal's model-setup wizard is not sent
    /// into it: with no model, chatting says what to do, and nothing opens.
    #[test]
    fn chatting_with_a_window_without_a_model_points_at_the_terminal() {
        let mut gui = state(gui_caps());
        assert!(gui.llm_selection.is_none());
        assert_eq!(
            apply(&mut gui, Intent::ChatWith { key: "w".into() }),
            Err(Refused::NotAllowed)
        );
        assert!(!gui.modal.is_open(), "no wizard opened");
        assert!(gui.chat.entries().iter().any(|e| matches!(e,
            crate::state::ChatEntry::Assistant { content, .. } if content.contains("ahma tui"))));
    }

    fn web_gate(state: &mut AppState, id: &str) {
        let mut gate =
            WebApprovalGate::from_request(ahma_common::web_approval::WebApprovalRequest {
                decision_id: id.into(),
                domain: "example.com".into(),
                url: "https://example.com/x".into(),
                tool: Some("fetch_webpage".into()),
            });
        gate.restart_arming(state.caps.wait_for_presentation);
        state.web_approval = Some(gate);
    }

    /// Web egress: always needs a session that may save and a second step;
    /// everything but denying waits to be read.
    #[test]
    fn a_web_question_is_taken_only_when_read_and_only_as_far_as_the_session_may() {
        let mut gui = state(gui_caps());
        web_gate(&mut gui, "w");
        assert_eq!(
            answer_web_approval(&mut gui, "w", WebAnswer::Session),
            Err(Refused::NotArmed)
        );
        report(
            &mut gui,
            Report::GatePresented {
                decision_id: "w".into(),
            },
        );
        gui.web_approval.as_mut().unwrap().shown_at =
            std::time::Instant::now() - GRANT_ARMING_DELAY * 2;
        assert_eq!(
            answer_web_approval(&mut gui, "w", WebAnswer::Always),
            Err(Refused::NotAllowed)
        );
        assert!(!gui.web_approval.as_ref().unwrap().confirm_always);
        assert_eq!(
            answer_web_approval(&mut gui, "w", WebAnswer::Session),
            Ok(())
        );

        let mut terminal = state(Capabilities::terminal());
        web_gate(&mut terminal, "w");
        terminal.web_approval.as_mut().unwrap().shown_at =
            std::time::Instant::now() - GRANT_ARMING_DELAY * 2;
        assert_eq!(
            answer_web_approval(&mut terminal, "w", WebAnswer::Always),
            Err(Refused::NeedsConfirmation)
        );
        assert!(
            terminal.web_approval.is_some(),
            "the first step saves nothing"
        );
        assert_eq!(
            answer_web_approval(&mut terminal, "w", WebAnswer::Always),
            Ok(())
        );
    }

    /// Denying a web question never waits.
    #[test]
    fn denying_a_web_question_is_immediate() {
        let mut gui = state(gui_caps());
        web_gate(&mut gui, "w");
        assert_eq!(answer_web_approval(&mut gui, "w", WebAnswer::Deny), Ok(()));
        assert!(gui.web_approval.is_none());
    }

    /// Always-allow for a tool persists, so a session that may not save
    /// cannot give it; once and reject are its to give.
    #[test]
    fn always_allow_for_a_tool_needs_a_session_that_may_save() {
        let gate = |state: &mut AppState| {
            state.approval = Some(crate::state::ApprovalGate {
                op_id: "op1".into(),
                tool: "cargo".into(),
                workspace: String::new(),
                description: String::new(),
                note: None,
                deadline: None,
                diff: None,
            });
        };
        let mut gui = state(gui_caps());
        gate(&mut gui);
        assert_eq!(
            answer_tool_approval(&mut gui, "op1", ToolAnswer::Always),
            Err(Refused::NotAllowed)
        );
        assert!(gui.approval.is_some());
        assert_eq!(
            answer_tool_approval(&mut gui, "op1", ToolAnswer::Once),
            Ok(())
        );
        assert!(gui.approval.is_none());
    }
}
