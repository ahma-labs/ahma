//! # Network egress
//!
//! With `--restrict-network` (or `[network] restrict = true`) every sandboxed
//! subprocess is pointed at a local HTTP proxy through `HTTP_PROXY` /
//! `HTTPS_PROXY`, and the proxy forwards only hosts on the effective allowlist:
//! the operator's `[network] allow` plus the hosts each enabled sandbox profile
//! declares (SPEC R-WEB.16, R-PERM.5.3). Restriction is off by default. The same
//! module holds the approval prompts for `fetch_webpage` (R-WEB).
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
//! - Each server gets its own OS-allocated port (`0`).
//!
//! ## Default allowlist
//!
//! An empty effective allowlist forwards nothing. Loopback is not proxied at
//! all (`NO_PROXY`, below).
//!
//! ## Where the server's allowlist comes from
//!
//! The allowlist is the union computed by [`host_grants::EgressGrants`](crate::egress::host_grants::EgressGrants):
//! the operator's `[network] allow` plus the hostnames each **enabled sandbox
//! profile** declares for its toolchain. Read that module first — it explains why
//! restriction stayed unused without it, and why the default is still off.
//! [`host_pattern::HostPattern`](crate::egress::host_pattern::HostPattern) is the single matcher.
//!
//! ## Environment variables injected into sandboxed subprocesses
//!
//! ```text
//! HTTP_PROXY=http://127.0.0.1:<port>
//! HTTPS_PROXY=http://127.0.0.1:<port>
//! NO_PROXY=127.0.0.1,::1,localhost
//! ```

pub mod allowlist;
pub mod host_grants;
pub mod host_pattern;
pub mod net_prompt;
pub mod proxy;
pub mod web_audit;
pub mod web_prompt;

pub use allowlist::EgressAllowlist;
pub use host_grants::{EgressGrantSources, EgressGrants, GrantSource, HostGrant};
pub use host_pattern::HostPattern;
pub use proxy::{EgressProxy, EgressProxyConfig, NetApprovalContext};
