//! Unit-level service creation and stability tests.
//!
//! Consolidates the former `basic_coverage.rs` and `coverage_expansion.rs`
//! near-clones. Tests use the shared `ahma_mcp::test_utils::build_test_service`
//! factory where the default configuration suffices; tests that exercise
//! non-default monitor/shell-pool timeouts or a guidance config go through the
//! local `build_service_inline` helper, because `build_test_service` cannot
//! express those variations.

use ahma_mcp::mcp_service::{AhmaMcpService, GuidanceConfig};
use ahma_mcp::shell_pool::ShellPoolConfig;
use ahma_mcp::utils::logging::init_test_logging;
use rmcp::handler::server::ServerHandler;
use std::collections::HashMap;
use std::time::Duration;

/// Thin wrapper matching the old local signature for tests that call it inline.
async fn create_test_service() -> (AhmaMcpService, tempfile::TempDir) {
    ahma_mcp::test_utils::build_test_service()
        .await
        .expect("Failed to create test service")
}

/// Build a service with a non-default monitor timeout, shell-pool config, or
/// guidance config — variations `build_test_service` cannot express.
async fn build_service_inline(
    monitor_timeout: Duration,
    shell_config: ShellPoolConfig,
    guidance: Option<GuidanceConfig>,
) -> (AhmaMcpService, tempfile::TempDir) {
    use ahma_mcp::adapter::Adapter;
    use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor};
    use ahma_mcp::sandbox::{Sandbox, SandboxMode};
    use ahma_mcp::shell_pool::ShellPoolManager;
    use std::sync::Arc;

    let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");

    let monitor_config = MonitorConfig::with_timeout(monitor_timeout);
    let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
    let shell_pool = Arc::new(ShellPoolManager::new(shell_config));
    let sandbox = Arc::new(
        Sandbox::new(
            vec![temp_dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap(),
    );
    let adapter =
        Arc::new(Adapter::new(Arc::clone(&operation_monitor), shell_pool, sandbox).unwrap());

    let service = AhmaMcpService::new(
        adapter,
        operation_monitor,
        Arc::new(HashMap::new()),
        Arc::new(guidance),
        false,
        false,
    )
    .await
    .unwrap();

    (service, temp_dir)
}

#[tokio::test]
async fn test_get_info_returns_complete_server_info() {
    let (service, _temp_dir) = create_test_service().await;

    let info = service.get_info();

    assert_eq!(info.protocol_version, rmcp::model::ProtocolVersion::LATEST);
    assert!(
        info.capabilities.tools.is_some(),
        "Server should advertise tool capabilities"
    );
}

#[tokio::test]
async fn test_service_creation_with_guidance_config() {
    let guidance_config = GuidanceConfig {
        guidance_blocks: HashMap::new(),
    };
    let (service, _temp_dir) = build_service_inline(
        Duration::from_secs(300),
        ShellPoolConfig::default(),
        Some(guidance_config),
    )
    .await;

    let info = service.get_info();
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn test_service_creation_with_existing_tool_configs() {
    let (service, _temp_dir) = create_test_service().await;

    // Verify service was created successfully
    let info = service.get_info();
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn test_service_creation_with_custom_shell_config() {
    // This test specifically exercises non-default monitor and shell pool timeouts.
    let shell_config = ShellPoolConfig {
        command_timeout: Duration::from_secs(120),
    };
    let (service, _temp_dir) =
        build_service_inline(Duration::from_secs(600), shell_config, None).await;

    let info = service.get_info();
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn test_multiple_service_instances() {
    let (service1, _temp_dir1) = create_test_service().await;
    let (service2, _temp_dir2) = create_test_service().await;

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
    let (service, _temp_dir) = create_test_service().await;

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
async fn test_service_stability_under_load() {
    init_test_logging();
    let (service, _temp) = create_test_service().await;

    let start_time = tokio::time::Instant::now();

    for _ in 0..100 {
        let _ = service.get_info();
    }

    let elapsed = start_time.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "100 get_info operations should complete very quickly"
    );

    let final_info = service.get_info();
    assert_eq!(
        final_info.protocol_version,
        rmcp::model::ProtocolVersion::LATEST
    );
}

#[tokio::test]
async fn test_service_protocol_version_consistency() {
    let (service, _temp_dir) = create_test_service().await;

    let info = service.get_info();
    assert_eq!(info.protocol_version, rmcp::model::ProtocolVersion::LATEST);

    let info2 = service.get_info();
    assert_eq!(info.protocol_version, info2.protocol_version);
}

#[tokio::test]
async fn test_service_capabilities_structure() {
    let (service, _temp_dir) = create_test_service().await;

    let info = service.get_info();
    assert!(
        info.capabilities.tools.is_some(),
        "Tools capability should be present"
    );
    if let Some(tools_capability) = &info.capabilities.tools {
        assert!(tools_capability.list_changed.is_some());
    }

    let info2 = service.get_info();
    assert_eq!(
        info.capabilities.tools.is_some(),
        info2.capabilities.tools.is_some()
    );
}

#[tokio::test]
async fn test_concurrent_service_creation() {
    let handles: Vec<_> = (0..5)
        .map(|_| tokio::spawn(async { create_test_service().await }))
        .collect();

    let results = futures::future::join_all(handles).await;

    for result in results {
        let (service, _temp_dir) = result.expect("Service creation should succeed");
        let info = service.get_info();
        assert!(info.capabilities.tools.is_some());
    }
}

#[tokio::test]
async fn test_service_with_guidance_blocks() {
    // Non-empty guidance with multiple blocks, including tool-specific keys.
    let mut guidance_blocks = HashMap::new();
    guidance_blocks.insert("general".to_string(), "Use tools carefully".to_string());
    guidance_blocks.insert(
        "shell".to_string(),
        "Shell commands should be safe".to_string(),
    );
    guidance_blocks.insert(
        "default".to_string(),
        "General guidance for all tools".to_string(),
    );
    guidance_blocks.insert("git".to_string(), "Git specific guidance".to_string());
    guidance_blocks.insert("cargo".to_string(), "Cargo specific guidance".to_string());

    let guidance_config = GuidanceConfig { guidance_blocks };
    let (service, _temp_dir) = build_service_inline(
        Duration::from_secs(300),
        ShellPoolConfig::default(),
        Some(guidance_config),
    )
    .await;

    let info = service.get_info();
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn test_service_creation_error_handling() {
    // This test specifically exercises a minimal (non-default) shell timeout config.
    let shell_config = ShellPoolConfig {
        command_timeout: Duration::from_secs(1),
    };
    let (service, _temp_dir) =
        build_service_inline(Duration::from_secs(300), shell_config, None).await;

    let info = service.get_info();
    assert!(info.capabilities.tools.is_some());
}
