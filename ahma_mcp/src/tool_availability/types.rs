use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::config::{SubcommandConfig, ToolConfig};
use crate::tool_availability::{DisabledSubcommand, DisabledTool};

fn find_subcommand_mut_in<'a>(
    subcommands: &'a mut [SubcommandConfig],
    path: &[String],
) -> Option<&'a mut SubcommandConfig> {
    let (segment, rest) = path.split_first()?;
    let sub = subcommands.iter_mut().find(|s| s.name == *segment)?;

    if rest.is_empty() {
        return Some(sub);
    }

    let children = sub.subcommand.as_mut()?;
    find_subcommand_mut_in(children.as_mut_slice(), rest)
}

#[derive(Debug, Clone)]
pub(super) enum ProbeTarget {
    Tool { name: String },
    Subcommand { tool: String, path: Vec<String> },
}

#[derive(Debug, Clone)]
pub(super) struct ProbePlan {
    pub(super) target: ProbeTarget,
    pub(super) command: Vec<String>,
    pub(super) working_dir: PathBuf,
    pub(super) success_codes: Vec<i32>,
    pub(super) timeout_ms: u64,
    pub(super) install_instructions: Option<String>,
}

#[derive(Debug)]
pub(super) struct ProbeOutcome {
    pub(super) plan: ProbePlan,
    pub(super) success: bool,
    pub(super) exit_code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

impl ProbeTarget {
    pub(super) fn tool_name(&self) -> &str {
        match self {
            ProbeTarget::Tool { name } => name,
            ProbeTarget::Subcommand { tool, .. } => tool,
        }
    }

    pub(super) fn label(&self) -> String {
        match self {
            ProbeTarget::Tool { name } => format!("Tool '{}'", name),
            ProbeTarget::Subcommand { tool, path } => {
                format!("Subcommand '{}::{}'", tool, path.join("_"))
            }
        }
    }

    pub(super) fn install_label(&self) -> String {
        match self {
            ProbeTarget::Tool { name } => name.clone(),
            ProbeTarget::Subcommand { tool, path } => format!("{}::{}", tool, path.join("_")),
        }
    }
}

impl ProbePlan {
    pub(super) async fn execute(self, sandbox: &crate::sandbox::Sandbox) -> ProbeOutcome {
        let (exit_code, stdout, stderr) = self.execute_direct(sandbox).await;
        let success = self.success_codes.contains(&exit_code);

        ProbeOutcome {
            plan: self,
            success,
            exit_code: Some(exit_code),
            stdout,
            stderr,
        }
    }

    /// Run the probe command directly inside the sandbox, returning
    /// `(exit_code, stdout, stderr)`.
    async fn execute_direct(&self, sandbox: &crate::sandbox::Sandbox) -> (i32, String, String) {
        let (program, args) = self.prepare_direct_command();

        let mut command = match sandbox.create_command(&program, &args, &self.working_dir) {
            Ok(cmd) => cmd,
            Err(e) => {
                return (
                    1,
                    String::new(),
                    format!("Failed to create sandboxed command: {e}"),
                );
            }
        };
        command.kill_on_drop(true);

        let timeout_duration = Duration::from_millis(self.timeout_ms);
        let result = timeout(timeout_duration, command.output()).await;

        self.process_direct_output(result)
    }

    fn prepare_direct_command(&self) -> (String, Vec<String>) {
        let mut cmd_iter = self.command.iter();
        let program = cmd_iter
            .next()
            .cloned()
            .unwrap_or_else(|| "true".to_string());
        let args: Vec<String> = cmd_iter.cloned().collect();
        (program, args)
    }

    fn process_direct_output(
        &self,
        result: Result<
            std::result::Result<std::process::Output, std::io::Error>,
            tokio::time::error::Elapsed,
        >,
    ) -> (i32, String, String) {
        match result {
            Ok(Ok(output)) => (
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stdout).to_string(),
                String::from_utf8_lossy(&output.stderr).to_string(),
            ),
            Ok(Err(err)) => (-1, String::new(), err.to_string()),
            Err(_) => (
                -1,
                String::new(),
                format!("probe timed out after {}ms", self.timeout_ms),
            ),
        }
    }
}

impl ProbeOutcome {
    pub(super) fn update_config(&self, configs: &mut HashMap<String, ToolConfig>) {
        if self.success {
            return;
        }

        let tool_name = self.plan.target.tool_name();
        let config = match configs.get_mut(tool_name) {
            Some(c) => c,
            None => return,
        };

        match &self.plan.target {
            ProbeTarget::Tool { .. } => {
                config.enabled = false;
            }
            ProbeTarget::Subcommand { path, .. } => {
                if let Some(subcommands) = config.subcommand.as_mut()
                    && let Some(sub) = find_subcommand_mut_in(subcommands, path)
                {
                    sub.enabled = false;
                }
            }
        }
    }

    pub(super) fn to_disabled_items(&self) -> (Option<DisabledTool>, Option<DisabledSubcommand>) {
        if self.success {
            return (None, None);
        }

        let label = self.plan.target.label();
        let install_label = self.plan.target.install_label();
        let message = self.log_and_build_message(&label, &install_label);
        let instructions = self.plan.install_instructions.clone();

        match &self.plan.target {
            ProbeTarget::Tool { name } => (
                Some(DisabledTool {
                    name: name.clone(),
                    message,
                    install_instructions: instructions,
                }),
                None,
            ),
            ProbeTarget::Subcommand { tool, path: _path } => (
                None,
                Some(DisabledSubcommand {
                    tool: tool.clone(),
                    subcommand_path: install_label
                        .strip_prefix(&format!("{}::", tool))
                        .unwrap_or(&install_label)
                        .to_string(),
                    message,
                    install_instructions: instructions,
                }),
            ),
        }
    }

    fn log_and_build_message(&self, label: &str, install_label: &str) -> String {
        let stdout = if self.stdout.trim().is_empty() {
            "<empty>"
        } else {
            self.stdout.trim()
        };
        let stderr = if self.stderr.trim().is_empty() {
            "<empty>"
        } else {
            self.stderr.trim()
        };

        let message = format!(
            "{} disabled. Probe command {:?} failed with exit {:?}. stdout: {} stderr: {}",
            label, self.plan.command, self.exit_code, stdout, stderr
        );
        warn!("{message}");
        if let Some(instructions) = self.plan.install_instructions.as_deref() {
            info!(
                "Install hint for '{}': {}",
                install_label,
                instructions.trim()
            );
        }
        message
    }

    pub(super) fn log_success(&self) {
        if !self.success {
            return;
        }
        let stdout = self.stdout.trim();
        let stderr = self.stderr.trim();
        let stdout = if stdout.is_empty() { "<empty>" } else { stdout };
        let stderr = if stderr.is_empty() { "<empty>" } else { stderr };
        debug!(
            "Probe success for {:?} (exit {:?}) stdout: {} stderr: {}",
            self.plan.target, self.exit_code, stdout, stderr,
        );
    }
}
