//! What a running command holds until it has ended, and the one way to end it.

use std::time::Instant;

use super::{PublishEnding, audit, finish_handoff_watch, workspace_queue};
use crate::operation_monitor::{OperationMonitor, Terminal};
use crate::sandbox::handoff_watch::{HandoffReport, HandoffWatch};

/// What a running command holds until it has ended: its place in the
/// workspace queue (SPEC R2.7.1), the deny-tier watch (R6.1.7) and its SSH key
/// broker (R-CRED.1).
///
/// [`RunGuards::end`] is the one way to end the operation, and it takes the
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
    handoff: Option<HandoffWatch>,
    #[cfg(unix)]
    broker: Option<crate::credentials::ssh_agent::host::BrokerLease>,
}

impl RunGuards {
    pub(super) fn new(op_id: &str, subject: &str) -> Self {
        Self {
            op_id: op_id.to_string(),
            subject: subject.to_string(),
            started: Instant::now(),
            lease: None,
            handoff: None,
            #[cfg(unix)]
            broker: None,
        }
    }

    /// Hold the workspace (SPEC R2.7.1) until the operation ends.
    pub(super) fn hold_lease(&mut self, lease: Option<workspace_queue::Lease>) {
        self.lease = lease;
    }

    pub(super) fn lease(&self) -> Option<&workspace_queue::Lease> {
        self.lease.as_ref()
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

    /// The deny tier compared with its inventory from before the spawn, for a
    /// command whose process tree is gone and whose result, which carries the
    /// report, is still to be built.
    pub(super) async fn take_handoff_report(&mut self) -> Option<HandoffReport> {
        let watch = self.handoff.take()?;
        finish_handoff_watch(watch, &self.op_id, &self.subject).await
    }

    /// End the operation with `ending`.
    ///
    /// A watch still held here (an ending that builds no result: a timeout, a
    /// cancellation, a failure to start) is compared for the log and the audit
    /// trail, before the workspace is handed on, so another command's writes
    /// cannot be counted as this one's. The workspace is then free before
    /// anyone can observe the operation as ended (SPEC R2.7.1).
    ///
    /// A run that stopped because its token was signalled ends `Cancelled`;
    /// when a cancel or the watchdog asked for that, the ending they asked for
    /// is published instead, with its own reason and kind.
    pub(super) async fn end(self, monitor: &OperationMonitor, ending: Terminal) {
        let Self {
            op_id,
            subject,
            started,
            lease,
            handoff,
            #[cfg(unix)]
            broker,
        } = self;
        // What the broker refused, one line each (SPEC R-CRED.10): before the
        // terminal transition, so a waiter observes it deterministically
        // rather than racing the broker thread's own alert.
        #[cfg(unix)]
        if let Some(broker) = &broker {
            for refusal in super::broker_refusal_lines(broker) {
                monitor.append_alert_unique(&op_id, refusal).await;
            }
        }
        if let Some(watch) = handoff {
            let _ = finish_handoff_watch(watch, &op_id, &subject).await;
        }
        #[cfg(unix)]
        drop(broker);
        drop(lease);
        let ending = match ending {
            Terminal::Cancelled(_) => monitor.take_stop_request(&op_id).await.unwrap_or(ending),
            other => other,
        };
        let (outcome, exit_code) = ending.publish(monitor, &op_id).await;
        audit::record_tool_complete(
            &op_id,
            outcome,
            started.elapsed().as_millis() as u64,
            exit_code,
        )
        .await;
    }
}
