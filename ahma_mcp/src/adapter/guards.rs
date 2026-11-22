//! What a running command holds until it has ended, and the one way to end it.

use std::sync::Arc;
use std::time::Instant;

use super::{PublishEnding, audit, finish_handoff_watch, workspace_queue};
use crate::operation_monitor::{CommandOutput, OperationMonitor, Terminal};
use crate::sandbox::handoff_watch::HandoffWatch;

/// What a running command holds until it has ended: its place in the
/// workspace queue (SPEC R2.7.1), its mutex group gate (R2.7.2), the
/// deny-tier watch (R6.1.7), its SSH key broker (R-CRED.1) and its link
/// broker (R-LINK.1).
///
/// [`RunGuards::end`] is the one way to end an operation, and it takes the
/// guards by value. Every ending therefore runs the same steps in the same
/// order: the broker's refusals are told, the deny tier is compared, the
/// workspace is handed on, and only then is the ending published and the end
/// of the call audited. Each path used to release the lease by hand, one
/// `drop(lease.take())` per early return, and each audited its own end.
pub(super) struct RunGuards {
    op_id: String,
    /// What the handoff watch's findings name: the tool or the command.
    subject: String,
    /// From the task's start, so time spent queued is in the audit record.
    started: Instant,
    lease: Option<workspace_queue::Lease>,
    /// The command's `tools.mutex_groups` gate (SPEC R2.7.2), taken after the
    /// lease and handed on with it.
    mutex: Option<super::mutex_groups::MutexGroupGuard>,
    handoff: Option<HandoffWatch>,
    #[cfg(unix)]
    broker: Option<crate::credentials::ssh_agent::host::BrokerLease>,
    #[cfg(unix)]
    links: Option<crate::links::host::LinkLease>,
}

impl RunGuards {
    /// Start a run, the one way for every async path: wait for its turn in
    /// the workspace queue (SPEC R2.7.1) when its admission holds a place in
    /// line, then mark it running, not before.
    /// `None` when it was cancelled first, while queued or as its turn came;
    /// it has then been ended. `subject` is what the handoff watch's findings
    /// name.
    pub(super) async fn begin(
        monitor: &Arc<OperationMonitor>,
        op_id: &str,
        subject: &str,
        admission: workspace_queue::Admission,
        cancel: &tokio_util::sync::CancellationToken,
        queue: Option<&super::QueueWait>,
    ) -> Option<Self> {
        // From the task's start, so time spent queued is in the audit record.
        let mut guards = Self::held(op_id, subject, None);
        match super::wait_for_lease(admission.into_ticket(), monitor, op_id, cancel, queue).await {
            // Held until the operation ends: until its process tree is gone.
            Ok(lease) => guards.lease = lease,
            Err(ending) => {
                guards.end(monitor, ending).await;
                return None;
            }
        }
        monitor.start(op_id).await;
        if cancel.is_cancelled() {
            tracing::info!("Operation {op_id} was cancelled before execution started");
            guards.end(monitor, super::cancelled()).await;
            return None;
        }
        Some(guards)
    }

    /// The guards of a run that holds `lease`, if it took one, with its clock
    /// started now.
    fn held(op_id: &str, subject: &str, lease: Option<workspace_queue::Lease>) -> Self {
        Self {
            op_id: op_id.to_string(),
            subject: subject.to_string(),
            started: Instant::now(),
            lease,
            mutex: None,
            handoff: None,
            #[cfg(unix)]
            broker: None,
            #[cfg(unix)]
            links: None,
        }
    }

    pub(super) fn lease(&self) -> Option<&workspace_queue::Lease> {
        self.lease.as_ref()
    }

    /// Wait for the command's `tools.mutex_groups` gate (SPEC R2.7.2), after
    /// the lease, and hold it until the operation ends: the one way every
    /// async path takes it. `None` when the wait gave up; the operation has
    /// then been ended `Failed`, as for any other failure to start.
    pub(super) async fn gate(
        mut self,
        monitor: &Arc<OperationMonitor>,
        registry: &super::CommandMutexRegistry,
        command_line: &str,
        workspace: &std::path::Path,
    ) -> Option<Self> {
        match super::acquire_mutex_gate(registry, monitor, &self.op_id, command_line, workspace)
            .await
        {
            Ok(permit) => {
                self.mutex = permit;
                Some(self)
            }
            Err(why) => {
                self.end(monitor, super::failed(why)).await;
                None
            }
        }
    }

    /// Watch the deny tier (SPEC R6.1.7) from now until the operation ends.
    pub(super) fn watch_handoff(&mut self, watch: Option<HandoffWatch>) {
        self.handoff = watch;
    }

    /// Hold the command's SSH key broker (SPEC R-CRED.1) until it has ended.
    #[cfg(unix)]
    pub(super) fn hold_broker(
        &mut self,
        broker: Option<crate::credentials::ssh_agent::host::BrokerLease>,
    ) {
        self.broker = broker;
    }

    #[cfg(unix)]
    pub(super) fn broker(&self) -> Option<&crate::credentials::ssh_agent::host::BrokerLease> {
        self.broker.as_ref()
    }

    /// Hold the command's link broker (SPEC R-LINK.1) until it has ended.
    #[cfg(unix)]
    pub(super) fn hold_links(&mut self, links: Option<crate::links::host::LinkLease>) {
        self.links = links;
    }

    #[cfg(unix)]
    pub(super) fn links(&self) -> Option<&crate::links::host::LinkLease> {
        self.links.as_ref()
    }

    /// Compare the deny tier with its inventory from before the spawn (SPEC
    /// R6.1.7) and put the report in `output` (`handoff_writes`,
    /// `handoff_alert`), raising the alert while the operation is still
    /// active: the `Alert` event is what the hub and the TUI show. For a
    /// command whose process tree is gone and whose result is still to be
    /// published; the one way every async path does it. `end` then finds no
    /// watch, so the deny tier is compared once.
    pub(super) async fn report_handoff_in(
        &mut self,
        output: &mut CommandOutput,
        monitor: &OperationMonitor,
    ) {
        let Some(watch) = self.handoff.take() else {
            return;
        };
        let Some(report) = finish_handoff_watch(watch, &self.op_id, &self.subject).await else {
            return;
        };
        let alert = report.render_alert();
        output.handoff_writes = Some(report.to_json());
        output.handoff_alert = Some(alert.clone());
        monitor.append_alert(&self.op_id, alert).await;
    }

    /// End the operation with `ending`.
    ///
    /// A watch still held here (an ending that builds no result: a timeout, a
    /// cancellation, a failure to start) is compared for the log and the audit
    /// trail, before the workspace is handed on, so another command's writes
    /// cannot be counted as this one's. The workspace is then free before
    /// anyone can observe the operation as ended (SPEC R2.7.1).
    ///
    /// When a cancel or the watchdog asked the run to stop (SPEC R13.3), the
    /// ending they asked for is published, with its own reason and kind,
    /// whatever ending the run reached: one that had just exited on its own
    /// still ends cancelled, as when the monitor ended it itself.
    pub(super) async fn end(self, monitor: &OperationMonitor, ending: Terminal) {
        // What the broker refused, one line each (SPEC R-CRED.10): before the
        // terminal transition, so a waiter observes it deterministically
        // rather than racing the broker thread's own alert.
        #[cfg(unix)]
        if let Some(broker) = &self.broker {
            for refusal in super::broker_refusal_lines(broker) {
                monitor.append_alert_unique(&self.op_id, refusal).await;
            }
        }
        // And what the link broker refused, once each (SPEC R-LINK.6), so a
        // tool that hid the helper's line still leaves it in the result.
        #[cfg(unix)]
        if let Some(links) = &self.links {
            for refusal in links.refusal_lines() {
                monitor.append_alert_unique(&self.op_id, refusal).await;
            }
        }
        let op_id = self.op_id.clone();
        // A report still to make goes to the log and the audit trail only:
        // this ending has no result built to carry it.
        let released = self.release().await;
        let ending = monitor.take_stop_request(&op_id).await.unwrap_or(ending);
        let (outcome, exit_code) = ending.publish(monitor, &op_id).await;
        audit::record_tool_complete(
            &op_id,
            outcome,
            released.started.elapsed().as_millis() as u64,
            exit_code,
        )
        .await;
    }

    /// Let go of everything the run holds, in the one order every path
    /// shares: compare the deny tier with its inventory from before the spawn
    /// (SPEC R6.1.7), saying what changed to the log and the audit trail, then
    /// hand on the brokers, the gate and, last, the workspace (R2.7.1).
    async fn release(self) -> Released {
        let Self {
            op_id,
            subject,
            started,
            lease,
            mutex,
            handoff,
            #[cfg(unix)]
            broker,
            #[cfg(unix)]
            links,
        } = self;
        if let Some(watch) = handoff {
            finish_handoff_watch(watch, &op_id, &subject).await;
        }
        #[cfg(unix)]
        drop(broker);
        #[cfg(unix)]
        drop(links);
        // The gate was taken after the lease: handed on first.
        drop(mutex);
        drop(lease);
        Released { started }
    }
}

/// What a run's guards leave once they have been let go.
struct Released {
    /// When the run's clock started, for its audit record's duration.
    started: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_monitor::{MonitorConfig, Operation, OperationStatus};
    use std::time::Duration;

    /// A cancel that arrives while the command is ending on its own still ends
    /// it cancelled, as when the monitor ended the operation itself: the stop a
    /// person asked for wins over whatever ending the run then reached. Only a
    /// run that saw its token signalled used to take the request, so a run
    /// that had just exited published its own ending and the cancel reported
    /// failure (seen on Windows CI).
    #[tokio::test]
    async fn a_stop_asked_for_wins_over_the_ending_the_run_reached() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            Duration::from_secs(60),
        )));
        let token = monitor
            .register(Operation::new("op".into(), "t".into(), "d".into(), None))
            .await;
        monitor.ends_by_task("op").await;
        let guards = RunGuards::begin(
            &monitor,
            "op",
            "t",
            workspace_queue::Admission::Exclusive(None),
            &token,
            None,
        )
        .await
        .expect("not cancelled yet");

        let cancel = {
            let monitor = monitor.clone();
            tokio::spawn(async move {
                monitor
                    .cancel_operation_with_reason("op", Some("stopped".into()))
                    .await
            })
        };
        crate::test_utils::assertions::assert_eventually(
            ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick),
            ahma_common::timeouts::TestTimeouts::poll_interval(),
            "the cancel is recorded",
            || async { token.is_cancelled() },
        )
        .await;

        // The run had already exited on its own and reports that.
        guards
            .end(&monitor, Terminal::Failed("exit code 1".into()))
            .await;

        assert!(cancel.await.unwrap(), "the cancel reports it cancelled");
        let op = monitor.wait_for_operation("op").await.unwrap();
        assert_eq!(op.state, OperationStatus::Cancelled);
        assert!(op.result.unwrap().to_string().contains("stopped"));
    }
}
