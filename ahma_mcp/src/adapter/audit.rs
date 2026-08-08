//! Append-only execution audit log for the default (non-vault) execution path.
//!
//! `ahma_vault`'s audit log answers "what happened inside this task vault?".  Its
//! problem is that a vault is the *rare* case: the overwhelmingly common path is a
//! plain `run_terminal_command`, which until now left behind operation *output*
//! (`<project log dir>/operations/<op>.log`) and some `tracing` lifecycle lines —
//! and output is not provenance.  It tells you what a command printed, not that
//! the command happened, when, in which directory, what it was, or what it wrote
//! that something else will later execute.
//!
//! This module records that, for every execution path, as newline-delimited JSON
//! at:
//!
//! ```text
//! <project log dir>/audit.jsonl
//! ```
//!
//! i.e. the sibling of `operations/`, so it inherits the same per-project
//! isolation ([`crate::utils::logging::project_log_dir`]) and the same "logs live
//! here" disclosure.
//!
//! ## Wire format — deliberately the vault's
//!
//! Each line is one JSON object shaped exactly like
//! [`crate::vault::audit::AuditEvent`]: a `timestamp`, an optional `trace_id`, and
//! a flattened, `type`-tagged payload.  `tool_call` and `tool_complete` carry the
//! vault's field names and add optional fields the vault does not have
//! (`working_dir`, `command`, `exit_code`).  Unknown fields are ignored by serde,
//! so a single reader parses both logs and a vault-era tool keeps working.  That
//! compatibility is not a convention to be remembered — it is asserted by
//! `tool_call_line_parses_as_a_vault_audit_event` below.
//!
//! ## Append-only, and why there is no mutex
//!
//! Every event is one `open(O_APPEND)` → one `write_all` → `flush` → close.  There
//! is no shared file handle and therefore no lock: `O_APPEND` (and
//! `FILE_APPEND_DATA` on Windows) makes the seek-to-end and the write a single
//! atomic step against the inode, so concurrent operations cannot interleave
//! *within* a write.  Lines are kept small ([`MAX_ARGS_SUMMARY_CHARS`],
//! [`MAX_COMMAND_CHARS`]) precisely so each event is one `write` syscall — that is
//! what makes "one complete JSON line per event" true under concurrency.
//!
//! Serialising every command behind a process-wide mutex would have been the easy
//! version and the wrong one: it puts a contended lock on the hot path of every
//! execution to buy an ordering guarantee the kernel already provides.
//!
//! ## Failure is loud, never fatal
//!
//! [`AuditLog::record`] cannot fail an operation.  A write error is reported at
//! `warn` with the path — an audit trail that stops silently is worse than one
//! that was never there, because it is still believed.
//!
//! ## Redaction
//!
//! Argument summaries and command strings go through
//! [`crate::log_monitor::redact_sensitive_line`] — the same function operation
//! output is redacted with before it is spilled.  One redaction standard, not two.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt as _;

/// File name of the execution audit log inside the project log directory.
pub const AUDIT_LOG_FILE_NAME: &str = "audit.jsonl";

/// Upper bound on a recorded argument summary.
///
/// Bounds the JSONL line so one event is one `write` syscall (see the module
/// docs on why that is what makes concurrent appends non-interleaving).
pub const MAX_ARGS_SUMMARY_CHARS: usize = 1_024;

/// Upper bound on a recorded command string.  Same reasoning as
/// [`MAX_ARGS_SUMMARY_CHARS`].
pub const MAX_COMMAND_CHARS: usize = 2_048;

/// Upper bound on a recorded working directory.
///
/// A path is *usually* short, which is exactly why it is easy to leave unbounded
/// and wrong: the bound is not there for the common case.  `PATH_MAX` is not a
/// bound on this string — the path is a validated in-scope directory, but it is
/// still attacker-influenced (an agent may `mkdir` deep inside the workspace),
/// and JSON escaping can inflate it further.  Every field on the line is bounded
/// or the "one event is one `write` syscall" invariant is only bounded in
/// aggregate by luck.
pub const MAX_WORKING_DIR_CHARS: usize = 1_024;

// ─────────────────────────────────────────────────────────────────────────────
// Events
// ─────────────────────────────────────────────────────────────────────────────

/// A single timestamped entry in the execution audit log.
///
/// Field-for-field the vault's [`crate::vault::audit::AuditEvent`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// UTC timestamp (RFC 3339).
    pub timestamp: String,
    /// Optional OTel trace ID for correlation.  Left `None` for now, exactly as
    /// the vault writer does, so the two logs stay field-compatible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// The event payload.
    #[serde(flatten)]
    pub kind: AuditEventKind,
}

/// Every kind of event the default execution path records.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditEventKind {
    /// A command was dispatched.  **Written before the process is spawned**, so a
    /// crash, a kill, a timeout, or a hang still leaves the record of what was
    /// asked for.  A `tool_call` with no matching `tool_complete` is meaningful
    /// evidence, not a gap.
    ToolCall {
        operation_id: String,
        tool_name: String,
        /// Redacted, length-bounded summary of the arguments.
        args_summary: String,
        /// Resolved, sandbox-validated working directory.
        #[serde(skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
        /// The command as it will actually run (program + argv), redacted.
        #[serde(skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },

    /// A command finished.  Written on every terminal path — exit, failure,
    /// timeout, cancellation.
    ToolComplete {
        operation_id: String,
        success: bool,
        duration_ms: u64,
        /// Process exit code where one exists (absent for a kill/timeout).
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// Terminal status name (`completed`, `failed`, `timed_out`, `cancelled`).
        #[serde(skip_serializing_if = "Option::is_none")]
        outcome: Option<String>,
    },

    /// The agent wrote a file that a component *outside* the sandbox will execute
    /// on a later trigger (SPEC R-HANDOFF, `Disclose` tier).
    ///
    /// This is the highest-value entry in the log.  The tool-result warning that
    /// accompanies it is transient — it scrolls away, and the whole point of the
    /// trust-handoff shape is that the execution happens *later*, when nobody is
    /// looking at the transcript any more.  Recording it here is what turns a
    /// warning into provenance.
    TrustHandoffDisclosure {
        /// Workspace-relative path that was written.
        path: String,
        /// What will execute it, and when.
        trigger: String,
        /// Which write tool produced it.
        tool_name: String,
    },

    /// The sandbox refused a path at runtime (SPEC R5.4.7).  Mirrors the
    /// structured `sandbox_denial` payload the MCP boundary returns.
    SandboxDenial {
        #[serde(skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
        /// The out-of-scope path the command was denied.
        path: String,
        /// `read` / `write` / … as classified by the denial scanner.
        access: String,
        tool_name: String,
    },
}

/// Terminal outcome label recorded on a [`AuditEventKind::ToolComplete`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The process exited zero.
    Completed,
    /// The process exited non-zero, or could not be spawned.
    Failed,
    /// The operation exceeded its timeout and was killed.
    TimedOut,
    /// The operation was cancelled before or during execution.
    Cancelled,
}

impl Outcome {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::Failed => "failed",
            Outcome::TimedOut => "timed_out",
            Outcome::Cancelled => "cancelled",
        }
    }

    /// Whether this outcome counts as success.
    pub fn is_success(self) -> bool {
        matches!(self, Outcome::Completed)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Redaction + summarisation
// ─────────────────────────────────────────────────────────────────────────────

/// Truncate on a character boundary, marking that it happened.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}… [truncated]")
}

/// Redact a value the same way operation output is redacted, then bound it.
///
/// The redaction rules are line-oriented, so the input is flattened to a single
/// line first — a multi-line argument (a heredoc, a patch) must not be able to
/// smuggle a secret past a rule by putting it on line two, nor break the JSONL
/// invariant of one line per event.
fn redact_and_bound(raw: &str, max: usize) -> String {
    let single_line = raw.replace(['\n', '\r'], " ");
    let redacted = crate::log_monitor::redact_sensitive_line(&single_line);
    truncate_chars(&redacted, max)
}

/// Build the redacted `args_summary` for a tool call from its JSON arguments.
///
/// Keys are kept (they are the useful part: *which* parameters were supplied);
/// values go through the standard redaction. Serialising the whole map first and
/// redacting the result means a secret is caught wherever it sits — in a value,
/// in a nested object, or in a free-form command string.
pub fn args_summary(args: Option<&Map<String, Value>>) -> String {
    let Some(args) = args else {
        return String::new();
    };
    let rendered =
        serde_json::to_string(args).unwrap_or_else(|_| "<unserializable arguments>".to_string());
    redact_and_bound(&rendered, MAX_ARGS_SUMMARY_CHARS)
}

/// Build the redacted "command as it will actually run" string from the resolved
/// program and argv — after subcommand aliasing and argument construction, so the
/// log shows what was executed rather than what was requested.
pub fn command_line(program: &str, argv: &[String]) -> String {
    let mut rendered = String::from(program);
    for a in argv {
        rendered.push(' ');
        rendered.push_str(a);
    }
    redact_and_bound(&rendered, MAX_COMMAND_CHARS)
}

// ─────────────────────────────────────────────────────────────────────────────
// Writer
// ─────────────────────────────────────────────────────────────────────────────

/// Appends [`AuditEvent`]s to an append-only JSONL file.
///
/// Cloning is cheap and safe — clones share only the path, never a file handle,
/// which is what keeps the writer lock-free (see the module docs).
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// Create a writer for an explicit path.  Used by tests and by anything that
    /// needs an audit log somewhere other than the project log directory.
    pub fn at(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// Path of the underlying log file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event, surfacing I/O errors to the caller.
    ///
    /// Prefer [`Self::record`] on any execution path: an audit write must never
    /// be able to fail a command.
    pub async fn emit(&self, kind: AuditEventKind) -> Result<()> {
        let event = AuditEvent {
            timestamp: Utc::now().to_rfc3339(),
            trace_id: None,
            kind,
        };
        let mut line = serde_json::to_string(&event)?;
        line.push('\n');

        // Try the open first and only pay for `create_dir_all` when it fails.
        // The directory is missing at most once per process, and this sits on the
        // hot path of every command — creating it eagerly would spend two
        // syscalls on every event to save one on the first.
        let mut file = match Self::open_for_append(&self.path).await {
            Ok(file) => file,
            Err(first_err) => {
                let Some(parent) = self.path.parent() else {
                    return Err(first_err.into());
                };
                tokio::fs::create_dir_all(parent).await?;
                Self::open_for_append(&self.path).await?
            }
        };
        // One `write_all` for the whole line, including the terminator: this is
        // the call whose atomicity under `O_APPEND` keeps concurrent operations
        // from interleaving mid-line.
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    async fn open_for_append(path: &Path) -> std::io::Result<tokio::fs::File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
    }

    /// Append one event, degrading gracefully.
    ///
    /// Never returns an error and never panics.  A failure is reported at `warn`
    /// with the log path so a silently-stopped audit trail is impossible to
    /// mistake for an empty one.
    pub async fn record(&self, kind: AuditEventKind) {
        if let Err(e) = self.emit(kind).await {
            tracing::warn!(
                audit_log = %self.path.display(),
                error = %e,
                "audit event could not be written; execution continues but this \
                 command will be missing from the audit trail"
            );
        }
    }
}

/// Path of the process-wide execution audit log:
/// `<project log dir>/audit.jsonl`.
pub fn audit_log_path() -> PathBuf {
    crate::utils::logging::project_log_dir().join(AUDIT_LOG_FILE_NAME)
}

static AUDIT_LOG: OnceLock<AuditLog> = OnceLock::new();

/// Point the process-wide audit log at an explicit path.
///
/// Returns `false` if the log was already resolved, in which case the call had no
/// effect.  Exists so tests can redirect the log into a `tempfile::tempdir()`
/// without writing into the project tree; production resolves it lazily from
/// [`audit_log_path`].
pub fn set_audit_log_path(path: PathBuf) -> bool {
    AUDIT_LOG.set(AuditLog::at(path)).is_ok()
}

/// The process-wide execution audit log.
pub fn audit_log() -> &'static AuditLog {
    AUDIT_LOG.get_or_init(|| AuditLog::at(audit_log_path()))
}

/// Record an event on the process-wide audit log, degrading gracefully.
pub async fn record(kind: AuditEventKind) {
    audit_log().record(kind).await;
}

/// Record the trust-handoff disclosure for a write that landed (SPEC R-HANDOFF).
pub async fn record_trust_handoff(path: &str, trigger: &str, tool_name: &str) {
    record(AuditEventKind::TrustHandoffDisclosure {
        path: redact_and_bound(path, MAX_ARGS_SUMMARY_CHARS),
        trigger: redact_and_bound(trigger, MAX_ARGS_SUMMARY_CHARS),
        tool_name: tool_name.to_string(),
    })
    .await;
}

/// Record a runtime sandbox denial (SPEC R5.4.7).
pub async fn record_sandbox_denial(
    operation_id: Option<&str>,
    path: &Path,
    access: &str,
    tool_name: &str,
) {
    record(AuditEventKind::SandboxDenial {
        operation_id: operation_id.map(str::to_string),
        path: redact_and_bound(&path.display().to_string(), MAX_ARGS_SUMMARY_CHARS),
        access: access.to_string(),
        tool_name: tool_name.to_string(),
    })
    .await;
}

/// Record a `tool_call` **before** the process is spawned.
pub async fn record_tool_call(
    operation_id: &str,
    tool_name: &str,
    args: Option<&Map<String, Value>>,
    working_dir: &str,
    program: &str,
    argv: &[String],
) {
    record(AuditEventKind::ToolCall {
        operation_id: operation_id.to_string(),
        tool_name: tool_name.to_string(),
        args_summary: args_summary(args),
        working_dir: Some(redact_and_bound(working_dir, MAX_WORKING_DIR_CHARS)),
        command: Some(command_line(program, argv)),
    })
    .await;
}

/// Record the matching `tool_complete`.
pub async fn record_tool_complete(
    operation_id: &str,
    outcome: Outcome,
    duration_ms: u64,
    exit_code: Option<i32>,
) {
    record(AuditEventKind::ToolComplete {
        operation_id: operation_id.to_string(),
        success: outcome.is_success(),
        duration_ms,
        exit_code,
        outcome: Some(outcome.as_str().to_string()),
    })
    .await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_lines(contents: &str) -> Vec<Value> {
        contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("every audit line must be complete JSON"))
            .collect()
    }

    #[tokio::test]
    async fn emit_appends_one_json_line_per_event() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(tmp.path().join("audit.jsonl"));

        log.emit(AuditEventKind::ToolCall {
            operation_id: "op_1".into(),
            tool_name: "run_terminal_command".into(),
            args_summary: "{\"command\":\"echo hi\"}".into(),
            working_dir: Some("/w".into()),
            command: Some("sh -c echo hi".into()),
        })
        .await
        .unwrap();
        log.emit(AuditEventKind::ToolComplete {
            operation_id: "op_1".into(),
            success: true,
            duration_ms: 12,
            exit_code: Some(0),
            outcome: Some("completed".into()),
        })
        .await
        .unwrap();

        let contents = tokio::fs::read_to_string(log.path()).await.unwrap();
        let events = parse_lines(&contents);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "tool_call");
        assert_eq!(events[0]["operation_id"], "op_1");
        assert_eq!(events[0]["working_dir"], "/w");
        assert_eq!(events[1]["type"], "tool_complete");
        assert_eq!(events[1]["exit_code"], 0);
        assert!(
            events[0]["timestamp"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "every event is timestamped"
        );
    }

    /// The whole reason this module reuses the vault's shape rather than
    /// inventing one: a reader written against the vault log must parse this log
    /// too.  Asserted, not merely intended.
    #[tokio::test]
    async fn tool_call_line_parses_as_a_vault_audit_event() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(tmp.path().join("audit.jsonl"));
        log.emit(AuditEventKind::ToolCall {
            operation_id: "op_7".into(),
            tool_name: "cargo".into(),
            args_summary: "--release".into(),
            working_dir: Some("/w".into()),
            command: Some("cargo build --release".into()),
        })
        .await
        .unwrap();
        log.emit(AuditEventKind::ToolComplete {
            operation_id: "op_7".into(),
            success: false,
            duration_ms: 9,
            exit_code: Some(101),
            outcome: Some("failed".into()),
        })
        .await
        .unwrap();

        let contents = tokio::fs::read_to_string(log.path()).await.unwrap();
        let lines: Vec<&str> = contents.lines().collect();

        let call: crate::vault::audit::AuditEvent = serde_json::from_str(lines[0])
            .expect("the execution log's tool_call must deserialize as the vault's event");
        match call.kind {
            crate::vault::audit::AuditEventKind::ToolCall {
                operation_id,
                tool_name,
                args_summary,
            } => {
                assert_eq!(operation_id, "op_7");
                assert_eq!(tool_name, "cargo");
                assert_eq!(args_summary, "--release");
            }
            other => panic!("expected a vault ToolCall, got {other:?}"),
        }

        let done: crate::vault::audit::AuditEvent = serde_json::from_str(lines[1])
            .expect("the execution log's tool_complete must deserialize as the vault's event");
        match done.kind {
            crate::vault::audit::AuditEventKind::ToolComplete {
                operation_id,
                success,
                duration_ms,
            } => {
                assert_eq!(operation_id, "op_7");
                assert!(!success);
                assert_eq!(duration_ms, 9);
            }
            other => panic!("expected a vault ToolComplete, got {other:?}"),
        }
    }

    #[test]
    fn args_summary_redacts_secrets_with_the_standard_rules() {
        let args = json!({
            "command": "deploy --token=ghp_abcdefghijklmnopqrstuvwxyz012345",
            "env": "ANTHROPIC_API_KEY=sk-ant-api03-notarealkeyvalue1234",
        });
        let map = args.as_object().unwrap().clone();
        let summary = args_summary(Some(&map));

        assert!(
            !summary.contains("ghp_abcdefghijklmnopqrstuvwxyz012345"),
            "github token must not survive into the audit log: {summary}"
        );
        assert!(
            !summary.contains("sk-ant-api03-notarealkeyvalue1234"),
            "provider key must not survive into the audit log: {summary}"
        );
        assert!(
            summary.contains("[REDACTED]"),
            "redaction must be visible, not silent: {summary}"
        );
        assert!(
            summary.contains("deploy"),
            "the non-secret part must survive so the record is still useful: {summary}"
        );
    }

    #[test]
    fn command_line_redacts_and_flattens() {
        let rendered = command_line(
            "sh",
            &[
                "-c".to_string(),
                "curl -H 'Authorization: Bearer abcdefghijklmnop'\nwhoami".to_string(),
            ],
        );
        assert!(!rendered.contains("abcdefghijklmnop"), "{rendered}");
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
        assert!(
            !rendered.contains('\n'),
            "a command must never break the one-line-per-event invariant: {rendered}"
        );
    }

    #[test]
    fn long_values_are_bounded_so_a_line_stays_one_write() {
        let huge = "a".repeat(MAX_COMMAND_CHARS * 4);
        let rendered = command_line("sh", &[huge]);
        assert!(rendered.chars().count() <= MAX_COMMAND_CHARS + 16);
        assert!(rendered.ends_with("[truncated]"), "{rendered}");

        let map = json!({ "blob": "b".repeat(MAX_ARGS_SUMMARY_CHARS * 4) })
            .as_object()
            .unwrap()
            .clone();
        let summary = args_summary(Some(&map));
        assert!(summary.chars().count() <= MAX_ARGS_SUMMARY_CHARS + 16);
    }

    /// `working_dir` is bounded like every other free-form field. It is the field
    /// most likely to be assumed short — and an agent can `mkdir -p` arbitrarily
    /// deep inside its own workspace, so "short" is a habit, not a guarantee.
    #[tokio::test]
    async fn a_pathological_working_dir_is_bounded_like_every_other_field() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("audit.jsonl");
        let deep = format!("/{}", vec!["nested"; 4_000].join("/"));
        assert!(deep.chars().count() > MAX_WORKING_DIR_CHARS);

        AuditLog::at(path.clone())
            .record(AuditEventKind::ToolCall {
                operation_id: "op".into(),
                tool_name: "run_terminal_command".into(),
                args_summary: String::new(),
                working_dir: Some(redact_and_bound(&deep, MAX_WORKING_DIR_CHARS)),
                command: Some("true".into()),
            })
            .await;

        let line = tokio::fs::read_to_string(&path).await.unwrap();
        let event: Value = serde_json::from_str(line.trim()).unwrap();
        let wd = event["working_dir"].as_str().unwrap();
        assert!(wd.chars().count() <= MAX_WORKING_DIR_CHARS + 16, "{wd}");
        assert!(wd.ends_with("[truncated]"), "{wd}");
    }

    #[test]
    fn args_summary_of_no_arguments_is_empty() {
        assert_eq!(args_summary(None), "");
    }

    /// A broken audit destination must cost the operation nothing.  `record` is
    /// the only entry point execution paths use, and it swallows the error after
    /// reporting it.
    #[tokio::test]
    async fn record_degrades_gracefully_when_the_log_cannot_be_written() {
        let tmp = tempfile::tempdir().unwrap();
        // A *file* where the parent directory has to be — `create_dir_all` and the
        // subsequent open both fail, on every platform.
        let blocker = tmp.path().join("blocked");
        tokio::fs::write(&blocker, b"not a directory")
            .await
            .unwrap();
        let log = AuditLog::at(blocker.join("nested").join("audit.jsonl"));

        assert!(
            log.emit(sample_event()).await.is_err(),
            "emit surfaces the error"
        );
        // …and record does not: it returns normally, so the operation continues.
        log.record(sample_event()).await;
    }

    fn sample_event() -> AuditEventKind {
        AuditEventKind::ToolComplete {
            operation_id: "op_sample".into(),
            success: true,
            duration_ms: 1,
            exit_code: Some(0),
            outcome: Some("completed".into()),
        }
    }

    /// The audit log lives next to `operations/` so it inherits the same
    /// per-project isolation — but it must *not* inherit the retention sweep.
    ///
    /// `cleanup_old_logs` deletes managed rolling logs older than 24h and
    /// `cleanup_old_spills_once` prunes `operations/`.  Neither may touch
    /// `audit.jsonl`: a trail that silently deletes its own oldest entries is not
    /// an audit trail, it is a log.  If someone later broadens the retention
    /// predicate, this fires.
    #[test]
    fn the_audit_log_is_not_swept_by_log_retention() {
        assert!(
            !crate::utils::logging::is_managed_log_file(AUDIT_LOG_FILE_NAME),
            "{AUDIT_LOG_FILE_NAME} must not be eligible for retention pruning"
        );
        assert_eq!(
            audit_log_path().parent(),
            Some(crate::utils::logging::project_log_dir().as_path()),
            "the audit log is a sibling of operations/, not inside it"
        );
    }

    #[test]
    fn outcome_names_and_success_mapping() {
        assert_eq!(Outcome::Completed.as_str(), "completed");
        assert_eq!(Outcome::Failed.as_str(), "failed");
        assert_eq!(Outcome::TimedOut.as_str(), "timed_out");
        assert_eq!(Outcome::Cancelled.as_str(), "cancelled");
        assert!(Outcome::Completed.is_success());
        for o in [Outcome::Failed, Outcome::TimedOut, Outcome::Cancelled] {
            assert!(!o.is_success(), "{o:?} is not success");
        }
    }

    /// Concurrent operations append to one file with no lock between them; every
    /// line must still be a single complete JSON object.
    #[tokio::test]
    async fn concurrent_appends_never_interleave_within_a_line() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(tmp.path().join("audit.jsonl"));
        let mut tasks = Vec::new();
        for op in 0..24u32 {
            let log = log.clone();
            tasks.push(tokio::spawn(async move {
                let id = format!("op_{op}");
                log.record(AuditEventKind::ToolCall {
                    operation_id: id.clone(),
                    tool_name: "run_terminal_command".into(),
                    args_summary: "x".repeat(200),
                    working_dir: Some("/w".into()),
                    command: Some("y".repeat(200)),
                })
                .await;
                log.record(AuditEventKind::ToolComplete {
                    operation_id: id,
                    success: true,
                    duration_ms: 1,
                    exit_code: Some(0),
                    outcome: Some("completed".into()),
                })
                .await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }

        let contents = tokio::fs::read_to_string(log.path()).await.unwrap();
        // parse_lines panics on any partial/interleaved line.
        let events = parse_lines(&contents);
        let calls = events.iter().filter(|e| e["type"] == "tool_call").count();
        let completes = events
            .iter()
            .filter(|e| e["type"] == "tool_complete")
            .count();
        assert_eq!(calls, 24, "every call recorded exactly once");
        assert_eq!(completes, 24, "every completion recorded exactly once");
    }
}
