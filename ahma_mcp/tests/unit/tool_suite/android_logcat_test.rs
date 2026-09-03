//! Tests for android-logcat.json livelog tool configuration.
//!
//! Validates that the android-logcat livelog tool config loads correctly,
//! has the right tool_type, and that the device-scoping fields (runtime
//! parameters, env mapping, clear command, pre-filter, env-driven provider)
//! are wired the way the live pipeline expects.

use ahma_mcp::config::{ToolConfig, ToolType};
use serde_json::{Map, Value, json};

fn load_config() -> ToolConfig {
    let json = include_str!("../../../../.ahma/android-logcat.json");
    serde_json::from_str(json).expect("android-logcat.json should parse")
}

#[test]
fn test_android_logcat_config_loads() {
    let config = load_config();
    assert_eq!(config.name, "android-logcat");
    assert_eq!(config.tool_type, Some(ToolType::Livelog));
    assert_eq!(config.command, "adb");
}

#[test]
fn test_android_logcat_livelog_block() {
    let config = load_config();
    let lc = config.livelog.expect("livelog block required");
    assert_eq!(lc.source_command, "adb");
    assert!(lc.source_args.contains(&"logcat".to_string()));
    assert!(!lc.detection_prompt.is_empty());
}

#[test]
fn test_android_logcat_has_detection_prompt() {
    let config = load_config();
    let lc = config.livelog.unwrap();
    let prompt = lc.detection_prompt.to_lowercase();
    assert!(
        prompt.contains("crash") || prompt.contains("exception") || prompt.contains("fatal"),
        "detection_prompt should reference crash/exception/fatal patterns"
    );
}

#[test]
fn test_android_logcat_declares_serial_and_pid_parameters() {
    let config = load_config();
    let lc = config.livelog.unwrap();
    let names: Vec<&str> = lc.parameters.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains(&"serial"), "should declare a `serial` param");
    assert!(names.contains(&"pid"), "should declare a `pid` param");
    // Both are optional — a single attached device needs neither.
    assert!(lc.parameters.iter().all(|p| !p.required));
}

#[test]
fn test_android_logcat_serial_maps_to_android_serial_env() {
    let config = load_config();
    let lc = config.livelog.unwrap();
    assert_eq!(
        lc.env.get("ANDROID_SERIAL").map(String::as_str),
        Some("${serial}"),
        "serial must flow to adb via ANDROID_SERIAL"
    );
}

#[test]
fn test_android_logcat_clears_buffer_and_prefilters() {
    let config = load_config();
    let lc = config.livelog.unwrap();
    assert_eq!(
        lc.clear_command,
        Some(vec!["logcat".to_string(), "-c".to_string()]),
        "should clear log buffers before monitoring"
    );
    let pat = lc
        .prefilter_regex
        .expect("a pre-filter keeps LLM cost down");
    let re = regex::Regex::new(&pat).expect("pre-filter must be a valid regex");
    assert!(re.is_match("FATAL EXCEPTION: main"));
    assert!(re.is_match("01-01 12:00:00.000  1000  1000 E ActivityManager: ANR in app"));
}

#[test]
fn test_android_logcat_provider_is_env_driven_with_defaults() {
    let config = load_config();
    let lc = config.livelog.unwrap();
    // Stored verbatim with `${VAR:-default}` placeholders…
    assert!(lc.llm_provider.base_url.contains("AHMA_LIVELOG_BASE_URL"));
    assert!(lc.llm_provider.model.contains("AHMA_LIVELOG_MODEL"));
    // …and resolve to the bumped defaults when the env vars are unset.
    let resolved = lc.llm_provider.resolve().expect("provider resolves");
    assert_eq!(resolved.base_url, "http://localhost:11434/v1");
    assert_eq!(resolved.model, "lfm2.5:8b");
}

#[test]
fn test_android_logcat_runtime_drops_pid_when_absent() {
    let config = load_config();
    let lc = config.livelog.unwrap();
    // No parameters supplied: --pid token drops, no ANDROID_SERIAL env set.
    let rt = lc.resolve_runtime(&Map::new()).unwrap();
    assert!(
        !rt.source_args.iter().any(|a| a.contains("--pid")),
        "pid flag should disappear when no pid is given: {:?}",
        rt.source_args
    );
    assert!(rt.source_args.contains(&"logcat".to_string()));
    assert!(rt.env.is_empty(), "no serial → no env override");
    assert!(rt.clear, "clear defaults to true");
}

#[test]
fn test_android_logcat_runtime_applies_serial_and_pid() {
    let config = load_config();
    let lc = config.livelog.unwrap();
    let mut args = Map::new();
    args.insert("serial".to_string(), json!("emulator-5554"));
    args.insert("pid".to_string(), json!("4321"));
    args.insert("clear".to_string(), json!(false));
    let rt = lc.resolve_runtime(&args).unwrap();
    assert!(
        rt.source_args.contains(&"--pid=4321".to_string()),
        "pid should be substituted: {:?}",
        rt.source_args
    );
    assert_eq!(
        rt.env,
        vec![("ANDROID_SERIAL".to_string(), "emulator-5554".to_string())]
    );
    assert!(!rt.clear, "explicit clear:false disables clearing");
}

/// android-logcat enables structured output so alerts carry discrete fields
/// (level, summary, exception_class, top_frame) instead of opaque prose.
#[test]
fn test_android_logcat_has_structured_output_enabled() {
    let config = load_config();
    let lc = config.livelog.unwrap();
    assert!(
        lc.structured_output,
        "android-logcat should enable structured_output for machine-readable alerts"
    );
}

/// The `clear` arg is read regardless of declared parameters; sanity-check that
/// an unrelated extra arg does not get treated as a parameter.
#[test]
fn test_android_logcat_runtime_ignores_unknown_args() {
    let config = load_config();
    let lc = config.livelog.unwrap();
    let mut args = Map::new();
    args.insert("not_a_param".to_string(), Value::String("x".into()));
    let rt = lc.resolve_runtime(&args).unwrap();
    assert!(rt.env.is_empty());
}
