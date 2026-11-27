//! Handler for the built-in `ahma_command` MCP tool (SPEC R-PROC.7).
//!
//! A few of ahma's own commands cannot run inside the sandbox an agent's
//! commands run in: `ahma update` writes a folder on PATH and asks the hub to
//! hand over, `ahma web` and `ahma desktop` reach the hub socket in `~/.ahma`,
//! and `ahma doctor` reads ahma's settings there. The unconfined ahma runs
//! them itself, from a closed list. The agent picks an entry and nothing
//! else: the program is ahma's own executable, the arguments are fixed, and
//! the environment is ahma's, not the command's.

use super::common::{mcp_invalid_params, text_result};
use crate::AhmaMcpService;
use crate::operation_monitor::{CommandOutput, Operation, Terminal};
use rmcp::model::{CallToolResult, ErrorData as McpError};
use serde_json::{Map, Value};
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The commands on the list, in the order the schema names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AhmaCommand {
    Update,
    Web,
    Desktop,
    Doctor,
}

impl AhmaCommand {
    const ALL: [AhmaCommand; 4] = [Self::Update, Self::Web, Self::Desktop, Self::Doctor];

    fn name(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Web => "web",
            Self::Desktop => "desktop",
            Self::Doctor => "doctor",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }
}

/// What the agent is told when the command ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// The command's own output: written for whoever runs it.
    Output,
    /// A fixed sentence, never the child's output. `ahma web` prints its
    /// address and handles a pairing code, which must never reach an agent:
    /// whoever holds the code drives the page, and the page answers grant
    /// prompts (R-GUI.2).
    Fixed {
        done: &'static str,
        failed: &'static str,
    },
}

/// How one command runs: its arguments after ahma's executable, what the
/// agent gets back, and how long it may take.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Plan {
    args: Vec<OsString>,
    answer: Answer,
    timeout: Duration,
}

/// The sentence for a command ahma will not run here, naming who can.
fn human_runs(command: AhmaCommand, why: &str) -> String {
    format!(
        "{why} A human runs `ahma {}` in their own terminal (in Claude Code: `! ahma {}`).",
        command.name(),
        command.name()
    )
}

/// Decide how `command` runs, or why it does not (SPEC R-PROC.7).
///
/// `confined` is whether this ahma is itself inside a sandbox, where what it
/// started would be confined too. `os` is `std::env::consts::OS`.
/// `workspace` is the session's first scope: the folder `web` and `desktop`
/// open.
fn plan(
    command: AhmaCommand,
    confined: bool,
    os: &str,
    workspace: Option<&Path>,
) -> Result<Plan, String> {
    if confined {
        return Err(human_runs(
            command,
            "ahma is itself running inside a sandbox here, so anything it started would be \
             confined too.",
        ));
    }
    match command {
        AhmaCommand::Update => Ok(Plan {
            args: vec!["update".into(), "--only-newer".into()],
            answer: Answer::Output,
            timeout: Duration::from_secs(600),
        }),
        AhmaCommand::Doctor => Ok(Plan {
            args: vec!["doctor".into()],
            answer: Answer::Output,
            timeout: Duration::from_secs(120),
        }),
        AhmaCommand::Web | AhmaCommand::Desktop => {
            // Only macOS's sandbox keeps the pairing code on the clipboard
            // from every sandboxed command (R6.2.15). `desktop` falls back to
            // `web` where it has no window, so the same rule holds for it.
            if os != "macos" {
                return Err(human_runs(
                    command,
                    "On this system a sandboxed command could read the pairing code from the \
                     clipboard, so ahma does not open it for an agent.",
                ));
            }
            let Some(workspace) = workspace else {
                return Err(human_runs(
                    command,
                    "This session has no workspace folder yet.",
                ));
            };
            let (done, failed) = if command == AhmaCommand::Web {
                (
                    "Opened ahma in your browser for this workspace. The pairing code is on \
                     your clipboard: paste it into the page. It works once, for two minutes.",
                    "`ahma web` did not start.",
                )
            } else {
                (
                    "Opened ahma in a window of its own for this workspace. It stops when \
                     you close the window.",
                    "`ahma desktop` did not start.",
                )
            };
            Ok(Plan {
                args: vec![command.name().into(), workspace.as_os_str().to_owned()],
                answer: Answer::Fixed { done, failed },
                timeout: Duration::from_secs(60),
            })
        }
    }
}

/// The child process for `plan`: ahma's own executable with the plan's
/// arguments, and ahma's own environment less what marks a process as a
/// sandboxed command's (R-PROC.5) or as a hub's session worker.
fn child(exe: &Path, plan: &Plan) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(&plan.args)
        .env_remove(ahma_common::process_guard::SANDBOX_RUN_ENV)
        .env_remove("AHMA_SERVER_CHILD")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// What the agent is told for a command that exited with `status` after
/// printing `stdout` and `stderr`.
fn answer_text(
    command: AhmaCommand,
    answer: Answer,
    ok: bool,
    stdout: &str,
    stderr: &str,
) -> String {
    match answer {
        Answer::Output => {
            let mut text = String::new();
            for part in [stdout.trim(), stderr.trim()] {
                if !part.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(part);
                }
            }
            if text.is_empty() {
                text = format!("`ahma {}` printed nothing.", command.name());
            }
            text
        }
        Answer::Fixed { done, .. } if ok => done.to_string(),
        Answer::Fixed { failed, .. } => human_runs(command, failed),
    }
}

/// Whether this ahma is inside a sandbox: a kernel profile on it, or the
/// stamp ahma puts on every command it sandboxes (R-PROC.5).
fn ahma_is_confined() -> bool {
    ahma_common::seatbelt::this_process_is_confined()
        || std::env::var_os(ahma_common::process_guard::SANDBOX_RUN_ENV).is_some()
}

impl AhmaMcpService {
    /// `ahma_command`: run one of ahma's own commands outside the sandbox
    /// (SPEC R-PROC.7), as a tracked operation.
    pub async fn handle_ahma_command(
        &self,
        args: Map<String, Value>,
        context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(extra) = args.keys().find(|k| k.as_str() != "command") {
            return Err(mcp_invalid_params(format!(
                "ahma_command takes only `command`, not `{extra}`"
            )));
        }
        let name = super::common::require_str(&args, "command", "command is required")?;
        let command = AhmaCommand::from_name(&name).ok_or_else(|| {
            mcp_invalid_params(format!(
                "unknown command `{name}`; one of: update, web, desktop, doctor"
            ))
        })?;
        let workspace = self.adapter.sandbox().scopes().first().cloned();
        let plan = match plan(
            command,
            ahma_is_confined(),
            std::env::consts::OS,
            workspace.as_deref(),
        ) {
            Ok(plan) => plan,
            Err(refusal) => return Ok(text_result(refusal)),
        };
        let exe = std::env::current_exe()
            .map_err(|e| super::common::mcp_internal(format!("ahma's own executable: {e}")))?;

        let id = crate::utils::operation::mint_id("ahma_command", command.name());
        let operation = Operation::new_with_timeout(
            id.clone(),
            "ahma_command".to_string(),
            format!("ahma {}", command.name()),
            None,
            Some(plan.timeout),
        );
        let monitor = self.operation_monitor.clone();
        let cancel = monitor.register(operation).await;
        let client_type = crate::client_type::McpClientType::from_peer(&context.peer);
        self.register_progress_if_requested(
            &id,
            context.peer.clone(),
            context.meta.get_progress_token(),
            client_type,
        )
        .await;

        let mut cmd = child(&exe, &plan);
        let task_monitor = monitor.clone();
        let task_id = id.clone();
        tokio::spawn(async move {
            task_monitor.start(&task_id).await;
            let terminal = match cmd.spawn() {
                Err(e) => Terminal::Failed(format!("could not start ahma: {e}")),
                Ok(child) => tokio::select! {
                    output = tokio::time::timeout(plan.timeout, child.wait_with_output()) => {
                        match output {
                            Err(_) => Terminal::TimedOut(format!(
                                "`ahma {}` did not finish within {}s",
                                command.name(),
                                plan.timeout.as_secs()
                            )),
                            Ok(Err(e)) => Terminal::Failed(format!("ahma: {e}")),
                            Ok(Ok(output)) => {
                                let ok = output.status.success();
                                let text = answer_text(
                                    command,
                                    plan.answer,
                                    ok,
                                    &String::from_utf8_lossy(&output.stdout),
                                    &String::from_utf8_lossy(&output.stderr),
                                );
                                Terminal::Exited(Box::new(CommandOutput {
                                    stdout: text,
                                    exit_code: output.status.code(),
                                    ..Default::default()
                                }))
                            }
                        }
                    }
                    _ = cancel.cancelled() => Terminal::Cancelled("cancelled".to_string()),
                },
            };
            task_monitor.finish(&task_id, terminal).await;
        });

        if let Some(result) = super::common::try_automatic_async_completion(
            &self.operation_monitor,
            &id,
            self.effective_request_budget(client_type),
        )
        .await
        {
            return Ok(result);
        }
        self.undelivered.started_without_result(&id);
        Ok(text_result(format!(
            "{}{id}{}",
            crate::utils::operation::STARTED_PREFIX,
            crate::tool_hints::preview(&id, "ahma_command")
        )))
    }
}

/// `{"command": "update" | "web" | "desktop" | "doctor"}`.
pub fn ahma_command_schema() -> Arc<Map<String, Value>> {
    let names: Vec<&str> = AhmaCommand::ALL.iter().map(|c| c.name()).collect();
    let mut properties = Map::new();
    properties.insert(
        "command".to_string(),
        crate::mcp_service::schema::enum_string_property(
            "Which of ahma's own commands to run: `update` installs the latest signed ahma \
             release if it is newer; `web` opens ahma in the browser (macOS); `desktop` opens \
             it in a window (macOS); `doctor` reports on ahma's health.",
            &names,
        ),
    );
    crate::mcp_service::schema::object_input_schema(properties, &["command"])
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = "/work/space";

    fn planned(command: AhmaCommand) -> Plan {
        plan(command, false, "macos", Some(Path::new(WS))).expect("planned")
    }

    #[test]
    fn the_schema_offers_the_four_commands_and_only_them() {
        let schema = ahma_command_schema();
        assert_eq!(
            schema["properties"]["command"]["enum"],
            serde_json::json!(["update", "web", "desktop", "doctor"])
        );
        assert_eq!(schema["required"], serde_json::json!(["command"]));
        assert_eq!(schema["properties"].as_object().unwrap().len(), 1);
    }

    /// The agent chooses an entry, never the arguments: `update` is only the
    /// forward-only one (no branch, no older release, no other directory,
    /// no skipped verification).
    #[test]
    fn each_command_runs_its_fixed_arguments() {
        let args = |c| -> Vec<String> {
            planned(c)
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(args(AhmaCommand::Update), ["update", "--only-newer"]);
        assert_eq!(args(AhmaCommand::Doctor), ["doctor"]);
        assert_eq!(args(AhmaCommand::Web), ["web", WS]);
        assert_eq!(args(AhmaCommand::Desktop), ["desktop", WS]);
    }

    #[test]
    fn a_confined_ahma_names_the_line_a_human_runs() {
        for command in AhmaCommand::ALL {
            let refusal = plan(command, true, "macos", Some(Path::new(WS))).unwrap_err();
            assert!(
                refusal.contains(&format!("! ahma {}", command.name())),
                "{refusal}"
            );
        }
    }

    /// Off macOS a sandboxed command can read the clipboard, and with it the
    /// pairing code; `desktop` falls back to `web` where it has no window.
    #[test]
    fn web_and_desktop_are_opened_for_an_agent_only_on_macos() {
        for os in ["linux", "windows"] {
            for command in [AhmaCommand::Web, AhmaCommand::Desktop] {
                assert!(
                    plan(command, false, os, Some(Path::new(WS))).is_err(),
                    "{os}"
                );
            }
            assert!(plan(AhmaCommand::Update, false, os, None).is_ok());
            assert!(plan(AhmaCommand::Doctor, false, os, None).is_ok());
        }
        assert!(plan(AhmaCommand::Web, false, "macos", None).is_err());
    }

    /// The pairing code never reaches the agent, whatever the child printed
    /// and however it ended.
    #[test]
    fn web_never_relays_what_the_child_printed() {
        let answer = planned(AhmaCommand::Web).answer;
        for ok in [true, false] {
            let text = answer_text(AhmaCommand::Web, answer, ok, "code K7Q2-ZP9M", "K7Q2-ZP9M");
            assert!(!text.contains("K7Q2"), "{text}");
        }
    }

    #[test]
    fn update_and_doctor_relay_their_own_report() {
        let answer = planned(AhmaCommand::Doctor).answer;
        let text = answer_text(AhmaCommand::Doctor, answer, true, "hub: running\n", "");
        assert_eq!(text, "hub: running");
        let text = answer_text(AhmaCommand::Update, answer, false, "", "no release");
        assert_eq!(text, "no release");
    }

    /// The child gets ahma's environment, never the stamp that marks a
    /// sandboxed command or a hub's session worker.
    #[test]
    fn the_child_is_ahma_itself_without_the_sandbox_stamps() {
        let cmd = child(Path::new("/opt/ahma"), &planned(AhmaCommand::Doctor));
        let std = cmd.as_std();
        assert_eq!(std.get_program(), "/opt/ahma");
        let removed: Vec<_> = std
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        for stamp in [
            ahma_common::process_guard::SANDBOX_RUN_ENV,
            "AHMA_SERVER_CHILD",
        ] {
            assert!(removed.iter().any(|k| k == stamp), "{stamp}: {removed:?}");
        }
    }
}
