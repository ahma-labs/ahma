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
//!    cannot override this; only a human editing the file by hand can.
//! 2. **A human decision, not the model's word.** For the autonomous in-process
//!    agent ([`McpClientType::Ahma`](crate::client_type::McpClientType)), which
//!    auto-approves its own calls, `confirm: true` does **not** persist: the
//!    request is routed to the human approval surface (the TUI grant modal, or an
//!    actionable log/CLI hint) and only a human key-press writes it. For an
//!    external client (Cursor, VS Code, …), `confirm: true` raises an MCP
//!    `elicitation/create` prompt when the client supports it, and persists only
//!    on an explicit human approval — closing the hole where a *headless* external
//!    client that does not gate its own calls could self-grant. Only when the
//!    client cannot elicit does it fall back to a direct persist (a gating client
//!    approved the call itself); a decline or timeout never persists. The model
//!    cannot forge its client type (it is set by the connecting client at MCP
//!    init), so these gates are real.
//! 3. **A two-phase confirm**. Without `confirm: true` the tool only *previews*
//!    (writes nothing) and shows the exact file and line. The default is Deny.
//!
//! The grant is written to `~/.ahma/settings.toml`, which lives outside every
//! sandbox scope and only takes effect on the next server start — never the live
//! session (SPEC R5.4.7 session-immutability). The tool converges on the same
//! [`persist_grant`] code path as the CLI `ahma sandbox grant` and the TUI prompt.

use super::common;
use crate::AhmaMcpService;
use crate::mcp_service::schema;
use ahma_common::config::{GrantOutcome, ScopeAccess, ahma_home_dir, settings_path};
use ahma_common::scope_grant::persist_grant;
use rmcp::model::{CallToolResult, ErrorData as McpError};
use serde_json::{Map, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// The risk tier of a proposed grant, decided purely from the path, the user's
/// home directory, and the live sandbox scopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantRisk {
    /// Catastrophic and never legitimate — refused even with `confirm: true`.
    Refused(String),
    /// Allowed with confirmation, but each reason is surfaced loudly first.
    High(Vec<String>),
    /// An ordinary grant (a build cache, a dependency source dir, a sibling
    /// project, …).
    Normal,
}

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
    /// `client_type` distinguishes the autonomous in-process agent
    /// ([`McpClientType::Ahma`], which auto-approves its own tool calls) from
    /// external clients (Cursor, VS Code, …) that gate each tool call behind a
    /// human. For the autonomous agent, `confirm: true` must **not** self-persist
    /// — the request is routed to the human approval surface instead — so the
    /// model cannot widen its own sandbox. The LLM cannot forge `client_type`
    /// (it is set by the connecting client at MCP init), so this is a real gate,
    /// not an honor-system one.
    pub async fn handle_sandbox_grant(
        &self,
        args: Map<String, Value>,
        client_type: crate::client_type::McpClientType,
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

        // Confirmed and not denylisted. For the autonomous in-process agent there
        // is no per-call human approval, so `confirm: true` must not self-persist:
        // route the request to the human approval surface (TUI grant modal / log)
        // and return without writing. Only a human key-press at that surface
        // persists the grant (via the same GrantCoordinator the denial detector
        // uses). External clients gated the tool call behind a human already, so
        // they persist directly.
        if matches!(client_type, crate::client_type::McpClientType::Ahma) {
            let raised = self
                .adapter
                .request_scope_grant(&path, access, Some("sandbox_grant".to_string()))
                .await;
            return Ok(common::text_result(agent_requested_text(
                &path,
                access,
                &settings_file,
                raised,
            )));
        }

        // External client + `confirm: true`. The pre-existing model trusts that the
        // client gated this tool call behind a human. That assumption breaks for a
        // *headless* external client that auto-approves its own calls. So when the
        // client supports MCP elicitation, raise a real human yes/no prompt and
        // persist only on approval — closing the headless self-grant hole. When the
        // client cannot elicit we fall back to the pre-existing direct persist (a
        // gating client like Cursor approved the call itself); a decline, timeout,
        // or transport error never persists.
        let human_approved = {
            let peer = self.peer.read().unwrap().clone();
            match peer {
                None => true, // no peer to ask; pre-existing trust model
                Some(peer) => match peer
                    .elicit_with_timeout::<ScopeGrantForm>(
                        grant_prompt_message(&path, access, &settings_file, &line, &risk),
                        Some(std::time::Duration::from_secs(120)),
                    )
                    .await
                {
                    Ok(Some(form)) => grant_approved(&form.decision),
                    // Accepted with no content, or an explicit decline: do not grant.
                    Ok(None) | Err(rmcp::service::ElicitationError::UserDeclined) => false,
                    // Client cannot elicit → fall back to the pre-existing behavior.
                    Err(rmcp::service::ElicitationError::CapabilityNotSupported) => true,
                    // Cancelled, timed out, or transport error: never persist on doubt.
                    Err(e) => {
                        tracing::debug!("sandbox_grant elicitation unavailable: {e}");
                        false
                    }
                },
            }
        };
        if !human_approved {
            return Ok(common::text_result(declined_text(
                &path,
                access,
                &settings_file,
            )));
        }

        // Confirmed and not denylisted: persist to ~/.ahma/settings.toml.
        let outcome = persist_grant(
            &settings_file,
            &path,
            access,
            Some("sandbox_grant".to_string()),
            Some(granted_at),
            note,
        )
        .map_err(|e| {
            common::mcp_internal(format!(
                "failed to persist grant to {}: {e:#}",
                settings_file.display()
            ))
        })?;

        Ok(common::text_result(success_text(
            &path,
            access,
            &settings_file,
            &line,
            &outcome,
            &risk,
        )))
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

/// Classify the risk of granting `path`.
///
/// `path` is expected to arrive canonicalized (see [`resolve_grant_path`]).
/// `home` is canonicalized **here**, on purpose: the caller passes
/// `ahma_home_dir()`, which is whatever the OS reports and may contain a symlink
/// component — `/home` → `/mnt/home` on many Linux setups, an automounted
/// corporate home, or a macOS home relocated to another volume. Comparing a
/// resolved path against an unresolved `$HOME` makes every equality rule below
/// silently miss, and these rules are the *hard* denylist: `$HOME` itself,
/// `~/.ssh`, `~/.aws`, `~/.ahma`. A denylist that quietly stops matching is worse
/// than no denylist, because everything downstream assumes it held.
pub fn classify_grant_risk(path: &Path, home: Option<&Path>, scopes: &[PathBuf]) -> GrantRisk {
    let home = home.map(|h| dunce::canonicalize(h).unwrap_or_else(|_| h.to_path_buf()));
    let home = home.as_deref();

    // 1. A filesystem root has no parent — granting it exposes the whole drive.
    if path.parent().is_none() {
        return GrantRisk::Refused(
            "it is a filesystem root — granting it would expose the entire drive".to_string(),
        );
    }

    // 2. The exact home directory exposes every dotfile, key, and credential.
    if let Some(home) = home
        && path == home
    {
        return GrantRisk::Refused(
            "it is your home directory — granting it would expose every dotfile, key, and \
             credential under $HOME"
                .to_string(),
        );
    }

    // 3. A strict ancestor of a live scope would widen the sandbox above the
    //    workspace. (Equality is merely redundant — handled as a High warning.)
    for scope in scopes {
        if scope != path && scope.starts_with(path) {
            return GrantRisk::Refused(format!(
                "it is a parent of the active sandbox scope {} — granting it would widen the \
                 sandbox above your workspace",
                scope.display()
            ));
        }
    }

    // 4. Credential directories and ahma's own settings directory.
    if let Some(home) = home {
        const SENSITIVE: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube", ".docker", ".ahma"];
        if SENSITIVE.iter().any(|name| path == home.join(name))
            || path == home.join(".config").join("gh")
            || path == home.join(".config").join("gcloud")
        {
            return GrantRisk::Refused(format!(
                "'{}' holds credentials/secrets (or ahma's own settings) and must never be \
                 exposed to a sandboxed tool",
                path.display()
            ));
        }
    }

    // 5. OS system directories.
    if is_system_dir(path) {
        return GrantRisk::Refused(format!(
            "'{}' is a system directory — granting it is never required for a build and risks \
             the OS",
            path.display()
        ));
    }

    // ── Not refused: collect elevated-risk warnings. ──
    let mut warnings = Vec::new();

    if scopes.iter().any(|s| s == path) {
        warnings.push(
            "this path is already inside the active sandbox scope; the grant is redundant"
                .to_string(),
        );
    }

    // A direct child of the filesystem root (e.g. `/data`, `/opt`).
    if path.parent().is_some_and(|p| p.parent().is_none()) {
        warnings.push(format!(
            "'{}' sits directly under the filesystem root; double-check it is the specific \
             directory you mean",
            path.display()
        ));
    }

    if !path.exists() {
        warnings.push(format!(
            "'{}' does not exist on disk — confirm the path is correct and not a typo",
            path.display()
        ));
    }

    // A hidden directory directly under home that is not a known build cache.
    if let Some(home) = home
        && path.parent() == Some(home)
        && let Some(name) = path.file_name().and_then(|n| n.to_str())
        && name.starts_with('.')
        && !is_known_cache_dir(name)
    {
        warnings.push(format!(
            "'{}' is a hidden directory in your home folder; make sure it does not hold private \
             data",
            path.display()
        ));
    }

    if warnings.is_empty() {
        GrantRisk::Normal
    } else {
        GrantRisk::High(warnings)
    }
}

/// Whether `path` is exactly an OS system directory that must never be granted.
fn is_system_dir(path: &Path) -> bool {
    // Exact matches only: `/usr/local/foo` is a legitimate grant, `/usr` is not.
    const UNIX_SYSTEM_DIRS: &[&str] = &[
        "/etc", "/usr", "/bin", "/sbin", "/var", "/boot", "/dev", "/proc", "/sys", "/root",
        "/System", "/Library", "/opt", "/private",
    ];
    const WINDOWS_SYSTEM_DIRS: &[&str] = &[
        "C:\\Windows",
        "C:\\Program Files",
        "C:\\Program Files (x86)",
        "C:\\ProgramData",
    ];
    UNIX_SYSTEM_DIRS
        .iter()
        .chain(WINDOWS_SYSTEM_DIRS)
        .any(|d| path == Path::new(d))
}

/// Whether `name` (a `~/<name>` hidden directory) is a well-known build cache,
/// in which case a hidden-home-dir grant is unremarkable rather than elevated.
fn is_known_cache_dir(name: &str) -> bool {
    const CACHES: &[&str] = &[
        ".cargo",
        ".rustup",
        ".cache",
        ".npm",
        ".gradle",
        ".m2",
        ".pub-cache",
        ".cocoapods",
        ".sccache",
        ".ccache",
        ".gem",
        ".yarn",
        ".pnpm-store",
        ".nuget",
        ".gradle-cache",
        ".deno",
        ".bun",
    ];
    CACHES.contains(&name)
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
         This file lives outside every sandbox scope. Show the human the full path and the exact \
         line above, and get their explicit approval. Only then call `sandbox_grant` again with \
         `confirm: true` to write it. The default is Deny.",
        access = access.label(),
        path = path.display(),
        banner = risk_banner(risk),
        file = settings_file.display(),
        line = line,
    )
}

/// Message for the autonomous agent when `confirm: true` was routed to the human
/// approval surface instead of persisted. It must make clear the agent did NOT
/// widen the sandbox and that a human decision is required.
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
        "A human approval prompt has been raised. It is NOT granted until a person approves it \
         (Enter/Esc deny)."
    } else {
        "No interactive approval surface is attached, so nothing was requested."
    };
    format!(
        "Requested {access} access to\n  {path}\n\n\
         The autonomous agent cannot widen its own sandbox: `confirm: true` does not self-grant \
         here. {surface}\n\n\
         A human must approve — either at the prompt, or by running:\n  \
         ahma sandbox grant {path}{ro_flag}\n\n\
         Grants are written to {file} (outside every sandbox scope) and take effect on the next \
         server start, not the live session.",
        access = access.label(),
        path = path.display(),
        surface = surface,
        ro_flag = ro_flag,
        file = settings_file.display(),
    )
}

fn success_text(
    path: &Path,
    access: ScopeAccess,
    settings_file: &Path,
    line: &str,
    outcome: &GrantOutcome,
    risk: &GrantRisk,
) -> String {
    let headline = match outcome {
        GrantOutcome::Added => format!("✓ Granted {} access to {}", access.label(), path.display()),
        GrantOutcome::Updated(old) => format!(
            "✓ Updated grant for {}: {} → {}",
            path.display(),
            old.access.label(),
            access.label()
        ),
    };
    let high_note = match risk {
        GrantRisk::High(_) => format!("\n\n{}", risk_banner(risk)),
        _ => String::new(),
    };
    format!(
        "{headline}{high_note}\n\n\
         Wrote to {file}:\n  {line}\n\n\
         This takes effect on the next server start, NOT the live session. To apply it now, run \
         the `restart` tool, then re-run the command that was blocked.",
        headline = headline,
        high_note = high_note,
        file = settings_file.display(),
        line = line,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Interactive human approval via MCP elicitation (external clients)
// ─────────────────────────────────────────────────────────────────────────────

/// The form an elicitation-capable external client renders to approve or deny a
/// `sandbox_grant`. A single choice field; rmcp auto-generates the JSON schema.
/// The value is mapped by [`grant_approved`], which **fails safe** — anything that
/// is not an explicit approval is a deny, so a misbehaving client can never widen
/// the sandbox by returning garbage.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ScopeGrantForm {
    /// `approve` to persist the grant, or `deny` to refuse. The default is deny.
    pub decision: String,
}

rmcp::elicit_safe!(ScopeGrantForm);

/// Whether the client's raw choice is an explicit approval. Lenient on spelling
/// (case-insensitive, trims whitespace, accepts a few synonyms) but fails safe:
/// anything unrecognized — including an empty string — is **not** an approval.
pub fn grant_approved(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "approve" | "allow" | "yes" | "grant" | "ok"
    )
}

/// The human-facing prompt shown at the elicitation surface. States the exact
/// path, access, risk banner, and the line that would be written, so the operator
/// can judge the grant before approving.
fn grant_prompt_message(
    path: &Path,
    access: ScopeAccess,
    settings_file: &Path,
    line: &str,
    risk: &GrantRisk,
) -> String {
    format!(
        "Grant {access} sandbox access to '{path}'?\n\n\
         {banner}\n\n\
         This appends to {file} (outside every sandbox scope) and takes effect on the \
         next server start:\n  {line}\n\n\
         Answer 'approve' to persist, or 'deny' to refuse (the default).",
        access = access.label(),
        path = path.display(),
        banner = risk_banner(risk),
        file = settings_file.display(),
        line = line,
    )
}

/// Message returned to an external client when the human declined the elicitation
/// prompt (or it could not be answered): nothing was written.
fn declined_text(path: &Path, access: ScopeAccess, settings_file: &Path) -> String {
    format!(
        "Not granted — the human declined the prompt, so nothing was written.\n\n\
         Requested {access} access to\n  {path}\n\n\
         Nothing was appended to {file}. Re-run `sandbox_grant` with `confirm: true` to \
         prompt again, or the human can run `ahma sandbox grant {path}` directly.",
        access = access.label(),
        path = path.display(),
        file = settings_file.display(),
    )
}

#[cfg(test)]
#[path = "sandbox_grant_tool_tests.rs"]
mod tests;
