//! Tests for the Android ADB and Gradle helper tool configurations.
//!
//! Validates that adb-devices, adb-pid-of, adb-crash-dump, adb-install,
//! adb-launch, and gradle-install-debug load correctly and declare the
//! expected subcommands, options, and positional arguments.

use ahma_mcp::config::{ToolConfig, ToolType};

fn load(json: &str) -> ToolConfig {
    serde_json::from_str(json).unwrap_or_else(|e| panic!("{json} failed to parse: {e}"))
}

/// Load a tool config from a path **relative to the workspace root**.
///
/// The `.ahma/` tool configs live at the workspace root, but cargo/nextest run
/// integration tests with the current directory set to the package dir
/// (`ahma_mcp/`), so a bare relative path like `.ahma/adb-devices.json` does not
/// resolve. Anchor to the workspace root via `CARGO_MANIFEST_DIR` (which is
/// `<workspace>/ahma_mcp`; its parent is the workspace root) so the tests pass
/// regardless of the process working directory.
fn load_file(rel: &str) -> ToolConfig {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has a parent (the workspace root)")
        .join(rel);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));
    load(&content)
}

// ---------------------------------------------------------------------------
// adb-devices
// ---------------------------------------------------------------------------

#[test]
fn test_adb_devices_config_loads() {
    let config = load_file(".ahma/adb-devices.json");
    assert_eq!(config.name, "adb-devices");
    assert_eq!(config.command, "adb");
    assert!(config.tool_type.is_none(), "not a livelog tool");
}

#[test]
fn test_adb_devices_has_devices_subcommand() {
    let config = load_file(".ahma/adb-devices.json");
    let subs = config.subcommand.expect("subcommand array required");
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].name, "devices");
}

// ---------------------------------------------------------------------------
// adb-pid-of
// ---------------------------------------------------------------------------

#[test]
fn test_adb_pid_of_config_loads() {
    let config = load_file(".ahma/adb-pid-of.json");
    assert_eq!(config.name, "adb-pid-of");
    // Multi-word command encoding the full `adb shell pidof -s` path.
    assert!(
        config.command.contains("adb"),
        "command must start with adb: {}",
        config.command
    );
    assert!(
        config.command.contains("pidof"),
        "command must include pidof: {}",
        config.command
    );
}

#[test]
fn test_adb_pid_of_has_required_package_arg() {
    let config = load_file(".ahma/adb-pid-of.json");
    let subs = config.subcommand.expect("subcommand required");
    let default_sub = &subs[0];
    let pos = default_sub
        .positional_args
        .as_ref()
        .expect("positional_args required");
    assert!(
        pos.iter()
            .any(|a| a.name == "package" && a.required == Some(true)),
        "package must be a required positional arg: {pos:?}"
    );
}

// ---------------------------------------------------------------------------
// adb-crash-dump
// ---------------------------------------------------------------------------

#[test]
fn test_adb_crash_dump_config_loads() {
    let config = load_file(".ahma/adb-crash-dump.json");
    assert_eq!(config.name, "adb-crash-dump");
    // Must include crash buffer and dump-mode flags.
    assert!(
        config.command.contains("logcat"),
        "command must include logcat: {}",
        config.command
    );
    assert!(
        config.command.contains("-d"),
        "command must use -d (dump mode, not tail): {}",
        config.command
    );
    assert!(
        config.command.contains("crash"),
        "command must target crash buffer: {}",
        config.command
    );
}

#[test]
fn test_adb_crash_dump_is_not_livelog() {
    let config = load_file(".ahma/adb-crash-dump.json");
    assert!(
        config.tool_type != Some(ToolType::Livelog),
        "adb-crash-dump is one-shot, not a live stream"
    );
    assert!(config.livelog.is_none());
}

// ---------------------------------------------------------------------------
// adb-install
// ---------------------------------------------------------------------------

#[test]
fn test_adb_install_config_loads() {
    let config = load_file(".ahma/adb-install.json");
    assert_eq!(config.name, "adb-install");
    assert_eq!(config.command, "adb");
}

#[test]
fn test_adb_install_subcommand_and_apk_path() {
    let config = load_file(".ahma/adb-install.json");
    let subs = config.subcommand.expect("subcommand required");
    let install_sub = subs
        .iter()
        .find(|s| s.name == "install")
        .expect("install subcommand");
    let pos = install_sub
        .positional_args
        .as_ref()
        .expect("positional_args required");
    assert!(
        pos.iter()
            .any(|a| a.name == "apk_path" && a.required == Some(true)),
        "apk_path must be a required positional arg: {pos:?}"
    );
}

// ---------------------------------------------------------------------------
// adb-launch
// ---------------------------------------------------------------------------

#[test]
fn test_adb_launch_config_loads() {
    let config = load_file(".ahma/adb-launch.json");
    assert_eq!(config.name, "adb-launch");
    assert!(
        config.command.contains("am start"),
        "command must call am start: {}",
        config.command
    );
}

#[test]
fn test_adb_launch_requires_component() {
    let config = load_file(".ahma/adb-launch.json");
    let subs = config.subcommand.expect("subcommand required");
    let default_sub = &subs[0];
    let pos = default_sub
        .positional_args
        .as_ref()
        .expect("positional_args required");
    assert!(
        pos.iter()
            .any(|a| a.name == "component" && a.required == Some(true)),
        "component must be required: {pos:?}"
    );
}

// ---------------------------------------------------------------------------
// gradle-install-debug
// ---------------------------------------------------------------------------

#[test]
fn test_gradle_install_debug_config_loads() {
    let config = load_file(".ahma/gradle-install-debug.json");
    assert_eq!(config.name, "gradle-install-debug");
    assert!(
        config.command.contains("gradlew"),
        "command must use gradlew: {}",
        config.command
    );
}

#[test]
fn test_gradle_install_debug_has_install_debug_subcommand() {
    let config = load_file(".ahma/gradle-install-debug.json");
    let subs = config.subcommand.expect("subcommand required");
    assert!(
        subs.iter().any(|s| s.name == "installDebug"),
        "must have installDebug subcommand: {subs:?}"
    );
}
