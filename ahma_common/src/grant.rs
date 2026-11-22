//! A grant as every consumer sees it: [`ResolvedGrant`], built from whichever
//! store holds it.
//!
//! Grants live in two stores with two serde shapes, and neither shape changes
//! here: the settings file's `[sandbox].persistent_scopes`
//! ([`PersistentScope`], saved grants and leases, SPEC R-PERM.2.3) and the
//! session-grant store ([`SessionGrant`], SPEC R-PERM.4.4). What the sandbox,
//! the edit guard and the harness-ask hook need from either is one shape:
//! which folder, what access, which workspaces it applies to, who made it and
//! how long it lives. A session grant once rode through those consumers
//! dressed as a saved one, with "for this session only" in its note; now how
//! long a grant lives is its [`GrantLife`], read from the store it came from,
//! never from text.
//!
//! `granted_by` is free text in both stores, and a decision hangs on it: a
//! grant the harness dialog's yes made is no deliberate setup (SPEC
//! R-PERM.11), so it never stops the "different repository" question. Reading
//! the text once, into [`GrantedBy`], makes "who made it" one exhaustive
//! match. A session grant carries its maker as made; it was once wrapped as
//! `session grant (<maker>)`, a second spelling a text comparison missed.
//!
//! The session-grant store, the link session grants and the ssh-sign grants
//! hold a [`GrantedBy`] itself, and serde stores it as the same optional
//! text it always was: a maker is written as the name it has always had
//! (`From<ApproverSurface>`), and any text already on disk reads back
//! unchanged. The settings file's `persistent_scopes` still keep the text and
//! read it here.
//!
//! Paths are kept as the store holds them. They are compared canonical where
//! they are compared ([`crate::scope_grant::grant_applies`],
//! [`crate::harness_asks::grant_covers`]), and the sandbox resolves them
//! before it applies them (creating a missing read-write folder, skipping a
//! missing read-only one), which a path type made canonical at construction
//! could not express.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::approver::ApproverSurface;
use crate::config::{PersistentScope, ScopeAccess};
use crate::scope_grant::GRANTED_BY_HARNESS_DIALOG;
use crate::session_grants::SessionGrant;

/// Who made a grant.
///
/// Stored as the optional text a record's `granted_by` has always held
/// ([`Self::as_str`]): `Unmarked` is no text, so a field of this type takes
/// `#[serde(default, skip_serializing_if = "GrantedBy::is_unmarked")]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum GrantedBy {
    /// No `granted_by` at all: a grant written by hand into the settings file,
    /// or by a version of ahma that did not record it.
    #[default]
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
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Unmarked => None,
            Self::HarnessDialog => Some(GRANTED_BY_HARNESS_DIALOG),
            Self::Named(by) => Some(by),
        }
    }

    /// [`Self::as_str`], owned.
    pub fn label(&self) -> Option<String> {
        self.as_str().map(str::to_string)
    }

    /// No maker recorded: the field is left out of the record.
    pub fn is_unmarked(&self) -> bool {
        matches!(self, Self::Unmarked)
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

/// A grant a human made at an approver surface records it under the name the
/// surface has always had (`tui`, `web`, `cli`, …; SPEC R-GUI.7).
impl From<ApproverSurface> for GrantedBy {
    fn from(surface: ApproverSurface) -> Self {
        Self::from(&surface)
    }
}

impl From<&ApproverSurface> for GrantedBy {
    fn from(surface: &ApproverSurface) -> Self {
        Self::Named(surface.to_string())
    }
}

impl Serialize for GrantedBy {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_str().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for GrantedBy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<String>::deserialize(deserializer).map(|raw| Self::parse(raw.as_deref()))
    }
}

/// Which sessions a grant applies to (SPEC R5.4.11).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Binding {
    /// Made for this workspace (project root): applies to a session working
    /// inside it, around it, or in a worktree of it
    /// ([`crate::scope_grant::grant_applies`]).
    Workspace(PathBuf),
    /// A legacy saved grant with no workspace: applies to every session.
    Global,
}

impl Binding {
    /// The binding a stored `workspace` field names: `None` is global.
    pub fn from_workspace(workspace: Option<&Path>) -> Self {
        workspace.map_or(Self::Global, |w| Self::Workspace(w.to_path_buf()))
    }

    /// The workspace, `None` for [`Self::Global`].
    pub fn workspace(&self) -> Option<&Path> {
        match self {
            Self::Workspace(w) => Some(w),
            Self::Global => None,
        }
    }
}

/// How long a grant lives, from the store it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GrantLife {
    /// In the settings file until revoked.
    Saved,
    /// In the settings file, applied until `expires_at` (Unix seconds; SPEC
    /// R-PERM.2.3).
    Lease { expires_at: u64 },
    /// In the session-grant store, alive while `owner_pid` lives (at most
    /// [`crate::session_grants::MAX_AGE_SECS`] from `granted_at`, Unix
    /// seconds; SPEC R-PERM.4.4). Never in the settings file, so a reload of
    /// that file leaves it alone.
    Session { owner_pid: u32, granted_at: u64 },
}

/// One grant in force or on record, whichever store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGrant {
    /// The granted folder, as its store holds it.
    pub path: PathBuf,
    pub access: ScopeAccess,
    pub binding: Binding,
    pub by: GrantedBy,
    pub life: GrantLife,
}

impl ResolvedGrant {
    /// A grant from the settings file: saved, or a lease when it expires.
    /// Its note and date are the human's record, and decide nothing.
    pub fn saved(scope: &PersistentScope) -> Self {
        Self {
            path: scope.path.clone(),
            access: scope.access,
            binding: Binding::from_workspace(scope.workspace.as_deref()),
            by: GrantedBy::parse(scope.granted_by.as_deref()),
            life: scope
                .expires_at
                .map_or(GrantLife::Saved, |expires_at| GrantLife::Lease {
                    expires_at,
                }),
        }
    }

    /// A grant from the session-grant store.
    pub fn session(grant: &SessionGrant) -> Self {
        Self {
            path: grant.path.clone(),
            access: grant.access,
            binding: Binding::Workspace(grant.workspace.clone()),
            by: grant.granted_by.clone(),
            life: GrantLife::Session {
                owner_pid: grant.owner_pid,
                granted_at: grant.granted_at,
            },
        }
    }

    /// The workspace it was made for; `None` for a legacy global grant.
    pub fn workspace(&self) -> Option<&Path> {
        self.binding.workspace()
    }

    /// When a lease stops applying; `None` for anything else.
    pub fn expires_at(&self) -> Option<u64> {
        match self.life {
            GrantLife::Lease { expires_at } => Some(expires_at),
            GrantLife::Saved | GrantLife::Session { .. } => None,
        }
    }

    /// Whether it applies at `now` (Unix seconds): a lease until it expires,
    /// as [`PersistentScope::applies_at`]. A session grant read from its store
    /// is alive (the store drops a dead or expired one as it reads).
    pub fn applies_at(&self, now: u64) -> bool {
        self.expires_at().is_none_or(|at| at > now)
    }

    /// Whether it applies to a session whose committed scopes are `scopes`:
    /// the one workspace rule every surface uses (SPEC R5.4.11).
    pub fn applies_to(&self, scopes: &[PathBuf]) -> bool {
        crate::scope_grant::grant_applies(self.workspace(), scopes)
    }

    /// Whether it lives in the session-grant store rather than the settings
    /// file.
    pub fn is_session(&self) -> bool {
        matches!(self.life, GrantLife::Session { .. })
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

    /// `granted_by` is a field of three on-disk records (a session grant, a
    /// link session grant, an ssh-sign grant), stored as plain text. Each
    /// maker writes exactly the text it wrote before the field was typed,
    /// reads back as the maker it was, and text this build never writes (a
    /// tool's name, a newer surface) survives a read and a write unchanged.
    #[test]
    fn granted_by_is_stored_as_the_text_each_maker_has_always_written() {
        use serde_json::{Value, json};

        #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Record {
            #[serde(default, skip_serializing_if = "GrantedBy::is_unmarked")]
            granted_by: GrantedBy,
        }

        let made = [
            (GrantedBy::from(ApproverSurface::Tui), Some("tui")),
            (GrantedBy::from(ApproverSurface::Web), Some("web")),
            (GrantedBy::from(ApproverSurface::Desktop), Some("desktop")),
            (GrantedBy::from(ApproverSurface::Cli), Some("cli")),
            (GrantedBy::from(ApproverSurface::Harness), Some("harness")),
            (GrantedBy::HarnessDialog, Some("harness dialog")),
            (GrantedBy::Unmarked, None),
        ];
        for (by, text) in made {
            let stored = serde_json::to_value(Record {
                granted_by: by.clone(),
            })
            .unwrap();
            assert_eq!(
                stored.get("granted_by").and_then(Value::as_str),
                text,
                "{by:?} is stored as {text:?}, and an unmarked one not at all"
            );
            assert_eq!(by.as_str(), text, "{by:?}");
            let read: Record = serde_json::from_value(stored).unwrap();
            assert_eq!(read.granted_by, by, "{text:?} reads back as its maker");
        }

        for text in ["sccache", "logs_approve", "a surface from a newer ahma", ""] {
            let read: Record = serde_json::from_value(json!({ "granted_by": text })).unwrap();
            assert_eq!(read.granted_by, GrantedBy::Named(text.into()));
            assert_eq!(
                serde_json::to_value(&read).unwrap(),
                json!({ "granted_by": text }),
                "{text:?} survives a read and a write"
            );
        }
        for absent in [json!({}), json!({ "granted_by": null })] {
            let read: Record = serde_json::from_value(absent.clone()).unwrap();
            assert_eq!(read.granted_by, GrantedBy::Unmarked, "{absent}");
            assert_eq!(serde_json::to_value(&read).unwrap(), json!({}));
        }
    }

    fn scope(note: Option<&str>, expires_at: Option<u64>) -> PersistentScope {
        PersistentScope {
            path: PathBuf::from("/cache/a"),
            access: ScopeAccess::Rw,
            workspace: Some(PathBuf::from("/ws/a")),
            granted_by: Some("cli".into()),
            granted_at: Some("2026-11-21".into()),
            note: note.map(str::to_string),
            expires_at,
        }
    }

    /// How long a grant lives comes from the store it was read from. The
    /// session tier was told apart by a note ("for this session only (owner
    /// pid …)") written onto a fabricated saved record, so a saved grant with
    /// that note was indistinguishable from one.
    #[test]
    fn a_grant_lives_as_its_store_says_whatever_its_note_reads() {
        for note in [
            None,
            Some("for this session only (owner pid 7)"),
            Some("session grant (harness dialog)"),
        ] {
            let g = ResolvedGrant::saved(&scope(note, None));
            assert_eq!(g.life, GrantLife::Saved, "{note:?}");
            assert!(!g.is_session(), "{note:?}");
            let g = ResolvedGrant::saved(&scope(note, Some(500)));
            assert_eq!(g.life, GrantLife::Lease { expires_at: 500 }, "{note:?}");
        }
        let session = ResolvedGrant::session(&SessionGrant {
            path: PathBuf::from("/cache/a"),
            access: ScopeAccess::Ro,
            workspace: PathBuf::from("/ws/a"),
            owner_pid: 7,
            granted_at: 100,
            granted_by: GrantedBy::HarnessDialog,
        });
        assert_eq!(
            session.life,
            GrantLife::Session {
                owner_pid: 7,
                granted_at: 100
            }
        );
        assert!(session.is_session());
        assert_eq!(session.by, GrantedBy::HarnessDialog, "its maker, as made");
        assert_eq!(session.workspace(), Some(Path::new("/ws/a")));
    }

    /// A lease applies exactly as long as the saved record it came from says.
    #[test]
    fn a_lease_applies_while_its_record_does() {
        for (expires_at, now) in [
            (None, 0),
            (Some(500), 499),
            (Some(500), 500),
            (Some(500), 501),
        ] {
            let record = scope(None, expires_at);
            assert_eq!(
                ResolvedGrant::saved(&record).applies_at(now),
                record.applies_at(now),
                "{expires_at:?} at {now}"
            );
        }
        let mut global = scope(None, None);
        global.workspace = None;
        assert_eq!(ResolvedGrant::saved(&global).binding, Binding::Global);
    }
}
