//! The channel from a detected scope violation to an approval surface.
//!
//! Detection (pre-execution path validation, or a stderr denial heuristic) produces
//! an offending `(path, access)`. A [`ScopeGrantNotifier`] decides whether to raise
//! a "grant access to X?" prompt and delivers it to a surface (TUI modal, MCP
//! `elicitation/create`). Dedup/debounce is the notifier's responsibility, owned by
//! the shared [`GrantCoordinator`] so the same path — which trips the kernel many
//! times — is only asked about once per session.
//!
//! This module also holds the two free wiring helpers the adapter calls so the
//! logic is unit-testable without constructing a full `Adapter`:
//! [`notify_pre_exec`] (a `PathOutsideSandbox` was returned up front) and
//! [`notify_stderr_denial`] (a sandboxed command failed and its stderr matched a
//! denial signature).
//!
//! PR boundary: this PR ships the trait + a [`LoggingGrantNotifier`] stub so
//! detection is observable in logs before any UI exists. The TUI and MCP surfaces
//! plug real notifiers into the same trait in later PRs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use ahma_common::config::ScopeAccess;
use ahma_common::scope_grant::{
    GrantContext, GrantCoordinator, GrantEvidence, GrantReason, GrantRequester, GrantRiskSummary,
    GrantStatus, ScopeGrantRequest,
};

/// The directory to actually offer for a denied `path` (P1c).
///
/// Granting a single *file* is nearly useless — the next sibling file in the
/// same cache trips the kernel again and re-prompts. So when a denial names a
/// file, offer its **parent directory** instead, which covers the whole cache
/// in one grant. A directory (or a path that already looks like one) is offered
/// as-is. This only ever walks up to the *immediate* parent and never suggests a
/// filesystem root, and the result is still only a suggestion the human approves
/// at the prompt — so it cannot widen scope on its own.
pub(crate) fn grant_dir_for(path: &Path) -> PathBuf {
    let looks_like_file = if path.is_dir() {
        false
    } else if path.is_file() {
        true
    } else {
        // Nonexistent (e.g. a cache file about to be created): treat a final
        // component bearing an extension as a file.
        path.extension().is_some()
    };

    if looks_like_file
        && let Some(parent) = path.parent()
        // `parent.parent().is_some()` is false only for a filesystem root, so
        // this refuses to ever suggest `/` (or a bare drive root) as a grant.
        && parent.parent().is_some()
        // Nor `$HOME`, `~/.ssh` or a system directory (SPEC R-PERM.4.3): a file
        // denied there is offered as itself, never widened to a directory no
        // prompt may grant.
        && ahma_common::scope_grant::refusal_reason(parent).is_none()
    {
        return parent.to_path_buf();
    }
    path.to_path_buf()
}

/// What every surface says about a refusal no grant can change (SPEC
/// R-PERM.4.3, R-PERM.9): one first line saying so, and why. Never grant
/// commands, a tier to pick or an `always` line: the hard denylist refuses
/// them, and offering them anyway was a question whose answer is always no.
/// `None` for a path a human may be asked about.
pub fn never_grantable_text(path: &Path, access: ScopeAccess) -> Option<String> {
    let why = ahma_common::scope_grant::refusal_reason(path)?;
    let verb = if access.is_write() { "write" } else { "read" };
    Some(format!(
        "Blocked, and no grant can change it: ahma's kernel sandbox refused a {verb} of '{}'. \
         {why}",
        path.display()
    ))
}

/// Agent-facing remediation for a *runtime* sandbox denial (a sandboxed command
/// exited non-zero because the kernel blocked an out-of-scope access). Describes
/// the supported grant -> restart -> retry loop using the MCP tools, so the sync
/// error payload and the async operation alert phrase the recovery identically.
///
/// References `grant_dir_for` so the suggested grant path matches what the
/// approval prompt offers (a file's parent directory, so one grant covers the
/// whole cache rather than re-prompting per file).
pub fn runtime_denial_remediation(path: &Path, access: ScopeAccess) -> String {
    if let Some(text) = never_grantable_text(path, access) {
        return text;
    }
    let target = grant_dir_for(path);
    let access_str = if access.is_write() { "rw" } else { "ro" };
    let verb = if access.is_write() { "write" } else { "read" };
    format!(
        "ahma's kernel sandbox blocked an out-of-scope {verb} to '{denied}'. This is expected: \
         writing outside the workspace (for example installing a global binary under ~/.cargo) is \
         denied by default. Only a human can allow it: tell them you need \"{access_str}\" access \
         to \"{target}\" and why (ask for the narrowest directory, and `ro` unless a write was \
         denied). They approve it in the ahma TUI or run `ahma sandbox grant {target}`. The \
         `sandbox_grant` tool with `confirm: true` only raises that prompt — it cannot grant. \
         A human-approved grant applies to this session immediately; then re-run the command.",
        verb = verb,
        denied = path.display(),
        target = target.display(),
        access_str = access_str,
    )
}

/// CLI-oriented variant of [`runtime_denial_remediation`] for contexts where the
/// MCP `sandbox_grant`/`restart` tools are not in play — notably the shell hook
/// running in the editor's *native* terminal. Points at `ahma sandbox grant`.
pub fn runtime_denial_remediation_cli(path: &Path, access: ScopeAccess) -> String {
    if let Some(text) = never_grantable_text(path, access) {
        return text;
    }
    let target = grant_dir_for(path);
    let ro_flag = if access.is_write() {
        ""
    } else {
        " --read-only"
    };
    let verb = if access.is_write() { "write" } else { "read" };
    // One first line that answers "do I need to do anything?" (SPEC R-PERM.9),
    // then the exact command for each tier, then why. Hooks re-derive the
    // sandbox per command, so the grant applies on the next command: no
    // restart, and no second sentence saying otherwise.
    format!(
        "Blocked until a human grants it: ahma's kernel sandbox refused an out-of-scope {verb} to \
         '{denied}'.\n\nOne thing to do (pick a tier), then re-run the command:\n  ahma sandbox grant \
         {target}{ro_flag} --session   # this terminal session only, at most 12h\n  ahma sandbox grant \
         {target}{ro_flag}             # until revoked, bound to this workspace\n\nEither applies on your \
         next command; nothing to restart. The grant is checked against ahma's denylist and \
         audited. Writing outside the workspace (a global binary under ~/.cargo, a cache under \
         ~/Library) is denied by default; the narrowest directory that fixes the denial is the \
         one to grant, read-only unless a write was refused.",
        verb = verb,
        denied = path.display(),
        target = target.display(),
        ro_flag = ro_flag,
    )
}

/// Shared actionable tail for the "blocked out-of-scope path" log lines: how to
/// allow it and how to make the grant take effect.
fn grant_hint(path: &Path, access: ScopeAccess) -> String {
    let ro_flag = if access == ScopeAccess::Ro {
        " --read-only"
    } else {
        ""
    };
    format!(
        "run `ahma sandbox grant {}{}`. The grant is saved to settings and applies from \
         the next command, in running servers too.",
        path.display(),
        ro_flag,
    )
}

/// Delivers a scope-grant request to a human approval surface. Implementors own
/// (a clone of) the shared [`GrantCoordinator`] and call
/// [`GrantCoordinator::begin_with_context`] to dedup before delivering.
///
/// The context-carrying method is the one implementors **must** write. It used
/// to be an optional extra with a default that dropped the context, and the
/// production broker never overrode it: every prompt it raised read "unknown
/// session" and "Risk: not assessed" while the tests, whose fakes were handed
/// their context directly, stayed green (SPEC R-PERM.3.4). There is no default
/// now, so a notifier cannot quietly be shorter than the body it renders.
#[async_trait]
pub trait ScopeGrantNotifier: Send + Sync + std::fmt::Debug {
    /// Consider raising a grant prompt for `path` at `access`, carrying the
    /// judgement aids the caller gathered (SPEC R-PERM.3.4). A no-op if the
    /// `(canonical_path, access)` was already asked or dismissed this session,
    /// is refused outright, or the prompt budget is spent. Returns the request
    /// that was raised, if one was, so the caller can show the same body the
    /// human sees and later ask where it stands ([`Self::status`]).
    async fn notify_violation_with(
        &self,
        path: &Path,
        access: ScopeAccess,
        reason: GrantReason,
        tool: Option<String>,
        context: GrantContext,
    ) -> Option<ScopeGrantRequest>;

    /// [`Self::notify_violation_with`] for a caller that knows nothing beyond
    /// the path. The body still renders every section; the unknown ones say so.
    async fn notify_violation(
        &self,
        path: &Path,
        access: ScopeAccess,
        reason: GrantReason,
        tool: Option<String>,
    ) {
        let _ = self
            .notify_violation_with(path, access, reason, tool, GrantContext::default())
            .await;
    }

    /// Whether the session's automatic prompt budget is spent (SPEC R-PERM.4.5).
    fn budget_exhausted(&self) -> bool;

    /// Where a question this notifier raised stands, so the surface that asked
    /// on the agent's behalf can tell it the answer rather than a guess
    /// (SPEC R-PERM.9).
    fn status(&self, decision_id: &str) -> GrantStatus;
}

/// Facts about a grant target a human can check at a glance (SPEC R-PERM.3.4):
/// names and counts only — never file contents, which could carry an injection
/// into the prompt or an advisor. Bounded to one directory listing.
pub fn inspect_grant_target(path: &Path) -> Vec<String> {
    let mut facts = Vec::new();
    match std::fs::symlink_metadata(path) {
        Err(_) => facts.push("does not exist yet (a grant would let the command create it)".into()),
        Ok(meta) if meta.file_type().is_symlink() => {
            facts.push("is a symlink; the grant applies to what it points at".into())
        }
        Ok(meta) if meta.is_file() => facts.push("is a single file".into()),
        Ok(_) => {
            let mut entries = 0usize;
            let mut dotfiles = 0usize;
            if let Ok(rd) = std::fs::read_dir(path) {
                for e in rd.flatten().take(5_000) {
                    entries += 1;
                    if e.file_name().to_string_lossy().starts_with('.') {
                        dotfiles += 1;
                    }
                }
            }
            facts.push(format!(
                "directory with {entries}{} entries{}",
                if entries >= 5_000 { "+" } else { "" },
                if dotfiles > 0 {
                    format!(", {dotfiles} hidden")
                } else {
                    String::new()
                }
            ));
        }
    }
    if let Some(file) = ahma_common::config::settings_path()
        && let Ok(settings) = ahma_common::config::AhmaSettings::load_from_result(&file)
    {
        let canon = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let others: Vec<String> = settings
            .sandbox
            .persistent_scopes
            .iter()
            .filter(|g| {
                let gp = ahma_common::config::expand_home(&g.path);
                dunce::canonicalize(&gp).unwrap_or(gp) == canon
            })
            .map(|g| {
                g.workspace
                    .as_ref()
                    .map(|w| w.display().to_string())
                    .unwrap_or_else(|| "every workspace (global)".into())
            })
            .collect();
        if !others.is_empty() {
            facts.push(format!(
                "already granted for {} other workspace{}: {}",
                others.len(),
                if others.len() == 1 { "" } else { "s" },
                others.join(", ")
            ));
        }
    }
    facts
}

/// Assemble the request context from what this process knows: who it is,
/// what was denied, and how risky the target is (SPEC R-PERM.3.4).
pub fn build_context(
    sandbox: &super::Sandbox,
    target: &Path,
    evidence: Option<GrantEvidence>,
    command: Option<&str>,
    op_id: Option<&str>,
    agent_claim: Option<&str>,
    write_denied: bool,
) -> GrantContext {
    let identity = crate::hub_reporter::current_identity();
    let scopes = sandbox.scopes().to_vec();
    let risk = risk_summary(target, &scopes);
    GrantContext {
        requester: Some(GrantRequester {
            client: identity.client.clone(),
            session_id: identity.session_id.clone(),
            workspace: scopes.first().cloned(),
            pid: std::process::id(),
        }),
        op_id: op_id.map(str::to_string),
        command: command.map(redact_command),
        evidence,
        agent_claim: agent_claim.map(str::to_string),
        risk: Some(risk),
        times_asked: 0,
        first_asked_at: None,
        write_denied,
    }
}

/// The risk section of a grant prompt for `target`, judged against `scopes`:
/// the denylist's class and warnings, plus what a human can check at a glance.
/// One implementation for the MCP path and the terminal-hook path, so the same
/// path never reads as assessed on one surface and "not assessed" on the other.
pub fn risk_summary(target: &Path, scopes: &[PathBuf]) -> GrantRiskSummary {
    match ahma_common::scope_grant::classify_grant_risk(
        target,
        ahma_common::config::ahma_home_dir().as_deref(),
        scopes,
    ) {
        ahma_common::scope_grant::GrantRisk::High(warnings) => GrantRiskSummary {
            class: "high".into(),
            warnings,
            facts: inspect_grant_target(target),
        },
        ahma_common::scope_grant::GrantRisk::Refused(why) => GrantRiskSummary {
            class: "refused".into(),
            warnings: vec![why],
            facts: Vec::new(),
        },
        ahma_common::scope_grant::GrantRisk::Normal => GrantRiskSummary {
            class: "normal".into(),
            warnings: Vec::new(),
            facts: inspect_grant_target(target),
        },
    }
}

/// A command line safe to show at a prompt: `KEY=secret` prefixes and
/// `--token x`-style values are masked, and it is cut to one screen line.
pub fn redact_command(cmd: &str) -> String {
    let mut out = Vec::new();
    let mut mask_next = false;
    for tok in cmd.split_whitespace() {
        if mask_next {
            out.push("***".to_string());
            mask_next = false;
            continue;
        }
        let lower = tok.to_ascii_lowercase();
        if let Some((k, _)) = tok.split_once('=')
            && (lower.contains("token")
                || lower.contains("secret")
                || lower.contains("password")
                || lower.contains("key="))
        {
            out.push(format!("{k}=***"));
        } else if lower.starts_with("--token") || lower.starts_with("--password") {
            out.push(tok.to_string());
            mask_next = !tok.contains('=');
        } else {
            out.push(tok.to_string());
        }
    }
    let joined = out.join(" ");
    if joined.chars().count() > 200 {
        let cut: String = joined.chars().take(197).collect();
        format!("{cut}...")
    } else {
        joined
    }
}

/// The line in `stderr`/`stdout` that names `needle`, trimmed, for evidence.
fn evidence_line(stderr: &str, stdout: &str, needle: &Path) -> Option<String> {
    let n = needle.to_string_lossy();
    stderr
        .lines()
        .chain(stdout.lines())
        .find(|l| l.contains(&*n))
        .map(|l| l.trim().chars().take(300).collect::<String>())
}

/// The PR-boundary stub: instead of a UI, log an actionable line (once per
/// `(path, access)`, via the coordinator) telling the human exactly how to grant
/// the path. Replaced by the TUI / MCP notifiers in later PRs.
#[derive(Debug)]
pub struct LoggingGrantNotifier {
    coordinator: Arc<GrantCoordinator>,
}

impl LoggingGrantNotifier {
    /// Create a logging notifier sharing `coordinator` (the session's single
    /// [`GrantCoordinator`]).
    pub fn new(coordinator: Arc<GrantCoordinator>) -> Self {
        Self { coordinator }
    }
}

#[async_trait]
impl ScopeGrantNotifier for LoggingGrantNotifier {
    async fn notify_violation_with(
        &self,
        path: &Path,
        access: ScopeAccess,
        reason: GrantReason,
        tool: Option<String>,
        context: GrantContext,
    ) -> Option<ScopeGrantRequest> {
        let req = self
            .coordinator
            .begin_with_context(path, access, reason, tool, context)?;
        tracing::warn!(
            path = %req.path.display(),
            access = req.access.label(),
            "Sandbox blocked an out-of-scope path. To allow it, {}",
            grant_hint(&req.path, req.access),
        );
        Some(req)
    }

    fn budget_exhausted(&self) -> bool {
        self.coordinator.budget_exhausted()
    }

    /// A log line is not a surface anyone answers: a question this notifier
    /// raised stays in flight only so the same path is logged once, and is
    /// reported as closed — nobody was asked.
    fn status(&self, decision_id: &str) -> GrantStatus {
        match self.coordinator.status(decision_id) {
            GrantStatus::Pending => GrantStatus::Closed,
            other => other,
        }
    }
}

/// Wiring helper: a `PathOutsideSandbox` was returned by path validation up front,
/// so the offending path is known exactly. Offers it as a read+write grant (a
/// working directory / path argument is used for both), with the same context a
/// runtime denial carries: who is asking, the risk, the evidence. A no-op when
/// there is no notifier or the error is a different `SandboxError`.
pub async fn notify_pre_exec(
    sandbox: &super::Sandbox,
    notifier: Option<&Arc<dyn ScopeGrantNotifier>>,
    err: &anyhow::Error,
    tool: &str,
) {
    let Some(n) = notifier else { return };
    if let Some(super::SandboxError::PathOutsideSandbox { path, .. }) =
        err.downcast_ref::<super::SandboxError>()
    {
        // Offer the enclosing directory when the argument is a file (P1c), so one
        // grant covers it and its siblings.
        let target = grant_dir_for(path);
        let evidence = GrantEvidence {
            raw_path: Some(path.clone()),
            pattern: Some("path validation before the command ran".into()),
            line: Some(err.to_string().lines().next().unwrap_or("").to_string()),
        };
        let context = build_context(
            sandbox,
            &target,
            Some(evidence),
            Some(tool),
            None,
            None,
            true,
        );
        let _ = n
            .notify_violation_with(
                &target,
                ScopeAccess::Rw,
                GrantReason::PreExecViolation,
                Some(tool.to_string()),
                context,
            )
            .await;
    }
}

/// What a terminal hook knows about the party asking for a grant: the harness
/// that ran the command (named from its environment markers, which may name a
/// requester but never decide enforcement, SPEC R7), the harness session, the
/// scopes the hook sandboxed it in, and the command itself.
#[derive(Debug, Clone, Default)]
pub struct HookRequester {
    /// The harness, e.g. "Claude Code"; `None` when no marker names one.
    pub harness: Option<String>,
    /// The harness's own session id, when its hook payload carried one.
    pub session_id: Option<String>,
    /// The scopes the hook sandboxed the command in; the first is the workspace.
    pub scopes: Vec<PathBuf>,
    /// The command line that was denied.
    pub command: Option<String>,
}

impl HookRequester {
    /// The prompt context for a denial of `target`: who asked, which command,
    /// and the risk, judged against the hook's own scopes.
    fn context(&self, target: &Path, write_denied: bool) -> GrantContext {
        GrantContext {
            requester: Some(GrantRequester {
                client: Some(format!(
                    "{} (terminal hook)",
                    self.harness.as_deref().unwrap_or("an unidentified harness")
                )),
                session_id: self.session_id.clone(),
                workspace: self.scopes.first().cloned(),
                pid: std::process::id(),
            }),
            command: self.command.as_deref().map(redact_command),
            risk: Some(risk_summary(target, &self.scopes)),
            write_denied,
            ..Default::default()
        }
    }
}

/// What a terminal hook prints for a denial (SPEC R-PERM.9, R-PERM.3.4): the
/// first line says the user must act, then the same body every other surface
/// shows, then the exact commands for each tier.
pub fn hook_denial_text(
    path: &Path,
    access: ScopeAccess,
    details: &str,
    who: &HookRequester,
) -> String {
    if let Some(text) = never_grantable_text(path, access) {
        return text;
    }
    let target = grant_dir_for(path);
    let body = ahma_common::grant_prompt::render_for_hook(
        &target,
        access,
        details,
        who.context(&target, access.is_write()),
    );
    let ro_flag = if access.is_write() {
        ""
    } else {
        " --read-only"
    };
    format!(
        "The sandbox refused a {} outside the workspace: '{}'. If this command needs it, a human \
         must grant it; ahma will not run it unsandboxed.\n\n{}\nOne thing to do (pick a tier), \
         then re-run the command:\n  ahma sandbox \
         grant {target}{ro_flag} --session   # this terminal session only, at most 12h\n  ahma \
         sandbox grant {target}{ro_flag}             # until revoked, bound to this workspace\n\n\
         Either applies on your next command; nothing to restart.",
        if access.is_write() { "write" } else { "read" },
        path.display(),
        body.to_message(),
        target = target.display(),
        ro_flag = ro_flag,
    )
}

/// One line for a hooked command that **succeeded** although the sandbox
/// refused one of its accesses outside the workspace (a lock-holder file, a
/// cache it can do without): what was refused and the command that would
/// allow it, said once. The full panel of [`hook_denial_text`] is for a
/// command that failed; a refusal the command shrugged off is a warning, not
/// a block. `None` when the output shows no refusal outside the scope.
pub fn hook_side_refusal_note(output: &str, who: &HookRequester) -> Option<String> {
    let (path, access) = hook_side_refusal(output, who)?;
    Some(refusal_note(&path, access))
}

/// The out-of-scope path and access a hooked command that **succeeded** was
/// refused, by the evidence rules of [`hook_side_refusal_note`].
pub fn hook_side_refusal(
    output: &str,
    who: &HookRequester,
) -> Option<(PathBuf, ahma_common::config::ScopeAccess)> {
    // A sandbox-extension failure is a framework declining to hand a helper
    // access (WebKit and fonts), not the command being refused an access.
    let output: String = output
        .lines()
        .filter(|l| !l.contains("sandbox_extension"))
        .collect::<Vec<_>>()
        .join("\n");
    let hit = super::denial_scan::scan_denial_streams(&output, "")?;
    let path = &hit.path;
    // Never suggest granting a filesystem root or a system directory: the
    // first is text that merely mentions a refusal (a `//` in listed source),
    // the second nobody should grant.
    let normal_parts = path
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count();
    const SYSTEM: &[&str] = &[
        "/System",
        "/usr",
        "/bin",
        "/sbin",
        "/dev",
        "/etc",
        "/private/etc",
        "/Library/Apple",
    ];
    if normal_parts < 2 || SYSTEM.iter().any(|p| path.starts_with(p)) {
        return None;
    }
    // For a command that succeeded, the evidence must be one line: the path
    // and the refusal together. Borrowing a path from an earlier line (the
    // multi-line form the failure path uses) paired `SSH_AUTH_SOCK=…` with an
    // unrelated `Permission denied (publickey)`.
    let shown = path.display().to_string();
    let refused_here = output.lines().any(|l| {
        let lower = l.to_ascii_lowercase();
        l.contains(&shown)
            && !is_listed_text(l)
            && (lower.contains("operation not permitted")
                || lower.contains("permission denied")
                || lower.contains("read-only file system"))
            && !lower.contains("(publickey)")
    });
    if !refused_here {
        return None;
    }
    // A refused access happens on this machine: its directory exists. A path
    // whose directory does not is text that mentions a refusal (a fixture, a
    // log from elsewhere), not one.
    if !path.parent().is_some_and(Path::exists) {
        return None;
    }
    // Nothing to offer for a path no grant can open, and a command that
    // succeeded did not need it.
    if ahma_common::scope_grant::refusal_reason(path).is_some() {
        return None;
    }
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if who
        .scopes
        .iter()
        .any(|s| canon(path).starts_with(canon(s)) || path.starts_with(s))
    {
        return None;
    }
    Some((path.clone(), hit.access))
}

/// Whether `line` is listed text rather than a tool's own diagnostic: `grep -n`
/// and compiler-style output begin `file:line:`, and what follows is quoted
/// content, not something that just happened.
fn is_listed_text(line: &str) -> bool {
    let mut parts = line.splitn(3, ':');
    let (Some(file), Some(num), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !file.is_empty()
        && !file.contains(char::is_whitespace)
        && !num.is_empty()
        && num.chars().all(|c| c.is_ascii_digit())
}

/// The one-line note for a refusal a hooked command shrugged off.
pub fn refusal_note(path: &Path, access: ahma_common::config::ScopeAccess) -> String {
    let target = grant_dir_for(path);
    let (verb, ro) = if access.is_write() {
        ("write", "")
    } else {
        ("read", " --read-only")
    };
    format!(
        "ahma: the sandbox refused a {verb} outside the workspace ({}) and the command went on \
         without it; if it needs that, a human can run `ahma sandbox grant {}{ro}` (add \
         `--session` for this terminal session only); in Claude Code, ahma asks in its \
         dialog when the command is run again.",
        path.display(),
        target.display()
    )
}

/// Wiring helper: a sandboxed command failed; scan its stderr for a denial and, if
/// the referenced path is genuinely out of scope, offer to grant it. A denial for a
/// path already in scope (e.g. a root-owned file inside the workspace) is unrelated
/// to the sandbox and is ignored.
pub async fn notify_stderr_denial(
    sandbox: &super::Sandbox,
    notifier: Option<&Arc<dyn ScopeGrantNotifier>>,
    stderr: &str,
    stdout: &str,
    tool: &str,
) {
    notify_stderr_denial_in_dir(sandbox, notifier, stderr, stdout, tool, None, None).await;
}

/// Variant of [`notify_stderr_denial`] that resolves relative candidate paths
/// against the command's actual working directory.
///
/// Waits for the human's answer. A command's own failure path must not: it
/// uses [`raise_stderr_denial_question`], which asks without waiting.
pub async fn notify_stderr_denial_in_dir(
    sandbox: &super::Sandbox,
    notifier: Option<&Arc<dyn ScopeGrantNotifier>>,
    stderr: &str,
    stdout: &str,
    tool: &str,
    working_dir: Option<&Path>,
    op_id: Option<&str>,
) {
    let Some(n) = notifier else { return };
    let Some(question) = stderr_denial_question(sandbox, stderr, stdout, tool, working_dir, op_id)
    else {
        return;
    };
    question.ask(n.as_ref()).await;
}

/// Raise the grant question for a failed command's out-of-scope denial, and
/// return without waiting for the answer.
///
/// A question is not a wait (SPEC R2.7.1, R-PERM.3): the command has already
/// failed, its result belongs to the agent now, and the workspace belongs to
/// whoever is next. Waiting here held both for as long as a dialog nobody was
/// looking at stayed open. An approval still applies live to the next command.
pub fn raise_stderr_denial_question(
    sandbox: &super::Sandbox,
    notifier: Option<&Arc<dyn ScopeGrantNotifier>>,
    stderr: &str,
    stdout: &str,
    tool: &str,
    working_dir: Option<&Path>,
    op_id: Option<&str>,
) {
    let Some(n) = notifier else { return };
    let Some(question) = stderr_denial_question(sandbox, stderr, stdout, tool, working_dir, op_id)
    else {
        return;
    };
    let n = Arc::clone(n);
    tokio::spawn(async move { question.ask(n.as_ref()).await });
}

/// What the kernel recorded for a failed command (SPEC R-DENY): the lines its
/// result carries, and the first path a human may grant with the access the
/// kernel refused, after asking about it without waiting (R-PERM.3.9).
///
/// Asked only when the output suggests a refusal, so other failures cost
/// nothing. A record may arrive late, and under heavy load macOS drops some
/// altogether, so an empty answer is asked once more and is never taken to
/// mean the sandbox refused nothing (R-DENY.3): `None` then, and the caller
/// judges from the output as before. `None` when the records cannot be read
/// here; the caller then scans the output as before. `Some(vec![])` means the
/// kernel refused nothing: the failure was not the sandbox's, and nothing is
/// suggested (R-DENY.3).
pub async fn kernel_denial_lines(
    sandbox: &super::Sandbox,
    notifier: Option<&Arc<dyn ScopeGrantNotifier>>,
    output: (&str, &str),
    tag: Option<&str>,
    within: std::time::Duration,
    tool: &str,
    op_id: Option<&str>,
) -> Option<KernelReport> {
    use super::kernel_denials::{DenialClass, classify, one_line, read_records};
    let tag = tag?;
    if super::denial_scan::scan_denial_streams(output.0, output.1).is_none()
        && !looks_refused(output.0)
        && !looks_refused(output.1)
    {
        return None;
    }
    let mut records = Vec::new();
    for wait_ms in [0u64, 300] {
        tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
        records = read_records(tag, within + std::time::Duration::from_millis(wait_ms)).await?;
        if !records.is_empty() {
            break;
        }
    }
    if records.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    let mut grant = None;
    let mut asked = false;
    for denial in &records {
        let class = classify(denial);
        let line = one_line(denial, &class);
        if !lines.contains(&line) {
            lines.push(line);
        }
        if let DenialClass::Grant { path, access } = &class
            && grant.is_none()
        {
            grant = Some((path.clone(), *access));
        }
        if let (DenialClass::Grant { path, access }, false, Some(n)) = (&class, asked, notifier) {
            asked = true;
            let target = resolve_grant_target(path, None, sandbox);
            let context = build_context(
                sandbox,
                &target,
                Some(GrantEvidence {
                    raw_path: Some(path.clone()),
                    pattern: Some(denial.operation.clone()),
                    line: Some(format!(
                        "Sandbox: {}({}) deny {} {}",
                        denial.process, denial.pid, denial.operation, denial.target
                    )),
                }),
                Some(tool),
                op_id,
                None,
                access.is_write(),
            );
            let (n, access, tool) = (Arc::clone(n), *access, tool.to_string());
            tokio::spawn(async move {
                let _ = n
                    .notify_violation_with(
                        &target,
                        access,
                        GrantReason::KernelRecord,
                        Some(tool),
                        context,
                    )
                    .await;
            });
        }
    }
    Some(KernelReport { lines, grant })
}

/// Whether output mentions a refusal at all, path or not.
fn looks_refused(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "not permitted",
        "permission denied",
        "read-only file system",
        "sandbox",
    ]
    .iter()
    .any(|w| lower.contains(w))
}

/// What [`kernel_denial_lines`] found.
#[derive(Debug, Clone, Default)]
pub struct KernelReport {
    /// One line per distinct denial, for the command's result.
    pub lines: Vec<String>,
    /// The first path a human may grant, and the access the kernel refused.
    pub grant: Option<(PathBuf, ScopeAccess)>,
}

/// One grant question, worked out from a failed command's output and ready to
/// ask: everything that needs the live sandbox is resolved up front, so asking
/// can happen after the command's result has gone back.
struct StderrDenialQuestion {
    target: PathBuf,
    access: ScopeAccess,
    tool: String,
    context: GrantContext,
}

impl StderrDenialQuestion {
    async fn ask(self, notifier: &dyn ScopeGrantNotifier) {
        let _ = notifier
            .notify_violation_with(
                &self.target,
                self.access,
                GrantReason::StderrHeuristic,
                Some(self.tool),
                self.context,
            )
            .await;
    }
}

/// The grant question a failed command's output calls for, if any: a denial
/// on a path outside the scope. A denial for a path already in scope (e.g. a
/// root-owned file inside the workspace) is unrelated to the sandbox.
fn stderr_denial_question(
    sandbox: &super::Sandbox,
    stderr: &str,
    stdout: &str,
    tool: &str,
    working_dir: Option<&Path>,
    op_id: Option<&str>,
) -> Option<StderrDenialQuestion> {
    // stdout too: a merged pipeline (`… 2>&1 | tail`) leaves stderr empty, and a
    // denial that disappears when a caller adds `2>&1` is a trap, not a feature.
    let hit = super::denial_scan::scan_denial_streams(stderr, stdout)?;
    let evidence = GrantEvidence {
        raw_path: Some(hit.path.clone()),
        pattern: Some(hit.pattern.to_string()),
        line: evidence_line(stderr, stdout, &hit.path),
    };
    let (in_scope, target) = {
        let scopes_guard = sandbox.scopes();
        let base_wd = working_dir.or_else(|| scopes_guard.first().map(|p| p.as_path()));
        let in_scope = if let Some(wd) = base_wd {
            sandbox.is_path_in_scope_in_dir(&hit.path, wd)
        } else {
            sandbox.is_path_in_scope(&hit.path)
        };
        let target = if !in_scope {
            resolve_grant_target(&hit.path, base_wd, sandbox)
        } else {
            PathBuf::new()
        };
        (in_scope, target)
    };
    // A path no grant can open is never asked about (R-PERM.4.3).
    if in_scope || ahma_common::scope_grant::refusal_reason(&target).is_some() {
        return None;
    }
    // The denial usually names a single cache file; offer its parent directory so
    // one grant covers the whole cache rather than re-prompting per file (P1c).
    // When the path was reached through a symlink to an out-of-scope tree (e.g. a
    // symlinked `target/` directory), offer the canonical external target root.
    let context = build_context(
        sandbox,
        &target,
        Some(evidence),
        Some(tool),
        op_id,
        None,
        hit.access.is_write(),
    );
    Some(StderrDenialQuestion {
        target,
        access: hit.access,
        tool: tool.to_string(),
        context,
    })
}

/// Resolve the candidate path from a denial into the directory that should actually
/// be offered for a grant.
///
/// Follows symlinks within the workspace (e.g. `target -> /shared/target`) to
/// recommend granting the external target root directly rather than an individual
/// non-existent leaf.
pub fn resolve_grant_target(
    path: &Path,
    working_dir: Option<&Path>,
    sandbox: &super::Sandbox,
) -> PathBuf {
    let scopes_guard = sandbox.scopes();
    let is_rooted = path.is_absolute() || path.has_root();
    let full_path = if is_rooted {
        path.to_path_buf()
    } else if let Some(wd) = working_dir {
        wd.join(path)
    } else if let Some(first_scope) = scopes_guard.first() {
        first_scope.join(path)
    } else {
        path.to_path_buf()
    };

    // Check if any ancestor (from full_path up to root) is a symlink pointing outside scope
    let mut current = full_path.as_path();
    while let Some(parent) = current.parent() {
        if parent.parent().is_none() {
            break;
        }
        if let Ok(meta) = std::fs::symlink_metadata(current)
            && meta.file_type().is_symlink()
            && let Ok(canon) = dunce::canonicalize(current)
            && !sandbox.is_path_allowed(&canon, &scopes_guard)
        {
            return canon;
        }
        current = parent;
    }

    if is_rooted {
        grant_dir_for(path)
    } else {
        grant_dir_for(&full_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::path::PathBuf;

    /// Records every delivered violation so wiring can be asserted.
    #[derive(Debug, Default)]
    struct RecordingNotifier {
        coordinator: Arc<GrantCoordinator>,
        seen: Mutex<Vec<(PathBuf, ScopeAccess, GrantReason)>>,
    }

    #[async_trait]
    impl ScopeGrantNotifier for RecordingNotifier {
        async fn notify_violation_with(
            &self,
            path: &Path,
            access: ScopeAccess,
            reason: GrantReason,
            tool: Option<String>,
            context: GrantContext,
        ) -> Option<ScopeGrantRequest> {
            // Exercise the same dedup the real notifiers use.
            let req = self
                .coordinator
                .begin_with_context(path, access, reason, tool, context)?;
            self.seen
                .lock()
                .push((req.path.clone(), req.access, req.reason));
            Some(req)
        }

        fn budget_exhausted(&self) -> bool {
            self.coordinator.budget_exhausted()
        }

        fn status(&self, decision_id: &str) -> GrantStatus {
            self.coordinator.status(decision_id)
        }
    }

    fn test_sandbox(scope: &Path) -> super::super::Sandbox {
        super::super::Sandbox::new(
            vec![scope.to_path_buf()],
            super::super::SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn pre_exec_notifies_with_offending_path_as_rw() {
        let scope = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(scope.path());
        let rec = Arc::new(RecordingNotifier::default());
        let notifier: Arc<dyn ScopeGrantNotifier> = rec.clone();
        let err: anyhow::Error = super::super::SandboxError::PathOutsideSandbox {
            path: PathBuf::from("/out/of/scope/dir"),
            scopes: vec![PathBuf::from("/ws")],
        }
        .into();
        notify_pre_exec(&sandbox, Some(&notifier), &err, "run_terminal_command").await;
        let seen = rec.seen.lock();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, PathBuf::from("/out/of/scope/dir"));
        assert_eq!(seen[0].1, ScopeAccess::Rw);
        assert_eq!(seen[0].2, GrantReason::PreExecViolation);
    }

    #[tokio::test]
    async fn pre_exec_ignores_unrelated_errors() {
        let scope = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(scope.path());
        let rec = Arc::new(RecordingNotifier::default());
        let notifier: Arc<dyn ScopeGrantNotifier> = rec.clone();
        let err = anyhow::anyhow!("some unrelated failure");
        notify_pre_exec(&sandbox, Some(&notifier), &err, "tool").await;
        assert!(rec.seen.lock().is_empty());
    }

    #[tokio::test]
    async fn stderr_denial_out_of_scope_notifies_and_preserves_scopes() {
        let scope = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(scope.path());
        let before: Vec<PathBuf> = sandbox.scopes().to_vec();

        let rec = Arc::new(RecordingNotifier::default());
        let notifier: Arc<dyn ScopeGrantNotifier> = rec.clone();
        let stderr =
            "error: failed to create directory `/opt/out/of/scope/cache`: Read-only file system";
        notify_stderr_denial(&sandbox, Some(&notifier), stderr, "", "sccache").await;

        let seen = rec.seen.lock();
        assert_eq!(seen.len(), 1, "an out-of-scope denial is offered");
        assert_eq!(seen[0].0, PathBuf::from("/opt/out/of/scope/cache"));
        assert_eq!(seen[0].1, ScopeAccess::Rw);
        assert_eq!(seen[0].2, GrantReason::StderrHeuristic);

        // Hard-constraint guard: detection must NEVER widen the live sandbox.
        assert_eq!(
            sandbox.scopes().to_vec(),
            before,
            "scanning stderr must not mutate live scopes"
        );
    }

    #[tokio::test]
    async fn stderr_denial_for_in_scope_path_is_ignored() {
        let scope = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(scope.path());
        let rec = Arc::new(RecordingNotifier::default());
        let notifier: Arc<dyn ScopeGrantNotifier> = rec.clone();
        // A permission error for a path that is already inside the scope is not a
        // scope problem — do not offer to grant it.
        let in_scope = scope.path().join("locked.txt");
        let stderr = format!("cat: {}: Permission denied", in_scope.display());
        notify_stderr_denial(&sandbox, Some(&notifier), &stderr, "", "cat").await;
        assert!(
            rec.seen.lock().is_empty(),
            "in-scope denials must not raise a grant prompt"
        );
    }

    #[tokio::test]
    async fn no_notifier_is_a_noop() {
        let scope = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(scope.path());
        // Simply must not panic with notifier = None.
        notify_stderr_denial(&sandbox, None, "/x: Permission denied", "", "t").await;
        let err = anyhow::anyhow!("x");
        notify_pre_exec(&sandbox, None, &err, "t").await;
    }

    // ── P1c: parent-directory suggestion ──────────────────────────────────────

    #[test]
    fn grant_dir_for_existing_file_returns_parent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache.bin");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(grant_dir_for(&file), dir.path());
    }

    #[test]
    fn grant_dir_for_existing_dir_returns_self() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(grant_dir_for(dir.path()), dir.path());
    }

    #[test]
    fn grant_dir_for_nonexistent_file_like_path_returns_parent() {
        // A cache file about to be created: extension ⇒ treat as file.
        let p = Path::new("/home/u/.cache/sccache/0/abc.o");
        assert_eq!(grant_dir_for(p), Path::new("/home/u/.cache/sccache/0"));
    }

    #[test]
    fn grant_dir_for_nonexistent_dir_like_path_returns_self() {
        // No extension ⇒ treat as a directory to create; offer it directly.
        let p = Path::new("/home/u/.cache/sccache/shard0");
        assert_eq!(grant_dir_for(p), p);
    }

    #[test]
    fn grant_dir_for_never_suggests_filesystem_root() {
        // A file directly under root must not collapse the suggestion to `/`.
        let p = Path::new("/lonely.bin");
        assert_eq!(grant_dir_for(p), p);
    }

    #[tokio::test]
    async fn stderr_denial_offers_parent_dir_for_a_file() {
        let scope = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(scope.path());
        let rec = Arc::new(RecordingNotifier::default());
        let notifier: Arc<dyn ScopeGrantNotifier> = rec.clone();
        // A write denial naming a specific out-of-scope cache *file*.
        let stderr =
            "error writing `/opt/ext/sccache/0/object.o`: Operation not permitted (os error 1)";
        notify_stderr_denial(&sandbox, Some(&notifier), stderr, "", "sccache").await;

        let seen = rec.seen.lock();
        assert_eq!(
            seen[0].0,
            PathBuf::from("/opt/ext/sccache/0"),
            "the cache file's parent dir is offered, not the leaf file"
        );
    }

    #[tokio::test]
    async fn stderr_denial_symlinked_target_notifies_external_target_dir() {
        let ws = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();

        let ws_canon = dunce::canonicalize(ws.path()).unwrap();
        let ext_canon = dunce::canonicalize(ext.path()).unwrap();

        let target_symlink = ws_canon.join("target");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&ext_canon, &target_symlink).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&ext_canon, &target_symlink).unwrap();

        let sandbox = test_sandbox(&ws_canon);
        let rec = Arc::new(RecordingNotifier::default());
        let notifier: Arc<dyn ScopeGrantNotifier> = rec.clone();

        let stderr = "\
error: failed to create directory 'target/debug'

Caused by:
  Operation not permitted (os error 1)";

        notify_stderr_denial(&sandbox, Some(&notifier), stderr, "", "cargo").await;

        let seen = rec.seen.lock();
        assert_eq!(seen.len(), 1, "out-of-scope symlink denial must notify");
        assert_eq!(seen[0].0, ext_canon);
        assert_eq!(seen[0].1, ScopeAccess::Rw);
        assert_eq!(seen[0].2, GrantReason::StderrHeuristic);
    }
}

#[cfg(test)]
mod grant_dir_gate_tests {
    use super::grant_dir_for;

    /// A denial on a file directly under `$HOME` (or inside a credential
    /// directory) must not collapse the suggestion to `$HOME`/`~/.ssh`: the
    /// prompt would then be asking the human to open everything, and a session
    /// answer would apply it. Offer the file itself instead.
    #[test]
    fn grant_dir_for_never_offers_home_or_a_denylisted_dir() {
        let home = ahma_common::config::ahma_home_dir().expect("home dir");
        let file = home.join("ahma-grant-gate-test.txt");
        assert_eq!(grant_dir_for(&file), file, "$HOME is never the suggestion");
        let key = home.join(".ssh").join("ahma-grant-gate-test.pub");
        assert_eq!(grant_dir_for(&key), key, "~/.ssh is never the suggestion");
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;

    /// A command that succeeded although the sandbox refused one of its writes
    /// gets one line naming the path and the grant, not the blocking panel
    /// (bug report: a build's lock-holder write printed the full panel inside
    /// every successful build).
    #[test]
    fn a_refused_write_in_a_command_that_succeeded_is_one_line() {
        let ws = tempfile::tempdir().unwrap();
        let who = HookRequester {
            harness: None,
            session_id: None,
            scopes: vec![ws.path().to_path_buf()],
            command: None,
        };
        let cache = tempfile::tempdir().unwrap();
        let holder = cache.path().join("heavy.lock.holder");
        let out = format!(
            "heavy: line 1: {}: Operation not permitted\nBUILD SUCCESSFUL",
            holder.display()
        );
        let note = hook_side_refusal_note(&out, &who).expect("a note");
        assert_eq!(note.lines().count(), 1, "{note}");
        assert!(note.contains(&holder.display().to_string()), "{note}");
        assert!(
            note.contains(&format!("ahma sandbox grant {}", cache.path().display())),
            "{note}"
        );
        assert!(hook_side_refusal_note("BUILD SUCCESSFUL", &who).is_none());
        let inside = format!("x: {}/f: Operation not permitted", ws.path().display());
        assert!(
            hook_side_refusal_note(&inside, &who).is_none(),
            "a refusal inside the scope is not the sandbox's"
        );
    }

    /// The one-line note fires on a refusal, not on text that mentions one:
    /// a WebKit sandbox-extension failure for a system font, or source code
    /// listed by `grep` whose comments say "operation not permitted" next to
    /// a `//`, produced grant suggestions for `/System/…` and `//`.
    #[test]
    fn the_one_line_note_ignores_what_is_not_a_refused_access() {
        let ws = tempfile::tempdir().unwrap();
        let who = HookRequester {
            harness: None,
            session_id: None,
            scopes: vec![ws.path().to_path_buf()],
            command: None,
        };
        for out in [
            "sandbox_extension_issue_file failed for /System/Library/AssetsV2/com_apple_MobileAsset_Font7: 1 (Operation not permitted)",
            "src/x.rs:12:    // operation not permitted: the path // is never granted",
            "cat: /usr/libexec/secret: Operation not permitted",
            "ls: /: Operation not permitted",
            // A path on one line and an unrelated refusal on the next.
            "SSH_AUTH_SOCK=/var/run/com.apple.launchd.2sJPtUp0xm/Listeners\ngit@github.com: Permission denied (publickey).",
        ] {
            assert!(hook_side_refusal_note(out, &who).is_none(), "{out}");
        }
    }

    /// Text that quotes a refusal is not one. `grep`, `cat` or `sed` over source,
    /// docs or logs printed "the sandbox refused a write" with a grant for a
    /// path that only appeared in a test fixture, and recorded it as a question
    /// for Claude Code's dialog.
    #[test]
    fn quoted_refusals_in_listed_text_are_not_refusals() {
        let ws = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let who = HookRequester {
            harness: None,
            session_id: None,
            scopes: vec![ws.path().to_path_buf()],
            command: None,
        };
        let real = elsewhere.path().join("x.lock");
        for out in [
            // A fixture path that exists nowhere.
            "let out = \"/Users/u/Library/Caches/sccache/0/1/obj: Operation not permitted\";"
                .to_string(),
            // `grep -n` output: `file:line:` then the quoted text, even when the
            // quoted path is real.
            format!(
                "src/x.rs:877:        notify(\"{}: Operation not permitted\");",
                real.display()
            ),
            format!("docs/a.md:12:{}: Permission denied", real.display()),
        ] {
            assert!(hook_side_refusal_note(&out, &who).is_none(), "{out}");
        }
    }

    /// A path no grant can ever open (credentials, ahma's own settings) gets
    /// one honest statement, never grant commands or an `always` settings
    /// line (SPEC R-PERM.4.3, R-PERM.9).
    #[test]
    fn a_never_grantable_path_offers_no_grant() {
        let home = ahma_common::config::ahma_home_dir().expect("home");
        let key = home.join(".ssh").join("id_ed25519");
        let ws = tempfile::tempdir().unwrap();
        let who = HookRequester {
            harness: Some("Claude Code".into()),
            session_id: None,
            scopes: vec![ws.path().to_path_buf()],
            command: Some("head ~/.ssh/id_ed25519".into()),
        };
        for t in [
            hook_denial_text(&key, ScopeAccess::Ro, "Operation not permitted", &who),
            runtime_denial_remediation(&key, ScopeAccess::Ro),
            runtime_denial_remediation_cli(&key, ScopeAccess::Ro),
        ] {
            assert!(
                t.starts_with("Blocked, and no grant can change it:"),
                "one first line that says nothing can be done here: {t}"
            );
            assert!(t.contains("a read of"), "{t}");
            for never in ["sandbox grant", "If you choose always", "pick a tier"] {
                assert!(
                    !t.contains(never),
                    "offers {never:?} for a refused path: {t}"
                );
            }
            assert!(
                t.contains("ssh-add"),
                "says how ssh can still use the key: {t}"
            );
            assert!(
                !t.contains("still work"),
                "never claims ssh works: it does not when the agent is empty: {t}"
            );
        }
    }

    #[test]
    fn hook_denial_text_leads_with_blocked_and_has_every_section() {
        let ws = tempfile::tempdir().unwrap();
        let who = HookRequester {
            harness: Some("Claude Code".into()),
            session_id: Some("8d387500-2ff3-4b2e".into()),
            scopes: vec![ws.path().to_path_buf()],
            command: Some("cargo build --release".into()),
        };
        let t = hook_denial_text(
            Path::new("/opt/cache/x.bin"),
            ScopeAccess::Rw,
            "write to /opt/cache/x.bin: Operation not permitted",
            &who,
        );
        // What was refused, and that a human must act if the command needs it;
        // not "blocked": the command may have failed for another reason.
        assert!(
            t.starts_with("The sandbox refused a write outside the workspace: '/opt/cache/x.bin'"),
            "{t}"
        );
        for h in [
            "Who is asking",
            "What was blocked",
            "What the agent says it needs",
            "Minimum that would work",
            "What a grant allows",
            "Risk",
            "If you choose always",
        ] {
            assert!(t.contains(h), "missing {h}: {t}");
        }
        // SPEC R-PERM.3.4: who asked, and for which command, in the terminal too.
        for needle in [
            "Claude Code (terminal hook)",
            "session 8d387500",
            &ws.path().display().to_string(),
            "command: cargo build --release",
        ] {
            assert!(t.contains(needle), "missing {needle:?}: {t}");
        }
        assert!(!t.contains("unknown session"), "{t}");
        assert!(!t.contains("not assessed"), "the risk is assessed: {t}");
        assert!(
            !t.contains("[n]"),
            "no TUI keys in a terminal; the commands are the choices: {t}"
        );
        assert!(t.contains("ahma sandbox grant /opt/cache --session"), "{t}");
    }

    #[test]
    fn command_lines_are_redacted_for_the_prompt() {
        assert_eq!(
            redact_command("GITHUB_TOKEN=abc cargo publish --token xyz"),
            "GITHUB_TOKEN=*** cargo publish --token ***"
        );
        assert_eq!(redact_command("cargo test -p stat3"), "cargo test -p stat3");
    }

    const GOLDEN_HOOK_HEAD: &str = "The sandbox refused a write outside the workspace: \
        '/opt/cache/x.bin'. If this command needs it, a human must grant it; ahma will not run it \
        unsandboxed.";
    const GOLDEN_HOOK_TAIL: &str = r#"One thing to do (pick a tier), then re-run the command:
  ahma sandbox grant /opt/cache --session   # this terminal session only, at most 12h
  ahma sandbox grant /opt/cache             # until revoked, bound to this workspace

Either applies on your next command; nothing to restart."#;

    #[test]
    fn golden_hook_denial_frame() {
        let who = HookRequester {
            harness: Some("Claude Code".into()),
            session_id: Some("8d387500-2ff3-4b2e".into()),
            scopes: vec![PathBuf::from("/home/u/proj")],
            command: Some("cargo build --release".into()),
        };
        let path = Path::new("/opt/cache/x.bin");
        let details = "write to /opt/cache/x.bin: Operation not permitted";
        let target = grant_dir_for(path);
        assert_eq!(
            target,
            Path::new("/opt/cache"),
            "precondition: a missing file with an extension is offered as its directory"
        );
        let body = ahma_common::grant_prompt::render_for_hook(
            &target,
            ScopeAccess::Rw,
            details,
            who.context(&target, true),
        )
        .to_message();
        let want = format!("{GOLDEN_HOOK_HEAD}\n\n{body}\n{GOLDEN_HOOK_TAIL}");
        let got = hook_denial_text(path, ScopeAccess::Rw, details, &who);
        assert_eq!(got, want, "the hook's denial text changed:\n{got}");
        for o in ahma_common::grant_prompt::options() {
            let key = format!("[{}]", o.key);
            assert!(
                !got.contains(&key),
                "TUI key {key} in a terminal hook's text"
            );
        }
    }

    // ------------------------------------------------------------ END PART 3 ---
}
