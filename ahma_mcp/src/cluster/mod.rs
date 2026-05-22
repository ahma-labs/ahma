//! # Local Cluster Scheduler
//!
//! Discovers `ahma worker` peers on the local network (mDNS / Tailscale) and
//! routes sub-tasks from the [`crate::decompose`] orchestrator to the
//! best-available peer for each model size.
//!
//! ## Architecture
//!
//! ```text
//!  ┌─────────────────────────────────────────────────────┐
//!  │ DecomposeOrchestrator (local coordinator)           │
//!  │   - SubTask[0]: needs gemma:4b                      │
//!  │   - SubTask[1]: needs llama3.2:3b                   │
//!  └──────────────────────┬──────────────────────────────┘
//!                         │ schedule()
//!                         ▼
//!  ┌─────────────────────────────────────────────────────┐
//!  │ ClusterScheduler                                    │
//!  │   ┌───────────────────────────────────────┐         │
//!  │   │ WorkerRegistry                        │         │
//!  │   │  worker-A: gemma:4b [free]            │         │
//!  │   │  worker-B: llama3.2:3b [busy]         │         │
//!  │   │  worker-C: gemma:4b [free]            │         │
//!  │   └───────────────────────────────────────┘         │
//!  └───────────────────┬─────────────────────────────────┘
//!                      │ HTTP POST task manifest (signed)
//!                      ▼
//!  ┌────────────────┐  ┌────────────────┐  ┌────────────────┐
//!  │ worker-A        │  │ worker-B        │  │ worker-C        │
//!  │ (this machine)  │  │ (LAN peer)      │  │ (Tailscale)    │
//!  │ sandbox: vault/ │  │ sandbox: vault/ │  │ sandbox: vault/ │
//!  └────────────────┘  └────────────────┘  └────────────────┘
//! ```
//!
//! ## Security
//!
//! - Workers only accept **signed task manifests** from known peers.
//! - The task vault `workdir/` is the unit of work that travels; inputs are
//!   copied over QUIC (Ahma's HTTP/3 preference).
//! - Each remote worker runs under its own kernel sandbox.
//!
//! ## Status
//!
//! The `in-progress` feature flag controls compilation of this module.
//! Full mDNS peer discovery requires `mdns-sd` crate (not yet in workspace).

pub mod discovery;
pub mod scheduler;

pub use discovery::{PeerInfo, WorkerRegistry};
pub use scheduler::ClusterScheduler;
