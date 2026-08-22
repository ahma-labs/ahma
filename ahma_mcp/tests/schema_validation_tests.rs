//! Consolidated schema_validation tests
//!
//! This module consolidates all schema_validation tests into a single test binary
//! to optimize cargo nextest test discovery time.

pub use ahma_mcp::test_utils as common;

#[path = "schema_validation/basic.rs"]
mod basic;

#[path = "schema_validation/comprehensive.rs"]
mod comprehensive;

#[path = "schema_validation/comprehensive_config.rs"]
mod comprehensive_config;

#[path = "schema_validation/comprehensive_duplicate.rs"]
mod comprehensive_duplicate;

#[path = "schema_validation/comprehensive_errors.rs"]
mod comprehensive_errors;

#[path = "schema_validation/comprehensive_fields.rs"]
mod comprehensive_fields;

#[path = "schema_validation/comprehensive_performance.rs"]
mod comprehensive_performance;

#[path = "schema_validation/comprehensive_validator.rs"]
mod comprehensive_validator;

#[path = "schema_validation/coverage.rs"]
mod coverage;

#[path = "schema_validation/fix.rs"]
mod fix;

#[path = "schema_validation/tool.rs"]
mod tool;
