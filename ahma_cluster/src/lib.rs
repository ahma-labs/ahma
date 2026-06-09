//! # ahma_cluster — Local cluster scheduler
//!
//! Discovers `ahma worker` peers on the local network (mDNS / Tailscale) and
//! routes sub-tasks from the decompose orchestrator to the best-available peer.
//!
//! ## Security
//!
//! All inter-node communication is authenticated with **HMAC-SHA256**:
//!
//! - [`TaskManifest`] is signed before dispatch and verified (with replay
//!   protection via [`scheduler::NonceCache`]) on receipt.
//! - Worker heartbeats are signed and verified using
//!   [`WorkerRegistry::receive_signed_heartbeat`], which uses constant-time
//!   comparison to prevent timing side-channels.
//!
//! ## License
//!
//! This crate is licensed under **AGPL-3.0-or-later**.  Because this crate
//! implements network-callable functionality (peer scheduling over HTTP/QUIC),
//! AGPL §13 applies: any modified version offered to remote users over a network
//! must provide access to its modified source code.

pub mod discovery;
/// MCP-based cluster peer dispatch (P3).
pub mod mcp_dispatch;
pub mod scheduler;
/// Deterministic multi-node simulation harness for cluster tests (P4).
pub mod test_grid;
pub mod tls;
pub mod transport;

pub use ahma_common::config::TransportMode;
pub use ahma_common::peer_transport::{InMemoryPeerDispatch, PeerDispatch};
pub use discovery::{PeerInfo, WorkerRegistry};
pub use mcp_dispatch::McpPeerDispatch;
pub use scheduler::{ClusterScheduler, TaskManifest, TaskResult};
pub use tls::{ClusterTlsConfig, generate_self_signed_cluster_certs, load_from_dir};
pub use transport::{ClusterTransport, default_transport_preference, new_cluster_dispatch};
