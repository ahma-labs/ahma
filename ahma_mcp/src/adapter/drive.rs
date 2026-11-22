//! The one way an asynchronous command runs, whichever runner it uses.
//!
//! The piped, PTY and session paths each wrote out the same lifecycle —
//! register, audit the call, queue, gate, watch the deny tier, run, report,
//! release, publish, audit the end, drop the task handle — and they drifted:
//! PTY and session commands reported "finished" while still holding the
//! workspace (#284), and took their mutex group, the near-limit warning, the
//! drift report and the deny-tier alert only once each was added to them by
//! hand (#320, #321, #324, #328). [`Adapter::drive`] now runs all three. It
//! has one exit: the task always ends through [`RunGuards::end`] (release,
//! then finish, then audit the end), and its handle is removed when the task
//! returns ([`Adapter::spawn_operation_task`]).
//!
//! What still differs by path is said once each: [`Run::stages`] for the
//! stages a path could be let off, and the arms of [`prepare`] and
//! [`Ready::run`] for what only one runner has.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::guards::RunGuards;
use super::{
    Adapter, CommandMutexRegistry, DriftProbe, audit, begin_handoff_watch, build_for_this_command,
    build_sandboxed_command, cpu_probe_threshold, execute_with_streaming, executor, failed,
    pty_exec, record_failure_diagnostics, run_session_operation, sink, stamp_lease,
    workspace_queue,
};
use crate::operation_monitor::{CommandOutput, Operation, OperationMonitor, Terminal};
use crate::sandbox;
use crate::shell_session::ShellSessionManager;

/// Which runner a command runs in, and what only that runner needs.
pub(super) enum Run {
    /// Spawned with stdout and stderr piped (SPEC R15.2).
    Piped {
        program: String,
        args: Vec<String>,
        /// Alerts on the configured error and warning patterns.
        log_monitor: Option<crate::log_monitor::LogMonitorConfig>,
        /// A human declared it long-running (SPEC R2.7.10), so what it
        /// starts may stay running after it (R-PROC.5).
        keep_leftovers: bool,
        executor: Arc<dyn executor::CommandExecutor>,
        /// Offered an out-of-scope path a failure names (SPEC R5).
        scope_grant_notifier: Option<Arc<dyn sandbox::ScopeGrantNotifier>>,
    },
    /// In a pseudo-terminal. `line` is what it runs: the line as written, or
    /// the link helper's for a whole-line `open <URL>` (SPEC R-LINK.10).
    Pty { line: String, keep_leftovers: bool },
    /// In the persistent shell session `id`, created on first use.
    Session {
        id: String,
        sessions: Arc<ShellSessionManager>,
    },
}

/// The stages a run goes through besides its runner, each one a path could
/// be let off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stages {
    /// It waits for its `tools.mutex_groups` gate after the lease, and holds
    /// it until it has ended (SPEC R2.7.2).
    mutex_group: bool,
    /// It is warned once at 80% of its time limit (SPEC R2.6.6).
    near_limit: bool,
    /// Its result names the files that changed in its workspace while it
    /// ran (SPEC R2.7.6).
    drift: bool,
    /// Its result carries the deny-tier report, and the alert is raised
    /// before it ends (SPEC R6.1.7). Without it the report goes only to the
    /// log and the audit trail.
    handoff_in_result: bool,
}

impl Stages {
    const ALL: Self = Self {
        mutex_group: true,
        near_limit: true,
        drift: true,
        handoff_in_result: true,
    };
}

impl Run {
    /// The one place a path is let off a stage: one line here, with its SPEC
    /// text. Every path goes through all four (PTY and session commands
    /// gained them in #320, #321, #324 and #328). A piped command in the
    /// service lane has no drift probe to run (SPEC R2.7.6), which its
    /// [`Prepared::drift`] says.
    fn stages(&self) -> Stages {
        match self {
            Run::Piped { .. } => Stages::ALL,
            Run::Pty { .. } => Stages::ALL,
            Run::Session { .. } => Stages::ALL,
        }
    }
}

/// The `tool_call` audit record of a run (SPEC R-HANDOFF.10), written before
/// its task is spawned.
pub(super) struct AuditCall {
    pub(super) tool: String,
    pub(super) args: Option<Map<String, Value>>,
    pub(super) working_dir: String,
    pub(super) program: String,
    pub(super) argv: Vec<String>,
}

/// A command ready to be driven: what its path's entry point worked out.
pub(super) struct Prepared {
    pub(super) op_id: String,
    /// Its lane and, if exclusive, its place in line, taken on arrival
    /// (SPEC R2.7.1).
    pub(super) admission: workspace_queue::Admission,
    pub(super) call: AuditCall,
    /// Said as an alert as soon as it is registered: that a whole-line
    /// `open <URL>` runs as the link helper (SPEC R-LINK.10).
    pub(super) link_note: Option<String>,
    /// What its deny-tier findings name: the tool or the command.
    pub(super) subject: String,
    /// The command line as the caller wrote it: what its mutex group
    /// (SPEC R2.7.2) and its brokers are matched on.
    pub(super) line: String,
    pub(super) working_dir: PathBuf,
    /// The workspace key (SPEC R2.7.2): the mutex groups' key.
    pub(super) workspace: PathBuf,
    /// Where and how to look for drift (SPEC R2.7.6); `None` for a service.
    pub(super) drift: Option<DriftProbe>,
    /// Inventory the trust-handoff deny tier around the run (SPEC R6.1.7).
    pub(super) watch_handoff: bool,
    /// Its time limit ([`super::command_limit`]).
    pub(super) limit: Duration,
    pub(super) run: Run,
}

/// What every run gets from the adapter.
struct Deps {
    monitor: Arc<OperationMonitor>,
    sandbox: Arc<sandbox::Sandbox>,
    mutex_registry: Arc<CommandMutexRegistry>,
    #[cfg(unix)]
    credential_brokers: Option<Arc<dyn crate::credentials::ssh_agent::consent::BrokerFactory>>,
    #[cfg(unix)]
    link_brokers: Option<Arc<dyn crate::links::host::LinkBrokerFactory>>,
}

impl Adapter {
    /// Register `operation` and run `prepared` to its end, in the background;
    /// return its id.
    ///
    /// The operation is registered and the call audited before the task is
    /// spawned, so nothing that happens after this returns — a panic, a
    /// SIGKILL, a machine losing power — can leave the log without a record
    /// of what was asked for (SPEC R-HANDOFF.10).
    ///
    /// Boxed: every path's entry point awaits it, and an unboxed future
    /// here would nest every server future one level deeper than before
    /// there was one `drive`, past the compiler's query depth limit for a
    /// caller as deep as the MCP client doctest.
    pub(super) fn drive(
        &self,
        operation: Operation,
        prepared: Prepared,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = String> + Send + '_>> {
        Box::pin(async move {
            let cancel = self.monitor.register(operation).await;
            let op_id = prepared.op_id.clone();
            if let Some(said) = &prepared.link_note {
                self.monitor.append_alert(&op_id, said.clone()).await;
            }
            let call = &prepared.call;
            audit::record_tool_call(
                &op_id,
                &call.tool,
                call.args.as_ref(),
                &call.working_dir,
                &call.program,
                &call.argv,
            )
            .await;
            let deps = Deps {
                monitor: self.monitor.clone(),
                sandbox: self.sandbox.clone(),
                mutex_registry: self.mutex_registry.clone(),
                #[cfg(unix)]
                credential_brokers: self.credential_brokers.clone(),
                #[cfg(unix)]
                link_brokers: self.link_brokers.clone(),
            };
            self.spawn_operation_task(&op_id, run_to_end(prepared, deps, cancel))
                .await;
            op_id
        })
    }
}

/// The task: every way out of it has ended the operation through its guards.
async fn run_to_end(prepared: Prepared, deps: Deps, cancel: CancellationToken) {
    let Prepared {
        op_id,
        admission,
        subject,
        line,
        working_dir,
        workspace,
        drift,
        watch_handoff,
        limit,
        run,
        ..
    } = prepared;
    let monitor = &deps.monitor;
    let stages = run.stages();
    let lane = admission.lane();

    // Wait for every exclusive operation that arrived earlier in this
    // workspace, in this process or any other (SPEC R2.7.1). The lease is
    // held until the run has ended: until its process tree is gone.
    let Some(guards) = RunGuards::begin(monitor, &op_id, &subject, admission, &cancel).await else {
        return;
    };
    let mut guards = if stages.mutex_group {
        let Some(guards) = guards
            .gate(monitor, &deps.mutex_registry, &line, &workspace)
            .await
        else {
            return;
        };
        guards
    } else {
        guards
    };
    // After the queue and the gate, so time spent waiting is not counted as
    // the run's for drift.
    let started = Instant::now();

    let mut ready = match prepare(run, lane, &op_id, &line, &working_dir, &deps, &mut guards) {
        Ok(ready) => ready,
        Err(why) => {
            guards.end(monitor, failed(why)).await;
            return;
        }
    };
    // The "before" inventory of the deny tier, as late as possible: just
    // before the spawn (SPEC R6.1.7).
    if watch_handoff {
        guards.watch_handoff(Some(begin_handoff_watch(&deps.sandbox, &working_dir).await));
    }

    let mut sink = ready.sink(monitor, &op_id, stages.near_limit);
    let end = ready
        .run(&deps, &working_dir, limit, started, &cancel, &mut sink)
        .await;
    let ending = sink.close(end).await;
    if let (Ready::Piped { explain, .. }, Terminal::Exited(output)) = (&ready, &ending) {
        explain
            .exit(output, &deps, &op_id, &subject, &working_dir)
            .await;
    }
    let drift = drift.filter(|_| stages.drift);
    let ending = carry_run_reports(
        ending,
        drift.as_ref(),
        started,
        stages.handoff_in_result,
        &mut guards,
        monitor,
    )
    .await;
    // The process tree is gone and the reports are in: the guards hand the
    // workspace on, then publish, then audit the end.
    guards.end(monitor, ending).await;
}

/// A run with its process about to start: what each runner needs to start
/// it, now that it holds its turn.
enum Ready {
    Piped {
        cmd: tokio::process::Command,
        log_monitor: Option<crate::log_monitor::LogMonitorConfig>,
        explain: ExplainExit,
    },
    Pty {
        line: String,
        brokers: pty_exec::BrokerEnv,
        keep_leftovers: bool,
    },
    Session {
        id: String,
        command: String,
        sessions: Arc<ShellSessionManager>,
    },
}

/// Everything a run needs before its process starts, by path; held by its
/// guards from here until it has ended. `Err` when it cannot be started.
fn prepare(
    run: Run,
    lane: workspace_queue::Lane,
    op_id: &str,
    line: &str,
    working_dir: &std::path::Path,
    deps: &Deps,
    guards: &mut RunGuards,
) -> Result<Ready, String> {
    #[cfg(not(unix))]
    let _ = line;
    match run {
        Run::Piped {
            program,
            args,
            log_monitor,
            keep_leftovers,
            executor,
            scope_grant_notifier,
        } => {
            // Its link broker, leased before its sandbox is built in either
            // lane: having one is what lets this command, and no other,
            // reach the broker directory (SPEC R-LINK.1).
            #[cfg(unix)]
            let links = deps
                .link_brokers
                .as_ref()
                .and_then(|f| f.lease_for_command(working_dir, Some(line)));
            #[cfg(unix)]
            let served_links = links.is_some();
            #[cfg(not(unix))]
            let served_links = false;
            let built = build_for_this_command(served_links, || {
                if lane == workspace_queue::Lane::ReadOnly {
                    executor.build_read_only_command(&deps.sandbox, &program, &args, working_dir)
                } else {
                    build_sandboxed_command(
                        executor.as_ref(),
                        &deps.sandbox,
                        &program,
                        &args,
                        working_dir,
                    )
                }
            });
            let mut cmd = built.map_err(|e| format!("Failed to create sandboxed command: {e}"))?;
            // An ahma it starts does not wait for the lease it runs under
            // (SPEC R2.7.7).
            stamp_lease(&mut cmd, guards.lease());
            if keep_leftovers {
                sandbox::leftovers::keep(&mut cmd);
            }
            // Its SSH key broker (SPEC R-CRED.1): a refused signature is an
            // alert while it still runs (R-CRED.10).
            #[cfg(unix)]
            guards.hold_broker(
                deps.credential_brokers
                    .as_ref()
                    .and_then(|f| f.lease_for_command(working_dir, Some(line)))
                    .inspect(|broker| {
                        cmd.env("SSH_AUTH_SOCK", broker.socket());
                        super::alert_broker_refusals(broker, &deps.monitor, op_id);
                    }),
            );
            // The link broker's helper and socket (SPEC R-LINK.1); what it
            // refuses and asks while the command waits is an alert (R-LINK.6).
            #[cfg(unix)]
            guards.hold_links(links.inspect(|links| {
                for (key, value) in links.env() {
                    cmd.env(key, value);
                }
                super::watch_links(links, &deps.monitor, op_id);
            }));
            // The tag the kernel's records of its denials carry (SPEC
            // R-DENY.1), for a failure to read them back.
            if let Some(tag) = sandbox::kernel_denials::tag_of(cmd.as_std().get_args()) {
                sandbox::kernel_denials::remember_tag(op_id, tag);
            }
            Ok(Ready::Piped {
                cmd,
                log_monitor,
                explain: ExplainExit {
                    scope_grant_notifier,
                },
            })
        }
        Run::Pty {
            line: runs,
            keep_leftovers,
        } => {
            // Not stamped with its lease (SPEC R2.7.7), unlike a piped command.
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut brokers = pty_exec::BrokerEnv::default();
            #[cfg(unix)]
            {
                // Its SSH key broker (SPEC R-CRED.1), as for a piped command.
                guards.hold_broker(
                    deps.credential_brokers
                        .as_ref()
                        .and_then(|f| f.lease_for_command(working_dir, Some(line)))
                        .inspect(|broker| {
                            super::alert_broker_refusals(broker, &deps.monitor, op_id)
                        }),
                );
                // And its link broker (SPEC R-LINK.1, R-LINK.6).
                let links = deps
                    .link_brokers
                    .as_ref()
                    .and_then(|f| f.lease_for_command(working_dir, Some(line)))
                    .inspect(|links| super::watch_links(links, &deps.monitor, op_id));
                if let Some(broker) = guards.broker() {
                    brokers.push("SSH_AUTH_SOCK", broker.socket().as_os_str().to_os_string());
                }
                if let Some(links) = &links {
                    brokers.links_served = true;
                    for (key, value) in links.env() {
                        brokers.push(key, value);
                    }
                }
                guards.hold_links(links);
            }
            Ok(Ready::Pty {
                line: runs,
                brokers,
                keep_leftovers,
            })
        }
        // No broker and no lease stamp: a session's commands share one
        // long-lived shell, which no per-command lease or environment can
        // reach. What it leaves running stays with the session's shell.
        Run::Session { id, sessions } => Ok(Ready::Session {
            id,
            command: line.to_string(),
            sessions,
        }),
    }
}

impl Ready {
    /// Where the run's output goes: shaped as its path's result is.
    fn sink(
        &mut self,
        monitor: &Arc<OperationMonitor>,
        op_id: &str,
        near_limit: bool,
    ) -> sink::OpSink {
        let (shape, log_monitor) = match self {
            Ready::Piped { log_monitor, .. } => (sink::Shape::Piped, log_monitor.take()),
            Ready::Pty { .. } => (sink::Shape::Pty, None),
            Ready::Session { id, .. } => (sink::Shape::Session(id.clone()), None),
        };
        sink::OpSink::new(monitor.clone(), op_id, shape)
            .watch_log(log_monitor)
            .warn_near_limit(near_limit)
    }

    /// Run the command to its end, writing its output to `sink`.
    async fn run(
        &mut self,
        deps: &Deps,
        working_dir: &std::path::Path,
        limit: Duration,
        started: Instant,
        cancel: &CancellationToken,
        sink: &mut sink::OpSink,
    ) -> sink::RunEnd {
        match self {
            Ready::Piped { cmd, .. } => {
                execute_with_streaming(
                    cmd,
                    limit,
                    started,
                    cpu_probe_threshold(deps.monitor.idle_timeout()),
                    cancel,
                    sink,
                )
                .await
            }
            Ready::Pty {
                line,
                brokers,
                keep_leftovers,
            } => {
                pty_exec::run_pty_operation(
                    &deps.sandbox,
                    line,
                    working_dir,
                    limit,
                    cancel,
                    sink,
                    brokers,
                    *keep_leftovers,
                )
                .await
            }
            Ready::Session {
                id,
                command,
                sessions,
            } => {
                run_session_operation(
                    sessions,
                    &deps.sandbox,
                    id,
                    command,
                    working_dir,
                    limit,
                    cancel,
                    sink,
                )
                .await
            }
        }
    }
}

/// What a piped command's exit is explained with, before its drift and its
/// deny tier are added to the result.
struct ExplainExit {
    /// Offered an out-of-scope path a failure names (SPEC R5).
    scope_grant_notifier: Option<Arc<dyn sandbox::ScopeGrantNotifier>>,
}

impl ExplainExit {
    /// A failure may have hit a sandbox denial: what the kernel recorded is
    /// an alert, and an out-of-scope path it names is offered as a grant. A
    /// success has nothing to read back, and gains only the GPU note.
    async fn exit(
        &self,
        output: &CommandOutput,
        deps: &Deps,
        op_id: &str,
        tool: &str,
        working_dir: &std::path::Path,
    ) {
        if output.exit_code == Some(0) {
            let _ = sandbox::kernel_denials::take_tag(op_id);
            // The async twin of the success note in `explain_ungrantable_failure`.
            if let Some(note) = sandbox::gpu_fallback_note(&output.stderr, &output.stdout) {
                deps.monitor.append_alert(op_id, note).await;
            }
        } else {
            // The async twin of the scan in `execute_sync_in_dir`.
            record_failure_diagnostics(
                &deps.sandbox,
                self.scope_grant_notifier.as_ref(),
                &deps.monitor,
                op_id,
                tool,
                &output.stdout,
                &output.stderr,
                Some(working_dir),
            )
            .await;
        }
    }
}

/// A command's ending with what its result owes beyond the output, in this
/// order: the files that changed in its workspace while it ran (SPEC
/// R2.7.6), then, when `handoff_in_result`, the deny-tier report and its
/// alert (R6.1.7). Only an ending that carries output (`Exited`) has a place
/// for them; any other leaves the watch for [`RunGuards::end`], which
/// reports it to the log and the audit trail.
async fn carry_run_reports(
    ending: Terminal,
    drift: Option<&DriftProbe>,
    started: Instant,
    handoff_in_result: bool,
    guards: &mut RunGuards,
    monitor: &OperationMonitor,
) -> Terminal {
    let mut output = match ending {
        Terminal::Exited(output) => output,
        other => return other,
    };
    output.changed_during_run = match drift {
        Some(probe) => probe.report(started.elapsed()).await,
        None => None,
    };
    if handoff_in_result {
        guards.report_handoff_in(&mut output, monitor).await;
    }
    Terminal::Exited(output)
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::operation_monitor::MonitorConfig;
    use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};

    /// The prefix of the one command line a held mutex group stops.
    const GATED: &str = "ahma_drive_gated";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Runner {
        Piped,
        Pty,
        Session,
    }

    /// How a case is brought to its end, once it has been started.
    #[derive(Debug, Clone, Copy)]
    enum Ending {
        Exits(i32),
        TimesOut,
        CancelledRunning,
        CancelledBeforeStart,
        GateGivesUp,
    }

    async fn start(adapter: &Adapter, path: Runner, dir: &str, line: &str, timeout: u64) -> String {
        match path {
            Runner::Piped => {
                let mut args = Map::new();
                args.insert("command".into(), json!(line));
                args.insert("c_flag".into(), json!(true));
                let config = crate::AhmaMcpService::build_shell_subcommand_config(
                    Some(timeout),
                    &ExecutionMode::AsyncResultPush,
                );
                adapter
                    .execute_async_in_dir_with_options(
                        "drive_test",
                        crate::shell_pool::platform_shell_program(),
                        dir,
                        AsyncExecOptions {
                            id: None,
                            args: Some(args),
                            timeout: Some(timeout),
                            subcommand_config: Some(&config),
                            log_monitor_config: None,
                        },
                    )
                    .await
            }
            Runner::Pty => {
                adapter
                    .execute_pty_async("drive_test", line, dir, Some(timeout), None)
                    .await
            }
            Runner::Session => {
                adapter
                    .execute_session_async("drive_test", "drive", line, dir, Some(timeout), None)
                    .await
            }
        }
        .expect("the command is accepted")
    }

    /// Wait until the operation's task has ended: its handle is gone.
    async fn task_gone(adapter: &Adapter, id: &str) {
        let deadline = tokio::time::Instant::now() + TestTimeouts::get(TimeoutCategory::ToolCall);
        while adapter.task_handles.lock().await.contains_key(id) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{id}: the task handle is still registered"
            );
            tokio::time::sleep(TestTimeouts::poll_interval() / 10).await;
        }
    }

    /// SPEC R-HANDOFF.10: every ending of every async path, the command's own
    /// and every way it can be stopped, ends the operation, writes the one
    /// `tool_complete` that pairs with its `tool_call`, with the outcome the
    /// operation ended with, and leaves no task handle behind. The handle is
    /// removed only once the task has returned, so the record is on disk by
    /// then.
    #[tokio::test]
    async fn every_ending_of_every_path_is_audited_once_and_leaves_no_task() {
        let logs = tempfile::tempdir().unwrap();
        let _ = audit::set_audit_log_path(logs.path().join("audit.jsonl"));
        let log = audit::audit_log().path().to_path_buf();

        let group = ahma_common::config::MutexGroupConfig {
            name: "gated".to_string(),
            prefixes: vec![GATED.to_string()],
            max_wait_secs: 0,
        };
        let locks = tempfile::tempdir().unwrap();
        let registry = Arc::new(CommandMutexRegistry::from_config_with_lock_dir(
            std::slice::from_ref(&group),
            Some(locks.path().to_path_buf()),
        ));
        let td = tempfile::tempdir().unwrap();
        let scope = dunce::canonicalize(td.path()).unwrap();
        let dir = scope.to_str().unwrap();
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::get(TimeoutCategory::ToolCall),
        )));
        let sandbox = Arc::new(
            sandbox::Sandbox::new(
                vec![scope.clone()],
                sandbox::SandboxMode::Test,
                Default::default(),
            )
            .unwrap(),
        );
        let adapter = Adapter::new_with_registry(
            monitor.clone(),
            Arc::new(ShellPoolManager::new(
                crate::shell_pool::ShellPoolConfig::default(),
            )),
            sandbox,
            registry.clone(),
        )
        .unwrap();
        // Another command holds the group in this workspace for the whole test.
        let _held = registry
            .acquire(&group, &adapter.workspace_key_for(&scope))
            .await
            .expect("the group is free");

        let mut paths = vec![Runner::Piped, Runner::Session];
        #[cfg(unix)]
        if pty_available() {
            paths.push(Runner::Pty);
        }
        let endings = [
            Ending::Exits(0),
            Ending::Exits(3),
            Ending::TimesOut,
            Ending::CancelledRunning,
            Ending::CancelledBeforeStart,
            Ending::GateGivesUp,
        ];
        for path in paths {
            for ending in endings {
                let case = format!("{path:?} {ending:?}");
                let (line, timeout, state, outcome) = match ending {
                    Ending::Exits(0) => (
                        "echo hi".to_string(),
                        30,
                        OperationStatus::Completed,
                        "completed",
                    ),
                    Ending::Exits(code) if path == Runner::Session => {
                        // `exit` would end the session's shell itself.
                        let _ = code;
                        ("false".to_string(), 30, OperationStatus::Failed, "failed")
                    }
                    Ending::Exits(code) => (
                        format!("exit {code}"),
                        30,
                        OperationStatus::Failed,
                        "failed",
                    ),
                    Ending::TimesOut => (
                        "sleep 30".to_string(),
                        1,
                        OperationStatus::TimedOut,
                        "timed_out",
                    ),
                    Ending::CancelledRunning | Ending::CancelledBeforeStart => (
                        "sleep 30".to_string(),
                        60,
                        OperationStatus::Cancelled,
                        "cancelled",
                    ),
                    Ending::GateGivesUp => {
                        (format!("{GATED} x"), 30, OperationStatus::Failed, "failed")
                    }
                };
                let id = start(&adapter, path, dir, &line, timeout).await;
                match ending {
                    // On this single-threaded runtime the task has not run yet.
                    Ending::CancelledBeforeStart => {
                        assert!(monitor.cancel_operation(&id).await, "{case}");
                    }
                    Ending::CancelledRunning => {
                        let deadline = tokio::time::Instant::now()
                            + TestTimeouts::get(TimeoutCategory::ToolCall);
                        while monitor
                            .get_operation(&id)
                            .await
                            .is_some_and(|op| op.state != OperationStatus::InProgress)
                        {
                            assert!(tokio::time::Instant::now() < deadline, "{case}: never ran");
                            tokio::time::sleep(TestTimeouts::poll_interval() / 10).await;
                        }
                        assert!(monitor.cancel_operation(&id).await, "{case}");
                    }
                    _ => {}
                }
                let op = monitor.wait_for_operation(&id).await.expect("in history");
                assert_eq!(op.state, state, "{case}: {:?}", op.result);
                task_gone(&adapter, &id).await;

                let text = tokio::fs::read_to_string(&log).await.unwrap_or_default();
                let events: Vec<Value> = text
                    .lines()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                    .filter(|e| e["operation_id"] == id.as_str())
                    .collect();
                let kinds: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
                assert_eq!(kinds, ["tool_call", "tool_complete"], "{case}: {events:?}");
                assert_eq!(events[1]["outcome"], outcome, "{case}: {events:?}");
            }
        }
    }
}
