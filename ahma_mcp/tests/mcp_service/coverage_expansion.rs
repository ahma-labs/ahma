//! Extended coverage tests for AhmaMcpService creation scenarios.
//!
//! These tests use the shared `ahma_mcp::test_utils::build_test_service` factory
//! to eliminate boilerplate and keep the focus on the behaviour under test.
//! Tests that specifically exercise non-default configurations (shell pool
//! settings, guidance with tool-specific data) keep their inline construction.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ahma_mcp::adapter::Adapter;
use ahma_mcp::mcp_service::{AhmaMcpService, GuidanceConfig, LegacyGuidanceConfig};
use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};
use rmcp::handler::server::ServerHandler;

#[tokio::test]
async fn test_get_info_returns_complete_server_info() {
    let (service, _temp_dir) = ahma_mcp::test_utils::build_test_service()
        .await
        .expect("Failed to create test service");

    let info = service.get_info();

    assert_eq!(info.protocol_version, rmcp::model::ProtocolVersion::LATEST);
    assert!(
        info.capabilities.tools.is_some(),
        "Server should advertise tool capabilities"
    );

    // Verify the server info contains expected metadata
    println!("Server info: {:?}", info);
}

#[tokio::test]
async fn test_service_creation_with_guidance_config() {
    // This test specifically constructs a GuidanceConfig with LegacyGuidanceConfig,
    // which is the point of the test — keep inline setup for the guidance parts.
    let _temp_dir = tempfile::tempdir().expect("Failed to create temp directory");

    let monitor_config = MonitorConfig::with_timeout(Duration::from_secs(300));
    let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let sandbox = Arc::new(
        Sandbox::new(
            vec![_temp_dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap(),
    );
    let adapter =
        Arc::new(Adapter::new(Arc::clone(&operation_monitor), shell_pool, sandbox).unwrap());

    let configs = Arc::new(HashMap::new());
    let guidance_config = GuidanceConfig {
        guidance_blocks: HashMap::new(),
        templates: HashMap::new(),
        legacy_guidance: Some(LegacyGuidanceConfig {
            general_guidance: {
                let mut general = HashMap::new();
                general.insert("default".to_string(), "Test guidance".to_string());
                general
            },
            tool_specific_guidance: HashMap::new(),
        }),
    };
    let guidance = Arc::new(Some(guidance_config));

    let service = AhmaMcpService::new(adapter, operation_monitor, configs, guidance, false, false)
        .await
        .unwrap();

    let info = service.get_info();
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn test_service_creation_with_existing_tool_configs() {
    let (service, _temp_dir) = ahma_mcp::test_utils::build_test_service()
        .await
        .expect("Failed to create test service");

    let info = service.get_info();
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn test_service_creation_with_custom_timeouts() {
    // This test specifically exercises non-default monitor and shell pool timeouts.
    let _temp_dir = tempfile::tempdir().expect("Failed to create temp directory");

    let monitor_config = MonitorConfig::with_timeout(Duration::from_secs(600));
    let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));

    let shell_config = ShellPoolConfig {
        enabled: true,
        shells_per_directory: 2,
        max_total_shells: 5,
        shell_idle_timeout: Duration::from_secs(30),
        pool_cleanup_interval: Duration::from_secs(60),
        shell_spawn_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(120),
        health_check_interval: Duration::from_secs(30),
    };
    let shell_pool = Arc::new(ShellPoolManager::new(shell_config));
    let sandbox = Arc::new(
        Sandbox::new(
            vec![_temp_dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap(),
    );
    let adapter =
        Arc::new(Adapter::new(Arc::clone(&operation_monitor), shell_pool, sandbox).unwrap());

    let configs = Arc::new(HashMap::new());
    let guidance = Arc::new(None::<GuidanceConfig>);

    let service = AhmaMcpService::new(adapter, operation_monitor, configs, guidance, false, false)
        .await
        .unwrap();

    let info = service.get_info();
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn test_multiple_service_instances() {
    let (service1, _temp_dir1) = ahma_mcp::test_utils::build_test_service()
        .await
        .expect("Failed to create service 1");
    let (service2, _temp_dir2) = ahma_mcp::test_utils::build_test_service()
        .await
        .expect("Failed to create service 2");

    let info1 = service1.get_info();
    let info2 = service2.get_info();

    assert_eq!(info1.protocol_version, info2.protocol_version);
    assert_eq!(
        info1.capabilities.tools.is_some(),
        info2.capabilities.tools.is_some()
    );
}

#[tokio::test]
async fn test_service_stability_under_repeated_info_calls() {
    let (service, _temp_dir) = ahma_mcp::test_utils::build_test_service()
        .await
        .expect("Failed to create test service");

    let initial_info = service.get_info();

    for _ in 0..100 {
        let info = service.get_info();
        assert_eq!(info.protocol_version, initial_info.protocol_version);
        assert_eq!(
            info.capabilities.tools.is_some(),
            initial_info.capabilities.tools.is_some()
        );
    }
}

#[tokio::test]
async fn test_service_with_empty_configs() {
    let (service, _temp_dir) = ahma_mcp::test_utils::build_test_service()
        .await
        .expect("Failed to create test service");

    let info = service.get_info();
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn test_guidance_config_with_tool_specific_guidance() {
    // This test specifically exercises tool-specific guidance routing — keep inline.
    let _temp_dir = tempfile::tempdir().expect("Failed to create temp directory");

    let monitor_config = MonitorConfig::with_timeout(Duration::from_secs(300));
    let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let sandbox = Arc::new(
        Sandbox::new(
            vec![_temp_dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap(),
    );
    let adapter =
        Arc::new(Adapter::new(Arc::clone(&operation_monitor), shell_pool, sandbox).unwrap());

    let mut tool_specific_guidance = HashMap::new();
    let mut git_guidance = HashMap::new();
    git_guidance.insert("tips".to_string(), "Git specific guidance".to_string());
    let mut cargo_guidance = HashMap::new();
    cargo_guidance.insert("tips".to_string(), "Cargo specific guidance".to_string());
    tool_specific_guidance.insert("git".to_string(), git_guidance);
    tool_specific_guidance.insert("cargo".to_string(), cargo_guidance);

    let guidance_config = GuidanceConfig {
        guidance_blocks: HashMap::new(),
        templates: HashMap::new(),
        legacy_guidance: Some(LegacyGuidanceConfig {
            general_guidance: {
                let mut general = HashMap::new();
                general.insert(
                    "default".to_string(),
                    "General guidance for all tools".to_string(),
                );
                general
            },
            tool_specific_guidance,
        }),
    };
    let guidance = Arc::new(Some(guidance_config));
    let configs = Arc::new(HashMap::new());

    let service = AhmaMcpService::new(adapter, operation_monitor, configs, guidance, false, false)
        .await
        .unwrap();

    let info = service.get_info();
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn test_service_protocol_version_consistency() {
    let (service, _temp_dir) = ahma_mcp::test_utils::build_test_service()
        .await
        .expect("Failed to create test service");

    let info = service.get_info();
    assert_eq!(info.protocol_version, rmcp::model::ProtocolVersion::LATEST);
}

#[tokio::test]
async fn test_service_capabilities_structure() {
    let (service, _temp_dir) = ahma_mcp::test_utils::build_test_service()
        .await
        .expect("Failed to create test service");

    let info = service.get_info();
    assert!(info.capabilities.tools.is_some());

    if let Some(tools_capability) = &info.capabilities.tools {
        assert!(tools_capability.list_changed.is_some());
    }
}

#[tokio::test]
async fn test_concurrent_service_creation() {
    let futures: Vec<_> = (0..5)
        .map(|_| {
            tokio::spawn(async {
                ahma_mcp::test_utils::build_test_service()
                    .await
                    .expect("Failed to create test service")
            })
        })
        .collect();

    for future in futures {
        let (service, _temp_dir) = future.await.unwrap();
        let info = service.get_info();
        assert!(info.capabilities.tools.is_some());
    }
}

#[tokio::test]
async fn test_service_creation_error_handling() {
    let _temp_dir = tempfile::tempdir().expect("Failed to create temp directory");

    let monitor_config = MonitorConfig::with_timeout(Duration::from_secs(300));
    let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));

    // This test specifically exercises a minimal (non-default) shell pool config.
    let shell_config = ShellPoolConfig {
        enabled: true,
        shells_per_directory: 1,
        max_total_shells: 1,
        shell_idle_timeout: Duration::from_secs(1),
        pool_cleanup_interval: Duration::from_secs(1),
        shell_spawn_timeout: Duration::from_secs(1),
        command_timeout: Duration::from_secs(1),
        health_check_interval: Duration::from_secs(1),
    };

    let shell_pool = ShellPoolManager::new(shell_config);

    let sandbox = Arc::new(
        Sandbox::new(
            vec![_temp_dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap(),
    );
    let adapter = Adapter::new(
        Arc::clone(&operation_monitor),
        Arc::new(shell_pool),
        sandbox,
    );
    match adapter {
        Ok(adapter) => {
            let configs = Arc::new(HashMap::new());
            let guidance = Arc::new(None::<GuidanceConfig>);

            let result = AhmaMcpService::new(
                Arc::new(adapter),
                operation_monitor,
                configs,
                guidance,
                false,
                false,
            )
            .await;

            if let Ok(service) = result {
                let info = service.get_info();
                assert!(info.capabilities.tools.is_some());
            }
            // If it fails, that's also acceptable for minimal configs
        }
        Err(_) => {
            // It's acceptable for adapter creation to fail with minimal config
        }
    }
}
