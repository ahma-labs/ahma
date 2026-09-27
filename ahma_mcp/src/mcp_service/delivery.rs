//! Nothing a model started is ever silently lost (SPEC R2.7.5).
//!
//! In async mode a long command hands back an operation id and the model moves
//! on. The documented contract is that it `await`s the id before relying on the
//! work — and models forget. This ledger closes that hole structurally: every
//! operation this session started whose result it has not yet been handed is
//! remembered, and the moment one has finished, its outcome is **prepended to
//! the next tool result the session returns**, whatever tool that is. A model
//! cannot miss a result it was not told to collect, because its next call
//! delivers it.
//!
//! It also explains the queue (SPEC R2.7.3): a call whose command is still
//! waiting for its workspace says so in plain words — "NOT started, queued
//! behind …" — so a queued `sed -i` is never mistaken for an applied one.

use crate::adapter::workspace_queue::HolderInfo;
use crate::operation_monitor::{Operation, OperationMonitor};
use rmcp::model::{CallToolResult, ContentBlock};

/// How much of each piggybacked result is shown (its tail): enough for the
/// verdict and the last errors, not a second copy of a long log. The complete
/// output is always in the operation's `output_file`.
const PIGGYBACK_MAX_CHARS: usize = 1500;

/// Operation ids this session started and has not yet delivered a result for.
#[derive(Debug, Default)]
pub struct UndeliveredOps(parking_lot::Mutex<Vec<String>>);

impl UndeliveredOps {
    /// Remember an operation whose call returned without its result.
    pub fn started_without_result(&self, id: &str) {
        let mut ids = self.0.lock();
        if !ids.iter().any(|x| x == id) {
            ids.push(id.to_string());
        }
    }

    /// The result of `id` has reached the model (inline, via `await`, or piggybacked).
    pub fn delivered(&self, id: &str) {
        self.0.lock().retain(|x| x != id);
    }

    /// Every id still pending delivery.
    pub fn pending(&self) -> Vec<String> {
        self.0.lock().clone()
    }

    /// Take every pending operation that has finished, oldest first. Ids the
    /// monitor no longer knows (evicted from its bounded history) are dropped:
    /// there is nothing left to deliver.
    pub async fn take_finished(&self, monitor: &OperationMonitor) -> Vec<Operation> {
        let mut finished = Vec::new();
        for id in self.pending() {
            if let Some(op) = monitor.check_completion_history_pub(&id).await {
                self.delivered(&id);
                finished.push(op);
            } else if monitor.get_operation(&id).await.is_none() {
                self.delivered(&id);
            }
        }
        finished
    }
}

/// Prepend finished operations to `result` (SPEC R2.7.5). The result's own
/// content follows unchanged, so a model — or a test — reading it for the
/// call it made still finds exactly that.
pub fn prepend_finished(mut result: CallToolResult, finished: &[Operation]) -> CallToolResult {
    if finished.is_empty() {
        return result;
    }
    let mut text =
        String::from("── Finished since your last call (not collected with `await`) ──\n");
    for op in finished {
        text.push_str(&summarize(op));
        text.push_str("\n\n");
    }
    text.push_str("── Result of this call ──");
    result.content.insert(0, ContentBlock::text(text));
    result
}

/// One finished operation, as the tail of its usual inline rendering.
fn summarize(op: &Operation) -> String {
    let rendered = super::handlers::common::render_completed_operation(op);
    if rendered.chars().count() <= PIGGYBACK_MAX_CHARS {
        return rendered;
    }
    // Keep the identity line (first line) and the tail of the rest.
    let (first, rest) = rendered.split_once('\n').unwrap_or((&rendered, ""));
    let tail: String = {
        let chars: Vec<char> = rest.chars().collect();
        chars[chars.len().saturating_sub(PIGGYBACK_MAX_CHARS)..]
            .iter()
            .collect()
    };
    let file = op
        .output_file
        .as_ref()
        .map(|p| format!(" Full output: {}", p.display()))
        .unwrap_or_default();
    format!("{first}\n[… earlier output omitted.{file}]\n{tail}")
}

/// The answer for a call whose operation has not started because it is
/// waiting for its workspace (SPEC R2.7.3).
pub fn queued_notice(id: &str, ahead: &[HolderInfo]) -> String {
    let behind = match ahead {
        [] => "another ahma process that holds this workspace".to_string(),
        [one] => one.describe(),
        [first, rest @ ..] => format!("{} and {} more", first.describe(), rest.len()),
    };
    format!(
        "AHMA ID: {id}\n\
         NOT started — queued behind {behind}. Commands that may write the workspace run one \
         at a time, in the order they were sent; this one runs automatically when its turn \
         comes. Do not send it again. `await` id `{id}` for its result, or `cancel` it (or \
         the operation ahead of it)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_monitor::{MonitorConfig, OperationStatus};
    use std::time::Duration;

    fn monitor() -> OperationMonitor {
        OperationMonitor::new(MonitorConfig::with_timeout(Duration::from_secs(60)))
    }

    async fn finished_op(m: &OperationMonitor, id: &str, stdout: &str) {
        m.add_operation(Operation::new(
            id.into(),
            "run_terminal_command".into(),
            "x".into(),
            None,
        ))
        .await;
        m.update_status(
            id,
            OperationStatus::Completed,
            Some(serde_json::json!({"stdout": stdout, "stderr": "", "exit_code": 0})),
        )
        .await;
    }

    #[tokio::test]
    async fn a_finished_operation_is_taken_once() {
        let m = monitor();
        let ledger = UndeliveredOps::default();
        finished_op(&m, "op1", "all tests passed").await;
        ledger.started_without_result("op1");
        let taken = ledger.take_finished(&m).await;
        assert_eq!(taken.len(), 1);
        assert!(
            ledger.take_finished(&m).await.is_empty(),
            "never delivered twice"
        );
    }

    #[tokio::test]
    async fn a_running_operation_stays_pending() {
        let m = monitor();
        let ledger = UndeliveredOps::default();
        m.add_operation(Operation::new("op2".into(), "t".into(), "x".into(), None))
            .await;
        ledger.started_without_result("op2");
        assert!(ledger.take_finished(&m).await.is_empty());
        assert_eq!(ledger.pending(), vec!["op2".to_string()]);
    }

    #[tokio::test]
    async fn an_unknown_operation_is_dropped() {
        let m = monitor();
        let ledger = UndeliveredOps::default();
        ledger.started_without_result("gone");
        assert!(ledger.take_finished(&m).await.is_empty());
        assert!(ledger.pending().is_empty());
    }

    #[tokio::test]
    async fn finished_results_come_first_and_the_call_result_is_intact() {
        let m = monitor();
        finished_op(&m, "op1", "test result: ok").await;
        let op = m.check_completion_history_pub("op1").await.unwrap();
        let result = prepend_finished(
            CallToolResult::success(vec![ContentBlock::text("git status output")]),
            &[op],
        );
        assert_eq!(result.content.len(), 2);
        let first = result.content[0].as_text().unwrap().text.clone();
        assert!(first.contains("Finished since your last call"), "{first}");
        assert!(first.contains("test result: ok"), "{first}");
        assert_eq!(
            result.content[1].as_text().unwrap().text,
            "git status output"
        );
    }

    #[test]
    fn nothing_finished_leaves_the_result_alone() {
        let result = prepend_finished(CallToolResult::success(vec![ContentBlock::text("x")]), &[]);
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn the_queued_notice_says_not_started_and_names_the_holder() {
        let holder = HolderInfo::new("op_7", "cargo nextest run");
        let text = queued_notice("op_8", &[holder]);
        assert!(text.contains("NOT started"));
        assert!(text.contains("op_7"));
        assert!(text.contains("cargo nextest run"));
        assert!(text.contains("Do not send it again"));
    }
}
