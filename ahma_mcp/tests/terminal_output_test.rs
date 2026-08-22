//! Tests for terminal output formatting, including edge cases.
//!
//! Merged from the former terminal_output_coverage_test.rs — same functions,
//! union of the assertion sets.

use ahma_mcp::terminal_output::TerminalOutput;
use ahma_mcp::test_utils::assertions::assert_formatted_json_contains;
use ahma_mcp::utils::logging::init_test_logging;
use serde_json::json;

#[test]
fn test_should_display_comprehensive() {
    init_test_logging();
    // Empty cases
    assert!(!TerminalOutput::should_display(""));
    assert!(!TerminalOutput::should_display("   "));
    assert!(!TerminalOutput::should_display("\n"));
    assert!(!TerminalOutput::should_display("\t"));
    assert!(!TerminalOutput::should_display("\r"));
    assert!(!TerminalOutput::should_display("\n\n\n"));
    assert!(!TerminalOutput::should_display("\t\t\t"));
    assert!(!TerminalOutput::should_display("  \n\t\r  "));
    assert!(!TerminalOutput::should_display("   \n\t  \r  "));

    // Content cases
    assert!(TerminalOutput::should_display("a"));
    assert!(TerminalOutput::should_display("some content"));
    assert!(TerminalOutput::should_display("  content  "));
    assert!(TerminalOutput::should_display("\n  content  \n"));
    assert!(TerminalOutput::should_display("0")); // Number as string
    assert!(TerminalOutput::should_display("false")); // Boolean as string

    // Edge cases with special characters
    assert!(TerminalOutput::should_display(".")); // Single period
    assert!(TerminalOutput::should_display("!")); // Exclamation
    assert!(TerminalOutput::should_display("@#$%")); // Special characters
    assert!(TerminalOutput::should_display("🚀")); // Unicode emoji
}

#[test]
fn test_format_content_json_pretty_printing() {
    init_test_logging();
    // Simple JSON object
    let json_input = r#"{"name":"test","version":"1.0.0"}"#;
    assert_formatted_json_contains(
        json_input,
        &["{\n", "  \"name\": \"test\"", "  \"version\": \"1.0.0\""],
    );

    // Nested JSON
    let nested_json = r#"{"user":{"id":123,"name":"Alice","settings":{"theme":"dark"}}}"#;
    assert_formatted_json_contains(
        nested_json,
        &[
            "\"user\": {",
            "    \"id\": 123",
            "    \"settings\": {",
            "      \"theme\": \"dark\"",
        ],
    );

    // JSON array
    let array_json = r#"[{"id":1,"name":"first"},{"id":2,"name":"second"}]"#;
    assert_formatted_json_contains(array_json, &["[\n", "  {\n", "    \"id\": 1"]);
}

#[test]
fn test_format_content_invalid_json() {
    init_test_logging();
    // Invalid JSON should be treated as regular string
    let invalid_json = r#"{"name": invalid}"#;
    let formatted = TerminalOutput::format_content(invalid_json);
    assert_eq!(formatted, r#"{"name": invalid}"#);

    // Partial JSON
    let partial = r#"{"incomplete":"#;
    let formatted = TerminalOutput::format_content(partial);
    assert_eq!(formatted, r#"{"incomplete":"#); // Returns as-is since it's not valid JSON
}

#[test]
fn test_format_content_string_cleanup() {
    init_test_logging();
    // Escaped newlines
    let input = "Line 1\\nLine 2\\nLine 3";
    let formatted = TerminalOutput::format_content(input);
    assert_eq!(formatted, "Line 1\nLine 2\nLine 3");

    // Escaped tabs
    let input = "Column1\\tColumn2\\tColumn3";
    let formatted = TerminalOutput::format_content(input);
    assert_eq!(formatted, "Column1\tColumn2\tColumn3");

    // Escaped quotes
    let input = "He said \\\"Hello World\\\"";
    let formatted = TerminalOutput::format_content(input);
    assert_eq!(formatted, "He said \"Hello World\"");

    // Mixed escapes
    let input = "Mixed:\\nNewline\\tTab\\\"Quote";
    let formatted = TerminalOutput::format_content(input);
    assert_eq!(formatted, "Mixed:\nNewline\tTab\"Quote");

    // Mixed escapes with doubled backslashes (Windows-style paths preserved)
    let input = "Path: C:\\\\folder\\\\file.txt\\nNew line\\tTab\\\"Quote\\\"";
    let formatted = TerminalOutput::format_content(input);
    assert_eq!(
        formatted,
        "Path: C:\\\\folder\\\\file.txt\nNew line\tTab\"Quote\""
    );

    // Already unescaped string passes through
    let input = "This is a normal string with spaces";
    let formatted = TerminalOutput::format_content(input);
    assert_eq!(formatted, "This is a normal string with spaces");

    // Whitespace trimming
    let input = "  \n  content with spaces  \n  ";
    let formatted = TerminalOutput::format_content(input);
    assert_eq!(formatted, "content with spaces");
}

#[test]
fn test_format_content_edge_cases() {
    init_test_logging();
    // Empty string
    let formatted = TerminalOutput::format_content("");
    assert_eq!(formatted, "");

    // Only whitespace
    let formatted = TerminalOutput::format_content("   \n\t  ");
    assert_eq!(formatted, "");

    // JSON null
    let formatted = TerminalOutput::format_content("null");
    assert_eq!(formatted, "null");

    // JSON boolean
    let formatted = TerminalOutput::format_content("true");
    assert_eq!(formatted, "true");

    // JSON number
    let formatted = TerminalOutput::format_content("42");
    assert_eq!(formatted, "42");

    // JSON string value
    let formatted = TerminalOutput::format_content("\"hello world\"");
    assert_eq!(formatted, "\"hello world\"");

    // Malformed JSON is returned as-is
    let malformed_json = r#"{"incomplete": "json""#;
    let formatted = TerminalOutput::format_content(malformed_json);
    assert_eq!(formatted, r#"{"incomplete": "json""#);

    // Valid JSON with complex escaping still pretty-prints
    let complex_json =
        r#"{"path": "C:\\Users\\test", "message": "Hello\nWorld", "quoted": "He said \"Hello\""}"#;
    let formatted = TerminalOutput::format_content(complex_json);
    assert!(formatted.contains("{\n"));
    assert!(formatted.contains("  \"path\""));
}

#[test]
fn test_format_content_json_parsing_edge_cases() {
    init_test_logging();

    // Deeply nested JSON
    let nested_json = r#"{"level1": {"level2": {"level3": {"data": "deep"}}}}"#;
    let result = TerminalOutput::format_content(nested_json);
    assert!(result.contains("{\n"));
    assert!(result.contains("  \"level1\""));

    // JSON array
    let json_array = r#"[{"name": "item1"}, {"name": "item2"}]"#;
    let result = TerminalOutput::format_content(json_array);
    assert!(result.contains("[\n"));

    // JSON with null values
    let json_with_null = r#"{"value": null, "empty": "", "number": 0}"#;
    let result = TerminalOutput::format_content(json_with_null);
    assert!(result.contains("null"));
    assert!(result.contains("\"\""));

    // JSON with boolean values
    let json_with_bool = r#"{"success": true, "failed": false}"#;
    let result = TerminalOutput::format_content(json_with_bool);
    assert!(result.contains("true"));
    assert!(result.contains("false"));

    // Malformed JSON variants are returned unchanged
    let malformed_variants = vec![
        r#"{"incomplete""#,
        r#"{"missing_value":}"#,
        r#"{"trailing_comma": "value",}"#,
        r#"{unquoted_key: "value"}"#,
        r#"{"single_quotes": 'value'}"#,
    ];

    for malformed in malformed_variants {
        let result = TerminalOutput::format_content(malformed);
        // Should return the original string when JSON parsing fails
        assert_eq!(result, malformed);
    }
}

#[test]
fn test_format_content_complex_json() {
    init_test_logging();
    // Real-world-like JSON structure
    let complex_json = json!({
        "status": "success",
        "data": {
            "items": [
                {"id": 1, "active": true, "metadata": null},
                {"id": 2, "active": false, "metadata": {"tags": ["urgent", "review"]}}
            ],
            "total": 2,
            "pagination": {
                "page": 1,
                "limit": 10,
                "has_more": false
            }
        },
        "timestamp": "2024-01-15T10:30:00Z"
    });

    let json_string = serde_json::to_string(&complex_json).unwrap();
    let formatted = TerminalOutput::format_content(&json_string);

    // Should be pretty printed
    assert!(formatted.contains("{\n"));
    assert!(formatted.contains("  \"status\": \"success\""));
    assert!(formatted.contains("  \"data\": {"));
    assert!(formatted.contains("    \"items\": ["));
    assert!(formatted.contains("      {"));
    assert!(formatted.contains("        \"id\": 1"));
    assert!(formatted.contains("        \"metadata\": null"));
    assert!(formatted.contains("          \"tags\": ["));
    assert!(formatted.contains("            \"urgent\""));
}

// Note: display_result and display_await_results write to stderr; capturing it
// is complex, so their core logic is asserted through format_content and
// should_display. The tests below exercise the display paths for panics and
// early returns.

#[tokio::test]
async fn test_display_result_with_empty_content() {
    init_test_logging();
    // This should not panic and should handle empty content gracefully
    // The function returns early for empty content, so no output is produced
    TerminalOutput::display_result("test_op", "test_cmd", "test description", "").await;
    TerminalOutput::display_result("test_op", "test_cmd", "test description", "   \n\t  ").await;
    TerminalOutput::display_result(
        "whitespace_test",
        "test command",
        "test description",
        "   \n\t  \r\n  ",
    )
    .await;
}

#[tokio::test]
async fn test_display_result_with_actual_content() {
    init_test_logging();

    // Actual content exercises the full formatting path
    TerminalOutput::display_result(
        "content_test",
        "echo hello",
        "Simple echo command",
        "Hello, World!\nThis is a test.",
    )
    .await;

    // JSON content
    TerminalOutput::display_result(
        "json_test",
        "cargo metadata",
        "Get cargo metadata",
        r#"{"name": "test", "version": "1.0.0", "dependencies": []}"#,
    )
    .await;
}

#[tokio::test]
async fn test_display_await_results_with_empty_results() {
    init_test_logging();
    // Should handle empty results vector gracefully
    TerminalOutput::display_await_results(&[]).await;

    // Should handle vector with empty strings
    TerminalOutput::display_await_results(&[String::new(), "  ".to_string()]).await;
}

#[tokio::test]
async fn test_display_await_results_with_content() {
    init_test_logging();
    // Single result
    TerminalOutput::display_await_results(&["Single result content".to_string()]).await;

    // Multiple results mixing JSON, plain text, and an empty entry
    let results = vec![
        r#"{"result": "first"}"#.to_string(),
        "Plain text result".to_string(),
        r#"{"operation": "build", "status": "success"}"#.to_string(),
        "Plain text output\nwith multiple lines".to_string(),
        "".to_string(), // Empty result
        r#"{"operation": "test", "status": "failed", "error": "Assertion failed"}"#.to_string(),
        r#"{"result": "third", "status": "complete"}"#.to_string(),
    ];
    TerminalOutput::display_await_results(&results).await;

    // Results containing escaped characters
    let escaped_results = vec![
        "Result with\\nescaped\\nlines".to_string(),
        r#"{"message": "Error\\noccurred", "path": "C:\\\\temp\\\\file.txt"}"#.to_string(),
    ];
    TerminalOutput::display_await_results(&escaped_results).await;
}
