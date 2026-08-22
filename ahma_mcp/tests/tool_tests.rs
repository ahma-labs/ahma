//! Consolidated tool-specific tests
//!
//! This module consolidates all tool-specific tests into a single test binary
//! to optimize cargo nextest test discovery time.

pub use ahma_mcp::test_utils as common;

#[path = "tool_suite/adb_tools_test.rs"]
mod adb_tools_test;

#[path = "tool_suite/android_logcat_test.rs"]
mod android_logcat_test;
