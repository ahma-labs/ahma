use anyhow::Result;
use std::path::Path;

pub trait CommandExecutor: Send + Sync + std::fmt::Debug {
    fn build_command(
        &self,
        sandbox: &crate::sandbox::Sandbox,
        program: &str,
        args: &[String],
        working_dir: &Path,
    ) -> Result<tokio::process::Command>;
}

#[derive(Debug, Clone, Default)]
pub struct DefaultCommandExecutor;

impl CommandExecutor for DefaultCommandExecutor {
    fn build_command(
        &self,
        sandbox: &crate::sandbox::Sandbox,
        program: &str,
        args: &[String],
        working_dir: &Path,
    ) -> Result<tokio::process::Command> {
        if program == "/bin/sh" {
            let full_command = args.join(" ");
            sandbox.create_shell_command(program, &full_command, working_dir)
        } else {
            sandbox.create_command(program, args, working_dir)
        }
    }
}
