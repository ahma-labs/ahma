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
//!   `[web]`).
//! * [`LogTargetSettings`] — the `[log_targets]` table: files outside the
//!   workspace that a `.ahma/logs/*.log` symlink may point at, approved with the
//!   `logs_approve` tool. [`records`] folds every table into one view.
//! * [`migrate_legacy_approvals`] and [`migrate_legacy_log_exceptions`] — the
//!   one-time, non-destructive moves of `~/.config/ahma/approvals.json` and
//!   `~/.config/ahma/log_exceptions.json` into the ledger.
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
    /// An outbound host or domain permitted by the subprocess network egress policy (`[network].allow`).
    NetHost,
    /// A tool the agent may run in a workspace without re-asking (`[permissions]`).
    Tool,
    /// A file outside the workspace that a `.ahma/logs/*.log` symlink may point
    /// at, readable (never writable) by live-log monitoring (`[log_targets]`).
    LogTarget,
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
            GrantKind::NetHost => "net-host",
            GrantKind::Tool => "tool",
            GrantKind::LogTarget => "log-target",
            GrantKind::HookUnsandboxed => "hook-unsandboxed",
        }
    }

    /// Every kind, in the order `ahma permissions list` groups them.
    pub const ALL: [GrantKind; 6] = [
        GrantKind::FsScope,
        GrantKind::WebDomain,
        GrantKind::NetHost,
        GrantKind::Tool,
        GrantKind::LogTarget,
        GrantKind::HookUnsandboxed,
    ];

    /// Whether grants of this kind are written to the settings file, and so can
    /// appear in [`records`]. An exhaustive match on purpose: a new kind cannot
    /// be added without deciding this, which is what keeps a persisted kind from
    /// silently missing from `ahma permissions list` (as `net-host` once did).
    pub const fn is_persisted(self) -> bool {
        match self {
            GrantKind::FsScope
            | GrantKind::WebDomain
            | GrantKind::NetHost
            | GrantKind::Tool
            | GrantKind::LogTarget => true,
            GrantKind::HookUnsandboxed => false,
        }
    }

    /// The kinds [`records`] can return, in listing order.
    pub fn persisted() -> impl Iterator<Item = GrantKind> {
        Self::ALL.into_iter().filter(|k| k.is_persisted())
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
    /// Persisted with an end: stops applying after its lease (SPEC R-PERM.2.3).
    Lease,
}

impl GrantTier {
    /// Stable lowercase label used in the CLI, the audit log, and prompts.
    pub fn label(self) -> &'static str {
        match self {
            GrantTier::Once => "once",
            GrantTier::Session => "session",
            GrantTier::Always => "always",
            GrantTier::Lease => "lease",
        }
    }

    /// Whether a grant at this tier is written to the settings file.
    pub fn is_persistent(self) -> bool {
        matches!(self, GrantTier::Always | GrantTier::Lease)
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
    /// When a leased grant stops applying, in Unix seconds (SPEC R-PERM.2.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
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
            scope_note: Some(match &s.workspace {
                Some(ws) => format!("workspace {}", ws.display()),
                None => "GLOBAL (legacy): applies to every workspace; re-grant with \
                         `ahma sandbox grant <path>` from the project that needs it"
                    .to_string(),
            }),
            expires_at: s.expires_at,
        });
    }

    for (patterns, access) in [
        (&settings.web.always_allow, "allow"),
        (&settings.web.never_allow, "deny"),
    ] {
        for pattern in patterns {
            out.push(GrantRecord {
                kind: GrantKind::WebDomain,
                subject: pattern.clone(),
                access: Some(access.to_string()),
                tier: GrantTier::Always,
                granted_by: None,
                granted_at: None,
                surface: None,
                note: None,
                scope_note: None,
                expires_at: None,
            });
        }
    }

    for host in &settings.network.allow {
        out.push(GrantRecord {
            kind: GrantKind::NetHost,
            subject: host.clone(),
            access: None,
            tier: GrantTier::Always,
            granted_by: None,
            granted_at: None,
            surface: None,
            note: None,
            scope_note: None,
            expires_at: None,
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
                expires_at: None,
            });
        }
    }

    for approval in &settings.log_targets.approvals {
        for target in &approval.targets {
            out.push(GrantRecord {
                kind: GrantKind::LogTarget,
                subject: target.display().to_string(),
                // Live-log monitoring only ever reads a target.
                access: Some("ro".to_string()),
                tier: GrantTier::Always,
                granted_by: approval.granted_by.clone(),
                granted_at: approval.granted_at.clone(),
                surface: approval.surface.clone(),
                note: None,
                scope_note: Some(approval.workspace.display().to_string()),
                expires_at: None,
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
    /// Show a model's one-line recommendation beside a grant prompt in the TUI
    /// (SPEC R-PERM.8). The model sees the evidence, never the agent's own
    /// words; it recommends and never answers. Default: `true`
    #[serde(default = "default_true")]
    pub advisor: bool,
    /// How long the TUI waits for the advisor before showing the prompt
    /// without it. Default: `6`
    #[serde(default = "default_advisor_timeout_secs")]
    pub advisor_timeout_secs: u64,
}

fn default_true() -> bool {
    true
}

fn default_advisor_timeout_secs() -> u64 {
    6
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

    /// Whether the workspace `key` is a **trusted folder** (SPEC R-PERM.1.3):
    /// the user answered the one-time "trust this folder?" question with yes.
    pub fn is_workspace_trusted(&self, key: &Path) -> bool {
        self.is_tool_approved(key, TRUSTED_WORKSPACE_TOOL)
    }

    /// Mark `key` as a trusted folder. Idempotent; returns `true` when this
    /// changed anything.
    pub fn trust_workspace(
        &mut self,
        key: &Path,
        granted_at: Option<String>,
        surface: Option<String>,
    ) -> bool {
        self.approve_tool(key, TRUSTED_WORKSPACE_TOOL, granted_at, surface)
    }
}

/// The tool-approval entry that records a **trusted folder** (SPEC R-PERM.1.3).
///
/// Trust is stored as a wildcard tool in the existing `tool_approvals` list
/// rather than a new `[permissions]` key on purpose: that table is
/// `deny_unknown_fields`, and a hub left running from an older release would
/// refuse to parse a settings file carrying a key it has never heard of —
/// taking every other grant down with it. An older binary reads `"*"` as a tool
/// literally named `*`, which never matches a real call, so it simply keeps
/// prompting: fail-closed. `ahma permissions revoke tool '*' --workspace <dir>`
/// withdraws the trust.
///
/// Trust never covers what crosses the sandbox boundary; that decision lives
/// with the caller (`ahma_core::approvals::covered_by_trust`), not here.
pub const TRUSTED_WORKSPACE_TOOL: &str = "*";

// ─────────────────────────────────────────────────────────────────────────────
// Approved log-symlink targets — the `[log_targets]` table
// ─────────────────────────────────────────────────────────────────────────────

/// Files outside a workspace that its `.ahma/logs/*.log` symlinks may point at
/// (approved with the `logs_approve` tool).
///
/// Keyed by workspace for the same reason tool approvals are: approving a log
/// target for one project must not make it readable from another.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogTargetApproval {
    /// Canonical workspace root (see [`workspace_key`]).
    pub workspace: PathBuf,
    /// Canonical paths of the approved targets. Kept sorted for a stable file.
    #[serde(default)]
    pub targets: Vec<PathBuf>,
    /// When the most recent target here was approved (`YYYY-MM-DD`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<String>,
    /// Who or what asked for it (`logs_approve`, `migrated`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_by: Option<String>,
    /// The surface the approval came from (`mcp:logs_approve`, `migrated`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
}

/// The `[log_targets]` table of `settings.toml`.
///
/// A table of its own rather than a key in `[permissions]`, deliberately:
/// `[permissions]` is `deny_unknown_fields`, so a new key there would make a hub
/// left running from an older release refuse the whole file when it re-reads it,
/// taking every other grant down with it. The top level of the file tolerates a
/// table an older build does not know, so an older build simply does not see
/// these approvals: fail-closed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogTargetSettings {
    /// Per-workspace approved targets (migrated from the retired
    /// `~/.config/ahma/log_exceptions.json`).
    /// Default: empty list
    pub approvals: Vec<LogTargetApproval>,
}

impl LogTargetSettings {
    /// Whether `target` is approved for the workspace identified by `key`.
    pub fn is_target_approved(&self, key: &Path, target: &Path) -> bool {
        self.approvals
            .iter()
            .any(|a| a.workspace == key && a.targets.iter().any(|t| t == target))
    }

    /// Every target approved for the workspace `key`, sorted.
    pub fn approved_targets(&self, key: &Path) -> Vec<PathBuf> {
        self.approvals
            .iter()
            .filter(|a| a.workspace == key)
            .flat_map(|a| a.targets.iter().cloned())
            .collect()
    }

    /// Approve `target` for the workspace `key`. Idempotent: re-approving
    /// refreshes provenance without duplicating the target. Returns `true` when
    /// this added a target that was not already approved.
    pub fn approve_target(
        &mut self,
        key: &Path,
        target: &Path,
        granted_at: Option<String>,
        granted_by: Option<String>,
        surface: Option<String>,
    ) -> bool {
        let entry = match self.approvals.iter().position(|a| a.workspace == key) {
            Some(i) => &mut self.approvals[i],
            None => {
                self.approvals.push(LogTargetApproval {
                    workspace: key.to_path_buf(),
                    ..LogTargetApproval::default()
                });
                self.approvals.last_mut().expect("just pushed an entry")
            }
        };
        entry.granted_at = granted_at.or_else(|| entry.granted_at.clone());
        if granted_by.is_some() {
            entry.granted_by = granted_by;
        }
        if surface.is_some() {
            entry.surface = surface;
        }
        if entry.targets.iter().any(|t| t == target) {
            return false;
        }
        entry.targets.push(target.to_path_buf());
        entry.targets.sort();
        true
    }

    /// Revoke `target` for the workspace `key`, dropping the workspace entry
    /// when its last target goes. Returns `true` when something was removed.
    pub fn revoke_target(&mut self, key: &Path, target: &Path) -> bool {
        let Some(i) = self.approvals.iter().position(|a| a.workspace == key) else {
            return false;
        };
        let entry = &mut self.approvals[i];
        let Some(t) = entry.targets.iter().position(|t| t == target) else {
            return false;
        };
        entry.targets.remove(t);
        if entry.targets.is_empty() {
            self.approvals.remove(i);
        }
        true
    }
}

/// Persist an approved log target for `workspace` into the ledger at
/// `settings_file`, and audit it.
///
/// Read-modify-write through [`AhmaSettings::update_at`]: a settings file that
/// does not parse is refused rather than overwritten, the write is atomic, and
/// grants another surface recorded since this process started are kept.
/// `target` should be canonical — the live-log check compares by equality.
/// `surface` names who asked (`mcp:logs_approve`); its part after `mcp:` is
/// recorded as `granted_by`. Returns `true` when the target was newly added; a
/// re-approval refreshes provenance and is not audited twice.
pub fn persist_log_target(
    settings_file: &Path,
    workspace: &Path,
    target: &Path,
    surface: &str,
) -> anyhow::Result<bool> {
    let granted_by = surface.strip_prefix("mcp:").unwrap_or(surface);
    persist_log_target_as(settings_file, workspace, target, granted_by, surface)
}

/// [`persist_log_target`] with the provenance spelled out: `granted_by` is the
/// tool or person that asked (`logs_approve`, `ahma tui`) and `surface` the
/// place a human answered (`harness`, `tui`). The one write path for a
/// `log-target` row: the human answer to `logs_approve`'s question (SPEC
/// R9.2) and the TUI's own `[a]` both come here.
///
/// The hard denylist (SPEC R-PERM.4.3) runs here too, so no caller can write a
/// credential file or a system directory as a log target by skipping its own
/// check: the agent can plant a `.ahma/logs` link to anything.
pub fn persist_log_target_as(
    settings_file: &Path,
    workspace: &Path,
    target: &Path,
    granted_by: &str,
    surface: &str,
) -> anyhow::Result<bool> {
    if let Some(why) = crate::scope_grant::refusal_reason(target) {
        anyhow::bail!(
            "refusing to record {} as a log target: {why}",
            target.display()
        );
    }
    let key = workspace_key(workspace);
    let stamp = crate::config::fmt_utc_datetime(crate::config::unix_now());
    // `YYYY-MM-DD`, the `granted_at` convention every other grant uses.
    let date = stamp.get(..10).map(str::to_string);
    let granted_by = granted_by.to_string();
    let mut added = false;
    AhmaSettings::update_at(settings_file, |s| {
        added = s.log_targets.approve_target(
            &key,
            target,
            date,
            Some(granted_by),
            Some(surface.to_string()),
        );
    })?;
    if added {
        append_audit(&audit_entry(
            stamp,
            AuditAction::Grant,
            GrantKind::LogTarget,
            target.display().to_string(),
            Some("ro".to_string()),
            GrantTier::Always,
            Some(surface.to_string()),
        ));
    }
    Ok(added)
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
                expires_at: None,
            })
            .collect();
        rows.sort_by(|a, b| (a.kind, &a.subject).cmp(&(b.kind, &b.subject)));
        rows
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Migration off the retired ~/.config/ahma tree (R-PERM.1)
// ─────────────────────────────────────────────────────────────────────────────

/// File name of the retired tool-approvals store.
const LEGACY_APPROVALS_FILE: &str = "approvals.json";
/// File name of the retired log-symlink exceptions store.
const LEGACY_LOG_EXCEPTIONS_FILE: &str = "log_exceptions.json";

/// The retired approvals file (`~/.config/ahma/approvals.json` on Linux,
/// `~/Library/Application Support/ahma/approvals.json` on macOS).
///
/// The location is no longer relocatable: `AHMA_CONFIG_DIR` is retired and
/// ignored (SPEC R-CFG1.2). See `legacy_config_file` for what a test build
/// does instead.
pub fn legacy_approvals_path() -> Option<PathBuf> {
    legacy_config_file(LEGACY_APPROVALS_FILE)
}

/// The retired log-symlink exceptions file (`~/.config/ahma/log_exceptions.json`
/// on Linux, `~/Library/Application Support/ahma/log_exceptions.json` on macOS).
pub fn legacy_log_exceptions_path() -> Option<PathBuf> {
    legacy_config_file(LEGACY_LOG_EXCEPTIONS_FILE)
}

/// `file_name` inside the retired `<platform config dir>/ahma` tree.
///
/// # Why a test build never reaches the real config dir
///
/// The migrations **move** a file the user owns. If a test redirects the home
/// directory (`AHMA_TEST_HOME`, or the per-run home every process under a test
/// harness gets) the naive answer would still be the *real* user's config dir,
/// and the test would archive their live `approvals.json` into a throwaway temp
/// ledger. That is not a hypothetical: it happened. So whenever the home is
/// redirected, the legacy tree is looked for *inside that home*, at
/// `<home>/.config/ahma/` — a test that wants to exercise a migration puts its
/// legacy file there, and every other test finds nothing to move.
fn legacy_config_file(file_name: &str) -> Option<PathBuf> {
    resolve_legacy_path(file_name, redirected_test_home(), dirs::config_dir())
}

/// The redirected home of a debug/test build, or `None` when `~` is the real
/// home (always the case in a release build).
fn redirected_test_home() -> Option<PathBuf> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let home = ahma_home_dir()?;
    (dirs::home_dir().as_deref() != Some(home.as_path())).then_some(home)
}

/// The decision behind [`legacy_config_file`], as a pure function so the safety
/// rule can be tested without touching process-global environment.
fn resolve_legacy_path(
    file_name: &str,
    redirected_home: Option<PathBuf>,
    platform_config_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    let config_dir = match redirected_home {
        // A redirected home means a test is running. Never let it reach into the
        // real user's config tree and move files out of it.
        Some(home) => home.join(".config"),
        None => platform_config_dir?,
    };
    Some(config_dir.join("ahma").join(file_name))
}

/// What a legacy-store migration did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// No legacy file existed — nothing to do (the overwhelmingly common case).
    NothingToDo,
    /// Grants were folded into the ledger; the legacy file was renamed aside.
    Migrated {
        /// How many grants moved across.
        grants: usize,
        /// Where the legacy file was moved to.
        archived_to: PathBuf,
    },
}

/// Read a retired `{ "<workspace>": ["<subject>", …] }` store.
///
/// `None` means "nothing to migrate" — and that covers more than a missing file:
///
/// * Unreadable (`PermissionDenied` when ahma runs inside its own sandbox) is
///   not an error: there is nothing we can safely do about it here, and failing
///   startup over it would reproduce the historical R5.4.8 wedge.
/// * A file we cannot parse is **not** an empty file. Treating it as "zero
///   grants" and then archiving it would move the user's record of what they
///   trusted out of the path ahma reads, having migrated nothing — the grants
///   would be silently orphaned. Leave it exactly where it is and say so; a
///   human can fix or delete it.
fn read_legacy_store(legacy: &Path) -> Option<BTreeMap<String, Vec<String>>> {
    let contents = std::fs::read_to_string(legacy).ok()?;
    match serde_json::from_str(&contents) {
        Ok(map) => Some(map),
        Err(e) => {
            warn!(
                "permissions: {} exists but does not parse ({e}); leaving it untouched. \
                 Fix or delete it, then re-run ahma to migrate its grants.",
                legacy.display()
            );
            None
        }
    }
}

/// Strict load of the ledger a migration writes into: never migrate into (and
/// so overwrite) a settings file that does not parse.
fn load_ledger_for_migration(settings_file: &Path) -> anyhow::Result<AhmaSettings> {
    AhmaSettings::load_from_result(settings_file)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .map_err(|e| e.context("refusing to migrate into an unparseable settings file"))
}

/// Rename the legacy store aside (`<name>.json.migrated`) once its grants are
/// safely in the ledger.
///
/// Renamed, not deleted, on purpose: it is the user's data, it records what they
/// chose to trust, and a migration bug that eats it is not recoverable. A
/// `.migrated` sibling costs nothing and keeps a mistake reversible by hand.
fn archive_legacy_store(
    legacy: &Path,
    moved: usize,
    settings_file: &Path,
) -> anyhow::Result<MigrationOutcome> {
    let archived_to = legacy.with_extension("json.migrated");
    std::fs::rename(legacy, &archived_to).map_err(|e| {
        anyhow::anyhow!(
            "migrated {moved} grant(s) into {} but could not archive {}: {e}",
            settings_file.display(),
            legacy.display()
        )
    })?;
    debug!(
        "permissions: migrated {moved} grant(s) from {} into {}",
        legacy.display(),
        settings_file.display()
    );
    Ok(MigrationOutcome::Migrated {
        grants: moved,
        archived_to,
    })
}

/// Fold `~/.config/ahma/approvals.json` into the ledger, once, non-destructively.
///
/// Idempotent: once the legacy file is renamed the next call sees no legacy file
/// and reports [`MigrationOutcome::NothingToDo`], so this is safe to call on
/// every startup.
pub fn migrate_legacy_approvals(settings_file: &Path) -> anyhow::Result<MigrationOutcome> {
    match legacy_approvals_path() {
        Some(legacy) => migrate_legacy_approvals_from(&legacy, settings_file),
        None => Ok(MigrationOutcome::NothingToDo),
    }
}

/// [`migrate_legacy_approvals`] from an explicit legacy file.
pub fn migrate_legacy_approvals_from(
    legacy: &Path,
    settings_file: &Path,
) -> anyhow::Result<MigrationOutcome> {
    let Some(legacy_grants) = read_legacy_store(legacy) else {
        return Ok(MigrationOutcome::NothingToDo);
    };
    let mut settings = load_ledger_for_migration(settings_file)?;

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

    archive_legacy_store(legacy, moved, settings_file)
}

/// Fold `~/.config/ahma/log_exceptions.json` — the targets `logs_approve` used
/// to write there — into `[log_targets]`, once, non-destructively.
///
/// Same contract as [`migrate_legacy_approvals`]: idempotent, a corrupt legacy
/// file is left untouched, and the legacy file is archived only after every
/// target it held has been read back from the ledger.
pub fn migrate_legacy_log_exceptions(settings_file: &Path) -> anyhow::Result<MigrationOutcome> {
    match legacy_log_exceptions_path() {
        Some(legacy) => migrate_legacy_log_exceptions_from(&legacy, settings_file),
        None => Ok(MigrationOutcome::NothingToDo),
    }
}

/// [`migrate_legacy_log_exceptions`] from an explicit legacy file.
pub fn migrate_legacy_log_exceptions_from(
    legacy: &Path,
    settings_file: &Path,
) -> anyhow::Result<MigrationOutcome> {
    let Some(legacy_targets) = read_legacy_store(legacy) else {
        return Ok(MigrationOutcome::NothingToDo);
    };
    let mut settings = load_ledger_for_migration(settings_file)?;

    let migrated = || Some("migrated".to_string());
    let mut moved = 0usize;
    for (workspace, targets) in &legacy_targets {
        let key = workspace_key(Path::new(workspace));
        for target in targets {
            if settings.log_targets.approve_target(
                &key,
                Path::new(target),
                None,
                migrated(),
                migrated(),
            ) {
                moved += 1;
            }
        }
    }

    if moved > 0 {
        settings.save_to(settings_file)?;

        // Verify before archiving, as for tool approvals — and verify every
        // target, not just "something landed": a missing target here is a log
        // the user approved silently going dark.
        let reloaded = AhmaSettings::load_from_result(settings_file)
            .map_err(|e| anyhow::anyhow!("could not re-read the ledger after migrating: {e}"))?;
        let all_landed = legacy_targets.iter().all(|(workspace, targets)| {
            let key = workspace_key(Path::new(workspace));
            targets
                .iter()
                .all(|t| reloaded.log_targets.is_target_approved(&key, Path::new(t)))
        });
        if !all_landed {
            anyhow::bail!(
                "migrated {moved} log target(s) into {} but not all of them are there on \
                 re-read (another ahma process may have rewritten the file). Leaving {} in \
                 place so nothing is lost; it will be retried on the next run.",
                settings_file.display(),
                legacy.display()
            );
        }
    }

    archive_legacy_store(legacy, moved, settings_file)
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

/// Run [`migrate_legacy_log_exceptions`] against the real settings file,
/// logging rather than propagating failures. Called from process startup, beside
/// [`migrate_legacy_approvals_best_effort`]; the worst case of a failure is that
/// a log symlink has to be approved again.
pub fn migrate_legacy_log_exceptions_best_effort() {
    let Some(settings_file) = crate::config::settings_path() else {
        return;
    };
    match migrate_legacy_log_exceptions(&settings_file) {
        Ok(MigrationOutcome::NothingToDo) => {}
        Ok(MigrationOutcome::Migrated {
            grants,
            archived_to,
        }) => {
            tracing::info!(
                "Migrated {grants} approved log target(s) into {} (the retired \
                 log_exceptions.json is kept at {}). Review them with \
                 `ahma permissions list --kind log-target`.",
                settings_file.display(),
                archived_to.display()
            );
        }
        Err(e) => warn!("permissions: could not migrate legacy log exceptions: {e:#}"),
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
    /// The request (`decision_id`) this answered, when it answered one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// How long the question was open before the human answered. This is the
    /// habituation signal: a median under a few seconds means the prompts are
    /// being clicked through, not read (SPEC R-PERM.9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_to_decision_ms: Option<u64>,
    /// The risk class shown at the prompt (`normal`, `high`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    /// The advisor's one-line recommendation shown at the prompt, if any
    /// (SPEC R-PERM.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice: Option<String>,
    /// Whether the human's answer was the one the advisor recommended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice_followed: Option<bool>,
}

impl AuditEntry {
    /// Attach the request this entry answered and how long it took.
    pub fn with_request(
        mut self,
        request_id: impl Into<String>,
        time_to_decision_ms: Option<u64>,
        risk: Option<&str>,
    ) -> Self {
        self.request_id = Some(request_id.into());
        self.time_to_decision_ms = time_to_decision_ms;
        self.risk = risk.map(str::to_string);
        self
    }

    /// Attach what the advisor said and whether the answer followed it.
    pub fn with_advice(mut self, advice: Option<String>, followed: Option<bool>) -> Self {
        self.advice = advice;
        self.advice_followed = followed;
        self
    }
}

/// Every parseable line of the audit log at `path`, oldest first. A corrupt
/// line is skipped: the log is append-only and a torn tail is expected.
pub fn read_audit_entries(path: &Path) -> Vec<AuditEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<AuditEntry>(l).ok())
        .collect()
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
        request_id: None,
        time_to_decision_ms: None,
        risk: None,
        advice: None,
        advice_followed: None,
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
    fn grant_tier_is_persistent() {
        assert!(!GrantTier::Once.is_persistent());
        assert!(!GrantTier::Session.is_persistent());
        assert!(GrantTier::Always.is_persistent());
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
    fn trust_is_workspace_scoped_and_does_not_approve_named_tools() {
        let mut p = PermissionSettings::default();
        assert!(!p.is_workspace_trusted(&key("/a")));
        assert!(p.trust_workspace(&key("/a"), None, None));
        assert!(!p.trust_workspace(&key("/a"), None, None), "idempotent");

        assert!(p.is_workspace_trusted(&key("/a")));
        assert!(!p.is_workspace_trusted(&key("/b")));
        // The wildcard is a marker, not a pattern: it approves no named tool on
        // its own — which tools trust covers is decided by the caller.
        assert!(!p.is_tool_approved(&key("/a"), "write_file"));

        assert!(p.revoke_tool(&key("/a"), TRUSTED_WORKSPACE_TOOL));
        assert!(!p.is_workspace_trusted(&key("/a")));
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

        // Production: home not redirected — the platform config dir is used.
        assert_eq!(
            resolve_legacy_path("approvals.json", None, real_config.clone()),
            Some(PathBuf::from("/real/config/ahma/approvals.json"))
        );
        assert_eq!(
            resolve_legacy_path("log_exceptions.json", None, real_config.clone()),
            Some(PathBuf::from("/real/config/ahma/log_exceptions.json"))
        );

        // A test that redirects HOME finds the legacy tree *inside that home*,
        // never the real one. The migration moves a file the user owns; letting a
        // test reach the real config tree would relocate live data into a
        // throwaway temp ledger.
        for name in ["approvals.json", "log_exceptions.json"] {
            assert_eq!(
                resolve_legacy_path(name, Some(PathBuf::from("/t/home")), real_config.clone()),
                Some(PathBuf::from("/t/home/.config/ahma").join(name)),
                "a redirected home must never migrate the real user's {name}"
            );
        }

        // No platform config dir and no redirected home: nothing to migrate.
        assert_eq!(resolve_legacy_path("approvals.json", None, None), None);
    }

    /// A legacy store in a tempdir, shaped like `{ "<workspace>": [...] }`.
    fn write_legacy_store(dir: &Path, name: &str, json: serde_json::Value) -> PathBuf {
        let legacy_dir = dir.join("ahma");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy = legacy_dir.join(name);
        std::fs::write(&legacy, json.to_string()).unwrap();
        legacy
    }

    #[test]
    fn migration_moves_grants_and_archives_the_legacy_file() {
        let home = tempdir().unwrap();
        let cfg = tempdir().unwrap();
        let settings_file = home.path().join(".ahma").join("settings.toml");
        let ws = home.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        let legacy = write_legacy_store(
            cfg.path(),
            "approvals.json",
            serde_json::json!({ ws.to_string_lossy(): ["cargo_build", "list_dir"] }),
        );

        match migrate_legacy_approvals_from(&legacy, &settings_file).unwrap() {
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

        for name in ["approvals.json", "log_exceptions.json"] {
            let legacy_dir = cfg.path().join("ahma");
            std::fs::create_dir_all(&legacy_dir).unwrap();
            let legacy = legacy_dir.join(name);
            std::fs::write(&legacy, "{ this is not json").unwrap();

            let migrated = if name == "approvals.json" {
                migrate_legacy_approvals_from(&legacy, &settings_file)
            } else {
                migrate_legacy_log_exceptions_from(&legacy, &settings_file)
            };
            let outcome = migrated.unwrap();

            assert_eq!(outcome, MigrationOutcome::NothingToDo, "{name}");
            assert_eq!(
                std::fs::read_to_string(&legacy).unwrap(),
                "{ this is not json",
                "a {name} we could not read must be left exactly where it is, untouched"
            );
        }
        assert!(
            !settings_file.exists(),
            "nothing was migrated, so no settings file is written"
        );
    }

    #[test]
    fn migration_with_no_legacy_file_is_a_no_op() {
        let home = tempdir().unwrap();
        let cfg = tempdir().unwrap();
        let settings_file = home.path().join(".ahma").join("settings.toml");
        let absent = cfg.path().join("ahma");

        assert_eq!(
            migrate_legacy_approvals_from(&absent.join("approvals.json"), &settings_file).unwrap(),
            MigrationOutcome::NothingToDo
        );
        assert_eq!(
            migrate_legacy_log_exceptions_from(&absent.join("log_exceptions.json"), &settings_file)
                .unwrap(),
            MigrationOutcome::NothingToDo
        );
        assert!(
            !settings_file.exists(),
            "a no-op migration must not create a settings file"
        );
    }

    /// The approvals `logs_approve` used to write to
    /// `~/.config/ahma/log_exceptions.json` move into `[log_targets]`, keyed by
    /// canonical workspace, marked as migrated; the legacy file is archived; and
    /// a second run finds nothing to do.
    #[test]
    fn log_exception_migration_imports_archives_and_is_idempotent() {
        let home = tempdir().unwrap();
        let cfg = tempdir().unwrap();
        let settings_file = home.path().join(".ahma").join("settings.toml");
        let ws = home.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        let legacy = write_legacy_store(
            cfg.path(),
            "log_exceptions.json",
            serde_json::json!({ ws.to_string_lossy(): ["/var/log/b.log", "/var/log/a.log"] }),
        );

        let archived_to = match migrate_legacy_log_exceptions_from(&legacy, &settings_file).unwrap()
        {
            MigrationOutcome::Migrated {
                grants,
                archived_to,
            } => {
                assert_eq!(grants, 2);
                archived_to
            }
            other => panic!("expected Migrated, got {other:?}"),
        };
        assert!(!legacy.exists(), "legacy file no longer at the old path");
        assert!(
            archived_to.exists(),
            "legacy file is archived, never deleted"
        );
        assert!(
            archived_to
                .to_string_lossy()
                .ends_with("log_exceptions.json.migrated")
        );

        let settings = AhmaSettings::load_from_result(&settings_file).unwrap();
        let key = workspace_key(&ws);
        assert_eq!(
            settings.log_targets.approved_targets(&key),
            vec![
                PathBuf::from("/var/log/a.log"),
                PathBuf::from("/var/log/b.log")
            ],
            "every target moves, sorted"
        );
        let entry = &settings.log_targets.approvals[0];
        assert_eq!(entry.granted_by.as_deref(), Some("migrated"));
        assert_eq!(entry.surface.as_deref(), Some("migrated"));

        let before = std::fs::read_to_string(&settings_file).unwrap();
        assert_eq!(
            migrate_legacy_log_exceptions_from(&legacy, &settings_file).unwrap(),
            MigrationOutcome::NothingToDo,
            "re-running finds no legacy file"
        );
        assert_eq!(
            std::fs::read_to_string(&settings_file).unwrap(),
            before,
            "re-running the migration changes nothing"
        );
    }

    #[test]
    fn approve_target_is_idempotent_sorted_and_workspace_scoped() {
        let mut t = LogTargetSettings::default();
        assert!(t.approve_target(&key("/ws"), &key("/logs/z.log"), None, None, None));
        assert!(t.approve_target(&key("/ws"), &key("/logs/a.log"), None, None, None));
        // Re-approving refreshes provenance, never duplicates.
        assert!(!t.approve_target(
            &key("/ws"),
            &key("/logs/z.log"),
            Some("2026-10-03".into()),
            Some("logs_approve".into()),
            Some("mcp:logs_approve".into()),
        ));

        assert_eq!(t.approvals.len(), 1);
        assert_eq!(
            t.approved_targets(&key("/ws")),
            vec![key("/logs/a.log"), key("/logs/z.log")]
        );
        assert_eq!(t.approvals[0].granted_at.as_deref(), Some("2026-10-03"));
        assert_eq!(t.approvals[0].granted_by.as_deref(), Some("logs_approve"));

        assert!(t.is_target_approved(&key("/ws"), &key("/logs/a.log")));
        // A target approved for one workspace is not approved for another.
        assert!(!t.is_target_approved(&key("/other"), &key("/logs/a.log")));
        assert!(t.approved_targets(&key("/other")).is_empty());
    }

    #[test]
    fn revoke_target_drops_workspace_entry_when_last_target_goes() {
        let mut t = LogTargetSettings::default();
        t.approve_target(&key("/ws"), &key("/logs/a.log"), None, None, None);
        t.approve_target(&key("/ws"), &key("/logs/b.log"), None, None, None);

        assert!(t.revoke_target(&key("/ws"), &key("/logs/a.log")));
        assert_eq!(t.approvals.len(), 1, "workspace still has b.log");
        assert!(!t.revoke_target(&key("/other"), &key("/logs/b.log")));

        assert!(t.revoke_target(&key("/ws"), &key("/logs/b.log")));
        assert!(
            t.approvals.is_empty(),
            "an empty workspace entry is removed, not left as a husk"
        );
        assert!(!t.revoke_target(&key("/ws"), &key("/logs/b.log")));
    }

    /// `persist_log_target` writes a row with provenance to the ledger file,
    /// leaves every other grant in it alone, and is idempotent.
    #[test]
    fn persist_log_target_writes_a_ledger_row_with_provenance() {
        let home = tempdir().unwrap();
        let settings_file = home.path().join(".ahma").join("settings.toml");
        let ws = home.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        let target = home.path().join("outside.log");

        // A pre-existing, unrelated grant must survive the read-modify-write.
        let mut seeded = AhmaSettings::default();
        seeded.network.allow.push("crates.io".into());
        seeded.save_to(&settings_file).unwrap();

        assert!(persist_log_target(&settings_file, &ws, &target, "mcp:logs_approve").unwrap());
        assert!(
            !persist_log_target(&settings_file, &ws, &target, "mcp:logs_approve").unwrap(),
            "approving the same target twice is a no-op"
        );

        let s = AhmaSettings::load_from_result(&settings_file).unwrap();
        assert_eq!(s.network.allow, vec!["crates.io".to_string()]);
        let key = workspace_key(&ws);
        assert!(s.log_targets.is_target_approved(&key, &target));
        let entry = &s.log_targets.approvals[0];
        assert_eq!(entry.granted_by.as_deref(), Some("logs_approve"));
        assert_eq!(entry.surface.as_deref(), Some("mcp:logs_approve"));
        assert_eq!(
            entry.granted_at.as_deref().map(str::len),
            Some(10),
            "granted_at is a YYYY-MM-DD date: {:?}",
            entry.granted_at
        );
    }

    /// The agent can plant a `.ahma/logs` link to anything, so the one write
    /// path for a `log-target` row applies the hard denylist itself (SPEC
    /// R-PERM.4.3): a key file is refused whoever asks, and nothing is written.
    #[test]
    fn persist_log_target_refuses_a_hard_denylisted_target() {
        let home = tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let key_file = ssh.join("id_ed25519");
        std::fs::write(&key_file, "secret").unwrap();
        let key_file = dunce::canonicalize(&key_file).unwrap();
        let settings_file = home.path().join(".ahma").join("settings.toml");
        // SAFETY: nextest runs every test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let result = persist_log_target_as(
            &settings_file,
            home.path(),
            &key_file,
            "logs_approve",
            "harness",
        );
        unsafe { std::env::remove_var("AHMA_TEST_HOME") };
        assert!(result.is_err(), "a credential file is never a log target");
        assert!(
            !settings_file.exists(),
            "a refused target must not reach the ledger"
        );
    }

    /// `persist_log_target_as` records who asked and where a human answered,
    /// separately.
    #[test]
    fn persist_log_target_as_records_granted_by_and_surface() {
        let home = tempdir().unwrap();
        let settings_file = home.path().join(".ahma").join("settings.toml");
        let ws = home.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        let target = home.path().join("outside.log");
        assert!(
            persist_log_target_as(&settings_file, &ws, &target, "logs_approve", "harness").unwrap()
        );
        let s = AhmaSettings::load_from_result(&settings_file).unwrap();
        let entry = &s.log_targets.approvals[0];
        assert_eq!(entry.granted_by.as_deref(), Some("logs_approve"));
        assert_eq!(entry.surface.as_deref(), Some("harness"));
    }

    #[test]
    fn persist_log_target_refuses_to_clobber_a_corrupt_ledger() {
        let home = tempdir().unwrap();
        let settings_file = home.path().join("settings.toml");
        std::fs::write(&settings_file, "[permissions\nbroken").unwrap();

        assert!(
            persist_log_target(&settings_file, home.path(), Path::new("/x.log"), "mcp:t").is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&settings_file).unwrap(),
            "[permissions\nbroken",
            "a ledger we cannot parse holds every other grant; it is never overwritten"
        );
    }

    #[test]
    fn every_persisted_kind_is_listed_and_hook_consent_is_not() {
        let persisted: Vec<GrantKind> = GrantKind::persisted().collect();
        assert_eq!(
            persisted,
            vec![
                GrantKind::FsScope,
                GrantKind::WebDomain,
                GrantKind::NetHost,
                GrantKind::Tool,
                GrantKind::LogTarget,
            ]
        );
        assert!(!GrantKind::HookUnsandboxed.is_persisted());
        assert_eq!(GrantKind::LogTarget.label(), "log-target");
        assert_eq!(
            serde_json::to_string(&GrantKind::LogTarget).unwrap(),
            "\"log-target\"",
            "the serde name and the CLI label agree"
        );
    }

    #[test]
    fn a_log_target_audit_entry_round_trips() {
        let e = audit_entry(
            "2026-10-03 12:00 UTC",
            AuditAction::Grant,
            GrantKind::LogTarget,
            "/var/log/app.log",
            Some("ro".into()),
            GrantTier::Always,
            Some("mcp:logs_approve".into()),
        );
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"kind\":\"log-target\""), "{json}");
        let back: AuditEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
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
            workspace: None,
            granted_by: Some("sccache".into()),
            granted_at: Some("2026-07-12".into()),
            note: None,
            expires_at: None,
        });
        s.web.always_allow.push("api.github.com".into());
        s.network.allow.push("crates.io".into());
        s.permissions
            .approve_tool(&key("/ws"), "cargo_build", None, None);
        s.log_targets.approve_target(
            &key("/ws"),
            &key("/var/log/app.log"),
            Some("2026-10-03".into()),
            Some("logs_approve".into()),
            Some("mcp:logs_approve".into()),
        );

        let rows = records(&s);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].kind, GrantKind::FsScope);
        assert_eq!(rows[0].access.as_deref(), Some("rw"));
        assert_eq!(rows[1].kind, GrantKind::WebDomain);
        assert!(
            rows.iter()
                .any(|r| r.kind == GrantKind::NetHost && r.subject == "crates.io")
        );
        let tool_row = rows.iter().find(|r| r.kind == GrantKind::Tool).unwrap();
        // A tool approval is qualified by its workspace, never conflated with it.
        assert_eq!(tool_row.subject, "cargo_build");
        assert_eq!(tool_row.scope_note.as_deref(), Some("/ws"));

        let log_row = rows
            .iter()
            .find(|r| r.kind == GrantKind::LogTarget)
            .unwrap();
        assert_eq!(log_row.subject, "/var/log/app.log");
        assert_eq!(
            log_row.access.as_deref(),
            Some("ro"),
            "log targets are read-only"
        );
        assert_eq!(log_row.scope_note.as_deref(), Some("/ws"));
        assert_eq!(log_row.granted_by.as_deref(), Some("logs_approve"));
        assert_eq!(log_row.surface.as_deref(), Some("mcp:logs_approve"));
        // Every row's kind is one `ahma permissions list` iterates over.
        assert!(rows.iter().all(|r| r.kind.is_persisted()));
    }
}

#[cfg(test)]
mod audit_context_tests {
    use super::*;

    /// The audit line is where rubber-stamping becomes measurable: it carries
    /// the request it answered, how long the human took, and the risk class.
    #[test]
    fn audit_entry_carries_request_id_and_time_to_decision() {
        let e = audit_entry(
            "2026-10-02T12:00:00Z",
            AuditAction::Grant,
            GrantKind::FsScope,
            "/x",
            Some("rw".into()),
            GrantTier::Session,
            Some("tui".into()),
        )
        .with_request("d1", Some(1_700), Some("high"));
        assert_eq!(e.request_id.as_deref(), Some("d1"));
        assert_eq!(e.time_to_decision_ms, Some(1_700));
        assert_eq!(e.risk.as_deref(), Some("high"));
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"request_id\":\"d1\""), "{json}");
        let plain: AuditEntry = serde_json::from_str(
            r#"{"at":"t","action":"grant","kind":"fs-scope","subject":"/x","tier":"session"}"#,
        )
        .unwrap();
        assert!(plain.request_id.is_none());
    }
}

/// Unix seconds as a local `HH:MM` for prompts; the date is dropped on purpose
/// (a prompt is about now, and a day-old one says "asked N times" anyway).
pub fn fmt_unix_secs(secs: u64) -> String {
    let s = secs % 86_400;
    format!("{:02}:{:02} UTC", s / 3_600, (s % 3_600) / 60)
}
