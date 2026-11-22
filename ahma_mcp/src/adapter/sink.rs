//! What a runner gets: somewhere to write a command's output, and nothing that
//! can end its operation.
//!
//! The piped, PTY and session runners each say how their command ended as a
//! [`RunEnd`], and write everything else through an [`OpSink`]: each line of
//! output, an alert, a progress note, proof of life. A sink holds the
//! [`OperationMonitor`] privately, so a runner has no way to publish a
//! terminal state. Only its caller can, through
//! [`RunGuards::end`](super::guards::RunGuards::end), after the workspace has
//! been handed on (SPEC R2.7.1). The PTY and session runners once reported
//! "finished" while still holding the workspace (#284); with the monitor out
//! of their reach, that order cannot be written again.

use std::sync::Arc;
use std::time::Duration;

use super::{BoundedLineCollector, cancelled, spill, timed_out};
use crate::log_monitor::{LogMonitor, LogMonitorConfig};
use crate::operation_monitor::{CommandOutput, OperationMonitor, Stream, Terminal};

/// How a runner's command ended, as the runner saw it. What the command
/// printed is in the [`OpSink`] it wrote to; [`OpSink::close`] puts the two
/// together into the operation's [`Terminal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RunEnd {
    /// It exited, with its exit code; `None` when it has none (a signal ended
    /// it, or a session could not read its end marker). `Completed` only for
    /// `Some(0)`.
    Exited(Option<i32>),
    /// It did not run to an exit of its own: it could not be started, or the
    /// session shell it ran in died or could not be written to. The reason is
    /// the operation's result.
    Failed(String),
    /// Its time limit stopped it, `ran_for` after it started.
    TimedOut { ran_for: Duration },
    /// It was cancelled.
    Cancelled,
}

impl RunEnd {
    /// The operation's ending: this end, with `output` as what an exit
    /// produced. Every other end has a reason in place of output.
    fn into_terminal(self, output: CommandOutput) -> Terminal {
        match self {
            Self::Exited(exit_code) => Terminal::Exited(Box::new(CommandOutput {
                exit_code,
                ..output
            })),
            Self::Failed(why) => Terminal::Failed(why),
            Self::TimedOut { ran_for } => timed_out(ran_for),
            Self::Cancelled => cancelled(),
        }
    }
}

/// Which result a run's output becomes. The fields a path lacks are left out
/// of its result, so each path keeps the shape agents, the hub and the TUI
/// already read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Shape {
    /// stdout and stderr kept apart, each with its own truncation counts.
    Piped,
    /// One stream from a pseudo-terminal, which merges stderr into it; the
    /// result says `pty: true`.
    Pty,
    /// One stream from a persistent shell session (each command's stderr is
    /// redirected into its stdout); the result names the session.
    Session(String),
}

/// Where a running command's output goes, and the only handle a runner has
/// on its operation (see the module docs).
///
/// Each line is redacted, written to the operation's spill file, kept in a
/// bounded collector for the result, and appended to the operation's tail,
/// which emits it on the unified event stream (SPEC R15.2); a log monitor,
/// when one is configured, raises an alert on a matching line.
pub(super) struct OpSink {
    op_id: String,
    monitor: Arc<OperationMonitor>,
    shape: Shape,
    /// The full-output record, created when the command starts: a command
    /// that never started has none.
    spill: Option<spill::SpillWriter>,
    stdout: BoundedLineCollector,
    stderr: BoundedLineCollector,
    log_monitor: Option<LogMonitor>,
    /// Whether the command is warned once at 80% of its limit (SPEC R2.6.6):
    /// its path's decision ([`super::drive`]), which the runner reads through
    /// [`Self::near_limit`].
    warn_near_limit: bool,
}

impl OpSink {
    pub(super) fn new(monitor: Arc<OperationMonitor>, op_id: &str, shape: Shape) -> Self {
        Self {
            op_id: op_id.to_string(),
            monitor,
            shape,
            spill: None,
            stdout: BoundedLineCollector::default(),
            stderr: BoundedLineCollector::default(),
            log_monitor: None,
            warn_near_limit: true,
        }
    }

    /// Watch each line for the configured error and warning patterns, and
    /// alert on a match.
    pub(super) fn watch_log(mut self, config: Option<LogMonitorConfig>) -> Self {
        self.log_monitor = config.map(LogMonitor::new);
        self
    }

    /// Whether the command is warned near its limit; it is unless told not.
    pub(super) fn warn_near_limit(mut self, warn: bool) -> Self {
        self.warn_near_limit = warn;
        self
    }

    /// The runner's near-limit warning for `limit`, measured from now: one
    /// that never fires when this command is not to be warned.
    pub(super) fn near_limit(&self, limit: Duration) -> super::time_limit::NearLimit {
        let near = super::time_limit::NearLimit::starting_now(limit);
        if self.warn_near_limit {
            near
        } else {
            near.spent()
        }
    }

    /// The operation this sink writes to, for a log line.
    pub(super) fn op_id(&self) -> &str {
        &self.op_id
    }

    /// The command is running: its spill file exists from now on, even while
    /// it prints nothing, so a caller can follow it. Idempotent.
    pub(super) async fn started(&mut self) {
        if self.spill.is_none() {
            self.spill = Some(spill::SpillWriter::create(&self.op_id).await);
        }
    }

    /// One line the command wrote to `stream`.
    pub(super) async fn line(&mut self, stream: Stream, line: &str) {
        self.started().await;
        let safe = crate::log_monitor::redact_sensitive_line(line);
        // The spill file records every redacted line: the faithful full
        // record, independent of the bounded tail.
        if let Some(spill) = &mut self.spill {
            spill.write_line(&safe, stream).await;
        }
        match stream {
            Stream::Stdout => self.stdout.push(safe.clone()),
            Stream::Stderr => self.stderr.push(safe.clone()),
        }
        self.monitor
            .append_output_line(&self.op_id, safe, stream)
            .await;

        // The full notification snapshot (trigger and recent context) is one
        // `Alert` event, so every subscriber gets the same content.
        if let Some(log_monitor) = &mut self.log_monitor
            && let Some(snapshot) = log_monitor.process_line(line, stream.is_stderr())
        {
            self.monitor
                .append_alert(&self.op_id, snapshot.format_for_notification())
                .await;
        }
    }

    /// An alert on the operation while it still runs.
    pub(super) async fn alert(&self, text: String) {
        self.monitor.append_alert(&self.op_id, text).await;
    }

    /// A transient progress note ("still running"), not kept.
    pub(super) fn progress(&self, text: String) {
        self.monitor.note_progress(&self.op_id, text);
    }

    /// Proof of life for the idle watchdog from something other than output.
    pub(super) async fn alive(&self) {
        self.monitor.note_liveness(&self.op_id).await;
    }

    /// Flush the spill file and turn `end` and what was written into the
    /// operation's ending. Publishing it is the caller's.
    pub(super) async fn close(mut self, end: RunEnd) -> Terminal {
        if let Some(spill) = &mut self.spill {
            spill.finish().await;
        }
        let output = self.output();
        end.into_terminal(output)
    }

    /// What was written, in this path's result shape, without an exit code.
    fn output(&self) -> CommandOutput {
        let output_file = spill::operation_spill_path(&self.op_id)
            .to_string_lossy()
            .into_owned();
        // A PTY and a session merge their streams: stderr is empty and has
        // no counts of its own.
        let merged = CommandOutput {
            stdout: self.stdout.rendered_output(),
            stdout_truncated_lines: self.stdout.dropped_lines(),
            stdout_truncated_bytes: self.stdout.dropped_bytes(),
            output_file,
            ..CommandOutput::default()
        };
        match &self.shape {
            Shape::Piped => CommandOutput {
                stderr: self.stderr.rendered_output(),
                stderr_truncated_lines: Some(self.stderr.dropped_lines()),
                stderr_truncated_bytes: Some(self.stderr.dropped_bytes()),
                ..merged
            },
            Shape::Pty => CommandOutput {
                pty: Some(true),
                ..merged
            },
            Shape::Session(id) => CommandOutput {
                session_id: Some(id.clone()),
                ..merged
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_monitor::{MonitorConfig, Operation, OperationStatus};
    use serde_json::Value;

    fn keys(result: &Option<Value>) -> Vec<String> {
        let mut keys: Vec<String> = result
            .as_ref()
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn an_exit_carries_its_code_and_is_completed_only_for_zero() {
        for (code, status) in [
            (Some(0), OperationStatus::Completed),
            (Some(3), OperationStatus::Failed),
            (None, OperationStatus::Failed),
        ] {
            let ending = RunEnd::Exited(code).into_terminal(CommandOutput {
                stdout: "hi".into(),
                ..CommandOutput::default()
            });
            assert_eq!(ending.status(), status, "{code:?}");
            assert_eq!(ending.exit_code(), code);
            let result = ending.into_result().expect("an exit has a result");
            assert_eq!(result["stdout"], "hi");
        }
    }

    #[test]
    fn a_failure_is_its_reason_as_a_string() {
        let ending = RunEnd::Failed("Failed to start PTY command: no".into())
            .into_terminal(CommandOutput::default());
        assert_eq!(ending.status(), OperationStatus::Failed);
        assert_eq!(ending.exit_code(), None);
        assert_eq!(
            ending.into_result(),
            Some(Value::String("Failed to start PTY command: no".into()))
        );
    }

    /// The text agents read when a command is stopped at its limit: how long
    /// it ran, in milliseconds, and how to raise the limit (SPEC R2.6.6).
    #[test]
    fn a_timeout_names_how_long_it_ran_and_how_to_raise_the_limit() {
        let ending = RunEnd::TimedOut {
            ran_for: Duration::from_millis(1500),
        }
        .into_terminal(CommandOutput::default());
        assert_eq!(ending.status(), OperationStatus::TimedOut);
        let result = ending.into_result().expect("a timeout has a result");
        assert_eq!(result["timed_out"], true);
        assert_eq!(
            result["reason"],
            format!(
                "Operation timed out after 1500ms (exceeded timeout limit). {}",
                super::super::time_limit::how_to_raise(2)
            )
        );
    }

    #[test]
    fn a_cancel_is_the_standard_reason() {
        let ending = RunEnd::Cancelled.into_terminal(CommandOutput::default());
        assert_eq!(ending.status(), OperationStatus::Cancelled);
        let result = ending.into_result().expect("a cancel has a result");
        assert_eq!(result["cancelled"], true);
        assert_eq!(result["reason"], "Operation was cancelled");
    }

    async fn monitor_with(op_id: &str) -> Arc<OperationMonitor> {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            Duration::from_secs(30),
        )));
        monitor
            .add_operation(Operation::new(
                op_id.into(),
                "sink_test".into(),
                "d".into(),
                None,
            ))
            .await;
        monitor
    }

    /// Each path's result keeps its shape: the piped one has stderr and its
    /// counts, a PTY says so, a session names itself.
    #[tokio::test]
    async fn each_shape_keeps_its_result_fields() {
        let common = [
            "exit_code",
            "output_file",
            "stderr",
            "stdout",
            "stdout_truncated_bytes",
            "stdout_truncated_lines",
        ];
        for (shape, extra) in [
            (
                Shape::Piped,
                &["stderr_truncated_bytes", "stderr_truncated_lines"][..],
            ),
            (Shape::Pty, &["pty"][..]),
            (Shape::Session("s1".into()), &["session_id"][..]),
        ] {
            let monitor = monitor_with("sink-shape").await;
            let sink = OpSink::new(monitor, "sink-shape", shape.clone());
            let result = sink.close(RunEnd::Exited(Some(0))).await.into_result();
            let mut want: Vec<String> = common
                .iter()
                .chain(extra.iter())
                .map(|k| k.to_string())
                .collect();
            want.sort_unstable();
            assert_eq!(keys(&result), want, "{shape:?}");
        }
    }

    /// A line reaches the operation's tail redacted and the result through
    /// its own stream; a piped result keeps stderr apart.
    #[tokio::test]
    async fn lines_reach_the_tail_and_the_result_by_stream() {
        let monitor = monitor_with("sink-lines").await;
        let mut sink = OpSink::new(monitor.clone(), "sink-lines", Shape::Piped);
        sink.line(Stream::Stdout, "out one").await;
        sink.line(Stream::Stderr, "err one").await;
        sink.line(Stream::Stdout, "out two").await;
        let tail = monitor
            .get_operation("sink-lines")
            .await
            .expect("still running")
            .stdout_tail
            .snapshot();
        assert!(tail.iter().any(|l| l == "out one"), "{tail:?}");
        assert!(tail.iter().any(|l| l == "err one"), "{tail:?}");

        let result = sink
            .close(RunEnd::Exited(Some(1)))
            .await
            .into_result()
            .unwrap();
        assert_eq!(result["stdout"], "out one\nout two");
        assert_eq!(result["stderr"], "err one");
        assert_eq!(result["exit_code"], 1);
    }
}
