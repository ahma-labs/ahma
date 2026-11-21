//! Web links a sandboxed command asks ahma to open in the human's browser
//! (SPEC R-LINK).
//!
//! - [`policy`](crate::links::policy): what counts as a link, who may open it, what a human is
//!   asked — pure decisions, no I/O.
//! - [`helper`](crate::links::helper): the sandboxed side, `BROWSER` / `ahma open-url`; forwards
//!   each argument and prints the broker's line.
//! - `broker` and `host` (Unix): the unsandboxed side, one broker per
//!   command on its own socket, and the factory that gives each command one.
//! - [`consent`](crate::links::consent): who says yes for a hooked command;
//!   `live` (Unix): who says yes on the MCP path, asked while the command
//!   waits; [`opener`](crate::links::opener): what opens an allowed link.

pub mod policy;

pub mod consent;
pub mod helper;
pub mod opener;

#[cfg(unix)]
pub mod broker;
#[cfg(unix)]
pub mod host;
#[cfg(unix)]
pub mod live;
