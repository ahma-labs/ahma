//! Tool-specific config suites (formerly the `tool_tests` driver binary).
//!
//! CI selects the Android/Kotlin group by name (`-E 'test(kotlin) or test(android)'`),
//! so the test names here are load-bearing.

mod adb_tools_test;
mod android_logcat_test;
