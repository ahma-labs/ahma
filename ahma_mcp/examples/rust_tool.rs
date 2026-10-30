//! Cargo Tool Configuration Example
//!
//! This example demonstrates how to load and validate the cargo tool configuration
//! from the examples/configs directory.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example cargo_tool
//! ```

use ahma_mcp::schema_validation::MtdfValidator;
use std::path::PathBuf;

fn main() {
    let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/configs/cargo.json");
    MtdfValidator::run_example_validation("cargo tool", &config_path);
}
