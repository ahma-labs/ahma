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
//! - The allowlist check is on the hostname, but the proxy then **resolves the
//!   host and refuses any connection to a private/loopback/link-local/cloud-
//!   metadata IP** (`block_private`, on by default). It connects to the exact
//!   address it vetted, so an allowlisted domain cannot DNS-rebind to
//!   `127.0.0.1` or `169.254.169.254` between the check and the connect. This
//!   reuses [`ahma_harness_tools::egress_guard::is_blocked_ip`], the same guard
//!   `fetch_webpage` uses.
//! - The kernel FS sandbox additionally prevents the subprocess from modifying
//!   its own `/etc/hosts` or `/etc/resolv.conf`.
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
