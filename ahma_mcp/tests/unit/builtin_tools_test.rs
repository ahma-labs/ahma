use ahma_mcp::config::load_tool_configs;
use ahma_mcp::shell::cli::AppConfig;
use tempfile::tempdir;

#[tokio::test]
async fn test_load_builtin_tools_async() {
    let temp_dir = tempdir().unwrap();

    let config_python = AppConfig {
        tool_bundles: vec!["python".to_string()],
        ..AppConfig::default()
    };
    let configs_python = load_tool_configs(&config_python, Some(temp_dir.path()))
        .await
        .unwrap();
    assert!(
        configs_python.contains_key("python"),
        "Should load bundled python.json"
    );

    let config_multiple = AppConfig {
        tool_bundles: vec!["python".to_string(), "simplify".to_string()],
        ..AppConfig::default()
    };
    let configs_multiple = load_tool_configs(&config_multiple, Some(temp_dir.path()))
        .await
        .unwrap();
    assert!(
        configs_multiple.contains_key("python"),
        "Should load python"
    );
    assert!(
        configs_multiple.contains_key("simplify"),
        "Should load simplify"
    );
}

/// Verify that a user-provided .ahma/ tool definition overrides the bundled version.
#[tokio::test]
async fn test_filesystem_overrides_bundled_tool() {
    let temp_dir = tempdir().unwrap();

    // Create a local python.json that defines "python" with a custom description
    let custom_python = r#"{
  "name": "python",
  "description": "Custom user-defined python tool",
  "command": "python",
  "enabled": true,
  "subcommand": [
    {
      "name": "run",
      "description": "Custom run",
      "options": [
        {
          "name": "version",
          "type": "boolean",
          "description": "Show version"
        }
      ]
    }
  ]
}"#;
    std::fs::write(temp_dir.path().join("python.json"), custom_python).unwrap();

    // Load with tool_bundles: ["python"] (bundled) AND the local override
    let config = AppConfig {
        tool_bundles: vec!["python".to_string()],
        ..AppConfig::default()
    };
    let configs = load_tool_configs(&config, Some(temp_dir.path()))
        .await
        .unwrap();

    assert!(configs.contains_key("python"), "Should have python tool");
    let python = &configs["python"];
    assert_eq!(
        python.description, "Custom user-defined python tool",
        "Local .ahma/ definition should override the bundled version"
    );
}

/// Verify that a workspace tool colliding with a built-in is rejected from
/// `.ahma/` files.
///
/// Iterates **every** built-in name rather than a hand-picked handful. The
/// hand-picked version of this test covered four names and passed for months
/// while the reserved list was five names short of the actual built-in set:
/// a workspace tool named `sandbox_grant` loaded without complaint and was
/// then dropped from `tools/list` by the dedup filter, so the user got no
/// error and no tool. Driving the loop from the canonical list means the
/// coverage cannot fall behind the tool set again.
#[tokio::test]
async fn test_reserved_names_rejected() {
    let temp_dir = tempdir().unwrap();

    for reserved in ahma_mcp::constants::BUILTIN_TOOL_NAMES {
        let config = format!(
            r#"{{
  "name": "{}",
  "description": "Should be rejected",
  "command": "echo",
  "enabled": true,
  "subcommand": [{{ "name": "default", "description": "test" }}]
}}"#,
            reserved
        );
        std::fs::write(temp_dir.path().join(format!("{}.json", reserved)), &config).unwrap();

        let config = AppConfig::default();
        let result = load_tool_configs(&config, Some(temp_dir.path())).await;
        assert!(
            result.is_err(),
            "a workspace tool named '{}' collides with the built-in of that name and must be refused",
            reserved
        );

        // Clean up for next iteration
        std::fs::remove_file(temp_dir.path().join(format!("{}.json", reserved))).unwrap();
    }
}

/// Verify that bundled tools load even when NO tools directory exists.
/// This is the exact scenario when a user runs `ahma_mcp --python --simplify`
/// from a repo that has no `.ahma/` directory and no `--tools-dir` flag.
#[tokio::test]
async fn test_bundled_tools_load_without_tools_dir() {
    // Pass tool_bundles with python + simplify but NO tools_dir (None)
    let config = AppConfig {
        tool_bundles: vec!["python".to_string(), "simplify".to_string()],
        ..AppConfig::default()
    };

    // Call with None — this is the code path that was previously broken
    let configs = load_tool_configs(&config, None).await.unwrap();

    assert!(
        configs.contains_key("python"),
        "--tool python flag should load bundled python tool even without tools_dir. Got keys: {:?}",
        configs.keys().collect::<Vec<_>>()
    );
    assert!(
        configs.contains_key("simplify"),
        "--tool simplify flag should load bundled simplify tool even without tools_dir. Got keys: {:?}",
        configs.keys().collect::<Vec<_>>()
    );

    // run_terminal_command synthetic config should also be present
    assert!(
        configs.contains_key("run_terminal_command"),
        "run_terminal_command synthetic config should always be present"
    );
}

/// Verify that each individual bundled flag works without a tools directory.
#[tokio::test]
async fn test_each_bundled_flag_works_without_tools_dir() {
    let tool_and_expected: &[(&str, &str)] = &[
        ("simplify", "simplify"),
        ("python", "python"),
        ("git", "git"),
        ("github", "gh"),
        ("fileutils", "file-tools"),
    ];

    for &(tool, expected_tool) in tool_and_expected {
        let config = AppConfig {
            tool_bundles: vec![tool.to_string()],
            ..AppConfig::default()
        };
        let configs = load_tool_configs(&config, None).await.unwrap();
        assert!(
            configs.contains_key(expected_tool),
            "Tool flag '{}' should load bundled tool '{}' even without tools_dir. Got keys: {:?}",
            tool,
            expected_tool,
            configs.keys().collect::<Vec<_>>()
        );
    }
}

/// Verify that `load_tool_configs` never produces duplicate keys.
/// The synthetic `run_terminal_command` config and RESERVED_TOOL_NAMES must be consistent.
#[tokio::test]
async fn test_no_duplicate_tool_names_in_config_output() {
    let temp_dir = tempdir().unwrap();

    // Create two tool configs in the temp dir
    for (file, name, desc) in &[
        ("tool_a.json", "tool_a", "Tool A"),
        ("tool_b.json", "tool_b", "Tool B"),
    ] {
        let json = format!(
            r#"{{
  "name": "{}",
  "description": "{}",
  "command": "echo",
  "enabled": true,
  "subcommand": [{{ "name": "default", "description": "test" }}]
}}"#,
            name, desc
        );
        std::fs::write(temp_dir.path().join(file), json).unwrap();
    }

    let config = AppConfig::default();
    let configs = load_tool_configs(&config, Some(temp_dir.path()))
        .await
        .unwrap();

    // HashMap keys are inherently unique, but verify that all names match their keys
    for (key, config) in &configs {
        assert_eq!(
            key, &config.name,
            "HashMap key '{}' must match config.name '{}'",
            key, config.name
        );
    }

    // Verify run_terminal_command synthetic config exists exactly once and is in RESERVED set
    assert!(
        configs.contains_key("run_terminal_command"),
        "Synthetic run_terminal_command config should always be present"
    );

    // Count occurrences of each name to confirm no logical duplicates
    let names: Vec<&str> = configs.values().map(|c| c.name.as_str()).collect();
    let mut seen = std::collections::HashSet::new();
    for name in &names {
        assert!(
            seen.insert(name),
            "Duplicate tool name '{}' found in configs",
            name
        );
    }
}

/// Verify that when bundle flags are set and .ahma/ is auto-detected,
/// ALL local .ahma/ tools are loaded (not just those matching the flags).
/// Bundle flags serve as fallbacks for tools not found locally.
#[tokio::test]
async fn test_bundle_flags_with_auto_detected_ahma_loads_all_local_tools() {
    let temp_dir = tempdir().unwrap();

    // Create three local tool definitions: two match flags, one does not
    let tools = [
        ("python.json", "python", "Local python tool"),
        ("simplify.json", "simplify", "Local simplify tool"),
        ("git.json", "git", "Local git tool"),
    ];
    for (file, name, desc) in &tools {
        let json = format!(
            r#"{{
  "name": "{}",
  "description": "{}",
  "command": "{}",
  "enabled": true,
  "subcommand": [{{ "name": "default", "description": "test" }}]
}}"#,
            name, desc, name
        );
        std::fs::write(temp_dir.path().join(file), json).unwrap();
    }

    // tool_bundles: ["python", "simplify"], but NOT git. Auto-detected dir (not explicit).
    let config = AppConfig {
        tool_bundles: vec!["python".to_string(), "simplify".to_string()],
        explicit_tools_dir: false,
        ..AppConfig::default()
    };

    let configs = load_tool_configs(&config, Some(temp_dir.path()))
        .await
        .unwrap();

    // ALL three local tools should be loaded (local .ahma/ always fully loaded)
    assert!(
        configs.contains_key("python"),
        "Local python should be loaded. Keys: {:?}",
        configs.keys().collect::<Vec<_>>()
    );
    assert!(
        configs.contains_key("simplify"),
        "Local simplify should be loaded. Keys: {:?}",
        configs.keys().collect::<Vec<_>>()
    );
    assert!(
        configs.contains_key("git"),
        "Local git should be loaded even without --git flag. Keys: {:?}",
        configs.keys().collect::<Vec<_>>()
    );

    // Verify local definitions are used (not bundled fallbacks)
    assert_eq!(
        configs["python"].description, "Local python tool",
        "Local .ahma/ definition should be used, not the bundled version"
    );
    assert_eq!(
        configs["simplify"].description, "Local simplify tool",
        "Local .ahma/ definition should be used, not the bundled version"
    );
}

/// Verify that local .ahma/ definitions override bundled tools AND
/// other non-flagged local tools are also loaded.
#[tokio::test]
async fn test_local_ahma_overrides_bundled_with_all_loaded() {
    let temp_dir = tempdir().unwrap();

    // Create a local python.json with custom description + a non-flagged tool
    let custom_python = r#"{
  "name": "python",
  "description": "Overridden python from local .ahma/",
  "command": "python",
  "enabled": true,
  "subcommand": [{ "name": "run", "description": "Run" }]
}"#;
    std::fs::write(temp_dir.path().join("python.json"), custom_python).unwrap();

    let extra_tool = r#"{
  "name": "my_extra_tool",
  "description": "Extra tool not matching any flag",
  "command": "echo",
  "enabled": true,
  "subcommand": [{ "name": "default", "description": "test" }]
}"#;
    std::fs::write(temp_dir.path().join("extra.json"), extra_tool).unwrap();

    let config = AppConfig {
        tool_bundles: vec!["python".to_string()],
        explicit_tools_dir: false,
        ..AppConfig::default()
    };

    let configs = load_tool_configs(&config, Some(temp_dir.path()))
        .await
        .unwrap();

    // Local python should override bundled
    assert_eq!(
        configs["python"].description, "Overridden python from local .ahma/",
        "Local definition should override bundled"
    );

    // Extra non-flagged tool should also be loaded
    assert!(
        configs.contains_key("my_extra_tool"),
        "Non-flagged local tools should also be loaded. Keys: {:?}",
        configs.keys().collect::<Vec<_>>()
    );
}
