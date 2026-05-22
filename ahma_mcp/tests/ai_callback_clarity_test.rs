/// Test to verify that async callback messages are clear and actionable for AI agents
/// This addresses the need for clear, structured callback messages that enable AI decision-making
use ahma_mcp::callback_system::ProgressUpdate;
use ahma_mcp::utils::logging::init_test_logging;
use ahma_test_support::path_helpers::test_temp_path;

#[tokio::test]
async fn test_callback_messages_are_ai_actionable() {
    init_test_logging();

    // Test different types of progress updates that an AI might receive
    let id = "test_op_123".to_string();
    let working_dir = test_temp_path("callback_clarity_test")
        .to_string_lossy()
        .into_owned();

    // Test 1: Started message should clearly indicate what's beginning
    let started = ProgressUpdate::Started {
        id: id.clone(),
        command: "cargo nextest run".to_string(),
        description: "Running tests with nextest".to_string(),
    };

    match started {
        ProgressUpdate::Started {
            id: started_id,
            command,
            description,
        } => {
            assert_eq!(started_id, id);
            assert_eq!(command, "cargo nextest run");
            assert!(description.contains("nextest"));
        }
        other => panic!("Expected Started update, got {other:?}"),
    }

    // Test 2: Final result message should provide comprehensive outcome
    let final_result = ProgressUpdate::FinalResult {
        id: id.clone(),
        command: "cargo nextest run".to_string(),
        description: "Running tests with nextest".to_string(),
        working_directory: working_dir,
        success: false, // Test failure case
        full_output: "test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.23s\n\nfailures:\n\n---- my_test stdout ----\nthread 'my_test' panicked at 'assertion failed: `(left == right)`'".to_string(),
        duration_ms: 1230,
    };

    // Verify the final result contains all necessary info for AI decision-making
    if let ProgressUpdate::FinalResult {
        success,
        full_output,
        duration_ms,
        ..
    } = final_result
    {
        // AI should be able to determine success/failure
        assert!(!success, "Should indicate failure for AI to understand");

        // AI should be able to parse failure details
        assert!(
            full_output.contains("FAILED"),
            "Output should clearly indicate failure"
        );
        assert!(
            full_output.contains("1 failed"),
            "Output should specify failure count"
        );
        assert!(
            full_output.contains("panicked"),
            "Output should include panic details for debugging"
        );

        // AI should be able to assess performance
        assert!(
            duration_ms > 0,
            "Duration should be available for performance assessment"
        );
    } else {
        panic!("FAIL Expected FinalResult variant");
    }

    // Test 3: Failed message should be actionable
    let failed = ProgressUpdate::Failed {
        id: id.clone(),
        error: "Process exited with non-zero status: exit code 101".to_string(),
        duration_ms: 1500,
    };

    if let ProgressUpdate::Failed { error, .. } = failed {
        // AI should understand this is a failure and why
        assert!(
            error.contains("exit code"),
            "Error should include exit code for AI diagnosis"
        );
        assert!(
            error.contains("101"),
            "Specific exit code should be available"
        );
    } else {
        panic!("Expected Failed variant");
    }
}

#[tokio::test]
async fn test_callback_message_formatting_for_nextest() {
    init_test_logging();

    // Simulate a realistic nextest failure that an AI should understand
    let nextest_output = r#"
test result: FAILED. 15 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.34s

failures:

---- tests::integration_test stdout ----
thread 'tests::integration_test' panicked at 'assertion failed: expected success but got error'
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

---- tests::unit_test stdout ----
thread 'tests::unit_test' panicked at 'index out of bounds: the len is 3 but the index is 5'

---- tests::validation_test stdout ----
thread 'tests::validation_test' panicked at 'validation error: invalid input format'

test result: FAILED. 15 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.34s
"#;

    let final_result = ProgressUpdate::FinalResult {
        id: "nextest_op_456".to_string(),
        command: "cargo nextest run".to_string(),
        description: "Running comprehensive test suite".to_string(),
        working_directory: "/Users/paul/github/ahma_mcp".to_string(),
        success: false,
        full_output: nextest_output.to_string(),
        duration_ms: 2340,
    };

    // Verify AI can extract key information
    let full_output = match final_result {
        ProgressUpdate::FinalResult { full_output, .. } => full_output,
        other => panic!("Expected FinalResult update, got {other:?}"),
    };

    // Parse test results
    assert!(
        full_output.contains("15 passed; 3 failed"),
        "AI should be able to parse test counts"
    );

    // Identify specific test failures
    assert!(
        full_output.contains("integration_test"),
        "AI should identify specific failing tests"
    );
    assert!(
        full_output.contains("index out of bounds"),
        "AI should see specific error types"
    );
    assert!(
        full_output.contains("validation error"),
        "AI should understand different error categories"
    );

    // Performance information
    assert!(
        full_output.contains("finished in 2.34s"),
        "AI should have timing information"
    );
}
