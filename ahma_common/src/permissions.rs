//! The unified permission ledger (SPEC R-PERM.1 / R-PERM.2).
//!
//! ahma grants permission for several different things — a filesystem scope
//! outside the workspace, an outbound web domain, a tool the agent may run
//! without re-asking, an unsandboxed terminal hook. Historically each of those
//! lived in its own store, and two of them lived in *different config trees*:
//! scope grants in `~/.ahma/settings.toml`, tool approvals in
//! `~/.config/ahma/approvals.json`. That split has a real cost — the user has
//! two places to look, two things to back up, and only one of them is covered
//! by the property that makes the whole model safe.
//!
//! That property is [SPEC R5.4.8]: `~/.ahma` is **never** part of any workspace
//! scope, so it is kernel-unreadable *and* kernel-unwritable from inside the
//! sandbox. A sandboxed command therefore cannot author a grant for itself, no
//! matter how thoroughly it is compromised. A store that lives anywhere else
//! does not get that guarantee for free. So: **one ledger, in `~/.ahma`**.
//!
//! ## What this module owns
//!
//! * [`GrantRecord`] — the one record shape every kind of permission normalizes
//!   into, so `ahma permissions list` can render them together and the audit log
//!   has a single schema.
//! * [`GrantTier`] — `once` / `session` / `always`. Only `always` is ever
//!   written to disk; `session` lives in [`SessionGrants`] and dies with the
//!   process; `once` is never stored anywhere.
//! * [`PermissionSettings`] — the `[permissions]` table in `settings.toml`,
//!   currently holding per-workspace tool approvals (filesystem scopes remain in
//!   `[sandbox].persistent_scopes`, their established home; web domains remain in
//!   `[web]`). [`records`] folds all three into one view.
//! * [`migrate_legacy_approvals`] — the one-time, non-destructive move of
//!   `~/.config/ahma/approvals.json` into the ledger.
//! * [`append_audit`] — an append-only JSONL trail of every persist and revoke.
//!
//! ## What this module deliberately does *not* own
//!
//! The **asking** — which surface gets the question, and in what order — is the
//! question ladder (R-PERM.3), and it lives with the broker. This module is the
//! ledger: it answers "what has been granted, and where is it written down".

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::config::{AhmaSettings, ScopeAccess, ahma_home_dir};

// ─────────────────────────────────────────────────────────────────────────────
// The record shape (R-PERM.2)
// ─────────────────────────────────────────────────────────────────────────────

/// What a permission grant is *about*. Every kind normalizes into the same
/// [`GrantRecord`], which is what lets one CLI, one preview, and one audit
/// schema serve all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GrantKind {
    /// A directory added to the kernel sandbox scope (`[sandbox].persistent_scopes`).
    FsScope,
    /// An outbound web domain the egress policy permits (`[web]`).
    WebDomain,
    /// A tool the agent may run in a workspace without re-asking (`[permissions]`).
    Tool,
    /// Consent for terminal hooks to run a command unsandboxed (session-scoped by
    /// R5.5.3 — recorded here for listing, never persisted to disk).
    HookUnsandboxed,
}

impl GrantKind {
    /// Stable lowercase label used in the CLI, the audit log, and prompts.
    pub fn label(self) -> &'static str {
        match self {
            GrantKind::FsScope => "fs-scope",
            GrantKind::WebDomain => "web-domain",
            GrantKind::Tool => "tool",
            GrantKind::HookUnsandboxed => "hook-unsandboxed",
        }
    }
}

/// How long a grant lives.
///
/// The tiers are not three shades of the same thing — they differ in *where the
/// answer is written*, which is the whole security story:
///
/// * [`Once`](GrantTier::Once) — applies to the single pending operation.
///   **Never stored**, anywhere.
/// * [`Session`](GrantTier::Session) — held in memory by [`SessionGrants`] and
///   lost when the instance exits. Never touches disk, so it cannot silently
///   re-downgrade a future session.
/// * [`Always`](GrantTier::Always) — written to `~/.ahma/settings.toml`, and
///   **only** after the user has seen the exact file and the exact line
///   (R-PERM.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GrantTier {
    /// This operation only. Never stored.
    Once,
    /// The life of this server instance. In memory only.
    Session,
    /// Persisted to the settings file until revoked.
    Always,
}

impl GrantTier {
    /// Stable lowercase label used in the CLI, the audit log, and prompts.
    pub fn label(self) -> &'static str {
        match self {
            GrantTier::Once => "once",
            GrantTier::Session => "session",
            GrantTier::Always => "always",
        }
    }

    /// Whether a grant at this tier is written to the settings file.
    pub fn is_persistent(self) -> bool {
        matches!(self, GrantTier::Always)
    }
}

/// One normalized row of the ledger — what `ahma permissions list` renders and
/// what the audit log records.
///
/// This is a *view* type: the authoritative storage for each kind stays in its
/// established settings table (scopes in `[sandbox]`, domains in `[web]`, tools
/// in `[permissions]`). [`records`] projects all of them into this shape so the
/// user sees one list instead of three.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRecord {
    /// What kind of permission this is.
    pub kind: GrantKind,
    /// The thing granted: a path, a domain pattern, a tool name.
    pub subject: String,
    /// Access level, where the kind has one (`ro` / `rw` for scopes). `None` for
    /// kinds where access is not a dimension (a domain is allowed or it is not).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    /// How long the grant lives.
    pub tier: GrantTier,
    /// Who or what asked for it (a tool name, `user`, `builtin-profile(rust)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_by: Option<String>,
    /// When it was granted (`YYYY-MM-DD`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<String>,
    /// Which surface the answer came from (`cli`, `tui`, `harness:cursor`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    /// Free-form human note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Extra context for kinds that need it — e.g. the workspace a tool approval
    /// is scoped to. Rendered as a qualifier, never as part of the subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_note: Option<String>,
}

/// Project every persisted permission in `settings` into one list (R-PERM.2.1).
///
/// Order is stable and grouped by kind so the output of `ahma permissions list`
/// does not shuffle between runs.
pub fn records(settings: &AhmaSettings) -> Vec<GrantRecord> {
    let mut out = Vec::new();

    for s in &settings.sandbox.persistent_scopes {
        out.push(GrantRecord {
            kind: GrantKind::FsScope,
            subject: s.path.display().to_string(),
            access: Some(
                match s.access {
                    ScopeAccess::Ro => "ro",
                    ScopeAccess::Rw => "rw",
                }
                .to_string(),
            ),
            tier: GrantTier::Always,
            granted_by: s.granted_by.clone(),
            granted_at: s.granted_at.clone(),
            surface: None,
            note: s.note.clone(),
            scope_note: None,
        });
    }

    for pattern in &settings.web.always_allow {
        out.push(GrantRecord {
            kind: GrantKind::WebDomain,
            subject: pattern.clone(),
            access: Some("allow".to_string()),
            tier: GrantTier::Always,
            granted_by: None,
            granted_at: None,
            surface: None,
            note: None,
            scope_note: None,
        });
    }
    for pattern in &settings.web.never_allow {
        out.push(GrantRecord {
            kind: GrantKind::WebDomain,
            subject: pattern.clone(),
            access: Some("deny".to_string()),
            tier: GrantTier::Always,
            granted_by: None,
            granted_at: None,
            surface: None,
            note: None,
            scope_note: None,
        });
    }

    for approval in &settings.permissions.tool_approvals {
        for tool in &approval.tools {
            out.push(GrantRecord {
                kind: GrantKind::Tool,
                subject: tool.clone(),
                access: None,
                tier: GrantTier::Always,
                granted_by: approval.granted_by.clone(),
                granted_at: approval.granted_at.clone(),
                surface: approval.surface.clone(),
                note: None,
                scope_note: Some(approval.workspace.display().to_string()),
            });
        }
    }

    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Persisted tool approvals — the `[permissions]` table
// ─────────────────────────────────────────────────────────────────────────────

/// Per-workspace "always allow this tool" grants (SPEC R-PERM.1.1).
///
/// Keyed by **workspace root**, deliberately: trusting `cargo_build` in one
/// project must not silently trust it in another. The key is the canonicalized
/// workspace path (see [`workspace_key`]), so a symlinked or non-normalized
/// spelling of the same directory still matches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolApproval {
    /// Canonical workspace root these grants apply to.
    pub workspace: PathBuf,
    /// Tool names approved in that workspace. Kept sorted for a stable file.
    #[serde(default)]
    pub tools: Vec<String>,
    /// When the most recent tool here was approved (`YYYY-MM-DD`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<String>,
    /// Who approved (`user`), retained for provenance in `permissions list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_by: Option<String>,
    /// The surface the approval was given at (`tui`, `cli`, `harness:<client>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
}

/// The `[permissions]` table of `settings.toml` — the ledger's own section.
///
/// Filesystem scopes and web domains keep their established tables; this holds
/// what previously had no home in the settings file at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionSettings {
    /// Per-workspace tool approvals ("always allow", migrated from the retired
    /// `~/.config/ahma/approvals.json`).
    /// Default: empty list
    pub tool_approvals: Vec<ToolApproval>,
}

impl PermissionSettings {
    /// Whether `tool` is approved in the workspace identified by `key`.
    pub fn is_tool_approved(&self, key: &Path, tool: &str) -> bool {
        self.tool_approvals
            .iter()
            .any(|a| a.workspace == key && a.tools.iter().any(|t| t == tool))
    }

    /// Whether the workspace `key` has any approvals recorded at all — the
    /// difference between "a fresh sandbox" and "a known workspace that just
    /// hasn't seen this tool".
    pub fn workspace_known(&self, key: &Path) -> bool {
        self.tool_approvals.iter().any(|a| a.workspace == key)
    }

    /// Whether `tool` is approved under some *other* workspace. Drives the
    /// "new workspace — re-confirm to allow it here too" note.
    pub fn tool_approved_elsewhere(&self, key: &Path, tool: &str) -> bool {
        self.tool_approvals
            .iter()
            .any(|a| a.workspace != key && a.tools.iter().any(|t| t == tool))
    }

    /// Approve `tool` in the workspace `key`. Idempotent: re-approving an
    /// existing grant refreshes its provenance without duplicating it. Returns
    /// `true` when this added a tool that was not already approved.
    pub fn approve_tool(
        &mut self,
        key: &Path,
        tool: &str,
        granted_at: Option<String>,
        surface: Option<String>,
    ) -> bool {
        let entry = match self
            .tool_approvals
            .iter_mut()
            .position(|a| a.workspace == key)
        {
            Some(i) => &mut self.tool_approvals[i],
            None => {
                self.tool_approvals.push(ToolApproval {
                    workspace: key.to_path_buf(),
                    tools: Vec::new(),
                    granted_at: None,
                    granted_by: Some("user".to_string()),
                    surface: None,
                });
                self.tool_approvals
                    .last_mut()
                    .expect("just pushed an entry")
            }
        };
        entry.granted_at = granted_at.or_else(|| entry.granted_at.clone());
        if surface.is_some() {
            entry.surface = surface;
        }
        if entry.tools.iter().any(|t| t == tool) {
            return false;
        }
        entry.tools.push(tool.to_string());
        entry.tools.sort();
        true
    }

    /// Revoke `tool` in the workspace `key`, dropping the workspace entry when
    /// its last tool goes. Returns `true` when something was removed.
    pub fn revoke_tool(&mut self, key: &Path, tool: &str) -> bool {
        let Some(i) = self.tool_approvals.iter().position(|a| a.workspace == key) else {
            return false;
        };
        let entry = &mut self.tool_approvals[i];
        let Some(t) = entry.tools.iter().position(|t| t == tool) else {
            return false;
        };
        entry.tools.remove(t);
        if entry.tools.is_empty() {
            self.tool_approvals.remove(i);
        }
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Workspace keys
// ─────────────────────────────────────────────────────────────────────────────

/// Normalize a workspace path into its ledger key.
///
/// Canonicalizes when possible so a symlinked or `..`-laden spelling of the same
/// directory maps to one entry; falls back to the path as given when the
/// directory does not exist (a key that cannot be canonicalized is still a
/// stable key — it just isn't symlink-proof, which is acceptable because a
/// *missing* workspace grants nothing).
pub fn workspace_key(workspace: &Path) -> PathBuf {
    dunce::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
}

/// Async counterpart of [`workspace_key`] for use inside an agent turn, where
/// blocking I/O on the runtime is prohibited (AGENTS.md).
pub async fn workspace_key_async(workspace: &Path) -> PathBuf {
    let canonical = tokio::fs::canonicalize(workspace).await;
    // `tokio::fs::canonicalize` is `std::fs::canonicalize`, which on Windows
    // yields the `\\?\` extended-length form that `dunce` exists to avoid — so
    // strip it back to the plain form to match the sync key exactly.
    match canonical {
        Ok(p) => dunce::simplified(&p).to_path_buf(),
        Err(_) => workspace.to_path_buf(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Session-tier grants (R-PERM.2, R-PERM.4)
// ─────────────────────────────────────────────────────────────────────────────

/// The key a session answer is remembered under: kind + subject + access.
type SessionKey = (GrantKind, String, Option<String>);

/// In-memory store of `session`-tier answers.
///
/// Two properties matter here, and both come straight from R-PERM.4:
///
/// * A session answer is remembered in **both** directions. A "no" is an answer,
///   and re-asking a question the user already declined is exactly the hassle the
///   permissions model exists to prevent.
/// * Nothing here ever reaches disk. A session grant that outlived its session
///   would be a silent re-downgrade of the next one.
#[derive(Debug, Default)]
pub struct SessionGrants {
    inner: Mutex<HashMap<SessionKey, bool>>,
}

impl SessionGrants {
    /// A fresh, empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the user's session-tier answer. `allowed = false` records a denial,
    /// which suppresses re-asking just as firmly as an approval does.
    pub fn record(&self, kind: GrantKind, subject: &str, access: Option<&str>, allowed: bool) {
        self.inner.lock().insert(
            (kind, subject.to_string(), access.map(str::to_string)),
            allowed,
        );
    }

    /// The recorded answer for this subject, or `None` if it has not been asked
    /// this session. `Some(true)` = allowed, `Some(false)` = denied — both mean
    /// "do not ask again".
    pub fn lookup(&self, kind: GrantKind, subject: &str, access: Option<&str>) -> Option<bool> {
        self.inner
            .lock()
            .get(&(kind, subject.to_string(), access.map(str::to_string)))
            .copied()
    }

    /// Whether this subject has been answered this session, either way.
    pub fn is_answered(&self, kind: GrantKind, subject: &str, access: Option<&str>) -> bool {
        self.lookup(kind, subject, access).is_some()
    }

    /// Forget a recorded answer — used only by an explicit human re-raise
    /// (R-PERM.7.1), which is a fresh action rather than an unsolicited re-prompt.
    pub fn forget(&self, kind: GrantKind, subject: &str, access: Option<&str>) {
        self.inner
            .lock()
            .remove(&(kind, subject.to_string(), access.map(str::to_string)));
    }

    /// Every session-tier answer recorded so far, as ledger rows, so that
    /// `permissions list` can show in-memory grants alongside persisted ones.
    pub fn records(&self) -> Vec<GrantRecord> {
        let inner = self.inner.lock();
        let mut rows: Vec<GrantRecord> = inner
            .iter()
            .filter(|(_, allowed)| **allowed)
            .map(|((kind, subject, access), _)| GrantRecord {
                kind: *kind,
                subject: subject.clone(),
                access: access.clone(),
                tier: GrantTier::Session,
                granted_by: Some("user".to_string()),
                granted_at: None,
                surface: None,
                note: None,
                scope_note: None,
            })
            .collect();
        rows.sort_by(|a, b| (a.kind, &a.subject).cmp(&(b.kind, &b.subject)));
        rows
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Migration off the retired ~/.config/ahma tree (R-PERM.1)
// ─────────────────────────────────────────────────────────────────────────────

/// The retired approvals file (`~/.config/ahma/approvals.json` on Linux,
/// `~/Library/Application Support/ahma/approvals.json` on macOS).
///
/// `AHMA_CONFIG_DIR` is still honored *for reading* so an installation that
/// relocated its config tree still gets migrated. It is not a supported location
/// to write to any more.
///
/// # Why a test build refuses the real config dir
///
/// The migration **moves** a file the user owns. If a test redirects the home
/// directory (`AHMA_TEST_HOME`) but says nothing about the legacy tree, the naive
/// fallback would read the *real* user's `approvals.json` and archive it into a
/// throwaway temp ledger — quietly relocating live data for a test's benefit.
/// That is not a hypothetical: it happened. So in debug builds a redirected home
/// without a redirected config dir yields `None` (nothing to migrate), and a test
/// that wants to exercise migration must opt in by setting `AHMA_CONFIG_DIR` too.
pub fn legacy_approvals_path() -> Option<PathBuf> {
    let home_redirected = cfg!(debug_assertions) && std::env::var_os("AHMA_TEST_HOME").is_some();
    resolve_legacy_path(
        std::env::var_os("AHMA_CONFIG_DIR").map(PathBuf::from),
        home_redirected,
        dirs::config_dir(),
    )
}

/// The decision behind [`legacy_approvals_path`], as a pure function so the
/// safety rule can be tested without touching process-global environment.
fn resolve_legacy_path(
    config_dir_override: Option<PathBuf>,
    home_redirected: bool,
    platform_config_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(dir) = config_dir_override {
        return Some(dir.join("ahma").join("approvals.json"));
    }
    if home_redirected {
        // A redirected home means a test is running. Never let it reach into the
        // real user's config tree and move files out of it.
        return None;
    }
    platform_config_dir.map(|d| d.join("ahma").join("approvals.json"))
}

/// What [`migrate_legacy_approvals`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// No legacy file existed — nothing to do (the overwhelmingly common case).
    NothingToDo,
    /// Grants were folded into the ledger; the legacy file was renamed aside.
    Migrated {
        /// How many `(workspace, tool)` grants moved across.
        grants: usize,
        /// Where the legacy file was moved to.
        archived_to: PathBuf,
    },
}

/// Fold `~/.config/ahma/approvals.json` into the ledger, once, non-destructively.
///
/// Non-destructive on purpose: the legacy file is **renamed**, not deleted. It is
/// the user's data, it records what they chose to trust, and a migration bug that
/// eats it is not recoverable. A `.migrated` sibling costs nothing and means a
/// mistake here is always reversible by hand.
///
/// Idempotent: once the legacy file is renamed the next call sees no legacy file
/// and reports [`MigrationOutcome::NothingToDo`], so this is safe to call on
/// every startup.
pub fn migrate_legacy_approvals(settings_file: &Path) -> anyhow::Result<MigrationOutcome> {
    let Some(legacy) = legacy_approvals_path() else {
        return Ok(MigrationOutcome::NothingToDo);
    };
    let contents = match std::fs::read_to_string(&legacy) {
        Ok(c) => c,
        // Missing is the normal case. Unreadable (`PermissionDenied` when ahma
        // runs inside its own sandbox) is *also* not an error: there is nothing
        // we can safely do about it here, and failing startup over it would
        // reproduce the historical R5.4.8 wedge.
        Err(_) => return Ok(MigrationOutcome::NothingToDo),
    };

    // A file we cannot parse is **not** an empty file. Treating a corrupt
    // approvals.json as "zero grants" and then archiving it would move the user's
    // record of what they trusted out of the path ahma reads, having migrated
    // nothing — the grants would be silently orphaned, and the user would only
    // find out by being re-prompted for everything. Leave it exactly where it is
    // and say so; a human can fix or delete it.
    let legacy_grants: BTreeMap<String, Vec<String>> = match serde_json::from_str(&contents) {
        Ok(g) => g,
        Err(e) => {
            warn!(
                "permissions: {} exists but does not parse ({e}); leaving it untouched. \
                 Fix or delete it, then re-run ahma to migrate its grants.",
                legacy.display()
            );
            return Ok(MigrationOutcome::NothingToDo);
        }
    };

    let mut settings = AhmaSettings::load_from_result(settings_file)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .map_err(|e| e.context("refusing to migrate into an unparseable settings file"))?;

    let mut moved = 0usize;
    for (workspace, tools) in &legacy_grants {
        let key = workspace_key(Path::new(workspace));
        for tool in tools {
            if settings
                .permissions
                .approve_tool(&key, tool, None, Some("migrated".to_string()))
            {
                moved += 1;
            }
        }
    }
    for entry in &mut settings.permissions.tool_approvals {
        if entry.granted_by.as_deref() == Some("user")
            && entry.surface.as_deref() == Some("migrated")
        {
            entry.granted_by = Some("migrated".to_string());
        }
    }

    if moved > 0 {
        settings.save_to(settings_file)?;

        // Read the grants back before archiving the only other copy of them.
        //
        // The write can fail to stick for reasons this function cannot see — a
        // concurrent ahma process re-rendering the same settings file, a full
        // disk, a permission quirk. Archiving on the *assumption* that it worked
        // is how a user ends up with an empty ledger and their approvals.json
        // renamed out from under them. So: verify, then archive. If verification
        // fails, the legacy file stays exactly where it is and the next run tries
        // again.
        let reloaded = AhmaSettings::load_from_result(settings_file)
            .map_err(|e| anyhow::anyhow!("could not re-read the ledger after migrating: {e}"))?;
        let landed: usize = reloaded
            .permissions
            .tool_approvals
            .iter()
            .map(|a| a.tools.len())
            .sum();
        if landed == 0 {
            anyhow::bail!(
                "migrated {moved} grant(s) into {} but they are not there on re-read \
                 (another ahma process may have rewritten the file). Leaving {} in place \
                 so nothing is lost; it will be retried on the next run.",
                settings_file.display(),
                legacy.display()
            );
        }
    }

    let archived_to = legacy.with_extension("json.migrated");
    std::fs::rename(&legacy, &archived_to).map_err(|e| {
        anyhow::anyhow!(
            "migrated {moved} grant(s) into {} but could not archive {}: {e}",
            settings_file.display(),
            legacy.display()
        )
    })?;

    debug!(
        "permissions: migrated {moved} tool approval(s) from {} into {}",
        legacy.display(),
        settings_file.display()
    );
    Ok(MigrationOutcome::Migrated {
        grants: moved,
        archived_to,
    })
}

/// Run [`migrate_legacy_approvals`] against the real settings file, logging
/// rather than propagating failures.
///
/// Called from process startup. A migration failure must never block ahma from
/// running (SPEC R-PERM.2.2): the worst case is that the user re-approves a tool
/// once.
pub fn migrate_legacy_approvals_best_effort() {
    let Some(settings_file) = crate::config::settings_path() else {
        return;
    };
    match migrate_legacy_approvals(&settings_file) {
        Ok(MigrationOutcome::NothingToDo) => {}
        Ok(MigrationOutcome::Migrated {
            grants,
            archived_to,
        }) => {
            tracing::info!(
                "Migrated {grants} tool approval(s) into {} (the retired approvals.json is kept at {}). \
                 Review them with `ahma permissions list`.",
                settings_file.display(),
                archived_to.display()
            );
        }
        Err(e) => warn!("permissions: could not migrate legacy approvals: {e:#}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Audit trail (R-PERM.2.1)
// ─────────────────────────────────────────────────────────────────────────────

/// What happened to a permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditAction {
    /// A permission was granted.
    Grant,
    /// A permission was revoked.
    Revoke,
    /// A permission request was denied by the user.
    Deny,
}

/// One line of `~/.ahma/permissions-audit.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// When it happened (RFC 3339).
    pub at: String,
    /// Grant, revoke, or deny.
    pub action: AuditAction,
    /// Which kind of permission.
    pub kind: GrantKind,
    /// The path / domain / tool.
    pub subject: String,
    /// Access level, where applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    /// Tier of the grant.
    pub tier: GrantTier,
    /// The surface the answer came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
}

/// The audit log path, `~/.ahma/permissions-audit.jsonl`.
pub fn audit_path() -> Option<PathBuf> {
    ahma_home_dir().map(|h| h.join(".ahma").join("permissions-audit.jsonl"))
}

/// Append one entry to the audit log at `path`, creating it if needed.
///
/// Append-only and one JSON object per line, so a corrupt or truncated tail
/// costs at most the last record and the file can be tailed while ahma runs.
pub fn append_audit_at(path: &Path, entry: &AuditEntry) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())
}

/// Append to the real audit log, swallowing (but logging) failures.
///
/// An audit write that fails must not fail the grant it is recording (SPEC
/// R-PERM.2.2): the grant itself was already confirmed by a human and is safely
/// in the settings file.
/// Losing the *record* of it is a lesser harm than losing the *grant*.
pub fn append_audit(entry: &AuditEntry) {
    let Some(path) = audit_path() else { return };
    if let Err(e) = append_audit_at(&path, entry) {
        debug!("permissions: could not append to audit log: {e}");
    }
}

/// Build an [`AuditEntry`].
///
/// `at` is supplied by the caller (stamp it with `chrono::Local::now()`), which
/// keeps this crate free of a date dependency — the same convention
/// [`crate::scope_grant::persist_grant`] follows for `granted_at`.
pub fn audit_entry(
    at: impl Into<String>,
    action: AuditAction,
    kind: GrantKind,
    subject: impl Into<String>,
    access: Option<String>,
    tier: GrantTier,
    surface: Option<String>,
) -> AuditEntry {
    AuditEntry {
        at: at.into(),
        action,
        kind,
        subject: subject.into(),
        access,
        tier,
        surface,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn key(p: &str) -> PathBuf {
        PathBuf::from(p)
    }

    #[test]
    fn approve_tool_is_idempotent_and_sorted() {
        let mut p = PermissionSettings::default();
        assert!(p.approve_tool(&key("/ws"), "list_dir", None, None));
        assert!(p.approve_tool(&key("/ws"), "cargo_build", None, None));
        // Re-approving an existing grant is a no-op, not a duplicate.
        assert!(!p.approve_tool(&key("/ws"), "list_dir", None, None));

        assert_eq!(p.tool_approvals.len(), 1);
        assert_eq!(p.tool_approvals[0].tools, vec!["cargo_build", "list_dir"]);
    }

    #[test]
    fn approvals_are_workspace_scoped() {
        let mut p = PermissionSettings::default();
        p.approve_tool(&key("/a"), "list_dir", None, None);

        assert!(p.is_tool_approved(&key("/a"), "list_dir"));
        // Trusting a tool in one workspace must never trust it in another.
        assert!(!p.is_tool_approved(&key("/b"), "list_dir"));
        assert!(p.tool_approved_elsewhere(&key("/b"), "list_dir"));
        assert!(!p.workspace_known(&key("/b")));
    }

    #[test]
    fn revoke_drops_workspace_entry_when_last_tool_goes() {
        let mut p = PermissionSettings::default();
        p.approve_tool(&key("/ws"), "list_dir", None, None);
        p.approve_tool(&key("/ws"), "cargo_build", None, None);

        assert!(p.revoke_tool(&key("/ws"), "list_dir"));
        assert_eq!(p.tool_approvals.len(), 1, "workspace still has cargo_build");

        assert!(p.revoke_tool(&key("/ws"), "cargo_build"));
        assert!(
            p.tool_approvals.is_empty(),
            "an empty workspace entry is removed, not left as a husk"
        );
        assert!(!p.revoke_tool(&key("/ws"), "cargo_build"));
    }

    #[test]
    fn session_grants_remember_denials_too() {
        let s = SessionGrants::new();
        assert!(!s.is_answered(GrantKind::FsScope, "/cache", Some("rw")));

        s.record(GrantKind::FsScope, "/cache", Some("rw"), false);
        // A "no" is an answer: it must suppress re-asking exactly as firmly as a
        // "yes" does, or the user gets nagged for declining.
        assert_eq!(
            s.lookup(GrantKind::FsScope, "/cache", Some("rw")),
            Some(false)
        );
        assert!(s.is_answered(GrantKind::FsScope, "/cache", Some("rw")));
        // A denial is not a grant, so it does not show up as one.
        assert!(s.records().is_empty());
    }

    #[test]
    fn session_grants_are_keyed_by_kind_subject_and_access() {
        let s = SessionGrants::new();
        s.record(GrantKind::FsScope, "/cache", Some("ro"), true);

        assert_eq!(
            s.lookup(GrantKind::FsScope, "/cache", Some("ro")),
            Some(true)
        );
        // A different access level is a different question.
        assert_eq!(s.lookup(GrantKind::FsScope, "/cache", Some("rw")), None);
        // As is a different kind with the same subject.
        assert_eq!(s.lookup(GrantKind::Tool, "/cache", Some("ro")), None);
    }

    #[test]
    fn session_forget_allows_an_explicit_reraise() {
        let s = SessionGrants::new();
        s.record(GrantKind::FsScope, "/cache", Some("rw"), false);
        s.forget(GrantKind::FsScope, "/cache", Some("rw"));
        assert!(
            !s.is_answered(GrantKind::FsScope, "/cache", Some("rw")),
            "an explicit human re-raise must be able to ask again"
        );
    }

    #[test]
    fn a_redirected_home_never_reaches_the_real_config_dir() {
        let real_config = Some(PathBuf::from("/real/config"));

        // Production: no overrides — the platform config dir is used.
        assert_eq!(
            resolve_legacy_path(None, false, real_config.clone()),
            Some(PathBuf::from("/real/config/ahma/approvals.json"))
        );

        // A test that redirects HOME but not the config dir must find *nothing*.
        // The migration moves a file the user owns; letting a test reach the real
        // config tree would relocate live data into a throwaway temp ledger.
        assert_eq!(
            resolve_legacy_path(None, true, real_config.clone()),
            None,
            "a redirected home must never migrate the real user's approvals.json"
        );

        // A test that redirects both is explicitly opting in, and gets its own tree.
        assert_eq!(
            resolve_legacy_path(Some(PathBuf::from("/tmp/x")), true, real_config),
            Some(PathBuf::from("/tmp/x/ahma/approvals.json"))
        );
    }

    #[test]
    fn migration_moves_grants_and_archives_the_legacy_file() {
        let home = tempdir().unwrap();
        let cfg = tempdir().unwrap();
        let settings_file = home.path().join(".ahma").join("settings.toml");

        let legacy_dir = cfg.path().join("ahma");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy = legacy_dir.join("approvals.json");
        let ws = home.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            &legacy,
            serde_json::json!({ ws.to_string_lossy(): ["cargo_build", "list_dir"] }).to_string(),
        )
        .unwrap();

        // SAFETY: single-threaded test; the var is read only by this call.
        unsafe { std::env::set_var("AHMA_CONFIG_DIR", cfg.path()) };
        let outcome = migrate_legacy_approvals(&settings_file).unwrap();
        unsafe { std::env::remove_var("AHMA_CONFIG_DIR") };

        match outcome {
            MigrationOutcome::Migrated {
                grants,
                archived_to,
            } => {
                assert_eq!(grants, 2);
                assert!(
                    archived_to.exists(),
                    "legacy file is archived, never deleted"
                );
                assert!(!legacy.exists(), "legacy file no longer at the old path");
            }
            other => panic!("expected Migrated, got {other:?}"),
        }

        let settings = AhmaSettings::load_from_result(&settings_file).unwrap();
        let canonical = workspace_key(&ws);
        assert!(
            settings
                .permissions
                .is_tool_approved(&canonical, "cargo_build")
        );
        assert!(
            settings
                .permissions
                .is_tool_approved(&canonical, "list_dir")
        );
    }

    #[test]
    fn a_corrupt_legacy_file_is_left_alone_not_archived() {
        // A file we cannot parse is not an empty file. Archiving it after migrating
        // *nothing* would move the user's only record of what they trusted out of
        // the path ahma reads — they would discover it by being re-prompted for
        // everything, with no obvious way back.
        let home = tempdir().unwrap();
        let cfg = tempdir().unwrap();
        let settings_file = home.path().join(".ahma").join("settings.toml");

        let legacy_dir = cfg.path().join("ahma");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy = legacy_dir.join("approvals.json");
        std::fs::write(&legacy, "{ this is not json").unwrap();

        // SAFETY: single-threaded test.
        unsafe { std::env::set_var("AHMA_CONFIG_DIR", cfg.path()) };
        let outcome = migrate_legacy_approvals(&settings_file).unwrap();
        unsafe { std::env::remove_var("AHMA_CONFIG_DIR") };

        assert_eq!(outcome, MigrationOutcome::NothingToDo);
        assert!(
            legacy.exists(),
            "a file we could not read must be left exactly where it is"
        );
        assert_eq!(
            std::fs::read_to_string(&legacy).unwrap(),
            "{ this is not json",
            "…and untouched, so a human can fix it"
        );
    }

    #[test]
    fn migration_with_no_legacy_file_is_a_no_op() {
        let home = tempdir().unwrap();
        let cfg = tempdir().unwrap();
        let settings_file = home.path().join(".ahma").join("settings.toml");

        // SAFETY: single-threaded test.
        unsafe { std::env::set_var("AHMA_CONFIG_DIR", cfg.path()) };
        let outcome = migrate_legacy_approvals(&settings_file).unwrap();
        unsafe { std::env::remove_var("AHMA_CONFIG_DIR") };

        assert_eq!(outcome, MigrationOutcome::NothingToDo);
        assert!(
            !settings_file.exists(),
            "a no-op migration must not create a settings file"
        );
    }

    #[test]
    fn audit_log_appends_one_json_object_per_line() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("permissions-audit.jsonl");

        append_audit_at(
            &log,
            &audit_entry(
                "2026-07-12T10:00:00+02:00",
                AuditAction::Grant,
                GrantKind::FsScope,
                "/cache",
                Some("rw".into()),
                GrantTier::Always,
                Some("cli".into()),
            ),
        )
        .unwrap();
        append_audit_at(
            &log,
            &audit_entry(
                "2026-07-12T10:00:01+02:00",
                AuditAction::Revoke,
                GrantKind::Tool,
                "cargo_build",
                None,
                GrantTier::Always,
                Some("tui".into()),
            ),
        )
        .unwrap();

        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "appended, not overwritten");
        for line in lines {
            let parsed: AuditEntry = serde_json::from_str(line).expect("each line parses alone");
            assert!(!parsed.at.is_empty());
        }
    }

    #[test]
    fn records_folds_every_kind_into_one_list() {
        use crate::config::PersistentScope;
        let mut s = AhmaSettings::default();
        s.sandbox.persistent_scopes.push(PersistentScope {
            path: PathBuf::from("/cache"),
            access: ScopeAccess::Rw,
            granted_by: Some("sccache".into()),
            granted_at: Some("2026-07-12".into()),
            note: None,
        });
        s.web.always_allow.push("api.github.com".into());
        s.permissions
            .approve_tool(&key("/ws"), "cargo_build", None, None);

        let rows = records(&s);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].kind, GrantKind::FsScope);
        assert_eq!(rows[0].access.as_deref(), Some("rw"));
        assert_eq!(rows[1].kind, GrantKind::WebDomain);
        assert_eq!(rows[2].kind, GrantKind::Tool);
        // A tool approval is qualified by its workspace, never conflated with it.
        assert_eq!(rows[2].subject, "cargo_build");
        assert_eq!(rows[2].scope_note.as_deref(), Some("/ws"));
    }
}
