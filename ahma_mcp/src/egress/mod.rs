//! # Egress Sandbox
//!
//! Closes the network egress carve-out that Cowork's model has (web-fetch and
//! MCP connections bypass org egress policies).  The ahma egress sandbox:
//!
//! 1. Runs a lightweight HTTP proxy (`ahma egress`) that each task subprocess
//!    uses via `HTTP_PROXY` / `HTTPS_PROXY` environment variables.
//! 2. Consults a per-vault `egress.allowlist` to decide whether to forward or
//!    reject each request.
//! 3. Defaults to **deny-all** — the cloud LLM domain is only added when the
//!    user explicitly opts the task into a cloud model.
//!
//! ## Security properties
//!
//! - The kernel FS sandbox prevents the subprocess from modifying its own
//!   `/etc/hosts` or `/etc/resolv.conf`, so DNS rebinding cannot be used to
//!   route traffic around the proxy.
//! - The proxy is bound to `127.0.0.1` only; no external network access.
//! - Each task gets a distinct port allocated by the OS (`0`), preventing
//!   cross-task traffic snooping.
//!
//! ## Default allowlist
//!
//! An empty `egress.allowlist` means **no egress** — not even `localhost`.
//! Compile-time built-in additions:
//!
//! | Domain | Added when |
//! |--------|------------|
//! | (none) | Default |
//! | `api.openai.com` | User opts task into OpenAI |
//! | `generativelanguage.googleapis.com` | User opts task into Gemini |
//!
//! ## Environment variables injected into sandboxed subprocesses
//!
//! ```text
//! HTTP_PROXY=http://127.0.0.1:<port>
//! HTTPS_PROXY=http://127.0.0.1:<port>
//! NO_PROXY=127.0.0.1,::1,localhost
//! ```

pub mod allowlist;
pub mod proxy;
pub mod web_audit;
pub mod web_prompt;

pub use allowlist::EgressAllowlist;
pub use proxy::{EgressProxy, EgressProxyConfig};
