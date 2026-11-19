//! Who made a grant, read from the `granted_by` text a grant carries.
//!
//! `granted_by` is free text in the settings file and the session-grant store,
//! and a decision hangs on it: a grant the harness dialog's yes made is no
//! deliberate setup (SPEC R-PERM.11), so it never stops the "different
//! repository" question. Comparing the text to one spelling missed a second
//! one, `session grant (harness dialog)`, which is how a dialog's session grant
//! reads once it is shown with the saved ones
//! (`session_grants::as_persistent_scopes`). Reading the text once, into
//! [`GrantedBy`], makes "who made it" one exhaustive match.

use crate::scope_grant::GRANTED_BY_HARNESS_DIALOG;

/// How a session grant's provenance is written beside the saved ones:
/// `session grant (<by>)`, or `session grant` with no maker.
const SESSION_PREFIX: &str = "session grant";

/// Who made a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantedBy {
    /// No `granted_by` at all: a grant written by hand into the settings file,
    /// or by a version of ahma that did not record it.
    Unmarked,
    /// The harness dialog's yes (SPEC R-PERM.10): never typed or confirmed by
    /// a human for this folder.
    HarnessDialog,
    /// A grant for this session only, made by the inner maker.
    Session(Box<GrantedBy>),
    /// Any other named maker: `cli`, `tui`, a tool's name.
    Named(String),
}

impl GrantedBy {
    /// Read a grant's `granted_by` text.
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::Unmarked;
        };
        if raw == GRANTED_BY_HARNESS_DIALOG {
            return Self::HarnessDialog;
        }
        match raw.strip_prefix(SESSION_PREFIX) {
            Some("") => Self::Session(Box::new(Self::Unmarked)),
            Some(rest) => match rest
                .strip_prefix(" (")
                .and_then(|inner| inner.strip_suffix(')'))
            {
                Some(inner) => Self::Session(Box::new(Self::parse(Some(inner)))),
                None => Self::Named(raw.to_string()),
            },
            None => Self::Named(raw.to_string()),
        }
    }

    /// The `granted_by` text this is read from; `None` for [`Self::Unmarked`].
    pub fn label(&self) -> Option<String> {
        match self {
            Self::Unmarked => None,
            Self::HarnessDialog => Some(GRANTED_BY_HARNESS_DIALOG.to_string()),
            Self::Session(inner) => Some(match inner.label() {
                Some(by) => format!("{SESSION_PREFIX} ({by})"),
                None => SESSION_PREFIX.to_string(),
            }),
            Self::Named(by) => Some(by.clone()),
        }
    }

    /// Whether a person set this grant up on purpose (SPEC R-PERM.11): anything
    /// but the harness dialog's yes, for the session or saved.
    pub fn is_deliberate(&self) -> bool {
        match self {
            Self::HarnessDialog => false,
            Self::Session(inner) => inner.is_deliberate(),
            Self::Unmarked | Self::Named(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spelling_of_the_dialog_is_not_deliberate_and_every_label_round_trips() {
        for (raw, deliberate) in [
            (None, true),
            (Some("harness dialog"), false),
            (Some("session grant (harness dialog)"), false),
            (Some("session grant"), true),
            (Some("session grant (cli)"), true),
            (
                Some("session grant (session grant (harness dialog))"),
                false,
            ),
            (Some("cli"), true),
            (Some("tui"), true),
            (Some("session granted by hand"), true),
            (Some("session grant (unclosed"), true),
        ] {
            let by = GrantedBy::parse(raw);
            assert_eq!(by.is_deliberate(), deliberate, "{raw:?} -> {by:?}");
            assert_eq!(by.label().as_deref(), raw, "{raw:?} round-trips");
        }
    }
}
