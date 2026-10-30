//! GitHub CLI Tool Configuration Example
//!
//! This example demonstrates how to load and validate the gh (GitHub CLI) tool configuration
//! from the examples/configs directory.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example gh_tool
//! ```

use ahma_mcp::schema_validation::MtdfValidator;
use std::path::PathBuf;

fn main() {
    let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/configs/gh.json");
    MtdfValidator::run_example_validation("gh tool", &config_path);
}
