//! Kotlin/Android Tool Configuration Example
//!
//! This example demonstrates how to load and validate the Kotlin/Android (Gradle Wrapper) tool configuration
//! from the examples/configs directory.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example kotlin_tool
//! ```

use ahma_mcp::schema_validation::MtdfValidator;
use std::path::PathBuf;

fn main() {
    let config_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/configs/kotlin.json");
    MtdfValidator::run_example_validation("Kotlin/Android tool", &config_path);
}
