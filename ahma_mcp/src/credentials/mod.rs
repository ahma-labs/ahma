//! Credentials a sandboxed command uses without reading them (SPEC R-CRED).
//!
//! The sandbox never lets a command read a credential file, and no grant can
//! change that. What a command needs is to *use* a credential — to sign in to
//! a server — so the parts here run outside the sandbox and do that on its
//! behalf, for a destination a human allowed, and audit every use.
//!
//! `host` (Unix) is what every broker shares: the thread brokers run on and
//! the per-command socket each is served on, which admits only a command this
//! process spawned. [`ssh_agent`](crate::credentials::ssh_agent) is the SSH key
//! broker.

#[cfg(unix)]
pub mod host;
pub mod ssh_agent;
