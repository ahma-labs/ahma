//! Full-lifecycle integration tests for AhmaMcpService.
//!
//! Uses the shared `ahma_mcp::test_utils::build_test_service` factory so the
//! temp dir is always kept alive for the duration of the test.

use ahma_mcp::mcp_service::AhmaMcpService;
use ahma_mcp::utils::logging::init_test_logging;
use rmcp::handler::server::ServerHandler;
use tokio::time::Instant;

/// Thin wrapper matching the old local signature, now keeping `TempDir` alive.
async fn create_test_service() -> (AhmaMcpService, tempfile::TempDir) {
    ahma_mcp::test_utils::build_test_service()
        .await
        .expect("Failed to create test service")
}

#[tokio::test]
async fn test_mcp_service_creation_and_info() {
    init_test_logging();
    let (service, _temp) = create_test_service().await;

    let info = service.get_info();
    assert_eq!(info.protocol_version, rmcp::model::ProtocolVersion::LATEST);
    assert!(
        info.capabilities.tools.is_some(),
        "Server should advertise tool capabilities"
    );

    let tools_capability = info.capabilities.tools.unwrap();
    println!("Tools capability: {:?}", tools_capability);
}

#[tokio::test]
async fn test_mcp_service_multiple_creation() {
    init_test_logging();
    let (service1, _temp1) = create_test_service().await;
    let (service2, _temp2) = create_test_service().await;

    let info1 = service1.get_info();
    let info2 = service2.get_info();

    assert_eq!(info1.protocol_version, info2.protocol_version);
    assert_eq!(
        info1.capabilities.tools.is_some(),
        info2.capabilities.tools.is_some()
    );
}

#[tokio::test]
async fn test_mcp_service_stability_under_load() {
    init_test_logging();
    let (service, _temp) = create_test_service().await;

    let start_time = Instant::now();

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
async fn test_mcp_service_with_tool_configs() {
    init_test_logging();
    // Test service behavior with empty tool configs (the default case).
    let (service, _temp) = ahma_mcp::test_utils::build_test_service()
        .await
        .expect("Failed to create test service");

    let info = service.get_info();
    assert_eq!(info.protocol_version, rmcp::model::ProtocolVersion::LATEST);
    assert!(
        info.capabilities.tools.is_some(),
        "Should still advertise tool capabilities"
    );
    assert!(service.configs.read().unwrap().is_empty());
}
