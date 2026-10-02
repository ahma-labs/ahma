//! The `sandbox_grant` MCP tool.
//!
//! When a tool hits an out-of-scope path, the server now attaches a structured
//! `sandbox_denial` payload to the error (see [`super::common::execution_error`]).
//! This tool is the AI-facing other half of that loop: given a path, it analyses
//! the request, classifies its risk, shows the human the **exact** file and line
//! that would be written, and — only on an explicit `confirm: true` — appends the
//! grant to the user-global `~/.ahma/settings.toml`.
//!
//! ## Security model
//!
//! Three independent gates stand between a confused/adversarial model and a
//! widened sandbox:
//!
//! 1. **A hard denylist** ([`classify_grant_risk`] → [`GrantRisk::Refused`]). The
//!    filesystem root, the exact `$HOME`, any parent of the live workspace scope,
//!    credential directories (`~/.ssh`, `~/.aws`, …), `~/.ahma` itself, and OS
//!    system directories are refused **even with `confirm: true`**. The model
//!    cannot override this; only a human editing the file by hand can. The same
//!    check runs again inside `ahma_common::scope_grant::persist_grant`, the one write path every
//!    surface shares.
//! 2. **A human decision, not the model's word.** `confirm: true` never
//!    persists anything by itself, for *any* client. It turns the preview into
//!    a request that is routed through the permission ladder (SPEC R-PERM.3):
//!    the client's own `elicitation/create` prompt when it declared that
//!    capability, else an attached ahma TUI, else the request fails closed with
//!    the `ahma sandbox grant` remediation. Only the ladder's human answer
//!    writes the grant. A client that cannot show a prompt is a client that
//!    cannot approve — it is **not** assumed to have asked a human before the
//!    call. (An earlier version made exactly that assumption, and a headless
//!    Antigravity session configured to auto-approve the tool granted itself
//!    a read-write directory with nobody asked.)
//! 3. **A two-phase confirm**. Without `confirm: true` the tool only *previews*
//!    (writes nothing) and shows the exact file and line. The default is Deny.
//!
//! The grant is written to `~/.ahma/settings.toml`, which lives outside every
//! sandbox scope. Once a human approves it, it applies immediately to the live
//! sandbox and persists for subsequent server starts.

use super::common;
use crate::AhmaMcpService;
use crate::mcp_service::schema;
use ahma_common::config::{ScopeAccess, ahma_home_dir, settings_path};
use rmcp::model::{CallToolResult, ErrorData as McpError};
use serde_json::{Map, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// The denylist lives in `ahma_common` so `persist_grant` — the one write
/// path — can apply it; re-exported so the CLI and these tests keep one name.
pub use ahma_common::scope_grant::{
    GrantRisk, classify_grant_risk, is_enclosing_git_repo, is_known_cache_dir, is_system_dir,
};

/// Build the JSON input schema advertised for the `sandbox_grant` tool.
pub fn sandbox_grant_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "path".to_string(),
        schema::path_property(
            "Absolute path to grant as a persistent sandbox root (e.g. the path named in a \
             `sandbox_denial` error). `~` is expanded to the home directory.",
        ),
    );
    props.insert(
        "access".to_string(),
        schema::enum_string_property_with_default(
            "`ro` for read-only (dependency source, toolchains) or `rw` for read+write \
             (build caches, sibling projects).",
            &["ro", "rw"],
            "ro",
        ),
    );
    props.insert(
        "confirm".to_string(),
        schema::boolean_property(
            "Must be `true` to actually write the grant. Omit (or `false`) to PREVIEW only: the \
             tool returns the full settings-file path and the exact line it would add so you can \
             show the human and get approval first. The default is always Deny.",
        ),
    );
    props.insert(
        "note".to_string(),
        schema::string_property("Optional provenance note recorded alongside the grant."),
    );
    schema::object_input_schema(props, &["path"])
}

impl AhmaMcpService {
    /// Handle a `sandbox_grant` call. See the module docs for the security model.
    ///
    /// The client type is deliberately not consulted: whether the caller is the
    /// in-process agent, an IDE that gates tool calls, or a headless harness
    /// that auto-approves them, `confirm: true` is routed to a human surface and
    /// never persists on its own (SPEC R5.4.5).
    pub async fn handle_sandbox_grant(
        &self,
        args: Map<String, Value>,
        _client_type: crate::client_type::McpClientType,
    ) -> Result<CallToolResult, McpError> {
        let raw = common::require_str(&args, "path", "sandbox_grant requires a `path` argument")?;
        let access = parse_access(&args)?;
        let confirm = args
            .get("confirm")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let note = common::opt_str(&args, "note");

        let home = ahma_home_dir();
        let scopes = self.adapter.sandbox().scopes().to_vec();
        let path = resolve_grant_path(&raw, home.as_deref(), scopes.first().map(|p| p.as_path()));

        let settings_file = settings_path().ok_or_else(|| {
            common::mcp_internal(
                "cannot locate ~/.ahma/settings.toml (home directory unknown); set HOME and retry",
            )
        })?;
        let granted_at = chrono::Local::now().format("%Y-%m-%d").to_string();
        let line = render_scope_line(&path, access, "sandbox_grant", &granted_at, note.as_deref());

        let risk = classify_grant_risk(&path, home.as_deref(), &scopes);

        // Gate 1: the hard denylist refuses outright, regardless of `confirm`.
        if let GrantRisk::Refused(reason) = &risk {
            return Err(common::mcp_invalid_params(format!(
                "sandbox_grant REFUSED for {}: {reason}.\n\nThis path is on the hard denylist and \
                 cannot be granted by the AI even with confirmation. If you genuinely need it, the \
                 human must edit {} by hand.",
                path.display(),
                settings_file.display(),
            )));
        }

        // Gate 2: without explicit confirmation, only preview — write nothing.
        if !confirm {
            return Ok(common::text_result(preview_text(
                &path,
                access,
                &settings_file,
                &line,
                &risk,
            )));
        }

        // Confirmed and not denylisted. `confirm: true` is the model's word,
        // never the human's (SPEC R5.4.5): raise the question at a human surface
        // through the permission ladder and return without writing. If the
        // ladder's human says yes, the broker persists the grant (audited,
        // denylisted) and we apply it to the live session here (R5.4.6).
        let raised = self
            .adapter
            .request_scope_grant(&path, access, Some("sandbox_grant".to_string()))
            .await;
        let approved = ahma_common::config::AhmaSettings::load_from_result(&settings_file)
            .ok()
            .and_then(|s| s.sandbox.find_scope(&path).map(|g| g.access));
        match approved {
            Some(granted) => {
                self.adapter.sandbox().add_live_grant(&path, granted);
                Ok(common::text_result(approved_text(
                    &path,
                    granted,
                    &settings_file,
                )))
            }
            None => Ok(common::text_result(agent_requested_text(
                &path,
                access,
                &settings_file,
                raised,
            ))),
        }
    }
}

/// Parse the optional `access` argument, defaulting to read-only.
fn parse_access(args: &Map<String, Value>) -> Result<ScopeAccess, McpError> {
    match args.get("access").and_then(Value::as_str) {
        None | Some("ro") => Ok(ScopeAccess::Ro),
        Some("rw") => Ok(ScopeAccess::Rw),
        Some(other) => Err(common::mcp_invalid_params(format!(
            "invalid access {other:?}; use \"ro\" or \"rw\""
        ))),
    }
}

/// Expand `~`, make the path absolute, and resolve it as far as the filesystem
/// allows. Existing paths are canonicalised (symlinks resolved); for a path that
/// does not exist yet the lexical form is cleaned (`.`/`..` collapsed) so risk
/// comparisons see a stable absolute path.
pub fn resolve_grant_path(raw: &str, home: Option<&Path>, workspace: Option<&Path>) -> PathBuf {
    let expanded = expand_tilde(raw, home);
    let mut pb = PathBuf::from(expanded);
    if pb.is_relative()
        && let Some(base) = workspace
    {
        pb = base.join(pb);
    }
    match dunce::canonicalize(&pb) {
        Ok(canon) => canon,
        Err(_) => clean_path(&pb),
    }
}

/// Expand a leading `~/` or `~\` (or a bare `~`) to `home`.
///
/// Thin wrapper over the shared [`ahma_common::config::expand_home_with`] so
/// every surface expands `~` the same way; kept here only to preserve the
/// `&str`-based call sites (and their injectable-home tests).
fn expand_tilde(raw: &str, home: Option<&Path>) -> String {
    ahma_common::config::expand_home_with(Path::new(raw), home)
        .to_string_lossy()
        .into_owned()
}

/// Lexically clean a path: collapse `.` and resolve `..` without touching the
/// filesystem. Used as the fallback for paths that do not exist yet.
fn clean_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Render the exact `persistent_scopes` array element that the grant would add,
/// matching the inline-table form used in `settings.toml`.
pub fn render_scope_line(
    path: &Path,
    access: ScopeAccess,
    granted_by: &str,
    granted_at: &str,
    note: Option<&str>,
) -> String {
    let access_str = if access.is_write() { "rw" } else { "ro" };
    let mut line = format!(
        "{{ path = \"{}\", access = \"{}\", granted_by = \"{}\", granted_at = \"{}\"",
        toml_escape(&path.to_string_lossy()),
        access_str,
        toml_escape(granted_by),
        toml_escape(granted_at),
    );
    if let Some(note) = note {
        line.push_str(&format!(", note = \"{}\"", toml_escape(note)));
    }
    line.push_str(" }");
    line
}

/// Escape the characters that matter inside a TOML basic string.
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn risk_banner(risk: &GrantRisk) -> String {
    match risk {
        GrantRisk::Normal => "Risk: NORMAL".to_string(),
        GrantRisk::High(warnings) => {
            let mut s = String::from("⚠ Risk: HIGH — review carefully before approving:");
            for w in warnings {
                s.push_str(&format!("\n  • {w}"));
            }
            s
        }
        // Refused never reaches the text builders.
        GrantRisk::Refused(reason) => format!("REFUSED: {reason}"),
    }
}

fn preview_text(
    path: &Path,
    access: ScopeAccess,
    settings_file: &Path,
    line: &str,
    risk: &GrantRisk,
) -> String {
    format!(
        "PREVIEW ONLY — nothing was written.\n\n\
         Proposed grant: {access} access to\n  {path}\n\n\
         {banner}\n\n\
         Would append this line to {file}:\n  {line}\n\n\
         This file lives outside every sandbox scope and only a human can change it. Tell the \
         human what you need and why, using the narrowest directory and `ro` unless a write was \
         denied. Then call `sandbox_grant` again with `confirm: true`: that does not grant — it \
         raises an approval prompt for the human (in your client, or the ahma TUI). The default \
         is Deny.",
        access = access.label(),
        path = path.display(),
        banner = risk_banner(risk),
        file = settings_file.display(),
        line = line,
    )
}

/// Message when `confirm: true` was routed to the human approval surface and
/// no grant exists yet. It must make clear the agent did NOT widen the sandbox
/// and that a human decision is required.
fn agent_requested_text(
    path: &Path,
    access: ScopeAccess,
    settings_file: &Path,
    raised: bool,
) -> String {
    let ro_flag = if access == ScopeAccess::Ro {
        " --read-only"
    } else {
        ""
    };
    let surface = if raised {
        "The request was handed to the human approval surfaces (your client's prompt if it \
         supports one, else an attached ahma TUI). It is NOT granted until a person approves it \
         (Enter/Esc deny); if it was already asked and declined this session, it will not be \
         asked again."
    } else {
        "No interactive approval surface is attached, so nothing was requested."
    };
    format!(
        "Requested {access} access to\n  {path}\n\n\
         You cannot widen your own sandbox: `confirm: true` does not self-grant, for any \
         client. {surface}\n\n\
         A human must approve — at the prompt, or by running:\n  \
         ahma sandbox grant {path}{ro_flag}\n\n\
         Grants are written to {file} (outside every sandbox scope); a human-approved grant \
         applies to this session immediately.",
        access = access.label(),
        path = path.display(),
        surface = surface,
        ro_flag = ro_flag,
        file = settings_file.display(),
    )
}

/// Message when the human approved the request at a surface and the grant is
/// now in the settings file and applied to the live session.
fn approved_text(path: &Path, granted: ScopeAccess, settings_file: &Path) -> String {
    format!(
        "✓ A human approved {access} access to\n  {path}\n\n\
         Recorded in {file} and applied to this session immediately. You can now re-run the \
         command that was blocked.",
        access = granted.label(),
        path = path.display(),
        file = settings_file.display(),
    )
}

#[cfg(test)]
#[path = "sandbox_grant_tool_tests.rs"]
mod tests;
