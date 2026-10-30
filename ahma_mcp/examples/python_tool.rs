//! Python Tool Configuration Example
//!
//! This example demonstrates how to load and validate the python tool configuration
//! from the examples/configs directory.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example python_tool
//! ```

use ahma_mcp::schema_validation::MtdfValidator;
use std::path::PathBuf;

fn main() {
    let config_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/configs/python.json");
    MtdfValidator::run_example_validation("python tool", &config_path);
}
