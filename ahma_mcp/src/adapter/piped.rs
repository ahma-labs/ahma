//! The one way a piped command's process is made ready to spawn, for
//! [`super::drive`]'s piped runner.
//!
//! The synchronous path and the async one used to write the same steps out by
//! hand — lease the link broker, build the sandboxed command, stamp the
//! workspace lease, keep what a declared long-running command leaves behind,
//! point it at its SSH key and link brokers, read its kernel-denial tag — and
//! each new step had to be added twice. [`PipedSpawn::build`] does them once,
//! in one order, and puts what the command holds until it ends in its
//! [`RunGuards`]. Since there is one execution path (SPEC R2.1), what the
//! brokers refused is an alert while the command runs, and also lines in the
//! result of a caller that waits for it.

use std::path::Path;

use anyhow::Result;

use super::guards::RunGuards;
use super::{build_for_this_command, executor, workspace_queue};
use crate::sandbox;

/// What a piped command is built from.
pub(super) struct PipedSpawn<'a> {
    pub(super) executor: &'a dyn executor::CommandExecutor,
    pub(super) sandbox: &'a sandbox::Sandbox,
    /// Serves the command an SSH key broker (SPEC R-CRED.1).
    #[cfg(unix)]
    pub(super) credential_brokers:
        Option<&'a std::sync::Arc<dyn crate::credentials::ssh_agent::consent::BrokerFactory>>,
    /// Serves the command a link broker (SPEC R-LINK.1).
    #[cfg(unix)]
    pub(super) link_brokers: Option<&'a std::sync::Arc<dyn crate::links::host::LinkBrokerFactory>>,
    pub(super) program: &'a str,
    pub(super) args: &'a [String],
    pub(super) working_dir: &'a Path,
    /// A read-only command is built for the read-only lane (SPEC R2.7.4).
    pub(super) lane: workspace_queue::Lane,
    /// What its brokers are leased for (SPEC R-CRED.1, R-LINK.1): the line
    /// as the caller wrote it, not the link helper's line a whole-line
    /// `open <URL>` runs as.
    pub(super) line: &'a str,
    /// A human declared it long-running (SPEC R2.7.10), so what it starts may
    /// stay running after it (R-PROC.5).
    pub(super) keep_leftovers: bool,
}

/// A piped command ready to spawn; its brokers are held by its guards.
pub(super) struct PipedCommand {
    pub(super) cmd: tokio::process::Command,
    /// The tag the kernel's records of its denials carry (SPEC R-DENY.1).
    pub(super) denial_tag: Option<String>,
}

impl PipedSpawn<'_> {
    /// Build the command, and hold its brokers in `guards` until it has
    /// ended. `Err` when its sandbox could not be built: it cannot be
    /// started.
    pub(super) fn build(self, guards: &mut RunGuards) -> Result<PipedCommand> {
        let Self {
            executor,
            sandbox,
            #[cfg(unix)]
            credential_brokers,
            #[cfg(unix)]
            link_brokers,
            program,
            args,
            working_dir,
            lane,
            line,
            keep_leftovers,
        } = self;
        #[cfg(not(unix))]
        let _ = line;
        // Its link broker, leased before its sandbox is built in either lane:
        // having one is what lets this command, and no other, reach the
        // broker directory (SPEC R-LINK.1).
        #[cfg(unix)]
        let links = link_brokers.and_then(|f| f.lease_for_command(working_dir, Some(line)));
        #[cfg(unix)]
        let served_links = links.is_some();
        #[cfg(not(unix))]
        let served_links = false;
        // `build_command` is the one entry point for every program: the
        // preparer already embeds a shell's `-c`/`-Command`, so nothing here
        // adds it twice.
        let mut cmd = build_for_this_command(served_links, || {
            if lane == workspace_queue::Lane::ReadOnly {
                executor.build_read_only_command(sandbox, program, args, working_dir)
            } else {
                executor.build_command(sandbox, program, args, working_dir)
            }
        })?;
        // An ahma it starts does not wait for the lease it runs under
        // (SPEC R2.7.7).
        stamp_lease(&mut cmd, guards.lease());
        if keep_leftovers {
            sandbox::leftovers::keep(&mut cmd);
        }
        // Its SSH key broker (SPEC R-CRED.1).
        #[cfg(unix)]
        guards.hold_broker(
            credential_brokers
                .and_then(|f| f.lease_for_command(working_dir, Some(line)))
                .inspect(|broker| {
                    cmd.env("SSH_AUTH_SOCK", broker.socket());
                }),
        );
        // The link broker's helper and socket (SPEC R-LINK.1).
        #[cfg(unix)]
        guards.hold_links(links.inspect(|links| {
            for (key, value) in links.env() {
                cmd.env(key, value);
            }
        }));
        let denial_tag = sandbox::kernel_denials::tag_of(cmd.as_std().get_args());
        Ok(PipedCommand { cmd, denial_tag })
    }
}

/// Stamp a child spawned under a lease with it (SPEC R2.7.7), so an ahma the
/// command itself starts does not wait for the lease its ancestor holds.
fn stamp_lease(cmd: &mut tokio::process::Command, lease: Option<&workspace_queue::Lease>) {
    if let Some(lease) = lease {
        cmd.env(
            workspace_queue::HELD_LEASE_ENV,
            workspace_queue::child_lease_env(lease.key()),
        );
    }
}
