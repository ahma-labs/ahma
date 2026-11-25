//! Git Tool Configuration Example
//!
//! This example demonstrates how to load and validate the git tool configuration
//! bundled with ahma (the repository's `.ahma/`).
//!
//! # Usage
//!
//! ```bash
//! cargo run --example git_tool
//! ```

use ahma_mcp::schema_validation::MtdfValidator;
use std::path::PathBuf;

fn main() {
    let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.ahma/git.json");
    MtdfValidator::run_example_validation("git tool", &config_path);
}
