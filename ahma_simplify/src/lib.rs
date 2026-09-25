//! # ahma_simplify
//!
//! Code complexity analysis behind `ahma simplify`: scores every source file in a
//! project (Rust via `rust-code-analysis`; Kotlin, Swift and a dozen other
//! languages through external analyzers with a Lizard fallback), ranks the
//! hotspots, and renders a Markdown/HTML report plus a structured AI fix prompt.
//!
//! This crate is deliberately independent of the MCP engine (`ahma_mcp`): it is an
//! optional dependency of the `ahma` binary (`ahma_bin` feature `simplify`, on by
//! default; `--no-default-features` drops it), so a lean binary carries none of
//! the analysis toolchain. The clap argument struct lives in
//! [`ahma_common::simplify_args`] so the CLI can reserve the subcommand without
//! this dependency; it is re-exported here as [`SimplifyArgs`].

pub mod analysis;
pub mod auto;
pub mod models;
pub mod report;
pub mod subcommand;

pub use ahma_common::simplify_args::{DEFAULT_EXTENSIONS, SimplifyArgs};
pub use auto::{PrioritizedFix, create_auto_report_md, prioritize_fixes};
pub use subcommand::run;
