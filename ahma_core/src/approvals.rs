//! Persistent "always allow" tool-approval grants.
//!
//! When a user chooses **always allow** for a tool, the grant is remembered so
//! the same tool is not re-prompted on every call.
//!
//! ## Where these live, and why it changed
//!
//! Grants used to live in their own file, `~/.config/ahma/approvals.json`. They
//! now live in the **unified permission ledger** (`~/.ahma/settings.toml`,
//! `[permissions].tool_approvals`) alongside every other kind of permission
//! ahma grants — filesystem scopes, web domains, hook consent. See SPEC R-PERM.1.
//!
//! The move is not cosmetic. `~/.ahma` is the one directory the sandbox **never**
//! includes (SPEC R5.4.8): it is kernel-unreadable *and* kernel-unwritable from
//! inside the sandbox, so a sandboxed agent can neither read the list of what it
//! has been trusted to run nor add itself to it. `~/.config/ahma` had no such
//! guarantee. One ledger, one place to look, one set of properties.
//!
//! Existing `approvals.json` files are migrated once, non-destructively, by
//! [`ahma_common::permissions::migrate_legacy_approvals`] — called at process
//! startup, and lazily here so a grant made in a process that skipped startup
//! migration still lands in the right place.
//!
//! Grants remain keyed by **workspace root**: trusting `cargo_build` in one
//! project does not silently trust it in another.
//!
//! The read path ([`is_tool_approved`]) is `async` because it runs inside the
//! agent turn (no blocking I/O in async — see AGENTS.md). The write path
//! ([`remember_tool_approval`]) is synchronous because it is invoked from the
//! TUI's (non-async) input handler.

use std::path::Path;
use std::sync::OnceLock;

use ahma_common::config::{AhmaSettings, settings_path};
use ahma_common::permissions::{
    AuditAction, GrantKind, GrantTier, PermissionSettings, append_audit, audit_entry,
    migrate_legacy_approvals, workspace_key, workspace_key_async,
};
use tracing::{debug, warn};

/// Run the legacy-approvals migration at most once per process.
///
/// Startup already calls the migration, but the approvals path is also reached
/// from contexts that may not have gone through it (a short-lived CLI, a test).
/// The migration is cheap when there is nothing to migrate (one `stat` of a file
/// that almost never exists) and idempotent, so guarding it with a `OnceLock` and
/// calling it from both entry points costs nothing and closes the gap.
fn migrate_once() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        if let Some(file) = settings_path()
            && let Err(e) = migrate_legacy_approvals(&file)
        {
            warn!("approvals: legacy migration failed: {e:#}");
        }
    });
}

/// Load the ledger's permission table. A missing or unreadable settings file
/// yields an empty table — which reads as "nothing approved", so the caller
/// prompts. Failing closed is the only safe direction here.
fn load_permissions() -> PermissionSettings {
    let Some(path) = settings_path() else {
        return PermissionSettings::default();
    };
    AhmaSettings::load_from(&path).permissions
}

/// Where a tool stands relative to the persisted grants for a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantStatus {
    /// Granted for this exact workspace — the agent would not prompt.
    ApprovedHere,
    /// Granted, but only under a *different* workspace key. Usually a fresh or
    /// relocated workspace (or a canonical-path mismatch), so we re-ask here.
    GrantedElsewhere,
    /// This workspace has no grants at all yet — a brand-new sandbox.
    NewWorkspace,
    /// Workspace already has grants, just not for this tool.
    Unseen,
}

fn classify(perms: &PermissionSettings, key: &Path, tool: &str) -> GrantStatus {
    if perms.is_tool_approved(key, tool) {
        return GrantStatus::ApprovedHere;
    }
    if perms.tool_approved_elsewhere(key, tool) {
        GrantStatus::GrantedElsewhere
    } else if perms.workspace_known(key) {
        GrantStatus::Unseen
    } else {
        GrantStatus::NewWorkspace
    }
}

/// Classify `tool` against the persisted grants (synchronous; intended for the
/// TUI's non-async event handler, where it runs only when a banner appears).
pub fn grant_status(workspace: &Path, tool: &str) -> GrantStatus {
    migrate_once();
    classify(&load_permissions(), &workspace_key(workspace), tool)
}

/// A short, dim note explaining the workspace scope when a prompt appears in an
/// unfamiliar context — or `None` when the workspace is already established and
/// no extra context is warranted. Kept terse on purpose.
pub fn reask_note(workspace: &Path, tool: &str) -> Option<String> {
    reask_note_for(grant_status(workspace, tool))
}

fn reask_note_for(status: GrantStatus) -> Option<String> {
    match status {
        // Allowed before, but for a different workspace path — explain the re-ask.
        GrantStatus::GrantedElsewhere => {
            Some("new workspace — re-confirm to allow it here too".to_string())
        }
        // First prompt in a fresh sandbox — flag that grants start empty here.
        GrantStatus::NewWorkspace => Some("new workspace — approvals start fresh".to_string()),
        GrantStatus::ApprovedHere | GrantStatus::Unseen => None,
    }
}

/// Built-in tools that mutate nothing and take no path, so an argument preview
/// on their approval prompt would be noise.
///
/// Everything *not* on this list previews — see [`shows_argument_preview`].
const PREVIEW_EXEMPT_TOOLS: &[&str] = &["status", "await", "cancel"];

/// Whether an approval prompt for `tool` should show its arguments.
///
/// Defaults to **yes**, and the exemption list is the narrow, known-harmless
/// case. That direction matters: the operator is being asked to authorise this
/// call, and the arguments are what the call will actually do. The previous
/// rule was the other way round — a positive guess,
/// `tool.contains("replace") || tool == "write_file"`, made inside the shared
/// approval funnel. Any file-mutating tool that did not match the guess
/// (`edit_file`, `apply_patch`, `create_file`, `sed`, `mv`) rendered a prompt
/// with no arguments at all and the operator approved blind — and it could
/// never be right for MTDF-defined custom tools, whose names ahma cannot know.
///
/// Lives here, beside [`reask_note`], so every approval surface asks the same
/// mechanism instead of pattern-matching a tool name locally.
pub fn shows_argument_preview(tool: &str) -> bool {
    !PREVIEW_EXEMPT_TOOLS.contains(&tool)
}

/// The argument preview for `tool`'s approval prompt: its call arguments,
/// pretty-printed, or `None` when the tool is exempt or the arguments are not
/// JSON.
pub fn argument_preview(tool: &str, args: &str) -> Option<String> {
    if !shows_argument_preview(tool) {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|val| serde_json::to_string_pretty(&val).ok())
}

/// Returns `true` if `tool` has been granted "always allow" for `workspace`.
///
/// Never errors: a missing or unreadable ledger simply means "not approved", so
/// the caller falls back to prompting — failing closed. In particular, when ahma
/// runs *inside its own sandbox*, reading `~/.ahma` is denied by the kernel by
/// design (R5.4.8); that reads as "not approved" and prompts, rather than
/// wedging.
pub async fn is_tool_approved(workspace: &Path, tool: &str) -> bool {
    let Some(path) = settings_path() else {
        return false;
    };
    let contents = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(_) => return false, // not yet created / unreadable → prompt
    };
    let settings = match AhmaSettings::parse(&contents) {
        Ok(s) => s,
        Err(e) => {
            warn!("approvals: settings file does not parse ({e}); treating as not approved");
            return false;
        }
    };
    let key = workspace_key_async(workspace).await;
    settings.permissions.is_tool_approved(&key, tool)
}

/// Persist an "always allow" grant for `tool` in `workspace`.
///
/// Reads the current ledger, inserts the grant idempotently, and writes it back.
/// Errors are logged and returned; a failed write degrades gracefully to
/// re-prompting next time.
pub fn remember_tool_approval(workspace: &Path, tool: &str) -> std::io::Result<()> {
    migrate_once();
    let Some(path) = settings_path() else {
        warn!("approvals: no home directory available; cannot persist grant");
        return Ok(());
    };

    // Strict load on the write path: never clobber a settings file we cannot
    // parse — it holds every other permission the user has granted.
    let mut settings = AhmaSettings::load_from_result(&path).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("refusing to overwrite unparseable {}: {e}", path.display()),
        )
    })?;

    let key = workspace_key(workspace);
    let now = chrono::Local::now();
    let added = settings.permissions.approve_tool(
        &key,
        tool,
        Some(now.format("%Y-%m-%d").to_string()),
        Some("tui".to_string()),
    );

    settings
        .save_to(&path)
        .map_err(|e| std::io::Error::other(format!("failed to write {}: {e:#}", path.display())))?;

    if added {
        append_audit(&audit_entry(
            now.to_rfc3339(),
            AuditAction::Grant,
            GrantKind::Tool,
            tool,
            None,
            GrantTier::Always,
            Some("tui".to_string()),
        ));
    }
    debug!("approvals: persisted always-allow for {tool}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn perms(pairs: &[(&str, &[&str])]) -> PermissionSettings {
        let mut p = PermissionSettings::default();
        for (ws, tools) in pairs {
            for t in *tools {
                p.approve_tool(&PathBuf::from(ws), t, None, None);
            }
        }
        p
    }

    #[test]
    fn classify_approved_here() {
        let p = perms(&[("/ws", &["list_dir"])]);
        assert_eq!(
            classify(&p, Path::new("/ws"), "list_dir"),
            GrantStatus::ApprovedHere
        );
    }

    #[test]
    fn classify_new_workspace_when_no_key_and_no_grant_anywhere() {
        let p = perms(&[("/other", &["cargo_build"])]);
        assert_eq!(
            classify(&p, Path::new("/ws"), "list_dir"),
            GrantStatus::NewWorkspace
        );
    }

    #[test]
    fn classify_granted_elsewhere_signals_reask() {
        // Same tool allowed under a different workspace path (e.g. canonical
        // mismatch or a different project) → we re-ask in this workspace.
        let p = perms(&[("/other/path", &["list_dir"])]);
        assert_eq!(
            classify(&p, Path::new("/ws"), "list_dir"),
            GrantStatus::GrantedElsewhere
        );
    }

    #[test]
    fn classify_unseen_when_workspace_known_but_tool_isnt() {
        let p = perms(&[("/ws", &["cargo_build"])]);
        assert_eq!(
            classify(&p, Path::new("/ws"), "list_dir"),
            GrantStatus::Unseen
        );
    }

    /// A file-mutating tool that the old name guess did not match rendered an
    /// approval prompt with no arguments, so the operator approved blind. The
    /// default must be to show them.
    #[test]
    fn argument_preview_shown_for_tools_the_name_guess_would_have_missed() {
        for tool in [
            "edit_file",
            "apply_patch",
            "create_file",
            "sed",
            "mv",
            "some_mtdf_custom_tool",
        ] {
            assert!(
                shows_argument_preview(tool),
                "{tool} must show its arguments on an approval prompt"
            );
            assert!(
                argument_preview(tool, r#"{"path":"/etc/hosts"}"#).is_some(),
                "{tool} must render an argument preview"
            );
        }
    }

    #[test]
    fn argument_preview_exempts_the_read_only_builtins() {
        for tool in PREVIEW_EXEMPT_TOOLS {
            assert!(!shows_argument_preview(tool));
            assert!(argument_preview(tool, "{}").is_none());
        }
    }

    #[test]
    fn argument_preview_is_none_for_non_json_arguments() {
        assert!(argument_preview("write_file", "not json").is_none());
    }

    #[test]
    fn notes_only_for_new_or_elsewhere() {
        assert!(reask_note_for(GrantStatus::NewWorkspace).is_some());
        assert!(reask_note_for(GrantStatus::GrantedElsewhere).is_some());
        assert!(reask_note_for(GrantStatus::Unseen).is_none());
        assert!(reask_note_for(GrantStatus::ApprovedHere).is_none());
    }
}
