//! File Tools Configuration Example
//!
//! This example demonstrates how to load and validate the file-tools configuration
//! bundled with ahma (the repository's `.ahma/`).
//!
//! # Usage
//!
//! ```bash
//! cargo run --example file-tools
//! ```

use ahma_mcp::schema_validation::MtdfValidator;
use std::path::PathBuf;

fn main() {
    let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.ahma/file-tools.json");
    MtdfValidator::run_example_validation("file-tools", &config_path);
}
