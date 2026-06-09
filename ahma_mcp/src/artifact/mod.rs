//! # Interactive HTML Artifact Channel
//!
//! Tools can emit self-contained HTML artifacts to `outputs/result.html` inside
//! the vault.  Each artifact:
//!
//! - Renders tool output data embedded as JSON in the HTML.
//! - Includes a chat widget backed by the local Ollama / OpenAI-compatible
//!   endpoint — the user keeps iterating **without** re-engaging the orchestrator.
//! - Talks back to a per-task localhost API server (short-lived bearer token).
//! - Survives as part of the vault (auditable, re-openable).
//!
//! ## Example
//!
//! ```no_run
//! use ahma_mcp::artifact::ArtifactBuilder;
//!
//! let html = ArtifactBuilder::new("Q4 Revenue Analysis")
//!     .data(serde_json::json!({"revenue": 42000, "growth": "12%"}))
//!     .chat_endpoint("http://localhost:11434/v1")
//!     .chat_model("llama3.2")
//!     .build();
//!
//! html.save(std::path::Path::new("outputs/result.html"))?;
//! # Ok::<(), anyhow::Error>(())
//! ```

pub mod html;
pub mod server;

pub use html::{ArtifactBuilder, ArtifactHtml};
pub use server::ArtifactServer;
