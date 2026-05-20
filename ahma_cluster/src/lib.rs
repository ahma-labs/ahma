//! # ahma_cluster — Local cluster scheduler
//!
//! Discovers `ahma worker` peers on the local network (mDNS / Tailscale) and
//! routes sub-tasks from the decompose orchestrator to the best-available peer.
//!
//! ## License
//!
//! This crate is licensed under **AGPL-3.0-or-later**.  Because this crate
//! implements network-callable functionality (peer scheduling over HTTP/QUIC),
//! AGPL §13 applies: any modified version offered to remote users over a network
//! must provide access to its modified source code.

pub mod discovery;
pub mod scheduler;

pub use discovery::{PeerInfo, WorkerRegistry};
pub use scheduler::{ClusterScheduler, TaskManifest, TaskResult};
