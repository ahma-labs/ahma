//! Who made a grant, read from the `granted_by` text a grant carries.
//!
//! `granted_by` is free text in the settings file and the session-grant store,
//! and a decision hangs on it: a grant the harness dialog's yes made is no
//! deliberate setup (SPEC R-PERM.11), so it never stops the "different
//! repository" question. Reading the text once, into [`GrantedBy`], makes
//! "who made it" one exhaustive match. A session grant carries its maker as
//! made (`session_grants::as_persistent_scopes`); it was once wrapped as
//! `session grant (<maker>)`, a second spelling a text comparison missed.

use crate::scope_grant::GRANTED_BY_HARNESS_DIALOG;

/// Who made a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantedBy {
    /// No `granted_by` at all: a grant written by hand into the settings file,
    /// or by a version of ahma that did not record it.
    Unmarked,
    /// The harness dialog's yes (SPEC R-PERM.10): never typed or confirmed by
    /// a human for this folder.
    HarnessDialog,
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
        Self::Named(raw.to_string())
    }

    /// The `granted_by` text this is read from; `None` for [`Self::Unmarked`].
    pub fn label(&self) -> Option<String> {
        match self {
            Self::Unmarked => None,
            Self::HarnessDialog => Some(GRANTED_BY_HARNESS_DIALOG.to_string()),
            Self::Named(by) => Some(by.clone()),
        }
    }

    /// Whether a person set this grant up on purpose (SPEC R-PERM.11): anything
    /// but the harness dialog's yes, for the session or saved.
    pub fn is_deliberate(&self) -> bool {
        match self {
            Self::HarnessDialog => false,
            Self::Unmarked | Self::Named(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_dialog_is_not_deliberate_and_every_label_round_trips() {
        for (raw, deliberate) in [
            (None, true),
            (Some("harness dialog"), false),
            (Some("cli"), true),
            (Some("tui"), true),
            (Some("granted by hand"), true),
        ] {
            let by = GrantedBy::parse(raw);
            assert_eq!(by.is_deliberate(), deliberate, "{raw:?} -> {by:?}");
            assert_eq!(by.label().as_deref(), raw, "{raw:?} round-trips");
        }
    }
}
