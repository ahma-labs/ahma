//! Regression coverage for the persistent completion-history design.
//!
//! These tests pin the interleavings that once produced the endless
//! notification loop: an operation completing after the notification loop
//! has already scanned, a late `update_status` on an already-completed
//! operation, and mixed-status operations persisting across repeated
//! history reads. The single-emission guarantee itself is asserted on the
//! unified event stream in `full_system_integration_bug_test.rs`.

use ahma_mcp::operation_monitor::{MonitorConfig, Operation, OperationMonitor, OperationStatus};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Completion racing the notification loop's clear: an operation that
/// completes *after* the loop has scanned must land in completion history
/// and stay there across repeated reads.
#[tokio::test]
async fn test_race_condition_between_completion_and_clearing() {
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(30),
    )));

    let test_op_id = "race-condition-test-op";

    // Operation is added in Pending state (as adapter.execute_async_in_dir does).
    let operation = Operation::new(
        test_op_id.to_string(),
        "test".to_string(),
        "race condition test".to_string(),
        None,
    );
    monitor.add_operation(operation).await;

    // A notification-loop scan while the operation is still pending finds nothing.
    let cleared_while_pending = monitor.get_completed_operations().await;
    assert!(
        cleared_while_pending.is_empty(),
        "Should not clear pending operations"
    );

    // The operation completes after that scan.
    monitor
        .update_status(
            test_op_id,
            OperationStatus::Completed,
            Some(Value::String("race condition test completed".to_string())),
        )
        .await;

    // Every subsequent read must find it, consistently: operations persist in
    // completion history so await/status can still observe them.
    for i in 1..=5 {
        let access = monitor.get_completed_operations().await;
        assert_eq!(
            access.len(),
            1,
            "Iteration {}: operation should remain in completion history",
            i
        );
        assert_eq!(access[0].id, test_op_id);
    }
}

/// A late `update_status` on an already-completed operation must be ignored:
/// the operation stays in history with its original result.
#[tokio::test]
async fn test_update_status_after_clear_race_condition() {
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(30),
    )));

    let test_op_id = "update-after-clear-test";

    let operation = Operation::new(
        test_op_id.to_string(),
        "test".to_string(),
        "update after clear test".to_string(),
        None,
    );
    monitor.add_operation(operation).await;
    monitor
        .update_status(
            test_op_id,
            OperationStatus::Completed,
            Some(Value::String("completed".to_string())),
        )
        .await;

    let initial_check = monitor.get_completed_operations().await;
    assert_eq!(initial_check.len(), 1);

    // Late-arriving update on the already-completed operation.
    monitor
        .update_status(
            test_op_id,
            OperationStatus::Completed,
            Some(Value::String("late update".to_string())),
        )
        .await;

    let recheck = monitor.get_completed_operations().await;
    assert_eq!(
        recheck.len(),
        1,
        "Operation should remain in completion history after late update"
    );
    assert_eq!(recheck[0].id, test_op_id);

    if let Some(result) = &recheck[0].result
        && let Some(result_str) = result.as_str()
    {
        assert_eq!(
            result_str, "completed",
            "Result should remain as original value - late updates are ignored"
        );
    }
}

/// Mixed-status operations (Failed and Completed) both persist in completion
/// history and stay accessible across repeated reads.
#[tokio::test]
async fn test_persistent_completion_history_behavior() {
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(30),
    )));

    let op1_id = "op_test_1";
    let op2_id = "op_test_2";

    monitor
        .add_operation(Operation::new(
            op1_id.to_string(),
            "cargo".to_string(),
            "cargo test".to_string(),
            None,
        ))
        .await;
    monitor
        .add_operation(Operation::new(
            op2_id.to_string(),
            "cargo".to_string(),
            "cargo check".to_string(),
            None,
        ))
        .await;

    monitor
        .update_status(
            op1_id,
            OperationStatus::Failed,
            Some(Value::String("test failed".to_string())),
        )
        .await;
    monitor
        .update_status(
            op2_id,
            OperationStatus::Completed,
            Some(Value::String("check succeeded".to_string())),
        )
        .await;

    monitor.wait_for_operation(op1_id).await;
    monitor.wait_for_operation(op2_id).await;

    for _ in 1..=3 {
        let completed_ops = monitor.get_completed_operations().await;
        assert_eq!(
            completed_ops.len(),
            2,
            "Both operations should persist in completion_history"
        );
        let ids: Vec<&str> = completed_ops.iter().map(|op| op.id.as_str()).collect();
        assert!(ids.contains(&op1_id));
        assert!(ids.contains(&op2_id));
    }
}
