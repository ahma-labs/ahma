//! Python Tool Configuration Example
//!
//! This example demonstrates how to load and validate the python tool configuration
//! bundled with ahma (the repository's `.ahma/`).
//!
//! # Usage
//!
//! ```bash
//! cargo run --example python_tool
//! ```

use ahma_mcp::schema_validation::MtdfValidator;
use std::path::PathBuf;

fn main() {
    let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.ahma/python.json");
    MtdfValidator::run_example_validation("python tool", &config_path);
}
