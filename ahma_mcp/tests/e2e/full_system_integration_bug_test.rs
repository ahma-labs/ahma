#[cfg(test)]
mod tests {
    use ahma_common::event_dispatcher::OperationEvent;
    use ahma_mcp::adapter::Adapter;
    use ahma_mcp::config::load_tool_configs;
    use ahma_mcp::mcp_service::AhmaMcpService;
    use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor};
    use ahma_mcp::sandbox::{Sandbox, SandboxMode};
    use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};

    use std::sync::Arc;
    use std::time::Duration;

    /// Drain all immediately-available events from a subscription, counting
    /// terminal events (Completed/Failed/Cancelled/TimedOut) for the given
    /// operation id.
    fn drain_terminal_count(
        rx: &mut tokio::sync::broadcast::Receiver<Arc<OperationEvent>>,
        id: &str,
    ) -> usize {
        let mut count = 0;
        while let Ok(event) = rx.try_recv() {
            if event.is_terminal() && event.operation_id() == id {
                count += 1;
            }
        }
        count
    }

    /// This test simulates the full system to ensure that with the
    /// `completion_history` architecture and the unified event stream,
    /// operations result in exactly one terminal `Completed` event.
    #[tokio::test]
    async fn test_full_system_integration_single_notification() {
        println!("🔍 Testing full system integration for single notification guarantee...");

        // System setup
        let monitor_config = MonitorConfig::with_timeout(Duration::from_secs(30));
        let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
        let shell_pool_config = ShellPoolConfig::default();
        let shell_pool_manager = Arc::new(ShellPoolManager::new(shell_pool_config));
        let sandbox = Arc::new(
            Sandbox::new(
                vec![std::env::current_dir().unwrap()],
                SandboxMode::Test,
                false,
                false,
                false,
            )
            .unwrap(),
        );
        let adapter =
            Arc::new(Adapter::new(operation_monitor.clone(), shell_pool_manager, sandbox).unwrap());
        let configs = Arc::new(
            load_tool_configs(
                &ahma_mcp::shell::cli::AppConfig::default(),
                Some(&std::path::PathBuf::from(".ahma")),
            )
            .await
            .unwrap(),
        );
        let _service = AhmaMcpService::new(
            adapter.clone(),
            operation_monitor.clone(),
            configs,
            Arc::new(None),
            false,
            false,
        )
        .await
        .unwrap();
        println!("OK Full system initialized");

        // Subscribe BEFORE starting the operation so no events are missed.
        let mut events = operation_monitor.subscribe_events();

        let current_dir = std::env::current_dir().unwrap();
        let current_dir_str = current_dir.to_str().unwrap();

        // Start an operation
        let id = adapter
            .execute_async_in_dir("cargo", "version", None, current_dir_str, Some(30))
            .await
            .expect("Failed to execute async operation");
        println!("🚀 Started operation: {}", id);

        // Wait for the operation to complete
        let completed_op = operation_monitor.wait_for_operation(&id).await;
        assert!(
            completed_op.is_some(),
            "Operation should have completed and been returned by wait_for_operation"
        );
        println!("OK Operation completed and is in history.");

        // The unified event stream must carry exactly one terminal event for
        // the operation — the single emission point guarantee.
        let terminal_events = drain_terminal_count(&mut events, &id);
        assert_eq!(
            terminal_events, 1,
            "BUG: Expected exactly 1 terminal event on the unified stream, got {}",
            terminal_events
        );

        // The operation must persist in completion history across repeated
        // reads (await/status rely on this; the loop-until-clear design that
        // caused the original bug would drop it).
        for _ in 1..=3 {
            let completed_ops = operation_monitor.get_completed_operations().await;
            assert!(
                completed_ops.iter().any(|op| op.id == id),
                "Operation should persist in completion history"
            );
        }

        println!("OK Full system integration test passed - exactly one terminal event emitted.");
    }

    /// Test system under load with multiple operations, ensuring each is notified once.
    #[tokio::test]
    async fn test_multiple_operations_system_integration() {
        println!("⚡ Testing multiple operations system integration...");

        // System setup
        let monitor_config = MonitorConfig::with_timeout(Duration::from_secs(30));
        let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
        let shell_pool_config = ShellPoolConfig::default();
        let shell_pool_manager = Arc::new(ShellPoolManager::new(shell_pool_config));
        let sandbox = Arc::new(
            Sandbox::new(
                vec![std::env::current_dir().unwrap()],
                SandboxMode::Test,
                false,
                false,
                false,
            )
            .unwrap(),
        );
        let adapter =
            Arc::new(Adapter::new(operation_monitor.clone(), shell_pool_manager, sandbox).unwrap());

        let current_dir = std::env::current_dir().unwrap();
        let current_dir_str = current_dir.to_str().unwrap();

        // Start multiple operations
        let op_ids = vec![
            adapter
                .execute_async_in_dir("cargo", "version", None, current_dir_str, Some(30))
                .await
                .expect("Failed to execute first async operation"),
            adapter
                .execute_async_in_dir("cargo", "--version", None, current_dir_str, Some(30))
                .await
                .expect("Failed to execute second async operation"),
        ];
        println!("🚀 Started operations: {:?}", op_ids);

        // Wait for all operations to complete
        for op_id in &op_ids {
            let completed_op = operation_monitor.wait_for_operation(op_id).await;
            assert!(
                completed_op.is_some(),
                "Operation {} should have completed",
                op_id
            );
        }
        println!("OK All operations completed.");

        assert_eq!(
            operation_monitor.get_completed_operations().await.len(),
            op_ids.len(),
            "Not all operations completed in time."
        );

        // Every started operation must be present in completion history, and
        // stay there across repeated reads.
        for _ in 1..=3 {
            let completed_ops = operation_monitor.get_completed_operations().await;
            for op_id in &op_ids {
                assert!(
                    completed_ops.iter().any(|op| op.id == *op_id),
                    "Operation {} should be in completion history",
                    op_id
                );
            }
        }

        println!("OK Multiple operations integration test passed.");
    }
}
