//! # Utility Modules
//!
//! This module serves as a container for various utility sub-modules that provide
//! common, cross-cutting functionality used throughout the application.
//!
//! ## Sub-modules
//!
//! - **`logging`**: Contains functions for initializing and configuring the application's
//!   logging infrastructure using the `tracing` crate.
//!
//! - **`timestamp`**: Provides helpers for generating and formatting timestamps, which
//!   can be useful for logging, creating unique identifiers, or tracking event times.
//!
//! - **`time`**: Offers functionality for working with time-related tasks, building
//!   upon the `chrono` crate to provide date and time manipulation features.

/// User-facing cancellation message formatting.
pub mod cancellation;
pub mod logging;
/// Helper for generating descriptive operation IDs.
pub mod operation;
/// Dead-man's switch that terminates an orphaned frontend process when its
/// spawning parent (e.g. an IDE) dies.
pub mod parent_watchdog;
/// Safe stdout notification delivery for the subprocess-to-bridge protocol.
pub mod stdio;
/// Redirection of standard output to standard error.
pub mod stdio_redirect;
/// Serde helpers for `SystemTime` values.
pub mod time;
/// Timestamp formatting helpers.
pub mod timestamp;
