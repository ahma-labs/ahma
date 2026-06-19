//! Persistent "always allow" tool-approval grants.
//!
//! When a user chooses **always allow** for a tool, the grant is remembered so
//! the same tool is not re-prompted on every call. Grants are stored *outside*
//! any workspace sandbox — in the user's config directory
//! (`~/.config/ahma/approvals.json` on Linux/macOS) — so a sandboxed agent
//! cannot read or tamper with the list of what it has been trusted to run.
//!
//! Grants are keyed by **workspace root**: trusting `cargo_build` in one
//! project does not silently trust it in another. The on-disk shape is:
//!
//! ```json
//! {
//!   "/Users/you/sandbox/ahma": ["list_dir", "cargo_build"]
//! }
//! ```
//!
//! The read path ([`is_tool_approved`]) is `async` because it runs inside the
//! agent turn (no blocking I/O in async — see AGENTS.md). The write path
//! ([`remember_tool_approval`]) is synchronous because it is invoked from the
//! TUI's (non-async) input handler.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

type Grants = BTreeMap<String, Vec<String>>;

/// Base config directory for ahma. Honors `AHMA_CONFIG_DIR` (used by tests and
/// for relocating config), else falls back to the platform config dir.
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("AHMA_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .map(|d| d.join("ahma"))
}

/// Location of the persisted approvals file, or `None` if no config directory
/// can be determined for this platform/user.
fn approvals_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("approvals.json"))
}

/// Build the map key from a (possibly failed) canonicalisation, falling back to
/// the original path's lossy string. Shared by the sync and async key paths so
/// the TUI (writer) and agent (reader) agree on the exact same key.
fn workspace_key_from(canonical: std::io::Result<PathBuf>, fallback: &Path) -> String {
    canonical
        .unwrap_or_else(|_| fallback.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Normalise a workspace path into the map key (synchronous; for the TUI's
/// non-async handler). Canonicalises when possible so a symlinked or
/// non-normalised path still matches; falls back to the lossy string otherwise.
fn workspace_key(workspace: &Path) -> String {
    workspace_key_from(std::fs::canonicalize(workspace), workspace)
}

/// Async counterpart used inside the agent turn — canonicalises via
/// `tokio::fs` so no blocking I/O happens on the async runtime (see AGENTS.md).
async fn workspace_key_async(workspace: &Path) -> String {
    workspace_key_from(tokio::fs::canonicalize(workspace).await, workspace)
}

fn parse_grants(content: &str) -> Grants {
    serde_json::from_str(content).unwrap_or_default()
}

fn read_grants_sync() -> Grants {
    approvals_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|c| parse_grants(&c))
        .unwrap_or_default()
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

fn classify(grants: &Grants, key: &str, tool: &str) -> GrantStatus {
    let here = grants.get(key);
    if here.is_some_and(|tools| tools.iter().any(|t| t == tool)) {
        return GrantStatus::ApprovedHere;
    }
    let elsewhere = grants
        .iter()
        .any(|(k, tools)| k != key && tools.iter().any(|t| t == tool));
    if elsewhere {
        GrantStatus::GrantedElsewhere
    } else if here.is_some() {
        GrantStatus::Unseen
    } else {
        GrantStatus::NewWorkspace
    }
}

/// Classify `tool` against the persisted grants (synchronous; intended for the
/// TUI's non-async event handler, where it runs only when a banner appears).
pub fn grant_status(workspace: &Path, tool: &str) -> GrantStatus {
    classify(&read_grants_sync(), &workspace_key(workspace), tool)
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

/// Returns `true` if `tool` has been granted "always allow" for `workspace`.
///
/// Never errors: a missing or unreadable file simply means "not approved", so
/// the caller falls back to prompting — failing closed.
pub async fn is_tool_approved(workspace: &Path, tool: &str) -> bool {
    let Some(path) = approvals_path() else {
        return false;
    };
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(_) => return false, // not yet created / unreadable → prompt
    };
    let grants = parse_grants(&content);
    let key = workspace_key_async(workspace).await;
    grants
        .get(&key)
        .is_some_and(|tools| tools.iter().any(|t| t == tool))
}

/// Persist an "always allow" grant for `tool` in `workspace`.
///
/// Reads the current file (if any), inserts the grant idempotently, and writes
/// it back. Creates the config directory if needed. Errors are logged and
/// returned; a failed write degrades gracefully to re-prompting next time.
pub fn remember_tool_approval(workspace: &Path, tool: &str) -> std::io::Result<()> {
    let Some(path) = approvals_path() else {
        warn!("approvals: no config directory available; cannot persist grant");
        return Ok(());
    };

    let mut grants: Grants = std::fs::read_to_string(&path)
        .map(|c| parse_grants(&c))
        .unwrap_or_default();

    let key = workspace_key(workspace);
    let entry = grants.entry(key).or_default();
    if !entry.iter().any(|t| t == tool) {
        entry.push(tool.to_string());
        entry.sort();
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_string_pretty(&grants)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, serialized)?;
    debug!("approvals: persisted always-allow for {tool}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_or_garbage_yields_no_grants() {
        assert!(parse_grants("").is_empty());
        assert!(parse_grants("not json").is_empty());
        assert!(parse_grants("{}").is_empty());
    }

    #[test]
    fn parse_roundtrips_grants() {
        let mut g: Grants = BTreeMap::new();
        g.insert("/ws".to_string(), vec!["list_dir".to_string()]);
        let s = serde_json::to_string(&g).unwrap();
        let back = parse_grants(&s);
        assert_eq!(back.get("/ws").unwrap(), &vec!["list_dir".to_string()]);
    }

    #[test]
    fn workspace_key_is_stable_for_same_path() {
        let p = Path::new("/some/workspace/path");
        assert_eq!(workspace_key(p), workspace_key(p));
    }

    fn grants_of(pairs: &[(&str, &[&str])]) -> Grants {
        pairs
            .iter()
            .map(|(k, tools)| (k.to_string(), tools.iter().map(|t| t.to_string()).collect()))
            .collect()
    }

    #[test]
    fn classify_approved_here() {
        let g = grants_of(&[("/ws", &["list_dir"])]);
        assert_eq!(classify(&g, "/ws", "list_dir"), GrantStatus::ApprovedHere);
    }

    #[test]
    fn classify_new_workspace_when_no_key_and_no_grant_anywhere() {
        let g = grants_of(&[("/other", &["cargo_build"])]);
        assert_eq!(classify(&g, "/ws", "list_dir"), GrantStatus::NewWorkspace);
    }

    #[test]
    fn classify_granted_elsewhere_signals_reask() {
        // Same tool allowed under a different workspace path (e.g. canonical
        // mismatch or a different project) → we re-ask in this workspace.
        let g = grants_of(&[("/other/path", &["list_dir"])]);
        assert_eq!(
            classify(&g, "/ws", "list_dir"),
            GrantStatus::GrantedElsewhere
        );
    }

    #[test]
    fn classify_unseen_when_workspace_known_but_tool_isnt() {
        let g = grants_of(&[("/ws", &["cargo_build"])]);
        assert_eq!(classify(&g, "/ws", "list_dir"), GrantStatus::Unseen);
    }

    #[test]
    fn notes_only_for_new_or_elsewhere() {
        assert!(reask_note_for(GrantStatus::NewWorkspace).is_some());
        assert!(reask_note_for(GrantStatus::GrantedElsewhere).is_some());
        assert!(reask_note_for(GrantStatus::Unseen).is_none());
        assert!(reask_note_for(GrantStatus::ApprovedHere).is_none());
    }
}
